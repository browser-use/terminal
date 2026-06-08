//! AgentMail inbox access: provision the agent's disposable inbox, list its
//! messages, and read a message's full body. General-purpose — the agent reads
//! whatever it needs (verification codes, magic links, confirmations); no
//! special-casing of 2FA here.

use anyhow::{anyhow, bail, Result};
use serde_json::Value;

const BASE_URL: &str = "https://api.agentmail.to";
/// Stable client id so repeated provisioning returns the same inbox/address.
const INBOX_CLIENT_ID: &str = "browser-use-terminal";

/// A thin AgentMail REST client.
#[derive(Clone)]
pub struct AgentMail {
    token: String,
}

impl AgentMail {
    pub fn new(token: impl Into<String>) -> Self {
        Self {
            token: token.into(),
        }
    }

    /// Run a blocking reqwest call off-thread to avoid panicking inside a Tokio
    /// runtime (the bridge resolver may run on a worker thread).
    fn off_runtime<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
        std::thread::spawn(f)
            .join()
            .expect("agentmail http thread panicked")
    }

    /// Provision (idempotently) an inbox and return its email address.
    pub fn inbox_address(&self) -> Result<String> {
        let token = self.token.clone();
        Self::off_runtime(move || {
            let resp = reqwest::blocking::Client::new()
                .post(format!("{BASE_URL}/inboxes"))
                .bearer_auth(&token)
                .json(&serde_json::json!({ "client_id": INBOX_CLIENT_ID }))
                .send()
                .map_err(|err| anyhow!("AgentMail create-inbox request failed: {err}"))?;
            let status = resp.status();
            let body: Value = resp
                .json()
                .map_err(|err| anyhow!("AgentMail inbox response not JSON: {err}"))?;
            if !status.is_success() {
                bail!(
                    "AgentMail inbox error ({}): {}",
                    status.as_u16(),
                    body.get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("request rejected")
                );
            }
            // `inbox_id` is the email address (e.g. name@agentmail.to).
            body.get("inbox_id")
                .or_else(|| body.get("id"))
                .and_then(Value::as_str)
                .map(str::to_string)
                .ok_or_else(|| anyhow!("AgentMail response missing inbox_id"))
        })
    }

    /// List recent messages, newest first, as lightweight metadata. The list
    /// endpoint returns `subject` + `preview` but no full body — read a specific
    /// message with [`get_message`](Self::get_message) when the preview isn't
    /// enough.
    pub fn list_messages(&self, inbox_id: &str, limit: u32) -> Result<Vec<Value>> {
        let token = self.token.clone();
        let inbox = inbox_id.to_string();
        let limit = limit.clamp(1, 50).to_string();
        Self::off_runtime(move || {
            let resp = reqwest::blocking::Client::new()
                .get(format!("{BASE_URL}/inboxes/{inbox}/messages"))
                .bearer_auth(&token)
                .query(&[("limit", limit.as_str())])
                .send()
                .map_err(|err| anyhow!("AgentMail list-messages request failed: {err}"))?;
            let status = resp.status();
            let body: Value = resp
                .json()
                .map_err(|err| anyhow!("AgentMail messages response not JSON: {err}"))?;
            if !status.is_success() {
                bail!("AgentMail messages error ({})", status.as_u16());
            }
            let messages = body
                .get("messages")
                .and_then(Value::as_array)
                .map(|items| items.iter().map(summarize_message).collect())
                .unwrap_or_default();
            Ok(messages)
        })
    }

    /// Fetch one message's full content (subject, sender, text + html body). The
    /// message id contains characters like `<`, `>`, and `@`, so it must be
    /// percent-encoded as a path segment.
    pub fn get_message(&self, inbox_id: &str, message_id: &str) -> Result<Value> {
        let token = self.token.clone();
        let inbox = inbox_id.to_string();
        let message_id = message_id.to_string();
        Self::off_runtime(move || {
            let mut url = reqwest::Url::parse(&format!("{BASE_URL}/inboxes/{inbox}/messages"))
                .map_err(|err| anyhow!("AgentMail message URL invalid: {err}"))?;
            url.path_segments_mut()
                .map_err(|_| anyhow!("AgentMail message URL is not a base"))?
                .push(&message_id);
            let resp = reqwest::blocking::Client::new()
                .get(url)
                .bearer_auth(&token)
                .send()
                .map_err(|err| anyhow!("AgentMail get-message request failed: {err}"))?;
            let status = resp.status();
            let body: Value = resp
                .json()
                .map_err(|err| anyhow!("AgentMail message response not JSON: {err}"))?;
            if !status.is_success() {
                bail!("AgentMail message error ({})", status.as_u16());
            }
            Ok(full_message(&body))
        })
    }
}

/// Project a list-endpoint message down to the fields the agent reads (no body).
fn summarize_message(message: &Value) -> Value {
    let pick = |key: &str| message.get(key).and_then(Value::as_str).unwrap_or("");
    serde_json::json!({
        "message_id": pick("message_id"),
        "from": pick("from"),
        "to": message.get("to").cloned().unwrap_or(Value::Null),
        "subject": pick("subject"),
        "preview": pick("preview"),
        "timestamp": pick("timestamp"),
    })
}

/// Project a single-message response down to the fields the agent reads,
/// including the full text and html body (preferring AgentMail's cleaned
/// `extracted_*` variants).
fn full_message(message: &Value) -> Value {
    let pick = |key: &str| message.get(key).and_then(Value::as_str).unwrap_or("");
    let body = |keys: &[&str]| {
        keys.iter()
            .find_map(|key| message.get(*key).and_then(Value::as_str))
            .unwrap_or("")
            .to_string()
    };
    serde_json::json!({
        "message_id": pick("message_id"),
        "from": pick("from"),
        "to": message.get("to").cloned().unwrap_or(Value::Null),
        "subject": pick("subject"),
        "preview": pick("preview"),
        "timestamp": pick("timestamp"),
        "text": body(&["extracted_text", "text"]),
        "html": body(&["extracted_html", "html"]),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn summarize_keeps_preview_and_drops_body() {
        let raw = json!({
            "message_id": "<abc@x>",
            "from": "Mock <m@x>",
            "subject": "Your verification code",
            "preview": "Your verification code is 997545.",
            "timestamp": "2026-06-06T22:47:00.000Z",
            "text": "ignored by the list projection",
        });
        let s = summarize_message(&raw);
        assert_eq!(s["preview"], "Your verification code is 997545.");
        assert_eq!(s["subject"], "Your verification code");
        assert!(s.get("text").is_none());
    }

    #[test]
    fn full_message_prefers_extracted_body() {
        let raw = json!({
            "message_id": "<abc@x>",
            "subject": "Hi",
            "text": "raw body",
            "extracted_text": "clean body",
            "html": "<p>x</p>",
        });
        let f = full_message(&raw);
        assert_eq!(f["text"], "clean body");
        assert_eq!(f["html"], "<p>x</p>");
    }
}
