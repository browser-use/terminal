//! TUI-side product-analytics adapter.
//!
//! origin/main's TUI called `browser_use_core::product_analytics::…`, a module
//! on the (now-deleted) `browser-use-core` engine. The new `browser-use-agent`
//! engine re-implements only the low-level capture primitives in
//! [`browser_use_agent::infra::analytics`] (`capture_async` / `capture_blocking`)
//! and does NOT port the higher-level `capture_user_message` /
//! `capture_user_message_blocked` helpers or the `MESSAGE_KIND_*` /
//! `BLOCKED_REASON_*` event-property constants the TUI calls.
//!
//! Rather than edit the engine, this thin TUI-local module restores that lost
//! surface on top of the engine's primitives. The helper signatures and base
//! event shapes follow the deleted `browser-use-core::product_analytics`
//! surface, with safe provider/model metadata appended for dashboard breakdowns.

use browser_use_store::Store;

pub use browser_use_agent::infra::analytics::capture_async;

pub const MESSAGE_KIND_INITIAL: &str = "initial";
pub const MESSAGE_KIND_FOLLOWUP: &str = "followup";
pub const BLOCKED_REASON_NO_AUTH: &str = "no_auth";

const APPROX_CHARS_PER_TOKEN: usize = 4;

#[derive(Clone, Copy, Debug, Default)]
pub struct ModelAnalytics<'a> {
    pub provider_kind: Option<&'a str>,
    pub provider: Option<&'a str>,
    pub model: Option<&'a str>,
}

#[allow(clippy::too_many_arguments)]
pub fn capture_user_message(
    store: &Store,
    surface: &str,
    session_id: &str,
    is_subagent: bool,
    kind: &str,
    seq: i64,
    text: &str,
    model: ModelAnalytics<'_>,
) {
    let mut properties = serde_json::json!({
        "surface": surface,
        "session_id": session_id,
        "is_subagent": is_subagent,
        "kind": kind,
        "seq": seq,
    });
    append_user_text_analytics(&mut properties, text);
    append_model_analytics(&mut properties, model);
    capture_async(store, "bu:tui user_message", properties);
}

#[allow(clippy::too_many_arguments)]
pub fn capture_user_message_blocked(
    store: &Store,
    surface: &str,
    session_id: &str,
    is_subagent: bool,
    seq: i64,
    text: &str,
    blocked_reason: &str,
    model: ModelAnalytics<'_>,
) {
    let mut properties = serde_json::json!({
        "surface": surface,
        "session_id": session_id,
        "is_subagent": is_subagent,
        "seq": seq,
        "blocked_reason": blocked_reason,
    });
    append_user_text_analytics(&mut properties, text);
    append_model_analytics(&mut properties, model);
    capture_async(store, "bu:tui user_message_blocked", properties);
}

fn append_user_text_analytics(properties: &mut serde_json::Value, text: &str) {
    let Some(object) = properties.as_object_mut() else {
        return;
    };
    let trimmed = text.trim();
    let char_count = trimmed.chars().count();
    let word_count = if trimmed.is_empty() {
        0
    } else {
        trimmed.split_whitespace().count()
    };
    let approx_tokens = char_count.div_ceil(APPROX_CHARS_PER_TOKEN);
    object.insert(
        "text".to_string(),
        serde_json::Value::String(text.to_string()),
    );
    object.insert(
        "text_chars".to_string(),
        serde_json::json!(text.chars().count()),
    );
    object.insert("char_count".to_string(), serde_json::json!(char_count));
    object.insert("word_count".to_string(), serde_json::json!(word_count));
    object.insert(
        "approx_tokens".to_string(),
        serde_json::json!(approx_tokens),
    );
}

pub fn append_model_analytics(properties: &mut serde_json::Value, model: ModelAnalytics<'_>) {
    let Some(object) = properties.as_object_mut() else {
        return;
    };
    for (key, value) in [
        ("provider_kind", model.provider_kind),
        ("provider", model.provider),
    ] {
        if let Some(value) = value.map(str::trim).filter(|value| !value.is_empty()) {
            object.insert(
                key.to_string(),
                serde_json::Value::String(value.to_string()),
            );
        }
    }
    if let Some(raw_model) = model.model.map(str::trim).filter(|value| !value.is_empty()) {
        let simple_model = simple_model_id(raw_model);
        object.insert(
            "model".to_string(),
            serde_json::Value::String(simple_model.clone()),
        );
        if simple_model != raw_model {
            object.insert(
                "provider_model".to_string(),
                serde_json::Value::String(raw_model.to_string()),
            );
        }
    }
}

pub fn simple_model_id(model: &str) -> String {
    model
        .trim()
        .rsplit('/')
        .next()
        .unwrap_or(model)
        .trim()
        .replace('_', "-")
        .to_ascii_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn appends_user_text_analytics_with_raw_text() {
        let mut properties = serde_json::json!({"surface": "tui"});
        append_user_text_analytics(&mut properties, "  open example.com  ");

        assert_eq!(properties["text"], "  open example.com  ");
        assert_eq!(properties["text_chars"], 20);
        assert_eq!(properties["char_count"], 16);
        assert_eq!(properties["word_count"], 2);
        assert_eq!(properties["approx_tokens"], 4);
    }

    #[test]
    fn appends_model_analytics() {
        let mut properties = serde_json::json!({"surface": "tui"});
        append_model_analytics(
            &mut properties,
            ModelAnalytics {
                provider_kind: Some("subscription"),
                provider: Some("codex"),
                model: Some("openai/gpt-5.5"),
            },
        );

        assert_eq!(properties["provider_kind"], "subscription");
        assert_eq!(properties["provider"], "codex");
        assert_eq!(properties["model"], "gpt-5.5");
        assert_eq!(properties["provider_model"], "openai/gpt-5.5");
    }

    #[test]
    fn simple_model_id_strips_provider_prefix() {
        assert_eq!(simple_model_id("openai/gpt-5.5"), "gpt-5.5");
        assert_eq!(simple_model_id("GPT_5.4"), "gpt-5.4");
    }
}
