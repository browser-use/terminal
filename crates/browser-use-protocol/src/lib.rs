use serde::{Deserialize, Serialize};
use serde_json::Value;

const SESSION_PENDING_ACTIVE_FOLLOWUP_EVENT: &str = "session.followup.pending";

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct SessionMeta {
    pub id: String,
    pub parent_id: Option<String>,
    pub cwd: String,
    pub artifact_root: String,
    pub status: SessionStatus,
    pub created_ms: i64,
    pub updated_ms: i64,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SessionStatus {
    Created,
    Running,
    Done,
    Failed,
    Cancelled,
}

impl SessionStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Created => "created",
            Self::Running => "running",
            Self::Done => "done",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }

    pub fn is_active(&self) -> bool {
        matches!(self, Self::Created | Self::Running)
    }
}

impl std::str::FromStr for SessionStatus {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "created" => Ok(Self::Created),
            "running" => Ok(Self::Running),
            "done" => Ok(Self::Done),
            "failed" => Ok(Self::Failed),
            "cancelled" => Ok(Self::Cancelled),
            other => Err(format!("unknown session status: {other}")),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct EventRecord {
    pub seq: i64,
    pub id: String,
    pub session_id: String,
    pub ts_ms: i64,
    #[serde(rename = "type")]
    pub event_type: String,
    pub payload: Value,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct ArtifactMeta {
    pub id: String,
    pub session_id: String,
    pub event_seq: Option<i64>,
    pub kind: String,
    pub path: String,
    pub mime: Option<String>,
    pub bytes: Option<i64>,
    pub created_ms: i64,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct ToolSpec {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub namespace: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub namespace_description: Option<String>,
    pub description: String,
    pub input_schema: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_schema: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub freeform: Option<FreeformToolFormat>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct FreeformToolFormat {
    #[serde(rename = "type")]
    pub kind: String,
    pub syntax: String,
    pub definition: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub namespace: Option<String>,
    pub arguments: Value,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct ToolImage {
    pub label: Option<String>,
    pub path: String,
    pub mime_type: String,
    pub detail: String,
    pub order: i64,
    pub ts_ms: i64,
    pub url: Option<String>,
    pub title: Option<String>,
    pub viewport: Option<Value>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct ToolResult {
    pub tool_call_id: String,
    pub ok: bool,
    pub text: String,
    pub data: Value,
    pub images: Vec<ToolImage>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
pub struct ModelUsage {
    pub input_tokens: Option<i64>,
    pub input_cached_tokens: Option<i64>,
    pub input_cache_creation_tokens: Option<i64>,
    pub output_tokens: Option<i64>,
    pub reasoning_output_tokens: Option<i64>,
    pub total_tokens: Option<i64>,
    pub input_cost_usd: Option<f64>,
    pub input_cached_cost_usd: Option<f64>,
    pub input_cache_creation_cost_usd: Option<f64>,
    pub output_cost_usd: Option<f64>,
    pub cost_usd: Option<f64>,
    pub cost_source: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
pub struct CreditsSnapshot {
    pub has_credits: bool,
    pub unlimited: bool,
    pub balance: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct RateLimitWindow {
    pub used_percent: f64,
    pub window_minutes: Option<i64>,
    pub resets_at: Option<i64>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
pub struct RateLimitSnapshot {
    pub limit_id: Option<String>,
    pub limit_name: Option<String>,
    pub primary: Option<RateLimitWindow>,
    pub secondary: Option<RateLimitWindow>,
    pub credits: Option<CreditsSnapshot>,
    pub plan_type: Option<String>,
    pub rate_limit_reached_type: Option<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelVerification {
    TrustedAccessForCyber,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ModelEvent {
    TextDelta {
        text: String,
    },
    ThinkingDelta {
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        label: Option<String>,
    },
    ToolCall {
        call: ToolCall,
    },
    CustomToolCallInputDelta {
        call_id: String,
        name: String,
        delta: String,
    },
    Usage {
        usage: ModelUsage,
    },
    ResponseOutputItem {
        item: Value,
    },
    ResponseCompleted {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        response_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        end_turn: Option<bool>,
    },
    ServerModel {
        model: String,
    },
    ModelRateLimits {
        snapshot: RateLimitSnapshot,
    },
    ModelVerifications {
        verifications: Vec<ModelVerification>,
    },
    ModelsEtag {
        etag: String,
    },
    ServerReasoningIncluded {
        included: bool,
    },
    Done,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BrowserSummary {
    pub backend: String,
    pub status: String,
    pub title: Option<String>,
    pub url: Option<String>,
    pub live_url: Option<String>,
    pub tabs: Option<i64>,
    pub viewport: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TelemetrySummary {
    pub trace_id: Option<String>,
    pub backend: Option<String>,
    pub endpoint: Option<String>,
    pub failure: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HistoryRow {
    pub session_id: String,
    pub task: String,
    pub status: SessionStatus,
    pub updated_ms: i64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkbenchState {
    pub setup_complete: bool,
    pub current_session: Option<SessionMeta>,
    pub task: Option<String>,
    pub result: Option<String>,
    pub failure: Option<String>,
    pub activity: Vec<String>,
    pub transcript: Vec<TranscriptTurn>,
    pub browser: BrowserSummary,
    pub telemetry: TelemetrySummary,
    pub history: Vec<HistoryRow>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TranscriptTurn {
    pub prompt: String,
    pub is_followup: bool,
    pub activity: Vec<String>,
    pub thinking_text: Option<String>,
    pub streaming_text: Option<String>,
    pub result: Option<String>,
    pub failure: Option<String>,
}

pub fn task_from_events(events: &[EventRecord]) -> Option<String> {
    events.iter().find_map(|event| {
        if event.event_type == "session.input" {
            event
                .payload
                .get("text")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|text| !text.is_empty())
                .map(ToOwned::to_owned)
        } else {
            None
        }
    })
}

pub fn startup_warnings_from_events(events: &[EventRecord]) -> Vec<String> {
    events
        .iter()
        .filter(|event| event.event_type == "session.startup_warning")
        .filter_map(|event| {
            event
                .payload
                .get("message")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|message| !message.is_empty())
                .map(ToOwned::to_owned)
        })
        .collect()
}

pub fn instruction_sources_from_events(events: &[EventRecord]) -> Vec<String> {
    let mut sources = Vec::new();
    for event in events {
        if !matches!(
            event.event_type.as_str(),
            "session.instruction_sources" | "workspace.context"
        ) {
            continue;
        }
        if event.event_type == "workspace.context"
            && event.payload.get("kind").and_then(Value::as_str) != Some("agents_md")
        {
            continue;
        }
        let Some(items) = event.payload.get("sources").and_then(Value::as_array) else {
            continue;
        };
        for item in items {
            let Some(source) = item
                .as_str()
                .map(str::trim)
                .filter(|source| !source.is_empty())
            else {
                continue;
            };
            if !sources.iter().any(|existing| existing == source) {
                sources.push(source.to_string());
            }
        }
    }
    sources
}

pub fn transcript_from_events(events: &[EventRecord]) -> Vec<TranscriptTurn> {
    let mut starts = Vec::new();
    for (idx, event) in events.iter().enumerate() {
        let is_followup = match event.event_type.as_str() {
            "session.input" => false,
            "session.followup" => {
                if pending_after_next_tool_call_followup(events, event)
                    || after_next_tool_call_followup_is_cancelled(events, event.seq)
                {
                    continue;
                }
                true
            }
            _ => continue,
        };
        let Some(prompt) = event
            .payload
            .get("text")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|text| !text.is_empty())
        else {
            continue;
        };
        starts.push((idx, prompt.to_string(), is_followup));
    }

    starts
        .iter()
        .enumerate()
        .map(|(turn_idx, (event_idx, prompt, is_followup))| {
            let next_idx = starts
                .get(turn_idx + 1)
                .map(|(idx, _, _)| *idx)
                .unwrap_or(events.len());
            let segment = events
                .get(event_idx.saturating_add(1)..next_idx)
                .unwrap_or_default();
            TranscriptTurn {
                prompt: prompt.clone(),
                is_followup: *is_followup,
                activity: activity_from_events(segment),
                thinking_text: turn_thinking_text_from_events(segment),
                streaming_text: turn_streaming_text_from_events(segment),
                result: turn_result_from_events(segment),
                failure: turn_failure_from_events(segment),
            }
        })
        .collect()
}

fn pending_after_next_tool_call_followup(events: &[EventRecord], followup: &EventRecord) -> bool {
    if !matches!(
        followup.event_type.as_str(),
        "session.followup" | SESSION_PENDING_ACTIVE_FOLLOWUP_EVENT
    ) {
        return false;
    }
    if !followup_delivery_is_after_next_tool_call(followup) {
        return false;
    }
    !events
        .iter()
        .any(|event| event_closes_after_next_tool_call_followup(event, followup.seq))
}

fn after_next_tool_call_followup_is_cancelled(events: &[EventRecord], followup_seq: i64) -> bool {
    events.iter().any(|event| {
        event.seq > followup_seq
            && event.event_type == "session.followup.cancelled"
            && followup_marker_seqs(event).contains(&followup_seq)
    })
}

fn followup_delivery_is_after_next_tool_call(event: &EventRecord) -> bool {
    event.payload.get("delivery").and_then(Value::as_str) == Some("after_next_tool_call")
}

fn event_closes_after_next_tool_call_followup(event: &EventRecord, followup_seq: i64) -> bool {
    if event.seq <= followup_seq {
        return false;
    }
    match event.event_type.as_str() {
        "agent.turn_queue_drained" => {
            let drained_session_messages = event
                .payload
                .get("session_messages")
                .and_then(Value::as_u64)
                .unwrap_or_default();
            let last_seq = event
                .payload
                .get("last_seq")
                .and_then(Value::as_i64)
                .unwrap_or_default();
            drained_session_messages > 0 && last_seq >= followup_seq
        }
        "session.followup.interrupt_sent" => followup_marker_seqs(event).contains(&followup_seq),
        "session.followup.cancelled" => followup_marker_seqs(event).contains(&followup_seq),
        _ => false,
    }
}

fn followup_marker_seqs(event: &EventRecord) -> Vec<i64> {
    if let Some(seq) = event
        .payload
        .get("followup_seq")
        .or_else(|| event.payload.get("seq"))
        .and_then(Value::as_i64)
    {
        return vec![seq];
    }
    event
        .payload
        .get("followup_seqs")
        .and_then(Value::as_array)
        .map(|seqs| seqs.iter().filter_map(Value::as_i64).collect::<Vec<_>>())
        .unwrap_or_default()
}

pub fn result_from_events(events: &[EventRecord]) -> Option<String> {
    session_result_from_events(events).or_else(|| {
        events
            .iter()
            .rev()
            .find_map(|event| match event.event_type.as_str() {
                "agent.completed" => event
                    .payload
                    .get("payload")
                    .and_then(|payload| payload.get("result"))
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|result| !result.is_empty())
                    .map(normalize_result_text),
                _ => None,
            })
    })
}

pub fn session_result_from_events(events: &[EventRecord]) -> Option<String> {
    events
        .iter()
        .rev()
        .find_map(|event| match event.event_type.as_str() {
            "session.done" => session_done_result_text(&event.payload),
            _ => None,
        })
}

fn session_done_result_text(payload: &Value) -> Option<String> {
    if payload.get("result_file").is_some() {
        let file = payload
            .get("result_file_url")
            .and_then(Value::as_str)
            .or_else(|| payload.get("result_file_path").and_then(Value::as_str))
            .or_else(|| payload.get("result_file").and_then(Value::as_str))
            .unwrap_or("<unknown>");
        let directory = payload
            .get("result_file_directory_url")
            .and_then(Value::as_str)
            .or_else(|| payload.get("result_file_directory").and_then(Value::as_str));
        let bytes = payload.get("result_file_bytes").and_then(Value::as_u64);
        let mut text = format!("Saved result file.\n\nFile:\n{file}");
        if let Some(directory) = directory {
            text.push_str(&format!("\n\nDirectory:\n{directory}"));
        }
        if let Some(bytes) = bytes {
            text.push_str(&format!("\n\nSize: {bytes} bytes"));
        }
        return Some(text);
    }
    payload
        .get("result")
        .and_then(Value::as_str)
        .map(normalize_result_text)
}

pub fn normalize_result_text(text: &str) -> String {
    let mut cleaned = text.trim_end().to_string();
    loop {
        let chars = cleaned.chars().collect::<Vec<_>>();
        let len = chars.len();
        let Some(repeated_len) = (24..=len / 2).rev().find(|candidate| {
            let start = len.saturating_sub(candidate * 2);
            chars[start..start + candidate] == chars[start + candidate..]
        }) else {
            break;
        };
        let keep = len.saturating_sub(repeated_len);
        cleaned = chars[..keep]
            .iter()
            .collect::<String>()
            .trim_end()
            .to_string();
    }
    cleaned
}

pub fn helper_failure_from_events(events: &[EventRecord]) -> Option<String> {
    events
        .iter()
        .rev()
        .find_map(|event| match event.event_type.as_str() {
            "agent.failed" => event
                .payload
                .get("payload")
                .and_then(|payload| payload.get("error"))
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|error| !error.is_empty())
                .map(ToOwned::to_owned),
            "agent.cancelled" => event
                .payload
                .get("payload")
                .and_then(|payload| payload.get("reason"))
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|reason| !reason.is_empty())
                .map(ToOwned::to_owned),
            _ => None,
        })
}

pub fn turn_failure_from_events(events: &[EventRecord]) -> Option<String> {
    failure_from_events(events).or_else(|| helper_failure_from_events(events))
}

pub fn turn_result_from_events(events: &[EventRecord]) -> Option<String> {
    result_from_events(events)
}

pub fn turn_streaming_text_from_events(events: &[EventRecord]) -> Option<String> {
    let mut text = String::new();
    for event in events {
        match event.event_type.as_str() {
            "model.turn.request" | "model.turn.retry" | "model.turn.error" => {
                text.clear();
            }
            "model.stream_delta" => {
                if let Some(incoming) = event
                    .payload
                    .get("text")
                    .or_else(|| event.payload.get("delta"))
                    .and_then(Value::as_str)
                {
                    append_streaming_text_delta(&mut text, incoming);
                }
            }
            _ => {}
        }
    }
    (!text.trim().is_empty()).then_some(text)
}

pub fn turn_thinking_text_from_events(events: &[EventRecord]) -> Option<String> {
    let mut text = String::new();
    for event in events {
        match event.event_type.as_str() {
            "model.turn.request" | "model.turn.retry" | "model.turn.error" => {
                text.clear();
            }
            "model.thinking_delta" => {
                if let Some(incoming) = event
                    .payload
                    .get("text")
                    .or_else(|| event.payload.get("delta"))
                    .and_then(Value::as_str)
                {
                    append_streaming_text_delta(&mut text, incoming);
                }
            }
            _ => {}
        }
    }
    (!text.trim().is_empty()).then_some(text)
}

fn append_streaming_text_delta(current: &mut String, incoming: &str) {
    if incoming.is_empty() {
        return;
    }
    if current.is_empty() {
        current.push_str(incoming);
        return;
    }
    if incoming == current || incoming.trim() == current.trim() {
        return;
    }
    if let Some(suffix) = incoming.strip_prefix(current.as_str()) {
        current.push_str(suffix);
        return;
    }
    if incoming.chars().count() >= 24 && current.ends_with(incoming) {
        return;
    }
    current.push_str(incoming);
}

pub fn failure_from_events(events: &[EventRecord]) -> Option<String> {
    events.iter().rev().find_map(|event| {
        if event.event_type == "session.failed" {
            event
                .payload
                .get("error")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned)
        } else {
            None
        }
    })
}

fn sanitize_terminal_text(value: &str) -> String {
    value.chars().filter(|ch| !ch.is_control()).collect()
}

pub fn browser_summary_from_events(
    events: &[EventRecord],
    backend: impl Into<String>,
) -> BrowserSummary {
    let backend = backend.into();
    let mut summary = BrowserSummary {
        backend,
        status: "not connected".to_string(),
        title: None,
        url: None,
        live_url: None,
        tabs: None,
        viewport: None,
    };
    for event in events {
        match event.event_type.as_str() {
            "browser.connected" | "browser.reconnected" | "browser.target_changed" => {
                summary.status = "connected".to_string();
                if event.payload.get("live_url").is_some() {
                    summary.live_url = event
                        .payload
                        .get("live_url")
                        .and_then(Value::as_str)
                        .filter(|value| !value.trim().is_empty())
                        .map(sanitize_terminal_text);
                }
                if let Some(url) = event.payload.get("url").and_then(Value::as_str) {
                    summary.url = Some(sanitize_terminal_text(url));
                }
                if let Some(title) = event.payload.get("title").and_then(Value::as_str) {
                    summary.title = Some(sanitize_terminal_text(title));
                }
            }
            "browser.disconnected" => {
                summary.status = "disconnected".to_string();
                summary.live_url = None;
            }
            "browser.backend_changed" => {
                summary.status = "not connected".to_string();
                summary.title = None;
                summary.url = None;
                summary.live_url = None;
                summary.tabs = None;
                summary.viewport = None;
            }
            "browser.live_url" => {
                summary.status = "connected".to_string();
                summary.live_url = event
                    .payload
                    .get("live_url")
                    .or_else(|| event.payload.get("url"))
                    .and_then(Value::as_str)
                    .map(sanitize_terminal_text);
            }
            "browser.page" | "browser.state" => {
                if let Some(status) = event.payload.get("status").and_then(Value::as_str) {
                    summary.status = status.to_string();
                }
                if let Some(url) = event.payload.get("url").and_then(Value::as_str) {
                    summary.url = Some(sanitize_terminal_text(url));
                    summary.status = "connected".to_string();
                }
                if let Some(title) = event.payload.get("title").and_then(Value::as_str) {
                    summary.title = Some(sanitize_terminal_text(title));
                }
                if let Some(tabs) = event
                    .payload
                    .get("tabs")
                    .or_else(|| event.payload.get("tab_count"))
                    .and_then(Value::as_i64)
                {
                    summary.tabs = Some(tabs);
                }
                if let Some(viewport) = viewport_label_from_payload(&event.payload) {
                    summary.viewport = Some(viewport);
                }
            }
            _ => {}
        }
    }
    if !browser_backend_supports_live_url(&summary.backend) {
        summary.live_url = None;
    }
    summary
}

fn browser_backend_supports_live_url(backend: &str) -> bool {
    let backend = backend.to_ascii_lowercase();
    backend.contains("cloud")
        || backend.contains("managed")
        || backend.contains("headless chromium")
        || backend.contains("managed chromium")
}

pub fn telemetry_summary_from_events(events: &[EventRecord]) -> TelemetrySummary {
    let mut summary = TelemetrySummary::default();
    for event in events {
        match event.event_type.as_str() {
            "telemetry.trace" => {
                summary.trace_id = event
                    .payload
                    .get("trace_id")
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned);
                summary.backend = event
                    .payload
                    .get("backend")
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned);
                summary.endpoint = event
                    .payload
                    .get("endpoint")
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned);
                summary.failure = None;
            }
            "telemetry.failed" => {
                summary.failure = event
                    .payload
                    .get("error")
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned)
                    .or_else(|| Some("Laminar exporter setup failed".to_string()));
                summary.trace_id = None;
                summary.backend = None;
                summary.endpoint = None;
            }
            _ => {}
        }
    }
    summary
}

fn viewport_label_from_payload(payload: &Value) -> Option<String> {
    if let Some(label) = payload.get("viewport").and_then(Value::as_str) {
        return (!label.trim().is_empty()).then(|| label.trim().to_string());
    }
    let viewport = payload.get("viewport").unwrap_or(payload);
    let width = viewport
        .get("w")
        .or_else(|| viewport.get("width"))
        .and_then(Value::as_i64)?;
    let height = viewport
        .get("h")
        .or_else(|| viewport.get("height"))
        .and_then(Value::as_i64)?;
    Some(format!("{width} x {height}"))
}

pub fn activity_from_events(events: &[EventRecord]) -> Vec<String> {
    let mut activity = Vec::new();
    for (idx, event) in events.iter().enumerate() {
        match event.event_type.as_str() {
            "browser.connected" => push_activity(&mut activity, "browser connected"),
            "browser.reconnected" => push_activity(&mut activity, "browser reconnected"),
            "browser.target_changed" => push_activity(&mut activity, "browser target changed"),
            "browser.disconnected" => push_activity(&mut activity, "browser disconnected"),
            "browser.live_url" => push_activity(&mut activity, "connected live browser"),
            "browser.page" | "browser.state" => {
                if let Some(url) = event.payload.get("url").and_then(Value::as_str) {
                    push_activity(&mut activity, format!("browsing {}", compact_url(url)));
                }
            }
            "model.turn.request" => {}
            "model.turn.retry" => {
                push_activity(&mut activity, model_turn_retry_activity(&event.payload));
            }
            "model.turn.error" => {
                push_activity(&mut activity, model_turn_error_activity(&event.payload));
            }
            "plan.updated" => push_activity(&mut activity, "updated plan"),
            "command.started" => {
                if let Some(cmd) = event.payload.get("cmd").and_then(Value::as_str) {
                    push_activity(&mut activity, format!("ran {}", truncate_activity(cmd, 72)));
                } else {
                    push_activity(&mut activity, "ran command");
                }
            }
            "command.finished" => {
                if event
                    .payload
                    .get("success")
                    .and_then(Value::as_bool)
                    .is_some_and(|success| !success)
                {
                    let code = event
                        .payload
                        .get("exit_code")
                        .and_then(Value::as_i64)
                        .map(|code| code.to_string())
                        .unwrap_or_else(|| "unknown".to_string());
                    push_activity(&mut activity, format!("command failed with exit {code}"));
                }
            }
            "patch.file_changed" => {
                let kind = event
                    .payload
                    .get("kind")
                    .and_then(Value::as_str)
                    .unwrap_or("changed");
                let path = event
                    .payload
                    .get("path")
                    .and_then(Value::as_str)
                    .map(compact_path)
                    .unwrap_or_else(|| "file".to_string());
                push_activity(&mut activity, format!("{kind} {path}"));
            }
            "file.read" => {
                if let Some(path) = event.payload.get("path").and_then(Value::as_str) {
                    push_activity(&mut activity, format!("read {}", compact_path(path)));
                }
            }
            "file.search" => {
                let query = event
                    .payload
                    .get("query")
                    .and_then(Value::as_str)
                    .unwrap_or("files");
                let matches = event
                    .payload
                    .get("matches")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                push_activity(
                    &mut activity,
                    format!("searched {query:?} ({matches} matches)"),
                );
            }
            "file.list" => push_activity(&mut activity, "listed files"),
            "collab_agent_spawn_end" => {
                push_activity(&mut activity, collab_agent_spawn_text(&event.payload))
            }
            "collab_agent_interaction_end" => {
                push_activity(&mut activity, collab_agent_interaction_text(&event.payload))
            }
            "collab_waiting_begin" => {
                push_activity(&mut activity, collab_wait_started_text(&event.payload))
            }
            "collab_waiting_end" => {
                if !has_later_event(events, idx, &event.session_id, "agent.wait.finished") {
                    push_activity(&mut activity, "subagent wait finished");
                }
            }
            "collab_close_end" => push_activity(
                &mut activity,
                collab_agent_status_text(&event.payload, "stopped"),
            ),
            "collab_resume_end" => push_activity(
                &mut activity,
                collab_agent_status_text(&event.payload, "resumed"),
            ),
            "agent.spawned" => push_activity(&mut activity, agent_started_text(&event.payload)),
            "agent.message" => push_activity(&mut activity, agent_message_text(&event.payload)),
            "agent.wait.started" => {
                push_activity(&mut activity, agent_wait_started_text(&event.payload))
            }
            "agent.wait.finished" => {
                if event
                    .payload
                    .get("timed_out")
                    .and_then(Value::as_bool)
                    .unwrap_or(false)
                {
                    push_activity(&mut activity, "subagent wait timed out");
                } else {
                    push_activity(&mut activity, "subagent wait finished");
                }
            }
            "agent.completed" => push_activity(&mut activity, "subagent finished"),
            "agent.failed" => push_activity(&mut activity, "subagent failed"),
            "agent.cancelled" => push_activity(&mut activity, "subagent stopped"),
            "agent.resumed" => {
                if !is_replay_materialization_event(event) {
                    push_activity(&mut activity, "subagent resumed");
                }
            }
            _ => {}
        }
    }
    activity
}

fn has_later_event(
    events: &[EventRecord],
    current_idx: usize,
    session_id: &str,
    event_type: &str,
) -> bool {
    events[current_idx.saturating_add(1)..]
        .iter()
        .any(|event| event.session_id == session_id && event.event_type == event_type)
}

fn is_replay_materialization_event(event: &EventRecord) -> bool {
    event
        .payload
        .get("materialized_from_replay")
        .or_else(|| {
            event
                .payload
                .get("payload")
                .and_then(Value::as_object)
                .and_then(|payload| payload.get("materialized_from_replay"))
        })
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

fn push_activity(activity: &mut Vec<String>, item: impl Into<String>) {
    let item = item.into();
    if activity.last().is_some_and(|last| last == &item) {
        return;
    }
    activity.push(item);
}

fn model_turn_retry_activity(payload: &Value) -> String {
    let attempt = payload.get("attempt").and_then(Value::as_u64);
    let max_retries = payload.get("max_retries").and_then(Value::as_u64);
    match (attempt, max_retries) {
        (Some(attempt), Some(max_retries)) => {
            format!("thinking retrying model request {attempt}/{max_retries}")
        }
        _ => "thinking retrying model request".to_string(),
    }
}

fn model_turn_error_activity(payload: &Value) -> String {
    let transient = payload
        .get("transient")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if transient {
        "thinking model request hit a transient error".to_string()
    } else {
        "thinking model request failed".to_string()
    }
}

fn compact_path(path: &str) -> String {
    let trimmed = path.trim();
    trimmed
        .rsplit_once('/')
        .map(|(_, tail)| tail)
        .filter(|tail| !tail.is_empty())
        .unwrap_or(trimmed)
        .to_string()
}

fn truncate_activity(value: &str, max: usize) -> String {
    if value.chars().count() <= max {
        return value.to_string();
    }
    let mut out = value
        .chars()
        .take(max.saturating_sub(3))
        .collect::<String>();
    out.push_str("...");
    out
}

pub fn sanitized_agent_context_from_events(events: &[EventRecord]) -> Value {
    let browser = browser_summary_from_events(events, "parent browser");
    let mut activity = activity_from_events(events);
    if activity.len() > 8 {
        activity = activity.split_off(activity.len() - 8);
    }
    serde_json::json!({
        "task": task_from_events(events),
        "result": session_result_from_events(events),
        "failure": failure_from_events(events),
        "browser": {
            "status": browser.status,
            "title": browser.title,
            "url": browser.url,
            "live_url": browser.live_url,
        },
        "activity": activity,
        "recent_errors": recent_error_context(events),
        "recent_artifacts": recent_artifact_context(events),
        "final_answer": final_answer_context(events),
    })
}

fn recent_error_context(events: &[EventRecord]) -> Vec<Value> {
    let mut errors = events
        .iter()
        .rev()
        .filter_map(|event| match event.event_type.as_str() {
            "tool.failed" => Some(serde_json::json!({
                "type": "tool.failed",
                "name": event.payload.get("name").and_then(Value::as_str),
                "tool_call_id": event.payload.get("tool_call_id").and_then(Value::as_str),
                "error": event.payload.get("error").and_then(Value::as_str).map(truncate_context_field),
            })),
            "model.turn.error" => Some(serde_json::json!({
                "type": "model.turn.error",
                "provider": event.payload.get("provider").and_then(Value::as_str),
                "model": event.payload.get("model").and_then(Value::as_str),
                "transient": event.payload.get("transient").and_then(Value::as_bool),
                "error": event.payload.get("error").and_then(Value::as_str).map(truncate_context_field),
            })),
            "model.turn.context_overflow" => Some(serde_json::json!({
                "type": "model.turn.context_overflow",
                "provider": event.payload.get("provider").and_then(Value::as_str),
                "model": event.payload.get("model").and_then(Value::as_str),
                "action": event.payload.get("action").and_then(Value::as_str),
            })),
            "browser.cloud_shutdown_failed" | "telemetry.failed" => Some(serde_json::json!({
                "type": event.event_type.as_str(),
                "error": event.payload.get("error").and_then(Value::as_str).map(truncate_context_field),
            })),
            "session.deadline_warning" => Some(serde_json::json!({
                "type": "session.deadline_warning",
                "reason": event.payload.get("reason").and_then(Value::as_str),
                "remaining_turns": event.payload.get("remaining_turns").and_then(Value::as_u64),
            })),
            _ => None,
        })
        .take(5)
        .collect::<Vec<_>>();
    errors.reverse();
    errors
}

fn recent_artifact_context(events: &[EventRecord]) -> Vec<Value> {
    let mut artifacts = events
        .iter()
        .rev()
        .filter_map(|event| match event.event_type.as_str() {
            "artifact.created" => event.payload.get("artifact").map(|artifact| {
                serde_json::json!({
                    "type": "artifact",
                    "kind": artifact.get("kind").and_then(Value::as_str),
                    "path": artifact.get("path").and_then(Value::as_str),
                    "mime": artifact.get("mime").and_then(Value::as_str),
                    "bytes": artifact.get("bytes").and_then(Value::as_i64),
                })
            }),
            "tool.output_spilled" => event.payload.get("artifact").map(|artifact| {
                serde_json::json!({
                    "type": "tool-output",
                    "tool_name": artifact.get("tool_name").and_then(Value::as_str),
                    "tool_call_id": artifact.get("tool_call_id").and_then(Value::as_str),
                    "path": artifact.get("path").and_then(Value::as_str),
                    "original_tokens_estimate": artifact.get("original_tokens_estimate").and_then(Value::as_u64),
                })
            }),
            "tool.image" => event.payload.get("image").map(|image| {
                serde_json::json!({
                    "type": "image",
                    "path": image.get("path").and_then(Value::as_str),
                    "label": image.get("label").and_then(Value::as_str),
                    "mime": image
                        .get("mime_type")
                        .or_else(|| image.get("mime"))
                        .and_then(Value::as_str),
                    "bytes": image.get("bytes").and_then(Value::as_i64),
                })
            }),
            _ => None,
        })
        .take(8)
        .collect::<Vec<_>>();
    artifacts.reverse();
    artifacts
}

fn final_answer_context(events: &[EventRecord]) -> Option<Value> {
    events.iter().rev().find_map(|event| {
        if event.event_type == "session.final_answer_ready" {
            return event.payload.get("final_answer").cloned();
        }
        if event.event_type != "tool.output" {
            return None;
        }
        event.payload.get("data")?.get("final_answer").cloned()
    })
}

fn truncate_context_field(value: &str) -> String {
    const MAX_CONTEXT_FIELD_CHARS: usize = 500;
    if value.chars().count() <= MAX_CONTEXT_FIELD_CHARS {
        return value.to_string();
    }
    let mut out = value
        .chars()
        .take(MAX_CONTEXT_FIELD_CHARS.saturating_sub(15))
        .collect::<String>();
    out.push_str("...[truncated]");
    out
}

fn compact_url(url: &str) -> String {
    url.trim()
        .strip_prefix("https://")
        .or_else(|| url.trim().strip_prefix("http://"))
        .unwrap_or_else(|| url.trim())
        .trim_end_matches('/')
        .to_string()
}

fn agent_started_text(payload: &Value) -> String {
    let label = payload
        .get("nickname")
        .and_then(Value::as_str)
        .or_else(|| payload.get("role").and_then(Value::as_str))
        .unwrap_or("subagent");
    format!("subagent {label} started")
}

fn agent_message_text(payload: &Value) -> String {
    let label = payload
        .get("recipient_path")
        .and_then(Value::as_str)
        .or_else(|| payload.get("agent_path").and_then(Value::as_str))
        .map(short_agent_path)
        .unwrap_or_else(|| "subagent".to_string());
    if payload
        .get("trigger_turn")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        format!("subagent {label} sent task")
    } else {
        format!("subagent {label} messaged")
    }
}

fn agent_wait_started_text(payload: &Value) -> String {
    let label = payload
        .get("target")
        .and_then(Value::as_str)
        .map(short_agent_path)
        .or_else(|| {
            payload
                .get("targets")
                .and_then(Value::as_array)
                .map(|targets| {
                    if targets.len() == 1 {
                        targets
                            .first()
                            .and_then(|target| {
                                target
                                    .get("nickname")
                                    .and_then(Value::as_str)
                                    .or_else(|| target.get("task_name").and_then(Value::as_str))
                            })
                            .map(short_agent_path)
                            .unwrap_or_else(|| "subagent".to_string())
                    } else {
                        format!("{} subagents", targets.len())
                    }
                })
        })
        .unwrap_or_else(|| "subagents".to_string());
    format!("subagent waiting on {label}")
}

fn collab_agent_spawn_text(payload: &Value) -> String {
    let label = collab_agent_label(
        payload,
        "new_agent_nickname",
        "new_agent_role",
        "new_thread_id",
    );
    format!("subagent {label} started")
}

fn collab_agent_interaction_text(payload: &Value) -> String {
    let label = collab_agent_label(
        payload,
        "receiver_agent_nickname",
        "receiver_agent_role",
        "receiver_thread_id",
    );
    format!("subagent {label} messaged")
}

fn collab_agent_status_text(payload: &Value, status: &str) -> String {
    let label = collab_agent_label(
        payload,
        "receiver_agent_nickname",
        "receiver_agent_role",
        "receiver_thread_id",
    );
    format!("subagent {label} {status}")
}

fn collab_wait_started_text(payload: &Value) -> String {
    let label = payload
        .get("receiver_agents")
        .and_then(Value::as_array)
        .map(|agents| match agents.as_slice() {
            [agent] => collab_agent_label(agent, "agent_nickname", "agent_role", "thread_id"),
            [] => "subagents".to_string(),
            _ => format!("{} subagents", agents.len()),
        })
        .unwrap_or_else(|| "subagents".to_string());
    format!("subagent waiting on {label}")
}

fn collab_agent_label(
    payload: &Value,
    nickname_key: &str,
    role_key: &str,
    thread_id_key: &str,
) -> String {
    payload
        .get(nickname_key)
        .and_then(Value::as_str)
        .or_else(|| payload.get(role_key).and_then(Value::as_str))
        .or_else(|| payload.get(thread_id_key).and_then(Value::as_str))
        .map(short_agent_path)
        .unwrap_or_else(|| "subagent".to_string())
}

fn short_agent_path(path: &str) -> String {
    path.trim()
        .trim_matches('/')
        .rsplit('/')
        .next()
        .filter(|segment| !segment.is_empty() && *segment != "root")
        .unwrap_or(path)
        .to_string()
}

fn activity_with_child_agents(
    sessions: &[SessionMeta],
    events_for_current: &[EventRecord],
    all_events: &[(String, Vec<EventRecord>)],
    selected_session_id: Option<&str>,
) -> Vec<String> {
    let mut activity = activity_from_events(events_for_current);
    let Some(parent_id) = selected_session_id else {
        return activity;
    };
    let mut child_activity = Vec::new();
    for child in sessions
        .iter()
        .filter(|session| is_descendant_session(sessions, &session.id, parent_id))
    {
        let child_events = all_events
            .iter()
            .find(|(id, _)| id == &child.id)
            .map(|(_, events)| events.as_slice())
            .unwrap_or_default();
        child_activity.extend(child_agent_activity(child, child_events));
    }
    if !child_activity.is_empty() {
        activity.retain(|item| {
            !matches!(
                item.as_str(),
                "subagent finished" | "subagent failed" | "subagent stopped"
            )
        });
    }
    activity.extend(child_activity);
    activity
}

fn is_descendant_session(sessions: &[SessionMeta], session_id: &str, parent_id: &str) -> bool {
    let mut cursor = sessions
        .iter()
        .find(|session| session.id == session_id)
        .and_then(|session| session.parent_id.as_deref());
    while let Some(current_parent_id) = cursor {
        if current_parent_id == parent_id {
            return true;
        }
        cursor = sessions
            .iter()
            .find(|session| session.id == current_parent_id)
            .and_then(|session| session.parent_id.as_deref());
    }
    false
}

fn child_agent_activity(child: &SessionMeta, child_events: &[EventRecord]) -> Vec<String> {
    let label = child_agent_label(child, child_events);
    let mut activity = Vec::new();
    match child.status {
        SessionStatus::Created | SessionStatus::Running => {
            push_activity(&mut activity, format!("subagent {label} working"));
        }
        SessionStatus::Done => push_activity(&mut activity, format!("subagent {label} finished")),
        SessionStatus::Failed => push_activity(&mut activity, format!("subagent {label} failed")),
        SessionStatus::Cancelled => {
            push_activity(&mut activity, format!("subagent {label} stopped"))
        }
    }

    let mut recent = activity_from_events(child_events)
        .into_iter()
        .filter(|item| !item.starts_with("started "))
        .collect::<Vec<_>>();
    if recent.len() > 6 {
        recent = recent.split_off(recent.len() - 6);
    }
    for item in recent {
        push_activity(
            &mut activity,
            format!("subagent {label}: {}", child_agent_activity_text(&item)),
        );
    }
    if let Some(streaming_text) = turn_streaming_text_from_events(child_events) {
        push_activity(
            &mut activity,
            format!(
                "subagent {label}: streaming {}",
                truncate_activity(streaming_text.trim(), 96)
            ),
        );
    }
    activity
}

fn child_agent_label(child: &SessionMeta, child_events: &[EventRecord]) -> String {
    child_events
        .iter()
        .find(|event| event.event_type == "agent.context")
        .and_then(|event| {
            event
                .payload
                .get("nickname")
                .and_then(Value::as_str)
                .or_else(|| event.payload.get("role").and_then(Value::as_str))
                .or_else(|| event.payload.get("agent_path").and_then(Value::as_str))
        })
        .map(str::trim)
        .filter(|label| !label.is_empty())
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| compact_path(&child.id))
}

fn child_agent_activity_text(item: &str) -> String {
    item.strip_prefix("thinking ").unwrap_or(item).to_string()
}

pub fn project_workbench(
    sessions: &[SessionMeta],
    events_for_current: &[EventRecord],
    all_events: &[(String, Vec<EventRecord>)],
    selected_session_id: Option<&str>,
    browser_backend: impl Into<String>,
) -> WorkbenchState {
    let current_session = selected_session_id
        .and_then(|id| sessions.iter().find(|session| session.id == id))
        .cloned();

    let mut history = sessions
        .iter()
        .filter(|session| session.parent_id.is_none())
        .map(|session| {
            let events = all_events
                .iter()
                .find(|(id, _)| id == &session.id)
                .map(|(_, events)| events.as_slice())
                .unwrap_or_default();
            HistoryRow {
                session_id: session.id.clone(),
                task: task_from_events(events).unwrap_or_else(|| "untitled task".to_string()),
                status: session.status.clone(),
                updated_ms: session.updated_ms,
            }
        })
        .collect::<Vec<_>>();
    history.sort_by(|a, b| b.updated_ms.cmp(&a.updated_ms));

    let result = if current_session
        .as_ref()
        .is_some_and(|session| session.status == SessionStatus::Done)
    {
        session_result_from_events(events_for_current)
    } else {
        None
    };
    let failure = if current_session
        .as_ref()
        .is_some_and(|session| session.status == SessionStatus::Failed)
    {
        failure_from_events(events_for_current)
    } else {
        None
    };

    WorkbenchState {
        setup_complete: false,
        current_session,
        task: task_from_events(events_for_current),
        result,
        failure,
        activity: activity_with_child_agents(
            sessions,
            events_for_current,
            all_events,
            selected_session_id,
        ),
        transcript: transcript_from_events(events_for_current),
        browser: browser_summary_from_events(events_for_current, browser_backend),
        telemetry: telemetry_summary_from_events(events_for_current),
        history,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn event(seq: i64, event_type: &str, payload: Value) -> EventRecord {
        EventRecord {
            seq,
            id: format!("e{seq}"),
            session_id: "s1".to_string(),
            ts_ms: seq,
            event_type: event_type.to_string(),
            payload,
        }
    }

    #[test]
    fn browser_summary_strips_control_chars_from_untrusted_strings() {
        let events = vec![
            event(
                1,
                "browser.live_url",
                json!({ "live_url": "https://live.example.com/?wss=ws\x1b]0;pwned\x07x" }),
            ),
            event(
                2,
                "browser.page",
                json!({
                    "url": "https://evil.test/\x1b[2J",
                    "title": "hi\x1bthere\u{009c}",
                }),
            ),
        ];
        let summary = browser_summary_from_events(&events, "browser use cloud");
        assert_eq!(
            summary.live_url.as_deref(),
            Some("https://live.example.com/?wss=ws]0;pwnedx")
        );
        assert_eq!(summary.url.as_deref(), Some("https://evil.test/[2J"));
        assert_eq!(summary.title.as_deref(), Some("hithere"));
        // No control characters survive in any displayed field
        for field in [&summary.live_url, &summary.url, &summary.title] {
            if let Some(value) = field {
                assert!(!value.chars().any(|ch| ch.is_control()));
            }
        }
    }

    #[test]
    fn startup_warnings_and_instruction_sources_are_extracted_from_events() {
        let events = vec![
            event(
                1,
                "workspace.context",
                json!({
                    "kind": "agents_md",
                    "sources": ["/repo/AGENTS.md"],
                }),
            ),
            event(
                2,
                "session.instruction_sources",
                json!({
                    "source": "agents_md",
                    "sources": ["/repo/AGENTS.md", "/repo/crates/AGENTS.md"],
                }),
            ),
            event(
                3,
                "session.startup_warning",
                json!({
                    "source": "agents_md",
                    "message": "Project AGENTS.md contains invalid UTF-8",
                }),
            ),
        ];

        assert_eq!(
            instruction_sources_from_events(&events),
            vec![
                "/repo/AGENTS.md".to_string(),
                "/repo/crates/AGENTS.md".to_string()
            ]
        );
        assert_eq!(
            startup_warnings_from_events(&events),
            vec!["Project AGENTS.md contains invalid UTF-8".to_string()]
        );
    }

    #[test]
    fn projects_task_result_browser_and_activity_from_events() {
        let events = vec![
            EventRecord {
                seq: 1,
                id: "e1".to_string(),
                session_id: "s1".to_string(),
                ts_ms: 1,
                event_type: "session.input".to_string(),
                payload: json!({"text": "Find flights"}),
            },
            EventRecord {
                seq: 2,
                id: "e2".to_string(),
                session_id: "s1".to_string(),
                ts_ms: 2,
                event_type: "browser.page".to_string(),
                payload: json!({
                    "url": "https://example.com/",
                    "title": "Example",
                    "tabs": 2,
                    "viewport": {"w": 1440, "h": 900},
                }),
            },
            EventRecord {
                seq: 3,
                id: "e3".to_string(),
                session_id: "s1".to_string(),
                ts_ms: 3,
                event_type: "command.started".to_string(),
                payload: json!({"cmd": "cargo test -p browser-use-core"}),
            },
            EventRecord {
                seq: 4,
                id: "e4".to_string(),
                session_id: "s1".to_string(),
                ts_ms: 4,
                event_type: "patch.file_changed".to_string(),
                payload: json!({"kind": "modified", "path": "/repo/src/main.rs"}),
            },
            EventRecord {
                seq: 5,
                id: "e5".to_string(),
                session_id: "s1".to_string(),
                ts_ms: 5,
                event_type: "session.done".to_string(),
                payload: json!({"result": "Done"}),
            },
        ];
        assert_eq!(task_from_events(&events).as_deref(), Some("Find flights"));
        assert_eq!(result_from_events(&events).as_deref(), Some("Done"));
        let browser = browser_summary_from_events(&events, "local chrome");
        assert_eq!(browser.status, "connected");
        assert_eq!(browser.title.as_deref(), Some("Example"));
        assert_eq!(browser.tabs, Some(2));
        assert_eq!(browser.viewport.as_deref(), Some("1440 x 900"));
        assert_eq!(
            activity_from_events(&events),
            vec![
                "browsing example.com",
                "ran cargo test -p browser-use-core",
                "modified main.rs",
            ]
        );
    }

    #[test]
    fn browser_live_url_projection_accepts_current_and_legacy_payload_keys() {
        let current = vec![event(
            1,
            "browser.live_url",
            json!({"live_url": "https://live.browser-use.com/watch"}),
        )];
        let browser = browser_summary_from_events(&current, "browser use cloud");
        assert_eq!(browser.status, "connected");
        assert_eq!(
            browser.live_url.as_deref(),
            Some("https://live.browser-use.com/watch")
        );

        let legacy = vec![event(
            1,
            "browser.live_url",
            json!({"url": "https://live.browser-use.com/legacy"}),
        )];
        let browser = browser_summary_from_events(&legacy, "browser use cloud");
        assert_eq!(
            browser.live_url.as_deref(),
            Some("https://live.browser-use.com/legacy")
        );
    }

    #[test]
    fn browser_live_url_projection_accepts_managed_chromium_backends() {
        let events = vec![event(
            1,
            "browser.live_url",
            json!({"live_url": "https://chrome-devtools-frontend.appspot.com/serve_rev/@rev/inspector.html?ws=127.0.0.1:9222/devtools/page/abc"}),
        )];

        for backend in ["managed", "Managed Chromium", "Headless Chromium"] {
            let browser = browser_summary_from_events(&events, backend);
            assert_eq!(
                browser.live_url.as_deref(),
                Some("https://chrome-devtools-frontend.appspot.com/serve_rev/@rev/inspector.html?ws=127.0.0.1:9222/devtools/page/abc"),
                "backend {backend} should keep local DevTools live URL"
            );
        }
    }

    #[test]
    fn browser_live_url_projection_keeps_local_devtools_after_done() {
        let events = vec![
            event(
                1,
                "browser.live_url",
                json!({"live_url": "https://chrome-devtools-frontend.appspot.com/serve_rev/@rev/inspector.html?ws=127.0.0.1:9222/devtools/page/abc"}),
            ),
            event(2, "session.done", json!({"result": "Done"})),
        ];

        let browser = browser_summary_from_events(&events, "Headless Chromium");
        assert_eq!(
            browser.live_url.as_deref(),
            Some("https://chrome-devtools-frontend.appspot.com/serve_rev/@rev/inspector.html?ws=127.0.0.1:9222/devtools/page/abc")
        );
    }

    #[test]
    fn browser_live_url_projection_keeps_cloud_live_url_after_done() {
        let events = vec![
            event(
                1,
                "browser.live_url",
                json!({"live_url": "https://live.browser-use.com/watch"}),
            ),
            event(2, "session.done", json!({"result": "Done"})),
        ];

        let browser = browser_summary_from_events(&events, "Browser Use Cloud");
        assert_eq!(
            browser.live_url.as_deref(),
            Some("https://live.browser-use.com/watch")
        );
    }

    #[test]
    fn browser_backend_change_clears_stale_cloud_live_url() {
        let events = vec![
            event(
                1,
                "browser.live_url",
                json!({"live_url": "https://live.browser-use.com/watch"}),
            ),
            event(
                2,
                "browser.backend_changed",
                json!({"previous_browser": "Browser Use cloud", "browser": "Local Chrome"}),
            ),
        ];
        let browser = browser_summary_from_events(&events, "Local Chrome");
        assert_eq!(browser.status, "not connected");
        assert_eq!(browser.live_url, None);
        assert_eq!(browser.url, None);
    }

    #[test]
    fn browser_state_with_null_live_url_clears_stale_cloud_live_url() {
        let events = vec![
            event(
                1,
                "browser.live_url",
                json!({"live_url": "https://live.browser-use.com/watch-stale"}),
            ),
            event(
                2,
                "browser.target_changed",
                json!({
                    "backend": "remote-cloud",
                    "status": "connected",
                    "target_id": "target-2",
                    "session_id": "session-2",
                    "live_url": null,
                }),
            ),
        ];
        let browser = browser_summary_from_events(&events, "Browser Use Cloud");
        assert_eq!(browser.status, "connected");
        assert_eq!(browser.live_url, None);
    }

    #[test]
    fn projects_child_agent_activity_into_parent_workbench() {
        let sessions = vec![
            SessionMeta {
                id: "parent".to_string(),
                parent_id: None,
                cwd: "/repo".to_string(),
                artifact_root: "/tmp/parent".to_string(),
                status: SessionStatus::Running,
                created_ms: 1,
                updated_ms: 4,
            },
            SessionMeta {
                id: "child".to_string(),
                parent_id: Some("parent".to_string()),
                cwd: "/repo".to_string(),
                artifact_root: "/tmp/child".to_string(),
                status: SessionStatus::Running,
                created_ms: 2,
                updated_ms: 4,
            },
        ];
        let parent_events = vec![
            EventRecord {
                seq: 1,
                id: "e1".to_string(),
                session_id: "parent".to_string(),
                ts_ms: 1,
                event_type: "session.input".to_string(),
                payload: json!({"text": "explain the repo"}),
            },
            EventRecord {
                seq: 2,
                id: "e2".to_string(),
                session_id: "parent".to_string(),
                ts_ms: 2,
                event_type: "agent.spawned".to_string(),
                payload: json!({"child_session_id": "child", "nickname": "repo-explorer", "role": "explorer"}),
            },
        ];
        let child_events = vec![
            EventRecord {
                seq: 3,
                id: "e3".to_string(),
                session_id: "child".to_string(),
                ts_ms: 3,
                event_type: "agent.context".to_string(),
                payload: json!({"nickname": "repo-explorer", "role": "explorer"}),
            },
            EventRecord {
                seq: 4,
                id: "e4".to_string(),
                session_id: "child".to_string(),
                ts_ms: 4,
                event_type: "file.read".to_string(),
                payload: json!({"path": "/repo/README.md"}),
            },
            EventRecord {
                seq: 5,
                id: "e5".to_string(),
                session_id: "child".to_string(),
                ts_ms: 5,
                event_type: "model.stream_delta".to_string(),
                payload: json!({"text": "I am mapping the crates now."}),
            },
        ];
        let state = project_workbench(
            &sessions,
            &parent_events,
            &[
                ("parent".to_string(), parent_events.clone()),
                ("child".to_string(), child_events),
            ],
            Some("parent"),
            "local chrome",
        );
        assert!(state
            .activity
            .contains(&"subagent repo-explorer started".to_string()));
        assert!(state
            .activity
            .contains(&"subagent repo-explorer working".to_string()));
        assert!(state
            .activity
            .contains(&"subagent repo-explorer: read README.md".to_string()));
        assert!(state
            .activity
            .iter()
            .any(|item| item.contains("mapping the crates")));
    }

    #[test]
    fn result_projection_dedupes_repeated_full_text_delta_artifacts() {
        let answer = "Your callback has been scheduled.\n\nThey will call you tomorrow.";
        let events = vec![EventRecord {
            seq: 1,
            id: "e1".to_string(),
            session_id: "s1".to_string(),
            ts_ms: 1,
            event_type: "session.done".to_string(),
            payload: json!({"result": format!("{answer}{answer}")}),
        }];
        assert_eq!(result_from_events(&events).as_deref(), Some(answer));
    }

    #[test]
    fn session_result_ignores_subagent_completion_after_done() {
        let events = vec![
            EventRecord {
                seq: 1,
                id: "e1".to_string(),
                session_id: "parent".to_string(),
                ts_ms: 1,
                event_type: "session.done".to_string(),
                payload: json!({"result": "parent answer"}),
            },
            EventRecord {
                seq: 2,
                id: "e2".to_string(),
                session_id: "parent".to_string(),
                ts_ms: 2,
                event_type: "agent.completed".to_string(),
                payload: json!({
                    "child_session_id": "child",
                    "status": "done",
                    "payload": {"result": "child answer"},
                }),
            },
        ];

        assert_eq!(
            session_result_from_events(&events).as_deref(),
            Some("parent answer")
        );
        assert_eq!(
            result_from_events(&events).as_deref(),
            Some("parent answer")
        );

        let context = sanitized_agent_context_from_events(&events);
        assert_eq!(context["result"], "parent answer");
    }

    #[test]
    fn result_projection_uses_pointer_for_done_result_file() {
        let events = vec![EventRecord {
            seq: 1,
            id: "e1".to_string(),
            session_id: "s1".to_string(),
            ts_ms: 1,
            event_type: "session.done".to_string(),
            payload: json!({
                "source": "done.result_file",
                "result_file": "answer.json",
                "result_file_url": "file:///tmp/but/answer.json",
                "result_file_directory_url": "file:///tmp/but",
                "result_file_bytes": 123,
                "result": "SHOULD_NOT_RENDER ".repeat(100),
            }),
        }];
        let result = result_from_events(&events).expect("result");

        assert!(result.contains("Saved result file."));
        assert!(result.contains("file:///tmp/but/answer.json"));
        assert!(result.contains("file:///tmp/but"));
        assert!(result.contains("123 bytes"));
        assert!(!result.contains("SHOULD_NOT_RENDER"));
    }

    #[test]
    fn projects_browser_identity_events() {
        let events = vec![
            EventRecord {
                seq: 1,
                id: "e1".to_string(),
                session_id: "s1".to_string(),
                ts_ms: 1,
                event_type: "browser.connected".to_string(),
                payload: json!({
                    "target_id": "target-1",
                    "session_id": "session-1",
                    "url": "https://example.com/one",
                    "title": "One",
                }),
            },
            EventRecord {
                seq: 2,
                id: "e2".to_string(),
                session_id: "s1".to_string(),
                ts_ms: 2,
                event_type: "browser.reconnected".to_string(),
                payload: json!({
                    "target_id": "target-1",
                    "session_id": "session-2",
                    "previous_session_id": "session-1",
                    "url": "https://example.com/two",
                    "title": "Two",
                    "stale_object_ids": true,
                }),
            },
            EventRecord {
                seq: 3,
                id: "e3".to_string(),
                session_id: "s1".to_string(),
                ts_ms: 3,
                event_type: "browser.target_changed".to_string(),
                payload: json!({
                    "target_id": "target-2",
                    "previous_target_id": "target-1",
                    "session_id": "session-3",
                    "url": "https://example.com/three",
                    "title": "Three",
                    "stale_object_ids": true,
                }),
            },
        ];
        let browser = browser_summary_from_events(&events, "browser use cloud");
        assert_eq!(browser.status, "connected");
        assert_eq!(browser.url.as_deref(), Some("https://example.com/three"));
        assert_eq!(browser.title.as_deref(), Some("Three"));
        assert_eq!(
            activity_from_events(&events),
            vec![
                "browser connected",
                "browser reconnected",
                "browser target changed",
            ]
        );
    }

    #[test]
    fn ignores_internal_python_tool_start_activity() {
        let events = vec![EventRecord {
            seq: 1,
            id: "e1".to_string(),
            session_id: "s1".to_string(),
            ts_ms: 1,
            event_type: "tool.started".to_string(),
            payload: json!({"name": "python", "call_id": "call-1"}),
        }];
        assert!(activity_from_events(&events).is_empty());
    }

    #[test]
    fn hides_model_waits_but_keeps_retries_as_activity() {
        let events = vec![
            EventRecord {
                seq: 1,
                id: "e1".to_string(),
                session_id: "s1".to_string(),
                ts_ms: 1,
                event_type: "model.turn.request".to_string(),
                payload: json!({"model": "GPT-5.5", "provider": "codex"}),
            },
            EventRecord {
                seq: 2,
                id: "e2".to_string(),
                session_id: "s1".to_string(),
                ts_ms: 2,
                event_type: "model.turn.error".to_string(),
                payload: json!({"transient": true, "error": "stream disconnected"}),
            },
            EventRecord {
                seq: 3,
                id: "e3".to_string(),
                session_id: "s1".to_string(),
                ts_ms: 3,
                event_type: "model.turn.retry".to_string(),
                payload: json!({"attempt": 1, "max_retries": 5}),
            },
        ];

        assert_eq!(
            activity_from_events(&events),
            vec![
                "thinking model request hit a transient error",
                "thinking retrying model request 1/5",
            ]
        );
    }

    #[test]
    fn projects_streaming_text_for_running_turn_and_resets_failed_attempts() {
        let events = vec![
            EventRecord {
                seq: 1,
                id: "e1".to_string(),
                session_id: "s1".to_string(),
                ts_ms: 1,
                event_type: "session.input".to_string(),
                payload: json!({"text": "stream please"}),
            },
            EventRecord {
                seq: 2,
                id: "e2".to_string(),
                session_id: "s1".to_string(),
                ts_ms: 2,
                event_type: "model.turn.request".to_string(),
                payload: json!({"model": "GPT-5.5"}),
            },
            EventRecord {
                seq: 3,
                id: "e3".to_string(),
                session_id: "s1".to_string(),
                ts_ms: 3,
                event_type: "model.stream_delta".to_string(),
                payload: json!({"text": "stale"}),
            },
            EventRecord {
                seq: 4,
                id: "e4".to_string(),
                session_id: "s1".to_string(),
                ts_ms: 4,
                event_type: "model.turn.error".to_string(),
                payload: json!({"transient": true}),
            },
            EventRecord {
                seq: 5,
                id: "e5".to_string(),
                session_id: "s1".to_string(),
                ts_ms: 5,
                event_type: "model.turn.retry".to_string(),
                payload: json!({"attempt": 1, "max_retries": 5}),
            },
            EventRecord {
                seq: 6,
                id: "e6".to_string(),
                session_id: "s1".to_string(),
                ts_ms: 6,
                event_type: "model.thinking_delta".to_string(),
                payload: json!({"text": "checking "}),
            },
            EventRecord {
                seq: 7,
                id: "e7".to_string(),
                session_id: "s1".to_string(),
                ts_ms: 7,
                event_type: "model.thinking_delta".to_string(),
                payload: json!({"text": "checking context"}),
            },
            EventRecord {
                seq: 8,
                id: "e8".to_string(),
                session_id: "s1".to_string(),
                ts_ms: 8,
                event_type: "model.stream_delta".to_string(),
                payload: json!({"text": "fresh "}),
            },
            EventRecord {
                seq: 9,
                id: "e9".to_string(),
                session_id: "s1".to_string(),
                ts_ms: 9,
                event_type: "model.stream_delta".to_string(),
                payload: json!({"text": "fresh answer"}),
            },
        ];

        let transcript = transcript_from_events(&events);
        assert_eq!(transcript.len(), 1);
        assert_eq!(
            transcript[0].thinking_text.as_deref(),
            Some("checking context")
        );
        assert_eq!(
            transcript[0].streaming_text.as_deref(),
            Some("fresh answer")
        );
    }

    #[test]
    fn pending_after_next_tool_call_followup_does_not_split_live_turn() {
        let events = vec![
            event(1, "session.input", json!({"text": "run"})),
            event(2, "model.turn.request", json!({"model": "GPT-5.5"})),
            event(3, "model.stream_delta", json!({"text": "still running"})),
            event(
                4,
                "session.followup",
                json!({"text": "steer after tool", "delivery": "after_next_tool_call"}),
            ),
        ];

        let transcript = transcript_from_events(&events);
        assert_eq!(transcript.len(), 1);
        assert_eq!(transcript[0].prompt, "run");
        assert_eq!(
            transcript[0].streaming_text.as_deref(),
            Some("still running")
        );

        let mut drained = events;
        drained.push(event(
            5,
            "agent.turn_queue_drained",
            json!({
                "phase": "after_tool_outputs",
                "session_messages": 1,
                "last_seq": 4,
            }),
        ));
        let transcript = transcript_from_events(&drained);
        assert_eq!(transcript.len(), 2);
        assert_eq!(transcript[1].prompt, "steer after tool");
        assert!(transcript[1].is_followup);

        let cancelled = vec![
            event(1, "session.input", json!({"text": "run"})),
            event(
                2,
                "session.followup",
                json!({"text": "edit this", "delivery": "after_next_tool_call"}),
            ),
            event(
                3,
                "session.followup.cancelled",
                json!({"followup_seq": 2, "reason": "reclaimed from escape"}),
            ),
        ];
        let transcript = transcript_from_events(&cancelled);
        assert_eq!(transcript.len(), 1);
        assert_eq!(transcript[0].prompt, "run");
    }

    #[test]
    fn pending_active_followup_event_waits_for_committed_followup_boundary() {
        let events = vec![
            event(1, "session.input", json!({"text": "run"})),
            event(2, "model.stream_delta", json!({"text": "still running"})),
            event(
                3,
                SESSION_PENDING_ACTIVE_FOLLOWUP_EVENT,
                json!({"text": "steer after tool", "delivery": "after_next_tool_call"}),
            ),
            event(4, "file.read", json!({"path": "README.md"})),
            event(
                5,
                "session.followup",
                json!({"text": "steer after tool", "pending_from_seq": 3}),
            ),
            event(
                6,
                "agent.turn_queue_drained",
                json!({
                    "phase": "after_tool_outputs",
                    "session_messages": 1,
                    "last_seq": 5,
                }),
            ),
            event(7, "model.stream_delta", json!({"text": "new answer"})),
        ];

        let transcript = transcript_from_events(&events);
        assert_eq!(transcript.len(), 2);
        assert_eq!(transcript[0].prompt, "run");
        assert_eq!(
            transcript[0].streaming_text.as_deref(),
            Some("still running")
        );
        assert_eq!(transcript[1].prompt, "steer after tool");
        assert!(transcript[1].is_followup);
        assert_eq!(transcript[1].streaming_text.as_deref(), Some("new answer"));
    }

    #[test]
    fn collapses_consecutive_duplicate_activity_pings() {
        let events = vec![
            EventRecord {
                seq: 1,
                id: "e1".to_string(),
                session_id: "s1".to_string(),
                ts_ms: 1,
                event_type: "browser.state".to_string(),
                payload: json!({"url": "https://example.com/dashboard"}),
            },
            EventRecord {
                seq: 2,
                id: "e2".to_string(),
                session_id: "s1".to_string(),
                ts_ms: 2,
                event_type: "browser.state".to_string(),
                payload: json!({"url": "https://example.com/dashboard"}),
            },
            EventRecord {
                seq: 3,
                id: "e3".to_string(),
                session_id: "s1".to_string(),
                ts_ms: 3,
                event_type: "browser.state".to_string(),
                payload: json!({"url": "https://example.com/settings"}),
            },
            EventRecord {
                seq: 4,
                id: "e4".to_string(),
                session_id: "s1".to_string(),
                ts_ms: 4,
                event_type: "browser.state".to_string(),
                payload: json!({"url": "https://example.com/dashboard"}),
            },
        ];
        assert_eq!(
            activity_from_events(&events),
            vec![
                "browsing example.com/dashboard",
                "browsing example.com/settings",
                "browsing example.com/dashboard",
            ]
        );
    }

    #[test]
    fn projects_latest_telemetry_state_from_events() {
        let events = vec![
            EventRecord {
                seq: 1,
                id: "e1".to_string(),
                session_id: "s1".to_string(),
                ts_ms: 1,
                event_type: "telemetry.failed".to_string(),
                payload: json!({"error": "bad endpoint"}),
            },
            EventRecord {
                seq: 2,
                id: "e2".to_string(),
                session_id: "s1".to_string(),
                ts_ms: 2,
                event_type: "telemetry.trace".to_string(),
                payload: json!({
                    "trace_id": "abc123",
                    "backend": "laminar",
                    "endpoint": "https://api.lmnr.ai/v1/traces",
                }),
            },
        ];
        let telemetry = telemetry_summary_from_events(&events);
        assert_eq!(telemetry.trace_id.as_deref(), Some("abc123"));
        assert_eq!(telemetry.backend.as_deref(), Some("laminar"));
        assert_eq!(
            telemetry.endpoint.as_deref(),
            Some("https://api.lmnr.ai/v1/traces")
        );
        assert!(telemetry.failure.is_none());
    }

    #[test]
    fn does_not_select_history_without_an_explicit_current_session() {
        let sessions = vec![SessionMeta {
            id: "s1".to_string(),
            parent_id: None,
            cwd: "/tmp".to_string(),
            artifact_root: "/tmp/artifacts/s1".to_string(),
            status: SessionStatus::Done,
            created_ms: 1,
            updated_ms: 2,
        }];
        let state = project_workbench(&sessions, &[], &[], None, "local chrome");
        assert!(state.current_session.is_none());
        assert_eq!(state.history.len(), 1);
    }

    #[test]
    fn history_contains_only_root_tasks() {
        let sessions = vec![
            SessionMeta {
                id: "parent".to_string(),
                parent_id: None,
                cwd: "/tmp".to_string(),
                artifact_root: "/tmp/artifacts/parent".to_string(),
                status: SessionStatus::Done,
                created_ms: 1,
                updated_ms: 2,
            },
            SessionMeta {
                id: "child".to_string(),
                parent_id: Some("parent".to_string()),
                cwd: "/tmp".to_string(),
                artifact_root: "/tmp/artifacts/child".to_string(),
                status: SessionStatus::Done,
                created_ms: 3,
                updated_ms: 4,
            },
        ];
        let state = project_workbench(&sessions, &[], &[], None, "local chrome");
        assert_eq!(state.history.len(), 1);
        assert_eq!(state.history[0].session_id, "parent");
    }

    #[test]
    fn projects_agent_events_as_compact_activity() {
        let events = vec![
            EventRecord {
                seq: 1,
                id: "e1".to_string(),
                session_id: "s1".to_string(),
                ts_ms: 1,
                event_type: "agent.spawned".to_string(),
                payload: json!({"child_session_id": "c1", "nickname": "checkout"}),
            },
            EventRecord {
                seq: 2,
                id: "e2".to_string(),
                session_id: "s1".to_string(),
                ts_ms: 2,
                event_type: "agent.completed".to_string(),
                payload: json!({
                    "child_session_id": "c1",
                    "status": "done",
                    "payload": {"result": "checkout flow documented"},
                }),
            },
        ];
        assert_eq!(
            activity_from_events(&events),
            vec!["subagent checkout started", "subagent finished"]
        );
        assert_eq!(
            result_from_events(&events).as_deref(),
            Some("checkout flow documented"),
        );
    }

    #[test]
    fn projects_agent_wait_events_as_compact_activity() {
        let events = vec![
            EventRecord {
                seq: 1,
                id: "e1".to_string(),
                session_id: "s1".to_string(),
                ts_ms: 1,
                event_type: "agent.wait.started".to_string(),
                payload: json!({
                    "target": "/root/repo_explorer",
                    "targets": [{
                        "child_session_id": "c1",
                        "task_name": "/root/repo_explorer",
                        "nickname": "repo_explorer",
                    }],
                }),
            },
            EventRecord {
                seq: 2,
                id: "e2".to_string(),
                session_id: "s1".to_string(),
                ts_ms: 2,
                event_type: "agent.wait.finished".to_string(),
                payload: json!({"timed_out": false}),
            },
        ];
        assert_eq!(
            activity_from_events(&events),
            vec![
                "subagent waiting on repo_explorer",
                "subagent wait finished"
            ]
        );
    }

    #[test]
    fn projects_collab_agent_events_as_activity() {
        let events = vec![
            event(
                1,
                "collab_agent_spawn_end",
                json!({
                    "new_thread_id": "c1",
                    "new_agent_nickname": "builder",
                    "status": "running",
                }),
            ),
            event(
                2,
                "collab_agent_interaction_end",
                json!({
                    "receiver_thread_id": "c1",
                    "receiver_agent_nickname": "builder",
                    "status": "running",
                }),
            ),
            event(
                3,
                "collab_waiting_begin",
                json!({
                    "receiver_agents": [{
                        "thread_id": "c1",
                        "agent_nickname": "builder"
                    }],
                }),
            ),
            event(4, "collab_waiting_end", json!({"statuses": {}})),
            event(
                5,
                "collab_close_end",
                json!({
                    "receiver_thread_id": "c1",
                    "receiver_agent_nickname": "builder",
                    "status": "running",
                }),
            ),
            event(
                6,
                "collab_resume_end",
                json!({
                    "receiver_thread_id": "c1",
                    "receiver_agent_nickname": "builder",
                    "status": "running",
                }),
            ),
        ];

        assert_eq!(
            activity_from_events(&events),
            vec![
                "subagent builder started",
                "subagent builder messaged",
                "subagent waiting on builder",
                "subagent wait finished",
                "subagent builder stopped",
                "subagent builder resumed",
            ]
        );
    }

    #[test]
    fn sanitized_agent_context_omits_raw_tool_output() {
        let events = vec![
            EventRecord {
                seq: 1,
                id: "e1".to_string(),
                session_id: "s1".to_string(),
                ts_ms: 1,
                event_type: "session.input".to_string(),
                payload: json!({"text": "Book a hotel"}),
            },
            EventRecord {
                seq: 2,
                id: "e2".to_string(),
                session_id: "s1".to_string(),
                ts_ms: 2,
                event_type: "tool.output".to_string(),
                payload: json!({"text": "raw page dump with token SECRET"}),
            },
            EventRecord {
                seq: 3,
                id: "e3".to_string(),
                session_id: "s1".to_string(),
                ts_ms: 3,
                event_type: "browser.state".to_string(),
                payload: json!({"url": "https://example.com", "title": "Example"}),
            },
            EventRecord {
                seq: 4,
                id: "e4".to_string(),
                session_id: "s1".to_string(),
                ts_ms: 4,
                event_type: "tool.failed".to_string(),
                payload: json!({
                    "name": "read_file",
                    "tool_call_id": "read_missing",
                    "error": "read missing.txt: No such file or directory",
                }),
            },
            EventRecord {
                seq: 5,
                id: "e5".to_string(),
                session_id: "s1".to_string(),
                ts_ms: 5,
                event_type: "artifact.created".to_string(),
                payload: json!({
                    "artifact": {
                        "kind": "file",
                        "path": "/tmp/report.csv",
                        "mime": "text/csv",
                        "bytes": 123,
                    },
                }),
            },
            EventRecord {
                seq: 6,
                id: "e6".to_string(),
                session_id: "s1".to_string(),
                ts_ms: 6,
                event_type: "tool.output".to_string(),
                payload: json!({
                    "name": "python",
                    "text": "final output bridge",
                    "data": {
                        "final_answer": {
                            "count": 3,
                            "artifact": "/tmp/final.json",
                        },
                    },
                }),
            },
        ];
        let context = sanitized_agent_context_from_events(&events);
        assert_eq!(context["task"], "Book a hotel");
        assert_eq!(context["browser"]["url"], "https://example.com");
        assert_eq!(context["recent_errors"][0]["name"], "read_file");
        assert_eq!(context["recent_artifacts"][0]["path"], "/tmp/report.csv");
        assert_eq!(context["final_answer"]["count"], 3);
        assert!(!context.to_string().contains("SECRET"));
    }

    #[test]
    fn workbench_uses_latest_projected_status_for_result_or_failure() {
        let session = SessionMeta {
            id: "s1".to_string(),
            parent_id: None,
            cwd: "/tmp".to_string(),
            artifact_root: "/tmp/artifacts/s1".to_string(),
            status: SessionStatus::Done,
            created_ms: 1,
            updated_ms: 4,
        };
        let events = vec![
            EventRecord {
                seq: 1,
                id: "e1".to_string(),
                session_id: "s1".to_string(),
                ts_ms: 1,
                event_type: "session.input".to_string(),
                payload: json!({"text": "retry task"}),
            },
            EventRecord {
                seq: 2,
                id: "e2".to_string(),
                session_id: "s1".to_string(),
                ts_ms: 2,
                event_type: "session.failed".to_string(),
                payload: json!({"error": "first failure"}),
            },
            EventRecord {
                seq: 3,
                id: "e3".to_string(),
                session_id: "s1".to_string(),
                ts_ms: 3,
                event_type: "session.done".to_string(),
                payload: json!({"result": "retry succeeded"}),
            },
        ];
        let state = project_workbench(
            &[session],
            &events,
            &[("s1".to_string(), events.clone())],
            Some("s1"),
            "local chrome",
        );
        assert_eq!(state.result.as_deref(), Some("retry succeeded"));
        assert!(state.failure.is_none());
    }

    #[test]
    fn model_event_json_round_trips() {
        let event = ModelEvent::ToolCall {
            call: ToolCall {
                id: "call_1".to_string(),
                name: "python".to_string(),
                namespace: None,
                arguments: json!({"code": "print(1)"}),
            },
        };
        let json = serde_json::to_value(&event).unwrap();
        assert_eq!(json["type"], "tool_call");
        let decoded: ModelEvent = serde_json::from_value(json).unwrap();
        assert_eq!(decoded, event);
    }
}
