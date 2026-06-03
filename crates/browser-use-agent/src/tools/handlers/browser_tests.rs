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

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use browser_use_browser::{BrowserCommandOutput, BrowserScriptOutput};
use browser_use_llm::schema::ContentPart;
use browser_use_store::Store;
use serde_json::json;

use super::browser::{
    BrowserAction, BrowserBackend, BrowserRequest, BrowserTool,
    BROWSER_SCRIPT_CONTENT_STDOUT_PREFIX,
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
    last_session: Mutex<Option<String>>,
    last_paths: Mutex<Option<(PathBuf, PathBuf)>>,
    last_timeout_secs: Mutex<Option<u64>>,
    script_images: Mutex<Vec<serde_json::Value>>,
    fail: bool,
}

struct SlowCommandBackend;

impl BrowserBackend for SlowCommandBackend {
    fn command(
        &self,
        _session_id: &str,
        _cwd: &std::path::Path,
        _artifact_dir: &std::path::Path,
        _command: &str,
    ) -> anyhow::Result<BrowserCommandOutput> {
        std::thread::sleep(std::time::Duration::from_secs(2));
        Ok(FakeBackend::ok_command())
    }

    fn run_script(
        &self,
        _session_id: &str,
        _cwd: &std::path::Path,
        _artifact_dir: &std::path::Path,
        _code: &str,
        _timeout_secs: u64,
    ) -> anyhow::Result<BrowserScriptOutput> {
        anyhow::bail!("not used")
    }

    fn start_script(
        &self,
        _session_id: &str,
        _cwd: &std::path::Path,
        _artifact_dir: &std::path::Path,
        _code: &str,
        _timeout_secs: u64,
    ) -> anyhow::Result<BrowserScriptOutput> {
        anyhow::bail!("not used")
    }

    fn observe_script(
        &self,
        _session_id: &str,
        _run_id: &str,
        _observe_timeout_ms: u64,
    ) -> anyhow::Result<BrowserScriptOutput> {
        anyhow::bail!("not used")
    }

    fn cancel_script(
        &self,
        _session_id: &str,
        _run_id: &str,
    ) -> anyhow::Result<BrowserScriptOutput> {
        anyhow::bail!("not used")
    }
}

impl FakeBackend {
    fn last(&self) -> LastCall {
        self.last.lock().unwrap().clone()
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

    fn ok_script(status: Option<&str>, ok: bool) -> BrowserScriptOutput {
        BrowserScriptOutput {
            ok,
            status: status.map(|s| s.to_string()),
            run_id: Some("run-1".to_string()),
            text: "script-output".to_string(),
            ..Default::default()
        }
    }

    fn ok_script_with_images(&self, status: Option<&str>, ok: bool) -> BrowserScriptOutput {
        BrowserScriptOutput {
            images: self.script_images(),
            ..Self::ok_script(status, ok)
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
        self.record_paths(cwd, artifact_dir);
        if self.fail {
            anyhow::bail!("boom");
        }
        Ok(Self::ok_command())
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
        _observe_timeout_ms: u64,
    ) -> anyhow::Result<BrowserScriptOutput> {
        *self.last_session.lock().unwrap() = Some(session_id.to_string());
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
async fn cancelled_browser_task_returns_before_blocking_backend_finishes() {
    let tool = BrowserTool::with_backend(Arc::new(SlowCommandBackend));
    let req = BrowserRequest::command("sess-1", "go https://example.com");
    let launch = none_launch();
    let cancel = tokio_util::sync::CancellationToken::new();
    cancel.cancel();
    let attempt = SandboxAttempt {
        sandbox: SandboxType::None,
        permissions: SandboxPermissions::UseDefault,
        enforce_managed_network: false,
        launch: &launch,
        cancel: Some(cancel),
    };

    let err = tokio::time::timeout(
        std::time::Duration::from_millis(500),
        tool.run(&req, &attempt, &ctx()),
    )
    .await
    .expect("outer cancellation should win")
    .expect_err("cancelled browser task should error");

    match err {
        ToolError::Other(error) => assert!(
            error.to_string().contains("cancelled"),
            "expected cancellation error, got {error:#}"
        ),
        other => panic!("expected Other cancellation error, got {other:?}"),
    }
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
    assert_eq!(out.exit_code, 0);
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
    let (_dir, store, session) = shared_store();
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
    assert_eq!(output.payload["text"], "script-output");
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
