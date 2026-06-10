//! `ToolStream` — accumulates streamed tool-call argument JSON keyed by the
//! provider's stream-local id, and emits a normalized
//! `tool_input_start → tool_input_delta* → tool_input_end → tool_call`
//! sequence. Handles both "identity on first delta" (OpenAI chat) and
//! "explicit start event" (Anthropic / OpenAI Responses) provider shapes.
//! Pure, synchronous.

use std::collections::BTreeMap;

use serde_json::Value;

use crate::schema::{LlmError, LlmErrorReason, LlmEvent};

#[derive(Debug, Default)]
struct Accum {
    name: String,
    namespace: Option<String>,
    provider_metadata: Option<Value>,
    args: String,
    started: bool,
    ended: bool,
}

#[derive(Debug, Default)]
pub struct ToolStream {
    calls: BTreeMap<String, Accum>,
    order: Vec<String>,
}

impl ToolStream {
    pub fn new() -> Self {
        Self::default()
    }

    fn entry(&mut self, id: &str) -> &mut Accum {
        if !self.calls.contains_key(id) {
            self.calls.insert(id.to_string(), Accum::default());
            self.order.push(id.to_string());
        }
        self.calls.get_mut(id).expect("just inserted")
    }

    /// Explicit start (Anthropic / Responses). Emits `ToolInputStart` once.
    pub fn start(&mut self, id: impl AsRef<str>, name: impl Into<String>) -> Vec<LlmEvent> {
        self.start_with_namespace(id, name, None)
    }

    /// Explicit start with a provider namespace. Emits `ToolInputStart` once.
    pub fn start_with_namespace(
        &mut self,
        id: impl AsRef<str>,
        name: impl Into<String>,
        namespace: Option<String>,
    ) -> Vec<LlmEvent> {
        let id = id.as_ref().to_string();
        let name = name.into();
        let e = self.entry(&id);
        if e.name.is_empty() {
            e.name = name;
        }
        if e.namespace.is_none() {
            e.namespace = namespace;
        }
        if e.started {
            return Vec::new();
        }
        e.started = true;
        let resolved = e.name.clone();
        vec![LlmEvent::ToolInputStart { id, name: resolved }]
    }

    /// Attach opaque provider metadata to a call. The next request can replay it
    /// without the core understanding provider-specific fields.
    pub fn set_provider_metadata(&mut self, id: impl AsRef<str>, metadata: Option<Value>) {
        if metadata.is_none() {
            return;
        }
        let e = self.entry(id.as_ref());
        if e.provider_metadata.is_none() {
            e.provider_metadata = metadata;
        }
    }

    /// Argument fragment. `name` may be supplied here for providers that only
    /// reveal the tool name on the first delta. Emits `ToolInputStart` (if not
    /// already started) followed by `ToolInputDelta`.
    pub fn delta(
        &mut self,
        id: impl AsRef<str>,
        name: Option<&str>,
        fragment: impl AsRef<str>,
    ) -> Vec<LlmEvent> {
        let id = id.as_ref().to_string();
        let frag = fragment.as_ref().to_string();
        let e = self.entry(&id);
        if let Some(n) = name {
            if e.name.is_empty() {
                e.name = n.to_string();
            }
        }
        let mut out = Vec::new();
        if !e.started {
            e.started = true;
            out.push(LlmEvent::ToolInputStart {
                id: id.clone(),
                name: e.name.clone(),
            });
        }
        e.args.push_str(&frag);
        out.push(LlmEvent::ToolInputDelta { id, delta: frag });
        out
    }

    /// Close one tool call: emit `ToolInputEnd` then a parsed `ToolCall`.
    /// No-op if the id is unknown or already ended.
    pub fn end(&mut self, id: impl AsRef<str>) -> Result<Vec<LlmEvent>, LlmError> {
        let id = id.as_ref().to_string();
        let (name, namespace, provider_metadata, args) = match self.calls.get_mut(&id) {
            Some(e) if !e.ended => {
                e.ended = true;
                (
                    e.name.clone(),
                    e.namespace.clone(),
                    e.provider_metadata.clone(),
                    e.args.clone(),
                )
            }
            _ => return Ok(Vec::new()),
        };
        let input = parse_args(&args)?;
        Ok(vec![
            LlmEvent::ToolInputEnd { id: id.clone() },
            LlmEvent::ToolCall {
                id,
                name,
                namespace,
                provider_metadata,
                input,
            },
        ])
    }

    /// Close one tool call, using a provider's final full `arguments` payload when
    /// no argument deltas were observed. OpenAI Responses can send only
    /// `function_call_arguments.done` with the complete arguments string.
    pub fn end_with_arguments(
        &mut self,
        id: impl AsRef<str>,
        arguments: Option<&str>,
    ) -> Result<Vec<LlmEvent>, LlmError> {
        let id = id.as_ref().to_string();
        let should_add_full_arguments = arguments
            .filter(|arguments| !arguments.is_empty())
            .is_some_and(|_| {
                self.calls
                    .get(&id)
                    .map(|entry| !entry.ended && entry.args.is_empty())
                    .unwrap_or(false)
            });
        let mut out = Vec::new();
        if should_add_full_arguments {
            out.extend(self.delta(&id, None, arguments.unwrap_or_default()));
        }
        out.extend(self.end(&id)?);
        Ok(out)
    }

    /// Close every still-open call, in arrival order.
    pub fn flush(&mut self) -> Result<Vec<LlmEvent>, LlmError> {
        let ids: Vec<String> = self
            .order
            .iter()
            .filter(|id| self.calls.get(*id).map(|e| !e.ended).unwrap_or(false))
            .cloned()
            .collect();
        let mut out = Vec::new();
        for id in ids {
            out.extend(self.end(&id)?);
        }
        Ok(out)
    }
}

fn parse_args(s: &str) -> Result<Value, LlmError> {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return Ok(Value::Object(serde_json::Map::new()));
    }
    serde_json::from_str(trimmed)
        .map_err(|e| LlmError::new(LlmErrorReason::Decode, format!("tool input JSON: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn anthropic_style_explicit_start() {
        let mut ts = ToolStream::new();
        assert_eq!(
            ts.start("c0", "shell"),
            vec![LlmEvent::ToolInputStart {
                id: "c0".into(),
                name: "shell".into()
            }]
        );
        assert_eq!(
            ts.delta("c0", None, "{\"command\":"),
            vec![LlmEvent::ToolInputDelta {
                id: "c0".into(),
                delta: "{\"command\":".into()
            }]
        );
        ts.delta("c0", None, "[\"ls\"]}");
        let end = ts.end("c0").unwrap();
        assert_eq!(
            end,
            vec![
                LlmEvent::ToolInputEnd { id: "c0".into() },
                LlmEvent::ToolCall {
                    id: "c0".into(),
                    name: "shell".into(),
                    namespace: None,
                    provider_metadata: None,
                    input: json!({ "command": ["ls"] }),
                },
            ]
        );
    }

    #[test]
    fn chat_style_name_on_first_delta_then_flush() {
        let mut ts = ToolStream::new();
        // no explicit start; name arrives with the first delta
        let first = ts.delta("0", Some("get_weather"), "{\"city\":\"NYC\"}");
        assert_eq!(
            first,
            vec![
                LlmEvent::ToolInputStart {
                    id: "0".into(),
                    name: "get_weather".into()
                },
                LlmEvent::ToolInputDelta {
                    id: "0".into(),
                    delta: "{\"city\":\"NYC\"}".into()
                },
            ]
        );
        let flushed = ts.flush().unwrap();
        assert_eq!(
            flushed,
            vec![
                LlmEvent::ToolInputEnd { id: "0".into() },
                LlmEvent::ToolCall {
                    id: "0".into(),
                    name: "get_weather".into(),
                    namespace: None,
                    provider_metadata: None,
                    input: json!({ "city": "NYC" }),
                },
            ]
        );
    }

    #[test]
    fn empty_args_parse_to_empty_object() {
        let mut ts = ToolStream::new();
        ts.start("c0", "now");
        let end = ts.end("c0").unwrap();
        assert_eq!(
            end.last().unwrap(),
            &LlmEvent::ToolCall {
                id: "c0".into(),
                name: "now".into(),
                namespace: None,
                provider_metadata: None,
                input: json!({}),
            }
        );
    }

    #[test]
    fn malformed_json_is_a_decode_error() {
        let mut ts = ToolStream::new();
        ts.delta("c0", Some("x"), "{not json");
        let err = ts.end("c0").unwrap_err();
        assert_eq!(err.reason, LlmErrorReason::Decode);
    }

    #[test]
    fn end_unknown_or_double_end_is_noop() {
        let mut ts = ToolStream::new();
        assert_eq!(ts.end("missing").unwrap(), Vec::new());
        ts.start("c0", "t");
        assert!(!ts.end("c0").unwrap().is_empty());
        assert_eq!(ts.end("c0").unwrap(), Vec::new());
    }
}
