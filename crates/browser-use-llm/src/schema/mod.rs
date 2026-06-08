//! The typed, provider-neutral canonical model (rearchitecture Phase 1.1).
//!
//! Pure data: request shape (`messages`), streaming/aggregate output (`event`),
//! sampling knobs (`options`), ids/enums (`ids`), and the error taxonomy
//! (`error`). No I/O, no provider, no `async`.

mod error;
mod event;
mod ids;
mod messages;
mod options;

pub use error::*;
pub use event::*;
pub use ids::*;
pub use messages::*;
pub use options::*;

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn roundtrip<T>(value: &T) -> T
    where
        T: serde::Serialize + serde::de::DeserializeOwned,
    {
        let s = serde_json::to_string(value).expect("serialize");
        serde_json::from_str(&s).expect("deserialize")
    }

    #[test]
    fn request_roundtrips_with_mixed_content() {
        let mut req = LlmRequest::new("gpt-5.1-codex", "openai");
        req.system
            .push(SystemPart::new("You are a terminal agent."));
        req.messages.push(Message::user_text("hello"));
        req.messages.push(Message::new(
            MessageRole::Assistant,
            vec![
                ContentPart::Reasoning {
                    text: "thinking".into(),
                    signature: Some("sig".into()),
                    provider_metadata: None,
                },
                ContentPart::ToolCall {
                    id: "call_1".into(),
                    name: "shell".into(),
                    input: json!({ "command": ["ls"] }),
                    provider_metadata: Some(json!({ "openai": { "item_id": "i_1" } })),
                },
            ],
        ));
        req.messages.push(Message::new(
            MessageRole::Tool,
            vec![ContentPart::ToolResult {
                tool_call_id: "call_1".into(),
                content: vec![ContentPart::text("file.txt")],
                is_error: false,
            }],
        ));
        req.tools.push(ToolDefinition {
            name: "shell".into(),
            description: "run a command".into(),
            input_schema: json!({ "type": "object" }),
            output_schema: None,
            namespace: None,
            namespace_description: None,
        });
        req.tool_choice = Some(ToolChoice::Auto);
        req.generation.temperature = Some(0.2);
        req.generation.reasoning_effort = Some(ReasoningEffort::High);

        assert_eq!(roundtrip(&req), req);
    }

    #[test]
    fn content_part_tag_is_stable() {
        let v = serde_json::to_value(ContentPart::text("hi")).unwrap();
        assert_eq!(v, json!({ "type": "text", "text": "hi" }));
    }

    #[test]
    fn events_roundtrip_and_tag_snake_case() {
        let events = vec![
            LlmEvent::StepStart,
            LlmEvent::TextStart { id: "t0".into() },
            LlmEvent::TextDelta {
                id: "t0".into(),
                delta: "he".into(),
            },
            LlmEvent::TextEnd {
                id: "t0".into(),
                phase: None,
            },
            LlmEvent::ToolInputStart {
                id: "c0".into(),
                name: "shell".into(),
            },
            LlmEvent::ToolCall {
                id: "c0".into(),
                name: "shell".into(),
                namespace: None,
                input: json!({}),
            },
            LlmEvent::Finish {
                usage: Usage {
                    input_tokens: 10,
                    cached_input_tokens: 4,
                    cache_creation_input_tokens: 0,
                    output_tokens: 6,
                    reasoning_output_tokens: 2,
                    total_tokens: 18,
                },
                finish_reason: Some(FinishReason::Stop),
            },
        ];
        assert_eq!(roundtrip(&events), events);

        let start = serde_json::to_value(LlmEvent::StepStart).unwrap();
        assert_eq!(start, json!({ "type": "step_start" }));
    }

    #[test]
    fn usage_computed_total_excludes_cached() {
        let u = Usage {
            input_tokens: 100,
            cached_input_tokens: 40,
            cache_creation_input_tokens: 0,
            output_tokens: 20,
            reasoning_output_tokens: 5,
            total_tokens: 0,
        };
        // cached is a subset of input, so it is not double-counted.
        assert_eq!(u.computed_total(), 125);
    }

    #[test]
    fn error_is_retryable_by_reason() {
        assert!(LlmError::new(LlmErrorReason::RateLimit, "slow down").retryable);
        assert!(!LlmError::new(LlmErrorReason::InvalidRequest, "bad").retryable);
    }
}
