//! End-to-end fusion tests (WP-I-fusion): turn loop -> sampling -> tool dispatch
//! -> context, with tools actually running.
//!
//! These exercise the FUSED path: the production [`ModelSamplingDriver`] streams
//! a (scripted) model response, dispatches the tool calls it emitted through a
//! real [`ToolDispatcher`], records the assistant message + tool outputs back
//! into the SAME conversation buffer the [`TurnLoop`] re-samples from, and
//! reports follow-up so the loop loops. When the model finally emits no tool
//! call, the turn completes.
//!
//! Everything is offline & deterministic: a `ScriptedTransport` replays a fixed
//! per-iteration [`LlmEvent`] sequence (no `ModelClient`, no socket), and tools
//! run through a `ScriptedRunner` (an echo runner + a counter runner) so we can
//! assert the tool was actually invoked AND its output appears in the recorded
//! transcript.
//!
//! The wiring is **Option A** (DESIGN.md "production SamplingDriver fuses
//! dispatch"): the fused driver owns the [`ToolDispatcher`] and a
//! [`FusionRecorder`] pointing at the same buffer the loop's [`TurnState`] reads.
//! The frozen `TurnLoop` / `SamplingDriver` / `SamplingOutcome` shapes are
//! unchanged.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use browser_use_llm::schema::{
    ContentPart, FinishReason, LlmError, LlmEvent, LlmRequest, Message, MessageRole, Usage,
};
use futures_util::stream;
use tokio_util::sync::CancellationToken;

use crate::decision::TokenStatus;
use crate::events::{EventSink, TurnCtx};
use crate::task::TurnLifecycleEvent;
use crate::testkit::RecordingSink;
use crate::turn::dispatch::{CallRunner, ToolDispatcher};
use crate::turn::sampling::{EventStream, FusionRecorder, ModelSamplingDriver, SamplingTransport};
use crate::turn::{TurnLoop, TurnObserver, TurnState};

// ---------------------------------------------------------------------------
// Scripted transport: replays a distinct LlmEvent stream per sampling call.
// ---------------------------------------------------------------------------

/// A transport that returns a fresh scripted event stream on each `open_stream`
/// call, popping the next per-iteration script from a queue. This drives a
/// multi-iteration turn (iter 1 emits a tool call, iter 2 emits only text).
struct ScriptedTransport {
    scripts: Mutex<std::collections::VecDeque<Vec<LlmEvent>>>,
}

impl ScriptedTransport {
    fn new(scripts: Vec<Vec<LlmEvent>>) -> Self {
        Self {
            scripts: Mutex::new(scripts.into_iter().collect()),
        }
    }
}

impl SamplingTransport for ScriptedTransport {
    fn open_stream<'a>(&'a self, _req: &LlmRequest) -> Result<EventStream<'a>, LlmError> {
        // Pop the next iteration's script; reuse an empty stream past the end so
        // a mis-scripted test fails by assertion rather than panicking here.
        let events = self.scripts.lock().unwrap().pop_front().unwrap_or_default();
        let items: Vec<Result<LlmEvent, LlmError>> = events.into_iter().map(Ok).collect();
        Ok(Box::pin(stream::iter(items)))
    }
}

// ---------------------------------------------------------------------------
// Scripted call runner: real tool execution, instrumented for assertions.
// ---------------------------------------------------------------------------

/// A [`CallRunner`] that records every invoked call (id + name + raw input) and
/// returns a deterministic tool-result `Message` keyed by the call id, so a test
/// can assert the tool ran with the model's exact arguments and that its output
/// lands in the transcript in model order.
///
/// Each call's output text is `"<name>:<input>"` — every call in these tests has
/// a distinct (name, input) so the recorded outputs are individually
/// identifiable; *order* is what the dispatcher (`FuturesOrdered`) guarantees and
/// what we assert, so no shared sequence counter is needed.
struct ScriptedRunner {
    /// (call_id, tool_name, raw_input_json) for each invocation, in run order.
    invocations: Arc<Mutex<Vec<(String, String, String)>>>,
    /// Per-call parallel-safety (all serial here; ordering is what we assert).
    parallel_safe: bool,
}

impl ScriptedRunner {
    fn new(parallel_safe: bool) -> Arc<Self> {
        Arc::new(Self {
            invocations: Arc::new(Mutex::new(Vec::new())),
            parallel_safe,
        })
    }
}

#[async_trait]
impl CallRunner for ScriptedRunner {
    fn parallel_safe(&self, _call: &ContentPart) -> bool {
        self.parallel_safe
    }

    async fn run(
        &self,
        call: ContentPart,
        _cancel: tokio_util::sync::CancellationToken,
    ) -> Message {
        let (id, name, input) = match &call {
            ContentPart::ToolCall {
                id, name, input, ..
            } => (id.clone(), name.clone(), input.to_string()),
            _ => (String::new(), String::new(), String::new()),
        };
        self.invocations
            .lock()
            .unwrap()
            .push((id.clone(), name.clone(), input.clone()));
        // Echo the call back; (name, input) uniquely identifies it in assertions.
        Message::new(
            MessageRole::Tool,
            vec![ContentPart::ToolResult {
                tool_call_id: id,
                content: vec![ContentPart::text(format!("{name}:{input}"))],
                is_error: false,
            }],
        )
    }
}

// ---------------------------------------------------------------------------
// Shared conversation: both the loop's TurnState AND the driver's recorder.
// ---------------------------------------------------------------------------

/// A network-free conversation buffer used as BOTH the [`TurnLoop`]'s
/// [`TurnState`] and the fused driver's [`FusionRecorder`] (shared via `Arc`),
/// so what the driver records is exactly what the loop re-samples next iteration
/// — the load-bearing fusion seam.
struct SharedConversation {
    history: Mutex<Vec<Message>>,
    token_status: Mutex<TokenStatus>,
}

impl SharedConversation {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            history: Mutex::new(Vec::new()),
            token_status: Mutex::new(token_status_ok()),
        })
    }

    fn messages(&self) -> Vec<Message> {
        self.history.lock().unwrap().clone()
    }
}

// The loop drives `TurnState` through a shared `Arc<SharedConversation>`.
impl TurnState for Arc<SharedConversation> {
    async fn clone_history_for_prompt(&self) -> Vec<Message> {
        self.history.lock().unwrap().clone()
    }

    async fn record_items(&self, items: &[Message]) {
        self.history.lock().unwrap().extend_from_slice(items);
    }

    async fn has_pending_input(&self) -> bool {
        false
    }

    async fn take_pending_input(&self) -> Vec<Message> {
        Vec::new()
    }

    async fn token_status(&self) -> TokenStatus {
        self.token_status.lock().unwrap().clone()
    }
}

// The fused driver records through the SAME buffer via `FusionRecorder`.
#[async_trait]
impl FusionRecorder for SharedConversation {
    async fn record(&self, messages: &[Message]) {
        self.history.lock().unwrap().extend_from_slice(messages);
    }
}

// ---------------------------------------------------------------------------
// Recording observer.
// ---------------------------------------------------------------------------

#[derive(Default)]
struct RecordingObserver {
    kinds: Mutex<Vec<&'static str>>,
}

impl RecordingObserver {
    fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }
    fn kinds(&self) -> Vec<&'static str> {
        self.kinds.lock().unwrap().clone()
    }
}

impl TurnObserver for Arc<RecordingObserver> {
    fn on_lifecycle(&self, ev: TurnLifecycleEvent) {
        let k = match ev {
            TurnLifecycleEvent::TurnStarted { .. } => "started",
            TurnLifecycleEvent::TurnComplete { .. } => "complete",
            TurnLifecycleEvent::TurnAborted { .. } => "aborted",
        };
        self.kinds.lock().unwrap().push(k);
    }
}

// ---------------------------------------------------------------------------
// Helpers.
// ---------------------------------------------------------------------------

fn ctx() -> TurnCtx {
    TurnCtx {
        session_id: "sess-fusion".to_string(),
        model: "gpt-5-codex".to_string(),
        provider: "openai".to_string(),
        base_instructions: crate::prompts::browser_agent_system_prompt(),
        browser_mode_instruction: None,
        turn_idx: 0,
        attempt: 0,
    }
}

fn token_status_ok() -> TokenStatus {
    TokenStatus {
        auto_compact_scope_tokens: 0,
        auto_compact_scope_limit: 1,
        full_context_window_limit_reached: false,
        token_limit_reached: false,
    }
}

fn text_delta(s: &str) -> LlmEvent {
    LlmEvent::TextDelta {
        id: "t0".to_string(),
        delta: s.to_string(),
    }
}

fn tool_call_ev(id: &str, name: &str, input: serde_json::Value) -> LlmEvent {
    LlmEvent::ToolCall {
        id: id.to_string(),
        name: name.to_string(),
        namespace: None,
        input,
    }
}

fn finish() -> LlmEvent {
    LlmEvent::Finish {
        usage: Usage::default(),
        finish_reason: Some(FinishReason::Stop),
    }
}

/// Every `ToolResult` payload text across the recorded transcript, in order.
fn tool_result_texts(conv: &SharedConversation) -> Vec<String> {
    conv.messages()
        .into_iter()
        .filter(|m| matches!(m.role, MessageRole::Tool))
        .flat_map(|m| m.content.into_iter())
        .filter_map(|p| match p {
            ContentPart::ToolResult { content, .. } => Some(
                content
                    .into_iter()
                    .filter_map(|c| match c {
                        ContentPart::Text { text } => Some(text),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join(""),
            ),
            _ => None,
        })
        .collect()
}

fn build_driver(
    scripts: Vec<Vec<LlmEvent>>,
    runner: Arc<ScriptedRunner>,
    conv: Arc<SharedConversation>,
) -> ModelSamplingDriver<ScriptedTransport, Arc<ScriptedRunner>> {
    let sink: Arc<dyn EventSink> = Arc::new(RecordingSink::default());
    let dispatcher = Arc::new(ToolDispatcher::with_runner(
        runner, /* model_supports */ true,
    ));
    let recorder: Arc<dyn FusionRecorder> = conv;
    ModelSamplingDriver::new(ScriptedTransport::new(scripts), sink, ctx(), 3)
        .without_jitter()
        .with_fusion(dispatcher, recorder)
}

// ---------------------------------------------------------------------------
// THE key end-to-end test.
// ---------------------------------------------------------------------------

/// Iteration 1's model response contains a tool call; the fused driver
/// dispatches it and records the tool-result message into the shared
/// conversation; iteration 2's request (built from that conversation) sees the
/// output and the model emits no tool call, so the turn completes. Asserts the
/// tool was actually invoked with the model's exact arguments AND that its
/// output is in the recorded transcript.
#[tokio::test]
async fn turn_dispatches_tool_then_completes_on_followup() {
    let runner = ScriptedRunner::new(/* parallel_safe */ false);
    let invocations = runner.invocations.clone();
    let conv = SharedConversation::new();

    // iter 1: model emits one echo tool call; iter 2: model emits final text.
    let scripts = vec![
        vec![
            text_delta("calling echo"),
            tool_call_ev("call-1", "echo", serde_json::json!({ "msg": "hi" })),
            finish(),
        ],
        vec![text_delta("all done"), finish()],
    ];

    let driver = build_driver(scripts, runner, conv.clone());
    let observer = RecordingObserver::new();
    let turn = TurnLoop::new(conv.clone(), driver, observer.clone());

    let out = turn
        .run(
            ctx(),
            /* turn_has_fresh_input */ false,
            CancellationToken::new(),
        )
        .await
        .expect("fused turn should complete");

    // Final assistant text is the turn result.
    assert_eq!(out.as_deref(), Some("all done"));

    // The tool actually ran exactly once, with the model's exact arguments.
    let invoked = invocations.lock().unwrap().clone();
    assert_eq!(invoked.len(), 1, "echo tool must run exactly once");
    assert_eq!(invoked[0].0, "call-1");
    assert_eq!(invoked[0].1, "echo");
    assert_eq!(invoked[0].2, "{\"msg\":\"hi\"}");

    // The tool output landed in the recorded transcript.
    let results = tool_result_texts(&conv);
    assert_eq!(
        results,
        vec!["echo:{\"msg\":\"hi\"}".to_string()],
        "the dispatched tool output must be recorded into the conversation"
    );

    // Transcript shape: assistant(text+call) -> tool(result). The FINAL text-only
    // turn ("all done") is returned by the loop as `last_agent_message`; the fused
    // driver records on the tool-call branch only (recording the final answer is
    // the loop/session caller's job), so iteration 2 appends nothing.
    let roles: Vec<MessageRole> = conv.messages().iter().map(|m| m.role).collect();
    assert_eq!(
        roles,
        vec![MessageRole::Assistant, MessageRole::Tool],
        "fused turn records the assistant call then its tool output"
    );

    assert_eq!(observer.kinds(), vec!["started", "complete"]);
}

// ---------------------------------------------------------------------------
// Multi-tool-call iteration records outputs in model order.
// ---------------------------------------------------------------------------

/// A single iteration emitting three tool calls records their outputs back in
/// the exact order the model emitted them, in one `Tool`-role message per output.
#[tokio::test]
async fn multi_tool_iteration_records_outputs_in_model_order() {
    // Serial runner so there is a single, deterministic invocation order.
    let runner = ScriptedRunner::new(/* parallel_safe */ false);
    let invocations = runner.invocations.clone();
    let conv = SharedConversation::new();

    // Model order: gamma, alpha, beta (deliberately not alphabetical).
    let scripts = vec![
        vec![
            tool_call_ev("c", "gamma", serde_json::json!({})),
            tool_call_ev("a", "alpha", serde_json::json!({})),
            tool_call_ev("b", "beta", serde_json::json!({})),
            finish(),
        ],
        vec![text_delta("finished"), finish()],
    ];

    let driver = build_driver(scripts, runner, conv.clone());
    let observer = RecordingObserver::new();
    let turn = TurnLoop::new(conv.clone(), driver, observer);

    let out = turn
        .run(ctx(), false, CancellationToken::new())
        .await
        .expect("fused multi-call turn should complete");
    assert_eq!(out.as_deref(), Some("finished"));

    // All three tools ran, in model order.
    let names: Vec<String> = invocations
        .lock()
        .unwrap()
        .iter()
        .map(|i| i.1.clone())
        .collect();
    assert_eq!(names, vec!["gamma", "alpha", "beta"]);

    // Their recorded outputs are in model order. The dispatcher's FuturesOrdered
    // guarantees output[i] corresponds to call[i] regardless of completion order;
    // each output is its own `<name>:<input>` payload.
    let results = tool_result_texts(&conv);
    assert_eq!(
        results,
        vec![
            "gamma:{}".to_string(),
            "alpha:{}".to_string(),
            "beta:{}".to_string(),
        ],
        "tool outputs must be recorded in model order"
    );

    // The dispatcher returns one `Message` per tool output, so three calls record
    // three Tool-role messages (in model order) after the single assistant message.
    let roles: Vec<MessageRole> = conv.messages().iter().map(|m| m.role).collect();
    assert_eq!(
        roles,
        vec![
            MessageRole::Assistant,
            MessageRole::Tool,
            MessageRole::Tool,
            MessageRole::Tool,
        ],
        "assistant(3 calls) then one Tool message per output, in model order"
    );
}

// ---------------------------------------------------------------------------
// Regression: a turn with zero tool calls completes in one iteration.
// ---------------------------------------------------------------------------

/// A turn whose first (and only) model response has no tool calls completes in a
/// single iteration and records nothing via the fusion path — even with a
/// dispatcher configured (the dispatch path must be skipped when no call fires).
#[tokio::test]
async fn zero_tool_calls_completes_in_one_iteration() {
    let runner = ScriptedRunner::new(false);
    let invocations = runner.invocations.clone();
    let conv = SharedConversation::new();

    let scripts = vec![vec![text_delta("just text, no tools"), finish()]];

    let driver = build_driver(scripts, runner, conv.clone());
    let observer = RecordingObserver::new();
    let turn = TurnLoop::new(conv.clone(), driver, observer.clone());

    let out = turn
        .run(ctx(), false, CancellationToken::new())
        .await
        .expect("text-only turn should complete");
    assert_eq!(out.as_deref(), Some("just text, no tools"));

    // No tool ran; nothing recorded through the fusion path; one iteration only.
    assert!(invocations.lock().unwrap().is_empty(), "no tool should run");
    assert!(
        tool_result_texts(&conv).is_empty(),
        "no tool output recorded"
    );
    assert!(
        conv.messages().is_empty(),
        "a no-tool-call turn records nothing via the fusion path (text-only)"
    );
    assert_eq!(observer.kinds(), vec!["started", "complete"]);
}
