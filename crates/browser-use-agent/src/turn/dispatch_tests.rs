//! Tests for the async in-turn [`ToolDispatcher`] (WP-C1).
//!
//! NETWORK-FREE: every test injects a [`ScriptedRunner`] in place of the real
//! [`OrchestratorRunner`]/[`tools::ToolOrchestrator`]. No `ModelClient`, sandbox,
//! or socket is ever touched. The scripted runner:
//! - records the order in which calls are *invoked*,
//! - tracks a live concurrency counter (and its max) so a test can prove that
//!   parallel-safe calls actually overlap and serial calls don't,
//! - can sleep per-call (so a later call can finish before an earlier one,
//!   proving the output ordering is by *model order*, not completion order),
//! - returns a canned [`Message`] tagged with the call id.
//!
//! Sleeps are short (tens of ms) and deterministic; `tokio`'s `start_paused` is
//! intentionally not required, so no Cargo manifest change is needed.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use browser_use_llm::schema::{ContentPart, Message, MessageRole};
use tokio_util::sync::CancellationToken;

use crate::turn::dispatch::{CallRunner, ToolDispatcher};

// ---- scripted runner ------------------------------------------------------

/// Per-call script: how long the call sleeps and whether it is parallel-safe.
#[derive(Clone)]
struct CallScript {
    /// Tool call id (used to key the returned message + assert ordering).
    id: String,
    /// How long the call "runs" before returning.
    sleep: Duration,
    /// codex `ToolRuntime::parallel_safe` for this call.
    parallel_safe: bool,
    /// Optional canned tool output text.
    output: Option<String>,
}

/// A [`CallRunner`] that replays a per-call script and instruments concurrency.
struct ScriptedRunner {
    scripts: Mutex<std::collections::HashMap<String, CallScript>>,
    /// Order in which calls were *invoked* (push of the call id at run start).
    invocation_order: Mutex<Vec<String>>,
    /// Live count of in-flight `run` calls.
    in_flight: AtomicUsize,
    /// High-water mark of `in_flight` (proves overlap / exclusivity).
    max_concurrency: AtomicUsize,
    /// Number of `run` calls that actually started (proves cancel stops scheduling).
    runs_started: AtomicUsize,
}

impl ScriptedRunner {
    fn new(scripts: Vec<CallScript>) -> Arc<Self> {
        let map = scripts.into_iter().map(|s| (s.id.clone(), s)).collect();
        Arc::new(Self {
            scripts: Mutex::new(map),
            invocation_order: Mutex::new(Vec::new()),
            in_flight: AtomicUsize::new(0),
            max_concurrency: AtomicUsize::new(0),
            runs_started: AtomicUsize::new(0),
        })
    }

    fn max_concurrency(&self) -> usize {
        self.max_concurrency.load(Ordering::SeqCst)
    }
    fn runs_started(&self) -> usize {
        self.runs_started.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl CallRunner for ScriptedRunner {
    fn parallel_safe(&self, call: &ContentPart) -> bool {
        let id = call_id(call);
        self.scripts
            .lock()
            .unwrap()
            .get(&id)
            .map(|s| s.parallel_safe)
            .unwrap_or(false)
    }

    async fn run(&self, call: ContentPart, _cancel: CancellationToken) -> Message {
        let id = call_id(&call);
        self.runs_started.fetch_add(1, Ordering::SeqCst);
        self.invocation_order.lock().unwrap().push(id.clone());

        // Enter the critical "running" window and bump the high-water mark.
        let now = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
        self.max_concurrency.fetch_max(now, Ordering::SeqCst);

        let sleep = self
            .scripts
            .lock()
            .unwrap()
            .get(&id)
            .map(|s| s.sleep)
            .unwrap_or(Duration::ZERO);
        if !sleep.is_zero() {
            tokio::time::sleep(sleep).await;
        }

        self.in_flight.fetch_sub(1, Ordering::SeqCst);
        let output = self
            .scripts
            .lock()
            .unwrap()
            .get(&id)
            .and_then(|s| s.output.clone());
        match output {
            Some(output) => canned_output_with_text(&id, output),
            None => canned_output(&id),
        }
    }
}

// ---- helpers --------------------------------------------------------------

/// Build a model tool-call content part with the given id (the dispatcher input).
fn tool_call(id: &str) -> ContentPart {
    named_tool_call(id, "shell")
}

fn named_tool_call(id: &str, name: &str) -> ContentPart {
    ContentPart::ToolCall {
        id: id.to_string(),
        name: name.to_string(),
        input: serde_json::json!({ "command": ["echo", id] }),
        provider_metadata: None,
    }
}

fn browser_script_call(id: &str) -> ContentPart {
    named_tool_call(id, "browser_script")
}

fn browser_script_call_with_input(id: &str, input: serde_json::Value) -> ContentPart {
    ContentPart::ToolCall {
        id: id.to_string(),
        name: "browser_script".to_string(),
        input,
        provider_metadata: None,
    }
}

/// Extract the call id from a `ToolCall` content part.
fn call_id(call: &ContentPart) -> String {
    match call {
        ContentPart::ToolCall { id, .. } => id.clone(),
        _ => String::new(),
    }
}

/// The canned output message a scripted call returns, tagged with its call id so
/// a test can assert the *output order* matches the *input order*.
fn canned_output(id: &str) -> Message {
    canned_output_with_text(id, format!("output:{id}"))
}

fn canned_output_with_text(id: &str, text: String) -> Message {
    Message::new(
        MessageRole::Tool,
        vec![ContentPart::ToolResult {
            tool_call_id: id.to_string(),
            content: vec![ContentPart::text(text)],
            is_error: false,
        }],
    )
}

/// Read the `tool_call_id` out of a recorded output message (for ordering asserts).
fn output_id(msg: &Message) -> String {
    for part in &msg.content {
        if let ContentPart::ToolResult { tool_call_id, .. } = part {
            return tool_call_id.clone();
        }
    }
    String::new()
}

fn output_text_and_error(msg: &Message) -> (String, bool) {
    for part in &msg.content {
        if let ContentPart::ToolResult {
            content, is_error, ..
        } = part
        {
            let text = content
                .iter()
                .filter_map(|part| match part {
                    ContentPart::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("");
            return (text, *is_error);
        }
    }
    (String::new(), false)
}

fn script(id: &str, sleep_ms: u64, parallel_safe: bool) -> CallScript {
    CallScript {
        id: id.to_string(),
        sleep: Duration::from_millis(sleep_ms),
        parallel_safe,
        output: None,
    }
}

fn script_with_output(id: &str, output: &str, sleep_ms: u64, parallel_safe: bool) -> CallScript {
    CallScript {
        id: id.to_string(),
        sleep: Duration::from_millis(sleep_ms),
        parallel_safe,
        output: Some(output.to_string()),
    }
}

// ---- (1) outputs recorded in MODEL order, not completion order ------------

#[tokio::test]
async fn outputs_are_recorded_in_model_order_even_when_later_calls_finish_first() {
    // call[0] sleeps far longer than call[1], so call[1] *completes first*. The
    // FuturesOrdered drain must still yield [a, b] in model order. Both are
    // parallel-safe so they actually run concurrently (otherwise call[1] couldn't
    // even start before call[0] finished).
    let runner = ScriptedRunner::new(vec![script("a", 80, true), script("b", 5, true)]);
    let dispatcher = ToolDispatcher::with_runner(runner.clone(), /* model_supports */ true);

    let out = dispatcher
        .dispatch_ordered(
            vec![tool_call("a"), tool_call("b")],
            CancellationToken::new(),
        )
        .await;

    let order: Vec<String> = out.outputs_in_order.iter().map(output_id).collect();
    assert_eq!(
        order,
        vec!["a".to_string(), "b".to_string()],
        "outputs must be in MODEL order [a, b] even though b finishes before a"
    );
    assert!(out.needs_follow_up, "dispatched calls -> needs_follow_up");
}

// ---- (2) parallel-safe calls actually overlap -----------------------------

#[tokio::test]
async fn parallel_safe_calls_overlap() {
    // Three parallel-safe calls that all sleep: they should be in-flight at the
    // same time, so observed max concurrency must exceed 1.
    let runner = ScriptedRunner::new(vec![
        script("a", 60, true),
        script("b", 60, true),
        script("c", 60, true),
    ]);
    let dispatcher = ToolDispatcher::with_runner(runner.clone(), true);

    let out = dispatcher
        .dispatch_ordered(
            vec![tool_call("a"), tool_call("b"), tool_call("c")],
            CancellationToken::new(),
        )
        .await;

    assert_eq!(out.outputs_in_order.len(), 3);
    assert!(
        runner.max_concurrency() > 1,
        "parallel-safe calls must overlap (max_concurrency={}, expected >1)",
        runner.max_concurrency()
    );
}

// ---- (3) a serial call forces exclusivity ---------------------------------

#[tokio::test]
async fn serial_call_is_exclusive_and_does_not_overlap() {
    // Every call is parallel-*unsafe* (serial), so each takes a write guard on the
    // shared RwLock: they must run strictly one-at-a-time. Max concurrency == 1.
    let runner = ScriptedRunner::new(vec![
        script("a", 40, false),
        script("b", 40, false),
        script("c", 40, false),
    ]);
    let dispatcher = ToolDispatcher::with_runner(runner.clone(), true);

    let out = dispatcher
        .dispatch_ordered(
            vec![tool_call("a"), tool_call("b"), tool_call("c")],
            CancellationToken::new(),
        )
        .await;

    assert_eq!(out.outputs_in_order.len(), 3);
    assert_eq!(
        runner.max_concurrency(),
        1,
        "serial calls must NOT overlap (max_concurrency must be 1)"
    );
    // Even serial, outputs stay in model order.
    let order: Vec<String> = out.outputs_in_order.iter().map(output_id).collect();
    assert_eq!(order, vec!["a", "b", "c"]);
}

#[tokio::test]
async fn model_without_parallel_support_forces_serial_even_for_parallel_safe_calls() {
    // Calls are individually parallel-safe, but the *model* does not support
    // parallel tool calls (`classify_parallelism(true, false) == Serial`), so they
    // must run exclusively. This proves the model-capability half of the gate.
    let runner = ScriptedRunner::new(vec![script("a", 40, true), script("b", 40, true)]);
    let dispatcher = ToolDispatcher::with_runner(runner.clone(), /* model_supports */ false);

    let out = dispatcher
        .dispatch_ordered(
            vec![tool_call("a"), tool_call("b")],
            CancellationToken::new(),
        )
        .await;

    assert_eq!(out.outputs_in_order.len(), 2);
    assert_eq!(
        runner.max_concurrency(),
        1,
        "model without parallel support must serialize parallel-safe calls"
    );
}

// ---- (4) cancellation mid-dispatch stops further calls --------------------

#[tokio::test]
async fn cancellation_mid_dispatch_stops_scheduling_further_calls() {
    // Serial calls (exclusive), each sleeping. We cancel shortly after dispatch
    // starts: the first call is already in-flight, but scheduling of the remaining
    // calls must stop, so far fewer than all 5 calls ever start.
    let runner = ScriptedRunner::new(vec![
        script("a", 60, false),
        script("b", 60, false),
        script("c", 60, false),
        script("d", 60, false),
        script("e", 60, false),
    ]);
    let dispatcher = ToolDispatcher::with_runner(runner.clone(), true);

    let cancel = CancellationToken::new();
    let cancel2 = cancel.clone();
    tokio::spawn(async move {
        // Let the first serial call get in-flight, then cancel.
        tokio::time::sleep(Duration::from_millis(20)).await;
        cancel2.cancel();
    });

    let calls = vec![
        tool_call("a"),
        tool_call("b"),
        tool_call("c"),
        tool_call("d"),
        tool_call("e"),
    ];
    let _out = dispatcher.dispatch_ordered(calls, cancel).await;

    // Because cancellation stops scheduling new calls AND in-flight calls
    // short-circuit on cancel, strictly fewer than all 5 calls ever started.
    let started = runner.runs_started();
    assert!(
        started < 5,
        "cancellation must stop scheduling further calls (runs_started={started}, expected <5)"
    );
}

#[tokio::test]
async fn cancellation_preserves_exec_command_result() {
    let runner = ScriptedRunner::new(vec![script("exec", 60, false)]);
    let dispatcher = ToolDispatcher::with_runner(runner.clone(), true);

    let cancel = CancellationToken::new();
    let cancel2 = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(20)).await;
        cancel2.cancel();
    });

    let out = dispatcher
        .dispatch_ordered(vec![named_tool_call("exec", "exec_command")], cancel)
        .await;

    assert_eq!(out.outputs_in_order.len(), 1);
    let (text, is_error) = output_text_and_error(&out.outputs_in_order[0]);
    assert!(
        !is_error,
        "exec_command result should not be aborted: {text}"
    );
    assert_eq!(text, "output:exec");
}

// ---- (5) needs_follow_up: true when dispatched, false for empty -----------

#[tokio::test]
async fn needs_follow_up_true_when_calls_dispatched_false_for_empty() {
    let runner = ScriptedRunner::new(vec![script("a", 1, true)]);
    let dispatcher = ToolDispatcher::with_runner(runner.clone(), true);

    let out = dispatcher
        .dispatch_ordered(vec![tool_call("a")], CancellationToken::new())
        .await;
    assert!(
        out.needs_follow_up,
        ">=1 dispatched call -> needs_follow_up=true"
    );
    assert_eq!(out.outputs_in_order.len(), 1);

    // Empty input: nothing dispatched, no follow-up.
    let empty_runner = ScriptedRunner::new(vec![]);
    let empty_dispatcher = ToolDispatcher::with_runner(empty_runner.clone(), true);
    let empty = empty_dispatcher
        .dispatch_ordered(vec![], CancellationToken::new())
        .await;
    assert!(
        !empty.needs_follow_up,
        "empty input -> needs_follow_up=false"
    );
    assert!(empty.outputs_in_order.is_empty());
    assert_eq!(empty_runner.runs_started(), 0);
}

#[tokio::test]
async fn repeated_browser_script_output_gets_recovery_guidance() {
    let runner = ScriptedRunner::new(vec![script("repeat", 1, false)]);
    let dispatcher = ToolDispatcher::with_runner(runner, true);

    for attempt in 1..=3 {
        let out = dispatcher
            .dispatch_ordered(
                vec![browser_script_call("repeat")],
                CancellationToken::new(),
            )
            .await;
        assert_eq!(out.outputs_in_order.len(), 1);
        let (text, is_error) = output_text_and_error(&out.outputs_in_order[0]);
        assert!(!is_error);
        if attempt < 3 {
            assert!(
                !text.contains("Repeated browser_script output detected"),
                "attempt {attempt} should not warn yet: {text}"
            );
        } else {
            assert!(
                text.contains("Repeated browser_script output detected"),
                "third repeated browser_script result should steer recovery: {text}"
            );
            assert!(
                text.contains("call done with the best verified result"),
                "guidance should encourage a user-facing result: {text}"
            );
        }
    }
}

#[tokio::test]
async fn repeated_running_browser_script_output_gets_recovery_guidance() {
    let running_output = "browser_script is still running.\nrun_id: bs-123\nNext step: call browser_script with action=\"observe\", run_id=\"bs-123\", and observe_timeout_ms=15000.";
    let runner = ScriptedRunner::new(vec![script_with_output("repeat", running_output, 1, false)]);
    let dispatcher = ToolDispatcher::with_runner(runner, true);

    for attempt in 1..=3 {
        let out = dispatcher
            .dispatch_ordered(
                vec![browser_script_call("repeat")],
                CancellationToken::new(),
            )
            .await;
        assert_eq!(out.outputs_in_order.len(), 1);
        let (text, is_error) = output_text_and_error(&out.outputs_in_order[0]);
        assert!(!is_error);
        if attempt < 3 {
            assert!(
                !text.contains("Repeated browser_script output detected"),
                "attempt {attempt} should not warn yet: {text}"
            );
        } else {
            assert!(
                text.contains("Repeated browser_script output detected"),
                "third repeated running browser_script result should steer recovery: {text}"
            );
            assert!(
                text.contains("observe once with a longer timeout"),
                "guidance should address active browser_script runs: {text}"
            );
        }
    }
}

#[tokio::test]
async fn repeated_browser_script_call_gets_recovery_guidance_even_when_output_changes() {
    let runner = ScriptedRunner::new(vec![
        script_with_output("repeat-1", "page snapshot version 1", 1, false),
        script_with_output("repeat-2", "page snapshot version 2", 1, false),
        script_with_output("repeat-3", "page snapshot version 3", 1, false),
    ]);
    let dispatcher = ToolDispatcher::with_runner(runner, true);
    let input = serde_json::json!({
        "action": "execute",
        "code": "page_info()",
    });

    for attempt in 1..=3 {
        let out = dispatcher
            .dispatch_ordered(
                vec![browser_script_call_with_input(
                    &format!("repeat-{attempt}"),
                    input.clone(),
                )],
                CancellationToken::new(),
            )
            .await;
        assert_eq!(out.outputs_in_order.len(), 1);
        let (text, is_error) = output_text_and_error(&out.outputs_in_order[0]);
        assert!(!is_error);
        if attempt < 3 {
            assert!(
                !text.contains("Repeated browser_script output detected"),
                "attempt {attempt} should not warn yet: {text}"
            );
        } else {
            assert!(
                text.contains("Repeated browser_script output detected"),
                "third repeated browser_script call should steer recovery: {text}"
            );
            assert!(
                text.contains("Do not repeat the same page_info"),
                "guidance should call out passive browser loops: {text}"
            );
        }
    }
}

// ---- (6) end-to-end: registry-backed RegistryRunner through the dispatcher --

mod registry_e2e {
    use std::sync::Arc;

    use browser_use_llm::schema::{ContentPart, Message, ToolDefinition};
    use serde_json::json;
    use tokio_util::sync::CancellationToken;

    use crate::tools::approval::AskForApproval;
    use crate::tools::handlers::browser::BROWSER_SCRIPT_CONTENT_STDOUT_PREFIX;
    use crate::tools::handlers::tool_search::{ToolSearchEntry, ToolSearchRequest, ToolSearchTool};
    use crate::tools::handlers::update_plan::{UpdatePlanRequest, UpdatePlanTool};
    use crate::tools::handlers::view_image::{ViewImageRequest, ViewImageTool};
    use crate::tools::orchestrator::{ToolOrchestrator, TurnEnv};
    use crate::tools::registry::ToolRegistry;
    use crate::tools::runtime::{
        Approvable, ExecOutput, SandboxAttempt, Sandboxable, ToolCtx, ToolError, ToolRuntime,
    };
    use crate::tools::sandbox::{FileSystemSandboxPolicy, SandboxPermissions, SandboxPreference};
    use crate::turn::dispatch::{RegistryRunner, ToolDispatcher};

    fn def(name: &str) -> ToolDefinition {
        ToolDefinition {
            name: name.to_string(),
            description: name.to_string(),
            input_schema: json!({ "type": "object" }),
            output_schema: None,
            namespace: None,
            namespace_description: None,
        }
    }

    fn env() -> TurnEnv {
        TurnEnv {
            file_system_sandbox_policy: FileSystemSandboxPolicy {
                restricted: false,
                denied_read: false,
            },
            managed_network_active: false,
            strict_auto_review: false,
            use_guardian: false,
        }
    }

    fn ctx() -> ToolCtx {
        ToolCtx {
            call_id: "c".to_string(),
            tool_name: "t".to_string(),
            cwd: std::path::PathBuf::from("/tmp"),
            artifact_root: std::path::PathBuf::from("/tmp/artifacts"),
        }
    }

    fn tool_call(id: &str, name: &str, input: serde_json::Value) -> ContentPart {
        ContentPart::ToolCall {
            id: id.to_string(),
            name: name.to_string(),
            input,
            provider_metadata: None,
        }
    }

    #[derive(Clone, Debug, serde::Deserialize)]
    struct MarkerRequest {}

    #[derive(Clone)]
    struct BrowserScriptMarkerTool;

    impl Approvable<MarkerRequest> for BrowserScriptMarkerTool {
        type ApprovalKey = String;

        fn approval_keys(&self, _req: &MarkerRequest) -> Vec<Self::ApprovalKey> {
            Vec::new()
        }

        fn sandbox_permissions(&self, _req: &MarkerRequest) -> SandboxPermissions {
            SandboxPermissions::UseDefault
        }
    }

    impl Sandboxable for BrowserScriptMarkerTool {
        fn sandbox_preference(&self) -> SandboxPreference {
            SandboxPreference::Never
        }
    }

    #[async_trait::async_trait]
    impl ToolRuntime<MarkerRequest, ExecOutput> for BrowserScriptMarkerTool {
        async fn run(
            &self,
            _req: &MarkerRequest,
            _attempt: &SandboxAttempt<'_>,
            _ctx: &ToolCtx,
        ) -> Result<ExecOutput, ToolError> {
            let parts = vec![
                ContentPart::text("browser_script failed."),
                ContentPart::Media {
                    mime_type: "image/png".to_string(),
                    data: Some("AAAA".to_string()),
                    url: None,
                    detail: None,
                },
            ];
            let payload = serde_json::to_string(&parts).unwrap();
            Ok(ExecOutput {
                exit_code: 1,
                stdout: format!(
                    "browser_script failed.{BROWSER_SCRIPT_CONTENT_STDOUT_PREFIX}{payload}"
                ),
                stderr: "RuntimeError: failed after screenshot".to_string(),
            })
        }
    }

    /// The recorded tool-result text + error flag for a dispatched output.
    fn result_of(msg: &Message) -> (String, bool) {
        for part in &msg.content {
            if let ContentPart::ToolResult {
                content, is_error, ..
            } = part
            {
                let text = content
                    .iter()
                    .filter_map(|p| match p {
                        ContentPart::Text { text } => Some(text.clone()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("");
                return (text, *is_error);
            }
        }
        (String::new(), false)
    }

    fn result_call_id(msg: &Message) -> String {
        for part in &msg.content {
            if let ContentPart::ToolResult { tool_call_id, .. } = part {
                return tool_call_id.clone();
            }
        }
        String::new()
    }

    #[tokio::test]
    async fn registry_runner_dispatches_tool_calls_in_model_order() {
        // Build a real registry with two Deserialize-able handlers, wrap it in the
        // RegistryRunner, and dispatch two tool calls end-to-end through the
        // dispatcher. Outputs are recorded in model order regardless of timing.
        let mut registry = ToolRegistry::new();
        registry.register::<_, UpdatePlanRequest>(
            "update_plan",
            def("update_plan"),
            false,
            UpdatePlanTool::new(),
        );
        registry.register::<_, ToolSearchRequest>(
            "tool_search",
            def("tool_search"),
            true,
            ToolSearchTool::new(vec![ToolSearchEntry::new(
                "kubernetes",
                "manage clusters",
                ["ns"],
            )]),
        );

        let runner = RegistryRunner::new(
            Arc::new(registry),
            Arc::new(ToolOrchestrator::stub()),
            ctx(),
            env(),
            AskForApproval::Never,
        );
        let dispatcher = ToolDispatcher::with_runner(runner, /* model_supports */ true);

        let calls = vec![
            tool_call(
                "1",
                "update_plan",
                json!({ "plan": [{"step": "do it", "status": "pending"}] }),
            ),
            tool_call("2", "tool_search", json!({ "query": "kubernetes" })),
        ];

        let out = dispatcher
            .dispatch_ordered(calls, CancellationToken::new())
            .await;
        assert_eq!(out.outputs_in_order.len(), 2);
        assert!(out.needs_follow_up);

        // Output 0: update_plan, in model order, success.
        assert_eq!(result_call_id(&out.outputs_in_order[0]), "1");
        let (text0, err0) = result_of(&out.outputs_in_order[0]);
        assert!(!err0, "update_plan should succeed: {text0}");
        assert!(text0.contains("Plan updated:"), "got: {text0}");

        // Output 1: tool_search, in model order, success.
        assert_eq!(result_call_id(&out.outputs_in_order[1]), "2");
        let (text1, err1) = result_of(&out.outputs_in_order[1]);
        assert!(!err1, "tool_search should succeed: {text1}");
        assert!(text1.contains("kubernetes"), "got: {text1}");
    }

    #[tokio::test]
    async fn registry_runner_wraps_view_image_stdout_as_media_tool_result() {
        let dir = tempfile::tempdir().expect("tempdir");
        let png_bytes: &[u8] = &[
            0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0xDE, 0xAD, 0xBE, 0xEF,
        ];
        std::fs::write(dir.path().join("pic.png"), png_bytes).expect("write png");

        let mut registry = ToolRegistry::new();
        registry.register::<_, ViewImageRequest>(
            "view_image",
            def("view_image"),
            false,
            ViewImageTool::new(),
        );
        let runner = RegistryRunner::new(
            Arc::new(registry),
            Arc::new(ToolOrchestrator::stub()),
            ToolCtx {
                cwd: dir.path().to_path_buf(),
                artifact_root: dir.path().join("artifacts"),
                ..ctx()
            },
            env(),
            AskForApproval::Never,
        );
        let dispatcher = ToolDispatcher::with_runner(runner, true);

        let out = dispatcher
            .dispatch_ordered(
                vec![tool_call(
                    "view-call",
                    "view_image",
                    json!({ "path": "pic.png" }),
                )],
                CancellationToken::new(),
            )
            .await;

        assert_eq!(out.outputs_in_order.len(), 1);
        let ContentPart::ToolResult {
            tool_call_id,
            content,
            is_error,
        } = &out.outputs_in_order[0].content[0]
        else {
            panic!("expected tool result, got {:?}", out.outputs_in_order[0]);
        };
        assert_eq!(tool_call_id, "view-call");
        assert!(!is_error);
        assert_eq!(content.len(), 1);
        let ContentPart::Media {
            mime_type,
            data,
            url,
            detail,
        } = &content[0]
        else {
            panic!("expected media result, got {content:?}");
        };
        assert_eq!(mime_type, "image/png");
        assert_eq!(detail, &None);
        assert!(data.as_deref().is_some_and(|data| !data.is_empty()));
        assert!(url.is_none());
    }

    #[tokio::test]
    async fn registry_runner_wraps_browser_script_marker_as_media_even_on_error() {
        let mut registry = ToolRegistry::new();
        registry.register::<_, MarkerRequest>(
            "browser_script",
            def("browser_script"),
            false,
            BrowserScriptMarkerTool,
        );
        let runner = RegistryRunner::new(
            Arc::new(registry),
            Arc::new(ToolOrchestrator::stub()),
            ctx(),
            env(),
            AskForApproval::Never,
        );
        let dispatcher = ToolDispatcher::with_runner(runner, true);

        let out = dispatcher
            .dispatch_ordered(
                vec![tool_call("browser-call", "browser_script", json!({}))],
                CancellationToken::new(),
            )
            .await;

        let ContentPart::ToolResult {
            tool_call_id,
            content,
            is_error,
        } = &out.outputs_in_order[0].content[0]
        else {
            panic!("expected tool result, got {:?}", out.outputs_in_order[0]);
        };
        assert_eq!(tool_call_id, "browser-call");
        assert!(*is_error);
        assert!(matches!(content.first(), Some(ContentPart::Text { .. })));
        assert!(
            content.iter().any(
                |part| matches!(part, ContentPart::Media { mime_type, data, .. }
                    if mime_type == "image/png" && data.as_deref() == Some("AAAA"))
            ),
            "expected media in browser_script error content: {content:?}"
        );
    }

    #[tokio::test]
    async fn registry_runner_reports_unknown_tool_as_error_result() {
        let registry: ToolRegistry = ToolRegistry::new();
        let runner = RegistryRunner::new(
            Arc::new(registry),
            Arc::new(ToolOrchestrator::stub()),
            ctx(),
            env(),
            AskForApproval::Never,
        );
        let dispatcher = ToolDispatcher::with_runner(runner, true);

        let out = dispatcher
            .dispatch_ordered(
                vec![tool_call("1", "ghost", json!({}))],
                CancellationToken::new(),
            )
            .await;
        assert_eq!(out.outputs_in_order.len(), 1);
        let (text, err) = result_of(&out.outputs_in_order[0]);
        assert!(err, "unknown tool must be an error result");
        assert!(text.contains("unknown tool `ghost`"), "got: {text}");
    }
}
