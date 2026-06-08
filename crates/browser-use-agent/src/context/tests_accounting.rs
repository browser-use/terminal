//! Pure parity tests for the context accounting / assembly / normalize /
//! image-estimate surface (WP-A3).

use serde_json::json;

use super::accounting::{
    approx_bytes_for_tokens, approx_tokens_from_byte_count_i64,
    estimate_encrypted_function_output_length, estimate_reasoning_length, TokenUsage,
    TokenUsageInfo,
};
use super::assembly::{
    ensure_call_outputs_present, estimate_item_model_visible_bytes, estimate_item_token_count,
    for_prompt, is_api_message, is_codex_generated_item, is_model_generated_item,
    is_user_turn_boundary, process_item, remove_corresponding_for, remove_orphan_outputs,
    strip_images_when_unsupported, total_token_usage, total_token_usage_breakdown, truncate_text,
    TruncationPolicy,
};
use super::constants::{RESIZED_IMAGE_BYTES_ESTIMATE, WORKSPACE_CONTEXT_MESSAGE_NAME};
use super::image_estimate::{
    base64_decode, estimate_image_context_bytes, parse_base64_image_data_url, sha1,
    ImageEstimateCache,
};
use super::Item;
use browser_use_llm::schema::Usage;

// ---------------------------------------------------------------------------
// Byte / token heuristics (exact values).
// ---------------------------------------------------------------------------

#[test]
fn approx_tokens_is_div_ceil_4_clamped() {
    assert_eq!(approx_tokens_from_byte_count_i64(0), 0);
    assert_eq!(approx_tokens_from_byte_count_i64(-100), 0);
    assert_eq!(approx_tokens_from_byte_count_i64(1), 1);
    assert_eq!(approx_tokens_from_byte_count_i64(4), 1);
    assert_eq!(approx_tokens_from_byte_count_i64(5), 2);
    assert_eq!(approx_tokens_from_byte_count_i64(8), 2);
    assert_eq!(approx_tokens_from_byte_count_i64(9), 3);
}

#[test]
fn approx_bytes_for_tokens_is_times_4() {
    assert_eq!(approx_bytes_for_tokens(0), 0);
    assert_eq!(approx_bytes_for_tokens(1), 4);
    assert_eq!(approx_bytes_for_tokens(100), 400);
}

#[test]
fn reasoning_length_is_three_quarters_minus_650_saturating() {
    // Tiny inputs saturate to zero (3/4 of small < 650).
    assert_eq!(estimate_reasoning_length(0), 0);
    assert_eq!(estimate_reasoning_length(1), 0);
    assert_eq!(estimate_reasoning_length(650), 0);
    assert_eq!(estimate_reasoning_length(800), 0); // 800*3/4 = 600 -> sat 0
    assert_eq!(estimate_reasoning_length(867), 0); // 867*3/4 = 650 -> 0
                                                   // 1000*3/4 = 750; 750-650 = 100.
    assert_eq!(estimate_reasoning_length(1000), 100);
    // 4000*3/4 = 3000; 3000-650 = 2350.
    assert_eq!(estimate_reasoning_length(4000), 2350);
}

#[test]
fn encrypted_output_length_is_nine_sixteenths_div_ceil() {
    assert_eq!(estimate_encrypted_function_output_length(0), 0);
    // 1*9 = 9; div_ceil 16 = 1.
    assert_eq!(estimate_encrypted_function_output_length(1), 1);
    // 16*9 = 144; div_ceil 16 = 9.
    assert_eq!(estimate_encrypted_function_output_length(16), 9);
    // 100*9 = 900; div_ceil 16 = 57 (900/16 = 56.25).
    assert_eq!(estimate_encrypted_function_output_length(100), 57);
}

// ---------------------------------------------------------------------------
// TokenUsage::from_llm_usage — total fallback excludes cached.
// ---------------------------------------------------------------------------

#[test]
fn from_llm_usage_uses_server_total_when_present() {
    let u = Usage {
        input_tokens: 100,
        cached_input_tokens: 40,
        cache_creation_input_tokens: 0,
        output_tokens: 30,
        reasoning_output_tokens: 10,
        total_tokens: 123,
    };
    let tu = TokenUsage::from_llm_usage(&u);
    assert_eq!(tu.input, 100);
    assert_eq!(tu.cached_input, 40);
    assert_eq!(tu.output, 30);
    assert_eq!(tu.reasoning_output, 10);
    assert_eq!(tu.total, 123);
}

#[test]
fn from_llm_usage_total_fallback_excludes_cached() {
    let u = Usage {
        input_tokens: 100,
        cached_input_tokens: 40,
        cache_creation_input_tokens: 0,
        output_tokens: 30,
        reasoning_output_tokens: 10,
        total_tokens: 0,
    };
    let tu = TokenUsage::from_llm_usage(&u);
    // Fallback = input + output + reasoning_output = 140, NOT including cached.
    assert_eq!(tu.total, 140);
}

#[test]
fn token_usage_add_is_fieldwise() {
    let a = TokenUsage {
        input: 1,
        cached_input: 2,
        output: 3,
        reasoning_output: 4,
        total: 5,
    };
    let b = TokenUsage {
        input: 10,
        cached_input: 20,
        output: 30,
        reasoning_output: 40,
        total: 50,
    };
    let c = a.add(&b);
    assert_eq!(c.input, 11);
    assert_eq!(c.cached_input, 22);
    assert_eq!(c.output, 33);
    assert_eq!(c.reasoning_output, 44);
    assert_eq!(c.total, 55);
}

#[test]
fn token_usage_info_new_or_append() {
    // None + None => None.
    assert_eq!(TokenUsageInfo::new_or_append(None, None, None), None);

    // Fresh init.
    let last = TokenUsage {
        total: 100,
        ..Default::default()
    };
    let info = TokenUsageInfo::new_or_append(None, Some(&last), Some(8192)).unwrap();
    assert_eq!(info.total.total, 100);
    assert_eq!(info.last.total, 100);
    assert_eq!(info.model_context_window, Some(8192));

    // Append accumulates total, replaces last, preserves window when None.
    let next = TokenUsage {
        total: 50,
        ..Default::default()
    };
    let info2 = TokenUsageInfo::new_or_append(Some(&info), Some(&next), None).unwrap();
    assert_eq!(info2.total.total, 150);
    assert_eq!(info2.last.total, 50);
    assert_eq!(info2.model_context_window, Some(8192));
}

#[test]
fn token_usage_info_window_helpers() {
    let mut info = TokenUsageInfo::full_context_window(4096);
    assert_eq!(info.model_context_window, Some(4096));
    assert_eq!(info.total.total, 4096);
    assert_eq!(info.last.total, 4096);
    info.fill_to_context_window(9000);
    assert_eq!(info.model_context_window, Some(9000));
    assert_eq!(info.total.total, 9000);
    assert_eq!(info.last.total, 4904);
}

// ---------------------------------------------------------------------------
// Item estimation.
// ---------------------------------------------------------------------------

#[test]
fn estimate_plain_message_uses_serialized_len() {
    let item: Item = json!({ "type": "message", "role": "user", "content": "hello" });
    let serialized = serde_json::to_string(&item).unwrap();
    assert_eq!(
        estimate_item_model_visible_bytes(&item),
        serialized.len() as i64
    );
    assert_eq!(
        estimate_item_token_count(&item),
        approx_tokens_from_byte_count_i64(serialized.len() as i64)
    );
}

#[test]
fn estimate_reasoning_item_uses_reasoning_length() {
    let text = "x".repeat(4000);
    let item: Item = json!({ "type": "reasoning", "text": text });
    // 4000*3/4 - 650 = 2350.
    assert_eq!(estimate_item_model_visible_bytes(&item), 2350);
}

#[test]
fn estimate_image_item_swaps_payload_for_estimate() {
    // A non-original-detail data url => replacement is RESIZED estimate.
    let data_url = format!("data:image/png;base64,{}", "A".repeat(1000));
    let item: Item = json!({
        "type": "message",
        "role": "user",
        "content": [ { "type": "input_image", "image_url": data_url } ],
    });
    let raw = serde_json::to_string(&item).unwrap().len() as i64;
    let url_len = (data_url.len()) as i64;
    let expected = raw - url_len + RESIZED_IMAGE_BYTES_ESTIMATE;
    assert_eq!(estimate_item_model_visible_bytes(&item), expected);
}

#[test]
fn estimate_encrypted_output_swaps_blob_for_estimate() {
    let blob = "Z".repeat(160);
    let item: Item = json!({
        "type": "function_call_output",
        "call_id": "c1",
        "output": { "encrypted_content": blob },
    });
    let raw = serde_json::to_string(&item).unwrap().len() as i64;
    let blob_len = 160i64;
    // 160*9 = 1440; div_ceil 16 = 90.
    let expected = raw - blob_len + 90;
    assert_eq!(estimate_item_model_visible_bytes(&item), expected);
}

// ---------------------------------------------------------------------------
// Classification predicates.
// ---------------------------------------------------------------------------

#[test]
fn classification_predicates() {
    let user: Item = json!({ "type": "message", "role": "user", "content": "hi" });
    let assistant: Item = json!({ "type": "message", "role": "assistant", "content": "yo" });
    let system: Item = json!({ "type": "message", "role": "system", "content": "sys" });
    let call: Item = json!({ "type": "function_call", "call_id": "c1" });
    let output: Item = json!({ "type": "function_call_output", "call_id": "c1", "output": "ok" });
    let reasoning: Item = json!({ "type": "reasoning", "text": "think" });

    assert!(is_api_message(&user));
    assert!(is_api_message(&assistant));
    assert!(!is_api_message(&system));
    assert!(is_api_message(&call));

    assert!(is_model_generated_item(&assistant));
    assert!(is_model_generated_item(&call));
    assert!(is_model_generated_item(&reasoning));
    assert!(!is_model_generated_item(&user));
    assert!(!is_model_generated_item(&output));

    assert!(is_user_turn_boundary(&user));
    assert!(!is_user_turn_boundary(&assistant));

    assert!(is_codex_generated_item(&call));
    assert!(is_codex_generated_item(&output));
    assert!(is_codex_generated_item(&reasoning));
    assert!(!is_codex_generated_item(&user));
}

// ---------------------------------------------------------------------------
// Truncation policy + process_item (×1.2).
// ---------------------------------------------------------------------------

#[test]
fn truncation_policy_byte_budget_and_scale() {
    assert_eq!(TruncationPolicy::Bytes(100).byte_budget(), 100);
    assert_eq!(TruncationPolicy::Tokens(100).byte_budget(), 400);
    assert_eq!(TruncationPolicy::Bytes(100).scale(1.2).byte_budget(), 120);
    assert_eq!(TruncationPolicy::Tokens(10).scale(1.2).byte_budget(), 48); // 10*1.2=12 tokens *4
}

#[test]
fn truncate_text_respects_budget() {
    assert_eq!(
        truncate_text("short", TruncationPolicy::Bytes(100)),
        "short"
    );
    let t = truncate_text("abcdefghij", TruncationPolicy::Bytes(4));
    assert!(t.starts_with("abcd"));
    assert!(t.len() > 4); // includes elision note
    assert_eq!(truncate_text("abc", TruncationPolicy::Bytes(0)), "");
}

#[test]
fn process_item_truncates_oversized_tool_output_at_policy_times_1_2() {
    // byte_budget 100 -> effective cap 120 after *1.2.
    let big = "y".repeat(500);
    let item: Item = json!({
        "type": "function_call_output",
        "call_id": "c1",
        "output": big,
    });
    let out = process_item(&item, TruncationPolicy::Bytes(100));
    let truncated = out.get("output").and_then(|v| v.as_str()).unwrap();
    // The kept prefix is 120 bytes (the ×1.2 budget) before the note.
    assert!(truncated.starts_with(&"y".repeat(120)));
    assert!(!truncated.starts_with(&"y".repeat(121)));
    assert!(truncated.len() < 500);
}

#[test]
fn process_item_leaves_small_output_unchanged() {
    let item: Item = json!({
        "type": "function_call_output",
        "call_id": "c1",
        "output": "small",
    });
    let out = process_item(&item, TruncationPolicy::Bytes(100));
    assert_eq!(out.get("output").and_then(|v| v.as_str()), Some("small"));
}

#[test]
fn process_item_ignores_non_outputs() {
    let item: Item = json!({ "type": "message", "role": "user", "content": "x".repeat(500) });
    let out = process_item(&item, TruncationPolicy::Bytes(1));
    assert_eq!(out, item);
}

// ---------------------------------------------------------------------------
// Normalization + for_prompt.
// ---------------------------------------------------------------------------

#[test]
fn ensure_call_outputs_present_appends_aborted_placeholder() {
    let mut items: Vec<Item> = vec![
        json!({ "type": "function_call", "call_id": "c1" }),
        json!({ "type": "function_call", "call_id": "c2" }),
        json!({ "type": "function_call_output", "call_id": "c1", "output": "done" }),
    ];
    ensure_call_outputs_present(&mut items);
    // c2 had no output; codex inserts a synthetic one immediately after the
    // c2 call (index 1), so the placeholder lands at index 2.
    assert_eq!(items.len(), 4);
    let placeholder = &items[2];
    assert_eq!(
        placeholder.get("type").and_then(|v| v.as_str()),
        Some("function_call_output")
    );
    assert_eq!(
        placeholder.get("call_id").and_then(|v| v.as_str()),
        Some("c2")
    );
    assert_eq!(
        placeholder
            .get("output")
            .and_then(|o| o.get("content"))
            .and_then(|v| v.as_str()),
        Some("aborted")
    );
}

#[test]
fn remove_orphan_outputs_drops_unanchored() {
    let mut items: Vec<Item> = vec![
        json!({ "type": "function_call", "call_id": "c1" }),
        json!({ "type": "function_call_output", "call_id": "c1", "output": "ok" }),
        json!({ "type": "function_call_output", "call_id": "orphan", "output": "?" }),
    ];
    remove_orphan_outputs(&mut items);
    assert_eq!(items.len(), 2);
    assert!(items
        .iter()
        .all(|i| i.get("call_id").and_then(|v| v.as_str()) != Some("orphan")));
}

#[test]
fn strip_images_removes_image_parts_when_unsupported() {
    let mut items: Vec<Item> = vec![json!({
        "type": "message",
        "role": "user",
        "content": [
            { "type": "input_text", "text": "look" },
            { "type": "input_image", "image_url": "data:image/png;base64,AAAA" },
        ],
    })];
    // supports_image = true => no change.
    let snapshot = items.clone();
    strip_images_when_unsupported(true, &mut items);
    assert_eq!(items, snapshot);

    // supports_image = false => image part removed, text retained.
    strip_images_when_unsupported(false, &mut items);
    let content = items[0].get("content").and_then(|v| v.as_array()).unwrap();
    assert_eq!(content.len(), 1);
    assert_eq!(
        content[0].get("type").and_then(|v| v.as_str()),
        Some("input_text")
    );
}

#[test]
fn remove_corresponding_for_drops_call_and_output_pair() {
    let mut items: Vec<Item> = vec![
        json!({ "type": "function_call", "call_id": "c1" }),
        json!({ "type": "function_call_output", "call_id": "c1", "output": "ok" }),
        json!({ "type": "function_call", "call_id": "c2" }),
        json!({ "type": "function_call_output", "call_id": "c2", "output": "ok" }),
    ];
    let removed: Item = json!({ "type": "function_call", "call_id": "c1" });
    remove_corresponding_for(&mut items, &removed);
    assert_eq!(items.len(), 2);
    assert!(items
        .iter()
        .all(|i| i.get("call_id").and_then(|v| v.as_str()) == Some("c2")));
}

#[test]
fn for_prompt_removes_orphans_and_strips_images() {
    let items: Vec<Item> = vec![
        json!({
            "type": "message",
            "role": "user",
            "content": [
                { "type": "input_text", "text": "hi" },
                { "type": "input_image", "image_url": "data:image/png;base64,AAAA" },
            ],
        }),
        // Orphan output (no matching call) — should be removed.
        json!({ "type": "function_call_output", "call_id": "ghost", "output": "?" }),
    ];
    let out = for_prompt(items, /* supports_image = */ false);
    // Orphan output removed.
    assert!(out
        .iter()
        .all(|i| i.get("call_id").and_then(|v| v.as_str()) != Some("ghost")));
    // Image stripped.
    let content = out[0].get("content").and_then(|v| v.as_array()).unwrap();
    assert_eq!(content.len(), 1);
    assert_eq!(
        content[0].get("type").and_then(|v| v.as_str()),
        Some("input_text")
    );
}

#[test]
fn for_prompt_adds_missing_output_then_keeps_it() {
    let items: Vec<Item> = vec![json!({ "type": "function_call", "call_id": "c1" })];
    let out = for_prompt(items, true);
    // ensure_call_outputs_present added a synthetic output for c1; it has a
    // matching call, so remove_orphan_outputs keeps it.
    assert_eq!(out.len(), 2);
    assert_eq!(
        out[1].get("type").and_then(|v| v.as_str()),
        Some("function_call_output")
    );
}

// ---------------------------------------------------------------------------
// total_token_usage — branch on server_reasoning_included.
// ---------------------------------------------------------------------------

#[test]
fn total_token_usage_branches_on_server_reasoning_included() {
    // Layout: [reasoning(pre), assistant(model), user_followup]
    // reasoning(pre) is BEFORE the last model item => counted only when
    // reasoning is NOT server-included.
    let pre_reasoning_text = "r".repeat(4000); // estimate 2350 bytes -> 588 tokens
    let items: Vec<Item> = vec![
        json!({ "type": "reasoning", "text": pre_reasoning_text }),
        json!({ "type": "message", "role": "assistant", "content": "answer" }),
        json!({ "type": "message", "role": "user", "content": "follow up" }),
    ];

    let info = TokenUsageInfo {
        last: TokenUsage {
            total: 1000,
            ..Default::default()
        },
        ..Default::default()
    };

    // "after last model item" = the trailing user message.
    let after_tokens = estimate_item_token_count(&items[2]);
    let reasoning_tokens = estimate_item_token_count(&items[0]);
    assert!(reasoning_tokens > 0);

    // server_reasoning_included = true: last + after only.
    let included = total_token_usage(&items, Some(&info), true);
    assert_eq!(included, 1000 + after_tokens);

    // server_reasoning_included = false: last + non_last_reasoning + after.
    let excluded = total_token_usage(&items, Some(&info), false);
    assert_eq!(excluded, 1000 + reasoning_tokens + after_tokens);

    // The two branches differ exactly by the pre-model reasoning tokens.
    assert_eq!(excluded - included, reasoning_tokens);
}

#[test]
fn total_token_usage_no_info_is_zero_last() {
    let items: Vec<Item> = vec![json!({ "type": "message", "role": "user", "content": "hi" })];
    // No model-generated item => everything is "after" (start = 0).
    let after = estimate_item_token_count(&items[0]);
    assert_eq!(total_token_usage(&items, None, true), after);
}

#[test]
fn total_token_usage_breakdown_fields() {
    let items: Vec<Item> = vec![
        json!({ "type": "message", "role": "assistant", "content": "a" }),
        json!({ "type": "message", "role": "user", "content": "b" }),
    ];
    let info = TokenUsageInfo {
        last: TokenUsage {
            total: 777,
            ..Default::default()
        },
        ..Default::default()
    };
    let bd = total_token_usage_breakdown(&items, Some(&info));
    assert_eq!(bd.last_api_response_total_tokens, 777);

    let mut all_bytes: i64 = 0;
    for item in &items {
        all_bytes += estimate_item_model_visible_bytes(item);
    }
    assert_eq!(bd.all_history_items_model_visible_bytes, all_bytes);

    // After last model item = the trailing user message.
    assert_eq!(
        bd.estimated_tokens_since_last_api_response,
        estimate_item_token_count(&items[1])
    );
    assert_eq!(
        bd.estimated_bytes_since_last_api_response,
        estimate_item_model_visible_bytes(&items[1])
    );
}

// ---------------------------------------------------------------------------
// Image estimate helpers.
// ---------------------------------------------------------------------------

#[test]
fn base64_decode_roundtrip_and_length() {
    // "hello" -> "aGVsbG8="
    assert_eq!(base64_decode("aGVsbG8="), Some(b"hello".to_vec()));
    // "hi" -> "aGk="
    assert_eq!(base64_decode("aGk="), Some(b"hi".to_vec()));
    // malformed
    assert_eq!(base64_decode("abc"), None); // not multiple of 4
    assert_eq!(base64_decode("****"), None); // illegal chars
    assert_eq!(base64_decode(""), None);
}

#[test]
fn parse_base64_image_data_url_extracts_mime_and_bytes() {
    let url = "data:image/png;base64,aGVsbG8="; // "hello"
    let parsed = parse_base64_image_data_url(url).unwrap();
    assert_eq!(parsed.mime, "image/png");
    assert_eq!(parsed.data, b"hello".to_vec());
    assert!(parse_base64_image_data_url("https://example.com/x.png").is_none());
}

#[test]
fn estimate_image_context_bytes_defaults_to_resized() {
    let url = "data:image/png;base64,AAAA";
    assert_eq!(
        estimate_image_context_bytes(url),
        RESIZED_IMAGE_BYTES_ESTIMATE
    );
}

#[test]
fn estimate_image_context_bytes_original_detail_uses_patches() {
    // Build a minimal PNG header for a 64x32 image, base64-encoded, with the
    // original-detail marker in the URL.
    let png = make_png_header(64, 32);
    let b64 = base64_encode(&png);
    // The detail marker sits before the base64 payload so it does not corrupt
    // the payload that `decode_image_dimensions` splits out on "base64,".
    let url = format!("data:image/png;detail=original;base64,{b64}");
    // ceil(64/32)=2, ceil(32/32)=1 => 2 patches => 2*4 = 8 bytes.
    assert_eq!(estimate_image_context_bytes(&url), 8);
}

#[test]
fn image_estimate_cache_is_deterministic_lru() {
    let mut cache = ImageEstimateCache::new(2);
    assert!(cache.is_empty());
    assert_eq!(cache.get_or_insert_with("a", || 1), 1);
    assert_eq!(cache.get_or_insert_with("b", || 2), 2);
    // Cache hit returns stored value, not recomputed.
    assert_eq!(cache.get_or_insert_with("a", || 999), 1);
    assert_eq!(cache.len(), 2);
    // Insert third evicts LRU. "a" was just touched, so "b" is LRU.
    assert_eq!(cache.get_or_insert_with("c", || 3), 3);
    assert_eq!(cache.get("b"), None);
    assert_eq!(cache.get("a"), Some(1));
    assert_eq!(cache.get("c"), Some(3));
}

#[test]
fn sha1_known_vector() {
    // SHA1("abc") = a9993e364706816aba3e25717850c26c9cd0d89d
    let digest = sha1(b"abc");
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    assert_eq!(hex, "a9993e364706816aba3e25717850c26c9cd0d89d");
}

#[test]
fn constants_message_name_values() {
    assert_eq!(WORKSPACE_CONTEXT_MESSAGE_NAME, "workspace_context");
}

// --- test helpers -------------------------------------------------------

/// Build a minimal valid PNG header (signature + IHDR width/height) padded to
/// at least 24 bytes so `parse_png_dimensions` accepts it.
fn make_png_header(width: u32, height: u32) -> Vec<u8> {
    let mut v = Vec::new();
    v.extend_from_slice(b"\x89PNG\r\n\x1a\n"); // 8-byte signature
    v.extend_from_slice(&[0, 0, 0, 13]); // IHDR length
    v.extend_from_slice(b"IHDR"); // chunk type (bytes 12..16)
    v.extend_from_slice(&width.to_be_bytes()); // bytes 16..20
    v.extend_from_slice(&height.to_be_bytes()); // bytes 20..24
    v
}

/// Minimal standard-alphabet base64 encoder for test fixtures.
fn base64_encode(data: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in data.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | (b[2] as u32);
        out.push(ALPHABET[((n >> 18) & 63) as usize] as char);
        out.push(ALPHABET[((n >> 12) & 63) as usize] as char);
        if chunk.len() > 1 {
            out.push(ALPHABET[((n >> 6) & 63) as usize] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(ALPHABET[(n & 63) as usize] as char);
        } else {
            out.push('=');
        }
    }
    out
}
