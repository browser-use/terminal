//! OpenAI-compatible Chat Completions protocol (`/chat/completions`).
//!
//! This is the de-facto standard chat wire format spoken by OpenAI as well as
//! the many compatible backends (Ollama, OpenRouter, DeepSeek, Fireworks, ...).
//! Requests carry a flat `messages` array with `system`/`user`/`assistant`/`tool`
//! roles; responses stream as SSE frames whose `choices[].delta` objects carry
//! text and incremental `tool_calls`, followed by a terminal frame bearing a
//! `finish_reason` and (when requested) a `usage` object.

use std::collections::BTreeMap;

use serde_json::{json, Map, Value};

use crate::protocols::utils::{Lifecycle, ToolStream};
use crate::route::framing::SseFrame;
use crate::route::protocol::{Protocol, ProtocolStream};
use crate::schema::{
    ContentPart, FinishReason, GenerationOptions, LlmError, LlmErrorReason, LlmEvent, LlmRequest,
    Message, MessageRole, ReasoningEffort, ToolChoice, Usage,
};

/// Stable block ids used to drive the [`Lifecycle`]. Chat Completions streams a
/// single text/reasoning channel per choice, so one id each is sufficient.
const TEXT_ID: &str = "0";
const REASONING_ID: &str = "reasoning";

/// Adapter for the OpenAI-compatible Chat Completions wire format.
#[derive(Debug, Clone, Copy)]
pub struct OpenAiChatProtocol {
    include_image_content: bool,
}

impl OpenAiChatProtocol {
    /// Create a new protocol adapter.
    pub fn new() -> Self {
        Self {
            include_image_content: true,
        }
    }

    /// Create a protocol adapter that keeps image media out of Chat content.
    ///
    /// Some OpenAI-compatible endpoints, including DeepSeek's chat endpoint, use
    /// the Chat Completions text-part schema but reject `image_url` content
    /// parts. Tool screenshots are retained as textual omission notes instead.
    pub fn without_image_content() -> Self {
        Self {
            include_image_content: false,
        }
    }
}

impl Default for OpenAiChatProtocol {
    fn default() -> Self {
        Self::new()
    }
}

impl Protocol for OpenAiChatProtocol {
    fn build_body(&self, req: &LlmRequest) -> Result<Value, LlmError> {
        let mut body = Map::new();

        body.insert("model".to_string(), json!(req.model.as_str()));

        // The system prompt is prepended as a `role:"system"` message.
        let mut messages: Vec<Value> = Vec::new();
        if !req.system.is_empty() {
            let system_text: String = req
                .system
                .iter()
                .map(|part| part.text.as_str())
                .collect::<Vec<_>>()
                .join("\n\n");
            messages.push(json!({ "role": "system", "content": system_text }));
        }
        for message in &req.messages {
            append_message(message, &mut messages, self.include_image_content)?;
        }
        body.insert("messages".to_string(), Value::Array(messages));

        if !req.tools.is_empty() {
            let tools: Vec<Value> = req
                .tools
                .iter()
                .map(|tool| {
                    json!({
                        "type": "function",
                        "function": {
                            "name": tool.name,
                            "description": tool.description,
                            "parameters": tool.input_schema,
                        }
                    })
                })
                .collect();
            body.insert("tools".to_string(), Value::Array(tools));
        }

        if let Some(choice) = &req.tool_choice {
            body.insert("tool_choice".to_string(), build_tool_choice(choice));
        }

        apply_generation(&mut body, &req.generation);

        body.insert("stream".to_string(), Value::Bool(true));
        body.insert(
            "stream_options".to_string(),
            json!({ "include_usage": true }),
        );

        Ok(Value::Object(body))
    }

    fn decoder(&self) -> Box<dyn ProtocolStream> {
        Box::new(OpenAiChatStream::new())
    }
}

/// Pass through the generation knobs Chat Completions understands.
fn apply_generation(body: &mut Map<String, Value>, gen: &GenerationOptions) {
    if let Some(temperature) = gen.temperature {
        body.insert("temperature".to_string(), json!(temperature));
    }
    if let Some(top_p) = gen.top_p {
        body.insert("top_p".to_string(), json!(top_p));
    }
    if let Some(max_tokens) = gen.max_tokens {
        body.insert("max_tokens".to_string(), json!(max_tokens));
    }
    if let Some(effort) = gen.reasoning_effort {
        body.insert(
            "reasoning_effort".to_string(),
            json!(reasoning_effort_str(effort)),
        );
    }
    if !gen.stop.is_empty() {
        body.insert("stop".to_string(), json!(gen.stop));
    }
}

/// Lower the neutral reasoning effort to the Chat Completions wire string.
fn reasoning_effort_str(effort: ReasoningEffort) -> &'static str {
    match effort {
        ReasoningEffort::None => "none",
        ReasoningEffort::Minimal => "minimal",
        ReasoningEffort::Low => "low",
        ReasoningEffort::Medium => "medium",
        ReasoningEffort::High => "high",
        ReasoningEffort::Xhigh => "xhigh",
    }
}

/// Translate a neutral [`Message`] into one or more Chat Completions message objects.
fn append_message(
    message: &Message,
    out: &mut Vec<Value>,
    include_image_content: bool,
) -> Result<(), LlmError> {
    match message.role {
        MessageRole::Tool => {
            append_tool_messages(message, out, include_image_content);
            Ok(())
        }
        MessageRole::Assistant => {
            out.push(build_assistant_message(message)?);
            Ok(())
        }
        MessageRole::System => {
            out.push(build_simple_message(
                "system",
                message,
                include_image_content,
            ));
            Ok(())
        }
        MessageRole::Developer => {
            out.push(build_simple_message(
                "developer",
                message,
                include_image_content,
            ));
            Ok(())
        }
        MessageRole::User => {
            out.push(build_simple_message("user", message, include_image_content));
            Ok(())
        }
    }
}

/// Render a `system`/`developer`/`user` message.
fn build_simple_message(role: &str, message: &Message, include_image_content: bool) -> Value {
    if role == "user" {
        json!({ "role": role, "content": build_user_content(message, include_image_content) })
    } else {
        json!({ "role": role, "content": collect_text(message) })
    }
}

/// Render an `assistant` message, including any `tool_calls`.
fn build_assistant_message(message: &Message) -> Result<Value, LlmError> {
    let mut obj = Map::new();
    obj.insert("role".to_string(), json!("assistant"));

    let mut tool_calls: Vec<Value> = Vec::new();
    for part in &message.content {
        if let ContentPart::ToolCall {
            id, name, input, ..
        } = part
        {
            let arguments = serde_json::to_string(input).map_err(|e| {
                LlmError::new(
                    LlmErrorReason::InvalidRequest,
                    format!("tool call arguments not serializable: {e}"),
                )
            })?;
            tool_calls.push(json!({
                "id": id,
                "type": "function",
                "function": { "name": name, "arguments": arguments },
            }));
        }
    }
    // Omit `content` for a tool-only assistant turn rather than sending an empty
    // string. OpenAI tolerates `content: ""`, but strict downstream providers
    // (e.g. Amazon Bedrock via OpenRouter) reject a blank text content block:
    // "The text field in the ContentBlock object ... is blank."
    let text = collect_text(message);
    if !text.is_empty() || tool_calls.is_empty() {
        obj.insert("content".to_string(), json!(text));
    }
    if !tool_calls.is_empty() {
        obj.insert("tool_calls".to_string(), Value::Array(tool_calls));
    }

    Ok(Value::Object(obj))
}

/// Render a `tool` message from the first [`ContentPart::ToolResult`] it carries.
///
/// Chat Completions tool messages are text-only in practice. When a tool result
/// carries image media, keep the required tool message as text and append a
/// follow-up user message that carries the actual image parts.
fn append_tool_messages(message: &Message, out: &mut Vec<Value>, include_image_content: bool) {
    let mut obj = Map::new();
    obj.insert("role".to_string(), json!("tool"));
    for part in &message.content {
        if let ContentPart::ToolResult {
            tool_call_id,
            content,
            ..
        } = part
        {
            obj.insert("tool_call_id".to_string(), json!(tool_call_id));
            obj.insert(
                "content".to_string(),
                json!(tool_result_text_for_chat(content, include_image_content)),
            );
            out.push(Value::Object(obj));
            if let Some(visual_context) =
                tool_result_visual_context(tool_call_id, content, include_image_content)
            {
                out.push(visual_context);
            }
            break;
        }
    }
}

fn build_user_content(message: &Message, include_image_content: bool) -> Value {
    let mut text = String::new();
    let mut parts = Vec::new();
    let mut has_media = false;
    for part in &message.content {
        match part {
            ContentPart::Text { text: fragment }
            | ContentPart::Reasoning { text: fragment, .. } => {
                text.push_str(fragment);
                if !fragment.is_empty() {
                    parts.push(json!({ "type": "text", "text": fragment }));
                }
            }
            ContentPart::Media {
                mime_type,
                data,
                url,
                detail,
            } => {
                if include_image_content {
                    if let Some(image) = chat_image_part(
                        mime_type,
                        data.as_deref(),
                        url.as_deref(),
                        detail.as_deref(),
                    ) {
                        has_media = true;
                        parts.push(image);
                    }
                } else if mime_type.starts_with("image/") {
                    append_omitted_image_text(&mut text, &mut parts);
                }
            }
            ContentPart::ToolResult { content, .. } => {
                append_chat_content_parts(
                    content,
                    &mut text,
                    &mut parts,
                    &mut has_media,
                    include_image_content,
                );
            }
            ContentPart::ToolCall { .. } => {}
        }
    }
    if has_media {
        Value::Array(parts)
    } else {
        Value::String(text)
    }
}

fn tool_result_text_for_chat(content: &[ContentPart], include_image_content: bool) -> String {
    let mut text = String::new();
    let mut image_count = 0usize;
    collect_tool_result_text_and_images(content, &mut text, &mut image_count);
    if image_count > 0 {
        if !text.is_empty() {
            text.push('\n');
        }
        if include_image_content {
            text.push_str(&format!(
                "[{image_count} image(s) attached in the following visual context message]"
            ));
        } else {
            text.push_str(&format!(
                "[{image_count} image(s) omitted because this model endpoint does not accept image content]"
            ));
        }
    }
    text
}

fn tool_result_visual_context(
    tool_call_id: &str,
    content: &[ContentPart],
    include_image_content: bool,
) -> Option<Value> {
    let mut parts = vec![json!({
        "type": "text",
        "text": if include_image_content {
            format!("Visual context from tool call {tool_call_id}. Use these images as the tool result.")
        } else {
            format!("Visual output from tool call {tool_call_id} was omitted because this model endpoint does not accept image content. Continue using text, DOM state, and browser script inspection instead.")
        },
    })];
    if include_image_content {
        append_tool_result_images(content, &mut parts);
        (parts.len() > 1).then_some(json!({
            "role": "user",
            "content": parts,
        }))
    } else {
        let mut text = String::new();
        let mut image_count = 0usize;
        collect_tool_result_text_and_images(content, &mut text, &mut image_count);
        (image_count > 0).then_some(json!({
            "role": "user",
            "content": parts,
        }))
    }
}

fn append_chat_content_parts(
    content: &[ContentPart],
    text: &mut String,
    parts: &mut Vec<Value>,
    has_media: &mut bool,
    include_image_content: bool,
) {
    for part in content {
        match part {
            ContentPart::Text { text: fragment }
            | ContentPart::Reasoning { text: fragment, .. } => {
                text.push_str(fragment);
                if !fragment.is_empty() {
                    parts.push(json!({ "type": "text", "text": fragment }));
                }
            }
            ContentPart::Media {
                mime_type,
                data,
                url,
                detail,
            } => {
                if include_image_content {
                    if let Some(image) = chat_image_part(
                        mime_type,
                        data.as_deref(),
                        url.as_deref(),
                        detail.as_deref(),
                    ) {
                        *has_media = true;
                        parts.push(image);
                    }
                } else if mime_type.starts_with("image/") {
                    append_omitted_image_text(text, parts);
                }
            }
            ContentPart::ToolResult { content, .. } => {
                append_chat_content_parts(content, text, parts, has_media, include_image_content);
            }
            ContentPart::ToolCall { .. } => {}
        }
    }
}

fn append_omitted_image_text(text: &mut String, parts: &mut Vec<Value>) {
    const OMITTED: &str = "[image omitted: selected model endpoint does not accept image content]";
    text.push_str(OMITTED);
    parts.push(json!({ "type": "text", "text": OMITTED }));
}

fn collect_tool_result_text_and_images(
    content: &[ContentPart],
    text: &mut String,
    image_count: &mut usize,
) {
    for part in content {
        match part {
            ContentPart::Text { text: fragment }
            | ContentPart::Reasoning { text: fragment, .. } => {
                text.push_str(fragment);
            }
            ContentPart::Media { mime_type, .. } => {
                if mime_type.starts_with("image/") {
                    *image_count += 1;
                }
            }
            ContentPart::ToolResult { content, .. } => {
                collect_tool_result_text_and_images(content, text, image_count);
            }
            ContentPart::ToolCall { .. } => {}
        }
    }
}

fn append_tool_result_images(content: &[ContentPart], parts: &mut Vec<Value>) {
    for part in content {
        match part {
            ContentPart::Media {
                mime_type,
                data,
                url,
                detail,
            } => {
                if let Some(image) = chat_image_part(
                    mime_type,
                    data.as_deref(),
                    url.as_deref(),
                    detail.as_deref(),
                ) {
                    parts.push(image);
                }
            }
            ContentPart::ToolResult { content, .. } => append_tool_result_images(content, parts),
            _ => {}
        }
    }
}

fn chat_image_part(
    mime_type: &str,
    data: Option<&str>,
    url: Option<&str>,
    detail: Option<&str>,
) -> Option<Value> {
    if !mime_type.starts_with("image/") {
        return None;
    }
    let resolved = match (url, data) {
        (Some(url), _) => url.to_string(),
        (None, Some(data)) => format!("data:{mime_type};base64,{data}"),
        (None, None) => return None,
    };
    let image_url = serde_json::Map::from_iter([
        ("url".to_string(), json!(resolved)),
        ("detail".to_string(), json!(detail.unwrap_or("auto"))),
    ]);
    Some(json!({
        "type": "image_url",
        "image_url": Value::Object(image_url),
    }))
}

/// Concatenate all [`ContentPart::Text`] fragments in a message into one string.
fn collect_text(message: &Message) -> String {
    collect_text_parts(&message.content)
}

/// Concatenate the text of a content-part slice into one string.
fn collect_text_parts(parts: &[ContentPart]) -> String {
    let mut text = String::new();
    for part in parts {
        if let ContentPart::Text { text: fragment } = part {
            text.push_str(fragment);
        }
    }
    text
}

/// Translate a neutral [`ToolChoice`] into the Chat Completions wire form.
fn build_tool_choice(choice: &ToolChoice) -> Value {
    match choice {
        ToolChoice::Auto => json!("auto"),
        ToolChoice::None => json!("none"),
        ToolChoice::Required => json!("required"),
        ToolChoice::Tool { name } => json!({
            "type": "function",
            "function": { "name": name }
        }),
    }
}

/// Streaming decoder for Chat Completions SSE responses.
struct OpenAiChatStream {
    lifecycle: Lifecycle,
    tools: ToolStream,
    /// Maps a tool call's stream-local `index` to the stable `id` reported on its
    /// first chunk. Later chunks carry only `index` + argument fragments, so the
    /// index is the correlator and the id is the value we emit.
    tool_ids: BTreeMap<u64, String>,
    finish_reason: Option<FinishReason>,
    usage: Usage,
}

impl OpenAiChatStream {
    fn new() -> Self {
        Self {
            lifecycle: Lifecycle::new(),
            tools: ToolStream::new(),
            tool_ids: BTreeMap::new(),
            finish_reason: None,
            usage: Usage::default(),
        }
    }
}

impl ProtocolStream for OpenAiChatStream {
    fn on_frame(&mut self, frame: &SseFrame) -> Result<Vec<LlmEvent>, LlmError> {
        let data = frame.data.trim();
        if data.is_empty() || data == "[DONE]" {
            return Ok(Vec::new());
        }

        let chunk: Value = serde_json::from_str(data).map_err(|e| {
            LlmError::new(LlmErrorReason::Decode, format!("invalid chat chunk: {e}"))
        })?;

        let mut events = Vec::new();

        if let Some(usage) = chunk.get("usage") {
            if !usage.is_null() {
                self.usage = parse_usage(usage);
            }
        }

        let Some(choice) = chunk
            .get("choices")
            .and_then(Value::as_array)
            .and_then(|c| c.first())
        else {
            return Ok(events);
        };

        if let Some(delta) = choice.get("delta") {
            if let Some(content) = delta.get("content").and_then(Value::as_str) {
                if !content.is_empty() {
                    events.extend(self.lifecycle.text_delta(TEXT_ID, content));
                }
            }
            if let Some(reasoning) = delta
                .get("reasoning")
                .or_else(|| delta.get("reasoning_content"))
                .and_then(Value::as_str)
            {
                if !reasoning.is_empty() {
                    events.extend(self.lifecycle.reasoning_delta(REASONING_ID, reasoning));
                }
            }
            if let Some(tool_calls) = delta.get("tool_calls").and_then(Value::as_array) {
                for call in tool_calls {
                    events.extend(self.handle_tool_call(call));
                }
            }
        }

        if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
            self.finish_reason = Some(map_finish_reason(reason));
        }

        Ok(events)
    }

    fn finish(&mut self) -> Result<Vec<LlmEvent>, LlmError> {
        if self.lifecycle.is_finished() {
            return Ok(Vec::new());
        }
        // Close text/reasoning, then emit completed tool calls, then the
        // terminal step_finish / finish carrying usage + reason.
        let mut events = Vec::new();
        events.extend(self.lifecycle.text_end(TEXT_ID));
        events.extend(self.lifecycle.reasoning_end(REASONING_ID));
        events.extend(self.tools.flush()?);
        if self.usage.total_tokens == 0 {
            self.usage.total_tokens = self.usage.computed_total();
        }
        events.extend(self.lifecycle.finish(self.usage, self.finish_reason));
        Ok(events)
    }
}

impl OpenAiChatStream {
    /// Feed one streamed `tool_calls[]` entry into the [`ToolStream`].
    ///
    /// The `id` and `function.name` arrive on a tool call's first chunk; later
    /// chunks for the same call carry only the array `index` plus
    /// `function.arguments` fragments. We therefore correlate on `index`,
    /// resolving the stable `id` (and `name`) recorded on the first chunk.
    fn handle_tool_call(&mut self, call: &Value) -> Vec<LlmEvent> {
        let explicit_id = call
            .get("id")
            .and_then(Value::as_str)
            .filter(|i| !i.is_empty());

        // Resolve the stable id this fragment belongs to. Prefer the array
        // `index`; fall back to a present explicit `id` if the index is absent.
        let id = match call.get("index").and_then(Value::as_u64) {
            Some(index) => match self.tool_ids.get(&index) {
                Some(id) => id.clone(),
                None => {
                    let id = explicit_id.map(str::to_string).unwrap_or(index.to_string());
                    self.tool_ids.insert(index, id.clone());
                    id
                }
            },
            None => match explicit_id {
                Some(id) => id.to_string(),
                None => return Vec::new(),
            },
        };

        let function = call.get("function");
        let name = function
            .and_then(|f| f.get("name"))
            .and_then(Value::as_str)
            .filter(|n| !n.is_empty());
        let fragment = function
            .and_then(|f| f.get("arguments"))
            .and_then(Value::as_str)
            .unwrap_or("");

        // Skip empty argument fragments: the first chunk's `"arguments":""` would
        // otherwise emit a no-op `ToolInputDelta`. When that first chunk carries
        // the tool name, open the block explicitly so `ToolInputStart` is still
        // emitted; otherwise there is nothing to do for this fragment.
        if fragment.is_empty() {
            return match name {
                Some(name) => self.tools.start(&id, name),
                None => Vec::new(),
            };
        }

        self.tools.delta(&id, name, fragment)
    }
}

/// Map a Chat Completions `finish_reason` string onto a [`FinishReason`].
fn map_finish_reason(reason: &str) -> FinishReason {
    match reason {
        "stop" => FinishReason::Stop,
        "length" => FinishReason::Length,
        "tool_calls" | "function_call" => FinishReason::ToolUse,
        "content_filter" => FinishReason::ContentFilter,
        _ => FinishReason::Other,
    }
}

/// Parse a Chat Completions `usage` object into [`Usage`].
fn parse_usage(usage: &Value) -> Usage {
    let u = |key: &str| usage.get(key).and_then(Value::as_u64).unwrap_or(0);
    let cached = usage
        .get("prompt_tokens_details")
        .and_then(|d| d.get("cached_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let reasoning = usage
        .get("completion_tokens_details")
        .and_then(|d| d.get("reasoning_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    Usage {
        input_tokens: u("prompt_tokens"),
        cached_input_tokens: cached,
        cache_creation_input_tokens: 0,
        output_tokens: u("completion_tokens"),
        reasoning_output_tokens: reasoning,
        total_tokens: u("total_tokens"),
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
    fn build_body_system_user_tool() {
        let mut req = LlmRequest::new("gpt-4o", "openai");
        req.system.push(SystemPart::new("be helpful"));
        req.messages
            .push(Message::user_text("what is the weather?"));
        req.messages.push(Message::new(
            MessageRole::Assistant,
            vec![ContentPart::ToolCall {
                id: "call_1".into(),
                name: "get_weather".into(),
                input: json!({ "city": "Paris" }),
                provider_metadata: None,
            }],
        ));
        req.messages.push(Message::new(
            MessageRole::Tool,
            vec![ContentPart::ToolResult {
                tool_call_id: "call_1".into(),
                content: vec![ContentPart::text("sunny")],
                is_error: false,
            }],
        ));
        req.tools.push(ToolDefinition {
            name: "get_weather".into(),
            description: "Look up the weather".into(),
            input_schema: json!({
                "type": "object",
                "properties": { "city": { "type": "string" } }
            }),
            output_schema: None,
            namespace: None,
            namespace_description: None,
        });
        req.tool_choice = Some(ToolChoice::Auto);
        // 0.5 is exactly representable in both f32 and f64, so the widening that
        // happens when serde lowers the f32 into a JSON number is lossless.
        req.generation.temperature = Some(0.5);
        req.generation.max_tokens = Some(256);

        let body = OpenAiChatProtocol::new().build_body(&req).unwrap();

        let expected = json!({
            "model": "gpt-4o",
            "messages": [
                { "role": "system", "content": "be helpful" },
                { "role": "user", "content": "what is the weather?" },
                {
                    "role": "assistant",
                    "tool_calls": [
                        {
                            "id": "call_1",
                            "type": "function",
                            "function": {
                                "name": "get_weather",
                                "arguments": "{\"city\":\"Paris\"}"
                            }
                        }
                    ]
                },
                {
                    "role": "tool",
                    "tool_call_id": "call_1",
                    "content": "sunny"
                }
            ],
            "tools": [
                {
                    "type": "function",
                    "function": {
                        "name": "get_weather",
                        "description": "Look up the weather",
                        "parameters": {
                            "type": "object",
                            "properties": { "city": { "type": "string" } }
                        }
                    }
                }
            ],
            "tool_choice": "auto",
            "temperature": 0.5,
            "max_tokens": 256,
            "stream": true,
            "stream_options": { "include_usage": true }
        });

        assert_eq!(body, expected);
    }

    #[test]
    fn build_body_preserves_user_image_content() {
        let mut req = LlmRequest::new("gpt-4o", "openai");
        req.messages.push(Message::new(
            MessageRole::User,
            vec![
                ContentPart::text("describe"),
                ContentPart::Media {
                    mime_type: "image/png".into(),
                    data: Some("AAAA".into()),
                    url: None,
                    detail: Some("original".into()),
                },
                ContentPart::Media {
                    mime_type: "image/jpeg".into(),
                    data: Some("BBBB".into()),
                    url: None,
                    detail: None,
                },
            ],
        ));

        let body = OpenAiChatProtocol::new().build_body(&req).unwrap();
        let content = body["messages"][0]["content"].as_array().unwrap();
        assert_eq!(content[0], json!({ "type": "text", "text": "describe" }));
        assert_eq!(content[1]["type"], json!("image_url"));
        assert_eq!(
            content[1]["image_url"]["url"],
            json!("data:image/png;base64,AAAA")
        );
        assert_eq!(content[1]["image_url"]["detail"], json!("original"));
        assert_eq!(
            content[2]["image_url"]["url"],
            json!("data:image/jpeg;base64,BBBB")
        );
        assert_eq!(content[2]["image_url"]["detail"], json!("auto"));
    }

    #[test]
    fn build_body_keeps_tool_image_context_in_followup_user_message() {
        let mut req = LlmRequest::new("gpt-4o", "openai");
        req.messages.push(Message::new(
            MessageRole::Assistant,
            vec![ContentPart::ToolCall {
                id: "call_view".into(),
                name: "view_image".into(),
                input: json!({ "path": "shot.png" }),
                provider_metadata: None,
            }],
        ));
        req.messages.push(Message::new(
            MessageRole::Tool,
            vec![ContentPart::ToolResult {
                tool_call_id: "call_view".into(),
                content: vec![ContentPart::Media {
                    mime_type: "image/png".into(),
                    data: Some("AAAA".into()),
                    url: None,
                    detail: Some("high".into()),
                }],
                is_error: false,
            }],
        ));

        let body = OpenAiChatProtocol::new().build_body(&req).unwrap();
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[1]["role"], json!("tool"));
        assert_eq!(messages[1]["tool_call_id"], json!("call_view"));
        assert!(messages[1]["content"]
            .as_str()
            .unwrap()
            .contains("image(s) attached"));
        assert_eq!(messages[2]["role"], json!("user"));
        assert_eq!(messages[2]["content"][1]["type"], json!("image_url"));
        assert_eq!(
            messages[2]["content"][1]["image_url"]["url"],
            json!("data:image/png;base64,AAAA")
        );
        assert_eq!(
            messages[2]["content"][1]["image_url"]["detail"],
            json!("high")
        );
    }

    #[test]
    fn build_body_specific_tool_choice_and_no_options() {
        let mut req = LlmRequest::new("llama3", "ollama");
        req.messages.push(Message::user_text("hi"));
        req.tool_choice = Some(ToolChoice::Tool { name: "go".into() });

        let body = OpenAiChatProtocol::new().build_body(&req).unwrap();
        assert_eq!(
            body["tool_choice"],
            json!({ "type": "function", "function": { "name": "go" } })
        );
        // No system message when `system` is empty.
        assert_eq!(body["messages"].as_array().unwrap().len(), 1);
        // No tools, no sampling knobs: those keys must be absent.
        assert!(body.get("tools").is_none());
        assert!(body.get("temperature").is_none());
        assert!(body.get("max_tokens").is_none());
    }

    #[test]
    fn decoder_text_then_tool_call_with_usage() {
        let mut stream = OpenAiChatProtocol::new().decoder();
        let mut events = Vec::new();

        // Streamed text answer across two deltas.
        events.extend(
            stream
                .on_frame(&frame(r#"{"choices":[{"delta":{"content":"Hel"}}]}"#))
                .unwrap(),
        );
        events.extend(
            stream
                .on_frame(&frame(r#"{"choices":[{"delta":{"content":"lo"}}]}"#))
                .unwrap(),
        );

        // Tool call: id + name on the first chunk, arguments in fragments.
        events.extend(
            stream
                .on_frame(&frame(
                    r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_42","type":"function","function":{"name":"get_weather","arguments":""}}]}}]}"#,
                ))
                .unwrap(),
        );
        events.extend(
            stream
                .on_frame(&frame(
                    r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"city\":"}}]}}]}"#,
                ))
                .unwrap(),
        );
        events.extend(
            stream
                .on_frame(&frame(
                    r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"Paris\"}"}}]}}]}"#,
                ))
                .unwrap(),
        );

        // Terminal chunk: finish_reason.
        events.extend(
            stream
                .on_frame(&frame(
                    r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#,
                ))
                .unwrap(),
        );
        // Usage chunk (from include_usage), choices empty.
        events.extend(
            stream
                .on_frame(&frame(
                    r#"{"choices":[],"usage":{"prompt_tokens":10,"completion_tokens":5,"total_tokens":15}}"#,
                ))
                .unwrap(),
        );
        events.extend(stream.on_frame(&frame("[DONE]")).unwrap());
        events.extend(stream.finish().unwrap());

        let usage = Usage {
            input_tokens: 10,
            cached_input_tokens: 0,
            cache_creation_input_tokens: 0,
            output_tokens: 5,
            reasoning_output_tokens: 0,
            total_tokens: 15,
        };
        let expected = vec![
            LlmEvent::StepStart,
            LlmEvent::TextStart { id: TEXT_ID.into() },
            LlmEvent::TextDelta {
                id: TEXT_ID.into(),
                delta: "Hel".into(),
            },
            LlmEvent::TextDelta {
                id: TEXT_ID.into(),
                delta: "lo".into(),
            },
            LlmEvent::ToolInputStart {
                id: "call_42".into(),
                name: "get_weather".into(),
            },
            LlmEvent::ToolInputDelta {
                id: "call_42".into(),
                delta: "{\"city\":".into(),
            },
            LlmEvent::ToolInputDelta {
                id: "call_42".into(),
                delta: "\"Paris\"}".into(),
            },
            LlmEvent::TextEnd {
                id: TEXT_ID.into(),
                phase: None,
            },
            LlmEvent::ToolInputEnd {
                id: "call_42".into(),
            },
            LlmEvent::ToolCall {
                id: "call_42".into(),
                name: "get_weather".into(),
                namespace: None,
                input: json!({ "city": "Paris" }),
            },
            LlmEvent::StepFinish {
                usage,
                finish_reason: Some(FinishReason::ToolUse),
            },
            LlmEvent::Finish {
                usage,
                finish_reason: Some(FinishReason::ToolUse),
            },
        ];

        assert_eq!(events, expected);
    }

    #[test]
    fn decoder_plain_text_finish_computes_total() {
        let mut stream = OpenAiChatProtocol::new().decoder();
        let mut events = Vec::new();
        events.extend(
            stream
                .on_frame(&frame(r#"{"choices":[{"delta":{"content":"hi"}}]}"#))
                .unwrap(),
        );
        events.extend(
            stream
                .on_frame(&frame(
                    r#"{"choices":[{"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":3,"completion_tokens":1}}"#,
                ))
                .unwrap(),
        );
        events.extend(stream.finish().unwrap());

        let usage = Usage {
            input_tokens: 3,
            cached_input_tokens: 0,
            cache_creation_input_tokens: 0,
            output_tokens: 1,
            reasoning_output_tokens: 0,
            total_tokens: 4, // computed: 3 + 1
        };
        assert_eq!(
            events,
            vec![
                LlmEvent::StepStart,
                LlmEvent::TextStart { id: TEXT_ID.into() },
                LlmEvent::TextDelta {
                    id: TEXT_ID.into(),
                    delta: "hi".into(),
                },
                LlmEvent::TextEnd {
                    id: TEXT_ID.into(),
                    phase: None,
                },
                LlmEvent::StepFinish {
                    usage,
                    finish_reason: Some(FinishReason::Stop),
                },
                LlmEvent::Finish {
                    usage,
                    finish_reason: Some(FinishReason::Stop),
                },
            ]
        );
    }

    #[test]
    fn finish_is_idempotent() {
        let mut stream = OpenAiChatProtocol::new().decoder();
        stream
            .on_frame(&frame(r#"{"choices":[{"delta":{"content":"hi"}}]}"#))
            .unwrap();
        assert!(!stream.finish().unwrap().is_empty());
        assert!(stream.finish().unwrap().is_empty());
    }
}
