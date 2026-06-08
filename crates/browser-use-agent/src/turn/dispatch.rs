//! In-turn tool dispatch: `FuturesOrdered` + `RwLock` gate (codex `turn.rs:106` +
//! `tools/parallel.rs`).
//!
//! ## What this does (codex parity)
//! When a model turn yields one or more tool calls, codex schedules them so that
//! their **outputs are recorded in model order** regardless of which call
//! finishes first, while still letting *parallel-safe* calls run concurrently and
//! forcing *serial* calls to run with exclusive access. We reproduce that here:
//!
//! - **Ordering** — every call is pushed onto a [`FuturesOrdered`]
//!   (`futures_util::stream::FuturesOrdered`). `FuturesOrdered` yields results in
//!   the order the futures were *pushed*, not the order they *complete*, so the
//!   collected `Vec<Message>` matches the model's call order even when a later
//!   call finishes first (codex `turn.rs:1655/1873` collects ordered outputs).
//!
//! - **Parallel-vs-serial gate** — a single shared
//!   `Arc<tokio::sync::RwLock<()>>` is the concurrency gate (codex `turn.rs:106`,
//!   `tools/parallel.rs`). The per-call parallelism is the *pure* decision
//!   [`decision::classify_parallelism`]: a call that is `parallel_safe` **and**
//!   whose model `supports_parallel_tool_calls` is
//!   [`ToolParallelism::Parallel`](crate::decision::ToolParallelism::Parallel) and
//!   takes a **read** guard (many can hold it at once); any other call is
//!   [`ToolParallelism::Serial`](crate::decision::ToolParallelism::Serial) and
//!   takes a **write** guard (exclusive — it waits for in-flight reads to drain
//!   and blocks new ones until it finishes). Because the per-call future acquires
//!   its own guard *inside* the future, scheduling stays cheap and the gate
//!   enforces the overlap rules at run time.
//!
//! - **Cancellation** — the [`CancellationToken`] is honored two ways: we stop
//!   *scheduling* new calls the moment cancel fires, and each in-flight future
//!   `select!`s on `cancel.cancelled()` so it can short-circuit. Calls that were
//!   already scheduled are still drained from the `FuturesOrdered` (codex
//!   `drain_in_flight` semantics: started work is observed before returning) so
//!   the result vector never has holes.
//!
//! ## Testability
//! The dispatcher never talks to a real `ModelClient`, sandbox, or network. It
//! runs each call through an injected [`CallRunner`]: the production constructor
//! ([`ToolDispatcher::new`]) backs it with [`OrchestratorRunner`] (which delegates
//! to the merged [`tools::ToolOrchestrator`](crate::tools::ToolOrchestrator)),
//! while tests inject a `ScriptedRunner` (see `dispatch_tests.rs`) that records
//! invocation order + observed concurrency and returns canned [`Message`]s. The
//! classifier (`parallel_safe` per call) is likewise injected so tests can drive
//! the gate directly.

use std::sync::Arc;

use browser_use_llm::schema::{ContentPart, Message, MessageRole, ToolDefinition};
use futures_util::stream::{FuturesOrdered, StreamExt};
use tokio::sync::{Mutex, RwLock};
use tokio_util::sync::CancellationToken;

use crate::decision::{self, ToolParallelism};
use crate::tools::approval::AskForApproval;
use crate::tools::handlers::browser::BROWSER_SCRIPT_CONTENT_STDOUT_PREFIX;
use crate::tools::handlers::view_image::VIEW_IMAGE_STDOUT_PREFIX;
use crate::tools::orchestrator::{ToolOrchestrator, TurnEnv};
use crate::tools::registry::ToolRegistry;
use crate::tools::runtime::{Approver, AutoApprover, ToolCtx};
use crate::tools::sandbox::{NoneSandboxProvider, SandboxProvider};

const BROWSER_SCRIPT_NO_PROGRESS_REPEAT_THRESHOLD: usize = 3;
const BROWSER_SCRIPT_NO_PROGRESS_NUDGE: &str = "Repeated browser_script output detected: this browser state, active script, or script error has already been returned multiple times. Do not repeat the same page_info, list_tabs, observe, or extraction script. Change strategy now: take a concrete browser action, observe once with a longer timeout if an active run is still useful, narrow the extraction, use search/fetch for source discovery, cancel/change the active script, or call done with the best verified result or partial result.";

/// Runs a single tool call to completion, producing the `Message` to record.
///
/// This is the seam that keeps the dispatcher network-free and unit-testable: the
/// real impl ([`OrchestratorRunner`]) routes through
/// [`tools::ToolOrchestrator`](crate::tools::ToolOrchestrator) (approval ->
/// sandbox -> exec), while tests use a scripted fake.
#[async_trait::async_trait]
pub trait CallRunner: Send + Sync {
    /// Whether this specific call is parallel-safe (codex `ToolRuntime::parallel_safe`).
    /// Combined with the model's `supports_parallel_tool_calls` by
    /// [`decision::classify_parallelism`] to pick the gate guard.
    fn parallel_safe(&self, call: &ContentPart) -> bool;

    /// Execute the call and return the `Message` (tool output) to record.
    async fn run(&self, call: ContentPart, cancel: CancellationToken) -> Message;
}

/// Blanket impl so a shared, already-`Arc`'d runner satisfies the bound. This is
/// what lets a caller (or a test) build a runner, keep a `clone()` of the `Arc`
/// to inspect afterward, and still hand the same `Arc` to the dispatcher as the
/// `CallRunner`.
#[async_trait::async_trait]
impl<T: CallRunner + ?Sized> CallRunner for Arc<T> {
    fn parallel_safe(&self, call: &ContentPart) -> bool {
        (**self).parallel_safe(call)
    }

    async fn run(&self, call: ContentPart, cancel: CancellationToken) -> Message {
        (**self).run(call, cancel).await
    }
}

/// Production [`CallRunner`] backing the dispatcher with the merged
/// [`tools::ToolOrchestrator`](crate::tools::ToolOrchestrator) (Wave-2 B1).
///
/// The orchestrator surface is generic over the concrete tool/request types,
/// which are wired per-tool by the turn loop (WP that owns the toolset). Until
/// that toolset is threaded through here, the production runner is a thin
/// placeholder so the dispatcher compiles and the *scheduling/ordering/gate*
/// logic — the load-bearing part of WP-C1 — is fully exercised by the injected
/// fakes. `dispatch_ordered` itself is runner-agnostic, so swapping in the real
/// per-tool routing later does not change the dispatch logic.
pub struct OrchestratorRunner {
    /// The model-level parallel-tool-calls capability (codex `turn.rs:872`,
    /// `AgentConfig::supports_parallel_tool_calls`). Per-call `parallel_safe`
    /// comes from the tool runtime; both feed `classify_parallelism`.
    supports_parallel_tool_calls: bool,
}

impl OrchestratorRunner {
    pub fn new(supports_parallel_tool_calls: bool) -> Self {
        Self {
            supports_parallel_tool_calls,
        }
    }
}

#[async_trait::async_trait]
impl CallRunner for OrchestratorRunner {
    fn parallel_safe(&self, _call: &ContentPart) -> bool {
        // Conservative default until the concrete toolset is threaded through:
        // codex's `ToolRuntime::parallel_safe` defaults to `false` (serial), which
        // is the safe choice. The model capability still gates via
        // `classify_parallelism` once a tool opts in.
        false
    }

    async fn run(&self, call: ContentPart, _cancel: CancellationToken) -> Message {
        // The real per-tool routing through `ToolOrchestrator::run` is wired by
        // the toolset-owning WP. Recording a tool-result message keeps the dispatch
        // contract intact in the meantime.
        let _ = self.supports_parallel_tool_calls;
        result_message_for(&call, "tool routing not yet wired", true)
    }
}

/// The REAL production [`CallRunner`]: routes each tool call through a
/// [`ToolRegistry`].
///
/// This un-stubs the dispatch path STATUS.md flagged
/// (`docs/STATUS.md`: "OrchestratorRunner is a placeholder that records
/// tool-result Messages rather than routing real per-tool Req/Out through
/// ToolOrchestrator::run"). A model-emitted `ContentPart::ToolCall { name, input }`
/// is dispatched BY NAME through the registry, which deserializes `input` into
/// the matching handler's typed `Req` and runs it THROUGH the
/// [`ToolOrchestrator`] (approval/sandbox/escalation policy). The
/// [`ExecOutput`](crate::tools::ExecOutput) is rendered into the tool-result
/// [`Message`] the dispatcher records, and the per-call [`parallel_safe`] is the
/// registry's own per-tool flag (falling back to the static name heuristic for
/// unregistered names).
///
/// Generic over the orchestrator seams `(S, A)`, defaulting to the `None`/auto
/// seams. Parity: codex `core/src/tools/router.rs::dispatch_tool_call`
/// (look the handler up by name, error if unknown, run under the orchestrator).
pub struct RegistryRunner<S = NoneSandboxProvider, A = AutoApprover>
where
    S: SandboxProvider,
    A: Approver,
{
    registry: Arc<ToolRegistry<S, A>>,
    orchestrator: Arc<ToolOrchestrator<S, A>>,
    ctx: ToolCtx,
    env: TurnEnv,
    policy: AskForApproval,
}

impl<S, A> RegistryRunner<S, A>
where
    S: SandboxProvider,
    A: Approver,
{
    /// Construct a runner over a registry, orchestrator, per-turn context/env,
    /// and the active approval policy.
    pub fn new(
        registry: Arc<ToolRegistry<S, A>>,
        orchestrator: Arc<ToolOrchestrator<S, A>>,
        ctx: ToolCtx,
        env: TurnEnv,
        policy: AskForApproval,
    ) -> Self {
        Self {
            registry,
            orchestrator,
            ctx,
            env,
            policy,
        }
    }
}

#[async_trait::async_trait]
impl<S, A> CallRunner for RegistryRunner<S, A>
where
    S: SandboxProvider + 'static,
    A: Approver + 'static,
{
    fn parallel_safe(&self, call: &ContentPart) -> bool {
        // Prefer the registered tool's own `parallel_safe`; fall back to the
        // conservative serial default for calls whose tool isn't registered (an
        // unknown name then errors serially, under the write guard).
        match call {
            ContentPart::ToolCall {
                name,
                provider_metadata,
                ..
            } => self
                .registry
                .parallel_safe_namespaced(tool_call_namespace(provider_metadata).as_deref(), name)
                .unwrap_or(false),
            _ => false,
        }
    }

    async fn run(&self, call: ContentPart, cancel: CancellationToken) -> Message {
        match &call {
            ContentPart::ToolCall {
                id,
                name,
                input,
                provider_metadata,
            } => {
                let mut ctx = self.ctx.clone();
                ctx.call_id = id.clone();
                let namespace = tool_call_namespace(provider_metadata);
                ctx.tool_name = tool_display_name(namespace.as_deref(), name);
                match self
                    .registry
                    .dispatch_namespaced_with_cancel(
                        namespace.as_deref(),
                        name,
                        input,
                        &ctx,
                        &self.env,
                        self.policy,
                        &self.orchestrator,
                        cancel,
                    )
                    .await
                {
                    Ok(output) => {
                        // Non-zero exit (or stderr) is a tool-level failure; the
                        // model-facing text prefers stdout, else stderr.
                        let is_error = output.exit_code != 0;
                        let text = if !output.stdout.is_empty() {
                            output.stdout
                        } else {
                            output.stderr
                        };
                        result_message_for(&call, &text, is_error)
                    }
                    // An unknown tool or a deserialize/approval/sandbox error is
                    // surfaced as an error tool-result so the turn loop records it.
                    Err(err) => result_message_for(&call, &format!("{err:?}"), true),
                }
            }
            _ => result_message_for(&call, "not a tool call", true),
        }
    }
}

fn tool_call_namespace(provider_metadata: &Option<serde_json::Value>) -> Option<String> {
    provider_metadata
        .as_ref()
        .and_then(|metadata| metadata.get("namespace"))
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
}

fn tool_display_name(namespace: Option<&str>, name: &str) -> String {
    match namespace {
        Some(namespace) => {
            let mut display = String::with_capacity(namespace.len() + name.len());
            display.push_str(namespace);
            display.push_str(name);
            display
        }
        None => name.to_string(),
    }
}

/// Build the recorded `Message` for a tool call's output (codex records a
/// function-call output keyed by the originating call id).
fn result_message_for(call: &ContentPart, text: &str, is_error: bool) -> Message {
    let tool_call_id = match call {
        ContentPart::ToolCall { id, .. } => id.clone(),
        _ => String::new(),
    };
    let content = tool_result_content_for(call, text, is_error);
    Message::new(
        MessageRole::Tool,
        vec![ContentPart::ToolResult {
            tool_call_id,
            content,
            is_error,
        }],
    )
}

fn tool_result_content_for(call: &ContentPart, text: &str, is_error: bool) -> Vec<ContentPart> {
    if let ContentPart::ToolCall { name, .. } = call {
        if name == "browser_script" {
            if let Some(parts) = content_parts_from_browser_script_stdout(text) {
                return parts;
            }
            return vec![ContentPart::text(strip_browser_script_content_marker(text))];
        }
    }
    if !is_error {
        if let ContentPart::ToolCall { name, .. } = call {
            if name == "view_image" {
                if let Some(media) = media_part_from_view_image_stdout(text) {
                    return vec![media];
                }
            }
        }
    }
    vec![ContentPart::text(text)]
}

fn content_parts_from_browser_script_stdout(text: &str) -> Option<Vec<ContentPart>> {
    let (_, payload) = text.rsplit_once(BROWSER_SCRIPT_CONTENT_STDOUT_PREFIX)?;
    let parts: Vec<ContentPart> = serde_json::from_str(payload.trim()).ok()?;
    (!parts.is_empty()).then_some(parts)
}

fn strip_browser_script_content_marker(text: &str) -> String {
    text.rsplit_once(BROWSER_SCRIPT_CONTENT_STDOUT_PREFIX)
        .map(|(visible, _)| visible)
        .unwrap_or(text)
        .to_string()
}

fn media_part_from_view_image_stdout(text: &str) -> Option<ContentPart> {
    let data_url = text.strip_prefix(VIEW_IMAGE_STDOUT_PREFIX)?;
    let rest = data_url.strip_prefix("data:")?;
    let (mime_type, data) = rest.split_once(";base64,")?;
    if !mime_type.starts_with("image/") || data.is_empty() {
        return None;
    }
    Some(ContentPart::Media {
        mime_type: mime_type.to_string(),
        data: Some(data.to_string()),
        url: None,
        detail: None,
    })
}

/// Result of dispatching a turn's tool calls.
pub struct ToolDispatchResult {
    /// Tool outputs in **model order** (call[i] -> outputs_in_order[i]),
    /// regardless of completion order.
    pub outputs_in_order: Vec<Message>,
    /// `true` iff at least one call was dispatched (there is output to feed back
    /// to the model on the next turn). `false` for empty input.
    pub needs_follow_up: bool,
}

/// Schedules a turn's tool calls: ordered outputs + parallel/serial gate + cancel.
pub struct ToolDispatcher<R: CallRunner = OrchestratorRunner> {
    /// The shared concurrency gate (codex `turn.rs:106`): read = parallel,
    /// write = serial-exclusive.
    gate: Arc<RwLock<()>>,
    /// Runs each call (real = orchestrator, test = scripted fake).
    runner: Arc<R>,
    /// Model-level parallel-tool-calls capability fed to `classify_parallelism`.
    supports_parallel_tool_calls: bool,
    /// The model-visible tool definitions the registry advertises
    /// (`ToolRegistry::model_visible_definitions()`), captured at construction so
    /// the fused [`ModelSamplingDriver`](crate::turn::sampling::ModelSamplingDriver)
    /// can populate `LlmRequest::tools` WITHOUT reaching through the generic `R`
    /// (the registry is private inside `RegistryRunner`). Order matches the
    /// registry's advertised order (name-sorted). Empty for bare/test dispatchers
    /// built without specs.
    tool_specs: Vec<ToolDefinition>,
    /// Tracks repeated browser_script results across turns so the agent gets a
    /// recovery nudge before spending the whole step budget on no-op state/error
    /// loops.
    no_progress_state: Arc<Mutex<NoProgressState>>,
}

impl ToolDispatcher<OrchestratorRunner> {
    /// Production dispatcher backed by the
    /// [`tools::ToolOrchestrator`](crate::tools::ToolOrchestrator) seam.
    pub fn new() -> Self {
        Self::with_runner(OrchestratorRunner::new(false), false)
    }
}

impl<R: CallRunner + 'static> ToolDispatcher<R> {
    /// Build a dispatcher with an explicit [`CallRunner`] and the model's
    /// parallel-tool-calls capability. Used by the production constructor and by
    /// tests (which inject a scripted runner).
    ///
    /// Carries NO model-visible tool specs (`tool_specs` empty), so a fused
    /// driver built over this dispatcher sends no tool definitions. Use
    /// [`with_runner_and_specs`](ToolDispatcher::with_runner_and_specs) to attach
    /// the registry's advertised specs so the model receives the tool catalog.
    pub fn with_runner(runner: R, supports_parallel_tool_calls: bool) -> Self {
        Self::with_runner_and_specs(runner, supports_parallel_tool_calls, Vec::new())
    }

    /// Build a dispatcher with an explicit [`CallRunner`], the model's
    /// parallel-tool-calls capability, and the model-visible tool `specs` the
    /// registry advertises (`ToolRegistry::model_visible_definitions()`).
    ///
    /// The `specs` are stored verbatim (same `Vec<ToolDefinition>`, order-stable)
    /// and exposed via [`tool_specs`](ToolDispatcher::tool_specs) so the fused
    /// [`ModelSamplingDriver`](crate::turn::sampling::ModelSamplingDriver) can set
    /// `LlmRequest::tools` on each per-turn request — without the driver needing
    /// to reach the registry (private inside `RegistryRunner`) through `R`.
    pub fn with_runner_and_specs(
        runner: R,
        supports_parallel_tool_calls: bool,
        tool_specs: Vec<ToolDefinition>,
    ) -> Self {
        Self {
            gate: Arc::new(RwLock::new(())),
            runner: Arc::new(runner),
            supports_parallel_tool_calls,
            tool_specs,
            no_progress_state: Arc::new(Mutex::new(NoProgressState::default())),
        }
    }

    /// The model-visible tool definitions this dispatcher carries, in the
    /// registry's advertised order. Empty unless built via
    /// [`with_runner_and_specs`](ToolDispatcher::with_runner_and_specs). The fused
    /// driver copies these into `LlmRequest::tools` so the model can emit tool
    /// calls (codex sends the tool catalog on every sampling request).
    pub fn tool_specs(&self) -> &[ToolDefinition] {
        &self.tool_specs
    }

    /// Dispatch the turn's tool `calls`.
    ///
    /// Outputs are recorded in **model order** via [`FuturesOrdered`]; parallel-safe
    /// calls overlap (read guard) while serial calls take an exclusive write guard;
    /// `cancel` stops scheduling further calls and lets in-flight calls
    /// short-circuit, while still draining whatever was already scheduled so the
    /// output vector has no holes.
    pub async fn dispatch_ordered(
        &self,
        calls: Vec<ContentPart>,
        cancel: CancellationToken,
    ) -> ToolDispatchResult {
        // needs_follow_up tracks whether anything was dispatched at all (codex:
        // any tool output to feed back -> the turn loop continues).
        let dispatched_any = !calls.is_empty();

        let mut ordered: FuturesOrdered<
            std::pin::Pin<Box<dyn std::future::Future<Output = Option<Message>> + Send>>,
        > = FuturesOrdered::new();

        let mut scheduled_calls = Vec::with_capacity(calls.len());
        for call in calls {
            // Stop *scheduling* new calls once cancellation has fired. Already
            // scheduled futures remain in `ordered` and are still drained below
            // (drain_in_flight: started work is observed, not dropped on the floor).
            if cancel.is_cancelled() {
                break;
            }

            scheduled_calls.push(call.clone());
            let parallelism = decision::classify_parallelism(
                self.runner.parallel_safe(&call),
                self.supports_parallel_tool_calls,
            );
            let gate = self.gate.clone();
            let runner = self.runner.clone();
            let cancel = cancel.clone();

            // Each future acquires its own gate guard *inside* the future so the
            // RwLock — not the scheduler — enforces the overlap rules: parallel
            // calls take read guards (concurrent), serial calls take a write guard
            // (exclusive). The guard is held for the duration of the call's run.
            let fut: std::pin::Pin<Box<dyn std::future::Future<Output = Option<Message>> + Send>> =
                Box::pin(async move {
                    match parallelism {
                        ToolParallelism::Parallel => {
                            let _guard = gate.read().await;
                            run_one(runner.as_ref(), call, &cancel).await
                        }
                        ToolParallelism::Serial => {
                            let _guard = gate.write().await;
                            run_one(runner.as_ref(), call, &cancel).await
                        }
                    }
                });
            ordered.push_back(fut);
        }

        // Drain in push order -> outputs are in model order regardless of which
        // future finished first.
        let mut outputs_in_order: Vec<Message> = Vec::with_capacity(ordered.len());
        while let Some(slot) = ordered.next().await {
            if let Some(msg) = slot {
                outputs_in_order.push(msg);
            }
        }
        self.annotate_repeated_browser_script_outputs(&scheduled_calls, &mut outputs_in_order)
            .await;

        ToolDispatchResult {
            outputs_in_order,
            needs_follow_up: dispatched_any,
        }
    }

    async fn annotate_repeated_browser_script_outputs(
        &self,
        calls: &[ContentPart],
        outputs: &mut [Message],
    ) {
        for (call, output) in calls.iter().zip(outputs.iter_mut()) {
            let Some(signature) = no_progress_signature(call, output) else {
                continue;
            };
            let mut state = self.no_progress_state.lock().await;
            let repeats = state.observe(signature);
            if repeats >= BROWSER_SCRIPT_NO_PROGRESS_REPEAT_THRESHOLD {
                append_tool_result_text(output, BROWSER_SCRIPT_NO_PROGRESS_NUDGE);
            }
        }
    }
}

#[derive(Debug, Default)]
struct NoProgressState {
    last: Option<String>,
    repeat_count: usize,
}

impl NoProgressState {
    fn observe(&mut self, signature: String) -> usize {
        if self.last.as_deref() == Some(signature.as_str()) {
            self.repeat_count = self.repeat_count.saturating_add(1);
        } else {
            self.last = Some(signature);
            self.repeat_count = 1;
        }
        self.repeat_count
    }
}

fn no_progress_signature(call: &ContentPart, output: &Message) -> Option<String> {
    let ContentPart::ToolCall { name, .. } = call else {
        return None;
    };
    if name != "browser_script" {
        return None;
    }
    let text = tool_result_text(output);
    if text.trim().is_empty() {
        return None;
    }
    Some(normalize_no_progress_text(&text))
}

fn normalize_no_progress_text(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn tool_result_text(message: &Message) -> String {
    let mut chunks = Vec::new();
    collect_tool_result_text(&message.content, &mut chunks);
    chunks.join("\n")
}

fn collect_tool_result_text(parts: &[ContentPart], chunks: &mut Vec<String>) {
    for part in parts {
        match part {
            ContentPart::Text { text } | ContentPart::Reasoning { text, .. } => {
                if !text.is_empty() {
                    chunks.push(text.clone());
                }
            }
            ContentPart::ToolResult { content, .. } => collect_tool_result_text(content, chunks),
            ContentPart::Media { .. } | ContentPart::ToolCall { .. } => {}
        }
    }
}

fn append_tool_result_text(message: &mut Message, text: &str) {
    for part in &mut message.content {
        if let ContentPart::ToolResult { content, .. } = part {
            content.push(ContentPart::text(text.to_string()));
            return;
        }
    }
}

/// Run a single call, honoring cancellation. Cancellation is model-visible: the
/// tool result records an aborted error instead of silently dropping the call.
async fn run_one<R: CallRunner + ?Sized>(
    runner: &R,
    call: ContentPart,
    cancel: &CancellationToken,
) -> Option<Message> {
    if preserves_result_on_cancel(&call) {
        return Some(runner.run(call, cancel.clone()).await);
    }

    let started = std::time::Instant::now();
    tokio::select! {
        biased;
        _ = cancel.cancelled() => Some(aborted_result_message_for(&call, started.elapsed())),
        msg = runner.run(call.clone(), cancel.clone()) => Some(msg),
    }
}

fn preserves_result_on_cancel(call: &ContentPart) -> bool {
    matches!(call, ContentPart::ToolCall { name, .. } if name == "exec_command")
}

fn aborted_result_message_for(call: &ContentPart, elapsed: std::time::Duration) -> Message {
    result_message_for(
        call,
        &format!("aborted by user after {:.1}s", elapsed.as_secs_f64()),
        true,
    )
}

impl Default for ToolDispatcher<OrchestratorRunner> {
    fn default() -> Self {
        Self::new()
    }
}
