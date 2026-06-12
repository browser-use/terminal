//! Browser tool handler.
//!
//! SANCTIONED DIVERGENCE: this is browser-use's product surface and has no
//! codex analog. The handler is a THIN adapter over the existing
//! `browser-use-browser` crate. It translates a typed [`BrowserRequest`] into
//! the appropriate `browser-use-browser` call and maps the returned
//! `BrowserCommandOutput` / `BrowserScriptOutput` into the seam's
//! [`ExecOutput`].
//!
//! ## What it wraps
//!
//! Two legacy model-facing paths are modeled here:
//!   * the hidden `browser <cmd-string>` command path
//!     -> [`browser_use_browser::run_browser_command`]
//!   * the start/observe/cancel script path
//!     -> [`browser_use_browser::start_browser_script`] /
//!        [`browser_use_browser::observe_browser_script`] /
//!        [`browser_use_browser::cancel_browser_script`]
//!
//! ## Testability without Bun/Chrome
//!
//! The real `browser-use-browser` functions spawn a Bun + Chrome toolchain
//! (external processes, a CDP websocket, a local bridge port) that is not
//! present in CI/test environments. To keep the adapter testable we put the
//! browser backend behind a small [`BrowserBackend`] trait. The production
//! implementation, [`RealBackend`], delegates 1:1 to `browser-use-browser`;
//! tests inject a fake backend instead and never touch Bun/Chrome/network.
//!
//! ## Concurrency
//!
//! The `browser-use-browser` functions are synchronous and spawn external
//! processes. To avoid blocking the async runtime, [`BrowserTool::run`] invokes
//! the backend on a blocking thread via [`tokio::task::spawn_blocking`].
//!
//! Browser actions are NOT parallel-safe: a single browser session/CDP
//! connection is shared and serialized, matching the legacy tool set where the
//! browser tool is excluded from the parallel set.

use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::{anyhow, bail};
use base64::{engine::general_purpose, Engine as _};
use browser_use_browser::{BrowserCommandOutput, BrowserScriptOutput};
use browser_use_llm::schema::ContentPart;
use browser_use_store::Store;
use regex::Regex;
use serde_json::{json, Value};

use crate::infra::{
    record_browser_command_response_events, record_browser_script_response_events_for_tool,
};
use crate::session::SharedStore;
use crate::tools::approval::ExecApprovalRequirement;
use crate::tools::runtime::{Approvable, Sandboxable};
use crate::tools::runtime::{ExecOutput, SandboxAttempt, ToolCtx, ToolError, ToolRuntime};
use crate::tools::sandbox::{SandboxPermissions, SandboxPreference};

/// Default per-script timeout (seconds) when a request omits one.
///
/// The `browser-use-browser` script fns take a `timeout_seconds`; we default to
/// a generous 300s so a single page interaction has room to complete while the
/// run-level task timebox remains responsible for bounding the whole agent.
pub const DEFAULT_BROWSER_SCRIPT_TIMEOUT_SECS: u64 = 300;

/// Default observe poll window (ms) for [`BrowserAction::Observe`].
///
/// Restored to the pre-regression (88-baseline) 1s default. The 30s default +
/// 30s clamp-floor ("observe30") blocked each observe for up to 30s, burning the
/// run-level task timebox on long-running scripts and leaving tasks unfinished
/// (e.g. real_v8 tasks 1, 4 never emitted session.done). The run-level timebox,
/// not coarse poll windows, is responsible for bounding total turns.
pub const DEFAULT_OBSERVE_TIMEOUT_MS: u64 = 1_000;
pub const MAX_OBSERVE_TIMEOUT_MS: u64 = 120_000;

/// Appended to `browser_script` stdout when the response carries image parts.
///
/// The dispatch layer strips this marker and re-wraps the JSON payload as typed
/// [`ContentPart`]s so provider protocols can send images to vision-capable
/// models while preserving a plain text fallback for logs/tests.
pub const BROWSER_SCRIPT_CONTENT_STDOUT_PREFIX: &str = "\n__browser_script_content__:";
/// Maximum bytes of browser-script text returned to the next model turn.
///
/// Matches the collected-output limit (SCRIPT_MAX_OUTPUT_CHARS = 120k): the
/// CURRENT turn must see the full script output to write correct follow-up
/// code (codex parity: fresh outputs are never capped; the context manager
/// truncates tool outputs only as they age into history via the policy*1.2
/// rule in context/mod.rs). The old 4KB cap forced blind guess-first coding
/// (KeyError-class bug explosion 8->53 across the 88->81 eval regression).
pub const MAX_INLINE_BROWSER_SCRIPT_STDOUT_BYTES: usize = 120 * 1024;

const BROWSER_PREF_MODE: &str = "browser.preference.mode";
const BROWSER_PREF_BROWSER: &str = "browser.preference.browser";
const BROWSER_PREF_BROWSER_LABEL: &str = "browser.preference.browser_label";
const BROWSER_PREF_PROFILE: &str = "browser.preference.profile";
const EVAL_MAX_OBSERVE_TIMEOUT_ENV: &str = "BROWSER_USE_EVAL_MAX_OBSERVE_TIMEOUT_MS";
const BROWSER_DOMAIN_PROFILE_PREFIX: &str = "browser.domain_profile.";
const BROWSER_SCRIPT_MAX_IMAGE_DIMENSION: u32 = 8_000;
const BROWSER_PREF_PROFILE_LABEL: &str = "browser.preference.profile_label";

/// What the model wants the browser to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BrowserAction {
    /// Hidden `browser` command tool: a single command string evaluated by the
    /// browser runtime. Maps to [`browser_use_browser::run_browser_command`].
    Command {
        /// The raw command string (e.g. `go https://example.com`).
        command: String,
    },
    /// `browser_script`: start a script and either return its final result or a
    /// running handle, matching main's `browser_script action=start` behavior.
    Execute {
        /// The script body to run in the browser runtime.
        script: String,
        /// Compatibility field for older current-branch calls. Main ignores
        /// this concept; script execution always uses `start_browser_script`.
        background: bool,
    },
    /// `observe`: poll an in-flight run.
    /// Maps to [`browser_use_browser::observe_browser_script`].
    Observe {
        /// Run identifier returned by a backgrounded `Execute`.
        run_id: String,
    },
    /// `cancel`: stop an in-flight run.
    /// Maps to [`browser_use_browser::cancel_browser_script`].
    Cancel {
        /// Run identifier returned by a backgrounded `Execute`.
        run_id: String,
    },
}

/// Request payload for the browser tool.
///
/// The browser-use-browser fns are session-scoped and need a working directory
/// plus an artifact directory; those identifiers are carried here so the adapter
/// stays thin (it forwards them unchanged).
///
/// # Deserialization (via [`BrowserWireArgs`])
///
/// The model's JSON arg object is FLAT (`action`/`session_id`/`script`/… — see
/// [`BrowserWireArgs`]), whereas this `Req` holds a tagged [`BrowserAction`] enum
/// and carried plumbing. So `BrowserRequest` deserializes THROUGH the flat wire
/// args: `#[serde(from = "BrowserWireArgs")]` runs the
/// [`From<BrowserWireArgs>`](BrowserRequest::from) adapter after deserializing the
/// model object. This makes `BrowserRequest: Deserialize`, so the tool registers
/// with the registry's plain `register` (the registry deserializes the model
/// object straight into `BrowserRequest`). Behavior is unchanged — the adapter
/// only reshapes the already-parsed fields.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
#[serde(from = "BrowserWireArgs")]
pub struct BrowserRequest {
    /// The action to perform.
    pub action: BrowserAction,
    /// Browser session id the action is bound to.
    pub session_id: String,
    /// Working directory for the browser runtime. When `None`, the
    /// [`ToolCtx::cwd`] is used.
    pub cwd: Option<PathBuf>,
    /// Directory for run artifacts (screenshots, downloads). When `None`,
    /// [`ToolCtx::artifact_root`] is used.
    pub artifact_dir: Option<PathBuf>,
    /// Script timeout in seconds (script paths only). When `None`,
    /// [`DEFAULT_BROWSER_SCRIPT_TIMEOUT_SECS`].
    pub timeout_secs: Option<u64>,
    /// Observe poll window in milliseconds (observe path only). When `None`,
    /// [`DEFAULT_OBSERVE_TIMEOUT_MS`].
    pub observe_timeout_ms: Option<u64>,
}

impl BrowserRequest {
    /// Convenience constructor for the `browser <cmd>` command path.
    pub fn command(session_id: impl Into<String>, command: impl Into<String>) -> Self {
        Self {
            action: BrowserAction::Command {
                command: command.into(),
            },
            session_id: session_id.into(),
            cwd: None,
            artifact_dir: None,
            timeout_secs: None,
            observe_timeout_ms: None,
        }
    }

    /// Convenience constructor for the script execute path.
    pub fn execute(
        session_id: impl Into<String>,
        script: impl Into<String>,
        background: bool,
    ) -> Self {
        Self {
            action: BrowserAction::Execute {
                script: script.into(),
                background,
            },
            session_id: session_id.into(),
            cwd: None,
            artifact_dir: None,
            timeout_secs: None,
            observe_timeout_ms: None,
        }
    }

    fn effective_timeout_secs(&self, default_timeout_secs: u64) -> u64 {
        self.timeout_secs.unwrap_or(default_timeout_secs)
    }

    fn effective_observe_ms(&self) -> u64 {
        let normal = self
            .observe_timeout_ms
            .unwrap_or(DEFAULT_OBSERVE_TIMEOUT_MS)
            .clamp(DEFAULT_OBSERVE_TIMEOUT_MS, MAX_OBSERVE_TIMEOUT_MS);
        match eval_max_observe_timeout_ms() {
            Some(max_ms) => normal.min(max_ms.max(1_000)),
            None => normal,
        }
    }
}

fn eval_max_observe_timeout_ms() -> Option<u64> {
    // Observe timing is controlled ONLY by its own env var. It used to also be
    // implicitly capped at 30s whenever BROWSER_USE_EVAL_DONE_AUDIT was set —
    // a hidden cross-subsystem coupling (a finalization flag silently changing
    // polling behavior) that made eval runs hard to reason about.
    std::env::var(EVAL_MAX_OBSERVE_TIMEOUT_ENV)
        .ok()
        .and_then(|value| value.trim().parse::<u64>().ok())
        .filter(|value| *value > 0)
}

#[allow(dead_code)]
fn env_flag_enabled(value: &str) -> bool {
    let normalized = value.trim().to_ascii_lowercase();
    !normalized.is_empty()
        && !matches!(
            normalized.as_str(),
            "0" | "false" | "off" | "no" | "disabled"
        )
}

/// Model-facing wire arguments for the browser tool.
///
/// [`BrowserRequest`] is a PARSED form: its [`BrowserAction`] is an internally
/// tagged enum whose payload fields differ per variant, and the request carries
/// plumbing fields (`cwd`/`artifact_dir`) the model never sets. So the registry
/// cannot deserialize a `BrowserRequest` directly. Instead this flat
/// `BrowserWireArgs` matches the JSON the model actually emits and an
/// [`From<BrowserWireArgs>`](BrowserRequest::from) adapter parses it into the
/// typed request (the registry registers the tool over `BrowserWireArgs`).
///
/// # Wire shape (model-facing args)
///
/// ```json
/// { "code": "..." }
/// { "action": "start", "code": "..." }
/// { "action": "command", "command": "go https://example.com" }
/// { "action": "observe", "run_id": "r1" }
/// { "action": "cancel",  "run_id": "r1" }
/// ```
///
/// The variants mirror the existing [`BrowserAction`] cases and the legacy
/// model-facing browser paths (the hidden `browser <cmd>` command path and the
/// `browser_script` start/observe/cancel paths; see the module docs and legacy
/// `browser-use-core/src/tools/mod.rs`). `cwd` / `artifact_dir` are
/// carried-but-optional plumbing fields the router supplies; the per-action
/// payload fields (`command` / `script` / `run_id`) are validated by the `From`
/// adapter against the chosen `action`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
pub struct BrowserWireArgs {
    /// Which browser operation to perform.
    #[serde(default)]
    pub action: Option<BrowserActionKind>,
    /// Browser session id the action is bound to. The model normally omits this;
    /// production fills it from the current agent session through `ToolCtx`.
    #[serde(default)]
    pub session_id: Option<String>,
    /// Command string for the `command` action.
    #[serde(default)]
    pub command: Option<String>,
    /// Main-branch browser command field.
    #[serde(default)]
    pub cmd: Option<String>,
    /// Script body for the `start` action.
    #[serde(default)]
    pub script: Option<String>,
    /// Alias used by Codex-style tool schemas.
    #[serde(default)]
    pub code: Option<String>,
    /// Compatibility field for older current-branch calls.
    #[serde(default)]
    pub background: bool,
    /// Run identifier for the `observe` / `cancel` actions.
    #[serde(default)]
    pub run_id: Option<String>,
    /// Working directory for the browser runtime.
    #[serde(default)]
    pub cwd: Option<PathBuf>,
    /// Directory for run artifacts.
    #[serde(default)]
    pub artifact_dir: Option<PathBuf>,
    /// Script timeout in seconds (script paths only).
    #[serde(default)]
    pub timeout_secs: Option<u64>,
    /// Observe poll window in milliseconds (observe path only).
    #[serde(default)]
    pub observe_timeout_ms: Option<u64>,
}

/// The `action` discriminator of [`BrowserWireArgs`], mirroring the
/// [`BrowserAction`] variants.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BrowserActionKind {
    /// Hidden `browser <cmd>` command path.
    Command,
    /// `browser_script` start path.
    #[serde(alias = "execute")]
    Start,
    /// Poll an in-flight run.
    Observe,
    /// Cancel an in-flight run.
    Cancel,
}

impl From<BrowserWireArgs> for BrowserRequest {
    /// Parse the flat model wire args into the typed [`BrowserRequest`].
    ///
    /// A payload field missing for the chosen `action` defaults to an empty
    /// string; the tool's `run` validation then rejects it with the same
    /// "must not be empty" error it uses for an explicitly-empty value (so a
    /// malformed call surfaces a clean rejection rather than a deserialize
    /// failure).
    fn from(w: BrowserWireArgs) -> Self {
        let script = w.script.or(w.code);
        let command = w.cmd.or(w.command);
        let action_kind = w.action.unwrap_or_else(|| {
            if script.is_some() {
                BrowserActionKind::Start
            } else if command.is_some() {
                BrowserActionKind::Command
            } else if w.run_id.is_some() {
                BrowserActionKind::Observe
            } else {
                BrowserActionKind::Start
            }
        });
        let action = match action_kind {
            BrowserActionKind::Command => BrowserAction::Command {
                command: command.unwrap_or_default(),
            },
            BrowserActionKind::Start => BrowserAction::Execute {
                script: script.unwrap_or_default(),
                background: w.background,
            },
            BrowserActionKind::Observe => BrowserAction::Observe {
                run_id: w.run_id.unwrap_or_default(),
            },
            BrowserActionKind::Cancel => BrowserAction::Cancel {
                run_id: w.run_id.unwrap_or_default(),
            },
        };
        BrowserRequest {
            action,
            session_id: w.session_id.unwrap_or_default(),
            cwd: w.cwd,
            artifact_dir: w.artifact_dir,
            timeout_secs: w.timeout_secs,
            observe_timeout_ms: w.observe_timeout_ms,
        }
    }
}

/// The seam over `browser-use-browser`.
///
/// Implemented for real by [`RealBackend`] (delegates to the wrapped crate) and
/// by a fake in tests so the adapter can be exercised without Bun/Chrome.
///
/// All methods are synchronous and may spawn external processes; the adapter is
/// responsible for running them off the async runtime. Errors are
/// `anyhow::Error`, mirroring the wrapped crate.
pub trait BrowserBackend: Send + Sync {
    /// Run a one-shot browser command. Wraps `run_browser_command`.
    fn command(
        &self,
        session_id: &str,
        cwd: &std::path::Path,
        artifact_dir: &std::path::Path,
        command: &str,
    ) -> anyhow::Result<BrowserCommandOutput>;

    /// Run a script to completion. Wraps `run_browser_script`.
    fn run_script(
        &self,
        session_id: &str,
        cwd: &std::path::Path,
        artifact_dir: &std::path::Path,
        code: &str,
        timeout_secs: u64,
    ) -> anyhow::Result<BrowserScriptOutput>;

    /// Start a script in the background. Wraps `start_browser_script`.
    fn start_script(
        &self,
        session_id: &str,
        cwd: &std::path::Path,
        artifact_dir: &std::path::Path,
        code: &str,
        timeout_secs: u64,
    ) -> anyhow::Result<BrowserScriptOutput>;

    /// Observe an in-flight run. Wraps `observe_browser_script`.
    fn observe_script(
        &self,
        session_id: &str,
        run_id: &str,
        observe_timeout_ms: u64,
    ) -> anyhow::Result<BrowserScriptOutput>;

    /// Cancel an in-flight run. Wraps `cancel_browser_script`.
    fn cancel_script(&self, session_id: &str, run_id: &str) -> anyhow::Result<BrowserScriptOutput>;

    /// Update the preferred browser mode for backends that auto-connect before
    /// browser actions. Test/fake backends can ignore this.
    fn set_browser_mode(&self, _browser_mode: Option<String>) {}

    /// Clean up backend-owned browser/session/script resources for a session.
    fn cleanup_session(&self, _session_id: &str) -> usize {
        0
    }
}

/// Production backend: a thin delegation to `browser-use-browser`.
#[derive(Debug, Clone)]
pub struct RealBackend {
    browser_mode: Arc<Mutex<Option<String>>>,
    session_registry: browser_use_browser::BrowserSessionRegistry,
    script_registry: browser_use_browser::BrowserScriptRunRegistry,
}

impl Default for RealBackend {
    fn default() -> Self {
        Self {
            browser_mode: Arc::new(Mutex::new(None)),
            session_registry: browser_use_browser::BrowserSessionRegistry::global(),
            script_registry: browser_use_browser::BrowserScriptRunRegistry::global(),
        }
    }
}

impl RealBackend {
    pub fn with_browser_mode(browser_mode: Option<String>) -> Self {
        Self {
            browser_mode: Arc::new(Mutex::new(browser_mode)),
            session_registry: browser_use_browser::BrowserSessionRegistry::global(),
            script_registry: browser_use_browser::BrowserScriptRunRegistry::global(),
        }
    }

    pub fn with_browser_mode_and_registries(
        browser_mode: Option<String>,
        session_registry: browser_use_browser::BrowserSessionRegistry,
        script_registry: browser_use_browser::BrowserScriptRunRegistry,
    ) -> Self {
        Self {
            browser_mode: Arc::new(Mutex::new(browser_mode)),
            session_registry,
            script_registry,
        }
    }

    pub fn with_shared_browser_mode(browser_mode: Arc<Mutex<Option<String>>>) -> Self {
        Self {
            browser_mode,
            session_registry: browser_use_browser::BrowserSessionRegistry::global(),
            script_registry: browser_use_browser::BrowserScriptRunRegistry::global(),
        }
    }

    pub fn with_shared_browser_mode_and_registries(
        browser_mode: Arc<Mutex<Option<String>>>,
        session_registry: browser_use_browser::BrowserSessionRegistry,
        script_registry: browser_use_browser::BrowserScriptRunRegistry,
    ) -> Self {
        Self {
            browser_mode,
            session_registry,
            script_registry,
        }
    }

    fn normalized_browser_mode(&self) -> Option<String> {
        let mode = self
            .browser_mode
            .lock()
            .ok()
            .and_then(|mode| mode.clone())?;
        let mode = mode.trim();
        if mode.is_empty() {
            return None;
        }
        Some(
            match mode {
                "cloud" | "browser-use-cloud" | "remote-cloud" => "cloud",
                "remote-cdp" | "cdp" => "remote-cdp",
                "headless" | "headless-chromium" | "managed-headless" => "managed-headless",
                other => other,
            }
            .to_string(),
        )
    }

    fn should_ensure_before_command(&self, command: &str) -> bool {
        let Some(mode) = self.normalized_browser_mode() else {
            return false;
        };
        let Ok(words) = browser_command_words(command) else {
            return false;
        };
        let words = words.iter().map(String::as_str).collect::<Vec<_>>();
        if mode == "remote-cdp"
            && matches!(words.as_slice(), ["browser", "status", ..] | ["status", ..])
        {
            return true;
        }
        if browser_command_is_passive(words.as_slice()) {
            return false;
        }
        !matches!(
            words.as_slice(),
            ["browser", "remote", "start", ..]
                | ["remote", "start", ..]
                | ["browser", "remote", "stop", ..]
                | ["remote", "stop", ..]
                | ["browser", "connect", "remote-cdp", ..]
                | ["connect", "remote-cdp", ..]
        )
    }

    fn rewrite_command_for_mode(&self, command: &str) -> String {
        let Ok(words) = browser_command_words(command) else {
            return command.to_string();
        };
        let words = words.iter().map(String::as_str).collect::<Vec<_>>();
        if self.normalized_browser_mode().as_deref() == Some("cloud")
            && matches!(
                words.as_slice(),
                ["browser", "local", ..]
                    | ["local", ..]
                    | ["browser", "connect", "local", ..]
                    | ["connect", "local", ..]
                    | ["browser", "connect", "managed", ..]
                    | ["connect", "managed", ..]
            )
        {
            return "browser status --json".to_string();
        }
        command.to_string()
    }

    fn ensure_configured_browser(
        &self,
        session_id: &str,
        cwd: &std::path::Path,
        artifact_dir: &std::path::Path,
    ) -> anyhow::Result<Vec<Value>> {
        let Some(mode) = self.normalized_browser_mode() else {
            return Ok(Vec::new());
        };
        let status = browser_use_browser::run_browser_command_with_options_and_registries(
            session_id,
            cwd,
            artifact_dir,
            "browser status --json",
            browser_use_browser::BrowserCommandOptions::default(),
            &self.script_registry,
            &self.session_registry,
        )?;
        let mut events = status.events;
        let connected =
            status.content.get("connection").and_then(Value::as_str) == Some("connected");
        let current_mode = status.content.get("mode").and_then(Value::as_str);
        let owner = status.content.get("owner").and_then(Value::as_str);
        let desired_command = if mode == "remote-cdp" {
            if connected && current_mode == Some("remote-cdp") {
                None
            } else {
                Some(remote_cdp_connect_command()?)
            }
        } else {
            desired_browser_connect_command(mode.as_str(), connected, current_mode, owner)
                .map(str::to_string)
        };
        let Some(desired_command) = desired_command else {
            return Ok(events);
        };
        let mut started = browser_use_browser::run_browser_command_with_options_and_registries(
            session_id,
            cwd,
            artifact_dir,
            &desired_command,
            browser_use_browser::BrowserCommandOptions::default(),
            &self.script_registry,
            &self.session_registry,
        )?;
        events.append(&mut started.events);
        Ok(events)
    }
}

pub(crate) fn browser_command_is_passive(words: &[&str]) -> bool {
    matches!(
        words,
        ["browser", "status", ..]
            | ["status", ..]
            // Read-only / informational commands must never trigger an
            // auto-connect: `help`/`doctor`/`domain` are the first things an
            // external assistant runs to orient itself.
            | ["browser", "help", ..]
            | ["help", ..]
            | ["browser", "--help", ..]
            | ["--help", ..]
            | ["browser", "-h", ..]
            | ["-h", ..]
            | ["browser", "doctor", ..]
            | ["doctor", ..]
            | ["browser", "domain", ..]
            | ["domain", ..]
            | ["browser", "connect", ..]
            | ["connect", ..]
            | ["browser", "local", "list", ..]
            | ["local", "list", ..]
            | ["browser", "local", "profiles", ..]
            | ["local", "profiles", ..]
            | ["browser", "local", "open", ..]
            | ["local", "open", ..]
            | ["browser", "local", "setup", ..]
            | ["local", "setup", ..]
            | ["browser", "runtime", "logs", ..]
            | ["runtime", "logs", ..]
            | ["browser", "runtime", "ownership", ..]
            | ["runtime", "ownership", ..]
    )
}

pub(crate) fn desired_browser_connect_command(
    selected_mode: &str,
    connected: bool,
    current_mode: Option<&str>,
    owner: Option<&str>,
) -> Option<&'static str> {
    match selected_mode {
        "cloud" => {
            if connected && current_mode == Some("remote-cloud") {
                None
            } else {
                Some("browser remote start")
            }
        }
        "local" | "local-chrome" => {
            if connected && current_mode == Some("local") {
                None
            } else if !connected && current_mode == Some("local") && owner == Some("external") {
                None
            } else {
                Some("browser connect local")
            }
        }
        "managed-headless" => {
            if connected && current_mode == Some("managed") {
                None
            } else {
                Some("browser connect managed --headless")
            }
        }
        "managed-headed" => {
            if connected && current_mode == Some("managed") {
                None
            } else {
                Some("browser connect managed --headed")
            }
        }
        _ => None,
    }
}

impl BrowserBackend for RealBackend {
    fn set_browser_mode(&self, browser_mode: Option<String>) {
        if let Ok(mut mode) = self.browser_mode.lock() {
            *mode = browser_mode;
        }
    }

    fn command(
        &self,
        session_id: &str,
        cwd: &std::path::Path,
        artifact_dir: &std::path::Path,
        command: &str,
    ) -> anyhow::Result<BrowserCommandOutput> {
        let mut events = if self.should_ensure_before_command(command) {
            self.ensure_configured_browser(session_id, cwd, artifact_dir)?
        } else {
            Vec::new()
        };
        let effective_command = self.rewrite_command_for_mode(command);
        let mut output = browser_use_browser::run_browser_command_with_options_and_registries(
            session_id,
            cwd,
            artifact_dir,
            &effective_command,
            browser_use_browser::BrowserCommandOptions::default(),
            &self.script_registry,
            &self.session_registry,
        )?;
        if !events.is_empty() {
            events.append(&mut output.events);
            output.events = events;
        }
        Ok(output)
    }

    fn run_script(
        &self,
        session_id: &str,
        cwd: &std::path::Path,
        artifact_dir: &std::path::Path,
        code: &str,
        timeout_secs: u64,
    ) -> anyhow::Result<BrowserScriptOutput> {
        let mut events = self.ensure_configured_browser(session_id, cwd, artifact_dir)?;
        let mut output = browser_use_browser::run_browser_script_with_session_registry(
            session_id,
            cwd,
            artifact_dir,
            code,
            timeout_secs,
            &self.session_registry,
        )?;
        if !events.is_empty() {
            events.append(&mut output.browser_events);
            output.browser_events = events;
        }
        Ok(output)
    }

    fn start_script(
        &self,
        session_id: &str,
        cwd: &std::path::Path,
        artifact_dir: &std::path::Path,
        code: &str,
        timeout_secs: u64,
    ) -> anyhow::Result<BrowserScriptOutput> {
        let mut events = self.ensure_configured_browser(session_id, cwd, artifact_dir)?;
        let mut output = browser_use_browser::start_browser_script_with_registries(
            session_id,
            cwd,
            artifact_dir,
            code,
            timeout_secs,
            &self.script_registry,
            &self.session_registry,
        )?;
        if !events.is_empty() {
            events.append(&mut output.browser_events);
            output.browser_events = events;
        }
        Ok(output)
    }

    fn observe_script(
        &self,
        session_id: &str,
        run_id: &str,
        observe_timeout_ms: u64,
    ) -> anyhow::Result<BrowserScriptOutput> {
        browser_use_browser::observe_browser_script_with_registry(
            session_id,
            run_id,
            observe_timeout_ms,
            &self.script_registry,
        )
    }

    fn cancel_script(&self, session_id: &str, run_id: &str) -> anyhow::Result<BrowserScriptOutput> {
        browser_use_browser::cancel_browser_script_with_registry(
            session_id,
            run_id,
            &self.script_registry,
        )
    }

    fn cleanup_session(&self, session_id: &str) -> usize {
        browser_use_browser::cleanup_session_with_registries(
            session_id,
            &self.script_registry,
            &self.session_registry,
        )
    }
}

fn dispatch_browser_preference_command_for_mode(
    store: &Store,
    backend: &dyn BrowserBackend,
    session_id: &str,
    cwd: &std::path::Path,
    artifact_dir: &std::path::Path,
    cmd: &str,
    selected_browser_mode: Option<&str>,
) -> anyhow::Result<Option<Value>> {
    let argv = browser_command_words(cmd)?;
    let args = strip_browser_prefix(&argv);
    let Some(first) = args.first().map(String::as_str) else {
        return Ok(None);
    };
    match first {
        "preference" | "preferences" => Ok(Some(dispatch_browser_preference(
            store,
            &args,
            selected_browser_mode,
        )?)),
        "profile" | "profiles"
            if args.get(1).is_some_and(|arg| {
                matches!(
                    arg.as_str(),
                    "suggest" | "use" | "remember" | "forget" | "current"
                )
            }) =>
        {
            Ok(Some(dispatch_browser_profile_preference(
                store,
                backend,
                session_id,
                cwd,
                artifact_dir,
                &args,
                selected_browser_mode,
            )?))
        }
        "secrets" | "secret" => Ok(Some(dispatch_secrets_command(store, &args)?)),
        "domains" | "domain" => Ok(Some(dispatch_domains_command(store, &args)?)),
        _ => Ok(None),
    }
}

/// Model-facing `browser secrets …`. Read-only: the agent can discover which
/// placeholders exist (so it can call `secret("name")`), but setting/removing a
/// secret carries a value and must be done by the human via the CLI/TUI so the
/// value never enters the model context.
fn dispatch_secrets_command(store: &Store, args: &[String]) -> anyhow::Result<Value> {
    use super::secrets_admin as sa;
    match args.get(1).map(String::as_str) {
        None | Some("list") | Some("--json") | Some("show") => {
            let secrets = sa::list_secrets(store)?
                .into_iter()
                .map(|meta| {
                    json!({
                        "domain": meta.domain,
                        "name": meta.placeholder,
                        "kind": meta.kind.as_str(),
                        "allowed_domains": meta.allowed_domains,
                    })
                })
                .collect::<Vec<_>>();
            Ok(json!({ "status": "ok", "secrets": secrets }))
        }
        Some("set" | "add" | "remove" | "rm" | "delete") => bail!(
            "Secrets must be set or removed by the user so the value never enters the agent's \
             context. Ask the user to run `browser-use-terminal secrets set --domain <domain> \
             --name <name>` (add `--totp` for 2FA), or `secrets remove --domain <domain> \
             --name <name>`."
        ),
        Some(other) => bail!("unknown browser secrets command: {other}"),
    }
}

/// Model-facing `browser domains …`. Read-only for the same reason the
/// navigation guard exists: the agent must not be able to widen its own policy.
fn dispatch_domains_command(store: &Store, args: &[String]) -> anyhow::Result<Value> {
    use super::secrets_admin as sa;
    match args.get(1).map(String::as_str) {
        None | Some("list") | Some("--json") | Some("show") => {
            let (allowed, denied) = sa::list_domains(store)?;
            Ok(json!({ "status": "ok", "allowed": allowed, "denied": denied }))
        }
        Some("allow" | "deny" | "clear") => bail!(
            "The navigation allow/deny policy must be changed by the user. Ask them to run \
             `browser-use-terminal domains allow <domain>` or `domains deny <domain>`."
        ),
        Some(other) => bail!("unknown browser domains command: {other}"),
    }
}

fn dispatch_browser_preference(
    store: &Store,
    args: &[String],
    selected_browser_mode: Option<&str>,
) -> anyhow::Result<Value> {
    match args.get(1).map(String::as_str) {
        None | Some("--json") | Some("show") => {
            browser_preference_json(store, selected_browser_mode)
        }
        Some("use") => {
            let mode = args.get(2).map(String::as_str).ok_or_else(|| {
                anyhow!("browser preference use requires <local|cloud|managed-headless|remote-cdp>")
            })?;
            let normalized = normalize_browser_preference_mode(mode)?;
            enforce_selected_browser_mode(selected_browser_mode, normalized)?;
            store.set_setting(BROWSER_PREF_MODE, normalized)?;
            store.set_setting("browser", browser_display_name(normalized))?;
            Ok(json!({
                "status": "ok",
                "preference": browser_preference_json(store, selected_browser_mode)?,
                "next_step": "browser connect",
            }))
        }
        Some(other) => bail!("unknown browser preference command: {other}"),
    }
}

fn dispatch_browser_profile_preference(
    store: &Store,
    backend: &dyn BrowserBackend,
    session_id: &str,
    cwd: &std::path::Path,
    artifact_dir: &std::path::Path,
    args: &[String],
    selected_browser_mode: Option<&str>,
) -> anyhow::Result<Value> {
    match args.get(1).map(String::as_str) {
        Some("current") => browser_preference_json(store, selected_browser_mode),
        Some("use") => {
            enforce_selected_browser_mode(selected_browser_mode, "local")?;
            let profile_id = args
                .get(2..)
                .filter(|parts| !parts.is_empty())
                .map(|parts| parts.join(" "))
                .ok_or_else(|| anyhow!("browser profile use requires <profile-id>"))?;
            let profile_label =
                local_profile_label_for_id(backend, session_id, cwd, artifact_dir, &profile_id);
            store.set_setting(BROWSER_PREF_PROFILE, &profile_id)?;
            store.set_setting(BROWSER_PREF_PROFILE_LABEL, &profile_label)?;
            store.set_setting(BROWSER_PREF_MODE, "local")?;
            store.set_setting("browser", browser_display_name("local"))?;
            Ok(json!({
                "status": "ok",
                "mode": "local",
                "profile_id": profile_id,
                "profile_label": profile_label,
                "default_profile_id": profile_id,
                "message": format!("Default Chrome profile set to {profile_label}. You can change it anytime with /profile."),
                "next_step": "browser connect local",
            }))
        }
        Some("remember") => {
            let profile_id = option_value_core(args, "--profile")
                .or_else(|| args.get(2).cloned())
                .ok_or_else(|| {
                    anyhow!("browser profile remember requires --profile <profile-id>")
                })?;
            let mode = option_value_core(args, "--mode")
                .map(|mode| normalize_browser_preference_mode(&mode).map(ToOwned::to_owned))
                .transpose()?
                .or_else(|| {
                    selected_browser_mode
                        .and_then(|mode| normalize_browser_preference_mode(mode).ok())
                        .map(ToOwned::to_owned)
                })
                .or_else(|| store.get_setting(BROWSER_PREF_MODE).ok().flatten())
                .unwrap_or_else(|| "local".to_string());
            enforce_selected_browser_mode(selected_browser_mode, &mode)?;
            let normalized_mode = normalize_browser_preference_mode(&mode)?;
            let profile_label = if normalized_mode == "cloud" {
                cloud_profile_label_for_id(backend, session_id, cwd, artifact_dir, &profile_id)
            } else {
                local_profile_label_for_id(backend, session_id, cwd, artifact_dir, &profile_id)
            };
            store.set_setting(BROWSER_PREF_PROFILE, &profile_id)?;
            store.set_setting(BROWSER_PREF_PROFILE_LABEL, &profile_label)?;
            store.set_setting(BROWSER_PREF_MODE, normalized_mode)?;
            store.set_setting("browser", browser_display_name(normalized_mode))?;
            let next_step = match normalized_mode {
                "cloud" => format!(
                    "browser remote start --profile-id {}",
                    shell_quote_browser_arg(&profile_id)
                ),
                "managed-headless" => "browser connect managed --headless".to_string(),
                "managed-headed" => "browser connect managed --headed".to_string(),
                _ => "browser connect local".to_string(),
            };
            let message = if normalized_mode == "cloud" {
                format!(
                    "Default Browser Use Cloud profile set to {profile_label}. You can change it anytime with /profile."
                )
            } else {
                format!(
                    "Default Chrome profile set to {profile_label}. You can change it anytime with /profile."
                )
            };
            Ok(json!({
                "status": "ok",
                "mode": normalized_mode,
                "profile_id": profile_id,
                "profile_label": profile_label,
                "default_profile_id": profile_id,
                "message": message,
                "next_step": next_step,
            }))
        }
        Some("forget") => {
            store.delete_setting(BROWSER_PREF_PROFILE)?;
            store.delete_setting(BROWSER_PREF_PROFILE_LABEL)?;
            Ok(json!({ "status": "ok", "default_profile_id": null }))
        }
        Some("suggest") => {
            let mode = effective_browser_mode(Some(store), selected_browser_mode)?;
            if mode == "cloud" {
                let profiles = backend
                    .command(
                        session_id,
                        cwd,
                        artifact_dir,
                        "browser remote profiles --json",
                    )
                    .map(|output| output.content)
                    .unwrap_or_else(|error| {
                        json!({
                            "status": "failed",
                            "error": format!("{error:#}"),
                            "profiles": [],
                        })
                    });
                let default_profile_id = stored_profile_for_mode(store, "cloud")?;
                let cloud_profiles = profiles
                    .get("profiles")
                    .cloned()
                    .unwrap_or_else(|| json!([]));
                let domain = option_value_core(args, "--domain");
                let matching_profiles =
                    cloud_profiles_matching_domain(&cloud_profiles, domain.as_deref());
                let compact_cloud_profiles =
                    compact_cloud_profiles_for_suggestion(&cloud_profiles, domain.as_deref());
                let compact_matching_profiles = compact_cloud_profiles_for_suggestion(
                    &serde_json::Value::Array(matching_profiles),
                    domain.as_deref(),
                );
                let profile_options =
                    formatted_cloud_profile_options(&cloud_profiles, domain.as_deref());
                let profile_choices =
                    formatted_cloud_profile_choices(&cloud_profiles, domain.as_deref());
                let user_prompt = cloud_profile_user_prompt(&cloud_profiles, domain.as_deref());
                return Ok(json!({
                    "status": "ok",
                    "mode": "cloud",
                    "default_profile_id": default_profile_id,
                    "cloud_profiles": compact_cloud_profiles,
                    "matching_profiles": compact_matching_profiles,
                    "profile_options": profile_options,
                    "profile_choices": profile_choices,
                    "user_prompt": user_prompt,
                    "next_step": "For login-sensitive cloud browser work, choose a matching cloud profile and run browser profile remember --mode cloud --profile <profile-id>, then browser connect. If default_profile_id is already set, plain browser connect will use it.",
                }));
            }
            enforce_selected_browser_mode(selected_browser_mode, "local")?;
            let profiles = backend
                .command(
                    session_id,
                    cwd,
                    artifact_dir,
                    "browser local profiles --json",
                )
                .map(|output| output.content)
                .unwrap_or_else(|error| {
                    json!({
                        "status": "failed",
                        "error": format!("{error:#}"),
                        "profiles": [],
                    })
                });
            let default_profile_id = stored_profile_for_mode(store, "local")?;
            let preferred_browser = store.get_setting(BROWSER_PREF_BROWSER)?;
            let local_profiles = profiles
                .get("local_profiles")
                .or_else(|| profiles.get("profiles"))
                .cloned()
                .unwrap_or_else(|| json!([]));
            let local_profiles =
                filter_local_profiles_for_browser(local_profiles, preferred_browser.as_deref());
            Ok(json!({
                "status": "ok",
                "default_profile_id": default_profile_id,
                "preferred_browser": preferred_browser,
                "local_profiles": local_profiles,
                "profile_options": formatted_profile_options(&local_profiles),
                "profile_choices": formatted_profile_choices(&local_profiles),
                "user_prompt": default_profile_user_prompt(&local_profiles),
                "next_step": "If default_profile_id is null, show user_prompt exactly and wait for the user's choice. Then run browser profile use <profile-id> before browser connect local.",
            }))
        }
        Some(other) => bail!("unknown browser profile command: {other}"),
        None => browser_preference_json(store, selected_browser_mode),
    }
}

fn resolve_browser_command_for_selected_mode(
    store: Option<&Store>,
    cmd: &str,
    selected_browser_mode: Option<&str>,
    selected_profile_id: Option<&str>,
) -> anyhow::Result<String> {
    let argv = browser_command_words(cmd)?;
    let args = strip_browser_prefix(&argv);
    if args.len() == 1 && args.first().is_some_and(|arg| arg == "connect") {
        let effective_mode = effective_browser_mode(store, selected_browser_mode)?;
        let profile_id = if effective_mode == "cloud" {
            match selected_profile_id
                .map(str::trim)
                .filter(|value| !value.is_empty())
            {
                Some(profile_id) => Some(profile_id.to_string()),
                None => store
                    .map(|store| stored_profile_for_mode(store, effective_mode))
                    .transpose()?
                    .flatten(),
            }
        } else if selected_browser_mode.is_none() {
            store
                .map(|store| stored_profile_for_mode(store, effective_mode))
                .transpose()?
                .flatten()
        } else {
            None
        };
        browser_connect_command_for_mode(effective_mode, profile_id.as_deref())
    } else {
        if let Some(command) =
            remote_cdp_compatibility_connect_command(&args, selected_browser_mode)?
        {
            return Ok(command);
        }
        enforce_browser_command_matches_selected_mode(&args, selected_browser_mode)?;
        Ok(cmd.to_string())
    }
}

fn remote_cdp_compatibility_connect_command(
    args: &[String],
    selected_browser_mode: Option<&str>,
) -> anyhow::Result<Option<String>> {
    let Some(selected_mode) = selected_browser_mode
        .map(str::trim)
        .filter(|value| !value.is_empty())
    else {
        return Ok(None);
    };
    if normalize_browser_preference_mode(selected_mode)? != "remote-cdp" {
        return Ok(None);
    }
    let requests_different_browser_setup = match args {
        [command, mode, ..] if command == "connect" => mode != "remote-cdp",
        [command, ..] if command == "local" => true,
        [command, action, ..] if command == "remote" && action == "start" => true,
        _ => false,
    };
    if requests_different_browser_setup {
        return Ok(Some(remote_cdp_connect_command()?));
    }
    Ok(None)
}

fn local_connect_default_profile_preflight(
    has_default_profile: bool,
    preferred_browser: Option<&str>,
    backend: &dyn BrowserBackend,
    session_id: &str,
    cwd: &std::path::Path,
    artifact_dir: &std::path::Path,
    resolved_command: &str,
) -> anyhow::Result<Option<BrowserCommandOutput>> {
    if !is_plain_local_connect_command(resolved_command) || has_default_profile {
        return Ok(None);
    }
    let profiles = match backend.command(
        session_id,
        cwd,
        artifact_dir,
        "browser local profiles --json",
    ) {
        Ok(output) => output.content,
        Err(error) => {
            return Ok(Some(local_profile_discovery_failed_preflight(format!(
                "{error:#}"
            ))));
        }
    };
    if profiles
        .get("status")
        .and_then(serde_json::Value::as_str)
        .is_some_and(|status| status == "failed")
    {
        let error = profiles
            .get("error")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("profile discovery failed")
            .to_string();
        return Ok(Some(local_profile_discovery_failed_preflight(error)));
    }
    let local_profiles = profiles
        .get("local_profiles")
        .or_else(|| profiles.get("profiles"))
        .cloned()
        .unwrap_or_else(|| json!([]));
    let local_profiles = filter_local_profiles_for_browser(local_profiles, preferred_browser);
    Ok(Some(BrowserCommandOutput {
        content: json!({
            "status": "needs-user-action",
            "reason": "No default local Chrome profile is set.",
            "preferred_browser": preferred_browser,
            "local_profiles": local_profiles,
            "profile_options": formatted_profile_options(&local_profiles),
            "profile_choices": formatted_profile_choices(&local_profiles),
            "user_prompt": default_profile_user_prompt(&local_profiles),
            "next_step": "Show user_prompt exactly and wait for the user's choice. Then run browser profile use <profile-id> and retry browser connect local.",
        }),
        events: Vec::new(),
    }))
}

fn local_profile_discovery_failed_preflight(error: String) -> BrowserCommandOutput {
    BrowserCommandOutput {
        content: json!({
            "status": "blocked",
            "state": "profile-discovery-failed",
            "reason": "No default local Chrome profile is set, and Chrome profile discovery failed.",
            "error": error,
            "browser_task_blocked": true,
            "next_step": "Report that local Chrome profile discovery failed. Do not run browser connect local without a selected default profile.",
            "model_instruction": "Browser work is blocked. Do not answer the user's browser/search/page task from memory. Do not connect to Local Chrome without a selected default profile; report the profile discovery failure and ask the user to fix profile discovery or set a default profile with /profile.",
        }),
        events: Vec::new(),
    }
}

pub(crate) fn enrich_local_connect_recovery_with_default_profile(
    mut output: BrowserCommandOutput,
    resolved_command: &str,
    default_profile_id: Option<&str>,
) -> BrowserCommandOutput {
    if !is_plain_local_connect_command(resolved_command) {
        return output;
    }
    let Some(profile_id) = default_profile_id
        .map(str::trim)
        .filter(|profile_id| !profile_id.is_empty())
    else {
        return output;
    };
    let Some(content) = output.content.as_object_mut() else {
        return output;
    };
    let status = content
        .get("status")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    if status != "blocked" {
        return output;
    }
    let state = content
        .get("state")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    let quoted_profile = shell_quote_browser_arg(profile_id);
    match state {
        "stale-port" | "browser-not-running" => {
            content.insert("default_profile_id".to_string(), json!(profile_id));
            content.insert(
                "profile_recovery_command".to_string(),
                json!(format!("browser local open --profile {quoted_profile}")),
            );
            content.insert(
                "next_step".to_string(),
                json!(format!(
                    "Run `browser local open --profile {quoted_profile}`, give Chrome a moment to start, then run `browser connect local`."
                )),
            );
        }
        "permission-blocked" => {
            content.insert("default_profile_id".to_string(), json!(profile_id));
            let focus_command =
                format!("browser local open --profile {quoted_profile} --no-marker");
            content.insert(
                "profile_recovery_command".to_string(),
                json!(focus_command.clone()),
            );
            content.insert(
                "next_step".to_string(),
                json!(format!(
                    "Run `{focus_command}` to open/focus the selected Chrome profile without creating a marker tab, then immediately run `browser connect local` again to trigger Chrome's 'Allow remote debugging?' popup. If that fresh connect still returns permission-blocked, ask the user to click Allow and reply when done."
                )),
            );
        }
        "profile-target-missing" => {
            content.insert("default_profile_id".to_string(), json!(profile_id));
            content.insert(
                "profile_recovery_command".to_string(),
                json!(format!("browser local open --profile {quoted_profile}")),
            );
            content.insert(
                "next_step".to_string(),
                json!(format!(
                    "Do not ask the user to open a Chrome profile manually. Run `browser local open --profile {quoted_profile}`, give Chrome a moment, then run `browser connect local` again. If this repeats, report that Chrome ignored the selected profile launch."
                )),
            );
        }
        _ => {}
    }
    output
}

fn enforce_local_connect_default_profile_context(
    output: BrowserCommandOutput,
    resolved_command: &str,
    default_profile_id: Option<&str>,
) -> BrowserCommandOutput {
    if !is_plain_local_connect_command(resolved_command) {
        return output;
    }
    let Some(profile_id) = default_profile_id
        .map(str::trim)
        .filter(|profile_id| !profile_id.is_empty())
    else {
        return output;
    };
    if output.content.get("status").and_then(Value::as_str) != Some("connected") {
        return output;
    }
    if browser_status_local_profile_id(&output.content) == Some(profile_id) {
        return output;
    }

    let candidate = output
        .content
        .get("candidate")
        .or_else(|| output.content.pointer("/browser/candidate"))
        .cloned()
        .unwrap_or(Value::Null);
    let quoted_profile = shell_quote_browser_arg(profile_id);
    BrowserCommandOutput {
        content: json!({
            "status": "blocked",
            "state": "profile-target-missing",
            "reason": "Local Chrome connected, but not to the selected default profile target. Refusing to continue in an arbitrary Chrome profile.",
            "default_profile_id": profile_id,
            "candidate": candidate,
            "profile_recovery_command": format!("browser local open --profile {quoted_profile}"),
            "next_step": format!("Run `browser local open --profile {quoted_profile}`, give Chrome a moment, then run `browser connect local` again. Do not ask the user to pick a profile and do not use shell/open to switch profiles."),
            "browser_task_blocked": true,
            "model_instruction": "Browser work is blocked. Do not answer the user's browser/search/page task from memory and do not ask the user to choose a Chrome profile when a default_profile_id is present. Follow next_step with the browser tool, then retry browser work.",
        }),
        events: output.events,
    }
}

fn enrich_local_profiles_with_default_profile(
    mut output: BrowserCommandOutput,
    resolved_command: &str,
    default_profile_id: Option<&str>,
) -> BrowserCommandOutput {
    let Ok(words) = browser_command_words(resolved_command) else {
        return output;
    };
    let words = words.iter().map(String::as_str).collect::<Vec<_>>();
    if !matches!(
        words.as_slice(),
        ["browser", "local", "profiles", ..] | ["local", "profiles", ..]
    ) {
        return output;
    }
    let Some(profile_id) = default_profile_id
        .map(str::trim)
        .filter(|profile_id| !profile_id.is_empty())
    else {
        return output;
    };
    let Some(content) = output.content.as_object_mut() else {
        return output;
    };
    content.insert("default_profile_id".to_string(), json!(profile_id));
    content.insert(
        "model_instruction".to_string(),
        json!(
            "A default local Chrome profile is already set. Do not ask the user which profile to use for browser work. Use the default profile; only change it if the user explicitly asks to switch profiles."
        ),
    );
    output
}

fn enrich_status_with_selected_browser_mode(
    mut output: BrowserCommandOutput,
    resolved_command: &str,
    selected_mode: Option<&str>,
) -> BrowserCommandOutput {
    let Ok(words) = browser_command_words(resolved_command) else {
        return output;
    };
    let words = words.iter().map(String::as_str).collect::<Vec<_>>();
    if !matches!(words.as_slice(), ["browser", "status", ..] | ["status", ..]) {
        return output;
    }
    let Some(selected_mode) = selected_mode
        .and_then(|mode| normalize_browser_preference_mode(mode).ok())
        .filter(|mode| *mode == "cloud")
    else {
        return output;
    };
    let Some(content) = output.content.as_object_mut() else {
        return output;
    };
    if content.get("connection").and_then(Value::as_str) != Some("not-configured") {
        return output;
    }
    if content.get("mode").and_then(Value::as_str) != Some("none") {
        return output;
    }

    content.insert("selected_browser_mode".to_string(), json!(selected_mode));
    content.insert("display_status".to_string(), json!("not-started"));
    content.insert(
        "reason".to_string(),
        json!("Browser Use Cloud is selected, but no cloud browser has been started yet."),
    );
    content.insert("next_step".to_string(), json!("browser remote start"));
    output
}

fn open_default_profile_before_local_connect(
    backend: &dyn BrowserBackend,
    session_id: &str,
    cwd: &std::path::Path,
    artifact_dir: &std::path::Path,
    resolved_command: &str,
    default_profile_id: Option<&str>,
) -> anyhow::Result<()> {
    if !is_plain_local_connect_command(resolved_command) {
        return Ok(());
    }
    let Some(profile_id) = default_profile_id
        .map(str::trim)
        .filter(|profile_id| !profile_id.is_empty())
    else {
        return Ok(());
    };
    if !local_profile_targeting_is_ambiguous(backend, session_id, cwd, artifact_dir)? {
        return Ok(());
    }
    let command = format!(
        "browser local open --profile {}",
        shell_quote_browser_arg(profile_id)
    );
    backend.command(session_id, cwd, artifact_dir, &command)?;
    std::thread::sleep(std::time::Duration::from_millis(750));
    Ok(())
}

fn ensure_browser_ready_for_work(
    backend: &dyn BrowserBackend,
    session_id: &str,
    cwd: &std::path::Path,
    artifact_dir: &std::path::Path,
    desired_mode: &str,
    desired_profile_id: Option<&str>,
) -> anyhow::Result<()> {
    match normalize_browser_preference_mode(desired_mode)? {
        "local" => ensure_local_browser_ready_for_work(
            backend,
            session_id,
            cwd,
            artifact_dir,
            desired_profile_id,
        ),
        _ => Ok(()),
    }
}

fn ensure_local_browser_ready_for_work(
    backend: &dyn BrowserBackend,
    session_id: &str,
    cwd: &std::path::Path,
    artifact_dir: &std::path::Path,
    desired_profile_id: Option<&str>,
) -> anyhow::Result<()> {
    let Some(profile_id) = desired_profile_id
        .map(str::trim)
        .filter(|profile_id| !profile_id.is_empty())
    else {
        return Ok(());
    };

    let status = backend.command(session_id, cwd, artifact_dir, "browser status --json")?;
    if local_browser_ready_for_profile(&status.content, profile_id) {
        return Ok(());
    }

    if browser_status_connection(&status.content) == Some("connected") {
        if let Some(active_profile_id) = browser_status_local_profile_id(&status.content) {
            if active_profile_id != profile_id {
                open_selected_local_profile(backend, session_id, cwd, artifact_dir, profile_id)?;
                let connect =
                    backend.command(session_id, cwd, artifact_dir, "browser connect local")?;
                if local_browser_ready_for_profile(&connect.content, profile_id) {
                    return Ok(());
                }
                return Err(local_browser_not_ready_error(&connect.content));
            }
        }
    }

    open_selected_local_profile(backend, session_id, cwd, artifact_dir, profile_id)?;
    let connect = backend.command(session_id, cwd, artifact_dir, "browser connect local")?;
    if local_browser_ready_for_profile(&connect.content, profile_id) {
        return Ok(());
    }
    Err(local_browser_not_ready_error(&connect.content))
}

fn open_selected_local_profile(
    backend: &dyn BrowserBackend,
    session_id: &str,
    cwd: &std::path::Path,
    artifact_dir: &std::path::Path,
    profile_id: &str,
) -> anyhow::Result<()> {
    let command = format!(
        "browser local open --profile {}",
        shell_quote_browser_arg(profile_id)
    );
    backend.command(session_id, cwd, artifact_dir, &command)?;
    std::thread::sleep(std::time::Duration::from_millis(750));
    Ok(())
}

fn local_browser_not_ready_error(content: &serde_json::Value) -> anyhow::Error {
    let status = content
        .get("status")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("unknown");
    let state = content
        .get("state")
        .or_else(|| content.get("loss_reason"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or("unknown");
    let next_step = content
        .get("next_step")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("follow the browser tool recovery instructions");
    anyhow!(
        "Browser work is blocked; do not answer the user's browser/search/page task from memory. local Chrome is not ready for the selected default profile before browser work; connect status: {status}; state: {state}; next_step: {next_step}"
    )
}

fn local_browser_ready_for_profile(value: &serde_json::Value, profile_id: &str) -> bool {
    browser_status_connection(value) == Some("connected")
        && browser_status_target_id(value).is_some()
        && browser_status_session_id(value).is_some()
        && browser_status_local_profile_id(value) == Some(profile_id)
}

fn browser_status_local_profile_id(value: &serde_json::Value) -> Option<&str> {
    value
        .get("local_profile_id")
        .or_else(|| value.pointer("/browser/local_profile_id"))
        .and_then(serde_json::Value::as_str)
}

fn browser_status_connection(value: &serde_json::Value) -> Option<&str> {
    value
        .get("connection")
        .or_else(|| value.pointer("/browser/connection"))
        .and_then(serde_json::Value::as_str)
}

fn browser_status_target_id(value: &serde_json::Value) -> Option<&str> {
    value
        .pointer("/page/target_id")
        .or_else(|| value.pointer("/browser/page/target_id"))
        .and_then(serde_json::Value::as_str)
}

fn browser_status_session_id(value: &serde_json::Value) -> Option<&str> {
    value
        .pointer("/page/session_id")
        .or_else(|| value.pointer("/browser/page/session_id"))
        .and_then(serde_json::Value::as_str)
}

fn local_profile_targeting_is_ambiguous(
    backend: &dyn BrowserBackend,
    session_id: &str,
    cwd: &std::path::Path,
    artifact_dir: &std::path::Path,
) -> anyhow::Result<bool> {
    let profiles = backend.command(
        session_id,
        cwd,
        artifact_dir,
        "browser local profiles --json",
    )?;
    if profile_values(&profiles.content).take(2).count() <= 1 {
        return Ok(false);
    }

    let local_state =
        backend.command(session_id, cwd, artifact_dir, "browser local list --json")?;
    Ok(has_reachable_local_candidate(&local_state.content))
}

fn has_reachable_local_candidate(value: &serde_json::Value) -> bool {
    value
        .get("candidates")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .any(|candidate| {
            candidate
                .get("connectable")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false)
        })
}

fn local_profile_label_for_id(
    backend: &dyn BrowserBackend,
    session_id: &str,
    cwd: &std::path::Path,
    artifact_dir: &std::path::Path,
    profile_id: &str,
) -> String {
    backend
        .command(
            session_id,
            cwd,
            artifact_dir,
            "browser local profiles --json",
        )
        .ok()
        .and_then(|output| {
            profile_values(&output.content).find_map(|profile| {
                let id = profile.get("id").and_then(serde_json::Value::as_str)?;
                if id == profile_id {
                    Some(profile_display_label(profile))
                } else {
                    None
                }
            })
        })
        .unwrap_or_else(|| profile_id.to_string())
}

fn cloud_profile_label_for_id(
    backend: &dyn BrowserBackend,
    session_id: &str,
    cwd: &std::path::Path,
    artifact_dir: &std::path::Path,
    profile_id: &str,
) -> String {
    backend
        .command(
            session_id,
            cwd,
            artifact_dir,
            "browser remote profiles --json",
        )
        .ok()
        .and_then(|output| {
            output
                .content
                .get("profiles")
                .and_then(serde_json::Value::as_array)
                .into_iter()
                .flatten()
                .find_map(|profile| {
                    let id = profile.get("id").and_then(serde_json::Value::as_str)?;
                    if id == profile_id {
                        Some(cloud_profile_display_label(profile))
                    } else {
                        None
                    }
                })
        })
        .unwrap_or_else(|| profile_id.to_string())
}

fn formatted_profile_options(profiles: &serde_json::Value) -> Vec<String> {
    profiles
        .as_array()
        .into_iter()
        .flatten()
        .enumerate()
        .map(|(idx, profile)| format!("{}) {}", idx + 1, profile_display_label(profile)))
        .collect()
}

fn formatted_profile_choices(profiles: &serde_json::Value) -> Vec<serde_json::Value> {
    profiles
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|profile| {
            let id = profile.get("id").and_then(serde_json::Value::as_str)?;
            Some(json!({
                "label": profile_display_label(profile),
                "profile_id": id,
            }))
        })
        .collect()
}

fn filter_local_profiles_for_browser(
    profiles: serde_json::Value,
    preferred_browser: Option<&str>,
) -> serde_json::Value {
    let Some(preferred_browser) = preferred_browser
        .map(str::trim)
        .filter(|browser| !browser.is_empty())
    else {
        return profiles;
    };
    let Some(items) = profiles.as_array() else {
        return profiles;
    };
    let filtered = items
        .iter()
        .filter(|profile| {
            profile
                .get("browser_name")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|browser| browser.eq_ignore_ascii_case(preferred_browser))
        })
        .cloned()
        .collect::<Vec<_>>();
    serde_json::Value::Array(filtered)
}

fn formatted_cloud_profile_options(
    profiles: &serde_json::Value,
    domain: Option<&str>,
) -> Vec<String> {
    profiles
        .as_array()
        .into_iter()
        .flatten()
        .enumerate()
        .map(|(idx, profile)| {
            let summary = cloud_profile_domain_summary(profile, domain);
            if summary.is_empty() {
                format!("{}) {}", idx + 1, cloud_profile_display_label(profile))
            } else {
                format!(
                    "{}) {} ({summary})",
                    idx + 1,
                    cloud_profile_display_label(profile)
                )
            }
        })
        .collect()
}

fn formatted_cloud_profile_choices(
    profiles: &serde_json::Value,
    domain: Option<&str>,
) -> Vec<serde_json::Value> {
    profiles
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|profile| {
            let id = profile.get("id").and_then(serde_json::Value::as_str)?;
            Some(json!({
                "label": cloud_profile_display_label(profile),
                "profile_id": id,
                "cookie_domain_count": cloud_profile_cookie_domain_count(profile),
                "matching_cookie_domains": matching_cookie_domains_for_profile(profile, domain),
            }))
        })
        .collect()
}

fn compact_cloud_profiles_for_suggestion(
    profiles: &serde_json::Value,
    domain: Option<&str>,
) -> Vec<serde_json::Value> {
    profiles
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|profile| {
            let id = profile.get("id").and_then(serde_json::Value::as_str)?;
            Some(json!({
                "id": id,
                "name": profile.get("name").and_then(serde_json::Value::as_str),
                "lastUsedAt": profile.get("lastUsedAt").cloned().unwrap_or(serde_json::Value::Null),
                "cookie_domain_count": cloud_profile_cookie_domain_count(profile),
                "matching_cookie_domains": matching_cookie_domains_for_profile(profile, domain),
            }))
        })
        .collect()
}

fn cloud_profiles_matching_domain(
    profiles: &serde_json::Value,
    domain: Option<&str>,
) -> Vec<serde_json::Value> {
    let Some(pattern) = cookie_domain_regex(domain) else {
        return Vec::new();
    };
    profiles
        .as_array()
        .into_iter()
        .flatten()
        .filter(|profile| {
            profile
                .get("cookieDomains")
                .and_then(serde_json::Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(serde_json::Value::as_str)
                .any(|cookie_domain| cookie_domain_matches_regex(cookie_domain, &pattern))
        })
        .cloned()
        .collect()
}

fn cloud_profile_user_prompt(profiles: &serde_json::Value, domain: Option<&str>) -> String {
    let matches = cloud_profiles_matching_domain(profiles, domain);
    let listed = if matches.is_empty() {
        formatted_cloud_profile_options(profiles, domain)
    } else {
        formatted_cloud_profile_options(&serde_json::Value::Array(matches), domain)
    };
    if listed.is_empty() {
        return "No Browser Use Cloud profiles were found. Ask the user to sync local cookies with /sync-cookies, or start a clean cloud browser.".to_string();
    }
    let scope = domain
        .map(|domain| format!(" for {domain}"))
        .unwrap_or_default();
    format!(
        "Which Browser Use Cloud profile should I use{scope}?\n\n{}\n\nChoose a profile with matching cookie domains for login-sensitive work.",
        listed.join("\n")
    )
}

fn default_profile_user_prompt(profiles: &serde_json::Value) -> String {
    let options = formatted_profile_options(profiles);
    if options.is_empty() {
        return "No local Chrome profiles were found. You can change the default anytime with /profile.".to_string();
    }
    format!(
        "Which Chrome profile should I set as the default for local browser access?\n\n{}\n\nThis will be the default from now on, and you can change it anytime with /profile.",
        options.join("\n")
    )
}

fn profile_values(value: &serde_json::Value) -> impl Iterator<Item = &serde_json::Value> {
    value
        .get("local_profiles")
        .or_else(|| value.get("profiles"))
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
}

fn cloud_profile_display_label(profile: &serde_json::Value) -> String {
    profile
        .get("name")
        .and_then(serde_json::Value::as_str)
        .or_else(|| profile.get("id").and_then(serde_json::Value::as_str))
        .map(clean_profile_label)
        .unwrap_or_else(|| "Cloud profile".to_string())
}

fn cloud_profile_domain_summary(profile: &serde_json::Value, domain: Option<&str>) -> String {
    let matching = matching_cookie_domains_for_profile(profile, domain);
    if !matching.is_empty() {
        return format!("matches {}", matching.join(", "));
    }
    let count = cloud_profile_cookie_domain_count(profile);
    if count == 0 {
        String::new()
    } else {
        format!("{count} cookie domains")
    }
}

fn cloud_profile_cookie_domain_count(profile: &serde_json::Value) -> usize {
    cloud_profile_cookie_domains(profile).len()
}

fn cloud_profile_cookie_domains(profile: &serde_json::Value) -> Vec<&str> {
    profile
        .get("cookieDomains")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(serde_json::Value::as_str)
        .collect()
}

fn matching_cookie_domains_for_profile(
    profile: &serde_json::Value,
    domain: Option<&str>,
) -> Vec<String> {
    let Some(pattern) = cookie_domain_regex(domain) else {
        return Vec::new();
    };
    cloud_profile_cookie_domains(profile)
        .into_iter()
        .filter(|cookie_domain| cookie_domain_matches_regex(cookie_domain, &pattern))
        .map(ToOwned::to_owned)
        .collect()
}

fn cookie_domain_regex(pattern: Option<&str>) -> Option<Regex> {
    let pattern = pattern?.trim();
    if pattern.is_empty() {
        return None;
    }
    Regex::new(&format!("(?i){pattern}")).ok()
}

fn cookie_domain_matches_regex(cookie_domain: &str, pattern: &Regex) -> bool {
    let cookie_domain = normalize_cookie_match_domain_for_agent(cookie_domain);
    pattern.is_match(&cookie_domain)
}

fn normalize_cookie_match_domain_for_agent(value: &str) -> String {
    let trimmed = value
        .trim()
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .trim_start_matches('.')
        .trim_end_matches('/');
    trimmed
        .split('/')
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase()
}

fn profile_display_label(profile: &serde_json::Value) -> String {
    profile
        .get("profile_name")
        .and_then(serde_json::Value::as_str)
        .or_else(|| {
            profile
                .get("display_name")
                .and_then(serde_json::Value::as_str)
        })
        .or_else(|| {
            profile
                .get("profile_dir")
                .and_then(serde_json::Value::as_str)
        })
        .or_else(|| profile.get("id").and_then(serde_json::Value::as_str))
        .map(clean_profile_label)
        .unwrap_or_else(|| "Profile".to_string())
}

fn clean_profile_label(label: &str) -> String {
    label
        .trim()
        .strip_prefix("Google Chrome - ")
        .unwrap_or_else(|| label.trim())
        .to_string()
}

fn is_plain_local_connect_command(command: &str) -> bool {
    browser_command_words(command)
        .map(|argv| {
            let args = strip_browser_prefix(&argv);
            args.len() == 2
                && args.first().is_some_and(|arg| arg == "connect")
                && args.get(1).is_some_and(|arg| arg == "local")
        })
        .unwrap_or(false)
}

fn effective_browser_mode(
    store: Option<&Store>,
    selected_browser_mode: Option<&str>,
) -> anyhow::Result<&'static str> {
    if let Some(mode) = selected_browser_mode
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        return normalize_browser_preference_mode(mode);
    }
    preferred_browser_mode(store)
}

fn preferred_browser_mode(store: Option<&Store>) -> anyhow::Result<&'static str> {
    let mode = match store {
        Some(store) => store
            .get_setting(BROWSER_PREF_MODE)?
            .or_else(|| {
                store
                    .get_setting("browser")
                    .ok()
                    .flatten()
                    .and_then(|value| display_browser_to_mode(&value).map(ToOwned::to_owned))
            })
            .unwrap_or_else(|| "local".to_string()),
        None => "local".to_string(),
    };
    normalize_browser_preference_mode(&mode)
}

fn remote_cdp_connect_command() -> anyhow::Result<String> {
    if let Some(ws) = env_trimmed("BU_CDP_WS") {
        return Ok(remote_cdp_connect_command_for_endpoint(&ws));
    }
    if let Some(url) = env_trimmed("BU_CDP_URL") {
        return Ok(remote_cdp_connect_command_for_endpoint(&url));
    }
    bail!("browser mode is locked to Remote CDP, but BU_CDP_URL or BU_CDP_WS is not set")
}

fn remote_cdp_connect_command_for_endpoint(endpoint: &str) -> String {
    let flag = if endpoint.starts_with("ws://") || endpoint.starts_with("wss://") {
        "--ws"
    } else {
        "--url"
    };
    format!(
        "browser connect remote-cdp {flag} {}",
        shell_quote_browser_arg(endpoint)
    )
}

fn env_trimmed(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn stored_profile_for_mode(store: &Store, mode: &str) -> anyhow::Result<Option<String>> {
    let normalized_mode = normalize_browser_preference_mode(mode)?;
    let stored_mode = preferred_browser_mode(Some(store))?;
    if stored_mode != normalized_mode {
        return Ok(None);
    }
    Ok(store
        .get_setting(BROWSER_PREF_PROFILE)?
        .filter(|profile| !profile.trim().is_empty()))
}

fn browser_connect_command_for_mode(
    mode: &str,
    profile_id: Option<&str>,
) -> anyhow::Result<String> {
    match normalize_browser_preference_mode(mode).unwrap_or("local") {
        "cloud" => {
            let mut command = "browser remote start".to_string();
            if let Some(profile_id) = profile_id.filter(|value| !value.is_empty()) {
                command.push_str(&format!(
                    " --profile-id {}",
                    shell_quote_browser_arg(profile_id)
                ));
            }
            if let Some(country) = env_trimmed("BU_BROWSER_PROXY_COUNTRY_CODE") {
                command.push_str(&format!(
                    " --proxy-country {}",
                    shell_quote_browser_arg(&country)
                ));
            }
            Ok(command)
        }
        "managed-headless" => Ok("browser connect managed --headless".to_string()),
        "managed-headed" => Ok("browser connect managed --headed".to_string()),
        "remote-cdp" => remote_cdp_connect_command(),
        _ => Ok("browser connect local".to_string()),
    }
}

fn enforce_selected_browser_mode(
    selected_browser_mode: Option<&str>,
    requested_mode: &str,
) -> anyhow::Result<()> {
    let Some(selected_mode) = selected_browser_mode
        .map(str::trim)
        .filter(|value| !value.is_empty())
    else {
        return Ok(());
    };
    let selected_mode = normalize_browser_preference_mode(selected_mode)?;
    let requested_mode = normalize_browser_preference_mode(requested_mode)?;
    if selected_mode == requested_mode {
        return Ok(());
    }
    bail!(
        "browser mode is locked to {} for this run; change the browser selector in the terminal UI before using {}",
        browser_display_name(selected_mode),
        browser_display_name(requested_mode),
    )
}

fn enforce_browser_command_matches_selected_mode(
    args: &[String],
    selected_browser_mode: Option<&str>,
) -> anyhow::Result<()> {
    let Some(selected_mode) = selected_browser_mode
        .map(str::trim)
        .filter(|value| !value.is_empty())
    else {
        return Ok(());
    };
    let selected_mode = normalize_browser_preference_mode(selected_mode)?;
    let Some(command) = args.first().map(String::as_str) else {
        return Ok(());
    };
    match command {
        "help" | "--help" | "-h" | "status" | "doctor" | "domain" | "runtime" | "script" => Ok(()),
        "connect" => match args.get(1).map(String::as_str) {
            None => Ok(()),
            Some("local") => enforce_selected_browser_mode(Some(selected_mode), "local"),
            Some("managed") => {
                let requested_mode =
                    if has_browser_arg(args, "--headed") || has_browser_arg(args, "--headful") {
                        "managed-headed"
                    } else {
                        "managed-headless"
                    };
                enforce_selected_browser_mode(Some(selected_mode), requested_mode)
            }
            Some("remote-cdp") => enforce_selected_browser_mode(Some(selected_mode), "remote-cdp"),
            Some(other) => bail!("unknown browser connect mode: {other}"),
        },
        "local" => enforce_selected_browser_mode(Some(selected_mode), "local"),
        "remote" => enforce_selected_browser_mode(Some(selected_mode), "cloud"),
        "recover" => match args.get(1).map(String::as_str) {
            Some("restart-owned-browser") => match selected_mode {
                "managed-headless" | "managed-headed" => Ok(()),
                _ => bail!(
                    "browser mode is locked to {} for this run; restart-owned-browser only applies to managed Chromium",
                    browser_display_name(selected_mode),
                ),
            },
            Some("stop-owned-remote") => enforce_selected_browser_mode(Some(selected_mode), "cloud"),
            _ => Ok(()),
        },
        _ => Ok(()),
    }
}

fn has_browser_arg(args: &[String], flag: &str) -> bool {
    args.iter().any(|arg| arg == flag)
}

fn browser_preference_json(
    store: &Store,
    selected_browser_mode: Option<&str>,
) -> anyhow::Result<Value> {
    let selected_mode = selected_browser_mode
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(normalize_browser_preference_mode)
        .transpose()?;
    let mode = selected_mode.map(ToOwned::to_owned).unwrap_or_else(|| {
        store
            .get_setting(BROWSER_PREF_MODE)
            .ok()
            .flatten()
            .or_else(|| {
                store
                    .get_setting("browser")
                    .ok()
                    .flatten()
                    .and_then(|value| display_browser_to_mode(&value).map(ToOwned::to_owned))
            })
            .unwrap_or_else(|| "local".to_string())
    });
    let domain_profiles = store
        .list_settings()?
        .into_iter()
        .filter_map(|(key, value)| {
            key.strip_prefix(BROWSER_DOMAIN_PROFILE_PREFIX)
                .and_then(|domain| {
                    serde_json::from_str::<Value>(&value)
                        .ok()
                        .map(|value| (domain.to_string(), value))
                })
        })
        .map(|(domain, value)| json!({ "domain": domain, "preference": value }))
        .collect::<Vec<_>>();
    let normalized_mode = normalize_browser_preference_mode(&mode)?;
    let profile_id = stored_profile_for_mode(store, normalized_mode)?;
    let profile_label = if profile_id.is_some() {
        store.get_setting(BROWSER_PREF_PROFILE_LABEL)?
    } else {
        None
    };
    Ok(json!({
        "mode": normalized_mode,
        "display": browser_display_name(normalized_mode),
        "browser": store.get_setting(BROWSER_PREF_BROWSER)?,
        "browser_label": store.get_setting(BROWSER_PREF_BROWSER_LABEL)?,
        "profile_id": profile_id,
        "profile_label": profile_label,
        "domain_profiles": domain_profiles,
        "connect_command": browser_connect_command_display_for_mode(normalized_mode, profile_id.as_deref())?,
    }))
}

fn browser_connect_command_display_for_mode(
    mode: &str,
    profile_id: Option<&str>,
) -> anyhow::Result<String> {
    match normalize_browser_preference_mode(mode)? {
        "remote-cdp" => Ok("browser connect remote-cdp --url <BU_CDP_URL>".to_string()),
        _ => browser_connect_command_for_mode(mode, profile_id),
    }
}

fn browser_command_words(cmd: &str) -> anyhow::Result<Vec<String>> {
    let mut words = Vec::new();
    let mut current = String::new();
    let mut chars = cmd.chars().peekable();
    let mut quote = None;
    while let Some(ch) = chars.next() {
        match (quote, ch) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), '\\') => {
                if let Some(next) = chars.next() {
                    current.push(next);
                }
            }
            (Some(_), c) => current.push(c),
            (None, '"' | '\'') => quote = Some(ch),
            (None, c) if c.is_whitespace() => {
                if !current.is_empty() {
                    words.push(std::mem::take(&mut current));
                }
            }
            (None, '\\') => {
                if let Some(next) = chars.next() {
                    current.push(next);
                }
            }
            (None, c) => current.push(c),
        }
    }
    if quote.is_some() {
        bail!("unterminated quote in browser command");
    }
    if !current.is_empty() {
        words.push(current);
    }
    Ok(words)
}

fn strip_browser_prefix(argv: &[String]) -> Vec<String> {
    if argv.first().is_some_and(|arg| arg == "browser") {
        argv[1..].to_vec()
    } else {
        argv.to_vec()
    }
}

fn option_value_core(argv: &[String], flag: &str) -> Option<String> {
    argv.windows(2)
        .find(|window| window[0] == flag)
        .map(|window| window[1].clone())
}

fn shell_quote_browser_arg(value: &str) -> String {
    if value
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | ':' | '/' | '.'))
    {
        value.to_string()
    } else {
        format!("'{}'", value.replace('\'', "'\\''"))
    }
}

fn normalize_browser_preference_mode(mode: &str) -> anyhow::Result<&'static str> {
    let normalized = mode.to_ascii_lowercase().replace(['_', ' '], "-");
    match normalized.as_str() {
        "local" | "local-chrome" => Ok("local"),
        "cloud" | "browser-use-cloud" | "remote-cloud" => Ok("cloud"),
        "remote-cdp" | "cdp" => Ok("remote-cdp"),
        "headless" | "headless-chromium" | "managed-headless" => Ok("managed-headless"),
        "managed" | "managed-headed" | "headed" => Ok("managed-headed"),
        other => bail!("unknown browser preference mode: {other}"),
    }
}

fn browser_display_name(mode: &str) -> &'static str {
    match mode {
        "cloud" => "Browser Use Cloud",
        "remote-cdp" => "Remote CDP",
        "managed-headless" => "Headless Chromium",
        "managed-headed" => "Managed Chromium",
        _ => "Local Chrome",
    }
}

fn display_browser_to_mode(display: &str) -> Option<&'static str> {
    match display {
        "Browser Use Cloud" | "Browser Use cloud" => Some("cloud"),
        "Remote CDP" => Some("remote-cdp"),
        "Headless Chromium" => Some("managed-headless"),
        "Managed Chromium" => Some("managed-headed"),
        "Local Chrome" => Some("local"),
        _ => None,
    }
}

/// Map a one-shot [`BrowserCommandOutput`] into [`ExecOutput`].
///
/// The command runtime returns a structured `content` JSON plus an `events`
/// list. We serialize `content` onto stdout (the model-facing payload) and the
/// events list onto stderr, with `exit_code = 0` (a failed command surfaces its
/// failure inside `content`; the wrapped fn errors are handled separately).
fn map_command_output(mut out: BrowserCommandOutput) -> ExecOutput {
    add_browser_blocked_model_instruction(&mut out.content);
    let stdout = match serde_json::to_string(&out.content) {
        Ok(s) => s,
        Err(e) => format!("<unserializable browser content: {e}>"),
    };
    let stderr = if out.events.is_empty() {
        String::new()
    } else {
        serde_json::to_string(&out.events).unwrap_or_default()
    };
    ExecOutput {
        exit_code: 0,
        stdout,
        stderr,
    }
}

fn add_browser_blocked_model_instruction(content: &mut Value) {
    let connection = content
        .get("connection")
        .or_else(|| content.pointer("/browser/connection"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let Some(object) = content.as_object_mut() else {
        return;
    };
    let status = object
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let state = object
        .get("state")
        .or_else(|| object.get("loss_reason"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    let blocked = matches!(status, "blocked" | "needs-user-action")
        || matches!(connection.as_str(), "disconnected" | "not-configured")
        || matches!(
            state,
            "cdp-disabled"
                | "permission-blocked"
                | "stale-port"
                | "browser-not-running"
                | "profile-target-missing"
        );
    if !blocked {
        return;
    }
    object.insert("browser_task_blocked".to_string(), json!(true));
    object.insert(
        "model_instruction".to_string(),
        json!(
            "Browser work is blocked. Do not answer the user's browser/search/page task from memory or cached knowledge. Follow next_step/tool instructions, or ask the user for the required Chrome action, then retry browser work."
        ),
    );
}

/// Map a [`BrowserScriptOutput`] into [`ExecOutput`], using the same
/// model-facing text contract as main's `browser_script` dispatcher.
fn map_script_output(out: BrowserScriptOutput) -> ExecOutput {
    let exit_code = if out.ok { 0 } else { 1 };
    let stderr = if out.ok {
        String::new()
    } else {
        out.error.clone().unwrap_or_default()
    };
    ExecOutput {
        exit_code,
        stdout: browser_script_stdout(&out),
        stderr,
    }
}

fn browser_script_stdout(response: &BrowserScriptOutput) -> String {
    let text = browser_script_tool_message_content(response);
    let (image_parts, warnings) = browser_script_image_parts(response);
    let text =
        cap_inline_browser_script_stdout(append_browser_script_image_warnings(text, &warnings));
    let Some(payload) = browser_script_content_payload(&text, image_parts) else {
        return text;
    };
    format!("{text}{BROWSER_SCRIPT_CONTENT_STDOUT_PREFIX}{payload}")
}

fn cap_inline_browser_script_stdout(text: String) -> String {
    if text.len() <= MAX_INLINE_BROWSER_SCRIPT_STDOUT_BYTES {
        return text;
    }
    let mut end = MAX_INLINE_BROWSER_SCRIPT_STDOUT_BYTES;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    let elided = text.len() - end;
    let mut out = text[..end].to_string();
    out.push_str(&format!(
        "\n... [browser_script stdout truncated, {elided} more bytes; full output persisted to the run artifact. Read the saved artifact or re-extract the missing portion before relying on this output.]"
    ));
    out
}

fn browser_script_content_payload(text: &str, image_parts: Vec<ContentPart>) -> Option<String> {
    if image_parts.is_empty() {
        return None;
    }
    let mut parts = Vec::new();
    if !text.is_empty() {
        parts.push(ContentPart::text(text.to_string()));
    }
    parts.extend(image_parts);
    serde_json::to_string(&parts).ok()
}

fn browser_script_image_parts(response: &BrowserScriptOutput) -> (Vec<ContentPart>, Vec<String>) {
    let mut parts = Vec::new();
    let mut warnings = Vec::new();
    for image in &response.images {
        match browser_script_image_part(image) {
            Ok(Some(media)) => parts.push(media),
            Ok(None) => {}
            Err(warning) => warnings.push(warning),
        }
    }
    (parts, warnings)
}

fn append_browser_script_image_warnings(mut text: String, warnings: &[String]) -> String {
    for warning in warnings {
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str(warning);
    }
    text
}

fn browser_script_image_part(image: &Value) -> Result<Option<ContentPart>, String> {
    let Some(path) = image.get("path").and_then(Value::as_str) else {
        return Ok(None);
    };
    let bytes = fs::read(path)
        .map_err(|error| format!("Warning: image artifact could not be read: {path} ({error})"))?;
    let mime_type = image
        .get("mime_type")
        .or_else(|| image.get("mime"))
        .and_then(Value::as_str)
        .unwrap_or("image/png");
    if !mime_type.starts_with("image/") {
        return Ok(None);
    }
    if let Some((width, height)) = png_dimensions(&bytes) {
        if width > BROWSER_SCRIPT_MAX_IMAGE_DIMENSION || height > BROWSER_SCRIPT_MAX_IMAGE_DIMENSION
        {
            return Err(format!(
                "Warning: image artifact was not attached because its dimensions {width}x{height} exceed provider limit; artifact remains at {path}"
            ));
        }
    }
    Ok(Some(ContentPart::Media {
        mime_type: mime_type.to_string(),
        data: Some(general_purpose::STANDARD.encode(bytes)),
        url: None,
        detail: None,
    }))
}

fn png_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    if bytes.len() < 24 || &bytes[..8] != b"\x89PNG\r\n\x1a\n" {
        return None;
    }
    let width = u32::from_be_bytes(bytes.get(16..20)?.try_into().ok()?);
    let height = u32::from_be_bytes(bytes.get(20..24)?.try_into().ok()?);
    Some((width, height))
}

fn browser_script_tool_message_content(response: &BrowserScriptOutput) -> String {
    if response.status.as_deref() == Some("running") {
        return browser_script_running_message(response);
    }
    if response.status.as_deref() == Some("cancelled") {
        return browser_script_cancelled_message(response);
    }
    if response.ok {
        let mut parts = Vec::new();
        if !response.text.trim().is_empty() {
            parts.push(response.text.trim().to_string());
        }
        parts.extend(browser_script_structured_message_parts(response));
        if parts.is_empty() {
            "browser_script completed".to_string()
        } else {
            parts.join("\n")
        }
    } else {
        browser_script_failure_message(response)
    }
}

fn browser_script_running_message(response: &BrowserScriptOutput) -> String {
    let mut parts = Vec::new();
    if !response.text.trim().is_empty() {
        parts.push(response.text.trim().to_string());
    } else {
        parts.push("browser_script is still running.".to_string());
    }
    if let Some(run_id) = response.run_id.as_deref() {
        if !parts
            .iter()
            .any(|part| part.contains(&format!("run_id: {run_id}")))
        {
            parts.push(format!("run_id: {run_id}"));
        }
        parts.push(format!(
            "Next step: call browser_script with action=\"observe\", run_id=\"{run_id}\", and observe_timeout_ms={}.",
            response.next_observe_ms.unwrap_or(DEFAULT_OBSERVE_TIMEOUT_MS)
        ));
    }
    parts.extend(browser_script_structured_message_parts(response));
    parts.join("\n")
}

fn browser_script_cancelled_message(response: &BrowserScriptOutput) -> String {
    let mut parts = Vec::new();
    if response.text.trim().is_empty() {
        parts.push("browser_script cancelled.".to_string());
    } else {
        parts.push(response.text.trim().to_string());
    }
    parts.extend(browser_script_structured_message_parts(response));
    parts.join("\n")
}

fn browser_script_failure_message(response: &BrowserScriptOutput) -> String {
    let mut parts = vec!["browser_script failed.".to_string()];
    if let Some(diagnosis) = response.diagnosis.as_ref() {
        parts.push(diagnosis.summary.clone());
        parts.push(format!("What happened: {}", diagnosis.what_happened));
        parts.push(format!("Next step: {}", diagnosis.next_step));
    }
    if let Some(error) = response.error.as_deref() {
        let detail = browser_script_error_detail(error);
        if !detail.is_empty() {
            parts.push(format!("Details: {detail}"));
        }
    } else if response.diagnosis.is_none() {
        parts.push("Details: unknown browser_script error".to_string());
    }
    parts.extend(browser_script_structured_message_parts(response));
    parts.join("\n")
}

fn browser_script_structured_message_parts(response: &BrowserScriptOutput) -> Vec<String> {
    let mut parts = Vec::new();
    if !response.summary.is_empty() {
        parts.push(format!(
            "summary: {}",
            Value::Array(response.summary.clone())
        ));
    }
    if !response.data.is_null() && response.data != serde_json::json!({}) {
        parts.push(format!("data: {}", response.data));
    }
    if !response.outputs.is_empty() {
        parts.push(format!(
            "outputs: {}",
            Value::Array(response.outputs.clone())
        ));
    }
    parts
}

fn browser_script_error_detail(error: &str) -> String {
    const MAX_DETAIL_CHARS: usize = 500;
    let detail = error
        .lines()
        .rev()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or(error.trim());
    if detail.chars().count() <= MAX_DETAIL_CHARS {
        return detail.to_string();
    }
    let mut out = detail
        .chars()
        .take(MAX_DETAIL_CHARS.saturating_sub(3))
        .collect::<String>();
    out.push_str("...");
    out
}

/// Browser tool handler.
///
/// Generic over the backend so production code uses [`RealBackend`] and tests
/// inject a fake. Construct with [`BrowserTool::new`] for the real backend or
/// [`BrowserTool::with_backend`] for a custom one.
#[derive(Clone)]
pub struct BrowserTool {
    backend: Arc<dyn BrowserBackend>,
    real_backend_mode: Option<Arc<Mutex<Option<String>>>>,
    selected_browser_mode: Option<String>,
    dynamic_browser_mode_from_store: bool,
    selected_browser_profile_id: Option<String>,
    selected_local_browser: Option<String>,
    default_script_timeout_secs: u64,
    session_id_fallback: Option<String>,
    persistence: Option<BrowserPersistence>,
}

#[derive(Clone)]
struct BrowserPersistence {
    store: SharedStore,
    session_id: String,
}

impl Default for BrowserTool {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for BrowserTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BrowserTool").finish_non_exhaustive()
    }
}

impl BrowserTool {
    /// Construct a browser tool backed by the real `browser-use-browser`
    /// runtime.
    pub fn new() -> Self {
        Self {
            backend: Arc::new(RealBackend::default()),
            real_backend_mode: None,
            selected_browser_mode: None,
            dynamic_browser_mode_from_store: false,
            selected_browser_profile_id: None,
            selected_local_browser: None,
            default_script_timeout_secs: DEFAULT_BROWSER_SCRIPT_TIMEOUT_SECS,
            session_id_fallback: None,
            persistence: None,
        }
    }

    /// Construct a real browser tool with the run's configured browser mode.
    pub fn with_browser_mode(browser_mode: Option<String>) -> Self {
        let real_backend_mode = Arc::new(Mutex::new(browser_mode.clone()));
        Self {
            backend: Arc::new(RealBackend::with_shared_browser_mode(Arc::clone(
                &real_backend_mode,
            ))),
            real_backend_mode: Some(real_backend_mode),
            selected_browser_mode: browser_mode,
            dynamic_browser_mode_from_store: false,
            selected_browser_profile_id: None,
            selected_local_browser: None,
            default_script_timeout_secs: DEFAULT_BROWSER_SCRIPT_TIMEOUT_SECS,
            session_id_fallback: None,
            persistence: None,
        }
    }

    /// Construct a browser tool with a custom backend (used by tests).
    pub fn with_backend(backend: Arc<dyn BrowserBackend>) -> Self {
        Self {
            backend,
            real_backend_mode: None,
            selected_browser_mode: None,
            dynamic_browser_mode_from_store: false,
            selected_browser_profile_id: None,
            selected_local_browser: None,
            default_script_timeout_secs: DEFAULT_BROWSER_SCRIPT_TIMEOUT_SECS,
            session_id_fallback: None,
            persistence: None,
        }
    }

    /// Override the selected browser mode while keeping the existing backend.
    ///
    /// Tests use this with a fake backend; production normally uses
    /// [`BrowserTool::with_browser_mode`] so the backend auto-connection behavior
    /// receives the same mode.
    pub fn with_selected_browser_mode(mut self, browser_mode: Option<String>) -> Self {
        self.selected_browser_mode = browser_mode;
        self
    }

    pub fn with_dynamic_browser_mode_from_store(mut self, dynamic: bool) -> Self {
        self.dynamic_browser_mode_from_store = dynamic;
        if dynamic {
            self.selected_browser_mode = None;
        }
        self
    }

    pub fn with_selected_browser_profile_id(mut self, profile_id: Option<String>) -> Self {
        self.selected_browser_profile_id = profile_id
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty());
        self
    }

    pub fn with_selected_local_browser(mut self, browser: Option<String>) -> Self {
        self.selected_local_browser = browser
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty());
        self
    }

    /// Configure the default browser_script timeout used when the model omits
    /// the hidden compatibility `timeout_secs` argument.
    pub fn with_default_script_timeout_secs(mut self, timeout_secs: u64) -> Self {
        self.default_script_timeout_secs = timeout_secs;
        self
    }

    /// Configure the browser tool with the live agent session id. The model can
    /// omit `session_id`; the runtime supplies it while keeping `ToolCtx.call_id`
    /// available for the actual model tool-call id.
    pub fn with_session_id(mut self, session_id: impl Into<String>) -> Self {
        self.session_id_fallback = Some(session_id.into());
        self
    }

    /// Configure durable event persistence for rich browser outputs.
    pub fn with_persistence(mut self, store: SharedStore, session_id: impl Into<String>) -> Self {
        self.persistence = Some(BrowserPersistence {
            store,
            session_id: session_id.into(),
        });
        self
    }

    fn fallback_session_id<'a>(&'a self, ctx: &'a ToolCtx) -> &'a str {
        self.session_id_fallback
            .as_deref()
            .unwrap_or_else(|| ctx.call_id.trim())
    }
}

/// Approval key: the session + action identify a browser call for session
/// caching, mirroring the shape the other handlers use. The browser tool needs
/// no approval by default (see [`Approvable::exec_approval_requirement`]).
#[derive(serde::Serialize, Clone, Debug, Eq, PartialEq, Hash)]
pub struct BrowserApprovalKey {
    session_id: String,
    action: String,
}

impl Approvable<BrowserRequest> for BrowserTool {
    type ApprovalKey = BrowserApprovalKey;

    fn approval_keys(&self, req: &BrowserRequest) -> Vec<Self::ApprovalKey> {
        let action = match &req.action {
            BrowserAction::Command { .. } => "command",
            BrowserAction::Execute { .. } => "execute",
            BrowserAction::Observe { .. } => "observe",
            BrowserAction::Cancel { .. } => "cancel",
        };
        vec![BrowserApprovalKey {
            session_id: req.session_id.clone(),
            action: action.to_string(),
        }]
    }

    /// The browser runtime manages its own session; request the default sandbox
    /// permissions (no escalation).
    fn sandbox_permissions(&self, _req: &BrowserRequest) -> SandboxPermissions {
        SandboxPermissions::UseDefault
    }

    // `exec_approval_requirement` left at its trait default (`None`): the
    // browser tool requires no approval by default, mirroring the legacy
    // browser_* tools. The orchestrator applies the policy default, which yields
    // `Skip` under any non-prompting policy.
    fn exec_approval_requirement(&self, _req: &BrowserRequest) -> Option<ExecApprovalRequirement> {
        None
    }
}

impl Sandboxable for BrowserTool {
    fn sandbox_preference(&self) -> SandboxPreference {
        // The browser runtime spawns its own external processes and manages its
        // own isolation; let the provider decide (today everything resolves to
        // `SandboxType::None`). `Auto` keeps the seam uniform with the other
        // tools.
        SandboxPreference::Auto
    }

    fn escalate_on_failure(&self) -> bool {
        // A browser failure is not a sandbox denial we can usefully retry
        // unsandboxed; keep it uniform with the other tools.
        true
    }
}

#[async_trait::async_trait]
impl ToolRuntime<BrowserRequest, ExecOutput> for BrowserTool {
    fn parallel_safe(&self, _req: &BrowserRequest) -> bool {
        // Browser actions share a single session/CDP connection and must run
        // serially. This matches the legacy tool set, which excludes the browser
        // tool from the parallel set.
        false
    }

    async fn run(
        &self,
        req: &BrowserRequest,
        attempt: &SandboxAttempt<'_>,
        ctx: &ToolCtx,
    ) -> Result<ExecOutput, ToolError> {
        // No sandbox backend is exercised here (the browser runtime spawns its
        // own processes); acknowledge the attempt to make the seam explicit.
        let _ = attempt;

        let effective_session_id = if req.session_id.trim().is_empty() {
            self.fallback_session_id(ctx).trim()
        } else {
            req.session_id.trim()
        };

        // Validate the request before touching the backend.
        if effective_session_id.is_empty() {
            return Err(ToolError::Rejected(
                "browser session_id must not be empty".to_string(),
            ));
        }
        match &req.action {
            BrowserAction::Command { command } if command.trim().is_empty() => {
                return Err(ToolError::Rejected(
                    "browser command must not be empty".to_string(),
                ));
            }
            BrowserAction::Execute { script, .. } if script.trim().is_empty() => {
                return Err(ToolError::Rejected(
                    "browser script must not be empty".to_string(),
                ));
            }
            BrowserAction::Observe { run_id } | BrowserAction::Cancel { run_id }
                if run_id.trim().is_empty() =>
            {
                return Err(ToolError::Rejected(
                    "browser run_id must not be empty".to_string(),
                ));
            }
            _ => {}
        }

        let selected_browser_mode = self.selected_browser_mode.clone();
        let selected_browser_mode = if self.dynamic_browser_mode_from_store {
            if let Some(persistence) = self.persistence.as_ref() {
                let dynamic_mode = persistence
                    .store
                    .lock()
                    .map_err(|_| ToolError::Other(anyhow!("store mutex poisoned")))
                    .and_then(|store| {
                        preferred_browser_mode(Some(&store))
                            .map(|mode| Some(mode.to_string()))
                            .map_err(ToolError::Other)
                    })?;
                if let Some(mode) = self.real_backend_mode.as_ref() {
                    *mode
                        .lock()
                        .map_err(|_| ToolError::Other(anyhow!("browser mode mutex poisoned")))? =
                        dynamic_mode.clone();
                }
                self.backend.set_browser_mode(dynamic_mode.clone());
                None
            } else {
                selected_browser_mode
            }
        } else {
            selected_browser_mode
        };
        let selected_browser_profile_id = self.selected_browser_profile_id.clone();
        let selected_local_browser = self.selected_local_browser.clone();
        let backend = Arc::clone(&self.backend);
        let session_id = effective_session_id.to_string();
        let cwd = req.cwd.clone().unwrap_or_else(|| ctx.cwd.clone());
        let artifact_dir = req
            .artifact_dir
            .clone()
            .unwrap_or_else(|| ctx.artifact_root.clone());
        let timeout_secs = req.effective_timeout_secs(self.default_script_timeout_secs);
        let observe_ms = req.effective_observe_ms();
        let action = req.action.clone();
        let persistence = self.persistence.clone();
        let tool_call_id = ctx.call_id.clone();
        let tool_name = if ctx.tool_name.trim().is_empty() {
            match &action {
                BrowserAction::Command { .. } => "browser".to_string(),
                BrowserAction::Execute { .. }
                | BrowserAction::Observe { .. }
                | BrowserAction::Cancel { .. } => "browser_script".to_string(),
            }
        } else {
            ctx.tool_name.clone()
        };

        // The browser fns are synchronous and spawn external processes; run on a
        // blocking thread so we never stall the async runtime.
        let result = tokio::task::spawn_blocking(move || -> Result<ExecOutput, ToolError> {
            match action {
                BrowserAction::Command { command } => {
                    let selected_browser_mode = selected_browser_mode.as_deref();
                    let out = if let Some(persistence) = &persistence {
                        run_browser_command_with_shared_store(
                            &persistence.store,
                            backend.as_ref(),
                            &session_id,
                            &cwd,
                            &artifact_dir,
                            &command,
                            selected_browser_mode,
                            selected_browser_profile_id.as_deref(),
                            selected_local_browser.as_deref(),
                        )?
                    } else {
                        let resolved = resolve_browser_command_for_selected_mode(
                            None,
                            &command,
                            selected_browser_mode,
                            selected_browser_profile_id.as_deref(),
                        )
                        .map_err(|error| ToolError::Rejected(format!("{error:#}")))?;
                        let output = backend
                            .command(&session_id, &cwd, &artifact_dir, &resolved)
                            .map_err(ToolError::Other)?;
                        enrich_status_with_selected_browser_mode(
                            output,
                            &resolved,
                            selected_browser_mode,
                        )
                    };
                    if let Some(persistence) = &persistence {
                        if let Ok(store) = persistence.store.lock() {
                            let _ = record_browser_command_response_events(
                                &store,
                                &persistence.session_id,
                                &tool_name,
                                &tool_call_id,
                                &out,
                            );
                        }
                    }
                    Ok(map_command_output(out))
                }
                BrowserAction::Execute { script, .. } => {
                    if let Some(persistence) = &persistence {
                        if let Some(preflight) = prepare_browser_for_script_with_shared_store(
                            &persistence.store,
                            backend.as_ref(),
                            &session_id,
                            &cwd,
                            &artifact_dir,
                            selected_browser_mode.as_deref(),
                            selected_browser_profile_id.as_deref(),
                            selected_local_browser.as_deref(),
                        )? {
                            return Ok(map_command_output(preflight));
                        }
                    }
                    // Re-resolve the secrets + nav policy on every run (fail closed)
                    // so secret/domain changes take effect mid-session
                    if let Some(persistence) = &persistence {
                        let store = persistence.store.lock().map_err(|_| {
                            ToolError::Other(anyhow::anyhow!("store mutex poisoned"))
                        })?;
                        super::secrets_admin::install_script_security(&store, &session_id)
                            .map_err(|error| {
                                ToolError::Other(anyhow::anyhow!(
                                    "failed to apply browser security policy: {error:#}"
                                ))
                            })?;
                    }
                    let out = backend
                        .start_script(&session_id, &cwd, &artifact_dir, &script, timeout_secs)
                        .map_err(ToolError::Other)?;
                    if let Some(persistence) = &persistence {
                        if let Ok(store) = persistence.store.lock() {
                            let _ = record_browser_script_response_events_for_tool(
                                &store,
                                &persistence.session_id,
                                &tool_name,
                                &tool_call_id,
                                &out,
                            );
                        }
                    }
                    Ok(map_script_output(out))
                }
                BrowserAction::Observe { run_id } => {
                    let out = backend
                        .observe_script(&session_id, &run_id, observe_ms)
                        .map_err(ToolError::Other)?;
                    if let Some(persistence) = &persistence {
                        if let Ok(store) = persistence.store.lock() {
                            let _ = record_browser_script_response_events_for_tool(
                                &store,
                                &persistence.session_id,
                                &tool_name,
                                &tool_call_id,
                                &out,
                            );
                        }
                    }
                    Ok(map_script_output(out))
                }
                BrowserAction::Cancel { run_id } => {
                    let out = backend
                        .cancel_script(&session_id, &run_id)
                        .map_err(ToolError::Other)?;
                    if let Some(persistence) = &persistence {
                        if let Ok(store) = persistence.store.lock() {
                            let _ = record_browser_script_response_events_for_tool(
                                &store,
                                &persistence.session_id,
                                &tool_name,
                                &tool_call_id,
                                &out,
                            );
                        }
                    }
                    Ok(map_script_output(out))
                }
            }
        })
        .await
        .map_err(|e| ToolError::Other(anyhow::anyhow!("browser task panicked: {e}")))?;

        result
    }
}

/// Run a `browser <cmd>` control-plane command with full store-backed
/// behavior: preference dispatch, mode/profile resolution, local-profile
/// preflight, and the default-profile enrichers.
///
/// Shared by the in-session [`BrowserTool`] command path and the external
/// assistant CLI surface ([`run_external_browser_command`]) so both see
/// identical heuristics.
#[allow(clippy::too_many_arguments)]
fn run_browser_command_with_shared_store(
    shared_store: &SharedStore,
    backend: &dyn BrowserBackend,
    session_id: &str,
    cwd: &std::path::Path,
    artifact_dir: &std::path::Path,
    command: &str,
    selected_browser_mode: Option<&str>,
    selected_browser_profile_id: Option<&str>,
    selected_local_browser: Option<&str>,
) -> Result<BrowserCommandOutput, ToolError> {
    let store = shared_store
        .lock()
        .map_err(|_| ToolError::Other(anyhow::anyhow!("store mutex poisoned")))?;
    if let Some(content) = dispatch_browser_preference_command_for_mode(
        &store,
        backend,
        session_id,
        cwd,
        artifact_dir,
        command,
        selected_browser_mode,
    )
    .map_err(|error| ToolError::Rejected(format!("{error:#}")))?
    {
        return Ok(BrowserCommandOutput {
            content,
            events: Vec::new(),
        });
    }
    let resolved = resolve_browser_command_for_selected_mode(
        Some(&store),
        command,
        selected_browser_mode,
        selected_browser_profile_id,
    )
    .map_err(|error| ToolError::Rejected(format!("{error:#}")))?;
    let preferred_browser = selected_local_browser.map(str::to_string).or_else(|| {
        store
            .get_setting(BROWSER_PREF_BROWSER)
            .ok()
            .flatten()
            .filter(|browser| !browser.trim().is_empty())
    });
    let effective_mode = effective_browser_mode(Some(&store), selected_browser_mode)
        .map_err(|error| ToolError::Rejected(format!("{error:#}")))?;
    let store_profile_id = if matches!(effective_mode, "local" | "cloud") {
        stored_profile_for_mode(&store, effective_mode)
            .map_err(|error| ToolError::Rejected(format!("{error:#}")))?
    } else {
        None
    };
    let default_profile_id = selected_browser_profile_id
        .map(str::to_string)
        .or(store_profile_id);
    let default_profile_id = if matches!(effective_mode, "local" | "cloud") {
        default_profile_id
    } else {
        None
    };
    let has_default_profile = default_profile_id.is_some();
    drop(store);
    if let Some(preflight) = local_connect_default_profile_preflight(
        has_default_profile,
        preferred_browser.as_deref(),
        backend,
        session_id,
        cwd,
        artifact_dir,
        &resolved,
    )
    .map_err(|error| ToolError::Rejected(format!("{error:#}")))?
    {
        return Ok(preflight);
    }
    open_default_profile_before_local_connect(
        backend,
        session_id,
        cwd,
        artifact_dir,
        &resolved,
        default_profile_id.as_deref(),
    )
    .map_err(ToolError::Other)?;
    let output = backend
        .command(session_id, cwd, artifact_dir, &resolved)
        .map_err(ToolError::Other)?;
    let output = enrich_local_profiles_with_default_profile(
        output,
        &resolved,
        default_profile_id.as_deref(),
    );
    let output = enforce_local_connect_default_profile_context(
        output,
        &resolved,
        default_profile_id.as_deref(),
    );
    let output = enrich_local_connect_recovery_with_default_profile(
        output,
        &resolved,
        default_profile_id.as_deref(),
    );
    Ok(enrich_status_with_selected_browser_mode(
        output,
        &resolved,
        Some(effective_mode),
    ))
}

/// Make the browser ready for a `browser_script` run: resolve the effective
/// mode from the store, run the local default-profile preflight, and
/// auto-connect/auto-start the configured browser when needed.
///
/// Returns `Some(preflight)` when browser work is blocked on a user decision
/// (e.g. no default local Chrome profile chosen yet); the caller must surface
/// that output instead of running the script.
///
/// Shared by the in-session [`BrowserTool`] execute path and the external
/// assistant CLI surface ([`run_external_browser_script`]).
#[allow(clippy::too_many_arguments)]
fn prepare_browser_for_script_with_shared_store(
    shared_store: &SharedStore,
    backend: &dyn BrowserBackend,
    session_id: &str,
    cwd: &std::path::Path,
    artifact_dir: &std::path::Path,
    selected_browser_mode: Option<&str>,
    selected_browser_profile_id: Option<&str>,
    selected_local_browser: Option<&str>,
) -> Result<Option<BrowserCommandOutput>, ToolError> {
    let store = shared_store
        .lock()
        .map_err(|_| ToolError::Other(anyhow::anyhow!("store mutex poisoned")))?;
    let mode = effective_browser_mode(Some(&store), selected_browser_mode)
        .map_err(|error| ToolError::Rejected(format!("{error:#}")))?;
    let default_profile_id = if mode == "local" {
        selected_browser_profile_id.map(str::to_string).or_else(|| {
            stored_profile_for_mode(&store, "local")
                .ok()
                .flatten()
                .filter(|profile| !profile.trim().is_empty())
        })
    } else {
        None
    };
    let preferred_browser = if mode == "local" {
        selected_local_browser.map(str::to_string).or_else(|| {
            store
                .get_setting(BROWSER_PREF_BROWSER)
                .ok()
                .flatten()
                .filter(|browser| !browser.trim().is_empty())
        })
    } else {
        None
    };
    drop(store);
    if mode == "local" && default_profile_id.is_none() {
        if let Some(preflight) = local_connect_default_profile_preflight(
            false,
            preferred_browser.as_deref(),
            backend,
            session_id,
            cwd,
            artifact_dir,
            "browser connect local",
        )
        .map_err(|error| ToolError::Rejected(format!("{error:#}")))?
        {
            return Ok(Some(preflight));
        }
    }
    ensure_browser_ready_for_work(
        backend,
        session_id,
        cwd,
        artifact_dir,
        mode,
        default_profile_id.as_deref(),
    )
    .map_err(ToolError::Other)?;
    Ok(None)
}

// ============================================================================
// External assistant CLI surface
// ============================================================================
//
// `browser-use-terminal browser ...` lets external coding assistants
// (Claude Code, Codex, OpenCode, ...) drive the browser through one-shot CLI
// invocations. These blocking entry points reuse the
// exact preference resolution, connect heuristics, security policy, and event
// persistence of the in-session `browser` / `browser_script` tools so an
// external assistant and the built-in agent see identical behavior.

/// Outcome of an external `browser exec` script request.
#[derive(Debug)]
pub enum ExternalBrowserScriptOutcome {
    /// The script ran. `output.ok` may still be false on script errors.
    Ran(BrowserScriptOutput),
    /// Browser work is blocked on a user decision before any script can run
    /// (e.g. no default local Chrome profile is chosen yet). The payload is the
    /// preflight JSON with `user_prompt` / `next_step` guidance.
    Blocked(Value),
}

fn external_tool_error(error: ToolError) -> anyhow::Error {
    match error {
        ToolError::Rejected(message) => anyhow!(message),
        ToolError::Other(error) => error,
        ToolError::Sandboxed(denial) => anyhow!(
            "browser call denied by sandbox: {}",
            denial.output.stderr.trim()
        ),
    }
}

/// Configure a backend with the store-preferred browser mode so its
/// auto-connect behavior (the harness `ensure_daemon()` analog) targets the
/// right browser.
fn set_backend_browser_mode_from_store(
    shared_store: &SharedStore,
    backend: &dyn BrowserBackend,
) -> anyhow::Result<()> {
    let store = shared_store
        .lock()
        .map_err(|_| anyhow!("store mutex poisoned"))?;
    let mode = preferred_browser_mode(Some(&store))?;
    drop(store);
    backend.set_browser_mode(Some(mode.to_string()));
    Ok(())
}

/// Run a `browser <cmd>` control-plane command for an external assistant CLI.
///
/// Blocking. Resolves the preferred mode from the store (so plain
/// `browser connect` honors `browser preference use ...`), runs the same
/// preflights/enrichers as the in-session tool, and records browser events to
/// the session's durable event log.
pub fn run_external_browser_command(
    shared_store: &SharedStore,
    session_id: &str,
    cwd: &std::path::Path,
    artifact_dir: &std::path::Path,
    command: &str,
) -> anyhow::Result<BrowserCommandOutput> {
    let backend = RealBackend::default();
    set_backend_browser_mode_from_store(shared_store, &backend)?;
    let out = run_browser_command_with_shared_store(
        shared_store,
        &backend,
        session_id,
        cwd,
        artifact_dir,
        command,
        None,
        None,
        None,
    )
    .map_err(external_tool_error)?;
    if let Ok(store) = shared_store.lock() {
        let _ = record_browser_command_response_events(
            &store,
            session_id,
            "browser",
            &format!("browser-cli-{session_id}"),
            &out,
        );
    }
    Ok(out)
}

/// Run a `browser_script` Python snippet for an external assistant CLI.
///
/// Blocking: auto-connects the preferred browser when needed, installs the
/// secrets/navigation policy, runs the script to completion, and records the
/// response (text, images, artifacts, browser events) to the session's durable
/// event log. Screenshots taken by the script land in `artifact_dir` and their
/// paths are reported in the returned output's `images`.
pub fn run_external_browser_script(
    shared_store: &SharedStore,
    session_id: &str,
    cwd: &std::path::Path,
    artifact_dir: &std::path::Path,
    code: &str,
    timeout_secs: u64,
) -> anyhow::Result<ExternalBrowserScriptOutcome> {
    let backend = RealBackend::default();
    set_backend_browser_mode_from_store(shared_store, &backend)?;
    if let Some(preflight) = prepare_browser_for_script_with_shared_store(
        shared_store,
        &backend,
        session_id,
        cwd,
        artifact_dir,
        None,
        None,
        None,
    )
    .map_err(external_tool_error)?
    {
        return Ok(ExternalBrowserScriptOutcome::Blocked(preflight.content));
    }
    {
        let store = shared_store
            .lock()
            .map_err(|_| anyhow!("store mutex poisoned"))?;
        super::secrets_admin::install_script_security(&store, session_id)
            .map_err(|error| anyhow!("failed to apply browser security policy: {error:#}"))?;
    }
    let out = backend.run_script(session_id, cwd, artifact_dir, code, timeout_secs)?;
    if let Ok(store) = shared_store.lock() {
        let _ = record_browser_script_response_events_for_tool(
            &store,
            session_id,
            "browser_script",
            &format!("browser-cli-{session_id}"),
            &out,
        );
    }
    Ok(ExternalBrowserScriptOutcome::Ran(out))
}

#[cfg(test)]
mod browser_mode_tests {
    use super::*;

    #[test]
    fn normalizes_remote_cloud_as_cloud() {
        // The browser layer serializes its cloud mode as "remote-cloud"; the
        // preference normalizer must treat it as cloud rather than bailing with
        // "unknown browser preference mode".
        assert_eq!(
            normalize_browser_preference_mode("remote-cloud").unwrap(),
            "cloud"
        );
        assert_eq!(
            normalize_browser_preference_mode("remote_cloud").unwrap(),
            "cloud"
        );
        assert_eq!(normalize_browser_preference_mode("cloud").unwrap(), "cloud");
        assert_eq!(
            normalize_browser_preference_mode("browser-use-cloud").unwrap(),
            "cloud"
        );
    }

    #[test]
    fn enforce_allows_cloud_command_when_run_locked_to_remote_cloud() {
        // A run locked to the cloud browser reports its mode as "remote-cloud";
        // issuing `browser remote ...` must be permitted, not rejected.
        enforce_selected_browser_mode(Some("remote-cloud"), "cloud").unwrap();
        enforce_browser_command_matches_selected_mode(
            &["remote".to_string(), "start".to_string()],
            Some("remote-cloud"),
        )
        .unwrap();
    }

    #[test]
    fn stored_cloud_preference_rejects_managed_only_recovery() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        store.set_setting(BROWSER_PREF_MODE, "cloud").unwrap();

        let err = resolve_browser_command_for_selected_mode(
            Some(&store),
            "browser recover restart-owned-browser",
            None,
            None,
        )
        .unwrap_err();

        assert!(
            err.to_string()
                .contains("restart-owned-browser only applies to managed Chromium"),
            "{err:#}"
        );
    }
}
