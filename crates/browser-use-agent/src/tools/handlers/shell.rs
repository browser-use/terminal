//! Shell / exec tool: spawns a process asynchronously, applies a timeout, and
//! captures byte-capped stdout/stderr.
//!
//! This is the async re-implementation of codex's shell tool over our merged
//! [`ToolRuntime`](crate::tools::runtime::ToolRuntime) seam. It implements the
//! full trait stack ([`Approvable`] + [`Sandboxable`] + [`ToolRuntime`]) so it
//! can be driven by the [`ToolOrchestrator`](crate::tools::orchestrator::ToolOrchestrator).
//!
//! # Parity grounding (file:line in `/home/exedev/repos/codex/codex-rs`)
//!
//! * **Default timeout** `10_000` ms — codex `DEFAULT_EXEC_COMMAND_TIMEOUT_MS`
//!   (core/src/exec.rs:51) and legacy `DEFAULT_SHELL_COMMAND_TIMEOUT_MS`
//!   (browser-use-core command.rs:26).
//! * **Timeout exit code** `124` — codex `EXEC_TIMEOUT_EXIT_CODE`
//!   (exec.rs:58); codex sets `exit_code = 124` on timeout (exec.rs:746-748),
//!   and the legacy impl uses `SHELL_COMMAND_TIMEOUT_EXIT_CODE = 124`
//!   (command.rs:39). We follow the legacy/`exec.rs`-exit-code shape: on timeout
//!   we kill the child and return an [`ExecOutput`] with `exit_code = 124` (the
//!   `ToolError` enum here has no `Timeout` variant; codex's
//!   `SandboxErr::Timeout` is a sandbox-subsystem concept not in this WP's seam).
//! * **Output byte cap** `1 MiB` per stream — codex `EXEC_OUTPUT_MAX_BYTES =
//!   DEFAULT_OUTPUT_BYTES_CAP` (exec.rs:68) and legacy
//!   `UNIFIED_EXEC_OUTPUT_MAX_BYTES = 1024 * 1024` (command.rs:33).
//! * **Drain-to-EOF after cap** — mirrors codex `read_output` (exec.rs:1441-1475)
//!   / `append_capped` (exec.rs:856-864): keep reading past the cap to avoid
//!   back-pressure / SIGPIPE on the child, but retain only the first cap bytes.
//! * **I/O drain guard on timeout** — codex bounds the post-kill drain by
//!   `IO_DRAIN_TIMEOUT_MS = 2_000` (exec.rs:81); we apply the same bound when
//!   collecting output after a timeout kill.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use crate::tools::approval::ExecApprovalRequirement;
use crate::tools::runtime::{
    Approvable, ExecOutput, SandboxAttempt, Sandboxable, ToolCtx, ToolError, ToolRuntime,
};
use crate::tools::sandbox::{SandboxPermissions, SandboxPreference};
use crate::tools::unified_exec::{
    SpawnProcessRequest, UnifiedExecEventEmitter, UnifiedExecManager,
    WriteStdinRequest as UnifiedWriteStdinRequest, DEFAULT_EXEC_YIELD_TIME_MS,
    DEFAULT_MAX_OUTPUT_TOKENS, DEFAULT_WRITE_STDIN_YIELD_TIME_MS,
};

/// Default per-command timeout in milliseconds.
///
/// Matches codex `DEFAULT_EXEC_COMMAND_TIMEOUT_MS` (exec.rs:51) and legacy
/// `DEFAULT_SHELL_COMMAND_TIMEOUT_MS` (command.rs:26).
pub const DEFAULT_SHELL_COMMAND_TIMEOUT_MS: u64 = 10_000;
pub const DEFAULT_EXEC_COMMAND_TIMEOUT_MS: u64 = 600_000;

/// Exit code reported when a command is killed for exceeding its timeout.
///
/// Matches codex `EXEC_TIMEOUT_EXIT_CODE` (exec.rs:58) and legacy
/// `SHELL_COMMAND_TIMEOUT_EXIT_CODE` (command.rs:39) — the conventional shell
/// exit status for `timeout(1)`-killed processes.
pub const TIMEOUT_EXIT_CODE: i32 = 124;

/// Maximum number of bytes retained per output stream (1 MiB).
///
/// Matches codex `EXEC_OUTPUT_MAX_BYTES` (= `DEFAULT_OUTPUT_BYTES_CAP`,
/// exec.rs:68) and legacy `UNIFIED_EXEC_OUTPUT_MAX_BYTES = 1024 * 1024`
/// (command.rs:33).
pub const MAX_STREAM_OUTPUT_BYTES: usize = 1024 * 1024;

/// Typed request for the shell/exec tool.
///
/// Field shape follows codex `ShellRequest`/`ExecParams` (core/src/tools/runtimes/shell.rs:48-64,
/// exec.rs:83-96): a tokenized command vector, an optional working directory, an
/// optional timeout, and an environment-variable map.
///
/// # Wire shape (model-facing args)
///
/// ```json
/// { "command": ["bash", "-lc", "echo hi"], "cwd": "/repo", "timeout_ms": 5000 }
/// ```
///
/// Deserializes directly from the model's argument object. The field names match
/// codex's `ExecParams` JSON (`command`/`cwd`/`timeout_ms`/`env`, exec.rs:83-96)
/// and the legacy shell spec (`browser-use-core/src/tools/mod.rs`). All fields
/// except `command` are optional on the wire: `cwd`/`timeout_ms` default to
/// `None` and `env` defaults to an empty map (codex `ExecParams.env` is likewise
/// an absent-means-empty map).
#[derive(Clone, Debug, PartialEq, Eq, serde::Deserialize)]
pub struct ShellRequest {
    /// The command and its arguments, already tokenized (argv-style). The first
    /// element is the program; the rest are arguments.
    pub command: Vec<String>,
    /// Working directory to run in. When `None`, the [`ToolCtx::cwd`] is used.
    #[serde(default)]
    pub cwd: Option<PathBuf>,
    /// Per-command timeout in milliseconds. When `None`,
    /// [`DEFAULT_SHELL_COMMAND_TIMEOUT_MS`] is used.
    #[serde(default)]
    pub timeout_ms: Option<u64>,
    /// Extra environment variables to set for the child process. Layered on top
    /// of the inherited environment (codex `ExecParams.env`).
    #[serde(default)]
    pub env: HashMap<String, String>,
    /// Maximum model-visible output tokens. Defaults to the same budget used by
    /// `exec_command`.
    #[serde(default)]
    pub max_output_tokens: Option<usize>,
}

impl ShellRequest {
    /// Convenience constructor from an argv slice, using context defaults for
    /// cwd, timeout, and env.
    pub fn from_argv<I, S>(argv: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            command: argv.into_iter().map(Into::into).collect(),
            cwd: None,
            timeout_ms: None,
            env: HashMap::new(),
            max_output_tokens: None,
        }
    }

    /// The effective timeout for this request.
    fn effective_timeout_ms(&self) -> u64 {
        self.timeout_ms.unwrap_or(DEFAULT_SHELL_COMMAND_TIMEOUT_MS)
    }
}

fn truncate_model_text(text: &str, max_output_tokens: Option<usize>) -> String {
    let byte_budget = max_output_tokens
        .unwrap_or(DEFAULT_MAX_OUTPUT_TOKENS)
        .saturating_mul(4);
    if text.len() <= byte_budget {
        return text.to_string();
    }
    if byte_budget == 0 {
        return String::new();
    }
    let mut end = byte_budget.min(text.len());
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n…\n", &text[..end])
}

/// A simple, conservative denylist of obviously destructive commands.
///
/// PARITY CAVEAT: the legacy impl used a tree-sitter-based
/// `dangerous_command_rejection` (command.rs:2645) that parses the shell AST via
/// `tree-sitter-bash` to catch destructive forms (`rm -rf /`, `rm -rf ~`,
/// `sudo rm`, nested in `sh -lc "..."` / pipelines) — see
/// `command_words_might_be_dangerous` (command.rs:2850). Faithfully porting that
/// parser (and pulling in the `tree-sitter` dep) is a later WP; see the TODO
/// below. This is a SIMPLE substring/token heuristic that catches the most
/// egregious whole-filesystem / home deletions, including the common
/// `<shell> -c "..."` and `sudo` wrappings. It is intentionally narrow to avoid
/// false positives on scoped relative paths.
///
/// TODO(WP-T-shell-denylist): port the tree-sitter `dangerous_command_rejection`
/// for full parity (pipeline/`sh -lc` AST awareness, the read-only safelist, and
/// the exact codex rm subset semantics).
pub fn dangerous_command_rejection(command: &[String]) -> Result<(), ToolError> {
    if command.is_empty() {
        return Ok(());
    }

    // Strip a leading `sudo` (legacy: `command_words_might_be_dangerous`
    // recurses through `sudo`, command.rs:2859).
    let effective: &[String] = if base_name(&command[0]) == "sudo" && command.len() > 1 {
        &command[1..]
    } else {
        command
    };

    let mut candidates: Vec<String> = vec![effective.join(" ")];
    // For `<shell> -c "<script>"` / `-lc`, also inspect the script payload.
    if effective.len() >= 3
        && matches!(
            base_name(&effective[0]).as_str(),
            "sh" | "bash" | "zsh" | "dash"
        )
        && matches!(effective[1].as_str(), "-c" | "-lc")
    {
        candidates.push(effective[2].clone());
    }

    for cand in &candidates {
        let norm = cand.split_whitespace().collect::<Vec<_>>().join(" ");
        if is_root_wipe(&norm) {
            return Err(ToolError::Rejected(format!(
                "refusing to run destructive command (matched denylist): {cand}"
            )));
        }
    }
    Ok(())
}

/// The trailing path component of a program name (so `/bin/rm` -> `rm`).
fn base_name(s: &str) -> String {
    s.rsplit('/').next().unwrap_or(s).to_string()
}

fn shell_argv(cmd: &str, shell: Option<&str>, login: Option<bool>) -> Vec<String> {
    #[cfg(windows)]
    {
        let shell = shell.unwrap_or("cmd");
        vec![shell.to_string(), "/C".to_string(), cmd.to_string()]
    }
    #[cfg(not(windows))]
    {
        let shell = shell
            .map(ToOwned::to_owned)
            .or_else(|| std::env::var("SHELL").ok())
            .unwrap_or_else(|| "sh".to_string());
        let flag = if login.unwrap_or(true) { "-lc" } else { "-c" };
        vec![shell, flag.to_string(), cmd.to_string()]
    }
}

/// Whether a normalized (whitespace-collapsed) command string looks like a
/// recursive-force delete of a filesystem root / home directory.
fn is_root_wipe(norm: &str) -> bool {
    let mut tokens = norm.split_whitespace();
    let Some(first) = tokens.next() else {
        return false;
    };
    if base_name(first) != "rm" {
        return false;
    }
    let recursive_force = norm.contains(" -rf")
        || norm.contains(" -fr")
        || norm.contains(" -rF")
        || norm.contains(" -fR")
        || (norm.contains(" -r") && norm.contains(" -f"))
        || norm.contains(" --recursive");
    if !recursive_force {
        return false;
    }
    // Root-ish targets: `/`, `/*`, `~`, `~/`, `$HOME`, `${HOME}`.
    const ROOTS: &[&str] = &["/", "/*", "~", "~/", "$HOME", "${HOME}"];
    norm.split_whitespace()
        .skip(1)
        .any(|tok| ROOTS.contains(&tok))
}

/// The async shell/exec tool.
///
/// Stateless; cheap to clone/construct. Limits and timeouts come from the
/// request and the constants above.
#[derive(Clone, Debug)]
pub struct ShellTool {
    manager: UnifiedExecManager,
    emitter: Option<Arc<UnifiedExecEventEmitter>>,
}

impl Default for ShellTool {
    fn default() -> Self {
        Self::new()
    }
}

impl ShellTool {
    /// Construct a new shell tool.
    pub fn new() -> Self {
        Self {
            manager: UnifiedExecManager::default(),
            emitter: None,
        }
    }

    pub fn with_manager(manager: UnifiedExecManager) -> Self {
        Self {
            manager,
            emitter: None,
        }
    }

    pub fn with_event_emitter(mut self, emitter: Arc<UnifiedExecEventEmitter>) -> Self {
        self.emitter = Some(emitter);
        self
    }
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Deserialize)]
#[serde(untagged)]
pub enum ExecCommandValue {
    Argv(Vec<String>),
    Shell(String),
}

/// Codex-style command execution request.
#[derive(Clone, Debug, PartialEq, Eq, serde::Deserialize)]
pub struct ExecCommandRequest {
    /// Shell command string. Mirrors Codex's `cmd` field.
    #[serde(default)]
    pub cmd: Option<String>,
    /// Alternate command field. Accepts either argv or a shell string.
    #[serde(default)]
    pub command: Option<ExecCommandValue>,
    /// Working directory. `workdir` matches the API tool; `cwd` is accepted for
    /// compatibility with the older shell tool.
    #[serde(default)]
    pub workdir: Option<PathBuf>,
    #[serde(default)]
    pub cwd: Option<PathBuf>,
    /// Optional shell binary to launch.
    #[serde(default)]
    pub shell: Option<String>,
    /// Whether to use login shell semantics where supported.
    #[serde(default)]
    pub login: Option<bool>,
    /// Initial output collection window before returning a process id.
    #[serde(default)]
    pub yield_time_ms: Option<u64>,
    /// Maximum wall time before the process tree is killed. Defaults to a
    /// generous cap so one forgotten command cannot pin the whole run.
    #[serde(default)]
    pub timeout_ms: Option<u64>,
    /// Maximum model-visible output tokens.
    #[serde(default)]
    pub max_output_tokens: Option<usize>,
    /// Extra env is accepted for backwards compatibility. It is not advertised
    /// in the Codex-style schema.
    #[serde(default)]
    pub env: HashMap<String, String>,
    /// Allocate a PTY for stdin-capable interactive command sessions.
    #[serde(default)]
    pub tty: bool,
}

impl ExecCommandRequest {
    fn argv(&self) -> Vec<String> {
        if let Some(cmd) = self.cmd.as_ref().filter(|cmd| !cmd.trim().is_empty()) {
            return shell_argv(cmd, self.shell.as_deref(), self.login);
        }
        match self.command.as_ref() {
            Some(ExecCommandValue::Argv(argv)) => argv.clone(),
            Some(ExecCommandValue::Shell(cmd)) => {
                shell_argv(cmd, self.shell.as_deref(), self.login)
            }
            None => Vec::new(),
        }
    }

    fn cwd(&self, ctx: &ToolCtx) -> PathBuf {
        self.workdir
            .clone()
            .or_else(|| self.cwd.clone())
            .unwrap_or_else(|| ctx.cwd.clone())
    }

    fn yield_time_ms(&self) -> u64 {
        self.yield_time_ms.unwrap_or(DEFAULT_EXEC_YIELD_TIME_MS)
    }

    fn timeout_ms(&self) -> u64 {
        self.timeout_ms.unwrap_or(DEFAULT_EXEC_COMMAND_TIMEOUT_MS)
    }
}

/// Send stdin to, or poll, a live unified exec process.
#[derive(Clone, Debug, PartialEq, Eq, serde::Deserialize)]
pub struct WriteStdinRequest {
    pub session_id: i32,
    #[serde(default)]
    pub chars: String,
    #[serde(default)]
    pub yield_time_ms: Option<u64>,
    #[serde(default)]
    pub max_output_tokens: Option<usize>,
}

impl WriteStdinRequest {
    fn yield_time_ms(&self) -> u64 {
        self.yield_time_ms
            .unwrap_or(DEFAULT_WRITE_STDIN_YIELD_TIME_MS)
    }
}

#[derive(Clone, Debug)]
pub struct ExecCommandTool {
    manager: UnifiedExecManager,
    emitter: Option<Arc<UnifiedExecEventEmitter>>,
}

impl ExecCommandTool {
    pub fn new(manager: UnifiedExecManager) -> Self {
        Self {
            manager,
            emitter: None,
        }
    }

    pub fn with_event_emitter(mut self, emitter: Arc<UnifiedExecEventEmitter>) -> Self {
        self.emitter = Some(emitter);
        self
    }
}

#[derive(Clone, Debug)]
pub struct WriteStdinTool {
    manager: UnifiedExecManager,
    emitter: Option<Arc<UnifiedExecEventEmitter>>,
}

impl WriteStdinTool {
    pub fn new(manager: UnifiedExecManager) -> Self {
        Self {
            manager,
            emitter: None,
        }
    }

    pub fn with_event_emitter(mut self, emitter: Arc<UnifiedExecEventEmitter>) -> Self {
        self.emitter = Some(emitter);
        self
    }
}

/// Approval key: the command + cwd identify a shell call for session caching.
///
/// Codex parity: `ApprovalKey` in runtimes/shell.rs:86-92 keys on the command +
/// cwd (+ sandbox permission selection, which is uniform here).
#[derive(serde::Serialize, Clone, Debug, Eq, PartialEq, Hash)]
pub struct ShellApprovalKey {
    command: Vec<String>,
    cwd: Option<PathBuf>,
}

#[derive(serde::Serialize, Clone, Debug, Eq, PartialEq, Hash)]
pub struct ExecCommandApprovalKey {
    command: Vec<String>,
    cwd: Option<PathBuf>,
}

#[derive(serde::Serialize, Clone, Debug, Eq, PartialEq, Hash)]
pub struct WriteStdinApprovalKey {
    session_id: i32,
}

impl Approvable<ShellRequest> for ShellTool {
    type ApprovalKey = ShellApprovalKey;

    fn approval_keys(&self, req: &ShellRequest) -> Vec<Self::ApprovalKey> {
        vec![ShellApprovalKey {
            command: req.command.clone(),
            cwd: req.cwd.clone(),
        }]
    }

    /// The shell may write anywhere under its working directory; request the
    /// default sandbox permissions (no escalation). Codex parity: the shell
    /// runtime returns the request's `sandbox_permissions` (runtimes/shell.rs:195),
    /// which defaults to `UseDefault` for an un-escalated command.
    fn sandbox_permissions(&self, _req: &ShellRequest) -> SandboxPermissions {
        SandboxPermissions::UseDefault
    }

    /// A denylisted destructive command is forbidden outright; otherwise defer
    /// to the orchestrator's policy-driven default
    /// ([`default_exec_approval_requirement`](crate::tools::runtime::default_exec_approval_requirement))
    /// by returning `None`. Codex parity: the shell runtime supplies its
    /// `exec_approval_requirement` (runtimes/shell.rs:184-186), which the exec
    /// policy populates; here only the hard-deny is tool-intrinsic.
    fn exec_approval_requirement(&self, req: &ShellRequest) -> Option<ExecApprovalRequirement> {
        if let Err(ToolError::Rejected(reason)) = dangerous_command_rejection(&req.command) {
            return Some(ExecApprovalRequirement::Forbidden { reason });
        }
        None
    }
}

impl Approvable<ExecCommandRequest> for ExecCommandTool {
    type ApprovalKey = ExecCommandApprovalKey;

    fn approval_keys(&self, req: &ExecCommandRequest) -> Vec<Self::ApprovalKey> {
        vec![ExecCommandApprovalKey {
            command: req.argv(),
            cwd: req.workdir.clone().or_else(|| req.cwd.clone()),
        }]
    }

    fn sandbox_permissions(&self, _req: &ExecCommandRequest) -> SandboxPermissions {
        SandboxPermissions::UseDefault
    }

    fn exec_approval_requirement(
        &self,
        req: &ExecCommandRequest,
    ) -> Option<ExecApprovalRequirement> {
        if let Err(ToolError::Rejected(reason)) = dangerous_command_rejection(&req.argv()) {
            return Some(ExecApprovalRequirement::Forbidden { reason });
        }
        None
    }
}

impl Approvable<WriteStdinRequest> for WriteStdinTool {
    type ApprovalKey = WriteStdinApprovalKey;

    fn approval_keys(&self, req: &WriteStdinRequest) -> Vec<Self::ApprovalKey> {
        vec![WriteStdinApprovalKey {
            session_id: req.session_id,
        }]
    }

    fn sandbox_permissions(&self, _req: &WriteStdinRequest) -> SandboxPermissions {
        SandboxPermissions::UseDefault
    }
}

impl Sandboxable for ShellTool {
    fn sandbox_preference(&self) -> SandboxPreference {
        // Let the provider decide. Today everything resolves to
        // `SandboxType::None`; real backends arrive in a later WP. Codex parity:
        // `ShellRuntime::sandbox_preference -> SandboxablePreference::Auto`
        // (runtimes/shell.rs:109-111).
        SandboxPreference::Auto
    }

    fn escalate_on_failure(&self) -> bool {
        // Codex parity: `ShellRuntime::escalate_on_failure -> true`
        // (runtimes/shell.rs:112-114).
        true
    }
}

impl Sandboxable for ExecCommandTool {
    fn sandbox_preference(&self) -> SandboxPreference {
        SandboxPreference::Auto
    }

    fn escalate_on_failure(&self) -> bool {
        true
    }
}

impl Sandboxable for WriteStdinTool {
    fn sandbox_preference(&self) -> SandboxPreference {
        SandboxPreference::Auto
    }

    fn escalate_on_failure(&self) -> bool {
        true
    }
}

#[async_trait::async_trait]
impl ToolRuntime<ShellRequest, ExecOutput> for ShellTool {
    fn parallel_safe(&self, _req: &ShellRequest) -> bool {
        // Match codex shell: serial / write-lock. The shell can mutate shared
        // state (filesystem, cwd), so it is not safe to run concurrently.
        false
    }

    async fn run(
        &self,
        req: &ShellRequest,
        attempt: &SandboxAttempt<'_>,
        ctx: &ToolCtx,
    ) -> Result<ExecOutput, ToolError> {
        // Defense in depth: reject destructive commands even if `run` is called
        // directly, bypassing the orchestrator's approval gate.
        dangerous_command_rejection(&req.command)?;

        // Working dir: explicit request cwd, else the ambient ToolCtx cwd.
        let cwd = req.cwd.clone().unwrap_or_else(|| ctx.cwd.clone());
        let timeout_ms = req.effective_timeout_ms();
        let snapshot = self
            .manager
            .run_to_completion(SpawnProcessRequest {
                argv: req.command.clone(),
                cwd,
                env: req.env.clone(),
                tty: false,
                yield_time_ms: DEFAULT_WRITE_STDIN_YIELD_TIME_MS,
                max_output_tokens: req.max_output_tokens,
                timeout_ms: Some(timeout_ms),
                kill_on_cancel: true,
                call_id: ctx.call_id.clone(),
                tool_name: ctx.tool_name.clone(),
                emitter: self.emitter.clone(),
                cancel: attempt.cancel.clone(),
            })
            .await?;
        let mut stderr = snapshot.stderr;
        if snapshot.timed_out && stderr.trim().is_empty() {
            stderr = format!("command timed out after {timeout_ms} ms");
        }
        Ok(ExecOutput {
            exit_code: snapshot.exit_code.unwrap_or(0),
            stdout: truncate_model_text(&snapshot.stdout, req.max_output_tokens),
            stderr: truncate_model_text(&stderr, req.max_output_tokens),
        })
    }
}

#[async_trait::async_trait]
impl ToolRuntime<ExecCommandRequest, ExecOutput> for ExecCommandTool {
    fn parallel_safe(&self, _req: &ExecCommandRequest) -> bool {
        true
    }

    async fn run(
        &self,
        req: &ExecCommandRequest,
        attempt: &SandboxAttempt<'_>,
        ctx: &ToolCtx,
    ) -> Result<ExecOutput, ToolError> {
        let argv = req.argv();
        dangerous_command_rejection(&argv)?;
        let snapshot = self
            .manager
            .spawn_process(SpawnProcessRequest {
                argv,
                cwd: req.cwd(ctx),
                env: req.env.clone(),
                tty: req.tty,
                yield_time_ms: req.yield_time_ms(),
                max_output_tokens: req.max_output_tokens,
                timeout_ms: Some(req.timeout_ms()),
                kill_on_cancel: false,
                call_id: ctx.call_id.clone(),
                tool_name: ctx.tool_name.clone(),
                emitter: self.emitter.clone(),
                cancel: attempt.cancel.clone(),
            })
            .await?;
        Ok(ExecOutput {
            exit_code: snapshot.exit_code.unwrap_or(0),
            stdout: snapshot.to_model_text(),
            stderr: String::new(),
        })
    }
}

#[async_trait::async_trait]
impl ToolRuntime<WriteStdinRequest, ExecOutput> for WriteStdinTool {
    fn parallel_safe(&self, _req: &WriteStdinRequest) -> bool {
        false
    }

    async fn run(
        &self,
        req: &WriteStdinRequest,
        _attempt: &SandboxAttempt<'_>,
        ctx: &ToolCtx,
    ) -> Result<ExecOutput, ToolError> {
        let snapshot = self
            .manager
            .write_stdin(UnifiedWriteStdinRequest {
                session_id: req.session_id,
                chars: req.chars.clone(),
                yield_time_ms: req.yield_time_ms(),
                max_output_tokens: req.max_output_tokens,
                call_id: ctx.call_id.clone(),
                tool_name: ctx.tool_name.clone(),
                emitter: self.emitter.clone(),
            })
            .await?;
        Ok(ExecOutput {
            exit_code: snapshot.exit_code.unwrap_or(0),
            stdout: snapshot.to_model_text(),
            stderr: String::new(),
        })
    }
}
