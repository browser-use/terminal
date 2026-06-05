//! Pure reducer tests (WP-A6): build `Vec<EventRecord>` fixtures and assert the
//! reconstructed `Vec<Value>` provider messages match codex/core parity.

use super::reconstruct::{
    is_real_user_event, provider_history_has_open_turn, provider_messages_from_events,
};
use browser_use_protocol::EventRecord;
use serde_json::{json, Value};

fn event(seq: i64, ty: &str, payload: Value) -> EventRecord {
    EventRecord {
        seq,
        id: format!("e{seq}"),
        session_id: "s1".to_string(),
        ts_ms: seq,
        event_type: ty.to_string(),
        payload,
    }
}

#[test]
fn simple_user_assistant_turn() {
    let events = vec![
        event(1, "session.input", json!({ "text": "hello there" })),
        event(
            2,
            "model.response.output_item",
            json!({
                "item": {
                    "type": "message",
                    "role": "assistant",
                    "content": [{ "type": "output_text", "text": "hi back" }],
                }
            }),
        ),
        event(3, "session.done", json!({})),
    ];

    let messages = provider_messages_from_events(&events);
    assert_eq!(messages.len(), 2, "user + assistant: {messages:#?}");

    assert_eq!(
        messages[0].get("role").and_then(Value::as_str),
        Some("user")
    );
    assert_eq!(
        messages[0].get("content").and_then(Value::as_str),
        Some("hello there")
    );

    assert_eq!(
        messages[1].get("role").and_then(Value::as_str),
        Some("assistant")
    );
    assert_eq!(
        messages[1].get("content").and_then(Value::as_str),
        Some("hi back")
    );
    // No tool calls -> `tool_calls` is removed by normalization.
    assert!(messages[1].get("tool_calls").is_none());
}

#[test]
fn inter_agent_mailbox_input_renders_as_direct_parent_task() {
    let events = vec![event(
        1,
        "agent.mailbox_input",
        json!({
            "author_path": "/root",
            "recipient_path": "/root/dev_env",
            "content": "Inspect the dev environment only.",
            "trigger_turn": true,
        }),
    )];

    let messages = provider_messages_from_events(&events);

    assert_eq!(messages.len(), 1, "messages: {messages:#?}");
    assert_eq!(
        messages[0].get("role").and_then(Value::as_str),
        Some("user")
    );
    assert_eq!(
        messages[0].get("content").and_then(Value::as_str),
        Some("Direct task from parent /root to you /root/dev_env:\nInspect the dev environment only.")
    );
}

#[test]
fn turn_with_tool_call_and_output() {
    let events = vec![
        event(1, "session.input", json!({ "text": "run the tool" })),
        event(
            2,
            "model.tool_call",
            json!({ "id": "call_1", "name": "do_thing", "arguments": { "x": 1 } }),
        ),
        event(
            3,
            "tool.output",
            json!({ "tool_call_id": "call_1", "name": "do_thing", "output": "tool result" }),
        ),
        event(4, "session.done", json!({})),
    ];

    let messages = provider_messages_from_events(&events);
    // user, assistant(tool_call), tool(output)
    assert_eq!(messages.len(), 3, "messages: {messages:#?}");

    assert_eq!(
        messages[0].get("role").and_then(Value::as_str),
        Some("user")
    );

    let assistant = &messages[1];
    assert_eq!(
        assistant.get("role").and_then(Value::as_str),
        Some("assistant")
    );
    let calls = assistant
        .get("tool_calls")
        .and_then(Value::as_array)
        .expect("assistant tool_calls");
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].get("id").and_then(Value::as_str), Some("call_1"));
    assert_eq!(
        calls[0].get("name").and_then(Value::as_str),
        Some("do_thing")
    );

    let tool = &messages[2];
    assert_eq!(tool.get("role").and_then(Value::as_str), Some("tool"));
    assert_eq!(
        tool.get("tool_call_id").and_then(Value::as_str),
        Some("call_1")
    );
    assert_eq!(
        tool.get("content").and_then(Value::as_str),
        Some("tool result")
    );
}

#[test]
fn tool_output_event_uses_structured_browser_script_fallback_text() {
    let events = vec![
        event(1, "session.input", json!({ "text": "open page" })),
        event(
            2,
            "model.tool_call",
            json!({
                "id": "call_browser",
                "name": "browser_script",
                "arguments": { "code": "emit_output(page_info(), label='page_info')" }
            }),
        ),
        event(
            3,
            "tool.output",
            json!({
                "tool_call_id": "call_browser",
                "name": "browser_script",
                "text": "",
                "summary": [{
                    "kind": "page",
                    "output_label": "page_info",
                    "title": "Example Domain",
                    "url": "https://example.com"
                }],
                "outputs": [{
                    "label": "page_info",
                    "value": {
                        "title": "Example Domain",
                        "url": "https://example.com"
                    }
                }]
            }),
        ),
        event(4, "session.done", json!({})),
    ];

    let messages = provider_messages_from_events(&events);
    assert_eq!(messages.len(), 3, "messages: {messages:#?}");
    let tool = &messages[2];
    assert_eq!(tool.get("role").and_then(Value::as_str), Some("tool"));
    let content = tool
        .get("content")
        .and_then(Value::as_str)
        .expect("structured fallback content");
    assert!(content.contains("summary:"));
    assert!(content.contains("outputs:"));
    assert!(content.contains("Example Domain"));
    assert!(!content.trim().is_empty());
}

#[test]
fn tool_output_event_preserves_image_content() {
    let events = vec![
        event(1, "session.input", json!({ "text": "load image" })),
        event(
            2,
            "model.tool_call",
            json!({ "id": "call_view", "name": "view_image", "arguments": { "path": "pic.png" } }),
        ),
        event(
            3,
            "tool.output",
            json!({
                "tool_call_id": "call_view",
                "name": "view_image",
                "text": "[media: image/png]",
                "content": [
                    { "type": "input_image", "image_url": "data:image/png;base64,AAAA", "detail": "high" }
                ],
            }),
        ),
        event(4, "session.done", json!({})),
    ];

    let messages = provider_messages_from_events(&events);
    assert_eq!(messages.len(), 3, "messages: {messages:#?}");
    let tool = &messages[2];
    assert_eq!(tool.get("role").and_then(Value::as_str), Some("tool"));
    let content = tool
        .get("content")
        .and_then(Value::as_array)
        .expect("tool image content array");
    assert_eq!(content[0]["type"], "input_image");
    assert_eq!(content[0]["image_url"], "data:image/png;base64,AAAA");
}

#[test]
fn tool_failed_event_preserves_image_content() {
    let events = vec![
        event(1, "session.input", json!({ "text": "inspect page" })),
        event(
            2,
            "model.tool_call",
            json!({ "id": "call_browser", "name": "browser_script", "arguments": { "action": "start" } }),
        ),
        event(
            3,
            "tool.failed",
            json!({
                "tool_call_id": "call_browser",
                "name": "browser_script",
                "error": "RuntimeError: failed after screenshot",
                "content": [
                    { "type": "input_text", "text": "browser_script failed: RuntimeError: failed after screenshot" },
                    { "type": "input_image", "image_url": "data:image/png;base64,AAAA", "detail": "high" }
                ],
            }),
        ),
        event(4, "session.done", json!({})),
    ];

    let messages = provider_messages_from_events(&events);
    assert_eq!(messages.len(), 3, "messages: {messages:#?}");
    let tool = &messages[2];
    assert_eq!(tool.get("role").and_then(Value::as_str), Some("tool"));
    let content = tool
        .get("content")
        .and_then(Value::as_array)
        .expect("tool failed content array");
    assert_eq!(content[0]["type"], "input_text");
    assert_eq!(content[1]["type"], "input_image");
}

#[test]
fn codex_shaped_stream_and_tool_events_replay() {
    let events = vec![
        event(1, "session.input", json!({ "text": "run the tool" })),
        event(
            2,
            "model.stream_delta",
            json!({ "text": "I will run it. " }),
        ),
        event(
            3,
            "tool.started",
            json!({
                "tool_call_id": "call_1",
                "name": "do_thing",
                "arguments": { "x": 1 },
            }),
        ),
        event(
            4,
            "tool.output",
            json!({
                "tool_call_id": "call_1",
                "name": "do_thing",
                "text": "tool result",
            }),
        ),
        event(5, "session.done", json!({})),
    ];

    let messages = provider_messages_from_events(&events);
    assert_eq!(messages.len(), 3, "messages: {messages:#?}");
    assert_eq!(
        messages[1].get("content").and_then(Value::as_str),
        Some("I will run it. ")
    );
    let calls = messages[1]
        .get("tool_calls")
        .and_then(Value::as_array)
        .expect("tool call replayed");
    assert_eq!(calls[0].get("id").and_then(Value::as_str), Some("call_1"));
    assert_eq!(
        calls[0].get("name").and_then(Value::as_str),
        Some("do_thing")
    );
    assert_eq!(
        messages[2].get("content").and_then(Value::as_str),
        Some("tool result")
    );
}

#[test]
fn streaming_delta_overlap_is_utf8_boundary_safe() {
    let events = vec![
        event(1, "session.input", json!({ "text": "read the title" })),
        event(
            2,
            "model.stream_delta",
            json!({ "text": "The page title is “" }),
        ),
        event(
            3,
            "model.stream_delta",
            json!({ "text": "Example Domain”." }),
        ),
        event(4, "session.done", json!({})),
    ];

    let messages = provider_messages_from_events(&events);
    assert_eq!(
        messages[1].get("content").and_then(Value::as_str),
        Some("The page title is “Example Domain”.")
    );
}

#[test]
fn session_done_result_replays_when_no_stream_text_exists() {
    let events = vec![
        event(1, "session.input", json!({ "text": "hello" })),
        event(2, "session.done", json!({ "result": "hi back" })),
    ];

    let messages = provider_messages_from_events(&events);
    assert_eq!(messages.len(), 2, "messages: {messages:#?}");
    assert_eq!(
        messages[1].get("role").and_then(Value::as_str),
        Some("assistant")
    );
    assert_eq!(
        messages[1].get("content").and_then(Value::as_str),
        Some("hi back")
    );
}

#[test]
fn compaction_checkpoint_seeds_replacement_history() {
    let events = vec![
        event(1, "session.input", json!({ "text": "first user message" })),
        event(
            2,
            "model.response.output_item",
            json!({
                "item": {
                    "type": "message",
                    "role": "assistant",
                    "content": [{ "type": "output_text", "text": "early answer" }],
                }
            }),
        ),
        // Compaction checkpoint replaces all history before seq 3 with this summary turn.
        event(
            3,
            "session.compacted",
            json!({
                "replacement_messages": [
                    { "role": "user", "content": "compacted summary user" }
                ]
            }),
        ),
        event(4, "session.followup", json!({ "text": "after compaction" })),
        event(
            5,
            "model.response.output_item",
            json!({
                "item": {
                    "type": "message",
                    "role": "assistant",
                    "content": [{ "type": "output_text", "text": "post-compaction answer" }],
                }
            }),
        ),
        event(6, "session.done", json!({})),
    ];

    let messages = provider_messages_from_events(&events);
    // The pre-checkpoint user/assistant turn must NOT appear; replacement seed + post turn must.
    let texts: Vec<String> = messages
        .iter()
        .map(|m| {
            m.get("content")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string()
        })
        .collect();
    assert!(
        texts.iter().any(|t| t == "compacted summary user"),
        "replacement history seeds messages: {messages:#?}"
    );
    assert!(
        texts.iter().any(|t| t == "after compaction"),
        "post-compaction user replayed: {messages:#?}"
    );
    assert!(
        texts.iter().any(|t| t == "post-compaction answer"),
        "post-compaction assistant replayed: {messages:#?}"
    );
    assert!(
        !texts
            .iter()
            .any(|t| t == "first user message" || t == "early answer"),
        "pre-checkpoint history must be dropped: {messages:#?}"
    );
}

#[test]
fn rollback_drops_last_user_turn() {
    let events = vec![
        event(1, "session.input", json!({ "text": "first task" })),
        event(
            2,
            "model.response.output_item",
            json!({
                "item": {
                    "type": "message",
                    "role": "assistant",
                    "content": [{ "type": "output_text", "text": "first reply" }],
                }
            }),
        ),
        event(3, "session.followup", json!({ "text": "second task" })),
        event(
            4,
            "model.response.output_item",
            json!({
                "item": {
                    "type": "message",
                    "role": "assistant",
                    "content": [{ "type": "output_text", "text": "second reply" }],
                }
            }),
        ),
        // Roll back the most recent user turn (num_turns = 1).
        event(5, "session.rollback", json!({ "num_turns": 1 })),
    ];

    let messages = provider_messages_from_events(&events);
    let texts: Vec<String> = messages
        .iter()
        .map(|m| {
            m.get("content")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string()
        })
        .collect();
    assert!(
        texts.iter().any(|t| t == "first task"),
        "first turn kept: {messages:#?}"
    );
    assert!(
        texts.iter().any(|t| t == "first reply"),
        "first reply kept: {messages:#?}"
    );
    assert!(
        !texts.iter().any(|t| t == "second task"),
        "rolled-back user turn dropped: {messages:#?}"
    );
    assert!(
        !texts.iter().any(|t| t == "second reply"),
        "rolled-back assistant dropped: {messages:#?}"
    );
}

#[test]
fn is_real_user_event_matches_input_and_followup() {
    assert!(is_real_user_event(&event(1, "session.input", json!({}))));
    assert!(is_real_user_event(&event(2, "session.followup", json!({}))));
    assert!(!is_real_user_event(&event(3, "model.tool_call", json!({}))));
    assert!(!is_real_user_event(&event(4, "tool.output", json!({}))));
    assert!(!is_real_user_event(&event(5, "session.done", json!({}))));
}

#[test]
fn provider_history_open_turn_detection() {
    // A user turn with no terminal marker after it => open turn.
    let open = vec![
        json!({ "role": "user", "content": "do something" }),
        json!({ "role": "assistant", "content": "working on it" }),
    ];
    assert!(provider_history_has_open_turn(&open));

    // A user turn followed by a turn-aborted marker => not open.
    let aborted = vec![
        json!({ "role": "user", "content": "do something" }),
        json!({
            "role": "user",
            "content": "<turn_aborted>\ninterrupted\n</turn_aborted>",
        }),
    ];
    assert!(!provider_history_has_open_turn(&aborted));

    // No user message at all => not open.
    let empty: Vec<Value> = vec![json!({ "role": "assistant", "content": "hello" })];
    assert!(!provider_history_has_open_turn(&empty));
}
