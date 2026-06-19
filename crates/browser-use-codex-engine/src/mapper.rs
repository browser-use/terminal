use std::collections::HashMap;

use codex_app_server_client::AppServerEvent;
use codex_app_server_protocol::{CommandExecutionStatus, ServerNotification, ThreadItem};
use serde_json::{json, Value};

#[derive(Clone, Debug, PartialEq)]
pub struct CodexProjectedEvent {
    pub event_type: String,
    pub payload: Value,
}

impl CodexProjectedEvent {
    fn new(event_type: impl Into<String>, payload: Value) -> Self {
        Self {
            event_type: event_type.into(),
            payload,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum CodexTerminalOutcome {
    Completed { result: String },
    Failed { error: String },
    Cancelled { reason: String },
}

#[derive(Clone, Debug, PartialEq)]
pub struct CodexMappedEvent {
    pub projected: Vec<CodexProjectedEvent>,
    pub terminal: Option<CodexTerminalOutcome>,
}

#[derive(Default)]
pub struct CodexEventMapper {
    final_message_by_turn: HashMap<String, String>,
}

impl CodexEventMapper {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn map_app_server_event(&mut self, event: AppServerEvent) -> CodexMappedEvent {
        match event {
            AppServerEvent::Lagged { skipped } => CodexMappedEvent {
                projected: vec![CodexProjectedEvent::new(
                    "codex.event_lagged",
                    json!({ "skipped": skipped, "fatal_for_audited_runs": true }),
                )],
                terminal: Some(CodexTerminalOutcome::Failed {
                    error: format!("Codex app-server event stream lagged by {skipped} events"),
                }),
            },
            AppServerEvent::ServerNotification(notification) => {
                self.map_server_notification(notification)
            }
            AppServerEvent::ServerRequest(request) => CodexMappedEvent {
                projected: vec![CodexProjectedEvent::new(
                    "codex.server_request",
                    json!({ "request": request }),
                )],
                terminal: None,
            },
            AppServerEvent::Disconnected { message } => CodexMappedEvent {
                projected: vec![CodexProjectedEvent::new(
                    "codex.disconnected",
                    json!({ "message": message }),
                )],
                terminal: Some(CodexTerminalOutcome::Failed {
                    error: "Codex app-server disconnected".to_string(),
                }),
            },
        }
    }

    pub fn map_server_notification(
        &mut self,
        notification: ServerNotification,
    ) -> CodexMappedEvent {
        match notification {
            ServerNotification::TurnStarted(payload) => CodexMappedEvent {
                projected: vec![CodexProjectedEvent::new(
                    "model.turn.request",
                    json!({
                        "thread_id": payload.thread_id,
                        "turn_id": payload.turn.id,
                        "source": "codex_app_server",
                    }),
                )],
                terminal: None,
            },
            ServerNotification::AgentMessageDelta(payload) => {
                self.final_message_by_turn
                    .entry(payload.turn_id.clone())
                    .or_default()
                    .push_str(&payload.delta);
                CodexMappedEvent {
                    projected: vec![CodexProjectedEvent::new(
                        "model.stream_delta",
                        json!({
                            "text": payload.delta,
                            "thread_id": payload.thread_id,
                            "turn_id": payload.turn_id,
                            "item_id": payload.item_id,
                            "source": "codex_app_server",
                        }),
                    )],
                    terminal: None,
                }
            }
            ServerNotification::ReasoningSummaryTextDelta(payload) => CodexMappedEvent {
                projected: vec![CodexProjectedEvent::new(
                    "model.thinking_delta",
                    json!({
                        "text": payload.delta,
                        "thread_id": payload.thread_id,
                        "turn_id": payload.turn_id,
                        "item_id": payload.item_id,
                        "source": "codex_app_server",
                    }),
                )],
                terminal: None,
            },
            ServerNotification::ReasoningTextDelta(payload) => CodexMappedEvent {
                projected: vec![CodexProjectedEvent::new(
                    "model.thinking_delta",
                    json!({
                        "text": payload.delta,
                        "thread_id": payload.thread_id,
                        "turn_id": payload.turn_id,
                        "item_id": payload.item_id,
                        "source": "codex_app_server",
                    }),
                )],
                terminal: None,
            },
            ServerNotification::CommandExecutionOutputDelta(payload) => CodexMappedEvent {
                projected: vec![
                    CodexProjectedEvent::new(
                        "exec_command.output_delta",
                        json!({
                            "delta": payload.delta,
                            "thread_id": payload.thread_id,
                            "turn_id": payload.turn_id,
                            "item_id": payload.item_id,
                            "source": "codex_app_server",
                        }),
                    ),
                    CodexProjectedEvent::new(
                        "tool.output_delta",
                        json!({
                            "tool_name": "exec_command",
                            "delta": payload.delta,
                            "thread_id": payload.thread_id,
                            "turn_id": payload.turn_id,
                            "item_id": payload.item_id,
                            "source": "codex_app_server",
                        }),
                    ),
                ],
                terminal: None,
            },
            ServerNotification::ItemCompleted(payload) => {
                self.map_item_completed(payload.thread_id, payload.turn_id, payload.item)
            }
            ServerNotification::RawResponseItemCompleted(payload) => CodexMappedEvent {
                projected: vec![CodexProjectedEvent::new(
                    "codex.raw_response_item.completed",
                    json!({
                        "thread_id": payload.thread_id,
                        "turn_id": payload.turn_id,
                        "item": payload.item,
                    }),
                )],
                terminal: None,
            },
            ServerNotification::TurnCompleted(payload) => {
                let result = self
                    .final_message_by_turn
                    .remove(&payload.turn.id)
                    .unwrap_or_default();
                let terminal = if result.trim().is_empty() {
                    None
                } else {
                    Some(CodexTerminalOutcome::Completed {
                        result: result.clone(),
                    })
                };
                let mut projected = vec![CodexProjectedEvent::new(
                    "model.turn.response",
                    json!({
                        "thread_id": payload.thread_id,
                        "turn_id": payload.turn.id,
                        "text": result,
                        "source": "codex_app_server",
                    }),
                )];
                if let Some(CodexTerminalOutcome::Completed { result }) = &terminal {
                    projected.push(CodexProjectedEvent::new(
                        "session.done",
                        json!({
                            "result": result,
                            "source": "codex_app_server",
                            "runtime_owned": true,
                        }),
                    ));
                }
                CodexMappedEvent {
                    projected,
                    terminal,
                }
            }
            ServerNotification::Error(payload) => CodexMappedEvent {
                projected: vec![CodexProjectedEvent::new(
                    "model.turn.error",
                    json!({
                        "error": payload.error.message,
                        "additional_details": payload.error.additional_details,
                        "will_retry": payload.will_retry,
                        "thread_id": payload.thread_id,
                        "turn_id": payload.turn_id,
                        "source": "codex_app_server",
                    }),
                )],
                terminal: Some(CodexTerminalOutcome::Failed {
                    error: payload.error.message,
                }),
            },
            other => CodexMappedEvent {
                projected: vec![CodexProjectedEvent::new(
                    "codex.notification",
                    json!({ "notification": other }),
                )],
                terminal: None,
            },
        }
    }

    fn map_item_completed(
        &mut self,
        thread_id: String,
        turn_id: String,
        item: ThreadItem,
    ) -> CodexMappedEvent {
        match item {
            ThreadItem::AgentMessage { id, text, .. } => {
                self.final_message_by_turn
                    .insert(turn_id.clone(), text.clone());
                CodexMappedEvent {
                    projected: vec![CodexProjectedEvent::new(
                        "model.turn.response",
                        json!({
                            "text": text,
                            "thread_id": thread_id,
                            "turn_id": turn_id,
                            "item_id": id,
                            "source": "codex_app_server",
                        }),
                    )],
                    terminal: None,
                }
            }
            ThreadItem::CommandExecution {
                id,
                command,
                cwd,
                status,
                aggregated_output,
                exit_code,
                duration_ms,
                ..
            } => {
                let success = matches!(status, CommandExecutionStatus::Completed);
                CodexMappedEvent {
                    projected: vec![
                        CodexProjectedEvent::new(
                            "exec_command.end",
                            json!({
                                "command": command,
                                "cwd": cwd,
                                "exit_code": exit_code,
                                "success": success,
                                "duration_ms": duration_ms,
                                "thread_id": thread_id,
                                "turn_id": turn_id,
                                "item_id": id,
                                "source": "codex_app_server",
                            }),
                        ),
                        CodexProjectedEvent::new(
                            if success {
                                "tool.output"
                            } else {
                                "tool.failed"
                            },
                            json!({
                                "tool_name": "exec_command",
                                "output": aggregated_output.unwrap_or_default(),
                                "exit_code": exit_code,
                                "success": success,
                                "thread_id": thread_id,
                                "turn_id": turn_id,
                                "item_id": id,
                                "source": "codex_app_server",
                            }),
                        ),
                    ],
                    terminal: None,
                }
            }
            other => CodexMappedEvent {
                projected: vec![CodexProjectedEvent::new(
                    "codex.item.completed",
                    json!({
                        "thread_id": thread_id,
                        "turn_id": turn_id,
                        "item": other,
                    }),
                )],
                terminal: None,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_app_server_protocol::{
        AgentMessageDeltaNotification, ItemCompletedNotification,
        RawResponseItemCompletedNotification, ThreadItem, TurnCompletedNotification,
    };
    use serde_json::json;

    fn turn(id: &str) -> codex_app_server_protocol::Turn {
        serde_json::from_value(json!({
            "id": id,
            "items": [],
            "itemsView": "full",
            "status": "inProgress",
            "startedAt": 1,
            "completedAt": null,
            "durationMs": null,
            "error": null,
        }))
        .expect("turn fixture")
    }

    #[test]
    fn assistant_delta_accumulates_until_turn_completed_session_done() {
        let mut mapper = CodexEventMapper::new();

        let first = mapper.map_server_notification(ServerNotification::AgentMessageDelta(
            AgentMessageDeltaNotification {
                thread_id: "thread-1".to_string(),
                turn_id: "turn-1".to_string(),
                item_id: "item-1".to_string(),
                delta: "hel".to_string(),
            },
        ));
        let second = mapper.map_server_notification(ServerNotification::AgentMessageDelta(
            AgentMessageDeltaNotification {
                thread_id: "thread-1".to_string(),
                turn_id: "turn-1".to_string(),
                item_id: "item-1".to_string(),
                delta: "lo".to_string(),
            },
        ));
        let done = mapper.map_server_notification(ServerNotification::TurnCompleted(
            TurnCompletedNotification {
                thread_id: "thread-1".to_string(),
                turn: turn("turn-1"),
            },
        ));

        assert_eq!(first.projected[0].event_type, "model.stream_delta");
        assert_eq!(second.projected[0].payload["text"], json!("lo"));
        assert_eq!(
            done.terminal,
            Some(CodexTerminalOutcome::Completed {
                result: "hello".to_string()
            })
        );
        assert_eq!(done.projected[1].event_type, "session.done");
        assert_eq!(done.projected[1].payload["result"], json!("hello"));
    }

    #[test]
    fn completed_agent_message_overrides_delta_accumulator() {
        let mut mapper = CodexEventMapper::new();

        mapper.map_server_notification(ServerNotification::AgentMessageDelta(
            AgentMessageDeltaNotification {
                thread_id: "thread-1".to_string(),
                turn_id: "turn-1".to_string(),
                item_id: "item-1".to_string(),
                delta: "partial".to_string(),
            },
        ));
        mapper.map_server_notification(ServerNotification::ItemCompleted(
            ItemCompletedNotification {
                item: ThreadItem::AgentMessage {
                    id: "item-1".to_string(),
                    text: "authoritative final".to_string(),
                    phase: None,
                    memory_citation: None,
                },
                thread_id: "thread-1".to_string(),
                turn_id: "turn-1".to_string(),
                completed_at_ms: 2,
            },
        ));
        let done = mapper.map_server_notification(ServerNotification::TurnCompleted(
            TurnCompletedNotification {
                thread_id: "thread-1".to_string(),
                turn: turn("turn-1"),
            },
        ));

        assert_eq!(
            done.projected[1].payload["result"],
            json!("authoritative final")
        );
    }

    #[test]
    fn lagged_event_is_fatal_for_audited_runs() {
        let mut mapper = CodexEventMapper::new();
        let mapped = mapper.map_app_server_event(AppServerEvent::Lagged { skipped: 3 });

        assert_eq!(mapped.projected[0].event_type, "codex.event_lagged");
        assert_eq!(
            mapped.projected[0].payload["fatal_for_audited_runs"],
            json!(true)
        );
        assert_eq!(
            mapped.terminal,
            Some(CodexTerminalOutcome::Failed {
                error: "Codex app-server event stream lagged by 3 events".to_string()
            })
        );
    }

    #[test]
    fn raw_response_items_are_preserved_for_judge_evidence() {
        let mut mapper = CodexEventMapper::new();
        let mapped = mapper.map_server_notification(ServerNotification::RawResponseItemCompleted(
            RawResponseItemCompletedNotification {
                thread_id: "thread-1".to_string(),
                turn_id: "turn-1".to_string(),
                item: serde_json::from_value(json!({
                    "type": "message",
                    "id": "msg_1",
                    "role": "assistant",
                    "content": [
                        { "type": "output_text", "text": "grounded value" }
                    ]
                }))
                .expect("raw response item"),
            },
        ));

        assert_eq!(
            mapped.projected[0].event_type,
            "codex.raw_response_item.completed"
        );
        assert_eq!(
            mapped.projected[0].payload["item"]["content"][0]["text"],
            json!("grounded value")
        );
    }
}
