//! Gemini `streamGenerateContent` protocol.

use std::collections::{HashMap, VecDeque};

use serde_json::{json, Map, Value};

use crate::protocols::utils::{Lifecycle, ToolStream};
use crate::route::framing::SseFrame;
use crate::route::protocol::{Protocol, ProtocolStream};
use crate::schema::{
    ContentPart, FinishReason, LlmError, LlmErrorReason, LlmEvent, LlmRequest, Message,
    MessageRole, ToolChoice, Usage,
};

const TEXT_ID: &str = "0";
const GEMINI_DUMMY_THOUGHT_SIGNATURE: &str = "skip_thought_signature_validator";

#[derive(Debug, Default, Clone, Copy)]
pub struct GeminiGenerateContentProtocol;

impl GeminiGenerateContentProtocol {
    pub fn new() -> Self {
        Self
    }
}

impl Protocol for GeminiGenerateContentProtocol {
    fn build_body(&self, req: &LlmRequest) -> Result<Value, LlmError> {
        let mut body = Map::new();

        if !req.system.is_empty() {
            body.insert(
                "systemInstruction".to_string(),
                json!({
                    "parts": req.system.iter().map(|part| json!({ "text": part.text })).collect::<Vec<_>>()
                }),
            );
        }

        body.insert(
            "contents".to_string(),
            Value::Array(
                req.messages
                    .iter()
                    .scan(FunctionNameTracker::default(), |tracker, message| {
                        Some(build_content(message, tracker))
                    })
                    .collect::<Result<Vec<_>, _>>()?,
            ),
        );

        let generation_config = build_generation_config(req);
        if !generation_config.is_empty() {
            body.insert(
                "generationConfig".to_string(),
                Value::Object(generation_config),
            );
        }

        if !req.tools.is_empty() {
            body.insert(
                "tools".to_string(),
                json!([{
                    "functionDeclarations": req.tools.iter().map(|tool| {
                        json!({
                            "name": tool.name,
                            "description": tool.description,
                            "parameters": sanitize_schema_for_gemini(&tool.input_schema),
                        })
                    }).collect::<Vec<_>>()
                }]),
            );
        }

        if let Some(choice) = &req.tool_choice {
            body.insert("toolConfig".to_string(), build_tool_config(choice));
        }

        Ok(Value::Object(body))
    }

    fn decoder(&self) -> Box<dyn ProtocolStream> {
        Box::new(GeminiStream::new())
    }
}

fn sanitize_schema_for_gemini(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut sanitized = Map::new();
            for (key, nested) in map {
                if key == "additionalProperties" {
                    continue;
                }
                sanitized.insert(key.clone(), sanitize_schema_for_gemini(nested));
            }
            Value::Object(sanitized)
        }
        Value::Array(items) => Value::Array(items.iter().map(sanitize_schema_for_gemini).collect()),
        _ => value.clone(),
    }
}

fn build_generation_config(req: &LlmRequest) -> Map<String, Value> {
    let mut config = Map::new();
    if let Some(temperature) = req.generation.temperature {
        config.insert("temperature".to_string(), json!(temperature));
    }
    if let Some(top_p) = req.generation.top_p {
        config.insert("topP".to_string(), json!(top_p));
    }
    if let Some(max_tokens) = req.generation.max_tokens {
        config.insert("maxOutputTokens".to_string(), json!(max_tokens));
    }
    if !req.generation.stop.is_empty() {
        config.insert("stopSequences".to_string(), json!(req.generation.stop));
    }
    config
}

#[derive(Default)]
struct FunctionNameTracker {
    pending_by_call_id: HashMap<String, VecDeque<String>>,
}

impl FunctionNameTracker {
    fn record_call(&mut self, id: &str, name: &str) {
        self.pending_by_call_id
            .entry(id.to_string())
            .or_default()
            .push_back(name.to_string());
    }

    fn take_name(&mut self, id: &str) -> Option<String> {
        let queue = self.pending_by_call_id.get_mut(id)?;
        let name = queue.pop_front();
        if queue.is_empty() {
            self.pending_by_call_id.remove(id);
        }
        name
    }
}

fn build_content(message: &Message, tracker: &mut FunctionNameTracker) -> Result<Value, LlmError> {
    let role = match message.role {
        MessageRole::Assistant => "model",
        MessageRole::User | MessageRole::Tool | MessageRole::System | MessageRole::Developer => {
            "user"
        }
    };
    Ok(json!({
        "role": role,
        "parts": message.content.iter().map(|part| build_part(part, tracker)).collect::<Result<Vec<_>, _>>()?,
    }))
}

fn build_part(part: &ContentPart, tracker: &mut FunctionNameTracker) -> Result<Value, LlmError> {
    match part {
        ContentPart::Text { text } | ContentPart::Reasoning { text, .. } => {
            Ok(json!({ "text": text }))
        }
        ContentPart::Media {
            mime_type,
            data,
            url,
            ..
        } => {
            if let Some(data) = data {
                Ok(json!({ "inlineData": { "mimeType": mime_type, "data": data } }))
            } else if let Some(url) = url {
                Ok(json!({ "fileData": { "mimeType": mime_type, "fileUri": url } }))
            } else {
                Err(LlmError::new(
                    LlmErrorReason::InvalidRequest,
                    "media content part has neither data nor url",
                ))
            }
        }
        ContentPart::ToolCall {
            id,
            name,
            input,
            provider_metadata,
        } => {
            tracker.record_call(id, name);
            let mut part = Map::new();
            part.insert(
                "functionCall".to_string(),
                json!({ "name": name, "args": input }),
            );
            let signature = gemini_thought_signature(provider_metadata)
                .unwrap_or_else(|| GEMINI_DUMMY_THOUGHT_SIGNATURE.to_string());
            part.insert("thoughtSignature".to_string(), Value::String(signature));
            Ok(Value::Object(part))
        }
        ContentPart::ToolResult {
            tool_call_id,
            content,
            is_error,
        } => {
            let name = tracker
                .take_name(tool_call_id)
                .unwrap_or_else(|| tool_call_id.clone());
            let response = json!({
                "content": flatten_tool_result_content(content),
                "is_error": is_error,
            });
            Ok(
                json!({ "functionResponse": { "name": name, "id": tool_call_id, "response": response } }),
            )
        }
    }
}

fn gemini_thought_signature(provider_metadata: &Option<Value>) -> Option<String> {
    provider_metadata
        .as_ref()
        .and_then(|meta| meta.get("google").or_else(|| meta.get("gemini")))
        .and_then(|google| google.get("thought_signature"))
        .or_else(|| {
            provider_metadata
                .as_ref()
                .and_then(|meta| meta.get("thought_signature"))
        })
        .or_else(|| {
            provider_metadata
                .as_ref()
                .and_then(|meta| meta.get("thoughtSignature"))
        })
        .and_then(Value::as_str)
        .map(str::to_string)
}

fn flatten_tool_result_content(content: &[ContentPart]) -> String {
    let mut text = String::new();
    for part in content {
        match part {
            ContentPart::Text { text: fragment }
            | ContentPart::Reasoning { text: fragment, .. } => {
                text.push_str(fragment);
            }
            ContentPart::Media { mime_type, .. } => {
                if !text.is_empty() {
                    text.push('\n');
                }
                text.push_str(&format!(
                    "[{mime_type} media omitted from function response]"
                ));
            }
            ContentPart::ToolResult { content, .. } => {
                text.push_str(&flatten_tool_result_content(content))
            }
            ContentPart::ToolCall { .. } => {}
        }
    }
    text
}

fn build_tool_config(choice: &ToolChoice) -> Value {
    let function_calling_config = match choice {
        ToolChoice::Auto => json!({ "mode": "AUTO" }),
        ToolChoice::None => json!({ "mode": "NONE" }),
        ToolChoice::Required => json!({ "mode": "ANY" }),
        ToolChoice::Tool { name } => json!({ "mode": "ANY", "allowedFunctionNames": [name] }),
    };
    json!({ "functionCallingConfig": function_calling_config })
}

struct GeminiStream {
    lifecycle: Lifecycle,
    tools: ToolStream,
    finish_reason: Option<FinishReason>,
    usage: Usage,
    next_tool_id: u64,
    tool_part_ids: Vec<String>,
}

impl GeminiStream {
    fn new() -> Self {
        Self {
            lifecycle: Lifecycle::new(),
            tools: ToolStream::new(),
            finish_reason: None,
            usage: Usage::default(),
            next_tool_id: 0,
            tool_part_ids: Vec::new(),
        }
    }

    fn tool_part_id(&mut self, index: usize) -> (String, bool) {
        let first_seen = self.tool_part_ids.len() <= index;
        while self.tool_part_ids.len() <= index {
            let id = format!("gemini_call_{}", self.next_tool_id);
            self.next_tool_id += 1;
            self.tool_part_ids.push(id);
        }
        (self.tool_part_ids[index].clone(), first_seen)
    }
}

impl ProtocolStream for GeminiStream {
    fn on_frame(&mut self, frame: &SseFrame) -> Result<Vec<LlmEvent>, LlmError> {
        let data = frame.data.trim();
        if data.is_empty() || data == "[DONE]" {
            return Ok(Vec::new());
        }
        let chunk: Value = serde_json::from_str(data).map_err(|e| {
            LlmError::new(LlmErrorReason::Decode, format!("invalid Gemini chunk: {e}"))
        })?;

        if let Some(usage) = chunk.get("usageMetadata") {
            self.usage = parse_usage(usage);
        }

        let mut events = Vec::new();
        let Some(candidate) = chunk
            .get("candidates")
            .and_then(Value::as_array)
            .and_then(|items| items.first())
        else {
            return Ok(events);
        };

        if let Some(reason) = candidate.get("finishReason").and_then(Value::as_str) {
            self.finish_reason = Some(map_finish_reason(reason));
        }

        if let Some(parts) = candidate
            .get("content")
            .and_then(|content| content.get("parts"))
            .and_then(Value::as_array)
        {
            let mut function_call_index = 0;
            for part in parts {
                if let Some(text) = part.get("text").and_then(Value::as_str) {
                    if !text.is_empty() {
                        events.extend(self.lifecycle.text_delta(TEXT_ID, text));
                    }
                }
                if let Some(call) = part.get("functionCall") {
                    let Some(name) = call.get("name").and_then(Value::as_str) else {
                        continue;
                    };
                    let args = call.get("args").cloned().unwrap_or_else(|| json!({}));
                    let (id, first_seen) = self.tool_part_id(function_call_index);
                    function_call_index += 1;
                    if first_seen {
                        events.extend(self.tools.delta(&id, Some(name), &args.to_string()));
                    }
                    if let Some(signature) = part.get("thoughtSignature").and_then(Value::as_str) {
                        self.tools.set_provider_metadata(
                            &id,
                            Some(json!({ "google": { "thought_signature": signature } })),
                        );
                    }
                }
            }
        }

        Ok(events)
    }

    fn finish(&mut self) -> Result<Vec<LlmEvent>, LlmError> {
        if self.lifecycle.is_finished() {
            return Ok(Vec::new());
        }
        let mut events = Vec::new();
        events.extend(self.lifecycle.text_end(TEXT_ID));
        events.extend(self.tools.flush()?);
        if self.usage.total_tokens == 0 {
            self.usage.total_tokens = self.usage.computed_total();
        }
        events.extend(self.lifecycle.finish(self.usage, self.finish_reason));
        Ok(events)
    }
}

fn map_finish_reason(reason: &str) -> FinishReason {
    match reason {
        "STOP" => FinishReason::Stop,
        "MAX_TOKENS" => FinishReason::Length,
        "SAFETY" | "RECITATION" => FinishReason::ContentFilter,
        "MALFORMED_FUNCTION_CALL" => FinishReason::ToolUse,
        _ => FinishReason::Other,
    }
}

fn parse_usage(usage: &Value) -> Usage {
    let u = |key: &str| usage.get(key).and_then(Value::as_u64).unwrap_or(0);
    Usage {
        input_tokens: u("promptTokenCount"),
        cached_input_tokens: u("cachedContentTokenCount"),
        cache_creation_input_tokens: 0,
        output_tokens: u("candidatesTokenCount"),
        reasoning_output_tokens: u("thoughtsTokenCount"),
        total_tokens: u("totalTokenCount"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{LlmRequest, SystemPart, ToolDefinition};

    fn frame(data: &str) -> SseFrame {
        SseFrame {
            event: None,
            data: data.to_string(),
        }
    }

    #[test]
    fn build_body_removes_json_schema_fields_that_gemini_rejects() {
        let mut req = LlmRequest::new("gemini-3.5-flash", "google");
        req.messages.push(Message::user_text("hi"));
        req.tools.push(ToolDefinition {
            name: "lookup".into(),
            description: "Look up data".into(),
            input_schema: json!({
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "q": {
                        "type": "string",
                        "additionalProperties": false
                    },
                    "filters": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "additionalProperties": false,
                            "properties": {
                                "value": {
                                    "type": "string",
                                    "additionalProperties": false
                                }
                            }
                        }
                    }
                }
            }),
            output_schema: None,
            namespace: None,
            namespace_description: None,
        });

        let body = GeminiGenerateContentProtocol::new()
            .build_body(&req)
            .unwrap();

        let parameters = &body["tools"][0]["functionDeclarations"][0]["parameters"];
        assert_eq!(parameters["type"], "object");
        assert_eq!(parameters["properties"]["q"]["type"], "string");
        assert_eq!(
            parameters["properties"]["filters"]["items"]["properties"]["value"]["type"],
            "string"
        );
        assert!(!body.to_string().contains("additionalProperties"));
    }

    #[test]
    fn build_body_maps_system_contents_tools_and_generation() {
        let mut req = LlmRequest::new("gemini-3.5-flash", "google");
        req.system.push(SystemPart::new("be terse"));
        req.messages.push(Message::user_text("hi"));
        req.tools.push(ToolDefinition {
            name: "lookup".into(),
            description: "Look up data".into(),
            input_schema: json!({"type":"object","properties":{"q":{"type":"string"}}}),
            output_schema: None,
            namespace: None,
            namespace_description: None,
        });
        req.tool_choice = Some(ToolChoice::Auto);
        req.generation.max_tokens = Some(128);

        let body = GeminiGenerateContentProtocol::new()
            .build_body(&req)
            .unwrap();

        assert_eq!(body["systemInstruction"]["parts"][0]["text"], "be terse");
        assert_eq!(body["contents"][0]["role"], "user");
        assert_eq!(body["contents"][0]["parts"][0]["text"], "hi");
        assert_eq!(
            body["tools"][0]["functionDeclarations"][0]["name"],
            "lookup"
        );
        assert_eq!(body["toolConfig"]["functionCallingConfig"]["mode"], "AUTO");
        assert_eq!(body["generationConfig"]["maxOutputTokens"], 128);
    }

    #[test]
    fn build_body_preserves_tool_call_thought_signature() {
        let mut req = LlmRequest::new("gemini-3.1-pro-preview", "google");
        req.messages.push(Message::new(
            MessageRole::Assistant,
            vec![ContentPart::ToolCall {
                id: "gemini_call_0".into(),
                name: "browser".into(),
                input: json!({ "action": "status" }),
                provider_metadata: Some(json!({
                    "google": {
                        "thought_signature": "sig-model-call"
                    }
                })),
            }],
        ));

        let body = GeminiGenerateContentProtocol::new()
            .build_body(&req)
            .unwrap();

        assert_eq!(
            body["contents"][0]["parts"][0]["functionCall"],
            json!({ "name": "browser", "args": { "action": "status" } })
        );
        assert_eq!(
            body["contents"][0]["parts"][0]["thoughtSignature"],
            json!("sig-model-call")
        );
    }

    #[test]
    fn build_body_adds_dummy_thought_signature_when_tool_call_has_none() {
        let mut req = LlmRequest::new("gemini-3.1-pro-preview", "google");
        req.messages.push(Message::new(
            MessageRole::Assistant,
            vec![ContentPart::ToolCall {
                id: "gemini_call_0".into(),
                name: "browser".into(),
                input: json!({ "cmd": "connect local" }),
                provider_metadata: None,
            }],
        ));

        let body = GeminiGenerateContentProtocol::new()
            .build_body(&req)
            .unwrap();

        assert_eq!(
            body["contents"][0]["parts"][0]["thoughtSignature"],
            json!("skip_thought_signature_validator")
        );
    }

    #[test]
    fn build_body_uses_function_name_for_function_response() {
        let mut req = LlmRequest::new("gemini-3.1-pro-preview", "google");
        req.messages.push(Message::new(
            MessageRole::Assistant,
            vec![ContentPart::ToolCall {
                id: "gemini_call_0".into(),
                name: "browser".into(),
                input: json!({ "cmd": "connect local" }),
                provider_metadata: Some(json!({
                    "google": {
                        "thought_signature": "sig-browser"
                    }
                })),
            }],
        ));
        req.messages.push(Message::new(
            MessageRole::Tool,
            vec![ContentPart::ToolResult {
                tool_call_id: "gemini_call_0".into(),
                content: vec![ContentPart::text("connected")],
                is_error: false,
            }],
        ));

        let body = GeminiGenerateContentProtocol::new()
            .build_body(&req)
            .unwrap();

        assert_eq!(
            body["contents"][1]["parts"][0]["functionResponse"]["name"],
            json!("browser")
        );
        assert_eq!(
            body["contents"][1]["parts"][0]["functionResponse"]["id"],
            json!("gemini_call_0")
        );
        assert!(body["contents"][1]["parts"][0]["functionResponse"]["response"]["id"].is_null());
    }

    #[test]
    fn build_body_pairs_reused_synthetic_call_ids_in_order() {
        let mut req = LlmRequest::new("gemini-3.1-pro-preview", "google");
        req.messages.push(Message::new(
            MessageRole::Assistant,
            vec![ContentPart::ToolCall {
                id: "gemini_call_0".into(),
                name: "browser".into(),
                input: json!({ "cmd": "connect local" }),
                provider_metadata: Some(json!({
                    "google": {
                        "thought_signature": "sig-browser"
                    }
                })),
            }],
        ));
        req.messages.push(Message::new(
            MessageRole::Tool,
            vec![ContentPart::ToolResult {
                tool_call_id: "gemini_call_0".into(),
                content: vec![ContentPart::text("connected")],
                is_error: false,
            }],
        ));
        req.messages.push(Message::new(
            MessageRole::Assistant,
            vec![ContentPart::ToolCall {
                id: "gemini_call_0".into(),
                name: "browser_script".into(),
                input: json!({ "code": "page_info()" }),
                provider_metadata: Some(json!({
                    "google": {
                        "thought_signature": "sig-script"
                    }
                })),
            }],
        ));
        req.messages.push(Message::new(
            MessageRole::Tool,
            vec![ContentPart::ToolResult {
                tool_call_id: "gemini_call_0".into(),
                content: vec![ContentPart::text("page loaded")],
                is_error: false,
            }],
        ));

        let body = GeminiGenerateContentProtocol::new()
            .build_body(&req)
            .unwrap();

        assert_eq!(
            body["contents"][1]["parts"][0]["functionResponse"]["name"],
            json!("browser")
        );
        assert_eq!(
            body["contents"][3]["parts"][0]["functionResponse"]["name"],
            json!("browser_script")
        );
    }

    #[test]
    fn decodes_text_usage_and_function_call() {
        let mut stream = GeminiStream::new();
        let mut events = stream
            .on_frame(&frame(
                r#"{"candidates":[{"content":{"parts":[{"text":"hello"},{"functionCall":{"name":"lookup","args":{"q":"x"}}}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":2,"candidatesTokenCount":3,"totalTokenCount":5}}"#,
            ))
            .unwrap();
        events.extend(stream.finish().unwrap());

        assert!(events
            .iter()
            .any(|event| matches!(event, LlmEvent::TextDelta { delta, .. } if delta == "hello")));
        assert!(events
            .iter()
            .any(|event| matches!(event, LlmEvent::ToolCall { name, input, .. } if name == "lookup" && input["q"] == "x")));
        assert!(events
            .iter()
            .any(|event| matches!(event, LlmEvent::Finish { usage, finish_reason: Some(FinishReason::Stop) } if usage.total_tokens == 5)));
    }

    #[test]
    fn decodes_function_call_thought_signature() {
        let mut stream = GeminiStream::new();
        let mut events = stream
            .on_frame(&frame(
                r#"{"candidates":[{"content":{"parts":[{"functionCall":{"name":"browser","args":{"action":"status"}},"thoughtSignature":"sig-model-call"}]},"finishReason":"STOP"}]}"#,
            ))
            .unwrap();
        events.extend(stream.finish().unwrap());

        assert!(events.iter().any(|event| matches!(
            event,
            LlmEvent::ToolCall {
                name,
                input,
                provider_metadata: Some(meta),
                ..
            } if name == "browser" && input["action"] == "status" && meta["google"]["thought_signature"] == "sig-model-call"
        )));
    }

    #[test]
    fn decodes_function_call_signature_from_later_stream_chunk() {
        let mut stream = GeminiStream::new();
        let mut events = stream
            .on_frame(&frame(
                r#"{"candidates":[{"content":{"parts":[{"functionCall":{"name":"browser","args":{"action":"status"}}}]}}]}"#,
            ))
            .unwrap();
        events.extend(
            stream
                .on_frame(&frame(
                    r#"{"candidates":[{"content":{"parts":[{"functionCall":{"name":"browser","args":{"action":"status"}},"thoughtSignature":"sig-model-call"}]},"finishReason":"STOP"}]}"#,
                ))
                .unwrap(),
        );
        events.extend(stream.finish().unwrap());

        let tool_calls = events
            .iter()
            .filter(|event| matches!(event, LlmEvent::ToolCall { .. }))
            .collect::<Vec<_>>();
        assert_eq!(tool_calls.len(), 1);
        assert!(matches!(
            tool_calls[0],
            LlmEvent::ToolCall {
                id,
                provider_metadata: Some(meta),
                ..
            } if id == "gemini_call_0"
                && meta["google"]["thought_signature"] == "sig-model-call"
        ));
    }
}
