//! Tests for the async `ContextManager` wrapper (WP-B2) over the pure cores.
//!
//! Async is allowed (the wrapper exposes an async `persist_snapshot`) but no
//! network / store I/O: `persist_snapshot` is a write-only no-op here.
//!
//! Coverage (per the WP-B2 contract):
//!   * `record_items` then `snapshot_for_prompt` reflects appended items +
//!     applies the `policy * 1.2` tool-output truncation + the `for_prompt`
//!     normalization (synthetic outputs / orphan removal / image stripping).
//!   * `lower_to_messages` produces valid `browser_use_llm::schema::Message`s
//!     (user/assistant/tool with the right roles + parts).
//!   * `update_token_info` accumulation + `total_token_usage` for BOTH
//!     `server_reasoning_included` branches; `set_token_usage_full`.
//!   * `history_version` monotonicity + `persist_snapshot` is an awaitable
//!     no-op.
//!   * the A4/A6 inject golden (permissions builder == legacy shape).

use serde_json::json;

use super::accounting::{TokenUsage, TokenUsageInfo};
use super::assembly::{self, TruncationPolicy};
use super::inject::{build_context_message, ContextKind};
use super::ContextManager;
use browser_use_llm::schema::{ContentPart, MessageRole};

// ---------------------------------------------------------------------------
// Helpers.
// ---------------------------------------------------------------------------

fn user_msg(text: &str) -> serde_json::Value {
    json!({ "type": "message", "role": "user", "content": text })
}

fn assistant_with_tool_call(id: &str, name: &str) -> serde_json::Value {
    json!({
        "role": "assistant",
        "content": "calling a tool",
        "tool_calls": [{ "id": id, "name": name, "arguments": { "x": 1 } }],
    })
}

fn assistant_with_provider_metadata(id: &str, name: &str) -> serde_json::Value {
    json!({
        "role": "assistant",
        "content": "calling a tool",
        "tool_calls": [{
            "id": id,
            "name": name,
            "arguments": { "x": 1 },
            "provider_metadata": {
                "google": {
                    "thought_signature": "sig-model-call"
                }
            }
        }],
    })
}

fn function_call(id: &str, name: &str) -> serde_json::Value {
    json!({ "type": "function_call", "call_id": id, "name": name, "arguments": "{}" })
}

fn function_call_output(id: &str, text: &str) -> serde_json::Value {
    json!({ "type": "function_call_output", "call_id": id, "output": text })
}

// ---------------------------------------------------------------------------
// record_items + snapshot_for_prompt.
// ---------------------------------------------------------------------------

#[test]
fn record_items_appends_and_snapshot_reflects_them() {
    let mut cm = ContextManager::new();
    cm.record_items(
        vec![user_msg("hello"), user_msg("world")],
        TruncationPolicy::Tokens(1000),
    );
    assert_eq!(cm.items().len(), 2);

    let snap = cm.snapshot_for_prompt(true);
    assert_eq!(snap.len(), 2);
    assert_eq!(snap[0], user_msg("hello"));
    assert_eq!(snap[1], user_msg("world"));
}

#[test]
fn estimate_total_tokens_grows_with_history_and_is_zero_when_empty() {
    // Empty buffer => 0 tokens.
    let empty = ContextManager::new();
    assert_eq!(empty.estimate_total_tokens(), 0);

    // A small history has a small (non-zero) estimate.
    let mut small = ContextManager::new();
    small.record_items(vec![user_msg("hi")], TruncationPolicy::Bytes(usize::MAX));
    let small_tokens = small.estimate_total_tokens();
    assert!(small_tokens > 0, "non-empty history estimates > 0 tokens");

    // A much larger history estimates many more tokens (the byte/4 estimate
    // scales). ~40_000 bytes / 4 ≈ 10_000 tokens (plus the small JSON envelope).
    let mut big = ContextManager::new();
    big.record_items(
        vec![user_msg(&"x".repeat(40_000))],
        TruncationPolicy::Bytes(usize::MAX),
    );
    let big_tokens = big.estimate_total_tokens();
    assert!(
        big_tokens >= 10_000,
        "≈40k bytes should estimate ≥10k tokens, got {big_tokens}"
    );
    assert!(big_tokens > small_tokens);
}

#[test]
fn record_items_truncates_oversized_tool_output_at_policy_times_1_2() {
    // A 100-byte tool output under a Bytes(50) policy: process_item truncates
    // function-call outputs at policy*1.2 = 60 bytes, appending the elision
    // note. Plain (non-output) items are NOT truncated.
    let big = "x".repeat(100);
    let mut cm = ContextManager::new();
    cm.record_items(
        vec![function_call_output("call-1", &big)],
        TruncationPolicy::Bytes(50),
    );

    let recorded = &cm.items()[0];
    let out = recorded.get("output").and_then(|v| v.as_str()).unwrap();
    // 60-byte budget (50 * 1.2) of 'x' plus the truncation note.
    assert!(
        out.len() < big.len(),
        "expected truncation, got {}",
        out.len()
    );
    assert!(out.starts_with(&"x".repeat(60)));
    assert!(out.ends_with("[output truncated]"));

    // Sanity: an identical-size *non-output* item is untouched.
    let mut cm2 = ContextManager::new();
    cm2.record_items(vec![user_msg(&big)], TruncationPolicy::Bytes(50));
    assert_eq!(
        cm2.items()[0]
            .get("content")
            .and_then(|v| v.as_str())
            .unwrap(),
        big
    );
}

#[test]
fn snapshot_for_prompt_inserts_synthetic_output_for_orphan_call() {
    // A function_call with no matching output -> for_prompt inserts a synthetic
    // "aborted" output right after it.
    let mut cm = ContextManager::new();
    cm.record_items(
        vec![function_call("call-1", "do_thing")],
        TruncationPolicy::Tokens(1000),
    );
    let snap = cm.snapshot_for_prompt(true);
    assert_eq!(snap.len(), 2);
    assert_eq!(
        snap[1].get("type").and_then(|v| v.as_str()),
        Some("function_call_output")
    );
    assert_eq!(
        snap[1].get("call_id").and_then(|v| v.as_str()),
        Some("call-1")
    );
}

#[test]
fn snapshot_for_prompt_strips_images_when_unsupported() {
    let mut cm = ContextManager::new();
    cm.record_items(
        vec![json!({
            "role": "user",
            "content": [
                { "type": "input_text", "text": "look" },
                { "type": "input_image", "image_url": "data:image/png;base64,AAAA" },
            ],
        })],
        TruncationPolicy::Tokens(1000),
    );

    // supports_image = false strips the image part.
    let stripped = cm.snapshot_for_prompt(false);
    let parts = stripped[0]
        .get("content")
        .and_then(|v| v.as_array())
        .unwrap();
    assert_eq!(parts.len(), 1);
    assert_eq!(
        parts[0].get("type").and_then(|v| v.as_str()),
        Some("input_text")
    );

    // supports_image = true keeps it.
    let kept = cm.snapshot_for_prompt(true);
    let parts = kept[0].get("content").and_then(|v| v.as_array()).unwrap();
    assert_eq!(parts.len(), 2);
}

// ---------------------------------------------------------------------------
// lower_to_messages.
// ---------------------------------------------------------------------------

#[test]
fn lower_to_messages_user_assistant_tool_roundtrip() {
    let cm = ContextManager::new();
    let items = vec![
        user_msg("hi there"),
        assistant_with_tool_call("call-1", "search"),
        json!({ "role": "tool", "tool_call_id": "call-1", "name": "search", "content": "result text" }),
    ];
    let messages = cm.lower_to_messages(&items);
    assert_eq!(messages.len(), 3);

    // User message: one text part.
    assert_eq!(messages[0].role, MessageRole::User);
    assert_eq!(messages[0].content, vec![ContentPart::text("hi there")]);

    // Assistant: text part + tool-call part.
    assert_eq!(messages[1].role, MessageRole::Assistant);
    assert_eq!(messages[1].content.len(), 2);
    assert!(matches!(messages[1].content[0], ContentPart::Text { .. }));
    match &messages[1].content[1] {
        ContentPart::ToolCall { id, name, .. } => {
            assert_eq!(id, "call-1");
            assert_eq!(name, "search");
        }
        other => panic!("expected ToolCall, got {other:?}"),
    }

    // Tool: a single ToolResult linked by tool_call_id.
    assert_eq!(messages[2].role, MessageRole::Tool);
    match &messages[2].content[0] {
        ContentPart::ToolResult {
            tool_call_id,
            content,
            is_error,
        } => {
            assert_eq!(tool_call_id, "call-1");
            assert!(!is_error);
            assert_eq!(content, &vec![ContentPart::text("result text")]);
        }
        other => panic!("expected ToolResult, got {other:?}"),
    }
}

#[test]
fn lower_to_messages_preserves_tool_call_provider_metadata() {
    let cm = ContextManager::new();
    let messages = cm.lower_to_messages(&[assistant_with_provider_metadata("call-1", "search")]);

    assert_eq!(messages.len(), 1);
    match &messages[0].content[1] {
        ContentPart::ToolCall {
            provider_metadata: Some(metadata),
            ..
        } => {
            assert_eq!(
                metadata["google"]["thought_signature"],
                json!("sig-model-call")
            );
        }
        other => panic!("expected ToolCall with provider metadata, got {other:?}"),
    }
}

#[test]
fn lower_to_messages_skips_empty_envelopes() {
    let cm = ContextManager::new();
    // An assistant envelope with empty content and no tool calls -> skipped.
    let items = vec![
        json!({ "role": "assistant", "content": "" }),
        user_msg("real content"),
    ];
    let messages = cm.lower_to_messages(&items);
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].role, MessageRole::User);
}

#[test]
fn lower_to_messages_array_content_and_images() {
    let cm = ContextManager::new();
    let items = vec![json!({
        "role": "user",
            "content": [
                { "type": "input_text", "text": "describe" },
                { "type": "input_image", "image_url": "https://example.com/a.png", "detail": "original" },
            ],
    })];
    let messages = cm.lower_to_messages(&items);
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].content.len(), 2);
    assert!(matches!(messages[0].content[0], ContentPart::Text { .. }));
    match &messages[0].content[1] {
        ContentPart::Media { url, detail, .. } => {
            assert_eq!(url.as_deref(), Some("https://example.com/a.png"));
            assert_eq!(detail.as_deref(), Some("original"));
        }
        other => panic!("expected Media, got {other:?}"),
    }
}

#[test]
fn lower_to_messages_tool_content_preserves_images() {
    let cm = ContextManager::new();
    let items = vec![json!({
        "role": "tool",
        "tool_call_id": "call-view",
        "name": "view_image",
        "content": [
            { "type": "input_image", "image_url": "data:image/png;base64,AAAA", "detail": "high" },
        ],
    })];

    let messages = cm.lower_to_messages(&items);
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].role, MessageRole::Tool);
    let ContentPart::ToolResult { content, .. } = &messages[0].content[0] else {
        panic!("expected tool result, got {:?}", messages[0].content);
    };
    let ContentPart::Media {
        mime_type,
        data,
        url,
        detail,
    } = &content[0]
    else {
        panic!("expected media content, got {content:?}");
    };
    assert_eq!(mime_type, "image/png");
    assert_eq!(detail.as_deref(), Some("high"));
    assert_eq!(data.as_deref(), Some("AAAA"));
    assert!(url.is_none());
}

// ---------------------------------------------------------------------------
// token accounting: update_token_info accumulation + total_token_usage branches.
// ---------------------------------------------------------------------------

#[test]
fn update_token_info_accumulates_total_and_tracks_last() {
    let mut cm = ContextManager::new();
    let first = TokenUsage {
        input: 100,
        output: 20,
        total: 120,
        ..Default::default()
    };
    let second = TokenUsage {
        input: 50,
        output: 10,
        total: 60,
        ..Default::default()
    };
    cm.update_token_info(&first, Some(8192));
    cm.update_token_info(&second, None);

    // total accumulates field-wise; last is the most recent usage; window kept.
    // total_token_usage with NO items after the last model-generated item ==
    // info.last.total under both branches (no extra items to estimate).
    assert_eq!(cm.total_token_usage(true), 60);
    assert_eq!(cm.total_token_usage(false), 60);

    // Cross-check the accumulation directly via the same pure path.
    let expected = TokenUsageInfo::new_or_append(
        TokenUsageInfo::new_or_append(None, Some(&first), Some(8192)).as_ref(),
        Some(&second),
        None,
    )
    .unwrap();
    assert_eq!(expected.total.total, 180);
    assert_eq!(expected.last.total, 60);
    assert_eq!(expected.model_context_window, Some(8192));
}

#[test]
fn total_token_usage_branches_on_server_reasoning_included() {
    // Buffer: a reasoning item BEFORE the last model-generated item, then an
    // assistant (the last model-generated item), then a trailing user item.
    //
    //   * server_reasoning_included = true  -> last + tokens(after-last-model-item)
    //   * server_reasoning_included = false -> last + non_last_reasoning + after
    //
    // The two branches therefore differ by exactly the *early* reasoning item's
    // estimated token count. `estimate_reasoning_length(len) = len*3/4 - 650`
    // saturates to zero below ~868 bytes, so the reasoning text must be long
    // enough to estimate strictly above zero for the branches to diverge.
    let early_reasoning_text = "z".repeat(4_000);
    let early_reasoning = json!({ "type": "reasoning", "text": early_reasoning_text });
    let mut cm = ContextManager::new();
    cm.record_items(
        vec![
            early_reasoning.clone(),
            json!({ "role": "assistant", "content": "the answer" }),
            user_msg("a follow up question after the model turn"),
        ],
        TruncationPolicy::Tokens(100_000),
    );
    let last = TokenUsage {
        total: 1000,
        ..Default::default()
    };
    cm.update_token_info(&last, Some(200_000));

    let with_reasoning = cm.total_token_usage(true);
    let without_reasoning = cm.total_token_usage(false);

    // Both branches include `last` (1000) + the trailing user item's tokens.
    assert!(with_reasoning >= 1000);

    // The branches differ by exactly the early reasoning item's token estimate
    // (counted only on the non-server-reasoning branch). That estimate must be
    // > 0 for the assertion to be meaningful.
    let early_reasoning_tokens = assembly::estimate_item_token_count(&early_reasoning);
    assert!(
        early_reasoning_tokens > 0,
        "reasoning estimate should be positive: {early_reasoning_tokens}"
    );
    assert_eq!(
        without_reasoning - with_reasoning,
        early_reasoning_tokens,
        "branch delta should equal the early reasoning item's tokens \
         (without={without_reasoning} with={with_reasoning})"
    );

    // Independently verify against the pure assembly core over the same buffer.
    let info = TokenUsageInfo::new_or_append(None, Some(&last), Some(200_000));
    assert_eq!(
        with_reasoning,
        assembly::total_token_usage(cm.items(), info.as_ref(), true)
    );
    assert_eq!(
        without_reasoning,
        assembly::total_token_usage(cm.items(), info.as_ref(), false)
    );
}

#[test]
fn total_token_usage_zero_when_no_info_and_no_items() {
    let cm = ContextManager::new();
    assert_eq!(cm.total_token_usage(true), 0);
    assert_eq!(cm.total_token_usage(false), 0);
}

#[test]
fn set_token_usage_full_installs_window_and_preserves_usage() {
    let mut cm = ContextManager::new();
    // No prior usage: installs a full-window usage snapshot.
    cm.set_token_usage_full(4096);
    assert_eq!(cm.breakdown().last_api_response_total_tokens, 4096);

    // With prior usage, the window fill records only the delta as the last usage.
    let mut cm2 = ContextManager::new();
    cm2.update_token_info(
        &TokenUsage {
            total: 321,
            ..Default::default()
        },
        None,
    );
    cm2.set_token_usage_full(4096);
    assert_eq!(cm2.breakdown().last_api_response_total_tokens, 3775);
}

// ---------------------------------------------------------------------------
// breakdown + history_version + persist_snapshot.
// ---------------------------------------------------------------------------

#[test]
fn breakdown_reports_bytes_and_tokens() {
    let mut cm = ContextManager::new();
    cm.record_items(
        vec![user_msg("some content here")],
        TruncationPolicy::Tokens(1000),
    );
    let bd = cm.breakdown();
    assert!(bd.all_history_items_model_visible_bytes > 0);
    // No model-generated item -> everything is "after", so the since-last
    // estimates cover the whole (single) item.
    assert!(bd.estimated_tokens_since_last_api_response > 0);
    assert!(bd.estimated_bytes_since_last_api_response > 0);
}

#[test]
fn history_version_is_monotonic_on_mutation() {
    let mut cm = ContextManager::new();
    assert_eq!(cm.history_version(), 0);
    cm.record_items(vec![user_msg("a")], TruncationPolicy::Tokens(100));
    assert_eq!(cm.history_version(), 1);
    cm.update_token_info(&TokenUsage::default(), None);
    assert_eq!(cm.history_version(), 2);
    cm.set_token_usage_full(1024);
    assert_eq!(cm.history_version(), 3);
    // Read-only calls don't bump the version.
    let _ = cm.snapshot_for_prompt(true);
    let _ = cm.total_token_usage(true);
    assert_eq!(cm.history_version(), 3);
}

#[tokio::test]
async fn persist_snapshot_is_awaitable_noop() {
    let mut cm = ContextManager::new();
    cm.record_items(vec![user_msg("x")], TruncationPolicy::Tokens(100));
    // Write-only contract: it returns Ok and never reads back / mutates state.
    let before = cm.history_version();
    cm.persist_snapshot().await.expect("persist is a no-op Ok");
    assert_eq!(cm.history_version(), before);
}

// ---------------------------------------------------------------------------
// A4/A6 inject golden (the parity-debt resolution).
// ---------------------------------------------------------------------------

#[test]
fn inject_permissions_golden_matches_legacy_shape() {
    // `inject::build_context_message(Permissions, text)` must equal the legacy
    // `browser-use-core::permissions_context_message` Value shape
    // (`{role:"developer", name:"permissions_context", content:[{input_text}]}`).
    // This is the single-source-of-truth golden for the A4/A6 reconciliation.
    let legacy_permissions_context_message = json!({
        "role": "developer",
        "name": "permissions_context",
        "content": [{ "type": "input_text", "text": "Default permissions apply." }],
    });
    assert_eq!(
        build_context_message(
            ContextKind::Permissions,
            "Default permissions apply.".to_string()
        ),
        legacy_permissions_context_message
    );
}
