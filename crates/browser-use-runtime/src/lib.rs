use std::any::Any;
use std::collections::{HashMap, HashSet, VecDeque};
use std::future::Future;
use std::io::{BufRead, BufReader, Write};
#[cfg(unix)]
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use browser_use_protocol::{EventRecord, SessionMeta, SessionStatus};
use browser_use_store::{AgentSummary, Store};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::{broadcast, watch};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

#[derive(Debug, thiserror::Error)]
pub enum RuntimeError {
    #[error("agent limit reached: limit {limit}, open spawned agents {open_spawned_agents}")]
    AgentLimitReached {
        limit: usize,
        open_spawned_agents: usize,
    },
    #[error("browser already in use: browser {browser_id}, active agent {active_agent_id}")]
    BrowserAlreadyInUse {
        browser_id: String,
        active_agent_id: String,
    },
    #[error("unknown browser: {0}")]
    UnknownBrowser(String),
    #[error("browser lease mismatch: browser {browser_id}, owner {owner_agent_id:?}, caller {caller_agent_id}")]
    BrowserLeaseMismatch {
        browser_id: String,
        owner_agent_id: Option<String>,
        caller_agent_id: String,
    },
    #[error("unknown agent: {0}")]
    UnknownAgent(String),
    #[error("agent is missing a parent: {0}")]
    MissingParentAgent(String),
    #[error("queued spawn not found: {0}")]
    QueuedSpawnNotFound(String),
}

macro_rules! id_type {
    ($name:ident) => {
        #[derive(Clone, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
        pub struct $name(String);

        impl $name {
            pub fn new() -> Self {
                Self(Uuid::new_v4().simple().to_string())
            }

            pub fn from_string(value: impl Into<String>) -> Result<Self> {
                let value = value.into();
                if value.trim().is_empty() {
                    bail!(concat!(stringify!($name), " must not be empty"));
                }
                Ok(Self(value))
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }

            pub fn into_string(self) -> String {
                self.0
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(&self.0)
            }
        }
    };
}

id_type!(AgentId);
id_type!(BrowserId);
id_type!(EventId);
id_type!(RootId);
id_type!(RunId);
id_type!(SessionId);
id_type!(ToolCallId);
id_type!(TurnId);

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Durability {
    Barrier,
    BestEffort,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeEventKind {
    RuntimeStarted,
    RuntimeShutdown,
    AgentCreated,
    AgentInputAccepted,
    AgentInputConsumed,
    AgentStarted,
    AgentQueued,
    AgentResumed,
    AgentCompleted,
    AgentFailed,
    AgentCancelRequested,
    AgentCancelled,
    AgentCloseRequested,
    AgentClosed,
    AgentContinuationStarted,
    AgentTurnStarted,
    AgentTurnCompleted,
    AgentTurnAborted,
    SubagentSpawnRequested,
    SubagentSpawnStarted,
    SubagentSpawnQueued,
    SubagentSpawnRejected,
    SubagentSpawnCompleted,
    MailboxEnqueued,
    MailboxDelivered,
    MailboxConsumed,
    WaitAgentStarted,
    WaitAgentCompleted,
    WaitAgentTimedOut,
    BrowserCreated,
    BrowserStarted,
    BrowserClaimed,
    BrowserReleased,
    BrowserClosed,
    BrowserScriptStarted,
    BrowserScriptOutputDelta,
    BrowserScriptCompleted,
    BrowserScriptCancelled,
    BrowserScriptFailed,
    ExecCommandBegin,
    ExecCommandOutputDelta,
    ExecCommandEnd,
    PythonStarted,
    PythonOutputDelta,
    PythonCompleted,
    McpConnected,
    McpToolStarted,
    McpToolCompleted,
    ArtifactCreated,
    ToolOutputDelta,
    ToolCompleted,
    ToolFailed,
    StoreEventAppended,
    ResourceLost,
}

impl RuntimeEventKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::RuntimeStarted => "runtime.started",
            Self::RuntimeShutdown => "runtime.shutdown",
            Self::AgentCreated => "agent.created",
            Self::AgentInputAccepted => "agent.input.accepted",
            Self::AgentInputConsumed => "agent.input.consumed",
            Self::AgentStarted => "agent.started",
            Self::AgentQueued => "agent.queued",
            Self::AgentResumed => "agent.resumed",
            Self::AgentCompleted => "agent.completed",
            Self::AgentFailed => "agent.failed",
            Self::AgentCancelRequested => "agent.cancel_requested",
            Self::AgentCancelled => "agent.cancelled",
            Self::AgentCloseRequested => "agent.close_requested",
            Self::AgentClosed => "agent.closed",
            Self::AgentContinuationStarted => "agent.continuation_started",
            Self::AgentTurnStarted => "agent.turn.started",
            Self::AgentTurnCompleted => "agent.turn.completed",
            Self::AgentTurnAborted => "agent.turn.aborted",
            Self::SubagentSpawnRequested => "subagent.spawn_requested",
            Self::SubagentSpawnStarted => "subagent.spawn_started",
            Self::SubagentSpawnQueued => "subagent.spawn_queued",
            Self::SubagentSpawnRejected => "subagent.spawn_rejected",
            Self::SubagentSpawnCompleted => "subagent.spawn_completed",
            Self::MailboxEnqueued => "mailbox.enqueued",
            Self::MailboxDelivered => "mailbox.delivered",
            Self::MailboxConsumed => "mailbox.consumed",
            Self::WaitAgentStarted => "wait_agent.started",
            Self::WaitAgentCompleted => "wait_agent.completed",
            Self::WaitAgentTimedOut => "wait_agent.timed_out",
            Self::BrowserCreated => "browser.created",
            Self::BrowserStarted => "browser.started",
            Self::BrowserClaimed => "browser.claimed",
            Self::BrowserReleased => "browser.released",
            Self::BrowserClosed => "browser.closed",
            Self::BrowserScriptStarted => "browser.script.started",
            Self::BrowserScriptOutputDelta => "browser.script.output_delta",
            Self::BrowserScriptCompleted => "browser.script.completed",
            Self::BrowserScriptCancelled => "browser.script.cancelled",
            Self::BrowserScriptFailed => "browser.script.failed",
            Self::ExecCommandBegin => "exec_command.begin",
            Self::ExecCommandOutputDelta => "exec_command.output_delta",
            Self::ExecCommandEnd => "exec_command.end",
            Self::PythonStarted => "python.started",
            Self::PythonOutputDelta => "python.output_delta",
            Self::PythonCompleted => "python.completed",
            Self::McpConnected => "mcp.connected",
            Self::McpToolStarted => "mcp.tool.started",
            Self::McpToolCompleted => "mcp.tool.completed",
            Self::ArtifactCreated => "artifact.created",
            Self::ToolOutputDelta => "tool.output_delta",
            Self::ToolCompleted => "tool.completed",
            Self::ToolFailed => "tool.failed",
            Self::StoreEventAppended => "store.event.appended",
            Self::ResourceLost => "resource.lost",
        }
    }

    pub fn from_observed_event_type(event_type: &str) -> Self {
        match event_type {
            "exec_command.begin" => Self::ExecCommandBegin,
            "exec_command.output_delta" => Self::ExecCommandOutputDelta,
            "exec_command.end" => Self::ExecCommandEnd,
            "browser_script.started" => Self::BrowserScriptStarted,
            "browser_script.output_delta" => Self::BrowserScriptOutputDelta,
            "browser_script.completed" => Self::BrowserScriptCompleted,
            "browser_script.cancelled" => Self::BrowserScriptCancelled,
            "browser_script.failed" => Self::BrowserScriptFailed,
            "python.started" => Self::PythonStarted,
            "python.output_delta" => Self::PythonOutputDelta,
            "python.completed" => Self::PythonCompleted,
            "mcp.connected" => Self::McpConnected,
            "mcp.tool.started" => Self::McpToolStarted,
            "mcp.tool.completed" => Self::McpToolCompleted,
            "artifact.created" => Self::ArtifactCreated,
            "tool.output_delta" => Self::ToolOutputDelta,
            "tool.completed" => Self::ToolCompleted,
            "tool.failed" => Self::ToolFailed,
            _ => Self::StoreEventAppended,
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct RuntimeEvent {
    pub id: EventId,
    pub ts_ms: i64,
    pub durability: Durability,
    pub kind: RuntimeEventKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root_id: Option<RootId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<AgentId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<SessionId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<RunId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub browser_id: Option<BrowserId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<TurnId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<ToolCallId>,
    #[serde(default)]
    pub payload: Value,
}

impl RuntimeEvent {
    pub fn new(kind: RuntimeEventKind, durability: Durability) -> Self {
        Self {
            id: EventId::new(),
            ts_ms: now_ms(),
            durability,
            kind,
            root_id: None,
            agent_id: None,
            session_id: None,
            run_id: None,
            browser_id: None,
            turn_id: None,
            tool_call_id: None,
            payload: Value::Object(Default::default()),
        }
    }

    pub fn with_session_id(mut self, session_id: SessionId) -> Self {
        self.session_id = Some(session_id);
        self
    }

    pub fn with_agent_id(mut self, agent_id: AgentId) -> Self {
        self.agent_id = Some(agent_id);
        self
    }

    pub fn with_root_id(mut self, root_id: RootId) -> Self {
        self.root_id = Some(root_id);
        self
    }

    pub fn with_run_id(mut self, run_id: RunId) -> Self {
        self.run_id = Some(run_id);
        self
    }

    pub fn with_browser_id(mut self, browser_id: BrowserId) -> Self {
        self.browser_id = Some(browser_id);
        self
    }

    pub fn with_payload(mut self, payload: Value) -> Self {
        self.payload = payload;
        self
    }

    pub fn event_type(&self) -> &'static str {
        self.kind.as_str()
    }

    pub fn journal_payload(&self) -> Value {
        json!({
            "runtime_event_id": self.id.as_str(),
            "durability": self.durability,
            "root_id": self.root_id.as_ref().map(|id| id.as_str()),
            "agent_id": self.agent_id.as_ref().map(|id| id.as_str()),
            "run_id": self.run_id.as_ref().map(|id| id.as_str()),
            "browser_id": self.browser_id.as_ref().map(|id| id.as_str()),
            "turn_id": self.turn_id.as_ref().map(|id| id.as_str()),
            "tool_call_id": self.tool_call_id.as_ref().map(|id| id.as_str()),
            "payload": self.payload,
        })
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct JournalAppend {
    pub seq: Option<i64>,
    pub durability: Durability,
}

pub trait JournalSink: Send + Sync {
    fn append_runtime_event(&self, event: &RuntimeEvent) -> Result<JournalAppend>;
    fn append_session_event(
        &self,
        session_id: &SessionId,
        event_type: &str,
        payload: Value,
        durability: Durability,
    ) -> Result<JournalAppend>;
    fn flush(&self) -> Result<()>;
}

pub trait JournalReader: Send + Sync {
    fn load_session(&self, session_id: &SessionId) -> Result<Option<SessionMeta>>;
    fn list_sessions(&self) -> Result<Vec<SessionMeta>>;
    fn events_for_session(&self, session_id: &SessionId) -> Result<Vec<EventRecord>>;
    fn events_after_seq(&self, session_id: &SessionId, after_seq: i64) -> Result<Vec<EventRecord>>;
}

#[derive(Clone, Debug)]
pub struct CreateThreadRequest {
    pub session_id: Option<SessionId>,
    pub parent_session_id: Option<SessionId>,
    pub cwd: PathBuf,
    pub artifact_root: Option<PathBuf>,
    pub agent_path: Option<String>,
    pub nickname: Option<String>,
    pub role: Option<String>,
}

pub trait LiveThreadPersistence: JournalReader + JournalSink {
    fn create_thread(&self, request: CreateThreadRequest) -> Result<SessionMeta>;
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SpawnEdgeStatus {
    Open,
    Done,
    Failed,
    Closed,
}

impl SpawnEdgeStatus {
    fn as_store_status(&self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Done => "done",
            Self::Failed => "failed",
            Self::Closed => "closed",
        }
    }

    fn from_store_status(status: &str) -> Self {
        match status {
            "open" => Self::Open,
            "done" => Self::Done,
            "failed" => Self::Failed,
            "closed" | "cancelled" => Self::Closed,
            _ => Self::Open,
        }
    }

    fn as_thread_status(&self) -> AgentThreadStatus {
        match self {
            Self::Open => AgentThreadStatus::Created,
            Self::Done => AgentThreadStatus::Completed,
            Self::Failed => AgentThreadStatus::Failed,
            Self::Closed => AgentThreadStatus::Closed,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SpawnEdge {
    pub parent_session_id: SessionId,
    pub child_session_id: SessionId,
    pub status: SpawnEdgeStatus,
    pub path: Option<String>,
    pub nickname: Option<String>,
    pub role: Option<String>,
}

pub trait StateIndex: Send + Sync {
    fn open_spawn_edge(&self, edge: SpawnEdge) -> Result<()>;
    fn finish_spawn_edge(
        &self,
        child_session_id: &SessionId,
        status: SpawnEdgeStatus,
    ) -> Result<()>;
    fn close_spawn_edge(&self, child_session_id: &SessionId, reason: &str) -> Result<()>;
    fn list_children(&self, parent_session_id: &SessionId) -> Result<Vec<SpawnEdge>>;
    fn list_descendants(&self, root_session_id: &SessionId) -> Result<Vec<SpawnEdge>>;
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct BrowserConfig {
    pub keep_alive: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub headless: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proxy_country_code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cdp_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cdp_headers: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_agent: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub viewport: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub storage_state: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub downloads_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allowed_domains: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocked_domains: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window_size: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state_dir: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub no_viewport: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub accept_downloads: Option<bool>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BrowserStatus {
    Created,
    Started,
    Claimed,
    Released,
    Closed,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct BrowserSnapshot {
    pub id: BrowserId,
    pub config: BrowserConfig,
    pub status: BrowserStatus,
    pub active_agent_id: Option<AgentId>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub active_scripts: Vec<BrowserScriptSnapshot>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct BrowserScriptSnapshot {
    pub run_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<SessionId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<AgentId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<ToolCallId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_delta: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct BrowserLease {
    pub browser_id: BrowserId,
    pub agent_id: AgentId,
}

#[derive(Clone, Debug)]
pub struct BrowserPhysicalRegistries {
    session_registry: browser_use_browser::BrowserSessionRegistry,
    script_registry: browser_use_browser::BrowserScriptRunRegistry,
}

impl BrowserPhysicalRegistries {
    fn new() -> Self {
        Self {
            session_registry: browser_use_browser::BrowserSessionRegistry::new(),
            script_registry: browser_use_browser::BrowserScriptRunRegistry::new(),
        }
    }

    pub fn session_registry(&self) -> browser_use_browser::BrowserSessionRegistry {
        self.session_registry.clone()
    }

    pub fn script_registry(&self) -> browser_use_browser::BrowserScriptRunRegistry {
        self.script_registry.clone()
    }
}

#[derive(Clone, Debug)]
struct BrowserHandleState {
    status: BrowserStatus,
    active_agent_id: Option<AgentId>,
    claim_depth: usize,
    active_scripts: HashMap<String, BrowserScriptSnapshot>,
}

#[derive(Clone, Debug)]
pub struct BrowserHandle {
    id: BrowserId,
    config: BrowserConfig,
    state: Arc<Mutex<BrowserHandleState>>,
    action_lock: Arc<Mutex<()>>,
    physical: BrowserPhysicalRegistries,
}

impl BrowserHandle {
    fn new(id: BrowserId, config: BrowserConfig) -> Self {
        Self {
            id,
            config,
            state: Arc::new(Mutex::new(BrowserHandleState {
                status: BrowserStatus::Created,
                active_agent_id: None,
                claim_depth: 0,
                active_scripts: HashMap::new(),
            })),
            action_lock: Arc::new(Mutex::new(())),
            physical: BrowserPhysicalRegistries::new(),
        }
    }

    pub fn id(&self) -> &BrowserId {
        &self.id
    }

    pub fn snapshot(&self) -> BrowserSnapshot {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut active_scripts = state.active_scripts.values().cloned().collect::<Vec<_>>();
        active_scripts.sort_by(|left, right| left.run_id.cmp(&right.run_id));
        BrowserSnapshot {
            id: self.id.clone(),
            config: self.config.clone(),
            status: state.status.clone(),
            active_agent_id: state.active_agent_id.clone(),
            active_scripts,
        }
    }

    pub fn physical_registries(&self) -> BrowserPhysicalRegistries {
        self.physical.clone()
    }

    fn claim(&self, agent_id: AgentId) -> Result<BrowserLease> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(active_agent_id) = state.active_agent_id.as_ref() {
            if active_agent_id != &agent_id {
                return Err(RuntimeError::BrowserAlreadyInUse {
                    browser_id: self.id.as_str().to_string(),
                    active_agent_id: active_agent_id.as_str().to_string(),
                }
                .into());
            }
            state.claim_depth = state.claim_depth.saturating_add(1);
            state.status = BrowserStatus::Claimed;
            return Ok(BrowserLease {
                browser_id: self.id.clone(),
                agent_id,
            });
        }
        state.active_agent_id = Some(agent_id.clone());
        state.status = BrowserStatus::Claimed;
        state.claim_depth = 1;
        Ok(BrowserLease {
            browser_id: self.id.clone(),
            agent_id,
        })
    }

    fn release(&self, agent_id: &AgentId) -> Result<()> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match state.active_agent_id.as_ref() {
            Some(active_agent_id) if active_agent_id == agent_id => {
                state.claim_depth = state.claim_depth.saturating_sub(1);
                if state.claim_depth == 0 {
                    state.active_agent_id = None;
                    state.status = BrowserStatus::Released;
                } else {
                    state.status = BrowserStatus::Claimed;
                }
                Ok(())
            }
            other => Err(RuntimeError::BrowserLeaseMismatch {
                browser_id: self.id.as_str().to_string(),
                owner_agent_id: other.map(|id| id.as_str().to_string()),
                caller_agent_id: agent_id.as_str().to_string(),
            }
            .into()),
        }
    }

    fn close(&self) -> Result<()> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(active_agent_id) = state.active_agent_id.as_ref() {
            bail!(
                "browser {} is still claimed by agent {}",
                self.id,
                active_agent_id
            );
        }
        state.status = BrowserStatus::Closed;
        state.active_scripts.clear();
        Ok(())
    }

    fn with_action_lock<T>(&self, action: impl FnOnce() -> Result<T>) -> Result<T> {
        let _guard = self
            .action_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        action()
    }

    fn record_script_started(&self, script: BrowserScriptSnapshot) {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .active_scripts
            .insert(script.run_id.clone(), script);
    }

    fn record_script_output_delta(&self, run_id: &str, delta: Option<String>) {
        let Some(delta) = delta else {
            return;
        };
        if let Some(script) = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .active_scripts
            .get_mut(run_id)
        {
            script.last_delta = Some(delta);
        }
    }

    fn record_script_finished(&self, run_id: &str) {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .active_scripts
            .remove(run_id);
    }
}

#[derive(Default)]
pub struct BrowserManager {
    browsers: Mutex<HashMap<BrowserId, BrowserHandle>>,
}

impl BrowserManager {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn create_browser(&self, config: BrowserConfig) -> BrowserId {
        let id = BrowserId::new();
        self.insert_browser_unchecked(id.clone(), config);
        id
    }

    pub fn create_browser_with_id(
        &self,
        id: BrowserId,
        config: BrowserConfig,
    ) -> Result<BrowserId> {
        let mut browsers = self
            .browsers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if browsers.contains_key(&id) {
            bail!("browser {} already exists", id);
        }
        browsers.insert(id.clone(), BrowserHandle::new(id.clone(), config));
        Ok(id)
    }

    fn insert_browser_unchecked(&self, id: BrowserId, config: BrowserConfig) {
        self.browsers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(id.clone(), BrowserHandle::new(id, config));
    }

    pub fn snapshot(&self, browser_id: &BrowserId) -> Result<BrowserSnapshot> {
        let browsers = self
            .browsers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let handle = browsers
            .get(browser_id)
            .ok_or_else(|| RuntimeError::UnknownBrowser(browser_id.as_str().to_string()))?;
        Ok(handle.snapshot())
    }

    pub fn physical_registries(&self, browser_id: &BrowserId) -> Result<BrowserPhysicalRegistries> {
        let browsers = self
            .browsers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let handle = browsers
            .get(browser_id)
            .ok_or_else(|| RuntimeError::UnknownBrowser(browser_id.as_str().to_string()))?;
        Ok(handle.physical_registries())
    }

    pub fn validate_browser_claim(&self, browser_id: &BrowserId, agent_id: &AgentId) -> Result<()> {
        let snapshot = self.snapshot(browser_id)?;
        if let Some(active_agent_id) = snapshot.active_agent_id.as_ref() {
            if active_agent_id != agent_id {
                return Err(RuntimeError::BrowserAlreadyInUse {
                    browser_id: browser_id.as_str().to_string(),
                    active_agent_id: active_agent_id.as_str().to_string(),
                }
                .into());
            }
        }
        Ok(())
    }

    pub fn validate_browser_release(&self, lease: &BrowserLease) -> Result<()> {
        let snapshot = self.snapshot(&lease.browser_id)?;
        match snapshot.active_agent_id.as_ref() {
            Some(active_agent_id) if active_agent_id == &lease.agent_id => Ok(()),
            other => Err(RuntimeError::BrowserLeaseMismatch {
                browser_id: lease.browser_id.as_str().to_string(),
                owner_agent_id: other.map(|id| id.as_str().to_string()),
                caller_agent_id: lease.agent_id.as_str().to_string(),
            }
            .into()),
        }
    }

    pub fn validate_browser_close(&self, browser_id: &BrowserId) -> Result<()> {
        let snapshot = self.snapshot(browser_id)?;
        if let Some(active_agent_id) = snapshot.active_agent_id.as_ref() {
            bail!("browser {browser_id} is still claimed by agent {active_agent_id}");
        }
        Ok(())
    }

    pub fn claim_browser(&self, browser_id: &BrowserId, agent_id: AgentId) -> Result<BrowserLease> {
        let browsers = self
            .browsers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let handle = browsers
            .get(browser_id)
            .ok_or_else(|| RuntimeError::UnknownBrowser(browser_id.as_str().to_string()))?;
        handle.claim(agent_id)
    }

    pub fn release_browser(&self, lease: &BrowserLease) -> Result<()> {
        let browsers = self
            .browsers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let handle = browsers
            .get(&lease.browser_id)
            .ok_or_else(|| RuntimeError::UnknownBrowser(lease.browser_id.as_str().to_string()))?;
        handle.release(&lease.agent_id)
    }

    pub fn with_action_lock<T>(
        &self,
        browser_id: &BrowserId,
        action: impl FnOnce() -> Result<T>,
    ) -> Result<T> {
        let handle = {
            let browsers = self
                .browsers
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            browsers
                .get(browser_id)
                .ok_or_else(|| RuntimeError::UnknownBrowser(browser_id.as_str().to_string()))?
                .clone()
        };
        handle.with_action_lock(action)
    }

    pub fn close_browser(&self, browser_id: &BrowserId) -> Result<()> {
        let mut browsers = self
            .browsers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let handle = browsers
            .get(browser_id)
            .ok_or_else(|| RuntimeError::UnknownBrowser(browser_id.as_str().to_string()))?;
        handle.close()?;
        browsers.remove(browser_id);
        Ok(())
    }

    pub fn record_script_started(
        &self,
        browser_id: &BrowserId,
        script: BrowserScriptSnapshot,
    ) -> Result<()> {
        let browsers = self
            .browsers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let handle = browsers
            .get(browser_id)
            .ok_or_else(|| RuntimeError::UnknownBrowser(browser_id.as_str().to_string()))?;
        handle.record_script_started(script);
        Ok(())
    }

    pub fn record_script_output_delta(
        &self,
        browser_id: &BrowserId,
        run_id: &str,
        delta: Option<String>,
    ) -> Result<()> {
        let browsers = self
            .browsers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let handle = browsers
            .get(browser_id)
            .ok_or_else(|| RuntimeError::UnknownBrowser(browser_id.as_str().to_string()))?;
        handle.record_script_output_delta(run_id, delta);
        Ok(())
    }

    pub fn record_script_finished(&self, browser_id: &BrowserId, run_id: &str) -> Result<()> {
        let browsers = self
            .browsers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let handle = browsers
            .get(browser_id)
            .ok_or_else(|| RuntimeError::UnknownBrowser(browser_id.as_str().to_string()))?;
        handle.record_script_finished(run_id);
        Ok(())
    }

    pub fn snapshots(&self) -> Vec<BrowserSnapshot> {
        self.browsers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .map(BrowserHandle::snapshot)
            .collect()
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CapacityMode {
    StrictReject,
    Queue,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct QueuedSpawn {
    pub request_id: String,
    pub child_agent_id: AgentId,
    pub task_name: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum SpawnAdmission {
    Reserved(SpawnReservation),
    Queued(QueuedSpawn),
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SpawnReservation {
    pub child_agent_id: AgentId,
    pub open_spawned_agents: usize,
    pub max_open_spawned_agents: usize,
}

#[derive(Debug)]
struct SubagentSchedulerState {
    open_spawned_agents: HashMap<AgentId, String>,
    queued: VecDeque<QueuedSpawn>,
}

#[derive(Debug)]
pub struct SubagentScheduler {
    max_open_spawned_agents: usize,
    capacity_mode: CapacityMode,
    state: Mutex<SubagentSchedulerState>,
}

impl SubagentScheduler {
    pub fn new(max_concurrent_threads_per_session: usize, capacity_mode: CapacityMode) -> Self {
        Self {
            max_open_spawned_agents: max_concurrent_threads_per_session.saturating_sub(1),
            capacity_mode,
            state: Mutex::new(SubagentSchedulerState {
                open_spawned_agents: HashMap::new(),
                queued: VecDeque::new(),
            }),
        }
    }

    pub fn admit_spawn(
        &self,
        child_agent_id: AgentId,
        task_name: impl Into<String>,
    ) -> Result<SpawnAdmission> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.open_spawned_agents.len() < self.max_open_spawned_agents {
            let task_name = task_name.into();
            state
                .open_spawned_agents
                .insert(child_agent_id.clone(), task_name);
            return Ok(SpawnAdmission::Reserved(SpawnReservation {
                child_agent_id,
                open_spawned_agents: state.open_spawned_agents.len(),
                max_open_spawned_agents: self.max_open_spawned_agents,
            }));
        }

        match self.capacity_mode {
            CapacityMode::StrictReject => Err(RuntimeError::AgentLimitReached {
                limit: self.max_open_spawned_agents,
                open_spawned_agents: state.open_spawned_agents.len(),
            }
            .into()),
            CapacityMode::Queue => {
                let queued = QueuedSpawn {
                    request_id: Uuid::new_v4().simple().to_string(),
                    child_agent_id,
                    task_name: task_name.into(),
                };
                state.queued.push_back(queued.clone());
                Ok(SpawnAdmission::Queued(queued))
            }
        }
    }

    pub fn close_spawned_agent(&self, child_agent_id: &AgentId) -> Option<SpawnReservation> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.open_spawned_agents.remove(child_agent_id)?;
        if self.capacity_mode != CapacityMode::Queue {
            return None;
        }
        let queued = state.queued.pop_front()?;
        state
            .open_spawned_agents
            .insert(queued.child_agent_id.clone(), queued.task_name);
        Some(SpawnReservation {
            child_agent_id: queued.child_agent_id,
            open_spawned_agents: state.open_spawned_agents.len(),
            max_open_spawned_agents: self.max_open_spawned_agents,
        })
    }

    pub fn open_count(&self) -> usize {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .open_spawned_agents
            .len()
    }

    pub fn queued_count(&self) -> usize {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .queued
            .len()
    }

    fn materialize_open_spawned_agent(
        &self,
        child_agent_id: AgentId,
        task_name: impl Into<String>,
    ) {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .open_spawned_agents
            .entry(child_agent_id)
            .or_insert_with(|| task_name.into());
    }
}

#[derive(Default)]
struct MemoryJournalState {
    sessions: HashMap<String, SessionMeta>,
    events: HashMap<String, Vec<EventRecord>>,
    edges: HashMap<String, SpawnEdge>,
    next_seq: i64,
}

#[derive(Clone, Default)]
pub struct MemoryJournal {
    inner: Arc<Mutex<MemoryJournalState>>,
}

impl MemoryJournal {
    pub fn new() -> Self {
        Self::default()
    }
}

fn status_for_session_event(event_type: &str, payload: &Value) -> Option<SessionStatus> {
    match event_type {
        "session.input" | "session.followup" | "agent.mailbox_input" | "agent.run.started" => {
            Some(SessionStatus::Running)
        }
        "session.done" => Some(SessionStatus::Done),
        "session.failed" => Some(SessionStatus::Failed),
        "session.cancelled" => Some(SessionStatus::Cancelled),
        "agent.closed"
            if payload.get("cancelled_active_run").and_then(Value::as_bool) == Some(true) =>
        {
            Some(SessionStatus::Cancelled)
        }
        "session.status" => payload
            .get("status")
            .and_then(Value::as_str)
            .and_then(|status| status.parse().ok()),
        _ => None,
    }
}

impl JournalSink for MemoryJournal {
    fn append_runtime_event(&self, event: &RuntimeEvent) -> Result<JournalAppend> {
        let session_id = event
            .session_id
            .as_ref()
            .ok_or_else(|| anyhow!("memory journal runtime events require a session id for now"))?;
        self.append_session_event(
            session_id,
            event.event_type(),
            event.journal_payload(),
            event.durability,
        )
    }

    fn append_session_event(
        &self,
        session_id: &SessionId,
        event_type: &str,
        payload: Value,
        durability: Durability,
    ) -> Result<JournalAppend> {
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !inner.sessions.contains_key(session_id.as_str()) {
            bail!("unknown session id: {session_id}");
        }
        let status_update = status_for_session_event(event_type, &payload);
        inner.next_seq = inner.next_seq.saturating_add(1);
        let record = EventRecord {
            seq: inner.next_seq,
            id: Uuid::new_v4().simple().to_string(),
            session_id: session_id.as_str().to_string(),
            ts_ms: now_ms(),
            event_type: event_type.to_string(),
            payload,
        };
        inner
            .events
            .entry(session_id.as_str().to_string())
            .or_default()
            .push(record);
        if let Some(status) = status_update {
            if let Some(session) = inner.sessions.get_mut(session_id.as_str()) {
                session.status = status;
                session.updated_ms = now_ms();
            }
        }
        Ok(JournalAppend {
            seq: Some(inner.next_seq),
            durability,
        })
    }

    fn flush(&self) -> Result<()> {
        Ok(())
    }
}

impl JournalReader for MemoryJournal {
    fn load_session(&self, session_id: &SessionId) -> Result<Option<SessionMeta>> {
        let inner = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Ok(inner.sessions.get(session_id.as_str()).cloned())
    }

    fn list_sessions(&self) -> Result<Vec<SessionMeta>> {
        let inner = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Ok(inner.sessions.values().cloned().collect())
    }

    fn events_for_session(&self, session_id: &SessionId) -> Result<Vec<EventRecord>> {
        let inner = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Ok(inner
            .events
            .get(session_id.as_str())
            .cloned()
            .unwrap_or_default())
    }

    fn events_after_seq(&self, session_id: &SessionId, after_seq: i64) -> Result<Vec<EventRecord>> {
        Ok(self
            .events_for_session(session_id)?
            .into_iter()
            .filter(|event| event.seq > after_seq)
            .collect())
    }
}

impl LiveThreadPersistence for MemoryJournal {
    fn create_thread(&self, request: CreateThreadRequest) -> Result<SessionMeta> {
        let id = request.session_id.unwrap_or_default();
        let now = now_ms();
        let parent_session_id = request.parent_session_id.clone();
        let artifact_root = request
            .artifact_root
            .unwrap_or_else(|| request.cwd.join("artifacts").join(id.as_str()));
        let session = SessionMeta {
            id: id.as_str().to_string(),
            parent_id: parent_session_id.as_ref().map(|id| id.as_str().to_string()),
            cwd: request.cwd.display().to_string(),
            artifact_root: artifact_root.display().to_string(),
            status: browser_use_protocol::SessionStatus::Created,
            created_ms: now,
            updated_ms: now,
        };
        {
            let mut inner = self
                .inner
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if inner.sessions.contains_key(&session.id) {
                bail!("session already exists: {}", session.id);
            }
            inner.sessions.insert(session.id.clone(), session.clone());
            if let Some(parent_session_id) = parent_session_id {
                inner.edges.insert(
                    session.id.clone(),
                    SpawnEdge {
                        parent_session_id,
                        child_session_id: id.clone(),
                        status: SpawnEdgeStatus::Open,
                        path: request.agent_path,
                        nickname: request.nickname,
                        role: request.role,
                    },
                );
            }
        }
        let session_id = SessionId::from_string(session.id.clone())?;
        self.append_session_event(
            &session_id,
            "session.created",
            json!({}),
            Durability::Barrier,
        )?;
        Ok(session)
    }
}

impl StateIndex for MemoryJournal {
    fn open_spawn_edge(&self, mut edge: SpawnEdge) -> Result<()> {
        edge.status = SpawnEdgeStatus::Open;
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        inner
            .edges
            .insert(edge.child_session_id.as_str().to_string(), edge);
        Ok(())
    }

    fn close_spawn_edge(&self, child_session_id: &SessionId, _reason: &str) -> Result<()> {
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(edge) = inner.edges.get_mut(child_session_id.as_str()) {
            edge.status = SpawnEdgeStatus::Closed;
        }
        if let Some(session) = inner.sessions.get_mut(child_session_id.as_str()) {
            if session.status.is_active() {
                session.status = SessionStatus::Cancelled;
                session.updated_ms = now_ms();
            }
        }
        Ok(())
    }

    fn finish_spawn_edge(
        &self,
        child_session_id: &SessionId,
        status: SpawnEdgeStatus,
    ) -> Result<()> {
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(edge) = inner.edges.get_mut(child_session_id.as_str()) {
            edge.status = status;
        }
        Ok(())
    }

    fn list_children(&self, parent_session_id: &SessionId) -> Result<Vec<SpawnEdge>> {
        let inner = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Ok(inner
            .edges
            .values()
            .filter(|edge| edge.parent_session_id == *parent_session_id)
            .cloned()
            .collect())
    }

    fn list_descendants(&self, root_session_id: &SessionId) -> Result<Vec<SpawnEdge>> {
        let mut descendants = Vec::new();
        let mut frontier = vec![root_session_id.clone()];
        while let Some(parent) = frontier.pop() {
            for edge in self.list_children(&parent)? {
                frontier.push(edge.child_session_id.clone());
                descendants.push(edge);
            }
        }
        Ok(descendants)
    }
}

#[derive(Clone)]
pub struct SqliteJournal {
    store: Arc<Mutex<Store>>,
}

impl SqliteJournal {
    pub fn open(state_dir: impl AsRef<Path>) -> Result<Self> {
        Ok(Self {
            store: Arc::new(Mutex::new(Store::open(state_dir)?)),
        })
    }

    pub fn from_store(store: Store) -> Self {
        Self {
            store: Arc::new(Mutex::new(store)),
        }
    }

    pub fn shared_store(&self) -> Arc<Mutex<Store>> {
        Arc::clone(&self.store)
    }
}

impl JournalSink for SqliteJournal {
    fn append_runtime_event(&self, event: &RuntimeEvent) -> Result<JournalAppend> {
        let session_id = event.session_id.as_ref().context(
            "sqlite journal runtime events require a session id until global events exist",
        )?;
        self.append_session_event(
            session_id,
            event.event_type(),
            event.journal_payload(),
            event.durability,
        )
    }

    fn append_session_event(
        &self,
        session_id: &SessionId,
        event_type: &str,
        payload: Value,
        durability: Durability,
    ) -> Result<JournalAppend> {
        let store = self
            .store
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let record = store.append_event(session_id.as_str(), event_type, payload)?;
        Ok(JournalAppend {
            seq: Some(record.seq),
            durability,
        })
    }

    fn flush(&self) -> Result<()> {
        Ok(())
    }
}

impl JournalReader for SqliteJournal {
    fn load_session(&self, session_id: &SessionId) -> Result<Option<SessionMeta>> {
        let store = self
            .store
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        store.load_session(session_id.as_str())
    }

    fn list_sessions(&self) -> Result<Vec<SessionMeta>> {
        let store = self
            .store
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        store.list_sessions()
    }

    fn events_for_session(&self, session_id: &SessionId) -> Result<Vec<EventRecord>> {
        let store = self
            .store
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        store.events_for_session(session_id.as_str())
    }

    fn events_after_seq(&self, session_id: &SessionId, after_seq: i64) -> Result<Vec<EventRecord>> {
        let store = self
            .store
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        store.events_after_seq(session_id.as_str(), after_seq)
    }
}

impl LiveThreadPersistence for SqliteJournal {
    fn create_thread(&self, request: CreateThreadRequest) -> Result<SessionMeta> {
        let store = self
            .store
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match (
            request.session_id,
            request.parent_session_id,
            request.artifact_root,
        ) {
            (None, None, None) => store.create_session(None, &request.cwd),
            (None, Some(parent), None) => store.create_child_session(
                parent.as_str(),
                &request.cwd,
                request.agent_path.as_deref(),
                request.nickname.as_deref(),
                request.role.as_deref(),
            ),
            (None, None, Some(artifact_root)) => {
                store.create_session_with_artifact_root(None, &request.cwd, artifact_root)
            }
            (Some(child_id), Some(parent), None) => {
                if let Some(existing) = store.load_session(child_id.as_str())? {
                    return Ok(existing);
                }
                store.create_child_session_with_id(
                    parent.as_str(),
                    &request.cwd,
                    request.agent_path.as_deref(),
                    request.nickname.as_deref(),
                    request.role.as_deref(),
                    child_id.into_string(),
                )
            }
            (Some(_), None, _) => {
                bail!("sqlite journal cannot create caller-chosen root thread ids yet")
            }
            (Some(_), Some(_), Some(_)) => {
                bail!("sqlite journal cannot create child thread with custom artifact root yet")
            }
            (None, Some(_), Some(_)) => {
                bail!("sqlite journal cannot create child thread with custom artifact root yet")
            }
        }
    }
}

impl StateIndex for SqliteJournal {
    fn open_spawn_edge(&self, edge: SpawnEdge) -> Result<()> {
        let store = self
            .store
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        store
            .agent_summary_for_child(edge.child_session_id.as_str())?
            .with_context(|| {
                format!(
                    "sqlite spawn edge must be created with its child thread: {}",
                    edge.child_session_id
                )
            })?;
        store.set_child_agent_status(edge.child_session_id.as_str(), "open")
    }

    fn close_spawn_edge(&self, child_session_id: &SessionId, reason: &str) -> Result<()> {
        let store = self
            .store
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        store.close_child_agent(child_session_id.as_str(), reason)
    }

    fn finish_spawn_edge(
        &self,
        child_session_id: &SessionId,
        status: SpawnEdgeStatus,
    ) -> Result<()> {
        let store = self
            .store
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        store.set_child_agent_status(child_session_id.as_str(), status.as_store_status())
    }

    fn list_children(&self, parent_session_id: &SessionId) -> Result<Vec<SpawnEdge>> {
        let store = self
            .store
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        store
            .list_child_agents(parent_session_id.as_str())?
            .into_iter()
            .map(spawn_edge_from_agent_summary)
            .collect()
    }

    fn list_descendants(&self, root_session_id: &SessionId) -> Result<Vec<SpawnEdge>> {
        let mut descendants = Vec::new();
        let mut frontier = vec![root_session_id.clone()];
        while let Some(parent_session_id) = frontier.pop() {
            for edge in self.list_children(&parent_session_id)? {
                frontier.push(edge.child_session_id.clone());
                descendants.push(edge);
            }
        }
        Ok(descendants)
    }
}

fn spawn_edge_from_agent_summary(summary: AgentSummary) -> Result<SpawnEdge> {
    Ok(SpawnEdge {
        parent_session_id: SessionId::from_string(summary.parent_session_id)?,
        child_session_id: SessionId::from_string(summary.child_session_id)?,
        status: SpawnEdgeStatus::from_store_status(&summary.status),
        path: summary.agent_path,
        nickname: summary.agent_nickname,
        role: summary.agent_role,
    })
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MailboxDeliveryPhase {
    CurrentTurn,
    NextTurn,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MailboxItemKind {
    Completion,
    Input,
    Followup,
    Notification,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct MailboxItem {
    pub seq: u64,
    pub id: String,
    pub kind: MailboxItemKind,
    pub author_agent_id: AgentId,
    pub target_agent_id: AgentId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_path: Option<String>,
    pub content: String,
    pub trigger_turn: bool,
    pub delivery_phase: MailboxDeliveryPhase,
    #[serde(default)]
    pub payload: Value,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AgentTarget {
    Any,
    AgentId(AgentId),
    Path(String),
}

#[derive(Clone, Debug, PartialEq)]
pub enum WaitAgentOutcome {
    Completed(MailboxItem),
    TimedOut,
}

struct AgentMailboxState {
    next_seq: u64,
    queue: VecDeque<MailboxItem>,
}

pub struct AgentMailbox {
    seq_tx: watch::Sender<u64>,
    state: Mutex<AgentMailboxState>,
}

impl Default for AgentMailbox {
    fn default() -> Self {
        let (seq_tx, _) = watch::channel(0);
        Self {
            seq_tx,
            state: Mutex::new(AgentMailboxState {
                next_seq: 0,
                queue: VecDeque::new(),
            }),
        }
    }
}

impl AgentMailbox {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn subscribe(&self) -> watch::Receiver<u64> {
        self.seq_tx.subscribe()
    }

    pub fn prepare_item(&self, mut item: MailboxItem) -> MailboxItem {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.next_seq = state.next_seq.saturating_add(1);
        item.seq = state.next_seq;
        item
    }

    pub fn enqueue(&self, item: MailboxItem) -> u64 {
        let item = self.prepare_item(item);
        self.enqueue_prepared(item)
    }

    pub fn enqueue_prepared(&self, item: MailboxItem) -> u64 {
        let seq = item.seq;
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.next_seq = state.next_seq.max(seq);
        state.queue.push_back(item);
        drop(state);
        let _ = self.seq_tx.send(seq);
        seq
    }

    fn materialize_pending(&self, items: Vec<MailboxItem>) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.next_seq = state
            .next_seq
            .max(items.iter().map(|item| item.seq).max().unwrap_or_default());
        state.queue = items.into();
        let _ = self.seq_tx.send(state.next_seq);
    }

    pub fn pending_items(&self) -> Vec<MailboxItem> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .queue
            .iter()
            .cloned()
            .collect()
    }

    pub fn has_pending_completion_for(&self, target: &AgentTarget) -> Option<MailboxItem> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .queue
            .iter()
            .find(|item| item.kind == MailboxItemKind::Completion && target_matches(item, target))
            .cloned()
    }

    pub fn has_pending_item_for(&self, target: &AgentTarget) -> Option<MailboxItem> {
        self.has_pending_item_after(target, 0)
    }

    pub fn has_pending_item_after(
        &self,
        target: &AgentTarget,
        after_seq: u64,
    ) -> Option<MailboxItem> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .queue
            .iter()
            .find(|item| item.seq > after_seq && target_matches(item, target))
            .cloned()
    }

    pub fn has_pending_phase(&self, delivery_phase: MailboxDeliveryPhase) -> bool {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .queue
            .iter()
            .any(|item| item.delivery_phase == delivery_phase)
    }

    pub fn pending_items_for_phase(
        &self,
        delivery_phase: MailboxDeliveryPhase,
    ) -> Vec<MailboxItem> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .queue
            .iter()
            .filter(|item| item.delivery_phase == delivery_phase)
            .cloned()
            .collect()
    }

    pub fn has_pending_trigger_turn_phase(&self, delivery_phase: MailboxDeliveryPhase) -> bool {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .queue
            .iter()
            .any(|item| item.delivery_phase == delivery_phase && item.trigger_turn)
    }

    pub fn drain_phase(&self, delivery_phase: MailboxDeliveryPhase) -> Vec<MailboxItem> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut kept = VecDeque::with_capacity(state.queue.len());
        let mut drained = Vec::new();
        while let Some(item) = state.queue.pop_front() {
            if item.delivery_phase == delivery_phase {
                drained.push(item);
            } else {
                kept.push_back(item);
            }
        }
        state.queue = kept;
        drained
    }

    pub async fn wait_for_completion(
        &self,
        target: AgentTarget,
        timeout_duration: Duration,
    ) -> Result<WaitAgentOutcome> {
        if let Some(item) = self.has_pending_completion_for(&target) {
            return Ok(WaitAgentOutcome::Completed(item));
        }
        let mut rx = self.subscribe();
        let wait = async {
            loop {
                rx.changed().await.map_err(|_| anyhow!("mailbox closed"))?;
                if let Some(item) = self.has_pending_completion_for(&target) {
                    return Ok(WaitAgentOutcome::Completed(item));
                }
            }
        };
        match tokio::time::timeout(timeout_duration, wait).await {
            Ok(outcome) => outcome,
            Err(_) => Ok(WaitAgentOutcome::TimedOut),
        }
    }

    pub async fn wait_for_item(
        &self,
        target: AgentTarget,
        after_seq: u64,
        timeout_duration: Duration,
    ) -> Result<WaitAgentOutcome> {
        if let Some(item) = self.has_pending_item_after(&target, after_seq) {
            return Ok(WaitAgentOutcome::Completed(item));
        }
        let mut rx = self.subscribe();
        let wait = async {
            loop {
                rx.changed().await.map_err(|_| anyhow!("mailbox closed"))?;
                if let Some(item) = self.has_pending_item_after(&target, after_seq) {
                    return Ok(WaitAgentOutcome::Completed(item));
                }
            }
        };
        match tokio::time::timeout(timeout_duration, wait).await {
            Ok(outcome) => outcome,
            Err(_) => Ok(WaitAgentOutcome::TimedOut),
        }
    }
}

fn target_matches(item: &MailboxItem, target: &AgentTarget) -> bool {
    match target {
        AgentTarget::Any => true,
        AgentTarget::AgentId(agent_id) => item.author_agent_id == *agent_id,
        AgentTarget::Path(path) => item.target_path.as_deref() == Some(path.as_str()),
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentThreadStatus {
    Created,
    Queued,
    Running,
    Cancelling,
    Completed,
    Failed,
    Cancelled,
    Closed,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct RuntimeActivitySnapshot {
    pub key: String,
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<ToolCallId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_delta: Option<String>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct RuntimeModelRequestSnapshot {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_idx: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attempt: Option<u64>,
    pub status: String,
    pub retry_count: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct AgentLiveStateSnapshot {
    pub accepted_input_count: usize,
    pub accepted_followup_count: usize,
    pub pending_prompt_input_count: usize,
    pub last_accepted_prompt_input_seq: i64,
    pub last_consumed_prompt_input_seq: i64,
    pub pending_mailbox_count: usize,
    pub pending_trigger_turn_count: usize,
    pub last_enqueued_mailbox_seq: u64,
    pub last_wait_observed_mailbox_seq: u64,
    pub last_delivered_mailbox_seq: u64,
    pub last_consumed_mailbox_seq: u64,
    pub current_run_id: Option<RunId>,
    pub cancellation_requested: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub active_items: Vec<RuntimeActivitySnapshot>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_model_delta: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_model_thinking_delta: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_model_request: Option<RuntimeModelRequestSnapshot>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_token_usage: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_token_usage: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_context_window: Option<i64>,
    pub compaction_window_ordinal: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compaction_prefill_input_tokens: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compaction_prefill_source: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub final_result: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure: Option<String>,
}

impl Default for AgentLiveStateSnapshot {
    fn default() -> Self {
        Self {
            accepted_input_count: 0,
            accepted_followup_count: 0,
            pending_prompt_input_count: 0,
            last_accepted_prompt_input_seq: 0,
            last_consumed_prompt_input_seq: 0,
            pending_mailbox_count: 0,
            pending_trigger_turn_count: 0,
            last_enqueued_mailbox_seq: 0,
            last_wait_observed_mailbox_seq: 0,
            last_delivered_mailbox_seq: 0,
            last_consumed_mailbox_seq: 0,
            current_run_id: None,
            cancellation_requested: false,
            active_items: Vec::new(),
            last_model_delta: None,
            last_model_thinking_delta: None,
            active_model_request: None,
            last_token_usage: None,
            total_token_usage: None,
            model_context_window: None,
            compaction_window_ordinal: 1,
            compaction_prefill_input_tokens: None,
            compaction_prefill_source: None,
            final_result: None,
            failure: None,
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct AgentThreadSnapshot {
    pub agent_id: AgentId,
    pub session_id: SessionId,
    pub root_id: RootId,
    pub cwd: PathBuf,
    pub parent_agent_id: Option<AgentId>,
    pub parent_session_id: Option<SessionId>,
    pub agent_path: String,
    pub nickname: Option<String>,
    pub role: Option<String>,
    pub status: AgentThreadStatus,
    pub live: AgentLiveStateSnapshot,
}

type AgentResourceCleanup = Arc<dyn Fn(Arc<dyn Any + Send + Sync>) -> usize + Send + Sync>;

struct AgentResourceSlot {
    handle: Arc<dyn Any + Send + Sync>,
    cleanup: Option<AgentResourceCleanup>,
}

#[derive(Default)]
pub struct ToolResourceBag {
    slots: Mutex<HashMap<String, AgentResourceSlot>>,
}

pub type AgentResourceSet = ToolResourceBag;

impl ToolResourceBag {
    pub fn get_or_insert_with<T, F, C>(&self, key: &str, init: F, cleanup: C) -> Result<Arc<T>>
    where
        T: Any + Send + Sync + 'static,
        F: FnOnce() -> T,
        C: Fn(Arc<T>) -> usize + Send + Sync + 'static,
    {
        self.try_get_or_insert_with(key, || Ok(init()), cleanup)
    }

    pub fn try_get_or_insert_with<T, F, C>(&self, key: &str, init: F, cleanup: C) -> Result<Arc<T>>
    where
        T: Any + Send + Sync + 'static,
        F: FnOnce() -> Result<T>,
        C: Fn(Arc<T>) -> usize + Send + Sync + 'static,
    {
        let mut slots = self
            .slots
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(slot) = slots.get(key) {
            return Arc::downcast::<T>(Arc::clone(&slot.handle)).map_err(|_| {
                anyhow!("runtime resource `{key}` has a different concrete type than requested")
            });
        }

        let typed = Arc::new(init()?);
        let erased: Arc<dyn Any + Send + Sync> = typed.clone();
        let cleanup: AgentResourceCleanup = Arc::new(move |handle| {
            Arc::downcast::<T>(handle)
                .map(|typed| cleanup(typed))
                .unwrap_or_default()
        });
        slots.insert(
            key.to_string(),
            AgentResourceSlot {
                handle: erased,
                cleanup: Some(cleanup),
            },
        );
        Ok(typed)
    }

    pub fn get<T>(&self, key: &str) -> Result<Option<Arc<T>>>
    where
        T: Any + Send + Sync + 'static,
    {
        let slots = self
            .slots
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(slot) = slots.get(key) else {
            return Ok(None);
        };
        Arc::downcast::<T>(Arc::clone(&slot.handle))
            .map(Some)
            .map_err(|_| anyhow!("runtime resource `{key}` has a different concrete type"))
    }

    pub fn cleanup_all(&self) -> usize {
        let slots = self
            .slots
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .drain()
            .map(|(_, slot)| slot)
            .collect::<Vec<_>>();
        slots
            .into_iter()
            .map(|slot| {
                slot.cleanup
                    .map(|cleanup| cleanup(slot.handle))
                    .unwrap_or_default()
            })
            .sum()
    }
}

#[derive(Default)]
struct AgentLiveState {
    inner: Mutex<AgentLiveStateSnapshot>,
}

impl AgentLiveState {
    fn snapshot(&self) -> AgentLiveStateSnapshot {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    fn begin_run(&self, run_id: RunId) {
        let mut state = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.current_run_id = Some(run_id);
        state.cancellation_requested = false;
    }

    fn request_cancel(&self) {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .cancellation_requested = true;
    }

    fn finish_run(&self) {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .current_run_id = None;
    }

    fn close(&self) {
        let mut state = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.current_run_id = None;
    }

    fn record_accepted_input(&self, item: &MailboxItem) {
        let mut state = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match item.kind {
            MailboxItemKind::Followup => {
                state.accepted_followup_count = state.accepted_followup_count.saturating_add(1);
            }
            MailboxItemKind::Input => {
                state.accepted_input_count = state.accepted_input_count.saturating_add(1);
            }
            MailboxItemKind::Completion | MailboxItemKind::Notification => {}
        }
    }

    fn has_accepted_prompt_input_seq(&self, source_event_seq: Option<i64>) -> bool {
        let Some(source_event_seq) = source_event_seq else {
            return false;
        };
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .last_accepted_prompt_input_seq
            >= source_event_seq
    }

    fn record_prompt_input_accepted(&self, source_event_seq: Option<i64>) {
        let mut state = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.accepted_input_count = state.accepted_input_count.saturating_add(1);
        state.pending_prompt_input_count = state.pending_prompt_input_count.saturating_add(1);
        if let Some(source_event_seq) = source_event_seq {
            state.last_accepted_prompt_input_seq =
                state.last_accepted_prompt_input_seq.max(source_event_seq);
        }
    }

    fn has_pending_prompt_input(&self) -> bool {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .pending_prompt_input_count
            > 0
    }

    fn record_prompt_input_consumed(&self) -> bool {
        let mut state = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.pending_prompt_input_count == 0 {
            return false;
        }
        state.pending_prompt_input_count = state.pending_prompt_input_count.saturating_sub(1);
        state.last_consumed_prompt_input_seq = state
            .last_consumed_prompt_input_seq
            .max(state.last_accepted_prompt_input_seq);
        true
    }

    fn record_mailbox_enqueued(&self, item: &MailboxItem) {
        let mut state = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.pending_mailbox_count = state.pending_mailbox_count.saturating_add(1);
        if item.trigger_turn {
            state.pending_trigger_turn_count = state.pending_trigger_turn_count.saturating_add(1);
        }
        state.last_enqueued_mailbox_seq = state.last_enqueued_mailbox_seq.max(item.seq);
    }

    fn record_mailbox_delivered(&self, items: &[MailboxItem]) {
        let max_seq = items.iter().map(|item| item.seq).max().unwrap_or_default();
        let mut state = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.last_delivered_mailbox_seq = state.last_delivered_mailbox_seq.max(max_seq);
    }

    fn record_wait_observed_mailbox(&self, item: &MailboxItem) {
        let mut state = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.last_wait_observed_mailbox_seq = state.last_wait_observed_mailbox_seq.max(item.seq);
    }

    fn record_mailbox_consumed(&self, items: &[MailboxItem]) {
        let max_seq = items.iter().map(|item| item.seq).max().unwrap_or_default();
        let trigger_count = items.iter().filter(|item| item.trigger_turn).count();
        let mut state = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.pending_mailbox_count = state.pending_mailbox_count.saturating_sub(items.len());
        state.pending_trigger_turn_count = state
            .pending_trigger_turn_count
            .saturating_sub(trigger_count);
        state.last_consumed_mailbox_seq = state.last_consumed_mailbox_seq.max(max_seq);
    }

    fn compaction_prefill_input_tokens(&self) -> Option<i64> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .compaction_prefill_input_tokens
    }

    fn record_estimated_compaction_prefill(&self, tokens: i64) {
        let mut state = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.compaction_prefill_source.as_deref() == Some("server_observed") {
            return;
        }
        state.compaction_prefill_input_tokens = Some(tokens.max(0));
        state.compaction_prefill_source = Some("estimated".to_string());
    }

    fn record_server_observed_compaction_prefill(&self, tokens: i64) {
        let mut state = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.compaction_prefill_source.as_deref() == Some("server_observed") {
            return;
        }
        state.compaction_prefill_input_tokens = Some(tokens.max(0));
        state.compaction_prefill_source = Some("server_observed".to_string());
    }

    fn start_next_compaction_window(&self) {
        let mut state = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.compaction_window_ordinal = state.compaction_window_ordinal.max(1).saturating_add(1);
        state.compaction_prefill_input_tokens = None;
        state.compaction_prefill_source = None;
    }

    fn materialize_from_replay(&self, replay: &MaterializedLiveState) {
        let mut state = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.accepted_input_count = replay.accepted_prompt_input_count;
        state.pending_prompt_input_count = replay.pending_prompt_input_count;
        state.last_accepted_prompt_input_seq = replay.last_accepted_prompt_input_seq;
        state.last_consumed_prompt_input_seq = replay.last_consumed_prompt_input_seq;
        state.pending_mailbox_count = replay.pending_mailbox_items.len();
        state.pending_trigger_turn_count = replay
            .pending_mailbox_items
            .iter()
            .filter(|item| item.trigger_turn)
            .count();
        state.last_enqueued_mailbox_seq = replay.last_enqueued_mailbox_seq;
        state.last_wait_observed_mailbox_seq = replay.last_wait_observed_mailbox_seq;
        state.last_delivered_mailbox_seq = replay.last_delivered_mailbox_seq;
        state.last_consumed_mailbox_seq = replay.last_consumed_mailbox_seq;
    }
}

pub struct AgentThread {
    agent_id: AgentId,
    session_id: SessionId,
    root_id: RootId,
    cwd: PathBuf,
    parent_agent_id: Option<AgentId>,
    parent_session_id: Option<SessionId>,
    agent_path: String,
    nickname: Option<String>,
    role: Option<String>,
    status_tx: watch::Sender<AgentThreadStatus>,
    mailbox: AgentMailbox,
    resources: ToolResourceBag,
    live_state: AgentLiveState,
}

impl AgentThread {
    fn new(
        agent_id: AgentId,
        session_id: SessionId,
        root_id: RootId,
        cwd: PathBuf,
        parent_agent_id: Option<AgentId>,
        parent_session_id: Option<SessionId>,
        agent_path: String,
        nickname: Option<String>,
        role: Option<String>,
    ) -> Self {
        let (status_tx, _) = watch::channel(AgentThreadStatus::Created);
        Self {
            agent_id,
            session_id,
            root_id,
            cwd,
            parent_agent_id,
            parent_session_id,
            agent_path,
            nickname,
            role,
            status_tx,
            mailbox: AgentMailbox::new(),
            resources: ToolResourceBag::default(),
            live_state: AgentLiveState::default(),
        }
    }

    pub fn agent_id(&self) -> &AgentId {
        &self.agent_id
    }

    pub fn session_id(&self) -> &SessionId {
        &self.session_id
    }

    pub fn mailbox(&self) -> &AgentMailbox {
        &self.mailbox
    }

    pub fn resources(&self) -> &ToolResourceBag {
        &self.resources
    }

    pub fn live_state_snapshot(&self) -> AgentLiveStateSnapshot {
        self.live_state.snapshot()
    }

    pub fn set_status(&self, status: AgentThreadStatus) {
        self.status_tx.send_replace(status);
    }

    pub fn snapshot(&self) -> AgentThreadSnapshot {
        AgentThreadSnapshot {
            agent_id: self.agent_id.clone(),
            session_id: self.session_id.clone(),
            root_id: self.root_id.clone(),
            cwd: self.cwd.clone(),
            parent_agent_id: self.parent_agent_id.clone(),
            parent_session_id: self.parent_session_id.clone(),
            agent_path: self.agent_path.clone(),
            nickname: self.nickname.clone(),
            role: self.role.clone(),
            status: self.status_tx.borrow().clone(),
            live: self.live_state.snapshot(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct CreateRootAgentRequest {
    pub cwd: PathBuf,
    pub task: String,
    pub max_concurrent_threads_per_session: usize,
}

#[derive(Clone, Debug)]
pub struct AttachRootAgentRequest {
    pub session_id: SessionId,
    pub cwd: PathBuf,
    pub task: String,
    pub max_concurrent_threads_per_session: usize,
}

#[derive(Clone, Debug)]
pub struct AttachChildAgentRequest {
    pub parent_agent_id: AgentId,
    pub child_agent_id: AgentId,
    pub child_session_id: SessionId,
    pub cwd: PathBuf,
    pub agent_path: String,
    pub nickname: Option<String>,
    pub role: Option<String>,
}

#[derive(Clone, Debug)]
pub struct SpawnChildRequest {
    pub parent_agent_id: AgentId,
    pub child_agent_id: Option<AgentId>,
    pub child_session_id: Option<SessionId>,
    pub task_name: String,
    pub message: String,
    pub nickname: Option<String>,
    pub role: Option<String>,
}

#[derive(Clone, Debug)]
pub struct CompleteAgentRequest {
    pub child_agent_id: AgentId,
    pub result: String,
}

#[derive(Clone, Debug)]
pub struct FailAgentRequest {
    pub child_agent_id: AgentId,
    pub error: String,
}

#[derive(Clone, Debug)]
pub struct CloseAgentRequest {
    pub agent_id: AgentId,
    pub reason: String,
}

#[derive(Clone, Debug)]
pub struct SendAgentMessageRequest {
    pub author_agent_id: AgentId,
    pub target_agent_id: AgentId,
    pub content: String,
    pub trigger_turn: bool,
    pub kind: MailboxItemKind,
    pub delivery_phase: MailboxDeliveryPhase,
    pub payload: Value,
}

#[derive(Clone, Debug)]
pub struct SendAgentMessageResponse {
    pub mailbox_item: MailboxItem,
}

#[derive(Clone, Debug)]
pub struct AcceptPromptInputRequest {
    pub target_agent_id: AgentId,
    pub source_event_seq: Option<i64>,
    pub payload: Value,
}

#[derive(Clone, Debug)]
pub struct AcceptPromptInputResponse {
    pub accepted: bool,
}

#[derive(Clone, Debug)]
pub struct ConsumePromptInputResponse {
    pub consumed: bool,
}

#[derive(Clone, Debug)]
pub struct SubmitInputRequest {
    pub target_agent_id: AgentId,
    pub content: String,
    pub trigger_turn: bool,
    pub delivery_phase: MailboxDeliveryPhase,
    pub input_items: Option<Value>,
    pub payload: Value,
}

#[derive(Clone, Debug)]
pub struct SubmitInputResponse {
    pub mailbox_item: MailboxItem,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "method", rename_all = "snake_case")]
pub enum LocalRuntimeRequest {
    Ping,
    PendingAgentMail {
        session_id: String,
    },
    SpawnChild {
        parent_agent_id: String,
        #[serde(default)]
        child_agent_id: Option<String>,
        #[serde(default)]
        child_session_id: Option<String>,
        task_name: String,
        message: String,
        #[serde(default)]
        nickname: Option<String>,
        #[serde(default)]
        role: Option<String>,
    },
    SendAgentMessage {
        author_agent_id: String,
        target_agent_id: String,
        content: String,
        trigger_turn: bool,
        kind: MailboxItemKind,
        delivery_phase: MailboxDeliveryPhase,
        #[serde(default)]
        payload: Value,
    },
    SubmitUserInput {
        session_id: String,
        content: String,
        trigger_turn: bool,
        delivery_phase: MailboxDeliveryPhase,
        #[serde(default)]
        input_items: Option<Value>,
        #[serde(default)]
        payload: Value,
    },
    WaitAgent {
        parent_agent_id: String,
        #[serde(default)]
        target: Option<LocalRuntimeWaitTarget>,
        timeout_ms: u64,
    },
    CancelRun {
        session_id: String,
    },
    CloseAgent {
        agent_id: String,
        reason: String,
    },
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum LocalRuntimeWaitTarget {
    Any,
    AgentId(String),
    Path(String),
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct LocalRuntimeResponse {
    pub ok: bool,
    #[serde(default)]
    pub result: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Clone, Debug)]
pub struct DrainAgentMailboxRequest {
    pub session_id: SessionId,
    pub delivery_phase: MailboxDeliveryPhase,
}

#[derive(Clone, Debug)]
pub struct DrainAgentMailboxResponse {
    pub mailbox_items: Vec<MailboxItem>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct AgentControlSnapshot {
    pub root_id: RootId,
    pub root_agent_id: AgentId,
    pub root_session_id: SessionId,
    pub open_spawned_agents: usize,
    pub queued_spawns: usize,
}

pub struct AgentControl {
    root_id: RootId,
    root_agent_id: AgentId,
    root_session_id: SessionId,
    scheduler: SubagentScheduler,
}

impl AgentControl {
    fn new(
        root_id: RootId,
        root_agent_id: AgentId,
        root_session_id: SessionId,
        max_concurrent_threads_per_session: usize,
    ) -> Self {
        Self {
            root_id,
            root_agent_id,
            root_session_id,
            scheduler: SubagentScheduler::new(
                max_concurrent_threads_per_session,
                CapacityMode::StrictReject,
            ),
        }
    }

    pub fn snapshot(&self) -> AgentControlSnapshot {
        AgentControlSnapshot {
            root_id: self.root_id.clone(),
            root_agent_id: self.root_agent_id.clone(),
            root_session_id: self.root_session_id.clone(),
            open_spawned_agents: self.scheduler.open_count(),
            queued_spawns: self.scheduler.queued_count(),
        }
    }
}

#[derive(Default)]
pub struct AgentManager {
    threads: Mutex<HashMap<AgentId, Arc<AgentThread>>>,
    session_to_agent: Mutex<HashMap<SessionId, AgentId>>,
    controls: Mutex<HashMap<RootId, Arc<AgentControl>>>,
}

#[derive(Default)]
pub struct ActiveRunRegistry {
    tokens: Mutex<HashMap<SessionId, CancellationToken>>,
}

impl ActiveRunRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&self, session_id: SessionId) -> CancellationToken {
        let token = CancellationToken::new();
        self.register_with_token(session_id, token.clone());
        token
    }

    pub fn register_with_token(
        &self,
        session_id: SessionId,
        token: CancellationToken,
    ) -> CancellationToken {
        self.tokens
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(session_id, token.clone());
        token
    }

    pub fn unregister(&self, session_id: &SessionId) {
        self.tokens
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(session_id);
    }

    pub fn token(&self, session_id: &SessionId) -> Option<CancellationToken> {
        self.tokens
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(session_id)
            .cloned()
    }

    pub fn cancel(&self, session_id: &SessionId) -> bool {
        let Some(token) = self.token(session_id) else {
            return false;
        };
        token.cancel();
        true
    }
}

struct ActiveRuntimeRunGuard {
    runtime: Arc<BrowserUseRuntime>,
    session_id: SessionId,
}

impl Drop for ActiveRuntimeRunGuard {
    fn drop(&mut self) {
        self.runtime.active_runs.unregister(&self.session_id);
    }
}

impl AgentManager {
    pub fn new() -> Self {
        Self::default()
    }

    fn insert_thread(&self, thread: Arc<AgentThread>) {
        self.session_to_agent
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(thread.session_id.clone(), thread.agent_id.clone());
        self.threads
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(thread.agent_id.clone(), thread);
    }

    fn insert_control(&self, control: Arc<AgentControl>) {
        self.controls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(control.root_id.clone(), control);
    }

    pub fn thread(&self, agent_id: &AgentId) -> Result<Arc<AgentThread>> {
        self.threads
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(agent_id)
            .cloned()
            .ok_or_else(|| RuntimeError::UnknownAgent(agent_id.as_str().to_string()).into())
    }

    pub fn thread_for_session(&self, session_id: &SessionId) -> Result<Arc<AgentThread>> {
        let agent_id = self
            .session_to_agent
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(session_id)
            .cloned()
            .ok_or_else(|| RuntimeError::UnknownAgent(session_id.as_str().to_string()))?;
        self.thread(&agent_id)
    }

    pub fn control_for_thread(&self, thread: &AgentThread) -> Result<Arc<AgentControl>> {
        self.controls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&thread.root_id)
            .cloned()
            .ok_or_else(|| RuntimeError::UnknownAgent(thread.agent_id.as_str().to_string()).into())
    }

    pub fn snapshots(&self) -> Vec<AgentThreadSnapshot> {
        self.threads
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .map(|thread| thread.snapshot())
            .collect()
    }

    pub fn control_snapshots(&self) -> Vec<AgentControlSnapshot> {
        self.controls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .map(|control| control.snapshot())
            .collect()
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProjectedEventKind {
    ThreadStatusChanged,
    TurnStarted,
    TurnCompleted,
    ItemStarted,
    ItemCompleted,
    AgentMessageDelta,
    CommandOutputDelta,
    McpOutputDelta,
    ToolUpdated,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ProjectedEvent {
    pub source_event_id: EventId,
    pub kind: ProjectedEventKind,
    pub session_id: Option<SessionId>,
    pub payload: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot: Option<RuntimeSnapshot>,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct RuntimeSnapshot {
    pub agents: Vec<AgentThreadSnapshot>,
    pub agent_controls: Vec<AgentControlSnapshot>,
    pub browsers: Vec<BrowserSnapshot>,
}

pub struct ProjectedRuntimeSubscription {
    projection: RuntimeProjectionState,
    rx: broadcast::Receiver<RuntimeEvent>,
}

impl ProjectedRuntimeSubscription {
    pub fn snapshot(&self) -> &RuntimeSnapshot {
        self.projection.snapshot()
    }

    pub async fn recv(&mut self) -> Result<ProjectedEvent> {
        loop {
            match self.rx.recv().await {
                Ok(event) => return Ok(self.projection.apply_event(&event)),
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => bail!("runtime event bus closed"),
            }
        }
    }
}

pub struct ProjectedAgentSubscription {
    agent_id: AgentId,
    snapshot: AgentThreadSnapshot,
    projection: RuntimeProjectionState,
    rx: broadcast::Receiver<RuntimeEvent>,
}

impl ProjectedAgentSubscription {
    pub fn snapshot(&self) -> &AgentThreadSnapshot {
        &self.snapshot
    }

    pub async fn recv(&mut self) -> Result<ProjectedEvent> {
        loop {
            match self.rx.recv().await {
                Ok(event) if event.agent_id.as_ref() == Some(&self.agent_id) => {
                    let projected = self.projection.apply_event(&event);
                    if let Some(snapshot) = self
                        .projection
                        .snapshot()
                        .agents
                        .iter()
                        .find(|agent| agent.agent_id == self.agent_id)
                    {
                        self.snapshot = snapshot.clone();
                    }
                    return Ok(projected);
                }
                Ok(event) => {
                    self.projection.apply_event(&event);
                    continue;
                }
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => bail!("runtime event bus closed"),
            }
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct RuntimeProjectionState {
    snapshot: RuntimeSnapshot,
}

impl RuntimeProjectionState {
    pub fn new(snapshot: RuntimeSnapshot) -> Self {
        Self { snapshot }
    }

    pub fn snapshot(&self) -> &RuntimeSnapshot {
        &self.snapshot
    }

    pub fn apply_event(&mut self, event: &RuntimeEvent) -> ProjectedEvent {
        self.reduce(event);
        let mut projected = RuntimeEventProjection::project(event);
        projected.snapshot = Some(self.snapshot.clone());
        projected
    }

    fn reduce(&mut self, event: &RuntimeEvent) {
        self.reduce_agent(event);
        self.reduce_live_activity(event);
        self.reduce_browser(event);
    }

    fn reduce_agent(&mut self, event: &RuntimeEvent) {
        match event.kind {
            RuntimeEventKind::AgentCreated | RuntimeEventKind::AgentResumed => {
                self.upsert_agent_from_event(event, None);
            }
            RuntimeEventKind::SubagentSpawnStarted => {
                self.upsert_spawned_child(event);
            }
            RuntimeEventKind::AgentInputAccepted => {
                if let Some(agent) = self.agent_snapshot_mut(event) {
                    agent.live.accepted_input_count =
                        agent.live.accepted_input_count.saturating_add(1);
                }
            }
            RuntimeEventKind::AgentInputConsumed => {
                if let Some(agent) = self.agent_snapshot_mut(event) {
                    agent.live.pending_prompt_input_count =
                        agent.live.pending_prompt_input_count.saturating_sub(1);
                }
            }
            RuntimeEventKind::AgentCancelRequested | RuntimeEventKind::AgentCloseRequested => {
                if let Some(agent) = self.agent_snapshot_mut(event) {
                    agent.live.cancellation_requested = true;
                }
            }
            RuntimeEventKind::AgentStarted
            | RuntimeEventKind::AgentQueued
            | RuntimeEventKind::AgentCompleted
            | RuntimeEventKind::AgentFailed
            | RuntimeEventKind::AgentCancelled
            | RuntimeEventKind::AgentClosed
            | RuntimeEventKind::AgentContinuationStarted
            | RuntimeEventKind::AgentTurnStarted
            | RuntimeEventKind::AgentTurnCompleted
            | RuntimeEventKind::AgentTurnAborted => {
                if let Some(status) = projected_thread_status_for_event(event) {
                    if let Some(agent) = self.agent_snapshot_mut(event) {
                        agent.status = status;
                        match event.kind {
                            RuntimeEventKind::AgentStarted
                            | RuntimeEventKind::AgentContinuationStarted
                            | RuntimeEventKind::AgentTurnStarted => {
                                if let Some(run_id) = event.run_id.clone() {
                                    agent.live.current_run_id = Some(run_id);
                                }
                                agent.live.cancellation_requested = false;
                            }
                            RuntimeEventKind::AgentCompleted
                            | RuntimeEventKind::AgentFailed
                            | RuntimeEventKind::AgentCancelled
                            | RuntimeEventKind::AgentClosed
                            | RuntimeEventKind::AgentTurnCompleted
                            | RuntimeEventKind::AgentTurnAborted => {
                                agent.live.current_run_id = None;
                            }
                            _ => {}
                        }
                    }
                }
            }
            RuntimeEventKind::MailboxEnqueued => {
                if let Some(agent) = self.agent_snapshot_mut(event) {
                    if let Some(item) = mailbox_item_from_payload(&event.payload) {
                        agent.live.pending_mailbox_count =
                            agent.live.pending_mailbox_count.saturating_add(1);
                        if item.trigger_turn {
                            agent.live.pending_trigger_turn_count =
                                agent.live.pending_trigger_turn_count.saturating_add(1);
                        }
                        agent.live.last_enqueued_mailbox_seq =
                            agent.live.last_enqueued_mailbox_seq.max(item.seq);
                    }
                }
            }
            RuntimeEventKind::MailboxDelivered => {
                if let Some(agent) = self.agent_snapshot_mut(event) {
                    let max_seq = mailbox_items_from_payload(&event.payload)
                        .iter()
                        .map(|item| item.seq)
                        .max()
                        .or_else(|| max_mailbox_seq_from_payload(&event.payload))
                        .unwrap_or_default();
                    agent.live.last_delivered_mailbox_seq =
                        agent.live.last_delivered_mailbox_seq.max(max_seq);
                }
            }
            RuntimeEventKind::MailboxConsumed => {
                if let Some(agent) = self.agent_snapshot_mut(event) {
                    let items = mailbox_items_from_payload(&event.payload);
                    let count = items
                        .len()
                        .max(event.payload["count"].as_u64().unwrap_or_default() as usize);
                    let trigger_count = items.iter().filter(|item| item.trigger_turn).count();
                    agent.live.pending_mailbox_count =
                        agent.live.pending_mailbox_count.saturating_sub(count);
                    agent.live.pending_trigger_turn_count = agent
                        .live
                        .pending_trigger_turn_count
                        .saturating_sub(trigger_count);
                    let max_seq = items
                        .iter()
                        .map(|item| item.seq)
                        .max()
                        .or_else(|| max_mailbox_seq_from_payload(&event.payload))
                        .unwrap_or_default();
                    agent.live.last_consumed_mailbox_seq =
                        agent.live.last_consumed_mailbox_seq.max(max_seq);
                }
            }
            _ => {}
        }
    }

    fn upsert_agent_from_event(
        &mut self,
        event: &RuntimeEvent,
        parent: Option<(AgentId, SessionId, PathBuf)>,
    ) {
        let Some(agent_id) = event.agent_id.clone() else {
            return;
        };
        let Some(session_id) = event.session_id.clone() else {
            return;
        };
        let root_id = event
            .root_id
            .clone()
            .or_else(|| {
                self.snapshot
                    .agents
                    .iter()
                    .find(|agent| agent.agent_id == agent_id || agent.session_id == session_id)
                    .map(|agent| agent.root_id.clone())
            })
            .unwrap_or_else(|| RootId::from_string(session_id.as_str()).unwrap_or_default());
        let cwd = event
            .payload
            .get("cwd")
            .and_then(Value::as_str)
            .map(PathBuf::from)
            .or_else(|| parent.as_ref().map(|(_, _, parent_cwd)| parent_cwd.clone()))
            .or_else(|| {
                self.snapshot
                    .agents
                    .iter()
                    .find(|agent| agent.agent_id == agent_id || agent.session_id == session_id)
                    .map(|agent| agent.cwd.clone())
            })
            .unwrap_or_default();
        let (parent_agent_id, parent_session_id, default_role) = match parent {
            Some((parent_agent_id, parent_session_id, _)) => {
                (Some(parent_agent_id), Some(parent_session_id), None)
            }
            None => (None, None, Some("default".to_string())),
        };
        let agent_path = event
            .payload
            .get("agent_path")
            .and_then(Value::as_str)
            .unwrap_or("/root")
            .to_string();
        let nickname = event
            .payload
            .get("nickname")
            .and_then(Value::as_str)
            .map(str::to_string);
        let role = event
            .payload
            .get("role")
            .and_then(Value::as_str)
            .map(str::to_string)
            .or(default_role);
        let status = projected_thread_status_for_event(event).unwrap_or(AgentThreadStatus::Created);

        if let Some(existing) = self
            .snapshot
            .agents
            .iter_mut()
            .find(|agent| agent.agent_id == agent_id || agent.session_id == session_id)
        {
            existing.session_id = session_id;
            existing.root_id = root_id;
            existing.cwd = cwd;
            existing.parent_agent_id = parent_agent_id;
            existing.parent_session_id = parent_session_id;
            existing.agent_path = agent_path;
            existing.nickname = nickname;
            existing.role = role;
            existing.status = status;
            return;
        }

        self.snapshot.agents.push(AgentThreadSnapshot {
            agent_id,
            session_id,
            root_id,
            cwd,
            parent_agent_id,
            parent_session_id,
            agent_path,
            nickname,
            role,
            status,
            live: AgentLiveStateSnapshot::default(),
        });
    }

    fn upsert_spawned_child(&mut self, event: &RuntimeEvent) {
        let Some(child_agent_id) = event
            .payload
            .get("child_agent_id")
            .and_then(Value::as_str)
            .and_then(|id| AgentId::from_string(id).ok())
        else {
            return;
        };
        let Some(child_session_id) = event
            .payload
            .get("child_session_id")
            .and_then(Value::as_str)
            .and_then(|id| SessionId::from_string(id).ok())
        else {
            return;
        };
        let parent = self
            .snapshot
            .agents
            .iter()
            .find(|agent| {
                event.agent_id.as_ref() == Some(&agent.agent_id)
                    || event.session_id.as_ref() == Some(&agent.session_id)
            })
            .map(|agent| {
                (
                    agent.agent_id.clone(),
                    agent.session_id.clone(),
                    agent.cwd.clone(),
                )
            });
        let mut child_event = event.clone();
        child_event.agent_id = Some(child_agent_id);
        child_event.session_id = Some(child_session_id);
        self.upsert_agent_from_event(&child_event, parent);
    }

    fn agent_snapshot_mut(&mut self, event: &RuntimeEvent) -> Option<&mut AgentThreadSnapshot> {
        self.snapshot.agents.iter_mut().find(|agent| {
            event.agent_id.as_ref() == Some(&agent.agent_id)
                || event.session_id.as_ref() == Some(&agent.session_id)
        })
    }

    fn reduce_live_activity(&mut self, event: &RuntimeEvent) {
        let Some(agent) = self.agent_snapshot_mut(event) else {
            return;
        };

        match event.kind {
            RuntimeEventKind::ExecCommandBegin => {
                start_activity(
                    agent,
                    event,
                    "exec_command",
                    Some("exec_command".to_string()),
                );
            }
            RuntimeEventKind::BrowserScriptStarted => {
                start_activity(
                    agent,
                    event,
                    "browser_script",
                    Some("browser_script".to_string()),
                );
            }
            RuntimeEventKind::PythonStarted => {
                start_activity(agent, event, "python", Some("python".to_string()));
            }
            RuntimeEventKind::McpToolStarted => {
                start_activity(
                    agent,
                    event,
                    "mcp_tool",
                    activity_name(event_payload(event)),
                );
            }
            RuntimeEventKind::ExecCommandOutputDelta
            | RuntimeEventKind::BrowserScriptOutputDelta
            | RuntimeEventKind::PythonOutputDelta
            | RuntimeEventKind::ToolOutputDelta => {
                update_activity_delta(agent, event, output_delta_text(event_payload(event)));
            }
            RuntimeEventKind::ExecCommandEnd
            | RuntimeEventKind::BrowserScriptCompleted
            | RuntimeEventKind::BrowserScriptCancelled
            | RuntimeEventKind::BrowserScriptFailed
            | RuntimeEventKind::PythonCompleted
            | RuntimeEventKind::McpToolCompleted
            | RuntimeEventKind::ToolCompleted
            | RuntimeEventKind::ToolFailed => {
                finish_activity(agent, event);
            }
            RuntimeEventKind::AgentTurnCompleted | RuntimeEventKind::AgentCompleted => {
                if let Some(result) = terminal_result(event_payload(event)) {
                    agent.live.final_result = Some(result);
                    agent.live.failure = None;
                }
                agent.live.active_items.clear();
                agent.live.active_model_request = None;
            }
            RuntimeEventKind::AgentTurnAborted
            | RuntimeEventKind::AgentFailed
            | RuntimeEventKind::AgentCancelled => {
                if let Some(failure) = terminal_failure(event_payload(event)) {
                    agent.live.failure = Some(failure);
                }
                agent.live.active_items.clear();
                agent.live.active_model_request = None;
            }
            _ => {}
        }

        let Some(event_type) = observed_event_type(event) else {
            return;
        };
        let payload = event_payload(event);
        match event_type {
            "model.turn.request" => {
                agent.live.active_model_request =
                    Some(RuntimeModelRequestSnapshot::from_request_payload(payload));
                agent.live.last_model_delta = None;
                agent.live.last_model_thinking_delta = None;
            }
            "model.turn.retry" => {
                let retry = RuntimeModelRequestSnapshot::from_retry_payload(
                    agent.live.active_model_request.take(),
                    payload,
                );
                agent.live.active_model_request = Some(retry);
                agent.live.last_model_delta = None;
                agent.live.last_model_thinking_delta = None;
            }
            "model.turn.error" => {
                let error = RuntimeModelRequestSnapshot::from_error_payload(
                    agent.live.active_model_request.take(),
                    payload,
                );
                agent.live.failure = error.last_error.clone();
                agent.live.active_model_request = Some(error);
                agent.live.last_model_delta = None;
                agent.live.last_model_thinking_delta = None;
            }
            "model.turn.response" => {
                agent.live.active_model_request = None;
            }
            "model.stream_delta" => {
                if let Some(text) = output_delta_text(payload) {
                    agent.live.last_model_delta = Some(text);
                }
            }
            "model.thinking_delta" => {
                if let Some(text) = output_delta_text(payload) {
                    agent.live.last_model_thinking_delta = Some(text);
                }
            }
            "tool.started" => {
                start_activity(agent, event, "tool", activity_name(payload));
            }
            "tool.output_delta" => {
                update_activity_delta(agent, event, output_delta_text(payload));
            }
            "tool.output" => {
                finish_activity(agent, event);
            }
            "tool.failed" | "tool.aborted" => {
                if let Some(failure) = terminal_failure(payload) {
                    agent.live.failure = Some(failure);
                }
                finish_activity(agent, event);
            }
            "token_count" => {
                if let Some(info) = payload.get("info") {
                    agent.live.last_token_usage = info.get("last_token_usage").cloned();
                    agent.live.total_token_usage = info.get("total_token_usage").cloned();
                    agent.live.model_context_window =
                        info.get("model_context_window").and_then(Value::as_i64);
                }
            }
            "session.done" => {
                if let Some(result) = terminal_result(payload) {
                    agent.live.final_result = Some(result);
                    agent.live.failure = None;
                }
                agent.live.active_items.clear();
                agent.live.active_model_request = None;
            }
            "session.failed" | "stream_error" => {
                if let Some(failure) = terminal_failure(payload) {
                    agent.live.failure = Some(failure);
                }
                agent.live.active_items.clear();
                if event_type == "stream_error" {
                    let error = RuntimeModelRequestSnapshot::from_error_payload(
                        agent.live.active_model_request.take(),
                        payload,
                    );
                    agent.live.active_model_request = Some(error);
                } else {
                    agent.live.active_model_request = None;
                }
            }
            _ => {}
        }
    }

    fn reduce_browser(&mut self, event: &RuntimeEvent) {
        let Some(browser_id) = event.browser_id.clone() else {
            return;
        };
        match event.kind {
            RuntimeEventKind::BrowserCreated => {
                let config = event
                    .payload
                    .get("config")
                    .cloned()
                    .and_then(|value| serde_json::from_value(value).ok())
                    .unwrap_or_default();
                self.upsert_browser(BrowserSnapshot {
                    id: browser_id,
                    config,
                    status: BrowserStatus::Created,
                    active_agent_id: None,
                    active_scripts: Vec::new(),
                });
            }
            RuntimeEventKind::BrowserStarted => {
                if let Some(browser) = self.browser_snapshot_mut(&browser_id) {
                    browser.status = BrowserStatus::Started;
                }
            }
            RuntimeEventKind::BrowserClaimed => {
                let active_agent_id = event.agent_id.clone().or_else(|| {
                    event
                        .payload
                        .get("agent_id")
                        .and_then(Value::as_str)
                        .and_then(|id| AgentId::from_string(id).ok())
                });
                if let Some(browser) = self.browser_snapshot_mut(&browser_id) {
                    browser.status = BrowserStatus::Claimed;
                    browser.active_agent_id = active_agent_id;
                }
            }
            RuntimeEventKind::BrowserReleased => {
                if let Some(browser) = self.browser_snapshot_mut(&browser_id) {
                    browser.status = BrowserStatus::Released;
                    browser.active_agent_id = None;
                }
            }
            RuntimeEventKind::BrowserClosed => {
                if let Some(browser) = self.browser_snapshot_mut(&browser_id) {
                    browser.status = BrowserStatus::Closed;
                    browser.active_agent_id = None;
                    browser.active_scripts.clear();
                } else {
                    self.snapshot.browsers.push(BrowserSnapshot {
                        id: browser_id,
                        config: BrowserConfig::default(),
                        status: BrowserStatus::Closed,
                        active_agent_id: None,
                        active_scripts: Vec::new(),
                    });
                }
            }
            RuntimeEventKind::BrowserScriptStarted => {
                let payload = event_payload(event);
                let Some(script) = browser_script_snapshot_from_payload(
                    payload,
                    event.session_id.clone(),
                    event.agent_id.clone(),
                ) else {
                    return;
                };
                if let Some(browser) = self.browser_snapshot_mut(&browser_id) {
                    upsert_browser_script(browser, script);
                }
            }
            RuntimeEventKind::BrowserScriptOutputDelta => {
                let payload = event_payload(event);
                let Some(run_id) = browser_script_run_id_from_payload(payload) else {
                    return;
                };
                let delta = output_delta_text(payload);
                if let Some(browser) = self.browser_snapshot_mut(&browser_id) {
                    update_browser_script_delta(browser, &run_id, delta);
                }
            }
            RuntimeEventKind::BrowserScriptCompleted
            | RuntimeEventKind::BrowserScriptCancelled
            | RuntimeEventKind::BrowserScriptFailed => {
                let payload = event_payload(event);
                let Some(run_id) = browser_script_run_id_from_payload(payload) else {
                    return;
                };
                if let Some(browser) = self.browser_snapshot_mut(&browser_id) {
                    browser
                        .active_scripts
                        .retain(|script| script.run_id != run_id);
                }
            }
            _ => {}
        }
    }

    fn upsert_browser(&mut self, snapshot: BrowserSnapshot) {
        if let Some(existing) = self.browser_snapshot_mut(&snapshot.id) {
            *existing = snapshot;
            return;
        }
        self.snapshot.browsers.push(snapshot);
    }

    fn browser_snapshot_mut(&mut self, browser_id: &BrowserId) -> Option<&mut BrowserSnapshot> {
        self.snapshot
            .browsers
            .iter_mut()
            .find(|browser| browser.id == *browser_id)
    }
}

impl RuntimeModelRequestSnapshot {
    fn from_request_payload(payload: &Value) -> Self {
        Self {
            model: payload
                .get("model")
                .and_then(Value::as_str)
                .map(str::to_string),
            provider: payload
                .get("provider")
                .and_then(Value::as_str)
                .map(str::to_string),
            turn_idx: payload.get("turn_idx").and_then(Value::as_i64),
            attempt: payload.get("attempt").and_then(value_to_u64),
            status: "requesting".to_string(),
            retry_count: 0,
            last_error: None,
        }
    }

    fn from_retry_payload(previous: Option<Self>, payload: &Value) -> Self {
        let mut next = previous.unwrap_or_else(|| Self {
            model: None,
            provider: None,
            turn_idx: None,
            attempt: None,
            status: "requesting".to_string(),
            retry_count: 0,
            last_error: None,
        });
        next.status = "retrying".to_string();
        next.retry_count = next.retry_count.saturating_add(1);
        if let Some(attempt) = payload.get("attempt").and_then(value_to_u64) {
            next.attempt = Some(attempt);
        }
        if let Some(error) = terminal_failure(payload) {
            next.last_error = Some(error);
        }
        next
    }

    fn from_error_payload(previous: Option<Self>, payload: &Value) -> Self {
        let mut next = previous.unwrap_or_else(|| Self {
            model: payload
                .get("model")
                .and_then(Value::as_str)
                .map(str::to_string),
            provider: payload
                .get("provider")
                .and_then(Value::as_str)
                .map(str::to_string),
            turn_idx: payload.get("turn_idx").and_then(Value::as_i64),
            attempt: payload.get("attempt").and_then(value_to_u64),
            status: "requesting".to_string(),
            retry_count: 0,
            last_error: None,
        });
        next.status = "error".to_string();
        if let Some(attempt) = payload.get("attempt").and_then(value_to_u64) {
            next.attempt = Some(attempt);
        }
        next.last_error = terminal_failure(payload);
        next
    }
}

pub struct RuntimeEventProjection;

impl RuntimeEventProjection {
    pub fn project(event: &RuntimeEvent) -> ProjectedEvent {
        let kind = match event.kind {
            RuntimeEventKind::AgentStarted
            | RuntimeEventKind::AgentCompleted
            | RuntimeEventKind::AgentFailed
            | RuntimeEventKind::AgentCancelled
            | RuntimeEventKind::AgentCloseRequested
            | RuntimeEventKind::AgentClosed => ProjectedEventKind::ThreadStatusChanged,
            RuntimeEventKind::AgentTurnStarted => ProjectedEventKind::TurnStarted,
            RuntimeEventKind::AgentTurnCompleted | RuntimeEventKind::AgentTurnAborted => {
                ProjectedEventKind::TurnCompleted
            }
            RuntimeEventKind::ExecCommandBegin
            | RuntimeEventKind::BrowserScriptStarted
            | RuntimeEventKind::PythonStarted
            | RuntimeEventKind::McpToolStarted => ProjectedEventKind::ItemStarted,
            RuntimeEventKind::ExecCommandEnd
            | RuntimeEventKind::BrowserScriptCompleted
            | RuntimeEventKind::BrowserScriptCancelled
            | RuntimeEventKind::BrowserScriptFailed
            | RuntimeEventKind::PythonCompleted
            | RuntimeEventKind::McpToolCompleted
            | RuntimeEventKind::ToolCompleted
            | RuntimeEventKind::ToolFailed => ProjectedEventKind::ItemCompleted,
            RuntimeEventKind::ExecCommandOutputDelta
            | RuntimeEventKind::BrowserScriptOutputDelta
            | RuntimeEventKind::PythonOutputDelta => ProjectedEventKind::CommandOutputDelta,
            RuntimeEventKind::ToolOutputDelta => ProjectedEventKind::AgentMessageDelta,
            _ => ProjectedEventKind::ToolUpdated,
        };
        ProjectedEvent {
            source_event_id: event.id.clone(),
            kind,
            session_id: event.session_id.clone(),
            payload: event.payload.clone(),
            snapshot: None,
        }
    }
}

fn projected_thread_status_for_event(event: &RuntimeEvent) -> Option<AgentThreadStatus> {
    if event
        .payload
        .get("child_session_id")
        .and_then(Value::as_str)
        .is_some_and(|child_session_id| {
            event
                .session_id
                .as_ref()
                .is_some_and(|session_id| session_id.as_str() != child_session_id)
        })
    {
        return None;
    }

    match event.kind {
        RuntimeEventKind::AgentCreated | RuntimeEventKind::AgentResumed => {
            Some(AgentThreadStatus::Created)
        }
        RuntimeEventKind::AgentQueued => Some(AgentThreadStatus::Queued),
        RuntimeEventKind::AgentStarted
        | RuntimeEventKind::AgentContinuationStarted
        | RuntimeEventKind::AgentTurnStarted => Some(AgentThreadStatus::Running),
        RuntimeEventKind::AgentCancelRequested | RuntimeEventKind::AgentCloseRequested => {
            Some(AgentThreadStatus::Cancelling)
        }
        RuntimeEventKind::AgentCompleted | RuntimeEventKind::AgentTurnCompleted => {
            Some(AgentThreadStatus::Completed)
        }
        RuntimeEventKind::AgentFailed | RuntimeEventKind::AgentTurnAborted => {
            Some(AgentThreadStatus::Failed)
        }
        RuntimeEventKind::AgentCancelled => Some(AgentThreadStatus::Cancelled),
        RuntimeEventKind::AgentClosed => Some(AgentThreadStatus::Closed),
        _ => None,
    }
}

fn observed_event_type(event: &RuntimeEvent) -> Option<&str> {
    event.payload.get("event_type").and_then(Value::as_str)
}

fn event_payload(event: &RuntimeEvent) -> &Value {
    event.payload.get("payload").unwrap_or(&event.payload)
}

fn activity_tool_call_id(event: &RuntimeEvent) -> Option<ToolCallId> {
    event.tool_call_id.clone().or_else(|| {
        event_payload(event)
            .get("tool_call_id")
            .and_then(Value::as_str)
            .and_then(|id| ToolCallId::from_string(id).ok())
    })
}

fn activity_key(event: &RuntimeEvent, kind: &str) -> String {
    if let Some(tool_call_id) = activity_tool_call_id(event) {
        return format!("{kind}:{}", tool_call_id.as_str());
    }
    if kind == "browser_script" {
        if let Some(run_id) = browser_script_run_id_from_payload(event_payload(event)) {
            return format!("{kind}:{run_id}");
        }
    }
    format!("{kind}:{}", event.id.as_str())
}

fn activity_name(payload: &Value) -> Option<String> {
    payload
        .get("name")
        .or_else(|| payload.get("tool_name"))
        .or_else(|| payload.get("command"))
        .and_then(Value::as_str)
        .map(str::to_string)
}

fn browser_script_run_id_from_payload(payload: &Value) -> Option<String> {
    payload_resource_id(
        payload,
        &[
            "run_id",
            "browser_script_run_id",
            "script_run_id",
            "tool_call_id",
        ],
    )
}

fn browser_script_snapshot_from_payload(
    payload: &Value,
    session_id: Option<SessionId>,
    agent_id: Option<AgentId>,
) -> Option<BrowserScriptSnapshot> {
    Some(BrowserScriptSnapshot {
        run_id: browser_script_run_id_from_payload(payload)?,
        session_id,
        agent_id,
        tool_call_id: payload
            .get("payload")
            .unwrap_or(payload)
            .get("tool_call_id")
            .and_then(Value::as_str)
            .and_then(|id| ToolCallId::from_string(id).ok()),
        last_delta: None,
    })
}

fn upsert_browser_script(browser: &mut BrowserSnapshot, script: BrowserScriptSnapshot) {
    if let Some(existing) = browser
        .active_scripts
        .iter_mut()
        .find(|active| active.run_id == script.run_id)
    {
        *existing = script;
        return;
    }
    browser.active_scripts.push(script);
    browser
        .active_scripts
        .sort_by(|left, right| left.run_id.cmp(&right.run_id));
}

fn update_browser_script_delta(browser: &mut BrowserSnapshot, run_id: &str, delta: Option<String>) {
    let Some(delta) = delta else {
        return;
    };
    if let Some(script) = browser
        .active_scripts
        .iter_mut()
        .find(|script| script.run_id == run_id)
    {
        script.last_delta = Some(delta);
    }
}

fn start_activity(
    agent: &mut AgentThreadSnapshot,
    event: &RuntimeEvent,
    kind: &str,
    name: Option<String>,
) {
    let key = activity_key(event, kind);
    let item = RuntimeActivitySnapshot {
        key: key.clone(),
        kind: kind.to_string(),
        name,
        tool_call_id: activity_tool_call_id(event),
        last_delta: None,
    };
    if let Some(existing) = agent
        .live
        .active_items
        .iter_mut()
        .find(|active| active.key == key)
    {
        *existing = item;
        return;
    }
    agent.live.active_items.push(item);
}

fn update_activity_delta(
    agent: &mut AgentThreadSnapshot,
    event: &RuntimeEvent,
    delta: Option<String>,
) {
    let Some(delta) = delta else {
        return;
    };
    let keys = activity_keys_for_event(event);
    if let Some(existing) = agent
        .live
        .active_items
        .iter_mut()
        .find(|active| keys.iter().any(|key| key == &active.key))
    {
        existing.last_delta = Some(delta);
    }
}

fn finish_activity(agent: &mut AgentThreadSnapshot, event: &RuntimeEvent) {
    let keys = activity_keys_for_event(event);
    agent
        .live
        .active_items
        .retain(|active| !keys.iter().any(|key| key == &active.key));
}

fn activity_keys_for_event(event: &RuntimeEvent) -> Vec<String> {
    let Some(tool_call_id) = activity_tool_call_id(event) else {
        if let Some(run_id) = browser_script_run_id_from_payload(event_payload(event)) {
            return vec![format!("browser_script:{run_id}")];
        }
        return vec![
            activity_key(event, "exec_command"),
            activity_key(event, "browser_script"),
            activity_key(event, "python"),
            activity_key(event, "mcp_tool"),
            activity_key(event, "tool"),
        ];
    };
    [
        "exec_command",
        "browser_script",
        "python",
        "mcp_tool",
        "tool",
    ]
    .into_iter()
    .map(|kind| format!("{kind}:{}", tool_call_id.as_str()))
    .collect()
}

fn output_delta_text(payload: &Value) -> Option<String> {
    payload
        .get("text")
        .or_else(|| payload.get("delta"))
        .or_else(|| payload.get("stdout"))
        .or_else(|| payload.get("stderr"))
        .or_else(|| payload.get("output"))
        .and_then(Value::as_str)
        .map(str::to_string)
}

fn terminal_result(payload: &Value) -> Option<String> {
    payload
        .get("result")
        .or_else(|| payload.get("completed"))
        .and_then(Value::as_str)
        .map(str::to_string)
}

fn terminal_failure(payload: &Value) -> Option<String> {
    payload
        .get("error")
        .or_else(|| payload.get("failure"))
        .or_else(|| payload.get("reason"))
        .or_else(|| payload.get("message"))
        .and_then(Value::as_str)
        .map(str::to_string)
}

fn value_to_u64(value: &Value) -> Option<u64> {
    value
        .as_u64()
        .or_else(|| value.as_i64().and_then(|n| u64::try_from(n).ok()))
}

fn mailbox_item_from_payload(payload: &Value) -> Option<MailboxItem> {
    serde_json::from_value(payload.get("mailbox_item")?.clone()).ok()
}

fn mailbox_items_from_payload(payload: &Value) -> Vec<MailboxItem> {
    payload
        .get("mailbox_items")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| serde_json::from_value(item.clone()).ok())
                .collect()
        })
        .unwrap_or_default()
}

fn max_mailbox_seq_from_payload(payload: &Value) -> Option<u64> {
    payload
        .get("mailbox_seqs")?
        .as_array()?
        .iter()
        .filter_map(Value::as_u64)
        .max()
}

#[derive(Clone)]
pub struct RuntimeEventBus {
    tx: broadcast::Sender<RuntimeEvent>,
}

impl RuntimeEventBus {
    pub fn new(capacity: usize) -> Self {
        let (tx, _) = broadcast::channel(capacity);
        Self { tx }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<RuntimeEvent> {
        self.tx.subscribe()
    }

    pub fn publish(&self, event: RuntimeEvent) {
        let _ = self.tx.send(event);
    }
}

impl Default for RuntimeEventBus {
    fn default() -> Self {
        Self::new(1024)
    }
}

#[derive(Clone)]
pub struct RuntimeHandle {
    inner: Arc<BrowserUseRuntime>,
}

#[derive(Clone, Debug)]
pub struct RunAgentRequest {
    pub agent_id: Option<AgentId>,
    pub session_id: SessionId,
    pub run_id: RunId,
    pub provider_config: Option<Value>,
    pub initial_input: Option<Value>,
    pub browser_id: Option<BrowserId>,
    pub cwd: Option<PathBuf>,
    pub input_source: Option<String>,
    pub resume_mode: ResumeMode,
    pub cancellation_token: CancellationToken,
}

impl RunAgentRequest {
    pub fn new(session_id: SessionId) -> Self {
        Self {
            agent_id: None,
            session_id,
            run_id: RunId::new(),
            provider_config: None,
            initial_input: None,
            browser_id: None,
            cwd: None,
            input_source: None,
            resume_mode: ResumeMode::Continue,
            cancellation_token: CancellationToken::new(),
        }
    }

    pub fn with_agent_id(mut self, agent_id: AgentId) -> Self {
        self.agent_id = Some(agent_id);
        self
    }

    pub fn with_run_id(mut self, run_id: RunId) -> Self {
        self.run_id = run_id;
        self
    }

    pub fn with_provider_config(mut self, provider_config: Value) -> Self {
        self.provider_config = Some(provider_config);
        self
    }

    pub fn with_initial_input(mut self, initial_input: Value) -> Self {
        self.initial_input = Some(initial_input);
        self
    }

    pub fn with_browser_id(mut self, browser_id: BrowserId) -> Self {
        self.browser_id = Some(browser_id);
        self
    }

    pub fn with_cwd(mut self, cwd: PathBuf) -> Self {
        self.cwd = Some(cwd);
        self
    }

    pub fn with_input_source(mut self, input_source: impl Into<String>) -> Self {
        self.input_source = Some(input_source.into());
        self
    }

    pub fn with_resume_mode(mut self, resume_mode: ResumeMode) -> Self {
        self.resume_mode = resume_mode;
        self
    }

    pub fn with_cancellation_token(mut self, cancellation_token: CancellationToken) -> Self {
        self.cancellation_token = cancellation_token;
        self
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ResumeMode {
    Continue,
    Replay,
    Fresh,
}

#[derive(Clone, Debug)]
pub struct RunAgentResponse<T> {
    pub agent_id: AgentId,
    pub session_id: SessionId,
    pub run_id: RunId,
    pub final_status: AgentThreadStatus,
    pub final_result: Option<String>,
    pub usage: Option<Value>,
    pub terminal_event_seq: Option<i64>,
    pub output: T,
}

fn initial_input_source_event_seq(initial_input: &Value) -> Option<i64> {
    initial_input
        .get("source_event_seq")
        .and_then(Value::as_i64)
        .or_else(|| {
            initial_input
                .get("payload")
                .and_then(|payload| payload.get("source_event_seq"))
                .and_then(Value::as_i64)
        })
}

fn final_result_event_from_events(events: &[EventRecord]) -> Option<(i64, String)> {
    events.iter().rev().find_map(|event| {
        if event.event_type != "session.done" {
            return None;
        }
        event
            .payload
            .get("result")
            .and_then(Value::as_str)
            .map(|result| (event.seq, result.to_owned()))
    })
}

impl RuntimeHandle {
    pub fn new(runtime: BrowserUseRuntime) -> Self {
        Self {
            inner: Arc::new(runtime),
        }
    }

    pub fn events(&self) -> &RuntimeEventBus {
        &self.inner.events
    }

    pub fn journal(&self) -> &dyn JournalSink {
        self.inner.persistence.as_ref()
    }

    pub fn load_session(&self, session_id: &SessionId) -> Result<Option<SessionMeta>> {
        self.inner.persistence.load_session(session_id)
    }

    pub fn events_for_session(&self, session_id: &SessionId) -> Result<Vec<EventRecord>> {
        self.inner.persistence.events_for_session(session_id)
    }

    pub fn events_after_seq(
        &self,
        session_id: &SessionId,
        after_seq: i64,
    ) -> Result<Vec<EventRecord>> {
        self.inner
            .persistence
            .events_after_seq(session_id, after_seq)
    }

    pub fn compaction_prefill_input_tokens_for_session(
        &self,
        session_id: &SessionId,
    ) -> Result<Option<i64>> {
        Ok(self
            .inner
            .agents
            .thread_for_session(session_id)?
            .live_state
            .compaction_prefill_input_tokens())
    }

    pub fn record_estimated_compaction_prefill_for_session(
        &self,
        session_id: &SessionId,
        tokens: i64,
    ) -> Result<()> {
        self.inner
            .agents
            .thread_for_session(session_id)?
            .live_state
            .record_estimated_compaction_prefill(tokens);
        Ok(())
    }

    pub fn record_server_observed_compaction_prefill_for_session(
        &self,
        session_id: &SessionId,
        tokens: i64,
    ) -> Result<()> {
        self.inner
            .agents
            .thread_for_session(session_id)?
            .live_state
            .record_server_observed_compaction_prefill(tokens);
        Ok(())
    }

    pub fn start_next_compaction_window_for_session(&self, session_id: &SessionId) -> Result<()> {
        self.inner
            .agents
            .thread_for_session(session_id)?
            .live_state
            .start_next_compaction_window();
        Ok(())
    }

    pub fn append_observed_session_event(
        &self,
        session_id: SessionId,
        event_type: &str,
        payload: Value,
        durability: Durability,
    ) -> Result<JournalAppend> {
        let append = self.inner.persistence.append_session_event(
            &session_id,
            event_type,
            payload.clone(),
            durability,
        )?;
        let agent_id = AgentId::from_string(session_id.as_str().to_string())?;
        let mut event = RuntimeEvent::new(
            RuntimeEventKind::from_observed_event_type(event_type),
            durability,
        )
        .with_session_id(session_id.clone())
        .with_agent_id(agent_id.clone())
        .with_payload(json!({
            "event_type": event_type,
            "seq": append.seq,
            "payload": payload,
        }));
        if let Ok(thread) = self.inner.agents.thread(&agent_id) {
            event.root_id = Some(thread.root_id.clone());
        }
        self.inner.events.publish(event);
        Ok(append)
    }

    pub fn append_observed_browser_session_event(
        &self,
        session_id: SessionId,
        browser_id: BrowserId,
        event_type: &str,
        payload: Value,
        durability: Durability,
    ) -> Result<JournalAppend> {
        let thread = self.inner.agents.thread_for_session(&session_id)?;
        let agent_id = thread.agent_id().clone();
        self.inner.browsers.snapshot(&browser_id)?;

        let append = self.inner.persistence.append_session_event(
            &session_id,
            event_type,
            payload.clone(),
            durability,
        )?;
        match event_type {
            "browser_script.started" => {
                if let Some(script) = browser_script_snapshot_from_payload(
                    &payload,
                    Some(session_id.clone()),
                    Some(agent_id.clone()),
                ) {
                    self.inner
                        .browsers
                        .record_script_started(&browser_id, script)?;
                }
            }
            "browser_script.output_delta" => {
                if let Some(run_id) = browser_script_run_id_from_payload(&payload) {
                    self.inner.browsers.record_script_output_delta(
                        &browser_id,
                        &run_id,
                        output_delta_text(&payload),
                    )?;
                }
            }
            "browser_script.completed" | "browser_script.cancelled" | "browser_script.failed" => {
                if let Some(run_id) = browser_script_run_id_from_payload(&payload) {
                    self.inner
                        .browsers
                        .record_script_finished(&browser_id, &run_id)?;
                }
            }
            _ => {}
        }

        let mut event = RuntimeEvent::new(
            RuntimeEventKind::from_observed_event_type(event_type),
            durability,
        )
        .with_session_id(session_id)
        .with_agent_id(agent_id)
        .with_browser_id(browser_id)
        .with_payload(json!({
            "event_type": event_type,
            "seq": append.seq,
            "payload": payload,
        }));
        event.root_id = Some(thread.root_id.clone());
        self.inner.events.publish(event);
        Ok(append)
    }

    pub fn agents(&self) -> &AgentManager {
        &self.inner.agents
    }

    pub fn browsers(&self) -> &BrowserManager {
        &self.inner.browsers
    }

    pub fn browser_physical_registries(
        &self,
        browser_id: &BrowserId,
    ) -> Result<BrowserPhysicalRegistries> {
        self.inner.browsers.physical_registries(browser_id)
    }

    pub fn create_browser(&self, config: BrowserConfig) -> BrowserId {
        let browser_id = self.inner.browsers.create_browser(config.clone());
        self.inner.events.publish(
            RuntimeEvent::new(RuntimeEventKind::BrowserCreated, Durability::BestEffort)
                .with_browser_id(browser_id.clone())
                .with_payload(json!({
                    "browser_id": browser_id.as_str(),
                    "config": config,
                    "runtime_owned": true,
                })),
        );
        browser_id
    }

    pub fn create_browser_for_agent(
        &self,
        agent_id: AgentId,
        config: BrowserConfig,
    ) -> Result<BrowserId> {
        let thread = self.inner.agents.thread(&agent_id)?;
        let browser_id = BrowserId::new();
        let mut event = RuntimeEvent::new(RuntimeEventKind::BrowserCreated, Durability::Barrier)
            .with_agent_id(agent_id.clone())
            .with_session_id(thread.session_id.clone())
            .with_browser_id(browser_id.clone())
            .with_payload(json!({
                "browser_id": browser_id.as_str(),
                "agent_id": agent_id.as_str(),
                "config": config.clone(),
                "runtime_owned": true,
            }));
        event.root_id = Some(thread.root_id.clone());
        self.inner.append_runtime_event(&event)?;
        self.inner
            .browsers
            .create_browser_with_id(browser_id.clone(), config)?;
        self.inner.events.publish(event);
        Ok(browser_id)
    }

    pub fn claim_browser(&self, browser_id: &BrowserId, agent_id: AgentId) -> Result<BrowserLease> {
        let thread = self.inner.agents.thread(&agent_id)?;
        self.inner
            .browsers
            .validate_browser_claim(browser_id, &agent_id)?;
        let mut event = RuntimeEvent::new(RuntimeEventKind::BrowserClaimed, Durability::Barrier)
            .with_agent_id(agent_id.clone())
            .with_session_id(thread.session_id.clone())
            .with_browser_id(browser_id.clone())
            .with_payload(json!({
                "browser_id": browser_id.as_str(),
                "agent_id": agent_id.as_str(),
                "runtime_owned": true,
            }));
        event.root_id = Some(thread.root_id.clone());
        self.inner.append_runtime_event(&event)?;
        let lease = self
            .inner
            .browsers
            .claim_browser(browser_id, agent_id.clone())?;
        self.inner.events.publish(event);
        Ok(lease)
    }

    pub fn release_browser(&self, lease: &BrowserLease) -> Result<()> {
        let thread = self.inner.agents.thread(&lease.agent_id)?;
        self.inner.browsers.validate_browser_release(lease)?;
        let mut event = RuntimeEvent::new(RuntimeEventKind::BrowserReleased, Durability::Barrier)
            .with_agent_id(lease.agent_id.clone())
            .with_session_id(thread.session_id.clone())
            .with_browser_id(lease.browser_id.clone())
            .with_payload(json!({
                "browser_id": lease.browser_id.as_str(),
                "agent_id": lease.agent_id.as_str(),
                "runtime_owned": true,
            }));
        event.root_id = Some(thread.root_id.clone());
        self.inner.append_runtime_event(&event)?;
        self.inner.browsers.release_browser(lease)?;
        self.inner.events.publish(event);
        Ok(())
    }

    pub fn with_browser_action<T>(
        &self,
        browser_id: &BrowserId,
        agent_id: AgentId,
        action: impl FnOnce() -> Result<T>,
    ) -> Result<T> {
        self.inner.browsers.with_action_lock(browser_id, || {
            let lease = self.claim_browser(browser_id, agent_id)?;
            let result = action();
            let release_result = self.release_browser(&lease);
            match (result, release_result) {
                (Ok(value), Ok(())) => Ok(value),
                (Ok(_), Err(error)) => Err(error),
                (Err(error), _) => Err(error),
            }
        })
    }

    pub fn close_browser(&self, browser_id: &BrowserId) -> Result<()> {
        self.inner.browsers.close_browser(browser_id)?;
        self.inner.events.publish(
            RuntimeEvent::new(RuntimeEventKind::BrowserClosed, Durability::BestEffort)
                .with_browser_id(browser_id.clone())
                .with_payload(json!({
                    "browser_id": browser_id.as_str(),
                    "runtime_owned": true,
                })),
        );
        Ok(())
    }

    pub fn close_browser_for_agent(
        &self,
        browser_id: &BrowserId,
        agent_id: &AgentId,
    ) -> Result<()> {
        let thread = self.inner.agents.thread(agent_id)?;
        self.inner.browsers.validate_browser_close(browser_id)?;
        let mut event = RuntimeEvent::new(RuntimeEventKind::BrowserClosed, Durability::Barrier)
            .with_agent_id(agent_id.clone())
            .with_session_id(thread.session_id.clone())
            .with_browser_id(browser_id.clone())
            .with_payload(json!({
                "browser_id": browser_id.as_str(),
                "agent_id": agent_id.as_str(),
                "runtime_owned": true,
            }));
        event.root_id = Some(thread.root_id.clone());
        self.inner.append_runtime_event(&event)?;
        self.inner.browsers.close_browser(browser_id)?;
        self.inner.events.publish(event);
        Ok(())
    }

    pub fn register_run(&self, session_id: SessionId) -> CancellationToken {
        self.inner.active_runs.register(session_id)
    }

    pub fn register_run_with_token(
        &self,
        session_id: SessionId,
        token: CancellationToken,
    ) -> CancellationToken {
        self.inner
            .active_runs
            .register_with_token(session_id, token)
    }

    pub fn unregister_run(&self, session_id: &SessionId) {
        self.inner.active_runs.unregister(session_id);
    }

    pub fn request_cancel_run(&self, session_id: &SessionId) -> Result<bool> {
        let thread = self.inner.agents.thread_for_session(session_id)?;
        let mut cancelled = self.request_cancel_run_for_thread(
            &thread,
            json!({
                "runtime_owned": true,
            }),
        )?;
        let descendant_edges = self
            .inner
            .state_index
            .list_descendants(&thread.session_id)?;
        for edge in descendant_edges {
            let Ok(descendant) = self.inner.agents.thread_for_session(&edge.child_session_id)
            else {
                continue;
            };
            cancelled |= self.request_cancel_run_for_thread(
                &descendant,
                json!({
                    "runtime_owned": true,
                    "propagated_from_session_id": thread.session_id.as_str(),
                    "propagated_from_agent_id": thread.agent_id.as_str(),
                }),
            )?;
        }
        Ok(cancelled)
    }

    fn request_cancel_run_for_thread(
        &self,
        thread: &AgentThread,
        mut payload: Value,
    ) -> Result<bool> {
        let Some(token) = self.inner.active_runs.token(&thread.session_id) else {
            return Ok(false);
        };
        if !payload.is_object() {
            payload = json!({
                "runtime_owned": true,
            });
        }
        let event = RuntimeEvent::new(RuntimeEventKind::AgentCancelRequested, Durability::Barrier)
            .with_agent_id(thread.agent_id.clone())
            .with_session_id(thread.session_id.clone())
            .with_root_id(thread.root_id.clone())
            .with_payload(payload);
        self.inner.append_runtime_event(&event)?;
        thread.set_status(AgentThreadStatus::Cancelling);
        thread.live_state.request_cancel();
        token.cancel();
        self.inner.events.publish(event);
        Ok(true)
    }

    pub fn cancel_run(&self, session_id: &SessionId) -> bool {
        self.request_cancel_run(session_id).unwrap_or(false)
    }

    pub async fn run_agent<T, Fut>(
        &self,
        request: RunAgentRequest,
        run: Fut,
    ) -> Result<RunAgentResponse<T>>
    where
        Fut: Future<Output = Result<T>> + Send,
        T: Send,
    {
        let thread = self.inner.agents.thread_for_session(&request.session_id)?;
        if let Some(requested_agent_id) = request.agent_id.as_ref() {
            if requested_agent_id != thread.agent_id() {
                bail!(
                    "run_agent agent/session mismatch: request agent {} maps to session {}, but runtime owns agent {}",
                    requested_agent_id,
                    request.session_id,
                    thread.agent_id()
                );
            }
        }
        let agent_id = thread.agent_id().clone();
        let browser_lease = request
            .browser_id
            .as_ref()
            .map(|browser_id| self.claim_browser(browser_id, agent_id.clone()))
            .transpose()?;

        if let Some(initial_input) = request.initial_input.clone() {
            if let Err(error) = self.inner.accept_prompt_input(AcceptPromptInputRequest {
                target_agent_id: agent_id.clone(),
                source_event_seq: initial_input_source_event_seq(&initial_input),
                payload: initial_input,
            }) {
                if let Some(lease) = browser_lease.as_ref() {
                    self.release_browser(lease)?;
                }
                return Err(error);
            }
        }

        if let Err(error) = self.inner.publish_after_barrier(
            RuntimeEvent::new(RuntimeEventKind::AgentStarted, Durability::Barrier)
                .with_agent_id(agent_id.clone())
                .with_session_id(request.session_id.clone())
                .with_root_id(thread.root_id.clone())
                .with_run_id(request.run_id.clone())
                .with_payload(json!({
                    "runtime_owned": true,
                    "run_id": request.run_id.as_str(),
                    "provider_config": request.provider_config.clone(),
                    "initial_input": request.initial_input.clone(),
                    "browser_id": request.browser_id.as_ref().map(BrowserId::as_str),
                    "cwd": request.cwd.as_ref().map(|cwd| cwd.display().to_string()),
                    "input_source": request.input_source.clone(),
                    "resume_mode": request.resume_mode.clone(),
                })),
        ) {
            if let Some(lease) = browser_lease.as_ref() {
                self.release_browser(lease)?;
            }
            return Err(error);
        }
        if let Err(error) = self.inner.publish_after_barrier(
            RuntimeEvent::new(RuntimeEventKind::AgentTurnStarted, Durability::Barrier)
                .with_agent_id(agent_id.clone())
                .with_session_id(request.session_id.clone())
                .with_root_id(thread.root_id.clone())
                .with_run_id(request.run_id.clone())
                .with_payload(json!({
                    "runtime_owned": true,
                    "run_id": request.run_id.as_str(),
                })),
        ) {
            if let Some(lease) = browser_lease.as_ref() {
                self.release_browser(lease)?;
            }
            return Err(error);
        }
        thread.live_state.begin_run(request.run_id.clone());
        thread.set_status(AgentThreadStatus::Running);

        self.inner.active_runs.register_with_token(
            request.session_id.clone(),
            request.cancellation_token.clone(),
        );
        let _active_run = ActiveRuntimeRunGuard {
            runtime: self.inner.clone(),
            session_id: request.session_id.clone(),
        };

        let output = match run.await {
            Ok(output) => output,
            Err(error) => {
                let final_status = if request.cancellation_token.is_cancelled() {
                    AgentThreadStatus::Cancelled
                } else {
                    AgentThreadStatus::Failed
                };
                if let Err(abort_error) = self.inner.publish_after_barrier(
                    RuntimeEvent::new(RuntimeEventKind::AgentTurnAborted, Durability::Barrier)
                        .with_agent_id(agent_id.clone())
                        .with_session_id(request.session_id.clone())
                        .with_root_id(thread.root_id.clone())
                        .with_run_id(request.run_id.clone())
                        .with_payload(json!({
                            "runtime_owned": true,
                            "run_id": request.run_id.as_str(),
                            "error": format!("{error:#}"),
                            "cancelled": request.cancellation_token.is_cancelled(),
                        })),
                ) {
                    thread.live_state.finish_run();
                    if let Some(lease) = browser_lease.as_ref() {
                        self.release_browser(lease)?;
                    }
                    return Err(abort_error);
                }
                let terminal_event_type = if request.cancellation_token.is_cancelled() {
                    "session.cancelled"
                } else {
                    "session.failed"
                };
                let terminal_payload = if request.cancellation_token.is_cancelled() {
                    json!({
                        "reason": format!("{error:#}"),
                        "runtime_owned": true,
                    })
                } else {
                    json!({
                        "error": format!("{error:#}"),
                        "runtime_owned": true,
                    })
                };
                let terminal_append = match self.inner.persistence.append_session_event(
                    &request.session_id,
                    terminal_event_type,
                    terminal_payload,
                    Durability::Barrier,
                ) {
                    Ok(append) => append,
                    Err(terminal_error) => {
                        thread.live_state.finish_run();
                        if let Some(lease) = browser_lease.as_ref() {
                            self.release_browser(lease)?;
                        }
                        return Err(terminal_error);
                    }
                };
                let terminal_kind = if request.cancellation_token.is_cancelled() {
                    RuntimeEventKind::AgentCancelled
                } else {
                    RuntimeEventKind::AgentFailed
                };
                if let Err(terminal_runtime_error) = self.inner.publish_after_barrier(
                    RuntimeEvent::new(terminal_kind, Durability::Barrier)
                        .with_agent_id(agent_id.clone())
                        .with_session_id(request.session_id.clone())
                        .with_root_id(thread.root_id.clone())
                        .with_run_id(request.run_id.clone())
                        .with_payload(json!({
                            "runtime_owned": true,
                            "run_id": request.run_id.as_str(),
                            "error": format!("{error:#}"),
                            "cancelled": request.cancellation_token.is_cancelled(),
                            "terminal_event_type": terminal_event_type,
                            "terminal_event_seq": terminal_append.seq,
                        })),
                ) {
                    thread.live_state.finish_run();
                    if let Some(lease) = browser_lease.as_ref() {
                        self.release_browser(lease)?;
                    }
                    return Err(terminal_runtime_error);
                }
                thread.set_status(final_status);
                thread.live_state.finish_run();
                if let Some(lease) = browser_lease.as_ref() {
                    self.release_browser(lease)?;
                }
                return Err(error);
            }
        };

        if request.cancellation_token.is_cancelled() {
            let reason = "cancelled by user";
            if let Err(abort_error) = self.inner.publish_after_barrier(
                RuntimeEvent::new(RuntimeEventKind::AgentTurnAborted, Durability::Barrier)
                    .with_agent_id(agent_id.clone())
                    .with_session_id(request.session_id.clone())
                    .with_root_id(thread.root_id.clone())
                    .with_run_id(request.run_id.clone())
                    .with_payload(json!({
                        "runtime_owned": true,
                        "run_id": request.run_id.as_str(),
                        "error": reason,
                        "cancelled": true,
                    })),
            ) {
                thread.live_state.finish_run();
                if let Some(lease) = browser_lease.as_ref() {
                    self.release_browser(lease)?;
                }
                return Err(abort_error);
            }
            let terminal_append = match self.inner.persistence.append_session_event(
                &request.session_id,
                "session.cancelled",
                json!({
                    "reason": reason,
                    "runtime_owned": true,
                }),
                Durability::Barrier,
            ) {
                Ok(append) => append,
                Err(error) => {
                    thread.live_state.finish_run();
                    if let Some(lease) = browser_lease.as_ref() {
                        self.release_browser(lease)?;
                    }
                    return Err(error);
                }
            };
            let terminal_append = match self.inner.publish_after_barrier(
                RuntimeEvent::new(RuntimeEventKind::AgentCancelled, Durability::Barrier)
                    .with_agent_id(agent_id.clone())
                    .with_session_id(request.session_id.clone())
                    .with_root_id(thread.root_id.clone())
                    .with_run_id(request.run_id.clone())
                    .with_payload(json!({
                        "runtime_owned": true,
                        "run_id": request.run_id.as_str(),
                        "error": reason,
                        "cancelled": true,
                        "terminal_event_type": "session.cancelled",
                        "terminal_event_seq": terminal_append.seq,
                    })),
            ) {
                Ok(append) => append,
                Err(error) => {
                    thread.live_state.finish_run();
                    if let Some(lease) = browser_lease.as_ref() {
                        self.release_browser(lease)?;
                    }
                    return Err(error);
                }
            };
            thread.set_status(AgentThreadStatus::Cancelled);
            thread.live_state.finish_run();
            if let Some(lease) = browser_lease.as_ref() {
                self.release_browser(lease)?;
            }

            return Ok(RunAgentResponse {
                agent_id,
                session_id: request.session_id,
                run_id: request.run_id,
                final_status: AgentThreadStatus::Cancelled,
                final_result: None,
                usage: None,
                terminal_event_seq: terminal_append.seq,
                output,
            });
        }

        let terminal_append = match self.inner.publish_after_barrier(
            RuntimeEvent::new(RuntimeEventKind::AgentTurnCompleted, Durability::Barrier)
                .with_agent_id(agent_id.clone())
                .with_session_id(request.session_id.clone())
                .with_root_id(thread.root_id.clone())
                .with_run_id(request.run_id.clone())
                .with_payload(json!({
                    "runtime_owned": true,
                    "run_id": request.run_id.as_str(),
                })),
        ) {
            Ok(append) => append,
            Err(error) => {
                thread.live_state.finish_run();
                if let Some(lease) = browser_lease.as_ref() {
                    self.release_browser(lease)?;
                }
                return Err(error);
            }
        };
        let final_result_event = self
            .inner
            .persistence
            .events_for_session(&request.session_id)
            .ok()
            .and_then(|events| final_result_event_from_events(&events));
        let final_result = final_result_event
            .as_ref()
            .map(|(_, result)| result.clone());
        let terminal_append = match self.inner.publish_after_barrier(
            RuntimeEvent::new(RuntimeEventKind::AgentCompleted, Durability::Barrier)
                .with_agent_id(agent_id.clone())
                .with_session_id(request.session_id.clone())
                .with_root_id(thread.root_id.clone())
                .with_run_id(request.run_id.clone())
                .with_payload(json!({
                    "runtime_owned": true,
                    "run_id": request.run_id.as_str(),
                    "result": final_result.clone(),
                    "terminal_event_type": "session.done",
                    "terminal_event_seq": final_result_event.map(|(seq, _)| seq),
                    "turn_completed_event_seq": terminal_append.seq,
                })),
        ) {
            Ok(append) => append,
            Err(error) => {
                thread.live_state.finish_run();
                if let Some(lease) = browser_lease.as_ref() {
                    self.release_browser(lease)?;
                }
                return Err(error);
            }
        };
        thread.set_status(AgentThreadStatus::Completed);
        thread.live_state.finish_run();
        if let Some(lease) = browser_lease.as_ref() {
            self.release_browser(lease)?;
        }

        Ok(RunAgentResponse {
            agent_id,
            session_id: request.session_id,
            run_id: request.run_id,
            final_status: AgentThreadStatus::Completed,
            final_result,
            usage: None,
            terminal_event_seq: terminal_append.seq,
            output,
        })
    }

    pub fn get_or_insert_session_resource<T, F, C>(
        &self,
        session_id: &SessionId,
        key: &str,
        init: F,
        cleanup: C,
    ) -> Result<Arc<T>>
    where
        T: Any + Send + Sync + 'static,
        F: FnOnce() -> T,
        C: Fn(Arc<T>) -> usize + Send + Sync + 'static,
    {
        self.inner
            .agents
            .thread_for_session(session_id)?
            .resources()
            .get_or_insert_with(key, init, cleanup)
    }

    pub fn try_get_or_insert_session_resource<T, F, C>(
        &self,
        session_id: &SessionId,
        key: &str,
        init: F,
        cleanup: C,
    ) -> Result<Arc<T>>
    where
        T: Any + Send + Sync + 'static,
        F: FnOnce() -> Result<T>,
        C: Fn(Arc<T>) -> usize + Send + Sync + 'static,
    {
        self.inner
            .agents
            .thread_for_session(session_id)?
            .resources()
            .try_get_or_insert_with(key, init, cleanup)
    }

    pub fn cleanup_session_resources(&self, session_id: &SessionId) -> Result<usize> {
        Ok(self
            .inner
            .agents
            .thread_for_session(session_id)?
            .resources()
            .cleanup_all())
    }

    pub fn snapshot(&self) -> RuntimeSnapshot {
        self.inner.snapshot()
    }

    pub fn snapshot_agent(&self, agent_id: &AgentId) -> Result<AgentThreadSnapshot> {
        Ok(self.inner.agents.thread(agent_id)?.snapshot())
    }

    pub fn agent_id_for_session(&self, session_id: &SessionId) -> Result<AgentId> {
        Ok(self
            .inner
            .agents
            .thread_for_session(session_id)?
            .agent_id()
            .clone())
    }

    pub fn subscribe_projected(&self) -> ProjectedRuntimeSubscription {
        self.inner.subscribe_projected()
    }

    pub fn subscribe_agent_projection(
        &self,
        agent_id: AgentId,
    ) -> Result<ProjectedAgentSubscription> {
        self.inner.subscribe_agent_projection(agent_id)
    }

    pub fn create_root_agent(&self, request: CreateRootAgentRequest) -> Result<Arc<AgentThread>> {
        self.inner.create_root_agent(request)
    }

    pub fn attach_root_agent(&self, request: AttachRootAgentRequest) -> Result<Arc<AgentThread>> {
        self.inner.attach_root_agent(request)
    }

    pub fn attach_child_agent(&self, request: AttachChildAgentRequest) -> Result<Arc<AgentThread>> {
        self.inner.attach_child_agent(request)
    }

    pub fn spawn_child(&self, request: SpawnChildRequest) -> Result<Arc<AgentThread>> {
        self.inner.spawn_child(request)
    }

    pub fn complete_agent(&self, request: CompleteAgentRequest) -> Result<()> {
        self.inner.complete_agent(request)
    }

    pub fn fail_agent(&self, request: FailAgentRequest) -> Result<()> {
        self.inner.fail_agent(request)
    }

    pub fn close_agent(&self, request: CloseAgentRequest) -> Result<()> {
        self.inner.close_agent(request)
    }

    pub fn send_agent_message(
        &self,
        request: SendAgentMessageRequest,
    ) -> Result<SendAgentMessageResponse> {
        self.inner.send_agent_message(request)
    }

    pub fn accept_prompt_input(
        &self,
        request: AcceptPromptInputRequest,
    ) -> Result<AcceptPromptInputResponse> {
        self.inner.accept_prompt_input(request)
    }

    pub fn consume_prompt_input_for_session(
        &self,
        session_id: &SessionId,
    ) -> Result<ConsumePromptInputResponse> {
        self.inner.consume_prompt_input_for_session(session_id)
    }

    pub fn submit_input(&self, request: SubmitInputRequest) -> Result<SubmitInputResponse> {
        self.inner.submit_input(request)
    }

    pub fn submit_followup(&self, request: SubmitInputRequest) -> Result<SubmitInputResponse> {
        self.inner.submit_input(request)
    }

    pub fn has_pending_agent_mail_for_session(
        &self,
        session_id: &SessionId,
        delivery_phase: MailboxDeliveryPhase,
    ) -> Result<bool> {
        self.inner
            .has_pending_agent_mail_for_session(session_id, delivery_phase)
    }

    pub fn pending_agent_mail_for_session(
        &self,
        session_id: &SessionId,
    ) -> Result<Vec<MailboxItem>> {
        self.inner.pending_agent_mail_for_session(session_id)
    }

    pub fn has_pending_trigger_turn_agent_mail_for_session(
        &self,
        session_id: &SessionId,
        delivery_phase: MailboxDeliveryPhase,
    ) -> Result<bool> {
        self.inner
            .has_pending_trigger_turn_agent_mail_for_session(session_id, delivery_phase)
    }

    pub fn drain_agent_mailbox(
        &self,
        request: DrainAgentMailboxRequest,
    ) -> Result<DrainAgentMailboxResponse> {
        self.inner.drain_agent_mailbox(request)
    }

    pub async fn wait_agent(
        &self,
        parent_agent_id: &AgentId,
        target: AgentTarget,
        timeout: Duration,
    ) -> Result<WaitAgentOutcome> {
        self.inner
            .wait_agent(parent_agent_id, target, timeout)
            .await
    }
}

pub struct BrowserUseRuntime {
    events: RuntimeEventBus,
    persistence: Arc<dyn LiveThreadPersistence>,
    state_index: Arc<dyn StateIndex>,
    agents: AgentManager,
    browsers: BrowserManager,
    active_runs: ActiveRunRegistry,
}

impl BrowserUseRuntime {
    pub fn new(
        persistence: Arc<dyn LiveThreadPersistence>,
        state_index: Arc<dyn StateIndex>,
    ) -> Self {
        Self {
            events: RuntimeEventBus::default(),
            persistence,
            state_index,
            agents: AgentManager::new(),
            browsers: BrowserManager::new(),
            active_runs: ActiveRunRegistry::new(),
        }
    }

    pub fn memory() -> (Self, Arc<MemoryJournal>) {
        let memory = Arc::new(MemoryJournal::new());
        let persistence: Arc<dyn LiveThreadPersistence> = memory.clone();
        let state_index: Arc<dyn StateIndex> = memory.clone();
        (Self::new(persistence, state_index), memory)
    }

    pub fn handle(self) -> RuntimeHandle {
        RuntimeHandle::new(self)
    }

    pub fn events(&self) -> &RuntimeEventBus {
        &self.events
    }

    pub fn agents(&self) -> &AgentManager {
        &self.agents
    }

    pub fn browsers(&self) -> &BrowserManager {
        &self.browsers
    }

    pub fn snapshot(&self) -> RuntimeSnapshot {
        RuntimeSnapshot {
            agents: self.agents.snapshots(),
            agent_controls: self.agents.control_snapshots(),
            browsers: self.browsers.snapshots(),
        }
    }

    pub fn subscribe_projected(&self) -> ProjectedRuntimeSubscription {
        let rx = self.events.subscribe();
        let snapshot = self.snapshot();
        ProjectedRuntimeSubscription {
            projection: RuntimeProjectionState::new(snapshot),
            rx,
        }
    }

    pub fn subscribe_agent_projection(
        &self,
        agent_id: AgentId,
    ) -> Result<ProjectedAgentSubscription> {
        let thread = self.agents.thread(&agent_id)?;
        let snapshot = self.snapshot();
        Ok(ProjectedAgentSubscription {
            agent_id,
            snapshot: thread.snapshot(),
            projection: RuntimeProjectionState::new(snapshot),
            rx: self.events.subscribe(),
        })
    }

    pub fn publish_after_barrier(&self, event: RuntimeEvent) -> Result<JournalAppend> {
        let append = self.persistence.append_runtime_event(&event)?;
        self.events.publish(event);
        Ok(append)
    }

    fn append_runtime_event(&self, event: &RuntimeEvent) -> Result<JournalAppend> {
        self.persistence.append_runtime_event(event)
    }

    pub fn create_root_agent(&self, request: CreateRootAgentRequest) -> Result<Arc<AgentThread>> {
        let cwd = request.cwd;
        let session = self.persistence.create_thread(CreateThreadRequest {
            session_id: None,
            parent_session_id: None,
            cwd: cwd.clone(),
            artifact_root: None,
            agent_path: None,
            nickname: None,
            role: None,
        })?;
        let session_id = SessionId::from_string(session.id)?;
        self.insert_root_agent(
            session_id,
            cwd,
            request.task,
            request.max_concurrent_threads_per_session,
            Some(RuntimeEventKind::AgentCreated),
        )
    }

    pub fn attach_root_agent(&self, request: AttachRootAgentRequest) -> Result<Arc<AgentThread>> {
        self.persistence
            .load_session(&request.session_id)?
            .with_context(|| format!("unknown root session id: {}", request.session_id))?;
        let thread = self.insert_root_agent(
            request.session_id,
            request.cwd,
            request.task,
            request.max_concurrent_threads_per_session,
            Some(RuntimeEventKind::AgentResumed),
        )?;
        self.materialize_live_state_after_resume(&thread)?;
        self.record_lost_resources_after_resume(&thread)?;
        self.materialize_descendants_after_resume(&thread)?;
        Ok(thread)
    }

    pub fn attach_child_agent(&self, request: AttachChildAgentRequest) -> Result<Arc<AgentThread>> {
        self.persistence
            .load_session(&request.child_session_id)?
            .with_context(|| format!("unknown child session id: {}", request.child_session_id))?;
        let parent = self.agents.thread(&request.parent_agent_id)?;
        let control = self.agents.control_for_thread(&parent)?;
        match control
            .scheduler
            .admit_spawn(request.child_agent_id.clone(), request.agent_path.clone())
        {
            Ok(SpawnAdmission::Reserved(_)) => {}
            Ok(SpawnAdmission::Queued(_)) => {
                bail!("queued subagent mode is not enabled for BrowserUseRuntime AgentControl");
            }
            Err(err) => return Err(err),
        }
        let child = Arc::new(AgentThread::new(
            request.child_agent_id.clone(),
            request.child_session_id.clone(),
            parent.root_id.clone(),
            request.cwd,
            Some(parent.agent_id.clone()),
            Some(parent.session_id.clone()),
            request.agent_path.clone(),
            request.nickname.clone(),
            request.role.clone(),
        ));
        self.agents.insert_thread(child.clone());
        let mut event = RuntimeEvent::new(RuntimeEventKind::AgentResumed, Durability::Barrier)
            .with_session_id(request.child_session_id)
            .with_agent_id(request.child_agent_id)
            .with_payload(json!({
                "agent_path": request.agent_path,
                "nickname": request.nickname,
                "role": request.role,
            }));
        event.root_id = Some(parent.root_id.clone());
        self.publish_after_barrier(event)?;
        self.materialize_live_state_after_resume(&child)?;
        self.record_lost_resources_after_resume(&child)?;
        self.materialize_descendants_after_resume(&child)?;
        Ok(child)
    }

    fn insert_root_agent(
        &self,
        session_id: SessionId,
        cwd: PathBuf,
        task: String,
        max_concurrent_threads_per_session: usize,
        event_kind: Option<RuntimeEventKind>,
    ) -> Result<Arc<AgentThread>> {
        let root_id = RootId::from_string(session_id.as_str())?;
        let agent_id = AgentId::from_string(session_id.as_str())?;
        let thread = Arc::new(AgentThread::new(
            agent_id.clone(),
            session_id.clone(),
            root_id.clone(),
            cwd,
            None,
            None,
            "/root".to_string(),
            None,
            Some("default".to_string()),
        ));
        self.agents.insert_thread(thread.clone());
        self.agents.insert_control(Arc::new(AgentControl::new(
            root_id.clone(),
            agent_id.clone(),
            session_id.clone(),
            max_concurrent_threads_per_session,
        )));
        let materialized_from_replay = matches!(event_kind, Some(RuntimeEventKind::AgentResumed));
        let mut event = RuntimeEvent::new(
            event_kind.unwrap_or(RuntimeEventKind::AgentCreated),
            Durability::Barrier,
        )
        .with_session_id(session_id)
        .with_agent_id(agent_id)
        .with_payload(json!({
            "agent_path": "/root",
            "task": task,
            "role": "default",
            "materialized_from_replay": materialized_from_replay,
        }));
        event.root_id = Some(root_id);
        self.publish_after_barrier(event)?;
        Ok(thread)
    }

    pub fn spawn_child(&self, request: SpawnChildRequest) -> Result<Arc<AgentThread>> {
        let parent = self.agents.thread(&request.parent_agent_id)?;
        let control = self.agents.control_for_thread(&parent)?;
        let child_agent_id = request.child_agent_id.clone().unwrap_or_default();
        let child_path = child_agent_path(&parent.agent_path, &request.task_name);
        match control
            .scheduler
            .admit_spawn(child_agent_id.clone(), request.task_name.clone())
        {
            Ok(SpawnAdmission::Reserved(_)) => {}
            Ok(SpawnAdmission::Queued(queued)) => {
                let mut event =
                    RuntimeEvent::new(RuntimeEventKind::SubagentSpawnQueued, Durability::Barrier)
                        .with_session_id(parent.session_id.clone())
                        .with_agent_id(parent.agent_id.clone())
                        .with_payload(json!({
                            "child_agent_id": queued.child_agent_id.as_str(),
                            "task_name": queued.task_name,
                        }));
                event.root_id = Some(parent.root_id.clone());
                self.publish_after_barrier(event)?;
                bail!("queued subagent mode is not enabled for BrowserUseRuntime AgentControl");
            }
            Err(err) => {
                let mut event =
                    RuntimeEvent::new(RuntimeEventKind::SubagentSpawnRejected, Durability::Barrier)
                        .with_session_id(parent.session_id.clone())
                        .with_agent_id(parent.agent_id.clone())
                        .with_payload(json!({
                            "task_name": request.task_name,
                            "reason": err.to_string(),
                        }));
                event.root_id = Some(parent.root_id.clone());
                self.publish_after_barrier(event)?;
                return Err(err);
            }
        }

        let child_session = self.persistence.create_thread(CreateThreadRequest {
            session_id: request.child_session_id.clone(),
            parent_session_id: Some(parent.session_id.clone()),
            cwd: parent.cwd.clone(),
            artifact_root: None,
            agent_path: Some(child_path.clone()),
            nickname: request.nickname.clone(),
            role: request.role.clone(),
        })?;
        let child_session_id = SessionId::from_string(child_session.id)?;
        let child = Arc::new(AgentThread::new(
            child_agent_id.clone(),
            child_session_id.clone(),
            parent.root_id.clone(),
            parent.cwd.clone(),
            Some(parent.agent_id.clone()),
            Some(parent.session_id.clone()),
            child_path.clone(),
            request.nickname.clone(),
            request.role.clone(),
        ));
        let mut spawn_event =
            RuntimeEvent::new(RuntimeEventKind::SubagentSpawnStarted, Durability::Barrier)
                .with_session_id(parent.session_id.clone())
                .with_agent_id(parent.agent_id.clone())
                .with_payload(json!({
                    "child_agent_id": child_agent_id.as_str(),
                    "child_session_id": child_session_id.as_str(),
                    "agent_path": child_path,
                    "task_name": request.task_name,
                    "message": request.message,
                    "nickname": request.nickname,
                    "role": request.role,
                }));
        spawn_event.root_id = Some(parent.root_id.clone());
        self.publish_after_barrier(spawn_event)?;
        self.agents.insert_thread(child.clone());
        Ok(child)
    }

    pub fn complete_agent(&self, request: CompleteAgentRequest) -> Result<()> {
        self.finish_child_agent(request.child_agent_id, request.result, true)
    }

    pub fn fail_agent(&self, request: FailAgentRequest) -> Result<()> {
        self.finish_child_agent(request.child_agent_id, request.error, false)
    }

    fn finish_child_agent(
        &self,
        child_agent_id: AgentId,
        terminal_text: String,
        success: bool,
    ) -> Result<()> {
        let child = self.agents.thread(&child_agent_id)?;
        let parent_agent_id = child
            .parent_agent_id
            .clone()
            .ok_or_else(|| RuntimeError::MissingParentAgent(child.agent_id.as_str().to_string()))?;
        let parent = self.agents.thread(&parent_agent_id)?;

        let terminal_event_kind = if success {
            RuntimeEventKind::AgentCompleted
        } else {
            RuntimeEventKind::AgentFailed
        };
        let status = if success { "done" } else { "failed" };
        let result = success.then(|| terminal_text.clone());
        let failure = (!success).then(|| terminal_text.clone());
        let parent_payload = json!({
            "child_session_id": child.session_id.as_str(),
            "status": status,
            "runtime_owned": true,
            "payload": {
                "child_session_id": child.session_id.as_str(),
                "status": status,
                "result": result,
                "failure": failure,
                "runtime_owned": true,
            },
        });
        let mut completed = RuntimeEvent::new(terminal_event_kind.clone(), Durability::Barrier)
            .with_session_id(child.session_id.clone())
            .with_agent_id(child.agent_id.clone())
            .with_payload(json!({
                "success": success,
                "result": terminal_text.clone(),
                "agent_path": child.agent_path.as_str(),
                "runtime_owned": true,
            }));
        completed.root_id = Some(child.root_id.clone());

        let mut parent_terminal = RuntimeEvent::new(terminal_event_kind, Durability::Barrier)
            .with_session_id(parent.session_id.clone())
            .with_agent_id(parent.agent_id.clone())
            .with_payload(parent_payload.clone());
        parent_terminal.root_id = Some(parent.root_id.clone());

        let notification_status = if success {
            json!({ "completed": terminal_text.clone() })
        } else {
            json!({ "errored": terminal_text.clone() })
        };
        let notification = format!(
            "<subagent_notification>\n{}\n</subagent_notification>",
            json!({
                "agent_path": child.agent_path.as_str(),
                "status": notification_status,
            })
        );
        let mailbox_item = parent.mailbox.prepare_item(MailboxItem {
            seq: 0,
            id: Uuid::new_v4().simple().to_string(),
            kind: MailboxItemKind::Completion,
            author_agent_id: child.agent_id.clone(),
            target_agent_id: parent.agent_id.clone(),
            target_path: Some(child.agent_path.clone()),
            content: notification,
            trigger_turn: false,
            delivery_phase: MailboxDeliveryPhase::CurrentTurn,
            payload: json!({
                "success": success,
                "author_session_id": child.session_id.as_str(),
                "target_session_id": parent.session_id.as_str(),
                "child_session_id": child.session_id.as_str(),
                "agent_path": child.agent_path.as_str(),
                "result": terminal_text,
                "runtime_owned": true,
            }),
        });
        let mut mailbox_event =
            RuntimeEvent::new(RuntimeEventKind::MailboxEnqueued, Durability::Barrier)
                .with_session_id(parent.session_id.clone())
                .with_agent_id(parent.agent_id.clone())
                .with_payload(json!({
                    "mailbox_item": mailbox_item,
                    "trigger_turn": false,
                }));
        mailbox_event.root_id = Some(parent.root_id.clone());
        self.append_runtime_event(&mailbox_event)?;
        self.append_runtime_event(&completed)?;
        self.append_runtime_event(&parent_terminal)?;
        child.set_status(if success {
            AgentThreadStatus::Completed
        } else {
            AgentThreadStatus::Failed
        });
        self.state_index.finish_spawn_edge(
            &child.session_id,
            if success {
                SpawnEdgeStatus::Done
            } else {
                SpawnEdgeStatus::Failed
            },
        )?;
        parent.mailbox.enqueue_prepared(mailbox_item.clone());
        parent.live_state.record_mailbox_enqueued(&mailbox_item);
        self.events.publish(completed);
        self.events.publish(parent_terminal);
        self.events.publish(mailbox_event);
        Ok(())
    }

    pub fn close_agent(&self, request: CloseAgentRequest) -> Result<()> {
        let thread = self.agents.thread(&request.agent_id)?;
        let reason = request.reason.clone();
        let descendant_edges = self.state_index.list_descendants(&thread.session_id)?;
        let mut edge_session_ids_to_close: HashSet<SessionId> = descendant_edges
            .iter()
            .map(|edge| edge.child_session_id.clone())
            .collect();
        if thread.parent_agent_id.is_some() {
            edge_session_ids_to_close.insert(thread.session_id.clone());
        }
        let mut threads = vec![thread.clone()];
        let mut seen_sessions = HashSet::from([thread.session_id.clone()]);
        for edge in descendant_edges {
            if let Ok(descendant) = self.agents.thread_for_session(&edge.child_session_id) {
                if seen_sessions.insert(descendant.session_id.clone()) {
                    threads.push(descendant);
                }
            }
        }
        let mut closed_live_edge_sessions = HashSet::new();
        for closing in threads.iter().rev() {
            let mut requested =
                RuntimeEvent::new(RuntimeEventKind::AgentCloseRequested, Durability::Barrier)
                    .with_session_id(closing.session_id.clone())
                    .with_agent_id(closing.agent_id.clone())
                    .with_payload(json!({
                        "reason": reason.clone(),
                        "agent_path": closing.agent_path.as_str(),
                    }));
            requested.root_id = Some(closing.root_id.clone());
            self.publish_after_barrier(requested)?;

            let cancelled = self.active_runs.cancel(&closing.session_id);
            if cancelled {
                closing.live_state.request_cancel();
            }
            let cleaned_resources = closing.resources.cleanup_all();
            let mut event = RuntimeEvent::new(RuntimeEventKind::AgentClosed, Durability::Barrier)
                .with_session_id(closing.session_id.clone())
                .with_agent_id(closing.agent_id.clone())
                .with_payload(json!({
                    "reason": reason.clone(),
                    "agent_path": closing.agent_path.as_str(),
                    "cleaned_resources": cleaned_resources,
                    "cancelled_active_run": cancelled,
                }));
            event.root_id = Some(closing.root_id.clone());
            self.append_runtime_event(&event)?;
            if closing.parent_agent_id.is_some() {
                let control = self.agents.control_for_thread(closing)?;
                control.scheduler.close_spawned_agent(&closing.agent_id);
                self.state_index
                    .close_spawn_edge(&closing.session_id, &reason)?;
                closed_live_edge_sessions.insert(closing.session_id.clone());
            }
            closing.set_status(AgentThreadStatus::Closed);
            closing.live_state.close();
            self.events.publish(event);
        }
        for child_session_id in edge_session_ids_to_close {
            if !closed_live_edge_sessions.contains(&child_session_id) {
                self.state_index
                    .close_spawn_edge(&child_session_id, &reason)?;
            }
        }
        if let Some(parent_agent_id) = thread.parent_agent_id.as_ref() {
            let parent = self.agents.thread(parent_agent_id)?;
            let payload = json!({
                "child_session_id": thread.session_id.as_str(),
                "status": "cancelled",
                "payload": { "reason": reason.clone() },
                "agent_path": thread.agent_path.as_str(),
            });
            self.persistence.append_session_event(
                &parent.session_id,
                RuntimeEventKind::AgentCancelled.as_str(),
                payload.clone(),
                Durability::Barrier,
            )?;
            let mut parent_event =
                RuntimeEvent::new(RuntimeEventKind::AgentCancelled, Durability::Barrier)
                    .with_session_id(parent.session_id.clone())
                    .with_agent_id(parent.agent_id.clone())
                    .with_payload(payload);
            parent_event.root_id = Some(parent.root_id.clone());
            self.events.publish(parent_event);
        }
        Ok(())
    }

    pub fn send_agent_message(
        &self,
        request: SendAgentMessageRequest,
    ) -> Result<SendAgentMessageResponse> {
        let content = request.content.trim();
        if content.is_empty() {
            bail!("Empty message can't be sent to an agent");
        }
        let author = self.agents.thread(&request.author_agent_id)?;
        let target = self.agents.thread(&request.target_agent_id)?;
        let is_user_followup_to_self = request.kind == MailboxItemKind::Followup
            && request.author_agent_id == request.target_agent_id;
        if request.trigger_turn && target.parent_agent_id.is_none() && !is_user_followup_to_self {
            bail!("Tasks can't be assigned to the root agent");
        }
        let payload = enrich_mailbox_payload(
            request.payload,
            &author.session_id,
            &target.session_id,
            &author.agent_path,
            &target.agent_path,
        );
        let mailbox_item = target.mailbox.prepare_item(MailboxItem {
            seq: 0,
            id: Uuid::new_v4().simple().to_string(),
            kind: request.kind,
            author_agent_id: author.agent_id.clone(),
            target_agent_id: target.agent_id.clone(),
            target_path: Some(target.agent_path.clone()),
            content: content.to_string(),
            trigger_turn: request.trigger_turn,
            delivery_phase: request.delivery_phase,
            payload,
        });
        let mut mailbox_event =
            RuntimeEvent::new(RuntimeEventKind::MailboxEnqueued, Durability::Barrier)
                .with_session_id(target.session_id.clone())
                .with_agent_id(target.agent_id.clone())
                .with_payload(json!({
                    "mailbox_item": mailbox_item,
                    "trigger_turn": request.trigger_turn,
                    "author_agent_id": author.agent_id.as_str(),
                    "target_agent_id": target.agent_id.as_str(),
                }));
        mailbox_event.root_id = Some(target.root_id.clone());
        self.append_runtime_event(&mailbox_event)?;
        target.mailbox.enqueue_prepared(mailbox_item.clone());
        target.live_state.record_mailbox_enqueued(&mailbox_item);
        self.events.publish(mailbox_event);
        Ok(SendAgentMessageResponse { mailbox_item })
    }

    pub fn accept_prompt_input(
        &self,
        mut request: AcceptPromptInputRequest,
    ) -> Result<AcceptPromptInputResponse> {
        let target = self.agents.thread(&request.target_agent_id)?;
        if target
            .live_state
            .has_accepted_prompt_input_seq(request.source_event_seq)
        {
            return Ok(AcceptPromptInputResponse { accepted: false });
        }
        if !request.payload.is_object() {
            request.payload = json!({});
        }
        if let Some(obj) = request.payload.as_object_mut() {
            obj.insert(
                "target_session_id".to_string(),
                json!(target.session_id.as_str()),
            );
            if let Some(source_event_seq) = request.source_event_seq {
                obj.insert("source_event_seq".to_string(), json!(source_event_seq));
            }
        }
        let mut event =
            RuntimeEvent::new(RuntimeEventKind::AgentInputAccepted, Durability::Barrier)
                .with_session_id(target.session_id.clone())
                .with_agent_id(target.agent_id.clone())
                .with_payload(json!({
                    "runtime_owned": true,
                    "source_event_seq": request.source_event_seq,
                    "payload": request.payload,
                }));
        event.root_id = Some(target.root_id.clone());
        self.append_runtime_event(&event)?;
        target
            .live_state
            .record_prompt_input_accepted(request.source_event_seq);
        self.events.publish(event);
        Ok(AcceptPromptInputResponse { accepted: true })
    }

    pub fn consume_prompt_input_for_session(
        &self,
        session_id: &SessionId,
    ) -> Result<ConsumePromptInputResponse> {
        let thread = self.agents.thread_for_session(session_id)?;
        if !thread.live_state.has_pending_prompt_input() {
            return Ok(ConsumePromptInputResponse { consumed: false });
        }
        let live = thread.live_state.snapshot();
        let mut event =
            RuntimeEvent::new(RuntimeEventKind::AgentInputConsumed, Durability::Barrier)
                .with_session_id(thread.session_id.clone())
                .with_agent_id(thread.agent_id.clone())
                .with_payload(json!({
                    "runtime_owned": true,
                    "source_event_seq": live.last_accepted_prompt_input_seq,
                }));
        event.root_id = Some(thread.root_id.clone());
        self.append_runtime_event(&event)?;
        let consumed = thread.live_state.record_prompt_input_consumed();
        if consumed {
            self.events.publish(event);
        }
        Ok(ConsumePromptInputResponse { consumed })
    }

    pub fn submit_input(&self, mut request: SubmitInputRequest) -> Result<SubmitInputResponse> {
        let target = self.agents.thread(&request.target_agent_id)?;
        if !request.payload.is_object() {
            request.payload = json!({});
        }
        if let Some(obj) = request.payload.as_object_mut() {
            obj.entry("source".to_string())
                .or_insert_with(|| json!("user_input"));
            obj.insert(
                "target_session_id".to_string(),
                json!(target.session_id.as_str()),
            );
            if let Some(input_items) = request.input_items.take() {
                obj.insert("input_items".to_string(), input_items);
            }
        }
        let response = self.send_agent_message(SendAgentMessageRequest {
            author_agent_id: target.agent_id.clone(),
            target_agent_id: target.agent_id.clone(),
            content: request.content,
            trigger_turn: request.trigger_turn,
            kind: MailboxItemKind::Followup,
            delivery_phase: request.delivery_phase,
            payload: request.payload,
        })?;
        target
            .live_state
            .record_accepted_input(&response.mailbox_item);
        Ok(SubmitInputResponse {
            mailbox_item: response.mailbox_item,
        })
    }

    pub fn has_pending_agent_mail_for_session(
        &self,
        session_id: &SessionId,
        delivery_phase: MailboxDeliveryPhase,
    ) -> Result<bool> {
        Ok(self
            .agents
            .thread_for_session(session_id)?
            .mailbox
            .has_pending_phase(delivery_phase))
    }

    pub fn pending_agent_mail_for_session(
        &self,
        session_id: &SessionId,
    ) -> Result<Vec<MailboxItem>> {
        Ok(self
            .agents
            .thread_for_session(session_id)?
            .mailbox
            .pending_items())
    }

    pub fn has_pending_trigger_turn_agent_mail_for_session(
        &self,
        session_id: &SessionId,
        delivery_phase: MailboxDeliveryPhase,
    ) -> Result<bool> {
        Ok(self
            .agents
            .thread_for_session(session_id)?
            .mailbox
            .has_pending_trigger_turn_phase(delivery_phase))
    }

    pub fn drain_agent_mailbox(
        &self,
        request: DrainAgentMailboxRequest,
    ) -> Result<DrainAgentMailboxResponse> {
        let thread = self.agents.thread_for_session(&request.session_id)?;
        let items = thread
            .mailbox
            .pending_items_for_phase(request.delivery_phase);
        if !items.is_empty() {
            let mut delivered =
                RuntimeEvent::new(RuntimeEventKind::MailboxDelivered, Durability::Barrier)
                    .with_session_id(thread.session_id.clone())
                    .with_agent_id(thread.agent_id.clone())
                    .with_payload(json!({
                        "delivery_phase": request.delivery_phase,
                        "count": items.len(),
                        "mailbox_seqs": items.iter().map(|item| item.seq).collect::<Vec<_>>(),
                        "mailbox_items": items.clone(),
                    }));
            delivered.root_id = Some(thread.root_id.clone());

            let mut consumed =
                RuntimeEvent::new(RuntimeEventKind::MailboxConsumed, Durability::Barrier)
                    .with_session_id(thread.session_id.clone())
                    .with_agent_id(thread.agent_id.clone())
                    .with_payload(json!({
                        "delivery_phase": request.delivery_phase,
                        "count": items.len(),
                        "mailbox_seqs": items.iter().map(|item| item.seq).collect::<Vec<_>>(),
                        "mailbox_items": items.clone(),
                    }));
            consumed.root_id = Some(thread.root_id.clone());

            self.append_runtime_event(&delivered)?;
            self.append_runtime_event(&consumed)?;
            let drained = thread.mailbox.drain_phase(request.delivery_phase);
            thread.live_state.record_mailbox_delivered(&drained);
            thread.live_state.record_mailbox_consumed(&drained);
            self.events.publish(delivered);
            self.events.publish(consumed);
        }
        Ok(DrainAgentMailboxResponse {
            mailbox_items: items,
        })
    }

    pub async fn wait_agent(
        &self,
        parent_agent_id: &AgentId,
        target: AgentTarget,
        timeout: Duration,
    ) -> Result<WaitAgentOutcome> {
        let parent = self.agents.thread(parent_agent_id)?;
        let mut started =
            RuntimeEvent::new(RuntimeEventKind::WaitAgentStarted, Durability::BestEffort)
                .with_session_id(parent.session_id.clone())
                .with_agent_id(parent.agent_id.clone())
                .with_payload(json!({ "timeout_ms": timeout.as_millis() as u64 }));
        started.root_id = Some(parent.root_id.clone());
        self.publish_after_barrier(started)?;

        let after_seq = parent.live_state.snapshot().last_wait_observed_mailbox_seq;
        let outcome = parent
            .mailbox
            .wait_for_item(target, after_seq, timeout)
            .await?;
        let (kind, payload) = match &outcome {
            WaitAgentOutcome::Completed(item) => (
                RuntimeEventKind::WaitAgentCompleted,
                json!({
                    "timed_out": false,
                    "mailbox_seq": item.seq,
                    "after_seq": after_seq,
                    "author_agent_id": item.author_agent_id.as_str(),
                }),
            ),
            WaitAgentOutcome::TimedOut => (
                RuntimeEventKind::WaitAgentTimedOut,
                json!({
                    "timed_out": true,
                    "after_seq": after_seq,
                }),
            ),
        };
        let mut finished = RuntimeEvent::new(kind, Durability::Barrier)
            .with_session_id(parent.session_id.clone())
            .with_agent_id(parent.agent_id.clone())
            .with_payload(payload);
        finished.root_id = Some(parent.root_id.clone());
        self.publish_after_barrier(finished)?;
        if let WaitAgentOutcome::Completed(item) = &outcome {
            parent.live_state.record_wait_observed_mailbox(item);
        }
        Ok(outcome)
    }

    fn record_lost_resources_after_resume(&self, thread: &AgentThread) -> Result<()> {
        let events = self.persistence.events_for_session(&thread.session_id)?;
        let lost = lost_live_resources_from_events(&events);
        for resource in lost {
            let mut event = RuntimeEvent::new(RuntimeEventKind::ResourceLost, Durability::Barrier)
                .with_session_id(thread.session_id.clone())
                .with_agent_id(thread.agent_id.clone())
                .with_payload(json!({
                    "resource": {
                        "kind": resource.kind,
                        "id": resource.id,
                    },
                    "reason": "resume_without_live_handle",
                }));
            event.root_id = Some(thread.root_id.clone());
            self.publish_after_barrier(event)?;
        }
        Ok(())
    }

    fn materialize_live_state_after_resume(&self, thread: &AgentThread) -> Result<()> {
        let events = self.persistence.events_for_session(&thread.session_id)?;
        let replay = materialized_live_state_from_events(&events);
        thread
            .mailbox
            .materialize_pending(replay.pending_mailbox_items.clone());
        thread.live_state.materialize_from_replay(&replay);
        Ok(())
    }

    fn materialize_descendants_after_resume(&self, root: &AgentThread) -> Result<()> {
        let mut remaining = self.state_index.list_descendants(&root.session_id)?;
        while !remaining.is_empty() {
            let before = remaining.len();
            let mut deferred = Vec::new();
            for edge in remaining {
                if self
                    .agents
                    .thread_for_session(&edge.child_session_id)
                    .is_ok()
                {
                    continue;
                }
                let Ok(parent) = self.agents.thread_for_session(&edge.parent_session_id) else {
                    deferred.push(edge);
                    continue;
                };
                self.materialize_child_edge_after_resume(&parent, edge)?;
            }
            if deferred.len() == before {
                let missing = deferred
                    .iter()
                    .map(|edge| {
                        format!(
                            "{} -> {}",
                            edge.parent_session_id.as_str(),
                            edge.child_session_id.as_str()
                        )
                    })
                    .collect::<Vec<_>>()
                    .join(", ");
                bail!("could not materialize durable child edges without parents: {missing}");
            }
            remaining = deferred;
        }
        Ok(())
    }

    fn materialize_child_edge_after_resume(
        &self,
        parent: &AgentThread,
        edge: SpawnEdge,
    ) -> Result<Arc<AgentThread>> {
        let session = self
            .persistence
            .load_session(&edge.child_session_id)?
            .with_context(|| format!("unknown child session id: {}", edge.child_session_id))?;
        let child_agent_id = AgentId::from_string(edge.child_session_id.as_str())?;
        let agent_path = edge.path.clone().unwrap_or_else(|| {
            child_agent_path(&parent.agent_path, edge.child_session_id.as_str())
        });
        let child = Arc::new(AgentThread::new(
            child_agent_id.clone(),
            edge.child_session_id.clone(),
            parent.root_id.clone(),
            PathBuf::from(session.cwd),
            Some(parent.agent_id.clone()),
            Some(parent.session_id.clone()),
            agent_path.clone(),
            edge.nickname.clone(),
            edge.role.clone(),
        ));
        child.set_status(edge.status.as_thread_status());
        self.agents.insert_thread(child.clone());
        if edge.status != SpawnEdgeStatus::Closed {
            let control = self.agents.control_for_thread(parent)?;
            control
                .scheduler
                .materialize_open_spawned_agent(child_agent_id.clone(), agent_path.clone());
        }
        let mut event = RuntimeEvent::new(RuntimeEventKind::AgentResumed, Durability::Barrier)
            .with_session_id(edge.child_session_id)
            .with_agent_id(child_agent_id)
            .with_payload(json!({
                "agent_path": agent_path,
                "nickname": edge.nickname,
                "role": edge.role,
                "status": edge.status,
                "materialized_from_replay": true,
            }));
        event.root_id = Some(parent.root_id.clone());
        self.publish_after_barrier(event)?;
        self.materialize_live_state_after_resume(&child)?;
        self.record_lost_resources_after_resume(&child)?;
        Ok(child)
    }
}

pub fn local_runtime_socket_path(state_dir: &Path) -> PathBuf {
    let mut hash = 0xcbf29ce484222325_u64;
    for byte in state_dir.to_string_lossy().as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    std::env::temp_dir().join(format!("browser-use-runtime-{hash:016x}.sock"))
}

pub fn handle_local_runtime_request(
    runtime: &RuntimeHandle,
    request: LocalRuntimeRequest,
) -> Result<Value> {
    match request {
        LocalRuntimeRequest::Ping => Ok(json!({ "ok": true })),
        LocalRuntimeRequest::PendingAgentMail { session_id } => {
            let session_id = SessionId::from_string(session_id)?;
            let items = runtime.pending_agent_mail_for_session(&session_id)?;
            Ok(json!({
                "count": items.len(),
                "items": items,
            }))
        }
        LocalRuntimeRequest::SpawnChild {
            parent_agent_id,
            child_agent_id,
            child_session_id,
            task_name,
            message,
            nickname,
            role,
        } => {
            let child = runtime.spawn_child(SpawnChildRequest {
                parent_agent_id: AgentId::from_string(parent_agent_id)?,
                child_agent_id: child_agent_id.map(AgentId::from_string).transpose()?,
                child_session_id: child_session_id.map(SessionId::from_string).transpose()?,
                task_name,
                message,
                nickname,
                role,
            })?;
            Ok(json!({
                "agent": child.snapshot(),
            }))
        }
        LocalRuntimeRequest::SendAgentMessage {
            author_agent_id,
            target_agent_id,
            content,
            trigger_turn,
            kind,
            delivery_phase,
            payload,
        } => {
            let response = runtime.send_agent_message(SendAgentMessageRequest {
                author_agent_id: AgentId::from_string(author_agent_id)?,
                target_agent_id: AgentId::from_string(target_agent_id)?,
                content,
                trigger_turn,
                kind,
                delivery_phase,
                payload,
            })?;
            Ok(json!({
                "mailbox_item": response.mailbox_item,
            }))
        }
        LocalRuntimeRequest::SubmitUserInput {
            session_id,
            content,
            trigger_turn,
            delivery_phase,
            input_items,
            payload,
        } => {
            let agent_id = AgentId::from_string(session_id.clone())?;
            let response = runtime.submit_input(SubmitInputRequest {
                target_agent_id: agent_id,
                content,
                trigger_turn,
                delivery_phase,
                input_items,
                payload,
            })?;
            Ok(json!({
                "mailbox_item": response.mailbox_item,
            }))
        }
        LocalRuntimeRequest::WaitAgent {
            parent_agent_id,
            target,
            timeout_ms,
        } => {
            let parent_agent_id = AgentId::from_string(parent_agent_id)?;
            let target = match target.unwrap_or(LocalRuntimeWaitTarget::Any) {
                LocalRuntimeWaitTarget::Any => AgentTarget::Any,
                LocalRuntimeWaitTarget::AgentId(agent_id) => {
                    AgentTarget::AgentId(AgentId::from_string(agent_id)?)
                }
                LocalRuntimeWaitTarget::Path(path) => AgentTarget::Path(path),
            };
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_time()
                .build()
                .context("build local runtime wait loop")?;
            let outcome = rt.block_on(runtime.wait_agent(
                &parent_agent_id,
                target,
                Duration::from_millis(timeout_ms),
            ))?;
            match outcome {
                WaitAgentOutcome::Completed(item) => Ok(json!({
                    "timed_out": false,
                    "mailbox_item": item,
                })),
                WaitAgentOutcome::TimedOut => Ok(json!({
                    "timed_out": true,
                })),
            }
        }
        LocalRuntimeRequest::CancelRun { session_id } => {
            let cancelled = runtime.request_cancel_run(&SessionId::from_string(session_id)?)?;
            Ok(json!({ "cancelled": cancelled }))
        }
        LocalRuntimeRequest::CloseAgent { agent_id, reason } => {
            runtime.close_agent(CloseAgentRequest {
                agent_id: AgentId::from_string(agent_id)?,
                reason,
            })?;
            Ok(json!({ "closed": true }))
        }
    }
}

pub fn local_runtime_response(result: Result<Value>) -> LocalRuntimeResponse {
    match result {
        Ok(result) => LocalRuntimeResponse {
            ok: true,
            result,
            error: None,
        },
        Err(error) => LocalRuntimeResponse {
            ok: false,
            result: Value::Null,
            error: Some(error.to_string()),
        },
    }
}

#[cfg(unix)]
pub fn send_local_runtime_request(
    state_dir: &Path,
    request: &LocalRuntimeRequest,
    timeout: Duration,
) -> Result<Option<LocalRuntimeResponse>> {
    let socket_path = local_runtime_socket_path(state_dir);
    if !socket_path.exists() {
        return Ok(None);
    }
    let mut stream = match UnixStream::connect(&socket_path) {
        Ok(stream) => stream,
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::NotFound
                    | std::io::ErrorKind::ConnectionRefused
                    | std::io::ErrorKind::AddrNotAvailable
            ) =>
        {
            return Ok(None);
        }
        Err(error) => return Err(error).context("connect local runtime socket"),
    };
    stream
        .set_read_timeout(Some(timeout))
        .context("set local runtime read timeout")?;
    stream
        .set_write_timeout(Some(timeout))
        .context("set local runtime write timeout")?;
    writeln!(stream, "{}", serde_json::to_string(request)?)
        .context("write local runtime request")?;
    stream.flush().context("flush local runtime request")?;
    let mut reader = BufReader::new(stream);
    let mut response = String::new();
    reader
        .read_line(&mut response)
        .context("read local runtime response")?;
    if response.trim().is_empty() {
        bail!("local runtime socket closed without a response");
    }
    serde_json::from_str(response.trim()).context("parse local runtime response")
}

#[cfg(not(unix))]
pub fn send_local_runtime_request(
    _state_dir: &Path,
    _request: &LocalRuntimeRequest,
    _timeout: Duration,
) -> Result<Option<LocalRuntimeResponse>> {
    Ok(None)
}

#[cfg(unix)]
pub fn spawn_local_runtime_server(state_dir: &Path, runtime: RuntimeHandle) -> Result<PathBuf> {
    let socket_path = local_runtime_socket_path(state_dir);
    if socket_path.exists() {
        if send_local_runtime_request(
            state_dir,
            &LocalRuntimeRequest::Ping,
            Duration::from_millis(100),
        )?
        .is_some_and(|response| response.ok)
        {
            return Ok(socket_path);
        }
        let _ = std::fs::remove_file(&socket_path);
    }
    let listener = UnixListener::bind(&socket_path)
        .with_context(|| format!("bind local runtime socket {}", socket_path.display()))?;
    let server_path = socket_path.clone();
    thread::Builder::new()
        .name("browser-use-local-runtime-rpc".to_string())
        .spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else {
                    continue;
                };
                let runtime = runtime.clone();
                let _ = thread::Builder::new()
                    .name("browser-use-local-runtime-rpc-request".to_string())
                    .spawn(move || {
                        let _ = handle_local_runtime_stream(stream, runtime);
                    });
            }
            let _ = std::fs::remove_file(server_path);
        })
        .context("spawn local runtime socket server")?;
    Ok(socket_path)
}

#[cfg(not(unix))]
pub fn spawn_local_runtime_server(_state_dir: &Path, _runtime: RuntimeHandle) -> Result<PathBuf> {
    bail!("local runtime server is only supported on Unix platforms")
}

#[cfg(unix)]
fn handle_local_runtime_stream(mut stream: UnixStream, runtime: RuntimeHandle) -> Result<()> {
    let mut line = String::new();
    {
        let mut reader = BufReader::new(&mut stream);
        reader
            .read_line(&mut line)
            .context("read local runtime request")?;
    }
    let response = match serde_json::from_str::<LocalRuntimeRequest>(line.trim()) {
        Ok(request) => local_runtime_response(handle_local_runtime_request(&runtime, request)),
        Err(error) => LocalRuntimeResponse {
            ok: false,
            result: Value::Null,
            error: Some(format!("parse local runtime request failed: {error}")),
        },
    };
    writeln!(stream, "{}", serde_json::to_string(&response)?)
        .context("write local runtime response")?;
    stream.flush().context("flush local runtime response")
}

fn enrich_mailbox_payload(
    payload: Value,
    author_session_id: &SessionId,
    target_session_id: &SessionId,
    author_path: &str,
    target_path: &str,
) -> Value {
    let mut map = match payload {
        Value::Object(map) => map,
        other => {
            let mut map = serde_json::Map::new();
            if !other.is_null() {
                map.insert("value".to_string(), other);
            }
            map
        }
    };
    map.entry("author_session_id".to_string())
        .or_insert_with(|| json!(author_session_id.as_str()));
    map.entry("target_session_id".to_string())
        .or_insert_with(|| json!(target_session_id.as_str()));
    map.entry("author_path".to_string())
        .or_insert_with(|| json!(author_path));
    map.entry("target_path".to_string())
        .or_insert_with(|| json!(target_path));
    Value::Object(map)
}

fn child_agent_path(parent_path: &str, task_name: &str) -> String {
    let task_name = task_name.trim_matches('/');
    if parent_path == "/root" {
        format!("/root/{task_name}")
    } else {
        format!("{parent_path}/{task_name}")
    }
}

fn now_ms() -> i64 {
    Utc::now().timestamp_millis()
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct LiveResourceKey {
    kind: &'static str,
    id: String,
}

fn lost_live_resources_from_events(events: &[EventRecord]) -> Vec<LiveResourceKey> {
    let mut open = HashSet::new();
    let mut terminal = HashSet::new();
    for event in events {
        if let Some(key) = live_resource_start_key(event.event_type.as_str(), &event.payload) {
            open.insert(key);
            continue;
        }
        if let Some(key) = live_resource_terminal_key(event.event_type.as_str(), &event.payload) {
            terminal.insert(key);
        }
    }
    open.into_iter()
        .filter(|key| !terminal.contains(key))
        .collect()
}

#[derive(Default)]
struct MaterializedLiveState {
    pending_mailbox_items: Vec<MailboxItem>,
    last_enqueued_mailbox_seq: u64,
    last_wait_observed_mailbox_seq: u64,
    last_delivered_mailbox_seq: u64,
    last_consumed_mailbox_seq: u64,
    accepted_prompt_input_count: usize,
    pending_prompt_input_count: usize,
    last_accepted_prompt_input_seq: i64,
    last_consumed_prompt_input_seq: i64,
}

fn materialized_live_state_from_events(events: &[EventRecord]) -> MaterializedLiveState {
    let mut replay = MaterializedLiveState::default();
    let mut enqueued = Vec::new();
    let mut consumed_mailbox_seqs = HashSet::new();
    let mut prompt_accepted_seqs = Vec::new();
    let mut prompt_consumed_count = 0usize;

    for event in events {
        match event.event_type.as_str() {
            "mailbox.enqueued" => {
                if let Some(item) = mailbox_item_from_event(event) {
                    replay.last_enqueued_mailbox_seq =
                        replay.last_enqueued_mailbox_seq.max(item.seq);
                    enqueued.push(item);
                }
            }
            "mailbox.delivered" => {
                for seq in mailbox_seqs_from_event(event) {
                    replay.last_delivered_mailbox_seq = replay.last_delivered_mailbox_seq.max(seq);
                }
            }
            "wait_agent.completed" => {
                if let Some(seq) = event
                    .payload
                    .pointer("/payload/mailbox_seq")
                    .or_else(|| event.payload.get("mailbox_seq"))
                    .and_then(Value::as_u64)
                {
                    replay.last_wait_observed_mailbox_seq =
                        replay.last_wait_observed_mailbox_seq.max(seq);
                }
            }
            "mailbox.consumed" => {
                for seq in mailbox_seqs_from_event(event) {
                    replay.last_consumed_mailbox_seq = replay.last_consumed_mailbox_seq.max(seq);
                    consumed_mailbox_seqs.insert(seq);
                }
            }
            "agent.input.accepted" => {
                replay.accepted_prompt_input_count =
                    replay.accepted_prompt_input_count.saturating_add(1);
                let source_event_seq = prompt_source_event_seq(event);
                replay.last_accepted_prompt_input_seq = replay
                    .last_accepted_prompt_input_seq
                    .max(source_event_seq.unwrap_or_default());
                prompt_accepted_seqs.push(source_event_seq);
            }
            "agent.input.consumed" => {
                prompt_consumed_count = prompt_consumed_count.saturating_add(1);
                replay.last_consumed_prompt_input_seq = replay
                    .last_consumed_prompt_input_seq
                    .max(prompt_source_event_seq(event).unwrap_or_default());
            }
            _ => {}
        }
    }

    replay.pending_mailbox_items = enqueued
        .into_iter()
        .filter(|item| !consumed_mailbox_seqs.contains(&item.seq))
        .collect();

    replay.pending_prompt_input_count =
        if replay.last_accepted_prompt_input_seq > replay.last_consumed_prompt_input_seq {
            prompt_accepted_seqs
                .into_iter()
                .filter(|source_event_seq| {
                    source_event_seq
                        .map(|seq| seq > replay.last_consumed_prompt_input_seq)
                        .unwrap_or(true)
                })
                .count()
        } else {
            replay
                .accepted_prompt_input_count
                .saturating_sub(prompt_consumed_count)
        };

    replay
}

fn runtime_payload(payload: &Value) -> &Value {
    payload.get("payload").unwrap_or(payload)
}

fn mailbox_item_from_event(event: &EventRecord) -> Option<MailboxItem> {
    runtime_payload(&event.payload)
        .get("mailbox_item")
        .and_then(|value| serde_json::from_value(value.clone()).ok())
}

fn mailbox_seqs_from_event(event: &EventRecord) -> Vec<u64> {
    let payload = runtime_payload(&event.payload);
    if let Some(values) = payload.get("mailbox_seqs").and_then(Value::as_array) {
        return values.iter().filter_map(Value::as_u64).collect();
    }
    if let Some(values) = payload.get("mailbox_items").and_then(Value::as_array) {
        return values
            .iter()
            .filter_map(|item| item.get("seq").and_then(Value::as_u64))
            .collect();
    }
    payload
        .get("mailbox_item")
        .and_then(|item| item.get("seq"))
        .and_then(Value::as_u64)
        .into_iter()
        .collect()
}

fn prompt_source_event_seq(event: &EventRecord) -> Option<i64> {
    runtime_payload(&event.payload)
        .get("source_event_seq")
        .and_then(Value::as_i64)
}

fn live_resource_start_key(event_type: &str, payload: &Value) -> Option<LiveResourceKey> {
    match event_type {
        "exec_command.begin" => {
            payload_resource_id(payload, &["process_id", "session_id"]).map(|id| LiveResourceKey {
                kind: "exec_command",
                id,
            })
        }
        "browser_script.started" => payload_resource_id(
            payload,
            &[
                "run_id",
                "browser_script_run_id",
                "script_run_id",
                "tool_call_id",
            ],
        )
        .map(|id| LiveResourceKey {
            kind: "browser_script",
            id,
        }),
        "python.started" => {
            payload_resource_id(payload, &["tool_call_id", "call_id", "session_id"])
                .map(|id| LiveResourceKey { kind: "python", id })
        }
        "mcp.tool.started" => payload_resource_id(payload, &["tool_call_id", "call_id"])
            .map(|id| LiveResourceKey { kind: "mcp", id }),
        _ => None,
    }
}

fn live_resource_terminal_key(event_type: &str, payload: &Value) -> Option<LiveResourceKey> {
    match event_type {
        "exec_command.end" => {
            payload_resource_id(payload, &["process_id", "session_id"]).map(|id| LiveResourceKey {
                kind: "exec_command",
                id,
            })
        }
        "browser_script.completed" | "browser_script.cancelled" | "browser_script.failed" => {
            payload_resource_id(
                payload,
                &[
                    "run_id",
                    "browser_script_run_id",
                    "script_run_id",
                    "tool_call_id",
                ],
            )
            .map(|id| LiveResourceKey {
                kind: "browser_script",
                id,
            })
        }
        "python.completed" | "python.failed" => {
            payload_resource_id(payload, &["tool_call_id", "call_id", "session_id"])
                .map(|id| LiveResourceKey { kind: "python", id })
        }
        "mcp.tool.completed" | "mcp.tool.failed" => {
            payload_resource_id(payload, &["tool_call_id", "call_id"])
                .map(|id| LiveResourceKey { kind: "mcp", id })
        }
        "resource.lost" => payload
            .get("payload")
            .unwrap_or(payload)
            .get("resource")
            .and_then(|resource| {
                Some(LiveResourceKey {
                    kind: match resource.get("kind").and_then(Value::as_str)? {
                        "exec_command" => "exec_command",
                        "browser_script" => "browser_script",
                        "python" => "python",
                        "mcp" => "mcp",
                        _ => return None,
                    },
                    id: resource.get("id").and_then(value_to_resource_id)?,
                })
            }),
        _ => None,
    }
}

fn payload_resource_id(payload: &Value, keys: &[&str]) -> Option<String> {
    let payload = payload.get("payload").unwrap_or(payload);
    keys.iter()
        .filter_map(|key| payload.get(*key))
        .find_map(value_to_resource_id)
}

fn value_to_resource_id(value: &Value) -> Option<String> {
    match value {
        Value::String(value) if !value.trim().is_empty() => Some(value.clone()),
        Value::Number(value) => Some(value.to_string()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mailbox_item(author: &AgentId, target: &AgentId, kind: MailboxItemKind) -> MailboxItem {
        MailboxItem {
            seq: 0,
            id: Uuid::new_v4().simple().to_string(),
            kind,
            author_agent_id: author.clone(),
            target_agent_id: target.clone(),
            target_path: Some("/root/research".to_string()),
            content: "done".to_string(),
            trigger_turn: false,
            delivery_phase: MailboxDeliveryPhase::NextTurn,
            payload: json!({}),
        }
    }

    #[derive(Clone, Default)]
    struct FailingJournal {
        inner: MemoryJournal,
        fail_event_types: Arc<Mutex<HashSet<String>>>,
    }

    impl FailingJournal {
        fn fail_event_type(&self, event_type: &str) {
            self.fail_event_types
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert(event_type.to_string());
        }

        fn should_fail(&self, event_type: &str) -> bool {
            self.fail_event_types
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .contains(event_type)
        }
    }

    impl JournalSink for FailingJournal {
        fn append_runtime_event(&self, event: &RuntimeEvent) -> Result<JournalAppend> {
            if self.should_fail(event.event_type()) {
                bail!("forced journal failure for {}", event.event_type());
            }
            self.inner.append_runtime_event(event)
        }

        fn append_session_event(
            &self,
            session_id: &SessionId,
            event_type: &str,
            payload: Value,
            durability: Durability,
        ) -> Result<JournalAppend> {
            if self.should_fail(event_type) {
                bail!("forced journal failure for {event_type}");
            }
            self.inner
                .append_session_event(session_id, event_type, payload, durability)
        }

        fn flush(&self) -> Result<()> {
            self.inner.flush()
        }
    }

    impl JournalReader for FailingJournal {
        fn load_session(&self, session_id: &SessionId) -> Result<Option<SessionMeta>> {
            self.inner.load_session(session_id)
        }

        fn list_sessions(&self) -> Result<Vec<SessionMeta>> {
            self.inner.list_sessions()
        }

        fn events_for_session(&self, session_id: &SessionId) -> Result<Vec<EventRecord>> {
            self.inner.events_for_session(session_id)
        }

        fn events_after_seq(
            &self,
            session_id: &SessionId,
            after_seq: i64,
        ) -> Result<Vec<EventRecord>> {
            self.inner.events_after_seq(session_id, after_seq)
        }
    }

    impl LiveThreadPersistence for FailingJournal {
        fn create_thread(&self, request: CreateThreadRequest) -> Result<SessionMeta> {
            self.inner.create_thread(request)
        }
    }

    impl StateIndex for FailingJournal {
        fn open_spawn_edge(&self, edge: SpawnEdge) -> Result<()> {
            self.inner.open_spawn_edge(edge)
        }

        fn finish_spawn_edge(
            &self,
            child_session_id: &SessionId,
            status: SpawnEdgeStatus,
        ) -> Result<()> {
            self.inner.finish_spawn_edge(child_session_id, status)
        }

        fn close_spawn_edge(&self, child_session_id: &SessionId, reason: &str) -> Result<()> {
            self.inner.close_spawn_edge(child_session_id, reason)
        }

        fn list_children(&self, parent_session_id: &SessionId) -> Result<Vec<SpawnEdge>> {
            self.inner.list_children(parent_session_id)
        }

        fn list_descendants(&self, root_session_id: &SessionId) -> Result<Vec<SpawnEdge>> {
            self.inner.list_descendants(root_session_id)
        }
    }

    fn runtime_with_failing_journal() -> (RuntimeHandle, Arc<FailingJournal>) {
        let journal = Arc::new(FailingJournal::default());
        let persistence: Arc<dyn LiveThreadPersistence> = journal.clone();
        let state_index: Arc<dyn StateIndex> = journal.clone();
        (
            BrowserUseRuntime::new(persistence, state_index).handle(),
            journal,
        )
    }

    #[test]
    fn memory_journal_barrier_appends_ordered_events() -> Result<()> {
        let journal = MemoryJournal::new();
        let session = journal.create_thread(CreateThreadRequest {
            session_id: Some(SessionId::from_string("root")?),
            parent_session_id: None,
            cwd: PathBuf::from("/tmp"),
            artifact_root: None,
            agent_path: None,
            nickname: None,
            role: None,
        })?;
        let session_id = SessionId::from_string(session.id)?;
        let first = journal.append_session_event(
            &session_id,
            "session.input",
            json!({"text": "hello"}),
            Durability::Barrier,
        )?;
        let second = journal.append_runtime_event(
            &RuntimeEvent::new(RuntimeEventKind::AgentStarted, Durability::Barrier)
                .with_session_id(session_id.clone())
                .with_payload(json!({"status": "running"})),
        )?;
        assert_eq!(first.seq, Some(2));
        assert_eq!(second.seq, Some(3));
        let events = journal.events_for_session(&session_id)?;
        assert_eq!(events.len(), 3);
        assert_eq!(events[0].event_type, "session.created");
        assert_eq!(events[2].event_type, "agent.started");
        Ok(())
    }

    #[tokio::test]
    async fn mailbox_wait_is_target_specific_and_non_draining() -> Result<()> {
        let mailbox = AgentMailbox::new();
        let parent = AgentId::from_string("parent")?;
        let child_a = AgentId::from_string("child-a")?;
        let child_b = AgentId::from_string("child-b")?;

        mailbox.enqueue(mailbox_item(&child_a, &parent, MailboxItemKind::Completion));

        let wrong = mailbox
            .wait_for_completion(
                AgentTarget::AgentId(child_b.clone()),
                Duration::from_millis(10),
            )
            .await?;
        assert_eq!(wrong, WaitAgentOutcome::TimedOut);

        let right = mailbox
            .wait_for_completion(
                AgentTarget::AgentId(child_a.clone()),
                Duration::from_millis(10),
            )
            .await?;
        let WaitAgentOutcome::Completed(item) = right else {
            panic!("expected completion");
        };
        assert_eq!(item.author_agent_id, child_a);
        assert_eq!(
            mailbox.pending_items().len(),
            1,
            "wait_agent must not drain mail"
        );
        Ok(())
    }

    #[tokio::test]
    async fn wait_agent_wakes_for_any_mailbox_item_kind() -> Result<()> {
        let mailbox = AgentMailbox::new();
        let parent = AgentId::from_string("parent")?;
        let child = AgentId::from_string("child")?;

        mailbox.enqueue(mailbox_item(&child, &parent, MailboxItemKind::Input));

        let outcome = mailbox
            .wait_for_item(
                AgentTarget::AgentId(child.clone()),
                0,
                Duration::from_millis(10),
            )
            .await?;
        let WaitAgentOutcome::Completed(item) = outcome else {
            panic!("expected mailbox item");
        };
        assert_eq!(item.kind, MailboxItemKind::Input);
        assert_eq!(item.author_agent_id, child);
        assert_eq!(
            mailbox.pending_items().len(),
            1,
            "wait_agent must not drain non-completion mail either"
        );
        Ok(())
    }

    #[test]
    fn state_index_lists_descendants() -> Result<()> {
        let journal = MemoryJournal::new();
        let root = SessionId::from_string("root")?;
        let child = SessionId::from_string("child")?;
        let grandchild = SessionId::from_string("grandchild")?;
        journal.open_spawn_edge(SpawnEdge {
            parent_session_id: root.clone(),
            child_session_id: child.clone(),
            status: SpawnEdgeStatus::Closed,
            path: Some("/root/child".to_string()),
            nickname: Some("child".to_string()),
            role: None,
        })?;
        journal.open_spawn_edge(SpawnEdge {
            parent_session_id: child,
            child_session_id: grandchild,
            status: SpawnEdgeStatus::Closed,
            path: Some("/root/child/grandchild".to_string()),
            nickname: None,
            role: None,
        })?;
        let descendants = journal.list_descendants(&root)?;
        assert_eq!(descendants.len(), 2);
        assert!(descendants
            .iter()
            .all(|edge| edge.status == SpawnEdgeStatus::Open));
        Ok(())
    }

    #[test]
    fn strict_scheduler_counts_open_spawned_agents_until_close() -> Result<()> {
        let scheduler = SubagentScheduler::new(3, CapacityMode::StrictReject);
        let child_a = AgentId::from_string("child-a")?;
        let child_b = AgentId::from_string("child-b")?;
        let child_c = AgentId::from_string("child-c")?;

        assert!(matches!(
            scheduler.admit_spawn(child_a.clone(), "a")?,
            SpawnAdmission::Reserved(_)
        ));
        assert!(matches!(
            scheduler.admit_spawn(child_b.clone(), "b")?,
            SpawnAdmission::Reserved(_)
        ));
        let err = scheduler
            .admit_spawn(child_c.clone(), "c")
            .expect_err("third spawned child should exceed cap because root consumes one slot");
        assert!(err.to_string().contains("agent limit reached"));
        assert_eq!(scheduler.open_count(), 2);

        assert!(
            scheduler.close_spawned_agent(&child_a).is_none(),
            "strict mode releases a slot but does not auto-start queued work"
        );
        assert!(matches!(
            scheduler.admit_spawn(child_c, "c")?,
            SpawnAdmission::Reserved(_)
        ));
        Ok(())
    }

    #[test]
    fn queue_scheduler_is_explicit_non_default_behavior() -> Result<()> {
        let scheduler = SubagentScheduler::new(2, CapacityMode::Queue);
        let child_a = AgentId::from_string("child-a")?;
        let child_b = AgentId::from_string("child-b")?;

        assert!(matches!(
            scheduler.admit_spawn(child_a.clone(), "a")?,
            SpawnAdmission::Reserved(_)
        ));
        assert!(matches!(
            scheduler.admit_spawn(child_b.clone(), "b")?,
            SpawnAdmission::Queued(_)
        ));
        assert_eq!(scheduler.open_count(), 1);
        assert_eq!(scheduler.queued_count(), 1);

        let reservation = scheduler
            .close_spawned_agent(&child_a)
            .expect("queue mode should reserve next child when a slot closes");
        assert_eq!(reservation.child_agent_id, child_b);
        assert_eq!(scheduler.open_count(), 1);
        assert_eq!(scheduler.queued_count(), 0);
        Ok(())
    }

    #[test]
    fn runtime_compaction_window_state_lives_on_agent_thread() -> Result<()> {
        let (runtime, _journal) = BrowserUseRuntime::memory();
        let handle = runtime.handle();
        let root = handle.create_root_agent(CreateRootAgentRequest {
            cwd: PathBuf::from("/tmp"),
            task: "root task".to_string(),
            max_concurrent_threads_per_session: 3,
        })?;
        let session_id = root.session_id().clone();

        let initial = root.live_state_snapshot();
        assert_eq!(initial.compaction_window_ordinal, 1);
        assert_eq!(initial.compaction_prefill_input_tokens, None);
        assert_eq!(initial.compaction_prefill_source, None);

        handle.record_estimated_compaction_prefill_for_session(&session_id, 42)?;
        let estimated = root.live_state_snapshot();
        assert_eq!(estimated.compaction_prefill_input_tokens, Some(42));
        assert_eq!(
            estimated.compaction_prefill_source.as_deref(),
            Some("estimated")
        );

        handle.record_server_observed_compaction_prefill_for_session(&session_id, 7)?;
        let observed = root.live_state_snapshot();
        assert_eq!(observed.compaction_prefill_input_tokens, Some(7));
        assert_eq!(
            observed.compaction_prefill_source.as_deref(),
            Some("server_observed")
        );

        handle.record_estimated_compaction_prefill_for_session(&session_id, 99)?;
        assert_eq!(
            root.live_state_snapshot().compaction_prefill_input_tokens,
            Some(7),
            "estimated prefill must not replace server-observed prefill"
        );

        handle.start_next_compaction_window_for_session(&session_id)?;
        let next = root.live_state_snapshot();
        assert_eq!(next.compaction_window_ordinal, 2);
        assert_eq!(next.compaction_prefill_input_tokens, None);
        assert_eq!(next.compaction_prefill_source, None);
        Ok(())
    }

    #[test]
    fn browser_manager_allows_one_active_agent_per_browser() -> Result<()> {
        let manager = BrowserManager::new();
        let browser_id = manager.create_browser(BrowserConfig {
            keep_alive: true,
            headless: Some(true),
            profile_id: Some("default".to_string()),
            ..BrowserConfig::default()
        });
        let agent_a = AgentId::from_string("agent-a")?;
        let agent_b = AgentId::from_string("agent-b")?;

        let lease = manager.claim_browser(&browser_id, agent_a.clone())?;
        let snapshot = manager.snapshot(&browser_id)?;
        assert_eq!(snapshot.status, BrowserStatus::Claimed);
        assert_eq!(snapshot.active_agent_id, Some(agent_a.clone()));

        let err = manager
            .claim_browser(&browser_id, agent_b.clone())
            .expect_err("same browser cannot be claimed by another running agent");
        assert!(err.to_string().contains("browser already in use"));

        manager.release_browser(&lease)?;
        let lease_b = manager.claim_browser(&browser_id, agent_b.clone())?;
        assert_eq!(lease_b.agent_id, agent_b);
        Ok(())
    }

    #[test]
    fn browser_manager_owns_isolated_physical_registries() -> Result<()> {
        let manager = BrowserManager::new();
        let browser_a = manager.create_browser(BrowserConfig::default());
        let browser_b = manager.create_browser(BrowserConfig::default());
        let registries_a = manager.physical_registries(&browser_a)?;
        let registries_b = manager.physical_registries(&browser_b)?;
        let workspace = tempfile::tempdir()?;

        browser_use_browser::run_browser_command_with_options_and_registries(
            "runtime-session-a",
            workspace.path(),
            workspace.path(),
            "browser status --json",
            browser_use_browser::BrowserCommandOptions::default(),
            &registries_a.script_registry(),
            &registries_a.session_registry(),
        )?;

        assert!(
            registries_a
                .session_registry()
                .contains_session("runtime-session-a"),
            "the session should live in browser A's runtime-owned registry"
        );
        assert!(
            !registries_b
                .session_registry()
                .contains_session("runtime-session-a"),
            "browser B must not see browser A's physical session"
        );
        assert!(
            !browser_use_browser::BrowserSessionRegistry::global()
                .contains_session("runtime-session-a"),
            "runtime-owned physical browser state must not hit the global compatibility registry"
        );
        Ok(())
    }

    #[test]
    fn browser_manager_same_agent_claims_are_depth_aware() -> Result<()> {
        let manager = BrowserManager::new();
        let browser_id = manager.create_browser(BrowserConfig::default());
        let agent = AgentId::from_string("agent-a")?;

        let outer = manager.claim_browser(&browser_id, agent.clone())?;
        let inner = manager.claim_browser(&browser_id, agent.clone())?;
        manager.release_browser(&inner)?;
        assert_eq!(
            manager.snapshot(&browser_id)?.active_agent_id,
            Some(agent.clone())
        );
        assert_eq!(
            manager.snapshot(&browser_id)?.status,
            BrowserStatus::Claimed
        );

        manager.release_browser(&outer)?;
        assert_eq!(manager.snapshot(&browser_id)?.active_agent_id, None);
        assert_eq!(
            manager.snapshot(&browser_id)?.status,
            BrowserStatus::Released
        );
        Ok(())
    }

    #[test]
    fn browser_manager_action_lock_serializes_actions() -> Result<()> {
        let manager = Arc::new(BrowserManager::new());
        let browser_id = manager.create_browser(BrowserConfig::default());
        let barrier = Arc::new(std::sync::Barrier::new(3));
        let active = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let max_active = Arc::new(std::sync::atomic::AtomicUsize::new(0));

        let handles = (0..2)
            .map(|_| {
                let manager = Arc::clone(&manager);
                let browser_id = browser_id.clone();
                let barrier = Arc::clone(&barrier);
                let active = Arc::clone(&active);
                let max_active = Arc::clone(&max_active);
                thread::spawn(move || -> Result<()> {
                    barrier.wait();
                    manager.with_action_lock(&browser_id, || {
                        let current = active.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
                        max_active.fetch_max(current, std::sync::atomic::Ordering::SeqCst);
                        thread::sleep(Duration::from_millis(25));
                        active.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
                        Ok(())
                    })
                })
            })
            .collect::<Vec<_>>();
        barrier.wait();
        for handle in handles {
            handle.join().expect("action thread should not panic")?;
        }

        assert_eq!(max_active.load(std::sync::atomic::Ordering::SeqCst), 1);
        Ok(())
    }

    #[test]
    fn browser_release_requires_matching_lease_owner() -> Result<()> {
        let manager = BrowserManager::new();
        let browser_id = manager.create_browser(BrowserConfig::default());
        let agent_a = AgentId::from_string("agent-a")?;
        let agent_b = AgentId::from_string("agent-b")?;
        let lease = manager.claim_browser(&browser_id, agent_a)?;

        let wrong_lease = BrowserLease {
            browser_id: lease.browser_id.clone(),
            agent_id: agent_b,
        };
        let err = manager
            .release_browser(&wrong_lease)
            .expect_err("wrong agent must not release another agent's browser");
        assert!(err.to_string().contains("browser lease mismatch"));
        Ok(())
    }

    #[test]
    fn runtime_browser_claim_and_release_are_barrier_journaled() -> Result<()> {
        let (runtime, journal) = BrowserUseRuntime::memory();
        let handle = runtime.handle();
        let root = handle.create_root_agent(CreateRootAgentRequest {
            cwd: PathBuf::from("/tmp"),
            task: "root task".to_string(),
            max_concurrent_threads_per_session: 3,
        })?;
        let browser_id = handle.create_browser(BrowserConfig {
            keep_alive: true,
            headless: Some(true),
            profile_id: Some("sdk".to_string()),
            ..BrowserConfig::default()
        });

        let lease = handle.claim_browser(&browser_id, root.agent_id().clone())?;
        assert_eq!(
            handle.browsers().snapshot(&browser_id)?.active_agent_id,
            Some(root.agent_id().clone())
        );
        handle.release_browser(&lease)?;
        assert_eq!(
            handle.browsers().snapshot(&browser_id)?.active_agent_id,
            None
        );

        let event_types = journal
            .events_for_session(root.session_id())?
            .into_iter()
            .map(|event| event.event_type)
            .collect::<Vec<_>>();
        assert!(event_types.contains(&"browser.claimed".to_string()));
        assert!(event_types.contains(&"browser.released".to_string()));
        Ok(())
    }

    #[test]
    fn browser_claim_barrier_failure_does_not_claim_browser() -> Result<()> {
        let (handle, journal) = runtime_with_failing_journal();
        let root = handle.create_root_agent(CreateRootAgentRequest {
            cwd: PathBuf::from("/tmp"),
            task: "root task".to_string(),
            max_concurrent_threads_per_session: 3,
        })?;
        let browser_id = handle.create_browser(BrowserConfig::default());
        journal.fail_event_type("browser.claimed");

        let err = handle
            .claim_browser(&browser_id, root.agent_id().clone())
            .expect_err("browser claim must fail before live ownership changes");

        assert!(err.to_string().contains("forced journal failure"));
        assert_eq!(
            handle.browsers().snapshot(&browser_id)?.active_agent_id,
            None
        );
        Ok(())
    }

    #[test]
    fn agent_scoped_browser_create_and_close_are_barrier_journaled() -> Result<()> {
        let (runtime, journal) = BrowserUseRuntime::memory();
        let handle = runtime.handle();
        let root = handle.create_root_agent(CreateRootAgentRequest {
            cwd: PathBuf::from("/tmp"),
            task: "root task".to_string(),
            max_concurrent_threads_per_session: 3,
        })?;

        let browser_id =
            handle.create_browser_for_agent(root.agent_id().clone(), BrowserConfig::default())?;
        assert_eq!(
            handle.browsers().snapshot(&browser_id)?.status,
            BrowserStatus::Created
        );
        handle.close_browser_for_agent(&browser_id, root.agent_id())?;
        assert!(handle.browsers().snapshot(&browser_id).is_err());

        let event_types = journal
            .events_for_session(root.session_id())?
            .into_iter()
            .map(|event| event.event_type)
            .collect::<Vec<_>>();
        assert!(event_types.contains(&"browser.created".to_string()));
        assert!(event_types.contains(&"browser.closed".to_string()));
        Ok(())
    }

    #[test]
    fn browser_create_barrier_failure_does_not_insert_or_publish() -> Result<()> {
        let (handle, journal) = runtime_with_failing_journal();
        let root = handle.create_root_agent(CreateRootAgentRequest {
            cwd: PathBuf::from("/tmp"),
            task: "root task".to_string(),
            max_concurrent_threads_per_session: 3,
        })?;
        let mut rx = handle.events().subscribe();
        journal.fail_event_type("browser.created");

        let err = handle
            .create_browser_for_agent(root.agent_id().clone(), BrowserConfig::default())
            .expect_err("browser create must fail before insertion");

        assert!(err.to_string().contains("forced journal failure"));
        assert!(handle.browsers().snapshots().is_empty());
        assert!(rx.try_recv().is_err());
        Ok(())
    }

    #[test]
    fn browser_close_barrier_failure_keeps_browser_visible() -> Result<()> {
        let (handle, journal) = runtime_with_failing_journal();
        let root = handle.create_root_agent(CreateRootAgentRequest {
            cwd: PathBuf::from("/tmp"),
            task: "root task".to_string(),
            max_concurrent_threads_per_session: 3,
        })?;
        let browser_id =
            handle.create_browser_for_agent(root.agent_id().clone(), BrowserConfig::default())?;
        journal.fail_event_type("browser.closed");

        let err = handle
            .close_browser_for_agent(&browser_id, root.agent_id())
            .expect_err("browser close must fail before live removal");

        assert!(err.to_string().contains("forced journal failure"));
        assert_eq!(
            handle.browsers().snapshot(&browser_id)?.status,
            BrowserStatus::Created
        );
        Ok(())
    }

    #[test]
    fn active_run_registry_cancels_registered_tokens() -> Result<()> {
        let registry = ActiveRunRegistry::new();
        let session_id = SessionId::from_string("session-1")?;
        let token = registry.register(session_id.clone());
        assert!(!token.is_cancelled());
        assert!(registry.cancel(&session_id));
        assert!(token.is_cancelled());
        registry.unregister(&session_id);
        assert!(!registry.cancel(&session_id));
        Ok(())
    }

    #[test]
    fn cancel_request_barrier_failure_does_not_cancel_token_or_live_state() -> Result<()> {
        let (handle, journal) = runtime_with_failing_journal();
        let root = handle.create_root_agent(CreateRootAgentRequest {
            cwd: PathBuf::from("/tmp"),
            task: "root task".to_string(),
            max_concurrent_threads_per_session: 3,
        })?;
        let token = CancellationToken::new();
        handle.register_run_with_token(root.session_id().clone(), token.clone());
        root.set_status(AgentThreadStatus::Running);
        journal.fail_event_type("agent.cancel_requested");

        let err = handle
            .request_cancel_run(root.session_id())
            .expect_err("cancel must fail before live token cancellation");

        assert!(err.to_string().contains("forced journal failure"));
        assert!(!token.is_cancelled());
        let snapshot = root.snapshot();
        assert_eq!(snapshot.status, AgentThreadStatus::Running);
        assert!(!snapshot.live.cancellation_requested);
        Ok(())
    }

    #[test]
    fn cancel_request_cascades_to_active_descendant_runs_after_barriers() -> Result<()> {
        let (runtime, journal) = BrowserUseRuntime::memory();
        let handle = runtime.handle();
        let root = handle.create_root_agent(CreateRootAgentRequest {
            cwd: PathBuf::from("/tmp"),
            task: "root task".to_string(),
            max_concurrent_threads_per_session: 3,
        })?;
        let child = handle.spawn_child(SpawnChildRequest {
            parent_agent_id: root.agent_id().clone(),
            child_agent_id: None,
            child_session_id: None,
            task_name: "sleeper".to_string(),
            message: "sleep".to_string(),
            nickname: None,
            role: None,
        })?;
        let root_token = CancellationToken::new();
        let child_token = CancellationToken::new();
        handle.register_run_with_token(root.session_id().clone(), root_token.clone());
        handle.register_run_with_token(child.session_id().clone(), child_token.clone());
        root.set_status(AgentThreadStatus::Running);
        child.set_status(AgentThreadStatus::Running);

        assert!(handle.request_cancel_run(root.session_id())?);

        assert!(root_token.is_cancelled());
        assert!(child_token.is_cancelled());
        assert_eq!(root.snapshot().status, AgentThreadStatus::Cancelling);
        assert_eq!(child.snapshot().status, AgentThreadStatus::Cancelling);
        assert!(root.live_state_snapshot().cancellation_requested);
        assert!(child.live_state_snapshot().cancellation_requested);

        let root_cancel = journal
            .events_for_session(root.session_id())?
            .into_iter()
            .find(|event| event.event_type == RuntimeEventKind::AgentCancelRequested.as_str())
            .context("root cancel event")?;
        assert_eq!(root_cancel.payload["payload"]["runtime_owned"], true);

        let child_cancel = journal
            .events_for_session(child.session_id())?
            .into_iter()
            .find(|event| event.event_type == RuntimeEventKind::AgentCancelRequested.as_str())
            .context("child propagated cancel event")?;
        assert_eq!(child_cancel.payload["payload"]["runtime_owned"], true);
        assert_eq!(
            child_cancel.payload["payload"]["propagated_from_session_id"].as_str(),
            Some(root.session_id().as_str())
        );
        Ok(())
    }

    #[tokio::test]
    async fn runtime_run_agent_owns_active_run_lifecycle_and_events() -> Result<()> {
        let (runtime, journal) = BrowserUseRuntime::memory();
        let handle = runtime.handle();
        let root = handle.create_root_agent(CreateRootAgentRequest {
            cwd: PathBuf::from("/tmp"),
            task: "root task".to_string(),
            max_concurrent_threads_per_session: 3,
        })?;
        let session_id = root.session_id().clone();
        let session_id_for_run = session_id.clone();
        let handle_for_run = handle.clone();

        let request = RunAgentRequest::new(session_id.clone())
            .with_agent_id(root.agent_id().clone())
            .with_initial_input(json!({"text": "root task"}))
            .with_input_source("test");
        let run_id = request.run_id.clone();
        let root_agent_id = root.agent_id().clone();
        let response = handle
            .run_agent(request, async move {
                assert!(handle_for_run
                    .inner
                    .active_runs
                    .tokens
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .contains_key(&session_id_for_run));
                let live = handle_for_run
                    .snapshot_agent(&root_agent_id)
                    .expect("agent snapshot")
                    .live;
                assert_eq!(live.current_run_id, Some(run_id));
                assert_eq!(live.accepted_input_count, 1);
                assert_eq!(live.pending_prompt_input_count, 1);
                handle_for_run.append_observed_session_event(
                    session_id_for_run.clone(),
                    "session.done",
                    json!({"result": "done"}),
                    Durability::Barrier,
                )?;
                Ok::<_, anyhow::Error>("done".to_string())
            })
            .await?;

        assert_eq!(response.agent_id, *root.agent_id());
        assert_eq!(response.session_id, session_id);
        assert_eq!(response.final_status, AgentThreadStatus::Completed);
        assert_eq!(response.final_result.as_deref(), Some("done"));
        assert!(response.terminal_event_seq.is_some());
        assert_eq!(response.output, "done");
        assert!(!handle.cancel_run(root.session_id()));
        let snapshot = root.snapshot();
        assert_eq!(snapshot.status, AgentThreadStatus::Completed);
        assert_eq!(snapshot.live.current_run_id, None);

        let events = journal.events_for_session(root.session_id())?;
        let event_types = events
            .iter()
            .map(|event| event.event_type.as_str())
            .collect::<Vec<_>>();
        assert!(event_types.contains(&"agent.input.accepted"));
        assert!(event_types.contains(&"agent.started"));
        assert!(event_types.contains(&"agent.turn.started"));
        assert!(event_types.contains(&"agent.turn.completed"));
        assert!(event_types.contains(&"agent.completed"));
        let turn_completed_seq = events
            .iter()
            .find(|event| event.event_type == "agent.turn.completed")
            .expect("agent.turn.completed")
            .seq;
        let completed = events
            .iter()
            .find(|event| event.event_type == "agent.completed")
            .expect("agent.completed");
        assert!(completed.seq > turn_completed_seq);
        assert_eq!(completed.payload["payload"]["result"], "done");
        Ok(())
    }

    #[tokio::test]
    async fn runtime_run_agent_uses_supplied_run_id() -> Result<()> {
        let (runtime, journal) = BrowserUseRuntime::memory();
        let handle = runtime.handle();
        let root = handle.create_root_agent(CreateRootAgentRequest {
            cwd: PathBuf::from("/tmp"),
            task: "root task".to_string(),
            max_concurrent_threads_per_session: 3,
        })?;
        let supplied_run_id = RunId::from_string("durable-child-run-1")?;

        let response = handle
            .run_agent(
                RunAgentRequest::new(root.session_id().clone())
                    .with_agent_id(root.agent_id().clone())
                    .with_run_id(supplied_run_id.clone()),
                async { Ok::<_, anyhow::Error>("done".to_string()) },
            )
            .await?;

        assert_eq!(response.run_id, supplied_run_id);
        let started = journal
            .events_for_session(root.session_id())?
            .into_iter()
            .find(|event| event.event_type == "agent.started")
            .expect("agent.started event");
        assert_eq!(
            started.payload["run_id"],
            serde_json::Value::String("durable-child-run-1".to_string())
        );
        Ok(())
    }

    #[tokio::test]
    async fn runtime_run_agent_marks_clean_return_after_cancel_as_cancelled() -> Result<()> {
        let (runtime, journal) = BrowserUseRuntime::memory();
        let handle = runtime.handle();
        let root = handle.create_root_agent(CreateRootAgentRequest {
            cwd: PathBuf::from("/tmp"),
            task: "root task".to_string(),
            max_concurrent_threads_per_session: 3,
        })?;
        let cancel = CancellationToken::new();
        let cancel_for_run = cancel.clone();

        let response = handle
            .run_agent(
                RunAgentRequest::new(root.session_id().clone())
                    .with_agent_id(root.agent_id().clone())
                    .with_cancellation_token(cancel.clone()),
                async move {
                    cancel_for_run.cancel();
                    Ok::<_, anyhow::Error>("clean unwind after cancel".to_string())
                },
            )
            .await?;

        assert_eq!(response.final_status, AgentThreadStatus::Cancelled);
        assert_eq!(root.snapshot().status, AgentThreadStatus::Cancelled);
        assert_eq!(
            journal
                .load_session(root.session_id())?
                .expect("root session")
                .status,
            SessionStatus::Cancelled
        );
        let events = journal.events_for_session(root.session_id())?;
        let event_types = events
            .iter()
            .map(|event| event.event_type.as_str())
            .collect::<Vec<_>>();
        assert!(event_types.contains(&"agent.turn.aborted"));
        assert!(event_types.contains(&"session.cancelled"));
        assert!(event_types.contains(&"agent.cancelled"));
        assert!(!event_types.contains(&"agent.completed"));
        Ok(())
    }

    #[tokio::test]
    async fn runtime_run_agent_claims_and_releases_browser() -> Result<()> {
        let (runtime, journal) = BrowserUseRuntime::memory();
        let handle = runtime.handle();
        let root = handle.create_root_agent(CreateRootAgentRequest {
            cwd: PathBuf::from("/tmp"),
            task: "root task".to_string(),
            max_concurrent_threads_per_session: 3,
        })?;
        let browser_id = handle.create_browser(BrowserConfig::default());
        let browser_id_for_run = browser_id.clone();
        let expected_agent_id = root.agent_id().clone();
        let handle_for_run = handle.clone();

        handle
            .run_agent(
                RunAgentRequest::new(root.session_id().clone())
                    .with_agent_id(root.agent_id().clone())
                    .with_browser_id(browser_id.clone()),
                async move {
                    let snapshot = handle_for_run.browsers().snapshot(&browser_id_for_run)?;
                    assert_eq!(snapshot.active_agent_id, Some(expected_agent_id));
                    Ok::<_, anyhow::Error>("done".to_string())
                },
            )
            .await?;

        let snapshot = handle.browsers().snapshot(&browser_id)?;
        assert_eq!(snapshot.active_agent_id, None);
        assert_eq!(snapshot.status, BrowserStatus::Released);
        let event_types = journal
            .events_for_session(root.session_id())?
            .into_iter()
            .map(|event| event.event_type)
            .collect::<Vec<_>>();
        assert!(event_types.contains(&"browser.claimed".to_string()));
        assert!(event_types.contains(&"browser.released".to_string()));
        Ok(())
    }

    #[tokio::test]
    async fn runtime_browser_action_preserves_run_level_browser_lease() -> Result<()> {
        let (runtime, _journal) = BrowserUseRuntime::memory();
        let handle = runtime.handle();
        let root = handle.create_root_agent(CreateRootAgentRequest {
            cwd: PathBuf::from("/tmp"),
            task: "root task".to_string(),
            max_concurrent_threads_per_session: 3,
        })?;
        let browser_id = handle.create_browser(BrowserConfig::default());
        let browser_id_for_run = browser_id.clone();
        let expected_agent_id = root.agent_id().clone();
        let handle_for_run = handle.clone();

        handle
            .run_agent(
                RunAgentRequest::new(root.session_id().clone())
                    .with_agent_id(root.agent_id().clone())
                    .with_browser_id(browser_id.clone()),
                async move {
                    handle_for_run.with_browser_action(
                        &browser_id_for_run,
                        expected_agent_id.clone(),
                        || Ok::<_, anyhow::Error>(()),
                    )?;
                    let snapshot = handle_for_run.browsers().snapshot(&browser_id_for_run)?;
                    assert_eq!(snapshot.active_agent_id, Some(expected_agent_id));
                    assert_eq!(snapshot.status, BrowserStatus::Claimed);
                    Ok::<_, anyhow::Error>("done".to_string())
                },
            )
            .await?;

        let snapshot = handle.browsers().snapshot(&browser_id)?;
        assert_eq!(snapshot.active_agent_id, None);
        assert_eq!(snapshot.status, BrowserStatus::Released);
        Ok(())
    }

    #[tokio::test]
    async fn runtime_run_agent_releases_browser_after_run_error() -> Result<()> {
        let (runtime, journal) = BrowserUseRuntime::memory();
        let handle = runtime.handle();
        let root = handle.create_root_agent(CreateRootAgentRequest {
            cwd: PathBuf::from("/tmp"),
            task: "root task".to_string(),
            max_concurrent_threads_per_session: 3,
        })?;
        let browser_id = handle.create_browser(BrowserConfig::default());

        let err = match handle
            .run_agent(
                RunAgentRequest::new(root.session_id().clone())
                    .with_agent_id(root.agent_id().clone())
                    .with_browser_id(browser_id.clone()),
                async { Err::<String, _>(anyhow!("model exploded")) },
            )
            .await
        {
            Ok(_) => panic!("run should fail"),
            Err(error) => error,
        };

        assert!(err.to_string().contains("model exploded"));
        let snapshot = handle.browsers().snapshot(&browser_id)?;
        assert_eq!(snapshot.active_agent_id, None);
        assert_eq!(snapshot.status, BrowserStatus::Released);
        assert_eq!(
            handle.snapshot_agent(root.agent_id())?.status,
            AgentThreadStatus::Failed
        );
        assert_eq!(
            journal
                .load_session(root.session_id())?
                .expect("root session")
                .status,
            SessionStatus::Failed
        );
        let events = journal.events_for_session(root.session_id())?;
        let event_types = events
            .iter()
            .map(|event| event.event_type.as_str())
            .collect::<Vec<_>>();
        assert!(event_types.contains(&"agent.turn.aborted"));
        assert!(event_types.contains(&"session.failed"));
        assert!(event_types.contains(&"agent.failed"));
        let failed = events
            .iter()
            .find(|event| event.event_type == "agent.failed")
            .expect("agent.failed");
        assert_eq!(
            failed.payload["payload"]["terminal_event_type"],
            "session.failed"
        );
        Ok(())
    }

    #[tokio::test]
    async fn run_start_barrier_failure_does_not_mark_agent_running() -> Result<()> {
        let (handle, journal) = runtime_with_failing_journal();
        let root = handle.create_root_agent(CreateRootAgentRequest {
            cwd: PathBuf::from("/tmp"),
            task: "root task".to_string(),
            max_concurrent_threads_per_session: 3,
        })?;
        journal.fail_event_type("agent.started");

        let err = handle
            .run_agent(RunAgentRequest::new(root.session_id().clone()), async {
                Ok::<_, anyhow::Error>("done".to_string())
            })
            .await
            .expect_err("agent.started barrier failure must fail run");

        assert!(err.to_string().contains("forced journal failure"));
        let snapshot = root.snapshot();
        assert_eq!(snapshot.status, AgentThreadStatus::Created);
        assert_eq!(snapshot.live.current_run_id, None);
        Ok(())
    }

    #[tokio::test]
    async fn run_completion_barrier_failure_does_not_mark_agent_completed() -> Result<()> {
        let (handle, journal) = runtime_with_failing_journal();
        let root = handle.create_root_agent(CreateRootAgentRequest {
            cwd: PathBuf::from("/tmp"),
            task: "root task".to_string(),
            max_concurrent_threads_per_session: 3,
        })?;
        journal.fail_event_type("agent.turn.completed");

        let err = handle
            .run_agent(RunAgentRequest::new(root.session_id().clone()), async {
                Ok::<_, anyhow::Error>("done".to_string())
            })
            .await
            .expect_err("agent.turn.completed barrier failure must fail run");

        assert!(err.to_string().contains("forced journal failure"));
        let snapshot = root.snapshot();
        assert_eq!(snapshot.status, AgentThreadStatus::Running);
        assert_eq!(snapshot.live.current_run_id, None);
        assert!(!handle.cancel_run(root.session_id()));
        Ok(())
    }

    #[test]
    fn runtime_session_resources_are_owned_by_agent_thread() -> Result<()> {
        let (runtime, _journal) = BrowserUseRuntime::memory();
        let handle = runtime.handle();
        let root = handle.create_root_agent(CreateRootAgentRequest {
            cwd: PathBuf::from("/tmp"),
            task: "root task".to_string(),
            max_concurrent_threads_per_session: 3,
        })?;

        let cleaned = Arc::new(Mutex::new(0usize));
        let cleaned_for_callback = Arc::clone(&cleaned);
        let first = handle.get_or_insert_session_resource(
            root.session_id(),
            "test.counter",
            || Mutex::new(41usize),
            move |counter: Arc<Mutex<usize>>| {
                let value = *counter
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                *cleaned_for_callback
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = value;
                1
            },
        )?;
        let second = handle.get_or_insert_session_resource(
            root.session_id(),
            "test.counter",
            || Mutex::new(999usize),
            |_counter: Arc<Mutex<usize>>| 1,
        )?;

        assert!(Arc::ptr_eq(&first, &second));
        *first
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = 42;
        assert_eq!(handle.cleanup_session_resources(root.session_id())?, 1);
        assert_eq!(
            *cleaned
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            42
        );
        assert_eq!(handle.cleanup_session_resources(root.session_id())?, 0);
        Ok(())
    }

    #[test]
    fn close_agent_cleans_runtime_owned_resources() -> Result<()> {
        let (runtime, _journal) = BrowserUseRuntime::memory();
        let handle = runtime.handle();
        let root = handle.create_root_agent(CreateRootAgentRequest {
            cwd: PathBuf::from("/tmp"),
            task: "root task".to_string(),
            max_concurrent_threads_per_session: 3,
        })?;
        let cleaned = Arc::new(Mutex::new(false));
        let cleaned_for_callback = Arc::clone(&cleaned);
        handle.get_or_insert_session_resource(
            root.session_id(),
            "test.close_cleanup",
            || "resource".to_string(),
            move |_resource: Arc<String>| {
                *cleaned_for_callback
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = true;
                1
            },
        )?;

        handle.close_agent(CloseAgentRequest {
            agent_id: root.agent_id().clone(),
            reason: "test close".to_string(),
        })?;

        assert!(*cleaned
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner));
        Ok(())
    }

    #[test]
    fn close_agent_closes_durable_only_descendant_edges() -> Result<()> {
        let (runtime, journal) = BrowserUseRuntime::memory();
        let handle = runtime.handle();
        let root = handle.create_root_agent(CreateRootAgentRequest {
            cwd: PathBuf::from("/tmp"),
            task: "root task".to_string(),
            max_concurrent_threads_per_session: 3,
        })?;
        let child = handle.spawn_child(SpawnChildRequest {
            parent_agent_id: root.agent_id().clone(),
            child_agent_id: None,
            child_session_id: None,
            task_name: "research".to_string(),
            message: "inspect docs".to_string(),
            nickname: Some("Curie".to_string()),
            role: Some("explorer".to_string()),
        })?;
        let durable_grandchild = SessionId::from_string("durable-grandchild")?;
        journal.create_thread(CreateThreadRequest {
            session_id: Some(durable_grandchild.clone()),
            parent_session_id: Some(child.session_id().clone()),
            cwd: PathBuf::from("/tmp"),
            artifact_root: None,
            agent_path: Some("/root/research/grandchild".to_string()),
            nickname: Some("Durable".to_string()),
            role: Some("explorer".to_string()),
        })?;

        handle.close_agent(CloseAgentRequest {
            agent_id: child.agent_id().clone(),
            reason: "done inspecting".to_string(),
        })?;

        let root_children = journal.list_children(root.session_id())?;
        assert_eq!(root_children[0].status, SpawnEdgeStatus::Closed);
        let child_children = journal.list_children(child.session_id())?;
        assert_eq!(child_children[0].child_session_id, durable_grandchild);
        assert_eq!(child_children[0].status, SpawnEdgeStatus::Closed);
        Ok(())
    }

    #[test]
    fn close_request_barrier_failure_does_not_clean_or_close_agent() -> Result<()> {
        let (handle, journal) = runtime_with_failing_journal();
        let root = handle.create_root_agent(CreateRootAgentRequest {
            cwd: PathBuf::from("/tmp"),
            task: "root task".to_string(),
            max_concurrent_threads_per_session: 3,
        })?;
        let cleaned = Arc::new(Mutex::new(false));
        let cleaned_for_callback = Arc::clone(&cleaned);
        handle.get_or_insert_session_resource(
            root.session_id(),
            "test.close_cleanup",
            || "resource".to_string(),
            move |_resource: Arc<String>| {
                *cleaned_for_callback
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = true;
                1
            },
        )?;
        journal.fail_event_type("agent.close_requested");

        let err = handle
            .close_agent(CloseAgentRequest {
                agent_id: root.agent_id().clone(),
                reason: "test close".to_string(),
            })
            .expect_err("close request must fail before live cleanup");

        assert!(err.to_string().contains("forced journal failure"));
        assert!(!*cleaned
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner));
        assert_eq!(root.snapshot().status, AgentThreadStatus::Created);
        let event_types = journal
            .events_for_session(root.session_id())?
            .into_iter()
            .map(|event| event.event_type)
            .collect::<Vec<_>>();
        assert!(!event_types.contains(&"agent.close_requested".to_string()));
        assert!(!event_types.contains(&"agent.closed".to_string()));
        Ok(())
    }

    #[test]
    fn close_terminal_barrier_failure_does_not_mark_agent_closed() -> Result<()> {
        let (handle, journal) = runtime_with_failing_journal();
        let root = handle.create_root_agent(CreateRootAgentRequest {
            cwd: PathBuf::from("/tmp"),
            task: "root task".to_string(),
            max_concurrent_threads_per_session: 3,
        })?;
        let cleaned = Arc::new(Mutex::new(false));
        let cleaned_for_callback = Arc::clone(&cleaned);
        handle.get_or_insert_session_resource(
            root.session_id(),
            "test.close_cleanup",
            || "resource".to_string(),
            move |_resource: Arc<String>| {
                *cleaned_for_callback
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = true;
                1
            },
        )?;
        journal.fail_event_type("agent.closed");

        let err = handle
            .close_agent(CloseAgentRequest {
                agent_id: root.agent_id().clone(),
                reason: "test close".to_string(),
            })
            .expect_err("terminal close must fail before closed status");

        assert!(err.to_string().contains("forced journal failure"));
        assert!(*cleaned
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner));
        assert_eq!(root.snapshot().status, AgentThreadStatus::Created);
        let event_types = journal
            .events_for_session(root.session_id())?
            .into_iter()
            .map(|event| event.event_type)
            .collect::<Vec<_>>();
        assert!(event_types.contains(&"agent.close_requested".to_string()));
        assert!(!event_types.contains(&"agent.closed".to_string()));
        Ok(())
    }

    #[test]
    fn resume_marks_unclosed_exec_resources_lost_once() -> Result<()> {
        let (runtime, journal) = BrowserUseRuntime::memory();
        let handle = runtime.handle();
        let root = journal.create_thread(CreateThreadRequest {
            session_id: Some(SessionId::from_string("root")?),
            parent_session_id: None,
            cwd: PathBuf::from("/tmp"),
            artifact_root: None,
            agent_path: None,
            nickname: None,
            role: None,
        })?;
        let session_id = SessionId::from_string(root.id)?;
        journal.append_session_event(
            &session_id,
            "exec_command.begin",
            json!({ "process_id": "123", "session_id": 123 }),
            Durability::Barrier,
        )?;

        for _ in 0..2 {
            handle.attach_root_agent(AttachRootAgentRequest {
                session_id: session_id.clone(),
                cwd: PathBuf::from("/tmp"),
                task: "resume".to_string(),
                max_concurrent_threads_per_session: 4,
            })?;
        }

        let lost = journal
            .events_for_session(&session_id)?
            .into_iter()
            .filter(|event| event.event_type == "resource.lost")
            .collect::<Vec<_>>();
        assert_eq!(lost.len(), 1);
        assert_eq!(
            lost[0].payload["payload"]["resource"]["kind"],
            "exec_command"
        );
        assert_eq!(lost[0].payload["payload"]["resource"]["id"], "123");
        Ok(())
    }

    #[test]
    fn resume_marks_all_unclosed_tool_resources_lost_once() -> Result<()> {
        let (runtime, journal) = BrowserUseRuntime::memory();
        let handle = runtime.handle();
        let root = journal.create_thread(CreateThreadRequest {
            session_id: Some(SessionId::from_string("root")?),
            parent_session_id: None,
            cwd: PathBuf::from("/tmp"),
            artifact_root: None,
            agent_path: None,
            nickname: None,
            role: None,
        })?;
        let session_id = SessionId::from_string(root.id)?;
        for (event_type, payload) in [
            (
                "exec_command.begin",
                json!({ "process_id": "exec-1", "session_id": 1 }),
            ),
            ("browser_script.started", json!({ "run_id": "browser-1" })),
            ("python.started", json!({ "tool_call_id": "python-1" })),
            ("mcp.tool.started", json!({ "tool_call_id": "mcp-1" })),
        ] {
            journal.append_session_event(&session_id, event_type, payload, Durability::Barrier)?;
        }

        for _ in 0..2 {
            handle.attach_root_agent(AttachRootAgentRequest {
                session_id: session_id.clone(),
                cwd: PathBuf::from("/tmp"),
                task: "resume".to_string(),
                max_concurrent_threads_per_session: 4,
            })?;
        }

        let mut lost = journal
            .events_for_session(&session_id)?
            .into_iter()
            .filter(|event| event.event_type == "resource.lost")
            .map(|event| {
                (
                    event.payload["payload"]["resource"]["kind"]
                        .as_str()
                        .unwrap_or_default()
                        .to_string(),
                    event.payload["payload"]["resource"]["id"]
                        .as_str()
                        .unwrap_or_default()
                        .to_string(),
                )
            })
            .collect::<Vec<_>>();
        lost.sort();
        assert_eq!(
            lost,
            vec![
                ("browser_script".to_string(), "browser-1".to_string()),
                ("exec_command".to_string(), "exec-1".to_string()),
                ("mcp".to_string(), "mcp-1".to_string()),
                ("python".to_string(), "python-1".to_string()),
            ]
        );
        Ok(())
    }

    #[test]
    fn resume_does_not_mark_completed_exec_resources_lost() -> Result<()> {
        let (runtime, journal) = BrowserUseRuntime::memory();
        let handle = runtime.handle();
        let root = journal.create_thread(CreateThreadRequest {
            session_id: Some(SessionId::from_string("root")?),
            parent_session_id: None,
            cwd: PathBuf::from("/tmp"),
            artifact_root: None,
            agent_path: None,
            nickname: None,
            role: None,
        })?;
        let session_id = SessionId::from_string(root.id)?;
        journal.append_session_event(
            &session_id,
            "exec_command.begin",
            json!({ "process_id": "123", "session_id": 123 }),
            Durability::Barrier,
        )?;
        journal.append_session_event(
            &session_id,
            "exec_command.end",
            json!({ "process_id": "123", "session_id": 123, "exit_code": 0 }),
            Durability::Barrier,
        )?;

        handle.attach_root_agent(AttachRootAgentRequest {
            session_id: session_id.clone(),
            cwd: PathBuf::from("/tmp"),
            task: "resume".to_string(),
            max_concurrent_threads_per_session: 4,
        })?;

        let lost = journal
            .events_for_session(&session_id)?
            .into_iter()
            .filter(|event| event.event_type == "resource.lost")
            .count();
        assert_eq!(lost, 0);
        Ok(())
    }

    #[test]
    fn sqlite_journal_uses_store_event_rows() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let journal = SqliteJournal::open(dir.path())?;
        let session = journal.create_thread(CreateThreadRequest {
            session_id: None,
            parent_session_id: None,
            cwd: PathBuf::from("/tmp"),
            artifact_root: None,
            agent_path: None,
            nickname: None,
            role: None,
        })?;
        let session_id = SessionId::from_string(session.id)?;
        journal.append_runtime_event(
            &RuntimeEvent::new(RuntimeEventKind::MailboxEnqueued, Durability::Barrier)
                .with_session_id(session_id.clone())
                .with_payload(json!({"target": "parent"})),
        )?;
        let events = journal.events_for_session(&session_id)?;
        assert_eq!(events.len(), 2);
        assert_eq!(events[1].event_type, "mailbox.enqueued");
        assert_eq!(events[1].payload["durability"], "barrier");
        Ok(())
    }

    #[test]
    fn sqlite_journal_persists_child_thread_tree() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let journal = Arc::new(SqliteJournal::open(dir.path())?);
        let persistence: Arc<dyn LiveThreadPersistence> = journal.clone();
        let state_index: Arc<dyn StateIndex> = journal.clone();
        let handle = BrowserUseRuntime::new(persistence, state_index).handle();

        let root = handle.create_root_agent(CreateRootAgentRequest {
            cwd: PathBuf::from("/tmp"),
            task: "root task".to_string(),
            max_concurrent_threads_per_session: 3,
        })?;
        let child = handle.spawn_child(SpawnChildRequest {
            parent_agent_id: root.agent_id().clone(),
            child_agent_id: None,
            child_session_id: None,
            task_name: "research".to_string(),
            message: "inspect docs".to_string(),
            nickname: Some("Curie".to_string()),
            role: Some("explorer".to_string()),
        })?;

        let children = journal.list_children(root.session_id())?;
        assert_eq!(children.len(), 1);
        assert_eq!(children[0].child_session_id, child.session_id().clone());
        assert_eq!(children[0].path.as_deref(), Some("/root/research"));
        assert_eq!(children[0].nickname.as_deref(), Some("Curie"));
        assert_eq!(children[0].role.as_deref(), Some("explorer"));

        handle.close_agent(CloseAgentRequest {
            agent_id: child.agent_id().clone(),
            reason: "done inspecting".to_string(),
        })?;
        let children = journal.list_children(root.session_id())?;
        assert_eq!(children[0].status, SpawnEdgeStatus::Closed);
        Ok(())
    }

    #[test]
    fn runtime_appends_observed_protocol_events_without_wrapping_payload() -> Result<()> {
        let (runtime, journal) = BrowserUseRuntime::memory();
        let handle = runtime.handle();
        let root = handle.create_root_agent(CreateRootAgentRequest {
            cwd: PathBuf::from("/tmp"),
            task: "root task".to_string(),
            max_concurrent_threads_per_session: 3,
        })?;
        let mut subscription = handle.subscribe_projected();

        handle.append_observed_session_event(
            root.session_id().clone(),
            "exec_command.begin",
            json!({"command": ["pwd"]}),
            Durability::Barrier,
        )?;

        let events = journal.events_for_session(root.session_id())?;
        let observed = events
            .iter()
            .find(|event| event.event_type == "exec_command.begin")
            .context("observed event")?;
        assert_eq!(observed.payload, json!({"command": ["pwd"]}));

        let rt = tokio::runtime::Runtime::new()?;
        let projected = rt.block_on(subscription.recv())?;
        assert_eq!(projected.kind, ProjectedEventKind::ItemStarted);
        assert_eq!(projected.payload["event_type"], "exec_command.begin");
        assert_eq!(projected.payload["payload"], json!({"command": ["pwd"]}));
        Ok(())
    }

    #[test]
    fn runtime_attaches_existing_sqlite_root_and_uses_caller_child_id() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let journal = Arc::new(SqliteJournal::open(dir.path())?);
        let root_session = journal.create_thread(CreateThreadRequest {
            session_id: None,
            parent_session_id: None,
            cwd: PathBuf::from("/tmp"),
            artifact_root: None,
            agent_path: None,
            nickname: None,
            role: None,
        })?;
        let root_session_id = SessionId::from_string(root_session.id)?;

        let persistence: Arc<dyn LiveThreadPersistence> = journal.clone();
        let state_index: Arc<dyn StateIndex> = journal.clone();
        let handle = BrowserUseRuntime::new(persistence, state_index).handle();
        let root = handle.attach_root_agent(AttachRootAgentRequest {
            session_id: root_session_id.clone(),
            cwd: PathBuf::from("/tmp"),
            task: "attached root".to_string(),
            max_concurrent_threads_per_session: 3,
        })?;
        assert_eq!(root.session_id(), &root_session_id);
        assert_eq!(root.agent_id().as_str(), root_session_id.as_str());

        let child_agent_id = AgentId::from_string("child-agent-1")?;
        let child_session_id = SessionId::from_string("child-session-1")?;
        let child = handle.spawn_child(SpawnChildRequest {
            parent_agent_id: root.agent_id().clone(),
            child_agent_id: Some(child_agent_id.clone()),
            child_session_id: Some(child_session_id.clone()),
            task_name: "research".to_string(),
            message: "inspect docs".to_string(),
            nickname: None,
            role: None,
        })?;
        assert_eq!(child.agent_id(), &child_agent_id);
        assert_eq!(child.session_id(), &child_session_id);

        let children = journal.list_children(&root_session_id)?;
        assert_eq!(children.len(), 1);
        assert_eq!(children[0].child_session_id, child_session_id);
        assert_eq!(children[0].path.as_deref(), Some("/root/research"));
        Ok(())
    }

    #[tokio::test]
    async fn runtime_spawn_completion_and_wait_are_mailbox_first() -> Result<()> {
        let (runtime, journal) = BrowserUseRuntime::memory();
        let handle = runtime.handle();
        let root = handle.create_root_agent(CreateRootAgentRequest {
            cwd: PathBuf::from("/tmp"),
            task: "root task".to_string(),
            max_concurrent_threads_per_session: 3,
        })?;
        let child = handle.spawn_child(SpawnChildRequest {
            parent_agent_id: root.agent_id().clone(),
            child_agent_id: None,
            child_session_id: None,
            task_name: "research".to_string(),
            message: "inspect docs".to_string(),
            nickname: Some("Curie".to_string()),
            role: Some("explorer".to_string()),
        })?;

        let edges = journal.list_children(root.session_id())?;
        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0].child_session_id, child.session_id().clone());
        assert_eq!(edges[0].path.as_deref(), Some("/root/research"));

        handle.complete_agent(CompleteAgentRequest {
            child_agent_id: child.agent_id().clone(),
            result: "findings ready".to_string(),
        })?;
        let edges = journal.list_children(root.session_id())?;
        assert_eq!(edges[0].status, SpawnEdgeStatus::Done);

        let pending = root.mailbox().pending_items();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].author_agent_id, child.agent_id().clone());
        assert!(
            !pending[0].trigger_turn,
            "child completion must not auto-trigger parent"
        );
        assert_eq!(
            pending[0].delivery_phase,
            MailboxDeliveryPhase::CurrentTurn,
            "active-turn child completion mail must be deliverable after wait_agent"
        );

        let outcome = handle
            .wait_agent(
                root.agent_id(),
                AgentTarget::AgentId(child.agent_id().clone()),
                Duration::from_millis(20),
            )
            .await?;
        let WaitAgentOutcome::Completed(item) = outcome else {
            panic!("expected wait completion");
        };
        assert!(item.content.contains("<subagent_notification>"));
        assert!(item.content.contains("findings ready"));
        assert_eq!(
            root.mailbox().pending_items().len(),
            1,
            "wait_agent observes mail but does not drain it"
        );
        let delivered = handle.drain_agent_mailbox(DrainAgentMailboxRequest {
            session_id: root.session_id().clone(),
            delivery_phase: MailboxDeliveryPhase::CurrentTurn,
        })?;
        assert_eq!(delivered.mailbox_items.len(), 1);
        assert!(delivered.mailbox_items[0]
            .content
            .contains("<subagent_notification>"));

        let root_events = journal.events_for_session(root.session_id())?;
        let root_event_types = root_events
            .iter()
            .map(|event| event.event_type.as_str())
            .collect::<Vec<_>>();
        assert!(root_event_types.contains(&"subagent.spawn_started"));
        assert!(root_events.iter().any(|event| {
            event.event_type == "agent.completed"
                && event.payload["payload"]["child_session_id"] == child.session_id().as_str()
                && event.payload["payload"]["runtime_owned"] == true
        }));
        assert!(root_event_types.contains(&"mailbox.enqueued"));
        assert!(root_event_types.contains(&"wait_agent.completed"));
        Ok(())
    }

    #[tokio::test]
    async fn runtime_wait_agent_advances_observed_cursor_without_draining() -> Result<()> {
        let (runtime, _journal) = BrowserUseRuntime::memory();
        let handle = runtime.handle();
        let root = handle.create_root_agent(CreateRootAgentRequest {
            cwd: PathBuf::from("/tmp"),
            task: "root task".to_string(),
            max_concurrent_threads_per_session: 3,
        })?;
        let child = handle.spawn_child(SpawnChildRequest {
            parent_agent_id: root.agent_id().clone(),
            child_agent_id: None,
            child_session_id: None,
            task_name: "research".to_string(),
            message: "inspect docs".to_string(),
            nickname: Some("Curie".to_string()),
            role: Some("explorer".to_string()),
        })?;

        handle.complete_agent(CompleteAgentRequest {
            child_agent_id: child.agent_id().clone(),
            result: "first findings".to_string(),
        })?;

        let first = handle
            .wait_agent(root.agent_id(), AgentTarget::Any, Duration::from_millis(20))
            .await?;
        let WaitAgentOutcome::Completed(first_item) = first else {
            panic!("expected first mailbox item");
        };
        assert_eq!(first_item.seq, 1);
        assert_eq!(
            root.mailbox().pending_items().len(),
            1,
            "wait_agent must observe mail without draining content delivery"
        );

        let repeated = handle
            .wait_agent(root.agent_id(), AgentTarget::Any, Duration::from_millis(1))
            .await?;
        assert_eq!(
            repeated,
            WaitAgentOutcome::TimedOut,
            "a second wait must block for newer mail instead of returning the same seq"
        );
        assert_eq!(root.live_state_snapshot().last_wait_observed_mailbox_seq, 1);

        let peer = handle.spawn_child(SpawnChildRequest {
            parent_agent_id: root.agent_id().clone(),
            child_agent_id: None,
            child_session_id: None,
            task_name: "second".to_string(),
            message: "inspect more".to_string(),
            nickname: Some("Noether".to_string()),
            role: Some("explorer".to_string()),
        })?;
        handle.complete_agent(CompleteAgentRequest {
            child_agent_id: peer.agent_id().clone(),
            result: "second findings".to_string(),
        })?;
        let next = handle
            .wait_agent(root.agent_id(), AgentTarget::Any, Duration::from_millis(20))
            .await?;
        let WaitAgentOutcome::Completed(next_item) = next else {
            panic!("expected newer mailbox item");
        };
        assert_eq!(next_item.seq, 2);
        Ok(())
    }

    #[tokio::test]
    async fn runtime_send_agent_message_journals_then_wakes_mailbox() -> Result<()> {
        let (runtime, journal) = BrowserUseRuntime::memory();
        let handle = runtime.handle();
        let root = handle.create_root_agent(CreateRootAgentRequest {
            cwd: PathBuf::from("/tmp"),
            task: "root task".to_string(),
            max_concurrent_threads_per_session: 3,
        })?;
        let child = handle.spawn_child(SpawnChildRequest {
            parent_agent_id: root.agent_id().clone(),
            child_agent_id: None,
            child_session_id: None,
            task_name: "research".to_string(),
            message: "inspect docs".to_string(),
            nickname: None,
            role: None,
        })?;

        let sent = handle.send_agent_message(SendAgentMessageRequest {
            author_agent_id: child.agent_id().clone(),
            target_agent_id: root.agent_id().clone(),
            content: "status update".to_string(),
            trigger_turn: false,
            kind: MailboxItemKind::Input,
            delivery_phase: MailboxDeliveryPhase::NextTurn,
            payload: json!({"source": "test"}),
        })?;
        assert_eq!(sent.mailbox_item.content, "status update");

        let outcome = handle
            .wait_agent(root.agent_id(), AgentTarget::Any, Duration::from_millis(20))
            .await?;
        let WaitAgentOutcome::Completed(item) = outcome else {
            panic!("expected runtime wait to wake from sent message");
        };
        assert_eq!(item.kind, MailboxItemKind::Input);
        assert_eq!(item.content, "status update");
        assert_eq!(root.mailbox().pending_items().len(), 1);

        let root_events = journal.events_for_session(root.session_id())?;
        let mailbox_event = root_events
            .iter()
            .find(|event| event.event_type == "mailbox.enqueued")
            .context("mailbox.enqueued event")?;
        assert_eq!(mailbox_event.payload["payload"]["trigger_turn"], false);
        assert_eq!(
            mailbox_event.payload["payload"]["mailbox_item"]["seq"],
            sent.mailbox_item.seq
        );
        let live = root.live_state_snapshot();
        assert_eq!(live.pending_mailbox_count, 1);
        assert_eq!(live.pending_trigger_turn_count, 0);
        assert_eq!(live.last_enqueued_mailbox_seq, sent.mailbox_item.seq);

        let err = handle
            .send_agent_message(SendAgentMessageRequest {
                author_agent_id: child.agent_id().clone(),
                target_agent_id: root.agent_id().clone(),
                content: "new root task".to_string(),
                trigger_turn: true,
                kind: MailboxItemKind::Followup,
                delivery_phase: MailboxDeliveryPhase::NextTurn,
                payload: json!({}),
            })
            .expect_err("trigger_turn messages to root must be rejected");
        assert!(err.to_string().contains("root agent"));
        Ok(())
    }

    #[test]
    fn runtime_submit_input_is_barrier_backed_self_followup() -> Result<()> {
        let (runtime, journal) = BrowserUseRuntime::memory();
        let handle = runtime.handle();
        let root = handle.create_root_agent(CreateRootAgentRequest {
            cwd: PathBuf::from("/tmp"),
            task: "root task".to_string(),
            max_concurrent_threads_per_session: 3,
        })?;

        let submitted = handle.submit_input(SubmitInputRequest {
            target_agent_id: root.agent_id().clone(),
            content: "operator follow-up".to_string(),
            trigger_turn: true,
            delivery_phase: MailboxDeliveryPhase::CurrentTurn,
            input_items: Some(json!([{ "type": "text", "text": "operator follow-up" }])),
            payload: json!({ "source": "test" }),
        })?;

        assert_eq!(submitted.mailbox_item.kind, MailboxItemKind::Followup);
        assert!(submitted.mailbox_item.trigger_turn);
        assert_eq!(
            submitted.mailbox_item.author_agent_id,
            root.agent_id().clone()
        );
        assert_eq!(
            submitted.mailbox_item.target_agent_id,
            root.agent_id().clone()
        );
        assert_eq!(
            submitted.mailbox_item.payload["target_session_id"].as_str(),
            Some(root.session_id().as_str())
        );
        assert!(submitted.mailbox_item.payload["input_items"].is_array());
        assert_eq!(root.mailbox().pending_items().len(), 1);
        let live = root.live_state_snapshot();
        assert_eq!(live.accepted_followup_count, 1);
        assert_eq!(live.pending_mailbox_count, 1);
        assert_eq!(live.pending_trigger_turn_count, 1);
        assert_eq!(live.last_enqueued_mailbox_seq, submitted.mailbox_item.seq);

        let root_events = journal.events_for_session(root.session_id())?;
        let mailbox_event = root_events
            .iter()
            .find(|event| event.event_type == "mailbox.enqueued")
            .context("mailbox.enqueued event")?;
        assert_eq!(
            mailbox_event.payload["payload"]["mailbox_item"]["id"],
            submitted.mailbox_item.id
        );
        assert_eq!(
            mailbox_event.payload["payload"]["mailbox_item"]["seq"],
            submitted.mailbox_item.seq
        );
        let drained = handle.drain_agent_mailbox(DrainAgentMailboxRequest {
            session_id: root.session_id().clone(),
            delivery_phase: MailboxDeliveryPhase::CurrentTurn,
        })?;
        assert_eq!(drained.mailbox_items.len(), 1);
        let live = root.live_state_snapshot();
        assert_eq!(live.pending_mailbox_count, 0);
        assert_eq!(live.pending_trigger_turn_count, 0);
        assert_eq!(live.last_delivered_mailbox_seq, submitted.mailbox_item.seq);
        assert_eq!(live.last_consumed_mailbox_seq, submitted.mailbox_item.seq);
        let event_types = journal
            .events_for_session(root.session_id())?
            .into_iter()
            .map(|event| event.event_type)
            .collect::<Vec<_>>();
        assert!(event_types.contains(&"mailbox.delivered".to_string()));
        assert!(event_types.contains(&"mailbox.consumed".to_string()));
        Ok(())
    }

    #[test]
    fn runtime_prompt_input_is_barrier_backed_and_one_shot() -> Result<()> {
        let (runtime, journal) = BrowserUseRuntime::memory();
        let handle = runtime.handle();
        let root = handle.create_root_agent(CreateRootAgentRequest {
            cwd: PathBuf::from("/tmp"),
            task: "root task".to_string(),
            max_concurrent_threads_per_session: 3,
        })?;

        let accepted = handle.accept_prompt_input(AcceptPromptInputRequest {
            target_agent_id: root.agent_id().clone(),
            source_event_seq: Some(42),
            payload: json!({ "source": "test" }),
        })?;
        assert!(accepted.accepted);
        let duplicate = handle.accept_prompt_input(AcceptPromptInputRequest {
            target_agent_id: root.agent_id().clone(),
            source_event_seq: Some(42),
            payload: json!({ "source": "test" }),
        })?;
        assert!(!duplicate.accepted);

        let live = root.live_state_snapshot();
        assert_eq!(live.accepted_input_count, 1);
        assert_eq!(live.pending_prompt_input_count, 1);
        assert_eq!(live.last_accepted_prompt_input_seq, 42);
        assert!(root.mailbox().pending_items().is_empty());

        let consumed = handle.consume_prompt_input_for_session(root.session_id())?;
        assert!(consumed.consumed);
        let consumed_again = handle.consume_prompt_input_for_session(root.session_id())?;
        assert!(!consumed_again.consumed);
        let live = root.live_state_snapshot();
        assert_eq!(live.pending_prompt_input_count, 0);
        assert_eq!(live.last_consumed_prompt_input_seq, 42);

        let event_types = journal
            .events_for_session(root.session_id())?
            .into_iter()
            .map(|event| event.event_type)
            .collect::<Vec<_>>();
        assert_eq!(
            event_types
                .iter()
                .filter(|event_type| event_type.as_str() == "agent.input.accepted")
                .count(),
            1
        );
        assert_eq!(
            event_types
                .iter()
                .filter(|event_type| event_type.as_str() == "agent.input.consumed")
                .count(),
            1
        );
        Ok(())
    }

    #[tokio::test]
    async fn attach_root_materializes_pending_mailbox_from_journal() -> Result<()> {
        let journal = Arc::new(MemoryJournal::new());
        let session = journal.create_thread(CreateThreadRequest {
            session_id: Some(SessionId::from_string("root")?),
            parent_session_id: None,
            cwd: PathBuf::from("/tmp"),
            artifact_root: None,
            agent_path: None,
            nickname: None,
            role: None,
        })?;
        let session_id = SessionId::from_string(session.id)?;
        let root_agent_id = AgentId::from_string(session_id.as_str())?;
        let child_agent_id = AgentId::from_string("child")?;
        let item = MailboxItem {
            seq: 7,
            id: "mail-7".to_string(),
            kind: MailboxItemKind::Completion,
            author_agent_id: child_agent_id.clone(),
            target_agent_id: root_agent_id.clone(),
            target_path: Some("/root/research".to_string()),
            content: "<subagent_notification>done</subagent_notification>".to_string(),
            trigger_turn: false,
            delivery_phase: MailboxDeliveryPhase::NextTurn,
            payload: json!({"source": "test"}),
        };
        journal.append_runtime_event(
            &RuntimeEvent::new(RuntimeEventKind::MailboxEnqueued, Durability::Barrier)
                .with_session_id(session_id.clone())
                .with_agent_id(root_agent_id.clone())
                .with_payload(json!({
                    "mailbox_item": item,
                    "trigger_turn": false,
                })),
        )?;

        let persistence: Arc<dyn LiveThreadPersistence> = journal.clone();
        let state_index: Arc<dyn StateIndex> = journal.clone();
        let handle = BrowserUseRuntime::new(persistence, state_index).handle();
        let root = handle.attach_root_agent(AttachRootAgentRequest {
            session_id: session_id.clone(),
            cwd: PathBuf::from("/tmp"),
            task: "resume".to_string(),
            max_concurrent_threads_per_session: 3,
        })?;

        let pending = handle.pending_agent_mail_for_session(&session_id)?;
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].seq, 7);
        assert_eq!(pending[0].author_agent_id, child_agent_id);
        let live = root.live_state_snapshot();
        assert_eq!(live.pending_mailbox_count, 1);
        assert_eq!(live.last_enqueued_mailbox_seq, 7);

        let outcome = handle
            .wait_agent(root.agent_id(), AgentTarget::Any, Duration::from_millis(20))
            .await?;
        let WaitAgentOutcome::Completed(item) = outcome else {
            panic!("materialized mail should wake wait_agent immediately");
        };
        assert_eq!(item.seq, 7);
        assert_eq!(root.live_state_snapshot().last_wait_observed_mailbox_seq, 7);
        let repeated = handle
            .wait_agent(root.agent_id(), AgentTarget::Any, Duration::from_millis(1))
            .await?;
        assert_eq!(repeated, WaitAgentOutcome::TimedOut);

        let persistence: Arc<dyn LiveThreadPersistence> = journal.clone();
        let state_index: Arc<dyn StateIndex> = journal.clone();
        let handle_after_resume = BrowserUseRuntime::new(persistence, state_index).handle();
        let resumed_root = handle_after_resume.attach_root_agent(AttachRootAgentRequest {
            session_id: session_id.clone(),
            cwd: PathBuf::from("/tmp"),
            task: "resume again".to_string(),
            max_concurrent_threads_per_session: 3,
        })?;
        assert_eq!(
            resumed_root
                .live_state_snapshot()
                .last_wait_observed_mailbox_seq,
            7
        );
        let after_resume = handle_after_resume
            .wait_agent(
                resumed_root.agent_id(),
                AgentTarget::Any,
                Duration::from_millis(1),
            )
            .await?;
        assert_eq!(after_resume, WaitAgentOutcome::TimedOut);
        Ok(())
    }

    #[test]
    fn attach_root_does_not_materialize_consumed_mailbox_from_journal() -> Result<()> {
        let journal = Arc::new(MemoryJournal::new());
        let session = journal.create_thread(CreateThreadRequest {
            session_id: Some(SessionId::from_string("root")?),
            parent_session_id: None,
            cwd: PathBuf::from("/tmp"),
            artifact_root: None,
            agent_path: None,
            nickname: None,
            role: None,
        })?;
        let session_id = SessionId::from_string(session.id)?;
        let root_agent_id = AgentId::from_string(session_id.as_str())?;
        let item = MailboxItem {
            seq: 3,
            id: "mail-3".to_string(),
            kind: MailboxItemKind::Followup,
            author_agent_id: root_agent_id.clone(),
            target_agent_id: root_agent_id.clone(),
            target_path: Some("/root".to_string()),
            content: "continue".to_string(),
            trigger_turn: true,
            delivery_phase: MailboxDeliveryPhase::CurrentTurn,
            payload: json!({"source": "test"}),
        };
        journal.append_runtime_event(
            &RuntimeEvent::new(RuntimeEventKind::MailboxEnqueued, Durability::Barrier)
                .with_session_id(session_id.clone())
                .with_agent_id(root_agent_id.clone())
                .with_payload(json!({
                    "mailbox_item": item,
                    "trigger_turn": true,
                })),
        )?;
        journal.append_runtime_event(
            &RuntimeEvent::new(RuntimeEventKind::MailboxConsumed, Durability::Barrier)
                .with_session_id(session_id.clone())
                .with_agent_id(root_agent_id)
                .with_payload(json!({
                    "delivery_phase": MailboxDeliveryPhase::CurrentTurn,
                    "mailbox_seqs": [3],
                })),
        )?;

        let persistence: Arc<dyn LiveThreadPersistence> = journal.clone();
        let state_index: Arc<dyn StateIndex> = journal.clone();
        let handle = BrowserUseRuntime::new(persistence, state_index).handle();
        let root = handle.attach_root_agent(AttachRootAgentRequest {
            session_id: session_id.clone(),
            cwd: PathBuf::from("/tmp"),
            task: "resume".to_string(),
            max_concurrent_threads_per_session: 3,
        })?;

        assert!(handle
            .pending_agent_mail_for_session(&session_id)?
            .is_empty());
        let live = root.live_state_snapshot();
        assert_eq!(live.pending_mailbox_count, 0);
        assert_eq!(live.pending_trigger_turn_count, 0);
        assert_eq!(live.last_enqueued_mailbox_seq, 3);
        assert_eq!(live.last_consumed_mailbox_seq, 3);
        Ok(())
    }

    #[test]
    fn attach_root_materializes_prompt_input_from_journal() -> Result<()> {
        let journal = Arc::new(MemoryJournal::new());
        let session = journal.create_thread(CreateThreadRequest {
            session_id: Some(SessionId::from_string("root")?),
            parent_session_id: None,
            cwd: PathBuf::from("/tmp"),
            artifact_root: None,
            agent_path: None,
            nickname: None,
            role: None,
        })?;
        let session_id = SessionId::from_string(session.id)?;
        let root_agent_id = AgentId::from_string(session_id.as_str())?;
        journal.append_runtime_event(
            &RuntimeEvent::new(RuntimeEventKind::AgentInputAccepted, Durability::Barrier)
                .with_session_id(session_id.clone())
                .with_agent_id(root_agent_id)
                .with_payload(json!({
                    "runtime_owned": true,
                    "source_event_seq": 11,
                })),
        )?;

        let persistence: Arc<dyn LiveThreadPersistence> = journal.clone();
        let state_index: Arc<dyn StateIndex> = journal.clone();
        let handle = BrowserUseRuntime::new(persistence, state_index).handle();
        let root = handle.attach_root_agent(AttachRootAgentRequest {
            session_id: session_id.clone(),
            cwd: PathBuf::from("/tmp"),
            task: "resume".to_string(),
            max_concurrent_threads_per_session: 3,
        })?;

        let live = root.live_state_snapshot();
        assert_eq!(live.accepted_input_count, 1);
        assert_eq!(live.pending_prompt_input_count, 1);
        assert_eq!(live.last_accepted_prompt_input_seq, 11);
        let consumed = handle.consume_prompt_input_for_session(&session_id)?;
        assert!(consumed.consumed);
        assert_eq!(
            root.live_state_snapshot().last_consumed_prompt_input_seq,
            11
        );
        Ok(())
    }

    #[test]
    fn attach_root_does_not_rematerialize_consumed_prompt_input() -> Result<()> {
        let journal = Arc::new(MemoryJournal::new());
        let session = journal.create_thread(CreateThreadRequest {
            session_id: Some(SessionId::from_string("root")?),
            parent_session_id: None,
            cwd: PathBuf::from("/tmp"),
            artifact_root: None,
            agent_path: None,
            nickname: None,
            role: None,
        })?;
        let session_id = SessionId::from_string(session.id)?;
        let root_agent_id = AgentId::from_string(session_id.as_str())?;
        journal.append_runtime_event(
            &RuntimeEvent::new(RuntimeEventKind::AgentInputAccepted, Durability::Barrier)
                .with_session_id(session_id.clone())
                .with_agent_id(root_agent_id.clone())
                .with_payload(json!({
                    "runtime_owned": true,
                    "source_event_seq": 11,
                })),
        )?;
        journal.append_runtime_event(
            &RuntimeEvent::new(RuntimeEventKind::AgentInputConsumed, Durability::Barrier)
                .with_session_id(session_id.clone())
                .with_agent_id(root_agent_id)
                .with_payload(json!({
                    "runtime_owned": true,
                    "source_event_seq": 11,
                })),
        )?;

        let persistence: Arc<dyn LiveThreadPersistence> = journal.clone();
        let state_index: Arc<dyn StateIndex> = journal.clone();
        let handle = BrowserUseRuntime::new(persistence, state_index).handle();
        let root = handle.attach_root_agent(AttachRootAgentRequest {
            session_id: session_id.clone(),
            cwd: PathBuf::from("/tmp"),
            task: "resume".to_string(),
            max_concurrent_threads_per_session: 3,
        })?;

        let live = root.live_state_snapshot();
        assert_eq!(live.accepted_input_count, 1);
        assert_eq!(live.pending_prompt_input_count, 0);
        assert_eq!(live.last_accepted_prompt_input_seq, 11);
        assert_eq!(live.last_consumed_prompt_input_seq, 11);
        let consumed = handle.consume_prompt_input_for_session(&session_id)?;
        assert!(!consumed.consumed);
        Ok(())
    }

    #[test]
    fn attach_root_materializes_durable_child_tree_and_status() -> Result<()> {
        let (runtime, journal) = BrowserUseRuntime::memory();
        let handle = runtime.handle();
        let root = handle.create_root_agent(CreateRootAgentRequest {
            cwd: PathBuf::from("/tmp"),
            task: "root task".to_string(),
            max_concurrent_threads_per_session: 3,
        })?;
        let child = handle.spawn_child(SpawnChildRequest {
            parent_agent_id: root.agent_id().clone(),
            child_agent_id: Some(AgentId::from_string("child")?),
            child_session_id: Some(SessionId::from_string("child")?),
            task_name: "research".to_string(),
            message: "inspect docs".to_string(),
            nickname: Some("Curie".to_string()),
            role: Some("explorer".to_string()),
        })?;
        handle.complete_agent(CompleteAgentRequest {
            child_agent_id: child.agent_id().clone(),
            result: "findings ready".to_string(),
        })?;

        let persistence: Arc<dyn LiveThreadPersistence> = journal.clone();
        let state_index: Arc<dyn StateIndex> = journal.clone();
        let resumed = BrowserUseRuntime::new(persistence, state_index).handle();
        let resumed_root = resumed.attach_root_agent(AttachRootAgentRequest {
            session_id: root.session_id().clone(),
            cwd: PathBuf::from("/tmp"),
            task: "resume".to_string(),
            max_concurrent_threads_per_session: 3,
        })?;

        let snapshot = resumed.snapshot();
        let resumed_child = snapshot
            .agents
            .iter()
            .find(|agent| agent.session_id.as_str() == "child")
            .context("resumed child")?;
        assert_eq!(
            resumed_child.parent_session_id,
            Some(root.session_id().clone())
        );
        assert_eq!(resumed_child.agent_path, "/root/research");
        assert_eq!(resumed_child.nickname.as_deref(), Some("Curie"));
        assert_eq!(resumed_child.role.as_deref(), Some("explorer"));
        assert_eq!(resumed_child.status, AgentThreadStatus::Completed);

        let pending = resumed.pending_agent_mail_for_session(root.session_id())?;
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].author_agent_id.as_str(), "child");
        assert_eq!(resumed_root.live_state_snapshot().pending_mailbox_count, 1);
        assert_eq!(snapshot.agent_controls[0].open_spawned_agents, 1);
        Ok(())
    }

    #[test]
    fn resumed_completed_child_holds_capacity_until_close() -> Result<()> {
        let (runtime, journal) = BrowserUseRuntime::memory();
        let handle = runtime.handle();
        let root = handle.create_root_agent(CreateRootAgentRequest {
            cwd: PathBuf::from("/tmp"),
            task: "root task".to_string(),
            max_concurrent_threads_per_session: 2,
        })?;
        let child = handle.spawn_child(SpawnChildRequest {
            parent_agent_id: root.agent_id().clone(),
            child_agent_id: Some(AgentId::from_string("child")?),
            child_session_id: Some(SessionId::from_string("child")?),
            task_name: "research".to_string(),
            message: "inspect docs".to_string(),
            nickname: None,
            role: None,
        })?;
        handle.complete_agent(CompleteAgentRequest {
            child_agent_id: child.agent_id().clone(),
            result: "done".to_string(),
        })?;

        let persistence: Arc<dyn LiveThreadPersistence> = journal.clone();
        let state_index: Arc<dyn StateIndex> = journal.clone();
        let resumed = BrowserUseRuntime::new(persistence, state_index).handle();
        let resumed_root = resumed.attach_root_agent(AttachRootAgentRequest {
            session_id: root.session_id().clone(),
            cwd: PathBuf::from("/tmp"),
            task: "resume".to_string(),
            max_concurrent_threads_per_session: 2,
        })?;

        let err = match resumed.spawn_child(SpawnChildRequest {
            parent_agent_id: resumed_root.agent_id().clone(),
            child_agent_id: None,
            child_session_id: None,
            task_name: "second".to_string(),
            message: "no capacity yet".to_string(),
            nickname: None,
            role: None,
        }) {
            Ok(_) => panic!("completed child should keep capacity until close"),
            Err(error) => error,
        };
        assert!(err.to_string().contains("agent limit reached"));

        resumed.close_agent(CloseAgentRequest {
            agent_id: AgentId::from_string("child")?,
            reason: "done".to_string(),
        })?;
        resumed.spawn_child(SpawnChildRequest {
            parent_agent_id: resumed_root.agent_id().clone(),
            child_agent_id: None,
            child_session_id: None,
            task_name: "second".to_string(),
            message: "capacity released".to_string(),
            nickname: None,
            role: None,
        })?;
        Ok(())
    }

    #[test]
    fn prompt_input_barrier_failure_does_not_mark_fresh_input() -> Result<()> {
        let (handle, journal) = runtime_with_failing_journal();
        let root = handle.create_root_agent(CreateRootAgentRequest {
            cwd: PathBuf::from("/tmp"),
            task: "root task".to_string(),
            max_concurrent_threads_per_session: 3,
        })?;
        let mut rx = handle.events().subscribe();
        journal.fail_event_type("agent.input.accepted");

        let err = handle
            .accept_prompt_input(AcceptPromptInputRequest {
                target_agent_id: root.agent_id().clone(),
                source_event_seq: Some(7),
                payload: json!({ "source": "test" }),
            })
            .expect_err("prompt input accept must fail before live mutation");

        assert!(err.to_string().contains("forced journal failure"));
        let live = root.live_state_snapshot();
        assert_eq!(live.accepted_input_count, 0);
        assert_eq!(live.pending_prompt_input_count, 0);
        assert_eq!(live.last_accepted_prompt_input_seq, 0);
        assert!(journal
            .events_for_session(root.session_id())?
            .iter()
            .all(|event| event.event_type != "agent.input.accepted"));
        assert!(matches!(
            rx.try_recv(),
            Err(tokio::sync::broadcast::error::TryRecvError::Empty)
        ));
        Ok(())
    }

    #[test]
    fn mailbox_barrier_failure_does_not_enqueue_or_publish() -> Result<()> {
        let (handle, journal) = runtime_with_failing_journal();
        let root = handle.create_root_agent(CreateRootAgentRequest {
            cwd: PathBuf::from("/tmp"),
            task: "root task".to_string(),
            max_concurrent_threads_per_session: 3,
        })?;
        let child = handle.spawn_child(SpawnChildRequest {
            parent_agent_id: root.agent_id().clone(),
            child_agent_id: None,
            child_session_id: None,
            task_name: "research".to_string(),
            message: "inspect docs".to_string(),
            nickname: None,
            role: None,
        })?;
        let mut rx = handle.events().subscribe();
        journal.fail_event_type("mailbox.enqueued");

        let err = handle
            .send_agent_message(SendAgentMessageRequest {
                author_agent_id: child.agent_id().clone(),
                target_agent_id: root.agent_id().clone(),
                content: "status update".to_string(),
                trigger_turn: false,
                kind: MailboxItemKind::Input,
                delivery_phase: MailboxDeliveryPhase::NextTurn,
                payload: json!({"source": "test"}),
            })
            .expect_err("mailbox enqueue must fail before wakeup");

        assert!(err.to_string().contains("forced journal failure"));
        assert!(root.mailbox().pending_items().is_empty());
        assert_eq!(root.live_state_snapshot().pending_mailbox_count, 0);
        assert!(journal
            .events_for_session(root.session_id())?
            .iter()
            .all(|event| event.event_type != "mailbox.enqueued"));
        assert!(matches!(
            rx.try_recv(),
            Err(tokio::sync::broadcast::error::TryRecvError::Empty)
        ));
        Ok(())
    }

    #[test]
    fn child_completion_barrier_failure_does_not_finish_or_wake_parent() -> Result<()> {
        let (handle, journal) = runtime_with_failing_journal();
        let root = handle.create_root_agent(CreateRootAgentRequest {
            cwd: PathBuf::from("/tmp"),
            task: "root task".to_string(),
            max_concurrent_threads_per_session: 3,
        })?;
        let child = handle.spawn_child(SpawnChildRequest {
            parent_agent_id: root.agent_id().clone(),
            child_agent_id: None,
            child_session_id: None,
            task_name: "research".to_string(),
            message: "inspect docs".to_string(),
            nickname: None,
            role: None,
        })?;
        let mut rx = handle.events().subscribe();
        journal.fail_event_type("mailbox.enqueued");

        let err = handle
            .complete_agent(CompleteAgentRequest {
                child_agent_id: child.agent_id().clone(),
                result: "findings ready".to_string(),
            })
            .expect_err("child completion must fail before live finish");

        assert!(err.to_string().contains("forced journal failure"));
        assert_eq!(child.snapshot().status, AgentThreadStatus::Created);
        assert_eq!(
            journal.list_children(root.session_id())?[0].status,
            SpawnEdgeStatus::Open
        );
        assert!(root.mailbox().pending_items().is_empty());
        assert!(journal
            .events_for_session(root.session_id())?
            .iter()
            .all(|event| event.event_type != "mailbox.enqueued"
                && event.event_type != "agent.completed"));
        assert!(matches!(
            rx.try_recv(),
            Err(tokio::sync::broadcast::error::TryRecvError::Empty)
        ));
        Ok(())
    }

    #[test]
    fn local_runtime_socket_routes_mailbox_requests() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let (runtime, journal) = BrowserUseRuntime::memory();
        let handle = runtime.handle();
        let root = handle.create_root_agent(CreateRootAgentRequest {
            cwd: PathBuf::from("/tmp"),
            task: "root task".to_string(),
            max_concurrent_threads_per_session: 3,
        })?;
        let child = handle.spawn_child(SpawnChildRequest {
            parent_agent_id: root.agent_id().clone(),
            child_agent_id: None,
            child_session_id: None,
            task_name: "research".to_string(),
            message: "inspect docs".to_string(),
            nickname: None,
            role: None,
        })?;

        let socket_path = spawn_local_runtime_server(dir.path(), handle.clone())?;
        let response = send_local_runtime_request(
            dir.path(),
            &LocalRuntimeRequest::SendAgentMessage {
                author_agent_id: child.agent_id().as_str().to_string(),
                target_agent_id: root.agent_id().as_str().to_string(),
                content: "status update".to_string(),
                trigger_turn: false,
                kind: MailboxItemKind::Input,
                delivery_phase: MailboxDeliveryPhase::NextTurn,
                payload: json!({"source": "test"}),
            },
            Duration::from_secs(1),
        )?
        .context("local runtime send response")?;
        assert!(response.ok, "{:?}", response.error);
        assert_eq!(
            response.result["mailbox_item"]["content"].as_str(),
            Some("status update")
        );

        let pending = send_local_runtime_request(
            dir.path(),
            &LocalRuntimeRequest::PendingAgentMail {
                session_id: root.session_id().as_str().to_string(),
            },
            Duration::from_secs(1),
        )?
        .context("local runtime pending response")?;
        assert!(pending.ok, "{:?}", pending.error);
        assert_eq!(pending.result["count"].as_u64(), Some(1));

        let waited = send_local_runtime_request(
            dir.path(),
            &LocalRuntimeRequest::WaitAgent {
                parent_agent_id: root.agent_id().as_str().to_string(),
                target: Some(LocalRuntimeWaitTarget::Any),
                timeout_ms: 50,
            },
            Duration::from_secs(1),
        )?
        .context("local runtime wait response")?;
        assert!(waited.ok, "{:?}", waited.error);
        assert_eq!(waited.result["timed_out"].as_bool(), Some(false));
        assert_eq!(
            waited.result["mailbox_item"]["content"].as_str(),
            Some("status update")
        );
        assert_eq!(
            root.mailbox().pending_items().len(),
            1,
            "wait_agent must observe but not drain mailbox items"
        );
        let submitted = send_local_runtime_request(
            dir.path(),
            &LocalRuntimeRequest::SubmitUserInput {
                session_id: root.session_id().as_str().to_string(),
                content: "continue with the follow-up".to_string(),
                trigger_turn: true,
                delivery_phase: MailboxDeliveryPhase::CurrentTurn,
                input_items: None,
                payload: json!({"pending_from_seq": 123}),
            },
            Duration::from_secs(1),
        )?
        .context("local runtime submit user input response")?;
        assert!(submitted.ok, "{:?}", submitted.error);
        assert_eq!(
            submitted.result["mailbox_item"]["kind"].as_str(),
            Some("followup")
        );
        assert_eq!(
            root.mailbox().pending_items().len(),
            2,
            "submit user input should enqueue into the same live mailbox"
        );
        let closed = send_local_runtime_request(
            dir.path(),
            &LocalRuntimeRequest::CloseAgent {
                agent_id: child.agent_id().as_str().to_string(),
                reason: "done with child".to_string(),
            },
            Duration::from_secs(1),
        )?
        .context("local runtime close response")?;
        assert!(closed.ok, "{:?}", closed.error);
        assert_eq!(closed.result["closed"].as_bool(), Some(true));
        let child_events = journal.events_for_session(child.session_id())?;
        assert!(child_events
            .iter()
            .any(|event| event.event_type == "agent.closed"));
        let parent_events = journal.events_for_session(root.session_id())?;
        let cancelled = parent_events
            .iter()
            .find(|event| event.event_type == "agent.cancelled")
            .context("agent.cancelled")?;
        assert_eq!(
            cancelled.payload["payload"]["reason"].as_str(),
            Some("done with child")
        );
        let _ = std::fs::remove_file(socket_path);
        Ok(())
    }

    #[test]
    fn runtime_strict_spawn_rejection_is_journaled() -> Result<()> {
        let (runtime, journal) = BrowserUseRuntime::memory();
        let handle = runtime.handle();
        let root = handle.create_root_agent(CreateRootAgentRequest {
            cwd: PathBuf::from("/tmp"),
            task: "root task".to_string(),
            max_concurrent_threads_per_session: 1,
        })?;

        let err = match handle.spawn_child(SpawnChildRequest {
            parent_agent_id: root.agent_id().clone(),
            child_agent_id: None,
            child_session_id: None,
            task_name: "blocked".to_string(),
            message: "no capacity".to_string(),
            nickname: None,
            role: None,
        }) {
            Ok(_) => panic!("cap 1 means root consumes the only slot"),
            Err(err) => err,
        };
        assert!(err.to_string().contains("agent limit reached"));

        let root_events = journal.events_for_session(root.session_id())?;
        assert!(root_events
            .iter()
            .any(|event| event.event_type == "subagent.spawn_rejected"));
        Ok(())
    }

    #[tokio::test]
    async fn runtime_wait_timeout_is_journaled_without_hiding_children() -> Result<()> {
        let (runtime, journal) = BrowserUseRuntime::memory();
        let handle = runtime.handle();
        let root = handle.create_root_agent(CreateRootAgentRequest {
            cwd: PathBuf::from("/tmp"),
            task: "root task".to_string(),
            max_concurrent_threads_per_session: 2,
        })?;
        let child = handle.spawn_child(SpawnChildRequest {
            parent_agent_id: root.agent_id().clone(),
            child_agent_id: None,
            child_session_id: None,
            task_name: "slow".to_string(),
            message: "take your time".to_string(),
            nickname: None,
            role: None,
        })?;

        let outcome = handle
            .wait_agent(
                root.agent_id(),
                AgentTarget::AgentId(child.agent_id().clone()),
                Duration::from_millis(1),
            )
            .await?;
        assert_eq!(outcome, WaitAgentOutcome::TimedOut);
        assert_eq!(
            handle.agents().thread(child.agent_id())?.snapshot().status,
            AgentThreadStatus::Created,
            "wait timeout must not close or hide the child"
        );
        let root_events = journal.events_for_session(root.session_id())?;
        assert!(root_events
            .iter()
            .any(|event| event.event_type == "wait_agent.timed_out"));
        Ok(())
    }

    #[test]
    fn runtime_completed_child_holds_capacity_until_close() -> Result<()> {
        let (runtime, journal) = BrowserUseRuntime::memory();
        let handle = runtime.handle();
        let root = handle.create_root_agent(CreateRootAgentRequest {
            cwd: PathBuf::from("/tmp"),
            task: "root task".to_string(),
            max_concurrent_threads_per_session: 2,
        })?;
        let first = handle.spawn_child(SpawnChildRequest {
            parent_agent_id: root.agent_id().clone(),
            child_agent_id: None,
            child_session_id: None,
            task_name: "first".to_string(),
            message: "one".to_string(),
            nickname: None,
            role: None,
        })?;
        handle.complete_agent(CompleteAgentRequest {
            child_agent_id: first.agent_id().clone(),
            result: "done".to_string(),
        })?;

        let err = match handle.spawn_child(SpawnChildRequest {
            parent_agent_id: root.agent_id().clone(),
            child_agent_id: None,
            child_session_id: None,
            task_name: "second".to_string(),
            message: "two".to_string(),
            nickname: None,
            role: None,
        }) {
            Ok(_) => panic!("completed-but-open child must still hold capacity"),
            Err(err) => err,
        };
        assert!(err.to_string().contains("agent limit reached"));

        handle.close_agent(CloseAgentRequest {
            agent_id: first.agent_id().clone(),
            reason: "done inspecting".to_string(),
        })?;
        let second = handle.spawn_child(SpawnChildRequest {
            parent_agent_id: root.agent_id().clone(),
            child_agent_id: None,
            child_session_id: None,
            task_name: "second".to_string(),
            message: "two".to_string(),
            nickname: None,
            role: None,
        })?;
        assert_eq!(second.snapshot().agent_path, "/root/second");

        let edges = journal.list_children(root.session_id())?;
        assert!(edges.iter().any(|edge| {
            edge.child_session_id == first.session_id().clone()
                && edge.status == SpawnEdgeStatus::Closed
        }));
        let first_events = journal.events_for_session(first.session_id())?;
        assert!(first_events
            .iter()
            .any(|event| event.event_type == "agent.closed"));
        Ok(())
    }

    #[tokio::test]
    async fn projected_subscription_returns_snapshot_before_live_events() -> Result<()> {
        let (runtime, _journal) = BrowserUseRuntime::memory();
        let handle = runtime.handle();
        let root = handle.create_root_agent(CreateRootAgentRequest {
            cwd: PathBuf::from("/tmp"),
            task: "root task".to_string(),
            max_concurrent_threads_per_session: 3,
        })?;
        let mut subscription = handle.subscribe_projected();
        assert_eq!(subscription.snapshot().agents.len(), 1);
        assert_eq!(
            subscription.snapshot().agents[0].agent_id,
            root.agent_id().clone()
        );

        let child = handle.spawn_child(SpawnChildRequest {
            parent_agent_id: root.agent_id().clone(),
            child_agent_id: None,
            child_session_id: None,
            task_name: "research".to_string(),
            message: "inspect".to_string(),
            nickname: None,
            role: None,
        })?;
        let event = subscription.recv().await?;
        assert_eq!(event.session_id, Some(root.session_id().clone()));
        assert_eq!(event.kind, ProjectedEventKind::ToolUpdated);
        assert_eq!(event.payload["child_agent_id"], child.agent_id().as_str());
        let projected_snapshot = event.snapshot.as_ref().expect("projected snapshot");
        assert_eq!(projected_snapshot.agents.len(), 2);
        let projected_child = projected_snapshot
            .agents
            .iter()
            .find(|agent| agent.agent_id == child.agent_id().clone())
            .expect("projected child");
        assert_eq!(projected_child.session_id, child.session_id().clone());
        assert_eq!(
            projected_child.parent_agent_id,
            Some(root.agent_id().clone())
        );
        assert_eq!(
            projected_child.parent_session_id,
            Some(root.session_id().clone())
        );
        assert_eq!(projected_child.status, AgentThreadStatus::Created);
        assert_eq!(subscription.snapshot().agents.len(), 2);

        let fresh_snapshot = handle.snapshot();
        assert_eq!(fresh_snapshot.agents.len(), 2);
        Ok(())
    }

    #[tokio::test]
    async fn projected_subscription_maps_terminal_agent_events() -> Result<()> {
        let (runtime, _journal) = BrowserUseRuntime::memory();
        let handle = runtime.handle();
        let root = handle.create_root_agent(CreateRootAgentRequest {
            cwd: PathBuf::from("/tmp"),
            task: "root task".to_string(),
            max_concurrent_threads_per_session: 3,
        })?;
        let child = handle.spawn_child(SpawnChildRequest {
            parent_agent_id: root.agent_id().clone(),
            child_agent_id: None,
            child_session_id: None,
            task_name: "research".to_string(),
            message: "inspect".to_string(),
            nickname: None,
            role: None,
        })?;
        let mut subscription = handle.subscribe_projected();
        handle.complete_agent(CompleteAgentRequest {
            child_agent_id: child.agent_id().clone(),
            result: "done".to_string(),
        })?;

        let completed = subscription.recv().await?;
        assert_eq!(completed.kind, ProjectedEventKind::ThreadStatusChanged);
        assert_eq!(completed.session_id, Some(child.session_id().clone()));
        assert_eq!(completed.payload["result"], "done");
        let completed_snapshot = completed.snapshot.as_ref().expect("completed snapshot");
        let completed_child = completed_snapshot
            .agents
            .iter()
            .find(|agent| agent.agent_id == child.agent_id().clone())
            .expect("completed child");
        assert_eq!(completed_child.status, AgentThreadStatus::Completed);

        let parent_terminal = subscription.recv().await?;
        assert_eq!(
            parent_terminal.kind,
            ProjectedEventKind::ThreadStatusChanged
        );
        assert_eq!(parent_terminal.session_id, Some(root.session_id().clone()));
        assert_eq!(parent_terminal.payload["runtime_owned"], true);
        assert_eq!(
            parent_terminal.payload["child_session_id"],
            child.session_id().as_str()
        );
        let parent_terminal_snapshot = parent_terminal
            .snapshot
            .as_ref()
            .expect("parent terminal snapshot");
        let parent_after_child_completion = parent_terminal_snapshot
            .agents
            .iter()
            .find(|agent| agent.agent_id == root.agent_id().clone())
            .expect("parent snapshot");
        assert_eq!(
            parent_after_child_completion.status,
            AgentThreadStatus::Created
        );

        let mailbox = subscription.recv().await?;
        assert_eq!(mailbox.kind, ProjectedEventKind::ToolUpdated);
        assert_eq!(mailbox.session_id, Some(root.session_id().clone()));
        assert_eq!(mailbox.payload["trigger_turn"], false);
        let mailbox_snapshot = mailbox.snapshot.as_ref().expect("mailbox snapshot");
        let parent_after_mail = mailbox_snapshot
            .agents
            .iter()
            .find(|agent| agent.agent_id == root.agent_id().clone())
            .expect("parent mailbox snapshot");
        assert_eq!(parent_after_mail.live.pending_mailbox_count, 1);
        assert_eq!(parent_after_mail.live.pending_trigger_turn_count, 0);
        Ok(())
    }

    #[tokio::test]
    async fn projected_subscription_reduces_browser_state() -> Result<()> {
        let (runtime, _journal) = BrowserUseRuntime::memory();
        let handle = runtime.handle();
        let root = handle.create_root_agent(CreateRootAgentRequest {
            cwd: PathBuf::from("/tmp"),
            task: "root task".to_string(),
            max_concurrent_threads_per_session: 3,
        })?;
        let mut subscription = handle.subscribe_projected();

        let browser_id = handle.create_browser(BrowserConfig {
            keep_alive: true,
            headless: Some(true),
            profile_id: Some("test-profile".to_string()),
            ..BrowserConfig::default()
        });
        let created = subscription.recv().await?;
        let created_snapshot = created.snapshot.as_ref().expect("created snapshot");
        let browser = created_snapshot
            .browsers
            .iter()
            .find(|browser| browser.id == browser_id)
            .expect("created browser");
        assert_eq!(browser.status, BrowserStatus::Created);
        assert_eq!(browser.config.profile_id.as_deref(), Some("test-profile"));

        let lease = handle.claim_browser(&browser_id, root.agent_id().clone())?;
        let claimed = subscription.recv().await?;
        let claimed_snapshot = claimed.snapshot.as_ref().expect("claimed snapshot");
        let browser = claimed_snapshot
            .browsers
            .iter()
            .find(|browser| browser.id == browser_id)
            .expect("claimed browser");
        assert_eq!(browser.status, BrowserStatus::Claimed);
        assert_eq!(browser.active_agent_id, Some(root.agent_id().clone()));

        handle.release_browser(&lease)?;
        let released = subscription.recv().await?;
        let released_snapshot = released.snapshot.as_ref().expect("released snapshot");
        let browser = released_snapshot
            .browsers
            .iter()
            .find(|browser| browser.id == browser_id)
            .expect("released browser");
        assert_eq!(browser.status, BrowserStatus::Released);
        assert_eq!(browser.active_agent_id, None);

        handle.close_browser(&browser_id)?;
        let closed = subscription.recv().await?;
        let closed_snapshot = closed.snapshot.as_ref().expect("closed snapshot");
        let browser = closed_snapshot
            .browsers
            .iter()
            .find(|browser| browser.id == browser_id)
            .expect("closed browser");
        assert_eq!(browser.status, BrowserStatus::Closed);
        assert_eq!(browser.active_agent_id, None);
        Ok(())
    }

    #[tokio::test]
    async fn browser_script_lifecycle_is_browser_scoped_and_projected() -> Result<()> {
        let (runtime, journal) = BrowserUseRuntime::memory();
        let handle = runtime.handle();
        let root = handle.create_root_agent(CreateRootAgentRequest {
            cwd: PathBuf::from("/tmp"),
            task: "root task".to_string(),
            max_concurrent_threads_per_session: 3,
        })?;
        let browser_id = handle.create_browser(BrowserConfig::default());
        let mut subscription = handle.subscribe_projected();

        handle.append_observed_browser_session_event(
            root.session_id().clone(),
            browser_id.clone(),
            "browser_script.started",
            json!({
                "run_id": "script-1",
                "tool_call_id": "call-1",
            }),
            Durability::Barrier,
        )?;
        let started = subscription.recv().await?;
        let browser = projected_browser(&started, &browser_id);
        assert_eq!(browser.active_scripts.len(), 1);
        assert_eq!(browser.active_scripts[0].run_id, "script-1");
        assert_eq!(
            browser.active_scripts[0].agent_id,
            Some(root.agent_id().clone())
        );
        let root_live = projected_agent_live(&started, root.agent_id());
        assert_eq!(root_live.active_items.len(), 1);
        assert_eq!(root_live.active_items[0].kind, "browser_script");

        handle.append_observed_browser_session_event(
            root.session_id().clone(),
            browser_id.clone(),
            "browser_script.output_delta",
            json!({
                "run_id": "script-1",
                "tool_call_id": "call-1",
                "text": "partial output",
            }),
            Durability::BestEffort,
        )?;
        let delta = subscription.recv().await?;
        let browser = projected_browser(&delta, &browser_id);
        assert_eq!(
            browser.active_scripts[0].last_delta.as_deref(),
            Some("partial output")
        );
        assert_eq!(
            handle.browsers().snapshot(&browser_id)?.active_scripts[0]
                .last_delta
                .as_deref(),
            Some("partial output")
        );

        handle.append_observed_browser_session_event(
            root.session_id().clone(),
            browser_id.clone(),
            "browser_script.completed",
            json!({
                "run_id": "script-1",
                "tool_call_id": "call-1",
            }),
            Durability::Barrier,
        )?;
        let completed = subscription.recv().await?;
        let browser = projected_browser(&completed, &browser_id);
        assert!(browser.active_scripts.is_empty());
        let root_live = projected_agent_live(&completed, root.agent_id());
        assert!(root_live.active_items.is_empty());
        assert!(handle
            .browsers()
            .snapshot(&browser_id)?
            .active_scripts
            .is_empty());

        let event_types = journal
            .events_for_session(root.session_id())?
            .into_iter()
            .map(|event| event.event_type)
            .collect::<Vec<_>>();
        assert_eq!(
            event_types
                .iter()
                .filter(|event_type| event_type.starts_with("browser_script."))
                .cloned()
                .collect::<Vec<_>>(),
            vec![
                "browser_script.started".to_string(),
                "browser_script.output_delta".to_string(),
                "browser_script.completed".to_string(),
            ]
        );
        Ok(())
    }

    #[test]
    fn browser_script_barrier_failure_does_not_publish_or_track_script() -> Result<()> {
        let (handle, journal) = runtime_with_failing_journal();
        let root = handle.create_root_agent(CreateRootAgentRequest {
            cwd: PathBuf::from("/tmp"),
            task: "root task".to_string(),
            max_concurrent_threads_per_session: 3,
        })?;
        let browser_id = handle.create_browser(BrowserConfig::default());
        let mut rx = handle.events().subscribe();
        journal.fail_event_type("browser_script.started");

        let err = handle
            .append_observed_browser_session_event(
                root.session_id().clone(),
                browser_id.clone(),
                "browser_script.started",
                json!({
                    "run_id": "script-1",
                    "tool_call_id": "call-1",
                }),
                Durability::Barrier,
            )
            .expect_err("browser script start must fail before becoming live");

        assert!(err.to_string().contains("forced journal failure"));
        assert!(handle
            .browsers()
            .snapshot(&browser_id)?
            .active_scripts
            .is_empty());
        assert!(rx.try_recv().is_err());
        Ok(())
    }

    #[tokio::test]
    async fn projected_subscription_reduces_observed_tool_and_terminal_state() -> Result<()> {
        let (runtime, _journal) = BrowserUseRuntime::memory();
        let handle = runtime.handle();
        let root = handle.create_root_agent(CreateRootAgentRequest {
            cwd: PathBuf::from("/tmp"),
            task: "root task".to_string(),
            max_concurrent_threads_per_session: 3,
        })?;
        let mut subscription = handle.subscribe_projected();

        handle.append_observed_session_event(
            root.session_id().clone(),
            "tool.started",
            json!({
                "name": "exec_command",
                "tool_call_id": "call_1",
                "arguments": { "cmd": "echo hi" },
            }),
            Durability::Barrier,
        )?;
        let started = subscription.recv().await?;
        let root_live = projected_agent_live(&started, root.agent_id());
        assert_eq!(root_live.active_items.len(), 1);
        assert_eq!(root_live.active_items[0].kind, "tool");
        assert_eq!(
            root_live.active_items[0].name.as_deref(),
            Some("exec_command")
        );

        handle.append_observed_session_event(
            root.session_id().clone(),
            "tool.output_delta",
            json!({
                "tool_call_id": "call_1",
                "text": "partial",
            }),
            Durability::BestEffort,
        )?;
        let delta = subscription.recv().await?;
        let root_live = projected_agent_live(&delta, root.agent_id());
        assert_eq!(
            root_live.active_items[0].last_delta.as_deref(),
            Some("partial")
        );

        handle.append_observed_session_event(
            root.session_id().clone(),
            "tool.output",
            json!({
                "name": "exec_command",
                "tool_call_id": "call_1",
                "text": "done",
            }),
            Durability::Barrier,
        )?;
        let output = subscription.recv().await?;
        let root_live = projected_agent_live(&output, root.agent_id());
        assert!(root_live.active_items.is_empty());

        handle.append_observed_session_event(
            root.session_id().clone(),
            "model.stream_delta",
            json!({ "text": "answer" }),
            Durability::BestEffort,
        )?;
        let model_delta = subscription.recv().await?;
        let root_live = projected_agent_live(&model_delta, root.agent_id());
        assert_eq!(root_live.last_model_delta.as_deref(), Some("answer"));

        handle.append_observed_session_event(
            root.session_id().clone(),
            "token_count",
            json!({
                "info": {
                    "last_token_usage": { "total_tokens": 7 },
                    "total_token_usage": { "total_tokens": 11 },
                    "model_context_window": 100,
                }
            }),
            Durability::Barrier,
        )?;
        let token_count = subscription.recv().await?;
        let root_live = projected_agent_live(&token_count, root.agent_id());
        assert_eq!(
            root_live.last_token_usage.as_ref().unwrap()["total_tokens"],
            7
        );
        assert_eq!(
            root_live.total_token_usage.as_ref().unwrap()["total_tokens"],
            11
        );
        assert_eq!(root_live.model_context_window, Some(100));

        handle.append_observed_session_event(
            root.session_id().clone(),
            "session.done",
            json!({ "result": "final answer" }),
            Durability::Barrier,
        )?;
        let done = subscription.recv().await?;
        let root_live = projected_agent_live(&done, root.agent_id());
        assert_eq!(root_live.final_result.as_deref(), Some("final answer"));
        assert_eq!(root_live.failure, None);
        Ok(())
    }

    #[tokio::test]
    async fn projected_subscription_reduces_observed_model_lifecycle_state() -> Result<()> {
        let (runtime, _journal) = BrowserUseRuntime::memory();
        let handle = runtime.handle();
        let root = handle.create_root_agent(CreateRootAgentRequest {
            cwd: PathBuf::from("/tmp"),
            task: "root task".to_string(),
            max_concurrent_threads_per_session: 3,
        })?;
        let mut subscription = handle.subscribe_projected();

        handle.append_observed_session_event(
            root.session_id().clone(),
            "model.turn.request",
            json!({
                "model": "gpt-5.5",
                "provider": "openai",
                "turn_idx": 4,
                "attempt": 0,
            }),
            Durability::Barrier,
        )?;
        let request = subscription.recv().await?;
        let root_live = projected_agent_live(&request, root.agent_id());
        let model = root_live
            .active_model_request
            .as_ref()
            .expect("active model request");
        assert_eq!(model.status, "requesting");
        assert_eq!(model.model.as_deref(), Some("gpt-5.5"));
        assert_eq!(model.provider.as_deref(), Some("openai"));
        assert_eq!(model.turn_idx, Some(4));
        assert_eq!(model.attempt, Some(0));
        assert_eq!(model.retry_count, 0);

        handle.append_observed_session_event(
            root.session_id().clone(),
            "model.stream_delta",
            json!({ "text": "partial before retry" }),
            Durability::BestEffort,
        )?;
        let delta = subscription.recv().await?;
        let root_live = projected_agent_live(&delta, root.agent_id());
        assert_eq!(
            root_live.last_model_delta.as_deref(),
            Some("partial before retry")
        );

        handle.append_observed_session_event(
            root.session_id().clone(),
            "model.turn.retry",
            json!({
                "attempt": 1,
                "message": "transport retry",
            }),
            Durability::Barrier,
        )?;
        let retry = subscription.recv().await?;
        let root_live = projected_agent_live(&retry, root.agent_id());
        let model = root_live
            .active_model_request
            .as_ref()
            .expect("retrying model request");
        assert_eq!(model.status, "retrying");
        assert_eq!(model.retry_count, 1);
        assert_eq!(model.attempt, Some(1));
        assert_eq!(model.last_error.as_deref(), Some("transport retry"));
        assert_eq!(root_live.last_model_delta, None);

        handle.append_observed_session_event(
            root.session_id().clone(),
            "stream_error",
            json!({ "message": "temporary stream failure" }),
            Durability::BestEffort,
        )?;
        let stream_error = subscription.recv().await?;
        let root_live = projected_agent_live(&stream_error, root.agent_id());
        let model = root_live
            .active_model_request
            .as_ref()
            .expect("stream error model request");
        assert_eq!(model.status, "error");
        assert_eq!(
            model.last_error.as_deref(),
            Some("temporary stream failure")
        );
        assert_eq!(
            root_live.failure.as_deref(),
            Some("temporary stream failure")
        );

        handle.append_observed_session_event(
            root.session_id().clone(),
            "model.turn.response",
            json!({ "turn_idx": 4 }),
            Durability::Barrier,
        )?;
        let response = subscription.recv().await?;
        let root_live = projected_agent_live(&response, root.agent_id());
        assert_eq!(root_live.active_model_request, None);
        Ok(())
    }

    fn projected_agent_live<'a>(
        event: &'a ProjectedEvent,
        agent_id: &AgentId,
    ) -> &'a AgentLiveStateSnapshot {
        &event
            .snapshot
            .as_ref()
            .expect("projected snapshot")
            .agents
            .iter()
            .find(|agent| &agent.agent_id == agent_id)
            .expect("projected agent")
            .live
    }

    fn projected_browser<'a>(
        event: &'a ProjectedEvent,
        browser_id: &BrowserId,
    ) -> &'a BrowserSnapshot {
        event
            .snapshot
            .as_ref()
            .expect("projected snapshot")
            .browsers
            .iter()
            .find(|browser| &browser.id == browser_id)
            .expect("projected browser")
    }
}
