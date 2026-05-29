use std::collections::{HashMap, HashSet};
use std::fmt;
#[cfg(not(test))]
use std::io::Read;
use std::io::{self, Write};
#[cfg(not(test))]
use std::net::{TcpListener, TcpStream};
#[cfg(unix)]
use std::os::unix::process::CommandExt;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::process::Command as ProcessCommand;
#[cfg(not(test))]
use std::process::Stdio;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    mpsc, Arc, Once,
};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use browser_use_core::{
    cleanup_agent_runtime_state_for_agent_subtree, configured_model_for_cwd_with_options,
    configured_model_provider_id_for_cwd_with_options, default_model_for_cwd_with_options,
    install_process_crypto_provider, model_catalog_for_cwd_with_options, parse_config_overrides,
    product_analytics, typed_user_input_payload_from_items_for_cwd,
    typed_user_input_payload_from_text_for_cwd, AgentRunOptions, CollaborationModeKind,
    ConfigOverrides, MessageHistoryConfig, MessageHistoryPersistence, UnifiedExecShutdownCleanup,
};
use browser_use_protocol::{
    project_workbench, EventRecord, SessionMeta, SessionStatus, WorkbenchState,
};
use browser_use_providers::{
    claude_code_oauth_authorize_url, claude_code_oauth_pkce, load_codex_auth,
    load_codex_managed_auth, ClaudeCodeOAuthCredential, CodexAuth,
};
#[cfg(not(test))]
use browser_use_providers::{
    exchange_claude_code_authorization_code, load_codex_auth_file,
    parse_claude_code_authorization_input, ClaudeCodeAuthorization, CLAUDE_CODE_CALLBACK_HOST,
    CLAUDE_CODE_CALLBACK_PATH, CLAUDE_CODE_CALLBACK_PORT,
};
use browser_use_store::{resolve_state_dir, Store, StoreNotification, StoreNotifier};
use clap::{Parser, ValueEnum};
use crossterm::cursor::{MoveTo, Show};
use crossterm::event::{
    self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, Event as TermEvent,
    KeyCode, KeyEvent, KeyEventKind, KeyModifiers, KeyboardEnhancementFlags, MouseEvent,
    MouseEventKind, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use crossterm::execute;
use crossterm::queue;
use crossterm::style::{
    Attribute, Color as CrosstermColor, Print, ResetColor, SetAttribute, SetBackgroundColor,
    SetForegroundColor,
};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode, Clear, ClearType};
use crossterm::Command;
use ratatui::backend::CrosstermBackend;
use ratatui::buffer::Buffer;
use ratatui::layout::{Margin, Position, Rect};
use ratatui::style::{Color as RatatuiColor, Modifier};
use ratatui::text::Line;
use ratatui::widgets::{Clear as RatatuiClear, Paragraph, Widget};
use ratatui::{Terminal, TerminalOptions, Viewport};
use serde::{Deserialize, Serialize};
#[cfg(unix)]
use signal_hook::consts::signal::SIGUSR2;
use unicode_width::UnicodeWidthStr;

mod clipboard_paste;
mod composer;
mod markdown;
mod palette;
mod render;
mod runtime;
mod settings;
mod theme;
mod transcript;
mod welcome;

use composer::Composer;
use palette::PaletteAction;
use render::{
    lines_plain_text, main_viewport_height, native_scrollback_lines, render, render_dump,
    APP_HORIZONTAL_MARGIN, NATIVE_TRANSCRIPT_HORIZONTAL_MARGIN,
};
use runtime::run_agent_thread;
use settings::{
    browser_use_cloud_env_key_present, display_and_provider_model_for_input,
    display_model_for_provider_model, fallback_model_choices, is_claude_code_account,
    model_choices_for_catalog, provider_model_for_display, AgentBackend, ModelChoice,
    ACCOUNT_ANTHROPIC, ACCOUNT_CHOICES, ACCOUNT_CODEX, ACCOUNT_DEEPSEEK, ACCOUNT_OPENAI,
    ACCOUNT_OPENROUTER, BROWSER_CHOICES, BROWSER_LOCAL_CHROME, BROWSER_USE_CLOUD,
    BROWSER_USE_CLOUD_API_KEY_SETTING,
};

const DOUBLE_ESCAPE_STOP_WINDOW: Duration = Duration::from_millis(1500);
const STORE_FALLBACK_REFRESH_INTERVAL: Duration = Duration::from_millis(750);
const INPUT_POLL_INTERVAL: Duration = Duration::from_millis(25);
const RESIZE_DEBOUNCE_INTERVAL: Duration = Duration::from_millis(80);
const ANIM_TICK_INTERVAL: Duration = Duration::from_millis(16); // ~60 fps
const LIVE_SPINNER_TICK_INTERVAL: Duration = Duration::from_millis(120);
const REEXEC_BINARY_ENV: &str = "BUT_REEXEC_BINARY";
const CODEX_DEVICE_AUTH_URL: &str = "https://auth.openai.com/codex/device";
const COLLABORATION_MODE_SETTING: &str = "collaboration.mode";
const REQUEST_USER_INPUT_REQUEST_EVENT: &str = "request_user_input.requested";
const REQUEST_USER_INPUT_RESPONSE_EVENT: &str = "request_user_input.response";
const REQUEST_USER_INPUT_OTHER_LABEL: &str = "None of the above";
const SESSION_MODEL_SELECTION_EVENT: &str = "session.model_selection";
pub(crate) const SESSION_QUEUED_FOLLOWUP_EVENT: &str = "session.queued_followup";
const SESSION_QUEUED_FOLLOWUP_SENT_EVENT: &str = "session.queued_followup.sent";
const SESSION_QUEUED_FOLLOWUP_CANCELLED_EVENT: &str = "session.queued_followup.cancelled";
const SESSION_ROLLBACK_EVENT: &str = "session.rollback";

#[derive(Debug, Parser)]
#[command(name = "but", bin_name = "but")]
#[command(version)]
struct Args {
    #[arg(long, default_value = "~/.browser-use-terminal")]
    state_dir: PathBuf,
    #[arg(long)]
    model: Option<String>,
    /// Layer $BROWSER_USE_TERMINAL_HOME/<name>.config.toml on top of the base user config.
    #[arg(long = "profile", short = 'p')]
    config_profile: Option<String>,
    /// Override a configuration value. Use a dotted path and TOML value.
    #[arg(short = 'c', long = "config", value_name = "key=value", action = clap::ArgAction::Append)]
    config_overrides: Vec<String>,
    #[arg(long, default_value = "Codex login")]
    account: String,
    #[arg(long, default_value = "Local Chrome")]
    browser: String,
    #[arg(long = "collaboration-mode", value_enum, default_value_t = CollaborationModeArg::Default)]
    collaboration_mode: CollaborationModeArg,
    #[arg(long)]
    dump_screen: bool,
    #[arg(long, default_value_t = 120)]
    width: u16,
    #[arg(long, default_value_t = 28)]
    height: u16,
    #[arg(long)]
    select_latest: bool,
    #[arg(long)]
    seed_demo: Option<String>,
    #[arg(long, value_enum)]
    overlay: Option<ScreenArg>,
    #[arg(long, value_enum, default_value = "codex", hide = true)]
    agent: AgentBackend,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
enum CollaborationModeArg {
    Default,
    Plan,
}

impl From<CollaborationModeArg> for CollaborationModeKind {
    fn from(value: CollaborationModeArg) -> Self {
        match value {
            CollaborationModeArg::Default => CollaborationModeKind::Default,
            CollaborationModeArg::Plan => CollaborationModeKind::Plan,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Surface {
    Main,
    Setup,
    SetupConfirm,
    SetupResult,
    Account,
    ApiKey,
    Telemetry,
    Model,
    Mode,
    Browser,
    BrowserSelect,
    History,
    Messages,
    Developer,
}

impl Surface {
    fn is_bottom_pane(self) -> bool {
        matches!(
            self,
            Self::Account
                | Self::ApiKey
                | Self::Telemetry
                | Self::Model
                | Self::Mode
                | Self::Browser
                | Self::BrowserSelect
                | Self::History
                | Self::Messages
                | Self::Developer
        )
    }

    /// Surfaces that render as a centered floating popup overlay on top of the
    /// main view, rather than as a fullscreen surface or an inline bottom pane.
    fn is_popup(self) -> bool {
        self.is_bottom_pane()
    }

    /// Popups that read text input from the shared composer buffer. While one
    /// of these is active the composer must not also be rendered underneath —
    /// the popup itself is the input field, with its own cursor.
    fn is_text_input_popup(self) -> bool {
        matches!(self, Self::ApiKey | Self::Telemetry)
    }

    fn uses_main_view(self) -> bool {
        self == Self::Main || self.is_bottom_pane()
    }
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum ScreenArg {
    Setup,
    Account,
    Telemetry,
    Model,
    Mode,
    Browser,
    History,
    Developer,
}

impl From<ScreenArg> for Surface {
    fn from(value: ScreenArg) -> Self {
        match value {
            ScreenArg::Setup => Self::Setup,
            ScreenArg::Account => Self::Account,
            ScreenArg::Telemetry => Self::Telemetry,
            ScreenArg::Model => Self::Model,
            ScreenArg::Mode => Self::Mode,
            ScreenArg::Browser => Self::Browser,
            ScreenArg::History => Self::History,
            ScreenArg::Developer => Self::Developer,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ProductState {
    SetupNeeded,
    Ready,
    Running,
    Result,
    Failed,
    Cancelled,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum SetupResultKind {
    Pending,
    Success,
    Failure,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct SetupResult {
    kind: SetupResultKind,
    account: String,
    message: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub(crate) struct RequestUserInputOption {
    pub(crate) label: String,
    pub(crate) description: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub(crate) struct RequestUserInputQuestion {
    pub(crate) id: String,
    pub(crate) header: String,
    pub(crate) question: String,
    #[serde(rename = "isOther", default)]
    pub(crate) is_other: bool,
    #[serde(rename = "isSecret", default)]
    pub(crate) is_secret: bool,
    pub(crate) options: Option<Vec<RequestUserInputOption>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PendingRequestUserInput {
    pub(crate) call_id: String,
    pub(crate) turn_id: String,
    pub(crate) questions: Vec<RequestUserInputQuestion>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RequestUserInputFocus {
    Options,
    Notes,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RequestUserInputAnswerDraft {
    pub(crate) option_cursor: Option<usize>,
    pub(crate) committed_option: Option<usize>,
    pub(crate) notes: String,
    pub(crate) answer_committed: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RequestUserInputState {
    pub(crate) session_id: String,
    pub(crate) call_id: String,
    pub(crate) turn_id: String,
    pub(crate) current_idx: usize,
    pub(crate) focus: RequestUserInputFocus,
    pub(crate) answers: Vec<RequestUserInputAnswerDraft>,
    pub(crate) confirm_unanswered: bool,
    pub(crate) confirm_selected: usize,
}

#[derive(Debug)]
struct ClaudeCodeOAuthEvent {
    account: String,
    result: Result<ClaudeCodeOAuthCredential, String>,
}

#[derive(Debug)]
struct ClaudeCodeOAuthFlow {
    account: String,
    url: String,
    started_at: Instant,
    stop_tx: mpsc::Sender<()>,
    rx: mpsc::Receiver<ClaudeCodeOAuthEvent>,
    browser_open_error: Option<String>,
    #[cfg(test)]
    event_tx_guard: Option<mpsc::Sender<ClaudeCodeOAuthEvent>>,
}

impl Drop for ClaudeCodeOAuthFlow {
    fn drop(&mut self) {
        let _ = self.stop_tx.send(());
    }
}

#[derive(Debug)]
enum CodexLoginEvent {
    Output(String),
    Finished(Result<CodexAuth, String>),
}

#[derive(Debug)]
struct CodexLoginFlow {
    account: String,
    output: String,
    started_at: Instant,
    stop_tx: mpsc::Sender<()>,
    rx: mpsc::Receiver<CodexLoginEvent>,
    #[cfg(test)]
    event_tx_guard: Option<mpsc::Sender<CodexLoginEvent>>,
}

impl Drop for CodexLoginFlow {
    fn drop(&mut self) {
        let _ = self.stop_tx.send(());
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum AppCommand {
    StartTask(String),
    StartTaskSubmission(UserSubmission),
    SendFollowup {
        session_id: String,
        text: String,
    },
    SendFollowupSubmission {
        session_id: String,
        submission: UserSubmission,
    },
    QueueFollowupSubmission {
        session_id: String,
        submission: UserSubmission,
    },
    AnswerRequestUserInput {
        session_id: String,
        call_id: String,
        text: String,
    },
    RetryTask(String),
    OpenBrowser,
    ReconnectBrowser,
    NewTask,
    OpenHistory,
    SelectHistory(String),
    ChangeModel,
    ChangeMode,
    SetCollaborationMode(CollaborationModeKind),
    SignIn,
    ConfigureTelemetry,
    ChangeBrowser,
    Reload,
    Update,
    SaveAccount(String),
    SaveModel(usize),
    SaveBrowser(usize),
    SaveAuth(String),
    SaveTelemetry(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct UserSubmission {
    text: String,
    local_images: Vec<PathBuf>,
}

impl UserSubmission {
    fn text(text: String) -> Self {
        Self {
            text,
            local_images: Vec::new(),
        }
    }

    fn has_local_images(&self) -> bool {
        !self.local_images.is_empty()
    }

    fn display_text(&self) -> String {
        display_text_for_local_images_and_text(self.local_images.len(), &self.text)
            .unwrap_or_default()
    }
}

fn typed_user_input_payload_for_submission_for_cwd(
    submission: &UserSubmission,
    cwd: impl AsRef<Path>,
) -> Result<serde_json::Value> {
    if submission.local_images.is_empty() {
        return typed_user_input_payload_from_text_for_cwd(&submission.text, cwd);
    }

    let mut items = Vec::new();
    for path in &submission.local_images {
        items.push(serde_json::json!({
            "type": "local_image",
            "path": path.display().to_string(),
            "detail": "high",
        }));
    }
    if !submission.text.is_empty() {
        items.push(serde_json::json!({
            "type": "text",
            "text": submission.text,
        }));
    }
    let mut payload =
        typed_user_input_payload_from_items_for_cwd(&serde_json::Value::Array(items), cwd)?;
    payload["text"] = serde_json::json!(submission.display_text());
    Ok(payload)
}

pub(crate) fn user_input_display_text_from_payload(payload: &serde_json::Value) -> Option<String> {
    if let Some(items) = payload.get("items").and_then(serde_json::Value::as_array) {
        let local_image_count = items
            .iter()
            .filter(|item| {
                matches!(
                    item.get("type").and_then(serde_json::Value::as_str),
                    Some("image" | "local_image")
                )
            })
            .count();
        if local_image_count > 0 {
            let text = items
                .iter()
                .filter(|item| item.get("type").and_then(serde_json::Value::as_str) == Some("text"))
                .filter_map(|item| item.get("text").and_then(serde_json::Value::as_str))
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .collect::<Vec<_>>()
                .join("\n");
            if let Some(display) = display_text_for_local_images_and_text(local_image_count, &text)
            {
                return Some(display);
            }
        }
    }

    payload
        .get("text")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

fn display_text_for_local_images_and_text(image_count: usize, text: &str) -> Option<String> {
    let text = text.trim();
    if image_count == 0 {
        return (!text.is_empty()).then(|| text.to_string());
    }
    let labels = (1..=image_count)
        .map(|idx| format!("[Image {idx}]"))
        .collect::<Vec<_>>()
        .join(" ");
    if text.is_empty() {
        Some(labels)
    } else {
        Some(format!("{labels}\n{text}"))
    }
}

fn event_payload_text(event: &EventRecord) -> Option<String> {
    user_input_display_text_from_payload(&event.payload)
}

fn event_is_pre_output_after_submission(event: &EventRecord, target_seq: i64) -> bool {
    match event.event_type.as_str() {
        "model.turn.request" | "model.thinking_delta" | "session.status" => true,
        "workspace.context"
        | "model.switch_context"
        | "model.personality_context"
        | "model.collaboration_context"
        | "model.generated_image_context" => {
            event
                .payload
                .get("before_seq")
                .and_then(serde_json::Value::as_i64)
                == Some(target_seq)
        }
        _ => false,
    }
}

fn queued_followup_marker_seq(event: &EventRecord) -> Option<i64> {
    event
        .payload
        .get("queued_seq")
        .or_else(|| event.payload.get("seq"))
        .and_then(serde_json::Value::as_i64)
}

fn pending_queued_followup_events_from_events(events: &[EventRecord]) -> Vec<&EventRecord> {
    let closed = events
        .iter()
        .filter(|event| {
            matches!(
                event.event_type.as_str(),
                SESSION_QUEUED_FOLLOWUP_SENT_EVENT | SESSION_QUEUED_FOLLOWUP_CANCELLED_EVENT
            )
        })
        .filter_map(queued_followup_marker_seq)
        .collect::<HashSet<_>>();
    events
        .iter()
        .filter(|event| {
            event.event_type == SESSION_QUEUED_FOLLOWUP_EVENT && !closed.contains(&event.seq)
        })
        .collect()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MessageActionKind {
    Submitted,
    Queued,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct MessageActionRow {
    seq: i64,
    kind: MessageActionKind,
    text: String,
    followup: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct SessionModelSelection {
    display_model: String,
    provider_model: String,
    account: String,
    backend: AgentBackend,
    model_provider_id: Option<String>,
}

struct App {
    store: Store,
    store_rx: mpsc::Receiver<StoreNotification>,
    state_cache: AppStateCache,
    args: Args,
    selected_session_id: Option<String>,
    composer: Composer,
    prompt_history: PromptHistoryState,
    request_input: Option<RequestUserInputState>,
    surface: Surface,
    selected_row: usize,
    setup_complete: bool,
    account: String,
    model: String,
    model_configured: bool,
    provider_model: String,
    model_provider_id: Option<String>,
    model_choices: Vec<ModelChoice>,
    collaboration_mode: CollaborationModeKind,
    browser: String,
    api_key_account: Option<String>,
    pending_model_after_auth: Option<usize>,
    setup_pending_account: Option<String>,
    setup_result: Option<SetupResult>,
    claude_code_oauth: Option<ClaudeCodeOAuthFlow>,
    codex_login: Option<CodexLoginFlow>,
    browser_notice: Option<String>,
    status_notice: Option<String>,
    agent_backend: AgentBackend,
    quit_hint_until: Option<Instant>,
    escape_stop_until: Option<Instant>,
    native_history: NativeHistoryState,
    welcome_anim: welcome::WelcomeAnim,
    live_spinner_frame: usize,
    /// Last-rendered logo bounding box on screen (terminal cells). Set by
    /// render.rs each frame and read by the mouse click handler.
    welcome_logo_rect: std::cell::Cell<Option<ratatui::layout::Rect>>,
    /// Last-rendered composer input rectangle on screen (terminal cells). Set
    /// by render.rs each frame and read by the mouse click handler.
    composer_input_rect: std::cell::Cell<Option<ratatui::layout::Rect>>,
    /// Whether the slash command palette popup is currently open. Independent
    /// of the composer's content — `/` opens it, Esc closes it, and the
    /// composer is never touched.
    palette_open: bool,
    /// Filter text shown inside the palette popup. Edited by typing while the
    /// palette is open; cleared whenever the palette is opened or closed.
    palette_filter: String,
    /// Substring filter applied to the History surface task list. Edited by
    /// typing while the History popup is open and cleared whenever the surface
    /// opens or closes. Empty string means "show everything".
    history_filter: String,
}

#[derive(Debug)]
struct AppStateCache {
    sessions: Vec<SessionMeta>,
    events_by_session: HashMap<String, Vec<EventRecord>>,
    last_seq_by_session: HashMap<String, i64>,
    projected: WorkbenchState,
    projection_key: Option<ProjectionKey>,
    dirty_projection: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ProjectionKey {
    selected_session_id: Option<String>,
    browser: String,
    history_tasks_visible: bool,
}

impl AppStateCache {
    fn hydrate(store: &Store, browser: &str) -> Result<Self> {
        let sessions = store.list_sessions()?;
        let mut events_by_session = HashMap::new();
        let mut last_seq_by_session = HashMap::new();
        for session in &sessions {
            let events = store.events_for_session(&session.id)?;
            let last_seq = events.last().map(|event| event.seq).unwrap_or_default();
            last_seq_by_session.insert(session.id.clone(), last_seq);
            events_by_session.insert(session.id.clone(), events);
        }
        Ok(Self {
            sessions,
            events_by_session,
            last_seq_by_session,
            projected: empty_workbench_state(browser),
            projection_key: None,
            dirty_projection: true,
        })
    }

    fn apply_notification(
        &mut self,
        store: &Store,
        notification: StoreNotification,
    ) -> Result<bool> {
        match notification {
            StoreNotification::SessionsChanged => self.refresh_sessions(store),
            StoreNotification::SessionChanged { session_id } => {
                self.refresh_session(store, &session_id)
            }
            StoreNotification::EventsChanged { session_id, seq: _ } => {
                self.refresh_events_after_seq(store, &session_id)
            }
            StoreNotification::SettingsChanged => Ok(false),
        }
    }

    fn refresh_all(&mut self, store: &Store) -> Result<bool> {
        let mut changed = self.refresh_sessions(store)?;
        let session_ids = self
            .sessions
            .iter()
            .map(|session| session.id.clone())
            .collect::<Vec<_>>();
        for session_id in session_ids {
            changed |= self.refresh_events_after_seq(store, &session_id)?;
        }
        Ok(changed)
    }

    fn refresh_sessions(&mut self, store: &Store) -> Result<bool> {
        let sessions = store.list_sessions()?;
        let sessions_changed = self.sessions != sessions;
        self.sessions = sessions;
        let live_ids = self
            .sessions
            .iter()
            .map(|session| session.id.as_str())
            .collect::<std::collections::HashSet<_>>();
        let old_event_count = self.events_by_session.len();
        self.events_by_session
            .retain(|session_id, _| live_ids.contains(session_id.as_str()));
        self.last_seq_by_session
            .retain(|session_id, _| live_ids.contains(session_id.as_str()));
        let removed_events = self.events_by_session.len() != old_event_count;
        let unknown_ids = self
            .sessions
            .iter()
            .filter(|session| !self.events_by_session.contains_key(&session.id))
            .map(|session| session.id.clone())
            .collect::<Vec<_>>();
        let loaded_events = !unknown_ids.is_empty();
        for session_id in unknown_ids {
            let events = store.events_for_session(&session_id)?;
            let last_seq = events.last().map(|event| event.seq).unwrap_or_default();
            self.last_seq_by_session
                .insert(session_id.clone(), last_seq);
            self.events_by_session.insert(session_id, events);
        }
        let changed = sessions_changed || removed_events || loaded_events;
        if changed {
            self.dirty_projection = true;
        }
        Ok(changed)
    }

    fn refresh_session(&mut self, store: &Store, session_id: &str) -> Result<bool> {
        let changed = match store.load_session(session_id)? {
            Some(session) => self.upsert_session(session),
            None => {
                let old_len = self.sessions.len();
                self.sessions.retain(|session| session.id != session_id);
                let removed_events = self.events_by_session.remove(session_id).is_some();
                let removed_seq = self.last_seq_by_session.remove(session_id).is_some();
                old_len != self.sessions.len() || removed_events || removed_seq
            }
        };
        if changed {
            self.dirty_projection = true;
        }
        Ok(changed)
    }

    fn refresh_events_after_seq(&mut self, store: &Store, session_id: &str) -> Result<bool> {
        let after_seq = self
            .last_seq_by_session
            .get(session_id)
            .copied()
            .unwrap_or_default();
        let events = store.events_after_seq(session_id, after_seq)?;
        if events.is_empty() {
            return Ok(false);
        }
        let last_seq = events.last().map(|event| event.seq).unwrap_or(after_seq);
        self.events_by_session
            .entry(session_id.to_string())
            .or_default()
            .extend(events);
        self.last_seq_by_session
            .insert(session_id.to_string(), last_seq);
        self.dirty_projection = true;
        Ok(true)
    }

    fn upsert_session(&mut self, session: SessionMeta) -> bool {
        if let Some(existing) = self
            .sessions
            .iter_mut()
            .find(|candidate| candidate.id == session.id)
        {
            if *existing == session {
                return false;
            }
            *existing = session;
        } else {
            self.sessions.push(session);
        }
        self.sessions
            .sort_by(|left, right| right.updated_ms.cmp(&left.updated_ms));
        true
    }

    fn project_if_needed(
        &mut self,
        selected_session_id: Option<&str>,
        browser: &str,
        history_tasks_visible: bool,
    ) -> &WorkbenchState {
        let key = ProjectionKey {
            selected_session_id: selected_session_id.map(ToOwned::to_owned),
            browser: browser.to_string(),
            history_tasks_visible,
        };
        if !self.dirty_projection && self.projection_key.as_ref() == Some(&key) {
            return &self.projected;
        }

        let current_events = selected_session_id
            .and_then(|id| self.events_by_session.get(id))
            .map(Vec::as_slice)
            .unwrap_or_default();
        let all_events = if history_tasks_visible {
            self.sessions
                .iter()
                .map(|session| {
                    (
                        session.id.clone(),
                        self.events_by_session
                            .get(&session.id)
                            .cloned()
                            .unwrap_or_default(),
                    )
                })
                .collect::<Vec<_>>()
        } else if let Some(id) = selected_session_id {
            let mut session_ids = vec![id.to_string()];
            let mut index = 0;
            while index < session_ids.len() {
                let parent_id = session_ids[index].clone();
                for session in self
                    .sessions
                    .iter()
                    .filter(|session| session.parent_id.as_deref() == Some(parent_id.as_str()))
                {
                    if !session_ids.iter().any(|id| id == &session.id) {
                        session_ids.push(session.id.clone());
                    }
                }
                index += 1;
            }
            session_ids
                .into_iter()
                .map(|session_id| {
                    (
                        session_id.clone(),
                        self.events_by_session
                            .get(&session_id)
                            .cloned()
                            .unwrap_or_default(),
                    )
                })
                .collect::<Vec<_>>()
        } else {
            Vec::new()
        };
        self.projected = project_workbench(
            &self.sessions,
            current_events,
            &all_events,
            selected_session_id,
            browser.to_string(),
        );
        self.projection_key = Some(key);
        self.dirty_projection = false;
        &self.projected
    }

    fn events_for_session(&self, session_id: &str) -> &[EventRecord] {
        self.events_by_session
            .get(session_id)
            .map(Vec::as_slice)
            .unwrap_or_default()
    }
}

fn empty_workbench_state(browser: &str) -> WorkbenchState {
    WorkbenchState {
        setup_complete: false,
        current_session: None,
        task: None,
        result: None,
        failure: None,
        activity: Vec::new(),
        transcript: Vec::new(),
        browser: browser_use_protocol::BrowserSummary {
            backend: browser.to_string(),
            status: "not connected".to_string(),
            ..Default::default()
        },
        telemetry: Default::default(),
        history: Vec::new(),
    }
}

#[derive(Debug, Default)]
struct NativeHistoryState {
    session_id: Option<String>,
    last_seq: i64,
    last_group: Option<String>,
    clear_before_replay: bool,
    live_stream: Option<NativeLiveStreamState>,
}

#[derive(Debug, Clone)]
struct NativeLiveStreamState {
    session_id: String,
    width: u16,
    emitted_lines: usize,
    emitted_text_lines: Vec<String>,
}

impl NativeHistoryState {
    fn reset(&mut self) {
        self.session_id = None;
        self.last_seq = 0;
        self.last_group = None;
        self.clear_before_replay = false;
        self.live_stream = None;
    }

    fn reset_with_clear(&mut self) {
        self.reset();
        self.clear_before_replay = true;
    }

    #[cfg(test)]
    fn reset_for_session(&mut self, session_id: String, last_seq: i64) {
        self.reset_for_session_with_group(session_id, last_seq, None);
    }

    fn reset_for_session_with_group(
        &mut self,
        session_id: String,
        last_seq: i64,
        last_group: Option<String>,
    ) {
        self.session_id = Some(session_id);
        self.last_seq = last_seq;
        self.last_group = last_group;
        self.clear_before_replay = false;
    }

    fn is_active_for(&self, session_id: Option<&str>) -> bool {
        self.session_id.as_deref().is_some() && self.session_id.as_deref() == session_id
    }

    fn live_stream_emitted_lines_for(&self, session_id: &str, width: u16) -> usize {
        self.live_stream
            .as_ref()
            .filter(|stream| stream.session_id == session_id && stream.width == width)
            .map(|stream| stream.emitted_lines)
            .unwrap_or(0)
    }

    fn live_stream_emitted_text_for(&self, session_id: &str, width: u16) -> Option<&[String]> {
        self.live_stream
            .as_ref()
            .filter(|stream| stream.session_id == session_id && stream.width == width)
            .map(|stream| stream.emitted_text_lines.as_slice())
    }

    fn set_live_stream_emitted_lines(
        &mut self,
        session_id: &str,
        width: u16,
        emitted_lines: usize,
        emitted_text_lines: Vec<String>,
    ) {
        self.live_stream = Some(NativeLiveStreamState {
            session_id: session_id.to_string(),
            width,
            emitted_lines,
            emitted_text_lines,
        });
    }

    fn clear_live_stream(&mut self) {
        self.live_stream = None;
    }

    fn take_clear_before_replay(&mut self) -> bool {
        let should_clear = self.clear_before_replay;
        self.clear_before_replay = false;
        should_clear
    }
}

const MAX_LOCAL_PROMPT_HISTORY_ENTRIES: usize = 1_000;

#[derive(Debug, Default)]
struct PromptHistoryState {
    persistent_initialized: bool,
    persistent_log_id: u64,
    persistent_count: usize,
    persistent_cache: HashMap<usize, String>,
    persistent_search_entries: Option<Vec<String>>,
    local_entries: Vec<String>,
    nav_index: Option<usize>,
    nav_draft: Option<String>,
    last_history_text: Option<String>,
    search: Option<PromptHistorySearchState>,
}

#[derive(Clone, Debug)]
struct PromptHistorySearchState {
    query: String,
    draft: String,
    matches: Vec<String>,
    selected: Option<usize>,
}

impl PromptHistoryState {
    fn refresh_persistent_metadata(&mut self, config: Option<&MessageHistoryConfig>) {
        let Some(config) = config else {
            self.replace_persistent_metadata(0, 0);
            return;
        };
        if matches!(config.settings.persistence, MessageHistoryPersistence::None) {
            self.replace_persistent_metadata(0, 0);
            return;
        }

        let (log_id, count) = browser_use_core::message_history_metadata(config);
        if !self.persistent_initialized
            || self.persistent_log_id != log_id
            || count < self.persistent_count
        {
            self.replace_persistent_metadata(log_id, count);
        }
    }

    fn replace_persistent_metadata(&mut self, log_id: u64, count: usize) {
        if self.persistent_initialized
            && self.persistent_log_id == log_id
            && self.persistent_count == count
        {
            return;
        }
        self.persistent_initialized = true;
        self.persistent_log_id = log_id;
        self.persistent_count = count;
        self.persistent_cache.clear();
        self.persistent_search_entries = None;
        self.reset_navigation();
    }

    fn record_submission(&mut self, text: &str) {
        let text = text.trim();
        if text.is_empty() {
            return;
        }
        if self.local_entries.last().is_some_and(|entry| entry == text) {
            self.reset_navigation();
            self.search = None;
            return;
        }
        self.local_entries.push(text.to_string());
        if self.local_entries.len() > MAX_LOCAL_PROMPT_HISTORY_ENTRIES {
            let overflow = self
                .local_entries
                .len()
                .saturating_sub(MAX_LOCAL_PROMPT_HISTORY_ENTRIES);
            self.local_entries.drain(0..overflow);
        }
        self.reset_navigation();
        self.search = None;
    }

    fn reset_navigation(&mut self) {
        self.nav_index = None;
        self.nav_draft = None;
        self.last_history_text = None;
    }

    fn is_navigating(&self) -> bool {
        self.nav_index.is_some()
    }

    fn should_handle_navigation(&self, text: &str, cursor_at_boundary: bool) -> bool {
        if self.total_entries() == 0 {
            return false;
        }
        if text.is_empty() {
            return true;
        }
        cursor_at_boundary && self.last_history_text.as_deref() == Some(text)
    }

    fn total_entries(&self) -> usize {
        self.persistent_count + self.local_entries.len()
    }

    fn entry_at(&mut self, index: usize, config: Option<&MessageHistoryConfig>) -> Option<String> {
        if index < self.persistent_count {
            return self.persistent_entry(index, config);
        }
        self.local_entries
            .get(index.saturating_sub(self.persistent_count))
            .cloned()
    }

    fn persistent_entry(
        &mut self,
        offset: usize,
        config: Option<&MessageHistoryConfig>,
    ) -> Option<String> {
        if offset >= self.persistent_count {
            return None;
        }
        if let Some(entry) = self.persistent_cache.get(&offset) {
            return Some(entry.clone());
        }
        let config = config?;
        let entry =
            browser_use_core::lookup_message_history_entry(self.persistent_log_id, offset, config)?;
        if entry.text.trim().is_empty() {
            return None;
        }
        self.persistent_cache.insert(offset, entry.text.clone());
        Some(entry.text)
    }

    fn search_entries(&mut self, config: Option<&MessageHistoryConfig>) -> Vec<String> {
        if self.persistent_search_entries.is_none() {
            let persistent_entries = config
                .filter(|_| self.persistent_count > 0)
                .map(|config| {
                    browser_use_core::message_history_entries(
                        self.persistent_log_id,
                        self.persistent_count,
                        config,
                    )
                    .into_iter()
                    .filter_map(|entry| (!entry.text.trim().is_empty()).then_some(entry.text))
                    .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            self.persistent_search_entries = Some(persistent_entries);
        }
        let persistent_entries = self.persistent_search_entries.clone().unwrap_or_default();
        let mut entries = Vec::with_capacity(persistent_entries.len() + self.local_entries.len());
        entries.extend(persistent_entries);
        entries.extend(self.local_entries.iter().cloned());
        entries
    }
}

fn prompt_history_search_matches(entries: &[String], query: &str) -> Vec<String> {
    let query = query.trim().to_lowercase();
    if query.is_empty() {
        return Vec::new();
    }
    let mut seen = HashSet::new();
    let mut matches = Vec::new();
    for entry in entries.iter().rev() {
        let normalized = entry.to_lowercase();
        if normalized.contains(&query) && seen.insert(entry.clone()) {
            matches.push(entry.clone());
        }
    }
    matches
}

fn key_matches_control_char(key: KeyEvent, ch: char, raw: char) -> bool {
    matches!(
        key,
        KeyEvent {
            code: KeyCode::Char(value),
            modifiers: KeyModifiers::CONTROL,
            ..
        } if value.eq_ignore_ascii_case(&ch)
    ) || matches!(
        key,
        KeyEvent {
            code: KeyCode::Char(value),
            modifiers: KeyModifiers::NONE,
            ..
        } if value == raw
    )
}

fn is_prompt_history_search_start_key(key: KeyEvent) -> bool {
    key_matches_control_char(key, 'r', '\u{0012}')
}

fn is_prompt_history_search_next_key(key: KeyEvent) -> bool {
    key_matches_control_char(key, 's', '\u{0013}')
}

fn is_prompt_history_search_cancel_key(key: KeyEvent) -> bool {
    key_matches_control_char(key, 'c', '\u{0003}')
}

fn is_prompt_history_search_backspace_key(key: KeyEvent) -> bool {
    key_matches_control_char(key, 'h', '\u{0008}')
}

fn is_prompt_history_search_clear_key(key: KeyEvent) -> bool {
    key_matches_control_char(key, 'u', '\u{0015}')
}

pub(crate) fn collaboration_mode_label(mode: CollaborationModeKind) -> &'static str {
    match mode {
        CollaborationModeKind::Default => "Default",
        CollaborationModeKind::Plan => "Plan",
    }
}

fn collaboration_mode_setting_value(mode: CollaborationModeKind) -> &'static str {
    match mode {
        CollaborationModeKind::Default => "default",
        CollaborationModeKind::Plan => "plan",
    }
}

fn collaboration_mode_from_setting(value: &str) -> Option<CollaborationModeKind> {
    match value.trim().to_ascii_lowercase().as_str() {
        "default" => Some(CollaborationModeKind::Default),
        "plan" => Some(CollaborationModeKind::Plan),
        _ => None,
    }
}

fn next_collaboration_mode(mode: CollaborationModeKind) -> CollaborationModeKind {
    match mode {
        CollaborationModeKind::Default => CollaborationModeKind::Plan,
        CollaborationModeKind::Plan => CollaborationModeKind::Default,
    }
}

fn pending_request_user_input_from_events(
    events: &[EventRecord],
) -> Option<PendingRequestUserInput> {
    let start_idx = events
        .iter()
        .rposition(|event| {
            matches!(
                event.event_type.as_str(),
                "session.done" | "session.failed" | "session.cancelled"
            )
        })
        .map(|idx| idx.saturating_add(1))
        .unwrap_or(0);
    let recent_events = &events[start_idx..];
    let mut answered = HashSet::new();
    for event in recent_events
        .iter()
        .filter(|event| event.event_type == REQUEST_USER_INPUT_RESPONSE_EVENT)
    {
        if let Some(turn_id) = event
            .payload
            .get("turn_id")
            .and_then(serde_json::Value::as_str)
        {
            answered.insert(turn_id);
        } else if let Some(call_id) = event
            .payload
            .get("call_id")
            .and_then(serde_json::Value::as_str)
        {
            answered.insert(call_id);
        }
    }
    recent_events.iter().find_map(|event| {
        if event.event_type != REQUEST_USER_INPUT_REQUEST_EVENT {
            return None;
        }
        let call_id = event
            .payload
            .get("call_id")
            .and_then(serde_json::Value::as_str)?;
        let turn_id = event
            .payload
            .get("turn_id")
            .and_then(serde_json::Value::as_str)
            .unwrap_or(call_id);
        if answered.contains(turn_id) || answered.contains(call_id) {
            return None;
        }
        let questions = event.payload.get("questions")?.clone();
        let questions = serde_json::from_value::<Vec<RequestUserInputQuestion>>(questions).ok()?;
        Some(Some(PendingRequestUserInput {
            call_id: call_id.to_string(),
            turn_id: turn_id.to_string(),
            questions,
        }))
    })?
}

impl RequestUserInputState {
    fn new(session_id: String, request: &PendingRequestUserInput) -> Self {
        let answers = request
            .questions
            .iter()
            .map(|question| {
                let option_cursor = (request_user_input_option_count(question) > 0).then_some(0);
                RequestUserInputAnswerDraft {
                    option_cursor,
                    committed_option: None,
                    notes: String::new(),
                    answer_committed: false,
                }
            })
            .collect::<Vec<_>>();
        Self {
            session_id,
            call_id: request.call_id.clone(),
            turn_id: request.turn_id.clone(),
            current_idx: 0,
            focus: RequestUserInputFocus::Options,
            answers,
            confirm_unanswered: false,
            confirm_selected: 0,
        }
    }
}

fn request_user_input_option_count(question: &RequestUserInputQuestion) -> usize {
    let option_count = question.options.as_ref().map(Vec::len).unwrap_or_default();
    if question.is_other && option_count > 0 {
        option_count.saturating_add(1)
    } else {
        option_count
    }
}

fn request_user_input_option_label(
    question: &RequestUserInputQuestion,
    idx: usize,
) -> Option<String> {
    let options = question.options.as_ref()?;
    if idx < options.len() {
        return options.get(idx).map(|option| option.label.clone());
    }
    if question.is_other && idx == options.len() {
        return Some(REQUEST_USER_INPUT_OTHER_LABEL.to_string());
    }
    None
}

fn request_user_input_state_answer_values(
    question: &RequestUserInputQuestion,
    draft: &RequestUserInputAnswerDraft,
) -> Vec<String> {
    if !draft.answer_committed {
        return Vec::new();
    }
    let mut values = draft
        .committed_option
        .and_then(|idx| request_user_input_option_label(question, idx))
        .into_iter()
        .collect::<Vec<_>>();
    let note = draft.notes.trim();
    if !note.is_empty() {
        values.push(format!("user_note: {note}"));
    }
    values
}

fn request_user_input_state_response_payload(
    request: &PendingRequestUserInput,
    state: &RequestUserInputState,
) -> serde_json::Value {
    let mut answers = serde_json::Map::new();
    for (idx, question) in request.questions.iter().enumerate() {
        let values = state
            .answers
            .get(idx)
            .map(|draft| request_user_input_state_answer_values(question, draft))
            .unwrap_or_default();
        answers.insert(
            question.id.clone(),
            serde_json::json!({
                "answers": values,
            }),
        );
    }
    serde_json::json!({
        "turn_id": request.turn_id.clone(),
        "call_id": request.call_id.clone(),
        "questions": request.questions.clone(),
        "answers": answers,
    })
}

fn request_user_input_response_payload(
    request: &PendingRequestUserInput,
    text: &str,
) -> serde_json::Value {
    let parts = request_user_input_answer_texts(request, text);
    let mut answers = serde_json::Map::new();
    for (idx, question) in request.questions.iter().enumerate() {
        let answer_text = parts.get(idx).map(String::as_str).unwrap_or_default();
        answers.insert(
            question.id.clone(),
            serde_json::json!({
                "answers": request_user_input_answer_values(question, answer_text),
            }),
        );
    }
    serde_json::json!({
        "turn_id": request.turn_id.clone(),
        "call_id": request.call_id.clone(),
        "questions": request.questions.clone(),
        "answers": answers,
    })
}

fn request_user_input_answer_texts(request: &PendingRequestUserInput, text: &str) -> Vec<String> {
    if let Some(parts) = keyed_request_user_input_answers(request, text) {
        return parts;
    }
    split_request_user_input_answers(text, request.questions.len())
}

fn keyed_request_user_input_answers(
    request: &PendingRequestUserInput,
    text: &str,
) -> Option<Vec<String>> {
    if request.questions.len() <= 1 {
        return None;
    }
    let mut parts = vec![String::new(); request.questions.len()];
    let mut used = vec![false; request.questions.len()];
    let mut unmatched = Vec::new();
    for part in request_user_input_text_parts(text) {
        let Some((raw_key, raw_value)) = part.split_once(':') else {
            unmatched.push(part);
            continue;
        };
        let Some(idx) = request_user_input_question_index(request, raw_key.trim()) else {
            unmatched.push(part);
            continue;
        };
        parts[idx] = raw_value.trim().to_string();
        used[idx] = true;
    }
    if !used.iter().any(|is_used| *is_used) {
        return None;
    }
    let mut next_unmatched = unmatched.into_iter();
    for (idx, is_used) in used.iter_mut().enumerate() {
        if *is_used {
            continue;
        }
        if let Some(value) = next_unmatched.next() {
            parts[idx] = value;
            *is_used = true;
        }
    }
    Some(parts)
}

fn request_user_input_text_parts(text: &str) -> Vec<String> {
    text.lines()
        .flat_map(|line| line.split(';'))
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

fn request_user_input_question_index(
    request: &PendingRequestUserInput,
    key: &str,
) -> Option<usize> {
    if let Ok(number) = key.trim().parse::<usize>() {
        return number
            .checked_sub(1)
            .filter(|idx| *idx < request.questions.len());
    }
    let normalized_key = normalize_request_user_input_key(key);
    request.questions.iter().position(|question| {
        normalize_request_user_input_key(&question.id) == normalized_key
            || normalize_request_user_input_key(&question.header) == normalized_key
            || normalize_request_user_input_key(&question.question) == normalized_key
    })
}

fn normalize_request_user_input_key(value: &str) -> String {
    value
        .trim()
        .trim_matches(|ch: char| matches!(ch, ':' | '.' | ')' | '('))
        .trim()
        .to_ascii_lowercase()
}

fn split_request_user_input_answers(text: &str, question_count: usize) -> Vec<String> {
    if question_count <= 1 {
        return vec![text.trim().to_string()];
    }
    let line_parts = request_user_input_text_parts(text);
    if line_parts.len() >= question_count {
        return line_parts;
    }
    line_parts
}

fn request_user_input_answer_values(
    question: &RequestUserInputQuestion,
    answer_text: &str,
) -> Vec<String> {
    let answer_text = answer_text.trim();
    let Some(options) = question
        .options
        .as_ref()
        .filter(|options| !options.is_empty())
    else {
        return (!answer_text.is_empty())
            .then(|| answer_text.to_string())
            .into_iter()
            .collect();
    };
    if answer_text.is_empty() {
        return Vec::new();
    }
    if matches_ignore_ascii_case(answer_text, "unanswered")
        || matches_ignore_ascii_case(answer_text, "skip")
    {
        return Vec::new();
    }
    if let Some((number, note)) = parse_numbered_request_user_input_answer(answer_text) {
        if let Some(option) = number.checked_sub(1).and_then(|idx| options.get(idx)) {
            return vec![option.label.clone()];
        }
        if question.is_other && number == options.len().saturating_add(1) {
            let mut out = vec![REQUEST_USER_INPUT_OTHER_LABEL.to_string()];
            if let Some(note) = note.filter(|note| !note.is_empty()) {
                out.push(format!("user_note: {note}"));
            }
            return out;
        }
    }
    if let Some(option) = options
        .iter()
        .find(|option| request_user_input_option_matches(&option.label, answer_text))
    {
        return vec![option.label.clone()];
    }
    if question.is_other {
        let note = strip_request_user_input_other_prefix(answer_text).unwrap_or(answer_text);
        if request_user_input_option_matches(REQUEST_USER_INPUT_OTHER_LABEL, answer_text) {
            return vec![REQUEST_USER_INPUT_OTHER_LABEL.to_string()];
        }
        let mut out = vec![REQUEST_USER_INPUT_OTHER_LABEL.to_string()];
        if !note.is_empty() && !note.eq_ignore_ascii_case(REQUEST_USER_INPUT_OTHER_LABEL) {
            out.push(format!("user_note: {note}"));
        }
        return out;
    }
    vec![answer_text.to_string()]
}

fn parse_numbered_request_user_input_answer(answer_text: &str) -> Option<(usize, Option<String>)> {
    let trimmed = answer_text.trim();
    let digit_len = trimmed
        .chars()
        .take_while(|ch| ch.is_ascii_digit())
        .map(char::len_utf8)
        .sum::<usize>();
    if digit_len == 0 {
        return None;
    }
    let number = trimmed[..digit_len].parse::<usize>().ok()?;
    let rest = trimmed[digit_len..].trim_start();
    if rest.is_empty() {
        return Some((number, None));
    }
    let note = rest
        .strip_prefix('.')
        .or_else(|| rest.strip_prefix(')'))
        .or_else(|| rest.strip_prefix(':'))
        .or_else(|| rest.strip_prefix('-'))
        .map(str::trim);
    note.map(|note| (number, Some(note.to_string())))
}

fn request_user_input_option_matches(option_label: &str, answer_text: &str) -> bool {
    normalize_request_user_input_option_label(option_label)
        == normalize_request_user_input_option_label(answer_text)
}

fn normalize_request_user_input_option_label(value: &str) -> String {
    let trimmed = value.trim();
    let without_recommended = trimmed
        .strip_suffix("(Recommended)")
        .or_else(|| trimmed.strip_suffix("(recommended)"))
        .map(str::trim)
        .unwrap_or(trimmed);
    without_recommended.to_ascii_lowercase()
}

fn strip_request_user_input_other_prefix(answer_text: &str) -> Option<&str> {
    let trimmed = answer_text.trim();
    let lower = trimmed.to_ascii_lowercase();
    lower
        .strip_prefix("other:")
        .map(|_| trimmed["other:".len()..].trim())
}

fn matches_ignore_ascii_case(left: &str, right: &str) -> bool {
    left.eq_ignore_ascii_case(right)
}

fn model_provider_id_for_backend(backend: AgentBackend) -> &'static str {
    backend.as_setting()
}

fn session_model_selection_from_event(event: &EventRecord) -> Option<SessionModelSelection> {
    if event.event_type != SESSION_MODEL_SELECTION_EVENT {
        return None;
    }
    let backend = event
        .payload
        .get("backend")
        .and_then(serde_json::Value::as_str)
        .and_then(AgentBackend::from_setting)?;
    let provider_model = event
        .payload
        .get("provider_model")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)?;
    let display_model = event
        .payload
        .get("display_model")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| provider_model.clone());
    let account = event
        .payload
        .get("account")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| ACCOUNT_CODEX.to_string());
    let model_provider_id = event
        .payload
        .get("model_provider_id")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string);
    Some(SessionModelSelection {
        display_model,
        provider_model,
        account,
        backend,
        model_provider_id,
    })
}

impl App {
    fn new(mut args: Args) -> Result<Self> {
        args.state_dir = resolve_state_dir(&args.state_dir);
        let (store_tx, store_rx) = mpsc::channel();
        let store = Store::open_with_notifier(&args.state_dir, store_tx)?;
        seed_demo_if_requested(&store, args.seed_demo.as_deref())?;
        let state_cache = AppStateCache::hydrate(&store, &args.browser)?;
        let selected_session_id = if args.select_latest {
            state_cache
                .sessions
                .first()
                .map(|session| session.id.clone())
        } else {
            None
        };
        let surface = args.overlay.map(Into::into).unwrap_or(Surface::Main);
        let current_dir = std::env::current_dir()?;
        let config_overrides = parse_config_overrides(&args.config_overrides)?;
        let config_profile = args.config_profile.as_deref();
        let setup_complete = store.get_setting("setup.complete")?.as_deref() == Some("1");
        let account = store
            .get_setting("account")?
            .unwrap_or_else(|| args.account.clone());
        let agent_backend = store
            .get_setting("agent.backend")?
            .and_then(|value| AgentBackend::from_setting(&value))
            .unwrap_or(args.agent);
        let model_choices =
            model_catalog_for_cwd_with_options(&current_dir, config_profile, &config_overrides)
                .map(|catalog| model_choices_for_catalog(&catalog))
                .unwrap_or_else(|_| fallback_model_choices());
        let stored_model = store.get_setting("model")?;
        let had_stored_model = stored_model.is_some();
        let explicit_model = args
            .model
            .as_deref()
            .map(str::trim)
            .filter(|model| !model.is_empty());
        let configured_model =
            configured_model_for_cwd_with_options(&current_dir, config_profile, &config_overrides)
                .unwrap_or(None);
        let selected_model_override = explicit_model.map(ToOwned::to_owned).or(configured_model);
        let has_model_override = selected_model_override.is_some();
        let (default_display_model, default_provider_model) = if let Some(model) =
            selected_model_override.as_deref()
        {
            display_and_provider_model_for_input(model, &model_choices)
        } else {
            let chatgpt_mode = matches!(agent_backend, AgentBackend::Codex);
            let provider_model = default_model_for_cwd_with_options(
                &current_dir,
                config_profile,
                &config_overrides,
                chatgpt_mode,
            )
            .unwrap_or_else(|_| "gpt-5.5".to_string());
            let display_model = display_model_for_provider_model(&provider_model, &model_choices);
            (display_model, provider_model)
        };
        let model_configured = has_model_override || had_stored_model || setup_complete;
        let model = if has_model_override {
            default_display_model
        } else {
            stored_model.unwrap_or(default_display_model)
        };
        let provider_model = if has_model_override {
            default_provider_model.clone()
        } else {
            store.get_setting("provider.model")?.unwrap_or_else(|| {
                if had_stored_model {
                    provider_model_for_display(&model, &model_choices)
                } else {
                    default_provider_model.clone()
                }
            })
        };
        let configured_model_provider_id = configured_model_provider_id_for_cwd_with_options(
            &current_dir,
            config_profile,
            &config_overrides,
        )
        .unwrap_or(None);
        let stored_model_provider_id = store
            .get_setting("provider.id")?
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty());
        let model_provider_id = configured_model_provider_id
            .or(stored_model_provider_id)
            .or_else(|| Some(model_provider_id_for_backend(agent_backend).to_string()));
        let collaboration_mode = store
            .get_setting(COLLABORATION_MODE_SETTING)?
            .and_then(|value| collaboration_mode_from_setting(&value))
            .unwrap_or_else(|| args.collaboration_mode.into());
        let browser = store
            .get_setting("browser")?
            .unwrap_or_else(|| args.browser.clone());
        let selected_row = 0;
        let _ = had_stored_model;
        let mut app = Self {
            store,
            store_rx,
            state_cache,
            args,
            selected_session_id,
            composer: Composer::default(),
            prompt_history: PromptHistoryState::default(),
            request_input: None,
            surface,
            selected_row,
            setup_complete,
            account,
            model,
            model_configured,
            provider_model,
            model_provider_id,
            model_choices,
            collaboration_mode,
            browser,
            api_key_account: None,
            pending_model_after_auth: None,
            setup_pending_account: None,
            setup_result: None,
            claude_code_oauth: None,
            codex_login: None,
            browser_notice: None,
            status_notice: None,
            agent_backend,
            quit_hint_until: None,
            escape_stop_until: None,
            native_history: NativeHistoryState::default(),
            welcome_anim: welcome::WelcomeAnim::new(),
            live_spinner_frame: 0,
            welcome_logo_rect: std::cell::Cell::new(None),
            composer_input_rect: std::cell::Cell::new(None),
            palette_open: false,
            palette_filter: String::new(),
            history_filter: String::new(),
        };
        app.refresh_cached_projection();
        Ok(app)
    }

    fn workbench_state(&mut self) -> Result<WorkbenchState> {
        Ok(self.refresh_cached_projection().clone())
    }

    fn refresh_cached_projection(&mut self) -> &WorkbenchState {
        let selected_session_id = self.selected_session_id.clone();
        let browser = self.browser.clone();
        let history_tasks_visible = self.history_tasks_are_visible();
        self.state_cache.project_if_needed(
            selected_session_id.as_deref(),
            &browser,
            history_tasks_visible,
        )
    }

    fn drain_store_notifications(&mut self) -> Result<bool> {
        let mut changed = false;
        while let Ok(notification) = self.store_rx.try_recv() {
            changed |= self
                .state_cache
                .apply_notification(&self.store, notification)?;
        }
        if changed {
            self.refresh_cached_projection();
        }
        if self.flush_ready_queued_followups()? {
            changed = true;
            self.state_cache.refresh_all(&self.store)?;
            self.refresh_cached_projection();
        }
        Ok(changed)
    }

    fn drain_oauth_notifications(&mut self) -> Result<bool> {
        let event = match self.claude_code_oauth.as_ref() {
            Some(flow) => match flow.rx.try_recv() {
                Ok(event) => Some(event),
                Err(mpsc::TryRecvError::Empty) => None,
                Err(mpsc::TryRecvError::Disconnected) => Some(ClaudeCodeOAuthEvent {
                    account: flow.account.clone(),
                    result: Err(
                        "OAuth callback listener stopped before sign-in completed.".to_string()
                    ),
                }),
            },
            None => None,
        };
        let Some(event) = event else {
            return Ok(false);
        };
        self.claude_code_oauth = None;
        match event.result {
            Ok(credential) => {
                self.store_claude_code_oauth(&credential)?;
                self.account = event.account.clone();
                self.persist_runtime_settings()?;
                self.show_setup_result(
                    SetupResultKind::Success,
                    event.account,
                    "Connected to Claude Code.".to_string(),
                );
            }
            Err(error) => {
                self.show_setup_result(
                    SetupResultKind::Failure,
                    event.account,
                    format!("Claude Code login failed: {error}"),
                );
            }
        }
        Ok(true)
    }

    fn drain_codex_login_notifications(&mut self) -> Result<bool> {
        let mut events = Vec::new();
        if let Some(flow) = self.codex_login.as_ref() {
            while let Ok(event) = flow.rx.try_recv() {
                events.push(event);
            }
        }
        if events.is_empty() {
            return Ok(false);
        }
        for event in events {
            match event {
                CodexLoginEvent::Output(text) => {
                    if let Some(flow) = self.codex_login.as_mut() {
                        flow.output.push_str(&strip_ansi(&text));
                    }
                }
                CodexLoginEvent::Finished(result) => {
                    let account = self
                        .codex_login
                        .as_ref()
                        .map(|flow| flow.account.clone())
                        .unwrap_or_else(|| ACCOUNT_CODEX.to_string());
                    self.codex_login = None;
                    match result {
                        Ok(auth) => {
                            self.store_codex_auth(&auth)?;
                            self.account = account.clone();
                            self.persist_runtime_settings()?;
                            self.show_setup_result(
                                SetupResultKind::Success,
                                account,
                                "Connected with Codex auth.".to_string(),
                            );
                        }
                        Err(error) => {
                            self.show_setup_result(
                                SetupResultKind::Failure,
                                account,
                                format!("Codex login failed: {error}"),
                            );
                        }
                    }
                }
            }
        }
        Ok(true)
    }

    fn refresh_state_cache_from_store(&mut self) -> Result<bool> {
        let mut changed = self.state_cache.refresh_all(&self.store)?;
        if changed {
            self.refresh_cached_projection();
        }
        if self.flush_ready_queued_followups()? {
            changed = true;
            self.state_cache.refresh_all(&self.store)?;
            self.refresh_cached_projection();
        }
        Ok(changed)
    }

    fn cached_events_for_session(&self, session_id: &str) -> &[EventRecord] {
        self.state_cache.events_for_session(session_id)
    }

    fn pending_queued_followup_events<'a>(
        &self,
        events: &'a [EventRecord],
    ) -> Vec<&'a EventRecord> {
        pending_queued_followup_events_from_events(events)
    }

    pub(crate) fn queued_followup_is_pending(&self, session_id: &str, queued_seq: i64) -> bool {
        self.pending_queued_followup_events(self.cached_events_for_session(session_id))
            .into_iter()
            .any(|event| event.seq == queued_seq)
    }

    fn flush_ready_queued_followups(&mut self) -> Result<bool> {
        let Some(session_id) = self.selected_session_id.clone() else {
            return Ok(false);
        };
        let Some(session) = self.store.load_session(&session_id)? else {
            return Ok(false);
        };
        if session.status.is_active() {
            return Ok(false);
        }
        let events = self.store.events_for_session(&session_id)?;
        let pending = pending_queued_followup_events_from_events(&events);
        if pending.is_empty() {
            return Ok(false);
        }
        let selection = self.session_model_selection_or_current(&session_id)?;
        if !self.ensure_agent_ready_for_selection(&selection)? {
            return Ok(false);
        }
        for queued in pending {
            let mut payload = queued.payload.clone();
            payload["queued_from_seq"] = serde_json::json!(queued.seq);
            self.store
                .append_event(&session_id, "session.followup", payload)?;
            self.store.append_event(
                &session_id,
                SESSION_QUEUED_FOLLOWUP_SENT_EVENT,
                serde_json::json!({ "queued_seq": queued.seq }),
            )?;
        }
        self.status_notice = Some("Sent queued follow-up.".to_string());
        self.start_agent_for_session(session_id)?;
        Ok(true)
    }

    pub(crate) fn message_action_rows(&self) -> Vec<MessageActionRow> {
        let Some(session_id) = self.selected_session_id.as_deref() else {
            return Vec::new();
        };
        let events = self.cached_events_for_session(session_id);
        let mut rows = browser_use_core::rollback_filtered_event_records(events)
            .into_iter()
            .filter(|event| {
                matches!(
                    event.event_type.as_str(),
                    "session.input" | "session.followup"
                )
            })
            .filter_map(|event| {
                event_payload_text(event).map(|text| MessageActionRow {
                    seq: event.seq,
                    kind: MessageActionKind::Submitted,
                    text,
                    followup: event.event_type == "session.followup",
                })
            })
            .collect::<Vec<_>>();
        rows.extend(
            self.pending_queued_followup_events(events)
                .into_iter()
                .filter_map(|event| {
                    event_payload_text(event).map(|text| MessageActionRow {
                        seq: event.seq,
                        kind: MessageActionKind::Queued,
                        text,
                        followup: true,
                    })
                }),
        );
        rows.sort_by_key(|row| row.seq);
        rows
    }

    fn latest_reclaimable_submitted_message(
        &mut self,
    ) -> Result<Option<(String, MessageActionRow, bool)>> {
        if !self.composer.is_empty() {
            return Ok(None);
        }
        self.refresh_state_cache_from_store()?;
        let Some(session_id) = self.selected_session_id.clone() else {
            return Ok(None);
        };
        let Some(session) = self.store.load_session(&session_id)? else {
            return Ok(None);
        };
        if !session.status.is_active() {
            return Ok(None);
        }
        let events = self.store.events_for_session(&session_id)?;
        let filtered = browser_use_core::rollback_filtered_event_records(&events);
        let Some(row) = filtered.iter().rev().find_map(|event| {
            if !matches!(
                event.event_type.as_str(),
                "session.input" | "session.followup"
            ) {
                return None;
            }
            event_payload_text(event).map(|text| MessageActionRow {
                seq: event.seq,
                kind: MessageActionKind::Submitted,
                text,
                followup: event.event_type == "session.followup",
            })
        }) else {
            return Ok(None);
        };
        let pre_output_only = events
            .iter()
            .filter(|event| event.seq > row.seq)
            .all(|event| event_is_pre_output_after_submission(event, row.seq));
        if !pre_output_only {
            return Ok(None);
        }
        let has_prior_submitted_message = filtered.iter().any(|event| {
            event.seq < row.seq
                && matches!(
                    event.event_type.as_str(),
                    "session.input" | "session.followup"
                )
        });
        Ok(Some((session_id, row, has_prior_submitted_message)))
    }

    fn empty_workbench_state_with_failure(&self) -> WorkbenchState {
        let mut state = empty_workbench_state(&self.browser);
        state.failure = Some("Could not load state.".to_string());
        state
    }

    fn history_tasks_are_visible(&self) -> bool {
        self.surface == Surface::History || self.selected_session_id.is_none()
    }

    pub(crate) fn pending_request_user_input(
        &self,
        session_id: &str,
    ) -> Option<PendingRequestUserInput> {
        pending_request_user_input_from_events(self.cached_events_for_session(session_id))
    }

    fn current_pending_request_user_input(&self) -> Option<(String, PendingRequestUserInput)> {
        let session_id = self.selected_session_id.as_deref()?;
        let active = self
            .state_cache
            .sessions
            .iter()
            .find(|session| session.id == session_id)
            .is_some_and(|session| session.status.is_active());
        if !active {
            return None;
        }
        self.pending_request_user_input(session_id)
            .map(|request| (session_id.to_string(), request))
    }

    pub(crate) fn request_input_display_state(
        &self,
        session_id: &str,
        request: &PendingRequestUserInput,
    ) -> RequestUserInputState {
        self.request_input
            .as_ref()
            .filter(|state| {
                state.session_id == session_id
                    && state.call_id == request.call_id
                    && state.turn_id == request.turn_id
            })
            .cloned()
            .unwrap_or_else(|| RequestUserInputState::new(session_id.to_string(), request))
    }

    fn ensure_request_input_state(&mut self, session_id: &str, request: &PendingRequestUserInput) {
        let should_reset = self.request_input.as_ref().is_none_or(|state| {
            state.session_id != session_id
                || state.call_id != request.call_id
                || state.turn_id != request.turn_id
                || state.answers.len() != request.questions.len()
        });
        if should_reset {
            self.request_input = Some(RequestUserInputState::new(session_id.to_string(), request));
        }
        self.clamp_request_input_state(request);
    }

    fn clamp_request_input_state(&mut self, request: &PendingRequestUserInput) {
        let Some(state) = self.request_input.as_mut() else {
            return;
        };
        if state.current_idx >= request.questions.len() {
            state.current_idx = request.questions.len().saturating_sub(1);
        }
        for (idx, answer) in state.answers.iter_mut().enumerate() {
            let count = request
                .questions
                .get(idx)
                .map(request_user_input_option_count)
                .unwrap_or_default();
            if count == 0 {
                answer.option_cursor = None;
                answer.committed_option = None;
                continue;
            }
            if let Some(cursor) = answer.option_cursor {
                answer.option_cursor = Some(cursor.min(count - 1));
            }
            if answer
                .committed_option
                .is_some_and(|selected| selected >= count)
            {
                answer.committed_option = None;
                answer.answer_committed = false;
            }
        }
        let current_has_options = request
            .questions
            .get(state.current_idx)
            .is_some_and(|question| request_user_input_option_count(question) > 0);
        if !current_has_options {
            state.focus = RequestUserInputFocus::Notes;
        }
    }

    fn save_current_request_input_notes(&mut self) {
        let Some(state) = self.request_input.as_mut() else {
            return;
        };
        if state.focus != RequestUserInputFocus::Notes {
            return;
        }
        if let Some(answer) = state.answers.get_mut(state.current_idx) {
            answer.notes = self.composer.input().to_string();
        }
    }

    fn restore_current_request_input_notes(&mut self) {
        let Some(state) = self.request_input.as_ref() else {
            self.composer.clear();
            return;
        };
        if state.focus == RequestUserInputFocus::Notes {
            let notes = state
                .answers
                .get(state.current_idx)
                .map(|answer| answer.notes.clone())
                .unwrap_or_default();
            self.composer.set_input(notes);
        } else {
            self.composer.clear();
        }
    }

    fn request_input_state_has_touched_answer(&self) -> bool {
        self.request_input.as_ref().is_some_and(|state| {
            state.focus == RequestUserInputFocus::Notes
                || state.confirm_unanswered
                || state.answers.iter().any(|answer| {
                    answer.answer_committed
                        || answer.committed_option.is_some()
                        || !answer.notes.trim().is_empty()
                })
        })
    }

    fn move_request_input_question(&mut self, request: &PendingRequestUserInput, next: bool) {
        if request.questions.is_empty() {
            return;
        }
        self.save_current_request_input_notes();
        if let Some(state) = self.request_input.as_mut() {
            let len = request.questions.len();
            let offset = if next { 1 } else { len.saturating_sub(1) };
            state.current_idx = (state.current_idx + offset) % len;
            state.confirm_unanswered = false;
            let current_has_options = request
                .questions
                .get(state.current_idx)
                .is_some_and(|question| request_user_input_option_count(question) > 0);
            state.focus = if current_has_options {
                RequestUserInputFocus::Options
            } else {
                RequestUserInputFocus::Notes
            };
        }
        self.restore_current_request_input_notes();
    }

    fn jump_to_request_input_question(&mut self, request: &PendingRequestUserInput, idx: usize) {
        if idx >= request.questions.len() {
            return;
        }
        self.save_current_request_input_notes();
        if let Some(state) = self.request_input.as_mut() {
            state.current_idx = idx;
            state.confirm_unanswered = false;
            let current_has_options = request
                .questions
                .get(state.current_idx)
                .is_some_and(|question| request_user_input_option_count(question) > 0);
            state.focus = if current_has_options {
                RequestUserInputFocus::Options
            } else {
                RequestUserInputFocus::Notes
            };
        }
        self.restore_current_request_input_notes();
    }

    fn move_request_input_option(&mut self, request: &PendingRequestUserInput, delta: isize) {
        let Some(state) = self.request_input.as_mut() else {
            return;
        };
        let Some(question) = request.questions.get(state.current_idx) else {
            return;
        };
        let count = request_user_input_option_count(question);
        if count == 0 {
            return;
        }
        let Some(answer) = state.answers.get_mut(state.current_idx) else {
            return;
        };
        let current = answer.option_cursor.unwrap_or(0) as isize;
        answer.option_cursor = Some((current + delta).rem_euclid(count as isize) as usize);
        answer.committed_option = None;
        answer.answer_committed = false;
    }

    fn clear_current_request_input_selection(&mut self) {
        let Some(state) = self.request_input.as_mut() else {
            return;
        };
        let Some(answer) = state.answers.get_mut(state.current_idx) else {
            return;
        };
        answer.option_cursor = None;
        answer.committed_option = None;
        answer.answer_committed = false;
        answer.notes.clear();
        self.composer.clear();
    }

    fn commit_current_request_input_answer(&mut self, request: &PendingRequestUserInput) {
        self.save_current_request_input_notes();
        let Some(state) = self.request_input.as_mut() else {
            return;
        };
        let Some(question) = request.questions.get(state.current_idx) else {
            return;
        };
        let Some(answer) = state.answers.get_mut(state.current_idx) else {
            return;
        };
        if request_user_input_option_count(question) > 0 {
            answer.committed_option = answer.option_cursor;
            answer.answer_committed = answer.committed_option.is_some();
        } else {
            answer.answer_committed = !answer.notes.trim().is_empty();
        }
    }

    fn first_unanswered_request_input_index(
        &self,
        request: &PendingRequestUserInput,
    ) -> Option<usize> {
        let state = self.request_input.as_ref()?;
        request
            .questions
            .iter()
            .enumerate()
            .find_map(|(idx, question)| {
                let values = state
                    .answers
                    .get(idx)
                    .map(|draft| request_user_input_state_answer_values(question, draft))
                    .unwrap_or_default();
                values.is_empty().then_some(idx)
            })
    }

    fn request_input_unanswered_count(&self, request: &PendingRequestUserInput) -> usize {
        let Some(state) = self.request_input.as_ref() else {
            return request.questions.len();
        };
        request
            .questions
            .iter()
            .enumerate()
            .filter(|(idx, question)| {
                state
                    .answers
                    .get(*idx)
                    .map(|draft| request_user_input_state_answer_values(question, draft))
                    .unwrap_or_default()
                    .is_empty()
            })
            .count()
    }

    fn open_request_input_unanswered_confirmation(&mut self, request: &PendingRequestUserInput) {
        let unanswered = self.request_input_unanswered_count(request);
        if let Some(state) = self.request_input.as_mut() {
            state.confirm_unanswered = true;
            state.confirm_selected = 0;
        }
        let suffix = if unanswered == 1 {
            "question"
        } else {
            "questions"
        };
        self.status_notice = Some(format!(
            "Submit with {unanswered} unanswered {suffix}? Press Enter to proceed."
        ));
    }

    fn submit_request_input_state(
        &mut self,
        session_id: &str,
        request: &PendingRequestUserInput,
    ) -> Result<()> {
        let Some(state) = self.request_input.clone() else {
            return Ok(());
        };
        let payload = request_user_input_state_response_payload(request, &state);
        self.store
            .append_event(session_id, REQUEST_USER_INPUT_RESPONSE_EVENT, payload)?;
        self.request_input = None;
        self.composer.clear();
        self.status_notice = None;
        Ok(())
    }

    fn advance_or_submit_request_input(
        &mut self,
        session_id: &str,
        request: &PendingRequestUserInput,
    ) -> Result<()> {
        let is_last = self
            .request_input
            .as_ref()
            .is_none_or(|state| state.current_idx + 1 >= request.questions.len());
        if !is_last {
            self.move_request_input_question(request, true);
            return Ok(());
        }
        if self.request_input_unanswered_count(request) > 0 {
            self.open_request_input_unanswered_confirmation(request);
            return Ok(());
        }
        self.submit_request_input_state(session_id, request)
    }

    fn handle_request_input_confirmation_key(
        &mut self,
        session_id: &str,
        request: &PendingRequestUserInput,
        key: KeyEvent,
    ) -> Result<bool> {
        if !self
            .request_input
            .as_ref()
            .is_some_and(|state| state.confirm_unanswered)
        {
            return Ok(false);
        }
        match key.code {
            KeyCode::Esc | KeyCode::Backspace => {
                if let Some(idx) = self.first_unanswered_request_input_index(request) {
                    self.jump_to_request_input_question(request, idx);
                }
                self.status_notice = None;
            }
            KeyCode::Up | KeyCode::Char('k') | KeyCode::Down | KeyCode::Char('j') => {
                if let Some(state) = self.request_input.as_mut() {
                    state.confirm_selected = 1usize.saturating_sub(state.confirm_selected.min(1));
                }
            }
            KeyCode::Char('1') | KeyCode::Char('2') => {
                if let Some(state) = self.request_input.as_mut() {
                    state.confirm_selected = if matches!(key.code, KeyCode::Char('1')) {
                        0
                    } else {
                        1
                    };
                }
            }
            KeyCode::Enter => {
                let proceed = self
                    .request_input
                    .as_ref()
                    .is_none_or(|state| state.confirm_selected == 0);
                if proceed {
                    return self
                        .submit_request_input_state(session_id, request)
                        .map(|_| true);
                }
                if let Some(idx) = self.first_unanswered_request_input_index(request) {
                    self.jump_to_request_input_question(request, idx);
                }
                self.status_notice = None;
            }
            _ => {}
        }
        Ok(true)
    }

    fn handle_request_user_input_key(&mut self, key: KeyEvent) -> Result<bool> {
        if self.surface != Surface::Main || self.is_slash_palette_active() {
            return Ok(false);
        }
        let Some((session_id, request)) = self.current_pending_request_user_input() else {
            self.request_input = None;
            return Ok(false);
        };
        self.ensure_request_input_state(&session_id, &request);
        if self.handle_request_input_confirmation_key(&session_id, &request, key)? {
            return Ok(true);
        }
        let state_uses_composer = self
            .request_input
            .as_ref()
            .is_some_and(|state| state.focus == RequestUserInputFocus::Notes);
        let composer_has_parser_text = !self.composer.is_empty()
            && !state_uses_composer
            && !self.request_input_state_has_touched_answer();
        if composer_has_parser_text {
            return Ok(false);
        }
        match key {
            KeyEvent {
                code: KeyCode::Char(c),
                modifiers,
                ..
            } if modifiers.contains(KeyModifiers::CONTROL) && c.eq_ignore_ascii_case(&'c') => {
                if !self.composer.is_empty() {
                    self.composer.clear();
                    self.save_current_request_input_notes();
                } else {
                    self.cancel_current_task()?;
                    self.request_input = None;
                }
                Ok(true)
            }
            KeyEvent {
                code: KeyCode::Esc, ..
            } => {
                let cleared_notes = self
                    .request_input
                    .as_ref()
                    .is_some_and(|state| state.focus == RequestUserInputFocus::Notes)
                    || !self.composer.is_empty();
                if cleared_notes {
                    if let Some(state) = self.request_input.as_mut() {
                        state.focus = RequestUserInputFocus::Options;
                        if let Some(answer) = state.answers.get_mut(state.current_idx) {
                            answer.notes.clear();
                            answer.answer_committed = false;
                        }
                    }
                    self.composer.clear();
                } else {
                    self.cancel_current_task()?;
                    self.request_input = None;
                }
                Ok(true)
            }
            KeyEvent {
                code: KeyCode::Tab, ..
            } => {
                let focus_is_notes = self
                    .request_input
                    .as_ref()
                    .is_some_and(|state| state.focus == RequestUserInputFocus::Notes);
                if focus_is_notes {
                    self.save_current_request_input_notes();
                    if let Some(state) = self.request_input.as_mut() {
                        state.focus = RequestUserInputFocus::Options;
                    }
                    self.composer.clear();
                } else {
                    if let Some(state) = self.request_input.as_mut() {
                        state.focus = RequestUserInputFocus::Notes;
                    }
                    self.restore_current_request_input_notes();
                }
                Ok(true)
            }
            KeyEvent {
                code: KeyCode::Char('p'),
                modifiers: KeyModifiers::CONTROL,
                ..
            }
            | KeyEvent {
                code: KeyCode::PageUp,
                ..
            }
            | KeyEvent {
                code: KeyCode::Left,
                ..
            } => {
                self.move_request_input_question(&request, false);
                Ok(true)
            }
            KeyEvent {
                code: KeyCode::Char('n'),
                modifiers: KeyModifiers::CONTROL,
                ..
            }
            | KeyEvent {
                code: KeyCode::PageDown,
                ..
            }
            | KeyEvent {
                code: KeyCode::Right,
                ..
            } => {
                self.move_request_input_question(&request, true);
                Ok(true)
            }
            KeyEvent {
                code: KeyCode::Up, ..
            } => {
                self.move_request_input_option(&request, -1);
                Ok(true)
            }
            KeyEvent {
                code: KeyCode::Char('k'),
                ..
            } if !state_uses_composer => {
                self.move_request_input_option(&request, -1);
                Ok(true)
            }
            KeyEvent {
                code: KeyCode::Down,
                ..
            } => {
                self.move_request_input_option(&request, 1);
                Ok(true)
            }
            KeyEvent {
                code: KeyCode::Char('j'),
                ..
            } if !state_uses_composer => {
                self.move_request_input_option(&request, 1);
                Ok(true)
            }
            KeyEvent {
                code: KeyCode::Backspace | KeyCode::Delete,
                ..
            } if !state_uses_composer => {
                self.clear_current_request_input_selection();
                Ok(true)
            }
            KeyEvent {
                code: KeyCode::Char(' '),
                ..
            } if !state_uses_composer => {
                self.commit_current_request_input_answer(&request);
                Ok(true)
            }
            KeyEvent {
                code: KeyCode::Char(ch),
                modifiers,
                ..
            } if !state_uses_composer
                && modifiers.is_empty()
                && ch.is_ascii_digit()
                && ch != '0' =>
            {
                let digit = ch.to_digit(10).unwrap_or_default() as usize;
                if let Some(state) = self.request_input.as_mut() {
                    let current_idx = state.current_idx;
                    if let Some(question) = request.questions.get(current_idx) {
                        let count = request_user_input_option_count(question);
                        if (1..=count).contains(&digit) {
                            if let Some(answer) = state.answers.get_mut(current_idx) {
                                answer.option_cursor = Some(digit - 1);
                            }
                        }
                    }
                }
                self.commit_current_request_input_answer(&request);
                self.advance_or_submit_request_input(&session_id, &request)?;
                Ok(true)
            }
            KeyEvent {
                code: KeyCode::Enter,
                modifiers: KeyModifiers::NONE,
                ..
            } => {
                self.commit_current_request_input_answer(&request);
                self.advance_or_submit_request_input(&session_id, &request)?;
                Ok(true)
            }
            _ if state_uses_composer && self.composer.handle_key(key) => {
                self.save_current_request_input_notes();
                if let Some(state) = self.request_input.as_mut() {
                    let current_idx = state.current_idx;
                    if let Some(answer) = state.answers.get_mut(current_idx) {
                        answer.answer_committed = false;
                    }
                }
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    fn open_surface(&mut self, surface: Surface) {
        self.close_slash_palette();
        self.surface = surface;
        self.selected_row = 0;
        self.history_filter.clear();
        if surface != Surface::Browser {
            self.browser_notice = None;
        }
    }

    fn close_surface(&mut self) {
        self.close_slash_palette();
        if matches!(self.surface, Surface::SetupConfirm | Surface::SetupResult) {
            self.setup_pending_account = None;
            self.setup_result = None;
            self.claude_code_oauth = None;
            self.codex_login = None;
        }
        self.surface = Surface::Main;
        self.selected_row = 0;
        self.history_filter.clear();
        self.browser_notice = None;
    }

    fn submit(&mut self) -> Result<()> {
        if let Some((session_id, request)) = self
            .selected_session_id
            .as_deref()
            .and_then(|id| {
                self.state_cache
                    .sessions
                    .iter()
                    .find(|session| session.id == id && session.status.is_active())
                    .map(|session| session.id.clone())
            })
            .and_then(|session_id| {
                self.pending_request_user_input(&session_id)
                    .map(|request| (session_id, request))
            })
        {
            let text = self.composer.take_trimmed();
            self.dispatch(AppCommand::AnswerRequestUserInput {
                session_id,
                call_id: request.turn_id,
                text,
            })?;
            return Ok(());
        }
        let text = self.composer.input().trim().to_string();
        let has_local_images = self.composer.has_local_images();
        if !has_local_images {
            if let Some(plan_text) = text
                .strip_prefix("/plan")
                .filter(|rest| rest.is_empty() || rest.starts_with(char::is_whitespace))
                .map(str::trim)
            {
                if self.current_task_is_active()?
                    && self.collaboration_mode != CollaborationModeKind::Plan
                {
                    self.status_notice = Some(
                        "Collaboration mode can change after the running turn finishes."
                            .to_string(),
                    );
                    return Ok(());
                }
                self.composer.take_trimmed();
                self.dispatch(AppCommand::SetCollaborationMode(
                    CollaborationModeKind::Plan,
                ))?;
                if !plan_text.is_empty() {
                    self.submit_plain_text(plan_text.to_string())?;
                }
                return Ok(());
            }
        }
        if !has_local_images && text == "/mode" {
            self.composer.take_trimmed();
            self.dispatch(AppCommand::ChangeMode)?;
            return Ok(());
        }
        if text.is_empty() && !has_local_images {
            if let Some(session) = self
                .selected_session_id
                .as_deref()
                .and_then(|id| {
                    self.state_cache
                        .sessions
                        .iter()
                        .find(|session| session.id == id)
                })
                .cloned()
            {
                if session.status == SessionStatus::Failed {
                    self.execute_failed_selection(session.id)?;
                } else if session.status == SessionStatus::Cancelled {
                    self.execute_cancelled_selection()?;
                }
            }
            return Ok(());
        }
        if let Some(session) = self
            .selected_session_id
            .as_deref()
            .and_then(|id| {
                self.state_cache
                    .sessions
                    .iter()
                    .find(|session| session.id == id)
            })
            .cloned()
        {
            let active = session.status.is_active();
            if !active && !self.ensure_agent_ready()? {
                return Ok(());
            }
            let submission = self.take_composer_submission();
            if submission.has_local_images() {
                self.dispatch(AppCommand::SendFollowupSubmission {
                    session_id: session.id,
                    submission,
                })?;
            } else {
                self.dispatch(AppCommand::SendFollowup {
                    session_id: session.id,
                    text: submission.text,
                })?;
            }
            return Ok(());
        }
        if !self.ensure_agent_ready()? {
            return Ok(());
        }
        let submission = self.take_composer_submission();
        if submission.has_local_images() {
            self.dispatch(AppCommand::StartTaskSubmission(submission))?;
        } else {
            self.dispatch(AppCommand::StartTask(submission.text))?;
        }
        Ok(())
    }

    fn take_composer_submission(&mut self) -> UserSubmission {
        let (text, local_images) = self.composer.take_submission();
        UserSubmission { text, local_images }
    }

    fn queue_current_composer_followup(&mut self) -> Result<bool> {
        if self.surface != Surface::Main || self.composer.is_empty() {
            return Ok(false);
        }
        let Some(session) = self
            .selected_session_id
            .as_deref()
            .and_then(|id| {
                self.state_cache
                    .sessions
                    .iter()
                    .find(|session| session.id == id)
            })
            .cloned()
        else {
            return Ok(false);
        };
        if !session.status.is_active() {
            return Ok(false);
        }
        let submission = self.take_composer_submission();
        self.dispatch(AppCommand::QueueFollowupSubmission {
            session_id: session.id,
            submission,
        })?;
        Ok(true)
    }

    fn submit_plain_text(&mut self, text: String) -> Result<()> {
        if let Some(session) = self
            .selected_session_id
            .as_deref()
            .and_then(|id| {
                self.state_cache
                    .sessions
                    .iter()
                    .find(|session| session.id == id)
            })
            .cloned()
        {
            self.dispatch(AppCommand::SendFollowup {
                session_id: session.id,
                text,
            })
        } else {
            self.dispatch(AppCommand::StartTask(text))
        }
    }

    fn ensure_agent_ready(&mut self) -> Result<bool> {
        let selection = self.current_model_selection();
        self.ensure_agent_ready_for_selection(&selection)
    }

    fn ensure_agent_ready_for_selection(
        &mut self,
        selection: &SessionModelSelection,
    ) -> Result<bool> {
        if let Some(notice) = self.auth_notice_for_selection(selection)? {
            self.status_notice = Some(notice);
            self.open_surface(Surface::Account);
            return Ok(false);
        }
        if let Some(notice) = self.browser_notice()? {
            self.status_notice = Some(notice);
            self.start_auth_flow(BROWSER_USE_CLOUD.to_string())?;
            return Ok(false);
        }
        self.status_notice = None;
        Ok(true)
    }

    fn dispatch(&mut self, command: AppCommand) -> Result<()> {
        match command {
            AppCommand::StartTask(text) => {
                self.start_task_submission(UserSubmission::text(text))?;
            }
            AppCommand::StartTaskSubmission(submission) => {
                self.start_task_submission(submission)?;
            }
            AppCommand::SendFollowup { session_id, text } => {
                self.send_followup_submission(session_id, UserSubmission::text(text))?;
            }
            AppCommand::SendFollowupSubmission {
                session_id,
                submission,
            } => {
                self.send_followup_submission(session_id, submission)?;
            }
            AppCommand::QueueFollowupSubmission {
                session_id,
                submission,
            } => {
                self.queue_followup_submission(session_id, submission)?;
            }
            AppCommand::AnswerRequestUserInput {
                session_id,
                call_id,
                text,
            } => {
                let Some(request) = self.pending_request_user_input(&session_id) else {
                    self.status_notice = Some("No pending user-input request.".to_string());
                    return Ok(());
                };
                if request.turn_id != call_id && request.call_id != call_id {
                    self.status_notice =
                        Some("That user-input request is no longer pending.".to_string());
                    return Ok(());
                }
                let payload = request_user_input_response_payload(&request, &text);
                self.store
                    .append_event(&session_id, REQUEST_USER_INPUT_RESPONSE_EVENT, payload)?;
                self.request_input = None;
                self.status_notice = None;
            }
            AppCommand::RetryTask(session_id) => {
                let selection = self.session_model_selection_or_current(&session_id)?;
                if !self.ensure_agent_ready_for_selection(&selection)? {
                    return Ok(());
                }
                self.store.append_event(
                    &session_id,
                    "session.status",
                    serde_json::json!({ "status": "running" }),
                )?;
                self.start_agent_for_session(session_id)?;
            }
            AppCommand::OpenBrowser => self.request_open_browser()?,
            AppCommand::ReconnectBrowser => self.request_reconnect_browser()?,
            AppCommand::NewTask => {
                self.selected_session_id = None;
                self.native_history.reset_with_clear();
                self.close_surface();
            }
            AppCommand::OpenHistory => self.open_surface(Surface::History),
            AppCommand::SelectHistory(session_id) => {
                self.selected_session_id = Some(session_id);
                self.native_history.reset_with_clear();
                self.close_surface();
            }
            AppCommand::ChangeModel => self.open_surface(Surface::Model),
            AppCommand::ChangeMode => self.open_surface(Surface::Mode),
            AppCommand::SetCollaborationMode(mode) => {
                if self.collaboration_mode != mode && self.current_task_is_active()? {
                    self.status_notice = Some(
                        "Collaboration mode can change after the running turn finishes."
                            .to_string(),
                    );
                    return Ok(());
                }
                self.collaboration_mode = mode;
                self.persist_runtime_settings()?;
                self.status_notice = Some(format!(
                    "Collaboration mode set to {}.",
                    collaboration_mode_label(mode)
                ));
            }
            AppCommand::SignIn => self.open_surface(Surface::Account),
            AppCommand::ConfigureTelemetry => self.start_telemetry_entry(),
            AppCommand::ChangeBrowser => self.open_surface(Surface::BrowserSelect),
            AppCommand::Reload => self.request_reexec()?,
            AppCommand::Update => self.run_update()?,
            AppCommand::SaveAccount(account) => self.save_account(account)?,
            AppCommand::SaveModel(index) => self.save_model(index)?,
            AppCommand::SaveBrowser(index) => self.save_browser(index)?,
            AppCommand::SaveAuth(secret) => self.save_auth(secret)?,
            AppCommand::SaveTelemetry(secret) => self.save_telemetry(secret)?,
        }
        self.drain_store_notifications()?;
        Ok(())
    }

    fn start_task_submission(&mut self, submission: UserSubmission) -> Result<()> {
        let selection = self.current_model_selection();
        if !self.ensure_agent_ready_for_selection(&selection)? {
            return Ok(());
        }
        let cwd = std::env::current_dir()?;
        let session = self.store.create_session(None, &cwd)?;
        self.append_session_model_selection(&session.id, &selection)?;
        let options = self.configured_agent_options()?;
        browser_use_core::append_workspace_context_event_with_options(
            &self.store,
            &session,
            &options,
        )?;
        let _ = self.refresh_prompt_history_for(&cwd, &options);
        self.store.append_event(
            &session.id,
            "session.input",
            typed_user_input_payload_for_submission_for_cwd(&submission, &cwd)?,
        )?;
        self.prompt_history.record_submission(&submission.text);
        self.maybe_append_message_history(&session.id, &submission.text, &cwd, &options);
        self.selected_session_id = Some(session.id.clone());
        self.native_history.reset_with_clear();
        self.start_agent_for_session(session.id)?;
        Ok(())
    }

    fn send_followup_submission(
        &mut self,
        session_id: String,
        submission: UserSubmission,
    ) -> Result<()> {
        let active = self
            .store
            .load_session(&session_id)?
            .is_some_and(|session| session.status.is_active());
        if !active {
            let selection = self.session_model_selection_or_current(&session_id)?;
            if !self.ensure_agent_ready_for_selection(&selection)? {
                return Ok(());
            }
        }
        let session = self
            .store
            .load_session(&session_id)?
            .with_context(|| format!("unknown session id: {session_id}"))?;
        let options = self.configured_agent_options().ok();
        if let Some(options) = options.as_ref() {
            let _ = self.refresh_prompt_history_for(Path::new(&session.cwd), options);
        }
        self.store.append_event(
            &session_id,
            "session.followup",
            typed_user_input_payload_for_submission_for_cwd(&submission, &session.cwd)?,
        )?;
        self.prompt_history.record_submission(&submission.text);
        if let Some(options) = options.as_ref() {
            self.maybe_append_message_history(
                &session_id,
                &submission.text,
                Path::new(&session.cwd),
                options,
            );
        }
        if !active {
            self.start_agent_for_session(session_id)?;
        }
        Ok(())
    }

    fn queue_followup_submission(
        &mut self,
        session_id: String,
        submission: UserSubmission,
    ) -> Result<()> {
        let session = self
            .store
            .load_session(&session_id)?
            .with_context(|| format!("unknown session id: {session_id}"))?;
        if !session.status.is_active() {
            return self.send_followup_submission(session_id, submission);
        }
        let mut payload =
            typed_user_input_payload_for_submission_for_cwd(&submission, &session.cwd)?;
        payload["delivery"] = serde_json::json!("after_current_turn");
        self.store
            .append_event(&session_id, SESSION_QUEUED_FOLLOWUP_EVENT, payload)?;
        self.prompt_history.record_submission(&submission.text);
        if let Ok(Some(options)) = self.configured_agent_options().map(Some) {
            self.maybe_append_message_history(
                &session_id,
                &submission.text,
                Path::new(&session.cwd),
                &options,
            );
        }
        self.status_notice = Some("Queued follow-up after the current turn.".to_string());
        Ok(())
    }

    fn start_agent_for_session(&self, session_id: String) -> Result<()> {
        let selection = self.session_model_selection_or_current(&session_id)?;
        if matches!(selection.backend, AgentBackend::None) {
            return Ok(());
        }
        let state_dir = self.args.state_dir.clone();
        let backend = selection.backend;
        let model = selection.provider_model.clone();
        let model_provider_id = selection.model_provider_id.clone();
        let browser = self.browser.clone();
        let collaboration_mode = self.collaboration_mode;
        let config_profile = self.args.config_profile.clone();
        let config_overrides = self.parsed_config_overrides()?;
        let notifier = self.store.notifier();
        thread::Builder::new()
            .name(format!("browser-use-agent-{session_id}"))
            .spawn(move || {
                let failure_state_dir = state_dir.clone();
                let failure_session_id = session_id.clone();
                let failure_notifier = notifier.clone();
                let result = catch_unwind(AssertUnwindSafe(|| {
                    run_agent_thread(
                        state_dir,
                        session_id,
                        backend,
                        model,
                        model_provider_id,
                        browser,
                        collaboration_mode,
                        config_profile,
                        config_overrides,
                        notifier,
                    )
                }));
                match result {
                    Ok(Ok(())) => {}
                    Ok(Err(error)) => eprintln!("agent thread failed: {error:#}"),
                    Err(panic) => record_agent_panic(
                        failure_state_dir,
                        failure_session_id,
                        failure_notifier,
                        panic_payload_message(panic),
                    ),
                }
            })
            .context("spawn agent thread")?;
        Ok(())
    }

    fn complete_demo_result(&mut self) -> Result<()> {
        let Some(id) = self.selected_session_id.clone() else {
            return Ok(());
        };
        self.store.append_event(
            &id,
            "session.done",
            serde_json::json!({"result": "Demo result from the Rust event store.\n\nThe browser task state is now rendered from SQLite."}),
        )?;
        Ok(())
    }

    fn cancel_current_task(&mut self) -> Result<bool> {
        let Some(id) = self.selected_session_id.clone() else {
            return Ok(false);
        };
        if !self.current_task_is_active()? {
            return Ok(false);
        }
        self.store.request_cancel(&id, "stopped from terminal")?;
        cleanup_agent_runtime_state_for_agent_subtree(&self.store, &id)?;
        Ok(true)
    }

    fn current_task_is_active(&self) -> Result<bool> {
        let Some(id) = self.selected_session_id.as_deref() else {
            return Ok(false);
        };
        Ok(self
            .state_cache
            .sessions
            .iter()
            .find(|session| session.id == id)
            .is_some_and(|session| session.status.is_active()))
    }

    fn escape_stop_is_pending(&self) -> bool {
        self.escape_stop_until
            .is_some_and(|until| Instant::now() <= until)
    }

    fn open_message_actions(&mut self) -> Result<()> {
        self.refresh_state_cache_from_store()?;
        let rows = self.message_action_rows();
        if rows.is_empty() {
            self.status_notice = Some("No messages to edit.".to_string());
            self.escape_stop_until = None;
            return Ok(());
        }
        self.close_slash_palette();
        self.surface = Surface::Messages;
        self.selected_row = rows.len().saturating_sub(1);
        self.escape_stop_until = None;
        Ok(())
    }

    fn selected_message_action_row(&self) -> Option<MessageActionRow> {
        let rows = self.message_action_rows();
        rows.get(self.selected_row.min(rows.len().saturating_sub(1)))
            .cloned()
    }

    pub(crate) fn selected_message_action_is_queued(&self) -> bool {
        self.selected_message_action_row()
            .is_some_and(|row| row.kind == MessageActionKind::Queued)
    }

    fn submitted_turns_to_rollback_from(&self, target_seq: i64) -> usize {
        let Some(session_id) = self.selected_session_id.as_deref() else {
            return 1;
        };
        browser_use_core::rollback_filtered_event_records(
            self.cached_events_for_session(session_id),
        )
        .into_iter()
        .filter(|event| {
            event.seq >= target_seq
                && matches!(
                    event.event_type.as_str(),
                    "session.input" | "session.followup"
                )
        })
        .count()
        .max(1)
    }

    fn cancel_queued_followup(&mut self, row: &MessageActionRow) -> Result<()> {
        let Some(session_id) = self.selected_session_id.as_deref() else {
            return Ok(());
        };
        self.store.append_event(
            session_id,
            SESSION_QUEUED_FOLLOWUP_CANCELLED_EVENT,
            serde_json::json!({
                "queued_seq": row.seq,
                "reason": "removed from message selector",
            }),
        )?;
        self.status_notice = Some("Removed queued follow-up.".to_string());
        Ok(())
    }

    fn rollback_submitted_message(&mut self, row: &MessageActionRow, action: &str) -> Result<()> {
        let Some(session_id) = self.selected_session_id.clone() else {
            return Ok(());
        };
        if self.current_task_is_active()? {
            self.cancel_current_task()?;
        }
        let num_turns = self.submitted_turns_to_rollback_from(row.seq);
        self.store.append_event(
            &session_id,
            SESSION_ROLLBACK_EVENT,
            serde_json::json!({
                "num_turns": num_turns,
                "target_seq": row.seq,
                "action": action,
                "source": if action == "take_back" { "tui_escape" } else { "tui_message_selector" },
            }),
        )?;
        self.native_history.reset_with_clear();
        Ok(())
    }

    fn reclaim_latest_submitted_message_before_output(&mut self) -> Result<bool> {
        let Some((_session_id, row, has_prior_submitted_message)) =
            self.latest_reclaimable_submitted_message()?
        else {
            return Ok(false);
        };
        self.rollback_submitted_message(&row, "take_back")?;
        self.composer.set_input(row.text);
        if !has_prior_submitted_message {
            self.selected_session_id = None;
            self.native_history.reset_with_clear();
        }
        self.close_surface();
        self.escape_stop_until = None;
        self.quit_hint_until = None;
        self.status_notice = Some("Message returned to composer.".to_string());
        self.refresh_state_cache_from_store()?;
        Ok(true)
    }

    fn edit_selected_message(&mut self) -> Result<()> {
        let Some(row) = self.selected_message_action_row() else {
            return Ok(());
        };
        match row.kind {
            MessageActionKind::Queued => self.cancel_queued_followup(&row)?,
            MessageActionKind::Submitted => self.rollback_submitted_message(&row, "edit")?,
        }
        self.composer.set_input(row.text);
        self.close_surface();
        self.status_notice = Some("Editing message. Press Enter to send.".to_string());
        Ok(())
    }

    fn remove_selected_message(&mut self) -> Result<()> {
        let Some(row) = self.selected_message_action_row() else {
            return Ok(());
        };
        match row.kind {
            MessageActionKind::Queued => self.cancel_queued_followup(&row)?,
            MessageActionKind::Submitted => {
                self.status_notice =
                    Some("Submitted messages can be edited, not removed.".to_string());
            }
        }
        if row.kind == MessageActionKind::Queued {
            self.close_surface();
        }
        Ok(())
    }

    fn handle_main_escape(&mut self) -> Result<()> {
        if self.escape_stop_is_pending() {
            self.open_message_actions()?;
            self.quit_hint_until = None;
            return Ok(());
        }
        if self.reclaim_latest_submitted_message_before_output()? {
            return Ok(());
        }
        self.escape_stop_until =
            if self.selected_session_id.is_some() && !self.message_action_rows().is_empty() {
                Some(Instant::now() + DOUBLE_ESCAPE_STOP_WINDOW)
            } else {
                None
            };
        self.close_surface();
        Ok(())
    }

    fn handle_key(&mut self, key: KeyEvent) -> Result<bool> {
        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            return Ok(false);
        }
        self.drain_store_notifications()?;
        if key.code != KeyCode::Esc || !key.modifiers.is_empty() {
            self.escape_stop_until = None;
        }
        let quit_requested = matches!(
            key,
            KeyEvent {
                code: KeyCode::Char('q'),
                modifiers: KeyModifiers::CONTROL,
                ..
            }
        );
        if quit_requested {
            return Ok(true);
        }
        if self.prompt_history.search.is_some() {
            self.handle_prompt_history_search_key(key)?;
            self.drain_store_notifications()?;
            return Ok(false);
        }
        if !quit_requested && self.handle_request_user_input_key(key)? {
            self.drain_store_notifications()?;
            return Ok(false);
        }
        if self.surface == Surface::Main
            && !self.is_slash_palette_active()
            && !self.is_first_run_setup_visible()?
        {
            match key {
                _ if !self.composer.has_local_images()
                    && is_prompt_history_search_start_key(key) =>
                {
                    self.begin_prompt_history_search()?;
                    self.drain_store_notifications()?;
                    return Ok(false);
                }
                KeyEvent {
                    code: KeyCode::Up, ..
                }
                | KeyEvent {
                    code: KeyCode::Char('p'),
                    modifiers: KeyModifiers::CONTROL,
                    ..
                } if self.should_prompt_history_handle_older()?
                    && self.navigate_prompt_history_older()? =>
                {
                    self.drain_store_notifications()?;
                    return Ok(false);
                }
                KeyEvent {
                    code: KeyCode::Down,
                    ..
                }
                | KeyEvent {
                    code: KeyCode::Char('n'),
                    modifiers: KeyModifiers::CONTROL,
                    ..
                } if self.prompt_history.is_navigating()
                    && self.navigate_prompt_history_newer()? =>
                {
                    self.drain_store_notifications()?;
                    return Ok(false);
                }
                _ => {}
            }
        }
        match key {
            KeyEvent {
                code: KeyCode::Char(c),
                modifiers,
                ..
            } if self.surface == Surface::Main && is_image_paste_shortcut(c, modifiers) => {
                self.paste_image_from_clipboard();
            }
            KeyEvent {
                code: KeyCode::Char('q'),
                modifiers: KeyModifiers::CONTROL,
                ..
            } => return Ok(true),
            KeyEvent {
                code: KeyCode::Char(c),
                modifiers,
                ..
            } if modifiers.contains(KeyModifiers::CONTROL) && c.eq_ignore_ascii_case(&'c') => {
                if !self.composer.is_empty() {
                    self.composer.clear();
                    self.prompt_history.reset_navigation();
                } else if self.cancel_current_task()? {
                    self.quit_hint_until = None;
                    if self.surface == Surface::Messages {
                        self.close_surface();
                    }
                } else if self
                    .quit_hint_until
                    .is_some_and(|until| Instant::now() <= until)
                {
                    return Ok(true);
                } else {
                    self.quit_hint_until = Some(Instant::now() + Duration::from_millis(1500));
                }
            }
            KeyEvent {
                code: KeyCode::Esc, ..
            } if self.is_slash_palette_active() => {
                self.escape_stop_until = None;
                self.close_slash_palette();
            }
            KeyEvent {
                code: KeyCode::Esc, ..
            } if self.surface == Surface::ApiKey => {
                self.escape_stop_until = None;
                self.cancel_auth_entry();
            }
            KeyEvent {
                code: KeyCode::Esc, ..
            } if self.surface == Surface::Telemetry => {
                self.escape_stop_until = None;
                self.cancel_secret_entry();
            }
            KeyEvent {
                code: KeyCode::Esc, ..
            } if self.surface == Surface::Main => self.handle_main_escape()?,
            // History: first Esc clears the live filter; a second Esc (with the
            // filter already empty) closes the popup like every other surface.
            KeyEvent {
                code: KeyCode::Esc, ..
            } if self.surface == Surface::History && !self.history_filter.is_empty() => {
                self.escape_stop_until = None;
                self.history_filter.clear();
                self.selected_row = 0;
            }
            KeyEvent {
                code: KeyCode::Esc, ..
            } => {
                self.escape_stop_until = None;
                self.close_surface();
            }
            KeyEvent {
                code: KeyCode::Up, ..
            } if self.is_first_run_setup_visible()? => self.move_selection(-1)?,
            KeyEvent {
                code: KeyCode::Down,
                ..
            } if self.is_first_run_setup_visible()? => self.move_selection(1)?,
            KeyEvent {
                code: KeyCode::Enter,
                modifiers: KeyModifiers::NONE,
                ..
            } if self.is_first_run_setup_visible()? => self.execute_first_run_setup_selection()?,
            _ if self.is_first_run_setup_visible()? => {}
            KeyEvent {
                code: KeyCode::BackTab,
                ..
            } if self.surface == Surface::Main => {
                if self.current_task_is_active()? {
                    self.status_notice = Some(
                        "Collaboration mode can change after the running turn finishes."
                            .to_string(),
                    );
                } else {
                    self.dispatch(AppCommand::SetCollaborationMode(next_collaboration_mode(
                        self.collaboration_mode,
                    )))?;
                }
            }
            KeyEvent {
                code: KeyCode::Tab, ..
            } if self.queue_current_composer_followup()? => {}
            KeyEvent {
                code: KeyCode::Tab, ..
            } => self.open_surface(Surface::History),
            KeyEvent {
                code: KeyCode::F(1),
                ..
            } => {}
            KeyEvent {
                code: KeyCode::F(2),
                ..
            } => self.open_surface(Surface::Browser),
            KeyEvent {
                code: KeyCode::Char('e'),
                modifiers: KeyModifiers::CONTROL,
                ..
            } if self.composer.is_empty() => self.open_surface(Surface::Developer),
            // `r` resumes the selected history row, but only when the live
            // filter is empty — otherwise it would steal a perfectly legal
            // letter mid-search ("foo<r>bar" → "fooba").
            KeyEvent {
                code: KeyCode::Char('r'),
                modifiers: KeyModifiers::NONE,
                ..
            } if self.surface == Surface::History && self.history_filter.is_empty() => {
                self.resume_selected_history()?
            }
            KeyEvent {
                code: KeyCode::Up, ..
            } if self.is_slash_palette_active() => self.move_slash_palette_selection(-1),
            KeyEvent {
                code: KeyCode::Down,
                ..
            } if self.is_slash_palette_active() => self.move_slash_palette_selection(1),
            KeyEvent {
                code: KeyCode::Up, ..
            } if self.surface == Surface::Main
                && !(self.composer.is_empty() && self.main_selection_count()? > 0)
                && self.handle_main_composer_key(key) => {}
            KeyEvent {
                code: KeyCode::Down,
                ..
            } if self.surface == Surface::Main
                && !(self.composer.is_empty() && self.main_selection_count()? > 0)
                && self.handle_main_composer_key(key) => {}
            KeyEvent {
                code: KeyCode::Up, ..
            } if self.surface != Surface::Main
                || self.is_first_run_setup_visible()?
                || (self.composer.is_empty() && self.main_selection_count()? > 0) =>
            {
                self.move_selection(-1)?
            }
            KeyEvent {
                code: KeyCode::Down,
                ..
            } if self.surface != Surface::Main
                || self.is_first_run_setup_visible()?
                || (self.composer.is_empty() && self.main_selection_count()? > 0) =>
            {
                self.move_selection(1)?
            }
            KeyEvent {
                code: KeyCode::Up, ..
            } if self.surface == Surface::Main => {}
            KeyEvent {
                code: KeyCode::Down,
                ..
            } if self.surface == Surface::Main => {}
            KeyEvent {
                code: KeyCode::Backspace | KeyCode::Delete,
                ..
            } if self.surface == Surface::Messages => self.remove_selected_message()?,
            KeyEvent {
                code: KeyCode::Char('x') | KeyCode::Char('d'),
                modifiers: KeyModifiers::NONE,
                ..
            } if self.surface == Surface::Messages => self.remove_selected_message()?,
            KeyEvent {
                code: KeyCode::Char('e'),
                modifiers: KeyModifiers::NONE,
                ..
            } if self.surface == Surface::Messages => self.edit_selected_message()?,
            KeyEvent {
                code: KeyCode::Enter,
                modifiers: KeyModifiers::NONE,
                ..
            } if self.is_slash_palette_active() => {
                if self.execute_slash_palette_selection()? {
                    return Ok(true);
                }
            }
            KeyEvent {
                code: KeyCode::Enter,
                modifiers: KeyModifiers::NONE,
                ..
            } if self.surface != Surface::Main => self.execute_surface_selection()?,
            KeyEvent {
                code: KeyCode::Enter,
                modifiers: KeyModifiers::NONE,
                ..
            } => self.submit()?,
            _ if matches!(self.surface, Surface::ApiKey | Surface::Telemetry)
                && self.handle_api_key_key(key) => {}
            // A leading `/` opens the slash palette popup. Once the composer
            // has text, slash is regular prompt input.
            KeyEvent {
                code: KeyCode::Char('/'),
                modifiers: KeyModifiers::NONE,
                ..
            } if self.surface == Surface::Main
                && self.composer.is_empty()
                && !self.palette_open =>
            {
                self.open_slash_palette();
            }
            // While the palette is open every typed character is appended
            // to its filter (printable ASCII only — control sequences fall
            // through to other handlers). Backspace pops a character; the
            // popup stays open even when the filter is empty.
            KeyEvent { .. } if self.is_slash_palette_active() && is_popup_clear_key(key) => {
                self.palette_filter.clear();
                self.clamp_slash_palette_selection();
            }
            KeyEvent {
                code: KeyCode::Char(ch),
                modifiers,
                ..
            } if self.is_slash_palette_active()
                && !modifiers.contains(KeyModifiers::CONTROL)
                && !modifiers.contains(KeyModifiers::ALT) =>
            {
                self.palette_filter.push(ch);
                self.clamp_slash_palette_selection();
            }
            KeyEvent {
                code: KeyCode::Backspace,
                ..
            } if self.is_slash_palette_active() => {
                self.palette_filter.pop();
                self.clamp_slash_palette_selection();
            }
            // History live filter: typed printable chars extend the substring
            // filter, Backspace shrinks it, Ctrl-U/Cmd-Backspace clears it.
            // Selection resets to the top of the filtered list whenever the
            // filter changes so the highlight is never pointing at a row that
            // just got filtered out.
            KeyEvent { .. }
                if self.surface == Surface::History && is_popup_clear_key(key) =>
            {
                self.history_filter.clear();
                self.selected_row = 0;
            }
            KeyEvent {
                code: KeyCode::Char(ch),
                modifiers,
                ..
            } if self.surface == Surface::History
                && !modifiers.contains(KeyModifiers::CONTROL)
                && !modifiers.contains(KeyModifiers::ALT)
                && !modifiers.intersects(
                    KeyModifiers::SUPER | KeyModifiers::HYPER | KeyModifiers::META,
                ) =>
            {
                self.history_filter.push(ch);
                self.selected_row = 0;
            }
            KeyEvent {
                code: KeyCode::Backspace,
                ..
            } if self.surface == Surface::History => {
                self.history_filter.pop();
                self.selected_row = 0;
            }
            // PgUp/PgDn page through the visible history rows by a fixed step
            // (the popup body height varies but never exceeds ~26 lines, so a
            // 10-row jump is comfortable without overshooting on small terms).
            KeyEvent {
                code: KeyCode::PageUp,
                ..
            } if self.surface == Surface::History => {
                let count = self.selectable_row_count()?;
                if count > 0 {
                    self.selected_row = self.selected_row.saturating_sub(10);
                }
            }
            KeyEvent {
                code: KeyCode::PageDown,
                ..
            } if self.surface == Surface::History => {
                let count = self.selectable_row_count()?;
                if count > 0 {
                    self.selected_row = (self.selected_row + 10).min(count - 1);
                }
            }
            _ if self.surface == Surface::Main && self.handle_main_composer_key(key) => {}
            KeyEvent {
                code: KeyCode::Char('d'),
                modifiers: KeyModifiers::CONTROL,
                ..
            } => self.complete_demo_result()?,
            _ => {}
        }
        self.drain_store_notifications()?;
        Ok(false)
    }

    fn handle_paste(&mut self, text: &str) {
        if self.is_slash_palette_active() {
            self.palette_filter.push_str(text);
            self.clamp_slash_palette_selection();
            return;
        }
        if let Some(search) = self.prompt_history.search.as_mut() {
            search.query.push_str(text);
            let _ = self.update_prompt_history_search_matches();
            return;
        }
        if self.is_first_run_setup_visible().unwrap_or(false) {
            return;
        }
        match self.surface {
            Surface::Main => {
                if self.composer.insert_paste(text) {
                    self.prompt_history.reset_navigation();
                }
            }
            Surface::ApiKey | Surface::Telemetry => {
                self.composer.insert_paste(text);
                self.selected_row = 0;
            }
            _ => {}
        }
    }

    fn paste_image_from_clipboard(&mut self) {
        match clipboard_paste::paste_image_to_temp_png() {
            Ok((path, _info)) => {
                self.composer.attach_image(path);
                self.prompt_history.reset_navigation();
            }
            Err(error) => {
                self.status_notice = Some(format!("Failed to paste image: {error}"));
            }
        }
    }

    fn is_first_run_setup_visible(&self) -> Result<bool> {
        Ok(!self.setup_complete
            && self.surface == Surface::Main
            && self.selected_session_id.is_none()
            && self.composer.is_empty())
    }

    /// True when the centered welcome screen is showing — drives the
    /// animation-tick redraw so the BU logo can spin while idle.
    fn is_welcome_surface(&self) -> bool {
        self.surface == Surface::Main && self.selected_session_id.is_none()
    }

    fn should_capture_welcome_mouse(&self) -> bool {
        self.is_welcome_surface()
            && self.composer.is_empty()
            && !self.is_slash_palette_active()
            && self.welcome_logo_rect.get().is_some()
    }

    fn should_capture_mouse(&self) -> bool {
        self.should_capture_welcome_mouse()
            || (self.surface == Surface::Main
                && !self.is_slash_palette_active()
                && self.composer_input_rect.get().is_some())
    }

    fn handle_composer_click(&mut self, column: u16, row: u16) -> bool {
        if self.surface != Surface::Main || self.is_slash_palette_active() {
            return false;
        }
        let Some(rect) = self.composer_input_rect.get() else {
            return false;
        };
        if column < rect.x
            || column >= rect.x.saturating_add(rect.width)
            || row < rect.y
            || row >= rect.y.saturating_add(rect.height)
        {
            return false;
        }
        self.composer.set_cursor_from_wrapped_position(
            rect.height.max(1) as usize,
            rect.width as usize,
            row.saturating_sub(rect.y) as usize,
            column.saturating_sub(rect.x) as usize,
        )
    }

    fn handle_welcome_logo_click(&mut self, column: u16, row: u16) -> bool {
        if !self.should_capture_welcome_mouse() {
            return false;
        }
        let Some(rect) = self.welcome_logo_rect.get() else {
            return false;
        };
        if column < rect.x
            || column >= rect.x.saturating_add(rect.width)
            || row < rect.y
            || row >= rect.y.saturating_add(rect.height)
        {
            return false;
        }
        self.welcome_anim.throw();
        true
    }

    fn execute_surface_selection(&mut self) -> Result<()> {
        match self.surface {
            Surface::History => {
                let state = self.workbench_state()?.clone();
                if let Some(session_id) = self.selected_history_session_id(&state) {
                    self.dispatch(AppCommand::SelectHistory(session_id))?;
                }
            }
            Surface::Setup => self.execute_first_run_setup_selection()?,
            Surface::SetupConfirm => self.execute_setup_confirm_selection()?,
            Surface::SetupResult => self.execute_setup_result_selection()?,
            Surface::Account => {
                let account = ACCOUNT_CHOICES
                    .get(
                        self.selected_row
                            .min(ACCOUNT_CHOICES.len().saturating_sub(1)),
                    )
                    .unwrap_or(&ACCOUNT_CHOICES[0])
                    .to_string();
                self.dispatch(AppCommand::SaveAccount(account))?;
            }
            Surface::ApiKey => match self.selected_row.min(1) {
                0 => {
                    let secret = self.composer.take_trimmed();
                    self.dispatch(AppCommand::SaveAuth(secret))?;
                }
                _ => self.cancel_auth_entry(),
            },
            Surface::Telemetry => match self.selected_row.min(1) {
                0 => {
                    let secret = self.composer.take_trimmed();
                    self.dispatch(AppCommand::SaveTelemetry(secret))?;
                }
                _ => self.cancel_secret_entry(),
            },
            Surface::Model => {
                let model_index = self
                    .selected_row
                    .min(self.model_choices.len().saturating_sub(1));
                self.dispatch(AppCommand::SaveModel(model_index))?;
            }
            Surface::Mode => {
                let mode = match self.selected_row.min(1) {
                    0 => CollaborationModeKind::Default,
                    _ => CollaborationModeKind::Plan,
                };
                self.close_surface();
                self.dispatch(AppCommand::SetCollaborationMode(mode))?;
            }
            Surface::Browser => match self.selected_row.min(2) {
                0 => self.dispatch(AppCommand::OpenBrowser)?,
                1 => self.dispatch(AppCommand::ReconnectBrowser)?,
                _ => self.dispatch(AppCommand::ChangeBrowser)?,
            },
            Surface::BrowserSelect => {
                self.dispatch(AppCommand::SaveBrowser(self.selected_row))?;
            }
            Surface::Messages => self.edit_selected_message()?,
            Surface::Developer => match self.selected_row.min(1) {
                0 => self.dispatch(AppCommand::ConfigureTelemetry)?,
                _ => self.close_surface(),
            },
            Surface::Main => {
                self.close_surface();
            }
        }
        Ok(())
    }

    fn execute_first_run_setup_selection(&mut self) -> Result<()> {
        let idx = self
            .selected_row
            .min(ACCOUNT_CHOICES.len().saturating_sub(1));
        let account = ACCOUNT_CHOICES[idx].to_string();
        self.setup_pending_account = Some(account);
        self.setup_result = None;
        self.open_surface(Surface::SetupConfirm);
        Ok(())
    }

    fn execute_setup_confirm_selection(&mut self) -> Result<()> {
        if self.selected_row.min(1) == 1 {
            self.setup_pending_account = None;
            self.close_surface();
            return Ok(());
        }
        let Some(account) = self.setup_pending_account.clone() else {
            self.close_surface();
            return Ok(());
        };
        if account == ACCOUNT_CODEX {
            self.start_codex_auth(account)?;
        } else if is_claude_code_account(&account) {
            self.account = account.clone();
            self.persist_runtime_settings()?;
            if self.account_ready(&account)? {
                self.show_claude_code_setup_result(account)?;
            } else {
                self.start_claude_code_oauth(account)?;
            }
        } else {
            self.start_auth_flow(account)?;
        }
        Ok(())
    }

    fn execute_setup_result_selection(&mut self) -> Result<()> {
        let Some(result) = self.setup_result.clone() else {
            self.close_surface();
            return Ok(());
        };
        match result.kind {
            SetupResultKind::Success => self.continue_after_setup_success(result.account),
            SetupResultKind::Failure if self.selected_row.min(1) == 0 => {
                if result.account == ACCOUNT_CODEX {
                    self.start_codex_auth(result.account)?;
                } else if is_claude_code_account(&result.account) {
                    self.start_claude_code_oauth(result.account)?;
                } else {
                    self.start_auth_flow(result.account)?;
                }
                Ok(())
            }
            SetupResultKind::Pending if self.selected_row.min(1) == 0 => {
                if result.account == ACCOUNT_CODEX {
                    self.reopen_codex_device_auth_url();
                } else {
                    self.reopen_claude_code_oauth_url();
                }
                Ok(())
            }
            SetupResultKind::Pending => {
                self.claude_code_oauth = None;
                self.codex_login = None;
                self.setup_result = None;
                self.setup_pending_account = None;
                self.close_surface();
                Ok(())
            }
            SetupResultKind::Failure => {
                self.setup_result = None;
                self.setup_pending_account = None;
                self.close_surface();
                Ok(())
            }
        }
    }

    fn show_claude_code_setup_result(&mut self, account: String) -> Result<()> {
        if self.account_ready(&account)? {
            self.show_setup_result(
                SetupResultKind::Success,
                account,
                "Connected to Claude Code.".to_string(),
            );
        } else {
            self.show_setup_result(
                SetupResultKind::Failure,
                account,
                "Could not find a Claude Code login.".to_string(),
            );
        }
        Ok(())
    }

    fn start_codex_auth(&mut self, account: String) -> Result<()> {
        if self.account_ready(&account)? {
            self.account = account.clone();
            self.persist_runtime_settings()?;
            self.show_setup_result(
                SetupResultKind::Success,
                account,
                "Connected with Codex auth.".to_string(),
            );
        } else {
            self.start_codex_device_login(account)?;
        }
        Ok(())
    }

    fn show_setup_result(&mut self, kind: SetupResultKind, account: String, message: String) {
        self.setup_result = Some(SetupResult {
            kind,
            account: account.clone(),
            message,
        });
        self.setup_pending_account = Some(account);
        self.status_notice = None;
        self.open_surface(Surface::SetupResult);
    }

    fn continue_after_setup_success(&mut self, account: String) -> Result<()> {
        self.setup_result = None;
        self.setup_pending_account = None;
        self.account = account;
        if let Some(index) = self.pending_model_after_auth.take() {
            return self.save_model(index);
        }
        self.advance_after_auth()
    }

    fn resume_selected_history(&mut self) -> Result<()> {
        let state = self.workbench_state()?.clone();
        if let Some(session_id) = self.selected_history_session_id(&state) {
            self.dispatch(AppCommand::SelectHistory(session_id))?;
        }
        Ok(())
    }

    /// Indices into `WorkbenchState::history` for rows visible under the
    /// current `history_filter` (case-insensitive substring match against the
    /// task text). When the filter is empty this returns every index.
    pub(crate) fn history_visible_indices(&mut self) -> Result<Vec<usize>> {
        let state = self.workbench_state()?;
        Ok(history_visible_indices_for(&state, &self.history_filter))
    }

    /// The session id of the currently highlighted row, after filter is
    /// applied. Returns `None` when the filtered list is empty.
    fn selected_history_session_id(&self, state: &WorkbenchState) -> Option<String> {
        let indices = history_visible_indices_for(state, &self.history_filter);
        if indices.is_empty() {
            return None;
        }
        let pick = self.selected_row.min(indices.len() - 1);
        let row = state.history.get(indices[pick])?;
        Some(row.session_id.clone())
    }

    pub(crate) fn history_filter(&self) -> &str {
        &self.history_filter
    }

    fn execute_failed_selection(&mut self, session_id: String) -> Result<()> {
        let state = self.workbench_state()?;
        let error = state.failure.as_deref().unwrap_or_default();
        match self.selected_row.min(3) {
            0 if error.to_ascii_lowercase().contains("browser") => {
                self.open_surface(Surface::Browser)
            }
            0 if self.auth_notice()?.is_some() => self.open_surface(Surface::Account),
            0 => self.dispatch(AppCommand::RetryTask(session_id))?,
            1 if error.to_ascii_lowercase().contains("browser") => {
                self.open_surface(Surface::BrowserSelect)
            }
            1 => self.open_surface(Surface::Model),
            2 => self.dispatch(AppCommand::RetryTask(session_id))?,
            _ => self.dispatch(AppCommand::NewTask)?,
        }
        Ok(())
    }

    fn execute_cancelled_selection(&mut self) -> Result<()> {
        match self.selected_row.min(2) {
            0 => {}
            1 => self.dispatch(AppCommand::NewTask)?,
            _ => self.dispatch(AppCommand::OpenHistory)?,
        }
        Ok(())
    }

    fn execute_palette_action(&mut self, action: PaletteAction) -> Result<bool> {
        match action {
            PaletteAction::NewTask => self.dispatch(AppCommand::NewTask)?,
            PaletteAction::ChangeBrowser => self.dispatch(AppCommand::ChangeBrowser)?,
            PaletteAction::ChangeMode => self.dispatch(AppCommand::ChangeMode)?,
            PaletteAction::PlanMode => self.dispatch(AppCommand::SetCollaborationMode(
                CollaborationModeKind::Plan,
            ))?,
            PaletteAction::PreviousWork => self.dispatch(AppCommand::OpenHistory)?,
            PaletteAction::ChooseModel => self.dispatch(AppCommand::ChangeModel)?,
            PaletteAction::Authenticate => self.dispatch(AppCommand::SignIn)?,
            PaletteAction::Reload => self.dispatch(AppCommand::Reload)?,
            PaletteAction::Update => self.dispatch(AppCommand::Update)?,
            PaletteAction::Exit => return Ok(true),
        }
        Ok(false)
    }

    fn run_update(&mut self) -> Result<()> {
        self.status_notice = Some("Checking for browser-use terminal updates...".to_string());
        product_analytics::capture_async(
            &self.store,
            "bu:tui update started",
            serde_json::json!({ "surface": "tui" }),
        );
        match run_update_installer() {
            Ok(message) => {
                self.status_notice = Some(message);
                product_analytics::capture_async(
                    &self.store,
                    "bu:tui update completed",
                    serde_json::json!({ "surface": "tui" }),
                );
            }
            Err(error) => {
                self.status_notice = Some(format!("Update failed: {error:#}"));
                product_analytics::capture_async(
                    &self.store,
                    "bu:tui update failed",
                    serde_json::json!({ "surface": "tui" }),
                );
            }
        }
        Ok(())
    }

    fn request_reexec(&mut self) -> Result<()> {
        self.status_notice = Some("Reloading browser-use terminal...".to_string());
        request_process_reexec()
    }

    fn save_account(&mut self, account: String) -> Result<()> {
        if account == ACCOUNT_CODEX {
            self.start_codex_auth(account)?;
            return Ok(());
        }
        self.account = account.clone();
        self.start_auth_flow(account)?;
        Ok(())
    }

    fn models_for_account(&self, account: &str) -> Vec<usize> {
        self.model_choices
            .iter()
            .enumerate()
            .filter(|(_, choice)| choice.account == account)
            .map(|(idx, _)| idx)
            .collect()
    }

    fn default_model_for_account(&self, account: &str) -> Option<usize> {
        self.models_for_account(account).into_iter().next()
    }

    fn advance_after_auth(&mut self) -> Result<()> {
        if let Some(index) = self.default_model_for_account(&self.account) {
            return self.save_model(index);
        }
        self.selected_row = 0;
        self.open_surface(Surface::Model);
        Ok(())
    }

    fn save_model(&mut self, index: usize) -> Result<()> {
        let choice = self
            .model_choices
            .get(index.min(self.model_choices.len().saturating_sub(1)))
            .cloned()
            .or_else(|| fallback_model_choices().into_iter().next())
            .context("no model choices available")?;
        self.model = choice.display.clone();
        self.account = choice.account.to_string();
        self.provider_model = choice.provider_model.clone();
        self.agent_backend = choice.backend;
        self.model_provider_id = Some(model_provider_id_for_backend(choice.backend).to_string());
        self.model_configured = true;
        self.track_model_selected();
        if self.account == ACCOUNT_CODEX && !self.has_codex_login()? {
            self.pending_model_after_auth = Some(index);
            self.start_codex_device_login(self.account.clone())?;
            return Ok(());
        }
        self.persist_runtime_settings()?;
        if !self.account_ready(&self.account)? {
            self.pending_model_after_auth = Some(index);
            self.start_auth_flow(self.account.clone())?;
            return Ok(());
        }
        let completing_setup = !self.setup_complete;
        if completing_setup {
            if self.browser == BROWSER_USE_CLOUD && !self.browser_use_cloud_key_ready()? {
                self.browser = BROWSER_LOCAL_CHROME.to_string();
            }
            self.complete_setup()?;
            self.persist_runtime_settings()?;
            self.status_notice = None;
        } else {
            self.status_notice = Some(format!("Model set to {}.", self.model));
        }
        if let Some(session_id) = self.selected_session_id.as_deref() {
            if self
                .store
                .load_session(session_id)?
                .is_some_and(|session| !session.status.is_active())
            {
                self.append_session_model_selection(session_id, &self.current_model_selection())?;
            }
        }
        self.close_surface();
        Ok(())
    }

    fn current_model_selection(&self) -> SessionModelSelection {
        SessionModelSelection {
            display_model: self.model.clone(),
            provider_model: self.provider_model.clone(),
            account: self.account.clone(),
            backend: self.agent_backend,
            model_provider_id: self.model_provider_id.clone(),
        }
    }

    fn parsed_config_overrides(&self) -> Result<ConfigOverrides> {
        parse_config_overrides(&self.args.config_overrides)
    }

    fn configured_agent_options(&self) -> Result<AgentRunOptions> {
        let mut options = AgentRunOptions::default()
            .with_collaboration_mode(self.collaboration_mode)
            .with_model_compaction(true);
        if let Some(profile) = self.args.config_profile.as_ref() {
            options = options.with_config_profile(profile.clone());
        }
        let config_overrides = self.parsed_config_overrides()?;
        if !config_overrides.is_empty() {
            options = options.with_config_overrides(config_overrides);
        }
        Ok(options)
    }

    fn maybe_append_message_history(
        &self,
        session_id: &str,
        text: &str,
        cwd: &Path,
        options: &AgentRunOptions,
    ) {
        #[cfg(not(test))]
        {
            let session_id = session_id.to_string();
            let text = text.to_string();
            let cwd = cwd.to_path_buf();
            let options = options.clone();
            std::thread::spawn(move || {
                let _ = browser_use_core::append_message_history_entry_for_cwd(
                    &text,
                    &session_id,
                    &cwd,
                    &options,
                );
            });
        }
        #[cfg(test)]
        {
            let _ = (session_id, text, cwd, options);
        }
    }

    fn prompt_history_config(&self) -> Result<Option<MessageHistoryConfig>> {
        let cwd = std::env::current_dir()?;
        let options = self.configured_agent_options()?;
        browser_use_core::message_history_config_for_cwd_with_options(&cwd, &options)
    }

    fn refresh_prompt_history(&mut self) -> Result<Option<MessageHistoryConfig>> {
        let config = self.prompt_history_config()?;
        self.prompt_history
            .refresh_persistent_metadata(config.as_ref());
        Ok(config)
    }

    fn refresh_prompt_history_for(
        &mut self,
        cwd: &Path,
        options: &AgentRunOptions,
    ) -> Result<Option<MessageHistoryConfig>> {
        let config = browser_use_core::message_history_config_for_cwd_with_options(cwd, options)?;
        self.prompt_history
            .refresh_persistent_metadata(config.as_ref());
        Ok(config)
    }

    fn should_prompt_history_handle_older(&mut self) -> Result<bool> {
        if self.surface != Surface::Main || self.is_slash_palette_active() {
            return Ok(false);
        }
        if self.composer.has_local_images() {
            return Ok(false);
        }
        if self.is_first_run_setup_visible()? {
            return Ok(false);
        }
        let _ = self.refresh_prompt_history()?;
        let text = self.composer.input().to_string();
        if text.is_empty() && self.main_selection_count()? > 0 {
            return Ok(false);
        }
        Ok(self
            .prompt_history
            .should_handle_navigation(&text, self.composer.cursor_is_at_text_boundary()))
    }

    fn navigate_prompt_history_older(&mut self) -> Result<bool> {
        let config = self.refresh_prompt_history()?;
        let total_entries = self.prompt_history.total_entries();
        if total_entries == 0 {
            return Ok(false);
        }
        let mut next_index = match self.prompt_history.nav_index {
            Some(index) if index > 0 => index - 1,
            Some(_) => return Ok(true),
            None => {
                self.prompt_history.nav_draft = Some(self.composer.input().to_string());
                total_entries - 1
            }
        };
        let entry = loop {
            if let Some(entry) = self.prompt_history.entry_at(next_index, config.as_ref()) {
                break entry;
            }
            if next_index == 0 {
                return Ok(false);
            }
            next_index -= 1;
        };
        self.prompt_history.nav_index = Some(next_index);
        self.prompt_history.last_history_text = Some(entry.clone());
        self.composer.set_input(entry);
        Ok(true)
    }

    fn navigate_prompt_history_newer(&mut self) -> Result<bool> {
        let Some(index) = self.prompt_history.nav_index else {
            return Ok(false);
        };
        let config = self.refresh_prompt_history()?;
        let total_entries = self.prompt_history.total_entries();
        if index + 1 < total_entries {
            let next_index = index + 1;
            let Some(entry) = self.prompt_history.entry_at(next_index, config.as_ref()) else {
                return Ok(false);
            };
            self.prompt_history.nav_index = Some(next_index);
            self.prompt_history.last_history_text = Some(entry.clone());
            self.composer.set_input(entry);
        } else {
            let draft = self.prompt_history.nav_draft.take().unwrap_or_default();
            self.prompt_history.nav_index = None;
            self.prompt_history.last_history_text = None;
            self.composer.set_input(draft);
        }
        Ok(true)
    }

    fn begin_prompt_history_search(&mut self) -> Result<()> {
        let _ = self.refresh_prompt_history()?;
        self.close_slash_palette();
        self.prompt_history.reset_navigation();
        self.prompt_history.search = Some(PromptHistorySearchState {
            query: String::new(),
            draft: self.composer.input().to_string(),
            matches: Vec::new(),
            selected: None,
        });
        Ok(())
    }

    fn handle_prompt_history_search_key(&mut self, key: KeyEvent) -> Result<bool> {
        if self.prompt_history.search.is_none() {
            return Ok(false);
        }
        if is_prompt_history_search_start_key(key)
            || matches!(
                key,
                KeyEvent {
                    code: KeyCode::Up,
                    ..
                }
            )
        {
            self.move_prompt_history_search_selection(1);
            return Ok(true);
        }
        if is_prompt_history_search_next_key(key)
            || matches!(
                key,
                KeyEvent {
                    code: KeyCode::Down,
                    ..
                }
            )
        {
            self.move_prompt_history_search_selection(-1);
            return Ok(true);
        }
        if is_prompt_history_search_cancel_key(key) {
            self.cancel_prompt_history_search();
            return Ok(true);
        }
        if is_prompt_history_search_backspace_key(key) {
            if let Some(search) = self.prompt_history.search.as_mut() {
                search.query.pop();
            }
            self.update_prompt_history_search_matches()?;
            return Ok(true);
        }
        if is_prompt_history_search_clear_key(key) {
            if let Some(search) = self.prompt_history.search.as_mut() {
                search.query.clear();
            }
            self.update_prompt_history_search_matches()?;
            return Ok(true);
        }
        let handled = match key {
            KeyEvent {
                code: KeyCode::Esc, ..
            } => {
                self.cancel_prompt_history_search();
                true
            }
            KeyEvent {
                code: KeyCode::Enter,
                modifiers: KeyModifiers::NONE,
                ..
            } => {
                if self
                    .prompt_history
                    .search
                    .as_ref()
                    .is_some_and(|search| search.selected.is_some())
                {
                    self.prompt_history.search = None;
                    self.prompt_history.reset_navigation();
                }
                true
            }
            KeyEvent {
                code: KeyCode::Backspace,
                ..
            } => {
                if let Some(search) = self.prompt_history.search.as_mut() {
                    search.query.pop();
                }
                self.update_prompt_history_search_matches()?;
                true
            }
            KeyEvent {
                code: KeyCode::Char(ch),
                modifiers,
                ..
            } if !ch.is_control()
                && !modifiers.contains(KeyModifiers::CONTROL)
                && !modifiers.contains(KeyModifiers::ALT) =>
            {
                if let Some(search) = self.prompt_history.search.as_mut() {
                    search.query.push(ch);
                }
                self.update_prompt_history_search_matches()?;
                true
            }
            _ => true,
        };
        Ok(handled)
    }

    fn update_prompt_history_search_matches(&mut self) -> Result<()> {
        let Some(search) = self.prompt_history.search.as_ref() else {
            return Ok(());
        };
        let query = search.query.clone();
        let draft = search.draft.clone();
        let config = self.refresh_prompt_history()?;
        let entries = self.prompt_history.search_entries(config.as_ref());
        let matches = prompt_history_search_matches(&entries, &query);
        let selected = (!matches.is_empty()).then_some(0);
        if let Some(search) = self.prompt_history.search.as_mut() {
            search.matches = matches;
            search.selected = selected;
        }
        if let Some(text) = self.prompt_history_selected_search_text() {
            self.composer.set_input(text);
        } else {
            self.composer.set_input(draft);
        }
        Ok(())
    }

    fn move_prompt_history_search_selection(&mut self, delta: isize) {
        let Some(search) = self.prompt_history.search.as_mut() else {
            return;
        };
        let Some(selected) = search.selected else {
            return;
        };
        let max = search.matches.len().saturating_sub(1);
        let next = (selected as isize + delta).clamp(0, max as isize) as usize;
        search.selected = Some(next);
        if let Some(text) = self.prompt_history_selected_search_text() {
            self.composer.set_input(text);
        }
    }

    fn prompt_history_selected_search_text(&self) -> Option<String> {
        let search = self.prompt_history.search.as_ref()?;
        let selected = search.selected?;
        search.matches.get(selected).cloned()
    }

    fn cancel_prompt_history_search(&mut self) {
        if let Some(search) = self.prompt_history.search.take() {
            self.composer.set_input(search.draft);
        }
        self.prompt_history.reset_navigation();
    }

    fn handle_main_composer_key(&mut self, key: KeyEvent) -> bool {
        let before = self.composer.input().to_string();
        let handled = self.composer.handle_key(key);
        if handled && self.composer.input() != before {
            self.prompt_history.reset_navigation();
        }
        if handled && self.is_slash_palette_active() {
            self.clamp_slash_palette_selection();
        }
        handled
    }

    fn session_model_selection_or_current(
        &self,
        session_id: &str,
    ) -> Result<SessionModelSelection> {
        Ok(self
            .session_model_selection(session_id)?
            .unwrap_or_else(|| self.current_model_selection()))
    }

    fn session_model_selection(&self, session_id: &str) -> Result<Option<SessionModelSelection>> {
        Ok(self
            .store
            .events_for_session(session_id)?
            .iter()
            .rev()
            .find_map(session_model_selection_from_event))
    }

    fn append_session_model_selection(
        &self,
        session_id: &str,
        selection: &SessionModelSelection,
    ) -> Result<()> {
        let mut payload = serde_json::json!({
            "display_model": selection.display_model,
            "provider_model": selection.provider_model,
            "account": selection.account,
            "backend": selection.backend.as_setting(),
        });
        if let Some(model_provider_id) = selection
            .model_provider_id
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            payload["model_provider_id"] = serde_json::Value::String(model_provider_id.to_string());
        }
        self.store
            .append_event(session_id, SESSION_MODEL_SELECTION_EVENT, payload)?;
        Ok(())
    }

    fn save_browser(&mut self, index: usize) -> Result<()> {
        let choice = BROWSER_CHOICES
            .get(index.min(BROWSER_CHOICES.len().saturating_sub(1)))
            .unwrap_or(&BROWSER_CHOICES[0]);
        self.browser = (*choice).to_string();
        self.track_browser_selected();
        self.persist_runtime_settings()?;
        if self.browser == BROWSER_USE_CLOUD && !self.browser_use_cloud_key_ready()? {
            self.status_notice = Some(
                "Browser Use cloud key is required before cloud browser tasks can run.".to_string(),
            );
            self.start_auth_flow(BROWSER_USE_CLOUD.to_string())?;
            return Ok(());
        }
        self.status_notice = Some(format!("Browser set to {}.", self.browser));
        if !self.setup_complete && self.model_configured && self.account_ready(&self.account)? {
            self.complete_setup()?;
            self.close_surface();
        } else if !self.setup_complete {
            self.open_surface(Surface::Setup);
        } else {
            self.close_surface();
        }
        Ok(())
    }

    fn save_auth(&mut self, secret: String) -> Result<()> {
        let Some(account) = self.api_key_account.clone() else {
            self.open_surface(Surface::Account);
            return Ok(());
        };
        if secret.trim().is_empty() {
            self.status_notice = Some(format!("{} is required.", auth_secret_label(&account)));
            self.open_surface(Surface::ApiKey);
            return Ok(());
        }
        if account == BROWSER_USE_CLOUD {
            self.store
                .set_setting(BROWSER_USE_CLOUD_API_KEY_SETTING, secret.trim())?;
            self.browser = BROWSER_USE_CLOUD.to_string();
            self.persist_runtime_settings()?;
            self.api_key_account = None;
            self.status_notice = Some("Saved Browser Use cloud key.".to_string());
            if !self.setup_complete && self.model_configured && self.account_ready(&self.account)? {
                self.complete_setup()?;
                self.close_surface();
            } else {
                self.close_surface();
            }
            return Ok(());
        }
        self.store
            .set_setting(auth_setting_key(&account), secret.trim())?;
        self.account = account.clone();
        self.persist_runtime_settings()?;
        self.api_key_account = None;
        if !self.setup_complete && self.setup_pending_account.as_deref() == Some(account.as_str()) {
            self.show_setup_result(
                SetupResultKind::Success,
                account.clone(),
                format!("Saved {}.", auth_secret_label(&account)),
            );
            return Ok(());
        }
        self.status_notice = Some(format!("Saved {}.", auth_secret_label(&account)));
        if let Some(index) = self.pending_model_after_auth.take() {
            return self.save_model(index);
        }
        self.advance_after_auth()
    }

    fn start_auth_flow(&mut self, account: String) -> Result<()> {
        self.track_auth_provider_selected(&account);
        if account == ACCOUNT_CODEX {
            self.start_codex_auth(account)?;
            return Ok(());
        }
        if is_claude_code_account(&account) {
            if self.account_ready(&account)? {
                self.account = account.clone();
                self.persist_runtime_settings()?;
                self.show_setup_result(
                    SetupResultKind::Success,
                    account,
                    "Connected to Claude Code.".to_string(),
                );
            } else {
                self.start_claude_code_oauth(account)?;
            }
            return Ok(());
        }
        self.start_auth_entry(account);
        Ok(())
    }

    fn start_auth_entry(&mut self, account: String) {
        self.api_key_account = Some(account);
        self.composer.clear();
        self.open_surface(Surface::ApiKey);
    }

    fn start_claude_code_oauth(&mut self, account: String) -> Result<()> {
        self.api_key_account = None;
        self.composer.clear();
        self.claude_code_oauth = None;
        let mut flow = match start_claude_code_oauth_flow(account.clone()) {
            Ok(flow) => flow,
            Err(error) => {
                self.show_setup_result(
                    SetupResultKind::Failure,
                    account,
                    format!("Could not start Claude Code OAuth: {error:#}"),
                );
                return Ok(());
            }
        };
        if let Err(error) = open_external_url(&flow.url) {
            flow.browser_open_error = Some(error.to_string());
        }
        self.claude_code_oauth = Some(flow);
        self.show_setup_result(
            SetupResultKind::Pending,
            account,
            "Waiting for Claude Code OAuth sign-in.".to_string(),
        );
        Ok(())
    }

    fn reopen_claude_code_oauth_url(&mut self) {
        let Some(url) = self.claude_code_oauth.as_ref().map(|flow| flow.url.clone()) else {
            return;
        };
        let message = match open_external_url(&url) {
            Ok(()) => "Waiting for Claude Code OAuth sign-in.".to_string(),
            Err(error) => format!("Could not open browser automatically: {error}"),
        };
        if let Some(result) = self.setup_result.as_mut() {
            result.message = message;
        }
    }

    fn start_codex_device_login(&mut self, account: String) -> Result<()> {
        self.api_key_account = None;
        self.composer.clear();
        self.codex_login = None;
        let flow = match start_codex_login_flow(account.clone(), self.args.state_dir.clone()) {
            Ok(flow) => flow,
            Err(error) => {
                self.show_setup_result(
                    SetupResultKind::Failure,
                    account,
                    format!("Could not start Codex login: {error:#}"),
                );
                return Ok(());
            }
        };
        self.codex_login = Some(flow);
        self.show_setup_result(
            SetupResultKind::Pending,
            account,
            "Waiting for Codex device sign-in.".to_string(),
        );
        Ok(())
    }

    fn reopen_codex_device_auth_url(&mut self) {
        let message = match open_external_url(CODEX_DEVICE_AUTH_URL) {
            Ok(()) => "Waiting for Codex device sign-in.".to_string(),
            Err(error) => format!("Could not open browser automatically: {error}"),
        };
        if let Some(result) = self.setup_result.as_mut() {
            result.message = message;
        }
    }

    fn store_claude_code_oauth(&self, credential: &ClaudeCodeOAuthCredential) -> Result<()> {
        self.store.set_setting(
            "auth.claude_code.access_token",
            credential.access_token.trim(),
        )?;
        if credential.refresh_token.trim().is_empty() {
            self.store
                .delete_setting("auth.claude_code.refresh_token")?;
        } else {
            self.store.set_setting(
                "auth.claude_code.refresh_token",
                credential.refresh_token.trim(),
            )?;
        }
        if credential.expires_ms > 0 {
            self.store.set_setting(
                "auth.claude_code.expires_ms",
                &credential.expires_ms.to_string(),
            )?;
        } else {
            self.store.delete_setting("auth.claude_code.expires_ms")?;
        }
        self.store.delete_setting("auth.claude_code.auth_token")?;
        Ok(())
    }

    fn cancel_auth_entry(&mut self) {
        self.api_key_account = None;
        self.pending_model_after_auth = None;
        if !self.setup_complete {
            self.setup_pending_account = None;
            self.setup_result = None;
        }
        self.cancel_secret_entry();
    }

    fn start_telemetry_entry(&mut self) {
        self.composer.clear();
        self.open_surface(Surface::Telemetry);
    }

    fn cancel_secret_entry(&mut self) {
        self.composer.clear();
        self.close_surface();
    }

    fn save_telemetry(&mut self, secret: String) -> Result<()> {
        if secret.trim().is_empty() {
            self.status_notice = Some("Laminar API key is required.".to_string());
            self.open_surface(Surface::Telemetry);
            return Ok(());
        }
        self.store
            .set_setting(LAMINAR_API_KEY_SETTING, secret.trim())?;
        self.status_notice = Some("Saved Laminar API key.".to_string());
        self.open_surface(Surface::Developer);
        Ok(())
    }

    fn handle_api_key_key(&mut self, key: KeyEvent) -> bool {
        let handled = self.composer.handle_key(key);
        if handled {
            self.selected_row = 0;
        }
        handled
    }

    fn setup_row_count(&self) -> usize {
        ACCOUNT_CHOICES.len()
    }

    fn request_open_browser(&mut self) -> Result<()> {
        let Some(session_id) = self.selected_session_id.clone() else {
            self.browser_notice = Some("No current browser task yet.".to_string());
            return Ok(());
        };
        let state = self.workbench_state()?;
        let target = state
            .browser
            .live_url
            .as_deref()
            .or(state.browser.url.as_deref())
            .unwrap_or("about:blank");
        self.store.append_event(
            &session_id,
            "browser.open_requested",
            serde_json::json!({ "target": target }),
        )?;
        self.browser_notice = Some(match open_external_url(target) {
            Ok(()) => format!("Opened {target}"),
            Err(error) => format!("Could not open {target}: {error}"),
        });
        Ok(())
    }

    fn request_reconnect_browser(&mut self) -> Result<()> {
        let Some(session_id) = self.selected_session_id.clone() else {
            self.browser_notice = Some("No current browser task yet.".to_string());
            return Ok(());
        };
        self.store.append_event(
            &session_id,
            "browser.reconnect_requested",
            serde_json::json!({ "browser": self.browser }),
        )?;
        self.browser_notice = Some("Reconnect requested.".to_string());
        Ok(())
    }

    fn complete_setup(&mut self) -> Result<()> {
        self.setup_complete = true;
        self.store.set_setting("setup.complete", "1")?;
        if cfg!(test) {
            return Ok(());
        }
        product_analytics::capture_async(
            &self.store,
            "bu:tui setup completed",
            serde_json::json!({
                "surface": "tui",
                "provider_kind": account_kind(&self.account),
                "browser_kind": browser_choice_kind(&self.browser),
            }),
        );
        Ok(())
    }

    fn track_app_opened(&self) {
        if cfg!(test) {
            return;
        }
        product_analytics::capture_async(
            &self.store,
            "bu:tui app opened",
            serde_json::json!({
                "surface": "tui",
                "provider_kind": account_kind(&self.account),
                "browser_kind": browser_choice_kind(&self.browser),
                "setup_complete": self.setup_complete,
            }),
        );
    }

    fn track_model_selected(&self) {
        if cfg!(test) {
            return;
        }
        product_analytics::capture_async(
            &self.store,
            "bu:tui model selected",
            serde_json::json!({
                "surface": "tui",
                "provider_kind": account_kind(&self.account),
                "model": self.provider_model,
            }),
        );
    }

    fn track_browser_selected(&self) {
        if cfg!(test) {
            return;
        }
        product_analytics::capture_async(
            &self.store,
            "bu:tui browser selected",
            serde_json::json!({
                "surface": "tui",
                "browser_kind": browser_choice_kind(&self.browser),
            }),
        );
    }

    fn track_auth_provider_selected(&self, account: &str) {
        if cfg!(test) {
            return;
        }
        product_analytics::capture_async(
            &self.store,
            "bu:tui auth provider selected",
            serde_json::json!({
                "surface": "tui",
                "provider_kind": account_kind(account),
            }),
        );
    }

    fn persist_runtime_settings(&self) -> Result<()> {
        self.store.set_setting("account", &self.account)?;
        self.store.set_setting("model", &self.model)?;
        self.store
            .set_setting("provider.model", &self.provider_model)?;
        self.store.set_setting(
            COLLABORATION_MODE_SETTING,
            collaboration_mode_setting_value(self.collaboration_mode),
        )?;
        self.store.set_setting("browser", &self.browser)?;
        self.store
            .set_setting("agent.backend", self.agent_backend.as_setting())?;
        if let Some(model_provider_id) = self
            .model_provider_id
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            self.store.set_setting("provider.id", model_provider_id)?;
        }
        Ok(())
    }

    fn selectable_row_count(&mut self) -> Result<usize> {
        Ok(match self.surface {
            Surface::Main => {
                if self.is_first_run_setup_visible()? {
                    self.setup_row_count()
                } else {
                    self.main_selection_count()?
                }
            }
            Surface::Setup => self.setup_row_count(),
            Surface::SetupConfirm => 2,
            Surface::SetupResult => self.setup_result_row_count(),
            Surface::Account => ACCOUNT_CHOICES.len(),
            Surface::ApiKey | Surface::Telemetry => 2,
            Surface::Model => self.model_choices.len(),
            Surface::Mode => 2,
            Surface::Browser => 3,
            Surface::BrowserSelect => BROWSER_CHOICES.len(),
            Surface::History => self.history_visible_indices()?.len(),
            Surface::Messages => self.message_action_rows().len(),
            Surface::Developer => 1,
        })
    }

    fn is_slash_palette_active(&self) -> bool {
        self.surface == Surface::Main && self.palette_open
    }

    fn setup_result_row_count(&self) -> usize {
        match self.setup_result.as_ref().map(|result| &result.kind) {
            Some(SetupResultKind::Failure) => 2,
            _ => 1,
        }
    }

    pub(crate) fn palette_filter(&self) -> &str {
        &self.palette_filter
    }

    fn open_slash_palette(&mut self) {
        self.prompt_history.search = None;
        self.prompt_history.reset_navigation();
        self.palette_open = true;
        self.palette_filter.clear();
        self.selected_row = 0;
    }

    fn close_slash_palette(&mut self) {
        self.palette_open = false;
        self.palette_filter.clear();
        self.selected_row = 0;
    }

    fn slash_palette_items(&self) -> Vec<palette::PaletteItem> {
        palette::items_filtered(&self.palette_filter)
    }

    fn move_slash_palette_selection(&mut self, delta: isize) {
        let count = self.slash_palette_items().len();
        if count == 0 {
            self.selected_row = 0;
            return;
        }
        // Wrap around the ends rather than stopping at them.
        self.selected_row =
            (self.selected_row as isize + delta).rem_euclid(count as isize) as usize;
    }

    fn clamp_slash_palette_selection(&mut self) {
        let count = self.slash_palette_items().len();
        if count == 0 {
            self.selected_row = 0;
        } else if self.selected_row >= count {
            self.selected_row = count - 1;
        }
    }

    fn execute_slash_palette_selection(&mut self) -> Result<bool> {
        let filter = self.palette_filter.trim().trim_start_matches('/');
        if let Some(plan_text) = filter
            .strip_prefix("plan")
            .filter(|rest| rest.is_empty() || rest.starts_with(char::is_whitespace))
            .map(str::trim)
            .map(str::to_string)
        {
            self.close_slash_palette();
            self.dispatch(AppCommand::SetCollaborationMode(
                CollaborationModeKind::Plan,
            ))?;
            if !plan_text.is_empty() {
                self.submit_plain_text(plan_text.to_string())?;
            }
            return Ok(false);
        }
        let action = palette::selected_action(&self.palette_filter, self.selected_row);
        if let Some(action) = action {
            self.close_slash_palette();
            return self.execute_palette_action(action);
        }
        Ok(false)
    }

    fn main_selection_count(&mut self) -> Result<usize> {
        let state = self.workbench_state()?;
        Ok(match self.product_state(&state) {
            ProductState::Failed => 4,
            ProductState::Cancelled => 3,
            _ => 0,
        })
    }

    fn move_selection(&mut self, delta: isize) -> Result<()> {
        let count = self.selectable_row_count()?;
        if count == 0 {
            self.selected_row = 0;
            return Ok(());
        }
        // Wrap around the ends — Down past the last row lands on the first.
        self.selected_row =
            (self.selected_row as isize + delta).rem_euclid(count as isize) as usize;
        Ok(())
    }

    #[cfg(test)]
    fn composer_height(&self) -> u16 {
        self.composer.height()
    }

    fn live_viewport_height(&self) -> u16 {
        self.args.height.clamp(8, 10)
    }

    fn native_scrollback_is_active(&self) -> bool {
        self.surface.uses_main_view()
            && self
                .native_history
                .is_active_for(self.selected_session_id.as_deref())
    }

    fn should_animate_live_spinner(&mut self) -> bool {
        if !self.native_scrollback_is_active() {
            return false;
        }
        let state = self.refresh_cached_projection().clone();
        let model = transcript::transcript_model(self, &state);
        transcript::has_shimmering_live_status(model.as_ref())
    }

    fn tick_live_spinner(&mut self) {
        self.live_spinner_frame = self.live_spinner_frame.wrapping_add(1);
    }

    #[cfg(test)]
    fn set_input(&mut self, value: String) {
        self.composer.set_input(value);
    }

    #[cfg(test)]
    fn set_input_cursor(&mut self, cursor: usize) {
        self.composer.set_cursor(cursor);
    }

    fn product_state(&self, state: &WorkbenchState) -> ProductState {
        if !self.setup_complete && state.history.is_empty() && state.current_session.is_none() {
            return ProductState::SetupNeeded;
        }
        let Some(session) = state.current_session.as_ref() else {
            return ProductState::Ready;
        };
        if session.status.is_active() {
            ProductState::Running
        } else if session.status == SessionStatus::Cancelled {
            ProductState::Cancelled
        } else if state.failure.is_some() {
            ProductState::Failed
        } else {
            ProductState::Result
        }
    }

    fn should_print_and_exit(&mut self) -> Result<bool> {
        if self.surface != Surface::Main || self.is_first_run_setup_visible()? {
            return Ok(false);
        }
        let state = self.workbench_state()?;
        Ok(matches!(
            self.product_state(&state),
            ProductState::Result | ProductState::Failed | ProductState::Cancelled
        ))
    }

    fn account_ready(&self, account: &str) -> Result<bool> {
        Ok(match account {
            ACCOUNT_OPENAI => self.has_stored_or_env(
                "auth.openai.api_key",
                &["LLM_BROWSER_OPENAI_API_KEY", "OPENAI_API_KEY"],
            )?,
            ACCOUNT_OPENROUTER => self.has_stored_or_env(
                "auth.openrouter.api_key",
                &["LLM_BROWSER_OPENAI_COMPAT_API_KEY", "OPENROUTER_API_KEY"],
            )?,
            ACCOUNT_DEEPSEEK => self.has_stored_or_env(
                "auth.deepseek.api_key",
                &["LLM_BROWSER_DEEPSEEK_API_KEY", "DEEPSEEK_API_KEY"],
            )?,
            ACCOUNT_ANTHROPIC => self.has_stored_or_env(
                "auth.anthropic.api_key",
                &["LLM_BROWSER_ANTHROPIC_API_KEY", "ANTHROPIC_API_KEY"],
            )?,
            account if is_claude_code_account(account) => self.has_claude_code_oauth()?,
            ACCOUNT_CODEX => self.has_codex_login()?,
            _ => false,
        })
    }

    fn auth_notice(&self) -> Result<Option<String>> {
        self.auth_notice_for_selection(&self.current_model_selection())
    }

    fn auth_notice_for_selection(
        &self,
        selection: &SessionModelSelection,
    ) -> Result<Option<String>> {
        let notice = match selection.backend {
            AgentBackend::Openai
                if !self.has_stored_or_env(
                    "auth.openai.api_key",
                    &["LLM_BROWSER_OPENAI_API_KEY", "OPENAI_API_KEY"],
                )? =>
            {
                Some("OpenAI API key is missing. Authenticate here before retrying.".to_string())
            }
            AgentBackend::Openrouter
                if !self.has_stored_or_env(
                    "auth.openrouter.api_key",
                    &["LLM_BROWSER_OPENAI_COMPAT_API_KEY", "OPENROUTER_API_KEY"],
                )? =>
            {
                Some(
                    "OpenRouter API key is missing. Authenticate here before retrying.".to_string(),
                )
            }
            AgentBackend::Deepseek
                if !self.has_stored_or_env(
                    "auth.deepseek.api_key",
                    &["LLM_BROWSER_DEEPSEEK_API_KEY", "DEEPSEEK_API_KEY"],
                )? =>
            {
                Some("DeepSeek API key is missing. Authenticate here before retrying.".to_string())
            }
            AgentBackend::Codex if !self.has_codex_login()? => {
                Some("Codex login is missing. Select Codex login to sign in.".to_string())
            }
            AgentBackend::Anthropic
                if is_claude_code_account(&selection.account)
                    && !self.has_claude_code_oauth()? =>
            {
                Some(
                    "Claude Code login is missing. Open Claude Code sign-in here before retrying."
                        .to_string(),
                )
            }
            AgentBackend::Anthropic
                if !is_claude_code_account(&selection.account)
                    && !self.has_stored_or_env(
                        "auth.anthropic.api_key",
                        &["LLM_BROWSER_ANTHROPIC_API_KEY", "ANTHROPIC_API_KEY"],
                    )? =>
            {
                Some("Anthropic API key is missing. Authenticate here before retrying.".to_string())
            }
            _ => None,
        };
        Ok(notice)
    }

    fn browser_notice(&self) -> Result<Option<String>> {
        if self.browser == BROWSER_USE_CLOUD && !self.browser_use_cloud_key_ready()? {
            Ok(Some(
                "Browser Use cloud key is missing. Set BROWSER_USE_API_KEY or choose Local Chrome."
                    .to_string(),
            ))
        } else {
            Ok(None)
        }
    }

    fn browser_use_cloud_key_ready(&self) -> Result<bool> {
        if self
            .store
            .get_setting(BROWSER_USE_CLOUD_API_KEY_SETTING)?
            .is_some_and(|value| !value.trim().is_empty())
        {
            return Ok(true);
        }
        Ok(browser_use_cloud_env_key_present())
    }

    fn has_stored_or_env(&self, setting_key: &str, env_names: &[&str]) -> Result<bool> {
        if self
            .store
            .get_setting(setting_key)?
            .is_some_and(|value| !value.trim().is_empty())
        {
            return Ok(true);
        }
        Ok(env_names
            .iter()
            .any(|name| std::env::var(name).is_ok_and(|value| !value.trim().is_empty())))
    }

    fn has_codex_login(&self) -> Result<bool> {
        if self
            .store
            .get_setting("auth.codex.access_token")?
            .is_some_and(|value| !value.trim().is_empty())
            && self
                .store
                .get_setting("auth.codex.account_id")?
                .is_some_and(|value| !value.trim().is_empty())
        {
            return Ok(true);
        }
        Ok(load_codex_managed_auth().is_ok()
            || load_codex_auth().is_ok()
            || codex_env_auth_present())
    }

    fn store_codex_auth(&self, auth: &CodexAuth) -> Result<()> {
        self.store
            .set_setting("auth.codex.access_token", auth.access_token.trim())?;
        self.store
            .set_setting("auth.codex.account_id", auth.account_id.trim())?;
        self.store.delete_setting("auth.codex.id_token")?;
        self.store.delete_setting("auth.codex.refresh_token")?;
        self.store.delete_setting("auth.codex.source_path")?;
        self.store.delete_setting("auth.codex.last_refresh")?;
        Ok(())
    }

    fn has_claude_code_oauth(&self) -> Result<bool> {
        Ok(self.has_stored_or_env(
            "auth.claude_code.access_token",
            &[
                "LLM_BROWSER_CLAUDE_CODE_OAUTH_TOKEN",
                "CLAUDE_CODE_OAUTH_TOKEN",
                "LLM_BROWSER_ANTHROPIC_OAUTH_TOKEN",
                "ANTHROPIC_OAUTH_TOKEN",
                "ANTHROPIC_AUTH_TOKEN",
            ],
        )? || self.has_stored_or_env("auth.claude_code.auth_token", &[])?)
    }

    pub(crate) fn claude_code_oauth_url(&self) -> Option<&str> {
        self.claude_code_oauth
            .as_ref()
            .map(|flow| flow.url.as_str())
    }

    pub(crate) fn claude_code_oauth_open_error(&self) -> Option<&str> {
        self.claude_code_oauth
            .as_ref()
            .and_then(|flow| flow.browser_open_error.as_deref())
    }

    pub(crate) fn claude_code_oauth_elapsed_seconds(&self) -> Option<u64> {
        self.claude_code_oauth
            .as_ref()
            .map(|flow| flow.started_at.elapsed().as_secs())
    }

    pub(crate) fn codex_login_elapsed_seconds(&self) -> Option<u64> {
        self.codex_login
            .as_ref()
            .map(|flow| flow.started_at.elapsed().as_secs())
    }

    pub(crate) fn codex_login_output_lines(&self) -> Vec<String> {
        self.codex_login
            .as_ref()
            .map(|flow| {
                flow.output
                    .lines()
                    .filter_map(|line| {
                        let line = line.trim();
                        (!line.is_empty()).then(|| line.to_string())
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    fn laminar_status(&self) -> Result<String> {
        if self
            .store
            .get_setting(LAMINAR_API_KEY_SETTING)?
            .is_some_and(|value| !value.trim().is_empty())
        {
            return Ok("connected via TUI config".to_string());
        }
        if std::env::var("LMNR_PROJECT_API_KEY").is_ok_and(|value| !value.trim().is_empty()) {
            return Ok("connected via LMNR_PROJECT_API_KEY".to_string());
        }
        Ok("not connected".to_string())
    }
}

const LAMINAR_API_KEY_SETTING: &str = "telemetry.laminar.api_key";

fn codex_env_auth_present() -> bool {
    if std::env::var("LLM_BROWSER_CODEX_ACCESS_TOKEN").is_ok_and(|value| !value.trim().is_empty())
        && std::env::var("LLM_BROWSER_CODEX_ACCOUNT_ID").is_ok_and(|value| !value.trim().is_empty())
    {
        return true;
    }
    std::env::var("LLM_BROWSER_CODEX_AUTH_FILE").is_ok_and(|value| !value.trim().is_empty())
}

fn auth_setting_key(account: &str) -> &'static str {
    match account {
        ACCOUNT_OPENAI => "auth.openai.api_key",
        ACCOUNT_OPENROUTER => "auth.openrouter.api_key",
        ACCOUNT_DEEPSEEK => "auth.deepseek.api_key",
        ACCOUNT_ANTHROPIC => "auth.anthropic.api_key",
        BROWSER_USE_CLOUD => BROWSER_USE_CLOUD_API_KEY_SETTING,
        account if is_claude_code_account(account) => "auth.claude_code.access_token",
        _ => "auth.codex.placeholder",
    }
}

fn auth_secret_label(account: &str) -> &'static str {
    match account {
        ACCOUNT_OPENAI => "OpenAI API key",
        ACCOUNT_OPENROUTER => "OpenRouter API key",
        ACCOUNT_DEEPSEEK => "DeepSeek API key",
        ACCOUNT_ANTHROPIC => "Anthropic API key",
        BROWSER_USE_CLOUD => "Browser Use cloud key",
        account if is_claude_code_account(account) => "Claude Code OAuth token",
        _ => "credential",
    }
}

fn account_kind(account: &str) -> &'static str {
    match account {
        ACCOUNT_CODEX => "codex",
        ACCOUNT_OPENAI => "openai",
        ACCOUNT_OPENROUTER => "openrouter",
        ACCOUNT_DEEPSEEK => "deepseek",
        ACCOUNT_ANTHROPIC => "anthropic",
        BROWSER_USE_CLOUD => "browser_use_cloud",
        account if is_claude_code_account(account) => "claude_code",
        _ => "unknown",
    }
}

#[cfg(not(test))]
fn app_codex_home(state_dir: &Path) -> PathBuf {
    state_dir.join("codex-home")
}

fn browser_choice_kind(browser: &str) -> &'static str {
    match browser {
        BROWSER_LOCAL_CHROME => "local",
        "Headless Chromium" => "headless",
        BROWSER_USE_CLOUD => "cloud",
        _ => "other",
    }
}

#[cfg(not(test))]
fn run_update_installer() -> Result<String> {
    let source = std::env::var("BUT_INSTALL_SCRIPT")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .or_else(|| local_install_script_path().map(|path| path.display().to_string()));

    let output = if let Some(source) = source {
        if source.starts_with("https://") || source.starts_with("http://") {
            run_remote_install_script(&source)?
        } else {
            std::process::Command::new("sh")
                .arg(&source)
                .arg("--no-launch")
                .output()
                .with_context(|| format!("run installer script {source}"))?
        }
    } else {
        let repo = std::env::var("BUT_RELEASE_REPO")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| "browser-use/terminal".to_string());
        let url =
            format!("https://raw.githubusercontent.com/{repo}/main/scripts/install/install.sh");
        run_remote_install_script(&url)?
    };

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        let detail = stderr.trim();
        if !detail.is_empty() {
            anyhow::bail!("{detail}");
        }
        anyhow::bail!("{}", stdout.trim());
    }

    Ok("Update installed. Restart browser-use terminal to use the latest release.".to_string())
}

#[cfg(test)]
fn run_update_installer() -> Result<String> {
    Ok("Update command is available.".to_string())
}

#[cfg(not(test))]
fn local_install_script_path() -> Option<PathBuf> {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .map(|root| root.join("scripts/install/install.sh"))
        .filter(|path| path.is_file())
}

#[cfg(not(test))]
fn run_remote_install_script(url: &str) -> Result<std::process::Output> {
    let script = r#"
if command -v curl >/dev/null 2>&1; then
  curl -fsSL "$1"
elif command -v wget >/dev/null 2>&1; then
  wget -q -O - "$1"
else
  echo "curl or wget is required to update browser-use terminal." >&2
  exit 1
fi | sh -s -- --no-launch
"#;
    std::process::Command::new("sh")
        .arg("-c")
        .arg(script)
        .arg("browser-use-terminal-update")
        .arg(url)
        .output()
        .with_context(|| format!("download and run installer script {url}"))
}

#[cfg(not(test))]
fn open_external_url(target: &str) -> Result<()> {
    let target = target.trim();
    if target.is_empty() {
        anyhow::bail!("browser target is empty");
    }
    open::that_detached(target).with_context(|| format!("launch external browser for {target}"))
}

#[cfg(test)]
fn open_external_url(target: &str) -> Result<()> {
    if target.trim().is_empty() {
        anyhow::bail!("browser target is empty");
    }
    Ok(())
}

#[cfg(not(test))]
fn start_claude_code_oauth_flow(account: String) -> Result<ClaudeCodeOAuthFlow> {
    let (verifier, challenge) = claude_code_oauth_pkce();
    let url = claude_code_oauth_authorize_url(&verifier, &challenge);
    let listener = TcpListener::bind((CLAUDE_CODE_CALLBACK_HOST, CLAUDE_CODE_CALLBACK_PORT))
        .with_context(|| {
            format!(
                "bind Claude Code OAuth callback on {CLAUDE_CODE_CALLBACK_HOST}:{CLAUDE_CODE_CALLBACK_PORT}"
            )
        })?;
    listener
        .set_nonblocking(true)
        .context("configure Claude Code OAuth callback listener")?;
    let (stop_tx, stop_rx) = mpsc::channel();
    let (event_tx, rx) = mpsc::channel();
    let flow_account = account.clone();
    thread::Builder::new()
        .name("browser-use-claude-code-oauth".to_string())
        .spawn(move || {
            let result = wait_for_claude_code_oauth_credential(listener, verifier.clone(), stop_rx)
                .map_err(|error| format!("{error:#}"));
            let _ = event_tx.send(ClaudeCodeOAuthEvent { account, result });
        })
        .context("spawn Claude Code OAuth callback listener")?;
    Ok(ClaudeCodeOAuthFlow {
        account: flow_account,
        url,
        started_at: Instant::now(),
        stop_tx,
        rx,
        browser_open_error: None,
    })
}

#[cfg(test)]
fn start_claude_code_oauth_flow(account: String) -> Result<ClaudeCodeOAuthFlow> {
    let (verifier, challenge) = claude_code_oauth_pkce();
    let url = claude_code_oauth_authorize_url(&verifier, &challenge);
    let (stop_tx, _stop_rx) = mpsc::channel();
    let (event_tx, rx) = mpsc::channel();
    Ok(ClaudeCodeOAuthFlow {
        account,
        url,
        started_at: Instant::now(),
        stop_tx,
        rx,
        browser_open_error: None,
        event_tx_guard: Some(event_tx),
    })
}

#[cfg(not(test))]
fn start_codex_login_flow(account: String, state_dir: PathBuf) -> Result<CodexLoginFlow> {
    let codex_home = app_codex_home(&state_dir);
    std::fs::create_dir_all(&codex_home)
        .with_context(|| format!("create app Codex home {}", codex_home.display()))?;
    let auth_path = codex_home.join("auth.json");
    let mut child = ProcessCommand::new("codex")
        .args(["login", "--device-auth"])
        .env("CODEX_HOME", &codex_home)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("start `codex login --device-auth`")?;
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let (stop_tx, stop_rx) = mpsc::channel();
    let (event_tx, rx) = mpsc::channel();
    if let Some(stdout) = stdout {
        spawn_codex_output_reader(stdout, event_tx.clone());
    }
    if let Some(stderr) = stderr {
        spawn_codex_output_reader(stderr, event_tx.clone());
    }
    thread::Builder::new()
        .name("browser-use-codex-login".to_string())
        .spawn(move || loop {
            if stop_rx.try_recv().is_ok() {
                let _ = child.kill();
                let _ = child.wait();
                let _ = event_tx.send(CodexLoginEvent::Finished(Err(
                    "Codex device sign-in was cancelled".to_string(),
                )));
                return;
            }
            match child.try_wait() {
                Ok(Some(status)) => {
                    let result = if status.success() {
                        load_codex_auth_file(&auth_path)
                            .with_context(|| {
                                format!(
                                    "load app Codex auth after device sign-in from {}",
                                    auth_path.display()
                                )
                            })
                            .map_err(|error| format!("{error:#}"))
                    } else {
                        Err(format!("`codex login --device-auth` exited with {status}"))
                    };
                    let _ = event_tx.send(CodexLoginEvent::Finished(result));
                    return;
                }
                Ok(None) => thread::sleep(Duration::from_millis(100)),
                Err(error) => {
                    let _ = event_tx.send(CodexLoginEvent::Finished(Err(format!(
                        "wait for Codex login process: {error}"
                    ))));
                    return;
                }
            }
        })
        .context("spawn Codex device login watcher")?;
    Ok(CodexLoginFlow {
        account,
        output: String::new(),
        started_at: Instant::now(),
        stop_tx,
        rx,
    })
}

#[cfg(not(test))]
fn spawn_codex_output_reader<R>(mut reader: R, event_tx: mpsc::Sender<CodexLoginEvent>)
where
    R: Read + Send + 'static,
{
    thread::spawn(move || {
        let mut buffer = [0_u8; 1024];
        loop {
            match reader.read(&mut buffer) {
                Ok(0) => return,
                Ok(read) => {
                    let text = String::from_utf8_lossy(&buffer[..read]).to_string();
                    let _ = event_tx.send(CodexLoginEvent::Output(text));
                }
                Err(_) => return,
            }
        }
    });
}

#[cfg(test)]
fn start_codex_login_flow(account: String, _state_dir: PathBuf) -> Result<CodexLoginFlow> {
    let (stop_tx, _stop_rx) = mpsc::channel();
    let (event_tx, rx) = mpsc::channel();
    Ok(CodexLoginFlow {
        account,
        output: String::new(),
        started_at: Instant::now(),
        stop_tx,
        rx,
        event_tx_guard: Some(event_tx),
    })
}

#[cfg(not(test))]
fn wait_for_claude_code_oauth_credential(
    listener: TcpListener,
    verifier: String,
    stop_rx: mpsc::Receiver<()>,
) -> Result<ClaudeCodeOAuthCredential> {
    let parsed = wait_for_claude_code_callback(listener, verifier.as_str(), stop_rx)?;
    let auth_code = parsed
        .code
        .context("Claude Code authorization code was missing")?;
    let state = parsed.state.unwrap_or_default();
    if state != verifier {
        anyhow::bail!("Claude Code OAuth state mismatch");
    }
    exchange_claude_code_authorization_code(&auth_code, &state, &verifier)
}

#[cfg(not(test))]
fn wait_for_claude_code_callback(
    listener: TcpListener,
    expected_state: &str,
    stop_rx: mpsc::Receiver<()>,
) -> Result<ClaudeCodeAuthorization> {
    let deadline = Instant::now() + Duration::from_secs(900);
    loop {
        if stop_rx.try_recv().is_ok() {
            anyhow::bail!("Claude Code OAuth sign-in was cancelled");
        }
        if Instant::now() >= deadline {
            anyhow::bail!("timed out waiting for Anthropic browser callback");
        }
        match listener.accept() {
            Ok((mut stream, _)) => {
                return handle_claude_code_callback(&mut stream, expected_state);
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(50));
            }
            Err(error) => return Err(error).context("accept Claude Code OAuth callback"),
        }
    }
}

#[cfg(not(test))]
fn handle_claude_code_callback(
    stream: &mut TcpStream,
    expected_state: &str,
) -> Result<ClaudeCodeAuthorization> {
    let mut request = [0_u8; 4096];
    let read = stream
        .read(&mut request)
        .context("read Claude Code OAuth callback")?;
    let request = String::from_utf8_lossy(&request[..read]);
    let path = request
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .context("parse Claude Code OAuth callback request")?;
    let parsed = parse_claude_code_authorization_input(path);
    let status = if !path.starts_with(CLAUDE_CODE_CALLBACK_PATH) {
        404
    } else if parsed.code.is_none() || parsed.state.as_deref() != Some(expected_state) {
        400
    } else {
        200
    };
    let text = match status {
        200 => "Anthropic authentication completed. You can close this window.",
        400 => "Anthropic authentication failed: missing code or state mismatch.",
        _ => "Anthropic callback route not found.",
    };
    let body = format!("<html><body><p>{text}</p></body></html>");
    let response = format!(
        "HTTP/1.1 {status} OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(response.as_bytes()).ok();
    if status == 200 {
        Ok(parsed)
    } else {
        anyhow::bail!("{text}")
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ResetKeyboardEnhancementFlags;

impl Command for ResetKeyboardEnhancementFlags {
    fn write_ansi(&self, f: &mut impl fmt::Write) -> fmt::Result {
        f.write_str("\x1b[<u")
    }

    #[cfg(windows)]
    fn execute_winapi(&self) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "keyboard enhancement reset is not implemented for legacy Windows terminals",
        ))
    }

    #[cfg(windows)]
    fn is_ansi_code_supported(&self) -> bool {
        false
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct EnableMouseClickCapture;

impl Command for EnableMouseClickCapture {
    fn write_ansi(&self, f: &mut impl fmt::Write) -> fmt::Result {
        // Crossterm's built-in EnableMouseCapture also enables drag and
        // all-motion tracking, which blocks ordinary terminal text selection.
        // The welcome logo only needs button press/release coordinates.
        f.write_str(concat!("\x1b[?1000h", "\x1b[?1006h"))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct DisableModifyOtherKeys;

impl Command for DisableModifyOtherKeys {
    fn write_ansi(&self, f: &mut impl fmt::Write) -> fmt::Result {
        f.write_str("\x1b[>4;0m")
    }

    #[cfg(windows)]
    fn execute_winapi(&self) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "modifyOtherKeys reset is not implemented for legacy Windows terminals",
        ))
    }

    #[cfg(windows)]
    fn is_ansi_code_supported(&self) -> bool {
        false
    }
}

static AGENT_PANIC_HOOK: Once = Once::new();

fn install_agent_panic_hook() {
    AGENT_PANIC_HOOK.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let is_agent_thread = thread::current()
                .name()
                .is_some_and(|name| name.starts_with("browser-use-agent-"));
            if !is_agent_thread {
                previous(info);
            }
        }));
    });
}

fn record_agent_panic(
    state_dir: PathBuf,
    session_id: String,
    notifier: Option<StoreNotifier>,
    message: String,
) {
    let error = format!("agent thread panicked: {message}");
    if let Ok(store) = Store::open_with_optional_notifier(state_dir, notifier) {
        let _ = store.append_event(
            &session_id,
            "session.failed",
            serde_json::json!({ "error": error }),
        );
    }
}

fn panic_payload_message(payload: Box<dyn std::any::Any + Send>) -> String {
    if let Some(message) = payload.downcast_ref::<&str>() {
        return (*message).to_string();
    }
    if let Some(message) = payload.downcast_ref::<String>() {
        return message.clone();
    }
    "non-string panic payload".to_string()
}

fn main() -> Result<()> {
    install_process_crypto_provider();
    let _unified_exec_cleanup = UnifiedExecShutdownCleanup::new();
    install_agent_panic_hook();
    load_dotenv()?;
    let args = Args::parse();
    if args.dump_screen {
        let mut app = App::new(args)?;
        let text = render_dump(&mut app)?;
        print!("{text}");
        return Ok(());
    }
    let mut app = App::new(args)?;
    if app.should_print_and_exit()? {
        print_native_transcript(&mut app)?;
        return Ok(());
    }
    app.track_app_opened();
    run_terminal(app)
}

fn load_dotenv() -> Result<()> {
    load_dotenv_path(Path::new(".env"))
}

fn load_dotenv_path(path: &Path) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }
    let contents =
        std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    for line in contents.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        if key.is_empty() || std::env::var_os(key).is_some() {
            continue;
        }
        let value = unquote_env_value(value.trim());
        unsafe {
            std::env::set_var(key, value);
        }
    }
    Ok(())
}

fn unquote_env_value(value: &str) -> String {
    if value.len() >= 2
        && ((value.starts_with('"') && value.ends_with('"'))
            || (value.starts_with('\'') && value.ends_with('\'')))
    {
        value[1..value.len() - 1].to_string()
    } else {
        value.to_string()
    }
}

fn strip_ansi(input: &str) -> String {
    let mut output = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch != '\x1b' {
            output.push(ch);
            continue;
        }
        if chars.peek() == Some(&'[') {
            chars.next();
            for next in chars.by_ref() {
                if ('@'..='~').contains(&next) {
                    break;
                }
            }
        }
    }
    output
}

fn print_native_transcript(app: &mut App) -> Result<()> {
    let width = crossterm::terminal::size()
        .map(|(width, _)| width)
        .unwrap_or(app.args.width);
    app.drain_store_notifications()?;
    let state = app.workbench_state()?;
    if let Some(model) = transcript::transcript_model(app, &state) {
        print!("{}", transcript::model_plain_text(&model));
    } else {
        let lines = native_scrollback_lines(app, width)?;
        print!("{}", lines_plain_text(&lines));
    }
    io::stdout().flush()?;
    Ok(())
}

enum TerminalRunOutcome {
    Quit,
    Reexec,
}

fn run_terminal(mut app: App) -> Result<()> {
    let reload_requested = install_reexec_signal_handler()?;
    let mut viewport_height = desired_terminal_viewport_height(&mut app)?;
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(
        stdout,
        Clear(ClearType::All),
        MoveTo(0, 0),
        EnableBracketedPaste,
        PushKeyboardEnhancementFlags(
            KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                | KeyboardEnhancementFlags::REPORT_EVENT_TYPES
                | KeyboardEnhancementFlags::REPORT_ALTERNATE_KEYS
        )
    )?;
    let mut terminal_driver = TerminalDriver::new(viewport_height)?;
    let result = (|| -> Result<TerminalRunOutcome> {
        let mut draw_needed = true;
        let mut last_fallback_refresh = Instant::now();
        let mut last_anim_tick = Instant::now();
        let mut last_live_spinner_tick = Instant::now();
        let mut pending_resize_at: Option<Instant> = None;
        loop {
            if reload_requested
                .as_ref()
                .is_some_and(|flag| flag.swap(false, Ordering::SeqCst))
            {
                break Ok(TerminalRunOutcome::Reexec);
            }
            draw_needed |= app.drain_store_notifications()?;
            draw_needed |= app.drain_oauth_notifications()?;
            draw_needed |= app.drain_codex_login_notifications()?;
            if last_fallback_refresh.elapsed() >= STORE_FALLBACK_REFRESH_INTERVAL {
                draw_needed |= app.refresh_state_cache_from_store()?;
                last_fallback_refresh = Instant::now();
            }
            if let Some(resize_at) = pending_resize_at {
                if resize_at.elapsed() >= RESIZE_DEBOUNCE_INTERVAL {
                    terminal_driver.settle_resize(&mut app)?;
                    pending_resize_at = None;
                    draw_needed = true;
                }
            }
            if pending_resize_at.is_none() && draw_needed {
                viewport_height = terminal_driver.resize_if_needed(&mut app, viewport_height)?;
                terminal_driver.draw(&mut app)?;
                draw_needed = false;
            }
            let mut poll_interval = pending_resize_at
                .map(|resize_at| {
                    RESIZE_DEBOUNCE_INTERVAL
                        .saturating_sub(resize_at.elapsed())
                        .min(INPUT_POLL_INTERVAL)
                })
                .unwrap_or(INPUT_POLL_INTERVAL);
            // While the welcome animation is running, don't block on input
            // longer than one anim frame — otherwise the redraw rate is
            // capped by INPUT_POLL_INTERVAL instead of ANIM_TICK_INTERVAL.
            if app.is_welcome_surface() {
                poll_interval = poll_interval.min(ANIM_TICK_INTERVAL);
            }
            if app.should_animate_live_spinner() {
                poll_interval = poll_interval.min(LIVE_SPINNER_TICK_INTERVAL);
            }
            if !event::poll(poll_interval)? {
                // Animate the welcome-screen logo by advancing the anim and
                // triggering a redraw every ~70ms while the welcome surface
                // is up. No-op on other surfaces.
                if app.is_welcome_surface() && last_anim_tick.elapsed() >= ANIM_TICK_INTERVAL {
                    app.welcome_anim.tick();
                    draw_needed = true;
                    last_anim_tick = Instant::now();
                }
                if app.should_animate_live_spinner()
                    && last_live_spinner_tick.elapsed() >= LIVE_SPINNER_TICK_INTERVAL
                {
                    app.tick_live_spinner();
                    draw_needed = true;
                    last_live_spinner_tick = Instant::now();
                }
                continue;
            }
            let event = event::read()?;
            if matches!(event, TermEvent::Resize(_, _)) {
                pending_resize_at = Some(Instant::now());
                continue;
            }
            if handle_terminal_event(event, &mut app, &mut terminal_driver)? {
                break Ok(TerminalRunOutcome::Quit);
            }
            draw_needed = true;
        }
    })();
    let restore_result = terminal_driver.restore_terminal_state();
    let cursor_result = terminal_driver.show_cursor();
    restore_result?;
    cursor_result?;
    match result? {
        TerminalRunOutcome::Quit => Ok(()),
        TerminalRunOutcome::Reexec => reexec_terminal_process(),
    }
}

#[cfg(unix)]
fn install_reexec_signal_handler() -> Result<Option<Arc<AtomicBool>>> {
    let flag = Arc::new(AtomicBool::new(false));
    signal_hook::flag::register(SIGUSR2, Arc::clone(&flag))
        .context("install SIGUSR2 reload handler")?;
    Ok(Some(flag))
}

#[cfg(not(unix))]
fn install_reexec_signal_handler() -> Result<Option<Arc<AtomicBool>>> {
    Ok(None)
}

#[cfg(unix)]
fn request_process_reexec() -> Result<()> {
    signal_hook::low_level::raise(SIGUSR2).context("request terminal reload")
}

#[cfg(not(unix))]
fn request_process_reexec() -> Result<()> {
    anyhow::bail!("terminal reload is only supported on Unix platforms")
}

#[cfg(unix)]
fn reexec_terminal_process() -> Result<()> {
    let exe = reexec_binary_path()?;
    let args = std::env::args_os().skip(1);
    io::stdout().flush()?;
    io::stderr().flush()?;
    Err(ProcessCommand::new(&exe).args(args).exec())
        .with_context(|| format!("re-exec {}", exe.display()))
}

#[cfg(not(unix))]
fn reexec_terminal_process() -> Result<()> {
    anyhow::bail!("terminal reload is only supported on Unix platforms")
}

fn reexec_binary_path() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os(REEXEC_BINARY_ENV)
        .map(PathBuf::from)
        .filter(|path| !path.as_os_str().is_empty())
    {
        return Ok(path);
    }
    std::env::current_exe().context("resolve current executable for reload")
}

struct TerminalDriver {
    terminal: Terminal<CrosstermBackend<io::Stdout>>,
    mouse_capture_enabled: bool,
    manual_modal_overlay_rect: Option<Rect>,
}

impl TerminalDriver {
    fn new(height: u16) -> Result<Self> {
        Ok(Self {
            terminal: new_inline_terminal(height)?,
            mouse_capture_enabled: false,
            manual_modal_overlay_rect: None,
        })
    }

    fn resize_if_needed(&mut self, app: &mut App, current_height: u16) -> Result<u16> {
        let desired_height = desired_terminal_viewport_height(app)?;
        if desired_height == current_height {
            return Ok(current_height);
        }
        reset_terminal_screen(self.terminal.backend_mut(), ClearType::Purge)?;
        self.terminal = new_inline_terminal(desired_height)?;
        app.native_history.reset();
        self.manual_modal_overlay_rect = None;
        Ok(desired_height)
    }

    fn settle_resize(&mut self, app: &mut App) -> Result<()> {
        reset_inline_terminal_after_resize(&mut self.terminal)?;
        app.native_history.reset();
        self.manual_modal_overlay_rect = None;
        Ok(())
    }

    fn draw(&mut self, app: &mut App) -> Result<()> {
        let manual_overlay_active = should_draw_manual_modal_overlay(app);
        let manual_overlay = if manual_overlay_active {
            let state = app.workbench_state()?;
            manual_modal_overlay(app, &state)
        } else {
            None
        };
        let manual_overlay_rect = manual_overlay.as_ref().map(|overlay| overlay.rect);
        if let Some(previous_rect) = self.manual_modal_overlay_rect {
            if manual_overlay_rect != Some(previous_rect) {
                clear_manual_modal_overlay_rect(self.terminal.backend_mut(), previous_rect)?;
            }
        }
        if self.manual_modal_overlay_rect.is_some() && !manual_overlay_active {
            app.native_history.reset_with_clear();
        }
        maybe_emit_native_transcript(&mut self.terminal, app)?;
        self.terminal.draw(|frame| render(frame, app))?;
        if let Some(overlay) = manual_overlay.as_ref() {
            draw_manual_modal_overlay(self.terminal.backend_mut(), overlay)?;
        }
        self.manual_modal_overlay_rect = manual_overlay_rect;
        self.sync_mouse_capture(app)?;
        Ok(())
    }

    fn restore_terminal_state(&mut self) -> Result<()> {
        restore_terminal(self.terminal.backend_mut())
    }

    fn show_cursor(&mut self) -> io::Result<()> {
        self.terminal.show_cursor()
    }

    fn sync_mouse_capture(&mut self, app: &App) -> Result<()> {
        let should_capture = app.should_capture_mouse();
        if should_capture == self.mouse_capture_enabled {
            return Ok(());
        }
        if should_capture {
            execute!(self.terminal.backend_mut(), EnableMouseClickCapture)?;
        } else {
            execute!(self.terminal.backend_mut(), DisableMouseCapture)?;
        }
        self.mouse_capture_enabled = should_capture;
        Ok(())
    }
}

fn new_inline_terminal(height: u16) -> Result<Terminal<CrosstermBackend<io::Stdout>>> {
    let backend = CrosstermBackend::new(io::stdout());
    Ok(Terminal::with_options(
        backend,
        TerminalOptions {
            viewport: Viewport::Inline(height),
        },
    )?)
}

fn desired_terminal_viewport_height(app: &mut App) -> Result<u16> {
    let (terminal_width, terminal_height) =
        crossterm::terminal::size().unwrap_or((app.args.width, app.args.height));
    desired_terminal_viewport_height_for(app, terminal_width, terminal_height)
}

fn desired_terminal_viewport_height_for(
    app: &mut App,
    terminal_width: u16,
    terminal_height: u16,
) -> Result<u16> {
    let full_height = terminal_height.max(app.live_viewport_height());
    let app_width = terminal_width
        .saturating_sub(APP_HORIZONTAL_MARGIN.saturating_mul(2))
        .max(1);
    let dock_height = main_viewport_height(app, app_width);
    if app.is_first_run_setup_visible()?
        || app.selected_session_id.is_none()
        || (app.surface.is_bottom_pane() && !app.native_scrollback_is_active())
    {
        return Ok(full_height);
    }

    let state = app.refresh_cached_projection().clone();
    let transcript_model = transcript::transcript_model(app, &state);
    let body_width = app_width.saturating_sub(4).max(1);
    let stream_skip_lines = state
        .current_session
        .as_ref()
        .map(|session| {
            app.native_history
                .live_stream_emitted_lines_for(&session.id, body_width)
        })
        .unwrap_or(0);
    let active_streaming_lines =
        transcript::active_streaming_lines(transcript_model.as_ref(), body_width);
    let estimated_stream_skip_lines =
        if transcript::active_streaming_can_commit_all(transcript_model.as_ref())
            && active_streaming_lines.len() > 1
        {
            active_streaming_lines.len()
        } else {
            active_streaming_lines.len().saturating_sub(1)
        };
    let stream_skip_lines = stream_skip_lines.max(estimated_stream_skip_lines);
    let active_lines = transcript::active_viewport_lines_with_stream_skip(
        transcript_model.as_ref(),
        body_width,
        u16::MAX,
        stream_skip_lines,
    );
    let active_line_count = if app.selected_session_id.is_some() && app.surface.uses_main_view() {
        active_lines.len().max(1)
    } else {
        active_lines.len()
    };
    Ok(dock_height
        .saturating_add(active_line_count.try_into().unwrap_or(u16::MAX))
        .min(full_height))
}

fn should_draw_manual_modal_overlay(app: &App) -> bool {
    (app.is_slash_palette_active() || app.surface.is_popup()) && app.native_scrollback_is_active()
}

fn manual_modal_overlay(app: &App, state: &WorkbenchState) -> Option<render::ModalOverlay> {
    let (term_w, term_h) = crossterm::terminal::size().unwrap_or((app.args.width, app.args.height));
    if term_w == 0 || term_h == 0 {
        return None;
    }
    let area = Rect::new(0, 0, term_w, term_h);
    render::active_modal_overlay(app, state, area)
}

fn clear_manual_modal_overlay_rect(
    target: &mut CrosstermBackend<io::Stdout>,
    rect: Rect,
) -> Result<()> {
    let (term_w, term_h) = crossterm::terminal::size().unwrap_or((rect.width, rect.height));
    queue!(
        target,
        ResetColor,
        SetAttribute(Attribute::Reset),
        SetForegroundColor(CrosstermColor::Reset),
        SetBackgroundColor(CrosstermColor::Reset)
    )?;
    for y in 0..rect.height {
        let row = rect.y.saturating_add(y);
        if row >= term_h {
            break;
        }
        for x in 0..rect.width {
            let col = rect.x.saturating_add(x);
            if col >= term_w {
                break;
            }
            queue!(target, MoveTo(col, row), Print(" "))?;
        }
    }
    queue!(target, ResetColor, SetAttribute(Attribute::Reset))?;
    target.flush()?;
    Ok(())
}

fn draw_manual_modal_overlay(
    target: &mut CrosstermBackend<io::Stdout>,
    overlay: &render::ModalOverlay,
) -> Result<()> {
    let (term_w, term_h) =
        crossterm::terminal::size().unwrap_or((overlay.rect.width, overlay.rect.height));
    for y in 0..overlay.rect.height {
        let row = overlay.rect.y.saturating_add(y);
        if row >= term_h {
            break;
        }
        for x in 0..overlay.rect.width {
            let col = overlay.rect.x.saturating_add(x);
            if col >= term_w {
                break;
            }
            let cell = &overlay.buffer[(x, y)];
            queue_ratatui_cell_style(target, cell.fg, cell.bg, cell.modifier)?;
            queue!(target, MoveTo(col, row), Print(cell.symbol()))?;
        }
    }
    queue!(target, ResetColor, SetAttribute(Attribute::Reset))?;
    if let Some(cursor) = overlay.cursor {
        queue!(
            target,
            MoveTo(
                cursor.x.min(term_w.saturating_sub(1)),
                cursor.y.min(term_h.saturating_sub(1))
            ),
            Show
        )?;
    }
    target.flush()?;
    Ok(())
}

fn queue_ratatui_cell_style(
    target: &mut CrosstermBackend<io::Stdout>,
    fg: RatatuiColor,
    bg: RatatuiColor,
    modifier: Modifier,
) -> io::Result<()> {
    queue!(
        target,
        SetAttribute(Attribute::Reset),
        SetForegroundColor(ratatui_color_to_crossterm(fg)),
        SetBackgroundColor(ratatui_color_to_crossterm(bg))
    )?;
    if modifier.contains(Modifier::BOLD) {
        queue!(target, SetAttribute(Attribute::Bold))?;
    }
    if modifier.contains(Modifier::DIM) {
        queue!(target, SetAttribute(Attribute::Dim))?;
    }
    if modifier.contains(Modifier::ITALIC) {
        queue!(target, SetAttribute(Attribute::Italic))?;
    }
    if modifier.contains(Modifier::UNDERLINED) {
        queue!(target, SetAttribute(Attribute::Underlined))?;
    }
    if modifier.contains(Modifier::SLOW_BLINK) {
        queue!(target, SetAttribute(Attribute::SlowBlink))?;
    }
    if modifier.contains(Modifier::RAPID_BLINK) {
        queue!(target, SetAttribute(Attribute::RapidBlink))?;
    }
    if modifier.contains(Modifier::REVERSED) {
        queue!(target, SetAttribute(Attribute::Reverse))?;
    }
    if modifier.contains(Modifier::HIDDEN) {
        queue!(target, SetAttribute(Attribute::Hidden))?;
    }
    if modifier.contains(Modifier::CROSSED_OUT) {
        queue!(target, SetAttribute(Attribute::CrossedOut))?;
    }
    Ok(())
}

fn ratatui_color_to_crossterm(color: RatatuiColor) -> CrosstermColor {
    match color {
        RatatuiColor::Reset => CrosstermColor::Reset,
        RatatuiColor::Black => CrosstermColor::Black,
        RatatuiColor::Red => CrosstermColor::DarkRed,
        RatatuiColor::Green => CrosstermColor::DarkGreen,
        RatatuiColor::Yellow => CrosstermColor::DarkYellow,
        RatatuiColor::Blue => CrosstermColor::DarkBlue,
        RatatuiColor::Magenta => CrosstermColor::DarkMagenta,
        RatatuiColor::Cyan => CrosstermColor::DarkCyan,
        RatatuiColor::Gray => CrosstermColor::Grey,
        RatatuiColor::DarkGray => CrosstermColor::DarkGrey,
        RatatuiColor::LightRed => CrosstermColor::Red,
        RatatuiColor::LightGreen => CrosstermColor::Green,
        RatatuiColor::LightYellow => CrosstermColor::Yellow,
        RatatuiColor::LightBlue => CrosstermColor::Blue,
        RatatuiColor::LightMagenta => CrosstermColor::Magenta,
        RatatuiColor::LightCyan => CrosstermColor::Cyan,
        RatatuiColor::White => CrosstermColor::White,
        RatatuiColor::Indexed(value) => CrosstermColor::AnsiValue(value),
        RatatuiColor::Rgb(r, g, b) => CrosstermColor::Rgb { r, g, b },
    }
}

fn handle_terminal_event(
    event: TermEvent,
    app: &mut App,
    terminal_driver: &mut TerminalDriver,
) -> Result<bool> {
    match event {
        TermEvent::Key(key) if is_escape_prefix_candidate(key, app) => {
            handle_escape_prefix_key(key, app, terminal_driver)
        }
        TermEvent::Key(key) => app.handle_key(key),
        TermEvent::Paste(text) => {
            app.handle_paste(&text);
            Ok(false)
        }
        TermEvent::Mouse(MouseEvent {
            kind: MouseEventKind::Down(_),
            column,
            row,
            ..
        }) => {
            app.handle_composer_click(column, row);
            app.handle_welcome_logo_click(column, row);
            Ok(false)
        }
        TermEvent::Resize(_, _) => Ok(false),
        _ => Ok(false),
    }
}

fn reset_inline_terminal_after_resize(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
) -> Result<()> {
    reset_terminal_screen(terminal.backend_mut(), ClearType::Purge)?;
    reset_inline_viewport_origin(terminal)?;
    terminal.autoresize()?;
    terminal.clear()?;
    Ok(())
}

fn is_escape_prefix_candidate(key: KeyEvent, app: &App) -> bool {
    app.surface == Surface::Main
        && matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat)
        && key.code == KeyCode::Esc
        && key.modifiers.is_empty()
}

fn handle_escape_prefix_key(
    escape_key: KeyEvent,
    app: &mut App,
    terminal_driver: &mut TerminalDriver,
) -> Result<bool> {
    if event::poll(Duration::ZERO)? {
        let next_event = event::read()?;
        if is_unmodified_enter_event(&next_event) {
            return app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT));
        }
        if let Some(alt_key) = escape_prefixed_alt_key_event(&next_event) {
            return app.handle_key(alt_key);
        }
        let should_quit = app.handle_key(escape_key)?;
        if should_quit {
            return Ok(true);
        }
        return handle_terminal_event(next_event, app, terminal_driver);
    }
    app.handle_key(escape_key)
}

fn escape_prefixed_alt_key_event(event: &TermEvent) -> Option<KeyEvent> {
    let TermEvent::Key(key) = event else {
        return None;
    };
    if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
        return None;
    }
    if !key.modifiers.is_empty() {
        return None;
    }
    match key.code {
        KeyCode::Left | KeyCode::Right | KeyCode::Char('b' | 'f') => {
            Some(KeyEvent::new(key.code, KeyModifiers::ALT))
        }
        _ => None,
    }
}

/// Case-insensitive substring filter over `state.history`. Returns the
/// indices into the original `Vec<HistoryRow>` whose task text matches.
/// An empty filter is treated as "match everything".
fn history_visible_indices_for(state: &WorkbenchState, filter: &str) -> Vec<usize> {
    let needle = filter.trim().to_ascii_lowercase();
    if needle.is_empty() {
        return (0..state.history.len()).collect();
    }
    state
        .history
        .iter()
        .enumerate()
        .filter(|(_, row)| row.task.to_ascii_lowercase().contains(&needle))
        .map(|(idx, _)| idx)
        .collect()
}

fn is_popup_clear_key(key: KeyEvent) -> bool {
    let command_delete = key
        .modifiers
        .intersects(KeyModifiers::SUPER | KeyModifiers::HYPER | KeyModifiers::META)
        && matches!(key.code, KeyCode::Backspace | KeyCode::Delete);
    let ctrl_u = key.modifiers.contains(KeyModifiers::CONTROL)
        && matches!(key.code, KeyCode::Char('u' | 'U'));
    let raw_ctrl_u = key.modifiers.is_empty() && matches!(key.code, KeyCode::Char('\u{15}'));
    command_delete || ctrl_u || raw_ctrl_u
}

fn is_image_paste_shortcut(ch: char, modifiers: KeyModifiers) -> bool {
    ch.eq_ignore_ascii_case(&'v')
        && modifiers.intersects(
            KeyModifiers::CONTROL
                | KeyModifiers::ALT
                | KeyModifiers::SUPER
                | KeyModifiers::HYPER
                | KeyModifiers::META,
        )
}

fn is_unmodified_enter_event(event: &TermEvent) -> bool {
    matches!(
        event,
        TermEvent::Key(KeyEvent {
            code: KeyCode::Enter,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press | KeyEventKind::Repeat,
            ..
        })
    )
}

fn maybe_emit_native_transcript(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut App,
) -> Result<()> {
    let size = terminal.size()?;
    let state = app.workbench_state()?;
    if !app.surface.uses_main_view()
        || app.is_first_run_setup_visible()?
        || app.surface.is_popup()
        || app.is_slash_palette_active()
    {
        return Ok(());
    }
    let should_clear = app.native_history.take_clear_before_replay();
    if should_clear {
        clear_native_transcript_screen(terminal)?;
    }
    let Some(session) = state.current_session.as_ref() else {
        return Ok(());
    };

    let session_id = session.id.clone();
    let width = native_scrollback_width(size.width);
    let Some(model) = transcript::transcript_model(app, &state) else {
        return Ok(());
    };
    debug_assert_eq!(model.session_id, session_id);
    let _model_revision = model.revision;
    let has_live_streaming_output =
        !transcript::active_streaming_lines(Some(&model), width).is_empty();
    let defer_open_tail = session.status.is_active() && !has_live_streaming_output;

    if !app.native_history.is_active_for(Some(&session_id)) {
        // The session opens with a committed header block (small BU mark,
        // "Browser Use Terminal", cwd) above the conversation content.
        let emission =
            transcript::terminal_scrollback_emission_since(&model, 0, width, defer_open_tail);
        let mut lines = crate::welcome::session_header_lines(width);
        lines.extend(emission.lines);
        insert_initial_native_lines(terminal, lines)?;
        app.native_history
            .reset_for_session_with_group(session_id, emission.last_seq, None);
        maybe_emit_native_live_stream(terminal, app, &model, width)?;
        return Ok(());
    }

    let after_seq = app.native_history.last_seq;
    if model.last_event_seq > after_seq {
        let live_stream_prefix = app
            .native_history
            .live_stream_emitted_text_for(&session_id, width)
            .map(|lines| lines.to_vec());
        let mut emission = transcript::terminal_scrollback_emission_since(
            &model,
            after_seq,
            width,
            defer_open_tail,
        );
        if let Some(prefix) = live_stream_prefix.as_deref() {
            emission.lines = strip_live_stream_prefix(emission.lines, prefix);
        }
        if emission.last_seq > after_seq {
            app.native_history.last_seq = emission.last_seq;
            app.native_history.last_group = None;
            app.native_history.clear_live_stream();
        }
        if !emission.lines.is_empty() {
            insert_native_lines(terminal, emission.lines)?;
        }
    }
    maybe_emit_native_live_stream(terminal, app, &model, width)?;
    Ok(())
}

fn maybe_emit_native_live_stream(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut App,
    model: &transcript::TranscriptModel,
    width: u16,
) -> Result<()> {
    let lines = transcript::active_streaming_lines(Some(model), width);
    let emit_count = if transcript::active_streaming_can_commit_all(Some(model)) && lines.len() > 1
    {
        lines.len()
    } else {
        lines.len().saturating_sub(1)
    };
    if emit_count == 0 {
        app.native_history.clear_live_stream();
        return Ok(());
    }
    let already = app
        .native_history
        .live_stream_emitted_lines_for(&model.session_id, width)
        .min(emit_count);
    if emit_count <= already {
        return Ok(());
    }
    let mut emitted_lines = lines[already..emit_count].to_vec();
    if already == 0 {
        emitted_lines.insert(0, Line::from(""));
    }
    insert_native_lines(terminal, emitted_lines)?;
    let emitted_text_lines = plain_text_lines(&lines[..emit_count]);
    app.native_history.set_live_stream_emitted_lines(
        &model.session_id,
        width,
        emit_count,
        emitted_text_lines,
    );
    Ok(())
}

fn strip_live_stream_prefix(
    lines: Vec<Line<'static>>,
    live_stream_prefix: &[String],
) -> Vec<Line<'static>> {
    if live_stream_prefix.is_empty() || lines.is_empty() {
        return lines;
    }
    let line_text = plain_text_lines(&lines);
    let Some(prefix_start) = live_stream_prefix_start(&line_text, live_stream_prefix) else {
        return lines;
    };
    let prefix_end = prefix_start + live_stream_prefix.len();
    lines
        .into_iter()
        .enumerate()
        .filter_map(|(idx, line)| {
            if (prefix_start..prefix_end).contains(&idx) {
                None
            } else {
                Some(line)
            }
        })
        .collect()
}

fn live_stream_prefix_start(line_text: &[String], live_stream_prefix: &[String]) -> Option<usize> {
    if live_stream_prefix.is_empty() || live_stream_prefix.len() > line_text.len() {
        return None;
    }
    let last_start = line_text.len() - live_stream_prefix.len();
    (0..=last_start).rev().find(|start| {
        line_text[*start..*start + live_stream_prefix.len()]
            .iter()
            .zip(live_stream_prefix.iter())
            .all(|(line, prefix)| line.trim_end() == prefix.trim_end())
    })
}

fn plain_text_lines(lines: &[Line<'static>]) -> Vec<String> {
    lines_plain_text(lines)
        .lines()
        .map(ToOwned::to_owned)
        .collect()
}

fn native_scrollback_width(terminal_width: u16) -> u16 {
    terminal_width
        .saturating_sub(NATIVE_TRANSCRIPT_HORIZONTAL_MARGIN.saturating_mul(2))
        .max(1)
}

fn clear_native_transcript_screen(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
) -> Result<()> {
    reset_terminal_screen(terminal.backend_mut(), ClearType::Purge)?;
    reset_inline_viewport_origin(terminal)?;
    terminal.clear()?;
    Ok(())
}

fn reset_terminal_screen(
    target: &mut CrosstermBackend<io::Stdout>,
    clear_type: ClearType,
) -> Result<()> {
    execute!(
        target,
        Clear(ClearType::All),
        Clear(clear_type),
        MoveTo(0, 0)
    )?;
    Ok(())
}

fn reset_inline_viewport_origin(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
) -> Result<()> {
    terminal.set_cursor_position(Position::ORIGIN)?;
    let size = terminal.size()?;
    let area = Rect::new(0, 0, size.width, size.height);
    terminal.resize(area)?;
    Ok(())
}

fn insert_native_lines(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    lines: Vec<Line<'static>>,
) -> Result<()> {
    if lines.is_empty() {
        return Ok(());
    }
    clear_inline_viewport_for_native_insert(terminal)?;
    let height = lines.len().try_into().unwrap_or(u16::MAX).max(1);
    let hyperlinks = collect_native_hyperlink_segments(&lines);
    terminal.insert_before(height, |buf| {
        let area = buf.area.inner(Margin {
            vertical: 0,
            horizontal: NATIVE_TRANSCRIPT_HORIZONTAL_MARGIN,
        });
        Paragraph::new(lines).render(area, buf);
        apply_native_hyperlinks(buf, area, &hyperlinks);
    })?;
    Ok(())
}

fn clear_inline_viewport_for_native_insert(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
) -> Result<()> {
    terminal.draw(|frame| {
        frame.render_widget(RatatuiClear, frame.area());
    })?;
    Ok(())
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct NativeHyperlinkSegment {
    line: usize,
    start_col: usize,
    width: usize,
    target: String,
}

#[derive(Clone, Debug)]
struct PendingNativeHyperlink {
    target: String,
    segments: Vec<NativeHyperlinkSegment>,
}

#[derive(Clone, Debug)]
struct LinkSpanFragment {
    start_col: usize,
    width: usize,
    text: String,
}

fn collect_native_hyperlink_segments(lines: &[Line<'static>]) -> Vec<NativeHyperlinkSegment> {
    let mut out = Vec::new();
    let mut pending: Option<PendingNativeHyperlink> = None;

    for (line_idx, line) in lines.iter().enumerate() {
        let fragments = link_span_fragments(line);
        let line_is_wrapped_link = !fragments.is_empty() && line_has_only_link_text(line);

        if !line_is_wrapped_link {
            flush_pending_hyperlink(&mut out, &mut pending);
            for fragment in fragments {
                let trimmed = fragment.text.trim();
                if clickable_target_for_link_text(trimmed).is_some() {
                    out.push(NativeHyperlinkSegment {
                        line: line_idx,
                        start_col: fragment.start_col,
                        width: fragment.width,
                        target: trimmed.to_string(),
                    });
                }
            }
            continue;
        }

        let Some(first_fragment) = fragments.first() else {
            continue;
        };
        let first_text = first_fragment.text.trim();
        if clickable_target_for_link_text(first_text).is_some() {
            flush_pending_hyperlink(&mut out, &mut pending);
            pending = Some(PendingNativeHyperlink {
                target: String::new(),
                segments: Vec::new(),
            });
        } else if pending.is_none() {
            continue;
        }

        if let Some(group) = pending.as_mut() {
            for fragment in fragments {
                group.target.push_str(fragment.text.trim());
                group.segments.push(NativeHyperlinkSegment {
                    line: line_idx,
                    start_col: fragment.start_col,
                    width: fragment.width,
                    target: String::new(),
                });
            }
        }
    }

    flush_pending_hyperlink(&mut out, &mut pending);
    out
}

fn flush_pending_hyperlink(
    out: &mut Vec<NativeHyperlinkSegment>,
    pending: &mut Option<PendingNativeHyperlink>,
) {
    let Some(group) = pending.take() else {
        return;
    };
    if clickable_target_for_link_text(&group.target).is_none() {
        return;
    }
    out.extend(
        group
            .segments
            .into_iter()
            .map(|segment| NativeHyperlinkSegment {
                target: group.target.clone(),
                ..segment
            }),
    );
}

fn link_span_fragments(line: &Line<'static>) -> Vec<LinkSpanFragment> {
    let mut fragments = Vec::new();
    let mut col = 0;
    for span in &line.spans {
        let text = span.content.as_ref();
        let width = UnicodeWidthStr::width(text);
        if span.style == theme::link() && !text.trim().is_empty() && width > 0 {
            fragments.push(LinkSpanFragment {
                start_col: col,
                width,
                text: text.to_string(),
            });
        }
        col += width;
    }
    fragments
}

fn line_has_only_link_text(line: &Line<'static>) -> bool {
    line.spans
        .iter()
        .all(|span| span.content.trim().is_empty() || span.style == theme::link())
}

fn clickable_target_for_link_text(value: &str) -> Option<String> {
    let value = value.trim();
    if value.is_empty() || value.chars().any(char::is_control) {
        return None;
    }
    if value.starts_with("https://") || value.starts_with("http://") || value.starts_with("file://")
    {
        return Some(value.replace('\\', "%5C"));
    }
    value
        .starts_with('/')
        .then(|| format!("file://{}", percent_encode_file_url_path(value)))
}

fn percent_encode_file_url_path(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    for byte in path.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'/' | b'.' | b'-' | b'_' | b'~' | b':' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

fn apply_native_hyperlinks(buf: &mut Buffer, area: Rect, hyperlinks: &[NativeHyperlinkSegment]) {
    for segment in hyperlinks {
        let Some(y) = area.y.checked_add(segment.line as u16) else {
            continue;
        };
        if y >= area.bottom() {
            continue;
        }
        let Some(start_x) = area.x.checked_add(segment.start_col as u16) else {
            continue;
        };
        if start_x >= area.right() {
            continue;
        }
        let visible_width = segment
            .width
            .min(area.right().saturating_sub(start_x) as usize);
        if visible_width == 0 {
            continue;
        }
        let end_x = start_x + visible_width as u16 - 1;
        let Some(target) = clickable_target_for_link_text(&segment.target) else {
            continue;
        };
        let open = format!("\x1b]8;;{target}\x1b\\");
        let close = "\x1b]8;;\x1b\\";

        if start_x == end_x {
            let symbol = buf[(start_x, y)].symbol().to_string();
            buf[(start_x, y)].set_symbol(&format!("{open}{symbol}{close}"));
            continue;
        }

        let first_symbol = buf[(start_x, y)].symbol().to_string();
        buf[(start_x, y)].set_symbol(&format!("{open}{first_symbol}"));

        let last_symbol = buf[(end_x, y)].symbol().to_string();
        buf[(end_x, y)].set_symbol(&format!("{last_symbol}{close}"));
    }
}

fn insert_initial_native_lines(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    lines: Vec<Line<'static>>,
) -> Result<()> {
    insert_native_lines(terminal, lines)
}

fn restore_terminal(mut target: impl io::Write) -> Result<()> {
    disable_raw_mode()?;
    execute!(
        target,
        PopKeyboardEnhancementFlags,
        ResetKeyboardEnhancementFlags,
        DisableModifyOtherKeys,
        DisableBracketedPaste,
        DisableMouseCapture,
    )?;
    Ok(())
}

fn seed_demo_if_requested(store: &Store, mode: Option<&str>) -> Result<()> {
    let Some(mode) = mode else {
        return Ok(());
    };
    if !store.list_sessions()?.is_empty() {
        return Ok(());
    }
    store.set_setting("setup.complete", "1")?;
    let session = store.create_session(None, std::env::current_dir()?)?;
    store.append_event(
        &session.id,
        "session.input",
        serde_json::json!({"text": "Find the top 5 Hacker News posts"}),
    )?;
    store.append_event(
        &session.id,
        "browser.page",
        serde_json::json!({
            "url": "https://news.ycombinator.com",
            "title": "Hacker News",
            "tabs": 1,
            "viewport": {"w": 1440, "h": 900},
        }),
    )?;
    store.append_event(
        &session.id,
        "browser.live_url",
        serde_json::json!({"live_url": "https://live.browser-use.com/?wss=example"}),
    )?;
    if mode == "running" {
        store.append_event(
            &session.id,
            "model.turn.request",
            serde_json::json!({"model": "GPT-5.5", "provider": "codex"}),
        )?;
        store.append_event(
            &session.id,
            "model.stream_delta",
            serde_json::json!({"text": "Reading the page and preparing the next browser action..."}),
        )?;
    } else if mode == "done" || mode == "followup" {
        store.append_event(
            &session.id,
            "session.done",
            serde_json::json!({"result": "Top 5 Hacker News posts\n\n1. Example story\n2. Another story\n3. Browser agents in practice"}),
        )?;
        if mode == "followup" {
            store.append_event(
                &session.id,
                "session.followup",
                serde_json::json!({"text": "Which one should I read first?"}),
            )?;
            store.append_event(
                &session.id,
                "session.done",
                serde_json::json!({"result": "Read Example story first. It has the strongest discussion and enough context to decide whether to open the others."}),
            )?;
        }
    } else if mode == "long" {
        let result = (1..=60)
            .map(|idx| format!("- scroll check line {idx}"))
            .collect::<Vec<_>>()
            .join("\n");
        store.append_event(
            &session.id,
            "session.done",
            serde_json::json!({ "result": result }),
        )?;
    } else if mode == "failed" {
        store.append_event(
            &session.id,
            "session.failed",
            serde_json::json!({"error": "OpenRouter API key is missing"}),
        )?;
    } else if mode == "cancelled" || mode == "stopped" {
        store.request_cancel(&session.id, "stopped from terminal")?;
    }
    Ok(())
}

#[cfg(test)]
mod redesign_tests {
    use super::*;

    static BROWSER_USE_TERMINAL_HOME_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn args(temp: &tempfile::TempDir) -> Args {
        Args {
            state_dir: temp.path().to_path_buf(),
            model: None,
            config_profile: None,
            config_overrides: Vec::new(),
            account: "Codex login".to_string(),
            browser: BROWSER_LOCAL_CHROME.to_string(),
            collaboration_mode: CollaborationModeArg::Default,
            dump_screen: true,
            width: 100,
            height: 28,
            select_latest: false,
            seed_demo: None,
            overlay: None,
            agent: AgentBackend::None,
        }
    }

    fn ready_app(temp: &tempfile::TempDir) -> Result<App> {
        let mut app = App::new(args(temp))?;
        app.setup_complete = true;
        app.model_configured = true;
        app.browser = "Local Chrome".to_string();
        app.store.set_setting("setup.complete", "1")?;
        app.store.set_setting("browser", "Local Chrome")?;
        Ok(app)
    }

    fn write_test_png(path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let image = image::ImageBuffer::from_pixel(2, 2, image::Rgba([12u8, 34, 56, 255]));
        image.save(path)?;
        Ok(())
    }

    fn with_browser_use_terminal_home<T>(app_home: &std::path::Path, f: impl FnOnce() -> T) -> T {
        let _lock = BROWSER_USE_TERMINAL_HOME_TEST_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let previous = std::env::var_os("BROWSER_USE_TERMINAL_HOME");
        unsafe {
            std::env::set_var("BROWSER_USE_TERMINAL_HOME", app_home);
        }
        let result = f();
        unsafe {
            match previous {
                Some(value) => std::env::set_var("BROWSER_USE_TERMINAL_HOME", value),
                None => std::env::remove_var("BROWSER_USE_TERMINAL_HOME"),
            }
        }
        result
    }

    #[test]
    fn submit_attached_image_as_local_image_item() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let mut app = ready_app(&temp)?;
        let image_path = temp.path().join("clipboard.png");
        write_test_png(&image_path)?;

        app.composer.set_input("describe this".to_string());
        app.composer.attach_image(image_path.clone());
        app.submit()?;

        let session_id = app
            .selected_session_id
            .clone()
            .context("selected session after submit")?;
        let events = app.store.events_for_session(&session_id)?;
        let input = events
            .iter()
            .find(|event| event.event_type == "session.input")
            .context("session.input")?;
        assert_eq!(input.payload["text"], "[Image 1]\ndescribe this");
        assert!(!input.payload["text"]
            .as_str()
            .unwrap_or_default()
            .contains(image_path.to_string_lossy().as_ref()));
        assert_eq!(input.payload["items"][0]["type"], "local_image");
        assert_eq!(
            input.payload["items"][0]["path"],
            image_path.display().to_string()
        );
        assert_eq!(input.payload["items"][1]["type"], "text");
        let content = input.payload["content"].as_array().context("content")?;
        assert!(content.iter().any(|part| {
            part["type"] == "input_image"
                && part["image_url"]
                    .as_str()
                    .is_some_and(|url| url.starts_with("data:image/png;base64,"))
        }));
        assert!(content
            .iter()
            .any(|part| part["type"] == "input_text" && part["text"] == "describe this"));
        Ok(())
    }

    #[test]
    fn local_image_display_text_uses_labels_not_paths() {
        let payload = serde_json::json!({
            "text": "[local_image:/tmp/but-clipboard-one.png]\n[local_image:/tmp/but-clipboard-two.png]\nwhat is here",
            "items": [
                {"type": "local_image", "path": "/tmp/but-clipboard-one.png"},
                {"type": "local_image", "path": "/tmp/but-clipboard-two.png"},
                {"type": "text", "text": "what is here"}
            ],
        });

        let display = user_input_display_text_from_payload(&payload).expect("display text");
        assert_eq!(display, "[Image 1] [Image 2]\nwhat is here");
        assert!(!display.contains("but-clipboard"));
    }

    #[test]
    fn image_paste_shortcut_accepts_control_alt_and_command_modifiers() {
        for modifiers in [
            KeyModifiers::CONTROL,
            KeyModifiers::ALT,
            KeyModifiers::SUPER,
            KeyModifiers::HYPER,
            KeyModifiers::META,
            KeyModifiers::SUPER | KeyModifiers::SHIFT,
            KeyModifiers::META | KeyModifiers::SHIFT,
        ] {
            assert!(is_image_paste_shortcut('v', modifiers));
            assert!(is_image_paste_shortcut('V', modifiers));
        }

        assert!(!is_image_paste_shortcut('v', KeyModifiers::NONE));
        assert!(!is_image_paste_shortcut('x', KeyModifiers::SUPER));
    }

    fn write_tui_model_catalog(app_home: &std::path::Path) -> Result<()> {
        std::fs::create_dir_all(app_home)?;
        std::fs::write(
            app_home.join("catalog.json"),
            r#"{
  "models": [
    {
      "slug": "hidden-catalog-model",
      "display_name": "Hidden Catalog Model",
      "description": "not picker-visible",
      "visibility": "none",
      "supported_in_api": true,
      "priority": 0
    },
    {
      "slug": "chatgpt-only-catalog",
      "display_name": "ChatGPT Only Catalog",
      "description": "ChatGPT-only catalog model",
      "visibility": "list",
      "supported_in_api": false,
      "priority": 1
    },
    {
      "slug": "catalog-gpt",
      "display_name": "Catalog GPT",
      "description": "Catalog API model",
      "visibility": "list",
      "supported_in_api": true,
      "priority": 2
    }
  ]
}"#,
        )?;
        std::fs::write(
            app_home.join("config.toml"),
            "model_catalog_json = \"catalog.json\"\n",
        )?;
        Ok(())
    }

    #[test]
    fn welcome_logo_click_spins_only_inside_armed_logo_rect() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let mut app = ready_app(&temp)?;
        let _screen = render_dump(&mut app)?;
        let rect = app.welcome_logo_rect.get().context("welcome logo rect")?;

        assert!(app.should_capture_welcome_mouse());
        let initial_vy = app.welcome_anim.vy;
        assert!(app.handle_welcome_logo_click(
            rect.x.saturating_add(rect.width / 2),
            rect.y.saturating_add(rect.height / 2),
        ));
        assert!(app.welcome_anim.vy > initial_vy);

        let after_click = (app.welcome_anim.vx, app.welcome_anim.vy);
        assert!(!app.handle_welcome_logo_click(rect.x.saturating_add(rect.width), rect.y));
        assert_eq!((app.welcome_anim.vx, app.welcome_anim.vy), after_click);

        app.set_input("typing should keep terminal text selection native".to_string());
        assert!(!app.should_capture_welcome_mouse());
        assert!(!app.handle_welcome_logo_click(
            rect.x.saturating_add(rect.width / 2),
            rect.y.saturating_add(rect.height / 2),
        ));
        assert_eq!((app.welcome_anim.vx, app.welcome_anim.vy), after_click);
        Ok(())
    }

    #[test]
    fn setup_logo_click_spins_inside_onboarding_rect() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let mut app = App::new(args(&temp))?;
        let screen = render_dump(&mut app)?;
        let rect = app.welcome_logo_rect.get().context("setup logo rect")?;

        assert!(screen.contains("click me!"));
        assert!(!screen.contains("click logo"));
        assert!(app.should_capture_welcome_mouse());
        let initial_vy = app.welcome_anim.vy;
        assert!(app.handle_welcome_logo_click(
            rect.x.saturating_add(rect.width / 2),
            rect.y.saturating_add(rect.height / 2),
        ));
        assert!(app.welcome_anim.vy > initial_vy);

        let after_click = (app.welcome_anim.vx, app.welcome_anim.vy);
        assert!(!app.handle_welcome_logo_click(rect.x.saturating_add(rect.width), rect.y));
        assert_eq!((app.welcome_anim.vx, app.welcome_anim.vy), after_click);

        let mut narrow_args = args(&temp);
        narrow_args.width = 70;
        let mut narrow_app = App::new(narrow_args)?;
        let narrow_screen = render_dump(&mut narrow_app)?;
        let click_line = narrow_screen
            .lines()
            .find(|line| line.contains("click me!"))
            .context("narrow setup click label")?;
        assert!(click_line.contains("⣿"));
        Ok(())
    }

    #[test]
    fn welcome_mouse_capture_is_scoped_to_rendered_empty_welcome_surface() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let mut app = ready_app(&temp)?;

        assert!(!app.should_capture_welcome_mouse());
        let _screen = render_dump(&mut app)?;
        assert!(app.should_capture_welcome_mouse());

        app.set_input("open example.com".to_string());
        assert!(!app.should_capture_welcome_mouse());
        app.set_input(String::new());

        app.open_surface(Surface::History);
        assert!(!app.should_capture_welcome_mouse());
        app.close_surface();

        let session = app.store.create_session(None, std::env::current_dir()?)?;
        app.selected_session_id = Some(session.id);
        assert!(!app.should_capture_welcome_mouse());
        Ok(())
    }

    #[test]
    fn welcome_mouse_capture_does_not_enable_drag_tracking() -> Result<()> {
        let mut sequence = String::new();
        EnableMouseClickCapture.write_ansi(&mut sequence)?;

        assert!(sequence.contains("\x1b[?1000h"));
        assert!(sequence.contains("\x1b[?1006h"));
        assert!(!sequence.contains("\x1b[?1002h"));
        assert!(!sequence.contains("\x1b[?1003h"));
        Ok(())
    }

    #[test]
    fn composer_click_maps_to_multiline_cursor_position() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let mut app = ready_app(&temp)?;
        app.set_input("first line\nsecond word\nthird".to_string());
        let _screen = render_dump(&mut app)?;
        let rect = app
            .composer_input_rect
            .get()
            .context("composer input rect")?;

        assert!(app.should_capture_mouse());
        assert!(app.handle_composer_click(
            rect.x.saturating_add(2 + "second ".chars().count() as u16),
            rect.y.saturating_add(1),
        ));
        assert_eq!(app.composer.cursor(), "first line\nsecond ".chars().count());

        assert!(app.handle_composer_click(
            rect.x.saturating_add(2 + "thi".chars().count() as u16),
            rect.y.saturating_add(2),
        ));
        assert_eq!(
            app.composer.cursor(),
            "first line\nsecond word\nthi".chars().count()
        );
        Ok(())
    }

    #[test]
    fn escape_prefixed_word_keys_decode_as_alt_navigation() {
        assert_eq!(
            escape_prefixed_alt_key_event(&TermEvent::Key(KeyEvent::new(
                KeyCode::Left,
                KeyModifiers::NONE,
            ))),
            Some(KeyEvent::new(KeyCode::Left, KeyModifiers::ALT))
        );
        assert_eq!(
            escape_prefixed_alt_key_event(&TermEvent::Key(KeyEvent::new(
                KeyCode::Char('b'),
                KeyModifiers::NONE,
            ))),
            Some(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::ALT))
        );
        assert_eq!(
            escape_prefixed_alt_key_event(&TermEvent::Key(KeyEvent::new(
                KeyCode::Up,
                KeyModifiers::NONE,
            ))),
            None
        );
    }

    fn row_containing(screen: &str, needle: &str) -> usize {
        screen
            .lines()
            .position(|line| line.contains(needle))
            .unwrap_or_else(|| panic!("screen did not contain {needle:?}\n{screen}"))
    }

    fn buffer_symbols(buffer: &Buffer) -> String {
        let area = buffer.area;
        let mut out = String::new();
        for y in area.y..area.y.saturating_add(area.height) {
            for x in area.x..area.x.saturating_add(area.width) {
                out.push_str(buffer[(x, y)].symbol());
            }
            out.push('\n');
        }
        out
    }

    fn surface_heading_for_test(surface: Surface) -> &'static str {
        match surface {
            Surface::Account => "Authenticate",
            Surface::Model => "Model",
            Surface::Mode => "Mode",
            Surface::Browser | Surface::BrowserSelect => "Browser",
            Surface::History => "History",
            Surface::Messages => "Messages",
            Surface::Developer => "Developer",
            Surface::ApiKey => "API key",
            Surface::Telemetry => "Laminar",
            Surface::Setup | Surface::SetupConfirm | Surface::SetupResult => "Setup",
            Surface::Main => "",
        }
    }

    #[test]
    fn plan_slash_command_starts_task_with_plan_mode_context() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let mut app = ready_app(&temp)?;

        app.set_input("/plan draft a migration".to_string());
        assert!(!app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))?);

        assert_eq!(app.collaboration_mode, CollaborationModeKind::Plan);
        assert_eq!(
            app.store
                .get_setting(COLLABORATION_MODE_SETTING)?
                .as_deref(),
            Some("plan")
        );
        let session_id = app
            .selected_session_id
            .clone()
            .context("selected session")?;
        let events = app.store.events_for_session(&session_id)?;
        let mode_idx = events
            .iter()
            .position(|event| {
                event.event_type == "session.collaboration_mode"
                    && event.payload["mode"] == serde_json::json!("plan")
            })
            .context("plan mode event")?;
        let input_idx = events
            .iter()
            .position(|event| {
                event.event_type == "session.input"
                    && event.payload["text"] == serde_json::json!("draft a migration")
            })
            .context("session input event")?;

        assert!(mode_idx < input_idx);
        Ok(())
    }

    #[test]
    fn plan_slash_command_does_not_submit_old_mode_followup_while_running() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let mut app = ready_app(&temp)?;
        let session = app.store.create_session(None, std::env::current_dir()?)?;
        app.store.append_event(
            &session.id,
            "session.input",
            serde_json::json!({"text": "existing turn"}),
        )?;
        app.selected_session_id = Some(session.id.clone());
        app.drain_store_notifications()?;

        app.set_input("/plan revise this".to_string());
        assert!(!app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))?);

        let events = app.store.events_for_session(&session.id)?;
        assert_eq!(
            events
                .iter()
                .filter(|event| event.event_type == "session.followup")
                .count(),
            0
        );
        assert_eq!(app.collaboration_mode, CollaborationModeKind::Default);
        assert_eq!(app.composer.input(), "/plan revise this");
        assert!(app.status_notice.as_deref().is_some_and(|notice| notice
            .contains("Collaboration mode can change after the running turn finishes")));
        Ok(())
    }

    #[test]
    fn tui_start_task_persists_typed_input_payload_for_linked_mentions() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let mut app = ready_app(&temp)?;

        app.dispatch(AppCommand::StartTask(
            "check [$Calendar](app://calendar)".to_string(),
        ))?;

        let session_id = app
            .selected_session_id
            .clone()
            .context("selected session")?;
        let input = app
            .store
            .events_for_session(&session_id)?
            .into_iter()
            .find(|event| event.event_type == "session.input")
            .context("session.input")?;
        assert_eq!(
            input.payload["app_connector_ids"],
            serde_json::json!(["calendar"])
        );
        assert!(!input.payload["content"]
            .as_array()
            .context("input content")?
            .iter()
            .any(|part| part["text"].as_str().unwrap_or_default().contains("app://")));
        Ok(())
    }

    #[test]
    fn tui_followup_persists_typed_input_payload_for_linked_mentions() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let mut app = ready_app(&temp)?;
        let session = app.store.create_session(None, std::env::current_dir()?)?;
        app.store.append_event(
            &session.id,
            "session.input",
            serde_json::json!({"text": "existing turn"}),
        )?;
        app.store.append_event(
            &session.id,
            "session.done",
            serde_json::json!({"result": "done"}),
        )?;

        app.dispatch(AppCommand::SendFollowup {
            session_id: session.id.clone(),
            text: "then use [@Notes](plugin://notes@example)".to_string(),
        })?;

        let followup = app
            .store
            .events_for_session(&session.id)?
            .into_iter()
            .find(|event| event.event_type == "session.followup")
            .context("session.followup")?;
        assert_eq!(
            followup.payload["plugin_mentions"][0]["path"],
            "plugin://notes@example"
        );
        assert!(!followup.payload["content"]
            .as_array()
            .context("followup content")?
            .iter()
            .any(|part| part["text"]
                .as_str()
                .unwrap_or_default()
                .contains("plugin://")));
        Ok(())
    }

    #[test]
    fn request_user_input_submit_answers_pending_request_not_followup() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let mut app = ready_app(&temp)?;
        let session = app.store.create_session(None, std::env::current_dir()?)?;
        app.selected_session_id = Some(session.id.clone());
        app.store.append_event(
            &session.id,
            "session.input",
            serde_json::json!({"text": "plan a change"}),
        )?;
        app.store.append_event(
            &session.id,
            REQUEST_USER_INPUT_REQUEST_EVENT,
            serde_json::json!({
                "turn_id": "turn-current",
                "call_id": "ask_scope",
                "questions": [{
                    "id": "scope",
                    "header": "Scope",
                    "question": "Which scope should I use?",
                    "isOther": true,
                    "options": [
                        {"label": "Plan (Recommended)", "description": "Plan only."},
                        {"label": "Build now", "description": "Implement it."}
                    ]
                }]
            }),
        )?;
        app.drain_store_notifications()?;

        let screen = render_dump(&mut app)?;
        assert!(screen.contains("Questions"));
        assert!(screen.contains("Scope [scope]"));
        assert!(screen.contains("Which scope should I use?"));
        assert!(screen.contains("Answer the agent's question..."));

        app.set_input("2".to_string());
        assert!(!app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))?);

        let events = app.store.events_for_session(&session.id)?;
        assert_eq!(
            events
                .iter()
                .filter(|event| event.event_type == "session.followup")
                .count(),
            0
        );
        let response = events
            .iter()
            .find(|event| event.event_type == REQUEST_USER_INPUT_RESPONSE_EVENT)
            .context("request_user_input response event")?;
        assert_eq!(
            response.payload["turn_id"],
            serde_json::json!("turn-current")
        );
        assert_eq!(response.payload["call_id"], serde_json::json!("ask_scope"));
        assert_eq!(
            response.payload["answers"]["scope"]["answers"],
            serde_json::json!(["Build now"])
        );
        assert!(app.composer.is_empty());
        Ok(())
    }

    #[test]
    fn request_user_input_pending_prefers_turn_id_over_stale_call_id_response() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let mut app = ready_app(&temp)?;
        let session = app.store.create_session(None, std::env::current_dir()?)?;
        app.selected_session_id = Some(session.id.clone());
        app.store.append_event(
            &session.id,
            REQUEST_USER_INPUT_REQUEST_EVENT,
            serde_json::json!({
                "turn_id": "turn-current",
                "call_id": "ask_scope",
                "questions": [{
                    "id": "scope",
                    "header": "Scope",
                    "question": "Which scope should I use?",
                    "isOther": true,
                    "options": [
                        {"label": "Plan (Recommended)", "description": "Plan only."},
                        {"label": "Build now", "description": "Implement it."}
                    ]
                }]
            }),
        )?;
        app.store.append_event(
            &session.id,
            REQUEST_USER_INPUT_RESPONSE_EVENT,
            serde_json::json!({
                "turn_id": "turn-stale",
                "call_id": "ask_scope",
                "answers": {
                    "scope": {
                        "answers": ["Plan (Recommended)"]
                    }
                }
            }),
        )?;
        app.drain_store_notifications()?;

        let pending = app
            .pending_request_user_input(&session.id)
            .context("pending request")?;
        assert_eq!(pending.turn_id, "turn-current");
        assert_eq!(pending.call_id, "ask_scope");
        Ok(())
    }

    #[test]
    fn request_user_input_digit_shortcut_selects_option() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let mut app = ready_app(&temp)?;
        let session = app.store.create_session(None, std::env::current_dir()?)?;
        app.selected_session_id = Some(session.id.clone());
        app.store.append_event(
            &session.id,
            REQUEST_USER_INPUT_REQUEST_EVENT,
            serde_json::json!({
                "call_id": "ask_scope",
                "questions": [{
                    "id": "scope",
                    "header": "Scope",
                    "question": "Which scope should I use?",
                    "isOther": true,
                    "options": [
                        {"label": "Plan (Recommended)", "description": "Plan only."},
                        {"label": "Build now", "description": "Implement it."}
                    ]
                }]
            }),
        )?;
        app.drain_store_notifications()?;

        assert!(!app.handle_key(KeyEvent::new(KeyCode::Char('2'), KeyModifiers::NONE))?);

        let events = app.store.events_for_session(&session.id)?;
        let response = events
            .iter()
            .find(|event| event.event_type == REQUEST_USER_INPUT_RESPONSE_EVENT)
            .context("request_user_input response event")?;
        assert_eq!(
            response.payload["answers"]["scope"]["answers"],
            serde_json::json!(["Build now"])
        );
        assert!(app.composer.is_empty());
        Ok(())
    }

    #[test]
    fn request_user_input_notes_append_user_note() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let mut app = ready_app(&temp)?;
        let session = app.store.create_session(None, std::env::current_dir()?)?;
        app.selected_session_id = Some(session.id.clone());
        app.store.append_event(
            &session.id,
            REQUEST_USER_INPUT_REQUEST_EVENT,
            serde_json::json!({
                "call_id": "ask_scope",
                "questions": [{
                    "id": "scope",
                    "header": "Scope",
                    "question": "Which scope should I use?",
                    "isOther": true,
                    "options": [
                        {"label": "Plan (Recommended)", "description": "Plan only."},
                        {"label": "Build now", "description": "Implement it."}
                    ]
                }]
            }),
        )?;
        app.drain_store_notifications()?;

        assert!(!app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE))?);
        assert!(!app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE))?);
        for ch in "keep minimal".chars() {
            assert!(!app.handle_key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE))?);
        }
        assert!(!app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))?);

        let events = app.store.events_for_session(&session.id)?;
        let response = events
            .iter()
            .find(|event| event.event_type == REQUEST_USER_INPUT_RESPONSE_EVENT)
            .context("request_user_input response event")?;
        assert_eq!(
            response.payload["answers"]["scope"]["answers"],
            serde_json::json!(["Build now", "user_note: keep minimal"])
        );
        let state = app.workbench_state()?;
        let model = transcript::transcript_model(&app, &state).expect("model");
        let text = lines_plain_text(&transcript::all_terminal_scrollback_lines(&model, 100));
        assert!(text.contains("Questions 1/1 answered"));
        assert!(text.contains("note: keep minimal"));
        Ok(())
    }

    #[test]
    fn request_user_input_empty_answer_requires_confirmation() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let mut app = ready_app(&temp)?;
        let session = app.store.create_session(None, std::env::current_dir()?)?;
        app.selected_session_id = Some(session.id.clone());
        app.store.append_event(
            &session.id,
            REQUEST_USER_INPUT_REQUEST_EVENT,
            serde_json::json!({
                "call_id": "ask_questions",
                "questions": [
                    {
                        "id": "scope",
                        "header": "Scope",
                        "question": "Which scope?",
                        "isOther": true,
                        "options": [
                            {"label": "Plan (Recommended)", "description": "Plan only."},
                            {"label": "Build", "description": "Build now."}
                        ]
                    },
                    {
                        "id": "risk",
                        "header": "Risk",
                        "question": "Which risk level?",
                        "isOther": true,
                        "options": [
                            {"label": "Low (Recommended)", "description": "Small change."},
                            {"label": "High", "description": "Large change."}
                        ]
                    }
                ]
            }),
        )?;
        app.drain_store_notifications()?;

        assert!(!app.handle_key(KeyEvent::new(KeyCode::Char('1'), KeyModifiers::NONE))?);
        assert!(!app.handle_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE))?);
        assert!(!app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))?);

        let events = app.store.events_for_session(&session.id)?;
        assert!(!events
            .iter()
            .any(|event| event.event_type == REQUEST_USER_INPUT_RESPONSE_EVENT));
        assert!(app
            .status_notice
            .as_deref()
            .is_some_and(|notice| notice.contains("Submit with 1 unanswered question")));
        let screen = render_dump(&mut app)?;
        assert!(screen.contains("Submit with unanswered questions?"));

        assert!(!app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))?);
        let events = app.store.events_for_session(&session.id)?;
        let response = events
            .iter()
            .find(|event| event.event_type == REQUEST_USER_INPUT_RESPONSE_EVENT)
            .context("request_user_input response event")?;
        assert_eq!(
            response.payload["answers"]["scope"]["answers"],
            serde_json::json!(["Plan (Recommended)"])
        );
        assert_eq!(
            response.payload["answers"]["risk"]["answers"],
            serde_json::json!([])
        );
        Ok(())
    }

    #[test]
    fn request_user_input_accepts_keyed_multi_question_answers() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let mut app = ready_app(&temp)?;
        let session = app.store.create_session(None, std::env::current_dir()?)?;
        app.selected_session_id = Some(session.id.clone());
        app.store.append_event(
            &session.id,
            REQUEST_USER_INPUT_REQUEST_EVENT,
            serde_json::json!({
                "call_id": "ask_questions",
                "questions": [
                    {
                        "id": "scope",
                        "header": "Scope",
                        "question": "Which scope?",
                        "isOther": true,
                        "options": [
                            {"label": "Plan (Recommended)", "description": "Plan only."},
                            {"label": "Build", "description": "Build now."}
                        ]
                    },
                    {
                        "id": "risk",
                        "header": "Risk",
                        "question": "Which risk level?",
                        "isOther": true,
                        "options": [
                            {"label": "Low (Recommended)", "description": "Small change."},
                            {"label": "High", "description": "Large change."}
                        ]
                    }
                ]
            }),
        )?;
        app.drain_store_notifications()?;

        app.set_input("risk: 2\nscope: Plan".to_string());
        assert!(!app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))?);

        let events = app.store.events_for_session(&session.id)?;
        let response = events
            .iter()
            .find(|event| event.event_type == REQUEST_USER_INPUT_RESPONSE_EVENT)
            .context("request_user_input response event")?;
        assert_eq!(
            response.payload["answers"]["scope"]["answers"],
            serde_json::json!(["Plan (Recommended)"])
        );
        assert_eq!(
            response.payload["answers"]["risk"]["answers"],
            serde_json::json!(["High"])
        );
        Ok(())
    }

    #[test]
    fn request_user_input_other_number_preserves_user_note() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let mut app = ready_app(&temp)?;
        let session = app.store.create_session(None, std::env::current_dir()?)?;
        app.selected_session_id = Some(session.id.clone());
        app.store.append_event(
            &session.id,
            REQUEST_USER_INPUT_REQUEST_EVENT,
            serde_json::json!({
                "call_id": "ask_scope",
                "questions": [{
                    "id": "scope",
                    "header": "Scope",
                    "question": "Which scope should I use?",
                    "isOther": true,
                    "options": [
                        {"label": "Plan (Recommended)", "description": "Plan only."},
                        {"label": "Build now", "description": "Implement it."}
                    ]
                }]
            }),
        )?;
        app.drain_store_notifications()?;

        app.set_input("3: keep this as a design review".to_string());
        assert!(!app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))?);

        let events = app.store.events_for_session(&session.id)?;
        let response = events
            .iter()
            .find(|event| event.event_type == REQUEST_USER_INPUT_RESPONSE_EVENT)
            .context("request_user_input response event")?;
        assert_eq!(
            response.payload["answers"]["scope"]["answers"],
            serde_json::json!([
                REQUEST_USER_INPUT_OTHER_LABEL,
                "user_note: keep this as a design review"
            ])
        );
        Ok(())
    }

    #[test]
    fn request_user_input_pending_requests_are_fifo() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let mut app = ready_app(&temp)?;
        let session = app.store.create_session(None, std::env::current_dir()?)?;
        app.selected_session_id = Some(session.id.clone());
        for (call_id, question_id) in [("call_1", "first"), ("call_2", "second")] {
            app.store.append_event(
                &session.id,
                REQUEST_USER_INPUT_REQUEST_EVENT,
                serde_json::json!({
                    "call_id": call_id,
                    "questions": [{
                        "id": question_id,
                        "header": question_id,
                        "question": format!("Answer {question_id}?"),
                        "isOther": true,
                        "options": [
                            {"label": "Yes (Recommended)", "description": "Proceed."},
                            {"label": "No", "description": "Stop."}
                        ]
                    }]
                }),
            )?;
        }
        app.drain_store_notifications()?;

        assert_eq!(
            app.pending_request_user_input(&session.id)
                .context("first pending")?
                .call_id,
            "call_1"
        );
        assert!(!app.handle_key(KeyEvent::new(KeyCode::Char('1'), KeyModifiers::NONE))?);
        app.drain_store_notifications()?;
        assert_eq!(
            app.pending_request_user_input(&session.id)
                .context("second pending")?
                .call_id,
            "call_2"
        );
        Ok(())
    }

    #[test]
    fn request_user_input_pending_request_clears_after_terminal_event() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let mut app = ready_app(&temp)?;
        let session = app.store.create_session(None, std::env::current_dir()?)?;
        app.selected_session_id = Some(session.id.clone());
        app.store.append_event(
            &session.id,
            REQUEST_USER_INPUT_REQUEST_EVENT,
            serde_json::json!({
                "call_id": "ask_scope",
                "questions": [{
                    "id": "scope",
                    "header": "Scope",
                    "question": "Which scope?",
                    "options": [
                        {"label": "Plan (Recommended)", "description": "Plan only."},
                        {"label": "Build", "description": "Build now."}
                    ]
                }]
            }),
        )?;
        app.drain_store_notifications()?;
        assert!(app.pending_request_user_input(&session.id).is_some());

        app.store.append_event(
            &session.id,
            "session.done",
            serde_json::json!({"result": "finished"}),
        )?;
        app.drain_store_notifications()?;

        assert!(app.pending_request_user_input(&session.id).is_none());
        Ok(())
    }

    #[test]
    fn streaming_proposed_plan_renders_as_plan_not_raw_tags() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let mut app = ready_app(&temp)?;
        let session = app.store.create_session(None, std::env::current_dir()?)?;
        app.store.append_event(
            &session.id,
            "session.input",
            serde_json::json!({"text": "plan the migration"}),
        )?;
        app.store.append_event(
            &session.id,
            "session.collaboration_mode",
            serde_json::json!({"mode": "plan"}),
        )?;
        app.store.append_event(
            &session.id,
            "model.stream_delta",
            serde_json::json!({
                "text": "Intro\n<proposed_plan>\n- Step 1\n- Step 2\n</proposed_plan>\nOutro",
            }),
        )?;
        app.selected_session_id = Some(session.id.clone());
        app.drain_store_notifications()?;

        let state = app.workbench_state()?;
        let model = transcript::transcript_model(&app, &state).expect("model");
        let text = lines_plain_text(&transcript::active_viewport_lines(Some(&model), 100, 20));

        assert!(text.contains("Intro"));
        assert!(text.contains("Outro"));
        assert!(text.contains("Proposed Plan"));
        assert!(text.contains("Step 1"));
        assert!(!text.contains("<proposed_plan>"));
        assert!(!text.contains("</proposed_plan>"));
        Ok(())
    }

    #[test]
    fn default_mode_streaming_keeps_literal_proposed_plan_tags() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let mut app = ready_app(&temp)?;
        let session = app.store.create_session(None, std::env::current_dir()?)?;
        app.store.append_event(
            &session.id,
            "session.input",
            serde_json::json!({"text": "write literal tags"}),
        )?;
        app.store.append_event(
            &session.id,
            "session.collaboration_mode",
            serde_json::json!({"mode": "default"}),
        )?;
        app.store.append_event(
            &session.id,
            "model.stream_delta",
            serde_json::json!({
                "text": "Literal\n<proposed_plan>\nnot a plan\n</proposed_plan>",
            }),
        )?;
        app.selected_session_id = Some(session.id.clone());
        app.drain_store_notifications()?;

        let state = app.workbench_state()?;
        let model = transcript::transcript_model(&app, &state).expect("model");
        let text = lines_plain_text(&transcript::active_viewport_lines(Some(&model), 100, 20));

        assert!(text.contains("Literal"));
        assert!(text.contains("<proposed_plan>"));
        assert!(text.contains("</proposed_plan>"));
        assert!(!text.contains("Proposed Plan"));
        Ok(())
    }

    #[test]
    fn dotenv_loader_sets_missing_env_vars_without_overriding_existing_values() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let loaded_key = format!("BUT_DOTENV_LOADED_{}", std::process::id());
        let existing_key = format!("BUT_DOTENV_EXISTING_{}", std::process::id());
        unsafe {
            std::env::remove_var(&loaded_key);
            std::env::set_var(&existing_key, "already-exported");
        }
        let result = (|| -> Result<()> {
            let path = temp.path().join(".env");
            std::fs::write(
                &path,
                format!(
                    "# comments are ignored\n{loaded_key}=\"from dotenv\"\n{existing_key}=from-file\nMALFORMED_LINE\n",
                ),
            )?;

            load_dotenv_path(&path)?;

            assert_eq!(std::env::var(&loaded_key).as_deref(), Ok("from dotenv"));
            assert_eq!(
                std::env::var(&existing_key).as_deref(),
                Ok("already-exported")
            );
            Ok(())
        })();
        unsafe {
            std::env::remove_var(&loaded_key);
            std::env::remove_var(&existing_key);
        }
        result
    }

    #[test]
    fn first_run_setup_is_activation_not_completion_modal() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let mut app = App::new(args(&temp))?;
        let screen = render_dump(&mut app)?;
        assert!(screen.contains("Welcome to Browser Use Terminal"));
        assert!(screen.contains("Choose a provider below."));
        assert!(screen.contains("PROVIDERS"));
        assert!(!screen.contains("CHOOSE PROVIDER"));
        assert!(screen.contains("Codex login"));
        assert!(!screen.contains("Claude Code subscription"));
        assert!(screen.contains("OpenRouter API key"));
        assert!(screen.contains("click me!"));
        assert!(!screen.contains("click logo"));
        assert!(!screen.contains("CHOOSE MODEL"));
        assert!(!screen.contains("CHOOSE ACCOUNT"));
        assert!(!screen.contains("with ChatGPT plan"));
        assert!(!screen.contains("with subscription"));
        assert!(!screen.contains("Qwen, Kimi, DeepSeek"));
        assert!(!screen.contains("step 1/3"));
        assert!(!screen.contains("[needs]"));

        assert!(!app.handle_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE))?);
        assert!(app.composer.is_empty());
        let screen = render_dump(&mut app)?;
        assert!(screen.contains("PROVIDERS"));
        assert!(!screen.contains("Tell the browser what to do"));
        app.store
            .set_setting("auth.codex.access_token", "codex-test-token")?;
        app.store
            .set_setting("auth.codex.account_id", "codex-test-account")?;

        // Up/Down navigate the provider rows and wrap around the edges.
        assert_eq!(app.selected_row, 0);
        for _ in 0..ACCOUNT_CHOICES.len() - 1 {
            assert!(!app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE))?);
        }
        assert_eq!(app.selected_row, ACCOUNT_CHOICES.len() - 1);
        assert!(!app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE))?);
        assert_eq!(app.selected_row, 0);
        assert!(!app.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE))?);
        assert_eq!(app.selected_row, ACCOUNT_CHOICES.len() - 1);
        for _ in 0..ACCOUNT_CHOICES.len() - 1 {
            assert!(!app.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE))?);
        }
        assert_eq!(app.selected_row, 0);

        // Default row 0 = Codex login / GPT-5.5. Enter first opens a
        // confirmation surface instead of completing setup immediately.
        assert!(!app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))?);
        assert_eq!(app.surface, Surface::SetupConfirm);
        let screen = render_dump(&mut app)?;
        assert!(screen.contains("Use Codex login?"));
        assert!(!app.setup_complete);

        assert!(!app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))?);
        assert_eq!(app.surface, Surface::SetupResult);
        let screen = render_dump(&mut app)?;
        assert!(screen.contains("Connected with Codex auth."));
        assert!(!app.setup_complete);

        assert!(!app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))?);
        assert_eq!(app.surface, Surface::Main);
        assert!(app.setup_complete);
        assert_eq!(app.account, "Codex login");
        assert_eq!(app.model, "GPT-5.5");
        assert_eq!(app.browser, BROWSER_LOCAL_CHROME);
        assert!(app.status_notice.is_none());
        let screen = render_dump(&mut app)?;
        assert!(screen.contains("Tell the browser what to do"));
        assert!(!screen.contains("Model set to"));
        Ok(())
    }

    #[test]
    fn reset_onboarding_shows_setup_even_with_existing_history() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let store = Store::open(temp.path())?;
        store.set_setting("setup.complete", "1")?;
        let session = store.create_session(None, std::env::current_dir()?)?;
        store.append_event(
            &session.id,
            "session.input",
            serde_json::json!({"text": "previous task"}),
        )?;
        store.append_event(
            &session.id,
            "session.done",
            serde_json::json!({"result": "previous result"}),
        )?;
        store.set_setting("setup.complete", "0")?;
        drop(store);

        let mut app = App::new(args(&temp))?;
        assert!(!app.state_cache.sessions.is_empty());
        assert!(app.is_first_run_setup_visible()?);

        let screen = render_dump(&mut app)?;
        assert!(screen.contains("PROVIDERS"));
        assert!(screen.contains("Codex login"));
        assert!(!screen.contains("Tell the browser what to do"));
        Ok(())
    }

    #[test]
    fn seeded_demo_state_stays_out_of_onboarding() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let app_args = Args {
            seed_demo: Some("done".to_string()),
            browser: "Local Chrome".to_string(),
            ..args(&temp)
        };
        let mut app = App::new(app_args)?;

        assert!(!app.state_cache.sessions.is_empty());
        assert!(!app.is_first_run_setup_visible()?);

        let screen = render_dump(&mut app)?;
        assert!(screen.contains("Tell the browser what to do"));
        assert!(!screen.contains("step 1/3"));
        Ok(())
    }

    #[test]
    fn cloud_browser_without_key_is_not_reported_ready() -> Result<()> {
        let saved = std::env::var("BROWSER_USE_API_KEY").ok();
        unsafe {
            std::env::remove_var("BROWSER_USE_API_KEY");
        }
        let result = (|| -> Result<()> {
            let temp = tempfile::tempdir()?;
            let mut app = App::new(args(&temp))?;
            app.setup_complete = true;
            app.model_configured = true;
            app.browser = BROWSER_USE_CLOUD.to_string();
            app.store.set_setting("setup.complete", "1")?;

            let _screen = render_dump(&mut app)?;
            // NOTE: the ready/welcome screen no longer carries the
            // "Browser Use cloud needs key" warning. That warning still
            // shows on the BrowserSelect surface (asserted below); the
            // welcome screen redesign needs a follow-up surface for it.

            app.open_surface(Surface::BrowserSelect);
            let screen = render_dump(&mut app)?;
            assert!(screen.contains("Browser Use cloud . needs key"));
            Ok(())
        })();
        if let Some(value) = saved {
            unsafe {
                std::env::set_var("BROWSER_USE_API_KEY", value);
            }
        }
        result
    }

    #[test]
    fn cloud_browser_without_key_blocks_task_submission() -> Result<()> {
        let saved = std::env::var("BROWSER_USE_API_KEY").ok();
        unsafe {
            std::env::remove_var("BROWSER_USE_API_KEY");
        }
        let result = (|| -> Result<()> {
            let temp = tempfile::tempdir()?;
            let mut app = App::new(args(&temp))?;
            app.setup_complete = true;
            app.model_configured = true;
            app.browser = BROWSER_USE_CLOUD.to_string();
            app.store.set_setting("setup.complete", "1")?;

            app.set_input("open example.com".to_string());
            assert!(!app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))?);
            assert_eq!(app.surface, Surface::ApiKey);
            assert_eq!(app.store.list_sessions()?.len(), 0);
            assert!(app
                .status_notice
                .as_deref()
                .is_some_and(|notice| notice.contains("Browser Use cloud key is missing")));
            Ok(())
        })();
        if let Some(value) = saved {
            unsafe {
                std::env::set_var("BROWSER_USE_API_KEY", value);
            }
        }
        result
    }

    #[test]
    fn browser_use_cloud_key_can_be_saved_from_tui() -> Result<()> {
        let saved = std::env::var("BROWSER_USE_API_KEY").ok();
        unsafe {
            std::env::remove_var("BROWSER_USE_API_KEY");
        }
        let result = (|| -> Result<()> {
            let temp = tempfile::tempdir()?;
            let mut app = App::new(args(&temp))?;
            app.setup_complete = true;
            app.model_configured = true;
            app.store.set_setting("setup.complete", "1")?;
            app.open_surface(Surface::BrowserSelect);
            app.selected_row = BROWSER_CHOICES
                .iter()
                .position(|browser| *browser == BROWSER_USE_CLOUD)
                .context("cloud browser choice")?;

            assert!(!app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))?);
            assert_eq!(app.surface, Surface::ApiKey);
            assert_eq!(app.api_key_account.as_deref(), Some(BROWSER_USE_CLOUD));
            app.set_input("bu-test-key".to_string());
            assert!(!app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))?);
            assert_eq!(
                app.store
                    .get_setting(BROWSER_USE_CLOUD_API_KEY_SETTING)?
                    .as_deref(),
                Some("bu-test-key")
            );
            assert_eq!(app.browser, BROWSER_USE_CLOUD);
            assert!(app.browser_use_cloud_key_ready()?);
            Ok(())
        })();
        if let Some(value) = saved {
            unsafe {
                std::env::set_var("BROWSER_USE_API_KEY", value);
            }
        }
        result
    }

    #[test]
    fn account_flow_collects_api_key_inline() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let mut app = App::new(args(&temp))?;
        app.open_surface(Surface::Account);
        app.selected_row = 3;
        assert!(!app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))?);
        assert_eq!(app.surface, Surface::ApiKey);
        for ch in "sk-or-v1-test".chars() {
            assert!(!app.handle_key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE))?);
        }
        let screen = render_dump(&mut app)?;
        assert!(screen.contains("OpenRouter API key"));
        assert!(screen.contains("sk-or-v1"));
        assert!(!screen.contains("sk-or-v1-test"));
        assert!(!app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))?);
        assert_eq!(
            app.store.get_setting("auth.openrouter.api_key")?.as_deref(),
            Some("sk-or-v1-test")
        );
        assert_eq!(app.surface, Surface::Main);
        assert!(app.setup_complete);
        assert_eq!(app.account, settings::ACCOUNT_OPENROUTER);
        assert_eq!(app.model, "Qwen3.6 Plus");
        Ok(())
    }

    #[test]
    fn model_selection_routes_to_required_sign_in() -> Result<()> {
        let saved = std::env::var("OPENROUTER_API_KEY").ok();
        std::env::remove_var("OPENROUTER_API_KEY");
        let result = (|| -> Result<()> {
            let temp = tempfile::tempdir()?;
            let mut app = App::new(args(&temp))?;
            app.open_surface(Surface::Model);
            app.selected_row = app
                .model_choices
                .iter()
                .position(|choice| choice.provider_model == "moonshotai/kimi-k2.5")
                .context("Kimi OpenRouter model row")?;
            assert!(!app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))?);
            assert_eq!(app.model, "Kimi K2.5");
            assert_eq!(app.account, "OpenRouter API key");
            assert_eq!(app.surface, Surface::ApiKey);
            Ok(())
        })();
        if let Some(value) = saved {
            std::env::set_var("OPENROUTER_API_KEY", value);
        }
        result
    }

    #[test]
    fn result_screen_is_transcript_first_and_markdown_is_clean() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let mut app = ready_app(&temp)?;
        let session = app.store.create_session(None, std::env::current_dir()?)?;
        app.store.append_event(
            &session.id,
            "session.input",
            serde_json::json!({"text": "inspect cart"}),
        )?;
        app.store.append_event(
            &session.id,
            "browser.state",
            serde_json::json!({"url": "https://example.com/cart", "title": "Cart", "tabs": 1, "viewport": {"w": 1440, "h": 900}}),
        )?;
        app.store.append_event(
            &session.id,
            "session.done",
            serde_json::json!({"result": "Your cart has **14 items**.\n\n- [Example item](https://example.com/item) with `coupon.json`\n- /tmp/cart.json"}),
        )?;
        app.selected_session_id = Some(session.id);
        let screen = render_dump(&mut app)?;
        assert!(screen.contains("inspect cart"));
        assert!(screen.contains("• browser"));
        assert!(!screen.contains("• answer"));
        assert!(screen.contains("source https://example.com/cart"));
        assert!(screen.contains("Your cart has 14 items."));
        assert!(screen.contains("Example item (https://example.com/item)"));
        assert!(screen.contains("/tmp/cart.json"));
        assert!(!screen.contains("**14 items**"));
        assert!(!screen.contains("`coupon.json`"));
        assert!(!screen.contains("┌"));
        Ok(())
    }

    #[test]
    fn startup_warnings_and_instruction_sources_are_visible() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let mut app = ready_app(&temp)?;
        let session = app.store.create_session(None, std::env::current_dir()?)?;
        let agents_path = temp.path().join("AGENTS.md").display().to_string();
        app.store.append_event(
            &session.id,
            "session.instruction_sources",
            serde_json::json!({
                "source": "agents_md",
                "sources": [agents_path],
            }),
        )?;
        app.store.append_event(
            &session.id,
            "session.startup_warning",
            serde_json::json!({
                "source": "agents_md",
                "message": "Project AGENTS.md instructions contain invalid UTF-8",
            }),
        )?;
        app.store.append_event(
            &session.id,
            "session.input",
            serde_json::json!({"text": "inspect cart"}),
        )?;
        app.selected_session_id = Some(session.id);

        let screen = render_dump(&mut app)?;
        assert!(screen.contains("• warning"), "{screen}");
        assert!(screen.contains("invalid UTF-8"), "{screen}");

        app.open_surface(Surface::Developer);
        let screen = render_dump(&mut app)?;
        assert!(screen.contains("Instruction sources"), "{screen}");
        assert!(screen.contains("AGENTS.md"), "{screen}");
        assert!(screen.contains("Startup warnings"), "{screen}");
        Ok(())
    }

    #[test]
    fn idle_and_completed_screens_stay_top_aligned() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let mut app = ready_app(&temp)?;
        app.args.height = 44;
        let running_session = app.store.create_session(None, std::env::current_dir()?)?;
        app.store.append_event(
            &running_session.id,
            "session.input",
            serde_json::json!({"text": "run near the top"}),
        )?;
        app.store.append_event(
            &running_session.id,
            "browser.state",
            serde_json::json!({"url": "https://example.com", "title": "Example"}),
        )?;
        app.selected_session_id = Some(running_session.id);
        let running_screen = render_dump(&mut app)?;
        assert!(running_screen.contains("> run near the top"));
        assert!(running_screen.contains("• browser"));
        assert!(!running_screen.contains("• thought"));
        let running_composer_row = row_containing(&running_screen, "Type to steer the agent...");
        assert!(!running_screen.contains("Processing browser task"));
        assert!(!running_screen.contains("AI ENGINE"));
        let running_activity_row = row_containing(&running_screen, "opened example.com");
        assert!(running_composer_row > running_activity_row);
        assert!(running_composer_row.saturating_sub(running_activity_row) <= 8);

        let session = app.store.create_session(None, std::env::current_dir()?)?;
        app.store.append_event(
            &session.id,
            "session.input",
            serde_json::json!({"text": "inspect top alignment"}),
        )?;
        app.store.append_event(
            &session.id,
            "session.done",
            serde_json::json!({"result": "Everything should sit near the top."}),
        )?;
        app.store.append_event(
            &session.id,
            "model.usage",
            serde_json::json!({"input_tokens": 24500, "cost_usd": 0.0731}),
        )?;

        app.selected_session_id = None;
        let ready_screen = render_dump(&mut app)?;
        // Current welcome screen: centered logo plus the shortcut hint.
        assert!(ready_screen.contains("Browser Use"));
        assert!(ready_screen.contains(concat!("v", env!("CARGO_PKG_VERSION"))));
        assert!(ready_screen.contains("press / for shortcuts"));
        // Fused composer carries model metadata in the status row.
        assert!(ready_screen.contains("GPT-5.5"));
        // Composer placeholder stays the same so users see the prompt-to-act.
        assert!(ready_screen.contains("Tell the browser what to do..."));
        assert!(!ready_screen.contains("[ new task ]"));

        app.selected_session_id = Some(session.id);
        let completed_screen = render_dump(&mut app)?;
        assert!(completed_screen.contains("inspect top alignment"));
        assert!(!completed_screen.contains("• answer"));
        assert!(!completed_screen.contains("• done"));
        // Footer status bar surfaces the active model and a context-fill bar.
        assert!(completed_screen.contains("24.5k/60k"));
        let composer_row = row_containing(&completed_screen, "Ask a follow-up...");
        let result_row = row_containing(&completed_screen, "Everything should sit near the top.");
        assert!(composer_row > result_row);
        Ok(())
    }

    #[test]
    fn composer_context_bar_uses_latest_token_count_like_codex() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let mut app = ready_app(&temp)?;
        let session = app.store.create_session(None, std::env::current_dir()?)?;
        app.store.append_event(
            &session.id,
            "session.input",
            serde_json::json!({"text": "inspect token count"}),
        )?;
        app.store.append_event(
            &session.id,
            "session.done",
            serde_json::json!({"result": "Context accounting is visible."}),
        )?;
        app.store.append_event(
            &session.id,
            "model.usage",
            serde_json::json!({"input_tokens": 999, "cost_usd": 0.0123}),
        )?;
        app.store.append_event(
            &session.id,
            "token_count",
            serde_json::json!({
                "info": {
                    "last_token_usage": {
                        "input_tokens": 0,
                        "cached_input_tokens": 0,
                        "output_tokens": 0,
                        "reasoning_output_tokens": 0,
                        "total_tokens": 12345
                    },
                    "total_token_usage": {
                        "input_tokens": 20,
                        "cached_input_tokens": 0,
                        "output_tokens": 10,
                        "reasoning_output_tokens": 0,
                        "total_tokens": 30
                    },
                    "model_context_window": 100000
                },
                "rate_limits": null,
                "turn_idx": 0
            }),
        )?;
        app.store.append_event(
            &session.id,
            "token_count",
            serde_json::json!({
                "info": null,
                "rate_limits": {"limit_id": "codex"},
                "turn_idx": 0
            }),
        )?;

        app.selected_session_id = Some(session.id);
        let screen = render_dump(&mut app)?;

        assert!(screen.contains("12.3k/100k"));
        assert!(!screen.contains("999/60k"));
        assert!(screen.contains("$0.0123"));
        Ok(())
    }

    #[test]
    fn composer_border_shows_current_browser() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let mut app = ready_app(&temp)?;
        app.browser = BROWSER_USE_CLOUD.to_string();

        let screen = render_dump(&mut app)?;

        assert!(screen.contains(BROWSER_USE_CLOUD));
        Ok(())
    }

    #[test]
    fn helper_completion_renders_as_result_not_activity_blob() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let mut app = ready_app(&temp)?;
        let session = app.store.create_session(None, std::env::current_dir()?)?;
        app.store.append_event(
            &session.id,
            "session.input",
            serde_json::json!({"text": "what is in this repo?"}),
        )?;
        app.store.append_event(
            &session.id,
            "agent.spawned",
            serde_json::json!({"child_session_id": "child", "nickname": "repo-explorer"}),
        )?;
        app.store.append_event(
            &session.id,
            "session.followup",
            serde_json::json!({"text": "whats happening"}),
        )?;
        app.store.append_event(
            &session.id,
            "agent.completed",
            serde_json::json!({
                "child_session_id": "child",
                "payload": {
                    "result": "Repository summary:\n\n- **Purpose:** Rust-first terminal workbench\n- `crates/browser-use-tui` owns the UI"
                },
            }),
        )?;
        app.selected_session_id = Some(session.id);
        let screen = render_dump(&mut app)?;
        assert!(screen.contains("whats happening"));
        assert!(screen.contains("• subagent repo-explorer started"));
        assert!(screen.contains("• subagent repo-explorer finished"));
        assert!(!screen.contains("• answer"));
        assert!(!screen.contains("Purpose: Rust-first terminal workbench"));
        // The child result body must not blob into the parent transcript. Match
        // the result's own text, not the bare crate path — the session header's
        // cwd line legitimately shows the working directory.
        assert!(!screen.contains("owns the UI"));
        assert!(!screen.contains("helper finished: Repository summary"));
        assert!(!screen.contains("**Purpose:**"));
        Ok(())
    }

    #[test]
    fn command_palette_filters_and_exposes_only_product_actions() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let mut app = ready_app(&temp)?;
        assert!(!app.handle_key(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE))?);
        assert!(app.is_slash_palette_active());
        let screen = render_dump(&mut app)?;
        let input_row = row_containing(&screen, "> ");
        // The palette owns its own input row, with command items rendered just
        // below it.
        assert!(screen
            .lines()
            .enumerate()
            .any(|(idx, line)| idx > input_row && line.contains("/task")));
        assert!(screen.contains("/task"));
        assert!(screen.contains("/history"));
        assert!(screen.contains("/browser"));
        assert!(screen.contains("/mode"));
        assert!(screen.contains("/plan"));
        assert!(screen.contains("/model"));
        assert!(!screen.contains("/auth"));
        assert!(!screen.contains("/laminar"));
        assert!(screen.contains("start a new task"));
        assert!(screen.contains("change browser backend"));
        assert!(screen.contains("choose collaboration mode"));
        assert!(screen.contains("switch to Plan mode"));
        assert!(!screen.contains("filter actions"));
        assert!(!screen.contains("tab history"));
        assert!(!screen.contains("Open browser"));
        assert!(!screen.contains("Reconnect browser"));
        for ch in "mo".chars() {
            assert!(!app.handle_key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE))?);
        }
        let screen = render_dump(&mut app)?;
        let input_row = row_containing(&screen, "> mo");
        assert!(screen
            .lines()
            .enumerate()
            .any(|(idx, line)| idx > input_row && line.contains("/model")));
        assert!(screen.contains("/model"));
        assert!(!app.handle_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::SUPER))?);
        assert_eq!(app.palette_filter(), "");
        let screen = render_dump(&mut app)?;
        let input_row = row_containing(&screen, "> ");
        assert!(screen
            .lines()
            .enumerate()
            .any(|(idx, line)| idx > input_row && line.contains("/task")));
        assert!(screen.contains("/history"));
        for ch in "auth".chars() {
            assert!(!app.handle_key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE))?);
        }
        let screen = render_dump(&mut app)?;
        let input_row = row_containing(&screen, "> auth");
        assert!(screen
            .lines()
            .enumerate()
            .any(|(idx, line)| idx > input_row && line.contains("/auth")));
        assert!(screen.contains("sign in to a provider"));
        assert!(!app.handle_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL))?);
        assert_eq!(app.palette_filter(), "");
        for ch in "bro".chars() {
            assert!(!app.handle_key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE))?);
        }
        assert_eq!(app.palette_filter(), "bro");
        assert!(!app.handle_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL))?);
        assert_eq!(app.palette_filter(), "");
        let screen = render_dump(&mut app)?;
        assert!(screen.contains("/task"));
        assert!(screen.contains("/model"));
        Ok(())
    }

    #[test]
    fn slash_exit_command_quits() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let mut app = ready_app(&temp)?;

        assert!(!app.handle_key(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE))?);
        for ch in "exit".chars() {
            assert!(!app.handle_key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE))?);
        }
        let screen = render_dump(&mut app)?;
        assert!(screen.contains("/exit"));

        assert!(app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))?);
        Ok(())
    }

    #[test]
    fn slash_in_non_empty_composer_is_prompt_text() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let mut app = ready_app(&temp)?;

        for ch in "open http:".chars() {
            assert!(!app.handle_key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE))?);
        }
        assert!(!app.handle_key(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE))?);

        assert!(!app.is_slash_palette_active());
        assert_eq!(app.composer.input(), "open http:/");
        Ok(())
    }

    #[test]
    fn slash_palette_closes_when_switching_surfaces() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let mut app = ready_app(&temp)?;

        assert!(!app.handle_key(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE))?);
        assert!(app.is_slash_palette_active());

        app.open_surface(Surface::Model);
        assert!(!app.palette_open);
        app.close_surface();
        assert_eq!(app.surface, Surface::Main);
        assert!(!app.is_slash_palette_active());
        Ok(())
    }

    #[test]
    fn popup_text_inputs_handle_command_delete() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let mut app = App::new(args(&temp))?;
        app.open_surface(Surface::Account);
        app.selected_row = 3;
        assert!(!app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))?);
        assert_eq!(app.surface, Surface::ApiKey);
        for ch in "sk-or-v1-test".chars() {
            assert!(!app.handle_key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE))?);
        }
        assert_eq!(app.composer.input(), "sk-or-v1-test");
        assert!(!app.handle_key(KeyEvent::new(KeyCode::Delete, KeyModifiers::META))?);
        assert_eq!(app.composer.input(), "");
        assert_eq!(app.surface, Surface::ApiKey);
        Ok(())
    }

    #[test]
    fn slash_palette_layers_over_running_content() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let mut app = ready_app(&temp)?;
        let session = app.store.create_session(None, std::env::current_dir()?)?;
        app.store.append_event(
            &session.id,
            "session.input",
            serde_json::json!({"text": "tell me about this repo"}),
        )?;
        app.store.append_event(
            &session.id,
            "model.stream_delta",
            serde_json::json!({"text": "Reading the repository layout..."}),
        )?;
        app.selected_session_id = Some(session.id);

        assert!(!app.handle_key(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE))?);
        let screen = render_dump(&mut app)?;
        assert!(screen.contains("/task"));
        assert!(screen.contains("Reading the repository layout"));
        assert!(screen.contains("Type to steer the agent"));
        Ok(())
    }

    #[test]
    fn slash_palette_layers_over_completed_native_transcript() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let mut app = ready_app(&temp)?;
        app.args.height = 28;
        let session = app.store.create_session(None, std::env::current_dir()?)?;
        app.store.append_event(
            &session.id,
            "session.input",
            serde_json::json!({"text": "describe this repo"}),
        )?;
        app.store.append_event(
            &session.id,
            "session.done",
            serde_json::json!({"result": "This is a Rust terminal UI with native scrollback."}),
        )?;
        let events = app.store.events_for_session(&session.id)?;
        let last_seq = events.last().map(|event| event.seq).unwrap_or_default();
        app.selected_session_id = Some(session.id.clone());
        app.native_history.reset_for_session(session.id, last_seq);

        assert!(!app.handle_key(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE))?);
        let screen = render_dump(&mut app)?;
        assert!(!screen.contains("/task"));
        assert!(screen.contains("Ask a follow-up"));
        let overlay = render::command_palette_overlay(&app, Rect::new(0, 0, 72, 11))
            .expect("command palette overlay should render");
        let overlay = buffer_symbols(&overlay.buffer);
        assert!(overlay.contains("/task"));
        assert!(overlay.contains("start a new task"));
        Ok(())
    }

    #[test]
    fn settings_popups_layer_over_running_content() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let mut app = ready_app(&temp)?;
        let session = app.store.create_session(None, std::env::current_dir()?)?;
        app.store.append_event(
            &session.id,
            "session.input",
            serde_json::json!({"text": "tell me about this repo"}),
        )?;
        app.store.append_event(
            &session.id,
            "model.stream_delta",
            serde_json::json!({"text": "Reading the repository layout..."}),
        )?;
        app.selected_session_id = Some(session.id);
        app.open_surface(Surface::Browser);

        let screen = render_dump(&mut app)?;
        assert!(screen.contains("Current browser"));
        Ok(())
    }

    #[test]
    fn history_selection_uses_projected_root_task_rows() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let mut app = ready_app(&temp)?;
        let parent = app.store.create_session(None, std::env::current_dir()?)?;
        app.store.append_event(
            &parent.id,
            "session.input",
            serde_json::json!({"text": "parent task"}),
        )?;
        let child = app.store.create_child_session(
            &parent.id,
            std::env::current_dir()?,
            Some("/root/repo-explorer"),
            Some("repo-explorer"),
            Some("explorer"),
        )?;
        app.store.append_event(
            &child.id,
            "session.input",
            serde_json::json!({"text": "child helper task"}),
        )?;
        app.store.append_event(
            &child.id,
            "session.cancelled",
            serde_json::json!({"reason": "test"}),
        )?;
        app.drain_store_notifications()?;
        app.open_surface(Surface::History);

        let state = app.workbench_state()?.clone();
        assert_eq!(state.history.len(), 1);
        assert_eq!(state.history[0].session_id, parent.id);

        app.resume_selected_history()?;
        assert_eq!(app.selected_session_id.as_deref(), Some(parent.id.as_str()));

        app.selected_session_id = None;
        app.open_surface(Surface::History);
        app.execute_surface_selection()?;
        assert_eq!(app.selected_session_id.as_deref(), Some(parent.id.as_str()));
        Ok(())
    }

    #[test]
    fn provider_auth_surfaces_explain_required_credentials() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let mut app = ready_app(&temp)?;

        app.start_auth_entry(settings::ACCOUNT_OPENROUTER.to_string());
        assert_eq!(app.surface, Surface::ApiKey);
        assert_eq!(
            app.api_key_account.as_deref(),
            Some(settings::ACCOUNT_OPENROUTER)
        );
        let screen = render_dump(&mut app)?;
        assert!(screen.contains("OpenRouter API key"));

        app.start_auth_flow(settings::ACCOUNT_CLAUDE_CODE.to_string())?;
        assert_eq!(app.surface, Surface::SetupResult);
        assert_eq!(
            app.setup_result.as_ref().map(|result| &result.kind),
            Some(&SetupResultKind::Pending)
        );
        let screen = render_dump(&mut app)?;
        assert!(screen.contains("Waiting for Claude Code OAuth sign-in."));
        assert!(screen.contains("OAuth link:"));
        assert!(screen.contains("https://claude.ai/oauth/authorize?"));
        assert!(!screen.contains("Run this in"));
        Ok(())
    }

    #[test]
    fn model_selector_uses_active_catalog_presets() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let app_home = temp.path().join("browser-use-terminal-home");
        write_tui_model_catalog(&app_home)?;

        with_browser_use_terminal_home(&app_home, || -> Result<()> {
            let mut app = ready_app(&temp)?;
            app.store.set_setting("auth.codex.access_token", "token")?;
            app.store.set_setting("auth.codex.account_id", "account")?;
            app.open_surface(Surface::Model);

            let screen = render_dump(&mut app)?;
            assert!(screen.contains("ChatGPT Only Catalog"));
            assert!(screen.contains("Catalog GPT"));
            assert!(!screen.contains("Hidden Catalog Model"));
            assert!(!app.model_choices.iter().any(|choice| {
                choice.account == ACCOUNT_OPENAI && choice.provider_model == "chatgpt-only-catalog"
            }));

            app.save_model(0)?;
            assert_eq!(app.provider_model, "chatgpt-only-catalog");
            assert_eq!(app.model, "ChatGPT Only Catalog");
            assert_eq!(app.account, ACCOUNT_CODEX);
            Ok(())
        })?;
        Ok(())
    }

    #[test]
    fn startup_uses_configured_model_and_provider_without_masking_provider() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let app_home = temp.path().join("browser-use-terminal-home");
        std::fs::create_dir_all(&app_home)?;
        std::fs::write(
            app_home.join("config.toml"),
            r#"
model = "configured-corp-model"
model_provider = "corp"

[model_providers.corp]
name = "Corp"
base_url = "https://corp.example/v1"
env_key = "CORP_API_KEY"
wire_api = "responses"
"#,
        )?;

        with_browser_use_terminal_home(&app_home, || -> Result<()> {
            let app_args = Args {
                agent: AgentBackend::Codex,
                ..args(&temp)
            };
            let app = App::new(app_args)?;
            let selection = app.current_model_selection();

            assert_eq!(app.model, "configured-corp-model");
            assert_eq!(selection.provider_model, "configured-corp-model");
            assert_eq!(selection.backend, AgentBackend::Codex);
            assert_eq!(selection.model_provider_id.as_deref(), Some("corp"));
            Ok(())
        })?;
        Ok(())
    }

    #[test]
    fn startup_and_workspace_context_use_profile_and_config_overrides() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let app_home = temp.path().join("browser-use-terminal-home");
        std::fs::create_dir_all(&app_home)?;
        std::fs::write(
            app_home.join("config.toml"),
            r#"
[model_providers.corp]
name = "Corp"
base_url = "https://corp.example/v1"
env_key = "CORP_API_KEY"
wire_api = "responses"
"#,
        )?;
        std::fs::write(
            app_home.join("work.config.toml"),
            r#"
model = "profile-model"
model_provider = "corp"
"#,
        )?;

        with_browser_use_terminal_home(&app_home, || -> Result<()> {
            let app_args = Args {
                agent: AgentBackend::Codex,
                config_profile: Some("work".to_string()),
                config_overrides: vec![
                    "model=\"override-model\"".to_string(),
                    "developer_instructions=\"TUI session policy\"".to_string(),
                ],
                ..args(&temp)
            };
            let app = App::new(app_args)?;
            let selection = app.current_model_selection();

            assert_eq!(selection.provider_model, "override-model");
            assert_eq!(selection.model_provider_id.as_deref(), Some("corp"));

            let session = app.store.create_session(None, temp.path())?;
            let options = app.configured_agent_options()?;
            browser_use_core::append_workspace_context_event_with_options(
                &app.store, &session, &options,
            )?;
            let events = app.store.events_for_session(&session.id)?;
            let permissions = events
                .iter()
                .find(|event| {
                    event.event_type == "workspace.context"
                        && event
                            .payload
                            .get("kind")
                            .and_then(serde_json::Value::as_str)
                            == Some("permissions")
                })
                .and_then(|event| event.payload.get("content"))
                .and_then(serde_json::Value::as_str)
                .context("permissions workspace context")?;
            assert!(permissions.contains("TUI session policy"));
            Ok(())
        })?;
        Ok(())
    }

    #[test]
    fn startup_config_overrides_beat_stored_model_selection() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let store = Store::open(temp.path())?;
        store.set_setting("model", "Stored Model")?;
        store.set_setting("provider.model", "stored-model")?;
        store.set_setting("provider.id", "codex")?;
        drop(store);

        let app_home = temp.path().join("browser-use-terminal-home");
        std::fs::create_dir_all(&app_home)?;
        std::fs::write(
            app_home.join("config.toml"),
            r#"
[model_providers.corp]
name = "Corp"
base_url = "https://corp.example/v1"
env_key = "CORP_API_KEY"
wire_api = "responses"
"#,
        )?;

        with_browser_use_terminal_home(&app_home, || -> Result<()> {
            let app_args = Args {
                agent: AgentBackend::Codex,
                config_overrides: vec![
                    "model=\"override-model\"".to_string(),
                    "model_provider=\"corp\"".to_string(),
                ],
                ..args(&temp)
            };
            let app = App::new(app_args)?;
            let selection = app.current_model_selection();

            assert_eq!(selection.display_model, "override-model");
            assert_eq!(selection.provider_model, "override-model");
            assert_eq!(selection.model_provider_id.as_deref(), Some("corp"));
            Ok(())
        })?;
        Ok(())
    }

    #[test]
    fn model_selection_is_session_scoped_for_followups() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let app_home = temp.path().join("browser-use-terminal-home");
        write_tui_model_catalog(&app_home)?;

        with_browser_use_terminal_home(&app_home, || -> Result<()> {
            let mut app = ready_app(&temp)?;
            app.store.set_setting("auth.openai.api_key", "openai-key")?;
            let session = app.store.create_session(None, std::env::current_dir()?)?;
            app.store.append_event(
                &session.id,
                "session.input",
                serde_json::json!({"text": "finished task"}),
            )?;
            app.store.append_event(
                &session.id,
                "session.done",
                serde_json::json!({"result": "done"}),
            )?;
            app.selected_session_id = Some(session.id.clone());

            let openai_catalog_index = app
                .model_choices
                .iter()
                .position(|choice| {
                    choice.account == ACCOUNT_OPENAI && choice.provider_model == "catalog-gpt"
                })
                .context("OpenAI catalog model row")?;
            app.save_model(openai_catalog_index)?;

            app.model = "GPT-5.5".to_string();
            app.provider_model = "gpt-5.5".to_string();
            app.account = ACCOUNT_CODEX.to_string();
            app.agent_backend = AgentBackend::Codex;

            let selection = app.session_model_selection_or_current(&session.id)?;
            assert_eq!(selection.provider_model, "catalog-gpt");
            assert_eq!(selection.account, ACCOUNT_OPENAI);
            assert_eq!(selection.backend, AgentBackend::Openai);
            assert_eq!(selection.model_provider_id.as_deref(), Some("openai"));
            Ok(())
        })?;
        Ok(())
    }

    #[test]
    fn credential_action_rows_are_real_menu_choices() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let mut app = ready_app(&temp)?;

        app.start_auth_entry(settings::ACCOUNT_OPENROUTER.to_string());
        assert_eq!(app.surface, Surface::ApiKey);
        assert_eq!(app.selected_row, 0);
        let screen = render_dump(&mut app)?;
        assert!(screen.contains("> Save key"));

        assert!(!app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE))?);
        assert_eq!(app.selected_row, 1);
        let screen = render_dump(&mut app)?;
        assert!(screen.contains("> Cancel"));
        assert!(!screen.contains("> Save key"));

        assert!(!app.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE))?);
        assert_eq!(app.selected_row, 0);
        app.selected_row = 1;
        app.handle_paste("legacy_token");
        assert_eq!(app.selected_row, 0);

        assert!(!app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE))?);
        assert!(!app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))?);
        assert_eq!(app.surface, Surface::Main);
        assert_eq!(app.api_key_account, None);
        assert!(app.composer.is_empty());
        assert_eq!(app.store.get_setting("auth.openrouter.api_key")?, None);

        app.open_surface(Surface::Developer);
        assert!(!app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))?);
        assert_eq!(app.surface, Surface::Telemetry);
        assert!(!app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE))?);
        let screen = render_dump(&mut app)?;
        assert!(screen.contains("> Cancel"));
        assert!(!app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))?);
        assert_eq!(app.surface, Surface::Main);
        assert_eq!(app.store.get_setting(LAMINAR_API_KEY_SETTING)?, None);
        Ok(())
    }

    #[test]
    fn setup_surface_enter_matches_visible_provider_choice() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let mut app = ready_app(&temp)?;
        app.open_surface(Surface::Setup);
        app.selected_row = 1;

        let screen = render_dump(&mut app)?;
        assert!(!screen.contains("Claude Code subscription"));
        assert!(!app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))?);

        assert_eq!(app.surface, Surface::SetupConfirm);
        assert_eq!(
            app.setup_pending_account.as_deref(),
            Some(settings::ACCOUNT_OPENAI)
        );
        let screen = render_dump(&mut app)?;
        assert!(screen.contains("Use OpenAI API key?"));
        assert!(!app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))?);

        assert_eq!(app.surface, Surface::ApiKey);
        assert_eq!(
            app.api_key_account.as_deref(),
            Some(settings::ACCOUNT_OPENAI)
        );

        app.open_surface(Surface::Setup);
        app.selected_row = 3;
        assert!(!app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))?);
        assert_eq!(app.surface, Surface::SetupConfirm);
        assert_eq!(
            app.setup_pending_account.as_deref(),
            Some(settings::ACCOUNT_OPENROUTER)
        );
        assert!(!app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))?);
        assert_eq!(app.surface, Surface::ApiKey);
        assert_eq!(
            app.api_key_account.as_deref(),
            Some(settings::ACCOUNT_OPENROUTER)
        );
        Ok(())
    }

    #[test]
    fn claude_code_oauth_callback_stores_credential_and_confirms() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let mut app = ready_app(&temp)?;

        app.start_auth_flow(settings::ACCOUNT_CLAUDE_CODE.to_string())?;
        assert_eq!(app.surface, Surface::SetupResult);
        assert_eq!(
            app.setup_result.as_ref().map(|result| &result.kind),
            Some(&SetupResultKind::Pending)
        );
        let tx = app
            .claude_code_oauth
            .as_ref()
            .and_then(|flow| flow.event_tx_guard.as_ref())
            .expect("test OAuth sender")
            .clone();
        tx.send(ClaudeCodeOAuthEvent {
            account: settings::ACCOUNT_CLAUDE_CODE.to_string(),
            result: Ok(ClaudeCodeOAuthCredential {
                access_token: "sk-ant-oat-test".to_string(),
                refresh_token: "refresh-test".to_string(),
                expires_ms: 1234,
            }),
        })
        .expect("send test OAuth result");

        assert!(app.drain_oauth_notifications()?);
        assert_eq!(app.surface, Surface::SetupResult);
        assert_eq!(
            app.setup_result.as_ref().map(|result| &result.kind),
            Some(&SetupResultKind::Success)
        );
        assert_eq!(
            app.store.get_setting("auth.claude_code.access_token")?,
            Some("sk-ant-oat-test".to_string())
        );
        assert_eq!(
            app.store.get_setting("auth.claude_code.refresh_token")?,
            Some("refresh-test".to_string())
        );
        assert_eq!(
            app.store.get_setting("auth.claude_code.expires_ms")?,
            Some("1234".to_string())
        );
        Ok(())
    }

    #[test]
    fn codex_device_login_output_stores_auth_and_uses_default_model() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let mut app = App::new(args(&temp))?;

        app.start_codex_device_login(settings::ACCOUNT_CODEX.to_string())?;
        assert_eq!(app.surface, Surface::SetupResult);
        assert_eq!(
            app.setup_result.as_ref().map(|result| &result.kind),
            Some(&SetupResultKind::Pending)
        );
        let tx = app
            .codex_login
            .as_ref()
            .and_then(|flow| flow.event_tx_guard.as_ref())
            .expect("test Codex login sender")
            .clone();
        tx.send(CodexLoginEvent::Output(
            "\u{1b}[94mhttps://auth.openai.com/codex/device\u{1b}[0m\n\u{1b}[94mABCD-EFGH\u{1b}[0m\n"
                .to_string(),
        ))
        .expect("send test Codex output");

        assert!(app.drain_codex_login_notifications()?);
        let screen = render_dump(&mut app)?;
        assert!(screen.contains("https://auth.openai.com/codex/device"));
        assert!(screen.contains("ABCD-EFGH"));
        assert!(!screen.contains("\u{1b}[94m"));

        tx.send(CodexLoginEvent::Finished(Ok(CodexAuth {
            access_token: "codex-access".to_string(),
            account_id: "codex-account".to_string(),
        })))
        .expect("send test Codex auth result");
        assert!(app.drain_codex_login_notifications()?);
        assert_eq!(
            app.store.get_setting("auth.codex.access_token")?,
            Some("codex-access".to_string())
        );
        assert_eq!(
            app.store.get_setting("auth.codex.account_id")?,
            Some("codex-account".to_string())
        );
        let screen = render_dump(&mut app)?;
        assert!(screen.contains("Connected with Codex auth."));
        assert!(screen.contains("A default model will be selected automatically."));

        assert!(!app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))?);
        assert_eq!(app.surface, Surface::Main);
        assert!(app.setup_complete);
        assert_eq!(app.account, settings::ACCOUNT_CODEX);
        assert_eq!(app.model, "GPT-5.5");
        Ok(())
    }

    #[test]
    fn model_selector_hides_claude_code_and_routes_anthropic_to_api_key() -> Result<()> {
        let saved_anthropic = std::env::var("ANTHROPIC_API_KEY").ok();
        let saved_llm_browser = std::env::var("LLM_BROWSER_ANTHROPIC_API_KEY").ok();
        std::env::remove_var("ANTHROPIC_API_KEY");
        std::env::remove_var("LLM_BROWSER_ANTHROPIC_API_KEY");
        let result = (|| -> Result<()> {
            let temp = tempfile::tempdir()?;
            let mut app = ready_app(&temp)?;
            app.open_surface(Surface::Model);
            let opus_index = app
                .model_choices
                .iter()
                .position(|choice| {
                    choice.account == settings::ACCOUNT_ANTHROPIC
                        && choice.provider_model == "claude-opus-4-7"
                })
                .context("Anthropic Opus model row")?;
            app.selected_row = opus_index;

            let screen = render_dump(&mut app)?;
            assert!(!screen.contains("Claude Code sub"));
            assert!(!screen.contains("Claude Code subscription"));
            assert!(!screen.contains("https://claude.ai/oauth/authorize?"));

            assert!(!app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))?);
            assert_eq!(app.surface, Surface::ApiKey);
            assert_eq!(app.pending_model_after_auth, Some(opus_index));
            assert_eq!(
                app.api_key_account.as_deref(),
                Some(settings::ACCOUNT_ANTHROPIC)
            );

            app.handle_paste("sk-ant-test");
            assert!(!app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))?);
            assert_eq!(app.surface, Surface::Main);
            assert_eq!(app.account, settings::ACCOUNT_ANTHROPIC);
            assert_eq!(app.model, "Claude Opus 4.7");
            assert_eq!(app.provider_model, "claude-opus-4-7");
            Ok(())
        })();
        if let Some(value) = saved_anthropic {
            std::env::set_var("ANTHROPIC_API_KEY", value);
        }
        if let Some(value) = saved_llm_browser {
            std::env::set_var("LLM_BROWSER_ANTHROPIC_API_KEY", value);
        }
        result
    }

    #[test]
    fn setup_api_key_flow_keeps_key_entry_in_modal_then_confirms_saved() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let mut app = App::new(args(&temp))?;
        app.selected_row = 1;

        assert!(!app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))?);
        assert_eq!(app.surface, Surface::SetupConfirm);
        let screen = render_dump(&mut app)?;
        assert!(screen.contains("Use OpenAI API key?"));
        assert!(screen.contains("API key modal"));

        assert!(!app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))?);
        assert_eq!(app.surface, Surface::ApiKey);
        assert_eq!(
            app.api_key_account.as_deref(),
            Some(settings::ACCOUNT_OPENAI)
        );
        app.handle_paste("sk-test-key");
        assert!(!app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))?);
        assert_eq!(app.surface, Surface::SetupResult);
        assert!(!app.setup_complete);
        let screen = render_dump(&mut app)?;
        assert!(screen.contains("Saved OpenAI API key."));
        assert!(screen.contains("OpenAI API key"));
        assert_eq!(
            app.store.get_setting("auth.openai.api_key")?.as_deref(),
            Some("sk-test-key")
        );

        assert!(!app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))?);
        assert_eq!(app.surface, Surface::Main);
        assert!(app.setup_complete);
        assert_eq!(app.account, settings::ACCOUNT_OPENAI);
        assert_eq!(app.model, "GPT-5.5");
        Ok(())
    }

    #[test]
    fn up_down_keys_navigate_every_choice_menu() -> Result<()> {
        fn assert_nav(app: &mut App, expected_count: usize) -> Result<()> {
            app.selected_row = 0;
            for _ in 0..expected_count - 1 {
                assert!(!app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE))?);
            }
            assert_eq!(app.selected_row, expected_count - 1);
            // Down past the last row wraps to the first; Up past the first wraps back.
            assert!(!app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE))?);
            assert_eq!(app.selected_row, 0);
            assert!(!app.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE))?);
            assert_eq!(app.selected_row, expected_count - 1);
            Ok(())
        }

        let first_run_temp = tempfile::tempdir()?;
        let mut first_run_app = App::new(args(&first_run_temp))?;
        assert_nav(&mut first_run_app, ACCOUNT_CHOICES.len())?;

        let temp = tempfile::tempdir()?;
        let mut app = ready_app(&temp)?;
        for surface in [
            Surface::Setup,
            Surface::Account,
            Surface::Model,
            Surface::Mode,
            Surface::Browser,
            Surface::BrowserSelect,
        ] {
            app.open_surface(surface);
            let count = match surface {
                Surface::Setup | Surface::Account => ACCOUNT_CHOICES.len(),
                Surface::Model => app.model_choices.len(),
                Surface::Mode => 2,
                Surface::Browser | Surface::BrowserSelect => BROWSER_CHOICES.len(),
                _ => unreachable!(),
            };
            assert_nav(&mut app, count)?;
        }

        app.start_auth_entry(settings::ACCOUNT_OPENROUTER.to_string());
        assert_nav(&mut app, 2)?;
        app.cancel_auth_entry();
        app.start_telemetry_entry();
        assert_nav(&mut app, 2)?;
        app.cancel_secret_entry();

        for idx in 0..3 {
            let session = app.store.create_session(None, std::env::current_dir()?)?;
            app.store.append_event(
                &session.id,
                "session.input",
                serde_json::json!({"text": format!("history task {idx}")}),
            )?;
        }
        app.open_surface(Surface::History);
        assert_nav(&mut app, 3)?;
        app.close_surface();

        assert!(!app.handle_key(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE))?);
        let slash_palette_count = app.slash_palette_items().len();
        assert_nav(&mut app, slash_palette_count)?;
        app.close_slash_palette();

        let failed = app.store.create_session(None, std::env::current_dir()?)?;
        app.store.append_event(
            &failed.id,
            "session.input",
            serde_json::json!({"text": "failed task"}),
        )?;
        app.store.append_event(
            &failed.id,
            "session.failed",
            serde_json::json!({"error": "OpenRouter API key is missing"}),
        )?;
        app.selected_session_id = Some(failed.id);
        assert_nav(&mut app, 4)?;

        let cancelled = app.store.create_session(None, std::env::current_dir()?)?;
        app.store.append_event(
            &cancelled.id,
            "session.input",
            serde_json::json!({"text": "cancelled task"}),
        )?;
        app.store.request_cancel(&cancelled.id, "test cancel")?;
        app.selected_session_id = Some(cancelled.id);
        assert_nav(&mut app, 3)?;
        Ok(())
    }

    #[test]
    fn palette_and_settings_selection_wrap_at_edges() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let mut app = ready_app(&temp)?;

        // The slash palette wraps around both ends.
        assert!(!app.handle_key(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE))?);
        let palette_count = app.slash_palette_items().len();
        for _ in 0..palette_count - 1 {
            assert!(!app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE))?);
        }
        assert_eq!(app.selected_row, palette_count - 1);
        assert!(!app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE))?);
        assert_eq!(app.selected_row, 0);
        assert!(!app.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE))?);
        assert_eq!(app.selected_row, palette_count - 1);
        app.composer.clear();
        app.selected_row = 0;

        // The model picker wraps the same way.
        app.open_surface(Surface::Model);
        let model_choice_count = app.model_choices.len();
        for _ in 0..model_choice_count - 1 {
            assert!(!app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE))?);
        }
        assert_eq!(app.selected_row, model_choice_count - 1);
        let screen = render_dump(&mut app)?;
        assert!(screen.contains("DeepSeek V4 Pro"));
        assert!(!app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE))?);
        assert_eq!(app.selected_row, 0);
        assert!(!app.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE))?);
        assert_eq!(app.selected_row, model_choice_count - 1);
        Ok(())
    }

    #[test]
    fn browser_panel_actions_record_explicit_events() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let mut app = ready_app(&temp)?;
        let session = app.store.create_session(None, std::env::current_dir()?)?;
        app.store.append_event(
            &session.id,
            "session.input",
            serde_json::json!({"text": "inspect"}),
        )?;
        app.store.append_event(
            &session.id,
            "browser.live_url",
            serde_json::json!({"live_url": "https://live.browser-use.com/?wss=example"}),
        )?;
        app.selected_session_id = Some(session.id.clone());
        app.open_surface(Surface::Browser);
        assert!(!app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))?);
        app.selected_row = 1;
        assert!(!app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))?);
        let events = app.store.events_for_session(&session.id)?;
        assert!(events
            .iter()
            .any(|event| event.event_type == "browser.open_requested"));
        assert!(events
            .iter()
            .any(|event| event.event_type == "browser.reconnect_requested"));
        Ok(())
    }

    #[test]
    fn browser_live_url_is_visible_in_browser_panel() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let mut app = ready_app(&temp)?;
        let session = app.store.create_session(None, std::env::current_dir()?)?;
        app.store.append_event(
            &session.id,
            "session.input",
            serde_json::json!({"text": "inspect"}),
        )?;
        app.store.append_event(
            &session.id,
            "browser.live_url",
            serde_json::json!({"live_url": "https://live.browser-use.com/?wss=example"}),
        )?;
        app.selected_session_id = Some(session.id.clone());
        app.open_surface(Surface::Browser);

        let screen = render_dump(&mut app)?;
        assert!(screen.contains("live view"));
        assert!(screen.contains("https://live.browser-use.com/?wss=example"));
        Ok(())
    }

    #[test]
    fn laminar_key_can_be_saved_from_developer_surface() -> Result<()> {
        let saved = std::env::var("LMNR_PROJECT_API_KEY").ok();
        std::env::remove_var("LMNR_PROJECT_API_KEY");
        let result = (|| -> Result<()> {
            let temp = tempfile::tempdir()?;
            let mut app = ready_app(&temp)?;
            app.open_surface(Surface::Developer);
            let screen = render_dump(&mut app)?;
            assert!(screen.contains("not connected"));
            assert!(screen.contains("Configure Laminar"));

            assert!(!app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))?);
            assert_eq!(app.surface, Surface::Telemetry);
            app.handle_paste("lmnr_test_key");
            let screen = render_dump(&mut app)?;
            assert!(screen.contains("Laminar API key"));
            assert!(screen.contains("lmnr_tes"));
            assert!(!screen.contains("lmnr_test_key"));

            assert!(!app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))?);
            assert_eq!(
                app.store.get_setting(LAMINAR_API_KEY_SETTING)?.as_deref(),
                Some("lmnr_test_key")
            );
            assert_eq!(app.surface, Surface::Developer);
            let screen = render_dump(&mut app)?;
            assert!(screen.contains("connected via TUI config"));
            Ok(())
        })();
        if let Some(value) = saved {
            std::env::set_var("LMNR_PROJECT_API_KEY", value);
        }
        result
    }

    #[test]
    fn composer_keeps_codex_like_multiline_behavior() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let mut app = ready_app(&temp)?;
        app.set_input("hello browser world".to_string());
        assert!(!app.handle_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::ALT))?);
        assert_eq!(app.composer.input(), "hello browser ");
        assert_eq!(app.composer.cursor(), app.composer.input_len());
        assert!(!app.handle_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL))?);
        assert_eq!(app.composer.input(), "");

        app.set_input("first line\nprefix suffix".to_string());
        app.set_input_cursor("first line\nprefix ".chars().count());
        assert!(!app.handle_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::SUPER))?);
        assert_eq!(app.composer.input(), "first line");

        app.set_input("a\nb".to_string());
        assert!(!app.handle_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL))?);
        assert_eq!(app.composer.input(), "a\n");
        assert!(!app.handle_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL))?);
        assert_eq!(app.composer.input(), "a");

        app.set_input("a".to_string());
        assert!(!app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT))?);
        assert!(!app.handle_key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::NONE))?);
        assert_eq!(app.composer.input(), "a\nb");

        app.set_input("option".to_string());
        assert!(!app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT))?);
        assert!(!app.handle_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE))?);
        assert_eq!(app.composer.input(), "option\nn");

        app.set_input("meta".to_string());
        assert!(!app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::META))?);
        assert_eq!(app.composer.input(), "meta\n");

        app.set_input("alt-cr".to_string());
        assert!(!app.handle_key(KeyEvent::new(KeyCode::Char('\r'), KeyModifiers::ALT))?);
        assert_eq!(app.composer.input(), "alt-cr\n");

        app.set_input("a\nb".to_string());
        assert_eq!(app.composer_height(), 4);
        let rendered_input = lines_plain_text(&app.composer.render_lines(10, "placeholder"));
        assert!(rendered_input.contains("> a"));
        assert!(rendered_input.contains("  b"));
        assert!(!rendered_input.contains('|'));

        app.handle_paste(" pasted\ntext");
        assert_eq!(app.composer.input(), "a\nb pasted\ntext");
        let rendered_paste = lines_plain_text(&app.composer.render_lines(10, "placeholder"));
        assert!(rendered_paste.contains("  b pasted"));
        assert!(!rendered_paste.contains('|'));

        app.set_input("first\nsecond".to_string());
        app.set_input_cursor(app.composer.input_len());
        assert!(!app.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE))?);
        assert_eq!(app.composer.cursor(), "first".chars().count());
        assert!(!app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE))?);
        assert_eq!(app.composer.cursor(), app.composer.input_len());
        Ok(())
    }

    #[test]
    fn prompt_history_recalls_persistent_and_local_entries() -> Result<()> {
        let codex_home = tempfile::tempdir()?;
        with_browser_use_terminal_home(codex_home.path(), || -> Result<()> {
            let config = browser_use_core::MessageHistoryConfig {
                app_home: codex_home.path().to_path_buf(),
                settings: browser_use_core::MessageHistorySettings::default(),
            };
            browser_use_core::append_message_history_entry(
                "older persisted prompt",
                "session-a",
                &config,
            )?;
            browser_use_core::append_message_history_entry(
                "newer persisted prompt",
                "session-b",
                &config,
            )?;

            let temp = tempfile::tempdir()?;
            let mut app = ready_app(&temp)?;
            assert!(!app.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE))?);
            assert_eq!(app.composer.input(), "newer persisted prompt");
            assert!(!app.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE))?);
            assert_eq!(app.composer.input(), "older persisted prompt");
            assert!(!app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE))?);
            assert_eq!(app.composer.input(), "newer persisted prompt");
            assert!(!app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE))?);
            assert_eq!(app.composer.input(), "");

            app.set_input("draft prompt".to_string());
            assert!(!app.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE))?);
            assert_eq!(app.composer.input(), "draft prompt");

            app.set_input(String::new());
            app.prompt_history.record_submission("local newest prompt");
            browser_use_core::append_message_history_entry(
                "local newest prompt",
                "session-c",
                &config,
            )?;
            assert!(!app.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE))?);
            assert_eq!(app.composer.input(), "local newest prompt");
            assert!(!app.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE))?);
            assert_eq!(app.composer.input(), "newer persisted prompt");

            app.set_input(String::new());
            app.prompt_history.reset_navigation();
            app.prompt_history
                .record_submission("local\nmultiline prompt");
            assert!(!app.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE))?);
            assert_eq!(app.composer.input(), "local\nmultiline prompt");
            app.set_input_cursor("local".chars().count());
            assert!(!app.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE))?);
            assert_eq!(app.composer.input(), "local\nmultiline prompt");

            app.set_input("draft first line\nsecond line".to_string());
            app.prompt_history.reset_navigation();
            app.set_input_cursor("draft first line".chars().count());
            assert!(!app.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE))?);
            assert_eq!(app.composer.input(), "draft first line\nsecond line");
            assert_eq!(app.composer.cursor(), "draft first line".chars().count());
            Ok(())
        })
    }

    #[test]
    fn prompt_history_snapshots_before_local_submission_and_skips_bad_offsets() -> Result<()> {
        let codex_home = tempfile::tempdir()?;
        with_browser_use_terminal_home(codex_home.path(), || -> Result<()> {
            let config = browser_use_core::MessageHistoryConfig {
                app_home: codex_home.path().to_path_buf(),
                settings: browser_use_core::MessageHistorySettings::default(),
            };
            let temp = tempfile::tempdir()?;
            let mut app = ready_app(&temp)?;
            let options = app.configured_agent_options()?;
            app.refresh_prompt_history_for(temp.path(), &options)?;
            app.prompt_history.record_submission("same-turn prompt");
            browser_use_core::append_message_history_entry(
                "same-turn prompt",
                "session-a",
                &config,
            )?;

            assert!(!app.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE))?);
            assert_eq!(app.composer.input(), "same-turn prompt");
            assert!(!app.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE))?);
            assert_eq!(app.composer.input(), "same-turn prompt");

            let history_path = codex_home.path().join("history.jsonl");
            std::fs::remove_file(&history_path)?;
            browser_use_core::append_message_history_entry(
                "valid persisted prompt",
                "session-b",
                &config,
            )?;
            use std::io::Write as _;
            let mut file = std::fs::OpenOptions::new()
                .append(true)
                .open(&history_path)?;
            writeln!(file, "{{not-json")?;

            let temp = tempfile::tempdir()?;
            let mut app = ready_app(&temp)?;
            assert!(!app.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE))?);
            assert_eq!(app.composer.input(), "valid persisted prompt");
            Ok(())
        })
    }

    #[test]
    fn prompt_history_ctrl_r_search_accepts_and_restores_drafts() -> Result<()> {
        let codex_home = tempfile::tempdir()?;
        with_browser_use_terminal_home(codex_home.path(), || -> Result<()> {
            let config = browser_use_core::MessageHistoryConfig {
                app_home: codex_home.path().to_path_buf(),
                settings: browser_use_core::MessageHistorySettings::default(),
            };
            browser_use_core::append_message_history_entry(
                "find old invoice",
                "session-a",
                &config,
            )?;
            browser_use_core::append_message_history_entry("book hotel", "session-b", &config)?;
            browser_use_core::append_message_history_entry(
                "find newer receipt",
                "session-c",
                &config,
            )?;

            let temp = tempfile::tempdir()?;
            let mut app = ready_app(&temp)?;
            assert!(!app.handle_key(KeyEvent::new(KeyCode::Char('\u{0012}'), KeyModifiers::NONE))?);
            for ch in "find".chars() {
                assert!(!app.handle_key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE))?);
            }
            assert_eq!(app.composer.input(), "find newer receipt");
            let screen = render_dump(&mut app)?;
            assert!(screen.contains("find newer receipt"));
            let matches = &app.prompt_history.search.as_ref().unwrap().matches;
            assert_eq!(
                matches,
                &vec![
                    "find newer receipt".to_string(),
                    "find old invoice".to_string()
                ]
            );

            assert!(!app.handle_key(KeyEvent::new(KeyCode::Char('\u{0012}'), KeyModifiers::NONE))?);
            assert_eq!(app.composer.input(), "find old invoice");
            assert!(!app.handle_key(KeyEvent::new(KeyCode::Char('\u{0013}'), KeyModifiers::NONE))?);
            assert_eq!(app.composer.input(), "find newer receipt");
            assert!(!app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))?);
            assert_eq!(app.composer.input(), "find newer receipt");
            assert!(app.prompt_history.search.is_none());

            app.set_input("draft text".to_string());
            assert!(!app.handle_key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL))?);
            for ch in "missing".chars() {
                assert!(!app.handle_key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE))?);
            }
            assert_eq!(app.composer.input(), "draft text");
            assert!(!app.handle_key(KeyEvent::new(KeyCode::Char('\u{0003}'), KeyModifiers::NONE))?);
            assert_eq!(app.composer.input(), "draft text");
            assert!(app.prompt_history.search.is_none());
            Ok(())
        })
    }

    #[test]
    fn wrapped_composer_keeps_first_visual_line_visible_at_wrap_boundary() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let mut app = ready_app(&temp)?;
        app.args.width = 40;
        let app_width = app
            .args
            .width
            .saturating_sub(APP_HORIZONTAL_MARGIN.saturating_mul(2))
            .max(1);
        let input_area_width = app_width.saturating_sub(4).max(1);
        let content_width = input_area_width.saturating_sub(2).max(1);
        let first_visual_line = "x".repeat(content_width as usize);
        app.set_input(format!("{first_visual_line}y"));

        let screen = render_dump(&mut app)?;
        let first_row = row_containing(&screen, &format!("> {first_visual_line}"));
        let second_row = row_containing(&screen, "  y");
        assert_eq!(second_row, first_row + 1, "{screen}");
        Ok(())
    }

    #[test]
    fn long_results_use_terminal_scrollback_not_internal_scroll() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let app_args = Args {
            height: 12,
            width: 80,
            ..args(&temp)
        };
        let mut app = App::new(app_args)?;
        app.setup_complete = true;
        app.model_configured = true;
        app.store.set_setting("setup.complete", "1")?;
        let session = app.store.create_session(None, std::env::current_dir()?)?;
        app.store.append_event(
            &session.id,
            "session.input",
            serde_json::json!({"text": "summarize a long page"}),
        )?;
        let result = (1..=40)
            .map(|idx| format!("- line {idx}"))
            .collect::<Vec<_>>()
            .join("\n");
        app.store.append_event(
            &session.id,
            "session.done",
            serde_json::json!({ "result": result }),
        )?;
        app.selected_session_id = Some(session.id);
        let lines = native_scrollback_lines(&mut app, 80)?;
        let text = format!("{lines:?}");
        assert!(lines.len() > app.args.height as usize);
        assert!(text.contains("line 1"));
        assert!(text.contains("line 40"));
        Ok(())
    }

    #[test]
    fn activity_rendering_does_not_cap_or_compact_steps() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let mut app = ready_app(&temp)?;
        let session = app.store.create_session(None, std::env::current_dir()?)?;
        app.store.append_event(
            &session.id,
            "session.input",
            serde_json::json!({"text": "exercise all activity rows"}),
        )?;
        for idx in 1..=14 {
            app.store.append_event(
                &session.id,
                "browser.state",
                serde_json::json!({"url": format!("https://example.com/page-{idx}")}),
            )?;
        }
        app.store.append_event(
            &session.id,
            "model.delta",
            serde_json::json!({"text": "result token"}),
        )?;
        app.selected_session_id = Some(session.id);
        let lines = native_scrollback_lines(&mut app, 120)?;
        let text = lines_plain_text(&lines);
        assert!(!text.contains("earlier steps"));
        assert!(!text.contains("writing result ("));
        assert!(!text.contains("writing result"));
        assert!(!text.contains("using browser"));
        assert_eq!(text.matches("opened example.com/page-").count(), 14);
        Ok(())
    }

    #[test]
    fn model_waits_do_not_render_as_transcript_activity() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let mut app = ready_app(&temp)?;
        let session = app.store.create_session(None, std::env::current_dir()?)?;
        app.store.append_event(
            &session.id,
            "session.input",
            serde_json::json!({"text": "wait on the model"}),
        )?;
        app.store.append_event(
            &session.id,
            "model.turn.request",
            serde_json::json!({"model": "GPT-5.5", "provider": "codex"}),
        )?;
        app.selected_session_id = Some(session.id);

        let screen = render_dump(&mut app)?;
        assert!(!screen.contains("• thinking"));
        assert!(!screen.contains("• thought"));
        assert!(!screen.contains("waiting for GPT-5.5"));
        Ok(())
    }

    #[test]
    fn provider_thinking_deltas_do_not_replace_streaming_text() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let mut app = ready_app(&temp)?;
        let session = app.store.create_session(None, std::env::current_dir()?)?;
        app.store.append_event(
            &session.id,
            "session.input",
            serde_json::json!({"text": "think visibly"}),
        )?;
        app.store.append_event(
            &session.id,
            "model.turn.request",
            serde_json::json!({"model": "GPT-5.5", "provider": "codex"}),
        )?;
        app.store.append_event(
            &session.id,
            "model.thinking_delta",
            serde_json::json!({"text": "Checking ", "label": "inspecting context"}),
        )?;
        app.store.append_event(
            &session.id,
            "model.thinking_delta",
            serde_json::json!({"text": "Checking the repository structure.", "label": "inspecting context"}),
        )?;
        app.store.append_event(
            &session.id,
            "model.stream_delta",
            serde_json::json!({"text": "This is the answer draft."}),
        )?;
        app.selected_session_id = Some(session.id);

        let screen = render_dump(&mut app)?;
        assert!(!screen.contains("• thinking"));
        assert!(!screen.contains("• thought inspecting context"));
        assert!(!screen.contains("Checking the repository structure."));
        assert!(!screen.contains("Checking \n"));
        assert!(!screen.contains("• answer draft"));
        assert!(screen.contains("This is the answer draft."));
        Ok(())
    }

    #[test]
    fn live_thinking_renders_compact_shimmer_status() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let mut app = ready_app(&temp)?;
        let session = app.store.create_session(None, std::env::current_dir()?)?;
        app.store.append_event(
            &session.id,
            "session.input",
            serde_json::json!({"text": "think through the repo"}),
        )?;
        app.store.append_event(
            &session.id,
            "model.turn.request",
            serde_json::json!({"model": "GPT-5.5", "provider": "codex"}),
        )?;
        let thinking = (1..=12)
            .map(|idx| format!("thinking line {idx}"))
            .collect::<Vec<_>>()
            .join("\n");
        app.store.append_event(
            &session.id,
            "model.thinking_delta",
            serde_json::json!({"text": thinking, "label": "reasoning"}),
        )?;
        app.selected_session_id = Some(session.id);

        app.drain_store_notifications()?;
        let state = app.workbench_state()?;
        let model = transcript::transcript_model(&app, &state).expect("model");
        let lines = transcript::active_viewport_lines(Some(&model), 100, 20);
        let text = lines_plain_text(&lines);

        assert!(text.contains("Thinking..."), "{text}");
        assert!(!text.contains("thinking line 1"), "{text}");
        assert!(!text.contains("thinking line 12"), "{text}");
        assert!(
            lines
                .iter()
                .flat_map(|line| line.spans.iter())
                .any(|span| span.style == theme::accent()),
            "live thinking status should include a moving shimmer highlight"
        );
        Ok(())
    }

    #[test]
    fn streaming_model_text_is_visible_while_task_runs() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let mut app = ready_app(&temp)?;
        let session = app.store.create_session(None, std::env::current_dir()?)?;
        app.store.append_event(
            &session.id,
            "session.input",
            serde_json::json!({"text": "write as it streams"}),
        )?;
        app.store.append_event(
            &session.id,
            "model.turn.request",
            serde_json::json!({"model": "GPT-5.5", "provider": "codex"}),
        )?;
        app.store.append_event(
            &session.id,
            "model.stream_delta",
            serde_json::json!({"text": "Streaming "}),
        )?;
        app.store.append_event(
            &session.id,
            "model.stream_delta",
            serde_json::json!({"text": "Streaming draft answer"}),
        )?;
        app.selected_session_id = Some(session.id);

        let screen = render_dump(&mut app)?;
        assert!(!screen.contains("• answer draft"));
        assert!(screen.contains("Streaming draft answer"));
        assert!(!screen.contains("Streaming \n"));
        Ok(())
    }

    #[test]
    fn active_streaming_viewport_moves_separator_to_native_scrollback() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let mut app = ready_app(&temp)?;
        let session = app.store.create_session(None, std::env::current_dir()?)?;
        app.store.append_event(
            &session.id,
            "session.input",
            serde_json::json!({"text": "write a long answer"}),
        )?;
        app.store.append_event(
            &session.id,
            "model.turn.request",
            serde_json::json!({"model": "GPT-5.5", "provider": "codex"}),
        )?;
        app.store.append_event(
            &session.id,
            "model.stream_delta",
            serde_json::json!({"text": "line 01"}),
        )?;
        app.selected_session_id = Some(session.id.clone());
        app.drain_store_notifications()?;

        let terminal_width = 120_u16;
        let terminal_height = 80_u16;
        let full_height = terminal_height.max(app.live_viewport_height());
        let initial_desired =
            desired_terminal_viewport_height_for(&mut app, terminal_width, terminal_height)?;
        assert!(
            initial_desired < full_height,
            "live stream rows should move to native scrollback instead of expanding the widget to full height"
        );

        let streamed = (1..=18)
            .map(|idx| format!("line {idx:02}"))
            .collect::<Vec<_>>()
            .join("\n");
        app.store.append_event(
            &session.id,
            "model.stream_delta",
            serde_json::json!({"text": streamed}),
        )?;
        app.drain_store_notifications()?;

        let grown_desired =
            desired_terminal_viewport_height_for(&mut app, terminal_width, terminal_height)?;
        assert_eq!(grown_desired.saturating_add(1), initial_desired);
        Ok(())
    }

    #[test]
    fn pre_tool_streaming_text_commits_before_tool_rows() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let mut app = ready_app(&temp)?;
        let session = app.store.create_session(None, std::env::current_dir()?)?;
        app.store.append_event(
            &session.id,
            "session.input",
            serde_json::json!({"text": "inspect the repo"}),
        )?;
        app.store.append_event(
            &session.id,
            "model.turn.request",
            serde_json::json!({"model": "GPT-5.5", "provider": "codex", "turn_idx": 0}),
        )?;
        app.store.append_event(
            &session.id,
            "model.stream_delta",
            serde_json::json!({"text": "Need", "turn_idx": 0}),
        )?;
        app.store.append_event(
            &session.id,
            "model.stream_delta",
            serde_json::json!({"text": " more targeted.", "turn_idx": 0}),
        )?;
        app.store.append_event(
            &session.id,
            "model.turn.response",
            serde_json::json!({"turn_idx": 0, "tool_call_count": 1, "text_delta_chars": 19}),
        )?;
        app.store.append_event(
            &session.id,
            "model.tool_call",
            serde_json::json!({"name": "read_file", "arguments": {"path": "README.md"}}),
        )?;
        app.store.append_event(
            &session.id,
            "file.read",
            serde_json::json!({"path": "/repo/README.md"}),
        )?;
        app.store.append_event(
            &session.id,
            "session.done",
            serde_json::json!({"result": "Final answer from session.done."}),
        )?;
        app.selected_session_id = Some(session.id);

        let screen = render_dump(&mut app)?;
        assert!(!screen.contains("• note"));
        assert!(screen.contains("Need more targeted."));
        assert!(screen.contains("Final answer from session.done."));
        assert!(!screen.contains("• answer draft"));
        Ok(())
    }

    #[test]
    fn failed_session_preserves_visible_streaming_text_before_error() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let mut app = ready_app(&temp)?;
        app.args.height = 40;
        let session = app.store.create_session(None, std::env::current_dir()?)?;
        app.store.append_event(
            &session.id,
            "session.input",
            serde_json::json!({"text": "inspect the repo"}),
        )?;
        app.store.append_event(
            &session.id,
            "model.turn.request",
            serde_json::json!({"model": "GPT-5.5", "provider": "codex"}),
        )?;
        app.store.append_event(
            &session.id,
            "model.stream_delta",
            serde_json::json!({"text": "I found the transcript handoff issue."}),
        )?;
        app.store.append_event(
            &session.id,
            "session.failed",
            serde_json::json!({"error": "provider disconnected"}),
        )?;
        app.selected_session_id = Some(session.id);

        let screen = render_dump(&mut app)?;
        assert!(
            screen.contains("I found the transcript handoff issue."),
            "{screen}"
        );
        assert!(screen.contains("provider disconnected"), "{screen}");
        assert_eq!(
            screen
                .matches("I found the transcript handoff issue.")
                .count(),
            1,
            "{screen}"
        );
        Ok(())
    }

    #[test]
    fn cancelled_session_preserves_visible_streaming_text_before_stopped_row() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let mut app = ready_app(&temp)?;
        app.args.height = 40;
        let session = app.store.create_session(None, std::env::current_dir()?)?;
        app.store.append_event(
            &session.id,
            "session.input",
            serde_json::json!({"text": "inspect the repo"}),
        )?;
        app.store.append_event(
            &session.id,
            "model.turn.request",
            serde_json::json!({"model": "GPT-5.5", "provider": "codex"}),
        )?;
        app.store.append_event(
            &session.id,
            "model.stream_delta",
            serde_json::json!({"text": "I found the transcript handoff issue."}),
        )?;
        app.store
            .append_event(&session.id, "session.cancelled", serde_json::json!({}))?;
        app.selected_session_id = Some(session.id);

        let screen = render_dump(&mut app)?;
        assert!(
            screen.contains("I found the transcript handoff issue."),
            "{screen}"
        );
        assert!(screen.contains("Progress is saved in history."), "{screen}");
        assert_eq!(
            screen
                .matches("I found the transcript handoff issue.")
                .count(),
            1,
            "{screen}"
        );
        Ok(())
    }

    #[test]
    fn tool_call_response_does_not_render_prior_turn_text() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let mut app = ready_app(&temp)?;
        let session = app.store.create_session(None, std::env::current_dir()?)?;
        app.store.append_event(
            &session.id,
            "session.input",
            serde_json::json!({"text": "first question"}),
        )?;
        app.store.append_event(
            &session.id,
            "model.turn.request",
            serde_json::json!({"model": "GPT-5.5", "provider": "codex", "turn_idx": 0}),
        )?;
        app.store.append_event(
            &session.id,
            "model.stream_delta",
            serde_json::json!({"text": "Old answer should not become a note.", "turn_idx": 0}),
        )?;
        app.store.append_event(
            &session.id,
            "model.turn.response",
            serde_json::json!({"turn_idx": 0, "tool_call_count": 0, "text_delta_chars": 36}),
        )?;
        app.store.append_event(
            &session.id,
            "session.followup",
            serde_json::json!({"text": "now use a tool"}),
        )?;
        app.store.append_event(
            &session.id,
            "model.turn.request",
            serde_json::json!({"model": "GPT-5.5", "provider": "codex", "turn_idx": 0}),
        )?;
        app.store.append_event(
            &session.id,
            "model.turn.response",
            serde_json::json!({"turn_idx": 0, "tool_call_count": 1, "text_delta_chars": 0}),
        )?;
        app.store.append_event(
            &session.id,
            "model.tool_call",
            serde_json::json!({"name": "browser", "arguments": {"cmd": "browser status --json"}}),
        )?;
        app.store.append_event(
            &session.id,
            "session.done",
            serde_json::json!({"result": "Done."}),
        )?;
        app.selected_session_id = Some(session.id);

        let screen = render_dump(&mut app)?;
        assert!(screen.contains("Done."));
        assert!(!screen.contains("• note"));
        assert!(
            !screen.contains("Old answer should not become a note."),
            "{screen}"
        );
        Ok(())
    }

    #[test]
    fn image_artifact_rows_show_the_saved_path() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let mut app = ready_app(&temp)?;
        let session = app.store.create_session(None, temp.path())?;
        let image_path = Path::new(&session.artifact_root).join("latest_screenshot.png");
        std::fs::write(&image_path, b"not a real png; path rendering only")?;
        app.store.append_event(
            &session.id,
            "session.input",
            serde_json::json!({"text": "latest screenshot pls"}),
        )?;
        app.store.append_event(
            &session.id,
            "tool.image",
            serde_json::json!({
                "name": "browser_script",
                "image": {
                    "path": image_path,
                    "mime_type": "image/png",
                    "label": "latest_screenshot",
                }
            }),
        )?;
        app.store.append_event(
            &session.id,
            "session.done",
            serde_json::json!({"result": "I captured the screenshot at the path above."}),
        )?;
        app.selected_session_id = Some(session.id);

        let screen = render_dump(&mut app)?;
        assert!(
            screen.contains("browser image: latest_screenshot"),
            "{screen}"
        );
        assert!(!screen.contains("received image artifact"), "{screen}");
        Ok(())
    }

    #[test]
    fn completed_result_file_renders_pointer_not_file_body() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let mut app = ready_app(&temp)?;
        let session = app.store.create_session(None, temp.path())?;
        let result_path = Path::new(&session.artifact_root).join("hn_top10_comments.json");
        std::fs::write(&result_path, r#"{"marker":"real file body"}"#)?;
        app.store.append_event(
            &session.id,
            "session.input",
            serde_json::json!({"text": "save hacker news comments"}),
        )?;
        app.store.append_event(
            &session.id,
            "session.done",
            serde_json::json!({
                "source": "done.result_file",
                "result_file": "hn_top10_comments.json",
                "result": format!("SHOULD_NOT_RENDER {}", "x".repeat(5000)),
            }),
        )?;
        app.selected_session_id = Some(session.id);

        let screen = render_dump(&mut app)?;

        assert!(screen.contains("Saved result file"), "{screen}");
        assert!(screen.contains("comments.json"), "{screen}");
        assert!(!screen.contains("Folder"), "{screen}");
        assert!(
            !screen.contains("Full contents are saved on disk"),
            "{screen}"
        );
        assert!(!screen.contains("file://"), "{screen}");
        assert!(!screen.contains("SHOULD_NOT_RENDER"), "{screen}");
        assert!(!screen.contains("real file body"), "{screen}");
        Ok(())
    }

    #[test]
    fn live_stream_prefix_strips_after_deferred_warning_rows() {
        let lines = vec![
            Line::from("• warning"),
            Line::from("  └ Model `gpt-5.5` is not in the active model catalog."),
            Line::from(
                "Not much. I'm in /Users/reagan/.superset/projects/browser-use-terminal and",
            ),
            Line::from("ready to work on the repo."),
        ];
        let prefix = vec![
            "Not much. I'm in /Users/reagan/.superset/projects/browser-use-terminal and"
                .to_string(),
        ];

        let stripped = plain_text_lines(&strip_live_stream_prefix(lines, &prefix));

        assert_eq!(
            stripped,
            vec![
                "• warning",
                "  └ Model `gpt-5.5` is not in the active model catalog.",
                "ready to work on the repo.",
            ]
        );
    }

    #[test]
    fn live_stream_prefix_preserves_separator_before_remaining_tail() {
        let lines = vec![
            Line::from(""),
            Line::from("I'll inspect the repo structure and key docs/config first, then"),
            Line::from("summarize what it appears to be and how it's organized."),
        ];
        let prefix =
            vec!["I'll inspect the repo structure and key docs/config first, then".to_string()];

        let stripped = plain_text_lines(&strip_live_stream_prefix(lines, &prefix));

        assert_eq!(
            stripped,
            vec![
                "",
                "summarize what it appears to be and how it's organized.",
            ]
        );
    }

    #[test]
    fn completed_final_stream_does_not_duplicate_session_done_answer() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let mut app = ready_app(&temp)?;
        let session = app.store.create_session(None, std::env::current_dir()?)?;
        app.store.append_event(
            &session.id,
            "session.input",
            serde_json::json!({"text": "answer directly"}),
        )?;
        app.store.append_event(
            &session.id,
            "model.turn.request",
            serde_json::json!({"model": "GPT-5.5", "provider": "codex", "turn_idx": 0}),
        )?;
        app.store.append_event(
            &session.id,
            "model.stream_delta",
            serde_json::json!({"text": "Draft answer that should not replay.", "turn_idx": 0}),
        )?;
        app.store.append_event(
            &session.id,
            "model.turn.response",
            serde_json::json!({"turn_idx": 0, "tool_call_count": 0, "text_delta_chars": 36}),
        )?;
        app.store.append_event(
            &session.id,
            "session.done",
            serde_json::json!({"result": "Canonical final answer."}),
        )?;
        app.selected_session_id = Some(session.id);

        let screen = render_dump(&mut app)?;
        assert!(screen.contains("Canonical final answer."));
        assert!(!screen.contains("Draft answer that should not replay."));
        Ok(())
    }

    #[test]
    fn completed_session_done_payload_dedupes_repeated_text() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let mut app = ready_app(&temp)?;
        let session = app.store.create_session(None, std::env::current_dir()?)?;
        let answer = "\"\"Please open Chrome with remote debugging enabled, then I can go to Gusto. If you want, run the suggested setup flow: browser local setup.";
        app.store.append_event(
            &session.id,
            "session.input",
            serde_json::json!({"text": "go to gusto"}),
        )?;
        app.store.append_event(
            &session.id,
            "session.done",
            serde_json::json!({"result": format!("{answer}{answer}")}),
        )?;
        app.selected_session_id = Some(session.id);

        let screen = render_dump(&mut app)?;

        assert_eq!(screen.matches("Please open Chrome").count(), 1, "{screen}");
        assert!(!screen.contains("setup.\"\"Please open Chrome"));
        Ok(())
    }

    #[test]
    fn native_scrollback_filters_transient_model_events() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let mut app = ready_app(&temp)?;
        let session = app.store.create_session(None, std::env::current_dir()?)?;
        app.store.append_event(
            &session.id,
            "session.input",
            serde_json::json!({"text": "write as it streams"}),
        )?;
        app.store.append_event(
            &session.id,
            "model.turn.request",
            serde_json::json!({"model": "GPT-5.5", "provider": "codex"}),
        )?;
        app.store.append_event(
            &session.id,
            "model.tool_call",
            serde_json::json!({"name": "spawn_agent", "arguments": {"nickname": "repo-explorer"}}),
        )?;
        app.store.append_event(
            &session.id,
            "agent.spawned",
            serde_json::json!({"nickname": "repo-explorer"}),
        )?;
        app.store.append_event(
            &session.id,
            "model.stream_delta",
            serde_json::json!({"text": "Live draft chunk"}),
        )?;
        app.selected_session_id = Some(session.id.clone());
        app.drain_store_notifications()?;
        let state = app.workbench_state()?;
        let model = transcript::transcript_model(&app, &state).expect("model");
        let lines = transcript::all_scrollback_lines(&model, 100);
        let text = lines_plain_text(&lines);

        assert!(text.contains("write as it streams"));
        assert!(text.contains("repo-explorer started"));
        assert!(!text.contains("waiting for GPT-5.5"));
        assert!(!text.contains("start repo-explorer helper"));
        assert!(!text.contains("• answer draft"));
        assert!(!text.contains("Live draft chunk"));
        Ok(())
    }

    #[test]
    fn wrapped_native_links_use_the_full_url_for_each_visible_fragment() {
        let lines = vec![
            Line::from(ratatui::text::Span::styled(
                "https://en.wikiped",
                theme::link(),
            )),
            Line::from(ratatui::text::Span::styled(
                "ia.org/wiki/Apple_Inc.",
                theme::link(),
            )),
        ];

        let hyperlinks = collect_native_hyperlink_segments(&lines);
        assert_eq!(hyperlinks.len(), 2);
        assert_eq!(
            hyperlinks[0].target,
            "https://en.wikipedia.org/wiki/Apple_Inc."
        );
        assert_eq!(hyperlinks[1].target, hyperlinks[0].target);
        assert_eq!(hyperlinks[0].line, 0);
        assert_eq!(hyperlinks[1].line, 1);
    }

    #[test]
    fn file_native_links_use_the_full_url_for_each_visible_fragment() {
        let lines = vec![
            Line::from(ratatui::text::Span::styled(
                "file:///tmp/browser-use-terminal/.browser-use-terminal/",
                theme::link(),
            )),
            Line::from(ratatui::text::Span::styled(
                "artifacts/session/result.json",
                theme::link(),
            )),
        ];

        let hyperlinks = collect_native_hyperlink_segments(&lines);
        assert_eq!(hyperlinks.len(), 2);
        assert_eq!(
            hyperlinks[0].target,
            "file:///tmp/browser-use-terminal/.browser-use-terminal/artifacts/session/result.json"
        );
        assert_eq!(hyperlinks[1].target, hyperlinks[0].target);
    }

    #[test]
    fn absolute_file_path_native_links_encode_to_file_urls() {
        let lines = vec![Line::from(ratatui::text::Span::styled(
            "/tmp/browser use/result #1.json",
            theme::link(),
        ))];

        let hyperlinks = collect_native_hyperlink_segments(&lines);
        assert_eq!(hyperlinks.len(), 1);
        assert_eq!(hyperlinks[0].target, "/tmp/browser use/result #1.json");

        let mut buffer = Buffer::empty(Rect::new(0, 0, 40, 1));
        let area = buffer.area;
        Paragraph::new(lines).render(area, &mut buffer);
        apply_native_hyperlinks(&mut buffer, area, &hyperlinks);

        assert!(buffer[(0, 0)]
            .symbol()
            .starts_with("\x1b]8;;file:///tmp/browser%20use/result%20%231.json\x1b\\/"));
    }

    #[test]
    fn native_link_escape_annotation_keeps_visible_symbols_clickable() {
        let lines = vec![
            Line::from(ratatui::text::Span::styled(
                "https://example",
                theme::link(),
            )),
            Line::from(ratatui::text::Span::styled(".com/docs", theme::link())),
        ];
        let hyperlinks = collect_native_hyperlink_segments(&lines);
        let mut buffer = Buffer::empty(Rect::new(0, 0, 40, 2));
        let area = buffer.area;
        Paragraph::new(lines).render(area, &mut buffer);
        apply_native_hyperlinks(&mut buffer, area, &hyperlinks);

        assert!(buffer[(0, 0)]
            .symbol()
            .starts_with("\x1b]8;;https://example.com/docs\x1b\\h"));
        assert!(buffer[(14, 0)].symbol().ends_with("\x1b]8;;\x1b\\"));
        assert!(buffer[(0, 1)]
            .symbol()
            .starts_with("\x1b]8;;https://example.com/docs\x1b\\."));
        assert!(buffer[(8, 1)].symbol().ends_with("\x1b]8;;\x1b\\"));
    }

    #[test]
    fn child_agent_progress_commits_only_lifecycle_rows() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let mut app = ready_app(&temp)?;
        let parent = app.store.create_session(None, std::env::current_dir()?)?;
        app.store.append_event(
            &parent.id,
            "session.input",
            serde_json::json!({"text": "explain this repo"}),
        )?;
        app.store.append_event(
            &parent.id,
            "model.tool_call",
            serde_json::json!({"name": "spawn_agent"}),
        )?;
        app.store.append_event(
            &parent.id,
            "tool.started",
            serde_json::json!({"name": "spawn_agent"}),
        )?;
        let child = app.store.create_child_session(
            &parent.id,
            std::env::current_dir()?,
            Some("/root/repo-explorer"),
            Some("repo-explorer"),
            Some("explorer"),
        )?;
        app.store.append_event(
            &child.id,
            "agent.context",
            serde_json::json!({"nickname": "repo-explorer", "role": "explorer"}),
        )?;
        app.store.append_event(
            &parent.id,
            "agent.spawned",
            serde_json::json!({"child_session_id": child.id, "nickname": "repo-explorer", "role": "explorer"}),
        )?;
        app.store.append_event(
            &parent.id,
            "model.thinking_delta",
            serde_json::json!({"text": "parent is waiting"}),
        )?;
        app.store.append_event(
            &child.id,
            "file.read",
            serde_json::json!({"path": "/repo/README.md"}),
        )?;
        app.store.append_event(
            &child.id,
            "model.stream_delta",
            serde_json::json!({"text": "Mapping the main crates."}),
        )?;
        app.selected_session_id = Some(parent.id);

        let screen = render_dump(&mut app)?;
        assert!(screen.contains("• subagent repo-explorer started"));
        assert!(!screen.contains("subagents  repo-explorer starting"));
        assert!(!screen.contains("read /repo/README.md"));
        assert!(!screen.contains("writing Mapping the main crates."));
        assert!(!screen.contains("Mapping the main crates."));
        assert!(!screen.contains("spawn_agent requested"));
        assert!(!screen.contains("spawn_agent started"));
        assert!(screen.contains("Working..."));
        assert!(screen.contains("(1 subagent running)"));
        assert!(!screen.contains("parent is waiting"));
        Ok(())
    }

    #[test]
    fn active_child_keeps_child_progress_out_but_keeps_parent_live_view() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let mut app = ready_app(&temp)?;
        let parent = app.store.create_session(None, std::env::current_dir()?)?;
        app.store.append_event(
            &parent.id,
            "session.input",
            serde_json::json!({"text": "explain this repo"}),
        )?;
        app.store.append_event(
            &parent.id,
            "model.tool_call",
            serde_json::json!({"name": "spawn_agent"}),
        )?;
        let child = app.store.create_child_session(
            &parent.id,
            std::env::current_dir()?,
            Some("/root/repo-explorer"),
            Some("repo-explorer"),
            Some("explorer"),
        )?;
        app.store.append_event(
            &child.id,
            "agent.context",
            serde_json::json!({"nickname": "repo-explorer", "role": "explorer"}),
        )?;
        app.store.append_event(
            &parent.id,
            "agent.spawned",
            serde_json::json!({"child_session_id": child.id, "nickname": "repo-explorer", "role": "explorer"}),
        )?;
        app.store.append_event(
            &parent.id,
            "model.thinking_delta",
            serde_json::json!({"text": "parent is waiting"}),
        )?;
        app.store.append_event(
            &child.id,
            "file.read",
            serde_json::json!({"path": "/repo/README.md"}),
        )?;
        app.store.append_event(
            &child.id,
            "model.stream_delta",
            serde_json::json!({"text": "Mapping the main crates."}),
        )?;
        app.selected_session_id = Some(parent.id);
        app.drain_store_notifications()?;
        let state = app.workbench_state()?;
        let model = transcript::transcript_model(&app, &state).expect("model");
        let lines = transcript::active_viewport_lines(Some(&model), 100, 20);
        let text = lines_plain_text(&lines);

        assert!(text.contains("Working..."), "{text}");
        assert!(text.contains("(1 subagent running)"), "{text}");
        assert!(!text.contains("parent is waiting"), "{text}");
        assert!(
            lines
                .iter()
                .flat_map(|line| line.spans.iter())
                .any(|span| span.style == theme::accent()),
            "parent live status should include a moving shimmer highlight"
        );
        assert!(!text.contains("writing Mapping the main crates."));
        assert!(!text.contains("Mapping the main crates."));
        assert!(!text.contains("spawn_agent requested"));
        Ok(())
    }

    #[test]
    fn active_child_progress_stays_out_of_parent_viewport() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let mut app = ready_app(&temp)?;
        let parent = app.store.create_session(None, std::env::current_dir()?)?;
        app.store.append_event(
            &parent.id,
            "session.input",
            serde_json::json!({"text": "tell me about this repo"}),
        )?;
        let child = app.store.create_child_session(
            &parent.id,
            std::env::current_dir()?,
            Some("/root/repo-explorer"),
            Some("repo-explorer"),
            Some("explorer"),
        )?;
        app.store.append_event(
            &parent.id,
            "agent.spawned",
            serde_json::json!({"child_session_id": child.id, "nickname": "repo-explorer", "role": "explorer"}),
        )?;
        for idx in 1..=12 {
            app.store.append_event(
                &child.id,
                "file.read",
                serde_json::json!({"path": format!("/repo/file-{idx}.rs")}),
            )?;
        }
        app.store.append_event(
            &child.id,
            "model.turn.request",
            serde_json::json!({"model": "gpt-5.5"}),
        )?;
        app.selected_session_id = Some(parent.id);

        let screen = render_dump(&mut app)?;
        assert!(screen.contains("• subagent repo-explorer started"));
        assert!(!screen.contains("subagents  repo-explorer starting"));
        assert!(!screen.contains("read /repo/file-1.rs"));
        assert!(!screen.contains("read /repo/file-12.rs"));
        assert!(!screen.contains("waiting for gpt-5.5"));
        Ok(())
    }

    #[test]
    fn transcript_hides_lifecycle_events_and_groups_semantic_activity() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let mut app = ready_app(&temp)?;
        let cwd = std::env::current_dir()?;
        let session = app.store.create_session(None, &cwd)?;
        app.store.append_event(
            &session.id,
            "session.input",
            serde_json::json!({"text": "inspect repository"}),
        )?;
        app.store.append_event(
            &session.id,
            "model.config",
            serde_json::json!({"provider": "codex", "model": "GPT-5.5"}),
        )?;
        app.store.append_event(
            &session.id,
            "model.turn.request",
            serde_json::json!({"model": "GPT-5.5", "provider": "codex"}),
        )?;
        app.store.append_event(
            &session.id,
            "model.tool_call",
            serde_json::json!({"name": "read_file", "arguments": {"path": "README.md"}}),
        )?;
        app.store.append_event(
            &session.id,
            "tool.started",
            serde_json::json!({"name": "read_file", "tool_call_id": "read_1"}),
        )?;
        app.store.append_event(
            &session.id,
            "file.read",
            serde_json::json!({"path": cwd.join("README.md").display().to_string()}),
        )?;
        app.store.append_event(
            &session.id,
            "tool.output",
            serde_json::json!({"name": "read_file", "text": "README raw body should stay out"}),
        )?;
        app.store.append_event(
            &session.id,
            "tool.finished",
            serde_json::json!({"name": "read_file", "tool_call_id": "read_1"}),
        )?;
        app.store.append_event(
            &session.id,
            "tool.batch_started",
            serde_json::json!({"mode": "parallel", "tools": ["read_file", "list_files"]}),
        )?;
        app.store.append_event(
            &session.id,
            "file.read",
            serde_json::json!({"path": cwd.join("Cargo.toml").display().to_string()}),
        )?;
        app.store.append_event(
            &session.id,
            "file.list",
            serde_json::json!({"path": cwd.display().to_string(), "count": 12}),
        )?;
        app.store.append_event(
            &session.id,
            "file.search",
            serde_json::json!({"query": "renderer", "matches": 7}),
        )?;
        app.store.append_event(
            &session.id,
            "tool.batch_finished",
            serde_json::json!({"mode": "parallel", "count": 2}),
        )?;
        app.store.append_event(
            &session.id,
            "session.compaction_started",
            serde_json::json!({"reason": "token_budget"}),
        )?;
        app.store.append_event(
            &session.id,
            "session.compacted",
            serde_json::json!({"reason": "token_budget"}),
        )?;
        app.store.append_event(
            &session.id,
            "telemetry.failed",
            serde_json::json!({"error": "trace exporter unavailable"}),
        )?;
        app.store.append_event(
            &session.id,
            "patch.started",
            serde_json::json!({"tool_call_id": "patch_1"}),
        )?;
        app.store.append_event(
            &session.id,
            "patch.file_changed",
            serde_json::json!({"kind": "changed", "path": cwd.join("README.md").display().to_string()}),
        )?;
        app.store.append_event(
            &session.id,
            "patch.finished",
            serde_json::json!({"changed_files": 1}),
        )?;
        app.store.append_event(
            &session.id,
            "session.done",
            serde_json::json!({"result": "Repository inspected."}),
        )?;
        app.selected_session_id = Some(session.id);
        app.drain_store_notifications()?;
        let state = app.workbench_state()?;
        let model = transcript::transcript_model(&app, &state).expect("model");
        let text = lines_plain_text(&transcript::all_scrollback_lines(&model, 120));
        let terminal_text =
            lines_plain_text(&transcript::all_terminal_scrollback_lines(&model, 120));

        assert!(text.contains("• explored"));
        assert_eq!(text.matches("• explored").count(), 1, "{text}");
        assert!(text.contains("read README.md, Cargo.toml"));
        assert!(text.contains("list "));
        assert!(text.contains("search \"renderer\" (7 matches)"));
        assert!(text.contains("• edit"));
        assert!(text.contains("changed README.md"));
        assert!(text.contains("Repository inspected."));
        assert!(!text.contains("read_file requested"));
        assert!(!text.contains("read_file started"));
        assert!(!text.contains("read_file finished"));
        assert!(!text.contains("batch_started"));
        assert!(!text.contains("README raw body should stay out"));
        assert!(!text.contains("trace exporter unavailable"));
        assert!(!text.contains("token_budget"));
        assert!(!text.contains("waiting for GPT-5.5"));
        assert!(!terminal_text.contains("waiting for GPT-5.5"));
        assert!(terminal_text.contains("read README.md"));
        Ok(())
    }

    #[test]
    fn parent_live_view_hides_subagent_wait_target() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let mut app = ready_app(&temp)?;
        let parent = app.store.create_session(None, std::env::current_dir()?)?;
        app.store.append_event(
            &parent.id,
            "session.input",
            serde_json::json!({"text": "explain this repo"}),
        )?;
        let child = app.store.create_child_session(
            &parent.id,
            std::env::current_dir()?,
            Some("/root/repo_explorer"),
            Some("repo_explorer"),
            Some("explorer"),
        )?;
        app.store.append_event(
            &parent.id,
            "agent.spawned",
            serde_json::json!({"child_session_id": child.id, "nickname": "repo_explorer", "role": "explorer"}),
        )?;
        app.store.append_event(
            &parent.id,
            "model.tool_call",
            serde_json::json!({
                "id": "wait_repo_explorer",
                "name": "wait_agent",
                "arguments": {"target": "repo_explorer", "timeout_ms": 300000},
            }),
        )?;
        app.store.append_event(
            &parent.id,
            "agent.wait.started",
            serde_json::json!({
                "tool_call_id": "wait_repo_explorer",
                "target": "repo_explorer",
                "targets": [{"child_session_id": child.id, "task_name": "/root/repo_explorer", "nickname": "repo_explorer"}],
                "timeout_ms": 300000,
            }),
        )?;
        app.selected_session_id = Some(parent.id);

        let screen = render_dump(&mut app)?;
        assert!(screen.contains("• subagent repo_explorer started"));
        assert!(!screen.contains("waiting on repo_explorer"));
        Ok(())
    }

    #[test]
    fn native_parent_scrollback_does_not_replay_child_session_turns() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let mut app = ready_app(&temp)?;
        let parent = app.store.create_session(None, std::env::current_dir()?)?;
        app.store.append_event(
            &parent.id,
            "session.input",
            serde_json::json!({"text": "explain this repo"}),
        )?;
        let child = app.store.create_child_session(
            &parent.id,
            std::env::current_dir()?,
            Some("/root/repo-explorer"),
            Some("repo-explorer"),
            Some("explorer"),
        )?;
        app.store.append_event(
            &parent.id,
            "agent.spawned",
            serde_json::json!({"child_session_id": child.id, "nickname": "repo-explorer"}),
        )?;
        app.store.append_event(
            &child.id,
            "session.input",
            serde_json::json!({"text": "read every repo file"}),
        )?;
        app.store.append_event(
            &child.id,
            "file.read",
            serde_json::json!({"path": "/repo/README.md"}),
        )?;
        app.store.append_event(
            &child.id,
            "session.done",
            serde_json::json!({"result": "CHILD FULL DETAILS SHOULD NOT BE TOP LEVEL"}),
        )?;
        app.store.append_event(
            &parent.id,
            "agent.completed",
            serde_json::json!({
                "child_session_id": child.id,
                "payload": {"result": "Short helper summary"}
            }),
        )?;
        app.selected_session_id = Some(parent.id.clone());
        app.drain_store_notifications()?;
        let state = app.workbench_state()?;
        let model = transcript::transcript_model(&app, &state).expect("model");
        let lines = transcript::all_scrollback_lines(&model, 100);
        let text = lines_plain_text(&lines);

        assert!(text.contains("explain this repo"));
        assert!(text.contains("subagent repo-explorer started"));
        assert!(text.contains("subagent repo-explorer finished"));
        assert!(!text.contains("read /repo/README.md"));
        assert!(!text.contains("Short helper summary"));
        assert!(!text.contains("read every repo file"));
        assert!(!text.contains("CHILD FULL DETAILS SHOULD NOT BE TOP LEVEL"));
        Ok(())
    }

    #[test]
    fn long_browser_urls_do_not_overrun_the_timeline_column() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let mut app = ready_app(&temp)?;
        let session = app.store.create_session(None, std::env::current_dir()?)?;
        app.store.append_event(
            &session.id,
            "session.input",
            serde_json::json!({"text": "what do you see"}),
        )?;
        app.store.append_event(
            &session.id,
            "browser.state",
            serde_json::json!({
                "title": "Example Account Sign-In",
                "tabs": 2,
                "url": "https://accounts.example.com/signin?redirect_uri=https%3A%2F%2Fconsole.example.com%2Fworkspace%2Fmanagement%2Fsettings%2Fnotifications%2Fcustom-notification-submitted%3F%26isauthcode%3Dtrue&client_id=example-client-id-with-a-long-value&forceMobileApp=0",
            }),
        )?;
        app.selected_session_id = Some(session.id);

        let screen = render_dump(&mut app)?;
        assert!(screen.contains("accounts.example.com/signin?..."));
        assert!(!screen.contains("redirect_uri=https"));
        Ok(())
    }

    #[test]
    fn native_scrollback_live_view_does_not_replay_completed_transcript() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let mut app = ready_app(&temp)?;
        app.args.height = 44;
        let session = app.store.create_session(None, std::env::current_dir()?)?;
        app.store.append_event(
            &session.id,
            "session.input",
            serde_json::json!({"text": "describe this repo"}),
        )?;
        app.store.append_event(
            &session.id,
            "session.done",
            serde_json::json!({"result": "It is a Rust browser-agent workbench."}),
        )?;
        app.store.append_event(
            &session.id,
            "session.followup",
            serde_json::json!({"text": "go say hi to aitor"}),
        )?;
        app.store.append_event(
            &session.id,
            "session.done",
            serde_json::json!({"result": "Hi Aitor - this is the short summary."}),
        )?;
        app.store.append_event(
            &session.id,
            "model.usage",
            serde_json::json!({"input_tokens": 18234, "cost_usd": 0.0412}),
        )?;
        let events = app.store.events_for_session(&session.id)?;
        let last_seq = events.last().map(|event| event.seq).unwrap_or_default();
        app.selected_session_id = Some(session.id.clone());
        app.native_history.reset_for_session(session.id, last_seq);

        let screen = render_dump(&mut app)?;
        let composer_row = row_containing(&screen, "Ask a follow-up");
        let status_row = row_containing(&screen, "/60k");
        assert!(status_row >= composer_row + 2);
        assert!(!screen.contains("describe this repo"));
        assert!(!screen.contains("go say hi to aitor"));
        assert!(!screen.contains("It is a Rust browser-agent workbench."));
        assert!(!screen.contains("Hi Aitor"));
        Ok(())
    }

    #[test]
    fn native_scrollback_running_live_view_stays_attached_to_committed_output() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let mut app = ready_app(&temp)?;
        app.args.height = 28;
        let session = app.store.create_session(None, std::env::current_dir()?)?;
        app.store.append_event(
            &session.id,
            "session.input",
            serde_json::json!({"text": "Find the top 5 Hacker News posts"}),
        )?;
        app.store.append_event(
            &session.id,
            "browser.page",
            serde_json::json!({
                "url": "https://news.ycombinator.com",
                "title": "Hacker News",
            }),
        )?;
        let committed_seq = app
            .store
            .events_for_session(&session.id)?
            .last()
            .map(|event| event.seq)
            .unwrap_or_default();
        app.store.append_event(
            &session.id,
            "model.stream_delta",
            serde_json::json!({"text": "Reading the page and preparing the next browser action..."}),
        )?;
        app.selected_session_id = Some(session.id.clone());
        app.native_history
            .reset_for_session(session.id, committed_seq);

        let screen = render_dump(&mut app)?;
        let live_row = row_containing(&screen, "Reading the page and preparing");
        let composer_row = row_containing(&screen, "Type to steer the agent");
        assert!(
            live_row <= 2,
            "live reasoning should render directly under native scrollback, not after a large gap\n{screen}"
        );
        assert!(
            composer_row > live_row,
            "composer should stay below live reasoning\n{screen}"
        );
        assert!(
            composer_row.saturating_sub(live_row) <= 8,
            "live reasoning and composer should not be separated by a large blank gap\n{screen}"
        );
        Ok(())
    }

    #[test]
    fn slash_palette_does_not_resize_completed_history_viewport() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let mut app = ready_app(&temp)?;
        let session = app.store.create_session(None, std::env::current_dir()?)?;
        app.store.append_event(
            &session.id,
            "session.input",
            serde_json::json!({"text": "describe this repo"}),
        )?;
        app.store.append_event(
            &session.id,
            "browser.state",
            serde_json::json!({"url": "https://example.com", "title": "Example"}),
        )?;
        app.store.append_event(
            &session.id,
            "session.done",
            serde_json::json!({"result": "It is a Rust browser-agent workbench."}),
        )?;
        let events = app.store.events_for_session(&session.id)?;
        let last_seq = events.last().map(|event| event.seq).unwrap_or_default();
        app.selected_session_id = Some(session.id.clone());
        app.native_history.reset_for_session(session.id, last_seq);

        let before = desired_terminal_viewport_height(&mut app)?;
        assert!(!app.handle_key(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE))?);
        assert!(app.is_slash_palette_active());
        let after = desired_terminal_viewport_height(&mut app)?;
        assert_eq!(after, before);
        Ok(())
    }

    #[test]
    fn followup_over_native_scrollback_keeps_full_transcript_viewport() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let mut app = ready_app(&temp)?;
        let session = app.store.create_session(None, std::env::current_dir()?)?;
        app.store.append_event(
            &session.id,
            "session.input",
            serde_json::json!({"text": "describe this repo"}),
        )?;
        app.store.append_event(
            &session.id,
            "session.done",
            serde_json::json!({"result": "It is a Rust browser-agent workbench."}),
        )?;
        let events = app.store.events_for_session(&session.id)?;
        let last_seq = events.last().map(|event| event.seq).unwrap_or_default();
        app.selected_session_id = Some(session.id.clone());
        app.native_history
            .reset_for_session(session.id.clone(), last_seq);

        let docked = desired_terminal_viewport_height_for(&mut app, 120, 28)?;
        app.dispatch(AppCommand::SendFollowup {
            session_id: session.id.clone(),
            text: "yo".to_string(),
        })?;
        app.drain_store_notifications()?;
        assert_eq!(
            app.store
                .load_session(&session.id)?
                .map(|session| session.status),
            Some(SessionStatus::Running)
        );
        let prompt_only = desired_terminal_viewport_height_for(&mut app, 120, 28)?;
        assert_eq!(prompt_only, docked.saturating_add(1));
        let native_prompt = lines_plain_text(&native_scrollback_lines(&mut app, 120)?);
        assert!(native_prompt.contains("> yo"));
        let prompt_only_screen = render_dump(&mut app)?;
        assert!(prompt_only_screen.contains("sending"));
        assert!(!prompt_only_screen.contains("> yo  - sending"));
        assert!(prompt_only_screen.contains("Type to steer the agent"));

        let state = app.workbench_state()?;
        let model = transcript::transcript_model(&app, &state).expect("model");
        assert!(transcript::active_viewport_has_live_content(Some(&model)));
        assert_eq!(prompt_only, docked.saturating_add(1));

        app.store.append_event(
            &session.id,
            "model.turn.request",
            serde_json::json!({"model": "GPT-5.5", "provider": "codex"}),
        )?;
        app.drain_store_notifications()?;
        let waiting = desired_terminal_viewport_height_for(&mut app, 120, 28)?;
        assert_eq!(waiting, prompt_only);
        let waiting_screen = render_dump(&mut app)?;
        assert!(waiting_screen.contains("thinking"));
        assert!(!waiting_screen.contains("> yo  - thinking"));

        app.store.append_event(
            &session.id,
            "model.stream_delta",
            serde_json::json!({"text": "streaming now"}),
        )?;
        app.drain_store_notifications()?;
        let streaming = desired_terminal_viewport_height_for(&mut app, 120, 28)?;
        assert_eq!(streaming, prompt_only);
        let streaming_screen = render_dump(&mut app)?;
        assert!(streaming_screen.contains("streaming now"));
        assert!(!streaming_screen.contains("Thinking..."));
        assert!(!streaming_screen.contains("Working..."));
        let state = app.workbench_state()?;
        let model = transcript::transcript_model(&app, &state).expect("model");
        let emission = transcript::terminal_scrollback_emission_since(&model, last_seq, 120, true);
        assert!(lines_plain_text(&emission.lines).contains("> yo"));
        Ok(())
    }

    #[test]
    fn quiet_streaming_commentary_shows_thinking_status_after_debounce() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let mut app = ready_app(&temp)?;
        let session = app.store.create_session(None, std::env::current_dir()?)?;
        app.store.append_event(
            &session.id,
            "session.input",
            serde_json::json!({"text": "inspect the repo"}),
        )?;
        app.store.append_event(
            &session.id,
            "model.turn.request",
            serde_json::json!({"model": "GPT-5.5", "provider": "codex"}),
        )?;
        app.store.append_event_with_identity(
            &session.id,
            "quiet-streaming-commentary".to_string(),
            browser_use_store::now_ms().saturating_sub(1_000),
            "model.stream_delta",
            serde_json::json!({"text": "I checked the top-level files and docs."}),
        )?;
        app.selected_session_id = Some(session.id);
        app.drain_store_notifications()?;

        let screen = render_dump(&mut app)?;
        assert!(screen.contains("I checked the top-level files and docs."));
        assert!(screen.contains("Thinking..."));
        assert!(!screen.contains("Working..."));
        Ok(())
    }

    #[test]
    fn commentary_completion_restores_thinking_status_without_debounce() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let mut app = ready_app(&temp)?;
        let session = app.store.create_session(None, std::env::current_dir()?)?;
        app.store.append_event(
            &session.id,
            "session.input",
            serde_json::json!({"text": "inspect the repo"}),
        )?;
        app.store.append_event(
            &session.id,
            "model.turn.request",
            serde_json::json!({"model": "GPT-5.5", "provider": "codex"}),
        )?;
        app.store.append_event(
            &session.id,
            "model.stream_delta",
            serde_json::json!({"text": "I checked the top-level files and docs."}),
        )?;
        app.store.append_event(
            &session.id,
            "model.response.output_item.completed",
            serde_json::json!({"item_type": "message", "phase": "commentary"}),
        )?;
        app.selected_session_id = Some(session.id);
        app.drain_store_notifications()?;

        let screen = render_dump(&mut app)?;
        assert!(screen.contains("I checked the top-level files and docs."));
        assert!(screen.contains("Thinking..."));
        assert!(!screen.contains("Working..."));
        Ok(())
    }

    #[test]
    fn wrapped_pending_followup_keeps_full_transcript_viewport_height() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let mut app = ready_app(&temp)?;
        let session = app.store.create_session(None, std::env::current_dir()?)?;
        app.store.append_event(
            &session.id,
            "session.input",
            serde_json::json!({"text": "greet me"}),
        )?;
        app.store.append_event(
            &session.id,
            "session.done",
            serde_json::json!({"result": "Hello! How can I help you today?"}),
        )?;
        let events = app.store.events_for_session(&session.id)?;
        let last_seq = events.last().map(|event| event.seq).unwrap_or_default();
        app.selected_session_id = Some(session.id.clone());
        app.native_history
            .reset_for_session(session.id.clone(), last_seq);

        let docked = desired_terminal_viewport_height_for(&mut app, 80, 28)?;
        let long_prompt =
            "mmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmm can you yell me about this repo";
        app.dispatch(AppCommand::SendFollowup {
            session_id: session.id.clone(),
            text: long_prompt.to_string(),
        })?;
        app.store.append_event(
            &session.id,
            "model.turn.request",
            serde_json::json!({"model": "GPT-5.5", "provider": "codex"}),
        )?;
        app.drain_store_notifications()?;

        let measured = desired_terminal_viewport_height_for(&mut app, 80, 28)?;
        assert_eq!(measured, docked.saturating_add(1));
        app.args.width = 80;
        app.args.height = measured;
        let native_prompt = lines_plain_text(&native_scrollback_lines(&mut app, 80)?);
        assert!(native_prompt.contains("> mmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmm"));
        assert!(native_prompt.contains("yell me about this repo"));
        let screen = render_dump(&mut app)?;
        assert!(screen.contains("thinking"));
        Ok(())
    }

    #[test]
    fn wrapped_streaming_followup_keeps_full_transcript_viewport_height() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let mut app = ready_app(&temp)?;
        let session = app.store.create_session(None, std::env::current_dir()?)?;
        app.store.append_event(
            &session.id,
            "session.input",
            serde_json::json!({"text": "greet me"}),
        )?;
        app.store.append_event(
            &session.id,
            "session.done",
            serde_json::json!({"result": "Hello! How can I help you today?"}),
        )?;
        let events = app.store.events_for_session(&session.id)?;
        let last_seq = events.last().map(|event| event.seq).unwrap_or_default();
        app.selected_session_id = Some(session.id.clone());
        app.native_history
            .reset_for_session(session.id.clone(), last_seq);

        let docked = desired_terminal_viewport_height_for(&mut app, 80, 28)?;
        app.dispatch(AppCommand::SendFollowup {
            session_id: session.id.clone(),
            text: "tell me more".to_string(),
        })?;
        app.store.append_event(
            &session.id,
            "model.turn.request",
            serde_json::json!({"model": "GPT-5.5", "provider": "codex"}),
        )?;
        app.store.append_event(
            &session.id,
            "model.stream_delta",
            serde_json::json!({"text": "This is a Rust-first browser agent terminal/workbench named browser-use terminal. The core design keeps active output redrawable until it is final."}),
        )?;
        app.drain_store_notifications()?;

        let measured = desired_terminal_viewport_height_for(&mut app, 80, 28)?;
        assert_eq!(measured, docked);
        app.args.width = 80;
        app.args.height = measured;
        let screen = render_dump(&mut app)?;
        assert!(screen.contains("Type to steer the agent"));
        assert!(!screen.contains("Thinking..."));
        let state = app.workbench_state()?;
        let model = transcript::transcript_model(&app, &state).expect("model");
        let streaming_lines = lines_plain_text(&transcript::active_streaming_lines(
            Some(&model),
            80_u16.saturating_sub(8).max(1),
        ));
        assert!(streaming_lines.contains("This is a Rust-first browser agent"));
        assert!(streaming_lines.contains("browser-use"));
        assert!(streaming_lines.contains("core design"));
        Ok(())
    }

    #[test]
    fn native_followup_streaming_crops_to_transcript_body_without_resizing() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let mut app = ready_app(&temp)?;
        let session = app.store.create_session(None, std::env::current_dir()?)?;
        app.store.append_event(
            &session.id,
            "session.input",
            serde_json::json!({"text": "summarize"}),
        )?;
        app.store.append_event(
            &session.id,
            "session.done",
            serde_json::json!({"result": "Previous completed answer."}),
        )?;
        let events = app.store.events_for_session(&session.id)?;
        let last_seq = events.last().map(|event| event.seq).unwrap_or_default();
        app.selected_session_id = Some(session.id.clone());
        app.native_history
            .reset_for_session(session.id.clone(), last_seq);

        let docked = desired_terminal_viewport_height_for(&mut app, 100, 28)?;
        app.dispatch(AppCommand::SendFollowup {
            session_id: session.id.clone(),
            text: "continue".to_string(),
        })?;
        app.store.append_event(
            &session.id,
            "model.turn.request",
            serde_json::json!({"model": "GPT-5.5", "provider": "codex"}),
        )?;
        app.store.append_event(
            &session.id,
            "model.stream_delta",
            serde_json::json!({"text": "live output line 01"}),
        )?;
        app.drain_store_notifications()?;

        let initial = desired_terminal_viewport_height_for(&mut app, 100, 28)?;
        assert_eq!(initial, docked.saturating_add(1));

        let streamed = (1..=24)
            .map(|idx| format!("live output line {idx:02}"))
            .collect::<Vec<_>>()
            .join("\n");
        app.store.append_event(
            &session.id,
            "model.stream_delta",
            serde_json::json!({"text": streamed}),
        )?;
        app.drain_store_notifications()?;

        let grown = desired_terminal_viewport_height_for(&mut app, 100, 28)?;
        assert_eq!(grown.saturating_add(1), initial);
        app.args.width = 100;
        app.args.height = grown;
        let screen = render_dump(&mut app)?;
        assert!(screen.contains("live output line 24"));
        assert!(!screen.contains("live output line 01"));
        assert!(!screen.contains("Thinking..."));
        assert!(!screen.contains("Working..."));
        assert!(screen.contains("Type to steer the agent"));
        Ok(())
    }

    #[test]
    fn tool_call_response_hides_committed_stream_text() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let mut app = ready_app(&temp)?;
        let session = app.store.create_session(None, std::env::current_dir()?)?;
        app.store.append_event(
            &session.id,
            "session.input",
            serde_json::json!({"text": "greet me"}),
        )?;
        app.store.append_event(
            &session.id,
            "session.done",
            serde_json::json!({"result": "Hello! How can I help you today?"}),
        )?;
        let events = app.store.events_for_session(&session.id)?;
        let done_seq = events.last().map(|event| event.seq).unwrap_or_default();
        app.selected_session_id = Some(session.id.clone());
        app.native_history
            .reset_for_session(session.id.clone(), done_seq);

        app.dispatch(AppCommand::SendFollowup {
            session_id: session.id.clone(),
            text: "can you tell me about this repo?".to_string(),
        })?;
        app.drain_store_notifications()?;
        let state = app.workbench_state()?;
        let model = transcript::transcript_model(&app, &state).expect("model");
        let prompt_emission =
            transcript::terminal_scrollback_emission_since(&model, done_seq, 120, true);
        let prompt_text = lines_plain_text(&prompt_emission.lines);
        assert!(prompt_text.contains("> can you tell me about this repo?"));
        assert!(!prompt_text.contains("• note"));

        app.store.append_event(
            &session.id,
            "model.turn.request",
            serde_json::json!({"model": "GPT-5.5", "provider": "codex", "turn_idx": 1}),
        )?;
        app.store.append_event(
            &session.id,
            "model.stream_delta",
            serde_json::json!({"text": "Yoooo! What can I help you with?\nNo worries.", "turn_idx": 1}),
        )?;
        app.store.append_event(
            &session.id,
            "model.turn.response",
            serde_json::json!({"tool_call_count": 1, "turn_idx": 1}),
        )?;
        app.drain_store_notifications()?;

        let state = app.workbench_state()?;
        let model = transcript::transcript_model(&app, &state).expect("model");
        let commentary_emission = transcript::terminal_scrollback_emission_since(
            &model,
            prompt_emission.last_seq,
            120,
            true,
        );
        let commentary_text = lines_plain_text(&commentary_emission.lines);
        assert!(!commentary_text.contains("> can you tell me about this repo?"));
        assert!(!commentary_text.contains("• note"));
        assert!(commentary_text.contains("Yoooo! What can I help you with?"));
        let replay_emission =
            transcript::terminal_scrollback_emission_since(&model, done_seq, 120, true);
        let replay_text = lines_plain_text(&replay_emission.lines);
        let replay_lines = replay_text.lines().collect::<Vec<_>>();
        assert!(
            replay_lines
                .iter()
                .any(|line| line.contains("> can you tell me about this repo?")),
            "{replay_text}"
        );
        assert!(!replay_text.contains("• note"));
        assert!(replay_text.contains("Yoooo! What can I help you with?"));

        app.args.width = 120;
        app.args.height = 28;
        let active_screen = render_dump(&mut app)?;
        assert_eq!(
            active_screen.matches("Yoooo! What can I help you with?").count(),
            0,
            "committed pre-tool commentary should not be duplicated in the active viewport\n{active_screen}"
        );
        Ok(())
    }

    #[test]
    fn native_activity_tail_grows_in_active_view_until_next_block() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let mut app = ready_app(&temp)?;
        let session = app.store.create_session(None, std::env::current_dir()?)?;
        app.store.append_event(
            &session.id,
            "session.input",
            serde_json::json!({"text": "greet me"}),
        )?;
        app.store.append_event(
            &session.id,
            "session.done",
            serde_json::json!({"result": "Hello! How can I help you today?"}),
        )?;
        let events = app.store.events_for_session(&session.id)?;
        let done_seq = events.last().map(|event| event.seq).unwrap_or_default();
        app.selected_session_id = Some(session.id.clone());
        app.native_history
            .reset_for_session(session.id.clone(), done_seq);

        app.dispatch(AppCommand::SendFollowup {
            session_id: session.id.clone(),
            text: "inspect repo".to_string(),
        })?;
        app.store.append_event(
            &session.id,
            "file.read",
            serde_json::json!({"path": "README.md"}),
        )?;
        app.drain_store_notifications()?;

        let state = app.workbench_state()?;
        let model = transcript::transcript_model(&app, &state).expect("model");
        let prompt_emission =
            transcript::terminal_scrollback_emission_since(&model, done_seq, 120, true);
        let prompt_text = lines_plain_text(&prompt_emission.lines);
        assert!(prompt_text.contains("> inspect repo"));
        assert!(!prompt_text.contains("README.md"), "{prompt_text}");
        let active_text =
            lines_plain_text(&transcript::active_viewport_lines(Some(&model), 120, 20));
        assert!(active_text.contains("• explored"), "{active_text}");
        assert!(active_text.contains("read README.md"), "{active_text}");

        app.store.append_event(
            &session.id,
            "file.read",
            serde_json::json!({"path": "Cargo.toml"}),
        )?;
        app.drain_store_notifications()?;
        let state = app.workbench_state()?;
        let model = transcript::transcript_model(&app, &state).expect("model");
        let deferred = transcript::terminal_scrollback_emission_since(
            &model,
            prompt_emission.last_seq,
            120,
            true,
        );
        assert!(
            lines_plain_text(&deferred.lines).trim().is_empty(),
            "{}",
            lines_plain_text(&deferred.lines)
        );
        let active_text =
            lines_plain_text(&transcript::active_viewport_lines(Some(&model), 120, 20));
        assert!(
            active_text.contains("read README.md, Cargo.toml"),
            "{active_text}"
        );

        app.store.append_event(
            &session.id,
            "command.started",
            serde_json::json!({"cmd": "git status --short"}),
        )?;
        app.drain_store_notifications()?;
        let state = app.workbench_state()?;
        let model = transcript::transcript_model(&app, &state).expect("model");
        let flushed = transcript::terminal_scrollback_emission_since(
            &model,
            prompt_emission.last_seq,
            120,
            true,
        );
        let flushed_text = lines_plain_text(&flushed.lines);
        assert!(
            flushed_text.contains("read README.md, Cargo.toml"),
            "{flushed_text}"
        );
        assert!(
            !flushed_text.contains("git status --short"),
            "{flushed_text}"
        );
        let active_text =
            lines_plain_text(&transcript::active_viewport_lines(Some(&model), 120, 20));
        assert!(active_text.contains("git status --short"), "{active_text}");
        assert!(!active_text.contains("README.md"), "{active_text}");
        Ok(())
    }

    #[test]
    fn streaming_flushes_deferred_activity_tail_before_replacing_live_view() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let mut app = ready_app(&temp)?;
        let session = app.store.create_session(None, std::env::current_dir()?)?;
        app.store.append_event(
            &session.id,
            "session.input",
            serde_json::json!({"text": "greet me"}),
        )?;
        app.store.append_event(
            &session.id,
            "session.done",
            serde_json::json!({"result": "Hello! How can I help you today?"}),
        )?;
        let events = app.store.events_for_session(&session.id)?;
        let done_seq = events.last().map(|event| event.seq).unwrap_or_default();
        app.selected_session_id = Some(session.id.clone());
        app.native_history
            .reset_for_session(session.id.clone(), done_seq);

        app.dispatch(AppCommand::SendFollowup {
            session_id: session.id.clone(),
            text: "inspect repo".to_string(),
        })?;
        app.store.append_event(
            &session.id,
            "file.read",
            serde_json::json!({"path": "README.md"}),
        )?;
        app.store.append_event(
            &session.id,
            "file.read",
            serde_json::json!({"path": "Cargo.toml"}),
        )?;
        app.drain_store_notifications()?;
        let state = app.workbench_state()?;
        let model = transcript::transcript_model(&app, &state).expect("model");
        let prompt_emission =
            transcript::terminal_scrollback_emission_since(&model, done_seq, 120, true);
        let prompt_text = lines_plain_text(&prompt_emission.lines);
        assert!(prompt_text.contains("> inspect repo"));
        assert!(!prompt_text.contains("README.md"), "{prompt_text}");
        app.native_history.last_seq = prompt_emission.last_seq;
        let active_before_stream =
            lines_plain_text(&transcript::active_viewport_lines(Some(&model), 120, 20));
        assert!(
            active_before_stream.contains("read README.md, Cargo.toml"),
            "{active_before_stream}"
        );

        app.store.append_event(
            &session.id,
            "model.turn.request",
            serde_json::json!({"model": "GPT-5.5", "provider": "codex"}),
        )?;
        app.store.append_event(
            &session.id,
            "model.stream_delta",
            serde_json::json!({"text": "I am checking the files before summarizing."}),
        )?;
        app.drain_store_notifications()?;
        let state = app.workbench_state()?;
        let model = transcript::transcript_model(&app, &state).expect("model");
        let has_live_streaming_output =
            !transcript::active_streaming_lines(Some(&model), 120).is_empty();
        let stream_emission = transcript::terminal_scrollback_emission_since(
            &model,
            app.native_history.last_seq,
            120,
            !has_live_streaming_output,
        );
        let stream_emission_text = lines_plain_text(&stream_emission.lines);
        assert!(
            stream_emission_text.contains("read README.md, Cargo.toml"),
            "{stream_emission_text}"
        );
        assert!(
            !stream_emission_text.contains("I am checking"),
            "{stream_emission_text}"
        );
        app.native_history.last_seq = stream_emission.last_seq;
        let active_during_stream =
            lines_plain_text(&transcript::active_viewport_lines(Some(&model), 120, 20));
        assert!(
            active_during_stream.contains("I am checking the files"),
            "{active_during_stream}"
        );
        assert!(
            !active_during_stream.contains("Thinking..."),
            "{active_during_stream}"
        );
        assert!(
            !active_during_stream.contains("Working..."),
            "{active_during_stream}"
        );
        assert!(
            !active_during_stream.contains("README.md"),
            "{active_during_stream}"
        );

        app.store.append_event(
            &session.id,
            "model.turn.response",
            serde_json::json!({"tool_call_count": 1}),
        )?;
        app.store.append_event(
            &session.id,
            "file.read",
            serde_json::json!({"path": "Taskfile.yml"}),
        )?;
        app.drain_store_notifications()?;
        let state = app.workbench_state()?;
        let model = transcript::transcript_model(&app, &state).expect("model");
        let active_after_response =
            lines_plain_text(&transcript::active_viewport_lines(Some(&model), 120, 20));
        assert!(
            active_after_response.contains("read Taskfile.yml"),
            "{active_after_response}"
        );
        assert!(
            !active_after_response.contains("README.md"),
            "{active_after_response}"
        );
        assert!(
            !active_after_response.contains("Cargo.toml"),
            "{active_after_response}"
        );
        Ok(())
    }

    #[test]
    fn multiline_composer_does_not_resize_completed_history_viewport() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let mut app = ready_app(&temp)?;
        let session = app.store.create_session(None, std::env::current_dir()?)?;
        app.store.append_event(
            &session.id,
            "session.input",
            serde_json::json!({"text": "describe this repo"}),
        )?;
        app.store.append_event(
            &session.id,
            "session.done",
            serde_json::json!({"result": "It is a Rust browser-agent workbench."}),
        )?;
        let events = app.store.events_for_session(&session.id)?;
        let last_seq = events.last().map(|event| event.seq).unwrap_or_default();
        app.selected_session_id = Some(session.id.clone());
        app.native_history.reset_for_session(session.id, last_seq);

        let before = desired_terminal_viewport_height(&mut app)?;
        app.set_input("first line".to_string());
        assert!(!app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT))?);
        assert!(!app.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE))?);
        let after = desired_terminal_viewport_height(&mut app)?;
        assert_eq!(before, after);
        Ok(())
    }

    #[test]
    fn completed_session_popups_do_not_resize_native_viewport() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let mut app = ready_app(&temp)?;
        let session = app.store.create_session(None, std::env::current_dir()?)?;
        app.store.append_event(
            &session.id,
            "session.input",
            serde_json::json!({"text": "describe this repo"}),
        )?;
        app.store.append_event(
            &session.id,
            "session.done",
            serde_json::json!({"result": "It is a Rust browser-agent workbench."}),
        )?;
        let events = app.store.events_for_session(&session.id)?;
        let last_seq = events.last().map(|event| event.seq).unwrap_or_default();
        app.selected_session_id = Some(session.id.clone());
        app.native_history.reset_for_session(session.id, last_seq);

        let docked = desired_terminal_viewport_height(&mut app)?;
        for surface in [
            Surface::History,
            Surface::Model,
            Surface::Mode,
            Surface::Browser,
            Surface::BrowserSelect,
            Surface::Account,
        ] {
            app.open_surface(surface);
            assert_eq!(desired_terminal_viewport_height(&mut app)?, docked);
            let state = app.workbench_state()?;
            let overlay = render::active_modal_overlay(&app, &state, Rect::new(0, 0, 100, 28))
                .expect("surface should render as a modal overlay");
            let overlay = buffer_symbols(&overlay.buffer);
            assert!(overlay.contains(surface_heading_for_test(surface)));
        }
        Ok(())
    }

    #[test]
    fn transcript_does_not_commit_child_events_as_parent_output() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let mut app = ready_app(&temp)?;
        let parent = app.store.create_session(None, std::env::current_dir()?)?;
        app.store.append_event(
            &parent.id,
            "session.input",
            serde_json::json!({"text": "inspect repository"}),
        )?;
        let child = app.store.create_child_session(
            &parent.id,
            std::env::current_dir()?,
            None,
            Some("repo-explorer"),
            Some("explorer"),
        )?;
        app.store.append_event(
            &parent.id,
            "agent.spawned",
            serde_json::json!({"child_session_id": child.id, "nickname": "repo-explorer"}),
        )?;
        app.store.append_event(
            &child.id,
            "file.read",
            serde_json::json!({"path": "SECRET_CHILD_ONLY.md"}),
        )?;
        app.store.append_event(
            &parent.id,
            "agent.completed",
            serde_json::json!({
                "child_session_id": child.id,
                "status": "done",
                "payload": {"result": "Repository inspected read-only."}
            }),
        )?;
        app.store.append_event(
            &parent.id,
            "session.done",
            serde_json::json!({"result": "This repo is a Rust terminal workbench."}),
        )?;
        app.selected_session_id = Some(parent.id);
        app.drain_store_notifications()?;
        let state = app.workbench_state()?;
        let model = transcript::transcript_model(&app, &state).expect("model");
        let text = lines_plain_text(&transcript::all_scrollback_lines(&model, 100));

        assert!(text.contains("subagent repo-explorer started"));
        assert!(text.contains("subagent repo-explorer finished"));
        assert!(!text.contains("Repository inspected read-only."));
        assert!(text.contains("This repo is a Rust terminal workbench."));
        assert!(!text.contains("SECRET_CHILD_ONLY.md"));
        Ok(())
    }

    #[test]
    fn followups_render_as_transcript_turns() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let mut app = ready_app(&temp)?;
        let session = app.store.create_session(None, std::env::current_dir()?)?;
        app.store.append_event(
            &session.id,
            "session.input",
            serde_json::json!({"text": "inspect repository"}),
        )?;
        app.store.append_event(
            &session.id,
            "session.done",
            serde_json::json!({"result": "It is a Rust TUI."}),
        )?;
        app.store.append_event(
            &session.id,
            "session.followup",
            serde_json::json!({"text": "which files matter most?"}),
        )?;
        app.store.append_event(
            &session.id,
            "session.done",
            serde_json::json!({"result": "Cargo.toml and crates/browser-use-tui/src/main.rs."}),
        )?;
        app.selected_session_id = Some(session.id);
        let screen = render_dump(&mut app)?;
        assert!(!screen.contains("• answer"));
        assert!(screen.contains("Cargo.toml"));
        Ok(())
    }

    #[test]
    fn followup_and_retry_enter_running_state_before_agent_events() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let mut app = ready_app(&temp)?;
        let done = app.store.create_session(None, std::env::current_dir()?)?;
        app.store.append_event(
            &done.id,
            "session.input",
            serde_json::json!({"text": "first task"}),
        )?;
        app.store.append_event(
            &done.id,
            "session.done",
            serde_json::json!({"result": "done"}),
        )?;
        app.dispatch(AppCommand::SendFollowup {
            session_id: done.id.clone(),
            text: "continue".to_string(),
        })?;
        assert_eq!(
            app.store
                .load_session(&done.id)?
                .map(|session| session.status),
            Some(SessionStatus::Running)
        );

        let failed = app.store.create_session(None, std::env::current_dir()?)?;
        app.store.append_event(
            &failed.id,
            "session.input",
            serde_json::json!({"text": "retry me"}),
        )?;
        app.store.append_event(
            &failed.id,
            "session.failed",
            serde_json::json!({"error": "read Codex SSE line"}),
        )?;
        app.dispatch(AppCommand::RetryTask(failed.id.clone()))?;
        assert_eq!(
            app.store
                .load_session(&failed.id)?
                .map(|session| session.status),
            Some(SessionStatus::Running)
        );
        Ok(())
    }

    #[test]
    fn followup_retry_cancel_and_developer_surface_work() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let app_args = Args {
            select_latest: true,
            seed_demo: Some("done".to_string()),
            agent: AgentBackend::Fake,
            browser: "Local Chrome".to_string(),
            ..args(&temp)
        };
        let mut app = App::new(app_args)?;
        app.setup_complete = true;
        app.store.set_setting("setup.complete", "1")?;
        let session_id = app.selected_session_id.clone().context("seed session")?;
        app.set_input("shorter".to_string());
        assert!(!app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))?);
        let events = app.store.events_for_session(&session_id)?;
        assert!(events
            .iter()
            .any(|event| event.event_type == "session.followup"));

        let running = app.store.create_session(None, std::env::current_dir()?)?;
        app.store.append_event(
            &running.id,
            "session.input",
            serde_json::json!({"text": "run"}),
        )?;
        app.selected_session_id = Some(running.id.clone());
        assert!(!app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL))?);
        assert_eq!(
            app.store
                .load_session(&running.id)?
                .map(|session| session.status),
            Some(SessionStatus::Cancelled)
        );

        app.open_surface(Surface::Developer);
        let screen = render_dump(&mut app)?;
        assert!(screen.contains("Laminar"));
        assert!(screen.contains("Events"));
        Ok(())
    }

    #[test]
    fn enter_steers_and_tab_queues_followup_like_codex() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let mut app = ready_app(&temp)?;
        let running = app.store.create_session(None, std::env::current_dir()?)?;
        app.store.append_event(
            &running.id,
            "session.input",
            serde_json::json!({"text": "run"}),
        )?;
        app.selected_session_id = Some(running.id.clone());

        app.set_input("steer before the next tool".to_string());
        assert!(!app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))?);
        let events = app.store.events_for_session(&running.id)?;
        assert!(events.iter().any(|event| {
            event.event_type == "session.followup"
                && event.payload["text"] == "steer before the next tool"
        }));

        app.set_input("after the current turn".to_string());
        assert!(!app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE))?);
        let events = app.store.events_for_session(&running.id)?;
        let queued = events
            .iter()
            .find(|event| {
                event.event_type == SESSION_QUEUED_FOLLOWUP_EVENT
                    && event.payload["text"] == "after the current turn"
            })
            .context("queued follow-up")?
            .seq;
        assert_eq!(
            events
                .iter()
                .filter(|event| event.event_type == "session.followup")
                .count(),
            1
        );

        app.store.append_event(
            &running.id,
            "session.done",
            serde_json::json!({"result": "done"}),
        )?;
        app.drain_store_notifications()?;
        let events = app.store.events_for_session(&running.id)?;
        assert!(events.iter().any(|event| {
            event.event_type == SESSION_QUEUED_FOLLOWUP_SENT_EVENT
                && event.payload["queued_seq"] == queued
        }));
        assert!(events.iter().any(|event| {
            event.event_type == "session.followup"
                && event.payload["text"] == "after the current turn"
                && event.payload["queued_from_seq"] == queued
        }));
        Ok(())
    }

    #[test]
    fn message_selector_edits_submitted_and_cancels_queued_messages() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let mut app = ready_app(&temp)?;
        let session = app.store.create_session(None, std::env::current_dir()?)?;
        app.store.append_event(
            &session.id,
            "session.input",
            serde_json::json!({"text": "initial task"}),
        )?;
        app.store.append_event(
            &session.id,
            "session.done",
            serde_json::json!({"result": "first answer"}),
        )?;
        app.store.append_event(
            &session.id,
            "session.followup",
            serde_json::json!({"text": "revise this"}),
        )?;
        app.store.append_event(
            &session.id,
            "session.done",
            serde_json::json!({"result": "second answer"}),
        )?;
        app.selected_session_id = Some(session.id.clone());
        app.drain_store_notifications()?;

        assert!(!app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))?);
        assert!(!app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))?);
        assert_eq!(app.surface, Surface::Messages);
        let submitted_selector = render_dump(&mut app)?;
        assert!(submitted_selector.contains("Enter:edit | Esc:close"));
        assert!(!submitted_selector.contains("Del:remove"));
        assert!(!app.handle_key(KeyEvent::new(KeyCode::Delete, KeyModifiers::NONE))?);
        assert_eq!(app.surface, Surface::Messages);
        assert!(app.composer.input().is_empty());
        let events = app.store.events_for_session(&session.id)?;
        assert!(!events
            .iter()
            .any(|event| event.event_type == SESSION_ROLLBACK_EVENT));
        assert!(!app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))?);
        assert_eq!(app.composer.input(), "revise this");
        let events = app.store.events_for_session(&session.id)?;
        assert!(events.iter().any(|event| {
            event.event_type == SESSION_ROLLBACK_EVENT
                && event.payload["action"] == "edit"
                && event.payload["num_turns"] == 1
        }));
        let visible = browser_use_core::rollback_filtered_event_records(&events)
            .into_iter()
            .filter_map(event_payload_text)
            .collect::<Vec<_>>();
        assert_eq!(visible, vec!["initial task"]);

        let running = app.store.create_session(None, std::env::current_dir()?)?;
        app.store.append_event(
            &running.id,
            "session.input",
            serde_json::json!({"text": "running"}),
        )?;
        app.selected_session_id = Some(running.id.clone());
        app.set_input("queued draft".to_string());
        assert!(!app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE))?);
        assert!(!app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))?);
        assert!(!app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))?);
        assert_eq!(app.surface, Surface::Messages);
        let queued_selector = render_dump(&mut app)?;
        assert!(queued_selector.contains("Del:cancel queued"));
        assert!(!app.handle_key(KeyEvent::new(KeyCode::Delete, KeyModifiers::NONE))?);
        let events = app.store.events_for_session(&running.id)?;
        assert!(events.iter().any(|event| {
            event.event_type == SESSION_QUEUED_FOLLOWUP_CANCELLED_EVENT
                && event.payload["reason"] == "removed from message selector"
        }));
        Ok(())
    }

    #[test]
    fn escape_once_reclaims_initial_prompt_before_output() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let mut app = ready_app(&temp)?;
        let session = app.store.create_session(None, std::env::current_dir()?)?;
        let submitted = app.store.append_event(
            &session.id,
            "session.input",
            serde_json::json!({"text": "whats up"}),
        )?;
        app.store.append_event(
            &session.id,
            "model.turn.request",
            serde_json::json!({"model": "GPT-5.5", "provider": "codex"}),
        )?;
        app.store.append_event(
            &session.id,
            "model.thinking_delta",
            serde_json::json!({"text": "checking context"}),
        )?;
        app.selected_session_id = Some(session.id.clone());
        app.drain_store_notifications()?;

        assert!(!app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))?);

        assert_eq!(app.composer.input(), "whats up");
        assert_eq!(app.selected_session_id, None);
        assert!(!app.escape_stop_is_pending());
        assert_eq!(app.surface, Surface::Main);
        assert_eq!(
            app.store
                .load_session(&session.id)?
                .map(|session| session.status),
            Some(SessionStatus::Cancelled)
        );
        let events = app.store.events_for_session(&session.id)?;
        assert!(events.iter().any(|event| {
            event.event_type == SESSION_ROLLBACK_EVENT
                && event.payload["action"] == "take_back"
                && event.payload["source"] == "tui_escape"
                && event.payload["target_seq"] == submitted.seq
                && event.payload["num_turns"] == 1
        }));
        let visible_submissions = browser_use_core::rollback_filtered_event_records(&events)
            .into_iter()
            .filter(|event| {
                matches!(
                    event.event_type.as_str(),
                    "session.input" | "session.followup"
                )
            })
            .filter_map(event_payload_text)
            .collect::<Vec<_>>();
        assert!(visible_submissions.is_empty());
        Ok(())
    }

    #[test]
    fn escape_once_reclaims_followup_before_output_without_clearing_history() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let mut app = ready_app(&temp)?;
        let session = app.store.create_session(None, std::env::current_dir()?)?;
        app.store.append_event(
            &session.id,
            "session.input",
            serde_json::json!({"text": "initial task"}),
        )?;
        app.store.append_event(
            &session.id,
            "session.done",
            serde_json::json!({"result": "first answer"}),
        )?;
        app.store.append_event(
            &session.id,
            "session.followup",
            serde_json::json!({"text": "revise this"}),
        )?;
        app.store.append_event(
            &session.id,
            "model.turn.request",
            serde_json::json!({"model": "GPT-5.5", "provider": "codex"}),
        )?;
        app.selected_session_id = Some(session.id.clone());
        app.drain_store_notifications()?;

        assert!(!app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))?);

        assert_eq!(app.composer.input(), "revise this");
        assert_eq!(
            app.selected_session_id.as_deref(),
            Some(session.id.as_str())
        );
        assert!(!app.escape_stop_is_pending());
        let events = app.store.events_for_session(&session.id)?;
        let visible_submissions = browser_use_core::rollback_filtered_event_records(&events)
            .into_iter()
            .filter(|event| {
                matches!(
                    event.event_type.as_str(),
                    "session.input" | "session.followup"
                )
            })
            .filter_map(event_payload_text)
            .collect::<Vec<_>>();
        assert_eq!(visible_submissions, vec!["initial task"]);
        Ok(())
    }

    #[test]
    fn escape_once_does_not_reclaim_after_streamed_output() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let mut app = ready_app(&temp)?;
        let session = app.store.create_session(None, std::env::current_dir()?)?;
        app.store.append_event(
            &session.id,
            "session.input",
            serde_json::json!({"text": "run"}),
        )?;
        app.store.append_event(
            &session.id,
            "model.stream_delta",
            serde_json::json!({"text": "visible output"}),
        )?;
        app.selected_session_id = Some(session.id.clone());
        app.drain_store_notifications()?;

        assert!(!app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))?);

        assert!(app.composer.input().is_empty());
        assert!(app.escape_stop_is_pending());
        let events = app.store.events_for_session(&session.id)?;
        assert!(!events
            .iter()
            .any(|event| event.event_type == SESSION_ROLLBACK_EVENT));
        Ok(())
    }

    #[test]
    fn agent_panic_records_failed_session() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let store = Store::open(temp.path())?;
        let session = store.create_session(None, std::env::current_dir()?)?;
        store.append_event(
            &session.id,
            "session.input",
            serde_json::json!({"text": "panic"}),
        )?;
        store.append_event(
            &session.id,
            "session.status",
            serde_json::json!({"status": "running"}),
        )?;

        record_agent_panic(
            temp.path().to_path_buf(),
            session.id.clone(),
            None,
            "test panic".to_string(),
        );

        let session = store.load_session(&session.id)?.context("session")?;
        assert_eq!(session.status, SessionStatus::Failed);
        let events = store.events_for_session(&session.id)?;
        assert!(events.iter().any(|event| {
            event.event_type == "session.failed"
                && event
                    .payload
                    .get("error")
                    .and_then(serde_json::Value::as_str)
                    .is_some_and(|error| error.contains("test panic"))
        }));
        Ok(())
    }

    #[test]
    fn escape_twice_opens_message_selector_and_ctrl_c_stops_running_task() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let mut app = ready_app(&temp)?;
        let running = app.store.create_session(None, std::env::current_dir()?)?;
        app.store.append_event(
            &running.id,
            "session.input",
            serde_json::json!({"text": "run"}),
        )?;
        app.store.append_event(
            &running.id,
            "model.stream_delta",
            serde_json::json!({"text": "visible output"}),
        )?;
        app.selected_session_id = Some(running.id.clone());

        assert!(!app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))?);
        assert!(app.escape_stop_is_pending());
        assert_eq!(
            app.store
                .load_session(&running.id)?
                .map(|session| session.status),
            Some(SessionStatus::Running)
        );
        let screen = render_dump(&mut app)?;
        assert!(screen.contains("esc again to edit messages"));

        assert!(!app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))?);
        assert!(!app.escape_stop_is_pending());
        assert_eq!(app.surface, Surface::Messages);
        let screen = render_dump(&mut app)?;
        assert!(screen.contains("Messages"));
        assert!(screen.contains("run"));

        assert!(!app.handle_key(KeyEvent::new(KeyCode::Char('C'), KeyModifiers::CONTROL))?);
        assert_eq!(
            app.store
                .load_session(&running.id)?
                .map(|session| session.status),
            Some(SessionStatus::Cancelled)
        );
        assert_eq!(app.surface, Surface::Main);
        let screen = render_dump(&mut app)?;
        assert!(screen.contains("stopped"));
        Ok(())
    }
}
