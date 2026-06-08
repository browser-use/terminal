//! OpenAI **Responses** wire protocol (`POST /v1/responses`, streamed).
//!
//! This lowers the provider-neutral [`LlmRequest`] into the Responses request
//! body and lifts the Responses SSE event stream back into [`LlmEvent`]s. The
//! event-type to [`LlmEvent`] mapping mirrors the OpenAI Responses streaming
//! contract (`response.output_text.delta`, `response.output_item.*`,
//! `response.function_call_arguments.*`, `response.reasoning_*`,
//! `response.completed`, ...), matching codex's `ModelClient` semantics.
//!
//! # Parity notes
//! * Reasoning *summary* and raw reasoning text deltas are both surfaced as
//!   [`LlmEvent::ReasoningDelta`]; the Responses API streams them under distinct
//!   event names but our schema models a single reasoning channel per item id.
//! * `instructions` carries the top-level system prompt (joined [`SystemPart`]
//!   texts). [`MessageRole::System`]/[`MessageRole::Developer`] messages in
//!   `messages` are additionally lowered to `developer` input messages.
//! * Encrypted/opaque reasoning items from prior turns are not round-tripped;
//!   only textual reasoning (as a `reasoning` summary item) is represented.
//! * `response.failed`/`error` events are surfaced as
//!   [`LlmEvent::ProviderError`] in-stream (not as a hard decode error), matching
//!   the streaming-event contract.

use std::collections::HashMap;

use serde_json::{json, Map, Value};

use crate::route::framing::SseFrame;
use crate::route::protocol::{Protocol, ProtocolStream};
use crate::schema::{
    ContentPart, FinishReason, LlmError, LlmErrorReason, LlmEvent, LlmRequest, Message,
    MessageRole, ReasoningEffort, TextPhase, ToolChoice, ToolDefinition, Usage,
};

use super::utils::{Lifecycle, ToolStream};

/// Protocol implementation for the OpenAI Responses API.
#[derive(Debug, Clone, Default)]
pub struct OpenAiResponsesProtocol;

impl OpenAiResponsesProtocol {
    /// Create a new protocol instance.
    pub fn new() -> Self {
        Self
    }
}

impl Protocol for OpenAiResponsesProtocol {
    fn build_body(&self, request: &LlmRequest) -> Result<Value, LlmError> {
        let mut body = Map::new();
        body.insert("model".to_string(), json!(request.model.as_str()));
        body.insert("stream".to_string(), json!(true));
        body.insert("store".to_string(), json!(false));

        // Top-level system prompt: join all system parts with blank lines.
        if !request.system.is_empty() {
            let instructions = request
                .system
                .iter()
                .map(|part| part.text.as_str())
                .collect::<Vec<_>>()
                .join("\n\n");
            body.insert("instructions".to_string(), json!(instructions));
        }

        let mut input: Vec<Value> = Vec::new();
        for message in &request.messages {
            lower_message(message, &mut input);
        }
        body.insert("input".to_string(), Value::Array(input));

        if !request.tools.is_empty() {
            let tools = lower_tools(&request.tools, request.provider.as_str());
            if !tools.is_empty() {
                body.insert("tools".to_string(), Value::Array(tools));
            }
        }

        if let Some(choice) = &request.tool_choice {
            body.insert("tool_choice".to_string(), lower_tool_choice(choice));
        }

        let gen = &request.generation;
        if let Some(t) = gen.temperature {
            body.insert("temperature".to_string(), json!(t));
        }
        if let Some(p) = gen.top_p {
            body.insert("top_p".to_string(), json!(p));
        }
        if let Some(m) = gen.max_tokens {
            body.insert("max_output_tokens".to_string(), json!(m));
        }
        if let Some(effort) = gen.reasoning_effort {
            if let Some(s) = reasoning_effort_str(effort) {
                body.insert("reasoning".to_string(), json!({ "effort": s }));
            }
        }

        Ok(Value::Object(body))
    }

    fn decoder(&self) -> Box<dyn ProtocolStream> {
        Box::new(ResponsesDecoder::new())
    }
}

/// The Responses role string for a schema [`MessageRole`].
fn responses_role(role: MessageRole) -> &'static str {
    match role {
        // The Responses API uses `developer` for instruction-style content.
        MessageRole::System | MessageRole::Developer => "developer",
        MessageRole::User => "user",
        MessageRole::Assistant => "assistant",
        // Tool messages never produce a `message` item (their parts become
        // `function_call_output`), but provide a sane fallback.
        MessageRole::Tool => "user",
    }
}

/// The content-part `type` for text, which differs by role direction.
fn text_part_type(role: MessageRole) -> &'static str {
    match role {
        MessageRole::Assistant => "output_text",
        _ => "input_text",
    }
}

/// Lower one [`Message`] into zero or more Responses `input` items.
///
/// Plain text/media parts accumulate into a single `message` item; tool calls,
/// tool results and reasoning become their own top-level items, flushing any
/// pending message content first so ordering is preserved.
fn lower_message(message: &Message, out: &mut Vec<Value>) {
    let role = responses_role(message.role);
    let mut content_parts: Vec<Value> = Vec::new();

    for part in &message.content {
        match part {
            ContentPart::Text { text } => {
                content_parts.push(json!({
                    "type": text_part_type(message.role),
                    "text": text,
                }));
            }
            ContentPart::Media {
                mime_type,
                data,
                url,
                detail,
            } => {
                content_parts.push(lower_media(
                    message.role,
                    mime_type,
                    data.as_deref(),
                    url.as_deref(),
                    detail.as_deref(),
                ));
            }
            ContentPart::Reasoning { text, .. } => {
                flush_message(role, &mut content_parts, out);
                out.push(json!({
                    "type": "reasoning",
                    "summary": [{ "type": "summary_text", "text": text }],
                }));
            }
            ContentPart::ToolCall {
                id,
                name,
                input,
                provider_metadata,
            } => {
                flush_message(role, &mut content_parts, out);
                let mut item = json!({
                    "type": "function_call",
                    "call_id": id,
                    "name": name,
                    "arguments": stringify_arguments(input),
                });
                if let Some(namespace) = provider_metadata
                    .as_ref()
                    .and_then(|metadata| metadata.get("namespace"))
                    .and_then(Value::as_str)
                {
                    item["namespace"] = Value::String(namespace.to_string());
                }
                out.push(item);
            }
            ContentPart::ToolResult {
                tool_call_id,
                content,
                ..
            } => {
                flush_message(role, &mut content_parts, out);
                out.push(json!({
                    "type": "function_call_output",
                    "call_id": tool_call_id,
                    "output": lower_tool_result_output(content),
                }));
            }
        }
    }

    flush_message(role, &mut content_parts, out);
}

/// Emit a `message` item for any accumulated content parts, then clear them.
fn flush_message(role: &str, parts: &mut Vec<Value>, out: &mut Vec<Value>) {
    if parts.is_empty() {
        return;
    }
    out.push(json!({
        "type": "message",
        "role": role,
        "content": std::mem::take(parts),
    }));
}

/// Lower an inline media part to a Responses content item.
///
/// Image MIME types become `input_image` (with an `image_url`), everything else
/// becomes `input_file` (with `file_data`). A base64 `data` payload is wrapped
/// in a `data:` URL; an explicit `url` is used verbatim.
fn lower_media(
    role: MessageRole,
    mime_type: &str,
    data: Option<&str>,
    url: Option<&str>,
    detail: Option<&str>,
) -> Value {
    let _ = role;
    let resolved = match (url, data) {
        (Some(u), _) => u.to_string(),
        (None, Some(d)) => format!("data:{mime_type};base64,{d}"),
        (None, None) => String::new(),
    };
    if mime_type.starts_with("image/") {
        let mut image = serde_json::Map::from_iter([
            ("type".to_string(), json!("input_image")),
            ("image_url".to_string(), json!(resolved)),
        ]);
        image.insert("detail".to_string(), json!(detail.unwrap_or("auto")));
        Value::Object(image)
    } else {
        json!({ "type": "input_file", "file_data": resolved })
    }
}

/// Function-call arguments are sent as a JSON string. An object becomes its
/// serialized form; a string is used verbatim (already-encoded args).
fn stringify_arguments(input: &Value) -> String {
    match input {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// Lower a tool result's content parts to a Responses `function_call_output.output`.
///
/// Text-only results remain the legacy string form. Results containing media use
/// the Responses content-array form so `view_image` and screenshot tools stay
/// model-visible instead of collapsing to placeholder text.
fn lower_tool_result_output(content: &[ContentPart]) -> Value {
    let mut text = String::new();
    let mut parts = Vec::new();
    let mut has_media = false;
    append_tool_result_output(content, &mut text, &mut parts, &mut has_media);
    if has_media {
        Value::Array(parts)
    } else {
        Value::String(text)
    }
}

fn append_tool_result_output(
    content: &[ContentPart],
    text: &mut String,
    parts: &mut Vec<Value>,
    has_media: &mut bool,
) {
    for part in content {
        match part {
            ContentPart::Text { text: t } | ContentPart::Reasoning { text: t, .. } => {
                text.push_str(t);
                if !t.is_empty() {
                    parts.push(json!({ "type": "input_text", "text": t }));
                }
            }
            ContentPart::ToolResult { content, .. } => {
                append_tool_result_output(content, text, parts, has_media);
            }
            ContentPart::Media {
                mime_type,
                data,
                url,
                detail,
            } => {
                *has_media = true;
                parts.push(lower_media(
                    MessageRole::User,
                    mime_type,
                    data.as_deref(),
                    url.as_deref(),
                    detail.as_deref(),
                ));
            }
            ContentPart::ToolCall { .. } => {}
        }
    }
}

/// Lower a [`ToolDefinition`] into a Responses tool entry.
///
/// Most tools are plain function tools (`{"type":"function", "name", ...}`).
/// `web_search` is the EXCEPTION for first-party OpenAI Responses: it is a
/// hosted tool encoded as `{"type":"web_search_preview"}`. The ChatGPT-backed
/// Codex endpoint currently rejects that hosted type, so the Codex route omits
/// it and relies on local/browser tools instead.
fn lower_tools(tools: &[ToolDefinition], provider: &str) -> Vec<Value> {
    let mut output = Vec::new();
    let mut namespace_indices = HashMap::<String, usize>::new();

    for tool in tools {
        let Some(lowered) = lower_tool(tool, provider) else {
            continue;
        };
        let Some(namespace) = tool.namespace.as_deref() else {
            output.push(lowered);
            continue;
        };
        if lowered.get("type").and_then(Value::as_str) != Some("function") {
            output.push(lowered);
            continue;
        }
        if let Some(index) = namespace_indices.get(namespace).copied() {
            if let Some(namespace_tools) =
                output[index].get_mut("tools").and_then(Value::as_array_mut)
            {
                namespace_tools.push(lowered);
            }
            continue;
        }
        namespace_indices.insert(namespace.to_string(), output.len());
        output.push(json!({
            "type": "namespace",
            "name": namespace,
            "description": tool
                .namespace_description
                .clone()
                .unwrap_or_else(|| format!("Tools in the {namespace} namespace.")),
            "tools": [lowered],
        }));
    }

    output
}

fn lower_function_tool(tool: &ToolDefinition) -> Value {
    json!({
        "type": "function",
        "name": tool.name,
        "description": tool.description,
        "parameters": tool.input_schema,
        "strict": false,
    })
}

fn lower_tool(tool: &ToolDefinition, provider: &str) -> Option<Value> {
    if tool.name == "web_search" {
        if provider.eq_ignore_ascii_case("codex") {
            return None;
        }
        // Hosted tool: the Responses API names it `web_search_preview` and takes
        // no function envelope.
        return Some(json!({ "type": "web_search_preview" }));
    }
    Some(lower_function_tool(tool))
}

/// Lower a [`ToolChoice`] into the Responses `tool_choice` value.
fn lower_tool_choice(choice: &ToolChoice) -> Value {
    match choice {
        ToolChoice::Auto => json!("auto"),
        ToolChoice::None => json!("none"),
        ToolChoice::Required => json!("required"),
        ToolChoice::Tool { name } => json!({ "type": "function", "name": name }),
    }
}

/// The wire string for a [`ReasoningEffort`], or `None` if the level has no
/// Responses representation (`ReasoningEffort::None`).
fn reasoning_effort_str(effort: ReasoningEffort) -> Option<&'static str> {
    match effort {
        ReasoningEffort::None => None,
        ReasoningEffort::Minimal => Some("minimal"),
        ReasoningEffort::Low => Some("low"),
        ReasoningEffort::Medium => Some("medium"),
        ReasoningEffort::High => Some("high"),
        // The Responses API does not expose an "xhigh" tier; map to its highest.
        ReasoningEffort::Xhigh => Some("high"),
    }
}

// ---------------------------------------------------------------------------
// Decoder
// ---------------------------------------------------------------------------

/// Which streaming block (if any) is currently open in the lifecycle.
#[derive(Debug, Clone)]
enum OpenBlock {
    None,
    Text(String),
    Reasoning(String),
}

/// Streaming decoder for the Responses SSE event stream.
struct ResponsesDecoder {
    lifecycle: Lifecycle,
    tools: ToolStream,
    usage: Usage,
    finish_reason: Option<FinishReason>,
    finished: bool,
    /// The currently-open text/reasoning block, so it can be explicitly closed
    /// when a different block or a tool call interrupts it ([`Lifecycle`] keeps
    /// per-id sets but closing the *active* block requires tracking it here).
    open: OpenBlock,
    /// Maps a streaming item id (`output_item` id) to its tool call id, because
    /// argument deltas key off `item_id` while [`ToolStream`] keys off call id.
    item_to_call: HashMap<String, String>,
    /// Optional assistant message phase keyed by the same text item id used by
    /// `response.output_text.*` events. Codex uses this to preempt mailbox mail
    /// only for commentary text; missing phase defaults to final-answer semantics.
    text_phases: HashMap<String, TextPhase>,
}

impl ResponsesDecoder {
    fn new() -> Self {
        Self {
            lifecycle: Lifecycle::new(),
            tools: ToolStream::new(),
            usage: Usage::default(),
            finish_reason: Some(FinishReason::Stop),
            finished: false,
            open: OpenBlock::None,
            item_to_call: HashMap::new(),
            text_phases: HashMap::new(),
        }
    }

    /// Resolve the tool call id for a streaming `item_id`.
    fn call_id_for(&self, item_id: &str) -> String {
        self.item_to_call
            .get(item_id)
            .cloned()
            .unwrap_or_else(|| item_id.to_string())
    }

    fn record_text_phase_from_message_item(&mut self, item: &Value) {
        if item.get("role").and_then(Value::as_str) != Some("assistant") {
            return;
        }
        let Some(item_id) = item
            .get("id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
        else {
            return;
        };
        if let Some(phase) = parse_text_phase(item.get("phase")) {
            self.text_phases.insert(item_id.to_string(), phase);
        }
    }

    fn record_text_phase_for_output_text_event(&mut self, value: &Value) {
        let Some(item_id) = value
            .get("item_id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
        else {
            return;
        };
        let phase = parse_text_phase(value.get("phase")).or_else(|| {
            value
                .get("item")
                .and_then(|item| parse_text_phase(item.get("phase")))
        });
        if let Some(phase) = phase {
            self.text_phases.insert(item_id.to_string(), phase);
        }
    }

    /// Close whichever text/reasoning block is currently open.
    fn close_open(&mut self, out: &mut Vec<LlmEvent>) {
        match std::mem::replace(&mut self.open, OpenBlock::None) {
            OpenBlock::Text(id) => {
                let phase = self.text_phases.remove(&id);
                out.extend(self.lifecycle.text_end_with_phase(id, phase));
            }
            OpenBlock::Reasoning(id) => out.extend(self.lifecycle.reasoning_end(id)),
            OpenBlock::None => {}
        }
    }

    /// Append a text delta for `id`, closing any other open block first.
    fn push_text(&mut self, id: &str, delta: &str, out: &mut Vec<LlmEvent>) {
        if !matches!(&self.open, OpenBlock::Text(open) if open == id) {
            self.close_open(out);
            self.open = OpenBlock::Text(id.to_string());
        }
        out.extend(self.lifecycle.text_delta(id, delta));
    }

    /// Append a reasoning delta for `id`, closing any other open block first.
    fn push_reasoning(&mut self, id: &str, delta: &str, out: &mut Vec<LlmEvent>) {
        if !matches!(&self.open, OpenBlock::Reasoning(open) if open == id) {
            self.close_open(out);
            self.open = OpenBlock::Reasoning(id.to_string());
        }
        out.extend(self.lifecycle.reasoning_delta(id, delta));
    }
}

fn parse_text_phase(value: Option<&Value>) -> Option<TextPhase> {
    let phase = value?.as_str()?.to_ascii_lowercase();
    match phase.as_str() {
        "commentary" => Some(TextPhase::Commentary),
        "final" | "final_answer" | "final-answer" => Some(TextPhase::FinalAnswer),
        _ => None,
    }
}

impl ProtocolStream for ResponsesDecoder {
    fn on_frame(&mut self, frame: &SseFrame) -> Result<Vec<LlmEvent>, LlmError> {
        let data = frame.data.trim();
        if data.is_empty() || data == "[DONE]" {
            return Ok(Vec::new());
        }

        let value: Value = serde_json::from_str(data).map_err(|e| {
            LlmError::new(
                LlmErrorReason::Decode,
                format!("invalid Responses SSE JSON: {e}"),
            )
        })?;

        // The `event:` line and the JSON `type` field carry the same name; the
        // JSON field is authoritative because some transports omit `event:`.
        let kind = value
            .get("type")
            .and_then(Value::as_str)
            .or(frame.event.as_deref())
            .unwrap_or("");

        let mut out = Vec::new();
        match kind {
            "response.created" | "response.in_progress" => {}

            "response.output_item.added" => {
                if let Some(item) = value.get("item") {
                    if item.get("type").and_then(Value::as_str) == Some("function_call") {
                        let item_id = item
                            .get("id")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string();
                        let call_id = item
                            .get("call_id")
                            .and_then(Value::as_str)
                            .unwrap_or(&item_id)
                            .to_string();
                        let name = item
                            .get("name")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string();
                        let namespace = item
                            .get("namespace")
                            .and_then(Value::as_str)
                            .map(str::to_string);
                        if !item_id.is_empty() {
                            self.item_to_call.insert(item_id, call_id.clone());
                        }
                        // A tool call interrupts any open text/reasoning block.
                        self.close_open(&mut out);
                        out.extend(self.tools.start_with_namespace(&call_id, name, namespace));
                        self.finish_reason = Some(FinishReason::ToolUse);
                    } else if item.get("type").and_then(Value::as_str) == Some("message") {
                        self.record_text_phase_from_message_item(item);
                    }
                }
            }

            "response.output_text.delta" => {
                if let Some(delta) = value.get("delta").and_then(Value::as_str) {
                    let id = value
                        .get("item_id")
                        .and_then(Value::as_str)
                        .unwrap_or("text")
                        .to_string();
                    self.push_text(&id, delta, &mut out);
                }
            }
            "response.output_text.done" => {
                self.record_text_phase_for_output_text_event(&value);
            }

            "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
                if let Some(delta) = value.get("delta").and_then(Value::as_str) {
                    let id = value
                        .get("item_id")
                        .and_then(Value::as_str)
                        .unwrap_or("reasoning")
                        .to_string();
                    self.push_reasoning(&id, delta, &mut out);
                }
            }
            "response.reasoning_summary_text.done" | "response.reasoning_text.done" => {}

            "response.function_call_arguments.delta" => {
                let item_id = value.get("item_id").and_then(Value::as_str).unwrap_or("");
                if let Some(delta) = value.get("delta").and_then(Value::as_str) {
                    let call_id = self.call_id_for(item_id);
                    out.extend(self.tools.delta(&call_id, None, delta));
                }
            }
            "response.function_call_arguments.done" => {
                let item_id = value.get("item_id").and_then(Value::as_str).unwrap_or("");
                let call_id = self.call_id_for(item_id);
                let arguments = value.get("arguments").and_then(Value::as_str);
                out.extend(self.tools.end_with_arguments(&call_id, arguments)?);
            }

            "response.output_item.done" => {
                // Function-call items are finalised by
                // `response.function_call_arguments.done`; nothing extra here.
                if let Some(item) = value.get("item") {
                    self.record_text_phase_from_message_item(item);
                }
            }

            "response.completed" => {
                if let Some(resp) = value.get("response") {
                    if let Some(u) = parse_usage(resp.get("usage")) {
                        self.usage = u;
                    }
                }
                self.close_open(&mut out);
                // Flush any tool whose `.done` never arrived, then finish.
                out.extend(self.tools.flush()?);
                out.extend(self.lifecycle.finish(self.usage, self.finish_reason));
                self.finished = true;
            }

            "response.failed" | "error" => {
                let message = value
                    .get("response")
                    .and_then(|r| r.get("error"))
                    .and_then(|e| e.get("message"))
                    .and_then(Value::as_str)
                    .or_else(|| value.get("message").and_then(Value::as_str))
                    .unwrap_or("provider error")
                    .to_string();
                out.push(LlmEvent::ProviderError {
                    message,
                    retryable: false,
                });
            }

            // Unknown / unhandled event types are ignored for forward-compat.
            _ => {}
        }

        Ok(out)
    }

    fn finish(&mut self) -> Result<Vec<LlmEvent>, LlmError> {
        let mut out = Vec::new();
        if self.finished {
            return Ok(out);
        }
        self.close_open(&mut out);
        out.extend(self.tools.flush()?);
        out.extend(self.lifecycle.finish(self.usage, self.finish_reason));
        self.finished = true;
        Ok(out)
    }
}

/// Parse a Responses `usage` object into [`Usage`].
fn parse_usage(usage: Option<&Value>) -> Option<Usage> {
    let usage = usage?;
    let input = usage
        .get("input_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let output = usage
        .get("output_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let total = usage
        .get("total_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(input + output);
    let cached = usage
        .get("input_tokens_details")
        .and_then(|d| d.get("cached_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let reasoning = usage
        .get("output_tokens_details")
        .and_then(|d| d.get("reasoning_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    Some(Usage {
        input_tokens: input,
        cached_input_tokens: cached,
        cache_creation_input_tokens: 0,
        output_tokens: output,
        reasoning_output_tokens: reasoning,
        total_tokens: total,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::SystemPart;

    fn frame(event: &str, data: &str) -> SseFrame {
        SseFrame {
            event: Some(event.to_string()),
            data: data.to_string(),
        }
    }

    #[test]
    fn build_body_golden() {
        let mut request = LlmRequest::new("gpt-5.1-codex", "openai");
        request
            .system
            .push(SystemPart::new("You are a helpful assistant."));
        request
            .messages
            .push(Message::user_text("What is the weather in NYC?"));
        request.tools.push(ToolDefinition {
            name: "get_weather".to_string(),
            description: "Get the weather for a city".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": { "city": { "type": "string" } },
                "required": ["city"]
            }),
            output_schema: None,
            namespace: None,
            namespace_description: None,
        });
        request.tool_choice = Some(ToolChoice::Auto);
        request.generation.temperature = Some(0.2);
        request.generation.reasoning_effort = Some(ReasoningEffort::High);

        let body = OpenAiResponsesProtocol::new()
            .build_body(&request)
            .expect("build_body");

        assert_eq!(body["model"], json!("gpt-5.1-codex"));
        assert_eq!(body["stream"], json!(true));
        assert_eq!(body["instructions"], json!("You are a helpful assistant."));
        // temperature is an f32; compare against the f32-derived JSON number so
        // the assertion does not trip on f32->f64 widening.
        assert_eq!(body["temperature"], json!(0.2_f32));
        assert_eq!(body["reasoning"], json!({ "effort": "high" }));
        assert_eq!(body["tool_choice"], json!("auto"));

        // Input: one user message with an input_text part.
        let input = body["input"].as_array().expect("input array");
        assert_eq!(input.len(), 1);
        assert_eq!(input[0]["type"], json!("message"));
        assert_eq!(input[0]["role"], json!("user"));
        assert_eq!(input[0]["content"][0]["type"], json!("input_text"));
        assert_eq!(
            input[0]["content"][0]["text"],
            json!("What is the weather in NYC?")
        );

        // Tools.
        let tools = body["tools"].as_array().expect("tools array");
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["type"], json!("function"));
        assert_eq!(tools[0]["name"], json!("get_weather"));
        assert_eq!(
            tools[0]["parameters"]["properties"]["city"]["type"],
            json!("string")
        );
    }

    #[test]
    fn build_body_lowers_tool_call_and_result_items() {
        let mut request = LlmRequest::new("gpt-5.1-codex", "openai");
        request.messages.push(Message::new(
            MessageRole::Assistant,
            vec![ContentPart::ToolCall {
                id: "call_1".into(),
                name: "get_weather".into(),
                input: json!({ "city": "NYC" }),
                provider_metadata: None,
            }],
        ));
        request.messages.push(Message::new(
            MessageRole::Tool,
            vec![ContentPart::ToolResult {
                tool_call_id: "call_1".into(),
                content: vec![ContentPart::text("sunny")],
                is_error: false,
            }],
        ));
        request.tool_choice = Some(ToolChoice::Tool {
            name: "get_weather".into(),
        });

        let body = OpenAiResponsesProtocol::new().build_body(&request).unwrap();
        let input = body["input"].as_array().unwrap();
        assert_eq!(input[0]["type"], json!("function_call"));
        assert_eq!(input[0]["call_id"], json!("call_1"));
        assert_eq!(input[0]["arguments"], json!("{\"city\":\"NYC\"}"));
        assert_eq!(input[1]["type"], json!("function_call_output"));
        assert_eq!(input[1]["call_id"], json!("call_1"));
        assert_eq!(input[1]["output"], json!("sunny"));
        assert_eq!(
            body["tool_choice"],
            json!({ "type": "function", "name": "get_weather" })
        );
    }

    #[test]
    fn build_body_lowers_tool_result_media_as_function_output_content() {
        let mut request = LlmRequest::new("gpt-5.1-codex", "openai");
        request.messages.push(Message::new(
            MessageRole::Assistant,
            vec![ContentPart::ToolCall {
                id: "call_view".into(),
                name: "view_image".into(),
                input: json!({ "path": "shot.png" }),
                provider_metadata: None,
            }],
        ));
        request.messages.push(Message::new(
            MessageRole::Tool,
            vec![ContentPart::ToolResult {
                tool_call_id: "call_view".into(),
                content: vec![
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
                is_error: false,
            }],
        ));

        let body = OpenAiResponsesProtocol::new().build_body(&request).unwrap();
        let input = body["input"].as_array().unwrap();
        assert_eq!(input[1]["type"], json!("function_call_output"));
        assert_eq!(input[1]["call_id"], json!("call_view"));
        assert_eq!(input[1]["output"][0]["type"], json!("input_image"));
        assert_eq!(
            input[1]["output"][0]["image_url"],
            json!("data:image/png;base64,AAAA")
        );
        assert_eq!(input[1]["output"][0]["detail"], json!("original"));
        assert_eq!(
            input[1]["output"][1]["image_url"],
            json!("data:image/jpeg;base64,BBBB")
        );
        assert_eq!(input[1]["output"][1]["detail"], json!("auto"));
    }

    #[test]
    fn decoder_text_tool_call_and_usage() {
        let proto = OpenAiResponsesProtocol::new();
        let mut dec = proto.decoder();

        let frames = vec![
            frame(
                "response.created",
                r#"{"type":"response.created","response":{"id":"resp_1"}}"#,
            ),
            frame(
                "response.output_text.delta",
                r#"{"type":"response.output_text.delta","item_id":"msg_1","delta":"Let me "}"#,
            ),
            frame(
                "response.output_text.delta",
                r#"{"type":"response.output_text.delta","item_id":"msg_1","delta":"check."}"#,
            ),
            frame(
                "response.output_text.done",
                r#"{"type":"response.output_text.done","item_id":"msg_1","text":"Let me check."}"#,
            ),
            frame(
                "response.output_item.added",
                r#"{"type":"response.output_item.added","item":{"type":"function_call","id":"item_1","call_id":"call_1","name":"get_weather"}}"#,
            ),
            frame(
                "response.function_call_arguments.delta",
                r#"{"type":"response.function_call_arguments.delta","item_id":"item_1","delta":"{\"city\":"}"#,
            ),
            frame(
                "response.function_call_arguments.delta",
                r#"{"type":"response.function_call_arguments.delta","item_id":"item_1","delta":"\"NYC\"}"}"#,
            ),
            frame(
                "response.function_call_arguments.done",
                r#"{"type":"response.function_call_arguments.done","item_id":"item_1","arguments":"{\"city\":\"NYC\"}"}"#,
            ),
            frame(
                "response.completed",
                r#"{"type":"response.completed","response":{"usage":{"input_tokens":11,"output_tokens":7,"total_tokens":18,"input_tokens_details":{"cached_tokens":4},"output_tokens_details":{"reasoning_tokens":2}}}}"#,
            ),
            frame("", "[DONE]"),
        ];

        let mut events = Vec::new();
        for f in &frames {
            events.extend(dec.on_frame(f).expect("on_frame"));
        }
        events.extend(dec.finish().expect("finish"));

        let expected = vec![
            LlmEvent::StepStart,
            LlmEvent::TextStart { id: "msg_1".into() },
            LlmEvent::TextDelta {
                id: "msg_1".into(),
                delta: "Let me ".into(),
            },
            LlmEvent::TextDelta {
                id: "msg_1".into(),
                delta: "check.".into(),
            },
            // output_item.added (function_call) closes the open text block.
            LlmEvent::TextEnd {
                id: "msg_1".into(),
                phase: None,
            },
            LlmEvent::ToolInputStart {
                id: "call_1".into(),
                name: "get_weather".into(),
            },
            LlmEvent::ToolInputDelta {
                id: "call_1".into(),
                delta: "{\"city\":".into(),
            },
            LlmEvent::ToolInputDelta {
                id: "call_1".into(),
                delta: "\"NYC\"}".into(),
            },
            LlmEvent::ToolInputEnd {
                id: "call_1".into(),
            },
            LlmEvent::ToolCall {
                id: "call_1".into(),
                name: "get_weather".into(),
                namespace: None,
                provider_metadata: None,
                input: json!({ "city": "NYC" }),
            },
            LlmEvent::StepFinish {
                usage: Usage {
                    input_tokens: 11,
                    cached_input_tokens: 4,
                    cache_creation_input_tokens: 0,
                    output_tokens: 7,
                    reasoning_output_tokens: 2,
                    total_tokens: 18,
                },
                finish_reason: Some(FinishReason::ToolUse),
            },
            LlmEvent::Finish {
                usage: Usage {
                    input_tokens: 11,
                    cached_input_tokens: 4,
                    cache_creation_input_tokens: 0,
                    output_tokens: 7,
                    reasoning_output_tokens: 2,
                    total_tokens: 18,
                },
                finish_reason: Some(FinishReason::ToolUse),
            },
        ];

        assert_eq!(events, expected);
    }

    #[test]
    fn decoder_preserves_commentary_text_phase() {
        let proto = OpenAiResponsesProtocol::new();
        let mut dec = proto.decoder();
        let frames = vec![
            frame(
                "response.output_item.added",
                r#"{"type":"response.output_item.added","item":{"type":"message","id":"msg_1","role":"assistant","phase":"commentary"}}"#,
            ),
            frame(
                "response.output_text.delta",
                r#"{"type":"response.output_text.delta","item_id":"msg_1","delta":"still working"}"#,
            ),
            frame(
                "response.output_item.added",
                r#"{"type":"response.output_item.added","item":{"type":"function_call","id":"item_1","call_id":"call_1","name":"get_weather"}}"#,
            ),
        ];

        let mut events = Vec::new();
        for f in &frames {
            events.extend(dec.on_frame(f).expect("on_frame"));
        }

        assert_eq!(
            events,
            vec![
                LlmEvent::StepStart,
                LlmEvent::TextStart { id: "msg_1".into() },
                LlmEvent::TextDelta {
                    id: "msg_1".into(),
                    delta: "still working".into(),
                },
                LlmEvent::TextEnd {
                    id: "msg_1".into(),
                    phase: Some(TextPhase::Commentary),
                },
                LlmEvent::ToolInputStart {
                    id: "call_1".into(),
                    name: "get_weather".into(),
                },
            ]
        );
    }

    #[test]
    fn decoder_reasoning_then_text() {
        let proto = OpenAiResponsesProtocol::new();
        let mut dec = proto.decoder();
        let frames = vec![
            frame(
                "response.reasoning_summary_text.delta",
                r#"{"type":"response.reasoning_summary_text.delta","item_id":"r1","delta":"thinking"}"#,
            ),
            frame(
                "response.output_text.delta",
                r#"{"type":"response.output_text.delta","item_id":"t1","delta":"hi"}"#,
            ),
            frame(
                "response.completed",
                r#"{"type":"response.completed","response":{"usage":{"input_tokens":1,"output_tokens":1,"total_tokens":2}}}"#,
            ),
        ];
        let mut events = Vec::new();
        for f in &frames {
            events.extend(dec.on_frame(f).unwrap());
        }
        assert_eq!(
            events,
            vec![
                LlmEvent::StepStart,
                LlmEvent::ReasoningStart { id: "r1".into() },
                LlmEvent::ReasoningDelta {
                    id: "r1".into(),
                    delta: "thinking".into()
                },
                LlmEvent::ReasoningEnd { id: "r1".into() },
                LlmEvent::TextStart { id: "t1".into() },
                LlmEvent::TextDelta {
                    id: "t1".into(),
                    delta: "hi".into()
                },
                LlmEvent::TextEnd {
                    id: "t1".into(),
                    phase: None,
                },
                LlmEvent::StepFinish {
                    usage: Usage {
                        input_tokens: 1,
                        cached_input_tokens: 0,
                        cache_creation_input_tokens: 0,
                        output_tokens: 1,
                        reasoning_output_tokens: 0,
                        total_tokens: 2,
                    },
                    finish_reason: Some(FinishReason::Stop),
                },
                LlmEvent::Finish {
                    usage: Usage {
                        input_tokens: 1,
                        cached_input_tokens: 0,
                        cache_creation_input_tokens: 0,
                        output_tokens: 1,
                        reasoning_output_tokens: 0,
                        total_tokens: 2,
                    },
                    finish_reason: Some(FinishReason::Stop),
                },
            ]
        );
    }

    #[test]
    fn decoder_finish_flushes_unterminated_tool_call() {
        let proto = OpenAiResponsesProtocol::new();
        let mut dec = proto.decoder();
        // Tool call begins but stream ends without `.done` or `response.completed`.
        let added = frame(
            "response.output_item.added",
            r#"{"type":"response.output_item.added","item":{"type":"function_call","id":"item_1","call_id":"call_9","name":"do_it"}}"#,
        );
        let delta = frame(
            "response.function_call_arguments.delta",
            r#"{"type":"response.function_call_arguments.delta","item_id":"item_1","delta":"{}"}"#,
        );
        let mut events = Vec::new();
        events.extend(dec.on_frame(&added).unwrap());
        events.extend(dec.on_frame(&delta).unwrap());
        events.extend(dec.finish().unwrap());

        assert!(events.contains(&LlmEvent::ToolCall {
            id: "call_9".into(),
            name: "do_it".into(),
            namespace: None,
            provider_metadata: None,
            input: json!({}),
        }));
        assert!(matches!(events.last(), Some(LlmEvent::Finish { .. })));
    }

    #[test]
    fn decoder_preserves_responses_function_namespace() {
        let proto = OpenAiResponsesProtocol::new();
        let mut dec = proto.decoder();
        let added = frame(
            "response.output_item.added",
            r#"{"type":"response.output_item.added","item":{"type":"function_call","id":"item_1","call_id":"call_9","name":"spawn_agent","namespace":"agents"}}"#,
        );
        let done = frame(
            "response.function_call_arguments.done",
            r#"{"type":"response.function_call_arguments.done","item_id":"item_1","arguments":"{\"task_name\":\"audit\",\"message\":\"check\"}"}"#,
        );
        let mut events = Vec::new();
        events.extend(dec.on_frame(&added).unwrap());
        events.extend(dec.on_frame(&done).unwrap());

        assert!(events.contains(&LlmEvent::ToolCall {
            id: "call_9".into(),
            name: "spawn_agent".into(),
            namespace: Some("agents".into()),
            provider_metadata: None,
            input: json!({ "task_name": "audit", "message": "check" }),
        }));
    }

    #[test]
    fn web_search_encodes_as_hosted_tool_and_shell_stays_flat_function() {
        let mut request = LlmRequest::new("gpt-5.1-codex", "openai");
        // A normal function tool (shell) and the hosted web_search tool.
        request.tools.push(ToolDefinition {
            name: "shell".to_string(),
            description: "Run a shell command".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": { "command": { "type": "array" } },
                "required": ["command"]
            }),
            output_schema: None,
            namespace: None,
            namespace_description: None,
        });
        request.tools.push(ToolDefinition {
            name: "web_search".to_string(),
            description: "Search the web".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": { "query": { "type": "string" } },
                "required": ["query"]
            }),
            output_schema: None,
            namespace: None,
            namespace_description: None,
        });

        let body = OpenAiResponsesProtocol::new()
            .build_body(&request)
            .expect("build_body");
        let tools = body["tools"].as_array().expect("tools array");
        assert_eq!(tools.len(), 2);

        // shell: a flat function tool (type=function, with name/parameters).
        let shell = tools
            .iter()
            .find(|t| t["name"] == json!("shell"))
            .expect("shell tool present");
        assert_eq!(shell["type"], json!("function"));
        assert_eq!(shell["name"], json!("shell"));
        assert_eq!(
            shell["parameters"]["properties"]["command"]["type"],
            json!("array")
        );

        // web_search: the HOSTED shape `{"type":"web_search_preview"}` — no
        // function wrapper, no `name`, no `parameters`.
        let web = tools
            .iter()
            .find(|t| t["type"] == json!("web_search_preview"))
            .expect("web_search must encode as web_search_preview");
        assert_eq!(web, &json!({ "type": "web_search_preview" }));
        assert!(
            web.get("name").is_none(),
            "hosted web_search must not carry a function `name`"
        );
        assert!(
            web.get("parameters").is_none(),
            "hosted web_search must not carry `parameters`"
        );
        assert!(
            tools.iter().all(|t| t["name"] != json!("web_search")),
            "web_search must NOT appear as a function tool named web_search"
        );
    }

    #[test]
    fn namespaced_tools_coalesce_like_codex() {
        let mut request = LlmRequest::new("gpt-5.1-codex", "openai");
        request.tools.push(ToolDefinition {
            name: "spawn_agent".to_string(),
            description: "Spawn".to_string(),
            input_schema: json!({ "type": "object" }),
            output_schema: Some(json!({
                "type": "object",
                "properties": { "task_name": { "type": "string" } },
                "required": ["task_name"],
                "additionalProperties": false
            })),
            namespace: Some("agents".to_string()),
            namespace_description: Some("Agent tools.".to_string()),
        });
        request.tools.push(ToolDefinition {
            name: "wait_agent".to_string(),
            description: "Wait".to_string(),
            input_schema: json!({ "type": "object" }),
            output_schema: None,
            namespace: Some("agents".to_string()),
            namespace_description: Some("Ignored after first namespace.".to_string()),
        });

        let body = OpenAiResponsesProtocol::new()
            .build_body(&request)
            .expect("build_body");
        let tools = body["tools"].as_array().expect("tools array");
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["type"], json!("namespace"));
        assert_eq!(tools[0]["name"], json!("agents"));
        assert_eq!(tools[0]["description"], json!("Agent tools."));
        let namespace_tools = tools[0]["tools"].as_array().expect("namespace tools");
        assert_eq!(namespace_tools.len(), 2);
        assert_eq!(namespace_tools[0]["name"], json!("spawn_agent"));
        assert_eq!(namespace_tools[0]["strict"], json!(false));
        assert!(namespace_tools[0].get("output_schema").is_none());
        assert_eq!(namespace_tools[1]["name"], json!("wait_agent"));
    }

    #[test]
    fn assistant_tool_call_history_preserves_namespace_metadata() {
        let mut request = LlmRequest::new("gpt-5.1-codex", "openai");
        request.messages.push(Message::new(
            MessageRole::Assistant,
            vec![ContentPart::ToolCall {
                id: "call_1".into(),
                name: "spawn_agent".into(),
                input: json!({ "task_name": "audit" }),
                provider_metadata: Some(json!({ "namespace": "agents" })),
            }],
        ));

        let body = OpenAiResponsesProtocol::new()
            .build_body(&request)
            .expect("build_body");
        let input = body["input"].as_array().expect("input array");
        assert_eq!(input[0]["type"], json!("function_call"));
        assert_eq!(input[0]["namespace"], json!("agents"));
    }

    #[test]
    fn codex_route_omits_hosted_web_search_preview() {
        let mut request = LlmRequest::new("gpt-5.5", "codex");
        request.tools.push(ToolDefinition {
            name: "shell".to_string(),
            description: "Run a shell command".to_string(),
            input_schema: json!({ "type": "object" }),
            output_schema: None,
            namespace: None,
            namespace_description: None,
        });
        request.tools.push(ToolDefinition {
            name: "web_search".to_string(),
            description: "Search the web".to_string(),
            input_schema: json!({ "type": "object" }),
            output_schema: None,
            namespace: None,
            namespace_description: None,
        });

        let body = OpenAiResponsesProtocol::new()
            .build_body(&request)
            .expect("build_body");
        let tools = body["tools"].as_array().expect("tools array");
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["name"], json!("shell"));
        assert!(tools
            .iter()
            .all(|tool| tool["type"] != "web_search_preview"));
    }

    #[test]
    fn decoder_surfaces_provider_error() {
        let proto = OpenAiResponsesProtocol::new();
        let mut dec = proto.decoder();
        let f = frame(
            "response.failed",
            r#"{"type":"response.failed","response":{"error":{"message":"boom"}}}"#,
        );
        let events = dec.on_frame(&f).unwrap();
        assert_eq!(
            events,
            vec![LlmEvent::ProviderError {
                message: "boom".into(),
                retryable: false,
            }]
        );
    }
}
