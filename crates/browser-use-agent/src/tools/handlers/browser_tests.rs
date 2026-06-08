//! Tests for the browser tool handler ([`BrowserTool`]).
//!
//! These NEVER touch the real `browser-use-browser` runtime (which requires
//! Bun + Chrome, an external CDP connection, and a local bridge port). Instead a
//! [`FakeBackend`] records the calls it receives and returns canned
//! `BrowserCommandOutput` / `BrowserScriptOutput` values, so the adapter's
//! translation and routing logic can be verified in isolation. No Bun, no
//! Chrome, no network.
//!
//! Tests cover: (1) a command request routes to the backend and maps output ->
//! ExecOutput; (2) script start/observe/cancel route correctly; (3)
//! parallel_safe = false; (4) backend error -> ToolError; (5) an
//! orchestrator-driven run with the fake backend.

use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};

use browser_use_browser::{BrowserCommandOutput, BrowserScriptOutput};
use browser_use_llm::schema::ContentPart;
use browser_use_store::Store;
use serde_json::json;

use super::browser::{
    browser_command_is_passive, desired_browser_connect_command,
    enrich_local_connect_recovery_with_default_profile, BrowserAction, BrowserBackend,
    BrowserRequest, BrowserTool, BROWSER_SCRIPT_CONTENT_STDOUT_PREFIX, DEFAULT_OBSERVE_TIMEOUT_MS,
    MAX_INLINE_BROWSER_SCRIPT_STDOUT_BYTES, MAX_OBSERVE_TIMEOUT_MS,
};
use crate::session::SharedStore;
use crate::tools::approval::AskForApproval;
use crate::tools::orchestrator::{ToolOrchestrator, TurnEnv};
use crate::tools::runtime::{
    Approvable, AutoApprover, ExecOutput, SandboxAttempt, ToolCtx, ToolError, ToolRuntime,
};
use crate::tools::sandbox::{
    FileSystemSandboxPolicy, NoneSandboxProvider, SandboxLaunch, SandboxPermissions, SandboxType,
};

/// Records which backend method was last invoked and with what arguments.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
enum LastCall {
    #[default]
    None,
    Command(String),
    RunScript(String),
    StartScript(String),
    Observe(String),
    Cancel(String),
}

/// A configurable fake backend. By default every call succeeds; set `fail` to
/// make every call return an `anyhow` error.
#[derive(Default)]
struct FakeBackend {
    last: Mutex<LastCall>,
    commands: Mutex<Vec<String>>,
    local_profiles: Mutex<Option<serde_json::Value>>,
    cloud_profiles: Mutex<Option<serde_json::Value>>,
    local_list: Mutex<Option<serde_json::Value>>,
    active_local_profile_id: Mutex<Option<String>>,
    browser_page_ready: Mutex<bool>,
    last_session: Mutex<Option<String>>,
    last_paths: Mutex<Option<(PathBuf, PathBuf)>>,
    last_timeout_secs: Mutex<Option<u64>>,
    last_observe_timeout_ms: Mutex<Option<u64>>,
    script_text: Mutex<Option<String>>,
    script_outputs: Mutex<Vec<serde_json::Value>>,
    script_summary: Mutex<Vec<serde_json::Value>>,
    script_images: Mutex<Vec<serde_json::Value>>,
    fail: bool,
    fail_local_profiles: bool,
}

struct EnvVarGuard {
    key: &'static str,
    previous: Option<OsString>,
    _lock: std::sync::MutexGuard<'static, ()>,
}

impl EnvVarGuard {
    fn set(key: &'static str, value: &str) -> Self {
        let lock = env_var_test_lock().lock().unwrap();
        let previous = std::env::var_os(key);
        std::env::set_var(key, value);
        Self {
            key,
            previous,
            _lock: lock,
        }
    }

    fn remove(key: &'static str) -> Self {
        let lock = env_var_test_lock().lock().unwrap();
        let previous = std::env::var_os(key);
        std::env::remove_var(key);
        Self {
            key,
            previous,
            _lock: lock,
        }
    }
}

fn env_var_test_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

impl Drop for EnvVarGuard {
    fn drop(&mut self) {
        if let Some(previous) = &self.previous {
            std::env::set_var(self.key, previous);
        } else {
            std::env::remove_var(self.key);
        }
    }
}

impl FakeBackend {
    fn last(&self) -> LastCall {
        self.last.lock().unwrap().clone()
    }

    fn commands(&self) -> Vec<String> {
        self.commands.lock().unwrap().clone()
    }

    fn set_local_list(&self, value: serde_json::Value) {
        *self.local_list.lock().unwrap() = Some(value);
    }

    fn active_local_profile_id(&self) -> Option<String> {
        self.active_local_profile_id.lock().unwrap().clone()
    }

    fn set_browser_page_ready(&self, ready: bool) {
        *self.browser_page_ready.lock().unwrap() = ready;
    }

    fn last_session(&self) -> Option<String> {
        self.last_session.lock().unwrap().clone()
    }

    fn last_paths(&self) -> Option<(PathBuf, PathBuf)> {
        self.last_paths.lock().unwrap().clone()
    }

    fn last_timeout_secs(&self) -> Option<u64> {
        *self.last_timeout_secs.lock().unwrap()
    }

    fn last_observe_timeout_ms(&self) -> Option<u64> {
        *self.last_observe_timeout_ms.lock().unwrap()
    }

    fn set_script_text(&self, text: impl Into<String>) {
        *self.script_text.lock().unwrap() = Some(text.into());
    }

    fn record_paths(&self, cwd: &std::path::Path, artifact_dir: &std::path::Path) {
        *self.last_paths.lock().unwrap() = Some((cwd.to_path_buf(), artifact_dir.to_path_buf()));
    }

    fn script_images(&self) -> Vec<serde_json::Value> {
        self.script_images.lock().unwrap().clone()
    }

    fn ok_command() -> BrowserCommandOutput {
        BrowserCommandOutput {
            content: json!({ "ok": true, "message": "navigated" }),
            events: vec![json!({ "type": "navigation" })],
        }
    }

    fn default_local_profiles() -> serde_json::Value {
        json!({
            "local_profiles": [
                { "id": "google-chrome:Default", "profile_name": "browser-use.com" },
                { "id": "google-chrome:Profile 1", "profile_name": "Laith" }
            ]
        })
    }

    fn default_local_list() -> serde_json::Value {
        json!({
            "candidates": [
                { "id": "local-1", "connectable": true, "state": "reachable" }
            ]
        })
    }

    fn default_cloud_profiles() -> serde_json::Value {
        json!({
            "status": "ok",
            "profiles": [
                {
                    "id": "cloud-work",
                    "name": "Browser Use - Work",
                    "cookieDomains": ["app.example.com", "accounts.example.com"]
                },
                {
                    "id": "cloud-personal",
                    "name": "Personal Profile",
                    "cookieDomains": ["shopping.example"]
                }
            ]
        })
    }

    fn command_output_for(&self, command: &str) -> BrowserCommandOutput {
        if let Some(profile_id) = command
            .strip_prefix("browser local open --profile ")
            .map(unquote_fake_browser_arg)
        {
            *self.active_local_profile_id.lock().unwrap() = Some(profile_id);
            return Self::ok_command();
        }
        if command == "browser connect local" {
            self.set_browser_page_ready(true);
        }
        let content = match command {
            "browser status --json" => self.local_status_json(),
            "browser connect local" => json!({
                "status": "connected",
                "browser": self.local_status_json(),
            }),
            "browser recover reattach-same-target" => json!({
                "status": "reattached",
                "browser": self.local_status_json(),
            }),
            "browser local profiles --json" => self
                .local_profiles
                .lock()
                .unwrap()
                .clone()
                .unwrap_or_else(Self::default_local_profiles),
            "browser remote profiles --json" => self
                .cloud_profiles
                .lock()
                .unwrap()
                .clone()
                .unwrap_or_else(Self::default_cloud_profiles),
            "browser local list --json" => self
                .local_list
                .lock()
                .unwrap()
                .clone()
                .unwrap_or_else(Self::default_local_list),
            _ => return Self::ok_command(),
        };
        BrowserCommandOutput {
            content,
            events: vec![],
        }
    }

    fn local_status_json(&self) -> serde_json::Value {
        let page = if *self.browser_page_ready.lock().unwrap() {
            json!({
                "target_id": "target-1",
                "session_id": "session-1",
            })
        } else {
            json!({
                "target_id": null,
                "session_id": null,
            })
        };
        json!({
            "mode": "local",
            "connection": "connected",
            "local_profile_id": self.active_local_profile_id(),
            "page": page,
        })
    }

    fn ok_script_with_text(status: Option<&str>, ok: bool, text: String) -> BrowserScriptOutput {
        BrowserScriptOutput {
            ok,
            status: status.map(|s| s.to_string()),
            run_id: Some("run-1".to_string()),
            text,
            ..Default::default()
        }
    }

    fn ok_script_with_images(&self, status: Option<&str>, ok: bool) -> BrowserScriptOutput {
        let text = self
            .script_text
            .lock()
            .unwrap()
            .clone()
            .unwrap_or_else(|| "script-output".to_string());
        BrowserScriptOutput {
            outputs: self.script_outputs.lock().unwrap().clone(),
            summary: self.script_summary.lock().unwrap().clone(),
            images: self.script_images(),
            ..Self::ok_script_with_text(status, ok, text)
        }
    }
}

impl BrowserBackend for FakeBackend {
    fn command(
        &self,
        session_id: &str,
        cwd: &std::path::Path,
        artifact_dir: &std::path::Path,
        command: &str,
    ) -> anyhow::Result<BrowserCommandOutput> {
        *self.last_session.lock().unwrap() = Some(session_id.to_string());
        *self.last_paths.lock().unwrap() = Some((cwd.to_path_buf(), artifact_dir.to_path_buf()));
        *self.last.lock().unwrap() = LastCall::Command(command.to_string());
        self.commands.lock().unwrap().push(command.to_string());
        self.record_paths(cwd, artifact_dir);
        if self.fail_local_profiles && command == "browser local profiles --json" {
            anyhow::bail!("profile discovery failed");
        }
        if self.fail {
            anyhow::bail!("boom");
        }
        Ok(self.command_output_for(command))
    }

    fn run_script(
        &self,
        session_id: &str,
        cwd: &std::path::Path,
        artifact_dir: &std::path::Path,
        code: &str,
        timeout_secs: u64,
    ) -> anyhow::Result<BrowserScriptOutput> {
        *self.last_session.lock().unwrap() = Some(session_id.to_string());
        *self.last_paths.lock().unwrap() = Some((cwd.to_path_buf(), artifact_dir.to_path_buf()));
        *self.last_timeout_secs.lock().unwrap() = Some(timeout_secs);
        *self.last.lock().unwrap() = LastCall::RunScript(code.to_string());
        self.record_paths(cwd, artifact_dir);
        if self.fail {
            anyhow::bail!("boom");
        }
        // Foreground run completed successfully.
        Ok(self.ok_script_with_images(None, true))
    }

    fn start_script(
        &self,
        session_id: &str,
        cwd: &std::path::Path,
        artifact_dir: &std::path::Path,
        code: &str,
        timeout_secs: u64,
    ) -> anyhow::Result<BrowserScriptOutput> {
        *self.last_session.lock().unwrap() = Some(session_id.to_string());
        *self.last_paths.lock().unwrap() = Some((cwd.to_path_buf(), artifact_dir.to_path_buf()));
        *self.last_timeout_secs.lock().unwrap() = Some(timeout_secs);
        *self.last.lock().unwrap() = LastCall::StartScript(code.to_string());
        self.record_paths(cwd, artifact_dir);
        if self.fail {
            anyhow::bail!("boom");
        }
        // A backgrounded start is still running.
        Ok(self.ok_script_with_images(Some("running"), true))
    }

    fn observe_script(
        &self,
        session_id: &str,
        run_id: &str,
        observe_timeout_ms: u64,
    ) -> anyhow::Result<BrowserScriptOutput> {
        *self.last_session.lock().unwrap() = Some(session_id.to_string());
        *self.last_observe_timeout_ms.lock().unwrap() = Some(observe_timeout_ms);
        *self.last.lock().unwrap() = LastCall::Observe(run_id.to_string());
        if self.fail {
            anyhow::bail!("unknown browser_script run_id {run_id:?}");
        }
        Ok(self.ok_script_with_images(None, true))
    }

    fn cancel_script(&self, session_id: &str, run_id: &str) -> anyhow::Result<BrowserScriptOutput> {
        *self.last_session.lock().unwrap() = Some(session_id.to_string());
        *self.last.lock().unwrap() = LastCall::Cancel(run_id.to_string());
        if self.fail {
            anyhow::bail!("unknown browser_script run_id {run_id:?}");
        }
        // Cancel reports a completed-but-not-ok run.
        Ok(self.ok_script_with_images(None, false))
    }
}

fn unquote_fake_browser_arg(value: &str) -> String {
    value
        .trim()
        .strip_prefix('\'')
        .and_then(|value| value.strip_suffix('\''))
        .unwrap_or_else(|| value.trim())
        .replace("'\\''", "'")
}

fn tool_with(backend: Arc<FakeBackend>) -> BrowserTool {
    BrowserTool::with_backend(backend)
}

fn none_launch() -> SandboxLaunch {
    SandboxLaunch {
        sandbox: SandboxType::None,
        cancel: None,
    }
}

fn none_attempt(launch: &SandboxLaunch) -> SandboxAttempt<'_> {
    SandboxAttempt {
        sandbox: SandboxType::None,
        permissions: SandboxPermissions::UseDefault,
        enforce_managed_network: false,
        launch,
        cancel: None,
    }
}

fn ctx() -> ToolCtx {
    ctx_with_call_id("call-browser")
}

fn ctx_with_call_id(call_id: &str) -> ToolCtx {
    let cwd = std::env::temp_dir();
    ToolCtx {
        call_id: call_id.to_string(),
        tool_name: "browser".to_string(),
        cwd: cwd.clone(),
        artifact_root: cwd.join("artifacts"),
    }
}

fn ctx_for_tool(tool_name: &str, call_id: &str) -> ToolCtx {
    let cwd = std::env::temp_dir();
    ToolCtx {
        call_id: call_id.to_string(),
        tool_name: tool_name.to_string(),
        cwd: cwd.clone(),
        artifact_root: cwd.join("artifacts"),
    }
}

fn shared_store() -> (tempfile::TempDir, SharedStore, String) {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = Store::open(dir.path()).expect("open store");
    let session = store
        .create_session(None, std::path::Path::new("/tmp"))
        .expect("create session")
        .id;
    (dir, Arc::new(Mutex::new(store)), session)
}

/// Run a request directly through the runtime with a `SandboxType::None`
/// attempt (no orchestrator).
async fn run_direct(tool: &BrowserTool, req: &BrowserRequest) -> Result<ExecOutput, ToolError> {
    let launch = none_launch();
    let attempt = none_attempt(&launch);
    tool.run(req, &attempt, &ctx()).await
}

async fn run_direct_with_ctx(
    tool: &BrowserTool,
    req: &BrowserRequest,
    ctx: &ToolCtx,
) -> Result<ExecOutput, ToolError> {
    let launch = none_launch();
    let attempt = none_attempt(&launch);
    tool.run(req, &attempt, ctx).await
}

#[test]
fn permission_blocked_default_profile_recovery_opens_selected_profile_first() {
    let output = BrowserCommandOutput {
        content: json!({
            "status": "blocked",
            "state": "permission-blocked",
            "reason": "Chrome rejected CDP control.",
            "next_step": "Ask the user to click Allow",
        }),
        events: vec![],
    };

    let output = enrich_local_connect_recovery_with_default_profile(
        output,
        "browser connect local",
        Some("google-chrome:Profile 1"),
    );

    assert_eq!(
        output.content["profile_recovery_command"],
        "browser local open --profile 'google-chrome:Profile 1' --no-marker"
    );
    let next_step = output.content["next_step"].as_str().unwrap();
    assert!(
        next_step.contains("browser local open --profile 'google-chrome:Profile 1' --no-marker"),
        "{next_step}"
    );
    assert!(
        next_step.contains("immediately run `browser connect local` again"),
        "{next_step}"
    );
}

// (1) A browser command request routes to the backend and maps output->ExecOutput.
#[tokio::test]
async fn command_routes_and_maps_output() {
    let backend = Arc::new(FakeBackend::default());
    let tool = tool_with(Arc::clone(&backend));

    let req = BrowserRequest::command("sess-1", "go https://example.com");
    let out = run_direct(&tool, &req).await.unwrap();

    assert_eq!(
        backend.last(),
        LastCall::Command("go https://example.com".to_string())
    );
    assert_eq!(out.exit_code, 0);
    assert!(out.stdout.contains("navigated"), "stdout: {}", out.stdout);
    assert!(
        out.stderr.contains("navigation"),
        "events should land on stderr: {}",
        out.stderr
    );
}

#[tokio::test]
async fn bare_browser_connect_resolves_to_selected_local_mode() {
    let backend = Arc::new(FakeBackend::default());
    let tool =
        tool_with(Arc::clone(&backend)).with_selected_browser_mode(Some("local".to_string()));

    let req = BrowserRequest::command("sess-1", "browser connect");
    let out = run_direct(&tool, &req).await.unwrap();

    assert_eq!(out.exit_code, 0);
    assert_eq!(
        backend.last(),
        LastCall::Command("browser connect local".to_string())
    );
}

#[tokio::test]
async fn bare_browser_connect_resolves_to_selected_cloud_mode() {
    let backend = Arc::new(FakeBackend::default());
    let tool =
        tool_with(Arc::clone(&backend)).with_selected_browser_mode(Some("cloud".to_string()));

    let req = BrowserRequest::command("sess-1", "browser connect");
    let out = run_direct(&tool, &req).await.unwrap();

    assert_eq!(out.exit_code, 0);
    assert_eq!(
        backend.last(),
        LastCall::Command("browser remote start".to_string())
    );
}

#[tokio::test]
async fn bare_browser_connect_resolves_to_selected_remote_cdp_mode() {
    let _guard = EnvVarGuard::set(
        "BU_CDP_URL",
        "ws://127.0.0.1:9222/devtools/browser/session-id",
    );
    let backend = Arc::new(FakeBackend::default());
    let tool =
        tool_with(Arc::clone(&backend)).with_selected_browser_mode(Some("remote-cdp".to_string()));

    let req = BrowserRequest::command("sess-1", "browser connect");
    let out = run_direct(&tool, &req).await.unwrap();

    assert_eq!(out.exit_code, 0);
    assert_eq!(
        backend.last(),
        LastCall::Command(
            "browser connect remote-cdp --ws ws://127.0.0.1:9222/devtools/browser/session-id"
                .to_string()
        )
    );
}

#[tokio::test]
async fn selected_remote_cdp_rewrites_wrong_browser_family_commands() {
    let _guard = EnvVarGuard::set(
        "BU_CDP_URL",
        "ws://127.0.0.1:9222/devtools/browser/session-id",
    );
    let backend = Arc::new(FakeBackend::default());
    let tool =
        tool_with(Arc::clone(&backend)).with_selected_browser_mode(Some("remote-cdp".to_string()));

    for command in [
        "browser connect managed --headed",
        "browser connect managed --headless",
        "browser connect local",
        "browser remote start",
    ] {
        let req = BrowserRequest::command("sess-1", command);
        let out = run_direct(&tool, &req).await.unwrap();

        assert_eq!(out.exit_code, 0, "{command}");
        assert_eq!(
            backend.last(),
            LastCall::Command(
                "browser connect remote-cdp --ws ws://127.0.0.1:9222/devtools/browser/session-id"
                    .to_string()
            ),
            "{command}"
        );
    }
}

#[tokio::test]
async fn selected_browser_mode_rejects_wrong_connection_family() {
    let backend = Arc::new(FakeBackend::default());
    let tool =
        tool_with(Arc::clone(&backend)).with_selected_browser_mode(Some("local".to_string()));

    let req = BrowserRequest::command("sess-1", "browser remote start");
    let err = run_direct(&tool, &req).await.unwrap_err();

    assert!(matches!(err, ToolError::Rejected(_)), "got {err:?}");
    assert_eq!(backend.last(), LastCall::None);
}

#[tokio::test]
async fn browser_preference_command_is_store_backed_and_synthetic() {
    let backend = Arc::new(FakeBackend::default());
    let (_dir, store, session) = shared_store();
    let tool = tool_with(Arc::clone(&backend))
        .with_selected_browser_mode(Some("cloud".to_string()))
        .with_persistence(store.clone(), session.clone());

    let req = BrowserRequest::command("sess-1", "browser preference use cloud");
    let out = run_direct(&tool, &req).await.unwrap();

    assert_eq!(backend.last(), LastCall::None);
    assert!(out.stdout.contains("\"next_step\":\"browser connect\""));
    assert_eq!(
        store
            .lock()
            .unwrap()
            .get_setting("browser.preference.mode")
            .unwrap()
            .as_deref(),
        Some("cloud")
    );
}

#[tokio::test]
async fn stored_cloud_profile_influences_bare_connect_when_mode_unlocked() {
    let _guard = EnvVarGuard::remove("BU_BROWSER_PROXY_COUNTRY_CODE");
    let backend = Arc::new(FakeBackend::default());
    let (_dir, store, session) = shared_store();
    {
        let store = store.lock().unwrap();
        store
            .set_setting("browser.preference.mode", "cloud")
            .unwrap();
        store
            .set_setting("browser.preference.profile", "profile with space")
            .unwrap();
    }
    let tool = tool_with(Arc::clone(&backend)).with_persistence(store, session);

    let req = BrowserRequest::command("sess-1", "browser connect");
    let out = run_direct(&tool, &req).await.unwrap();

    assert_eq!(out.exit_code, 0);
    assert_eq!(
        backend.last(),
        LastCall::Command("browser remote start --profile-id 'profile with space'".to_string())
    );
}

#[tokio::test]
async fn stored_cloud_profile_uses_sdk_proxy_country_env_when_connecting() {
    let _guard = EnvVarGuard::set("BU_BROWSER_PROXY_COUNTRY_CODE", "DE");
    let backend = Arc::new(FakeBackend::default());
    let (_dir, store, session) = shared_store();
    {
        let store = store.lock().unwrap();
        store
            .set_setting("browser.preference.mode", "cloud")
            .unwrap();
        store
            .set_setting("browser.preference.profile", "profile with space")
            .unwrap();
    }
    let tool = tool_with(Arc::clone(&backend)).with_persistence(store, session);

    let req = BrowserRequest::command("sess-1", "browser connect");
    let out = run_direct(&tool, &req).await.unwrap();

    assert_eq!(out.exit_code, 0);
    assert_eq!(
        backend.last(),
        LastCall::Command(
            "browser remote start --profile-id 'profile with space' --proxy-country DE".to_string()
        )
    );
}

#[tokio::test]
async fn stored_cloud_profile_influences_bare_connect_when_mode_locked() {
    let _guard = EnvVarGuard::remove("BU_BROWSER_PROXY_COUNTRY_CODE");
    let backend = Arc::new(FakeBackend::default());
    let (_dir, store, session) = shared_store();
    {
        let store = store.lock().unwrap();
        store
            .set_setting("browser.preference.mode", "cloud")
            .unwrap();
        store
            .set_setting("browser.preference.profile", "cloud-work")
            .unwrap();
    }
    let tool = tool_with(Arc::clone(&backend))
        .with_selected_browser_mode(Some("cloud".to_string()))
        .with_persistence(store, session);

    let req = BrowserRequest::command("sess-1", "browser connect");
    let out = run_direct(&tool, &req).await.unwrap();

    assert_eq!(out.exit_code, 0);
    assert_eq!(
        backend.last(),
        LastCall::Command("browser remote start --profile-id cloud-work".to_string())
    );
}

#[tokio::test]
async fn selected_cloud_mode_ignores_stored_local_profile_on_bare_connect() {
    let _guard = EnvVarGuard::remove("BU_BROWSER_PROXY_COUNTRY_CODE");
    let backend = Arc::new(FakeBackend::default());
    let (_dir, store, session) = shared_store();
    {
        let store = store.lock().unwrap();
        store
            .set_setting("browser.preference.mode", "local")
            .unwrap();
        store
            .set_setting("browser.preference.profile", "google-chrome:Profile 1")
            .unwrap();
    }
    let tool = tool_with(Arc::clone(&backend))
        .with_selected_browser_mode(Some("cloud".to_string()))
        .with_persistence(store, session);

    let req = BrowserRequest::command("sess-1", "browser connect");
    let out = run_direct(&tool, &req).await.unwrap();

    assert_eq!(out.exit_code, 0);
    assert_eq!(
        backend.last(),
        LastCall::Command("browser remote start".to_string())
    );
}

#[tokio::test]
async fn cloud_profile_suggest_lists_matching_cloud_profiles() {
    let backend = Arc::new(FakeBackend::default());
    let (_dir, store, session) = shared_store();
    {
        let store = store.lock().unwrap();
        store
            .set_setting("browser.preference.mode", "cloud")
            .unwrap();
    }
    let tool = tool_with(Arc::clone(&backend))
        .with_selected_browser_mode(Some("cloud".to_string()))
        .with_persistence(store, session);

    let req = BrowserRequest::command(
        "sess-1",
        "browser profile suggest --domain example[.]com$ --json",
    );
    let out = run_direct(&tool, &req).await.unwrap();

    assert_eq!(out.exit_code, 0);
    assert!(out.stdout.contains("\"mode\":\"cloud\""), "{}", out.stdout);
    assert!(out.stdout.contains("\"cloud_profiles\""), "{}", out.stdout);
    let parsed: serde_json::Value = serde_json::from_str(&out.stdout).unwrap();
    assert_eq!(parsed["default_profile_id"], serde_json::Value::Null);
    assert_eq!(
        parsed["matching_profiles"][0]["matching_cookie_domains"],
        json!(["app.example.com", "accounts.example.com"])
    );
    assert_eq!(
        parsed["matching_profiles"][0]["cookie_domain_count"],
        json!(2)
    );
    assert!(parsed["matching_profiles"][0]
        .get("cookieDomains")
        .is_none());
    assert!(parsed["matching_profiles"][0]
        .get("sample_cookie_domains")
        .is_none());
    assert!(parsed["cloud_profiles"][0].get("cookieDomains").is_none());
    assert!(parsed["cloud_profiles"][0]
        .get("sample_cookie_domains")
        .is_none());
    assert!(parsed["profile_choices"][0].get("cookie_domains").is_none());
    assert!(parsed["profile_choices"][0]
        .get("sample_cookie_domains")
        .is_none());
    assert!(out.stdout.contains("cloud-work"), "{}", out.stdout);
    assert_eq!(
        backend.commands(),
        vec!["browser remote profiles --json".to_string()]
    );
}

#[tokio::test]
async fn cloud_profile_remember_sets_mode_and_cloud_next_step() {
    let backend = Arc::new(FakeBackend::default());
    let (_dir, store, session) = shared_store();
    let tool = tool_with(Arc::clone(&backend))
        .with_selected_browser_mode(Some("cloud".to_string()))
        .with_persistence(store.clone(), session);

    let req = BrowserRequest::command(
        "sess-1",
        "browser profile remember --mode cloud --profile cloud-work",
    );
    let out = run_direct(&tool, &req).await.unwrap();

    assert_eq!(out.exit_code, 0);
    assert!(out.stdout.contains("\"mode\":\"cloud\""), "{}", out.stdout);
    assert!(
        out.stdout
            .contains("\"next_step\":\"browser remote start --profile-id cloud-work\""),
        "{}",
        out.stdout
    );
    {
        let store = store.lock().unwrap();
        assert_eq!(
            store
                .get_setting("browser.preference.mode")
                .unwrap()
                .as_deref(),
            Some("cloud")
        );
        assert_eq!(
            store
                .get_setting("browser.preference.profile")
                .unwrap()
                .as_deref(),
            Some("cloud-work")
        );
    }
}

#[tokio::test]
async fn stored_local_profile_is_opened_before_plain_local_connect_when_reachable_profiles_are_ambiguous(
) {
    let backend = Arc::new(FakeBackend::default());
    let (_dir, store, session) = shared_store();
    {
        let store = store.lock().unwrap();
        store.set_setting("browser", "Local Chrome").unwrap();
        store
            .set_setting("browser.preference.profile", "google-chrome:Profile 1")
            .unwrap();
    }
    let tool = tool_with(Arc::clone(&backend)).with_persistence(store, session);

    let req = BrowserRequest::command("sess-1", "browser connect local");
    let out = run_direct(&tool, &req).await.unwrap();

    assert_eq!(out.exit_code, 0);
    assert_eq!(
        backend.commands(),
        vec![
            "browser local profiles --json".to_string(),
            "browser local list --json".to_string(),
            "browser local open --profile 'google-chrome:Profile 1'".to_string(),
            "browser connect local".to_string(),
        ]
    );
}

#[tokio::test]
async fn local_connect_blocks_if_default_profile_context_is_not_active_after_connect() {
    let backend = Arc::new(FakeBackend::default());
    *backend.local_profiles.lock().unwrap() = Some(json!({
        "status": "ok",
        "profiles": [
            {
                "id": "google-chrome:Profile 1",
                "profile_name": "Laith",
                "profile_dir": "Profile 1"
            }
        ],
    }));
    let (_dir, store, session) = shared_store();
    {
        let store = store.lock().unwrap();
        store.set_setting("browser", "Local Chrome").unwrap();
        store
            .set_setting("browser.preference.profile", "google-chrome:Profile 1")
            .unwrap();
    }
    let tool = tool_with(Arc::clone(&backend)).with_persistence(store, session);

    let req = BrowserRequest::command("sess-1", "browser connect local");
    let out = run_direct(&tool, &req).await.unwrap();

    assert_eq!(out.exit_code, 0);
    assert!(
        out.stdout.contains("\"status\":\"blocked\""),
        "{}",
        out.stdout
    );
    assert!(
        out.stdout.contains("\"state\":\"profile-target-missing\""),
        "{}",
        out.stdout
    );
    assert!(
        out.stdout.contains("browser local open --profile"),
        "{}",
        out.stdout
    );
}

#[tokio::test]
async fn local_profiles_response_warns_model_not_to_ask_when_default_exists() {
    let backend = Arc::new(FakeBackend::default());
    let (_dir, store, session) = shared_store();
    {
        let store = store.lock().unwrap();
        store.set_setting("browser", "Local Chrome").unwrap();
        store
            .set_setting("browser.preference.profile", "google-chrome:Profile 1")
            .unwrap();
    }
    let tool = tool_with(Arc::clone(&backend)).with_persistence(store, session);

    let req = BrowserRequest::command("sess-1", "browser local profiles --json");
    let out = run_direct(&tool, &req).await.unwrap();

    assert_eq!(out.exit_code, 0);
    assert!(
        out.stdout
            .contains("\"default_profile_id\":\"google-chrome:Profile 1\""),
        "{}",
        out.stdout
    );
    assert!(
        out.stdout.contains("Do not ask the user which profile"),
        "{}",
        out.stdout
    );
}

#[tokio::test]
async fn stored_local_profile_does_not_open_marker_when_chrome_is_not_reachable() {
    let backend = Arc::new(FakeBackend::default());
    backend.set_local_list(json!({
        "candidates": [
            { "id": "local-1", "connectable": false, "state": "stale-port" }
        ]
    }));
    let (_dir, store, session) = shared_store();
    {
        let store = store.lock().unwrap();
        store.set_setting("browser", "Local Chrome").unwrap();
        store
            .set_setting("browser.preference.profile", "google-chrome:Profile 1")
            .unwrap();
    }
    let tool = tool_with(Arc::clone(&backend)).with_persistence(store, session);

    let req = BrowserRequest::command("sess-1", "browser connect local");
    let out = run_direct(&tool, &req).await.unwrap();

    assert_eq!(out.exit_code, 0);
    assert_eq!(
        backend.commands(),
        vec![
            "browser local profiles --json".to_string(),
            "browser local list --json".to_string(),
            "browser connect local".to_string(),
        ]
    );
}

#[tokio::test]
async fn local_connect_blocks_without_default_when_profile_discovery_errors() {
    let backend = Arc::new(FakeBackend {
        fail_local_profiles: true,
        ..Default::default()
    });
    let (_dir, store, session) = shared_store();
    {
        let store = store.lock().unwrap();
        store.set_setting("browser", "Local Chrome").unwrap();
    }
    let tool = tool_with(Arc::clone(&backend)).with_persistence(store, session);

    let req = BrowserRequest::command("sess-1", "browser connect local");
    let out = run_direct(&tool, &req).await.unwrap();

    assert_eq!(out.exit_code, 0);
    assert!(out.stdout.contains("\"status\":\"blocked\""));
    assert!(out
        .stdout
        .contains("\"state\":\"profile-discovery-failed\""));
    assert!(out.stdout.contains("profile discovery failed"));
    assert!(
        out.stdout
            .contains("Do not run browser connect local without a selected default profile"),
        "{}",
        out.stdout
    );
    assert_eq!(
        backend.commands(),
        vec!["browser local profiles --json".to_string()]
    );
}

#[tokio::test]
async fn local_connect_blocks_without_default_when_profile_discovery_reports_failed() {
    let backend = Arc::new(FakeBackend::default());
    *backend.local_profiles.lock().unwrap() = Some(json!({
        "status": "failed",
        "error": "profile discovery failed",
        "profiles": [],
    }));
    let (_dir, store, session) = shared_store();
    {
        let store = store.lock().unwrap();
        store.set_setting("browser", "Local Chrome").unwrap();
    }
    let tool = tool_with(Arc::clone(&backend)).with_persistence(store, session);

    let req = BrowserRequest::command("sess-1", "browser connect local");
    let out = run_direct(&tool, &req).await.unwrap();

    assert_eq!(out.exit_code, 0);
    assert!(out.stdout.contains("\"status\":\"blocked\""));
    assert!(out
        .stdout
        .contains("\"state\":\"profile-discovery-failed\""));
    assert!(out.stdout.contains("profile discovery failed"));
    assert_eq!(
        backend.commands(),
        vec!["browser local profiles --json".to_string()]
    );
}

#[tokio::test]
async fn dynamic_browser_mode_uses_latest_store_browser_setting_mid_run() {
    let backend = Arc::new(FakeBackend::default());
    let (_dir, store, session) = shared_store();
    {
        let store = store.lock().unwrap();
        store.set_setting("browser", "Local Chrome").unwrap();
        store
            .set_setting("browser.preference.profile", "google-chrome:Default")
            .unwrap();
    }
    let tool = tool_with(Arc::clone(&backend))
        .with_selected_browser_mode(Some("cloud".to_string()))
        .with_persistence(store, session)
        .with_dynamic_browser_mode_from_store(true);

    let req = BrowserRequest::command("sess-1", "browser connect");
    let out = run_direct(&tool, &req).await.unwrap();

    assert_eq!(out.exit_code, 0);
    assert_eq!(
        backend.last(),
        LastCall::Command("browser connect local".to_string())
    );
}

#[test]
fn selected_local_mode_reconnects_when_cloud_is_currently_attached() {
    assert_eq!(
        desired_browser_connect_command("local", true, Some("remote-cloud"), Some("rust")),
        Some("browser connect local")
    );
    assert_eq!(
        desired_browser_connect_command("local", true, Some("local"), Some("external")),
        None
    );
}

#[test]
fn selected_local_mode_does_not_auto_reconnect_disconnected_external_chrome() {
    assert_eq!(
        desired_browser_connect_command("local", false, Some("local"), Some("external")),
        None
    );
    assert_eq!(
        desired_browser_connect_command("local", false, Some("local"), Some("rust")),
        Some("browser connect local")
    );
}

#[test]
fn passive_browser_commands_do_not_preflight_connect() {
    for words in [
        ["browser", "status", "--json"].as_slice(),
        ["status", "--json"].as_slice(),
        ["browser", "connect", "local"].as_slice(),
        ["connect", "local"].as_slice(),
        ["browser", "runtime", "logs"].as_slice(),
        ["runtime", "logs"].as_slice(),
        ["browser", "runtime", "ownership", "--json"].as_slice(),
        ["runtime", "ownership", "--json"].as_slice(),
        ["browser", "local", "list", "--json"].as_slice(),
        ["local", "list", "--json"].as_slice(),
        ["browser", "local", "profiles", "--json"].as_slice(),
        ["local", "profiles", "--json"].as_slice(),
        [
            "browser",
            "local",
            "open",
            "--profile",
            "google-chrome:Default",
        ]
        .as_slice(),
        ["local", "open", "--profile", "google-chrome:Default"].as_slice(),
        ["browser", "local", "setup"].as_slice(),
        ["local", "setup"].as_slice(),
    ] {
        assert!(browser_command_is_passive(words), "{words:?}");
    }
    assert!(!browser_command_is_passive(&[
        "browser",
        "runtime",
        "cleanup-stale"
    ]));
    assert!(!browser_command_is_passive(&["go", "https://example.com"]));
}

// (2) Script start routes to start_script, matching main's browser_script tool.
#[tokio::test]
async fn script_start_routes_to_start_script() {
    let backend = Arc::new(FakeBackend::default());
    let tool = tool_with(Arc::clone(&backend));

    let req = BrowserRequest::execute("sess-1", "click('#go')", false);
    let out = run_direct(&tool, &req).await.unwrap();

    assert_eq!(
        backend.last(),
        LastCall::StartScript("click('#go')".to_string())
    );
    assert!(
        out.stdout.contains("script-output"),
        "stdout: {}",
        out.stdout
    );
    assert!(
        out.stdout.contains("run_id: run-1"),
        "stdout: {}",
        out.stdout
    );
    assert_eq!(out.exit_code, 0);
}

#[tokio::test]
async fn script_start_without_default_local_profile_prompts_before_connecting() {
    let backend = Arc::new(FakeBackend::default());
    let (_dir, store, session) = shared_store();
    {
        let store = store.lock().unwrap();
        store.set_setting("browser", "Local Chrome").unwrap();
    }
    let tool = tool_with(Arc::clone(&backend)).with_persistence(store, session);

    let req = BrowserRequest::execute("sess-1", "click('#go')", false);
    let out = run_direct(&tool, &req).await.unwrap();

    assert_eq!(out.exit_code, 0);
    assert!(out.stdout.contains("\"status\":\"needs-user-action\""));
    assert!(
        out.stdout
            .contains("No default local Chrome profile is set."),
        "{}",
        out.stdout
    );
    assert_eq!(
        backend.commands(),
        vec!["browser local profiles --json".to_string()]
    );
    assert_eq!(
        backend.last(),
        LastCall::Command("browser local profiles --json".to_string())
    );
}

#[tokio::test]
async fn script_start_switches_to_changed_default_local_profile_before_running() {
    let backend = Arc::new(FakeBackend::default());
    *backend.active_local_profile_id.lock().unwrap() = Some("google-chrome:Profile 1".to_string());
    let (_dir, store, session) = shared_store();
    {
        let store = store.lock().unwrap();
        store.set_setting("browser", "Local Chrome").unwrap();
        store
            .set_setting("browser.preference.profile", "google-chrome:Default")
            .unwrap();
    }
    let tool = tool_with(Arc::clone(&backend)).with_persistence(store, session);

    let req = BrowserRequest::execute("sess-1", "click('#go')", false);
    let out = run_direct(&tool, &req).await.unwrap();

    assert_eq!(out.exit_code, 0);
    assert_eq!(
        backend.commands(),
        vec![
            "browser status --json".to_string(),
            "browser local open --profile google-chrome:Default".to_string(),
            "browser connect local".to_string(),
        ]
    );
    assert_eq!(
        backend.last(),
        LastCall::StartScript("click('#go')".to_string())
    );
}

#[tokio::test]
async fn script_start_repairs_missing_local_target_before_running() {
    let backend = Arc::new(FakeBackend::default());
    *backend.active_local_profile_id.lock().unwrap() = Some("google-chrome:Default".to_string());
    backend.set_browser_page_ready(false);
    let (_dir, store, session) = shared_store();
    {
        let store = store.lock().unwrap();
        store.set_setting("browser", "Local Chrome").unwrap();
        store
            .set_setting("browser.preference.profile", "google-chrome:Default")
            .unwrap();
    }
    let tool = tool_with(Arc::clone(&backend)).with_persistence(store, session);

    let req = BrowserRequest::execute("sess-1", "click('#go')", false);
    let out = run_direct(&tool, &req).await.unwrap();

    assert_eq!(out.exit_code, 0);
    assert_eq!(
        backend.commands(),
        vec![
            "browser status --json".to_string(),
            "browser local open --profile google-chrome:Default".to_string(),
            "browser connect local".to_string(),
        ]
    );
    assert_eq!(
        backend.last(),
        LastCall::StartScript("click('#go')".to_string())
    );
}

#[tokio::test]
async fn script_start_reuses_ready_matching_local_profile_after_live_probe() {
    let backend = Arc::new(FakeBackend::default());
    *backend.active_local_profile_id.lock().unwrap() = Some("google-chrome:Default".to_string());
    backend.set_browser_page_ready(true);
    let (_dir, store, session) = shared_store();
    {
        let store = store.lock().unwrap();
        store.set_setting("browser", "Local Chrome").unwrap();
        store
            .set_setting("browser.preference.profile", "google-chrome:Default")
            .unwrap();
    }
    let tool = tool_with(Arc::clone(&backend)).with_persistence(store, session);

    let req = BrowserRequest::execute("sess-1", "click('#go')", false);
    let out = run_direct(&tool, &req).await.unwrap();

    assert_eq!(out.exit_code, 0);
    assert_eq!(
        backend.commands(),
        vec!["browser status --json".to_string()]
    );
    assert_eq!(
        backend.last(),
        LastCall::StartScript("click('#go')".to_string())
    );
}

#[tokio::test]
async fn script_start_uses_tool_default_timeout_when_request_omits_it() {
    let backend = Arc::new(FakeBackend::default());
    let tool = tool_with(Arc::clone(&backend)).with_default_script_timeout_secs(7);

    let req = BrowserRequest::execute("sess-1", "click('#go')", false);
    let out = run_direct(&tool, &req).await.unwrap();

    assert_eq!(out.exit_code, 0);
    assert_eq!(backend.last_timeout_secs(), Some(7));
}

#[tokio::test]
async fn script_start_request_timeout_overrides_tool_default() {
    let backend = Arc::new(FakeBackend::default());
    let tool = tool_with(Arc::clone(&backend)).with_default_script_timeout_secs(7);

    let mut req = BrowserRequest::execute("sess-1", "click('#go')", false);
    req.timeout_secs = Some(3);
    let out = run_direct(&tool, &req).await.unwrap();

    assert_eq!(out.exit_code, 0);
    assert_eq!(backend.last_timeout_secs(), Some(3));
}

#[tokio::test]
async fn script_images_are_appended_as_structured_stdout_payload() {
    let temp = tempfile::tempdir().expect("tempdir");
    let image_path = temp.path().join("shot.png");
    std::fs::write(&image_path, [0x89, b'P', b'N', b'G']).expect("write png");

    let backend = Arc::new(FakeBackend::default());
    backend.script_images.lock().unwrap().push(json!({
        "path": image_path,
        "mime_type": "image/png",
        "detail": "auto",
        "label": "viewport",
    }));
    let tool = tool_with(Arc::clone(&backend));

    let req = BrowserRequest::execute("sess-1", "capture_screenshot()", false);
    let out = run_direct(&tool, &req).await.unwrap();
    let (visible, payload) = out
        .stdout
        .rsplit_once(BROWSER_SCRIPT_CONTENT_STDOUT_PREFIX)
        .expect("browser_script content marker");
    assert!(
        visible.contains("script-output"),
        "visible stdout: {visible}"
    );
    let parts: Vec<ContentPart> = serde_json::from_str(payload).expect("content parts");
    assert!(matches!(parts.first(), Some(ContentPart::Text { .. })));
    let media = parts
        .iter()
        .find_map(|part| match part {
            ContentPart::Media {
                mime_type,
                data,
                url,
                ..
            } => Some((mime_type, data, url)),
            _ => None,
        })
        .expect("image media part");
    assert_eq!(media.0, "image/png");
    assert!(media.1.as_deref().is_some_and(|data| !data.is_empty()));
    assert!(media.2.is_none());
}

#[tokio::test]
async fn script_oversized_stdout_is_truncated_for_model_output() {
    let backend = Arc::new(FakeBackend::default());
    backend.set_script_text("x".repeat(MAX_INLINE_BROWSER_SCRIPT_STDOUT_BYTES + 5_000));
    let tool = tool_with(Arc::clone(&backend));

    let req = BrowserRequest::execute("sess-1", "document.body.innerText", false);
    let out = run_direct(&tool, &req).await.unwrap();

    assert_eq!(out.exit_code, 0);
    assert!(
        out.stdout.len() < MAX_INLINE_BROWSER_SCRIPT_STDOUT_BYTES + 1_000,
        "stdout should be capped, got {} bytes",
        out.stdout.len()
    );
    assert!(
        out.stdout.contains("[browser_script stdout truncated"),
        "stdout: {}",
        out.stdout
    );
    assert!(
        !out.stdout
            .contains(&"x".repeat(MAX_INLINE_BROWSER_SCRIPT_STDOUT_BYTES + 100)),
        "uncapped browser_script output leaked into model stdout"
    );
}

#[tokio::test]
async fn script_truncated_structured_output_preserves_summary_first() {
    let backend = Arc::new(FakeBackend::default());
    backend.script_summary.lock().unwrap().push(json!({
        "kind": "extracted",
        "message": "Read 40 candidate rows",
        "output_label": "candidate_rows"
    }));
    backend.script_outputs.lock().unwrap().push(json!({
        "label": "candidate_rows",
        "value": "x".repeat(MAX_INLINE_BROWSER_SCRIPT_STDOUT_BYTES + 8_000)
    }));
    let tool = tool_with(Arc::clone(&backend));

    let req = BrowserRequest::execute("sess-1", "emit_output(rows, label='candidate_rows')", false);
    let out = run_direct(&tool, &req).await.unwrap();

    assert_eq!(out.exit_code, 0);
    assert!(
        out.stdout.contains("summary:"),
        "summary should remain visible before large raw output: {}",
        out.stdout
    );
    assert!(
        out.stdout.contains("Read 40 candidate rows"),
        "stdout: {}",
        out.stdout
    );
    assert!(
        out.stdout.contains("[browser_script stdout truncated"),
        "stdout: {}",
        out.stdout
    );
    assert!(
        out.stdout
            .contains("Use a narrower browser_script extraction"),
        "stdout: {}",
        out.stdout
    );
    assert!(
        out.stdout.find("summary:") < out.stdout.find("outputs:"),
        "summary should precede raw outputs: {}",
        out.stdout
    );
}

#[test]
fn browser_script_stdout_cap_defaults_to_four_kib_for_eval_cost() {
    assert_eq!(MAX_INLINE_BROWSER_SCRIPT_STDOUT_BYTES, 4 * 1024);
}

#[tokio::test]
async fn script_unreadable_images_warn_in_stdout() {
    let temp = tempfile::tempdir().expect("tempdir");
    let missing_path = temp.path().join("missing.png");

    let backend = Arc::new(FakeBackend::default());
    backend.script_images.lock().unwrap().push(json!({
        "path": missing_path,
        "mime_type": "image/png",
        "detail": "auto",
        "label": "missing",
    }));
    let tool = tool_with(Arc::clone(&backend));

    let req = BrowserRequest::execute("sess-1", "capture_screenshot()", false);
    let out = run_direct(&tool, &req).await.unwrap();
    assert!(
        out.stdout
            .contains("Warning: image artifact could not be read:"),
        "stdout: {}",
        out.stdout
    );
    assert!(out.stdout.contains("missing.png"), "stdout: {}", out.stdout);
    assert!(
        !out.stdout.contains(BROWSER_SCRIPT_CONTENT_STDOUT_PREFIX),
        "unreadable-only images should not emit a media marker: {}",
        out.stdout
    );
}

#[tokio::test]
async fn script_oversized_png_images_warn_in_stdout_without_media_payload() {
    let temp = tempfile::tempdir().expect("tempdir");
    let image_path = temp.path().join("wide.png");
    let mut png = vec![0_u8; 24];
    png[0..8].copy_from_slice(b"\x89PNG\r\n\x1a\n");
    png[12..16].copy_from_slice(b"IHDR");
    png[16..20].copy_from_slice(&8001_u32.to_be_bytes());
    png[20..24].copy_from_slice(&600_u32.to_be_bytes());
    std::fs::write(&image_path, png).expect("write png");

    let backend = Arc::new(FakeBackend::default());
    backend.script_images.lock().unwrap().push(json!({
        "path": image_path,
        "mime_type": "image/png",
        "detail": "auto",
        "label": "wide",
    }));
    let tool = tool_with(Arc::clone(&backend));

    let req = BrowserRequest::execute("sess-1", "capture_screenshot()", false);
    let out = run_direct(&tool, &req).await.unwrap();
    assert!(
        out.stdout
            .contains("dimensions 8001x600 exceed provider limit"),
        "stdout: {}",
        out.stdout
    );
    assert!(
        out.stdout.contains("artifact remains at"),
        "stdout: {}",
        out.stdout
    );
    assert!(
        !out.stdout.contains(BROWSER_SCRIPT_CONTENT_STDOUT_PREFIX),
        "oversized-only images should not emit a media marker: {}",
        out.stdout
    );
}

#[tokio::test]
async fn default_artifact_dir_comes_from_tool_ctx_artifact_root() {
    let backend = Arc::new(FakeBackend::default());
    let tool = tool_with(Arc::clone(&backend));
    let temp = tempfile::tempdir().expect("tempdir");
    let cwd = temp.path().join("repo");
    let artifact_root = temp.path().join("state").join("artifacts").join("sess-1");
    let ctx = ToolCtx {
        call_id: "call-browser".to_string(),
        tool_name: "browser_script".to_string(),
        cwd: cwd.clone(),
        artifact_root: artifact_root.clone(),
    };

    let req = BrowserRequest::execute("sess-1", "click('#go')", false);
    let _ = run_direct_with_ctx(&tool, &req, &ctx).await.unwrap();

    let (seen_cwd, seen_artifact_dir) = backend.last_paths().expect("backend paths");
    assert_eq!(seen_cwd, cwd);
    assert_eq!(seen_artifact_dir, artifact_root);
}

// (2) The compatibility `background` flag is ignored: main always starts scripts.
#[tokio::test]
async fn script_background_compat_also_routes_to_start_script() {
    let backend = Arc::new(FakeBackend::default());
    let tool = tool_with(Arc::clone(&backend));

    let req = BrowserRequest::execute("sess-1", "longRunning()", true);
    let out = run_direct(&tool, &req).await.unwrap();

    assert_eq!(
        backend.last(),
        LastCall::StartScript("longRunning()".to_string())
    );
    assert_eq!(out.exit_code, 0);
    assert!(
        out.stdout.contains("action=\"observe\""),
        "stdout: {}",
        out.stdout
    );
}

// (2) Observe routes to observe_script.
#[tokio::test]
async fn observe_routes_to_observe_script() {
    let backend = Arc::new(FakeBackend::default());
    let tool = tool_with(Arc::clone(&backend));

    let req = BrowserRequest {
        action: BrowserAction::Observe {
            run_id: "run-1".to_string(),
        },
        session_id: "sess-1".to_string(),
        cwd: None,
        artifact_dir: None,
        timeout_secs: None,
        observe_timeout_ms: None,
    };
    let out = run_direct(&tool, &req).await.unwrap();

    assert_eq!(backend.last(), LastCall::Observe("run-1".to_string()));
    assert_eq!(
        backend.last_observe_timeout_ms(),
        Some(DEFAULT_OBSERVE_TIMEOUT_MS)
    );
    assert_eq!(out.exit_code, 0);
}

#[tokio::test]
async fn observe_timeout_is_clamped_to_coarse_poll_window() {
    let backend = Arc::new(FakeBackend::default());
    let tool = tool_with(Arc::clone(&backend));

    let mut req = BrowserRequest {
        action: BrowserAction::Observe {
            run_id: "run-1".to_string(),
        },
        session_id: "sess-1".to_string(),
        cwd: None,
        artifact_dir: None,
        timeout_secs: None,
        observe_timeout_ms: Some(5_000),
    };
    let out = run_direct(&tool, &req).await.unwrap();
    assert_eq!(out.exit_code, 0);
    assert_eq!(
        backend.last_observe_timeout_ms(),
        Some(DEFAULT_OBSERVE_TIMEOUT_MS)
    );

    req.observe_timeout_ms = Some(180_000);
    let out = run_direct(&tool, &req).await.unwrap();
    assert_eq!(out.exit_code, 0);
    assert_eq!(
        backend.last_observe_timeout_ms(),
        Some(MAX_OBSERVE_TIMEOUT_MS)
    );
}

// (2) Cancel routes to cancel_script.
#[tokio::test]
async fn cancel_routes_to_cancel_script() {
    let backend = Arc::new(FakeBackend::default());
    let tool = tool_with(Arc::clone(&backend));

    let req = BrowserRequest {
        action: BrowserAction::Cancel {
            run_id: "run-1".to_string(),
        },
        session_id: "sess-1".to_string(),
        cwd: None,
        artifact_dir: None,
        timeout_secs: None,
        observe_timeout_ms: None,
    };
    let out = run_direct(&tool, &req).await.unwrap();

    assert_eq!(backend.last(), LastCall::Cancel("run-1".to_string()));
    // cancel returns !ok in the fake -> exit code 1.
    assert_eq!(out.exit_code, 1);
}

// (3) parallel_safe = false, and approval/sandbox accessors are sane.
#[test]
fn browser_is_not_parallel_safe() {
    let tool = BrowserTool::with_backend(Arc::new(FakeBackend::default()));
    let req = BrowserRequest::command("sess-1", "screenshot");
    assert!(!tool.parallel_safe(&req));
    assert_eq!(tool.approval_keys(&req).len(), 1);
    assert!(tool.exec_approval_requirement(&req).is_none());
    assert_eq!(
        tool.sandbox_permissions(&req),
        SandboxPermissions::UseDefault
    );
}

// (4) Error from backend -> ToolError::Other.
#[tokio::test]
async fn backend_error_maps_to_tool_error() {
    let backend = Arc::new(FakeBackend {
        fail: true,
        ..Default::default()
    });
    let tool = tool_with(Arc::clone(&backend));

    let req = BrowserRequest::command("sess-1", "go x");
    let err = run_direct(&tool, &req).await.unwrap_err();
    assert!(matches!(err, ToolError::Other(_)), "got {err:?}");
}

#[tokio::test]
async fn observe_unknown_run_maps_to_error() {
    let backend = Arc::new(FakeBackend {
        fail: true,
        ..Default::default()
    });
    let tool = tool_with(Arc::clone(&backend));

    let req = BrowserRequest {
        action: BrowserAction::Observe {
            run_id: "missing".to_string(),
        },
        session_id: "sess-1".to_string(),
        cwd: None,
        artifact_dir: None,
        timeout_secs: None,
        observe_timeout_ms: None,
    };
    let err = run_direct(&tool, &req).await.unwrap_err();
    assert!(matches!(err, ToolError::Other(_)), "got {err:?}");
}

// Validation: empty command/run_id are rejected before touching backend; an
// empty request session can fall back to the runtime context session id.
#[tokio::test]
async fn empty_command_rejected_without_calling_backend() {
    let backend = Arc::new(FakeBackend::default());
    let tool = tool_with(Arc::clone(&backend));

    let req = BrowserRequest::command("sess-1", "   ");
    let err = run_direct(&tool, &req).await.unwrap_err();
    assert!(matches!(err, ToolError::Rejected(_)), "got {err:?}");
    assert_eq!(backend.last(), LastCall::None);
}

#[tokio::test]
async fn empty_session_id_rejected() {
    let backend = Arc::new(FakeBackend::default());
    let tool = tool_with(Arc::clone(&backend));

    let req = BrowserRequest::command("", "go x");
    let err = run_direct_with_ctx(&tool, &req, &ctx_with_call_id(""))
        .await
        .unwrap_err();
    assert!(matches!(err, ToolError::Rejected(_)), "got {err:?}");
    assert_eq!(backend.last(), LastCall::None);
}

#[tokio::test]
async fn empty_request_session_uses_context_session_id() {
    let backend = Arc::new(FakeBackend::default());
    let tool = tool_with(Arc::clone(&backend));

    let req = BrowserRequest::command("", "go x");
    let out = run_direct_with_ctx(&tool, &req, &ctx_with_call_id("sess-from-ctx"))
        .await
        .unwrap();
    assert_eq!(out.exit_code, 0);
    assert_eq!(backend.last(), LastCall::Command("go x".to_string()));
}

#[tokio::test]
async fn configured_session_id_keeps_tool_call_id_for_persistence() {
    let backend = Arc::new(FakeBackend::default());
    *backend.active_local_profile_id.lock().unwrap() = Some("google-chrome:Default".to_string());
    backend.set_browser_page_ready(true);
    let (_dir, store, session) = shared_store();
    {
        let store = store.lock().unwrap();
        store.set_setting("browser", "Local Chrome").unwrap();
        store
            .set_setting("browser.preference.profile", "google-chrome:Default")
            .unwrap();
    }
    let tool = tool_with(Arc::clone(&backend))
        .with_session_id("agent-session")
        .with_persistence(store.clone(), session.clone());

    let mut req = BrowserRequest::execute("", "extract()", false);
    req.session_id.clear();
    let out = run_direct_with_ctx(
        &tool,
        &req,
        &ctx_for_tool("browser_script", "model-call-123"),
    )
    .await
    .unwrap();

    assert_eq!(out.exit_code, 0);
    assert_eq!(backend.last_session().as_deref(), Some("agent-session"));
    let events = store.lock().unwrap().events_for_session(&session).unwrap();
    let output = events
        .iter()
        .find(|event| event.event_type == "tool.output")
        .expect("browser_script tool.output");
    assert_eq!(output.payload["name"], "browser_script");
    assert_eq!(output.payload["tool_call_id"], "model-call-123");
    assert!(
        output.payload["text"]
            .as_str()
            .is_some_and(|text| text.starts_with("script-output\nrun_id: run-1")),
        "unexpected browser_script output text: {:?}",
        output.payload["text"]
    );
}

#[tokio::test]
async fn empty_run_id_rejected() {
    let backend = Arc::new(FakeBackend::default());
    let tool = tool_with(Arc::clone(&backend));

    let req = BrowserRequest {
        action: BrowserAction::Observe {
            run_id: "".to_string(),
        },
        session_id: "sess-1".to_string(),
        cwd: None,
        artifact_dir: None,
        timeout_secs: None,
        observe_timeout_ms: None,
    };
    let err = run_direct(&tool, &req).await.unwrap_err();
    assert!(matches!(err, ToolError::Rejected(_)), "got {err:?}");
    assert_eq!(backend.last(), LastCall::None);
}

// (5) Orchestrator-driven run with the fake backend (no Bun/Chrome).
#[tokio::test]
async fn orchestrated_command_runs_under_none() {
    let backend = Arc::new(FakeBackend::default());
    let tool = tool_with(Arc::clone(&backend));
    let orch = ToolOrchestrator::new(NoneSandboxProvider, AutoApprover);
    let env = TurnEnv {
        file_system_sandbox_policy: FileSystemSandboxPolicy {
            restricted: false,
            denied_read: false,
        },
        managed_network_active: false,
        strict_auto_review: false,
        use_guardian: false,
    };

    let req = BrowserRequest::command("sess-1", "screenshot");
    let result = orch
        .run(&tool, &req, &ctx(), &env, AskForApproval::Never)
        .await
        .expect("orchestration ok");

    assert_eq!(result.sandbox_used, SandboxType::None);
    assert_eq!(result.output.exit_code, 0);
    assert!(result.output.stdout.contains("navigated"));
    assert_eq!(backend.last(), LastCall::Command("screenshot".to_string()));
}
