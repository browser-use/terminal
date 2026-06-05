//! Tests for the async unbounded [`TurnLoop`] driver (WP-C2).
//!
//! NETWORK-FREE & DETERMINISTIC: the loop is generic over the three frozen turn
//! traits, so every test injects fakes — a [`ScriptedSamplingDriver`] returning a
//! pre-baked queue of [`SamplingOutcome`]s, an [`InMemoryTurnState`] (a `Vec`
//! history + a `VecDeque` of pending steer + a scriptable [`TokenStatus`] and a
//! compaction counter), and a [`RecordingObserver`]. No `ModelClient`, sandbox,
//! or socket is ever touched, and there are no timers, so the tests are fast and
//! reproducible.
//!
//! Each test pins one branch of codex's loop (`turn.rs:131-400`) through the PURE
//! [`decision::classify_loop_step`] core that the loop routes every iteration
//! through:
//! 1. a 3-iteration `[follow_up, follow_up, complete]` run → exactly 3 samplings;
//! 2. pending input keeps the loop going even with `model_needs_follow_up=false`;
//! 3. `token_limit_reached && needs_follow_up` → a `CompactThenContinue` step
//!    (compact hook fires, `can_drain` set per `!model_needs_follow_up`);
//! 4. `initial_can_drain` respects `turn_has_fresh_input`;
//! 5. cancellation → the loop breaks and returns the accumulated message;
//! 6. the loop is UNBOUNDED — a 50-iteration scripted run completes (no cap).

use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use browser_use_llm::schema::{ContentPart, Message, MessageRole};
use tokio_util::sync::CancellationToken;

use crate::decision::{SamplingOutcome, TokenStatus};
use crate::events::TurnCtx;
use crate::task::{TurnAbortReason, TurnLifecycleEvent};
use crate::turn::{SamplingDriver, TurnLoop, TurnObserver, TurnState};
use crate::AgentError;

// ---- scripted sampling driver --------------------------------------------

/// One scripted sampling result: either a successful [`SamplingOutcome`] or a
/// hard error to return from that iteration's `run_sampling_request`.
enum SamplingScript {
    Ok(SamplingOutcome),
    Err(AgentError),
}

/// A [`SamplingDriver`] that replays a queue of [`SamplingScript`]s, one per
/// `run_sampling_request` call, and records how many requests were made + the
/// input each request carried (so a test can assert drain timing). It also
/// honors the [`CancellationToken`]: if cancellation has already fired when a
/// request is made, it returns [`AgentError::TurnAborted`] (modeling a sampler
/// that aborts on a cancelled turn).
struct ScriptedSamplingDriver {
    scripts: Mutex<VecDeque<SamplingScript>>,
    /// Number of `run_sampling_request` calls that returned an outcome/error.
    requests: Arc<AtomicUsize>,
    /// The input `Vec<Message>` of each request, in order (drain assertions).
    inputs: Arc<Mutex<Vec<Vec<Message>>>>,
}

impl ScriptedSamplingDriver {
    fn new(scripts: Vec<SamplingScript>) -> Self {
        Self {
            scripts: Mutex::new(scripts.into_iter().collect()),
            requests: Arc::new(AtomicUsize::new(0)),
            inputs: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn requests_handle(&self) -> Arc<AtomicUsize> {
        self.requests.clone()
    }

    fn inputs_handle(&self) -> Arc<Mutex<Vec<Vec<Message>>>> {
        self.inputs.clone()
    }
}

impl SamplingDriver for ScriptedSamplingDriver {
    async fn run_sampling_request(
        &self,
        input: Vec<Message>,
        cancel: CancellationToken,
    ) -> Result<SamplingOutcome, AgentError> {
        // A sampler aborts a request on an already-cancelled turn (mirrors the
        // real `ModelSamplingDriver`'s cancel-mid-stream → TurnAborted).
        if cancel.is_cancelled() {
            return Err(AgentError::TurnAborted);
        }
        self.requests.fetch_add(1, Ordering::SeqCst);
        self.inputs.lock().unwrap().push(input);
        match self.scripts.lock().unwrap().pop_front() {
            Some(SamplingScript::Ok(out)) => Ok(out),
            Some(SamplingScript::Err(e)) => Err(e),
            // Past the end of the queue, default to a Complete-shaped outcome so a
            // mis-scripted test fails by under-running rather than hanging.
            None => Ok(complete("end")),
        }
    }
}

// ---- in-memory turn state -------------------------------------------------

/// A network-free [`TurnState`]: a `Vec` history, a `VecDeque` of pending steer
/// input (drained by the loop when its gate is open), a scriptable
/// [`TokenStatus`] (to force a `CompactThenContinue`), and a compaction counter
/// (to prove the compact hook fired exactly when expected).
struct InMemoryTurnState {
    history: Mutex<Vec<Message>>,
    pending: Mutex<VecDeque<Message>>,
    token_status: Mutex<TokenStatus>,
    compactions: Arc<AtomicUsize>,
    /// Number of times the loop drained pending input (`take_pending_input`).
    drains: Arc<AtomicUsize>,
    /// The size of each drain, in order (so a test can see WHICH iteration drained).
    drain_sizes: Arc<Mutex<Vec<usize>>>,
    /// Number of times the loop asked the state to defer mailbox delivery.
    deferrals: Arc<AtomicUsize>,
}

impl InMemoryTurnState {
    fn new(pending: Vec<Message>, token_status: TokenStatus) -> Self {
        Self {
            history: Mutex::new(Vec::new()),
            pending: Mutex::new(pending.into_iter().collect()),
            token_status: Mutex::new(token_status),
            compactions: Arc::new(AtomicUsize::new(0)),
            drains: Arc::new(AtomicUsize::new(0)),
            drain_sizes: Arc::new(Mutex::new(Vec::new())),
            deferrals: Arc::new(AtomicUsize::new(0)),
        }
    }

    fn compactions_handle(&self) -> Arc<AtomicUsize> {
        self.compactions.clone()
    }

    fn drains_handle(&self) -> Arc<AtomicUsize> {
        self.drains.clone()
    }

    fn drain_sizes_handle(&self) -> Arc<Mutex<Vec<usize>>> {
        self.drain_sizes.clone()
    }

    fn deferrals_handle(&self) -> Arc<AtomicUsize> {
        self.deferrals.clone()
    }
}

impl TurnState for InMemoryTurnState {
    async fn clone_history_for_prompt(&self) -> Vec<Message> {
        self.history.lock().unwrap().clone()
    }

    async fn record_items(&self, items: &[Message]) {
        self.history.lock().unwrap().extend_from_slice(items);
    }

    async fn has_pending_input(&self) -> bool {
        !self.pending.lock().unwrap().is_empty()
    }

    async fn take_pending_input(&self) -> Vec<Message> {
        let drained: Vec<Message> = self.pending.lock().unwrap().drain(..).collect();
        self.drains.fetch_add(1, Ordering::SeqCst);
        self.drain_sizes.lock().unwrap().push(drained.len());
        drained
    }

    async fn token_status(&self) -> TokenStatus {
        self.token_status.lock().unwrap().clone()
    }

    async fn compact(&self, _mode: crate::turn::CompactionMode) -> Result<(), crate::AgentError> {
        self.compactions.fetch_add(1, Ordering::SeqCst);
        // Stub compaction body: after compacting, the (modeled) token pressure is
        // relieved so the loop does not compact forever. This keeps the
        // compaction CONTROL FLOW codex-faithful (compact-then-continue) without
        // a real summarizer.
        let mut st = self.token_status.lock().unwrap();
        st.full_context_window_limit_reached = false;
        st.token_limit_reached = false;
        Ok(())
    }

    async fn defer_mailbox_delivery_to_next_turn(&self) {
        self.deferrals.fetch_add(1, Ordering::SeqCst);
    }
}

// ---- recording observer ---------------------------------------------------

/// Records every [`TurnLifecycleEvent`] the loop emits, for assertion.
#[derive(Default)]
struct RecordingObserver {
    events: Mutex<Vec<TurnLifecycleEvent>>,
}

impl RecordingObserver {
    fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    fn kinds(&self) -> Vec<&'static str> {
        self.events
            .lock()
            .unwrap()
            .iter()
            .map(|e| match e {
                TurnLifecycleEvent::TurnStarted { .. } => "started",
                TurnLifecycleEvent::TurnComplete { .. } => "complete",
                TurnLifecycleEvent::TurnAborted { .. } => "aborted",
            })
            .collect()
    }
}

impl TurnObserver for Arc<RecordingObserver> {
    fn on_lifecycle(&self, ev: TurnLifecycleEvent) {
        self.events.lock().unwrap().push(ev);
    }
}

// ---- helpers --------------------------------------------------------------

fn ctx() -> TurnCtx {
    TurnCtx {
        session_id: "sess-loop".to_string(),
        model: "gpt-5-codex".to_string(),
        provider: "openai".to_string(),
        base_instructions: crate::prompts::browser_agent_system_prompt(),
        browser_mode_instruction: None,
        turn_idx: 0,
        attempt: 0,
    }
}

/// An outcome that asks for another round-trip (model emitted ≥1 tool call).
fn follow_up(msg: &str) -> SamplingOutcome {
    SamplingOutcome {
        model_needs_follow_up: true,
        last_agent_message: Some(msg.to_string()),
        defers_mailbox_delivery_to_next_turn: false,
        finish_reason: None,
    }
}

/// A terminal outcome (no follow-up; the model is done).
fn complete(msg: &str) -> SamplingOutcome {
    SamplingOutcome {
        model_needs_follow_up: false,
        last_agent_message: Some(msg.to_string()),
        defers_mailbox_delivery_to_next_turn: true,
        finish_reason: None,
    }
}

fn commentary_complete(msg: &str) -> SamplingOutcome {
    SamplingOutcome {
        model_needs_follow_up: false,
        last_agent_message: Some(msg.to_string()),
        defers_mailbox_delivery_to_next_turn: false,
        finish_reason: None,
    }
}

/// A `TokenStatus` with the loop's compaction trigger off / on.
fn token_status(limit_reached: bool) -> TokenStatus {
    TokenStatus {
        auto_compact_scope_tokens: 0,
        auto_compact_scope_limit: 1,
        full_context_window_limit_reached: limit_reached,
        token_limit_reached: limit_reached,
    }
}

fn user_msg(s: &str) -> Message {
    Message::new(MessageRole::User, vec![ContentPart::text(s)])
}

// ---- (1) 3-iteration run: [follow_up, follow_up, complete] ----------------

#[tokio::test]
async fn three_iteration_run_samples_thrice_then_completes() {
    let sampler = ScriptedSamplingDriver::new(vec![
        SamplingScript::Ok(follow_up("step 1")),
        SamplingScript::Ok(follow_up("step 2")),
        SamplingScript::Ok(complete("final answer")),
    ]);
    let requests = sampler.requests_handle();

    // No pending input, no token pressure → the loop runs purely on the model's
    // follow-up signal: continue, continue, then complete.
    let state = InMemoryTurnState::new(Vec::new(), token_status(false));
    let observer = RecordingObserver::new();

    let turn = TurnLoop::new(state, sampler, observer.clone());
    let out = turn
        .run(
            ctx(),
            /* turn_has_fresh_input */ false,
            CancellationToken::new(),
        )
        .await
        .expect("loop should complete");

    // Exactly 3 sampling round-trips, then completion.
    assert_eq!(
        requests.load(Ordering::SeqCst),
        3,
        "loop must sample exactly 3 times: follow_up, follow_up, complete"
    );
    assert_eq!(
        out.as_deref(),
        Some("final answer"),
        "the final assistant message is returned"
    );
    // Lifecycle: started once, completed once, never aborted.
    assert_eq!(observer.kinds(), vec!["started", "complete"]);
}

// ---- (2) pending input keeps the loop going even when model is done -------

#[tokio::test]
async fn pending_input_continues_loop_when_model_not_following_up() {
    // The model is DONE on BOTH rounds (model_needs_follow_up = false). The turn
    // started with fresh input, so iteration 1 HOLDS the drain
    // (`initial_can_drain(true) == false`): the steer message stays queued. After
    // iteration 1's (done) outcome, `has_pending_input` is still true, so
    // needs_follow_up = (false || true) = true → Continue, NOT Complete. Iteration
    // 2 (gate now open) drains the steer and — queue empty, model done — completes.
    //
    // This isolates the `has_pending_input` half of `needs_follow_up`
    // (turn.rs:255): a model that wants nothing still loops while steer is pending.
    let sampler = ScriptedSamplingDriver::new(vec![
        SamplingScript::Ok(complete("done, but...")),
        SamplingScript::Ok(complete("really done")),
    ]);
    let requests = sampler.requests_handle();

    // One pending steer message present, no token pressure.
    let state = InMemoryTurnState::new(vec![user_msg("wait, also do X")], token_status(false));
    let drains = state.drains_handle();
    let observer = RecordingObserver::new();

    let turn = TurnLoop::new(state, sampler, observer.clone());
    let out = turn
        .run(
            ctx(),
            /* turn_has_fresh_input */ true,
            CancellationToken::new(),
        )
        .await
        .expect("loop should complete");

    // The model said "done" on the first round, but the still-queued pending input
    // forced a second round-trip; the second round (queue now drained) completes.
    assert_eq!(
        requests.load(Ordering::SeqCst),
        2,
        "pending input must force a follow-up sampling even when model is done"
    );
    assert_eq!(out.as_deref(), Some("really done"));
    // The pending input was drained exactly once — on iteration 2, after the gate
    // opened (iteration 1 held it because the turn had fresh input).
    assert_eq!(
        drains.load(Ordering::SeqCst),
        1,
        "pending input drains once, on the iteration after the gate opens"
    );
    assert_eq!(observer.kinds(), vec!["started", "complete"]);
}

#[tokio::test]
async fn terminal_output_defers_mailbox_only_when_sampling_outcome_says_so() {
    let sampler = ScriptedSamplingDriver::new(vec![SamplingScript::Ok(complete("final"))]);
    let state = InMemoryTurnState::new(Vec::new(), token_status(false));
    let deferrals = state.deferrals_handle();
    let observer = RecordingObserver::new();

    let out = TurnLoop::new(state, sampler, observer)
        .run(
            ctx(),
            /* turn_has_fresh_input */ true,
            CancellationToken::new(),
        )
        .await
        .expect("loop should complete");

    assert_eq!(out.as_deref(), Some("final"));
    assert_eq!(
        deferrals.load(Ordering::SeqCst),
        1,
        "final assistant output should defer mailbox delivery to next turn"
    );

    let sampler =
        ScriptedSamplingDriver::new(vec![SamplingScript::Ok(commentary_complete("thinking"))]);
    let state = InMemoryTurnState::new(Vec::new(), token_status(false));
    let deferrals = state.deferrals_handle();
    let observer = RecordingObserver::new();

    let out = TurnLoop::new(state, sampler, observer)
        .run(
            ctx(),
            /* turn_has_fresh_input */ true,
            CancellationToken::new(),
        )
        .await
        .expect("loop should complete");

    assert_eq!(out.as_deref(), Some("thinking"));
    assert_eq!(
        deferrals.load(Ordering::SeqCst),
        0,
        "commentary assistant output should leave mailbox delivery current-turn"
    );
}

// ---- (3) token_limit_reached + needs_follow_up → CompactThenContinue ------

#[tokio::test]
async fn token_limit_with_follow_up_triggers_one_compaction() {
    // Iteration 1: model wants follow-up AND token_limit_reached →
    // CompactThenContinue. The compact hook (stub) relieves the pressure, so
    // iteration 2's outcome (complete) ends the turn with no further compaction.
    let sampler = ScriptedSamplingDriver::new(vec![
        SamplingScript::Ok(follow_up("needs more + over budget")),
        SamplingScript::Ok(complete("compacted, finished")),
    ]);
    let requests = sampler.requests_handle();

    // Token limit reached up front; no pending input.
    let state = InMemoryTurnState::new(Vec::new(), token_status(true));
    let compactions = state.compactions_handle();
    let observer = RecordingObserver::new();

    let turn = TurnLoop::new(state, sampler, observer.clone());
    let out = turn
        .run(ctx(), false, CancellationToken::new())
        .await
        .expect("loop should complete after compaction");

    assert_eq!(
        compactions.load(Ordering::SeqCst),
        1,
        "exactly one compaction when token_limit_reached && needs_follow_up"
    );
    assert_eq!(
        requests.load(Ordering::SeqCst),
        2,
        "compact-then-continue runs a second sampling round-trip"
    );
    assert_eq!(out.as_deref(), Some("compacted, finished"));
    assert_eq!(observer.kinds(), vec!["started", "complete"]);
}

#[tokio::test]
async fn compact_sets_can_drain_per_model_follow_up() {
    // can_drain_next after compaction == !model_needs_follow_up (turn.rs:306).
    // Here the model is DONE (model_needs_follow_up = false) but there is pending
    // input AND token_limit_reached → CompactThenContinue { can_drain_next: true }.
    // So on the iteration AFTER the compaction the loop drains the pending input.
    let sampler = ScriptedSamplingDriver::new(vec![
        SamplingScript::Ok(complete("done but over budget")), // iter 1: compact, may drain next
        SamplingScript::Ok(complete("all wrapped up")),       // iter 2: drains input, completes
    ]);
    let requests = sampler.requests_handle();

    // Pending input present + token limit reached + fresh input on this turn, so
    // the FIRST iteration holds the drain (initial_can_drain == false). The model
    // is done, but pending input + token limit force CompactThenContinue with
    // can_drain_next == true → the pending input is drained on iteration 2.
    let state = InMemoryTurnState::new(vec![user_msg("steer me")], token_status(true));
    let compactions = state.compactions_handle();
    let drain_sizes = state.drain_sizes_handle();
    let observer = RecordingObserver::new();

    let turn = TurnLoop::new(state, sampler, observer);
    let out = turn
        .run(
            ctx(),
            /* turn_has_fresh_input */ true,
            CancellationToken::new(),
        )
        .await
        .expect("loop should complete");

    assert_eq!(compactions.load(Ordering::SeqCst), 1, "one compaction");
    assert_eq!(requests.load(Ordering::SeqCst), 2);
    assert_eq!(out.as_deref(), Some("all wrapped up"));
    // With fresh input, iteration 1 did NOT drain (gate held). Compaction set
    // can_drain_next = !model_nfu = true, so iteration 2 drained the one steer
    // message. The recorded drain history therefore is exactly `[1]`.
    assert_eq!(
        *drain_sizes.lock().unwrap(),
        vec![1usize],
        "drain happens once, after compaction, when can_drain_next is true"
    );
}

// ---- (4) initial_can_drain respects turn_has_fresh_input ------------------

#[tokio::test]
async fn fresh_input_holds_initial_drain() {
    // turn_has_fresh_input == true → initial_can_drain == false: the very first
    // iteration must NOT drain pending input (codex samples the fresh input
    // first; turn.rs:168). With the model done on iteration 1 but pending input
    // present, the loop continues to iteration 2 where the gate is open and the
    // input is drained, then completes.
    let sampler = ScriptedSamplingDriver::new(vec![
        SamplingScript::Ok(complete("answering fresh input")), // iter 1: no drain
        SamplingScript::Ok(complete("answering steer")),       // iter 2: drains, completes
    ]);
    let requests = sampler.requests_handle();
    let state = InMemoryTurnState::new(vec![user_msg("queued steer")], token_status(false));
    let drain_sizes = state.drain_sizes_handle();
    let observer = RecordingObserver::new();

    let turn = TurnLoop::new(state, sampler, observer);
    let out = turn
        .run(
            ctx(),
            /* turn_has_fresh_input */ true,
            CancellationToken::new(),
        )
        .await
        .expect("loop should complete");

    assert_eq!(requests.load(Ordering::SeqCst), 2);
    assert_eq!(out.as_deref(), Some("answering steer"));
    // Drain history is exactly `[1]`: iteration 1 held the gate (fresh input),
    // iteration 2 (gate open) drained the single steer message.
    assert_eq!(
        *drain_sizes.lock().unwrap(),
        vec![1usize],
        "fresh input must hold the drain on the first iteration only"
    );
}

#[tokio::test]
async fn no_fresh_input_drains_on_first_iteration() {
    // turn_has_fresh_input == false → initial_can_drain == true: the first
    // iteration drains pending input immediately. The model is done and the queue
    // empties on iteration 1, so the turn completes in a single round-trip.
    let sampler = ScriptedSamplingDriver::new(vec![SamplingScript::Ok(complete("one shot"))]);
    let requests = sampler.requests_handle();
    let state = InMemoryTurnState::new(vec![user_msg("queued steer")], token_status(false));
    let drain_sizes = state.drain_sizes_handle();
    let inputs = sampler.inputs_handle();
    let observer = RecordingObserver::new();

    let turn = TurnLoop::new(state, sampler, observer);
    let out = turn
        .run(
            ctx(),
            /* turn_has_fresh_input */ false,
            CancellationToken::new(),
        )
        .await
        .expect("loop should complete");

    assert_eq!(requests.load(Ordering::SeqCst), 1, "single round-trip");
    assert_eq!(out.as_deref(), Some("one shot"));
    // The first (and only) iteration drained the steer message immediately.
    assert_eq!(*drain_sizes.lock().unwrap(), vec![1usize]);
    // The drained input reached the sampler's request body on iteration 1.
    let recorded = inputs.lock().unwrap();
    assert_eq!(recorded.len(), 1);
    assert_eq!(
        recorded[0],
        vec![user_msg("queued steer")],
        "drained pending input must be threaded into the sampling request"
    );
}

// ---- (5) cancellation → loop breaks and returns accumulated message -------

#[tokio::test]
async fn pre_cancelled_turn_aborts_first_request_and_returns_none() {
    // A turn whose cancellation token is already fired: the very first sampling
    // request observes the cancelled token and returns TurnAborted, so the loop
    // breaks with no message accumulated. It must emit TurnStarted → TurnAborted
    // (never TurnComplete) and return Ok(None) — codex reports the abort via an
    // event rather than as a hard error (turn.rs:357).
    let sampler = ScriptedSamplingDriver::new(vec![SamplingScript::Ok(complete("unreached"))]);
    let requests = sampler.requests_handle();
    let state = InMemoryTurnState::new(Vec::new(), token_status(false));
    let observer = RecordingObserver::new();

    let cancel = CancellationToken::new();
    cancel.cancel();
    let turn = TurnLoop::new(state, sampler, observer.clone());
    let out = turn
        .run(ctx(), false, cancel)
        .await
        .expect("a cancelled turn returns Ok, not a hard error");

    assert_eq!(
        requests.load(Ordering::SeqCst),
        0,
        "a pre-cancelled turn aborts the first request before it records"
    );
    assert_eq!(out, None, "no message accumulated before the abort");
    assert_eq!(
        observer.kinds(),
        vec!["started", "aborted"],
        "cancellation emits TurnStarted then TurnAborted (never TurnComplete)"
    );
}

#[tokio::test]
async fn cancellation_after_progress_returns_partial_message() {
    // A deterministic mid-turn cancel: the loop is driven by a sampler that
    // cancels the token *itself* on the FIRST request (after recording its
    // outcome), so the SECOND request observes the cancelled token and aborts.
    // This proves the loop returns the message accumulated on iteration 1.
    let cancel = CancellationToken::new();
    let sampler = CancelAfterFirst::new(follow_up("partial progress"), cancel.clone());
    let requests = sampler.requests.clone();
    let state = InMemoryTurnState::new(Vec::new(), token_status(false));
    let observer = RecordingObserver::new();

    let turn = TurnLoop::new(state, sampler, observer.clone());
    let out = turn
        .run(ctx(), false, cancel)
        .await
        .expect("cancelled turn returns Ok with the accumulated message");

    assert_eq!(
        requests.load(Ordering::SeqCst),
        1,
        "iteration 1 runs and records; iteration 2 sees cancel and aborts"
    );
    assert_eq!(
        out.as_deref(),
        Some("partial progress"),
        "the loop returns the message accumulated before the abort"
    );
    assert_eq!(observer.kinds(), vec!["started", "aborted"]);
}

/// A sampler that returns one (follow-up) outcome on its first request — and
/// cancels the shared token as a side effect — so the loop's *next* request hits
/// a cancelled turn and aborts. Deterministic, no timers.
struct CancelAfterFirst {
    first: Mutex<Option<SamplingOutcome>>,
    cancel: CancellationToken,
    requests: Arc<AtomicUsize>,
}

impl CancelAfterFirst {
    fn new(first: SamplingOutcome, cancel: CancellationToken) -> Self {
        Self {
            first: Mutex::new(Some(first)),
            cancel,
            requests: Arc::new(AtomicUsize::new(0)),
        }
    }
}

impl SamplingDriver for CancelAfterFirst {
    async fn run_sampling_request(
        &self,
        _input: Vec<Message>,
        cancel: CancellationToken,
    ) -> Result<SamplingOutcome, AgentError> {
        if cancel.is_cancelled() {
            return Err(AgentError::TurnAborted);
        }
        self.requests.fetch_add(1, Ordering::SeqCst);
        let out = self
            .first
            .lock()
            .unwrap()
            .take()
            .expect("first outcome consumed twice");
        // Cancel the turn so the NEXT request aborts (the model itself emitted a
        // follow-up, so the loop would otherwise sample again).
        self.cancel.cancel();
        Ok(out)
    }
}

// ---- (6) UNBOUNDED: a 50-iteration run completes without a cap ------------

#[tokio::test]
async fn loop_is_unbounded_fifty_iterations_complete() {
    // 49 follow-ups then a complete: a 50-round-trip turn. There is NO max-turns
    // counter in the loop (codex `turn.rs:214` is an unbounded `loop {}`), so this
    // must complete rather than bail at some cap.
    let mut scripts: Vec<SamplingScript> = (0..49)
        .map(|i| SamplingScript::Ok(follow_up(&format!("step {i}"))))
        .collect();
    scripts.push(SamplingScript::Ok(complete("finished after 50")));

    let sampler = ScriptedSamplingDriver::new(scripts);
    let requests = sampler.requests_handle();
    let state = InMemoryTurnState::new(Vec::new(), token_status(false));
    let observer = RecordingObserver::new();

    let turn = TurnLoop::new(state, sampler, observer.clone());
    let out = turn
        .run(ctx(), false, CancellationToken::new())
        .await
        .expect("an unbounded loop completes a long run");

    assert_eq!(
        requests.load(Ordering::SeqCst),
        50,
        "the loop must run all 50 round-trips (no max-turns cap)"
    );
    assert_eq!(out.as_deref(), Some("finished after 50"));
    assert_eq!(observer.kinds(), vec!["started", "complete"]);
}

// ---- (7) bounded run stops after max_turns -------------------------------

#[tokio::test]
async fn bounded_loop_aborts_after_max_turns() {
    let sampler = ScriptedSamplingDriver::new(vec![
        SamplingScript::Ok(follow_up("step 0")),
        SamplingScript::Ok(follow_up("step 1")),
        SamplingScript::Ok(complete("should not run")),
    ]);
    let requests = sampler.requests_handle();
    let inputs = sampler.inputs_handle();
    let state = InMemoryTurnState::new(Vec::new(), token_status(false));
    let observer = RecordingObserver::new();

    let turn = TurnLoop::new(state, sampler, observer.clone());
    let out = turn
        .run_with_max_turns(ctx(), false, CancellationToken::new(), 2)
        .await
        .expect("bounded loop should stop gracefully");

    assert_eq!(requests.load(Ordering::SeqCst), 2);
    assert_eq!(out.as_deref(), Some("step 1"));
    let recorded_inputs = inputs.lock().unwrap();
    let Some(Message {
        role: MessageRole::Developer,
        content,
        ..
    }) = recorded_inputs[1].last()
    else {
        panic!("last bounded request should include final-step developer nudge");
    };
    assert!(
        matches!(content.first(), Some(ContentPart::Text { text }) if text.contains("final allowed step")),
        "final nudge should tell the agent to finish"
    );
    assert_eq!(observer.kinds(), vec!["started", "aborted"]);
    let events = observer.events.lock().unwrap();
    assert!(matches!(
        events.last(),
        Some(TurnLifecycleEvent::TurnAborted {
            reason: TurnAbortReason::Interrupted,
            ..
        })
    ));
}

#[tokio::test]
async fn bounded_loop_adds_progress_nudge_for_long_runs() {
    let mut scripts: Vec<SamplingScript> = (0..50)
        .map(|i| SamplingScript::Ok(follow_up(&format!("step {i}"))))
        .collect();
    scripts.push(SamplingScript::Ok(complete("done after checkpoint")));

    let sampler = ScriptedSamplingDriver::new(scripts);
    let requests = sampler.requests_handle();
    let inputs = sampler.inputs_handle();
    let state = InMemoryTurnState::new(Vec::new(), token_status(false));
    let observer = RecordingObserver::new();

    let turn = TurnLoop::new(state, sampler, observer.clone());
    let out = turn
        .run_with_max_turns(ctx(), false, CancellationToken::new(), 100)
        .await
        .expect("bounded long run should complete");

    assert_eq!(requests.load(Ordering::SeqCst), 51);
    assert_eq!(out.as_deref(), Some("done after checkpoint"));
    let recorded_inputs = inputs.lock().unwrap();
    let Some(Message {
        role: MessageRole::Developer,
        content,
        ..
    }) = recorded_inputs[49].last()
    else {
        panic!("turn 50 should include the progress developer nudge");
    };
    assert!(
        matches!(content.first(), Some(ContentPart::Text { text }) if text.contains("Progress checkpoint")),
        "progress nudge should tell the agent to finalize once enough evidence exists"
    );
    assert_eq!(observer.kinds(), vec!["started", "complete"]);
}

// ---- (8) a hard (non-abort) sampling error propagates out of the loop ------

#[tokio::test]
async fn hard_sampling_error_propagates_and_does_not_complete() {
    // A non-`TurnAborted` error from sampling (e.g. an exhausted-retry provider
    // error) is NOT swallowed: it propagates out of the loop as `Err`. The loop
    // must not emit TurnComplete (the turn did not finish cleanly). This exercises
    // the `Err(other) => return Err(other)` arm and the `SamplingScript::Err`
    // path of the scripted driver.
    let sampler = ScriptedSamplingDriver::new(vec![SamplingScript::Err(AgentError::Provider(
        "stream exhausted".to_string(),
    ))]);
    let requests = sampler.requests_handle();
    let state = InMemoryTurnState::new(Vec::new(), token_status(false));
    let observer = RecordingObserver::new();

    let turn = TurnLoop::new(state, sampler, observer.clone());
    let err = turn
        .run(ctx(), false, CancellationToken::new())
        .await
        .expect_err("a hard sampling error must propagate, not complete");

    assert!(
        matches!(err, AgentError::Provider(_)),
        "the provider error propagates verbatim, got {err:?}"
    );
    assert_eq!(
        requests.load(Ordering::SeqCst),
        1,
        "one request, then it fails"
    );
    // Only TurnStarted was emitted — a hard error neither completes nor aborts.
    assert_eq!(
        observer.kinds(),
        vec!["started"],
        "a hard error emits neither TurnComplete nor TurnAborted"
    );
}
