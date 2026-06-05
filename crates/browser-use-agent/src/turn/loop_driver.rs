//! Unbounded turn-loop driver — the integration spine (codex `turn.rs:214-397`).
//!
//! This is the async driver that ties the merged pieces together. It owns NO
//! control-flow heuristics of its own: every branch routes through the PURE
//! [`decision::classify_loop_step`] core (WP-A1), so the loop is exactly codex's
//! `loop {}` shape with the policy factored out and unit-tested separately.
//!
//! ## Codex parity (`turn.rs:131-400`)
//! The driver reproduces codex's per-iteration sequence verbatim:
//! 1. **read pending input** — gated by `can_drain`. `can_drain` starts as
//!    [`decision::initial_can_drain`]`(turn_has_fresh_input)` (`turn.rs:168`):
//!    fresh user input must be *sampled* first, so we hold the drain on the very
//!    first iteration when there is fresh input; otherwise we may drain
//!    immediately. After the first iteration `can_drain` is always `true` except
//!    immediately after a compaction step, where it is set per the decision's
//!    `can_drain_next` (`turn.rs:306`).
//! 2. **run sampling** — [`SamplingDriver::run_sampling_request`] (WP-B5). One
//!    model round-trip per iteration. (See "Sampling/dispatch composition" below
//!    for why the loop calls sampling directly and does not separately invoke the
//!    [`ToolDispatcher`](super::ToolDispatcher).)
//! 3. **compute the step** — `has_pending_input` is recomputed from
//!    [`TurnState::has_pending_input`], and the whole `(outcome, pending, token
//!    status)` triple is handed to [`decision::classify_loop_step`], which folds
//!    `needs_follow_up = model_needs_follow_up || has_pending_input`
//!    (`turn.rs:255`) and the `token_limit_reached && needs_follow_up`
//!    compaction trigger (`turn.rs:282`) into a single [`decision::LoopStep`].
//! 4. **act on the step**:
//!    - [`LoopStep::CompactThenContinue`](decision::LoopStep::CompactThenContinue)
//!      → run the compaction hook, set `can_drain` to the decision's
//!      `can_drain_next`, and continue (`turn.rs:282-310`).
//!    - [`LoopStep::Continue`](decision::LoopStep::Continue) → set `can_drain =
//!      true` and continue (`turn.rs:250-255`).
//!    - [`LoopStep::Complete`](decision::LoopStep::Complete) → record the last
//!      agent message and break (`turn.rs:340-355`).
//!
//! On a [`AgentError::TurnAborted`](crate::AgentError::TurnAborted) from sampling
//! the loop breaks and returns the message accumulated so far (codex reports the
//! abort via a `TurnAborted` event and returns the interrupted result rather than
//! propagating a hard error; `turn.rs:357`). Any other error propagates.
//!
//! ## No max-turns counter (UNBOUNDED)
//! Codex's turn loop is an unbounded `loop {}` (`turn.rs:214`); there is no
//! iteration cap. We deliberately keep it unbounded — the only termination
//! conditions are `Complete`, cancellation, or a hard error. The loop tests prove
//! a 50-iteration scripted run completes without hitting any cap.
//!
//! ## Sampling/dispatch composition (read B5 + C1)
//! `turn/sampling.rs` ([`SamplingDriver`]) and `turn/dispatch.rs`
//! ([`ToolDispatcher`](super::ToolDispatcher)) are SEPARATE seams. The frozen
//! [`SamplingDriver::run_sampling_request`] returns a
//! [`decision::SamplingOutcome`] whose `model_needs_follow_up` is `true` iff the
//! model emitted ≥1 tool call; it does **not** itself run the tool calls. The
//! design (DESIGN.md "SamplingDriver vs ToolDispatcher boundary") notes codex
//! FUSES the model stream and tool dispatch inside one
//! `try_run_sampling_request`, while this rearchitecture SPLITS them. The frozen
//! `TurnLoop::new(state, sampler, observer)` takes NO dispatcher, and the
//! `SamplingDriver` trait surfaces no tool calls/messages to the loop — so the
//! loop drives **sampling only** and treats `model_needs_follow_up` as the signal
//! that another round-trip is needed. A production `SamplingDriver` therefore owns
//! the fused sampling+dispatch step internally (running the `ToolDispatcher`,
//! recording outputs into the shared [`TurnState`] / `ContextManager`, and
//! reporting follow-up via the outcome). This keeps the loop a thin, pure-policy
//! spine over the frozen traits and the `decision::` core, exactly the shapes
//! WP-C2 owns. If the split ever diverges from codex's drain timing, the design
//! note's escape hatch is to fuse the two traits — the loop control flow here is
//! unaffected.
//!
//! ## Compaction (stubbed body, codex-faithful control flow)
//! The real model-based compaction work package is not built yet. On a
//! `CompactThenContinue` step the loop calls [`TurnState::compact`] — a hook with
//! a default no-op body the tests override — and then sets `can_drain` from the
//! decision's `can_drain_next` (`= !model_needs_follow_up`, `turn.rs:306`). The
//! **control flow** is codex-faithful (compact, set drain, continue); only the
//! compaction body is a stub. When the compaction WP lands it fills
//! `TurnState::compact` (and the production `SamplingDriver` re-runs against the
//! compacted history) with no change to this driver.

use super::{CompactionMode, SamplingDriver, TurnObserver, TurnState};
use crate::decision::{self, LoopStep};
use crate::events::TurnCtx;
use crate::task::{TurnAbortReason, TurnLifecycleEvent};
use browser_use_llm::schema::{ContentPart, Message, MessageRole};
use tokio_util::sync::CancellationToken;

const FINAL_MAX_TURNS_NUDGE: &str = "This is the final allowed step for this run. Stop exploring and call the done tool with the best complete answer you can provide now. Include unknown or unavailable items explicitly instead of continuing to search.";
const PROGRESS_MAX_TURNS_NUDGE: &str = "Progress checkpoint: If you have enough evidence, a saved artifact, or a complete-enough answer, stop further exploration and call the done tool now. Continue only for clearly missing required information that is likely to change the final answer.";

/// The async, unbounded turn-loop driver. Generic over the three frozen turn
/// traits so production wires real impls (`ContextManager`+`Session`,
/// `ModelSamplingDriver`, a `StoreSink`-backed observer) while tests inject
/// deterministic, network-free fakes.
pub struct TurnLoop<St, Sd, Ob> {
    state: St,
    sampler: Sd,
    observer: Ob,
}

impl<St: TurnState, Sd: SamplingDriver, Ob: TurnObserver> TurnLoop<St, Sd, Ob> {
    /// Assemble a loop from its three collaborators. Pure constructor — no I/O.
    pub fn new(state: St, sampler: Sd, observer: Ob) -> Self {
        Self {
            state,
            sampler,
            observer,
        }
    }

    /// Read-only access to the turn state (tests assert recorded history /
    /// pending-input draining through this).
    pub fn state(&self) -> &St {
        &self.state
    }

    /// Run the unbounded driver to completion (`turn.rs:214-397`).
    ///
    /// `turn_has_fresh_input` is codex's "this turn started with new user input"
    /// flag; it seeds the initial drain gate via [`decision::initial_can_drain`].
    /// Returns the last assistant message (`None` if the model produced no text),
    /// or — on cancellation — the message accumulated up to the abort.
    pub async fn run(
        &self,
        ctx: TurnCtx,
        turn_has_fresh_input: bool,
        cancel: CancellationToken,
    ) -> Result<Option<String>, crate::AgentError> {
        self.run_inner(ctx, turn_has_fresh_input, cancel, None)
            .await
    }

    /// Run the driver with an optional sampling-round limit.
    ///
    /// Browser Use's Python API exposes this as `Agent.run(max_steps=...)`.
    /// The default [`run`](Self::run) remains unbounded for Codex parity, while
    /// terminal/browser-use bridge callers can opt into the cap.
    pub async fn run_with_max_turns(
        &self,
        ctx: TurnCtx,
        turn_has_fresh_input: bool,
        cancel: CancellationToken,
        max_turns: usize,
    ) -> Result<Option<String>, crate::AgentError> {
        self.run_inner(ctx, turn_has_fresh_input, cancel, Some(max_turns.max(1)))
            .await
    }

    async fn run_inner(
        &self,
        ctx: TurnCtx,
        turn_has_fresh_input: bool,
        cancel: CancellationToken,
        max_turns: Option<usize>,
    ) -> Result<Option<String>, crate::AgentError> {
        let turn_id = ctx.session_id.clone();
        self.observer.on_lifecycle(TurnLifecycleEvent::TurnStarted {
            turn_id: turn_id.clone(),
        });

        // `turn.rs:168`: fresh input is sampled before any pending steer is
        // drained; with no fresh input we may drain immediately.
        let mut can_drain = decision::initial_can_drain(turn_has_fresh_input);
        let mut last_agent_message: Option<String> = None;
        let mut turns_run = 0usize;

        // Unbounded (`turn.rs:214`): NO max-turns counter. The only exits are
        // Complete, cancellation, or a hard error.
        loop {
            // ---- 1. read pending input (gated by can_drain) ----
            // codex drains queued user/steer items into this turn's input only
            // when the drain gate is open; otherwise this round samples the
            // already-present history and leaves the queue for a later iteration.
            let input = if can_drain {
                self.state.take_pending_input().await
            } else {
                Vec::new()
            };

            // The request body is the prompt history snapshot plus any freshly
            // drained input. The production `TurnState` lowers its
            // `ContextManager` history; the loop simply threads it through.
            let mut request = self.state.clone_history_for_prompt().await;
            request.extend(input);
            let next_turn = turns_run + 1;
            if max_turns.is_some_and(|limit| next_turn == limit) {
                request.push(Message::new(
                    MessageRole::Developer,
                    vec![ContentPart::text(FINAL_MAX_TURNS_NUDGE)],
                ));
            } else if max_turns.is_some_and(|limit| should_emit_progress_nudge(limit, next_turn)) {
                request.push(Message::new(
                    MessageRole::Developer,
                    vec![ContentPart::text(PROGRESS_MAX_TURNS_NUDGE)],
                ));
            }

            // ---- 2. run one sampling round-trip ----
            let outcome = match self
                .sampler
                .run_sampling_request(request, cancel.clone())
                .await
            {
                Ok(out) => out,
                Err(crate::AgentError::TurnAborted) => {
                    // codex reports the abort via an event and returns the
                    // interrupted result rather than a hard error (`turn.rs:357`).
                    self.observer.on_lifecycle(TurnLifecycleEvent::TurnAborted {
                        turn_id,
                        reason: TurnAbortReason::Interrupted,
                    });
                    return Ok(last_agent_message);
                }
                Err(other) => return Err(other),
            };
            turns_run += 1;

            // Carry the latest assistant text forward (codex keeps the last
            // non-empty agent message as the turn result; `turn.rs:340`).
            if outcome.last_agent_message.is_some() {
                last_agent_message = outcome.last_agent_message.clone();
            }
            if outcome.defers_mailbox_delivery_to_next_turn && !outcome.model_needs_follow_up {
                self.state.defer_mailbox_delivery_to_next_turn().await;
            }

            // ---- 3. classify the step via the PURE decision core ----
            let has_pending_input = self.state.has_pending_input().await;
            let token_status = self.state.token_status().await;
            let step = decision::classify_loop_step(&outcome, has_pending_input, &token_status);

            // ---- 4. act on the step (codex `turn.rs:250-355`) ----
            match step {
                LoopStep::Complete => {
                    // Terminal: no follow-up needed and no compaction. Record the
                    // final agent message and break (`turn.rs:340-355`).
                    self.observer
                        .on_lifecycle(TurnLifecycleEvent::TurnComplete {
                            turn_id,
                            last_agent_message: last_agent_message.clone(),
                        });
                    return Ok(last_agent_message);
                }
                _ if max_turns.is_some_and(|limit| turns_run >= limit) => {
                    self.observer.on_lifecycle(TurnLifecycleEvent::TurnAborted {
                        turn_id,
                        reason: TurnAbortReason::Interrupted,
                    });
                    return Ok(last_agent_message);
                }
                LoopStep::CompactThenContinue { can_drain_next } => {
                    // Compact, then continue. The compaction BODY is a stub hook
                    // (real model-based compaction WP pending); the CONTROL FLOW
                    // is codex-faithful: compact (`turn.rs:282`), set the drain
                    // gate from the decision (`turn.rs:306`), loop again.
                    self.state.compact(CompactionMode::MidTurn).await?;
                    can_drain = can_drain_next;
                }
                LoopStep::Continue => {
                    // Another round-trip is needed (model wants follow-up and/or
                    // there is pending input). Past the first iteration the drain
                    // gate is always open (`turn.rs:250-255`).
                    can_drain = true;
                }
            }
        }
    }
}

fn should_emit_progress_nudge(max_turns: usize, next_turn: usize) -> bool {
    if max_turns < 40 || next_turn >= max_turns {
        return false;
    }
    next_turn >= max_turns / 2 && next_turn % 10 == 0
}
