//! PURE event reducer (codex `lib.rs:8863-9646`). Rebuilds provider history from events.
//!
//! Ported behavior-faithfully from `browser-use-core`'s carved reducer in
//! `crates/browser-use-core/src/lib.rs`:
//! - `provider_messages_from_events` / `_for_fork` (8863-8904)
//! - `latest_compaction_replacement_history` (8920-8943) via the `session.compacted`
//!   checkpoint scan (`latest_compaction_checkpoint`, 8945-8974)
//! - `provider_messages_from_event_slice` (9017-9646) — the stateful per-event reducer
//!   (`flush_assistant` / `merge_response_item_assistant_text` accumulator) with the
//!   exact provider-message shapes
//! - `initial_context_messages_from_events` / `_from_iter` (9597-9745)
//! - `normalize_provider_messages` and its tool-call/output reconciliation (9947+)
//! - the per-event message builders + message/event predicates shared with `rollback.rs`.
//!
//! Persistence policy mirrors `browser-use-core::store_event_types()`.

use super::ProviderMessage;
use crate::session::rollback;
use browser_use_protocol::EventRecord;
use serde_json::Value;
use std::collections::{HashMap, HashSet};

// ---------------------------------------------------------------------------
// Constants (mirror `browser-use-core/src/constants.rs`).
// ---------------------------------------------------------------------------

pub(crate) const SESSION_ROLLBACK_EVENT: &str = "session.rollback";
pub(crate) const MODEL_RESPONSE_INPUT_ITEM_EVENT: &str = "model.response.input_item";
pub(crate) const MODEL_SWITCH_CONTEXT_EVENT: &str = "model.switch_context";
pub(crate) const PERSONALITY_CONTEXT_EVENT: &str = "model.personality_context";
pub(crate) const COLLABORATION_CONTEXT_EVENT: &str = "model.collaboration_context";
pub(crate) const GENERATED_IMAGE_CONTEXT_EVENT: &str = "model.generated_image_context";

pub(crate) const WORKSPACE_CONTEXT_MESSAGE_NAME: &str = "workspace_context";
pub(crate) const PERMISSIONS_CONTEXT_MESSAGE_NAME: &str = "permissions_context";
pub(crate) const MULTI_AGENT_USAGE_HINT_CONTEXT_MESSAGE_NAME: &str = "multi_agent_usage_hint";
pub(crate) const MODEL_SWITCH_CONTEXT_MESSAGE_NAME: &str = "model_switch_context";
pub(crate) const PERSONALITY_CONTEXT_MESSAGE_NAME: &str = "personality_context";
pub(crate) const COLLABORATION_CONTEXT_MESSAGE_NAME: &str = "collaboration_context";
pub(crate) const MENTION_CONTEXT_MESSAGE_NAME: &str = "typed_mention_context";
pub(crate) const GENERATED_IMAGE_CONTEXT_MESSAGE_NAME: &str = "generated_image_context";

pub(crate) const WORKSPACE_CONTEXT_PERMISSIONS_KIND: &str = "permissions";
pub(crate) const WORKSPACE_CONTEXT_MULTI_AGENT_USAGE_HINT_KIND: &str = "multi_agent_v2_usage_hint";
pub(crate) const WORKSPACE_CONTEXT_AGENTS_KIND: &str = "agents_md";
pub(crate) const WORKSPACE_CONTEXT_ENVIRONMENT_KIND: &str = "environment_context";
pub(crate) const WORKSPACE_CONTEXT_USER_SHELL_KIND: &str = "user_shell_command";

pub(crate) const TURN_ABORTED_START_MARKER: &str = "<turn_aborted>";
pub(crate) const TURN_ABORTED_END_MARKER: &str = "</turn_aborted>";
pub(crate) const TURN_ABORTED_INTERRUPTED_GUIDANCE: &str = "The user interrupted the previous turn on purpose. Any running unified exec processes may still be running in the background. If any tools/commands were aborted, they may have partially executed.";

/// Default permissions instructions injected when `inject_default_permissions` is set and
/// no permissions context event was seen.
///
/// NOTE (parity gap): the legacy `default_permissions_instructions()` body could not be
/// located in the carved source available to this work package. It is only reachable via
/// `initial_context_messages_from_events(.., inject_default_permissions = true)`, which the
/// reducer entry points only invoke during compaction replacement-history reconstruction.
fn default_permissions_instructions() -> &'static str {
    "Default permissions apply to this session."
}

/// Event types this engine persists to (and therefore replays from) the store.
/// Mirrors `browser-use-core::store_event_types()`.
pub(crate) const STORE_EVENT_TYPES: &[&str] = &[
    "session.input",
    "session.followup",
    "session.done",
    "session.failed",
    "session.cancelled",
    "session.rollback",
    "session.compacted",
    "model.response.output_item",
    "model.response.input_item",
    "model.tool_call",
    "model.delta",
    "model.stream_delta",
    "model.thinking_delta",
    "tool.started",
    "tool.output",
    "tool.failed",
    "tool.finished",
    "agent.context",
    "agent.message",
    "agent.mailbox_input",
    "workspace.context",
    MODEL_SWITCH_CONTEXT_EVENT,
    PERSONALITY_CONTEXT_EVENT,
    COLLABORATION_CONTEXT_EVENT,
    GENERATED_IMAGE_CONTEXT_EVENT,
];

// ---------------------------------------------------------------------------
// Top-level reducers (lib.rs:8863-8904).
// ---------------------------------------------------------------------------

pub fn provider_messages_from_events(events: &[EventRecord]) -> Vec<ProviderMessage> {
    let (replay_start_seq, mut messages, initial_context_already_in_history) =
        latest_compaction_replacement_history(events)
            .map(|state| {
                (
                    state.seq,
                    state.messages,
                    state.initial_context_already_in_history,
                )
            })
            .unwrap_or((0, Vec::new(), false));
    let replay_events =
        rollback::rollback_filtered_events_after(events, replay_start_seq, &mut messages);
    provider_messages_from_event_slice(
        &replay_events,
        &mut messages,
        initial_context_already_in_history,
        false,
    )
}

pub fn provider_messages_from_events_for_fork(events: &[EventRecord]) -> Vec<ProviderMessage> {
    let (replay_start_seq, mut messages, initial_context_already_in_history) =
        latest_compaction_replacement_history(events)
            .map(|state| {
                (
                    state.seq,
                    state.messages,
                    state.initial_context_already_in_history,
                )
            })
            .unwrap_or((0, Vec::new(), false));
    let replay_events =
        rollback::rollback_filtered_events_after_for_fork(events, replay_start_seq, &mut messages);
    provider_messages_from_event_slice(
        &replay_events,
        &mut messages,
        initial_context_already_in_history,
        true,
    )
}

pub struct CompactionReplayState {
    pub seq: i64,
    pub messages: Vec<ProviderMessage>,
    pub initial_context_already_in_history: bool,
}

struct CompactionCheckpoint {
    seq: i64,
    replay_from_seq: Option<i64>,
    messages: Vec<Value>,
    response_items: Option<Vec<Value>>,
    initial_context_already_in_history: bool,
}

/// lib.rs:8920-8943.
pub fn latest_compaction_replacement_history(
    events: &[EventRecord],
) -> Option<CompactionReplayState> {
    latest_compaction_checkpoint(events).map(|checkpoint| {
        let messages = match checkpoint.response_items {
            Some(response_items) => response_items_to_provider_messages(&response_items),
            None => checkpoint.messages,
        };
        let mut initial_context_already_in_history = checkpoint.initial_context_already_in_history;
        let mut messages = messages;
        if !initial_context_already_in_history {
            let mut initial_context =
                initial_context_messages_from_events(events, None, true, true);
            if !initial_context.is_empty() {
                initial_context.append(&mut messages);
                messages = initial_context;
                initial_context_already_in_history = true;
            }
        }
        CompactionReplayState {
            seq: checkpoint.replay_from_seq.unwrap_or(checkpoint.seq),
            messages,
            initial_context_already_in_history,
        }
    })
}

fn latest_compaction_checkpoint(events: &[EventRecord]) -> Option<CompactionCheckpoint> {
    events.iter().rev().find_map(|event| {
        if event.event_type != "session.compacted" {
            return None;
        }
        let messages = event
            .payload
            .get("replacement_messages")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let response_items = event
            .payload
            .get("replacement_response_items")
            .and_then(Value::as_array)
            .cloned();
        let has_replacement_history = event.payload.get("replacement_messages").is_some()
            || event.payload.get("replacement_response_items").is_some();
        let initial_context_already_in_history = event
            .payload
            .get("initial_context_already_in_history")
            .and_then(Value::as_bool)
            .unwrap_or(has_replacement_history);
        Some(CompactionCheckpoint {
            seq: event.seq,
            replay_from_seq: event.payload.get("replay_from_seq").and_then(Value::as_i64),
            messages,
            response_items,
            initial_context_already_in_history,
        })
    })
}

// ---------------------------------------------------------------------------
// The per-event reducer (lib.rs:9017-9646).
// ---------------------------------------------------------------------------

pub fn provider_messages_from_event_slice(
    events: &[&EventRecord],
    messages: &mut Vec<ProviderMessage>,
    has_compaction_checkpoint: bool,
    include_agent_messages: bool,
) -> Vec<ProviderMessage> {
    let mut assistant_text = String::new();
    let mut assistant_phase = None::<String>;
    let mut assistant_reasoning_content = String::new();
    let mut assistant_tool_calls = Vec::<Value>::new();
    let mut tool_names = HashMap::<String, String>::new();
    let mut emitted_tool_messages = HashSet::<String>::new();
    let mut turn_open = has_compaction_checkpoint && provider_history_has_open_turn(messages);
    let mut suppress_terminal_tail = false;
    let first_user_message_seq = first_user_message_seq_from_iter(events.iter().copied());
    let mut initial_context_messages = if has_compaction_checkpoint {
        Vec::new()
    } else {
        initial_context_messages_from_iter(
            events.iter().copied(),
            first_user_message_seq,
            false,
            false,
        )
    };
    let mut emitted_initial_context_messages = has_compaction_checkpoint;
    let mut workspace_contexts_by_before_seq = events
        .iter()
        .filter(|event| event.event_type == "workspace.context")
        .filter_map(|event| {
            let before_seq = event.payload.get("before_seq").and_then(Value::as_i64)?;
            if !has_compaction_checkpoint && Some(before_seq) == first_user_message_seq {
                return None;
            }
            workspace_context_message_from_payload(&event.payload)
                .map(|message| (before_seq, message))
        })
        .fold(
            HashMap::<i64, Vec<Value>>::new(),
            |mut acc, (seq, message)| {
                acc.entry(seq).or_default().push(message);
                acc
            },
        );
    let mut developer_contexts_by_before_seq = events
        .iter()
        .filter(|event| {
            matches!(
                event.event_type.as_str(),
                MODEL_SWITCH_CONTEXT_EVENT
                    | PERSONALITY_CONTEXT_EVENT
                    | COLLABORATION_CONTEXT_EVENT
                    | GENERATED_IMAGE_CONTEXT_EVENT
            )
        })
        .filter_map(|event| {
            let before_seq = event.payload.get("before_seq").and_then(Value::as_i64)?;
            let content = event.payload.get("content").and_then(Value::as_str)?;
            Some((
                before_seq,
                developer_context_message_for_event(&event.event_type, content.to_string()),
            ))
        })
        .fold(
            HashMap::<i64, Vec<Value>>::new(),
            |mut acc, (seq, message)| {
                acc.entry(seq).or_default().push(message);
                acc
            },
        );

    for event in events {
        match event.event_type.as_str() {
            "agent.context" => {
                flush_assistant(
                    messages,
                    &mut assistant_text,
                    &mut assistant_phase,
                    &mut assistant_reasoning_content,
                    &mut assistant_tool_calls,
                );
                if let Some(fork_response_items) = event
                    .payload
                    .get("fork_response_items")
                    .and_then(Value::as_array)
                {
                    messages.extend(response_items_to_provider_messages(fork_response_items));
                }
                let mut sections = Vec::new();
                if let Some(role) = event.payload.get("role").and_then(Value::as_str) {
                    sections.push(helper_session_identity_section(role, &event.payload));
                }
                let has_fork_history = event.payload.get("history_mode").and_then(Value::as_str)
                    == Some("fork_response_items");
                if !has_fork_history {
                    if let Some(context) = event.payload.get("context") {
                        sections.push(helper_session_inherited_context_section(
                            &context.to_string(),
                        ));
                    }
                }
                if !sections.is_empty() {
                    messages.push(serde_json::json!({
                        "role": "system",
                        "content": sections.join("\n\n"),
                    }));
                }
            }
            "agent.message" => {
                if include_agent_messages {
                    flush_assistant(
                        messages,
                        &mut assistant_text,
                        &mut assistant_phase,
                        &mut assistant_reasoning_content,
                        &mut assistant_tool_calls,
                    );
                    if let Some(message) = inter_agent_provider_message_from_event(event) {
                        messages.push(message);
                    }
                }
            }
            "agent.mailbox_input" => {
                flush_assistant(
                    messages,
                    &mut assistant_text,
                    &mut assistant_phase,
                    &mut assistant_reasoning_content,
                    &mut assistant_tool_calls,
                );
                if let Some(message) = inter_agent_provider_message_from_event(event) {
                    messages.push(message);
                }
            }
            "workspace.context" => {
                continue;
            }
            GENERATED_IMAGE_CONTEXT_EVENT => {
                if event
                    .payload
                    .get("before_seq")
                    .and_then(Value::as_i64)
                    .is_some()
                {
                    continue;
                }
                flush_assistant(
                    messages,
                    &mut assistant_text,
                    &mut assistant_phase,
                    &mut assistant_reasoning_content,
                    &mut assistant_tool_calls,
                );
                if let Some(content) = event.payload.get("content").and_then(Value::as_str) {
                    messages.push(generated_image_context_message(content.to_string()));
                    turn_open = true;
                }
            }
            "session.input" | "session.followup" => {
                suppress_terminal_tail = false;
                if !emitted_initial_context_messages {
                    flush_assistant(
                        messages,
                        &mut assistant_text,
                        &mut assistant_phase,
                        &mut assistant_reasoning_content,
                        &mut assistant_tool_calls,
                    );
                    messages.append(&mut initial_context_messages);
                    emitted_initial_context_messages = true;
                }
                if let Some(contexts) = developer_contexts_by_before_seq.remove(&event.seq) {
                    flush_assistant(
                        messages,
                        &mut assistant_text,
                        &mut assistant_phase,
                        &mut assistant_reasoning_content,
                        &mut assistant_tool_calls,
                    );
                    for message in contexts {
                        messages.push(message);
                    }
                }
                if let Some(contexts) = workspace_contexts_by_before_seq.remove(&event.seq) {
                    flush_assistant(
                        messages,
                        &mut assistant_text,
                        &mut assistant_phase,
                        &mut assistant_reasoning_content,
                        &mut assistant_tool_calls,
                    );
                    for message in contexts {
                        messages.push(message);
                    }
                }
                flush_assistant(
                    messages,
                    &mut assistant_text,
                    &mut assistant_phase,
                    &mut assistant_reasoning_content,
                    &mut assistant_tool_calls,
                );
                messages.extend(session_event_user_messages(&event.payload));
                turn_open = true;
            }
            MODEL_SWITCH_CONTEXT_EVENT
            | PERSONALITY_CONTEXT_EVENT
            | COLLABORATION_CONTEXT_EVENT => {
                if event
                    .payload
                    .get("before_seq")
                    .and_then(Value::as_i64)
                    .is_some()
                {
                    continue;
                }
                flush_assistant(
                    messages,
                    &mut assistant_text,
                    &mut assistant_phase,
                    &mut assistant_reasoning_content,
                    &mut assistant_tool_calls,
                );
                if let Some(content) = event.payload.get("content").and_then(Value::as_str) {
                    messages.push(developer_context_message_for_event(
                        &event.event_type,
                        content.to_string(),
                    ));
                }
            }
            "model.response.output_item" => {
                if suppress_terminal_tail {
                    continue;
                }
                let Some(item) = event.payload.get("item") else {
                    continue;
                };
                match item.get("type").and_then(Value::as_str) {
                    Some("message") => {
                        if item.get("role").and_then(Value::as_str) == Some("assistant") {
                            if let Some(text) = response_message_item_text(item) {
                                merge_response_item_assistant_text(
                                    messages,
                                    &mut assistant_text,
                                    &mut assistant_phase,
                                    &mut assistant_reasoning_content,
                                    &mut assistant_tool_calls,
                                    &text,
                                    item.get("phase").and_then(Value::as_str),
                                );
                                turn_open = true;
                            }
                        }
                    }
                    Some("function_call") | Some("custom_tool_call") => {
                        if let Some(call) = response_output_item_tool_call_value(item) {
                            if let Some(call_id) = tool_call_id_from_value(&call) {
                                if assistant_tool_calls.iter().any(|existing| {
                                    tool_call_id_from_value(existing) == Some(call_id)
                                }) {
                                    continue;
                                }
                                if let Some(name) = call.get("name").and_then(Value::as_str) {
                                    tool_names.insert(call_id.to_string(), name.to_string());
                                }
                            }
                            assistant_tool_calls.push(call);
                            turn_open = true;
                        }
                    }
                    Some(_) if is_raw_response_item_provider_message(item) => {
                        flush_assistant(
                            messages,
                            &mut assistant_text,
                            &mut assistant_phase,
                            &mut assistant_reasoning_content,
                            &mut assistant_tool_calls,
                        );
                        messages.push(item.clone());
                        turn_open = true;
                    }
                    _ => {}
                }
            }
            MODEL_RESPONSE_INPUT_ITEM_EVENT => {
                if suppress_terminal_tail {
                    continue;
                }
                let Some(item) = event.payload.get("item") else {
                    continue;
                };
                let Some(call_id) = response_input_item_call_id(item) else {
                    continue;
                };
                flush_assistant(
                    messages,
                    &mut assistant_text,
                    &mut assistant_phase,
                    &mut assistant_reasoning_content,
                    &mut assistant_tool_calls,
                );
                let message = response_input_item_tool_message(item, &event.payload, &tool_names);
                if let Some(pos) = messages.iter().rposition(|message| {
                    message.get("role").and_then(Value::as_str) == Some("tool")
                        && message.get("tool_call_id").and_then(Value::as_str) == Some(call_id)
                }) {
                    messages[pos] = message;
                } else {
                    messages.push(message);
                }
                emitted_tool_messages.insert(call_id.to_string());
                turn_open = true;
            }
            "model.delta" | "model.stream_delta" => {
                if suppress_terminal_tail {
                    continue;
                }
                if let Some(text) = event
                    .payload
                    .get("text")
                    .or_else(|| event.payload.get("delta"))
                    .and_then(Value::as_str)
                {
                    if let Some(delta) = assistant_delta_to_append(&assistant_text, text) {
                        assistant_text.push_str(&delta);
                        turn_open = true;
                    }
                }
            }
            "model.thinking_delta" => {
                if let Some(text) = event.payload.get("text").and_then(Value::as_str) {
                    assistant_reasoning_content.push_str(text);
                }
            }
            "model.tool_call" => {
                if suppress_terminal_tail {
                    continue;
                }
                let call = event.payload.clone();
                if let Some(call_id) = call.get("id").and_then(Value::as_str) {
                    if assistant_tool_calls
                        .iter()
                        .any(|existing| tool_call_id_from_value(existing) == Some(call_id))
                    {
                        continue;
                    }
                    if let Some(name) = call.get("name").and_then(Value::as_str) {
                        tool_names.insert(call_id.to_string(), name.to_string());
                    }
                }
                assistant_tool_calls.push(call);
                turn_open = true;
            }
            "tool.started" => {
                if suppress_terminal_tail {
                    continue;
                }
                let call_id = event
                    .payload
                    .get("tool_call_id")
                    .or_else(|| event.payload.get("id"))
                    .and_then(Value::as_str)
                    .unwrap_or("call");
                let Some(name) = event.payload.get("name").and_then(Value::as_str) else {
                    continue;
                };
                if assistant_tool_calls
                    .iter()
                    .any(|existing| tool_call_id_from_value(existing) == Some(call_id))
                {
                    continue;
                }
                tool_names.insert(call_id.to_string(), name.to_string());
                assistant_tool_calls.push(serde_json::json!({
                    "id": call_id,
                    "name": name,
                    "arguments": event
                        .payload
                        .get("arguments")
                        .or_else(|| event.payload.get("input"))
                        .cloned()
                        .unwrap_or_else(|| serde_json::json!({})),
                }));
                turn_open = true;
            }
            "tool.output" => {
                if suppress_terminal_tail {
                    continue;
                }
                flush_assistant(
                    messages,
                    &mut assistant_text,
                    &mut assistant_phase,
                    &mut assistant_reasoning_content,
                    &mut assistant_tool_calls,
                );
                if let Some(call_id) = event.payload.get("tool_call_id").and_then(Value::as_str) {
                    if !emitted_tool_messages.contains(call_id) {
                        messages.push(tool_message_from_output_event(&event.payload, call_id));
                        emitted_tool_messages.insert(call_id.to_string());
                        turn_open = true;
                    }
                }
            }
            "tool.failed" => {
                if suppress_terminal_tail {
                    continue;
                }
                flush_assistant(
                    messages,
                    &mut assistant_text,
                    &mut assistant_phase,
                    &mut assistant_reasoning_content,
                    &mut assistant_tool_calls,
                );
                if let Some(call_id) = event.payload.get("tool_call_id").and_then(Value::as_str) {
                    if !emitted_tool_messages.contains(call_id) {
                        if event.payload.get("content").is_some() {
                            messages.push(tool_message_from_output_event(&event.payload, call_id));
                            emitted_tool_messages.insert(call_id.to_string());
                            turn_open = true;
                            continue;
                        }
                        let name = event
                            .payload
                            .get("name")
                            .and_then(Value::as_str)
                            .unwrap_or("tool");
                        let error = event
                            .payload
                            .get("error")
                            .and_then(Value::as_str)
                            .unwrap_or("tool failed");
                        messages.push(serde_json::json!({
                            "role": "tool",
                            "tool_call_id": call_id,
                            "name": name,
                            "content": format!("{name} failed: {error}"),
                        }));
                        emitted_tool_messages.insert(call_id.to_string());
                        turn_open = true;
                    }
                }
            }
            "tool.finished" => {
                if suppress_terminal_tail {
                    continue;
                }
                flush_assistant(
                    messages,
                    &mut assistant_text,
                    &mut assistant_phase,
                    &mut assistant_reasoning_content,
                    &mut assistant_tool_calls,
                );
                if let Some(call_id) = event.payload.get("tool_call_id").and_then(Value::as_str) {
                    if !emitted_tool_messages.contains(call_id) {
                        let name = event
                            .payload
                            .get("name")
                            .and_then(Value::as_str)
                            .or_else(|| tool_names.get(call_id).map(String::as_str))
                            .unwrap_or("tool");
                        messages.push(serde_json::json!({
                            "role": "tool",
                            "tool_call_id": call_id,
                            "name": name,
                            "content": synthetic_tool_result_text(name),
                        }));
                        emitted_tool_messages.insert(call_id.to_string());
                        turn_open = true;
                    }
                }
            }
            "session.cancelled" => {
                let should_insert_marker =
                    turn_open || !assistant_text.is_empty() || !assistant_tool_calls.is_empty();
                flush_assistant(
                    messages,
                    &mut assistant_text,
                    &mut assistant_phase,
                    &mut assistant_reasoning_content,
                    &mut assistant_tool_calls,
                );
                if should_insert_marker {
                    messages.push(turn_aborted_user_message());
                }
                turn_open = false;
                suppress_terminal_tail = true;
            }
            "session.done" => {
                if !suppress_terminal_tail
                    && assistant_text.is_empty()
                    && assistant_tool_calls.is_empty()
                {
                    if let Some(result) = event.payload.get("result").and_then(Value::as_str) {
                        assistant_text.push_str(result);
                    }
                }
                flush_assistant(
                    messages,
                    &mut assistant_text,
                    &mut assistant_phase,
                    &mut assistant_reasoning_content,
                    &mut assistant_tool_calls,
                );
                turn_open = false;
                suppress_terminal_tail = true;
            }
            "session.failed" => {
                flush_assistant(
                    messages,
                    &mut assistant_text,
                    &mut assistant_phase,
                    &mut assistant_reasoning_content,
                    &mut assistant_tool_calls,
                );
                turn_open = false;
                suppress_terminal_tail = true;
            }
            _ => {}
        }
    }
    flush_assistant(
        messages,
        &mut assistant_text,
        &mut assistant_phase,
        &mut assistant_reasoning_content,
        &mut assistant_tool_calls,
    );
    if !emitted_initial_context_messages {
        messages.append(&mut initial_context_messages);
    }
    normalize_provider_messages(messages);
    std::mem::take(messages)
}

fn flush_assistant(
    messages: &mut Vec<Value>,
    assistant_text: &mut String,
    assistant_phase: &mut Option<String>,
    assistant_reasoning_content: &mut String,
    assistant_tool_calls: &mut Vec<Value>,
) {
    if assistant_text.is_empty() && assistant_tool_calls.is_empty() {
        assistant_reasoning_content.clear();
        return;
    }
    let mut message = serde_json::json!({
        "role": "assistant",
        "content": std::mem::take(assistant_text),
        "tool_calls": std::mem::take(assistant_tool_calls),
    });
    if let Some(phase) = assistant_phase.take() {
        message["phase"] = Value::String(phase);
    }
    if message
        .get("tool_calls")
        .and_then(Value::as_array)
        .is_some_and(|calls| !calls.is_empty())
        && !assistant_reasoning_content.is_empty()
    {
        message["reasoning_content"] =
            serde_json::json!(std::mem::take(assistant_reasoning_content));
    } else {
        assistant_reasoning_content.clear();
    }
    messages.push(message);
}

#[allow(clippy::too_many_arguments)]
fn merge_response_item_assistant_text(
    messages: &mut Vec<Value>,
    assistant_text: &mut String,
    assistant_phase: &mut Option<String>,
    assistant_reasoning_content: &mut String,
    assistant_tool_calls: &mut Vec<Value>,
    text: &str,
    phase: Option<&str>,
) {
    if text.is_empty() {
        return;
    }
    if assistant_text == text || assistant_text.trim() == text.trim() {
        assistant_text.clear();
        assistant_text.push_str(text);
        *assistant_phase = phase.map(ToOwned::to_owned);
        return;
    }
    if !assistant_text.is_empty()
        && assistant_text.ends_with(text)
        && assistant_text.len() > text.len()
    {
        let prefix_len = assistant_text.len() - text.len();
        let prefix = assistant_text[..prefix_len].to_string();
        assistant_text.clear();
        assistant_text.push_str(&prefix);
        flush_assistant(
            messages,
            assistant_text,
            assistant_phase,
            assistant_reasoning_content,
            assistant_tool_calls,
        );
        assistant_text.push_str(text);
        *assistant_phase = phase.map(ToOwned::to_owned);
        return;
    }
    if !assistant_text.is_empty() || !assistant_tool_calls.is_empty() {
        flush_assistant(
            messages,
            assistant_text,
            assistant_phase,
            assistant_reasoning_content,
            assistant_tool_calls,
        );
    }
    assistant_text.push_str(text);
    *assistant_phase = phase.map(ToOwned::to_owned);
}

// ---------------------------------------------------------------------------
// Initial-context reconstruction (lib.rs:9597-9745).
// ---------------------------------------------------------------------------

pub fn initial_context_messages_from_events(
    events: &[EventRecord],
    first_user_message_seq: Option<i64>,
    include_all_anchored: bool,
    inject_default_permissions: bool,
) -> Vec<ProviderMessage> {
    initial_context_messages_from_iter(
        events.iter(),
        first_user_message_seq,
        include_all_anchored,
        inject_default_permissions,
    )
}

fn initial_context_messages_from_iter<'a, I>(
    events: I,
    first_user_message_seq: Option<i64>,
    include_all_anchored: bool,
    inject_default_permissions: bool,
) -> Vec<Value>
where
    I: IntoIterator<Item = &'a EventRecord>,
{
    let mut context_messages = Vec::new();
    let mut saw_permissions = false;
    for event in events {
        if event.event_type != "workspace.context" {
            continue;
        }
        if let Some(before_seq) = event.payload.get("before_seq").and_then(Value::as_i64) {
            if !include_all_anchored {
                match first_user_message_seq {
                    Some(first_user_message_seq) if before_seq == first_user_message_seq => {}
                    _ => continue,
                }
            }
        }
        let Some(kind) = event.payload.get("kind").and_then(Value::as_str) else {
            continue;
        };
        if kind == WORKSPACE_CONTEXT_PERMISSIONS_KIND
            && event
                .payload
                .get("suppressed")
                .and_then(Value::as_bool)
                .unwrap_or(false)
        {
            saw_permissions = true;
            continue;
        }
        let Some(content) = event.payload.get("content").and_then(Value::as_str) else {
            continue;
        };
        match kind {
            WORKSPACE_CONTEXT_PERMISSIONS_KIND => {
                saw_permissions = true;
                context_messages.push(permissions_context_message(content.to_string()));
            }
            WORKSPACE_CONTEXT_MULTI_AGENT_USAGE_HINT_KIND => {
                context_messages.push(multi_agent_usage_hint_context_message(content.to_string()));
            }
            WORKSPACE_CONTEXT_AGENTS_KIND
            | WORKSPACE_CONTEXT_ENVIRONMENT_KIND
            | WORKSPACE_CONTEXT_USER_SHELL_KIND => {
                context_messages.push(workspace_context_message(vec![content.to_string()]));
            }
            _ => {}
        }
    }
    if inject_default_permissions && !saw_permissions {
        context_messages.insert(
            0,
            permissions_context_message(default_permissions_instructions().to_string()),
        );
    }
    move_workspace_context_before_first_user_message(&mut context_messages);
    context_messages
}

// ---------------------------------------------------------------------------
// Re-exported rollback predicates for the reducer API surface.
// ---------------------------------------------------------------------------

/// rollback.rs:114-119.
pub fn is_real_user_event(event: &EventRecord) -> bool {
    matches!(
        event.event_type.as_str(),
        "session.input" | "session.followup"
    )
}

/// rollback.rs:195-203.
pub fn provider_history_has_open_turn(messages: &[ProviderMessage]) -> bool {
    let Some(user_pos) = messages
        .iter()
        .rposition(|message| is_user_message_for_rollback(message, true))
    else {
        return false;
    };
    !messages[user_pos + 1..].iter().any(is_turn_aborted_message)
}

/// Persistence policy: an event is persistable (and thus replayable) iff its type is in
/// `store_event_types()`. Mirrors `browser-use-core::store_event_types()`.
pub fn is_persistable_event(ty: &str, _payload: &Value) -> bool {
    STORE_EVENT_TYPES.contains(&ty)
}

// ---------------------------------------------------------------------------
// response_items_to_provider_messages + helpers (lib.rs:6036-6130).
// ---------------------------------------------------------------------------

fn response_items_to_provider_messages(items: &[Value]) -> Vec<Value> {
    let mut messages = Vec::new();
    let mut tool_names = HashMap::<String, String>::new();
    for item in items {
        match item.get("type").and_then(Value::as_str) {
            Some("message") => {
                if let Some(message) = response_message_item_provider_message(item) {
                    messages.push(message);
                }
            }
            Some("function_call") | Some("custom_tool_call") => {
                if let Some(call) = response_output_item_tool_call_value(item) {
                    if let Some(call_id) = tool_call_id_from_value(&call) {
                        if let Some(name) = call.get("name").and_then(Value::as_str) {
                            tool_names.insert(call_id.to_string(), name.to_string());
                        }
                    }
                    messages.push(serde_json::json!({
                        "role": "assistant",
                        "content": "",
                        "tool_calls": [call],
                    }));
                }
            }
            Some("function_call_output") | Some("custom_tool_call_output") => {
                messages.push(response_input_item_tool_message(
                    item,
                    &Value::Null,
                    &tool_names,
                ));
            }
            Some(_) if is_raw_response_item_provider_message(item) => {
                messages.push(item.clone());
            }
            _ => {}
        }
    }
    messages
}

fn is_raw_response_item_provider_message(item: &Value) -> bool {
    let Some(item_type) = item.get("type").and_then(Value::as_str) else {
        return false;
    };
    response_item_type_is_api_history_item(item_type, item)
}

fn response_item_type_is_api_history_item(item_type: &str, item: &Value) -> bool {
    match item_type {
        "message" => item.get("role").and_then(Value::as_str) != Some("system"),
        "function_call_output"
        | "function_call"
        | "tool_search_call"
        | "tool_search_output"
        | "custom_tool_call"
        | "custom_tool_call_output"
        | "local_shell_call"
        | "reasoning"
        | "web_search_call"
        | "image_generation_call"
        | "compaction"
        | "compaction_summary"
        | "context_compaction" => true,
        "compaction_trigger" | "other" => false,
        _ => false,
    }
}

fn response_message_item_provider_message(item: &Value) -> Option<Value> {
    let role = item.get("role").and_then(Value::as_str).unwrap_or("user");
    let content = item
        .get("content")
        .and_then(Value::as_array)
        .map(|parts| {
            parts
                .iter()
                .filter_map(response_item_content_part_to_message_part)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    if content.is_empty() {
        return None;
    }
    Some(serde_json::json!({
        "role": role,
        "content": content,
    }))
}

fn response_item_content_part_to_message_part(part: &Value) -> Option<Value> {
    let part_type = part.get("type").and_then(Value::as_str)?;
    match part_type {
        "output_text" | "text" | "input_text" => {
            let text = part.get("text").and_then(Value::as_str)?;
            Some(serde_json::json!({ "type": "input_text", "text": text }))
        }
        "input_image" | "image" => Some(part.clone()),
        _ => None,
    }
}

fn response_message_item_text(item: &Value) -> Option<String> {
    let role = item.get("role").and_then(Value::as_str).unwrap_or("user");
    if role != "assistant" {
        return None;
    }
    let content = item.get("content")?;
    let text = match content {
        Value::String(text) => text.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|part| {
                let part_type = part.get("type").and_then(Value::as_str)?;
                if part_type == "output_text" || part_type == "text" || part_type == "input_text" {
                    part.get("text").and_then(Value::as_str)
                } else {
                    None
                }
            })
            .collect::<Vec<_>>()
            .join(""),
        _ => return None,
    };
    Some(text)
}

fn response_output_item_tool_call_value(item: &Value) -> Option<Value> {
    let call_id = item
        .get("call_id")
        .or_else(|| item.get("id"))
        .and_then(Value::as_str)?;
    if let Some(name) = item.get("name").and_then(Value::as_str) {
        let arguments_raw = item
            .get("arguments")
            .and_then(Value::as_str)
            .unwrap_or("{}");
        let arguments =
            serde_json::from_str::<Value>(arguments_raw).unwrap_or_else(|_| serde_json::json!({}));
        return Some(serde_json::json!({
            "id": call_id,
            "name": name,
            "arguments": arguments,
        }));
    }
    None
}

fn tool_call_id_from_value(call: &Value) -> Option<&str> {
    call.get("id")
        .or_else(|| call.get("call_id"))
        .and_then(Value::as_str)
}

fn response_input_item_call_id(item: &Value) -> Option<&str> {
    item.get("call_id")
        .or_else(|| item.get("id"))
        .and_then(Value::as_str)
}

fn response_input_item_tool_message(
    item: &Value,
    payload: &Value,
    tool_names: &HashMap<String, String>,
) -> Value {
    let call_id = response_input_item_call_id(item).unwrap_or_default();
    let mut name = item
        .get("name")
        .and_then(Value::as_str)
        .or_else(|| payload.get("name").and_then(Value::as_str))
        .or_else(|| tool_names.get(call_id).map(String::as_str))
        .unwrap_or("tool")
        .to_string();
    if name.trim().is_empty() {
        name = "tool".to_string();
    }
    let content = response_input_item_output_content(item);
    serde_json::json!({
        "role": "tool",
        "tool_call_id": call_id,
        "name": name,
        "content": content,
    })
}

fn response_input_item_output_content(item: &Value) -> Value {
    if let Some(output) = item.get("output") {
        return match output {
            Value::String(_) | Value::Array(_) => output.clone(),
            _ => Value::String(value_to_tool_output_text(output)),
        };
    }
    if let Some(content) = item.get("content") {
        return match content {
            Value::String(_) | Value::Array(_) => content.clone(),
            _ => Value::String(value_to_tool_output_text(content)),
        };
    }
    Value::String(String::new())
}

fn value_to_tool_output_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Array(parts) => {
            let text = parts
                .iter()
                .filter_map(|part| {
                    part.get("text")
                        .and_then(Value::as_str)
                        .map(ToOwned::to_owned)
                })
                .collect::<Vec<_>>()
                .join("");
            if text.trim().is_empty() {
                value.to_string()
            } else {
                text
            }
        }
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

// ---------------------------------------------------------------------------
// assistant-delta merge (lib.rs:4318-4334).
// ---------------------------------------------------------------------------

fn assistant_delta_to_append(current: &str, incoming: &str) -> Option<String> {
    if incoming.is_empty() {
        return None;
    }
    if current.is_empty() {
        return Some(incoming.to_string());
    }
    if incoming.len() >= current.len() && incoming.starts_with(current) {
        return Some(incoming[current.len()..].to_string());
    }
    if current.ends_with(incoming) {
        return None;
    }
    if let Some(overlap) = longest_suffix_prefix_overlap(current, incoming) {
        return Some(incoming[overlap..].to_string());
    }
    Some(incoming.to_string())
}

fn longest_suffix_prefix_overlap(current: &str, incoming: &str) -> Option<usize> {
    let max = current.len().min(incoming.len());
    (1..=max).rev().find_map(|len| {
        let current_start = current.len() - len;
        if !current.is_char_boundary(current_start) || !incoming.is_char_boundary(len) {
            return None;
        }
        let current_tail = &current[current_start..];
        let incoming_head = &incoming[..len];
        (current_tail == incoming_head).then_some(len)
    })
}

// ---------------------------------------------------------------------------
// User-input / inter-agent message builders.
// ---------------------------------------------------------------------------

fn first_user_message_seq_from_iter<'a, I>(events: I) -> Option<i64>
where
    I: IntoIterator<Item = &'a EventRecord>,
{
    events.into_iter().find_map(|event| {
        matches!(
            event.event_type.as_str(),
            "session.input" | "session.followup"
        )
        .then_some(event.seq)
    })
}

fn session_event_user_messages(payload: &Value) -> Vec<Value> {
    collab_input_from_payload(payload)
        .unwrap_or_else(|| session_event_user_messages_text_only(payload))
}

fn collab_input_from_payload(payload: &Value) -> Option<Vec<Value>> {
    let content = payload.get("content")?;
    let array = content.as_array()?;
    let mut messages = Vec::new();
    let mut user_content = Vec::new();
    for part in array {
        user_content.push(part.clone());
    }
    let skill_messages = payload
        .get("skill_context_messages")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    messages.extend(skill_messages);
    let mention_messages = payload
        .get("mention_context_messages")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    messages.extend(mention_messages);
    if !user_content.is_empty() {
        messages.push(serde_json::json!({
            "role": "user",
            "content": user_content,
        }));
    }
    Some(messages)
}

fn session_event_user_messages_text_only(payload: &Value) -> Vec<Value> {
    let text = payload
        .get("text")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    if text.trim().is_empty() {
        Vec::new()
    } else {
        vec![serde_json::json!({
            "role": "user",
            "content": text,
        })]
    }
}

fn inter_agent_provider_message_from_event(event: &EventRecord) -> Option<Value> {
    let content = event.payload.get("content").and_then(Value::as_str)?;
    if is_v1_subagent_notification_event(event, content) {
        return Some(serde_json::json!({
            "role": "user",
            "content": content,
        }));
    }
    let author = event
        .payload
        .get("author_path")
        .and_then(Value::as_str)
        .unwrap_or("/root");
    let recipient = event
        .payload
        .get("recipient_path")
        .and_then(Value::as_str)
        .or_else(|| event.payload.get("agent_path").and_then(Value::as_str))
        .unwrap_or("/root");
    let trigger_turn = event
        .payload
        .get("trigger_turn")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    Some(serde_json::json!({
        "role": "user",
        "content": inter_agent_prompt_text(author, recipient, content, trigger_turn),
    }))
}

fn inter_agent_prompt_text(
    author: &str,
    recipient: &str,
    content: &str,
    trigger_turn: bool,
) -> String {
    let label = if trigger_turn {
        "Direct task from parent"
    } else {
        "Inter-agent message"
    };
    format!("{label} {author} to you {recipient}:\n{content}")
}

fn is_v1_subagent_notification_event(event: &EventRecord, content: &str) -> bool {
    event
        .payload
        .get("v1_subagent_notification")
        .and_then(Value::as_bool)
        .unwrap_or_else(|| {
            is_subagent_notification_text(content)
                && event
                    .payload
                    .get("author_path")
                    .and_then(Value::as_str)
                    .is_some_and(|path| !path.starts_with("/root"))
        })
}

// ---------------------------------------------------------------------------
// Context message builders.
// ---------------------------------------------------------------------------

fn permissions_context_message(text: String) -> Value {
    serde_json::json!({
        "role": "developer",
        "name": PERMISSIONS_CONTEXT_MESSAGE_NAME,
        "content": [{ "type": "input_text", "text": text }],
    })
}

fn workspace_context_message(sections: Vec<String>) -> Value {
    let content = sections
        .into_iter()
        .map(|text| serde_json::json!({ "type": "input_text", "text": text }))
        .collect::<Vec<_>>();
    serde_json::json!({
        "role": "user",
        "name": WORKSPACE_CONTEXT_MESSAGE_NAME,
        "content": content,
    })
}

fn multi_agent_usage_hint_context_message(text: String) -> Value {
    serde_json::json!({
        "role": "developer",
        "name": MULTI_AGENT_USAGE_HINT_CONTEXT_MESSAGE_NAME,
        "content": [{ "type": "input_text", "text": text }],
    })
}

fn workspace_context_message_from_payload(payload: &Value) -> Option<Value> {
    let kind = payload.get("kind").and_then(Value::as_str)?;
    if !matches!(
        kind,
        WORKSPACE_CONTEXT_PERMISSIONS_KIND
            | WORKSPACE_CONTEXT_MULTI_AGENT_USAGE_HINT_KIND
            | WORKSPACE_CONTEXT_AGENTS_KIND
            | WORKSPACE_CONTEXT_ENVIRONMENT_KIND
            | WORKSPACE_CONTEXT_USER_SHELL_KIND
    ) {
        return None;
    }
    let content = payload
        .get("content")
        .and_then(Value::as_str)
        .filter(|content| !content.is_empty())?;
    match kind {
        WORKSPACE_CONTEXT_PERMISSIONS_KIND => {
            Some(permissions_context_message(content.to_string()))
        }
        WORKSPACE_CONTEXT_MULTI_AGENT_USAGE_HINT_KIND => {
            Some(multi_agent_usage_hint_context_message(content.to_string()))
        }
        _ => Some(workspace_context_message(vec![content.to_string()])),
    }
}

fn developer_context_message_for_event(event_type: &str, text: String) -> Value {
    match event_type {
        PERSONALITY_CONTEXT_EVENT => personality_context_message(text),
        COLLABORATION_CONTEXT_EVENT => collaboration_context_message(text),
        GENERATED_IMAGE_CONTEXT_EVENT => generated_image_context_message(text),
        _ => model_switch_context_message(text),
    }
}

fn model_switch_context_message(text: String) -> Value {
    developer_named_context_message(MODEL_SWITCH_CONTEXT_MESSAGE_NAME, text)
}

fn personality_context_message(text: String) -> Value {
    developer_named_context_message(PERSONALITY_CONTEXT_MESSAGE_NAME, text)
}

fn collaboration_context_message(text: String) -> Value {
    developer_named_context_message(COLLABORATION_CONTEXT_MESSAGE_NAME, text)
}

fn generated_image_context_message(text: String) -> Value {
    developer_named_context_message(GENERATED_IMAGE_CONTEXT_MESSAGE_NAME, text)
}

fn developer_named_context_message(name: &str, text: String) -> Value {
    serde_json::json!({
        "role": "developer",
        "name": name,
        "content": [{ "type": "input_text", "text": text }],
    })
}

fn helper_session_identity_section(role: &str, payload: &Value) -> String {
    let canonical_task_sentence = payload
        .get("agent_path")
        .and_then(Value::as_str)
        .map(|path| {
            format!(
                " Canonical task name: {path}. This is an agent routing name, not a filesystem path."
            )
        })
        .unwrap_or_default();
    let explorer_instruction = if role.to_ascii_lowercase().contains("explor") {
        " As the explorer, inspect the repository/codebase directly with local tools."
    } else {
        ""
    };
    format!("You are operating as the {role} agent.{canonical_task_sentence}{explorer_instruction}")
}

fn helper_session_inherited_context_section(context: &str) -> String {
    format!("Inherited context from the parent session:\n{context}")
}

// ---------------------------------------------------------------------------
// Tool-output message builders + synthetic results.
// ---------------------------------------------------------------------------

fn tool_message_from_output_event(payload: &Value, call_id: &str) -> Value {
    let name = payload
        .get("name")
        .and_then(Value::as_str)
        .filter(|name| !name.trim().is_empty())
        .unwrap_or("tool")
        .to_string();
    let content = tool_output_event_content(payload);
    serde_json::json!({
        "role": "tool",
        "tool_call_id": call_id,
        "name": name,
        "content": content,
    })
}

fn tool_output_event_content(payload: &Value) -> Value {
    if let Some(content) = payload.get("content") {
        return content.clone();
    }
    Value::String(tool_output_event_text(payload))
}

fn tool_output_event_text(payload: &Value) -> String {
    if let Some(text) = payload.get("text").and_then(Value::as_str) {
        if !text.trim().is_empty() {
            return text.to_string();
        }
    }
    if let Some(output) = payload.get("output") {
        let text = value_to_tool_output_text(output);
        if !text.trim().is_empty() {
            return text;
        }
    }
    if let Some(content) = payload.get("content") {
        let text = value_to_tool_output_text(content);
        if !text.trim().is_empty() {
            return text;
        }
    }
    if let Some(text) = browser_script_running_tool_text(payload) {
        return text;
    }
    let mut parts = Vec::new();
    for key in ["summary", "data", "outputs"] {
        let Some(value) = payload.get(key) else {
            continue;
        };
        if value.is_null() || value == &serde_json::json!({}) || value == &serde_json::json!([]) {
            continue;
        }
        parts.push(format!("{key}: {}", value_to_tool_output_text(value)));
    }
    parts.join("\n")
}

fn browser_script_running_tool_text(payload: &Value) -> Option<String> {
    let name = payload.get("name").and_then(Value::as_str)?;
    if name != "browser_script" {
        return None;
    }
    if payload.get("status").and_then(Value::as_str) != Some("running") {
        return None;
    }
    let mut parts = vec!["browser_script is still running.".to_string()];
    if let Some(run_id) = payload.get("run_id").and_then(Value::as_str) {
        parts.push(format!("run_id: {run_id}"));
        let observe_ms = payload
            .get("next_observe_ms")
            .and_then(Value::as_u64)
            .unwrap_or(1_000);
        parts.push(format!(
            "Next step: call browser_script with action=\"observe\", run_id=\"{run_id}\", and observe_timeout_ms={observe_ms}."
        ));
    }
    Some(parts.join("\n"))
}

fn synthetic_tool_result_text(name: &str) -> String {
    match name {
        "update_plan" => "Plan updated".to_string(),
        "done" => "done".to_string(),
        other => format!("{other} completed"),
    }
}

fn turn_aborted_user_message() -> Value {
    serde_json::json!({
        "role": "user",
        "content": format!(
            "{TURN_ABORTED_START_MARKER}\n{TURN_ABORTED_INTERRUPTED_GUIDANCE}\n{TURN_ABORTED_END_MARKER}"
        ),
    })
}

// ---------------------------------------------------------------------------
// move_workspace_context_before_first_user_message (lib.rs:9889-9946).
// ---------------------------------------------------------------------------

fn move_workspace_context_before_first_user_message(messages: &mut Vec<Value>) {
    let mut context_sections = Vec::new();
    let mut environment_context_section = None;
    let mut permissions_sections = Vec::new();
    let mut other_messages = Vec::with_capacity(messages.len());
    for message in std::mem::take(messages) {
        if is_workspace_context_message(&message) {
            let content = message_content_text(&message);
            if !content.trim().is_empty() {
                if is_environment_context_section(&content) {
                    environment_context_section = Some(content);
                } else {
                    context_sections.push(content);
                }
            }
        } else if is_permissions_context_message(&message) {
            let content = message_content_text(&message);
            if !content.trim().is_empty() {
                permissions_sections.push(content);
            }
        } else {
            other_messages.push(message);
        }
    }
    if let Some(environment_context_section) = environment_context_section {
        context_sections.push(environment_context_section);
    }
    if context_sections.is_empty() && permissions_sections.is_empty() {
        *messages = other_messages;
        return;
    }
    let insert_at = other_messages
        .iter()
        .position(|message| message.get("role").and_then(Value::as_str) == Some("user"))
        .unwrap_or(other_messages.len());
    let mut insert_messages = Vec::new();
    if !permissions_sections.is_empty() {
        insert_messages.push(permissions_context_message(
            permissions_sections.join("\n\n"),
        ));
    }
    if !context_sections.is_empty() {
        insert_messages.push(workspace_context_message(context_sections));
    }
    other_messages.splice(insert_at..insert_at, insert_messages);
    *messages = other_messages;
}

fn is_environment_context_section(content: &str) -> bool {
    content.contains("<environment_context>")
}

fn is_workspace_context_message(message: &Value) -> bool {
    message.get("name").and_then(Value::as_str) == Some(WORKSPACE_CONTEXT_MESSAGE_NAME)
}

fn is_permissions_context_message(message: &Value) -> bool {
    message.get("name").and_then(Value::as_str) == Some(PERMISSIONS_CONTEXT_MESSAGE_NAME)
}

// ---------------------------------------------------------------------------
// normalize_provider_messages (lib.rs:9947+).
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
struct PendingToolCall {
    id: String,
    name: String,
}

fn normalize_provider_messages(messages: &mut Vec<Value>) {
    let mut normalized = Vec::with_capacity(messages.len());
    let mut pending = Vec::<PendingToolCall>::new();
    let mut emitted_outputs = HashSet::<String>::new();

    for message in std::mem::take(messages) {
        match message
            .get("role")
            .and_then(Value::as_str)
            .unwrap_or("user")
        {
            "assistant" => {
                append_synthetic_outputs_for_pending(
                    &mut normalized,
                    &mut pending,
                    &mut emitted_outputs,
                );
                if let Some((assistant, calls)) = normalized_assistant_message(message) {
                    for call in calls {
                        pending.push(call);
                    }
                    normalized.push(assistant);
                }
            }
            "tool" => {
                let call_id = message
                    .get("tool_call_id")
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned);
                let Some(call_id) = call_id else {
                    if let Some(context) = orphan_tool_output_context_message(&message, "<missing>")
                    {
                        normalized.push(context);
                    }
                    continue;
                };
                if let Some(index) = pending.iter().position(|call| call.id == call_id) {
                    if index > 0 {
                        let mut earlier_missing =
                            pending.drain(..index).collect::<Vec<PendingToolCall>>();
                        append_synthetic_outputs_for_pending(
                            &mut normalized,
                            &mut earlier_missing,
                            &mut emitted_outputs,
                        );
                    }
                    let call = pending.remove(0);
                    normalized.push(normalized_tool_message(message, &call.id, &call.name));
                    emitted_outputs.insert(call.id);
                } else if let Some(context) = orphan_tool_output_context_message(&message, &call_id)
                {
                    normalized.push(context);
                }
            }
            _ => {
                append_synthetic_outputs_for_pending(
                    &mut normalized,
                    &mut pending,
                    &mut emitted_outputs,
                );
                normalized.push(message);
            }
        }
    }

    append_synthetic_outputs_for_pending(&mut normalized, &mut pending, &mut emitted_outputs);
    *messages = normalized;
}

fn append_synthetic_outputs_for_pending(
    messages: &mut Vec<Value>,
    pending: &mut Vec<PendingToolCall>,
    emitted_outputs: &mut HashSet<String>,
) {
    for call in pending.drain(..) {
        if emitted_outputs.insert(call.id.clone()) {
            messages.push(serde_json::json!({
                "role": "tool",
                "tool_call_id": call.id,
                "name": call.name,
                "content": "aborted",
            }));
        }
    }
}

fn normalized_assistant_message(mut message: Value) -> Option<(Value, Vec<PendingToolCall>)> {
    let calls = normalized_assistant_tool_calls(&message);
    let text = message_content_text(&message);
    if text.trim().is_empty() && calls.is_empty() {
        return None;
    }
    let pending = calls
        .iter()
        .filter_map(|call| {
            Some(PendingToolCall {
                id: call.get("id").and_then(Value::as_str)?.to_string(),
                name: call.get("name").and_then(Value::as_str)?.to_string(),
            })
        })
        .collect::<Vec<_>>();
    let object = message.as_object_mut()?;
    object.insert("role".to_string(), Value::String("assistant".to_string()));
    if calls.is_empty() {
        object.remove("tool_calls");
    } else {
        object.insert("tool_calls".to_string(), Value::Array(calls));
    }
    Some((message, pending))
}

fn normalized_assistant_tool_calls(message: &Value) -> Vec<Value> {
    let mut seen = HashSet::new();
    message
        .get("tool_calls")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|call| {
            let id = call
                .get("id")
                .or_else(|| call.get("call_id"))
                .and_then(Value::as_str)?
                .to_string();
            if !seen.insert(id.clone()) {
                return None;
            }
            let name = call
                .get("name")
                .and_then(Value::as_str)
                .or_else(|| {
                    call.get("function")
                        .and_then(|function| function.get("name"))
                        .and_then(Value::as_str)
                })?
                .to_string();
            let arguments = call
                .get("arguments")
                .cloned()
                .or_else(|| {
                    call.get("function")
                        .and_then(|function| function.get("arguments"))
                        .and_then(Value::as_str)
                        .and_then(|raw| serde_json::from_str::<Value>(raw).ok())
                })
                .unwrap_or_else(|| serde_json::json!({}));
            let namespace = call.get("namespace").and_then(Value::as_str).or_else(|| {
                call.get("function")
                    .and_then(|function| function.get("namespace"))
                    .and_then(Value::as_str)
            });
            let mut normalized = serde_json::json!({
                "id": id,
                "name": name,
                "arguments": arguments,
            });
            if let Some(namespace) = namespace {
                normalized["namespace"] = Value::String(namespace.to_string());
            }
            Some(normalized)
        })
        .collect()
}

fn normalized_tool_message(mut message: Value, call_id: &str, name: &str) -> Value {
    let object = message
        .as_object_mut()
        .expect("tool message should be a JSON object");
    object.insert("role".to_string(), Value::String("tool".to_string()));
    object.insert(
        "tool_call_id".to_string(),
        Value::String(call_id.to_string()),
    );
    if !object
        .get("name")
        .and_then(Value::as_str)
        .is_some_and(|value| !value.trim().is_empty())
    {
        object.insert("name".to_string(), Value::String(name.to_string()));
    }
    if !object.contains_key("content") {
        object.insert("content".to_string(), Value::String(String::new()));
    }
    message
}

fn orphan_tool_output_context_message(message: &Value, call_id: &str) -> Option<Value> {
    let text = message_content_text(message);
    if text.trim().is_empty() {
        return None;
    }
    Some(serde_json::json!({
        "role": "user",
        "content": format!(
            "Tool output retained as context after history normalization. (call_id: {call_id})\n{text}"
        ),
    }))
}

// ---------------------------------------------------------------------------
// Message / event predicates (lib.rs:9729-9748, 10455-10470, 22156-22160).
// Shared with rollback.rs through this module.
// ---------------------------------------------------------------------------

pub(crate) fn message_content_text(message: &Value) -> String {
    match message.get("content") {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|part| {
                part.get("text")
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned)
            })
            .collect::<Vec<_>>()
            .join(""),
        _ => String::new(),
    }
}

pub(crate) fn is_turn_aborted_message(message: &Value) -> bool {
    matches!(
        message.get("role").and_then(Value::as_str),
        Some("user") | Some("developer")
    ) && is_turn_aborted_text(&message_content_text(message))
}

fn is_turn_aborted_text(text: &str) -> bool {
    let trimmed_start = text.trim_start();
    let starts_with_marker = trimmed_start
        .get(..TURN_ABORTED_START_MARKER.len())
        .is_some_and(|candidate| candidate.eq_ignore_ascii_case(TURN_ABORTED_START_MARKER));
    let trimmed_end = trimmed_start.trim_end();
    let ends_with_marker = trimmed_end
        .get(
            trimmed_end
                .len()
                .saturating_sub(TURN_ABORTED_END_MARKER.len())..,
        )
        .is_some_and(|candidate| candidate.eq_ignore_ascii_case(TURN_ABORTED_END_MARKER));
    starts_with_marker && ends_with_marker
}

pub(crate) fn is_skill_context_message(message: &Value) -> bool {
    message.get("role").and_then(Value::as_str) == Some("user")
        && message_content_text(message)
            .trim_start()
            .starts_with("<skill>")
}

pub(crate) fn is_subagent_notification_context_message(message: &Value) -> bool {
    message.get("role").and_then(Value::as_str) == Some("user")
        && is_subagent_notification_text(&message_content_text(message))
}

fn is_subagent_notification_text(text: &str) -> bool {
    text.trim_start().starts_with("<subagent_notification>")
}

pub(crate) fn provider_message_is_inter_agent_instruction(message: &Value) -> bool {
    if message.get("role").and_then(Value::as_str) != Some("assistant") {
        return false;
    }
    parsed_inter_agent_communication(&message_content_text(message)).is_some()
}

fn parsed_inter_agent_communication(text: &str) -> Option<Value> {
    let value = serde_json::from_str::<Value>(text).ok()?;
    value.get("trigger_turn").and_then(Value::as_bool)?;
    if value.get("author").and_then(Value::as_str).is_none()
        || value.get("recipient").and_then(Value::as_str).is_none()
        || value.get("content").and_then(Value::as_str).is_none()
    {
        return None;
    }
    if value
        .get("other_recipients")
        .and_then(Value::as_array)
        .is_some_and(|recipients| {
            recipients
                .iter()
                .any(|recipient| recipient.as_str().is_none())
        })
    {
        return None;
    }
    Some(value)
}

pub(crate) fn is_real_user_message(message: &Value) -> bool {
    message.get("role").and_then(Value::as_str) == Some("user")
        && message.get("name").is_none()
        && !is_turn_aborted_message(message)
        && !is_skill_context_message(message)
        && !is_subagent_notification_context_message(message)
}

pub(crate) fn is_user_message_for_rollback(message: &Value, count_inter_agent_turns: bool) -> bool {
    is_real_user_message(message)
        || (count_inter_agent_turns && provider_message_is_inter_agent_instruction(message))
}
