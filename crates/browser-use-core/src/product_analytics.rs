use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result};
use browser_use_store::Store;
use serde_json::{json, Map, Value};
use uuid::Uuid;

const PROD_POSTHOG_KEY: &str = "phc_F8JMNjW1i2KbGUTaW1unnDdLSPCoyc52SGRU0JecaUh";
const DEV_POSTHOG_KEY: &str = "phc_zA2V4ziA7SjefWYGP4Gg9CCJj9r25rPiG5c926aKhGTG";
const DEFAULT_POSTHOG_HOST: &str = "https://eu.i.posthog.com";
const INSTALL_ID_RELATIVE_PATH: &[&str] = &["product_analytics", "install_id"];
const DEFAULT_TIMEOUT_MS: u64 = 800;
/// `message_kind` values for `bu:<surface> message sent` events.
pub const MESSAGE_KIND_INITIAL: &str = "initial";
pub const MESSAGE_KIND_FOLLOWUP: &str = "followup";
/// A user's answer to an agent's `request_user_input` prompt.
pub const MESSAGE_KIND_REQUEST_INPUT_RESPONSE: &str = "request_input_response";
/// `blocked_reason` value for a task submitted with no API key / no auth.
pub const BLOCKED_REASON_NO_AUTH: &str = "no_auth";
/// Fallback surface when a caller has no analytics source configured.
const DEFAULT_SURFACE: &str = "core";

pub fn capture_async(store: &Store, event: impl Into<String>, properties: Value) {
    if analytics_disabled() {
        return;
    }
    let state_dir = store.state_dir().to_path_buf();
    let event = event.into();
    thread::Builder::new()
        .name("browser-use-product-analytics".to_string())
        .spawn(move || {
            let _ = capture_for_state_dir(&state_dir, &event, properties);
        })
        .ok();
}

pub fn capture_blocking(store: &Store, event: &str, properties: Value) {
    if analytics_disabled() {
        return;
    }
    let _ = capture_for_state_dir(store.state_dir(), event, properties);
}

/// Capture a single user message — the initial task prompt or any follow-up — as
/// a `bu:<surface> message sent` event, so the full conversation log is queryable
/// in PostHog. Messages are grouped by `session_id`; `model`/`provider_kind` live
/// on the task-started event for the same session, so they are not duplicated here.
///
/// Text only: callers pass the typed message text, so pasted images and other
/// attachment content are never sent. Empty/whitespace-only messages are skipped.
/// Gated by the same telemetry toggle as every other event.
pub fn capture_user_message(
    store: &Store,
    surface: &str,
    session_id: &str,
    is_child_task: bool,
    message_kind: &str,
    message_seq: i64,
    text: &str,
) {
    if text.trim().is_empty() {
        return;
    }
    capture_async(
        store,
        format!("bu:{} message sent", normalized_surface(surface)),
        user_message_properties(
            surface,
            session_id,
            is_child_task,
            message_kind,
            message_seq,
            text,
            None,
        ),
    );
}

/// Capture an initial message that was blocked before the agent could run — e.g.
/// the user submitted a task with no API key / no auth, so it is stored but never
/// executed unless they later authenticate. Same event name and shape as
/// [`capture_user_message`], plus a `blocked_reason` property so these drop-offs
/// are queryable on their own.
pub fn capture_user_message_blocked(
    store: &Store,
    surface: &str,
    session_id: &str,
    is_child_task: bool,
    message_seq: i64,
    text: &str,
    blocked_reason: &str,
) {
    if text.trim().is_empty() {
        return;
    }
    capture_async(
        store,
        format!("bu:{} message sent", normalized_surface(surface)),
        user_message_properties(
            surface,
            session_id,
            is_child_task,
            MESSAGE_KIND_INITIAL,
            message_seq,
            text,
            Some(blocked_reason),
        ),
    );
}

fn normalized_surface(surface: &str) -> &str {
    let surface = surface.trim();
    if surface.is_empty() {
        DEFAULT_SURFACE
    } else {
        surface
    }
}

fn user_message_properties(
    surface: &str,
    session_id: &str,
    is_child_task: bool,
    message_kind: &str,
    message_seq: i64,
    text: &str,
    blocked_reason: Option<&str>,
) -> Value {
    let mut properties = json!({
        "surface": normalized_surface(surface),
        "session_id": session_id,
        "is_child_task": is_child_task,
        "message_kind": message_kind,
        "message_seq": message_seq,
        "text": text,
        "text_chars": text.chars().count() as i64,
    });
    if let Some(reason) = blocked_reason {
        properties["blocked_reason"] = Value::String(reason.to_string());
    }
    properties
}

fn capture_for_state_dir(state_dir: &Path, event: &str, properties: Value) -> Result<()> {
    let Some(api_key) = posthog_key() else {
        return Ok(());
    };
    let install_id = install_id(state_dir)?;
    let payload = json!({
        "api_key": api_key,
        "event": event,
        "distinct_id": install_id,
        "properties": event_properties(properties),
    });
    let endpoint = format!("{}/i/v0/e/", posthog_host().trim_end_matches('/'));
    reqwest::blocking::Client::builder()
        .timeout(Duration::from_millis(posthog_timeout_ms()))
        .build()
        .context("build PostHog analytics client")?
        .post(endpoint)
        .json(&payload)
        .send()
        .context("send PostHog analytics event")?
        .error_for_status()
        .context("PostHog analytics request failed")?;
    Ok(())
}

fn event_properties(properties: Value) -> Value {
    let mut object = match properties {
        Value::Object(object) => object,
        _ => Map::new(),
    };
    object.insert("$process_person_profile".to_string(), Value::Bool(false));
    object.insert(
        "analytics_env".to_string(),
        Value::String(analytics_env().to_string()),
    );
    object.insert(
        "app".to_string(),
        Value::String("browser-use-terminal".to_string()),
    );
    object.insert(
        "app_version".to_string(),
        Value::String(env!("CARGO_PKG_VERSION").to_string()),
    );
    object.insert(
        "os".to_string(),
        Value::String(std::env::consts::OS.to_string()),
    );
    object.insert(
        "arch".to_string(),
        Value::String(std::env::consts::ARCH.to_string()),
    );
    object.insert(
        "debug_build".to_string(),
        Value::Bool(cfg!(debug_assertions)),
    );
    Value::Object(object)
}

fn install_id(state_dir: &Path) -> Result<String> {
    let path = install_id_path(state_dir);
    if let Ok(value) = std::fs::read_to_string(&path) {
        let value = value.trim();
        if !value.is_empty() {
            return Ok(value.to_string());
        }
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create product analytics dir {}", parent.display()))?;
    }
    let id = format!("bu_{}", Uuid::new_v4().simple());
    std::fs::write(&path, format!("{id}\n"))
        .with_context(|| format!("write product analytics install id {}", path.display()))?;
    Ok(id)
}

fn install_id_path(state_dir: &Path) -> PathBuf {
    INSTALL_ID_RELATIVE_PATH
        .iter()
        .fold(state_dir.to_path_buf(), |path, segment| path.join(segment))
}

fn analytics_disabled() -> bool {
    cfg!(test) || env_flag_is_false("BUT_TELEMETRY") || env_flag_is_false("BUT_PRODUCT_ANALYTICS")
}

fn posthog_key() -> Option<String> {
    env_value("BUT_POSTHOG_KEY").or_else(|| match analytics_env().as_str() {
        "development" | "dev" => Some(DEV_POSTHOG_KEY.to_string()),
        "production" | "prod" => Some(PROD_POSTHOG_KEY.to_string()),
        _ if cfg!(debug_assertions) => Some(DEV_POSTHOG_KEY.to_string()),
        _ => Some(PROD_POSTHOG_KEY.to_string()),
    })
}

fn posthog_host() -> String {
    env_value("BUT_POSTHOG_HOST").unwrap_or_else(|| DEFAULT_POSTHOG_HOST.to_string())
}

fn analytics_env() -> String {
    env_value("BUT_ANALYTICS_ENV").unwrap_or_else(|| {
        if cfg!(debug_assertions) {
            "development".to_string()
        } else {
            "production".to_string()
        }
    })
}

fn posthog_timeout_ms() -> u64 {
    env_value("BUT_POSTHOG_TIMEOUT_MS")
        .and_then(|value| value.parse().ok())
        .filter(|value| *value > 0)
        .unwrap_or(DEFAULT_TIMEOUT_MS)
}

fn env_value(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn env_flag_is_false(name: &str) -> bool {
    matches!(
        env_value(name)
            .unwrap_or_default()
            .to_ascii_lowercase()
            .as_str(),
        "0" | "false" | "off" | "no"
    )
}

pub fn duration_bucket(duration: Duration) -> &'static str {
    let seconds = duration.as_secs();
    if seconds < 10 {
        "<10s"
    } else if seconds < 60 {
        "10-60s"
    } else if seconds < 300 {
        "1-5m"
    } else if seconds < 900 {
        "5-15m"
    } else {
        ">15m"
    }
}

pub fn browser_kind(mode: Option<&str>) -> &'static str {
    let Some(mode) = mode else {
        return "unknown";
    };
    let normalized = mode.to_ascii_lowercase().replace(['_', ' '], "-");
    match normalized.as_str() {
        "local" | "local-chrome" => "local",
        "headless" | "headless-chromium" | "managed-headless" => "headless",
        "managed" | "managed-headed" => "managed",
        "cloud" | "browser-use-cloud" => "cloud",
        _ => "other",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_properties_force_anonymous_capture() {
        let properties = event_properties(json!({"surface": "tui"}));
        assert_eq!(properties["$process_person_profile"], false);
        assert_eq!(properties["surface"], "tui");
        assert_eq!(properties["app"], "browser-use-terminal");
    }

    #[test]
    fn buckets_duration_without_exact_values() {
        assert_eq!(duration_bucket(Duration::from_secs(0)), "<10s");
        assert_eq!(duration_bucket(Duration::from_secs(10)), "10-60s");
        assert_eq!(duration_bucket(Duration::from_secs(60)), "1-5m");
        assert_eq!(duration_bucket(Duration::from_secs(300)), "5-15m");
        assert_eq!(duration_bucket(Duration::from_secs(900)), ">15m");
    }

    #[test]
    fn browser_kind_normalizes_modes() {
        assert_eq!(browser_kind(Some("Local Chrome")), "local");
        assert_eq!(browser_kind(Some("managed-headless")), "headless");
        assert_eq!(browser_kind(Some("cloud")), "cloud");
    }

    #[test]
    fn user_message_properties_capture_text_and_grouping_keys() {
        let properties = user_message_properties(
            "tui",
            "sess-123",
            false,
            MESSAGE_KIND_INITIAL,
            7,
            "book me a flight",
            None,
        );
        assert_eq!(properties["surface"], "tui");
        assert_eq!(properties["session_id"], "sess-123");
        assert_eq!(properties["message_kind"], "initial");
        assert_eq!(properties["message_seq"], 7);
        assert_eq!(properties["text"], "book me a flight");
        assert_eq!(properties["text_chars"], 16);
        assert_eq!(properties["is_child_task"], false);
        // No blocked_reason key on a normal message.
        assert!(properties.get("blocked_reason").is_none());
    }

    #[test]
    fn user_message_properties_default_surface_and_unicode_length() {
        let properties =
            user_message_properties("", "sess-9", true, MESSAGE_KIND_FOLLOWUP, 0, "café ☕", None);
        assert_eq!(properties["surface"], DEFAULT_SURFACE);
        assert_eq!(properties["is_child_task"], true);
        // char count, not byte length (café ☕ = 6 chars, 9 bytes)
        assert_eq!(properties["text_chars"], 6);
    }

    #[test]
    fn user_message_properties_record_blocked_reason() {
        let properties = user_message_properties(
            "tui",
            "sess-x",
            false,
            MESSAGE_KIND_INITIAL,
            0,
            "do a thing",
            Some(BLOCKED_REASON_NO_AUTH),
        );
        assert_eq!(properties["blocked_reason"], "no_auth");
        assert_eq!(properties["message_kind"], "initial");
        assert_eq!(properties["text"], "do a thing");
    }
}
