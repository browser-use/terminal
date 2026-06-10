//! `context/` — `ContextManager` + REAL token accounting. Pure core, thin async wrapper.
//!
//! `Item` == browser-use-protocol canonical transcript item (`ResponseItem` equivalent).
//! Until protocol exposes it, the frozen surface aliases `serde_json::Value` (the legacy
//! provider-message currency, which is what session reconstruction returns today).
//!
//! ## Layering
//!
//! Everything below the [`ContextManager`] is a **pure sync** core (WP-A3 +
//! WP-A4, already merged): [`accounting`] (token math), [`assembly`] (per-item
//! estimation / truncation / `for_prompt` normalization / total-usage), and
//! [`inject`] (contextual-message builders + the reference-context diff).
//! `ContextManager` is the thin async wrapper (WP-B2): it owns the item buffer,
//! a [`accounting::TokenUsageInfo`], and a monotonic history version, and it
//! delegates every computation to those pure cores. The only `async` surface is
//! [`ContextManager::persist_snapshot`], the store **write-sink** (B4 wires the
//! real store; here it is a no-op that never reads back).

pub mod accounting;
pub mod assembly;
pub mod constants;
pub mod image_estimate;
pub mod inject;
pub mod normalize;
pub mod user_input;
pub mod workspace_context;

/// Surface the durable workspace-context append helpers at the `context` module root so
/// the tui/cli can reach them without naming the `workspace_context` submodule. These
/// already exist in [`workspace_context`]; this only re-exports them.
pub use workspace_context::{
    append_user_shell_command_context_event, append_workspace_context_event,
};

/// Surface the typed `session.input` payload builders at the `context` module root.
pub use user_input::{
    typed_user_input_payload_from_items_for_cwd, typed_user_input_payload_from_text_for_cwd,
    typed_user_input_preview_from_items,
};

#[cfg(test)]
mod inject_tests;
#[cfg(test)]
mod mod_tests;
#[cfg(test)]
mod tests_accounting;

use browser_use_llm::schema::{ContentPart, Message, MessageRole};
use serde_json::Value;

/// FROZEN ALIAS; swap to `protocol::ResponseItem` when available (open q).
pub type Item = serde_json::Value;
/// Modality probe; the real enum lives in route capabilities.
pub type InputModality = browser_use_llm::schema::ContentPart;

/// Async wrapper — the ONLY non-pure surface. browser-use-store = WRITE-SINK + notify.
///
/// Owns the canonical item buffer (the legacy provider-message currency), a
/// running [`accounting::TokenUsageInfo`], and a monotonically increasing
/// [`history_version`](ContextManager::history_version) bumped on every mutation.
/// All accounting/assembly/injection logic lives in the pure cores; this struct
/// only sequences calls into them and (eventually) writes to the store sink.
#[derive(Default)]
pub struct ContextManager {
    /// The full transcript buffer, in order. Each `Item` is a provider message
    /// `Value` (`{type,role,content,...}` / tool-call / output / reasoning).
    items: Vec<Item>,
    /// Running token accounting (`total` accumulates, `last` is the latest API
    /// response's usage). `None` until the first `update_token_info`.
    token_info: Option<accounting::TokenUsageInfo>,
    /// Monotonic version, bumped on every state mutation (record / token
    /// update). Lets observers cheaply detect "did the context change".
    history_version: u64,
}

impl ContextManager {
    /// A fresh, empty manager (no items, no usage, version 0).
    pub fn new() -> Self {
        Self::default()
    }

    /// Append `items` to the buffer, applying the truncation policy per item
    /// (tool/function-call outputs are truncated at `policy * 1.2`; all other
    /// kinds pass through unchanged).
    ///
    /// Delegates to [`assembly::process_item`] and bumps the history version
    /// once for the batch.
    pub fn record_items<I: IntoIterator<Item = Item>>(
        &mut self,
        items: I,
        p: assembly::TruncationPolicy,
    ) {
        for item in items {
            self.items.push(assembly::process_item(&item, p));
        }
        self.bump_version();
    }

    /// The item list to send to the model this turn: the full normalize
    /// pipeline ([`assembly::for_prompt`] = ensure-call-outputs +
    /// remove-orphan-outputs + strip-images-when-unsupported) over a clone of
    /// the buffer. Non-mutating.
    pub fn snapshot_for_prompt(&self, supports_image: bool) -> Vec<Item> {
        assembly::for_prompt(self.items.clone(), supports_image)
    }

    /// `Item` (provider-message `Value`) -> browser-use-llm `Message` for an
    /// `LlmRequest`. Items that carry no usable content (e.g. a bare tool-call
    /// envelope with empty text, or an unrecognized shape) are skipped.
    pub fn lower_to_messages(&self, items: &[Item]) -> Vec<Message> {
        items.iter().filter_map(item_to_message).collect()
    }

    /// Fold a fresh per-turn [`accounting::TokenUsage`] into the running info,
    /// updating `total` (accumulated), `last`, and the model context window when
    /// `window` is supplied. Bumps the history version.
    pub fn update_token_info(&mut self, u: &accounting::TokenUsage, window: Option<i64>) {
        self.token_info =
            accounting::TokenUsageInfo::new_or_append(self.token_info.as_ref(), Some(u), window);
        self.bump_version();
    }

    /// Mark the model context window as full (the auto-compaction trigger).
    /// Preserves accumulated usage when present; otherwise installs a
    /// usage-empty info carrying just the window. Bumps the history version.
    pub fn set_token_usage_full(&mut self, window: i64) {
        match self.token_info.as_mut() {
            Some(info) => info.fill_to_context_window(window),
            None => {
                self.token_info = Some(accounting::TokenUsageInfo::full_context_window(window));
            }
        }
        self.bump_version();
    }

    /// Total token usage for the current buffer, branching on whether the
    /// server already counted reasoning tokens. Delegates to
    /// [`assembly::total_token_usage`].
    pub fn total_token_usage(&self, server_reasoning_included: bool) -> i64 {
        assembly::total_token_usage(
            &self.items,
            self.token_info.as_ref(),
            server_reasoning_included,
        )
    }

    /// Detailed token-usage breakdown for the current buffer
    /// ([`assembly::total_token_usage_breakdown`]).
    pub fn breakdown(&self) -> assembly::TotalTokenUsageBreakdown {
        assembly::total_token_usage_breakdown(&self.items, self.token_info.as_ref())
    }

    /// Estimate the total tokens of the WHOLE current buffer, from the model-
    /// visible byte size of every item (codex `all_history_items_model_visible_bytes`
    /// → tokens via `approx_tokens_from_byte_count` = `bytes.div_ceil(4)`).
    ///
    /// This is the size-of-the-conversation estimate the auto-compaction trigger
    /// compares to the context window. Unlike [`total_token_usage`], it does NOT
    /// depend on a server `Usage` having arrived (a store-seeded manager with no
    /// API response yet still reports a real size) and it counts the ENTIRE
    /// prompt, not just the items after the last model turn. Returned as `i64` for
    /// [`crate::decision::TokenStatus::from_estimate`].
    ///
    /// Ground: codex `context_manager` byte/token math —
    /// `estimate_item_model_visible_bytes` per item (`assembly.rs`),
    /// `approx_tokens_from_byte_count_i64 = bytes.div_ceil(4)` (`accounting.rs`,
    /// memory note `APPROX_CHARS_PER_TOKEN = 4`).
    ///
    /// [`total_token_usage`]: ContextManager::total_token_usage
    pub fn estimate_total_tokens(&self) -> i64 {
        let bytes = self.breakdown().all_history_items_model_visible_bytes;
        accounting::approx_tokens_from_byte_count_i64(bytes)
    }

    /// The monotonic history version (bumped on every mutation).
    pub fn history_version(&self) -> u64 {
        self.history_version
    }

    /// Write + notify, never read back. The store wiring is WP-B4; for now this
    /// is an async no-op so the frozen signature is honored and callers can
    /// `.await` it. It MUST never read from the store.
    pub async fn persist_snapshot(&self) -> anyhow::Result<()> {
        // WP-B4 wires the real `browser-use-store` write-sink + notify here.
        // Until then this is intentionally a no-op (write-only contract).
        Ok(())
    }

    /// Read-only access to the current buffer (for tests / observers).
    pub fn items(&self) -> &[Item] {
        &self.items
    }

    fn bump_version(&mut self) {
        self.history_version = self.history_version.saturating_add(1);
    }
}

/// Lower one provider-message `Item` (`Value`) into a typed [`Message`].
///
/// Recognizes the legacy provider-message shapes the assembly/reconstruct cores
/// produce:
///   * `role` -> [`MessageRole`] (`system`/`user`/`assistant`/`tool`/
///     `developer`; unknown roles fall back to `user`, mirroring the reducer's
///     `unwrap_or("user")`).
///   * `content` either a bare string or an array of `{type,text}` /
///     `{type:input_text,text}` / image parts -> [`ContentPart`]s.
///   * `tool_calls` (assistant) -> [`ContentPart::ToolCall`].
///   * a `tool`-role message -> a single [`ContentPart::ToolResult`] carrying
///     the output content, linked by `tool_call_id`.
///
/// Returns `None` for items with no usable content (so empty envelopes don't
/// produce empty `Message`s).
fn item_to_message(item: &Item) -> Option<Message> {
    let obj = item.as_object()?;
    let role = role_from_str(obj.get("role").and_then(Value::as_str));

    // Tool-result messages: one ToolResult part keyed by tool_call_id.
    if matches!(role, MessageRole::Tool) {
        let tool_call_id = obj
            .get("tool_call_id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let mut content = obj.get("content").map(content_to_parts).unwrap_or_default();
        if content.is_empty() {
            content.push(ContentPart::text(content_text(obj.get("content"))));
        }
        return Some(Message::new(
            MessageRole::Tool,
            vec![ContentPart::ToolResult {
                tool_call_id,
                content,
                is_error: false,
            }],
        ));
    }

    let mut parts: Vec<ContentPart> = Vec::new();
    if let Some(content) = obj.get("content") {
        parts.extend(content_to_parts(content));
    }

    // Assistant tool-calls.
    if let Some(calls) = obj.get("tool_calls").and_then(Value::as_array) {
        for call in calls {
            let Some(id) = call
                .get("id")
                .or_else(|| call.get("call_id"))
                .and_then(Value::as_str)
            else {
                continue;
            };
            let Some(name) = call.get("name").and_then(Value::as_str) else {
                continue;
            };
            let input = call.get("arguments").cloned().unwrap_or(Value::Null);
            let provider_metadata = call.get("provider_metadata").cloned();
            parts.push(ContentPart::ToolCall {
                id: id.to_string(),
                name: name.to_string(),
                input,
                provider_metadata,
            });
        }
    }

    if parts.is_empty() {
        return None;
    }
    Some(Message::new(role, parts))
}

/// Map a provider role string to [`MessageRole`], defaulting to `user`.
fn role_from_str(role: Option<&str>) -> MessageRole {
    match role {
        Some("system") => MessageRole::System,
        Some("assistant") => MessageRole::Assistant,
        Some("tool") => MessageRole::Tool,
        Some("developer") => MessageRole::Developer,
        _ => MessageRole::User,
    }
}

/// Convert a `content` Value (string or array of parts) into typed parts.
fn content_to_parts(content: &Value) -> Vec<ContentPart> {
    match content {
        Value::String(text) if !text.is_empty() => vec![ContentPart::text(text.clone())],
        Value::String(_) => Vec::new(),
        Value::Array(parts) => parts.iter().filter_map(part_to_content_part).collect(),
        _ => Vec::new(),
    }
}

/// Convert a single content-array element into a [`ContentPart`].
fn part_to_content_part(part: &Value) -> Option<ContentPart> {
    let part_type = part.get("type").and_then(Value::as_str)?;
    match part_type {
        "text" | "input_text" | "output_text" => {
            let text = part.get("text").and_then(Value::as_str)?;
            Some(ContentPart::text(text))
        }
        "input_image" | "image" | "image_url" | "output_image" => {
            let url = part
                .get("image_url")
                .and_then(|v| v.as_str().or_else(|| v.get("url").and_then(Value::as_str)))
                .or_else(|| part.get("url").and_then(Value::as_str))
                .map(ToOwned::to_owned);
            if let Some((mime_type, data)) = url.as_deref().and_then(data_url_media) {
                return Some(ContentPart::Media {
                    mime_type,
                    data: Some(data),
                    url: None,
                    detail: image_detail_from_part(part),
                });
            }
            let mime_type = part
                .get("mime_type")
                .and_then(Value::as_str)
                .unwrap_or("image/png")
                .to_string();
            Some(ContentPart::Media {
                mime_type,
                data: part
                    .get("data")
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned),
                url,
                detail: image_detail_from_part(part),
            })
        }
        _ => None,
    }
}

fn image_detail_from_part(part: &Value) -> Option<String> {
    part.get("detail")
        .and_then(Value::as_str)
        .filter(|detail| !detail.trim().is_empty())
        .map(ToOwned::to_owned)
}

fn data_url_media(url: &str) -> Option<(String, String)> {
    let rest = url.strip_prefix("data:")?;
    let (header, data) = rest.split_once(',')?;
    let mime_type = header.split(';').next()?.to_string();
    if !mime_type.starts_with("image/") || !header.contains(";base64") || data.is_empty() {
        return None;
    }
    Some((mime_type, data.to_string()))
}

/// Flatten a `content` Value to plain text (string content, or the joined
/// `text` fields of an array). Mirrors the reducer's `message_content_text`.
fn content_text(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|part| {
                part.get("text")
                    .and_then(Value::as_str)
                    .or_else(|| part.as_str())
            })
            .collect::<Vec<_>>()
            .join(""),
        Some(value) if !value.is_null() => value.to_string(),
        _ => String::new(),
    }
}
