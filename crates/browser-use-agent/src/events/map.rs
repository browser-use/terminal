//! PURE `LlmEvent` -> `PendingEvent` mapper + usage/payload helpers.
//!
//! Parity source: browser-use-core `src/lib.rs`
//! (`record_model_stream_delta` / `record_model_thinking_delta`,
//! `append_codex_token_count_event` / `build_codex_token_count_info`) and the
//! `tool.started` emission in `tools/*`. These functions are intentionally
//! pure — no `&self`, no DB, no `.await` — so the turn loop's mapping logic
//! stays unit-testable.

use super::{names, PendingEvent, TurnCtx};
use browser_use_llm::schema::{LlmEvent, Usage};
use browser_use_protocol::ModelUsage;
use serde_json::{json, Value};

/// Map a single `LlmEvent` to zero or more `PendingEvent`s.
///
/// Only events that carry UI-facing data map to anything. The streaming
/// lifecycle markers (`StepStart`, `Text{Start,End}`, `Reasoning{Start,End}`,
/// `ToolInput{Start,Delta,End}`, `StepFinish`) carry no UI payload of their own
/// and map to nothing — codex/core records no per-marker UI event for them, so
/// they return an empty `Vec`.
///
/// `task_complete` / `turn_aborted` (and `task_started`) are NOT mapped here:
/// they are turn-lifecycle events synthesized by the turn layer, not carried by
/// any single `LlmEvent`.
pub fn map_llm_event(ctx: &TurnCtx, ev: &LlmEvent) -> Vec<PendingEvent> {
    let session_id = &ctx.session_id;
    match ev {
        // Assistant text streaming -> `model.stream_delta { text }`. Keep the
        // legacy `delta` alias so older reducers/tests can tolerate both shapes.
        LlmEvent::TextDelta { delta, .. } => vec![PendingEvent::new(
            session_id,
            names::MODEL_STREAM_DELTA,
            json!({ "text": delta, "delta": delta }),
        )],
        // Reasoning/thinking streaming -> `model.thinking_delta { text }`.
        LlmEvent::ReasoningDelta { delta, .. } => vec![PendingEvent::new(
            session_id,
            names::MODEL_THINKING_DELTA,
            json!({ "text": delta, "delta": delta }),
        )],
        // Fully-assembled tool call -> `tool.started { name, arguments }`.
        // `arguments` is the parsed JSON input the model produced; core forwards
        // it under the `arguments` key.
        LlmEvent::ToolCall {
            id, name, input, ..
        } => vec![PendingEvent::new(
            session_id,
            names::TOOL_STARTED,
            json!({ "name": name, "tool_call_id": id, "arguments": input }),
        )],
        // Provider-side mid-stream error -> `stream_error { message }`.
        LlmEvent::ProviderError { message, .. } => vec![PendingEvent::new(
            session_id,
            names::STREAM_ERROR,
            json!({ "message": message }),
        )],
        // Terminal completion of the turn's model response: emit `token_count`
        // from the carried usage. `task_complete` is emitted by the turn layer,
        // not here.
        LlmEvent::Finish { usage, .. } => {
            let mu = usage_to_model_usage(usage);
            vec![PendingEvent::new(
                session_id,
                names::TOKEN_COUNT,
                token_count_payload(&mu, &Value::Null, None, ctx.turn_idx),
            )]
        }
        // Streaming lifecycle markers with no UI-facing payload.
        LlmEvent::StepStart
        | LlmEvent::TextStart { .. }
        | LlmEvent::TextEnd { .. }
        | LlmEvent::ReasoningStart { .. }
        | LlmEvent::ReasoningEnd { .. }
        | LlmEvent::ToolInputStart { .. }
        | LlmEvent::ToolInputDelta { .. }
        | LlmEvent::ToolInputEnd { .. }
        | LlmEvent::StepFinish { .. } => vec![],
    }
}

/// Map provider `Usage` -> protocol `ModelUsage`.
///
/// Uses `Usage::computed_total()` (which excludes cached tokens, since they are
/// a subset of `input_tokens`) as the total when the provider reported
/// `total_tokens == 0`. `u64` counts are widened to the protocol's `Option<i64>`
/// fields. Cost / cache-creation fields are unknown at this layer and left
/// `None`.
pub fn usage_to_model_usage(u: &Usage) -> ModelUsage {
    let total = if u.total_tokens == 0 {
        u.computed_total()
    } else {
        u.total_tokens
    };
    ModelUsage {
        input_tokens: Some(u.input_tokens as i64),
        input_cached_tokens: Some(u.cached_input_tokens as i64),
        input_cache_creation_tokens: positive_i64(u.cache_creation_input_tokens),
        output_tokens: Some(u.output_tokens as i64),
        reasoning_output_tokens: Some(u.reasoning_output_tokens as i64),
        total_tokens: Some(total as i64),
        ..Default::default()
    }
}

fn positive_i64(value: u64) -> Option<i64> {
    (value > 0).then_some(value as i64)
}

/// Codex-shaped token-usage object (mirrors core `model_usage_to_codex_token_usage`):
/// `{ input_tokens, cached_input_tokens, output_tokens, reasoning_output_tokens,
/// total_tokens }`, where a missing `total_tokens` falls back to the sum of the
/// (non-cached) breakdown.
fn codex_token_usage(usage: &ModelUsage) -> Value {
    let input_tokens = usage.input_tokens.unwrap_or(0);
    let cached_input_tokens = usage.input_cached_tokens.unwrap_or(0);
    let cache_creation_input_tokens = usage.input_cache_creation_tokens.unwrap_or(0);
    let output_tokens = usage.output_tokens.unwrap_or(0);
    let reasoning_output_tokens = usage.reasoning_output_tokens.unwrap_or(0);
    let total_tokens = usage.total_tokens.unwrap_or_else(|| {
        input_tokens
            .saturating_add(output_tokens)
            .saturating_add(reasoning_output_tokens)
    });
    let mut value = json!({
        "input_tokens": input_tokens,
        "cached_input_tokens": cached_input_tokens,
        "output_tokens": output_tokens,
        "reasoning_output_tokens": reasoning_output_tokens,
        "total_tokens": total_tokens,
    });
    if cache_creation_input_tokens > 0 {
        value["input_cache_creation_tokens"] = json!(cache_creation_input_tokens);
    }
    value
}

/// Field-wise sum of two codex token-usage objects (mirrors core
/// `add_codex_token_usage`). Missing keys are treated as `0`.
fn add_codex_token_usage(previous: &Value, addition: &Value) -> Value {
    let get = |value: &Value, key: &str| value.get(key).and_then(Value::as_i64).unwrap_or(0);
    let cache_creation_input_tokens =
        get(previous, "input_cache_creation_tokens") + get(addition, "input_cache_creation_tokens");
    let mut value = json!({
        "input_tokens": get(previous, "input_tokens") + get(addition, "input_tokens"),
        "cached_input_tokens":
            get(previous, "cached_input_tokens") + get(addition, "cached_input_tokens"),
        "output_tokens": get(previous, "output_tokens") + get(addition, "output_tokens"),
        "reasoning_output_tokens":
            get(previous, "reasoning_output_tokens") + get(addition, "reasoning_output_tokens"),
        "total_tokens": get(previous, "total_tokens") + get(addition, "total_tokens"),
    });
    if cache_creation_input_tokens > 0 {
        value["input_cache_creation_tokens"] = json!(cache_creation_input_tokens);
    }
    value
}

/// Build the `token_count` payload (core parity:
/// `append_codex_token_count_event`):
///
/// ```json
/// { "info": { "total_token_usage": { input_tokens, cached_input_tokens,
///                                    output_tokens, reasoning_output_tokens,
///                                    total_tokens },
///             "last_token_usage":  { ...same shape... },
///             "model_context_window": <window|null> },
///   "turn_idx": <turn_idx> }
/// ```
///
/// `last_token_usage` is THIS turn's usage; `total_token_usage` is the running
/// cumulative across the session. `prev_total` is the cumulative usage object
/// from the previous `token_count` event (the value of `info.total_token_usage`);
/// it is added field-wise to this turn's usage. When `prev_total` is `null`
/// (no prior turn), the cumulative equals this turn's usage.
///
/// Note: core also folds in a `rate_limits` snapshot and reads `prev_total` from
/// the store; those are stateful concerns left to the store/turn layer. This
/// pure helper takes `prev_total` explicitly and omits `rate_limits`.
pub fn token_count_payload(
    usage: &ModelUsage,
    prev_total: &Value,
    window: Option<i64>,
    turn_idx: usize,
) -> Value {
    let last_token_usage = codex_token_usage(usage);
    let total_token_usage = if prev_total.is_null() {
        last_token_usage.clone()
    } else {
        add_codex_token_usage(prev_total, &last_token_usage)
    };
    json!({
        "info": {
            "total_token_usage": total_token_usage,
            "last_token_usage": last_token_usage,
            "model_context_window": window,
        },
        "turn_idx": turn_idx,
    })
}

/// Pointer to a final result artifact (file) accompanying `session.done`.
pub struct ResultFilePtr {
    pub url: Option<String>,
    pub path: Option<String>,
    pub bytes: Option<u64>,
}

/// Build the `session.done` payload: an object containing `result` and/or
/// `result_file` only when present, where `result_file` itself contains
/// `url` / `path` / `bytes` only when present.
pub fn session_done_payload(result: Option<&str>, result_file: Option<&ResultFilePtr>) -> Value {
    let mut payload = serde_json::Map::new();
    if let Some(r) = result {
        payload.insert("result".to_string(), json!(r));
    }
    if let Some(rf) = result_file {
        let mut file = serde_json::Map::new();
        if let Some(url) = &rf.url {
            file.insert("url".to_string(), json!(url));
        }
        if let Some(path) = &rf.path {
            file.insert("path".to_string(), json!(path));
        }
        if let Some(bytes) = rf.bytes {
            file.insert("bytes".to_string(), json!(bytes));
        }
        payload.insert("result_file".to_string(), Value::Object(file));
    }
    Value::Object(payload)
}
