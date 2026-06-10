//! The agentic tool loop: run the model, dispatch tool calls, feed results back.
//!
//! Given a model, a [`Route`], an initial [`LlmRequest`], a [`ToolSet`], and a
//! `stop_when` predicate, [`run_tool_loop`] drives the classic agent loop:
//!
//! 1. run one model turn over the current request;
//! 2. append the assistant message reconstructed from the turn's events
//!    (text / reasoning / tool-calls);
//! 3. for every [`LlmEvent::ToolCall`], look the tool up in the set, dispatch
//!    its handler with the decoded input, and encode the outcome as a
//!    [`ContentPart::ToolResult`] (`is_error: true` on [`ToolFailure`] or an
//!    unknown tool, so the model can self-correct);
//! 4. append a single `tool` message carrying all those results;
//! 5. repeat until the turn made no tool calls, or `stop_when` says to stop.
//!
//! ## Testability (no network)
//!
//! The model call is abstracted behind the tiny [`TurnSource`] trait
//! (`run_turn(&Route, &LlmRequest) -> Vec<LlmEvent>`). The real
//! [`ModelClient`](crate::route::ModelClient) implements it by driving its async
//! stream on a tokio runtime; a [`ScriptedTurnSource`] implements it by handing
//! back canned [`LlmEvent`] sequences. The whole loop — lookup, decode,
//! dispatch, result-encoding, termination — is therefore unit-tested against
//! scripted turns with zero sockets (see the tests in this module).

use crate::route::{ModelClient, Route};
use crate::schema::{
    ContentPart, FinishReason, LlmError, LlmEvent, LlmRequest, Message, MessageRole, Usage,
};
use crate::tool::{ToolFailure, ToolSet};

/// One model turn's worth of events, abstracted so the loop can be tested
/// without a network.
///
/// `run_turn` returns the full, ordered [`LlmEvent`] sequence for a single turn
/// (the same shape [`ModelClient::stream`] yields), or a hard [`LlmError`] that
/// aborts the loop. Implemented for real by [`ModelClient`] and for tests by
/// [`ScriptedTurnSource`].
pub trait TurnSource {
    /// Run exactly one model turn over `req`, returning its events in order.
    fn run_turn(&self, route: &Route, req: &LlmRequest) -> Result<Vec<LlmEvent>, LlmError>;
}

/// Drive the real async [`ModelClient`] as a synchronous turn source.
///
/// Each `run_turn` builds a current-thread tokio runtime, runs
/// [`ModelClient::stream`], and drains it into a `Vec<LlmEvent>`. Decode errors
/// surfaced inline as `Err` items abort the turn (and the loop).
impl TurnSource for ModelClient {
    fn run_turn(&self, route: &Route, req: &LlmRequest) -> Result<Vec<LlmEvent>, LlmError> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| LlmError::new(crate::schema::LlmErrorReason::Transport, e.to_string()))?;
        runtime.block_on(async {
            use futures_util::StreamExt;
            let mut stream = self.stream(route, req).await?;
            let mut events = Vec::new();
            while let Some(item) = stream.next().await {
                events.push(item?);
            }
            Ok(events)
        })
    }
}

/// What a single turn produced once its events were reduced.
#[derive(Debug, Clone, PartialEq)]
struct TurnOutcome {
    /// The assistant content parts (reasoning, text, tool calls) for the turn.
    assistant_content: Vec<ContentPart>,
    /// The tool calls the model requested this turn (id, name, decoded input).
    tool_calls: Vec<ToolCall>,
    /// Token usage reported for the turn (summed across step/finish events).
    usage: Usage,
    /// The terminal finish reason, if the turn reported one.
    finish_reason: Option<FinishReason>,
}

/// A single decoded tool call extracted from a turn's events.
#[derive(Debug, Clone, PartialEq)]
struct ToolCall {
    id: String,
    name: String,
    namespace: Option<String>,
    input: serde_json::Value,
}

/// Reduce one turn's ordered events into assistant content + tool calls + usage.
///
/// Mirrors the aggregation the non-streaming client does, but also pulls the
/// tool calls out separately so the loop can dispatch them. Text and reasoning
/// deltas are concatenated; `ToolCall` events become both an assistant
/// `ContentPart::ToolCall` (for the transcript) and a [`ToolCall`] to dispatch.
fn reduce_turn(events: Vec<LlmEvent>) -> TurnOutcome {
    let mut reasoning = String::new();
    let mut text = String::new();
    let mut tool_calls = Vec::new();
    let mut assistant_tool_parts: Vec<ContentPart> = Vec::new();
    let mut usage = Usage::default();
    let mut finish_reason = None;

    for ev in events {
        match ev {
            LlmEvent::TextDelta { delta, .. } => text.push_str(&delta),
            LlmEvent::ReasoningDelta { delta, .. } => reasoning.push_str(&delta),
            LlmEvent::ToolCall {
                id,
                name,
                namespace,
                provider_metadata,
                input,
            } => {
                assistant_tool_parts.push(ContentPart::ToolCall {
                    id: id.clone(),
                    name: name.clone(),
                    input: input.clone(),
                    provider_metadata: provider_metadata.clone().or_else(|| {
                        namespace
                            .clone()
                            .map(|namespace| serde_json::json!({ "namespace": namespace }))
                    }),
                });
                tool_calls.push(ToolCall {
                    id,
                    name,
                    namespace,
                    input,
                });
            }
            LlmEvent::Finish {
                usage: u,
                finish_reason: r,
            }
            | LlmEvent::StepFinish {
                usage: u,
                finish_reason: r,
            } => {
                if u != Usage::default() {
                    usage = u;
                }
                if r.is_some() {
                    finish_reason = r;
                }
            }
            _ => {}
        }
    }

    let mut assistant_content: Vec<ContentPart> = Vec::new();
    if !reasoning.is_empty() {
        assistant_content.push(ContentPart::Reasoning {
            text: reasoning,
            signature: None,
            provider_metadata: None,
        });
    }
    if !text.is_empty() {
        assistant_content.push(ContentPart::text(text));
    }
    assistant_content.extend(assistant_tool_parts);

    TurnOutcome {
        assistant_content,
        tool_calls,
        usage,
        finish_reason,
    }
}

/// Information about a finished turn, passed to the `stop_when` predicate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LoopStatus {
    /// Zero-based index of the turn that just completed.
    pub turn: usize,
    /// Whether that turn requested any tool calls.
    pub made_tool_calls: bool,
}

/// The accumulated result of an agent loop: the full transcript plus the final
/// assistant content.
#[derive(Debug, Clone, PartialEq)]
pub struct LoopOutput {
    /// Every message appended during the loop, in order: alternating assistant
    /// turns and the tool-result messages they triggered.
    pub transcript: Vec<Message>,
    /// The assistant content from the final turn (the model's answer).
    pub final_content: Vec<ContentPart>,
    /// Number of model turns run.
    pub turns: usize,
    /// Token usage summed across every turn.
    pub usage: Usage,
    /// The finish reason of the final turn, if any.
    pub finish_reason: Option<FinishReason>,
}

/// Encode a dispatched tool call into a [`ContentPart::ToolResult`].
///
/// On success the handler's content is wrapped with `is_error: false`; on a
/// [`ToolFailure`] (or an unknown tool) the message is wrapped with
/// `is_error: true` so the model reads it and can correct itself.
fn encode_tool_result(
    call_id: &str,
    outcome: Result<Vec<ContentPart>, ToolFailure>,
) -> ContentPart {
    match outcome {
        Ok(content) => ContentPart::ToolResult {
            tool_call_id: call_id.to_string(),
            content,
            is_error: false,
        },
        Err(failure) => ContentPart::ToolResult {
            tool_call_id: call_id.to_string(),
            content: failure.into_content(),
            is_error: true,
        },
    }
}

/// Sum two usages field-by-field (the breakdown is non-overlapping by design).
fn add_usage(acc: &mut Usage, turn: Usage) {
    acc.input_tokens += turn.input_tokens;
    acc.cached_input_tokens += turn.cached_input_tokens;
    acc.output_tokens += turn.output_tokens;
    acc.reasoning_output_tokens += turn.reasoning_output_tokens;
    acc.total_tokens += turn.total_tokens;
}

/// A guard against runaway loops if a (misconfigured) model keeps calling tools
/// forever. `stop_when` is the primary control; this is a final backstop.
const MAX_TURNS: usize = 64;

/// Run the agentic tool loop to completion.
///
/// Repeatedly runs a model turn via `source`, appends the assistant message and
/// any tool-result message to a growing request, and loops until a turn makes no
/// tool calls or `stop_when(&LoopStatus)` returns `true`. The initial `request`
/// is consumed and grown in place; the appended messages (assistant turns + tool
/// results) form the returned [`LoopOutput::transcript`].
///
/// `stop_when` is consulted after each turn: returning `true` ends the loop even
/// if the turn requested tools (their results are still recorded first).
pub fn run_tool_loop<S, F>(
    source: &S,
    route: &Route,
    mut request: LlmRequest,
    tools: &ToolSet,
    mut stop_when: F,
) -> Result<LoopOutput, LlmError>
where
    S: TurnSource,
    F: FnMut(&LoopStatus) -> bool,
{
    let mut transcript: Vec<Message> = Vec::new();
    let mut total_usage = Usage::default();
    let mut final_content: Vec<ContentPart> = Vec::new();
    let mut finish_reason = None;
    let mut turns = 0usize;

    // Ensure the model is told about the tools (define-once → render here). We
    // overwrite rather than append so repeated calls stay idempotent.
    request.tools = tools.definitions();

    for turn in 0..MAX_TURNS {
        let events = source.run_turn(route, &request)?;
        let outcome = reduce_turn(events);
        turns = turn + 1;
        add_usage(&mut total_usage, outcome.usage);
        finish_reason = outcome.finish_reason;
        final_content = outcome.assistant_content.clone();

        // Record the assistant turn (text/reasoning/tool-calls) in both the
        // running request and the returned transcript.
        let assistant_msg = Message::new(MessageRole::Assistant, outcome.assistant_content);
        request.messages.push(assistant_msg.clone());
        transcript.push(assistant_msg);

        let made_tool_calls = !outcome.tool_calls.is_empty();

        if made_tool_calls {
            // Dispatch each tool call and collect its result part.
            let mut results: Vec<ContentPart> = Vec::with_capacity(outcome.tool_calls.len());
            for call in &outcome.tool_calls {
                let dispatch_name = tool_display_name(call.namespace.as_deref(), &call.name);
                let dispatched = match tools.get(&dispatch_name) {
                    Some(tool) => tool.invoke(call.input.clone()).map(|r| r.content),
                    None => Err(ToolFailure::new(format!("unknown tool: {dispatch_name}"))),
                };
                results.push(encode_tool_result(&call.id, dispatched));
            }
            let tool_msg = Message::new(MessageRole::Tool, results);
            request.messages.push(tool_msg.clone());
            transcript.push(tool_msg);
        }

        let status = LoopStatus {
            turn,
            made_tool_calls,
        };
        if stop_when(&status) || !made_tool_calls {
            break;
        }
    }

    Ok(LoopOutput {
        transcript,
        final_content,
        turns,
        usage: total_usage,
        finish_reason,
    })
}

/// A canned [`TurnSource`] for tests: hands back pre-scripted turns in order.
///
/// Each call to [`run_turn`](TurnSource::run_turn) pops the next scripted turn
/// and records the request it was asked to run, so tests can assert both the
/// emitted events and that the loop fed tool results back into the next request.
/// Running out of scripted turns yields an empty turn (no tool calls → the loop
/// terminates), which keeps a misbehaving test from looping.
pub struct ScriptedTurnSource {
    turns: std::cell::RefCell<std::collections::VecDeque<Vec<LlmEvent>>>,
    /// The requests passed to each `run_turn`, in order (for assertions).
    pub seen_requests: std::cell::RefCell<Vec<LlmRequest>>,
}

impl ScriptedTurnSource {
    /// Build a source that replays the given turns in order.
    pub fn new(turns: impl IntoIterator<Item = Vec<LlmEvent>>) -> Self {
        Self {
            turns: std::cell::RefCell::new(turns.into_iter().collect()),
            seen_requests: std::cell::RefCell::new(Vec::new()),
        }
    }
}

impl TurnSource for ScriptedTurnSource {
    fn run_turn(&self, _route: &Route, req: &LlmRequest) -> Result<Vec<LlmEvent>, LlmError> {
        self.seen_requests.borrow_mut().push(req.clone());
        Ok(self.turns.borrow_mut().pop_front().unwrap_or_default())
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::ContentPart;
    use crate::tool::{Tool, ToolResult};
    use serde_json::json;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    fn test_route() -> Route {
        // The scripted source ignores the route entirely; any route shape works.
        use crate::protocols::OpenAiResponsesProtocol;
        use crate::route::{Auth, Endpoint};
        Route::new(
            Box::new(OpenAiResponsesProtocol::new()),
            Endpoint::new("http://example.invalid", "/v1/responses"),
            Auth::bearer("unused-in-scripted-source"),
        )
    }

    /// A turn where the model calls `add(a, b)`.
    fn add_call_turn(id: &str, a: i64, b: i64) -> Vec<LlmEvent> {
        vec![
            LlmEvent::StepStart,
            LlmEvent::ToolInputStart {
                id: id.into(),
                name: "add".into(),
            },
            LlmEvent::ToolInputEnd { id: id.into() },
            LlmEvent::ToolCall {
                id: id.into(),
                name: "add".into(),
                namespace: None,
                provider_metadata: None,
                input: json!({ "a": a, "b": b }),
            },
            LlmEvent::Finish {
                usage: Usage {
                    input_tokens: 5,
                    output_tokens: 3,
                    total_tokens: 8,
                    ..Usage::default()
                },
                finish_reason: Some(FinishReason::ToolUse),
            },
        ]
    }

    /// A final answer turn with plain text.
    fn answer_turn(text: &str) -> Vec<LlmEvent> {
        vec![
            LlmEvent::StepStart,
            LlmEvent::TextStart { id: "m".into() },
            LlmEvent::TextDelta {
                id: "m".into(),
                delta: text.into(),
            },
            LlmEvent::TextEnd {
                id: "m".into(),
                phase: None,
            },
            LlmEvent::Finish {
                usage: Usage {
                    input_tokens: 7,
                    output_tokens: 4,
                    total_tokens: 11,
                    ..Usage::default()
                },
                finish_reason: Some(FinishReason::Stop),
            },
        ]
    }

    fn add_tool_recording(seen: Arc<std::sync::Mutex<Vec<(i64, i64)>>>) -> Tool {
        Tool::new(
            "add",
            "Add two integers",
            json!({
                "type": "object",
                "properties": { "a": { "type": "integer" }, "b": { "type": "integer" } },
                "required": ["a", "b"]
            }),
            move |input: serde_json::Value| {
                let a = input
                    .get("a")
                    .and_then(serde_json::Value::as_i64)
                    .ok_or_else(|| ToolFailure::new("`a` must be an integer"))?;
                let b = input
                    .get("b")
                    .and_then(serde_json::Value::as_i64)
                    .ok_or_else(|| ToolFailure::new("`b` must be an integer"))?;
                seen.lock().unwrap().push((a, b));
                Ok(ToolResult::text((a + b).to_string()))
            },
        )
    }

    #[test]
    fn two_turn_exchange_dispatches_tool_and_terminates() {
        // Turn 1: model calls add(2, 3). Turn 2: model emits the final answer.
        let source =
            ScriptedTurnSource::new([add_call_turn("call_1", 2, 3), answer_turn("The sum is 5.")]);

        let invoked = Arc::new(std::sync::Mutex::new(Vec::<(i64, i64)>::new()));
        let tools = ToolSet::from_iter([add_tool_recording(invoked.clone())]);

        let req = LlmRequest::new("gpt-5.1-codex", "openai");
        let out = run_tool_loop(&source, &test_route(), req, &tools, |_| false).unwrap();

        // The handler was invoked once, with the decoded arguments.
        assert_eq!(*invoked.lock().unwrap(), vec![(2, 3)]);

        // Two model turns ran, and the loop terminated on the no-tool-call turn.
        assert_eq!(out.turns, 2);
        assert_eq!(out.finish_reason, Some(FinishReason::Stop));

        // Transcript: assistant(tool-call), tool(result), assistant(final text).
        assert_eq!(out.transcript.len(), 3);
        assert_eq!(out.transcript[0].role, MessageRole::Assistant);
        assert!(out.transcript[0]
            .content
            .iter()
            .any(|p| matches!(p, ContentPart::ToolCall { name, .. } if name == "add")));

        assert_eq!(out.transcript[1].role, MessageRole::Tool);
        match &out.transcript[1].content[0] {
            ContentPart::ToolResult {
                tool_call_id,
                content,
                is_error,
            } => {
                assert_eq!(tool_call_id, "call_1");
                assert!(!is_error);
                assert_eq!(content, &vec![ContentPart::text("5")]);
            }
            other => panic!("expected tool result, got {other:?}"),
        }

        // Final assistant content is the answer text.
        assert_eq!(out.final_content, vec![ContentPart::text("The sum is 5.")]);

        // Usage is summed across both turns: (5+3+8) + (7+4+11).
        assert_eq!(out.usage.input_tokens, 12);
        assert_eq!(out.usage.output_tokens, 7);
        assert_eq!(out.usage.total_tokens, 19);
    }

    #[test]
    fn tool_result_is_fed_into_the_next_request() {
        let source = ScriptedTurnSource::new([add_call_turn("c1", 4, 6), answer_turn("done")]);
        let invoked = Arc::new(std::sync::Mutex::new(Vec::new()));
        let tools = ToolSet::from_iter([add_tool_recording(invoked)]);

        let _ = run_tool_loop(
            &source,
            &test_route(),
            LlmRequest::new("m", "p"),
            &tools,
            |_| false,
        )
        .unwrap();

        // The second turn's request must contain the assistant tool-call message
        // and the tool-result message produced from turn 1.
        let seen = source.seen_requests.borrow();
        assert_eq!(seen.len(), 2);
        let second = &seen[1];
        assert!(second.messages.iter().any(|m| m.role == MessageRole::Tool
            && m.content.iter().any(|c| matches!(
                c,
                ContentPart::ToolResult { content, is_error: false, .. }
                    if content == &vec![ContentPart::text("10")]
            ))));
        // The tools were rendered onto the request (define-once → request).
        assert_eq!(second.tools.len(), 1);
        assert_eq!(second.tools[0].name, "add");
    }

    #[test]
    fn handler_failure_produces_error_tool_result() {
        // Model calls add with a non-integer arg → handler returns ToolFailure.
        let bad_turn = vec![
            LlmEvent::StepStart,
            LlmEvent::ToolCall {
                id: "bad_1".into(),
                name: "add".into(),
                namespace: None,
                provider_metadata: None,
                input: json!({ "a": "oops", "b": 3 }),
            },
            LlmEvent::Finish {
                usage: Usage::default(),
                finish_reason: Some(FinishReason::ToolUse),
            },
        ];
        let source = ScriptedTurnSource::new([bad_turn, answer_turn("sorry")]);
        let tools = ToolSet::from_iter([add_tool_recording(Arc::new(std::sync::Mutex::new(
            Vec::new(),
        )))]);

        let out = run_tool_loop(
            &source,
            &test_route(),
            LlmRequest::new("m", "p"),
            &tools,
            |_| false,
        )
        .unwrap();

        // The tool message carries an *error* result the model can self-correct on.
        let tool_msg = &out.transcript[1];
        assert_eq!(tool_msg.role, MessageRole::Tool);
        match &tool_msg.content[0] {
            ContentPart::ToolResult {
                tool_call_id,
                content,
                is_error,
            } => {
                assert_eq!(tool_call_id, "bad_1");
                assert!(is_error, "handler failure must mark the result as an error");
                assert_eq!(content, &vec![ContentPart::text("`a` must be an integer")]);
            }
            other => panic!("expected tool result, got {other:?}"),
        }
    }

    #[test]
    fn unknown_tool_yields_error_result_not_hard_failure() {
        let call_unknown = vec![
            LlmEvent::StepStart,
            LlmEvent::ToolCall {
                id: "u1".into(),
                name: "nonexistent".into(),
                namespace: None,
                provider_metadata: None,
                input: json!({}),
            },
            LlmEvent::Finish {
                usage: Usage::default(),
                finish_reason: Some(FinishReason::ToolUse),
            },
        ];
        let source = ScriptedTurnSource::new([call_unknown, answer_turn("ok")]);
        let tools = ToolSet::new();

        let out = run_tool_loop(
            &source,
            &test_route(),
            LlmRequest::new("m", "p"),
            &tools,
            |_| false,
        )
        .unwrap();

        match &out.transcript[1].content[0] {
            ContentPart::ToolResult {
                content, is_error, ..
            } => {
                assert!(is_error);
                assert_eq!(
                    content,
                    &vec![ContentPart::text("unknown tool: nonexistent")]
                );
            }
            other => panic!("expected error tool result, got {other:?}"),
        }
        // The loop still ran a second (answer) turn and finished cleanly.
        assert_eq!(out.turns, 2);
    }

    #[test]
    fn stop_when_halts_the_loop_after_a_tool_turn() {
        // Even though turn 1 calls a tool, stop_when ends the loop immediately.
        let source = ScriptedTurnSource::new([add_call_turn("c", 1, 1), answer_turn("unreached")]);
        let invoked = Arc::new(std::sync::Mutex::new(Vec::new()));
        let tools = ToolSet::from_iter([add_tool_recording(invoked.clone())]);

        let calls = AtomicUsize::new(0);
        let out = run_tool_loop(
            &source,
            &test_route(),
            LlmRequest::new("m", "p"),
            &tools,
            |status| {
                calls.fetch_add(1, Ordering::SeqCst);
                // Stop after the very first turn.
                status.turn == 0
            },
        )
        .unwrap();

        assert_eq!(out.turns, 1, "loop should stop after the first turn");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        // The tool still ran and its result was recorded before stopping.
        assert_eq!(*invoked.lock().unwrap(), vec![(1, 1)]);
        assert_eq!(out.transcript.len(), 2); // assistant tool-call + tool result
    }

    #[test]
    fn single_turn_with_no_tool_calls_terminates_immediately() {
        let source = ScriptedTurnSource::new([answer_turn("hi there")]);
        let tools = ToolSet::new();
        let out = run_tool_loop(
            &source,
            &test_route(),
            LlmRequest::new("m", "p"),
            &tools,
            |_| false,
        )
        .unwrap();
        assert_eq!(out.turns, 1);
        assert_eq!(out.final_content, vec![ContentPart::text("hi there")]);
        assert_eq!(out.transcript.len(), 1);
    }

    #[test]
    fn run_turn_error_aborts_the_loop() {
        struct FailingSource;
        impl TurnSource for FailingSource {
            fn run_turn(
                &self,
                _route: &Route,
                _req: &LlmRequest,
            ) -> Result<Vec<LlmEvent>, LlmError> {
                Err(LlmError::new(
                    crate::schema::LlmErrorReason::ProviderInternal,
                    "boom",
                ))
            }
        }
        let tools = ToolSet::new();
        let err = run_tool_loop(
            &FailingSource,
            &test_route(),
            LlmRequest::new("m", "p"),
            &tools,
            |_| false,
        )
        .unwrap_err();
        assert_eq!(err.reason, crate::schema::LlmErrorReason::ProviderInternal);
    }
}
