//! Integration proof that **automatic context compaction fires at the real
//! ~90%-of-window token threshold**, then the turn CONTINUES with reduced
//! context — driven end-to-end through the production [`TurnLoop`] +
//! [`CompactingTurnState`] + model-based [`run_compaction`].
//!
//! ## Why this is not a tautology
//! The sibling `tests.rs` end-to-end case hands the loop a hand-set
//! `token_status(true)` (a hardcoded "limit reached" bool). That proves the
//! *control flow* but NOT that the trigger fires at the production threshold.
//!
//! Here, the loop's [`TurnState::token_status`] is computed by the **production**
//! [`decision::TokenStatus::from_estimate`] (the codex/legacy 90%-of-window
//! auto-compact math, `decision/loop_decision.rs`) applied to the **live**
//! [`ContextManager::estimate_total_tokens`] of the SAME shared history the
//! turn loop samples. Nothing hardcodes the trigger bool: it is derived, every
//! iteration, from the real token estimate vs. the real 90% limit. The window
//! is chosen from that real estimate so the seeded history lands just over 90%;
//! a control case sizes the window so the identical history sits under 90% and
//! proves NO compaction occurs. So the boundary itself is what is under test.
//!
//! ## What is real vs. simulated
//! - REAL: the 90% threshold function (`from_estimate`), the trigger gate
//!   (`classify_loop_step` → `CompactThenContinue`), the loop sequencing
//!   (`TurnLoop::run`), the model-based summary pass plumbing
//!   (`CompactingTurnState::compact` → `run_compaction`), the shared
//!   `ContextManager`, and its `estimate_total_tokens` byte→token math.
//! - SIMULATED (network-free, deterministic): the *model responses*. The loop's
//!   sampling round-trips come from a scripted [`LoopSampler`] and the
//!   compaction summary comes from a scripted [`ScriptedSummarySampler`]. There
//!   is no socket, timer, or live model. Token "usage" is not injected as a raw
//!   number — it is the genuine serialized-byte estimate of the seeded history.
//!
//! Honesty caveat: this proves the *engine's* auto-compaction trigger and
//! continuation. It does not exercise a live provider; the model text is canned.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use browser_use_llm::schema::Message;
use serde_json::json;
use tokio_util::sync::CancellationToken;

use crate::compact::{
    is_summary_message, CompactingTurnState, CompactionSampler, CompactionSummary, SUMMARY_PREFIX,
};
use crate::context::assembly::TruncationPolicy;
use crate::context::{ContextManager, Item};
use crate::decision::{SamplingOutcome, TokenStatus};
use crate::events::TurnCtx;
use crate::task::TurnLifecycleEvent;
use crate::turn::{SamplingDriver, TurnLoop, TurnObserver, TurnState};
use crate::AgentError;

// ---------------------------------------------------------------------------
// scripted, network-free model doubles
// ---------------------------------------------------------------------------

/// Scripted compaction summary pass (the ONLY model interaction inside
/// `run_compaction`). Records how many times it ran so a test can prove the
/// summary round-trip happened exactly when the threshold tripped.
struct ScriptedSummarySampler {
    summary: String,
    calls: AtomicUsize,
}

impl ScriptedSummarySampler {
    fn new(summary: &str) -> Arc<Self> {
        Arc::new(Self {
            summary: summary.to_string(),
            calls: AtomicUsize::new(0),
        })
    }
    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

impl CompactionSampler for ScriptedSummarySampler {
    async fn summarize(
        &self,
        _request: Vec<Message>,
        _cancel: CancellationToken,
    ) -> Result<CompactionSummary, AgentError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(CompactionSummary::text(self.summary.clone()))
    }
}

/// Scripted loop sampling driver: replays a queue of [`SamplingOutcome`]s (the
/// loop's per-iteration model round-trips, distinct from the summary pass) and
/// counts requests.
struct LoopSampler {
    scripts: Mutex<VecDeque<SamplingOutcome>>,
    requests: Arc<AtomicUsize>,
}

impl LoopSampler {
    fn new(scripts: Vec<SamplingOutcome>) -> Self {
        Self {
            scripts: Mutex::new(scripts.into_iter().collect()),
            requests: Arc::new(AtomicUsize::new(0)),
        }
    }
    fn requests_handle(&self) -> Arc<AtomicUsize> {
        self.requests.clone()
    }
}

impl SamplingDriver for LoopSampler {
    async fn run_sampling_request(
        &self,
        _input: Vec<Message>,
        cancel: CancellationToken,
    ) -> Result<SamplingOutcome, AgentError> {
        if cancel.is_cancelled() {
            return Err(AgentError::TurnAborted);
        }
        self.requests.fetch_add(1, Ordering::SeqCst);
        Ok(self
            .scripts
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| complete("end")))
    }
}

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

// ---------------------------------------------------------------------------
// the production-threshold-driven turn state
// ---------------------------------------------------------------------------

/// A [`TurnState`] that delegates EVERY operation to a real
/// [`CompactingTurnState`] but computes [`token_status`](TurnState::token_status)
/// from the **production** [`TokenStatus::from_estimate`] applied to the **live**
/// shared [`ContextManager`]'s real `estimate_total_tokens()`.
///
/// This is the load-bearing seam of the test: the loop's compaction trigger is
/// recomputed each iteration from the genuine token estimate of the genuine
/// history vs. the genuine 90% limit — so when the loop decides to compact, it
/// is the production threshold logic that decided, and once `compact()` shrinks
/// the shared history the recomputed estimate drops back under 90% on its own
/// (no hand-flipped flag), letting the turn CONTINUE to completion.
struct ThresholdDrivenState<S: CompactionSampler> {
    inner: CompactingTurnState<S>,
    /// The model context-window budget the production 90% math runs against.
    context_window: i64,
    /// Recorded `(estimated_tokens, token_limit_reached)` per `token_status`
    /// read, so the test can prove the boundary crossed exactly as expected.
    samples: Mutex<Vec<(i64, bool)>>,
}

impl<S: CompactionSampler> ThresholdDrivenState<S> {
    fn new(inner: CompactingTurnState<S>, context_window: i64) -> Self {
        Self {
            inner,
            context_window,
            samples: Mutex::new(Vec::new()),
        }
    }

    fn context(&self) -> Arc<Mutex<ContextManager>> {
        self.inner.context()
    }

    fn samples(&self) -> Vec<(i64, bool)> {
        self.samples.lock().unwrap().clone()
    }
}

impl<S: CompactionSampler + 'static> TurnState for ThresholdDrivenState<S> {
    async fn clone_history_for_prompt(&self) -> Vec<Message> {
        self.inner.clone_history_for_prompt().await
    }

    async fn record_items(&self, items: &[Message]) {
        self.inner.record_items(items).await
    }

    async fn has_pending_input(&self) -> bool {
        self.inner.has_pending_input().await
    }

    async fn take_pending_input(&self) -> Vec<Message> {
        self.inner.take_pending_input().await
    }

    async fn token_status(&self) -> TokenStatus {
        // REAL token estimate of the REAL live history the loop just sampled.
        let estimated = self.context().lock().unwrap().estimate_total_tokens();
        // REAL production 90%-of-window auto-compact math.
        let status = TokenStatus::from_estimate(estimated, self.context_window);
        self.samples
            .lock()
            .unwrap()
            .push((estimated, status.token_limit_reached));
        status
    }

    async fn compact(&self, mode: crate::turn::CompactionMode) -> Result<(), crate::AgentError> {
        self.inner.compact(mode).await
    }
}

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

fn user_item(text: &str) -> Item {
    json!({ "role": "user", "content": [{ "type": "text", "text": text }] })
}

fn assistant_item(text: &str) -> Item {
    json!({ "role": "assistant", "content": [{ "type": "text", "text": text }] })
}

fn item_role(item: &Item) -> String {
    item.get("role")
        .and_then(|r| r.as_str())
        .unwrap_or_default()
        .to_string()
}

fn ctx() -> TurnCtx {
    TurnCtx {
        session_id: "sess-threshold".to_string(),
        model: "gpt-5-codex".to_string(),
        provider: "openai".to_string(),
        base_instructions: crate::prompts::browser_agent_system_prompt(),
        browser_mode_instruction: None,
        turn_idx: 0,
        attempt: 0,
    }
}

fn follow_up(msg: &str) -> SamplingOutcome {
    SamplingOutcome {
        model_needs_follow_up: true,
        last_agent_message: Some(msg.to_string()),
        defers_mailbox_delivery_to_next_turn: false,
        finish_reason: None,
    }
}

fn complete(msg: &str) -> SamplingOutcome {
    SamplingOutcome {
        model_needs_follow_up: false,
        last_agent_message: Some(msg.to_string()),
        defers_mailbox_delivery_to_next_turn: true,
        finish_reason: None,
    }
}

/// Seed a real [`ContextManager`] with `n` user/assistant turn pairs of
/// substantial text and return it plus its real token estimate.
fn seed_history(n: usize) -> (ContextManager, i64) {
    let mut manager = ContextManager::new();
    let mut seed: Vec<Item> = Vec::new();
    for i in 0..n {
        seed.push(user_item(&format!(
            "user turn {i}: {}",
            "context that accumulates and consumes the window ".repeat(8)
        )));
        seed.push(assistant_item(&format!(
            "assistant turn {i}: {}",
            "a long reply that also consumes tokens over the conversation ".repeat(8)
        )));
    }
    manager.record_items(seed, TruncationPolicy::Bytes(usize::MAX));
    let estimate = manager.estimate_total_tokens();
    (manager, estimate)
}

/// The production auto-compact limit for `window`, recomputed exactly like
/// `decision::TokenStatus::from_estimate` does (`(window * 9) / 10`).
fn production_auto_compact_limit(window: i64) -> i64 {
    TokenStatus::from_estimate(window, window).auto_compact_scope_limit
}

// ---------------------------------------------------------------------------
// (A) AT the production threshold: compaction fires + turn continues
// ---------------------------------------------------------------------------

#[tokio::test]
async fn auto_compaction_fires_at_production_threshold_then_turn_continues() {
    // 1. A real history with a real token estimate.
    let (manager, estimate) = seed_history(12);
    let original_len = manager.items().len();
    assert!(estimate > 0, "seeded history has a real token footprint");
    assert!(original_len >= 2, "seeded history has many items");

    // 2. Pick the context window so the REAL estimate lands JUST OVER the real
    //    90% limit: window = floor(estimate / 0.9) makes limit = (window*0.9) <=
    //    estimate, i.e. token_limit_reached == true at exactly this estimate.
    //    The window is derived from the production fraction, not hardcoded.
    let window = ((estimate as f64) / 0.9).floor() as i64;
    let limit = production_auto_compact_limit(window);
    assert!(
        estimate >= limit,
        "precondition: estimate {estimate} >= 90% limit {limit} (over threshold)"
    );
    assert!(
        estimate < window,
        "precondition: estimate {estimate} < window {window} (not at the hard ceiling — \
         we are exercising the SOFT 90% auto-compact trigger)"
    );

    // 3. Wire the REAL CompactingTurnState (shared ContextManager + model-based
    //    summary sampler), wrapped so the loop's token_status comes from the
    //    PRODUCTION from_estimate over the live history. `without_pressure_relief`
    //    is NOT used: we rely on the real post-compaction estimate dropping back
    //    under 90% to end the loop, proving genuine continuation.
    let summary = ScriptedSummarySampler::new("auto-compaction handoff summary");
    let ctx_handle = Arc::new(Mutex::new(manager));
    let compacting = CompactingTurnState::new(
        ctx_handle.clone(),
        summary.clone(),
        crate::compact::COMPACT_USER_MESSAGE_MAX_TOKENS,
        Vec::new(),
        // Seed status is irrelevant: ThresholdDrivenState recomputes it from the
        // live estimate every read. Start it "clear" to avoid pre-arming.
        TokenStatus::default(),
    )
    .without_pressure_relief();
    let state = ThresholdDrivenState::new(compacting, window);

    // 4. Loop round-trips: iter 1 the model wants follow-up → with the threshold
    //    crossed this is CompactThenContinue (compaction fires); iter 2 completes
    //    (the shrunken history is now under 90%).
    let loop_sampler = LoopSampler::new(vec![
        follow_up("model wants to continue, and we are over budget"),
        complete("done after auto-compaction"),
    ]);
    let requests = loop_sampler.requests_handle();
    let observer = RecordingObserver::new();

    let turn = TurnLoop::new(state, loop_sampler, observer.clone());
    let out = turn
        .run(ctx(), false, CancellationToken::new())
        .await
        .expect("loop completes after auto-compaction relieves the threshold");

    // --- assertions -------------------------------------------------------

    // (a) Compaction FIRED: the model-based summary pass ran exactly once,
    //     driven by the production threshold (not a hardcoded flag).
    assert_eq!(
        summary.calls(),
        1,
        "compaction's model summary pass ran exactly once after the 90% trigger"
    );

    // (b) The trigger was decided by the REAL threshold function: the FIRST
    //     token_status read (pre-compaction) was over the limit; a LATER read
    //     (post-compaction) is back under it. We never set the bool by hand.
    let samples = turn.state().samples();
    assert!(
        samples.len() >= 2,
        "token_status was read across the compaction boundary"
    );
    let (first_estimate, first_over) = samples[0];
    assert!(
        first_over,
        "the production from_estimate flagged the seeded history ({first_estimate} tokens) \
         as over the 90% limit ({limit})"
    );
    let (last_estimate, last_over) = *samples.last().unwrap();
    assert!(
        !last_over,
        "after compaction the production from_estimate is back under the 90% limit \
         (estimate dropped {first_estimate} -> {last_estimate})"
    );
    assert!(
        last_estimate < first_estimate,
        "the live token estimate genuinely dropped post-compaction \
         ({last_estimate} < {first_estimate})"
    );

    // (c) The turn CONTINUED to completion with REDUCED context.
    assert_eq!(
        requests.load(Ordering::SeqCst),
        2,
        "the compaction step continued the loop into a second round-trip"
    );
    assert_eq!(out.as_deref(), Some("done after auto-compaction"));
    assert_eq!(observer.kinds(), vec!["started", "complete"]);

    // (d) The shared history (what the next turn would sample) shrank to the
    //     codex-parity [preserved real users + summary] shape.
    let compacted_items = ctx_handle.lock().unwrap().items().to_vec();
    assert!(
        compacted_items.len() < original_len,
        "history shrank: {} < {}",
        compacted_items.len(),
        original_len
    );
    assert!(
        compacted_items.iter().all(|it| item_role(it) == "user"),
        "compacted history is only preserved user messages + the summary"
    );
    let last_text = compacted_items
        .last()
        .and_then(|it| it.get("content"))
        .and_then(|c| c.as_array())
        .and_then(|a| a.first())
        .and_then(|p| p.get("text"))
        .and_then(|t| t.as_str())
        .unwrap_or_default()
        .to_string();
    assert!(
        is_summary_message(&last_text),
        "the compacted history ends with the prefixed handoff summary"
    );
    assert_eq!(
        last_text,
        format!("{SUMMARY_PREFIX}\nauto-compaction handoff summary"),
        "summary carries the production prefix + the model's summary text"
    );
}

// ---------------------------------------------------------------------------
// (B) BELOW the production threshold: NO compaction (control)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn no_compaction_below_production_threshold() {
    // Identical history; but size the window so the SAME real estimate sits
    // comfortably under the real 90% limit. The production from_estimate must
    // therefore never flag the limit, the summary pass must never run, and the
    // history must be left intact even though the model asks to continue.
    let (manager, estimate) = seed_history(12);
    let original_len = manager.items().len();

    // window large enough that 90% of it strictly exceeds the estimate:
    //   limit = (window * 0.9) > estimate  <=>  window > estimate / 0.9.
    let window = ((estimate as f64) / 0.9).ceil() as i64 + 10_000;
    let limit = production_auto_compact_limit(window);
    assert!(
        estimate < limit,
        "precondition: estimate {estimate} < 90% limit {limit} (under threshold)"
    );

    let summary = ScriptedSummarySampler::new("should-never-be-produced");
    let ctx_handle = Arc::new(Mutex::new(manager));
    let compacting = CompactingTurnState::new(
        ctx_handle.clone(),
        summary.clone(),
        crate::compact::COMPACT_USER_MESSAGE_MAX_TOKENS,
        Vec::new(),
        TokenStatus::default(),
    )
    .without_pressure_relief();
    let state = ThresholdDrivenState::new(compacting, window);

    // The model asks to continue once, then completes. Under threshold, the
    // first step is a plain Continue (NOT CompactThenContinue).
    let loop_sampler = LoopSampler::new(vec![
        follow_up("continue, but we are under budget"),
        complete("done without compaction"),
    ]);
    let requests = loop_sampler.requests_handle();
    let observer = RecordingObserver::new();

    let turn = TurnLoop::new(state, loop_sampler, observer.clone());
    let out = turn
        .run(ctx(), false, CancellationToken::new())
        .await
        .expect("loop completes with no compaction");

    // No compaction occurred.
    assert_eq!(
        summary.calls(),
        0,
        "no summary pass below the 90% threshold"
    );
    // The production threshold function never flagged the limit.
    let samples = turn.state().samples();
    assert!(!samples.is_empty(), "token_status was read");
    assert!(
        samples.iter().all(|(_, over)| !*over),
        "the production from_estimate never crossed 90% for this window"
    );
    // History untouched; turn still completed via plain follow-up/continue.
    let after = ctx_handle.lock().unwrap().items().to_vec();
    assert_eq!(
        after.len(),
        original_len,
        "history is unchanged when below the threshold"
    );
    assert_eq!(requests.load(Ordering::SeqCst), 2);
    assert_eq!(out.as_deref(), Some("done without compaction"));
    assert_eq!(observer.kinds(), vec!["started", "complete"]);
}
