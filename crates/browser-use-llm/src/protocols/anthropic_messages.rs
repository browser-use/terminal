//! Anthropic Messages wire protocol (rearchitecture WP 1.5).
//!
//! Lowers the canonical [`LlmRequest`] to the Anthropic `/v1/messages` request
//! body and decodes the provider's Server-Sent-Events stream into normalized
//! [`LlmEvent`]s, reusing the shared [`Lifecycle`] and [`ToolStream`] helpers.
//!
//! Reference: <https://docs.anthropic.com/en/api/messages>.

use std::io::Cursor;

use base64::{engine::general_purpose, Engine as _};
use image::imageops::FilterType;
use serde_json::{json, Map, Value};

const ANTHROPIC_MAX_MANY_IMAGE_DIMENSION: u32 = 2_000;
const ANTHROPIC_MAX_INLINE_IMAGE_BASE64_CHARS: usize = 8 * 1024 * 1024;

use crate::protocols::utils::{Lifecycle, ToolStream};
use crate::route::framing::SseFrame;
use crate::route::protocol::{Protocol, ProtocolStream};
use crate::schema::{
    CacheHint, ContentPart, FinishReason, LlmError, LlmErrorReason, LlmEvent, LlmRequest, Message,
    MessageRole, SystemPart, ToolChoice, ToolDefinition, Usage,
};

/// Default `max_tokens` when the request does not specify one.
const DEFAULT_MAX_TOKENS: u32 = 16_000;

/// The Anthropic Messages protocol.
///
/// Stateless: the target model and all generation knobs come from the
/// [`LlmRequest`]. A fresh [`AnthropicMessagesStream`] decoder is created per
/// streamed response.
#[derive(Debug, Default, Clone, Copy)]
pub struct AnthropicMessagesProtocol;

impl AnthropicMessagesProtocol {
    pub fn new() -> Self {
        Self
    }
}

impl Protocol for AnthropicMessagesProtocol {
    fn build_body(&self, req: &LlmRequest) -> Result<Value, LlmError> {
        let mut body = Map::new();

        body.insert("model".to_string(), Value::String(req.model.to_string()));
        body.insert(
            "max_tokens".to_string(),
            json!(req.generation.max_tokens.unwrap_or(DEFAULT_MAX_TOKENS)),
        );
        body.insert("stream".to_string(), Value::Bool(true));

        if let Some(temperature) = req.generation.temperature {
            body.insert("temperature".to_string(), json!(temperature));
        }
        if let Some(top_p) = req.generation.top_p {
            body.insert("top_p".to_string(), json!(top_p));
        }
        if !req.generation.stop.is_empty() {
            body.insert("stop_sequences".to_string(), json!(req.generation.stop));
        }

        // `system` is a top-level array of text blocks, not a message.
        if !req.system.is_empty() {
            let system: Vec<Value> = req.system.iter().map(build_system_block).collect();
            body.insert("system".to_string(), Value::Array(system));
        }

        // Conversation messages.
        let messages: Result<Vec<Value>, LlmError> =
            req.messages.iter().map(build_message).collect();
        body.insert("messages".to_string(), Value::Array(messages?));

        // Tool definitions.
        if !req.tools.is_empty() {
            let mut tools: Vec<Value> = req.tools.iter().map(build_tool).collect();
            if let Some(Value::Object(last_tool)) = tools.last_mut() {
                last_tool.insert(
                    "cache_control".to_string(),
                    cache_control(CacheHint::Ephemeral),
                );
            }
            body.insert("tools".to_string(), Value::Array(tools));
        }

        // Tool choice.
        if let Some(choice) = &req.tool_choice {
            body.insert("tool_choice".to_string(), build_tool_choice(choice));
        }

        Ok(Value::Object(body))
    }

    fn decoder(&self) -> Box<dyn ProtocolStream> {
        Box::new(AnthropicMessagesStream::new())
    }
}

/// Build a top-level `system` text block, honoring an optional cache hint.
fn build_system_block(part: &SystemPart) -> Value {
    let mut block = Map::new();
    block.insert("type".to_string(), Value::String("text".to_string()));
    block.insert("text".to_string(), Value::String(part.text.clone()));
    if let Some(cache) = part.cache {
        block.insert("cache_control".to_string(), cache_control(cache));
    }
    Value::Object(block)
}

fn cache_control(hint: CacheHint) -> Value {
    match hint {
        CacheHint::Ephemeral => json!({ "type": "ephemeral" }),
    }
}

/// Lower a canonical [`Message`] to an Anthropic message object.
///
/// Anthropic only has `user` / `assistant` roles; tool *results* are `user`
/// turns carrying `tool_result` blocks, and the canonical `Tool` role maps to
/// `user`. `System` / `Developer` roles are not valid here (system text is a
/// top-level field) and are surfaced as user turns defensively.
fn build_message(message: &Message) -> Result<Value, LlmError> {
    let role = match message.role {
        MessageRole::Assistant => "assistant",
        MessageRole::User | MessageRole::Tool | MessageRole::System | MessageRole::Developer => {
            "user"
        }
    };

    let mut content: Vec<Value> = message
        .content
        .iter()
        .map(build_content_block)
        .collect::<Result<Vec<Value>, LlmError>>()?;
    apply_cache_control_to_last_content_block(&mut content, message.cache);

    Ok(json!({
        "role": role,
        "content": content,
    }))
}

fn apply_cache_control_to_last_content_block(content: &mut [Value], cache: Option<CacheHint>) {
    let Some(cache) = cache else {
        return;
    };

    for block in content.iter_mut().rev() {
        let Some(obj) = block.as_object_mut() else {
            continue;
        };
        if obj.get("type").and_then(Value::as_str) == Some("image") {
            continue;
        }
        obj.insert("cache_control".to_string(), cache_control(cache));
        break;
    }
}

/// Translate a canonical [`ContentPart`] into an Anthropic content block.
fn build_content_block(part: &ContentPart) -> Result<Value, LlmError> {
    match part {
        ContentPart::Text { text } => Ok(json!({ "type": "text", "text": text })),
        ContentPart::Media {
            mime_type,
            data,
            url,
            ..
        } => {
            let source = if let Some(data) = data {
                let (media_type, data) = anthropic_safe_inline_image(mime_type, data)?
                    .unwrap_or_else(|| (mime_type.clone(), data.clone()));
                json!({ "type": "base64", "media_type": media_type, "data": data })
            } else if let Some(url) = url {
                json!({ "type": "url", "url": url })
            } else {
                return Err(LlmError::new(
                    LlmErrorReason::InvalidRequest,
                    "media content part has neither data nor url",
                ));
            };
            Ok(json!({ "type": "image", "source": source }))
        }
        ContentPart::ToolCall {
            id, name, input, ..
        } => Ok(json!({
            "type": "tool_use",
            "id": id,
            "name": name,
            "input": input,
        })),
        ContentPart::ToolResult {
            tool_call_id,
            content,
            is_error,
        } => {
            let blocks = if *is_error {
                vec![json!({
                    "type": "text",
                    "text": flatten_error_tool_result_content(content),
                })]
            } else {
                content
                    .iter()
                    .map(build_content_block)
                    .collect::<Result<Vec<Value>, LlmError>>()?
            };
            let mut block = Map::new();
            block.insert("type".to_string(), Value::String("tool_result".to_string()));
            block.insert(
                "tool_use_id".to_string(),
                Value::String(tool_call_id.clone()),
            );
            block.insert("content".to_string(), Value::Array(blocks));
            if *is_error {
                block.insert("is_error".to_string(), Value::Bool(true));
            }
            Ok(Value::Object(block))
        }
        ContentPart::Reasoning {
            text, signature, ..
        } => {
            // Recover the thinking-block signature from the dedicated field,
            // falling back to `provider_metadata` for round-tripping.
            let signature = signature
                .clone()
                .or_else(|| reasoning_signature_from_metadata(part));
            let mut block = Map::new();
            block.insert("type".to_string(), Value::String("thinking".to_string()));
            block.insert("thinking".to_string(), Value::String(text.clone()));
            if let Some(signature) = signature {
                block.insert("signature".to_string(), Value::String(signature));
            }
            Ok(Value::Object(block))
        }
    }
}

fn anthropic_safe_inline_image(
    mime_type: &str,
    data: &str,
) -> Result<Option<(String, String)>, LlmError> {
    if !mime_type.starts_with("image/") {
        return Ok(None);
    }
    if data.len() > ANTHROPIC_MAX_INLINE_IMAGE_BASE64_CHARS {
        return Err(LlmError::new(
            LlmErrorReason::InvalidRequest,
            format!(
                "inline image is too large to normalize safely: {} base64 chars exceeds {}",
                data.len(),
                ANTHROPIC_MAX_INLINE_IMAGE_BASE64_CHARS
            ),
        ));
    }
    let bytes = match general_purpose::STANDARD.decode(data.as_bytes()) {
        Ok(bytes) => bytes,
        Err(_) => return Ok(None),
    };
    let image = match image::load_from_memory(&bytes) {
        Ok(image) => image,
        Err(_) => return Ok(None),
    };
    let width = image.width();
    let height = image.height();
    if width <= ANTHROPIC_MAX_MANY_IMAGE_DIMENSION && height <= ANTHROPIC_MAX_MANY_IMAGE_DIMENSION {
        return Ok(None);
    }

    let scale = (ANTHROPIC_MAX_MANY_IMAGE_DIMENSION as f32 / width as f32)
        .min(ANTHROPIC_MAX_MANY_IMAGE_DIMENSION as f32 / height as f32);
    let resized_width = ((width as f32 * scale).round() as u32).max(1);
    let resized_height = ((height as f32 * scale).round() as u32).max(1);
    let resized = image.resize(resized_width, resized_height, FilterType::Lanczos3);
    let mut encoded = Cursor::new(Vec::new());
    resized
        .write_to(&mut encoded, image::ImageFormat::Png)
        .map_err(|err| LlmError::new(LlmErrorReason::InvalidRequest, err.to_string()))?;
    Ok(Some((
        "image/png".to_string(),
        general_purpose::STANDARD.encode(encoded.into_inner()),
    )))
}

fn flatten_error_tool_result_content(content: &[ContentPart]) -> String {
    let mut chunks = Vec::new();
    collect_error_tool_result_text(content, &mut chunks);
    if chunks.is_empty() {
        return "Tool call failed.".to_string();
    }
    chunks.join("\n")
}

fn collect_error_tool_result_text(content: &[ContentPart], chunks: &mut Vec<String>) {
    for part in content {
        match part {
            ContentPart::Text { text } | ContentPart::Reasoning { text, .. } => {
                if !text.trim().is_empty() {
                    chunks.push(text.clone());
                }
            }
            ContentPart::Media {
                mime_type,
                data,
                url,
                ..
            } => {
                let pointer = url
                    .as_deref()
                    .map(|url| format!(" at {url}"))
                    .unwrap_or_else(|| {
                        data.as_ref()
                            .map(|_| " inline".to_string())
                            .unwrap_or_default()
                    });
                chunks.push(format!("[{mime_type} media{pointer}]"));
            }
            ContentPart::ToolCall { name, .. } => {
                chunks.push(format!("[nested tool call: {name}]"));
            }
            ContentPart::ToolResult { content, .. } => {
                collect_error_tool_result_text(content, chunks);
            }
        }
    }
}

/// Extract a thinking signature stored under `provider_metadata` if present.
fn reasoning_signature_from_metadata(part: &ContentPart) -> Option<String> {
    if let ContentPart::Reasoning {
        provider_metadata: Some(meta),
        ..
    } = part
    {
        meta.get("signature")
            .and_then(Value::as_str)
            .map(str::to_string)
    } else {
        None
    }
}

/// Build an Anthropic tool definition.
fn build_tool(tool: &ToolDefinition) -> Value {
    let mut obj = Map::new();
    obj.insert("name".to_string(), Value::String(tool.name.clone()));
    if !tool.description.is_empty() {
        obj.insert(
            "description".to_string(),
            Value::String(tool.description.clone()),
        );
    }
    obj.insert("input_schema".to_string(), tool.input_schema.clone());
    Value::Object(obj)
}

/// Translate a [`ToolChoice`] into Anthropic's `tool_choice` object.
fn build_tool_choice(choice: &ToolChoice) -> Value {
    match choice {
        ToolChoice::Auto => json!({ "type": "auto" }),
        ToolChoice::Required => json!({ "type": "any" }),
        ToolChoice::None => json!({ "type": "none" }),
        ToolChoice::Tool { name } => json!({ "type": "tool", "name": name }),
    }
}

// ---------------------------------------------------------------------------
// Streaming decoder
// ---------------------------------------------------------------------------

/// Stateful decoder for the Anthropic Messages SSE stream.
///
/// Anthropic keys content blocks by integer `index`; the canonical event model
/// keys text / reasoning / tool blocks by a string `id`. We derive a stable
/// per-block id from the index (`block-{index}`) so the [`Lifecycle`] and
/// [`ToolStream`] helpers can track open blocks, and remember each block's kind
/// so `content_block_stop` closes the right one.
struct AnthropicMessagesStream {
    lifecycle: Lifecycle,
    tools: ToolStream,
    usage: Usage,
    finish_reason: Option<FinishReason>,
    /// Active content blocks, keyed by Anthropic `index` → (kind, id).
    blocks: std::collections::BTreeMap<u64, Block>,
    started: bool,
}

#[derive(Debug, Clone)]
struct Block {
    kind: BlockKind,
    id: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BlockKind {
    Text,
    Thinking,
    ToolUse,
}

impl AnthropicMessagesStream {
    fn new() -> Self {
        Self {
            lifecycle: Lifecycle::new(),
            tools: ToolStream::new(),
            usage: Usage::default(),
            finish_reason: None,
            blocks: std::collections::BTreeMap::new(),
            started: false,
        }
    }

    /// Emit `StepStart` exactly once, before any content events.
    ///
    /// `StepStart` can originate either here (e.g. on `message_start`, or before
    /// a tool-only block that the [`Lifecycle`] never sees) or from the
    /// [`Lifecycle`] itself on its first text/reasoning delta. To guarantee a
    /// single `StepStart`, this records that the step has begun; events later
    /// produced by the helpers are funneled through [`Self::push`], which drops
    /// any duplicate `StepStart`.
    fn ensure_started(&mut self, out: &mut Vec<LlmEvent>) {
        if !self.started {
            self.started = true;
            out.push(LlmEvent::StepStart);
        }
    }

    /// Append helper-produced events, dropping a duplicate leading `StepStart`
    /// once the step has already been announced.
    fn push(&mut self, out: &mut Vec<LlmEvent>, events: impl IntoIterator<Item = LlmEvent>) {
        for event in events {
            if matches!(event, LlmEvent::StepStart) {
                if self.started {
                    continue;
                }
                self.started = true;
            }
            out.push(event);
        }
    }

    fn block_id(index: u64) -> String {
        format!("block-{index}")
    }

    fn handle_message_start(&mut self, data: &Value, out: &mut Vec<LlmEvent>) {
        if let Some(usage) = data.pointer("/message/usage") {
            self.apply_usage(usage);
        }
        self.ensure_started(out);
    }

    fn handle_content_block_start(
        &mut self,
        data: &Value,
        out: &mut Vec<LlmEvent>,
    ) -> Result<(), LlmError> {
        self.ensure_started(out);
        let index = data.get("index").and_then(Value::as_u64).unwrap_or(0);
        let id = Self::block_id(index);
        let block = data.get("content_block");
        let kind = block.and_then(|b| b.get("type")).and_then(Value::as_str);
        match kind {
            Some("text") => {
                self.blocks.insert(
                    index,
                    Block {
                        kind: BlockKind::Text,
                        id: id.clone(),
                    },
                );
                // Opening a text block with no delta yet: emit the start
                // immediately so consumers see a well-formed boundary.
                let events = self.lifecycle.text_delta(&id, "");
                self.push(out, events);
            }
            Some("thinking") => {
                self.blocks.insert(
                    index,
                    Block {
                        kind: BlockKind::Thinking,
                        id: id.clone(),
                    },
                );
                let events = self.lifecycle.reasoning_delta(&id, "");
                self.push(out, events);
            }
            Some("tool_use") => {
                let tool_id = block
                    .and_then(|b| b.get("id"))
                    .and_then(Value::as_str)
                    .unwrap_or(&id)
                    .to_string();
                let name = block
                    .and_then(|b| b.get("name"))
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                self.blocks.insert(
                    index,
                    Block {
                        kind: BlockKind::ToolUse,
                        id: tool_id.clone(),
                    },
                );
                let events = self.tools.start(&tool_id, name);
                self.push(out, events);
            }
            // `redacted_thinking` and other unknown block kinds are ignored.
            _ => {}
        }
        Ok(())
    }

    fn handle_content_block_delta(&mut self, data: &Value, out: &mut Vec<LlmEvent>) {
        let index = data.get("index").and_then(Value::as_u64).unwrap_or(0);
        let delta = match data.get("delta") {
            Some(delta) => delta,
            None => return,
        };
        let id = match self.blocks.get(&index) {
            Some(block) => block.id.clone(),
            None => Self::block_id(index),
        };
        match delta.get("type").and_then(Value::as_str) {
            Some("text_delta") => {
                if let Some(text) = delta.get("text").and_then(Value::as_str) {
                    let events = self.lifecycle.text_delta(&id, text);
                    self.push(out, events);
                }
            }
            Some("thinking_delta") => {
                if let Some(thinking) = delta.get("thinking").and_then(Value::as_str) {
                    let events = self.lifecycle.reasoning_delta(&id, thinking);
                    self.push(out, events);
                }
            }
            // The signature arrives as its own delta at block end; the canonical
            // event model has no signature field on the stream, so it is dropped
            // here (it is recovered on the request side via provider_metadata).
            Some("signature_delta") => {}
            Some("input_json_delta") => {
                if let Some(partial) = delta.get("partial_json").and_then(Value::as_str) {
                    let events = self.tools.delta(&id, None, partial);
                    self.push(out, events);
                }
            }
            _ => {}
        }
    }

    fn handle_content_block_stop(
        &mut self,
        data: &Value,
        out: &mut Vec<LlmEvent>,
    ) -> Result<(), LlmError> {
        let index = data.get("index").and_then(Value::as_u64).unwrap_or(0);
        if let Some(block) = self.blocks.remove(&index) {
            let events = match block.kind {
                BlockKind::Text => self.lifecycle.text_end(&block.id),
                BlockKind::Thinking => self.lifecycle.reasoning_end(&block.id),
                BlockKind::ToolUse => self.tools.end(&block.id)?,
            };
            self.push(out, events);
        }
        Ok(())
    }

    fn handle_message_delta(&mut self, data: &Value) {
        if let Some(reason) = data.pointer("/delta/stop_reason").and_then(Value::as_str) {
            self.finish_reason = Some(map_stop_reason(reason));
        }
        if let Some(usage) = data.get("usage") {
            self.apply_usage(usage);
        }
    }

    /// Merge any usage fields present in `usage` into the running total.
    fn apply_usage(&mut self, usage: &Value) {
        if let Some(v) = usage.get("input_tokens").and_then(Value::as_u64) {
            self.set_uncached_input_tokens(v);
        }
        if let Some(v) = usage.get("output_tokens").and_then(Value::as_u64) {
            self.usage.output_tokens = v;
        }
        if let Some(v) = usage.get("cache_read_input_tokens").and_then(Value::as_u64) {
            self.set_cached_input_tokens(v);
        }
        if let Some(v) = usage
            .get("cache_creation_input_tokens")
            .and_then(Value::as_u64)
        {
            self.usage.cache_creation_input_tokens = v;
        }
    }

    fn set_uncached_input_tokens(&mut self, raw_input_tokens: u64) {
        self.usage.input_tokens = raw_input_tokens.saturating_add(self.usage.cached_input_tokens);
    }

    fn set_cached_input_tokens(&mut self, cached_input_tokens: u64) {
        let raw_input_tokens = self
            .usage
            .input_tokens
            .saturating_sub(self.usage.cached_input_tokens);
        self.usage.cached_input_tokens = cached_input_tokens;
        self.usage.input_tokens = raw_input_tokens.saturating_add(cached_input_tokens);
    }

    /// Flush open blocks and emit `StepFinish` + `Finish` (idempotent).
    fn flush_finish(&mut self) -> Result<Vec<LlmEvent>, LlmError> {
        let mut out = Vec::new();
        if self.lifecycle.is_finished() {
            return Ok(out);
        }
        // Close any blocks left open by a missing `content_block_stop`.
        let open: Vec<Block> = std::mem::take(&mut self.blocks).into_values().collect();
        for block in open {
            let events = match block.kind {
                BlockKind::Text => self.lifecycle.text_end(&block.id),
                BlockKind::Thinking => self.lifecycle.reasoning_end(&block.id),
                BlockKind::ToolUse => self.tools.end(&block.id)?,
            };
            self.push(&mut out, events);
        }
        let flushed = self.tools.flush()?;
        self.push(&mut out, flushed);
        let finished = self.lifecycle.finish(self.usage, self.finish_reason);
        self.push(&mut out, finished);
        Ok(out)
    }
}

impl ProtocolStream for AnthropicMessagesStream {
    fn on_frame(&mut self, frame: &SseFrame) -> Result<Vec<LlmEvent>, LlmError> {
        let event_name = match frame.event.as_deref() {
            Some(name) => name,
            None => return Ok(Vec::new()),
        };

        if event_name == "ping" {
            return Ok(Vec::new());
        }

        let data: Value = serde_json::from_str(&frame.data).map_err(|e| {
            LlmError::new(
                LlmErrorReason::Decode,
                format!("anthropic SSE frame is not valid JSON: {e}"),
            )
        })?;

        let mut out = Vec::new();
        match event_name {
            "message_start" => self.handle_message_start(&data, &mut out),
            "content_block_start" => self.handle_content_block_start(&data, &mut out)?,
            "content_block_delta" => self.handle_content_block_delta(&data, &mut out),
            "content_block_stop" => self.handle_content_block_stop(&data, &mut out)?,
            "message_delta" => self.handle_message_delta(&data),
            "message_stop" => out.extend(self.flush_finish()?),
            "error" => {
                let message = data
                    .pointer("/error/message")
                    .and_then(Value::as_str)
                    .unwrap_or("anthropic stream error")
                    .to_string();
                let retryable = matches!(
                    data.pointer("/error/type").and_then(Value::as_str),
                    Some("overloaded_error") | Some("api_error")
                );
                out.push(LlmEvent::ProviderError { message, retryable });
            }
            _ => {}
        }
        Ok(out)
    }

    fn finish(&mut self) -> Result<Vec<LlmEvent>, LlmError> {
        self.flush_finish()
    }
}

/// Map an Anthropic `stop_reason` to a canonical [`FinishReason`].
fn map_stop_reason(reason: &str) -> FinishReason {
    match reason {
        "end_turn" | "stop_sequence" => FinishReason::Stop,
        "max_tokens" => FinishReason::Length,
        "tool_use" => FinishReason::ToolUse,
        "refusal" => FinishReason::ContentFilter,
        _ => FinishReason::Other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{GenerationOptions, Message, MessageRole, SystemPart};
    use serde_json::json;

    fn frame(event: &str, data: Value) -> SseFrame {
        SseFrame {
            event: Some(event.to_string()),
            data: data.to_string(),
        }
    }

    fn drive(frames: &[SseFrame]) -> Vec<LlmEvent> {
        let proto = AnthropicMessagesProtocol::new();
        let mut stream = proto.decoder();
        let mut events = Vec::new();
        for f in frames {
            events.extend(stream.on_frame(f).expect("frame decodes"));
        }
        events.extend(stream.finish().expect("finish"));
        events
    }

    #[test]
    fn build_body_golden_system_user_and_tool() {
        let mut req = LlmRequest::new("claude-sonnet-4-6", "anthropic");
        req.system.push(SystemPart::new("You are helpful."));
        req.system.push(SystemPart::new("Be concise."));
        req.messages
            .push(Message::user_text("What is the weather in Paris?"));
        req.tools.push(ToolDefinition {
            name: "get_weather".into(),
            description: "Look up the weather.".into(),
            input_schema: json!({
                "type": "object",
                "properties": { "city": { "type": "string" } },
                "required": ["city"],
            }),
            output_schema: None,
            namespace: None,
            namespace_description: None,
        });
        req.tool_choice = Some(ToolChoice::Auto);

        let body = AnthropicMessagesProtocol::new()
            .build_body(&req)
            .expect("body builds");

        assert_eq!(body["model"], json!("claude-sonnet-4-6"));
        assert_eq!(body["stream"], json!(true));
        // max_tokens defaults to 16000 when unset.
        assert_eq!(body["max_tokens"], json!(16_000));

        // `system` is a top-level array of text blocks.
        assert_eq!(
            body["system"],
            json!([
                { "type": "text", "text": "You are helpful." },
                { "type": "text", "text": "Be concise." },
            ])
        );

        // messages with content blocks.
        assert_eq!(
            body["messages"],
            json!([
                {
                    "role": "user",
                    "content": [
                        { "type": "text", "text": "What is the weather in Paris?" }
                    ]
                }
            ])
        );

        // tools shape: name / description / input_schema.
        assert_eq!(
            body["tools"],
            json!([
                {
                    "name": "get_weather",
                    "description": "Look up the weather.",
                    "input_schema": {
                        "type": "object",
                        "properties": { "city": { "type": "string" } },
                        "required": ["city"],
                    },
                    "cache_control": { "type": "ephemeral" }
                }
            ])
        );

        // tool_choice: Auto -> {"type":"auto"}.
        assert_eq!(body["tool_choice"], json!({ "type": "auto" }));
    }

    #[test]
    fn build_body_marks_cache_control_breakpoints() {
        let mut req = LlmRequest::new("claude-sonnet-4-6", "anthropic");
        let mut system = SystemPart::new("Stable system prompt.");
        system.cache = Some(CacheHint::Ephemeral);
        req.system.push(system);
        req.messages
            .push(Message::user_text("Current browser state.").with_cache(CacheHint::Ephemeral));
        for name in ["first_tool", "last_tool"] {
            req.tools.push(ToolDefinition {
                name: name.into(),
                description: String::new(),
                input_schema: json!({ "type": "object" }),
                output_schema: None,
                namespace: None,
                namespace_description: None,
            });
        }

        let body = AnthropicMessagesProtocol::new().build_body(&req).unwrap();

        assert_eq!(
            body["system"][0]["cache_control"],
            json!({ "type": "ephemeral" })
        );
        assert_eq!(
            body["messages"][0]["content"][0]["cache_control"],
            json!({ "type": "ephemeral" })
        );
        assert!(body["tools"][0].get("cache_control").is_none());
        assert_eq!(
            body["tools"][1]["cache_control"],
            json!({ "type": "ephemeral" })
        );
    }

    #[test]
    fn build_body_respects_max_tokens_and_omits_empty_sections() {
        let mut req = LlmRequest::new("m", "anthropic");
        req.messages.push(Message::user_text("hi"));
        req.generation = GenerationOptions {
            max_tokens: Some(512),
            // 0.5 is exactly representable as f32, avoiding lossy-widening noise
            // when the f32 is serialized through serde_json as an f64.
            temperature: Some(0.5),
            ..Default::default()
        };

        let body = AnthropicMessagesProtocol::new().build_body(&req).unwrap();
        assert_eq!(body["max_tokens"], json!(512));
        assert_eq!(body["temperature"], json!(0.5));
        assert!(body.get("system").is_none());
        assert!(body.get("tools").is_none());
        assert!(body.get("tool_choice").is_none());
    }

    #[test]
    fn build_body_maps_tool_use_and_tool_result() {
        let mut req = LlmRequest::new("m", "anthropic");
        req.messages.push(Message::new(
            MessageRole::Assistant,
            vec![ContentPart::ToolCall {
                id: "toolu_1".into(),
                name: "get_weather".into(),
                input: json!({ "city": "Paris" }),
                provider_metadata: None,
            }],
        ));
        req.messages.push(Message::new(
            MessageRole::Tool,
            vec![ContentPart::ToolResult {
                tool_call_id: "toolu_1".into(),
                content: vec![ContentPart::text("Sunny, 20C")],
                is_error: false,
            }],
        ));

        let body = AnthropicMessagesProtocol::new().build_body(&req).unwrap();
        assert_eq!(
            body["messages"],
            json!([
                {
                    "role": "assistant",
                    "content": [
                        {
                            "type": "tool_use",
                            "id": "toolu_1",
                            "name": "get_weather",
                            "input": { "city": "Paris" }
                        }
                    ]
                },
                {
                    "role": "user",
                    "content": [
                        {
                            "type": "tool_result",
                            "tool_use_id": "toolu_1",
                            "content": [ { "type": "text", "text": "Sunny, 20C" } ]
                        }
                    ]
                }
            ])
        );
    }

    #[test]
    fn build_body_flattens_error_tool_result_content_to_text_blocks() {
        let mut req = LlmRequest::new("m", "anthropic");
        req.messages.push(Message::new(
            MessageRole::Tool,
            vec![ContentPart::ToolResult {
                tool_call_id: "toolu_error".into(),
                content: vec![
                    ContentPart::text("browser script failed"),
                    ContentPart::Media {
                        mime_type: "image/png".into(),
                        data: Some("base64-image".into()),
                        url: None,
                        detail: None,
                    },
                ],
                is_error: true,
            }],
        ));

        let body = AnthropicMessagesProtocol::new().build_body(&req).unwrap();
        assert_eq!(
            body["messages"],
            json!([
                {
                    "role": "user",
                    "content": [
                        {
                            "type": "tool_result",
                            "tool_use_id": "toolu_error",
                            "is_error": true,
                            "content": [
                                {
                                    "type": "text",
                                    "text": "browser script failed\n[image/png media inline]"
                                }
                            ]
                        }
                    ]
                }
            ])
        );
    }

    #[test]
    fn build_body_maps_reasoning_signature_to_thinking_block() {
        let mut req = LlmRequest::new("m", "anthropic");
        req.messages.push(Message::new(
            MessageRole::Assistant,
            vec![ContentPart::Reasoning {
                text: "Let me think.".into(),
                signature: Some("sig-abc".into()),
                provider_metadata: None,
            }],
        ));

        let body = AnthropicMessagesProtocol::new().build_body(&req).unwrap();
        assert_eq!(
            body["messages"][0]["content"][0],
            json!({
                "type": "thinking",
                "thinking": "Let me think.",
                "signature": "sig-abc"
            })
        );
    }

    #[test]
    fn build_body_recovers_thinking_signature_from_provider_metadata() {
        let mut req = LlmRequest::new("m", "anthropic");
        req.messages.push(Message::new(
            MessageRole::Assistant,
            vec![ContentPart::Reasoning {
                text: "Hmm.".into(),
                signature: None,
                provider_metadata: Some(json!({ "signature": "sig-meta" })),
            }],
        ));

        let body = AnthropicMessagesProtocol::new().build_body(&req).unwrap();
        assert_eq!(
            body["messages"][0]["content"][0]["signature"],
            json!("sig-meta")
        );
    }

    #[test]
    fn build_body_tool_choice_variants() {
        let mk = |choice: ToolChoice| {
            let mut req = LlmRequest::new("m", "anthropic");
            req.messages.push(Message::user_text("hi"));
            req.tool_choice = Some(choice);
            AnthropicMessagesProtocol::new().build_body(&req).unwrap()["tool_choice"].clone()
        };

        assert_eq!(mk(ToolChoice::Auto), json!({ "type": "auto" }));
        assert_eq!(mk(ToolChoice::Required), json!({ "type": "any" }));
        assert_eq!(mk(ToolChoice::None), json!({ "type": "none" }));
        assert_eq!(
            mk(ToolChoice::Tool { name: "x".into() }),
            json!({ "type": "tool", "name": "x" })
        );
    }

    #[test]
    fn build_body_image_media_block() {
        let mut req = LlmRequest::new("m", "anthropic");
        req.messages.push(Message::new(
            MessageRole::User,
            vec![ContentPart::Media {
                mime_type: "image/png".into(),
                data: Some("AAAA".into()),
                url: None,
                detail: None,
            }],
        ));
        let body = AnthropicMessagesProtocol::new().build_body(&req).unwrap();
        assert_eq!(
            body["messages"][0]["content"][0],
            json!({
                "type": "image",
                "source": { "type": "base64", "media_type": "image/png", "data": "AAAA" }
            })
        );
    }

    #[test]
    fn build_body_downsamples_oversized_inline_media_for_anthropic() {
        let oversized = image::DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(
            2400,
            1200,
            image::Rgba([10, 20, 30, 255]),
        ));
        let mut png = Cursor::new(Vec::new());
        oversized
            .write_to(&mut png, image::ImageFormat::Png)
            .unwrap();
        let original_data = general_purpose::STANDARD.encode(png.into_inner());

        let mut req = LlmRequest::new("m", "anthropic");
        req.messages.push(Message::new(
            MessageRole::User,
            vec![ContentPart::Media {
                mime_type: "image/png".into(),
                data: Some(original_data.clone()),
                url: None,
                detail: None,
            }],
        ));

        let body = AnthropicMessagesProtocol::new().build_body(&req).unwrap();
        let source = &body["messages"][0]["content"][0]["source"];
        assert_eq!(source["media_type"], "image/png");
        let data = source["data"].as_str().expect("base64 image data");
        assert_ne!(data, original_data);
        let bytes = general_purpose::STANDARD.decode(data).unwrap();
        let resized = image::load_from_memory(&bytes).unwrap();
        assert_eq!(resized.width(), 2000);
        assert_eq!(resized.height(), 1000);
    }

    #[test]
    fn build_body_rejects_unbounded_inline_image_normalization() {
        let mut req = LlmRequest::new("m", "anthropic");
        req.messages.push(Message::new(
            MessageRole::User,
            vec![ContentPart::Media {
                mime_type: "image/png".into(),
                data: Some("A".repeat(ANTHROPIC_MAX_INLINE_IMAGE_BASE64_CHARS + 1)),
                url: None,
                detail: None,
            }],
        ));

        let err = AnthropicMessagesProtocol::new()
            .build_body(&req)
            .expect_err("oversized inline image should fail before decode");

        assert_eq!(err.reason, LlmErrorReason::InvalidRequest);
        assert!(err.message.contains("inline image is too large"));
    }

    #[test]
    fn decoder_full_stream_sequence_and_usage() {
        let frames = vec![
            frame(
                "message_start",
                json!({
                    "type": "message_start",
                    "message": {
                        "id": "msg_1",
                        "role": "assistant",
                        "content": [],
                        "usage": { "input_tokens": 25, "output_tokens": 1 }
                    }
                }),
            ),
            frame(
                "content_block_start",
                json!({
                    "type": "content_block_start",
                    "index": 0,
                    "content_block": { "type": "text", "text": "" }
                }),
            ),
            frame(
                "content_block_delta",
                json!({
                    "type": "content_block_delta",
                    "index": 0,
                    "delta": { "type": "text_delta", "text": "Hello" }
                }),
            ),
            frame(
                "content_block_delta",
                json!({
                    "type": "content_block_delta",
                    "index": 0,
                    "delta": { "type": "text_delta", "text": " there" }
                }),
            ),
            frame(
                "content_block_stop",
                json!({ "type": "content_block_stop", "index": 0 }),
            ),
            frame(
                "content_block_start",
                json!({
                    "type": "content_block_start",
                    "index": 1,
                    "content_block": {
                        "type": "tool_use",
                        "id": "toolu_1",
                        "name": "get_weather",
                        "input": {}
                    }
                }),
            ),
            frame(
                "content_block_delta",
                json!({
                    "type": "content_block_delta",
                    "index": 1,
                    "delta": { "type": "input_json_delta", "partial_json": "{\"city\":" }
                }),
            ),
            frame(
                "content_block_delta",
                json!({
                    "type": "content_block_delta",
                    "index": 1,
                    "delta": { "type": "input_json_delta", "partial_json": "\"Paris\"}" }
                }),
            ),
            frame(
                "content_block_stop",
                json!({ "type": "content_block_stop", "index": 1 }),
            ),
            frame(
                "message_delta",
                json!({
                    "type": "message_delta",
                    "delta": { "stop_reason": "tool_use", "stop_sequence": null },
                    "usage": { "output_tokens": 15 }
                }),
            ),
            frame("message_stop", json!({ "type": "message_stop" })),
        ];

        let events = drive(&frames);

        let expected = vec![
            LlmEvent::StepStart,
            // text block start (empty delta emits start, then an empty delta)
            LlmEvent::TextStart {
                id: "block-0".into(),
            },
            LlmEvent::TextDelta {
                id: "block-0".into(),
                delta: "".into(),
            },
            LlmEvent::TextDelta {
                id: "block-0".into(),
                delta: "Hello".into(),
            },
            LlmEvent::TextDelta {
                id: "block-0".into(),
                delta: " there".into(),
            },
            LlmEvent::TextEnd {
                id: "block-0".into(),
                phase: None,
            },
            LlmEvent::ToolInputStart {
                id: "toolu_1".into(),
                name: "get_weather".into(),
            },
            LlmEvent::ToolInputDelta {
                id: "toolu_1".into(),
                delta: "{\"city\":".into(),
            },
            LlmEvent::ToolInputDelta {
                id: "toolu_1".into(),
                delta: "\"Paris\"}".into(),
            },
            LlmEvent::ToolInputEnd {
                id: "toolu_1".into(),
            },
            LlmEvent::ToolCall {
                id: "toolu_1".into(),
                name: "get_weather".into(),
                namespace: None,
                provider_metadata: None,
                input: json!({ "city": "Paris" }),
            },
            LlmEvent::StepFinish {
                usage: Usage {
                    input_tokens: 25,
                    output_tokens: 15,
                    ..Default::default()
                },
                finish_reason: Some(FinishReason::ToolUse),
            },
            LlmEvent::Finish {
                usage: Usage {
                    input_tokens: 25,
                    output_tokens: 15,
                    ..Default::default()
                },
                finish_reason: Some(FinishReason::ToolUse),
            },
        ];

        assert_eq!(events, expected);
    }

    #[test]
    fn decoder_normalizes_anthropic_cached_usage_to_inclusive_input() {
        let frames = vec![
            frame(
                "message_start",
                json!({
                    "type": "message_start",
                    "message": {
                        "id": "msg_cache",
                        "role": "assistant",
                        "content": [],
                        "usage": {
                            "input_tokens": 12,
                            "cache_creation_input_tokens": 44,
                            "output_tokens": 0
                        }
                    }
                }),
            ),
            frame(
                "message_delta",
                json!({
                    "type": "message_delta",
                    "delta": { "stop_reason": "end_turn", "stop_sequence": null },
                    "usage": {
                        "output_tokens": 3088,
                        "cache_read_input_tokens": 183250
                    }
                }),
            ),
            frame("message_stop", json!({ "type": "message_stop" })),
        ];

        let events = drive(&frames);
        let usage = Usage {
            input_tokens: 183262,
            cached_input_tokens: 183250,
            cache_creation_input_tokens: 44,
            output_tokens: 3088,
            ..Default::default()
        };

        assert!(events.contains(&LlmEvent::StepFinish {
            usage,
            finish_reason: Some(FinishReason::Stop),
        }));
        assert!(events.contains(&LlmEvent::Finish {
            usage,
            finish_reason: Some(FinishReason::Stop),
        }));
    }

    #[test]
    fn decoder_handles_thinking_block_and_signature() {
        let frames = vec![
            frame(
                "message_start",
                json!({
                    "type": "message_start",
                    "message": { "usage": { "input_tokens": 5, "output_tokens": 0 } }
                }),
            ),
            frame(
                "content_block_start",
                json!({
                    "type": "content_block_start",
                    "index": 0,
                    "content_block": { "type": "thinking", "thinking": "" }
                }),
            ),
            frame(
                "content_block_delta",
                json!({
                    "type": "content_block_delta",
                    "index": 0,
                    "delta": { "type": "thinking_delta", "thinking": "Reasoning..." }
                }),
            ),
            frame(
                "content_block_delta",
                json!({
                    "type": "content_block_delta",
                    "index": 0,
                    "delta": { "type": "signature_delta", "signature": "sig-x" }
                }),
            ),
            frame(
                "content_block_stop",
                json!({ "type": "content_block_stop", "index": 0 }),
            ),
            frame(
                "message_delta",
                json!({
                    "type": "message_delta",
                    "delta": { "stop_reason": "end_turn" },
                    "usage": { "output_tokens": 7 }
                }),
            ),
            frame("message_stop", json!({ "type": "message_stop" })),
        ];

        let events = drive(&frames);
        assert_eq!(
            events,
            vec![
                LlmEvent::StepStart,
                LlmEvent::ReasoningStart {
                    id: "block-0".into()
                },
                LlmEvent::ReasoningDelta {
                    id: "block-0".into(),
                    delta: "".into()
                },
                LlmEvent::ReasoningDelta {
                    id: "block-0".into(),
                    delta: "Reasoning...".into()
                },
                LlmEvent::ReasoningEnd {
                    id: "block-0".into()
                },
                LlmEvent::StepFinish {
                    usage: Usage {
                        input_tokens: 5,
                        output_tokens: 7,
                        ..Default::default()
                    },
                    finish_reason: Some(FinishReason::Stop),
                },
                LlmEvent::Finish {
                    usage: Usage {
                        input_tokens: 5,
                        output_tokens: 7,
                        ..Default::default()
                    },
                    finish_reason: Some(FinishReason::Stop),
                },
            ]
        );
    }

    #[test]
    fn decoder_finish_is_idempotent() {
        let proto = AnthropicMessagesProtocol::new();
        let mut stream = proto.decoder();
        let mut events = stream
            .on_frame(&frame("message_stop", json!({ "type": "message_stop" })))
            .unwrap();
        // A subsequent explicit finish() must not emit a second Finish.
        events.extend(stream.finish().unwrap());
        assert_eq!(
            events,
            vec![
                LlmEvent::StepStart,
                LlmEvent::StepFinish {
                    usage: Usage::default(),
                    finish_reason: None,
                },
                LlmEvent::Finish {
                    usage: Usage::default(),
                    finish_reason: None,
                },
            ]
        );
    }

    #[test]
    fn decoder_emits_provider_error() {
        let events = AnthropicMessagesProtocol::new()
            .decoder()
            .on_frame(&frame(
                "error",
                json!({
                    "type": "error",
                    "error": { "type": "overloaded_error", "message": "Overloaded" }
                }),
            ))
            .unwrap();
        assert_eq!(
            events,
            vec![LlmEvent::ProviderError {
                message: "Overloaded".into(),
                retryable: true,
            }]
        );
    }

    #[test]
    fn decoder_ignores_ping_and_unframed() {
        let proto = AnthropicMessagesProtocol::new();
        let mut stream = proto.decoder();
        assert!(stream
            .on_frame(&frame("ping", json!({ "type": "ping" })))
            .unwrap()
            .is_empty());
        assert!(stream
            .on_frame(&SseFrame {
                event: None,
                data: "{}".into(),
            })
            .unwrap()
            .is_empty());
    }

    #[test]
    fn decoder_rejects_invalid_json() {
        let err = AnthropicMessagesProtocol::new()
            .decoder()
            .on_frame(&SseFrame {
                event: Some("message_start".into()),
                data: "{not json".into(),
            })
            .unwrap_err();
        assert_eq!(err.reason, LlmErrorReason::Decode);
    }

    #[test]
    fn map_stop_reason_covers_known_values() {
        assert_eq!(map_stop_reason("end_turn"), FinishReason::Stop);
        assert_eq!(map_stop_reason("stop_sequence"), FinishReason::Stop);
        assert_eq!(map_stop_reason("max_tokens"), FinishReason::Length);
        assert_eq!(map_stop_reason("tool_use"), FinishReason::ToolUse);
        assert_eq!(map_stop_reason("refusal"), FinishReason::ContentFilter);
        assert_eq!(map_stop_reason("something_new"), FinishReason::Other);
    }
}
