//! Tests for **model-based** compaction (codex `core/src/compact.rs` parity).
//!
//! NETWORK-FREE & DETERMINISTIC. The summary pass is the only model interaction,
//! and it is driven by a [`ScriptedSampler`] returning a canned summary (or a
//! scripted error sequence to exercise the drop-oldest retry). No `ModelClient`,
//! socket, or timer is touched. There is NO no-LLM compaction path: every test
//! goes through [`run_compaction`], which always performs the summary pass.
//!
//! Coverage (per WP spec):
//! 1. `run_compaction` produces `[preserved recent user messages] + [summary]`,
//!    the summary carrying the byte-identical prefix.
//! 2. the summary prefix is byte-identical to the legacy/codex prefix.
//! 3. the preserved-user budget caps at `COMPACT_USER_MESSAGE_MAX_TOKENS`.
//! 4. `ContextWindowExceeded` → drops the oldest item, retries, eventually
//!    succeeds.
//! 5. end-to-end through the real [`TurnLoop`]: a scripted `token_status` forcing
//!    `CompactThenContinue` actually invokes `run_compaction` (history shrinks)
//!    and the loop continues to completion.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use browser_use_llm::schema::{ContentPart, Message, MessageRole};
use serde_json::json;
use tokio_util::sync::CancellationToken;

use crate::compact::{
    build_compacted_history, is_summary_message, run_compaction, CompactingTurnState,
    CompactionSampler, CompactionSummary, COMPACT_USER_MESSAGE_MAX_TOKENS, SUMMARIZATION_PROMPT,
    SUMMARY_PREFIX,
};
use crate::context::assembly::TruncationPolicy;
use crate::context::{ContextManager, Item};
use crate::decision::{SamplingOutcome, TokenStatus};
use crate::events::TurnCtx;
use crate::task::TurnLifecycleEvent;
use crate::turn::{SamplingDriver, TurnLoop, TurnObserver};
use crate::AgentError;

// ---- scripted summary sampler ---------------------------------------------

/// A [`CompactionSampler`] that replays a queue of scripted results, one per
/// `summarize` call, recording the request body length each call carried (so a
/// test can assert the drop-oldest retry shrank the request). The summary pass is
/// always model-based; this is just a network-free stand-in for the real model.
struct ScriptedSampler {
    results: Mutex<VecDeque<Result<CompactionSummary, AgentError>>>,
    calls: AtomicUsize,
    request_lens: Mutex<Vec<usize>>,
    request_texts: Mutex<Vec<Vec<String>>>,
}

impl ScriptedSampler {
    fn new(results: Vec<Result<String, AgentError>>) -> Arc<Self> {
        Arc::new(Self {
            results: Mutex::new(
                results
                    .into_iter()
                    .map(|result| result.map(CompactionSummary::text))
                    .collect(),
            ),
            calls: AtomicUsize::new(0),
            request_lens: Mutex::new(Vec::new()),
            request_texts: Mutex::new(Vec::new()),
        })
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

impl CompactionSampler for ScriptedSampler {
    async fn summarize(
        &self,
        request: Vec<Message>,
        _cancel: CancellationToken,
    ) -> Result<CompactionSummary, AgentError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.request_lens.lock().unwrap().push(request.len());
        self.request_texts.lock().unwrap().push(
            request
                .iter()
                .flat_map(|message| message.content.iter())
                .filter_map(|part| match part {
                    ContentPart::Text { text } => Some(text.clone()),
                    _ => None,
                })
                .collect(),
        );
        // Past the end of the queue, default to an empty summary so a mis-scripted
        // test fails loudly rather than hanging.
        self.results
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| Ok(CompactionSummary::text(String::new())))
    }
}

// ---- helpers --------------------------------------------------------------

/// A `user`-role provider-message item carrying `text`.
fn user_item(text: &str) -> Item {
    json!({ "role": "user", "content": [{ "type": "text", "text": text }] })
}

/// An `assistant`-role item carrying `text` (a non-user item, to prove only real
/// user messages are preserved).
fn assistant_item(text: &str) -> Item {
    json!({ "role": "assistant", "content": [{ "type": "text", "text": text }] })
}

fn item_text(item: &Item) -> String {
    item.get("content")
        .and_then(|c| c.as_array())
        .map(|parts| {
            parts
                .iter()
                .filter_map(|p| p.get("text").and_then(|t| t.as_str()))
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default()
}

fn item_role(item: &Item) -> String {
    item.get("role")
        .and_then(|r| r.as_str())
        .unwrap_or_default()
        .to_string()
}

// ---- (1) run_compaction: [preserved users] + [summary-with-prefix] --------

#[tokio::test]
async fn run_compaction_preserves_recent_users_and_appends_prefixed_summary() {
    // History: user, assistant, user, assistant. Only the two real user messages
    // are preserved (assistant items are dropped), in chronological order, then a
    // final summary item carrying the prefix.
    let history = vec![
        user_item("first user ask"),
        assistant_item("assistant reply 1"),
        user_item("second user ask"),
        assistant_item("assistant reply 2"),
    ];
    let sampler = ScriptedSampler::new(vec![Ok("model wrote this handoff summary".to_string())]);

    let compacted = run_compaction(
        &history,
        sampler.as_ref(),
        SUMMARIZATION_PROMPT,
        COMPACT_USER_MESSAGE_MAX_TOKENS,
        CancellationToken::new(),
    )
    .await
    .expect("compaction should succeed");

    // Exactly one no-tools model pass ran (model-based; never skipped).
    assert_eq!(sampler.calls(), 1, "exactly one summary round-trip");

    // Shape: [user, user, summary].
    assert_eq!(
        compacted.items.len(),
        3,
        "two preserved users + one summary"
    );
    assert_eq!(item_text(&compacted.items[0]), "first user ask");
    assert_eq!(item_text(&compacted.items[1]), "second user ask");
    for it in &compacted.items {
        assert_eq!(
            item_role(it),
            "user",
            "every compacted item is a user message"
        );
    }

    // The final item is the prefixed summary; the summary_text matches it.
    let summary_item_text = item_text(&compacted.items[2]);
    assert_eq!(summary_item_text, compacted.summary_text);
    assert!(
        is_summary_message(&summary_item_text),
        "the summary item begins with the summary prefix"
    );
    assert_eq!(
        compacted.summary_text,
        format!("{SUMMARY_PREFIX}\nmodel wrote this handoff summary"),
        "summary_text = PREFIX + newline + model summary (codex compact.rs:263)"
    );
    // The summary suffix (the model's text) survives verbatim after the prefix+nl.
    let suffix = summary_item_text
        .strip_prefix(&format!("{SUMMARY_PREFIX}\n"))
        .expect("summary carries the exact prefix");
    assert_eq!(suffix, "model wrote this handoff summary");
}

#[tokio::test]
async fn run_compaction_uses_supplied_compact_prompt() {
    let history = vec![user_item("keep this")];
    let sampler = ScriptedSampler::new(vec![Ok("summary".to_string())]);
    let custom_prompt = "Write a tiny checkpoint.";

    run_compaction(
        &history,
        sampler.as_ref(),
        custom_prompt,
        COMPACT_USER_MESSAGE_MAX_TOKENS,
        CancellationToken::new(),
    )
    .await
    .expect("compaction should succeed");

    let request_texts = sampler.request_texts.lock().unwrap().clone();
    let first_request = request_texts.first().expect("summary request recorded");
    assert_eq!(
        first_request.last().map(String::as_str),
        Some(custom_prompt)
    );
    assert!(
        !first_request
            .iter()
            .any(|text| text == crate::compact::SUMMARIZATION_PROMPT),
        "custom compact prompt should replace the default prompt"
    );
}

// ---- (2) the summary prefix is byte-identical to legacy/codex --------------

#[test]
fn summary_prefix_is_byte_identical_to_legacy_and_codex() {
    // Byte-for-byte assertion of the exact prefix string. This is the legacy
    // `browser-use-core::COMPACTION_SUMMARY_PREFIX` (constants.rs:26) and the
    // content of codex `core/templates/compact/summary_prefix.md` — 399 bytes,
    // no trailing newline (both are byte-identical at 399 bytes).
    const EXPECTED: &str = "Another language model started to solve this problem and produced a summary of its thinking process. You also have access to the state of the tools that were used by that language model. Use this to build on the work that has already been done and avoid duplicating work. Here is the summary produced by the other language model, use the information in this summary to assist with your own analysis:";
    assert_eq!(SUMMARY_PREFIX, EXPECTED, "prefix text must match exactly");
    assert_eq!(SUMMARY_PREFIX.len(), 399, "prefix is 399 bytes");
    assert!(
        !SUMMARY_PREFIX.ends_with('\n'),
        "prefix carries no trailing newline (matches legacy const)"
    );
    // And the prefix end is the load-bearing ':' the next-turn header relies on.
    assert!(SUMMARY_PREFIX.ends_with("your own analysis:"));
}

// ---- (3) preserved-user budget caps at COMPACT_USER_MESSAGE_MAX_TOKENS -----

#[test]
fn build_compacted_history_caps_preserved_users_at_token_budget() {
    // Two messages, each ~ (budget * 4) bytes => ~budget tokens apiece. With a
    // budget of exactly one message's worth, only the NEWEST fits; the older one
    // is dropped entirely (walk is newest→oldest, codex compact.rs:488).
    let budget = 1000usize; // tokens
    let one_msg = "x".repeat(budget * 4); // ~budget tokens
    let user_messages = vec![format!("OLD {one_msg}"), format!("NEW {one_msg}")];

    let items = build_compacted_history(&user_messages, "the summary", budget);

    // Only the newest preserved user message + the summary => 2 items.
    assert_eq!(
        items.len(),
        2,
        "older message dropped: only newest fits the budget, then the summary"
    );
    assert!(
        item_text(&items[0]).starts_with("NEW "),
        "the surviving preserved message is the newest one"
    );
    assert_eq!(item_text(&items[1]), "the summary");

    // A zero budget drops ALL preserved users, leaving only the summary
    // (codex `if max_tokens > 0` guard, compact.rs:486).
    let only_summary = build_compacted_history(&user_messages, "s", 0);
    assert_eq!(only_summary.len(), 1, "zero budget => summary only");
    assert_eq!(item_text(&only_summary[0]), "s");

    // An over-budget single message is TRUNCATED to the remaining budget and ends
    // the walk (codex truncate_text branch, compact.rs:497). The truncated text is
    // shorter than the original but non-empty.
    let huge = "y".repeat(budget * 8); // ~2*budget tokens, over budget
    let truncated_items = build_compacted_history(&[huge.clone()], "s", budget);
    assert_eq!(truncated_items.len(), 2, "truncated message + summary");
    let kept = item_text(&truncated_items[0]);
    assert!(kept.len() < huge.len(), "over-budget message is truncated");
    assert!(
        !kept.is_empty(),
        "truncation keeps the budget-worth of bytes"
    );
    assert!(kept.len() <= budget * 4, "kept bytes within token budget");

    // The DEFAULT budget is the codex constant.
    assert_eq!(COMPACT_USER_MESSAGE_MAX_TOKENS, 20_000);
}

// ---- (4) ContextWindowExceeded → drop oldest + retry → succeed ------------

#[tokio::test]
async fn context_window_exceeded_drops_oldest_then_retries_and_succeeds() {
    // History of 3 real items + the appended prompt = 4 working items. The summary
    // pass fails with ContextWindowExceeded twice, then succeeds: each failure
    // drops the OLDEST working item, so the 3rd attempt's request is 2 items
    // smaller than the first (codex remove_first_item loop, compact.rs:224-233).
    let history = vec![
        user_item("oldest user"),
        user_item("middle user"),
        user_item("newest user"),
    ];
    let sampler = ScriptedSampler::new(vec![
        Err(AgentError::ContextWindowExceeded),
        Err(AgentError::ContextWindowExceeded),
        Ok("summary after trimming".to_string()),
    ]);

    let compacted = run_compaction(
        &history,
        sampler.as_ref(),
        SUMMARIZATION_PROMPT,
        COMPACT_USER_MESSAGE_MAX_TOKENS,
        CancellationToken::new(),
    )
    .await
    .expect("compaction eventually succeeds after dropping oldest items");

    // Three summary attempts: fail, fail, succeed.
    assert_eq!(sampler.calls(), 3, "two failures then a success");

    // The request shrank by one item per retry (4 → 3 → 2 working items).
    let lens = sampler.request_lens.lock().unwrap().clone();
    assert_eq!(
        lens,
        vec![4, 3, 2],
        "each ContextWindowExceeded drops the oldest working item before retry"
    );

    // The compacted result still preserves the recent (real) user messages from
    // the ORIGINAL history (run_compaction collects users from `history`, not the
    // trimmed working set) + the summary.
    assert_eq!(
        compacted.summary_text,
        format!("{SUMMARY_PREFIX}\nsummary after trimming"),
    );
    assert!(is_summary_message(&item_text(
        compacted.items.last().unwrap()
    )));
    // 3 preserved users + 1 summary.
    assert_eq!(compacted.items.len(), 4);
}

#[tokio::test]
async fn context_window_exceeded_with_only_prompt_left_propagates() {
    // Empty history => working set is just the lone prompt item. A
    // ContextWindowExceeded there cannot trim further (codex falls through to the
    // error path when turn_input_len <= 1, compact.rs:234), so it propagates.
    let history: Vec<Item> = Vec::new();
    let sampler = ScriptedSampler::new(vec![Err(AgentError::ContextWindowExceeded)]);

    let err = run_compaction(
        &history,
        sampler.as_ref(),
        SUMMARIZATION_PROMPT,
        COMPACT_USER_MESSAGE_MAX_TOKENS,
        CancellationToken::new(),
    )
    .await
    .expect_err("with only the prompt left, ContextWindowExceeded propagates");

    assert!(matches!(err, AgentError::ContextWindowExceeded));
    assert_eq!(sampler.calls(), 1, "no retry once only the prompt remains");
}

#[tokio::test]
async fn empty_model_summary_yields_no_summary_available_fallback() {
    // The model returns an empty summary => summary_text = PREFIX + "\n" + "".
    // (The "(no summary available)" placeholder only applies when the WHOLE
    // summary_text is empty, which it never is here because the prefix is present;
    // codex compact.rs:516.)
    let history = vec![user_item("a user message")];
    let sampler = ScriptedSampler::new(vec![Ok(String::new())]);

    let compacted = run_compaction(
        &history,
        sampler.as_ref(),
        SUMMARIZATION_PROMPT,
        COMPACT_USER_MESSAGE_MAX_TOKENS,
        CancellationToken::new(),
    )
    .await
    .expect("compaction succeeds with an empty model summary");

    let last = item_text(compacted.items.last().unwrap());
    assert_eq!(last, compacted.summary_text);
    assert_eq!(
        compacted.summary_text,
        format!("{SUMMARY_PREFIX}\n(no summary available)")
    );
}

// ---- (5) end-to-end through the real TurnLoop -----------------------------

/// A [`SamplingDriver`] that returns scripted [`SamplingOutcome`]s (the loop's
/// model round-trips, distinct from the compaction summary pass) and records how
/// many requests ran.
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
            .unwrap_or_else(|| SamplingOutcome {
                model_needs_follow_up: false,
                last_agent_message: Some("end".to_string()),
                defers_mailbox_delivery_to_next_turn: true,
                finish_reason: None,
            }))
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

fn ctx() -> TurnCtx {
    TurnCtx {
        session_id: "sess-compact".to_string(),
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

fn token_status(limit_reached: bool) -> TokenStatus {
    TokenStatus {
        auto_compact_scope_tokens: 0,
        auto_compact_scope_limit: 1,
        full_context_window_limit_reached: limit_reached,
        token_limit_reached: limit_reached,
    }
}

#[tokio::test]
async fn turn_loop_compact_step_invokes_run_compaction_and_history_shrinks() {
    // A CompactingTurnState whose ContextManager starts with a long history of
    // many real user messages. The loop's first round-trip wants follow-up AND
    // token_limit_reached → CompactThenContinue, which fires the REAL compact hook
    // (model-based run_compaction via the scripted summary sampler). The compacted
    // history is [preserved users + summary], strictly smaller than the original,
    // and the loop then completes on its second round-trip.

    // Seed a large history: 6 user messages + 6 assistant replies = 12 items.
    let mut manager = ContextManager::new();
    let mut seed: Vec<Item> = Vec::new();
    for i in 0..6 {
        seed.push(user_item(&format!("user turn {i}")));
        seed.push(assistant_item(&format!("assistant turn {i}")));
    }
    manager.record_items(seed, TruncationPolicy::Bytes(usize::MAX));
    let original_len = manager.items().len();
    assert_eq!(original_len, 12, "seeded 12 history items");

    let ctx_handle = Arc::new(Mutex::new(manager));
    let summary_sampler = ScriptedSampler::new(vec![Ok("compacted handoff".to_string())]);

    let state = CompactingTurnState::new(
        ctx_handle.clone(),
        summary_sampler.clone(),
        COMPACT_USER_MESSAGE_MAX_TOKENS,
        Vec::new(),
        token_status(true), // token limit reached up front → compaction trigger armed
    );

    // Loop model round-trips: iter 1 follow_up (→ compact-then-continue), iter 2
    // complete (after the hook relieved pressure).
    let loop_sampler = LoopSampler::new(vec![
        follow_up("needs more + over budget"),
        complete("done after compaction"),
    ]);
    let requests = loop_sampler.requests_handle();
    let observer = RecordingObserver::new();

    let turn = TurnLoop::new(state, loop_sampler, observer.clone());
    let out = turn
        .run(ctx(), false, CancellationToken::new())
        .await
        .expect("loop completes after a model-based compaction");

    // The compaction summary pass ran exactly once (model-based, in the hook).
    assert_eq!(
        summary_sampler.calls(),
        1,
        "the compact hook ran run_compaction's model summary pass exactly once"
    );
    // Two loop round-trips: the compaction step continued the loop.
    assert_eq!(requests.load(Ordering::SeqCst), 2);
    assert_eq!(out.as_deref(), Some("done after compaction"));
    assert_eq!(observer.kinds(), vec!["started", "complete"]);

    // History SHRANK: original 12 items → [6 preserved users + 1 summary] = 7.
    let compacted_items = ctx_handle.lock().unwrap().items().to_vec();
    assert!(
        compacted_items.len() < original_len,
        "compaction shrank history: {} < {}",
        compacted_items.len(),
        original_len
    );
    assert_eq!(
        compacted_items.len(),
        7,
        "6 preserved real user messages + 1 summary message"
    );
    // The last item is the prefixed summary.
    let last_text = item_text(compacted_items.last().unwrap());
    assert!(
        is_summary_message(&last_text),
        "compacted history ends with the prefixed summary"
    );
    assert_eq!(last_text, format!("{SUMMARY_PREFIX}\ncompacted handoff"));
    // No assistant items survived (only real user messages are preserved).
    assert!(
        compacted_items.iter().all(|it| item_role(it) == "user"),
        "every compacted item is a user message"
    );
}

#[tokio::test]
async fn record_items_roundtrips_through_context_manager() {
    // Sanity: CompactingTurnState::record_items lowers typed messages into the
    // ContextManager and clone_history_for_prompt reads them back, so the loop's
    // history threading works against the real manager.
    let ctx_handle = Arc::new(Mutex::new(ContextManager::new()));
    let sampler = ScriptedSampler::new(vec![Ok("s".to_string())]);
    let state = CompactingTurnState::new(
        ctx_handle.clone(),
        sampler,
        COMPACT_USER_MESSAGE_MAX_TOKENS,
        Vec::new(),
        token_status(false),
    );

    use crate::turn::TurnState as _;
    let msgs = vec![
        Message::new(MessageRole::User, vec![ContentPart::text("hello")]),
        Message::new(MessageRole::Assistant, vec![ContentPart::text("hi there")]),
    ];
    state.record_items(&msgs).await;

    let read_back = state.clone_history_for_prompt().await;
    assert_eq!(read_back.len(), 2, "both recorded messages lower back");
    assert_eq!(read_back[0].role, MessageRole::User);
    assert_eq!(read_back[1].role, MessageRole::Assistant);
}
