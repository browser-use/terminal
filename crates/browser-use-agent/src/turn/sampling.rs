//! Sampling driver implementation (stream + retry + transport fallback).
//!
//! The concrete [`ModelSamplingDriver`] is the default [`SamplingDriver`]
//! (`turn/mod.rs`) impl over `browser_use_llm::route::ModelClient`. Per turn it:
//!
//! 1. builds an [`LlmRequest`] from the input `Vec<Message>` + a `Route`,
//! 2. calls `ModelClient::stream(route, req)` to obtain an event stream,
//! 3. consumes that stream under a [`CancellationToken`] (via `tokio::select!`
//!    on `cancel.cancelled()` — on cancel it stops and returns
//!    [`AgentError::TurnAborted`]),
//! 4. maps each [`LlmEvent`] to UI events via [`events::map_llm_event`] and emits
//!    them through an injected [`EventSink`],
//! 5. accumulates the assistant message text, tool calls, and [`Usage`], and
//! 6. returns a [`decision::SamplingOutcome`] whose `model_needs_follow_up` is
//!    `true` iff the response carried >=1 tool call.
//!
//! The network send is wrapped in a retry loop that delegates *all* branching to
//! the merged pure core ([`decision::retry_decision`] + [`decision::backoff_ms`]).
//! On a retryable error it sleeps `backoff_ms` then retries up to
//! `stream_max_retries`; on `RetryAction::Fail` it returns the error.
//!
//! ## Codex parity (`codex-rs/core/src/session/turn.rs::run_sampling_request`)
//! - retry budget = `provider.stream_max_retries()` (here `AgentConfig::stream_max_retries`);
//! - `ContextWindowExceeded` / `UsageLimitReached` short-circuit to the matching
//!   [`AgentError`] variant *before* the retry branch (they are never retried);
//! - `model_needs_follow_up = !tool_calls.is_empty()`;
//! - `last_agent_message = (!full_text.is_empty()).then_some(full_text)`;
//! - cancellation mid-stream -> `TurnAborted`;
//! - codex gates retries on the *pre-increment* counter (`if retries < max_retries`),
//!   so `max_retries == N` allows the initial attempt + N retries (N+1 opens). We
//!   pass the pre-increment `attempt` to `decision::retry_decision` to match;
//! - codex computes its delay as `backoff(retries)` *after* `retries += 1`, so the
//!   first retry waits `backoff(1)`. The pure `backoff_ms` is 0-indexed, so we feed
//!   the post-increment `backoff_ms(attempt + 1)` to the decision as the requested
//!   delay — the realized sleep matches codex exactly while the pure budget gate
//!   stays untouched (see `handle_stream_error`).
//!
//! ## Transport switch
//! `RetryAction::SwitchTransport` (codex's WS fallback) is **not yet wired** — the
//! WS transport does not exist in `browser-use-llm` yet. We therefore report
//! `can_switch_transport = false` to the pure decision, which folds the
//! at-max-with-no-transport case straight into `RetryAction::Fail`. When the WS
//! transport lands, set `can_switch_transport` from the live client and handle the
//! returned `SwitchTransport` action here. (TODO: WP that wires WS fallback.)
//!
//! ## Jitter (I/O-layer only)
//! `decision::backoff_ms` is intentionally deterministic so the pure decision stays
//! testable. To match codex's real-world jitter we multiply the *already-decided*
//! delay by a random `0.9..=1.1` factor **here**, strictly behind the actual
//! `tokio::time::sleep`. This never feeds back into the pure decision, so parity of
//! the decision core is preserved. Tests disable jitter (it only affects sleep wall
//! time, never control flow or the returned outcome).

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Instant;

use browser_use_llm::route::{ModelClient, Route};
use browser_use_llm::schema::{
    CacheHint, ContentPart, FinishReason, LlmError, LlmErrorReason, LlmEvent, LlmRequest, Message,
    MessageRole, SystemPart, TextPhase, Usage,
};
use futures_util::{Stream, StreamExt};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::decision::{self, RetryAction, SamplingOutcome};
use crate::events::{self, names, EventSink, PendingEvent, TurnCtx};
use crate::tools::handlers::goal::GoalStore;
use crate::turn::dispatch::ToolDispatcher;
use crate::turn::{CallRunner, SamplingDriver};
use crate::AgentError;

/// A normalized model event stream: the exact shape [`ModelClient::stream`]
/// yields. Borrows the transport for the duration of one attempt.
pub type EventStream<'a> = Pin<Box<dyn Stream<Item = Result<LlmEvent, LlmError>> + Send + 'a>>;

pub type MailboxPreemptionProbe =
    Arc<dyn Fn() -> Pin<Box<dyn Future<Output = bool> + Send + 'static>> + Send + Sync>;

/// Abstraction over "open one model stream for this request".
///
/// This is the seam that keeps the driver testable without a network: the real
/// transport opens a [`ModelClient`] stream, while tests inject a
/// `ScriptedTransport` that yields canned [`LlmEvent`] sequences (and can fail N
/// times before succeeding, to exercise the retry path).
///
/// The transport yields a *stream* of `Result<LlmEvent, LlmError>` so that both
/// the "open failed" path (codex `stream(&prompt).await` returning `Err`) and the
/// "broke mid-flight" path (a stream item being `Err`) are representable and
/// exercised. Opening the stream is itself fallible.
///
/// `open_stream` takes the request by reference and the returned stream borrows
/// `self` (not `req`), so the caller may reuse `req` across retries.
pub trait SamplingTransport: Send + Sync {
    /// Open a fresh event stream for `req`.
    fn open_stream<'a>(&'a self, req: &LlmRequest) -> Result<EventStream<'a>, LlmError>;
}

/// The real transport: drives a [`ModelClient`] over a fixed [`Route`].
///
/// `ModelClient::stream` is an async fn that returns `Result<Stream, LlmError>`
/// (setup/transport failures surface as the outer `Err`; decode errors surface as
/// `Err` items inside the stream).
///
/// ## Per-call request (the fix)
/// The request lives behind a [`std::sync::Mutex`] so each [`open_stream`] call
/// can install the *driver's real per-turn request* (populated input + tools)
/// before opening the stream. Previously this transport held a fixed request
/// built once with empty messages and ignored the per-call `req`, so production
/// turns sent an EMPTY body to the provider (HTTP 400: "One of
/// input/previous_response_id/prompt/conversation_id must be provided"). The cell
/// is seeded by [`new`](ModelClientTransport::new) (so the very first open is well
/// defined even before the driver writes) and overwritten on every open.
///
/// A `Mutex` (not `RefCell`) is required: the driver is `Send + Sync`, and the
/// transport is shared across the multi-thread runtime.
///
/// [`open_stream`]: SamplingTransport::open_stream
pub struct ModelClientTransport {
    client: Arc<ModelClient>,
    route: Route,
    /// The request the next stream open will use. Interior-mutable so each
    /// `open_stream(&req)` can install the driver's real per-turn request before
    /// the blocking open clones it out (see [`open_blocking`]).
    ///
    /// [`open_blocking`]: ModelClientTransport::open_blocking
    req: std::sync::Mutex<LlmRequest>,
}

impl ModelClientTransport {
    pub fn new(client: Arc<ModelClient>, route: Route, req: LlmRequest) -> Self {
        // Seed the cell with the initial request so the first open is well-defined
        // even if the driver has not written yet; every `open_stream` overwrites it.
        Self {
            client,
            route,
            req: std::sync::Mutex::new(req),
        }
    }

    /// Open a stream by awaiting `ModelClient::stream` on the current runtime.
    ///
    /// `ModelClient::stream` is `async`, but [`SamplingTransport::open_stream`] is
    /// sync (so the scripted test transport stays trivial). We bridge with
    /// `block_in_place` + the current handle. This is the only place that blocks,
    /// and it runs on the multi-thread runtime the agent uses.
    ///
    /// ## Clone soundness
    /// We snapshot the cell's request into a *local* `LlmRequest` and stream the
    /// local clone. This is sound because `ModelClient::stream` borrows
    /// `&LlmRequest` only to build the wire body (`prepare` → `Protocol::build_body`)
    /// and POST it (`send_with_retry`) *upfront*, before it returns. The stream it
    /// hands back is a `Box::pin`'d `Stream` whose state owns the reqwest byte
    /// stream + decoders — it does NOT borrow `req` (it is `Send + 'static`). So
    /// dropping the local clone once `block_on` returns is safe. Cloning also keeps
    /// the `Mutex` guard from being held across the blocking `block_on`.
    fn open_blocking(&self) -> Result<EventStream<'_>, LlmError> {
        let req = self.req.lock().unwrap().clone();
        let handle = tokio::runtime::Handle::current();
        tokio::task::block_in_place(|| handle.block_on(self.client.stream(&self.route, &req)))
    }
}

impl SamplingTransport for ModelClientTransport {
    fn open_stream<'a>(&'a self, req: &LlmRequest) -> Result<EventStream<'a>, LlmError> {
        // Install the driver's real per-turn request (populated input + tools)
        // before opening, so the provider receives a non-empty body. This is the
        // fix for the empty-request bug: the passed `req` is now used instead of a
        // fixed empty one held at construction.
        *self.req.lock().unwrap() = req.clone();
        self.open_blocking()
    }
}

/// Does this [`LlmError`] denote a context-window-exceeded condition?
///
/// The schema has no dedicated variant, so we key off the codex-style signal: a
/// non-retryable `InvalidRequest` whose message mentions the context window.
fn is_context_window_exceeded(e: &LlmError) -> bool {
    e.reason == LlmErrorReason::InvalidRequest && {
        let m = e.message.to_ascii_lowercase();
        m.contains("context") && m.contains("window")
    }
}

/// Does this [`LlmError`] denote a hard usage/quota limit (never retried)?
fn is_usage_limit_reached(e: &LlmError) -> bool {
    e.reason == LlmErrorReason::QuotaExceeded
}

/// Map an [`LlmError`] to the agent's [`AgentError`].
///
/// Context-window / usage-limit conditions get dedicated variants (codex
/// short-circuits on these); everything else becomes `Provider(_)`.
fn llm_error_to_agent(e: &LlmError) -> AgentError {
    if is_context_window_exceeded(e) {
        AgentError::ContextWindowExceeded
    } else if is_usage_limit_reached(e) {
        AgentError::UsageLimitReached
    } else {
        AgentError::Provider(e.to_string())
    }
}

fn provider_error_event_to_agent(message: String) -> AgentError {
    let lower = message.to_ascii_lowercase();
    if lower.contains("context") && lower.contains("window") {
        AgentError::ContextWindowExceeded
    } else {
        AgentError::Provider(message)
    }
}

/// Server-requested retry delay, if any.
///
/// The structured `Retry-After` hint is carried by `browser-use-llm`'s retry
/// machinery (`Outcome::Fail { retry_after_ms }`), not on the public [`LlmError`]
/// (which deliberately omits it so it never leaks via `Display`). At this layer we
/// therefore have no hint and fall back to the deterministic `backoff_ms`. Kept as
/// a seam for when the executor threads `Retry-After` through to this layer.
fn requested_delay_ms(_e: &LlmError) -> Option<u64> {
    None
}

/// Outcome of one decision-driven retry step.
enum RetryStep {
    /// The error is terminal — return it.
    Fail(AgentError),
    /// Slept for the decided delay; the loop should retry from the top.
    Retry,
}

/// Decide + act on an [`LlmError`] using the PURE decision core.
///
/// `attempt` is the count of retries already performed (0 on the first failure),
/// i.e. exactly codex's `retries` counter *before* it is incremented for this
/// failure. The at-max budget gate therefore uses `*attempt` directly, matching
/// codex's `if retries < max_retries` (pre-increment) — so `max_retries == N`
/// permits the initial attempt plus N retries (N+1 opens total).
///
/// On `RetryAction::Backoff`, this sleeps for the decided delay (with optional
/// I/O-layer jitter), bumps `*attempt`, and returns [`RetryStep::Retry`].
/// `SwitchTransport` is not wired (see module docs) — it is treated as `Fail`.
///
/// ## Backoff index (A1 parity caveat)
/// Codex computes its delay as `backoff(retries)` *after* `retries += 1`, so the
/// first retry waits `backoff(1)`, the second `backoff(2)`, …. The pure
/// `decision::retry_decision`, however, derives its default delay from the same
/// (pre-increment) `retries` it gates on, which would yield `backoff(0)` for the
/// first retry. To reproduce codex's post-increment delay *without* perturbing
/// the pure budget gate, we pass the post-increment `backoff_ms(*attempt + 1)`
/// explicitly as the "server-requested" delay. The decision stays pure and the
/// realized sleep matches codex exactly.
async fn handle_stream_error(
    err: LlmError,
    attempt: &mut u32,
    max_retries: u32,
    jitter: bool,
) -> RetryStep {
    // Context/usage limits never retry — short-circuit to the dedicated variant.
    if is_context_window_exceeded(&err) {
        return RetryStep::Fail(AgentError::ContextWindowExceeded);
    }
    if is_usage_limit_reached(&err) {
        return RetryStep::Fail(AgentError::UsageLimitReached);
    }

    // Prefer a real server `Retry-After` hint; otherwise use codex's
    // post-increment backoff index (see the backoff-index note above).
    let next_attempt = *attempt + 1;
    let requested = requested_delay_ms(&err).or(Some(decision::backoff_ms(next_attempt)));

    // Gate on the PRE-increment count, exactly like codex's `retries < max`.
    let action = decision::retry_decision(
        *attempt,
        max_retries,
        err.retryable,
        // Transport switch (WS fallback) is not wired yet; never offer it.
        false,
        requested,
    );

    match action {
        RetryAction::Fail => RetryStep::Fail(llm_error_to_agent(&err)),
        // Unreachable while can_switch_transport is hard-coded false, but kept
        // explicit so the WS-fallback wiring has an obvious home. Treat as Fail.
        RetryAction::SwitchTransport => RetryStep::Fail(llm_error_to_agent(&err)),
        RetryAction::Backoff { delay_ms } => {
            *attempt = next_attempt;
            let delay = apply_jitter(delay_ms, jitter);
            tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
            RetryStep::Retry
        }
    }
}

/// Apply I/O-layer jitter to an already-decided backoff (see module docs).
///
/// Deterministic decision in, jittered wall-clock delay out. Jitter is a
/// `0.9..=1.1` multiplier; with `jitter == false` (tests) the delay is returned
/// verbatim. Uses a cheap nanosecond-seeded factor — no external `rand` dep — and
/// it only ever perturbs sleep duration, never control flow.
fn apply_jitter(delay_ms: u64, jitter: bool) -> u64 {
    if !jitter || delay_ms == 0 {
        return delay_ms;
    }
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    // Map nanos -> [0.9, 1.1].
    let frac = (nanos % 1000) as f64 / 1000.0; // [0.0, 1.0)
    let factor = 0.9 + 0.2 * frac;
    (delay_ms as f64 * factor) as u64
}

/// Default [`SamplingDriver`]: stream + retry over the pure decision core, with
/// optional **fused tool dispatch** (WP-I-fusion).
///
/// ## Fusion (codex `try_run_turn` / `try_run_sampling_request` parity)
/// Codex fuses the model stream and tool dispatch inside one sampling step. This
/// driver does the same when a [`ToolDispatcher`] + [`FusionRecorder`] are
/// attached via [`ModelSamplingDriver::with_fusion`]: after the stream finishes,
/// it records the assistant message, dispatches the tool calls the model emitted
/// (in model order, through the [`ToolDispatcher`]'s parallel/serial gate),
/// records each tool output back into the shared conversation, and reports
/// `model_needs_follow_up = true` iff at least one tool ran. The
/// [`TurnLoop`](crate::turn::loop_driver::TurnLoop) is unchanged: it re-samples
/// while follow-up is requested, and on the next iteration its
/// `clone_history_for_prompt` sees the recorded tool outputs.
///
/// Without fusion configured ([`ModelSamplingDriver::new`]), the driver is the
/// text-only WP-B5 sampler: it reports follow-up iff the model emitted ≥1 tool
/// call but neither dispatches nor records (the loop's history is owned
/// elsewhere). Both shapes share the exact same stream/retry/cancel core.
pub struct ModelSamplingDriver<
    T: SamplingTransport,
    R: CallRunner = crate::turn::OrchestratorRunner,
> {
    transport: T,
    sink: Arc<dyn EventSink>,
    ctx: TurnCtx,
    next_turn_idx: AtomicUsize,
    /// Retry budget (codex `provider.stream_max_retries()`).
    max_retries: u32,
    /// Whether to apply I/O-layer jitter to the post-decision backoff sleep.
    jitter: bool,
    /// Fused dispatcher: runs the tool calls the model emitted (model order +
    /// parallel/serial gate). `None` = text-only sampler (WP-B5 behavior).
    dispatcher: Option<Arc<ToolDispatcher<R>>>,
    /// Where dispatched outputs (and the assistant message) are recorded so the
    /// next sampling iteration sees them. `None` unless fusion is configured.
    recorder: Option<Arc<dyn FusionRecorder>>,
    /// Shared event-sourced goal state. When present, prompt steering and usage
    /// accounting are folded through the same store as the model-facing goal
    /// tools.
    goal_store: Option<Arc<GoalStore>>,
    /// Codex-style mid-stream mailbox preemption. After an assistant text item or
    /// reasoning item is done, the driver checks this probe; pending mailbox mail
    /// stops the current response and reports follow-up so the turn loop drains
    /// that mail on the next iteration.
    mailbox_preemption_probe: Option<MailboxPreemptionProbe>,
    /// Include the full provider input in `model.turn.request` events. Disabled
    /// for normal CLI/TUI persistence because it can duplicate large screenshots
    /// and prompt text into the local event log on every turn.
    full_llm_input_events: bool,
}

impl<T: SamplingTransport> ModelSamplingDriver<T> {
    /// Build a text-only driver (no tool dispatch). `ctx` carries session/model
    /// identity for emitted events. The transport owns the [`Route`] and the
    /// *current* [`LlmRequest`] cell (see [`ModelClientTransport`]); the driver
    /// builds the real per-turn request (model/provider + input messages) and
    /// installs it via [`SamplingTransport::open_stream`] on each open.
    ///
    /// Tool calls the model emits set `model_needs_follow_up` but are NOT
    /// executed; attach a dispatcher with [`ModelSamplingDriver::with_fusion`]
    /// for the fused path.
    pub fn new(transport: T, sink: Arc<dyn EventSink>, ctx: TurnCtx, max_retries: u32) -> Self {
        Self {
            transport,
            sink,
            next_turn_idx: AtomicUsize::new(ctx.turn_idx),
            ctx,
            max_retries,
            jitter: true,
            dispatcher: None,
            recorder: None,
            goal_store: None,
            mailbox_preemption_probe: None,
            full_llm_input_events: false,
        }
    }

    /// Attach the fused dispatch path: tool calls the model emits are dispatched
    /// through `dispatcher` and their outputs (plus the assistant message) are
    /// recorded into `recorder` so the loop re-samples with them in context
    /// (codex `try_run_turn`).
    ///
    /// This **rebinds the runner type parameter** from the default
    /// `OrchestratorRunner` (which [`new`](ModelSamplingDriver::new) produces) to
    /// the dispatcher's concrete `R2`, consuming the text-only driver and
    /// returning a fused `ModelSamplingDriver<T, R2>`. Chainable with
    /// [`without_jitter`](ModelSamplingDriver::without_jitter).
    pub fn with_fusion<R2: CallRunner + 'static>(
        self,
        dispatcher: Arc<ToolDispatcher<R2>>,
        recorder: Arc<dyn FusionRecorder>,
    ) -> ModelSamplingDriver<T, R2> {
        ModelSamplingDriver {
            transport: self.transport,
            sink: self.sink,
            next_turn_idx: self.next_turn_idx,
            ctx: self.ctx,
            max_retries: self.max_retries,
            jitter: self.jitter,
            dispatcher: Some(dispatcher),
            recorder: Some(recorder),
            goal_store: self.goal_store,
            mailbox_preemption_probe: self.mailbox_preemption_probe,
            full_llm_input_events: self.full_llm_input_events,
        }
    }
}

impl<T: SamplingTransport, R: CallRunner + 'static> ModelSamplingDriver<T, R> {
    /// Disable I/O-layer jitter (deterministic sleeps). Used by tests.
    pub fn without_jitter(mut self) -> Self {
        self.jitter = false;
        self
    }

    pub fn with_goal_store(mut self, goal_store: Arc<GoalStore>) -> Self {
        self.goal_store = Some(goal_store);
        self
    }

    pub fn with_mailbox_preemption_probe(mut self, probe: MailboxPreemptionProbe) -> Self {
        self.mailbox_preemption_probe = Some(probe);
        self
    }

    pub fn with_full_llm_input_events(mut self, enabled: bool) -> Self {
        self.full_llm_input_events = enabled;
        self
    }

    async fn has_mailbox_preemption(&self) -> bool {
        match &self.mailbox_preemption_probe {
            Some(probe) => probe().await,
            None => false,
        }
    }

    /// Map an [`LlmEvent`] to UI events and emit them through the sink.
    fn ctx_for_turn(&self, turn_idx: usize) -> TurnCtx {
        let mut ctx = self.ctx.clone();
        ctx.turn_idx = turn_idx;
        ctx
    }

    fn emit_event(&self, ev: &LlmEvent, turn_idx: usize) {
        let ctx = self.ctx_for_turn(turn_idx);
        for pending in events::map_llm_event(&ctx, ev) {
            self.sink.emit(pending);
        }
    }

    fn emit_turn_request(
        &self,
        turn_idx: usize,
        attempt: u32,
        composition: &Value,
        llm_input: &Value,
    ) {
        self.sink.emit(PendingEvent::new(
            self.ctx.session_id.clone(),
            names::MODEL_TURN_REQUEST,
            serde_json::json!({
                "model": &self.ctx.model,
                "provider": &self.ctx.provider,
                "turn_idx": turn_idx,
                "attempt": attempt,
                "composition": composition,
                "llm_input": llm_input,
            }),
        ));
    }

    fn emit_tool_result(&self, call: &ContentPart, output: &Message) {
        let (tool_call_id, name) = tool_call_identity(call);
        let (text, is_error) = tool_result_text_and_status(output);
        let mut payload = serde_json::json!({
            "name": name,
            "tool_call_id": tool_call_id,
            "ok": !is_error,
            "text": text,
        });
        if !is_error {
            if let Some(content) = tool_result_event_content(output) {
                if let Value::Object(obj) = &mut payload {
                    obj.insert("content".to_string(), Value::Array(content));
                }
            }
        }
        if is_error {
            if text.starts_with("aborted by user") {
                self.sink.emit(PendingEvent::new(
                    self.ctx.session_id.clone(),
                    names::TOOL_ABORTED,
                    serde_json::json!({
                        "name": name,
                        "tool_call_id": tool_call_id,
                        "error": text,
                    }),
                ));
            }
            self.sink.emit(PendingEvent::new(
                self.ctx.session_id.clone(),
                names::TOOL_FAILED,
                serde_json::json!({
                    "name": name,
                    "tool_call_id": tool_call_id,
                    "error": text,
                }),
            ));
        } else {
            self.sink.emit(PendingEvent::new(
                self.ctx.session_id.clone(),
                names::TOOL_OUTPUT,
                payload,
            ));
        }
    }

    fn emit_goal_accounting(&self, usage: &Usage, time_used_seconds: i64) {
        let Some(goal_store) = self.goal_store.as_ref() else {
            return;
        };
        let _ = goal_store.account_usage(usage, time_used_seconds);
    }

    fn emit_goal_elapsed_accounting(&self, started_at: Instant) {
        let Some(goal_store) = self.goal_store.as_ref() else {
            return;
        };
        let _ = goal_store.account_elapsed_seconds(started_at.elapsed().as_secs() as i64);
    }

    fn input_with_goal_context(&self, input: Vec<Message>) -> Vec<Message> {
        let Some(goal_text) = self
            .goal_store
            .as_ref()
            .and_then(|store| store.goal_context_text())
        else {
            return input;
        };
        let mut messages = Vec::with_capacity(input.len() + 1);
        messages.push(Message::new(
            MessageRole::User,
            vec![ContentPart::text(goal_text)],
        ));
        messages.extend(input);
        messages
    }

    /// Fold a single successfully-decoded event into the accumulator + sink.
    fn consume_event(
        &self,
        acc: &mut TurnAccumulator,
        ev: LlmEvent,
        started_at: Instant,
        turn_idx: usize,
    ) -> Result<StreamProgress, AgentError> {
        // Emit UI events first (map is pure; emit is the only side effect).
        self.emit_event(&ev, turn_idx);
        match ev {
            LlmEvent::TextDelta { id, delta } => {
                let has_content = !delta.trim().is_empty();
                acc.full_text.push_str(&delta);
                if has_content {
                    acc.text_items_with_content.insert(id, true);
                }
                Ok(StreamProgress::Continue)
            }
            LlmEvent::TextEnd { id, phase } => {
                let text_item_has_content = acc.text_items_with_content.remove(&id).unwrap_or(
                    // Older/non-Responses decoders may not preserve per-item
                    // phase. If text exists and the close is untagged, treat it
                    // as final-answer output rather than commentary.
                    !acc.full_text.trim().is_empty(),
                );
                if text_item_has_content && !matches!(phase, Some(TextPhase::Commentary)) {
                    acc.defers_mailbox_delivery_to_next_turn = true;
                }
                Ok(StreamProgress::Continue)
            }
            LlmEvent::ToolCall {
                id,
                name,
                namespace,
                provider_metadata,
                input,
            } => {
                // Capture the actual call (model order) so the fused dispatch can
                // run it; the count is derived from this vec's length.
                acc.tool_calls.push(ContentPart::ToolCall {
                    id,
                    name,
                    input,
                    provider_metadata: tool_call_provider_metadata(namespace, provider_metadata),
                });
                Ok(StreamProgress::Continue)
            }
            LlmEvent::Finish {
                usage,
                finish_reason,
            } => {
                self.emit_goal_accounting(&usage, started_at.elapsed().as_secs() as i64);
                acc.usage = Some(usage);
                acc.finish_reason = finish_reason;
                Ok(StreamProgress::Done)
            }
            LlmEvent::ProviderError { message, .. } => Err(provider_error_event_to_agent(message)),
            // Reasoning, lifecycle markers, and step finishes:
            // no accumulation; their UI mapping (if any) already happened above.
            _ => Ok(StreamProgress::Continue),
        }
    }
}

fn tool_call_provider_metadata(
    namespace: Option<String>,
    provider_metadata: Option<serde_json::Value>,
) -> Option<serde_json::Value> {
    match (namespace, provider_metadata) {
        (Some(namespace), Some(serde_json::Value::Object(mut meta))) => {
            meta.insert("namespace".to_string(), serde_json::json!(namespace));
            Some(serde_json::Value::Object(meta))
        }
        (Some(namespace), Some(meta)) => {
            Some(serde_json::json!({ "namespace": namespace, "provider": meta }))
        }
        (Some(namespace), None) => Some(serde_json::json!({ "namespace": namespace })),
        (None, metadata) => metadata,
    }
}

fn checks_mailbox_preemption_after_event(ev: &LlmEvent) -> bool {
    matches!(
        ev,
        LlmEvent::ReasoningEnd { .. }
            | LlmEvent::TextEnd {
                phase: Some(TextPhase::Commentary),
                ..
            }
    )
}

/// Accumulated state for one assistant turn, threaded through the stream loop.
#[derive(Default)]
struct TurnAccumulator {
    full_text: String,
    text_items_with_content: HashMap<String, bool>,
    defers_mailbox_delivery_to_next_turn: bool,
    /// The tool calls the model emitted, in model order. The length doubles as
    /// the "did the model request follow-up" signal (codex
    /// `!tool_calls.is_empty()`); the calls themselves feed the fused dispatch.
    tool_calls: Vec<ContentPart>,
    #[allow(dead_code)]
    usage: Option<Usage>,
    finish_reason: Option<FinishReason>,
}

/// What consuming one streamed event told the loop to do.
enum StreamProgress {
    /// Keep pulling events.
    Continue,
    /// The stream ended (`Finish`) — assemble the outcome.
    Done,
}

/// Records the assistant message + dispatched tool outputs back into the shared
/// conversation state (codex `try_run_turn` records the function-call and its
/// output into history before re-sampling).
///
/// This is the seam that closes the fusion loop without changing the frozen
/// [`SamplingDriver`] signature (which receives no [`TurnState`]): the fused
/// [`ModelSamplingDriver`] holds an `Arc<dyn FusionRecorder>` pointing at the
/// SAME conversation buffer the [`TurnLoop`] reads via
/// [`TurnState::clone_history_for_prompt`], so the next iteration re-samples
/// with the tool outputs in context. Production backs it with the
/// `ContextManager`/`Session`-backed `TurnState`; tests back it with an
/// in-memory `Vec` recorder.
///
/// [`SamplingDriver`]: crate::turn::SamplingDriver
/// [`TurnState`]: crate::turn::TurnState
/// [`TurnState::clone_history_for_prompt`]: crate::turn::TurnState::clone_history_for_prompt
/// [`TurnLoop`]: crate::turn::loop_driver::TurnLoop
#[async_trait::async_trait]
pub trait FusionRecorder: Send + Sync {
    /// Append `messages` (assistant turn, then tool-result turn) to the shared
    /// conversation, in order.
    async fn record(&self, messages: &[Message]);
}

/// Extract the model's tool calls from accumulated assistant content, in the
/// order the model emitted them (codex preserves model order through dispatch).
fn extract_tool_calls(parts: &[ContentPart]) -> Vec<ContentPart> {
    parts
        .iter()
        .filter(|p| matches!(p, ContentPart::ToolCall { .. }))
        .cloned()
        .collect()
}

/// The completion tool's model-visible name. A model call to this tool declares
/// the task finished; the fused driver treats it as TERMINAL (no follow-up), so
/// the turn loop stops instead of re-sampling. Mirrors the `done` handler's
/// [`DONE_TOOL_NAME`](crate::tools::handlers::done::DONE_TOOL_NAME).
const DONE_TOOL_NAME: &str = "done";

/// Whether any of `tool_calls` is a call to the completion (`done`) tool.
///
/// When the model calls `done`, it has declared the turn complete: the fused
/// driver dispatches the call (so the `done` summary is recorded into history),
/// then reports `model_needs_follow_up = false` so the loop terminates instead
/// of re-sampling. This is the engine-side terminal signal the `done` handler's
/// module doc flagged as deferred ("wiring the loop to treat a successful `done`
/// output as terminal needs the loop's classifier") — wired here.
fn calls_done_tool(tool_calls: &[ContentPart]) -> bool {
    tool_calls
        .iter()
        .any(|p| matches!(p, ContentPart::ToolCall { name, .. } if name == DONE_TOOL_NAME))
}

fn done_tool_succeeded(tool_calls: &[ContentPart], outputs: &[Message]) -> bool {
    tool_calls
        .iter()
        .zip(outputs.iter())
        .any(|(call, output)| match call {
            ContentPart::ToolCall { name, .. } if name == DONE_TOOL_NAME => {
                let (_, is_error) = tool_result_text_and_status(output);
                !is_error
            }
            _ => false,
        })
}

/// The final summary carried by the model's `done` call, if any.
///
/// Reads the `result` field from the first `done` tool call's JSON arguments,
/// falling back to the legacy `text` alias and then to a compact `result_file`
/// pointer. Returns `None` when there is no `done` call or it carried no
/// non-empty completion payload, so the caller only overrides the turn result
/// when there is a real message to surface.
fn done_summary(tool_calls: &[ContentPart]) -> Option<String> {
    tool_calls.iter().find_map(|p| match p {
        ContentPart::ToolCall { name, input, .. } if name == DONE_TOOL_NAME => {
            for field in ["result", "text"] {
                if let Some(value) = input
                    .get(field)
                    .and_then(|value| value.as_str())
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                {
                    return Some(value.to_string());
                }
            }
            input
                .get("result_file")
                .and_then(|value| value.as_str())
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(|path| format!("Result file: {path}"))
        }
        _ => None,
    })
}

/// Assemble the assistant `Message` recorded for this turn: its streamed text
/// (if any) followed by the tool calls it emitted, in model order. Mirrors codex
/// recording the assistant function-call item before its outputs.
fn assistant_message(full_text: &str, tool_calls: &[ContentPart]) -> Message {
    let mut content: Vec<ContentPart> = Vec::new();
    if !full_text.is_empty() {
        content.push(ContentPart::text(full_text));
    }
    content.extend(tool_calls.iter().cloned());
    Message::new(MessageRole::Assistant, content)
}

fn tool_call_identity(call: &ContentPart) -> (String, String) {
    match call {
        ContentPart::ToolCall { id, name, .. } => (id.clone(), name.clone()),
        _ => (String::new(), "tool".to_string()),
    }
}

fn tool_result_text_and_status(message: &Message) -> (String, bool) {
    for part in &message.content {
        if let ContentPart::ToolResult {
            content, is_error, ..
        } = part
        {
            return (flatten_content_text(content), *is_error);
        }
    }
    (flatten_content_text(&message.content), true)
}

fn tool_result_event_content(message: &Message) -> Option<Vec<Value>> {
    for part in &message.content {
        if let ContentPart::ToolResult { content, .. } = part {
            return event_content_parts_if_media(content);
        }
    }
    event_content_parts_if_media(&message.content)
}

fn event_content_parts_if_media(parts: &[ContentPart]) -> Option<Vec<Value>> {
    let mut out = Vec::new();
    let mut has_media = false;
    append_event_content_parts(parts, &mut out, &mut has_media);
    has_media.then_some(out)
}

fn append_event_content_parts(parts: &[ContentPart], out: &mut Vec<Value>, has_media: &mut bool) {
    for part in parts {
        match part {
            ContentPart::Text { text } | ContentPart::Reasoning { text, .. } => {
                if !text.is_empty() {
                    out.push(serde_json::json!({ "type": "input_text", "text": text }));
                }
            }
            ContentPart::Media {
                mime_type,
                data,
                url,
                detail,
            } => {
                if let Some(part) = media_event_content_part(
                    mime_type,
                    data.as_deref(),
                    url.as_deref(),
                    detail.as_deref(),
                ) {
                    *has_media = true;
                    out.push(part);
                }
            }
            ContentPart::ToolResult { content, .. } => {
                append_event_content_parts(content, out, has_media);
            }
            ContentPart::ToolCall { .. } => {}
        }
    }
}

fn media_event_content_part(
    mime_type: &str,
    data: Option<&str>,
    url: Option<&str>,
    detail: Option<&str>,
) -> Option<Value> {
    let resolved = match (url, data) {
        (Some(url), _) => url.to_string(),
        (None, Some(data)) => format!("data:{mime_type};base64,{data}"),
        (None, None) => return None,
    };
    if mime_type.starts_with("image/") {
        Some(serde_json::json!({
            "type": "input_image",
            "image_url": resolved,
            "detail": detail.unwrap_or("auto"),
        }))
    } else {
        Some(serde_json::json!({
            "type": "input_file",
            "file_data": resolved,
        }))
    }
}

fn flatten_content_text(parts: &[ContentPart]) -> String {
    let mut chunks = Vec::new();
    collect_content_text(parts, &mut chunks);
    if chunks.is_empty() && !parts.is_empty() {
        serde_json::to_string(parts).unwrap_or_default()
    } else {
        chunks.join("\n")
    }
}

fn collect_content_text(parts: &[ContentPart], chunks: &mut Vec<String>) {
    for part in parts {
        match part {
            ContentPart::Text { text } | ContentPart::Reasoning { text, .. } => {
                if !text.is_empty() {
                    chunks.push(text.clone());
                }
            }
            ContentPart::ToolResult { content, .. } => collect_content_text(content, chunks),
            ContentPart::Media { mime_type, .. } => chunks.push(format!("[media: {mime_type}]")),
            ContentPart::ToolCall { .. } => {}
        }
    }
}

impl<T: SamplingTransport + 'static, R: CallRunner + 'static> SamplingDriver
    for ModelSamplingDriver<T, R>
{
    async fn run_sampling_request(
        &self,
        input: Vec<Message>,
        cancel: CancellationToken,
    ) -> Result<SamplingOutcome, AgentError> {
        // `attempt` == retries already performed (codex `retries`). The outer
        // loop re-opens the stream on each retryable failure. We build the REAL
        // per-turn request from the input here and hand it to `open_stream`, which
        // installs it into the transport before opening — so the provider receives
        // the populated conversation, not an empty body.
        let input = self.input_with_goal_context(input);
        let mut req = build_request(&self.ctx, input);
        let turn_idx = self.next_turn_idx.fetch_add(1, Ordering::Relaxed);
        // Advertise the tool catalog. When a dispatcher is attached (the fused
        // path), it carries the registry's model-visible definitions; we copy them
        // verbatim (order-stable) into `req.tools` so the model can actually emit
        // browser/python/shell tool calls. The text-only driver has no dispatcher,
        // so it sends no tools — which is correct (codex sends tools only when the
        // turn's toolset is non-empty).
        if let Some(dispatcher) = &self.dispatcher {
            req.tools = dispatcher.tool_specs().to_vec();
        }
        // Measure the REAL assembled request so the /context view can attribute
        // the window dynamically: the system prompt and per-tool schemas only
        // exist here (they are never persisted as message events). Uses the same
        // byte->token estimator the agent uses elsewhere, so it stays consistent.
        let composition = request_composition(&req);
        let llm_input = request_observability_input(&req, self.full_llm_input_events);
        let mut attempt: u32 = 0;
        loop {
            self.emit_turn_request(turn_idx, attempt, &composition, &llm_input);
            // ---- open the stream (codex: `client.stream(&prompt).await`) ----
            let mut stream = match self.transport.open_stream(&req) {
                Ok(s) => s,
                Err(e) => {
                    match handle_stream_error(e, &mut attempt, self.max_retries, self.jitter).await
                    {
                        RetryStep::Retry => continue,
                        RetryStep::Fail(err) => return Err(err),
                    }
                }
            };

            // ---- consume the stream under the cancellation token ----
            let mut acc = TurnAccumulator::default();
            let started_at = Instant::now();
            // Set when a retryable mid-stream error tells us to restart the outer
            // loop (codex breaks the inner loop then re-opens).
            let mut restart = false;
            let mut preempted_for_mailbox = false;

            loop {
                let maybe_event = tokio::select! {
                    _ = cancel.cancelled() => {
                        return Err(AgentError::TurnAborted);
                    }
                    ev = stream.next() => ev,
                };

                match maybe_event {
                    Some(Ok(ev)) => {
                        let check_mailbox_preemption = checks_mailbox_preemption_after_event(&ev);
                        match self.consume_event(&mut acc, ev, started_at, turn_idx)? {
                            StreamProgress::Continue => {
                                if check_mailbox_preemption && self.has_mailbox_preemption().await {
                                    preempted_for_mailbox = true;
                                    break;
                                }
                            }
                            StreamProgress::Done => break,
                        }
                    }
                    Some(Err(e)) => {
                        match handle_stream_error(e, &mut attempt, self.max_retries, self.jitter)
                            .await
                        {
                            RetryStep::Retry => {
                                restart = true;
                                break;
                            }
                            RetryStep::Fail(err) => return Err(err),
                        }
                    }
                    None => break,
                }
            }

            if restart {
                continue;
            }

            // ---- assemble the outcome (codex parity) ----
            let mut last_agent_message =
                (!acc.full_text.trim().is_empty()).then(|| acc.full_text.clone());

            // Fused tool dispatch (codex `try_run_turn` / `try_run_sampling_request`):
            // when a dispatcher + recorder are configured AND the model emitted
            // ≥1 tool call, record the assistant message, run the calls in model
            // order through the dispatcher, record their outputs, and report
            // follow-up so the loop re-samples with those outputs in context.
            let tool_calls = extract_tool_calls(&acc.tool_calls);
            let model_needs_follow_up =
                match (&self.dispatcher, &self.recorder, tool_calls.is_empty()) {
                    // Fused path with at least one tool call.
                    (Some(dispatcher), Some(recorder), false) => {
                        // A successful `done` call declares the turn finished. Detect
                        // the call before dispatch, but decide terminality only after
                        // the handler output has been recorded; eval-mode done audit
                        // can reject weak completions and should get a repair turn.
                        let has_done_call = calls_done_tool(&tool_calls);

                        // 1. Record the assistant message (text + tool calls), so the
                        //    recorded transcript carries the call before its output.
                        let assistant = assistant_message(&acc.full_text, &tool_calls);
                        recorder.record(std::slice::from_ref(&assistant)).await;

                        // 2. Dispatch in model order through the parallel/serial gate.
                        let tool_started_at = Instant::now();
                        let result = dispatcher
                            .dispatch_ordered(tool_calls.clone(), cancel.clone())
                            .await;
                        self.emit_goal_elapsed_accounting(tool_started_at);

                        // 3. Record each tool output (already in model order).
                        if !result.outputs_in_order.is_empty() {
                            for (call, output) in
                                tool_calls.iter().zip(result.outputs_in_order.iter())
                            {
                                self.emit_tool_result(call, output);
                            }
                            recorder.record(&result.outputs_in_order).await;
                        }

                        let done_succeeded = has_done_call
                            && done_tool_succeeded(&tool_calls, &result.outputs_in_order);

                        // Surface the successful `done` summary as the turn result
                        // when the model declared completion and streamed no other
                        // text, so the loop returns the summary (codex keeps the final
                        // message). Rejected done calls keep following up.
                        if done_succeeded && last_agent_message.is_none() {
                            last_agent_message = done_summary(&tool_calls);
                            if last_agent_message.is_some() {
                                acc.defers_mailbox_delivery_to_next_turn = true;
                            }
                        }

                        // A successful `done` call is TERMINAL. A rejected `done` call
                        // needs follow-up so the model can repair the final answer.
                        if done_succeeded {
                            false
                        } else {
                            decision::needs_follow_up(
                                result.needs_follow_up || has_done_call,
                                false,
                            )
                        }
                    }
                    // Text-only sampler, OR fusion configured but no tool call: the
                    // model is done (no dispatch). `model_needs_follow_up` still
                    // reflects whether the model emitted a call so a non-fused driver
                    // signals the loop the same way it did pre-fusion (WP-B5).
                    _ => decision::needs_follow_up(!acc.tool_calls.is_empty(), false),
                };
            let model_needs_follow_up =
                decision::needs_follow_up(model_needs_follow_up, preempted_for_mailbox);

            return Ok(SamplingOutcome {
                model_needs_follow_up,
                last_agent_message,
                defers_mailbox_delivery_to_next_turn: acc.defers_mailbox_delivery_to_next_turn,
                finish_reason: acc.finish_reason,
            });
        }
    }
}

/// Build the per-turn [`LlmRequest`] from the input messages + turn identity.
///
/// The model / provider come from the [`TurnCtx`] and the per-turn `input`
/// messages are installed here. This request is what
/// [`run_sampling_request`](ModelSamplingDriver::run_sampling_request) hands to
/// [`SamplingTransport::open_stream`], so it MUST carry the real conversation —
/// the transport now uses *this* request (not a fixed empty one) on every open.
///
/// `req.tools` is left empty here and is populated by the caller
/// ([`run_sampling_request`](ModelSamplingDriver::run_sampling_request)) from the
/// attached [`ToolDispatcher`]'s model-visible specs
/// (`ToolDispatcher::tool_specs`), which the dispatcher captured from the
/// registry at construction. That keeps this builder pure (no toolset access) and
/// unit-reachable while the fused driver still advertises the catalog.
fn build_request(ctx: &TurnCtx, input: Vec<Message>) -> LlmRequest {
    let mut req = LlmRequest::new(ctx.model.clone(), ctx.provider.clone());
    let mut base_system = SystemPart::new(ctx.base_instructions.clone());
    base_system.cache = Some(CacheHint::Ephemeral);
    req.system.push(base_system);
    req.messages = input;
    if let Some(instruction) = ctx.browser_mode_instruction.as_deref() {
        req.messages.insert(
            0,
            Message::new(
                MessageRole::System,
                vec![ContentPart::text(instruction.to_string())],
            ),
        );
    }
    super::model_path::apply_browser_use_provider_options(&ctx.provider, &mut req);
    mark_message_cache_breakpoints(&mut req.messages);
    req
}

fn mark_message_cache_breakpoints(messages: &mut [Message]) {
    const LOOKBACK_TARGET_BLOCKS: usize = 16;
    const MAX_MESSAGE_BREAKPOINTS: usize = 2;

    for message in messages.iter_mut() {
        message.cache = None;
    }

    let eligible: Vec<(usize, usize)> = messages
        .iter()
        .enumerate()
        .filter(|(_, message)| {
            !matches!(message.role, MessageRole::System | MessageRole::Developer)
        })
        .map(|(index, message)| (index, message.content.len().max(1)))
        .collect();
    let Some((last_index, _)) = eligible.last().copied() else {
        return;
    };

    let mut selected = vec![last_index];
    let mut blocks_since_last = 0usize;
    for (index, block_count) in eligible.into_iter().rev().skip(1) {
        blocks_since_last = blocks_since_last.saturating_add(block_count);
        if blocks_since_last >= LOOKBACK_TARGET_BLOCKS {
            selected.push(index);
            break;
        }
    }
    selected.sort_unstable();
    selected.dedup();
    if selected.len() > MAX_MESSAGE_BREAKPOINTS {
        selected.drain(0..selected.len() - MAX_MESSAGE_BREAKPOINTS);
    }
    for index in selected {
        if let Some(message) = messages.get_mut(index) {
            message.cache = Some(CacheHint::Ephemeral);
        }
    }
}

/// Token attribution for the per-turn request, computed from the REAL assembled
/// [`LlmRequest`]. The system prompt and tool schemas are not message events, so
/// this is the only place the `/context` view can learn their size. Counts use
/// the agent's standard byte->token estimate so they line up with the
/// compaction accounting.
fn request_composition(req: &LlmRequest) -> Value {
    use crate::context::accounting::approx_tokens_from_byte_count_i64;

    let system_bytes: i64 = req.system.iter().map(|part| part.text.len() as i64).sum();
    let system_prompt_tokens = approx_tokens_from_byte_count_i64(system_bytes);

    let tools: Vec<Value> = req
        .tools
        .iter()
        .map(|tool| {
            let bytes = serde_json::to_string(tool).map(|s| s.len()).unwrap_or(0) as i64;
            serde_json::json!({
                "name": tool.name,
                "tokens": approx_tokens_from_byte_count_i64(bytes),
            })
        })
        .collect();

    serde_json::json!({
        "system_prompt_tokens": system_prompt_tokens,
        "tools": tools,
    })
}

fn request_observability_input(req: &LlmRequest, include_full_input: bool) -> Value {
    let message_count = req.messages.len();
    if !include_full_input {
        return serde_json::json!({
            "message_count": message_count,
            "system_count": req.system.len(),
            "tools_count": req.tools.len(),
            "omitted_earlier_messages": message_count,
            "truncated": true,
            "full_input_omitted": true,
        });
    }

    let messages: Vec<Value> = req.messages.iter().map(observability_json_value).collect();
    let system: Vec<Value> = req.system.iter().map(observability_json_value).collect();
    let tools: Vec<Value> = req.tools.iter().map(observability_json_value).collect();

    serde_json::json!({
        "system": system,
        "messages": messages,
        "tools": tools,
        "tools_count": tools.len(),
        "message_count": message_count,
        "omitted_earlier_messages": 0,
        "truncated": false,
    })
}

fn observability_json_value<T: serde::Serialize>(value: &T) -> Value {
    serde_json::to_value(value)
        .map(sanitize_observability_value)
        .unwrap_or(Value::Null)
}

fn sanitize_observability_value(value: Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut out = serde_json::Map::with_capacity(map.len());
            for (key, value) in map {
                if is_observability_secret_key(&key) {
                    out.insert(key, Value::String("[redacted]".to_string()));
                } else {
                    out.insert(key, sanitize_observability_value(value));
                }
            }
            Value::Object(out)
        }
        Value::Array(values) => Value::Array(
            values
                .into_iter()
                .map(sanitize_observability_value)
                .collect(),
        ),
        other => other,
    }
}

fn is_observability_secret_key(key: &str) -> bool {
    let key = key.to_ascii_lowercase();
    key.contains("api_key")
        || key.contains("apikey")
        || key.contains("authorization")
        || key.contains("auth_token")
        || key.contains("password")
        || key.contains("secret")
        || key.contains("token")
        || key.contains("cookie")
}
