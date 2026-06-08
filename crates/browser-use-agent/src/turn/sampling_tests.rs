//! Tests for the async [`ModelSamplingDriver`] (WP-B5).
//!
//! NETWORK-FREE: every test injects a `ScriptedTransport` that returns canned
//! [`LlmEvent`] sequences (and can fail N times before succeeding). No
//! `ModelClient` / no real socket is ever touched. Jitter is disabled via
//! `ModelSamplingDriver::without_jitter` so backoff sleeps are deterministic;
//! the retry tests therefore sleep the exact `backoff_ms` values (a few hundred
//! ms total — `tokio`'s `test-util`/`start_paused` is intentionally not required
//! so no Cargo manifest change is needed).

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use browser_use_llm::schema::{
    CacheHint, ContentPart, FinishReason, LlmError, LlmErrorReason, LlmEvent, LlmRequest, Message,
    MessageRole, TextPhase, Usage,
};
use browser_use_protocol::EventRecord;
use futures_util::stream;
use tokio_util::sync::CancellationToken;

use crate::events::names;
use crate::events::{EventSink, TurnCtx};
use crate::goals::{GOAL_ACCOUNTED_EVENT, GOAL_SET_EVENT};
use crate::testkit::RecordingSink;
use crate::tools::handlers::goal::GoalStore;
use crate::turn::sampling::{EventStream, ModelSamplingDriver, SamplingTransport};
use crate::turn::SamplingDriver;
use crate::AgentError;

// ---- scripted transport ---------------------------------------------------

/// One open attempt's result: either the open itself fails (codex
/// `stream().await -> Err`), or it yields a stream of items (each of which may be
/// an `Err`, modeling a mid-flight break).
enum OpenScript {
    OpenErr(LlmError),
    Stream(Vec<Result<LlmEvent, LlmError>>),
}

/// A transport that replays a queue of [`OpenScript`]s, one per `open_stream`
/// call. The shared `opens` counter lets a test assert how many opens (retries +
/// the final success) happened; a `Clone`able `Arc` handle to it is returned so
/// the test can read it after the driver consumes the transport.
struct ScriptedTransport {
    scripts: Vec<OpenScript>,
    next: AtomicUsize,
    opens: Arc<AtomicUsize>,
}

impl ScriptedTransport {
    fn new(scripts: Vec<OpenScript>) -> (Self, Arc<AtomicUsize>) {
        let opens = Arc::new(AtomicUsize::new(0));
        (
            Self {
                scripts,
                next: AtomicUsize::new(0),
                opens: opens.clone(),
            },
            opens,
        )
    }
}

impl SamplingTransport for ScriptedTransport {
    fn open_stream<'a>(&'a self, _req: &LlmRequest) -> Result<EventStream<'a>, LlmError> {
        self.opens.fetch_add(1, Ordering::SeqCst);
        let idx = self.next.fetch_add(1, Ordering::SeqCst);
        // Past the end of the queue, reuse the last script (defensive).
        let script = self.scripts.get(idx).or_else(|| self.scripts.last());
        match script {
            Some(OpenScript::OpenErr(e)) => Err(e.clone()),
            Some(OpenScript::Stream(items)) => Ok(Box::pin(stream::iter(items.clone()))),
            None => Ok(Box::pin(stream::empty())),
        }
    }
}

/// A transport whose stream never yields (so only cancellation can end the loop).
struct PendingTransport;

impl SamplingTransport for PendingTransport {
    fn open_stream<'a>(&'a self, _req: &LlmRequest) -> Result<EventStream<'a>, LlmError> {
        Ok(Box::pin(stream::pending()))
    }
}

/// A transport that RECORDS the [`LlmRequest`] handed to `open_stream` so a test
/// can assert the driver threads the real per-turn request (populated input)
/// through — i.e. the per-call `req` is used, not the empty one seeded at
/// construction. This is the regression seam for the empty-request bug.
struct RecordingTransport {
    /// The requests captured on each `open_stream`, in order.
    seen: Arc<Mutex<Vec<LlmRequest>>>,
    /// Canned events to stream back so the driver completes a turn.
    events: Vec<Result<LlmEvent, LlmError>>,
}

impl RecordingTransport {
    fn new(events: Vec<Result<LlmEvent, LlmError>>) -> (Self, Arc<Mutex<Vec<LlmRequest>>>) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        (
            Self {
                seen: seen.clone(),
                events,
            },
            seen,
        )
    }
}

impl SamplingTransport for RecordingTransport {
    fn open_stream<'a>(&'a self, req: &LlmRequest) -> Result<EventStream<'a>, LlmError> {
        // Capture exactly what the driver passed for this open.
        self.seen.lock().unwrap().push(req.clone());
        Ok(Box::pin(stream::iter(self.events.clone())))
    }
}

// ---- helpers --------------------------------------------------------------

fn ctx() -> TurnCtx {
    TurnCtx {
        session_id: "sess-1".to_string(),
        model: "gpt-5-codex".to_string(),
        provider: "openai".to_string(),
        base_instructions: crate::prompts::browser_agent_system_prompt(),
        browser_mode_instruction: None,
        turn_idx: 0,
        attempt: 0,
    }
}

fn driver(
    transport: ScriptedTransport,
    sink: Arc<RecordingSink>,
    max_retries: u32,
) -> ModelSamplingDriver<ScriptedTransport> {
    let sink: Arc<dyn EventSink> = sink;
    ModelSamplingDriver::new(transport, sink, ctx(), max_retries).without_jitter()
}

fn event_record(seq: i64, ty: &str, payload: serde_json::Value) -> EventRecord {
    EventRecord {
        seq,
        id: format!("event-{seq}"),
        session_id: "sess-1".to_string(),
        ts_ms: seq * 1000,
        event_type: ty.to_string(),
        payload,
    }
}

fn active_goal_store(sink: Arc<RecordingSink>) -> Arc<GoalStore> {
    let sink: Arc<dyn EventSink> = sink;
    Arc::new(GoalStore::from_event_records(
        "sess-1",
        sink,
        &[event_record(
            1,
            GOAL_SET_EVENT,
            serde_json::json!({
                "goal_id": "goal-1",
                "objective": "finish the active goal",
                "status": "active",
                "token_budget": 1000,
            }),
        )],
    ))
}

fn user_input() -> Vec<Message> {
    vec![Message::new(
        MessageRole::User,
        vec![ContentPart::text("hello")],
    )]
}

fn text_delta(s: &str) -> Result<LlmEvent, LlmError> {
    Ok(LlmEvent::TextDelta {
        id: "t0".to_string(),
        delta: s.to_string(),
    })
}

fn text_end() -> Result<LlmEvent, LlmError> {
    Ok(LlmEvent::TextEnd {
        id: "t0".to_string(),
        phase: None,
    })
}

fn commentary_text_end() -> Result<LlmEvent, LlmError> {
    Ok(LlmEvent::TextEnd {
        id: "t0".to_string(),
        phase: Some(TextPhase::Commentary),
    })
}

fn final_answer_text_end() -> Result<LlmEvent, LlmError> {
    Ok(LlmEvent::TextEnd {
        id: "t0".to_string(),
        phase: Some(TextPhase::FinalAnswer),
    })
}

fn reasoning_end() -> Result<LlmEvent, LlmError> {
    Ok(LlmEvent::ReasoningEnd {
        id: "r0".to_string(),
    })
}

fn tool_call(name: &str) -> Result<LlmEvent, LlmError> {
    Ok(LlmEvent::ToolCall {
        id: "call-1".to_string(),
        name: name.to_string(),
        namespace: None,
        input: serde_json::json!({"arg": 1}),
    })
}

fn tool_call_with_input(name: &str, input: serde_json::Value) -> Result<LlmEvent, LlmError> {
    Ok(LlmEvent::ToolCall {
        id: "call-1".to_string(),
        name: name.to_string(),
        namespace: None,
        input,
    })
}

fn finish(reason: FinishReason) -> Result<LlmEvent, LlmError> {
    Ok(LlmEvent::Finish {
        usage: Usage {
            input_tokens: 10,
            output_tokens: 5,
            total_tokens: 15,
            ..Default::default()
        },
        finish_reason: Some(reason),
    })
}

/// A retryable transport-level error (codex `is_retryable() == true`).
fn retryable_err(msg: &str) -> LlmError {
    let e = LlmError::new(LlmErrorReason::Transport, msg);
    assert!(e.retryable, "Transport errors must be retryable");
    e
}

fn provider_error(message: &str) -> Result<LlmEvent, LlmError> {
    Ok(LlmEvent::ProviderError {
        message: message.to_string(),
        retryable: false,
    })
}

// ---- (1) text deltas + tool call -> follow_up + emitted events ------------

#[tokio::test]
async fn tool_call_sets_follow_up_and_records_message_and_events() {
    let (transport, _opens) = ScriptedTransport::new(vec![OpenScript::Stream(vec![
        text_delta("Let me "),
        text_delta("look that up."),
        tool_call("search"),
        finish(FinishReason::ToolUse),
    ])]);
    let sink = Arc::new(RecordingSink::default());
    let d = driver(transport, sink.clone(), 5);

    let out = d
        .run_sampling_request(user_input(), CancellationToken::new())
        .await
        .expect("sampling should succeed");

    assert!(
        out.model_needs_follow_up,
        ">=1 tool call must set model_needs_follow_up"
    );
    assert_eq!(
        out.last_agent_message.as_deref(),
        Some("Let me look that up."),
        "assistant text deltas must be concatenated into last_agent_message"
    );
    assert_eq!(out.finish_reason, Some(FinishReason::ToolUse));

    // Events were emitted to the sink: two stream deltas, a tool.started, and
    // terminal token_count. Goal accounting is active-goal gated.
    let events = sink.drain();
    let types: Vec<&str> = events.iter().map(|e| e.event_type.as_str()).collect();
    assert_eq!(
        types,
        vec![
            names::MODEL_TURN_REQUEST,
            names::MODEL_STREAM_DELTA,
            names::MODEL_STREAM_DELTA,
            names::TOOL_STARTED,
            names::TOKEN_COUNT,
        ],
        "mapped UI events must be emitted in stream order"
    );
    // The tool.started payload carries the tool name.
    let tool_started = &events[3];
    assert_eq!(tool_started.payload["name"], serde_json::json!("search"));
}

#[tokio::test]
async fn finish_accounts_usage_only_when_goal_is_active() {
    let (transport, _opens) =
        ScriptedTransport::new(vec![OpenScript::Stream(vec![finish(FinishReason::Stop)])]);
    let sink = Arc::new(RecordingSink::default());
    let goal_store = active_goal_store(sink.clone());
    let d = driver(transport, sink.clone(), 5).with_goal_store(goal_store);

    let _ = d
        .run_sampling_request(user_input(), CancellationToken::new())
        .await
        .expect("sampling should succeed");

    let events = sink.drain();
    let accounted = events
        .iter()
        .find(|event| event.event_type == GOAL_ACCOUNTED_EVENT)
        .expect("active goal should emit accounting");
    assert_eq!(accounted.payload["tokensUsed"], serde_json::json!(15));
    assert_eq!(
        accounted.payload["goal"]["tokensUsed"],
        serde_json::json!(15)
    );
}

#[tokio::test]
async fn repeated_sampling_requests_emit_monotonic_turn_indices() {
    let (transport, _opens) = ScriptedTransport::new(vec![
        OpenScript::Stream(vec![finish(FinishReason::Stop)]),
        OpenScript::Stream(vec![finish(FinishReason::Stop)]),
    ]);
    let sink = Arc::new(RecordingSink::default());
    let d = driver(transport, sink.clone(), 5);

    let _ = d
        .run_sampling_request(user_input(), CancellationToken::new())
        .await
        .expect("first sampling request should succeed");
    let _ = d
        .run_sampling_request(user_input(), CancellationToken::new())
        .await
        .expect("second sampling request should succeed");

    let events = sink.drain();
    let turn_request_indices: Vec<i64> = events
        .iter()
        .filter(|event| event.event_type == names::MODEL_TURN_REQUEST)
        .map(|event| event.payload["turn_idx"].as_i64().expect("turn_idx"))
        .collect();
    let token_count_indices: Vec<i64> = events
        .iter()
        .filter(|event| event.event_type == names::TOKEN_COUNT)
        .map(|event| event.payload["turn_idx"].as_i64().expect("turn_idx"))
        .collect();

    assert_eq!(turn_request_indices, vec![0, 1]);
    assert_eq!(token_count_indices, vec![0, 1]);
}

#[tokio::test]
async fn active_goal_context_is_injected_with_codex_envelope() {
    let (transport, seen) =
        RecordingTransport::new(vec![text_delta("ok"), finish(FinishReason::Stop)]);
    let sink = Arc::new(RecordingSink::default());
    let goal_store = active_goal_store(sink.clone());
    let d = ModelSamplingDriver::new(transport, sink, ctx(), 5)
        .without_jitter()
        .with_goal_store(goal_store);

    let _ = d
        .run_sampling_request(user_input(), CancellationToken::new())
        .await
        .expect("sampling should succeed");

    let captured = seen.lock().unwrap();
    let req = captured.first().expect("request captured");
    assert_eq!(req.messages.len(), 2);
    let ContentPart::Text { text } = &req.messages[0].content[0] else {
        panic!("goal context should be text");
    };
    assert!(text.starts_with("<goal_context>\n"));
    assert!(text.ends_with("\n</goal_context>"));
    assert!(text.contains("<objective>\nfinish the active goal\n</objective>"));
    let mut expected_user = user_input()[0].clone();
    expected_user.cache = Some(CacheHint::Ephemeral);
    assert_eq!(req.messages[1], expected_user);
}

// ---- (2) text-only stream -> no follow_up ---------------------------------

#[tokio::test]
async fn text_only_stream_does_not_request_follow_up() {
    let (transport, _opens) = ScriptedTransport::new(vec![OpenScript::Stream(vec![
        text_delta("All done."),
        finish(FinishReason::Stop),
    ])]);
    let sink = Arc::new(RecordingSink::default());
    let d = driver(transport, sink.clone(), 5);

    let out = d
        .run_sampling_request(user_input(), CancellationToken::new())
        .await
        .expect("sampling should succeed");

    assert!(
        !out.model_needs_follow_up,
        "no tool call -> model_needs_follow_up must be false"
    );
    assert_eq!(out.last_agent_message.as_deref(), Some("All done."));
    assert!(
        !out.defers_mailbox_delivery_to_next_turn,
        "without a text close, the driver should not infer a turn boundary"
    );
    assert_eq!(out.finish_reason, Some(FinishReason::Stop));

    let types: Vec<String> = sink.drain().iter().map(|e| e.event_type.clone()).collect();
    assert!(
        !types.iter().any(|t| t == names::TOOL_STARTED),
        "no tool.started event for a text-only turn"
    );
}

#[tokio::test]
async fn final_answer_text_end_defers_mailbox_delivery_to_next_turn() {
    let (transport, _opens) = ScriptedTransport::new(vec![OpenScript::Stream(vec![
        text_delta("All done."),
        final_answer_text_end(),
        finish(FinishReason::Stop),
    ])]);
    let sink = Arc::new(RecordingSink::default());
    let d = driver(transport, sink, 5);

    let out = d
        .run_sampling_request(user_input(), CancellationToken::new())
        .await
        .expect("sampling should succeed");

    assert!(!out.model_needs_follow_up);
    assert_eq!(out.last_agent_message.as_deref(), Some("All done."));
    assert!(
        out.defers_mailbox_delivery_to_next_turn,
        "final-answer text is a Codex next-turn mailbox boundary"
    );
}

// ---- (3) retryable failures then success ----------------------------------

#[tokio::test]
async fn retries_after_retryable_failures_then_succeeds() {
    // The first two opens fail with a retryable transport error; the third serves
    // a successful script. With max_retries == 5 the driver must retry twice and
    // then succeed. Jitter is off, so the two sleeps are backoff_ms(1)+backoff_ms(2)
    // == 400ms + 800ms (sub-second, deterministic).
    let (transport, opens) = ScriptedTransport::new(vec![
        OpenScript::OpenErr(retryable_err("boom 1")),
        OpenScript::OpenErr(retryable_err("boom 2")),
        OpenScript::Stream(vec![text_delta("recovered"), finish(FinishReason::Stop)]),
    ]);
    let sink = Arc::new(RecordingSink::default());
    let d = driver(transport, sink.clone(), 5);

    let out = d
        .run_sampling_request(user_input(), CancellationToken::new())
        .await
        .expect("driver should recover after retryable failures");

    assert_eq!(out.last_agent_message.as_deref(), Some("recovered"));
    // 2 failed opens + 1 successful open == 3 total open attempts.
    assert_eq!(
        opens.load(Ordering::SeqCst),
        3,
        "driver must retry the two failures then succeed on the third open"
    );
}

// ---- (3b) retries are bounded by max_retries; non-retryable fails fast -----

#[tokio::test]
async fn retries_are_bounded_by_max_retries() {
    // Every open fails retryably and max_retries == 2, so the driver makes the
    // initial attempt + 2 retries == 3 opens, then returns the provider error.
    // Jitter off: the two sleeps are backoff_ms(1)+backoff_ms(2) (sub-second).
    let (transport, opens) =
        ScriptedTransport::new(vec![OpenScript::OpenErr(retryable_err("always down"))]);
    let sink = Arc::new(RecordingSink::default());
    let d = driver(transport, sink, 2);

    let err = d
        .run_sampling_request(user_input(), CancellationToken::new())
        .await
        .expect_err("exhausting the retry budget must fail");
    assert!(
        matches!(err, AgentError::Provider(_)),
        "exhausted retries surface the provider error, got {err:?}"
    );
    assert_eq!(
        opens.load(Ordering::SeqCst),
        3,
        "initial attempt + max_retries(2) == 3 opens"
    );
}

#[tokio::test]
async fn non_retryable_error_fails_without_retrying() {
    // An InvalidRequest is not retryable: one open, immediate failure.
    let (transport, opens) = ScriptedTransport::new(vec![OpenScript::OpenErr(LlmError::new(
        LlmErrorReason::InvalidRequest,
        "bad request",
    ))]);
    let sink = Arc::new(RecordingSink::default());
    let d = driver(transport, sink, 5);

    let err = d
        .run_sampling_request(user_input(), CancellationToken::new())
        .await
        .expect_err("non-retryable error must fail");
    assert!(matches!(err, AgentError::Provider(_)), "got {err:?}");
    assert_eq!(
        opens.load(Ordering::SeqCst),
        1,
        "non-retryable errors must not be retried"
    );
}

#[tokio::test]
async fn streamed_provider_error_fails_turn_after_emitting_stream_error() {
    let (transport, opens) =
        ScriptedTransport::new(vec![OpenScript::Stream(vec![provider_error(
            "provider exploded",
        )])]);
    let sink = Arc::new(RecordingSink::default());
    let d = driver(transport, sink.clone(), 5);

    let err = d
        .run_sampling_request(user_input(), CancellationToken::new())
        .await
        .expect_err("in-stream provider errors must fail the turn");

    assert!(matches!(err, AgentError::Provider(_)), "got {err:?}");
    assert_eq!(opens.load(Ordering::SeqCst), 1);
    let events = sink.events.lock().expect("recording sink poisoned");
    assert!(events.iter().any(|event| {
        event.event_type == names::STREAM_ERROR
            && event.payload["message"].as_str() == Some("provider exploded")
    }));
}

#[tokio::test]
async fn streamed_context_window_provider_error_maps_to_context_window_exceeded() {
    let (transport, _opens) =
        ScriptedTransport::new(vec![OpenScript::Stream(vec![provider_error(
            "Your input exceeds the context window of this model.",
        )])]);
    let sink = Arc::new(RecordingSink::default());
    let d = driver(transport, sink, 5);

    let err = d
        .run_sampling_request(user_input(), CancellationToken::new())
        .await
        .expect_err("context-window provider events must fail the turn");

    assert!(
        matches!(err, AgentError::ContextWindowExceeded),
        "got {err:?}"
    );
}

// ---- (4) cancellation mid-stream -> TurnAborted ---------------------------

#[tokio::test]
async fn cancellation_mid_stream_returns_turn_aborted() {
    // A never-ending stream: the cancel branch of the select! must win.
    let sink: Arc<dyn EventSink> = Arc::new(RecordingSink::default());
    let d = ModelSamplingDriver::new(PendingTransport, sink, ctx(), 5).without_jitter();

    let cancel = CancellationToken::new();
    let cancel2 = cancel.clone();
    // Cancel shortly after the request starts consuming.
    tokio::spawn(async move {
        cancel2.cancel();
    });

    let err = d
        .run_sampling_request(user_input(), cancel)
        .await
        .expect_err("cancellation must abort the turn");
    assert!(
        matches!(err, AgentError::TurnAborted),
        "cancelled turn must return AgentError::TurnAborted, got {err:?}"
    );
}

// ---- (5) the driver passes the REAL per-call request to open_stream --------

#[tokio::test]
async fn driver_passes_populated_per_call_request_to_open_stream() {
    // Regression for the empty-request bug: the production transport used to ignore
    // the `req` passed to `open_stream` and stream a fixed request built once with
    // EMPTY messages, so the provider received no input (HTTP 400). Here a
    // RecordingTransport captures the request the driver actually hands it; we
    // assert it carries the populated input messages, not an empty body.
    let (transport, seen) =
        RecordingTransport::new(vec![text_delta("ok"), finish(FinishReason::Stop)]);
    let sink: Arc<dyn EventSink> = Arc::new(RecordingSink::default());
    let d = ModelSamplingDriver::new(transport, sink, ctx(), 5).without_jitter();

    let input = user_input();
    let _ = d
        .run_sampling_request(input.clone(), CancellationToken::new())
        .await
        .expect("sampling should succeed");

    let captured = seen.lock().unwrap();
    assert_eq!(
        captured.len(),
        1,
        "exactly one open for a single successful turn"
    );
    let req = &captured[0];
    // The body the provider would receive MUST be non-empty (the bug shipped an
    // empty `messages` vec, which the provider rejects).
    assert!(
        !req.messages.is_empty(),
        "open_stream received an EMPTY request — the empty-input bug is back"
    );
    // And it must be EXACTLY the input the driver was asked to sample, with the
    // turn's model/provider identity from `ctx()`.
    let mut expected_messages = input.clone();
    expected_messages.last_mut().unwrap().cache = Some(CacheHint::Ephemeral);
    assert_eq!(
        req.messages, expected_messages,
        "open_stream must receive the driver's per-call input messages with the current-state cache hint"
    );
    // `req.model`/`req.provider` are the `ModelId`/`ProviderId` newtypes; compare
    // against the same `.into()` conversion `LlmRequest::new` applies to `ctx()`.
    assert_eq!(
        req.model,
        ctx().model.into(),
        "request carries the turn's model"
    );
    assert_eq!(
        req.provider,
        ctx().provider.into(),
        "request carries the turn's provider"
    );
    assert_eq!(
        req.system.first().and_then(|part| part.cache),
        Some(CacheHint::Ephemeral),
        "stable base system prompt should be cacheable for providers that support prompt caching"
    );
    assert_eq!(
        req.messages.last().and_then(|message| message.cache),
        Some(CacheHint::Ephemeral),
        "latest browser-state message should be cacheable like the Python Anthropic serializer"
    );
}

#[tokio::test]
async fn open_stream_marks_an_earlier_cache_breakpoint_for_long_histories() {
    let (transport, seen) =
        RecordingTransport::new(vec![text_delta("ok"), finish(FinishReason::Stop)]);
    let sink: Arc<dyn EventSink> = Arc::new(RecordingSink::default());
    let d = ModelSamplingDriver::new(transport, sink, ctx(), 5).without_jitter();

    let input: Vec<Message> = (0..25)
        .map(|index| {
            Message::new(
                MessageRole::User,
                vec![ContentPart::text(format!("browser state {index}"))],
            )
        })
        .collect();
    let _ = d
        .run_sampling_request(input, CancellationToken::new())
        .await
        .expect("sampling should succeed");

    let captured = seen.lock().unwrap();
    let req = &captured[0];
    let cache_indices: Vec<usize> = req
        .messages
        .iter()
        .enumerate()
        .filter_map(|(index, message)| {
            (message.cache == Some(CacheHint::Ephemeral)).then_some(index)
        })
        .collect();

    assert_eq!(
        cache_indices,
        vec![8, 24],
        "long browser histories should keep the latest message cacheable and add one earlier breakpoint inside Anthropic's lookback window"
    );
}

#[tokio::test]
async fn open_stream_cache_breakpoints_account_for_browser_mode_system_message() {
    let (transport, seen) =
        RecordingTransport::new(vec![text_delta("ok"), finish(FinishReason::Stop)]);
    let sink: Arc<dyn EventSink> = Arc::new(RecordingSink::default());
    let mut turn_ctx = ctx();
    turn_ctx.browser_mode_instruction = Some("Browser mode: cloud".to_string());
    let d = ModelSamplingDriver::new(transport, sink, turn_ctx, 5).without_jitter();

    let input: Vec<Message> = (0..25)
        .map(|index| {
            Message::new(
                MessageRole::User,
                vec![ContentPart::text(format!("browser state {index}"))],
            )
        })
        .collect();
    let _ = d
        .run_sampling_request(input, CancellationToken::new())
        .await
        .expect("sampling should succeed");

    let captured = seen.lock().unwrap();
    let req = &captured[0];
    assert_eq!(req.messages[0].role, MessageRole::System);
    let cache_indices: Vec<usize> = req
        .messages
        .iter()
        .enumerate()
        .filter_map(|(index, message)| {
            (message.cache == Some(CacheHint::Ephemeral)).then_some(index)
        })
        .collect();

    assert_eq!(
        cache_indices,
        vec![9, 25],
        "cache hints should be computed after browser-mode system insertion, not shifted onto stale indices"
    );
}

#[tokio::test]
async fn turn_request_event_omits_full_llm_input_by_default() {
    let (transport, _opens) =
        ScriptedTransport::new(vec![OpenScript::Stream(vec![finish(FinishReason::Stop)])]);
    let sink = Arc::new(RecordingSink::default());
    let d = driver(transport, sink.clone(), 5);

    let _ = d
        .run_sampling_request(
            vec![Message::new(
                MessageRole::User,
                vec![
                    ContentPart::text("Find the account page."),
                    ContentPart::Media {
                        mime_type: "image/png".to_string(),
                        data: Some("iVBORw0KGgoAAAANSUhEUgAAAAEAAAAB".to_string()),
                        url: None,
                        detail: Some("low".to_string()),
                    },
                ],
            )],
            CancellationToken::new(),
        )
        .await
        .expect("sampling should succeed");

    let events = sink.drain();
    let request = events
        .iter()
        .find(|event| event.event_type == names::MODEL_TURN_REQUEST)
        .expect("turn request event emitted");
    let llm_input = &request.payload["llm_input"];
    assert_eq!(llm_input["message_count"], serde_json::json!(1));
    assert_eq!(llm_input["omitted_earlier_messages"], serde_json::json!(1));
    assert_eq!(llm_input["full_input_omitted"], serde_json::json!(true));
    assert_eq!(llm_input["truncated"], serde_json::json!(true));
    assert!(llm_input.get("messages").is_none());
    assert!(llm_input.get("system").is_none());
    assert!(llm_input.get("tools").is_none());
    let serialized = serde_json::to_string(llm_input).expect("llm_input serializes");
    assert!(!serialized.contains("iVBORw0KGgoAAAANSUhEUgAAAAEAAAAB"));
}

#[tokio::test]
async fn turn_request_event_carries_full_llm_input_messages() {
    let (transport, _opens) =
        ScriptedTransport::new(vec![OpenScript::Stream(vec![finish(FinishReason::Stop)])]);
    let sink = Arc::new(RecordingSink::default());
    let d = driver(transport, sink.clone(), 5).with_full_llm_input_events(true);

    let input = vec![
        Message::new(
            MessageRole::User,
            vec![
                ContentPart::text("Find the account page."),
                ContentPart::Media {
                    mime_type: "image/png".to_string(),
                    data: Some("iVBORw0KGgoAAAANSUhEUgAAAAEAAAAB".to_string()),
                    url: None,
                    detail: Some("low".to_string()),
                },
            ],
        ),
        Message::new(
            MessageRole::Assistant,
            vec![ContentPart::ToolCall {
                id: "call-1".to_string(),
                name: "browser_script".to_string(),
                input: serde_json::json!({
                    "code": "goto_url('https://example.com')",
                    "api_key": "secret-value",
                }),
                provider_metadata: None,
            }],
        ),
    ];

    let _ = d
        .run_sampling_request(input, CancellationToken::new())
        .await
        .expect("sampling should succeed");

    let events = sink.drain();
    let request = events
        .iter()
        .find(|event| event.event_type == names::MODEL_TURN_REQUEST)
        .expect("turn request event emitted");
    let llm_input = &request.payload["llm_input"];
    assert_eq!(llm_input["message_count"], serde_json::json!(2));
    assert_eq!(llm_input["omitted_earlier_messages"], serde_json::json!(0));
    assert_eq!(
        llm_input["messages"][0]["content"][0]["text"],
        serde_json::json!("Find the account page.")
    );
    assert_eq!(
        llm_input["messages"][0]["content"][1]["data"],
        serde_json::json!("iVBORw0KGgoAAAANSUhEUgAAAAEAAAAB")
    );
    assert_eq!(
        llm_input["messages"][1]["content"][0]["input"]["api_key"],
        serde_json::json!("[redacted]")
    );
    assert_eq!(llm_input["truncated"], serde_json::json!(false));
    assert!(!llm_input["system"][0]["text"]
        .as_str()
        .unwrap_or_default()
        .is_empty());
}

#[tokio::test]
async fn turn_request_event_carries_all_observability_messages_without_text_budget() {
    let (transport, _opens) =
        ScriptedTransport::new(vec![OpenScript::Stream(vec![finish(FinishReason::Stop)])]);
    let sink = Arc::new(RecordingSink::default());
    let d = driver(transport, sink.clone(), 5).with_full_llm_input_events(true);

    let long_text = "observe-this-text".repeat(6_000);
    let mut input: Vec<Message> = (0..85)
        .map(|index| {
            Message::new(
                MessageRole::User,
                vec![ContentPart::text(format!("msg-{index}"))],
            )
        })
        .collect();
    input.push(Message::new(
        MessageRole::User,
        vec![ContentPart::text(long_text.clone())],
    ));

    let _ = d
        .run_sampling_request(input, CancellationToken::new())
        .await
        .expect("sampling should succeed");

    let events = sink.drain();
    let request = events
        .iter()
        .find(|event| event.event_type == names::MODEL_TURN_REQUEST)
        .expect("turn request event emitted");
    let llm_input = &request.payload["llm_input"];
    let messages = llm_input["messages"].as_array().expect("messages array");
    assert_eq!(llm_input["message_count"], serde_json::json!(86));
    assert_eq!(llm_input["omitted_earlier_messages"], serde_json::json!(0));
    assert_eq!(messages.len(), 86);
    assert_eq!(
        messages[85]["content"][0]["text"],
        serde_json::json!(long_text)
    );
    assert_eq!(llm_input["truncated"], serde_json::json!(false));

    let serialized = serde_json::to_string(llm_input).expect("llm_input serializes");
    assert!(!serialized.contains("request observability text budget exhausted"));
    assert!(!serialized.contains("...[truncated]"));
}

#[tokio::test]
async fn driver_prepends_selected_browser_mode_instruction_to_messages() {
    let (transport, seen) =
        RecordingTransport::new(vec![text_delta("ok"), finish(FinishReason::Stop)]);
    let sink: Arc<dyn EventSink> = Arc::new(RecordingSink::default());
    let mut ctx = ctx();
    ctx.browser_mode_instruction = Some(crate::prompts::browser_mode_instruction("local"));
    let d = ModelSamplingDriver::new(transport, sink, ctx, 5).without_jitter();

    let input = user_input();
    let _ = d
        .run_sampling_request(input.clone(), CancellationToken::new())
        .await
        .expect("sampling should succeed");

    let captured = seen.lock().unwrap();
    let req = &captured[0];
    assert_eq!(req.messages.len(), input.len() + 1);
    assert_eq!(req.messages[0].role, MessageRole::System);
    assert!(
        matches!(
            req.messages[0].content.first(),
            Some(ContentPart::Text { text }) if text.contains("Use `browser connect local` before page work")
        ),
        "mode instruction message was not prepended: {:?}",
        req.messages[0]
    );
    let mut expected_input = input.clone();
    expected_input.last_mut().unwrap().cache = Some(CacheHint::Ephemeral);
    assert_eq!(&req.messages[1..], expected_input.as_slice());
}

// ---- (6) each turn installs its OWN request (the cell is per-call) ----------

#[tokio::test]
async fn each_open_sees_its_own_per_call_request() {
    // Two opens (retry path): both must capture the same populated per-call
    // request — proving the installed request is the driver's, not a stale fixed
    // one. (Mid-stream retry re-opens; we use an open-error retry for simplicity.)
    let events = vec![text_delta("done"), finish(FinishReason::Stop)];
    let (transport, seen) = RetryThenRecordTransport::new(events);
    let sink: Arc<dyn EventSink> = Arc::new(RecordingSink::default());
    let d = ModelSamplingDriver::new(transport, sink, ctx(), 5).without_jitter();

    let input = user_input();
    let _ = d
        .run_sampling_request(input.clone(), CancellationToken::new())
        .await
        .expect("driver should recover and succeed");

    let captured = seen.lock().unwrap();
    assert_eq!(captured.len(), 2, "one failed open + one successful open");
    for (i, req) in captured.iter().enumerate() {
        assert!(
            !req.messages.is_empty(),
            "open #{i} received an EMPTY request"
        );
        let mut expected_input = input.clone();
        expected_input.last_mut().unwrap().cache = Some(CacheHint::Ephemeral);
        assert_eq!(
            req.messages, expected_input,
            "open #{i} must carry the per-call input plus provider cache hint"
        );
    }
}

/// A transport that fails the FIRST open with a retryable error, then on the
/// second open records the request and streams the canned events. Records every
/// request it is handed (including the one for the failed open).
struct RetryThenRecordTransport {
    seen: Arc<Mutex<Vec<LlmRequest>>>,
    opens: AtomicUsize,
    events: Vec<Result<LlmEvent, LlmError>>,
}

impl RetryThenRecordTransport {
    fn new(events: Vec<Result<LlmEvent, LlmError>>) -> (Self, Arc<Mutex<Vec<LlmRequest>>>) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        (
            Self {
                seen: seen.clone(),
                opens: AtomicUsize::new(0),
                events,
            },
            seen,
        )
    }
}

impl SamplingTransport for RetryThenRecordTransport {
    fn open_stream<'a>(&'a self, req: &LlmRequest) -> Result<EventStream<'a>, LlmError> {
        // Record the request on every open, including the failing first one — the
        // driver must hand its real per-call request to each attempt.
        self.seen.lock().unwrap().push(req.clone());
        if self.opens.fetch_add(1, Ordering::SeqCst) == 0 {
            return Err(retryable_err("first open fails, retry"));
        }
        let stream: EventStream<'a> = Box::pin(stream::iter(self.events.clone()));
        Ok(stream)
    }
}

// ---- (7) the fused driver advertises the dispatcher's tool specs -----------

/// A trivial [`CallRunner`] for wiring a fused driver in a tool-defs test: it
/// never actually runs (these tests stream only text, so no tool call is
/// dispatched) — it exists solely so the driver can be built with a
/// [`ToolDispatcher`] that carries specs.
struct NoopRunner;

#[async_trait::async_trait]
impl crate::turn::dispatch::CallRunner for NoopRunner {
    fn parallel_safe(&self, _call: &ContentPart) -> bool {
        false
    }
    async fn run(
        &self,
        call: ContentPart,
        _cancel: tokio_util::sync::CancellationToken,
    ) -> Message {
        // Unreachable in these tests (no tool call is emitted), but satisfy the
        // trait with a benign tool-result so the type checks.
        let id = match &call {
            ContentPart::ToolCall { id, .. } => id.clone(),
            _ => String::new(),
        };
        Message::new(
            MessageRole::Tool,
            vec![ContentPart::ToolResult {
                tool_call_id: id,
                content: vec![ContentPart::text("noop")],
                is_error: false,
            }],
        )
    }
}

/// A no-op [`FusionRecorder`] (the fused path requires both a dispatcher AND a
/// recorder; these tests never dispatch, so nothing is ever recorded).
struct NoopRecorder;

#[async_trait::async_trait]
impl crate::turn::sampling::FusionRecorder for NoopRecorder {
    async fn record(&self, _messages: &[Message]) {}
}

/// A tool definition with an empty input schema; the request only needs the
/// `name` to be carried through to the wire, so the schema content is irrelevant
/// to what this test proves.
fn tool_def(name: &str) -> browser_use_llm::schema::ToolDefinition {
    browser_use_llm::schema::ToolDefinition {
        name: name.to_string(),
        description: format!("{name} model-visible tool description"),
        input_schema: serde_json::json!({"type": "object"}),
        output_schema: None,
        namespace: None,
        namespace_description: None,
    }
}

#[tokio::test]
async fn fused_driver_advertises_dispatcher_tool_specs_on_request() {
    use crate::turn::dispatch::ToolDispatcher;
    use crate::turn::sampling::FusionRecorder;

    // The dispatcher carries the SAME `Vec<ToolDefinition>` the registry would
    // advertise (here: the three product tools, order-stable). The fused driver
    // must copy these into `LlmRequest::tools` so the model receives the tool
    // catalog and can actually emit browser/python/shell tool calls — without
    // this the model gets no tools and fusion never fires on a real turn.
    let specs = vec![tool_def("browser"), tool_def("python"), tool_def("shell")];
    let dispatcher = Arc::new(ToolDispatcher::with_runner_and_specs(
        NoopRunner, /* model_supports */ true, specs,
    ));
    // Sanity: the accessor returns exactly what we stored, order-stable.
    assert_eq!(
        dispatcher
            .tool_specs()
            .iter()
            .map(|t| t.name.as_str())
            .collect::<Vec<_>>(),
        vec!["browser", "python", "shell"],
        "dispatcher must carry the registry's specs verbatim (order-stable)"
    );

    // Record the request the driver hands to the transport. The stream is
    // text-only (no tool call), so nothing is dispatched/recorded — we only care
    // about the request the driver built.
    let (transport, seen) =
        RecordingTransport::new(vec![text_delta("ok"), finish(FinishReason::Stop)]);
    let sink = Arc::new(RecordingSink::default());
    let sink_for_driver: Arc<dyn EventSink> = sink.clone();
    let recorder: Arc<dyn FusionRecorder> = Arc::new(NoopRecorder);
    let d = ModelSamplingDriver::new(transport, sink_for_driver, ctx(), 5)
        .without_jitter()
        .with_fusion(dispatcher, recorder)
        .with_full_llm_input_events(true);

    let _ = d
        .run_sampling_request(user_input(), CancellationToken::new())
        .await
        .expect("sampling should succeed");

    let captured = seen.lock().unwrap();
    assert_eq!(
        captured.len(),
        1,
        "exactly one open for one successful turn"
    );
    let req = &captured[0];
    // The whole point of this WP: the per-turn request the provider receives now
    // carries the registered tool definitions.
    assert!(
        !req.tools.is_empty(),
        "fused driver must advertise the dispatcher's tool specs — req.tools is EMPTY"
    );
    let tool_names: Vec<&str> = req.tools.iter().map(|t| t.name.as_str()).collect();
    assert_eq!(
        tool_names,
        vec!["browser", "python", "shell"],
        "req.tools must carry the registered tool names, in the registry's order"
    );

    let events = sink.drain();
    let request = events
        .iter()
        .find(|event| event.event_type == names::MODEL_TURN_REQUEST)
        .expect("turn request event emitted");
    let llm_tools = request.payload["llm_input"]["tools"]
        .as_array()
        .expect("llm_input tools array");
    assert_eq!(
        request.payload["llm_input"]["tools_count"],
        serde_json::json!(3)
    );
    assert_eq!(llm_tools[0]["name"], serde_json::json!("browser"));
    assert_eq!(
        llm_tools[0]["description"],
        serde_json::json!("browser model-visible tool description")
    );
    assert_eq!(
        llm_tools[0]["input_schema"],
        serde_json::json!({"type": "object"})
    );
}

#[tokio::test]
async fn fused_browser_script_dispatch_emits_runtime_tool_output_event() {
    use crate::turn::dispatch::ToolDispatcher;
    use crate::turn::sampling::FusionRecorder;

    let dispatcher = Arc::new(ToolDispatcher::with_runner_and_specs(
        NoopRunner,
        /* model_supports */ true,
        vec![tool_def("browser_script")],
    ));
    let (transport, _seen) = RecordingTransport::new(vec![
        tool_call("browser_script"),
        finish(FinishReason::Stop),
    ]);
    let sink = Arc::new(RecordingSink::default());
    let sink_for_driver: Arc<dyn EventSink> = sink.clone();
    let recorder: Arc<dyn FusionRecorder> = Arc::new(NoopRecorder);
    let driver = ModelSamplingDriver::new(transport, sink_for_driver, ctx(), 5)
        .without_jitter()
        .with_fusion(dispatcher, recorder);

    let outcome = driver
        .run_sampling_request(user_input(), CancellationToken::new())
        .await
        .expect("sampling should succeed");

    assert!(outcome.model_needs_follow_up);
    let events = sink.drain();
    let output = events
        .iter()
        .find(|event| event.event_type == names::TOOL_OUTPUT)
        .expect("browser_script dispatch must emit a runtime tool.output event");
    assert_eq!(output.payload["name"], "browser_script");
    assert_eq!(output.payload["tool_call_id"], "call-1");
    assert_eq!(output.payload["text"], "noop");
}

#[tokio::test]
async fn fused_done_result_becomes_final_message_without_follow_up() {
    use crate::turn::dispatch::ToolDispatcher;
    use crate::turn::sampling::FusionRecorder;

    let specs = vec![tool_def("done")];
    let dispatcher = Arc::new(ToolDispatcher::with_runner_and_specs(
        NoopRunner, /* model_supports */ true, specs,
    ));
    let (transport, _opens) = ScriptedTransport::new(vec![OpenScript::Stream(vec![
        tool_call_with_input(
            "done",
            serde_json::json!({
                "result": "full table answer",
                "text": "legacy summary"
            }),
        ),
        finish(FinishReason::ToolUse),
    ])]);
    let sink: Arc<dyn EventSink> = Arc::new(RecordingSink::default());
    let recorder: Arc<dyn FusionRecorder> = Arc::new(NoopRecorder);
    let d = ModelSamplingDriver::new(transport, sink, ctx(), 5)
        .without_jitter()
        .with_fusion(dispatcher, recorder);

    let out = d
        .run_sampling_request(user_input(), CancellationToken::new())
        .await
        .expect("sampling should succeed");

    assert!(
        !out.model_needs_follow_up,
        "done must terminate the fused turn instead of requesting another sample"
    );
    assert_eq!(
        out.last_agent_message.as_deref(),
        Some("full table answer"),
        "canonical done.result must be surfaced over the legacy text alias"
    );
    assert!(
        out.defers_mailbox_delivery_to_next_turn,
        "terminal done output is the final-answer boundary"
    );
}

#[tokio::test]
async fn text_only_driver_sends_no_tool_specs() {
    // The text-only driver (no dispatcher) must send NO tools — codex sends the
    // tool catalog only when the turn's toolset is non-empty, and this is the
    // counterpart that proves the fused path's behavior is dispatcher-gated.
    let (transport, seen) =
        RecordingTransport::new(vec![text_delta("ok"), finish(FinishReason::Stop)]);
    let sink: Arc<dyn EventSink> = Arc::new(RecordingSink::default());
    let d = ModelSamplingDriver::new(transport, sink, ctx(), 5).without_jitter();

    let _ = d
        .run_sampling_request(user_input(), CancellationToken::new())
        .await
        .expect("sampling should succeed");

    let captured = seen.lock().unwrap();
    assert_eq!(captured.len(), 1);
    assert!(
        captured[0].tools.is_empty(),
        "a driver with no dispatcher must not advertise any tools"
    );
}

#[tokio::test]
async fn mailbox_preemption_after_commentary_text_end_stops_stream_and_requests_follow_up() {
    let (transport, opens) = ScriptedTransport::new(vec![OpenScript::Stream(vec![
        text_delta("Working."),
        commentary_text_end(),
        tool_call("search"),
        finish(FinishReason::ToolUse),
    ])]);
    let sink = Arc::new(RecordingSink::default());
    let probe_calls = Arc::new(AtomicUsize::new(0));
    let d = driver(transport, sink.clone(), 5).with_mailbox_preemption_probe({
        let probe_calls = Arc::clone(&probe_calls);
        Arc::new(
            move || -> std::pin::Pin<
                Box<dyn std::future::Future<Output = bool> + Send + 'static>,
            > {
                let probe_calls = Arc::clone(&probe_calls);
                Box::pin(async move {
                    probe_calls.fetch_add(1, Ordering::SeqCst);
                    true
                })
            },
        )
    });

    let out = d
        .run_sampling_request(user_input(), CancellationToken::new())
        .await
        .expect("sampling should succeed");

    assert_eq!(opens.load(Ordering::SeqCst), 1);
    assert_eq!(probe_calls.load(Ordering::SeqCst), 1);
    assert!(
        out.model_needs_follow_up,
        "commentary mailbox preemption must force a follow-up sampling iteration"
    );
    assert_eq!(out.last_agent_message.as_deref(), Some("Working."));
    assert!(
        !out.defers_mailbox_delivery_to_next_turn,
        "commentary text remains current-turn deliverable in Codex"
    );

    let types: Vec<String> = sink.drain().iter().map(|e| e.event_type.clone()).collect();
    assert!(
        !types.iter().any(|t| t == names::TOOL_STARTED),
        "events after the preempted assistant commentary item must not be consumed"
    );
    assert!(
        !types.iter().any(|t| t == names::TOKEN_COUNT),
        "preempted streams stop before provider finish/token events"
    );
}

#[tokio::test]
async fn mailbox_preemption_after_reasoning_end_stops_stream_and_requests_follow_up() {
    let (transport, opens) = ScriptedTransport::new(vec![OpenScript::Stream(vec![
        text_delta("Working."),
        reasoning_end(),
        tool_call("search"),
        finish(FinishReason::ToolUse),
    ])]);
    let sink = Arc::new(RecordingSink::default());
    let probe_calls = Arc::new(AtomicUsize::new(0));
    let d = driver(transport, sink.clone(), 5).with_mailbox_preemption_probe({
        let probe_calls = Arc::clone(&probe_calls);
        Arc::new(
            move || -> std::pin::Pin<
                Box<dyn std::future::Future<Output = bool> + Send + 'static>,
            > {
                let probe_calls = Arc::clone(&probe_calls);
                Box::pin(async move {
                    probe_calls.fetch_add(1, Ordering::SeqCst);
                    true
                })
            },
        )
    });

    let out = d
        .run_sampling_request(user_input(), CancellationToken::new())
        .await
        .expect("sampling should succeed");

    assert_eq!(opens.load(Ordering::SeqCst), 1);
    assert_eq!(probe_calls.load(Ordering::SeqCst), 1);
    assert!(
        out.model_needs_follow_up,
        "mailbox preemption must force a follow-up sampling iteration"
    );
    assert_eq!(out.last_agent_message.as_deref(), Some("Working."));
    assert!(
        !out.defers_mailbox_delivery_to_next_turn,
        "reasoning preemption must not manufacture a final-answer boundary"
    );

    let types: Vec<String> = sink.drain().iter().map(|e| e.event_type.clone()).collect();
    assert!(
        !types.iter().any(|t| t == names::TOOL_STARTED),
        "events after the preempted assistant text item must not be consumed"
    );
    assert!(
        !types.iter().any(|t| t == names::TOKEN_COUNT),
        "preempted streams stop before provider finish/token events"
    );
}

#[tokio::test]
async fn mailbox_preemption_ignores_untagged_text_end() {
    let (transport, opens) = ScriptedTransport::new(vec![OpenScript::Stream(vec![
        text_delta("Final."),
        text_end(),
        finish(FinishReason::Stop),
    ])]);
    let sink = Arc::new(RecordingSink::default());
    let probe_calls = Arc::new(AtomicUsize::new(0));
    let d = driver(transport, sink.clone(), 5).with_mailbox_preemption_probe({
        let probe_calls = Arc::clone(&probe_calls);
        Arc::new(
            move || -> std::pin::Pin<
                Box<dyn std::future::Future<Output = bool> + Send + 'static>,
            > {
                let probe_calls = Arc::clone(&probe_calls);
                Box::pin(async move {
                    probe_calls.fetch_add(1, Ordering::SeqCst);
                    true
                })
            },
        )
    });

    let out = d
        .run_sampling_request(user_input(), CancellationToken::new())
        .await
        .expect("sampling should succeed");

    assert_eq!(opens.load(Ordering::SeqCst), 1);
    assert_eq!(
        probe_calls.load(Ordering::SeqCst),
        0,
        "untagged assistant text must not check mailbox preemption"
    );
    assert!(
        !out.model_needs_follow_up,
        "untagged assistant text follows Codex's safe default: final-answer text defers mailbox mail"
    );
    assert_eq!(out.last_agent_message.as_deref(), Some("Final."));
    assert!(
        out.defers_mailbox_delivery_to_next_turn,
        "untagged text is treated as final-answer output"
    );

    let types: Vec<String> = sink.drain().iter().map(|e| e.event_type.clone()).collect();
    assert!(
        types.iter().any(|t| t == names::TOKEN_COUNT),
        "the stream must continue through provider finish/token events"
    );
}
