//! Network-free unit tests for the goals subsystem.
//!
//! Covers: event-sourcing (reduce/replay determinism, lifecycle transitions),
//! the budget formula `max(input - cached, 0) + max(output, 0)` with clamping and
//! boundaries, REUSE of the shared `context/accounting.rs` byte->token heuristic,
//! steering emission (goal-set + threshold crossings, fire-once), and the
//! `goal_context` message wire shape.

use std::sync::Arc;
use std::sync::Mutex;

use browser_use_llm::schema::Usage;
use serde_json::json;

use crate::events::EventSink;
use crate::events::PendingEvent;

use super::budget;
use super::budget::BudgetLevel;
use super::budget::BudgetState;
use super::state;
use super::state::status;
use super::state::GoalEvent;
use super::state::GoalState;
use super::steering;
use super::GoalManager;

// --- test recording sink ---------------------------------------------------

#[derive(Default)]
struct RecordingSink {
    events: Mutex<Vec<PendingEvent>>,
}

impl EventSink for RecordingSink {
    fn emit(&self, ev: PendingEvent) {
        self.events.lock().unwrap().push(ev);
    }
}

impl RecordingSink {
    fn types(&self) -> Vec<String> {
        self.events
            .lock()
            .unwrap()
            .iter()
            .map(|e| e.event_type.clone())
            .collect()
    }
    fn len(&self) -> usize {
        self.events.lock().unwrap().len()
    }
}

fn usage(input: u64, cached: u64, output: u64) -> Usage {
    Usage {
        input_tokens: input,
        cached_input_tokens: cached,
        cache_creation_input_tokens: 0,
        output_tokens: output,
        reasoning_output_tokens: 0,
        total_tokens: 0,
    }
}

// --- event-sourcing --------------------------------------------------------

#[test]
fn reduce_sequence_produces_expected_state() {
    let events = vec![
        GoalEvent::Set {
            goal_id: "g1".into(),
            text: "ship the feature".into(),
            status: None,
            token_budget: Some(1000),
            turn_idx: Some(3),
        },
        GoalEvent::Accounted {
            tokens_used: 120,
            time_used_seconds: 4,
        },
        GoalEvent::Accounted {
            tokens_used: 80,
            time_used_seconds: 9,
        },
    ];
    let s = state::replay(&events);
    assert_eq!(s.goal_id.as_deref(), Some("g1"));
    assert_eq!(s.text.as_deref(), Some("ship the feature"));
    assert_eq!(s.status.as_deref(), Some(status::ACTIVE));
    assert_eq!(s.token_budget, Some(1000));
    assert_eq!(s.tokens_used, 200); // 120 + 80, accumulated
    assert_eq!(s.created_turn_idx, Some(3));
    // Elapsed seconds ACCUMULATE across responses (4 + 9), matching the legacy
    // `goal.accounted` fold (`time_used_seconds += delta`). This previously
    // ASSIGNED the latest delta (would leave 9), dropping prior elapsed time.
    assert_eq!(s.time_used_seconds, 13);
    assert!(s.is_active());
}

#[test]
fn accounted_accumulates_elapsed_time_not_assigns() {
    // Regression guard for the reducer fix: two `Accounted` events must SUM their
    // elapsed seconds (and tokens), not overwrite with the latest delta. Legacy
    // `goal.accounted` folds `time_used_seconds += delta`
    // (`browser-use-core/src/goals.rs:110-131`).
    let sink = Arc::new(RecordingSink::default());
    let mut mgr = GoalManager::new("s", sink);
    mgr.set_goal("g", "t", None, None);
    mgr.record_usage(&usage(100, 0, 20), 5);
    mgr.record_usage(&usage(100, 0, 20), 7);
    assert_eq!(mgr.state().tokens_used, 240); // 120 + 120
    assert_eq!(mgr.state().time_used_seconds, 12); // 5 + 7, accumulated
}

#[test]
fn accounted_before_or_after_terminal_goal_is_ignored() {
    let events = vec![
        GoalEvent::Accounted {
            tokens_used: 10,
            time_used_seconds: 1,
        },
        GoalEvent::Set {
            goal_id: "g".into(),
            text: "t".into(),
            status: None,
            token_budget: None,
            turn_idx: None,
        },
        GoalEvent::Completed,
        GoalEvent::Accounted {
            tokens_used: 20,
            time_used_seconds: 2,
        },
    ];
    let s = state::replay(&events);
    assert_eq!(s.status.as_deref(), Some(status::COMPLETE));
    assert_eq!(s.tokens_used, 0);
    assert_eq!(s.time_used_seconds, 0);
}

#[test]
fn replay_is_deterministic() {
    let events = vec![
        GoalEvent::Set {
            goal_id: "g".into(),
            text: "t".into(),
            status: None,
            token_budget: Some(50),
            turn_idx: None,
        },
        GoalEvent::Accounted {
            tokens_used: 10,
            time_used_seconds: 1,
        },
        GoalEvent::Updated {
            status: Some(status::BLOCKED.into()),
            text: None,
            token_budget: None,
        },
    ];
    let a = state::replay(&events);
    let b = state::replay(&events);
    assert_eq!(a, b, "same events must fold to the same state");
}

#[test]
fn clear_resets_to_default() {
    let events = vec![
        GoalEvent::Set {
            goal_id: "g".into(),
            text: "t".into(),
            status: None,
            token_budget: Some(100),
            turn_idx: Some(1),
        },
        GoalEvent::Accounted {
            tokens_used: 30,
            time_used_seconds: 2,
        },
        GoalEvent::Cleared,
    ];
    let s = state::replay(&events);
    assert_eq!(s, GoalState::default());
    assert!(!s.is_active());
}

#[test]
fn complete_transitions_to_terminal_and_inactive() {
    let events = vec![
        GoalEvent::Set {
            goal_id: "g".into(),
            text: "t".into(),
            status: None,
            token_budget: None,
            turn_idx: None,
        },
        GoalEvent::Completed,
    ];
    let s = state::replay(&events);
    assert_eq!(s.status.as_deref(), Some(status::COMPLETE));
    assert!(!s.is_active(), "complete is terminal");
}

#[test]
fn updated_overwrites_present_fields_only() {
    let mut s = state::reduce(
        GoalState::default(),
        &GoalEvent::Set {
            goal_id: "g".into(),
            text: "orig".into(),
            status: None,
            token_budget: Some(10),
            turn_idx: None,
        },
    );
    s = state::reduce(
        s,
        &GoalEvent::Updated {
            status: None,
            text: Some("new".into()),
            token_budget: None,
        },
    );
    assert_eq!(s.text.as_deref(), Some("new"));
    assert_eq!(s.token_budget, Some(10)); // untouched
    assert_eq!(s.status.as_deref(), Some(status::ACTIVE)); // untouched
}

// --- budget formula --------------------------------------------------------

// Formula: max(input - cached, 0) + max(output, 0). FULL PARITY with codex
// `core/src/goals.rs:1684-1688` (`non_cached_input() + output.max(0)`) and
// legacy `browser-use-core/src/goals.rs:330-334`
// (`input - cached_input + max(output, 0)`, clamped).
#[test]
fn budget_account_adds_non_cached_input_plus_max_output() {
    let mut b = BudgetState::new(Some(10_000));
    // input 300 (of which 100 cached), output 200 -> (300-100) + 200 = 400.
    let added = b.account(&usage(300, 100, 200));
    assert_eq!(added, 400);
    assert_eq!(b.total_accounted(), 400);
}

#[test]
fn budget_clamps_negative_and_zero_output() {
    // Negative output delta clamps to 0: max(400-0, 0) + max(-50, 0) = 400. The
    // `Usage` wire type carries `u64` so a negative output can only arise from an
    // already-computed `i64` delta; the raw helper is where the clamp lives.
    assert_eq!(budget::tokens_from_usage(400, 0, -50), 400);
    // Zero output: max(400-0, 0) + 0 = 400.
    assert_eq!(budget::tokens_from_usage(400, 0, 0), 400);
    let mut b = BudgetState::new(None);
    b.account(&usage(400, 0, 0));
    assert_eq!(b.total_accounted(), 400);
}

#[test]
fn budget_subtracts_cached_input() {
    // Cached input IS subtracted (parity with codex `non_cached_input()` /
    // legacy `input - cached_input_tokens`).
    // Required example: input=100, cached=40, output=10 -> (100-40)+10 = 70.
    assert_eq!(budget::tokens_from_llm_usage(&usage(100, 40, 10)), 70);
    let mut b = BudgetState::new(None);
    b.account(&usage(100, 40, 10));
    assert_eq!(b.total_accounted(), 70);
    // Fully-cached input bills only output: input=500, cached=500, output=0 -> 0.
    assert_eq!(budget::tokens_from_llm_usage(&usage(500, 500, 0)), 0);
}

#[test]
fn budget_clamps_non_cached_term_when_cached_exceeds_input() {
    // Degenerate `cached > input`: non-cached term clamps to 0 (matching legacy's
    // outer `.max(0)`), so only `max(output, 0)` is billed.
    // input=40, cached=100, output=10 -> max(40-100, 0) + 10 = 0 + 10 = 10.
    assert_eq!(budget::tokens_from_usage(40, 100, 10), 10);
    assert_eq!(budget::tokens_from_llm_usage(&usage(40, 100, 10)), 10);
    // And with zero output the whole delta is 0, never negative.
    assert_eq!(budget::tokens_from_usage(40, 100, 0), 0);
}

#[test]
fn budget_remaining_and_exhausted_boundaries() {
    let mut b = BudgetState::new(Some(1000));
    b.account_tokens(999);
    assert_eq!(b.remaining(), Some(1)); // saturating_sub then max(0)
    assert!(!b.is_exhausted());
    // exactly at budget => exhausted (legacy `tokens_used >= budget`).
    b.account_tokens(1);
    assert_eq!(b.total_accounted(), 1000);
    assert_eq!(b.remaining(), Some(0));
    assert!(b.is_exhausted());
    // over budget stays remaining 0 (clamped).
    b.account_tokens(500);
    assert_eq!(b.remaining(), Some(0));
    assert!(b.is_exhausted());
}

#[test]
fn budget_unlimited_never_exhausts() {
    let mut b = BudgetState::new(None);
    b.account_tokens(1_000_000);
    assert_eq!(b.remaining(), None);
    assert!(!b.is_exhausted());
    assert!(!b.is_warning());
    assert_eq!(b.level(), BudgetLevel::Ok);
}

#[test]
fn budget_warn_threshold_and_levels() {
    // 80% of 1000 = 800.
    assert_eq!(
        budget::warn_threshold(1000, budget::DEFAULT_WARN_FRACTION),
        800
    );
    let mut b = BudgetState::new(Some(1000));
    b.account_tokens(799);
    assert_eq!(b.level(), BudgetLevel::Ok);
    b.account_tokens(1); // 800
    assert_eq!(b.level(), BudgetLevel::Warn);
    assert!(b.is_warning());
    b.account_tokens(200); // 1000
    assert_eq!(b.level(), BudgetLevel::Exhausted);
    assert!(!b.is_warning(), "exhausted is not warn");
}

// --- budget reuse of context/accounting.rs ---------------------------------

// The byte->token heuristic must come from `context/accounting.rs`
// (`(b + 3) / 4`), NOT a private copy here.
#[test]
fn budget_reuses_shared_byte_to_token_heuristic() {
    // Re-exported from context::accounting via budget.
    assert_eq!(
        budget::approx_tokens_from_byte_count_i64(10),
        crate::context::accounting::approx_tokens_from_byte_count_i64(10),
    );
    // Known mapping: 10 bytes -> ceil(10/4) = 3 via the shared `(b + 3) / 4`.
    assert_eq!(budget::approx_tokens_from_byte_count_i64(10), 3);
    // 8 bytes -> exactly 2; 9 bytes -> 3 (ceiling).
    assert_eq!(budget::approx_tokens_from_byte_count_i64(8), 2);
    assert_eq!(budget::approx_tokens_from_byte_count_i64(9), 3);
    // non-positive clamps to 0.
    assert_eq!(budget::approx_tokens_from_byte_count_i64(0), 0);
    assert_eq!(budget::approx_tokens_from_byte_count_i64(-4), 0);
}

// --- steering: goal context message wire shape -----------------------------

// Envelope parity: legacy `goal_context_message`
// (`browser-use-core/src/lib.rs:9796-9805`). Body is a documented simplification
// (see steering.rs module header).
#[test]
fn goal_context_message_wire_shape() {
    let s = state::reduce(
        GoalState::default(),
        &GoalEvent::Set {
            goal_id: "g".into(),
            text: "do the thing".into(),
            status: None,
            token_budget: Some(1000),
            turn_idx: None,
        },
    );
    let s = state::reduce(
        s,
        &GoalEvent::Accounted {
            tokens_used: 250,
            time_used_seconds: 0,
        },
    );
    let msg = steering::render_goal_context_message(&s).expect("active goal => message");
    // Envelope matches legacy `goal_context_message` (lib.rs:9796-9805) and the
    // agent's `context::inject::build_context_message` (inject.rs:262-270): NO
    // `"type":"message"` key.
    assert_eq!(msg["role"], json!("user"));
    assert_eq!(msg["name"], json!("goal_context"));
    assert_eq!(msg["content"][0]["type"], json!("input_text"));
    let body = msg["content"][0]["text"].as_str().expect("goal text");
    assert!(body.starts_with("<goal_context>\n"));
    assert!(body.ends_with("\n</goal_context>"));
    assert!(body.contains("Continue working toward the active thread goal."));
    assert!(body.contains("<objective>\ndo the thing\n</objective>"));
    assert!(body.contains("- Tokens used: 250"));
    assert!(body.contains("- Tokens remaining: 750"));
    assert!(body.contains("Completion audit:"));
    assert!(body.contains("Blocked audit:"));
}

#[test]
fn no_goal_context_message_when_inactive() {
    assert!(steering::render_goal_context_message(&GoalState::default()).is_none());
    let done = state::replay(&[
        GoalEvent::Set {
            goal_id: "g".into(),
            text: "t".into(),
            status: None,
            token_budget: None,
            turn_idx: None,
        },
        GoalEvent::Completed,
    ]);
    assert!(steering::render_goal_context_message(&done).is_none());
}

// --- steering: events through the sink -------------------------------------

#[test]
fn set_goal_emits_goal_set_event() {
    let sink = Arc::new(RecordingSink::default());
    let mut mgr = GoalManager::new("sess-1", sink.clone());
    let emitted = mgr.set_goal("g1", "ship it", Some(1000), Some(0));
    assert_eq!(emitted.len(), 1);
    assert_eq!(emitted[0].event_type, steering::GOAL_SET_EVENT);
    assert_eq!(emitted[0].session_id, "sess-1");
    assert_eq!(sink.types(), vec![steering::GOAL_SET_EVENT.to_string()]);
    // The manager exposes a renderable context message now.
    assert!(mgr.goal_context_message().is_some());
}

#[test]
fn crossing_warn_then_exhaust_fires_each_once() {
    let sink = Arc::new(RecordingSink::default());
    let mut mgr = GoalManager::new("s", sink.clone());
    mgr.set_goal("g", "t", Some(1000), None); // 1 goal.created event so far
    assert_eq!(sink.len(), 1);

    // Below warn: 700 tokens (input 700, output 0) -> no steering.
    mgr.record_usage(&usage(700, 0, 0), 0);
    assert_eq!(sink.len(), 1, "below threshold emits nothing");

    // Cross into warn band: +120 -> 820 (>= 800) -> one warning.
    mgr.record_usage(&usage(120, 0, 0), 0);
    assert_eq!(sink.len(), 2);
    assert_eq!(
        sink.types().last().unwrap(),
        steering::GOAL_BUDGET_WARNING_EVENT
    );

    // Stay in warn band: +50 -> 870, still warn -> no new event.
    mgr.record_usage(&usage(50, 0, 0), 0);
    assert_eq!(sink.len(), 2, "re-warn at same level is silent");

    // Cross into exhausted: +200 -> 1070 (>= 1000) -> one limit event.
    mgr.record_usage(&usage(200, 0, 0), 0);
    assert_eq!(sink.len(), 3);
    assert_eq!(
        sink.types().last().unwrap(),
        steering::GOAL_BUDGET_LIMIT_STEERING_EVENT
    );

    // Stay exhausted: +500 -> no new event.
    mgr.record_usage(&usage(500, 0, 0), 0);
    assert_eq!(sink.len(), 3, "re-exhaust at same level is silent");
}

#[test]
fn jump_straight_to_exhausted_emits_only_limit_event() {
    let sink = Arc::new(RecordingSink::default());
    let mut mgr = GoalManager::new("s", sink.clone());
    mgr.set_goal("g", "t", Some(1000), None);
    // One big response that blows past both warn and hard budget.
    mgr.record_usage(&usage(5000, 0, 0), 0);
    // goal.created + the single limit event (NOT a separate warn).
    assert_eq!(
        sink.types(),
        vec![
            steering::GOAL_SET_EVENT.to_string(),
            steering::GOAL_BUDGET_LIMIT_STEERING_EVENT.to_string(),
        ]
    );
}

#[test]
fn record_usage_is_noop_without_active_goal() {
    let sink = Arc::new(RecordingSink::default());
    let mut mgr = GoalManager::new("s", sink.clone());
    let emitted = mgr.record_usage(&usage(1000, 0, 1000), 5);
    assert!(emitted.is_empty());
    assert_eq!(sink.len(), 0);
    assert_eq!(mgr.budget().total_accounted(), 0);
    assert!(mgr.events().is_empty());
}

#[test]
fn manager_budget_tracks_folded_tokens_used() {
    let sink = Arc::new(RecordingSink::default());
    let mut mgr = GoalManager::new("s", sink);
    mgr.set_goal("g", "t", Some(10_000), None);
    mgr.record_usage(&usage(300, 100, 200), 1); // +400 ((300-100) + 200)
    mgr.record_usage(&usage(100, 0, 0), 2); // +100 (non-cached input only)
    assert_eq!(mgr.state().tokens_used, 500);
    assert_eq!(mgr.budget().total_accounted(), 500);
    assert_eq!(mgr.budget().remaining(), Some(9_500));
}

#[test]
fn accounting_over_budget_marks_goal_budget_limited() {
    let sink = Arc::new(RecordingSink::default());
    let mut mgr = GoalManager::new("s", sink);
    mgr.set_goal("g", "t", Some(100), None);
    mgr.record_usage(&usage(80, 0, 10), 1);
    assert_eq!(mgr.state().status.as_deref(), Some(status::ACTIVE));
    mgr.record_usage(&usage(20, 0, 0), 1);
    assert_eq!(mgr.state().status.as_deref(), Some(status::BUDGET_LIMITED));
    assert!(
        mgr.state().is_active(),
        "budget-limited goals remain active for wrap-up steering"
    );
    let body = mgr.goal_context_text().expect("budget-limited prompt");
    assert!(body.contains("has reached its token budget"));
    assert!(body.contains("do not start new substantive work"));
}

#[test]
fn clear_goal_via_manager_stops_budget_steering() {
    let sink = Arc::new(RecordingSink::default());
    let mut mgr = GoalManager::new("s", sink.clone());
    mgr.set_goal("g", "t", Some(1000), None);
    mgr.record_usage(&usage(900, 0, 0), 0); // warn fired
    let before = sink.len();
    mgr.clear_goal();
    // Accounting after clear is a no-op (no active goal) and emits nothing.
    let emitted = mgr.record_usage(&usage(5000, 0, 0), 0);
    assert!(emitted.is_empty());
    assert_eq!(sink.len(), before, "cleared goal emits no further steering");
    assert!(mgr.goal_context_message().is_none());
}
