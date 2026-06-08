//! The provider-neutral request shape: content parts, messages, tools, request.
//!
//! Reasoning and tool-calls are first-class content parts (not provider blobs).
//! Each part that round-trips provider-specific data (Anthropic thinking
//! signatures, OpenAI encrypted reasoning, etc.) carries an open
//! `provider_metadata` escape hatch.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::ids::{MessageRole, ModelId, ProviderId};
use super::options::GenerationOptions;

/// A single piece of message content.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentPart {
    Text {
        text: String,
    },
    /// Inline media. Exactly one of `data` (base64) or `url` is expected.
    Media {
        mime_type: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        data: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        url: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        detail: Option<String>,
    },
    ToolCall {
        id: String,
        name: String,
        input: Value,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provider_metadata: Option<Value>,
    },
    ToolResult {
        tool_call_id: String,
        #[serde(default)]
        content: Vec<ContentPart>,
        #[serde(default)]
        is_error: bool,
    },
    Reasoning {
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        signature: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provider_metadata: Option<Value>,
    },
}

impl ContentPart {
    pub fn text(s: impl Into<String>) -> Self {
        ContentPart::Text { text: s.into() }
    }
}

/// One system-prompt block (Anthropic wants an array; OpenAI a single string).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SystemPart {
    pub text: String,
    /// Optional prompt-cache hint; only honored by protocols that support
    /// inline cache markers (Anthropic / Bedrock).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache: Option<CacheHint>,
}

impl SystemPart {
    pub fn new(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            cache: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CacheHint {
    Ephemeral,
}

/// A conversation message.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Message {
    pub role: MessageRole,
    /// Optional prompt-cache hint; only honored by protocols that support
    /// inline cache markers (Anthropic / Bedrock).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache: Option<CacheHint>,
    #[serde(default)]
    pub content: Vec<ContentPart>,
}

impl Message {
    pub fn new(role: MessageRole, content: Vec<ContentPart>) -> Self {
        Self {
            role,
            cache: None,
            content,
        }
    }
    pub fn user_text(s: impl Into<String>) -> Self {
        Self::new(MessageRole::User, vec![ContentPart::text(s)])
    }
    pub fn with_cache(mut self, cache: CacheHint) -> Self {
        self.cache = Some(cache);
        self
    }
}

/// A tool the model may call. The handler is never on the wire — only schema.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolDefinition {
    pub name: String,
    #[serde(default)]
    pub description: String,
    /// JSON Schema for the tool input.
    pub input_schema: Value,
    /// Optional JSON Schema for the tool output. Protocol lowerers that cannot
    /// send output schemas may keep this as local metadata.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_schema: Option<Value>,
    /// Optional Responses API namespace. Protocol lowerers that cannot send
    /// namespaces keep the tool flat.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub namespace: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub namespace_description: Option<String>,
}

/// How the model should choose tools.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolChoice {
    Auto,
    None,
    Required,
    Tool { name: String },
}

/// The provider-neutral request. Each protocol lowers this to its native body.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LlmRequest {
    pub model: ModelId,
    pub provider: ProviderId,
    #[serde(default)]
    pub system: Vec<SystemPart>,
    #[serde(default)]
    pub messages: Vec<Message>,
    #[serde(default)]
    pub tools: Vec<ToolDefinition>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<ToolChoice>,
    #[serde(default)]
    pub generation: GenerationOptions,
    /// Provider-specific overrides not normalized by the schema.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_options: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_format: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Value>,
}

impl LlmRequest {
    pub fn new(model: impl Into<ModelId>, provider: impl Into<ProviderId>) -> Self {
        Self {
            model: model.into(),
            provider: provider.into(),
            system: Vec::new(),
            messages: Vec::new(),
            tools: Vec::new(),
            tool_choice: None,
            generation: GenerationOptions::default(),
            provider_options: None,
            response_format: None,
            metadata: None,
        }
    }
}
