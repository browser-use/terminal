//! `compact/` — **model-based** context compaction (codex `core/src/compact.rs`).
//!
//! When the turn loop reaches a [`decision::LoopStep::CompactThenContinue`] step
//! (`token_limit_reached && needs_follow_up`, codex `turn.rs:282`), it must shrink
//! the conversation so the next sampling round-trip fits the model's context
//! window. Codex does this by asking the **model itself** to write a handoff
//! summary, then replacing history with `[preserved recent user messages] +
//! [summary message]`. This module ports that flow.
//!
//! ## CRITICAL: compaction is model-based — there is NO no-LLM path
//! Every compaction performs a real (no-tools) sampling round-trip to produce the
//! summary. The "structured dump / no-LLM" alternative was explicitly **rejected**
//! by the product owner and is deliberately NOT implemented here. The summarizer
//! is abstracted behind [`CompactionSampler`] purely so tests can inject a
//! *scripted* model response (a canned summary) and stay network-free — it is NOT
//! a non-model fallback. A production [`CompactionSampler`] drives the real
//! `ModelClient` exactly like [`crate::turn::sampling::ModelSamplingDriver`], with
//! tool dispatch disabled (the summary pass must not call tools — codex's compact
//! task streams `OutputItemDone`/`Completed` only and never dispatches).
//!
//! ## Codex parity (`codex-rs/core/src/compact.rs`)
//! - [`SUMMARIZATION_PROMPT`] reproduces codex `templates/compact/prompt.md`
//!   (the "CONTEXT CHECKPOINT COMPACTION" instruction; `compact.rs:46`).
//! - [`SUMMARY_PREFIX`] reproduces codex `templates/compact/summary_prefix.md`
//!   (`compact.rs:47`, 399 bytes, no trailing newline) and is **byte-identical**
//!   to legacy `browser-use-core::COMPACTION_SUMMARY_PREFIX` (constants.rs:26) —
//!   see the const docs.
//! - [`run_compaction`] mirrors `run_compact_task_inner_impl` (`compact.rs:171`):
//!   build the summary request (history + the summarization prompt), run ONE
//!   no-tools sampling pass, take the assistant summary, and assemble
//!   `summary_text = format!("{SUMMARY_PREFIX}\n{summary_suffix}")`.
//! - [`build_compacted_history`] mirrors `build_compacted_history`
//!   (`compact.rs:466`): the most-recent real user messages, oldest-first, capped
//!   at [`COMPACT_USER_MESSAGE_MAX_TOKENS`] (20_000; `compact.rs:48`), then the
//!   summary message appended last.
//! - On [`AgentError::ContextWindowExceeded`] during the summary request, the
//!   oldest history item is dropped and the request is retried (codex's
//!   `remove_first_item` loop, `compact.rs:224-233`), until only the lone prompt
//!   item remains.
//!
//! ## Wiring
//! [`CompactingTurnState`] is the concrete [`TurnState`] the production loop uses:
//! it holds the shared [`ContextManager`] and a [`CompactionSampler`], and its
//! [`compact`](TurnState::compact) hook calls [`run_compaction`] and **replaces**
//! the context history with the compacted result. The pure
//! [`build_compacted_history`] / token-budget helpers stay unit testable, and the
//! end-to-end path is exercised through the real [`TurnLoop`].
//!
//! [`TurnLoop`]: crate::turn::TurnLoop

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use browser_use_llm::schema::{ContentPart, Message, MessageRole};
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use crate::context::accounting::TokenUsage;
use crate::context::assembly::TruncationPolicy;
use crate::context::{ContextManager, Item};
use crate::decision::TokenStatus;
use crate::turn::{CompactionMode, TurnState};
use crate::AgentError;

#[cfg(test)]
mod tests;
#[cfg(test)]
mod threshold_tests;

/// The "CONTEXT CHECKPOINT COMPACTION" instruction sent as the final user message
/// of the summary request (codex `core/templates/compact/prompt.md`, referenced by
/// `compact.rs:46`). Reproduced verbatim, including the trailing newline the file
/// carries (codex `include_str!`s the file, so the newline is part of the const).
pub const SUMMARIZATION_PROMPT: &str = "You are performing a CONTEXT CHECKPOINT COMPACTION. Create a handoff summary for another LLM that will resume the task.

Include:
- Current progress and key decisions made
- Important context, constraints, or user preferences
- What remains to be done (clear next steps)
- Any critical data, examples, or references needed to continue

Be concise, structured, and focused on helping the next LLM seamlessly continue the work.
";

/// Prefix prepended to the model summary so the *next* turn knows the summary is a
/// handoff from a prior model (codex `core/templates/compact/summary_prefix.md`,
/// referenced by `compact.rs:47`).
///
/// **Byte-identical** to legacy `browser-use-core::COMPACTION_SUMMARY_PREFIX`
/// (`crates/browser-use-core/src/constants.rs:26`) AND to codex
/// `core/templates/compact/summary_prefix.md`: 399 bytes, NO trailing newline
/// (codex's template file itself carries no trailing `\n`). Codex writes
/// `format!("{SUMMARY_PREFIX}\n{summary_suffix}")` (`compact.rs:263`); legacy spells
/// the same no-newline const + an explicit `\n` separator. We match that form
/// (const without trailing newline, explicit `\n` in [`run_compaction`]) so the
/// emitted summary message and the `is_summary_message` prefix check are identical
/// to both codex and the live engine (verified: legacy == codex, 399 bytes each).
pub const SUMMARY_PREFIX: &str = "Another language model started to solve this problem and produced a summary of its thinking process. You also have access to the state of the tools that were used by that language model. Use this to build on the work that has already been done and avoid duplicating work. Here is the summary produced by the other language model, use the information in this summary to assist with your own analysis:";

/// Token budget for the recent user messages preserved in the compacted history
/// (codex `COMPACT_USER_MESSAGE_MAX_TOKENS = 20_000`, `compact.rs:48`).
pub const COMPACT_USER_MESSAGE_MAX_TOKENS: usize = 20_000;

/// Approximate token count for `text`: 1 token per 4 bytes, rounded up.
///
/// Ground: codex `codex-utils-string::approx_token_count`
/// (`text.len().div_ceil(4)`) — the heuristic
/// `build_compacted_history_with_limit` uses for the preserved-user-message budget
/// (`compact.rs:492`).
pub fn approx_token_count(text: &str) -> usize {
    if text.is_empty() {
        0
    } else {
        text.len().div_ceil(4)
    }
}

/// Truncate `text` to at most `max_tokens` approximate tokens (`max_tokens * 4`
/// bytes), respecting char boundaries.
///
/// Ground: codex `truncate_text(text, TruncationPolicy::Tokens(remaining))`
/// (`compact.rs:497`), whose token policy maps to `max_tokens * 4` bytes.
fn truncate_to_tokens(text: &str, max_tokens: usize) -> String {
    let max_bytes = max_tokens.saturating_mul(4);
    if text.len() <= max_bytes {
        return text.to_string();
    }
    // Walk back to the nearest char boundary at or below max_bytes.
    let mut end = max_bytes;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_string()
}

/// Result of one successful no-tools summary pass.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompactionSummary {
    pub text: String,
    pub token_usage: Option<TokenUsage>,
}

impl CompactionSummary {
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            token_usage: None,
        }
    }

    pub fn with_usage(text: impl Into<String>, token_usage: TokenUsage) -> Self {
        Self {
            text: text.into(),
            token_usage: Some(token_usage),
        }
    }
}

/// The no-tools summary pass: run ONE model round-trip over the summary request
/// and return the assistant's summary text plus provider token usage when the
/// stream reports it (codex records `Completed { token_usage, .. }` before
/// recomputing active context, `compact.rs:208/572`).
///
/// This MUST be model-based. Implementors drive the real `ModelClient` with tool
/// dispatch DISABLED; tests inject a scripted response. The pass returns:
/// - `Ok(summary)` with the assistant summary text and optional usage (empty
///   string allowed — codex falls back to `(no summary available)` downstream),
/// - `Err(AgentError::ContextWindowExceeded)` so [`run_compaction`] can drop the
///   oldest item and retry (codex `compact.rs:224`),
/// - any other `Err` to abort compaction.
///
/// Uses the same native RPITIT async-trait style as [`crate::turn::TurnState`] /
/// [`crate::turn::SamplingDriver`] (no `async_trait` macro).
pub trait CompactionSampler: Send + Sync {
    /// Summarize `request` (the conversation so far + the summarization prompt) in
    /// a single no-tools model round-trip.
    fn summarize(
        &self,
        request: Vec<Message>,
        cancel: CancellationToken,
    ) -> impl std::future::Future<Output = Result<CompactionSummary, AgentError>> + Send;
}

/// Lower a history [`Item`] (provider-message `Value`) to its plain user text iff
/// it is a *real* user message (role == "user" and NOT a compaction summary).
///
/// Ground: codex `collect_user_messages` (`compact.rs:389`) keeps `UserMessage`
/// items, skipping any whose text is a compaction summary (`is_summary_message`).
fn real_user_message_text(item: &Item) -> Option<String> {
    let obj = item.as_object()?;
    let role = obj.get("role").and_then(Value::as_str)?;
    if role != "user" {
        return None;
    }
    if obj.get("name").and_then(Value::as_str).is_some() {
        return None;
    }
    let text = item_text(item);
    if text.is_empty() || is_summary_message(&text) {
        return None;
    }
    Some(text)
}

/// Flatten an item's `content` (string or array of `{text}` parts) to plain text.
/// Mirrors codex `content_items_to_text` joining `Input/OutputText` with `\n`
/// (`compact.rs:370`) and the context reducer's `message_content_text`.
fn item_text(item: &Item) -> String {
    let Some(content) = item.get("content") else {
        return String::new();
    };
    match content {
        Value::String(s) => s.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|p| p.get("text").and_then(Value::as_str).or_else(|| p.as_str()))
            .collect::<Vec<_>>()
            .join("\n"),
        other if !other.is_null() => other.to_string(),
        _ => String::new(),
    }
}

/// Is `message` a compaction summary (i.e. begins with `"{SUMMARY_PREFIX}\n"`)?
///
/// Ground: codex `is_summary_message` (`compact.rs:405`) /
/// legacy `browser-use-core` (`strip_prefix(COMPACTION_SUMMARY_PREFIX)`).
pub fn is_summary_message(message: &str) -> bool {
    message.starts_with(&format!("{SUMMARY_PREFIX}\n"))
}

/// Build one provider-message `Item` for a user-role text message
/// (`{"role":"user","content":[{"type":"text","text":...}]}`), the shape
/// [`ContextManager`] records and lowers.
///
/// Mirrors codex `build_compacted_history` pushing
/// `ResponseItem::Message { role: "user", content: [InputText{text}] }`
/// (`compact.rs:505/522`).
fn user_text_item(text: &str) -> Item {
    json!({
        "role": "user",
        "content": [ { "type": "text", "text": text } ],
    })
}

/// Build the final user message appended to a compaction summary request.
pub fn compaction_prompt_item(compact_prompt: &str) -> Item {
    user_text_item(compact_prompt)
}

/// Assemble the compacted replacement history: the most-recent real user messages
/// (oldest-first) within the token budget, then the summary message last.
///
/// Ground: codex `build_compacted_history` /
/// `build_compacted_history_with_limit` (`compact.rs:466-530`). The budget walks
/// `user_messages` from newest to oldest accumulating up to `max_tokens`; a
/// message that would overflow is truncated to the remaining budget and ends the
/// walk; the selection is then reversed back to chronological order. Each selected
/// message becomes a `user` item, and the summary is appended as the final `user`
/// item. An empty summary becomes `"(no summary available)"` (`compact.rs:516`).
pub fn build_compacted_history(
    user_messages: &[String],
    summary_text: &str,
    max_tokens: usize,
) -> Vec<Item> {
    let mut selected: Vec<String> = Vec::new();
    if max_tokens > 0 {
        let mut remaining = max_tokens;
        for message in user_messages.iter().rev() {
            if remaining == 0 {
                break;
            }
            let tokens = approx_token_count(message);
            if tokens <= remaining {
                selected.push(message.clone());
                remaining = remaining.saturating_sub(tokens);
            } else {
                selected.push(truncate_to_tokens(message, remaining));
                break;
            }
        }
        selected.reverse();
    }

    let mut history: Vec<Item> = Vec::with_capacity(selected.len() + 1);
    for message in &selected {
        history.push(user_text_item(message));
    }

    let summary_text = if summary_text.is_empty() {
        "(no summary available)".to_string()
    } else {
        summary_text.to_string()
    };
    history.push(user_text_item(&summary_text));
    history
}

/// Build the replacement history from a completed summary suffix.
pub fn compacted_history_from_summary(
    history: &[Item],
    summary_suffix: CompactionSummary,
    token_limit: usize,
) -> CompactedHistory {
    // Apply the empty-summary fallback to the SUFFIX before prepending the
    // prefix — otherwise the prefix makes the string non-empty and the fallback
    // in build_compacted_history is dead code, shipping PREFIX + "" (total
    // post-compaction amnesia; real_v8 task 24).
    let suffix_text = if summary_suffix.text.trim().is_empty() {
        "(no summary available)".to_string()
    } else {
        summary_suffix.text.clone()
    };
    let summary_text = format!("{SUMMARY_PREFIX}\n{suffix_text}");
    let user_messages: Vec<String> = history.iter().filter_map(real_user_message_text).collect();
    let items = build_compacted_history(&user_messages, &summary_text, token_limit);

    CompactedHistory {
        items,
        summary_text,
        summary_token_usage: summary_suffix.token_usage,
    }
}

/// The compacted replacement history produced by [`run_compaction`]: a fresh item
/// buffer that REPLACES the pre-compaction history (codex
/// `replace_compacted_history`, `compact.rs:284`).
#[derive(Debug, Clone, PartialEq)]
pub struct CompactedHistory {
    /// `[preserved recent user messages...] + [summary message]`, in order.
    pub items: Vec<Item>,
    /// The full summary message text (`"{SUMMARY_PREFIX}\n{summary_suffix}"`), the
    /// last item's text. Surfaced for the lifecycle/compacted-item event.
    pub summary_text: String,
    /// Provider-reported usage for the no-tools compaction model call.
    pub summary_token_usage: Option<TokenUsage>,
}

/// Run **model-based** compaction over `history` and return the compacted
/// replacement history (codex `run_compact_task_inner_impl`, `compact.rs:171`).
///
/// Steps (codex parity):
/// 1. Seed a working history = `history` + the [`SUMMARIZATION_PROMPT`] as a final
///    user item (codex records `initial_input_for_turn` then loops; `compact.rs:182`).
/// 2. Run ONE no-tools [`CompactionSampler::summarize`] pass over the working
///    history (lowered to [`Message`]s). On [`AgentError::ContextWindowExceeded`],
///    drop the OLDEST working item and retry while >1 item remains
///    (codex `remove_first_item`, `compact.rs:224-233`); when only the lone prompt
///    item remains, the error propagates.
/// 3. `summary_text = format!("{SUMMARY_PREFIX}\n{summary_suffix}")` over the
///    assistant summary (`compact.rs:263`).
/// 4. Collect the real user messages from the ORIGINAL `history` and
///    [`build_compacted_history`] = preserved recent user messages (≤20k tokens) +
///    the summary message (`compact.rs:264-266`).
///
/// `cancel` threads codex's interrupt token into the summary pass.
pub async fn run_compaction<S: CompactionSampler + ?Sized>(
    history: &[Item],
    sampler: &S,
    compact_prompt: &str,
    token_limit: usize,
    cancel: CancellationToken,
) -> Result<CompactedHistory, AgentError> {
    // 1. Working history = original history + the summarization prompt (recorded as
    //    the final user item, codex `compact.rs:182`).
    let mut working: Vec<Item> = history.to_vec();
    working.push(compaction_prompt_item(compact_prompt));

    // 2. No-tools summary pass with the drop-oldest-on-ContextWindowExceeded loop
    //    (codex `compact.rs:195-258`). Lower items to typed messages each attempt
    //    because the working set may shrink between retries.
    let summary_suffix = loop {
        let request = compaction_request_messages(&working);
        match sampler.summarize(request, cancel.clone()).await {
            Ok(summary) => break summary,
            Err(AgentError::ContextWindowExceeded) => {
                // codex trims from the front to preserve prefix cache while a real
                // message (beyond the lone prompt) remains; otherwise it gives up.
                if working.len() > 1 {
                    working.remove(0);
                    continue;
                }
                return Err(AgentError::ContextWindowExceeded);
            }
            Err(other) => return Err(other),
        }
    };

    // 3-4. Prefix the summary and preserve recent real user messages (from the ORIGINAL history)
    //    (codex `compact.rs:264-266`).
    Ok(compacted_history_from_summary(
        history,
        summary_suffix,
        token_limit,
    ))
}

/// Lower provider-message `Item`s to typed [`Message`]s for an `LlmRequest`,
/// reusing [`ContextManager::lower_to_messages`] so the summary request is built
/// exactly like a normal sampling request.
pub fn compaction_request_messages(items: &[Item]) -> Vec<Message> {
    ContextManager::new().lower_to_messages(items)
}

/// A concrete [`TurnState`] whose [`compact`](TurnState::compact) hook performs
/// **model-based** [`run_compaction`] and replaces the [`ContextManager`] history
/// with the result.
///
/// This is the production wiring described in `turn/mod.rs`: rather than mutate the
/// frozen `TurnState` trait, the loop is constructed over this concrete state. It
/// owns the shared [`ContextManager`] (behind a `Mutex` so the loop's read methods
/// and the compaction write are sequenced) plus a [`CompactionSampler`] and the
/// per-state inputs the loop needs.
///
/// On `compact()`:
/// 1. snapshot the current history items,
/// 2. run [`run_compaction`] (the no-tools model summary pass),
/// 3. **replace** the manager's history with the compacted items.
///
/// Token accounting resets implicitly: the new manager holds only the compacted
/// items, so the next [`token_status`](TurnState::token_status) sees the shrunken
/// history (codex `recompute_token_usage`, `compact.rs:286`).
pub struct CompactingTurnState<S: CompactionSampler> {
    ctx: Arc<Mutex<ContextManager>>,
    sampler: Arc<S>,
    token_limit: usize,
    /// Pending steer input (drained by the loop). Mirrors the in-memory test state.
    pending: Mutex<VecDeque<Message>>,
    /// Scriptable token status driving the compaction trigger.
    token_status: Mutex<TokenStatus>,
    /// `true` once a compaction has fired and relieved token pressure, so the loop
    /// does not compact forever (mirrors the live engine clearing the full-window
    /// flag after a successful compaction).
    relieve_after_compact: bool,
}

impl<S: CompactionSampler> CompactingTurnState<S> {
    /// Build a compacting turn state over a shared [`ContextManager`] + sampler.
    pub fn new(
        ctx: Arc<Mutex<ContextManager>>,
        sampler: Arc<S>,
        token_limit: usize,
        pending: Vec<Message>,
        token_status: TokenStatus,
    ) -> Self {
        Self {
            ctx,
            sampler,
            token_limit,
            pending: Mutex::new(pending.into_iter().collect()),
            token_status: Mutex::new(token_status),
            relieve_after_compact: true,
        }
    }

    /// Disable the post-compaction pressure relief (the next compaction trigger
    /// stays armed). Used by tests that drive token pressure explicitly.
    pub fn without_pressure_relief(mut self) -> Self {
        self.relieve_after_compact = false;
        self
    }

    /// Shared handle to the [`ContextManager`] (tests assert the post-compaction
    /// history through it).
    pub fn context(&self) -> Arc<Mutex<ContextManager>> {
        self.ctx.clone()
    }
}

impl<S: CompactionSampler + 'static> TurnState for CompactingTurnState<S> {
    async fn clone_history_for_prompt(&self) -> Vec<Message> {
        let ctx = self.ctx.lock().expect("context manager poisoned");
        ctx.lower_to_messages(ctx.items())
    }

    async fn record_items(&self, items: &[Message]) {
        let provider_items: Vec<Item> = items.iter().map(message_to_item).collect();
        self.ctx
            .lock()
            .expect("context manager poisoned")
            .record_items(provider_items, TruncationPolicy::Bytes(usize::MAX));
    }

    async fn has_pending_input(&self) -> bool {
        !self.pending.lock().expect("pending poisoned").is_empty()
    }

    async fn take_pending_input(&self) -> Vec<Message> {
        self.pending
            .lock()
            .expect("pending poisoned")
            .drain(..)
            .collect()
    }

    async fn token_status(&self) -> TokenStatus {
        self.token_status
            .lock()
            .expect("token status poisoned")
            .clone()
    }

    async fn compact(&self, _mode: CompactionMode) -> Result<(), AgentError> {
        // Snapshot current history (codex `clone_history`, `compact.rs:182`).
        let items: Vec<Item> = {
            let ctx = self.ctx.lock().expect("context manager poisoned");
            ctx.items().to_vec()
        };

        // Model-based summary pass + compacted-history assembly.
        let compacted = match run_compaction(
            &items,
            self.sampler.as_ref(),
            SUMMARIZATION_PROMPT,
            self.token_limit,
            CancellationToken::new(),
        )
        .await
        {
            Ok(c) => c,
            Err(error) => return Err(error),
        };

        // Replace history with the compacted items (codex `replace_compacted_history`,
        // `compact.rs:284`). A brand-new manager drops the old token accounting, so
        // the next `token_status` reflects the shrunken history.
        {
            let mut ctx = self.ctx.lock().expect("context manager poisoned");
            let mut fresh = ContextManager::new();
            fresh.record_items(compacted.items, TruncationPolicy::Bytes(usize::MAX));
            *ctx = fresh;
        }

        if self.relieve_after_compact {
            let mut st = self.token_status.lock().expect("token status poisoned");
            st.full_context_window_limit_reached = false;
            st.token_limit_reached = false;
        }
        Ok(())
    }
}

/// Lower a typed [`Message`] back to a provider-message `Item` (`Value`) for the
/// [`ContextManager`] buffer. Inverse of [`ContextManager::lower_to_messages`] for
/// the shapes the loop records (text/user/assistant; tool calls + results).
fn message_to_item(message: &Message) -> Item {
    let role = match message.role {
        MessageRole::System => "system",
        MessageRole::Developer => "developer",
        MessageRole::User => "user",
        MessageRole::Assistant => "assistant",
        MessageRole::Tool => "tool",
    };

    // Tool-result messages lower to a `tool` role item keyed by tool_call_id.
    if message.role == MessageRole::Tool {
        if let Some(ContentPart::ToolResult {
            tool_call_id,
            content,
            ..
        }) = message.content.first()
        {
            return json!({
                "role": "tool",
                "tool_call_id": tool_call_id,
                "content": tool_result_content_to_item_content(content),
            });
        }
    }

    let mut content_parts: Vec<Value> = Vec::new();
    let mut tool_calls: Vec<Value> = Vec::new();
    for part in &message.content {
        match part {
            ContentPart::Text { text } => {
                content_parts.push(json!({ "type": "text", "text": text }));
            }
            ContentPart::Media {
                mime_type,
                data,
                url,
                detail,
            } => {
                content_parts.push(json!({
                    "type": "image",
                    "mime_type": mime_type,
                    "data": data,
                    "url": url,
                    "detail": detail,
                }));
            }
            ContentPart::ToolCall {
                id, name, input, ..
            } => {
                tool_calls.push(json!({
                    "id": id,
                    "name": name,
                    "arguments": input,
                }));
            }
            ContentPart::ToolResult { .. } | ContentPart::Reasoning { .. } => {}
        }
    }

    let mut obj = serde_json::Map::new();
    obj.insert("role".to_string(), json!(role));
    obj.insert("content".to_string(), Value::Array(content_parts));
    if !tool_calls.is_empty() {
        obj.insert("tool_calls".to_string(), Value::Array(tool_calls));
    }
    Value::Object(obj)
}

fn tool_result_content_to_item_content(content: &[ContentPart]) -> Value {
    let mut text = String::new();
    let mut parts = Vec::new();
    let mut has_non_text = false;
    for part in content {
        match part {
            ContentPart::Text { text: fragment }
            | ContentPart::Reasoning { text: fragment, .. } => {
                text.push_str(fragment);
                if !fragment.is_empty() {
                    parts.push(json!({ "type": "input_text", "text": fragment }));
                }
            }
            ContentPart::Media {
                mime_type,
                data,
                url,
                detail,
            } => {
                has_non_text = true;
                if let Some(media) = media_item_content_part(
                    mime_type,
                    data.as_deref(),
                    url.as_deref(),
                    detail.as_deref(),
                ) {
                    parts.push(media);
                }
            }
            ContentPart::ToolResult { content, .. } => {
                let nested = tool_result_content_to_item_content(content);
                match nested {
                    Value::String(fragment) => {
                        text.push_str(&fragment);
                        if !fragment.is_empty() {
                            parts.push(json!({ "type": "input_text", "text": fragment }));
                        }
                    }
                    Value::Array(nested_parts) => {
                        has_non_text = true;
                        parts.extend(nested_parts);
                    }
                    _ => {}
                }
            }
            ContentPart::ToolCall { .. } => {}
        }
    }
    if has_non_text {
        Value::Array(parts)
    } else {
        Value::String(text)
    }
}

fn media_item_content_part(
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
        Some(json!({
            "type": "input_image",
            "image_url": resolved,
            "detail": detail.unwrap_or("auto"),
        }))
    } else {
        Some(json!({
            "type": "input_file",
            "file_data": resolved,
        }))
    }
}
