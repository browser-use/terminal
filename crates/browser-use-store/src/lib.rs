use std::fs::File;
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::{mpsc, Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use browser_use_protocol::{ArtifactMeta, EventRecord, SessionMeta, SessionStatus};
use chrono::Utc;
use rusqlite::{params, Connection, OptionalExtension, Transaction, TransactionBehavior};
use serde_json::Value;
use uuid::Uuid;

const MIGRATIONS: &[(i64, &str)] = &[
    (1, include_str!("../migrations/0001_initial.sql")),
    (
        2,
        include_str!("../migrations/0002_agent_session_fields.sql"),
    ),
    (3, include_str!("../migrations/0003_agent_messages.sql")),
    (4, include_str!("../migrations/0004_app_settings.sql")),
    (5, include_str!("../migrations/0005_unique_agent_paths.sql")),
    (
        6,
        include_str!("../migrations/0006_agent_message_input_items.sql"),
    ),
    (
        7,
        include_str!("../migrations/0007_events_session_type_seq.sql"),
    ),
    (
        8,
        include_str!("../migrations/0008_agent_message_consumed.sql"),
    ),
];

pub struct Store {
    state_dir: PathBuf,
    conn: Connection,
    notifier: Option<StoreNotifier>,
    notification_state: Arc<StoreNotificationState>,
}

pub fn default_state_dir() -> PathBuf {
    home_dir()
        .map(|home| home.join(".browser-use-terminal"))
        .unwrap_or_else(|| PathBuf::from(".browser-use-terminal"))
}

pub fn resolve_state_dir(path: impl AsRef<Path>) -> PathBuf {
    let path = path.as_ref();
    if path == Path::new("~/.browser-use-terminal") {
        return default_state_dir();
    }
    if let Ok(rest) = path.strip_prefix("~") {
        if let Some(home) = home_dir() {
            return home.join(rest);
        }
    }
    path.to_path_buf()
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("USERPROFILE")
                .filter(|value| !value.is_empty())
                .map(PathBuf::from)
        })
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StoreNotification {
    SessionsChanged,
    SessionChanged { session_id: String },
    EventsChanged { session_id: String, seq: i64 },
    SettingsChanged,
}

pub type StoreNotifier = mpsc::Sender<StoreNotification>;

#[derive(Default)]
struct StoreNotificationState {
    version: Mutex<u64>,
    changed: Condvar,
}

impl StoreNotificationState {
    fn notify(&self) {
        let mut version = self
            .version
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *version = version.saturating_add(1);
        self.changed.notify_all();
    }
}

#[derive(Clone)]
pub struct StoreNotificationWatcher {
    state: Arc<StoreNotificationState>,
}

impl StoreNotificationWatcher {
    pub fn cursor(&self) -> u64 {
        *self
            .state
            .version
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub fn wait_for_change(&self, cursor: u64, timeout: Duration) -> bool {
        let version = self
            .state
            .version
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if *version != cursor {
            return true;
        }
        let (version, _) = self
            .state
            .changed
            .wait_timeout_while(version, timeout, |version| *version == cursor)
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *version != cursor
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentSummary {
    pub parent_session_id: String,
    pub child_session_id: String,
    pub status: String,
    pub agent_path: Option<String>,
    pub agent_nickname: Option<String>,
    pub agent_role: Option<String>,
    pub updated_ms: i64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentMessage {
    pub id: String,
    pub author_session_id: String,
    pub target_session_id: String,
    pub content: String,
    pub input_items: Option<Value>,
    pub input_kind: String,
    pub trigger_turn: bool,
    pub created_ms: i64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunSummary {
    pub id: String,
    pub session_id: String,
    pub pid: Option<i64>,
    pub status: String,
    pub started_ms: i64,
    pub ended_ms: Option<i64>,
}

impl Store {
    pub fn open(state_dir: impl AsRef<Path>) -> Result<Self> {
        Self::open_with_optional_notifier(state_dir, None)
    }

    /// Open an ephemeral store over an in-memory SQLite connection.
    ///
    /// This keeps the legacy `Store` API available to compatibility callers
    /// without creating `state.db`. Files/artifacts still use `state_dir` so
    /// tool outputs have a real filesystem home.
    pub fn open_in_memory(state_dir: impl AsRef<Path>) -> Result<Self> {
        Self::open_in_memory_with_optional_notifier(state_dir, None)
    }

    pub fn open_in_memory_with_optional_notifier(
        state_dir: impl AsRef<Path>,
        notifier: Option<StoreNotifier>,
    ) -> Result<Self> {
        let state_dir = resolve_state_dir(state_dir);
        std::fs::create_dir_all(&state_dir)
            .with_context(|| format!("create state dir {}", state_dir.display()))?;
        std::fs::create_dir_all(state_dir.join("artifacts")).with_context(|| {
            format!(
                "create artifact dir {}",
                state_dir.join("artifacts").display()
            )
        })?;
        let conn = Connection::open_in_memory().context("open in-memory state database")?;
        conn.busy_timeout(Duration::from_secs(30))?;
        conn.execute_batch("PRAGMA busy_timeout = 30000;")?;
        let store = Self {
            state_dir,
            conn,
            notifier,
            notification_state: Arc::new(StoreNotificationState::default()),
        };
        store.migrate()?;
        Ok(store)
    }

    pub fn open_with_notifier(
        state_dir: impl AsRef<Path>,
        notifier: StoreNotifier,
    ) -> Result<Self> {
        Self::open_with_optional_notifier(state_dir, Some(notifier))
    }

    pub fn open_with_optional_notifier(
        state_dir: impl AsRef<Path>,
        notifier: Option<StoreNotifier>,
    ) -> Result<Self> {
        let state_dir = resolve_state_dir(state_dir);
        std::fs::create_dir_all(&state_dir)
            .with_context(|| format!("create state dir {}", state_dir.display()))?;
        std::fs::create_dir_all(state_dir.join("artifacts")).with_context(|| {
            format!(
                "create artifact dir {}",
                state_dir.join("artifacts").display()
            )
        })?;
        let conn = Connection::open(state_dir.join("state.db"))
            .with_context(|| format!("open {}", state_dir.join("state.db").display()))?;
        conn.busy_timeout(Duration::from_secs(30))?;
        conn.execute_batch(
            r#"
            PRAGMA journal_mode = WAL;
            PRAGMA busy_timeout = 30000;
            "#,
        )?;
        let store = Self {
            state_dir,
            conn,
            notifier,
            notification_state: Arc::new(StoreNotificationState::default()),
        };
        store.migrate()?;
        Ok(store)
    }

    pub fn state_dir(&self) -> &Path {
        &self.state_dir
    }

    pub fn notifier(&self) -> Option<StoreNotifier> {
        self.notifier.clone()
    }

    pub fn notification_watcher(&self) -> StoreNotificationWatcher {
        StoreNotificationWatcher {
            state: Arc::clone(&self.notification_state),
        }
    }

    fn notify(&self, notification: StoreNotification) {
        if let Some(notifier) = self.notifier.as_ref() {
            let _ = notifier.send(notification.clone());
        }
        self.notification_state.notify();
    }

    fn migrate(&self) -> Result<()> {
        self.conn.execute_batch(
            r#"
            PRAGMA foreign_keys = ON;
            CREATE TABLE IF NOT EXISTS schema_migrations (
                version INTEGER PRIMARY KEY,
                applied_ms INTEGER NOT NULL
            );
            "#,
        )?;
        for (version, sql) in MIGRATIONS {
            let applied = self
                .conn
                .query_row(
                    "SELECT 1 FROM schema_migrations WHERE version = ?1",
                    params![version],
                    |_| Ok(()),
                )
                .optional()?
                .is_some();
            if applied {
                continue;
            }
            let tx = self.conn.unchecked_transaction()?;
            tx.execute_batch(sql)?;
            tx.execute(
                "INSERT INTO schema_migrations(version, applied_ms) VALUES (?1, ?2)",
                params![version, now_ms()],
            )?;
            tx.commit()?;
        }
        Ok(())
    }

    pub fn create_session(
        &self,
        parent_id: Option<&str>,
        cwd: impl AsRef<Path>,
    ) -> Result<SessionMeta> {
        let id = new_thread_id();
        let artifact_root = self.state_dir.join("artifacts").join(&id);
        self.create_session_with_id_and_artifact_root(parent_id, cwd, artifact_root, id)
    }

    /// Agent reads/writes everything inside the per-session tmp dir
    pub fn create_session_in_artifact_root(&self, parent_id: Option<&str>) -> Result<SessionMeta> {
        let id = new_thread_id();
        let artifact_root = self.state_dir.join("artifacts").join(&id);
        self.create_session_with_id_and_artifact_root(parent_id, &artifact_root, &artifact_root, id)
    }

    pub fn create_session_with_artifact_root(
        &self,
        parent_id: Option<&str>,
        cwd: impl AsRef<Path>,
        artifact_root: impl AsRef<Path>,
    ) -> Result<SessionMeta> {
        let id = new_thread_id();
        self.create_session_with_id_and_artifact_root(parent_id, cwd, artifact_root, id)
    }

    pub fn create_session_with_id_and_artifact_root(
        &self,
        parent_id: Option<&str>,
        cwd: impl AsRef<Path>,
        artifact_root: impl AsRef<Path>,
        id: impl Into<String>,
    ) -> Result<SessionMeta> {
        self.create_session_with_id_and_artifact_root_inner(
            parent_id,
            cwd,
            artifact_root,
            id.into(),
        )
    }

    fn create_session_with_id_and_artifact_root_inner(
        &self,
        parent_id: Option<&str>,
        cwd: impl AsRef<Path>,
        artifact_root: impl AsRef<Path>,
        id: String,
    ) -> Result<SessionMeta> {
        let now = now_ms();
        let artifact_root = artifact_root.as_ref().to_path_buf();
        std::fs::create_dir_all(&artifact_root)
            .with_context(|| format!("create artifact root {}", artifact_root.display()))?;
        let session = SessionMeta {
            id: id.clone(),
            parent_id: parent_id.map(ToOwned::to_owned),
            cwd: cwd.as_ref().display().to_string(),
            artifact_root: artifact_root.display().to_string(),
            status: SessionStatus::Created,
            created_ms: now,
            updated_ms: now,
        };
        self.insert_session(&session)?;
        self.notify(StoreNotification::SessionsChanged);
        self.append_event(&id, "session.created", serde_json::json!({}))?;
        self.load_session(&id)?
            .context("created session was not readable")
    }

    pub fn create_child_session(
        &self,
        parent_id: &str,
        cwd: impl AsRef<Path>,
        agent_path: Option<&str>,
        nickname: Option<&str>,
        role: Option<&str>,
    ) -> Result<SessionMeta> {
        let id = new_thread_id();
        self.create_child_session_with_id(parent_id, cwd, agent_path, nickname, role, id)
    }

    pub fn create_child_session_with_id(
        &self,
        parent_id: &str,
        cwd: impl AsRef<Path>,
        agent_path: Option<&str>,
        nickname: Option<&str>,
        role: Option<&str>,
        id: impl Into<String>,
    ) -> Result<SessionMeta> {
        self.load_session(parent_id)?
            .with_context(|| format!("unknown parent session id: {parent_id}"))?;
        let id = id.into();
        if id.trim().is_empty() {
            bail!("child session id must not be empty");
        }
        let now = now_ms();
        let artifact_root = self.state_dir.join("artifacts").join(&id);
        std::fs::create_dir_all(&artifact_root)
            .with_context(|| format!("create artifact root {}", artifact_root.display()))?;
        let session = SessionMeta {
            id: id.clone(),
            parent_id: Some(parent_id.to_string()),
            cwd: cwd.as_ref().display().to_string(),
            artifact_root: artifact_root.display().to_string(),
            status: SessionStatus::Created,
            created_ms: now,
            updated_ms: now,
        };
        let tx = self.conn.unchecked_transaction()?;
        tx.execute(
            "INSERT INTO sessions(id, parent_id, cwd, artifact_root, status, created_ms, updated_ms, agent_path, agent_nickname, agent_role) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                session.id,
                session.parent_id,
                session.cwd,
                session.artifact_root,
                session.status.as_str(),
                session.created_ms,
                session.updated_ms,
                agent_path,
                nickname,
                role,
            ],
        )?;
        tx.execute(
            "INSERT INTO agent_edges(parent_session_id, child_session_id, status, created_ms, updated_ms) VALUES (?1, ?2, 'open', ?3, ?3)",
            params![parent_id, id, now],
        )?;
        tx.commit()?;
        self.notify(StoreNotification::SessionsChanged);
        self.append_event(&id, "session.created", serde_json::json!({}))?;
        self.load_session(&id)?
            .context("created child session was not readable")
    }

    pub fn set_status(&self, session_id: &str, status: SessionStatus) -> Result<()> {
        let now = now_ms();
        self.conn.execute(
            "UPDATE sessions SET status = ?1, updated_ms = ?2 WHERE id = ?3",
            params![status.as_str(), now, session_id],
        )?;
        self.notify(StoreNotification::SessionChanged {
            session_id: session_id.to_string(),
        });
        Ok(())
    }

    pub fn request_cancel(&self, session_id: &str, reason: &str) -> Result<()> {
        self.load_session(session_id)?
            .with_context(|| format!("unknown session id: {session_id}"))?;
        self.append_event(
            session_id,
            "session.cancel_requested",
            serde_json::json!({ "reason": reason }),
        )?;
        self.append_event(
            session_id,
            "session.cancelled",
            serde_json::json!({ "reason": reason }),
        )?;
        Ok(())
    }

    pub fn append_event(
        &self,
        session_id: &str,
        event_type: &str,
        payload: Value,
    ) -> Result<EventRecord> {
        self.append_event_with_identity(
            session_id,
            Uuid::new_v4().simple().to_string(),
            now_ms(),
            event_type,
            payload,
        )
    }

    pub fn append_event_with_identity(
        &self,
        session_id: &str,
        event_id: String,
        ts_ms: i64,
        event_type: &str,
        payload: Value,
    ) -> Result<EventRecord> {
        let payload_json = serde_json::to_string(&payload)?;
        let tx = self.conn.unchecked_transaction()?;
        tx.execute(
            "INSERT INTO events(id, session_id, ts_ms, type, payload_json) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![event_id, session_id, ts_ms, event_type, payload_json],
        )?;
        let seq = tx.last_insert_rowid();
        if let Some(status) = status_for_event(event_type, &payload) {
            tx.execute(
                "UPDATE sessions SET status = ?1, updated_ms = ?2 WHERE id = ?3",
                params![status.as_str(), ts_ms, session_id],
            )?;
        } else {
            tx.execute(
                "UPDATE sessions SET updated_ms = ?1 WHERE id = ?2",
                params![ts_ms, session_id],
            )?;
        }
        tx.commit()?;
        self.notify(StoreNotification::EventsChanged {
            session_id: session_id.to_string(),
            seq,
        });
        self.notify(StoreNotification::SessionChanged {
            session_id: session_id.to_string(),
        });
        Ok(EventRecord {
            seq,
            id: event_id,
            session_id: session_id.to_string(),
            ts_ms,
            event_type: event_type.to_string(),
            payload,
        })
    }

    pub fn load_session(&self, session_id: &str) -> Result<Option<SessionMeta>> {
        self.conn
            .query_row(
                "SELECT id, parent_id, cwd, artifact_root, status, created_ms, updated_ms FROM sessions WHERE id = ?1",
                params![session_id],
                row_to_session,
            )
            .optional()
            .map_err(Into::into)
    }

    pub fn list_sessions(&self) -> Result<Vec<SessionMeta>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, parent_id, cwd, artifact_root, status, created_ms, updated_ms FROM sessions ORDER BY updated_ms DESC",
        )?;
        let rows = stmt
            .query_map([], row_to_session)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn events_for_session(&self, session_id: &str) -> Result<Vec<EventRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT seq, id, session_id, ts_ms, type, payload_json FROM events WHERE session_id = ?1 ORDER BY seq ASC",
        )?;
        let rows = stmt
            .query_map(params![session_id], row_to_event)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn latest_event_for_session_by_type(
        &self,
        session_id: &str,
        event_type: &str,
    ) -> Result<Option<EventRecord>> {
        self.conn
            .query_row(
                "SELECT seq, id, session_id, ts_ms, type, payload_json FROM events WHERE session_id = ?1 AND type = ?2 ORDER BY seq DESC LIMIT 1",
                params![session_id, event_type],
                row_to_event,
            )
            .optional()
            .map_err(Into::into)
    }

    pub fn events_after_seq(&self, session_id: &str, after_seq: i64) -> Result<Vec<EventRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT seq, id, session_id, ts_ms, type, payload_json FROM events WHERE session_id = ?1 AND seq > ?2 ORDER BY seq ASC",
        )?;
        let rows = stmt
            .query_map(params![session_id, after_seq], row_to_event)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn wait_for_events_after_seq(
        &self,
        session_id: &str,
        after_seq: i64,
        timeout: Duration,
    ) -> Result<Vec<EventRecord>> {
        let deadline = Instant::now() + timeout;
        let watcher = self.notification_watcher();
        let mut cursor = watcher.cursor();
        loop {
            let events = self.events_after_seq(session_id, after_seq)?;
            if !events.is_empty() || Instant::now() >= deadline {
                return Ok(events);
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if !watcher.wait_for_change(cursor, remaining) {
                return self.events_after_seq(session_id, after_seq);
            }
            cursor = watcher.cursor();
        }
    }

    pub fn record_artifact(
        &self,
        session_id: &str,
        event_seq: Option<i64>,
        kind: &str,
        path: impl AsRef<Path>,
        mime: Option<&str>,
        metadata: Value,
    ) -> Result<ArtifactMeta> {
        self.load_session(session_id)?
            .with_context(|| format!("unknown session id: {session_id}"))?;
        let id = Uuid::new_v4().simple().to_string();
        let path = path.as_ref().display().to_string();
        let bytes = std::fs::metadata(&path)
            .ok()
            .and_then(|metadata| i64::try_from(metadata.len()).ok());
        let created_ms = now_ms();
        self.conn.execute(
            "INSERT INTO artifacts(id, session_id, event_seq, kind, path, mime, bytes, created_ms, metadata_json) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                id,
                session_id,
                event_seq,
                kind,
                path,
                mime,
                bytes,
                created_ms,
                serde_json::to_string(&metadata)?
            ],
        )?;
        Ok(ArtifactMeta {
            id,
            session_id: session_id.to_string(),
            event_seq,
            kind: kind.to_string(),
            path,
            mime: mime.map(ToOwned::to_owned),
            bytes,
            created_ms,
        })
    }

    pub fn artifacts_for_session(&self, session_id: &str) -> Result<Vec<ArtifactMeta>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, session_id, event_seq, kind, path, mime, bytes, created_ms FROM artifacts WHERE session_id = ?1 ORDER BY created_ms ASC, id ASC",
        )?;
        let rows = stmt
            .query_map(params![session_id], row_to_artifact)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn record_run_started(&self, session_id: &str, pid: Option<i64>) -> Result<String> {
        self.load_session(session_id)?
            .with_context(|| format!("unknown session id: {session_id}"))?;
        let id = Uuid::new_v4().simple().to_string();
        self.conn.execute(
            "INSERT INTO runs(id, session_id, pid, status, started_ms, ended_ms) VALUES (?1, ?2, ?3, 'running', ?4, NULL)",
            params![id, session_id, pid, now_ms()],
        )?;
        Ok(id)
    }

    pub fn finish_run(&self, run_id: &str, status: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE runs SET status = ?1, ended_ms = ?2 WHERE id = ?3",
            params![status, now_ms(), run_id],
        )?;
        Ok(())
    }

    pub fn runs_for_session(&self, session_id: &str) -> Result<Vec<RunSummary>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, session_id, pid, status, started_ms, ended_ms FROM runs WHERE session_id = ?1 ORDER BY started_ms ASC, id ASC",
        )?;
        let rows = stmt
            .query_map(params![session_id], row_to_run_summary)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn list_child_agents(&self, parent_session_id: &str) -> Result<Vec<AgentSummary>> {
        let mut stmt = self.conn.prepare(
            r#"
            SELECT
                e.parent_session_id,
                e.child_session_id,
                e.status,
                s.agent_path,
                s.agent_nickname,
                s.agent_role,
                e.updated_ms
            FROM agent_edges e
            JOIN sessions s ON s.id = e.child_session_id
            WHERE e.parent_session_id = ?1
            ORDER BY e.updated_ms DESC, e.child_session_id ASC
            "#,
        )?;
        let rows = stmt
            .query_map(params![parent_session_id], row_to_agent_summary)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn agent_path_for_session(&self, session_id: &str) -> Result<Option<String>> {
        self.conn
            .query_row(
                "SELECT agent_path FROM sessions WHERE id = ?1",
                params![session_id],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()
            .map(|value| value.flatten())
            .map_err(Into::into)
    }

    pub fn agent_summary_for_child(&self, child_session_id: &str) -> Result<Option<AgentSummary>> {
        self.conn
            .query_row(
                r#"
                SELECT
                    e.parent_session_id,
                    e.child_session_id,
                    e.status,
                    s.agent_path,
                    s.agent_nickname,
                    s.agent_role,
                    e.updated_ms
                FROM agent_edges e
                JOIN sessions s ON s.id = e.child_session_id
                WHERE e.child_session_id = ?1
                "#,
                params![child_session_id],
                row_to_agent_summary,
            )
            .optional()
            .map_err(Into::into)
    }

    pub fn close_child_agent(&self, child_session_id: &str, reason: &str) -> Result<()> {
        self.load_session(child_session_id)?
            .with_context(|| format!("unknown child session id: {child_session_id}"))?;
        let now = now_ms();
        self.conn.execute(
            "UPDATE agent_edges SET status = 'closed', updated_ms = ?1 WHERE child_session_id = ?2",
            params![now, child_session_id],
        )?;
        let mut to_cancel = vec![child_session_id.to_string()];
        self.collect_descendant_agent_ids(child_session_id, &mut to_cancel)?;
        for id in to_cancel {
            let is_active = self
                .load_session(&id)?
                .is_some_and(|session| session.status.is_active());
            if is_active {
                self.request_cancel(&id, reason)?;
            }
            self.notify(StoreNotification::SessionChanged { session_id: id });
        }
        Ok(())
    }

    pub fn reopen_child_agent_subtree(&self, child_session_id: &str) -> Result<()> {
        self.load_session(child_session_id)?
            .with_context(|| format!("unknown child session id: {child_session_id}"))?;
        self.reopen_child_agent_subtree_inner(child_session_id, true)
    }

    fn reopen_child_agent_subtree_inner(
        &self,
        child_session_id: &str,
        is_target: bool,
    ) -> Result<()> {
        let summary = self
            .agent_summary_for_child(child_session_id)?
            .with_context(|| {
                format!("unknown child agent edge for session id: {child_session_id}")
            })?;
        if !is_target && summary.status != "open" {
            return Ok(());
        }
        let now = now_ms();
        self.conn.execute(
            "UPDATE agent_edges SET status = 'open', updated_ms = ?1 WHERE child_session_id = ?2",
            params![now, child_session_id],
        )?;
        if self.load_session(child_session_id)?.is_some_and(|session| {
            matches!(
                session.status,
                SessionStatus::Done | SessionStatus::Failed | SessionStatus::Cancelled
            )
        }) {
            self.set_status(child_session_id, SessionStatus::Created)?;
        } else {
            self.notify(StoreNotification::SessionChanged {
                session_id: child_session_id.to_string(),
            });
        }
        for child in self.list_child_agents(child_session_id)? {
            if child.status == "open" {
                self.reopen_child_agent_subtree_inner(&child.child_session_id, false)?;
            }
        }
        Ok(())
    }

    fn collect_descendant_agent_ids(
        &self,
        parent_session_id: &str,
        out: &mut Vec<String>,
    ) -> Result<()> {
        let mut stmt = self
            .conn
            .prepare("SELECT child_session_id FROM agent_edges WHERE parent_session_id = ?1")?;
        let children = stmt
            .query_map(params![parent_session_id], |row| row.get::<_, String>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        drop(stmt);
        for child in children {
            out.push(child.clone());
            self.collect_descendant_agent_ids(&child, out)?;
        }
        Ok(())
    }

    pub fn set_child_agent_status(&self, child_session_id: &str, status: &str) -> Result<()> {
        let now = now_ms();
        self.conn.execute(
            "UPDATE agent_edges SET status = ?1, updated_ms = ?2 WHERE child_session_id = ?3",
            params![status, now, child_session_id],
        )?;
        self.notify(StoreNotification::SessionChanged {
            session_id: child_session_id.to_string(),
        });
        Ok(())
    }

    pub fn send_agent_message(
        &self,
        author_session_id: &str,
        target_session_id: &str,
        content: &str,
        trigger_turn: bool,
    ) -> Result<AgentMessage> {
        self.send_agent_message_with_kind(
            author_session_id,
            target_session_id,
            content,
            None,
            "inter_agent",
            trigger_turn,
        )
    }

    pub fn send_agent_message_with_input_items(
        &self,
        author_session_id: &str,
        target_session_id: &str,
        content: &str,
        input_items: Option<Value>,
        trigger_turn: bool,
    ) -> Result<AgentMessage> {
        self.send_agent_message_with_kind(
            author_session_id,
            target_session_id,
            content,
            input_items,
            "user_input",
            trigger_turn,
        )
    }

    pub fn send_agent_message_with_kind(
        &self,
        author_session_id: &str,
        target_session_id: &str,
        content: &str,
        input_items: Option<Value>,
        input_kind: &str,
        trigger_turn: bool,
    ) -> Result<AgentMessage> {
        self.load_session(author_session_id)?
            .with_context(|| format!("unknown author session id: {author_session_id}"))?;
        self.load_session(target_session_id)?
            .with_context(|| format!("unknown target session id: {target_session_id}"))?;
        if input_kind.trim().is_empty() {
            bail!("agent message input_kind must not be empty");
        }
        let input_items_json = input_items
            .as_ref()
            .map(serde_json::to_string)
            .transpose()
            .context("serialize agent message input items")?;
        let message = AgentMessage {
            id: Uuid::new_v4().simple().to_string(),
            author_session_id: author_session_id.to_string(),
            target_session_id: target_session_id.to_string(),
            content: content.to_string(),
            input_items,
            input_kind: input_kind.to_string(),
            trigger_turn,
            created_ms: now_ms(),
        };
        self.conn.execute(
            "INSERT INTO agent_messages(id, author_session_id, target_session_id, content, input_items, input_kind, trigger_turn, created_ms) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                message.id,
                message.author_session_id,
                message.target_session_id,
                message.content,
                input_items_json,
                message.input_kind,
                if message.trigger_turn { 1 } else { 0 },
                message.created_ms,
            ],
        )?;
        self.notify(StoreNotification::SessionChanged {
            session_id: author_session_id.to_string(),
        });
        self.notify(StoreNotification::SessionChanged {
            session_id: target_session_id.to_string(),
        });
        Ok(message)
    }

    pub fn messages_for_agent(&self, target_session_id: &str) -> Result<Vec<AgentMessage>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, author_session_id, target_session_id, content, input_items, input_kind, trigger_turn, created_ms FROM agent_messages WHERE target_session_id = ?1 AND consumed_ms IS NULL ORDER BY created_ms ASC, id ASC",
        )?;
        let rows = stmt
            .query_map(params![target_session_id], row_to_agent_message)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn drain_agent_messages_for_agent(
        &self,
        target_session_id: &str,
    ) -> Result<Vec<AgentMessage>> {
        self.load_session(target_session_id)?
            .with_context(|| format!("unknown target session id: {target_session_id}"))?;
        let messages = self.messages_for_agent(target_session_id)?;
        if messages.is_empty() {
            return Ok(messages);
        }
        let consumed_ms = now_ms();
        let tx = self.conn.unchecked_transaction()?;
        for message in &messages {
            tx.execute(
                "UPDATE agent_messages SET consumed_ms = ?2 WHERE id = ?1 AND consumed_ms IS NULL",
                params![message.id, consumed_ms],
            )?;
        }
        tx.commit()?;
        self.notify(StoreNotification::SessionChanged {
            session_id: target_session_id.to_string(),
        });
        Ok(messages)
    }

    pub fn get_setting(&self, key: &str) -> Result<Option<String>> {
        self.conn
            .query_row(
                "SELECT value FROM app_settings WHERE key = ?1",
                params![key],
                |row| row.get(0),
            )
            .optional()
            .map_err(Into::into)
    }

    pub fn set_setting(&self, key: &str, value: &str) -> Result<()> {
        self.conn.execute(
            r#"
            INSERT INTO app_settings(key, value, updated_ms)
            VALUES (?1, ?2, ?3)
            ON CONFLICT(key) DO UPDATE SET
                value = excluded.value,
                updated_ms = excluded.updated_ms
            "#,
            params![key, value, now_ms()],
        )?;
        self.notify(StoreNotification::SettingsChanged);
        Ok(())
    }

    pub fn increment_u64_setting(&self, key: &str) -> Result<u64> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let current = tx
            .query_row(
                "SELECT value FROM app_settings WHERE key = ?1",
                params![key],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .and_then(|value| value.trim().parse::<u64>().ok())
            .unwrap_or(0);
        let next = current.saturating_add(1);
        tx.execute(
            r#"
            INSERT INTO app_settings(key, value, updated_ms)
            VALUES (?1, ?2, ?3)
            ON CONFLICT(key) DO UPDATE SET
                value = excluded.value,
                updated_ms = excluded.updated_ms
            "#,
            params![key, next.to_string(), now_ms()],
        )?;
        tx.commit()?;
        self.notify(StoreNotification::SettingsChanged);
        Ok(next)
    }

    pub fn delete_setting(&self, key: &str) -> Result<()> {
        self.conn
            .execute("DELETE FROM app_settings WHERE key = ?1", params![key])?;
        self.notify(StoreNotification::SettingsChanged);
        Ok(())
    }

    pub fn list_settings(&self) -> Result<Vec<(String, String)>> {
        let mut stmt = self
            .conn
            .prepare("SELECT key, value FROM app_settings ORDER BY key ASC")?;
        let rows = stmt
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn export_legacy_session(
        &self,
        session_id: &str,
        output_dir: impl AsRef<Path>,
    ) -> Result<()> {
        let session = self
            .load_session(session_id)?
            .with_context(|| format!("unknown session id: {session_id}"))?;
        let output_dir = output_dir.as_ref();
        std::fs::create_dir_all(output_dir)
            .with_context(|| format!("create export dir {}", output_dir.display()))?;
        let session_json = serde_json::json!({
            "id": session.id,
            "parent_id": session.parent_id,
            "state_dir": self.state_dir.display().to_string(),
            "artifact_dir": session.artifact_root,
            "cwd": session.cwd,
            "status": session.status.as_str(),
            "created_ms": session.created_ms,
            "updated_ms": session.updated_ms,
        });
        std::fs::write(
            output_dir.join("session.json"),
            format!("{}\n", serde_json::to_string_pretty(&session_json)?),
        )
        .with_context(|| format!("write {}", output_dir.join("session.json").display()))?;

        let file = File::create(output_dir.join("events.jsonl"))
            .with_context(|| format!("write {}", output_dir.join("events.jsonl").display()))?;
        let mut writer = BufWriter::new(file);
        for event in self.events_for_session(session_id)? {
            let line = serde_json::json!({
                "version": 1,
                "id": event.id,
                "ts_ms": event.ts_ms,
                "type": event.event_type,
                "session_id": event.session_id,
                "payload": event.payload,
            });
            writeln!(writer, "{}", serde_json::to_string(&line)?)?;
        }
        Ok(())
    }

    pub fn import_legacy_session(&self, input: impl AsRef<Path>) -> Result<SessionMeta> {
        let input = input.as_ref();
        let session_json_path = if input.is_dir() {
            input.join("session.json")
        } else {
            input.with_file_name("session.json")
        };
        let events_jsonl_path = if input.is_dir() {
            input.join("events.jsonl")
        } else {
            input.to_path_buf()
        };
        if !events_jsonl_path.exists() {
            bail!("events jsonl not found: {}", events_jsonl_path.display());
        }

        let events = read_legacy_events(&events_jsonl_path)?;
        let metadata_json = if session_json_path.exists() {
            Some(read_json(&session_json_path)?)
        } else {
            None
        };
        let session_id = metadata_json
            .as_ref()
            .and_then(|data| data.get("id"))
            .and_then(Value::as_str)
            .map(ToOwned::to_owned)
            .or_else(|| events.first().map(|event| event.session_id.clone()))
            .context("could not infer session id from import")?;
        if self.load_session(&session_id)?.is_some() {
            bail!("session already exists: {session_id}");
        }

        let now = now_ms();
        let artifact_root = metadata_json
            .as_ref()
            .and_then(|data| data.get("artifact_dir"))
            .and_then(Value::as_str)
            .map(ToOwned::to_owned)
            .unwrap_or_else(|| {
                self.state_dir
                    .join("artifacts")
                    .join(&session_id)
                    .display()
                    .to_string()
            });
        std::fs::create_dir_all(&artifact_root)
            .with_context(|| format!("create artifact root {artifact_root}"))?;

        let status = metadata_json
            .as_ref()
            .and_then(|data| data.get("status"))
            .and_then(Value::as_str)
            .unwrap_or("created")
            .parse::<SessionStatus>()
            .unwrap_or(SessionStatus::Created);
        let session = SessionMeta {
            id: session_id.clone(),
            parent_id: metadata_json
                .as_ref()
                .and_then(|data| data.get("parent_id"))
                .and_then(Value::as_str)
                .map(ToOwned::to_owned),
            cwd: metadata_json
                .as_ref()
                .and_then(|data| data.get("cwd"))
                .and_then(Value::as_str)
                .unwrap_or(".")
                .to_string(),
            artifact_root,
            status,
            created_ms: metadata_json
                .as_ref()
                .and_then(|data| data.get("created_ms"))
                .and_then(Value::as_i64)
                .or_else(|| events.first().map(|event| event.ts_ms))
                .unwrap_or(now),
            updated_ms: metadata_json
                .as_ref()
                .and_then(|data| data.get("updated_ms"))
                .and_then(Value::as_i64)
                .or_else(|| events.last().map(|event| event.ts_ms))
                .unwrap_or(now),
        };
        self.insert_session(&session)?;
        self.notify(StoreNotification::SessionsChanged);
        for event in events {
            self.append_event_with_identity(
                &session_id,
                event.id,
                event.ts_ms,
                &event.event_type,
                event.payload,
            )?;
        }
        self.load_session(&session_id)?
            .context("imported session was not readable")
    }

    fn insert_session(&self, session: &SessionMeta) -> Result<()> {
        self.conn.execute(
            "INSERT INTO sessions(id, parent_id, cwd, artifact_root, status, created_ms, updated_ms) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                session.id,
                session.parent_id,
                session.cwd,
                session.artifact_root,
                session.status.as_str(),
                session.created_ms,
                session.updated_ms
            ],
        )?;
        Ok(())
    }
}

fn row_to_session(row: &rusqlite::Row<'_>) -> rusqlite::Result<SessionMeta> {
    let status: String = row.get(4)?;
    Ok(SessionMeta {
        id: row.get(0)?,
        parent_id: row.get(1)?,
        cwd: row.get(2)?,
        artifact_root: row.get(3)?,
        status: SessionStatus::from_str(&status).map_err(|err| {
            rusqlite::Error::FromSqlConversionFailure(
                4,
                rusqlite::types::Type::Text,
                Box::new(std::io::Error::new(std::io::ErrorKind::InvalidData, err)),
            )
        })?,
        created_ms: row.get(5)?,
        updated_ms: row.get(6)?,
    })
}

fn row_to_event(row: &rusqlite::Row<'_>) -> rusqlite::Result<EventRecord> {
    let payload_json: String = row.get(5)?;
    let payload = serde_json::from_str(&payload_json).map_err(|err| {
        rusqlite::Error::FromSqlConversionFailure(5, rusqlite::types::Type::Text, Box::new(err))
    })?;
    Ok(EventRecord {
        seq: row.get(0)?,
        id: row.get(1)?,
        session_id: row.get(2)?,
        ts_ms: row.get(3)?,
        event_type: row.get(4)?,
        payload,
    })
}

fn row_to_artifact(row: &rusqlite::Row<'_>) -> rusqlite::Result<ArtifactMeta> {
    Ok(ArtifactMeta {
        id: row.get(0)?,
        session_id: row.get(1)?,
        event_seq: row.get(2)?,
        kind: row.get(3)?,
        path: row.get(4)?,
        mime: row.get(5)?,
        bytes: row.get(6)?,
        created_ms: row.get(7)?,
    })
}

fn row_to_agent_summary(row: &rusqlite::Row<'_>) -> rusqlite::Result<AgentSummary> {
    Ok(AgentSummary {
        parent_session_id: row.get(0)?,
        child_session_id: row.get(1)?,
        status: row.get(2)?,
        agent_path: row.get(3)?,
        agent_nickname: row.get(4)?,
        agent_role: row.get(5)?,
        updated_ms: row.get(6)?,
    })
}

fn row_to_agent_message(row: &rusqlite::Row<'_>) -> rusqlite::Result<AgentMessage> {
    let input_items_json: Option<String> = row.get(4)?;
    let input_items = input_items_json
        .as_deref()
        .and_then(|raw| serde_json::from_str(raw).ok());
    let trigger_turn: i64 = row.get(6)?;
    Ok(AgentMessage {
        id: row.get(0)?,
        author_session_id: row.get(1)?,
        target_session_id: row.get(2)?,
        content: row.get(3)?,
        input_items,
        input_kind: row.get(5)?,
        trigger_turn: trigger_turn != 0,
        created_ms: row.get(7)?,
    })
}

fn row_to_run_summary(row: &rusqlite::Row<'_>) -> rusqlite::Result<RunSummary> {
    Ok(RunSummary {
        id: row.get(0)?,
        session_id: row.get(1)?,
        pid: row.get(2)?,
        status: row.get(3)?,
        started_ms: row.get(4)?,
        ended_ms: row.get(5)?,
    })
}

fn status_for_event(event_type: &str, payload: &Value) -> Option<SessionStatus> {
    match event_type {
        "session.input" => Some(SessionStatus::Running),
        "session.followup" => Some(SessionStatus::Running),
        "agent.mailbox_input" => Some(SessionStatus::Running),
        "agent.run.started" => Some(SessionStatus::Running),
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
            .and_then(|status| SessionStatus::from_str(status).ok()),
        _ => None,
    }
}

pub fn now_ms() -> i64 {
    Utc::now().timestamp_millis()
}

/// Generate a public thread/session id with the same wire shape Codex uses for
/// subagent ids: a UUID string accepted by `ThreadId::from_string`.
pub fn new_thread_id() -> String {
    Uuid::new_v4().to_string()
}

/// Validate the public thread/session id grammar used by Codex's v1 multi-agent
/// tools. Existing imported sessions can still have arbitrary ids, but v1 tool
/// targets must be UUID strings.
pub fn is_thread_id(value: &str) -> bool {
    Uuid::parse_str(value.trim()).is_ok()
}

fn read_json(path: &Path) -> Result<Value> {
    let content =
        std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    serde_json::from_str(&content).with_context(|| format!("parse {}", path.display()))
}

fn read_legacy_events(path: &Path) -> Result<Vec<EventRecord>> {
    let file = File::open(path).with_context(|| format!("read {}", path.display()))?;
    let reader = BufReader::new(file);
    let mut events = Vec::new();
    for (idx, line) in reader.lines().enumerate() {
        let line =
            line.with_context(|| format!("read line {} from {}", idx + 1, path.display()))?;
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let data: Value = serde_json::from_str(line)
            .with_context(|| format!("parse line {} from {}", idx + 1, path.display()))?;
        events.push(EventRecord {
            seq: (idx + 1) as i64,
            id: data
                .get("id")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned)
                .unwrap_or_else(|| Uuid::new_v4().simple().to_string()),
            session_id: data
                .get("session_id")
                .and_then(Value::as_str)
                .context("legacy event missing session_id")?
                .to_string(),
            ts_ms: data
                .get("ts_ms")
                .and_then(Value::as_i64)
                .unwrap_or_else(now_ms),
            event_type: data
                .get("type")
                .and_then(Value::as_str)
                .context("legacy event missing type")?
                .to_string(),
            payload: data
                .get("payload")
                .cloned()
                .unwrap_or(Value::Object(Default::default())),
        });
    }
    Ok(events)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn creates_sessions_and_appends_events_in_sqlite() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let store = Store::open(temp.path())?;
        let session = store.create_session(None, "/tmp")?;
        store.append_event(
            &session.id,
            "session.input",
            serde_json::json!({"text": "open example"}),
        )?;
        store.append_event(
            &session.id,
            "session.done",
            serde_json::json!({"result": "complete"}),
        )?;

        let sessions = store.list_sessions()?;
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].status, SessionStatus::Done);
        let events = store.events_for_session(&session.id)?;
        assert_eq!(events.len(), 3);
        assert_eq!(events[1].event_type, "session.input");
        assert_eq!(events[2].payload["result"], "complete");
        Ok(())
    }

    #[test]
    fn followup_marks_session_running() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let store = Store::open(temp.path())?;
        let session = store.create_session(None, "/tmp")?;
        store.append_event(
            &session.id,
            "session.input",
            serde_json::json!({"text": "open example"}),
        )?;
        store.append_event(
            &session.id,
            "session.done",
            serde_json::json!({"result": "complete"}),
        )?;
        store.append_event(
            &session.id,
            "session.followup",
            serde_json::json!({"text": "continue"}),
        )?;

        assert_eq!(
            store
                .load_session(&session.id)?
                .map(|session| session.status),
            Some(SessionStatus::Running)
        );
        Ok(())
    }

    #[test]
    fn subagent_input_events_mark_session_running() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let store = Store::open(temp.path())?;
        let parent = store.create_session(None, "/tmp")?;
        let child = store.create_child_session(
            &parent.id,
            "/tmp",
            Some("/root/worker"),
            Some("worker"),
            Some("default"),
        )?;
        store.append_event(
            &child.id,
            "agent.mailbox_input",
            serde_json::json!({"content": "direct task"}),
        )?;
        assert_eq!(
            store.load_session(&child.id)?.map(|session| session.status),
            Some(SessionStatus::Running)
        );

        store.set_status(&child.id, SessionStatus::Created)?;
        store.append_event(
            &child.id,
            "agent.run.started",
            serde_json::json!({"run_id": "run-1"}),
        )?;
        assert_eq!(
            store.load_session(&child.id)?.map(|session| session.status),
            Some(SessionStatus::Running)
        );
        Ok(())
    }

    #[test]
    fn can_read_and_wait_for_events_after_seq() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let store = Store::open(temp.path())?;
        let session = store.create_session(None, "/tmp")?;
        let first = store.append_event(
            &session.id,
            "session.input",
            serde_json::json!({"text": "start"}),
        )?;
        assert!(store.events_after_seq(&session.id, first.seq)?.is_empty());
        let second = store.append_event(
            &session.id,
            "model.delta",
            serde_json::json!({"text": "working"}),
        )?;
        let events = store.events_after_seq(&session.id, first.seq)?;
        assert_eq!(events, vec![second.clone()]);
        let waited =
            store.wait_for_events_after_seq(&session.id, first.seq, Duration::from_millis(1))?;
        assert_eq!(waited, vec![second]);
        Ok(())
    }

    #[test]
    fn latest_event_for_session_by_type_returns_latest_matching_event() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let store = Store::open(temp.path())?;
        let session = store.create_session(None, "/tmp")?;
        store.append_event(
            &session.id,
            "agent.run.started",
            serde_json::json!({"run": 1}),
        )?;
        store.append_event(&session.id, "other", serde_json::json!({"run": 2}))?;
        let latest = store.append_event(
            &session.id,
            "agent.run.started",
            serde_json::json!({"run": 3}),
        )?;

        assert_eq!(
            store
                .latest_event_for_session_by_type(&session.id, "agent.run.started")?
                .expect("latest event")
                .seq,
            latest.seq
        );
        assert!(store
            .latest_event_for_session_by_type(&session.id, "missing")?
            .is_none());
        Ok(())
    }

    #[test]
    fn store_notifier_reports_sessions_events_and_settings() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let (tx, rx) = mpsc::channel();
        let store = Store::open_with_notifier(temp.path(), tx)?;
        let watcher = store.notification_watcher();
        let cursor = watcher.cursor();

        let session = store.create_session(None, "/tmp")?;
        assert!(
            watcher.wait_for_change(cursor, Duration::from_millis(1)),
            "store-owned watcher must wake on create_session notifications"
        );
        let startup_notifications: Vec<_> = rx.try_iter().collect();
        assert!(startup_notifications.contains(&StoreNotification::SessionsChanged));
        assert!(startup_notifications.iter().any(|notification| matches!(
            notification,
            StoreNotification::EventsChanged { session_id, .. } if session_id == &session.id
        )));

        let event = store.append_event(
            &session.id,
            "session.input",
            serde_json::json!({"text": "start"}),
        )?;
        let event_notifications: Vec<_> = rx.try_iter().collect();
        assert!(
            event_notifications.contains(&StoreNotification::EventsChanged {
                session_id: session.id.clone(),
                seq: event.seq,
            })
        );
        assert!(
            event_notifications.contains(&StoreNotification::SessionChanged {
                session_id: session.id.clone(),
            })
        );

        store.set_setting("setup.complete", "1")?;
        assert_eq!(rx.recv()?, StoreNotification::SettingsChanged);

        Ok(())
    }

    #[test]
    fn request_cancel_records_events_and_status() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let store = Store::open(temp.path())?;
        let session = store.create_session(None, "/tmp")?;
        store.append_event(
            &session.id,
            "session.input",
            serde_json::json!({"text": "open example"}),
        )?;
        store.request_cancel(&session.id, "test stop")?;

        let session = store.load_session(&session.id)?.expect("session");
        assert_eq!(session.status, SessionStatus::Cancelled);
        let events = store.events_for_session(&session.id)?;
        assert_eq!(events[2].event_type, "session.cancel_requested");
        assert_eq!(events[3].event_type, "session.cancelled");
        Ok(())
    }

    #[test]
    fn agent_closed_with_active_cancel_projects_cancelled_status() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let store = Store::open(temp.path())?;
        let session = store.create_session(None, "/tmp")?;
        store.append_event(
            &session.id,
            "session.input",
            serde_json::json!({"text": "start child"}),
        )?;
        store.append_event(
            &session.id,
            "agent.closed",
            serde_json::json!({
                "reason": "closed by close_agent",
                "cancelled_active_run": true,
            }),
        )?;

        let session = store.load_session(&session.id)?.expect("session");
        assert_eq!(session.status, SessionStatus::Cancelled);
        Ok(())
    }

    #[test]
    fn exports_and_imports_legacy_session_shape() -> Result<()> {
        let source = tempfile::tempdir()?;
        let source_store = Store::open(source.path())?;
        let session = source_store.create_session(None, "/tmp")?;
        source_store.append_event(
            &session.id,
            "session.input",
            serde_json::json!({"text": "find flights"}),
        )?;
        source_store.append_event(
            &session.id,
            "session.done",
            serde_json::json!({"result": "found"}),
        )?;

        let export_dir = tempfile::tempdir()?;
        source_store.export_legacy_session(&session.id, export_dir.path())?;
        assert!(export_dir.path().join("session.json").exists());
        assert!(export_dir.path().join("events.jsonl").exists());

        let imported = tempfile::tempdir()?;
        let imported_store = Store::open(imported.path())?;
        let imported_session = imported_store.import_legacy_session(export_dir.path())?;
        assert_eq!(imported_session.id, session.id);
        assert_eq!(imported_session.status, SessionStatus::Done);
        let events = imported_store.events_for_session(&session.id)?;
        assert_eq!(events.len(), 3);
        assert_eq!(events[1].payload["text"], "find flights");
        assert_eq!(events[2].payload["result"], "found");
        Ok(())
    }

    #[test]
    fn imports_checked_in_golden_legacy_session() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let store = Store::open(temp.path())?;
        let fixture_root =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/golden-events");
        let fixture = fixture_root.join("legacy-session");

        let session = store.import_legacy_session(&fixture)?;
        assert_eq!(session.id, "legacy-golden-001");
        assert_eq!(session.status, SessionStatus::Done);
        assert_eq!(session.cwd, "/tmp/browser-use-legacy-golden");

        let events = store.events_for_session(&session.id)?;
        assert_eq!(events.len(), 4);
        assert_eq!(events[0].event_type, "session.input");
        assert_eq!(events[1].event_type, "browser.page");
        assert_eq!(events[1].payload["viewport"]["w"], 1440);
        assert_eq!(events[2].event_type, "tool.output");
        assert_eq!(events[3].payload["result"], "Top story found");
        Ok(())
    }

    #[test]
    fn imports_all_checked_in_golden_event_fixtures() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let store = Store::open(temp.path())?;
        let fixture_root =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/golden-events");
        let mut imported = Vec::new();
        for entry in std::fs::read_dir(&fixture_root)? {
            let path = entry?.path();
            if path.is_dir() {
                let session = store.import_legacy_session(&path)?;
                let events = store.events_for_session(&session.id)?;
                assert!(
                    !events.is_empty(),
                    "fixture {} has no events",
                    path.display()
                );
                imported.push(session.id);
            }
        }
        imported.sort();
        assert_eq!(
            imported,
            vec![
                "golden-artifact-image-001",
                "golden-running-browser-001",
                "golden-subagent-followup-001",
                "legacy-golden-001",
            ]
        );
        Ok(())
    }

    #[test]
    fn records_artifact_index_rows() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let store = Store::open(temp.path())?;
        let session = store.create_session(None, "/tmp")?;
        let file_path = temp.path().join("example.txt");
        std::fs::write(&file_path, "hello")?;
        let event = store.append_event(
            &session.id,
            "artifact.created",
            serde_json::json!({"path": file_path.display().to_string()}),
        )?;
        let artifact = store.record_artifact(
            &session.id,
            Some(event.seq),
            "file",
            &file_path,
            Some("text/plain"),
            serde_json::json!({"label": "example"}),
        )?;
        assert_eq!(artifact.bytes, Some(5));
        let artifacts = store.artifacts_for_session(&session.id)?;
        assert_eq!(artifacts.len(), 1);
        assert_eq!(artifacts[0].id, artifact.id);
        assert_eq!(artifacts[0].event_seq, Some(event.seq));
        Ok(())
    }

    #[test]
    fn records_run_lifecycle_rows() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let store = Store::open(temp.path())?;
        let session = store.create_session(None, "/tmp")?;
        let run_id = store.record_run_started(&session.id, Some(1234))?;
        let runs = store.runs_for_session(&session.id)?;
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].id, run_id);
        assert_eq!(runs[0].pid, Some(1234));
        assert_eq!(runs[0].status, "running");
        assert_eq!(runs[0].ended_ms, None);
        store.finish_run(&run_id, "done")?;
        let runs = store.runs_for_session(&session.id)?;
        assert_eq!(runs[0].status, "done");
        assert!(runs[0].ended_ms.is_some());
        Ok(())
    }

    #[test]
    fn creates_lists_and_closes_child_agents() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let store = Store::open(temp.path())?;
        let parent = store.create_session(None, "/tmp")?;
        let child = store.create_child_session(
            &parent.id,
            "/tmp",
            Some("/root/research"),
            Some("research"),
            Some("explorer"),
        )?;
        let grandchild = store.create_child_session(
            &child.id,
            "/tmp",
            Some("/root/research/detail"),
            Some("detail"),
            Some("explorer"),
        )?;
        let agents = store.list_child_agents(&parent.id)?;
        assert_eq!(agents.len(), 1);
        assert_eq!(agents[0].child_session_id, child.id);
        assert_eq!(agents[0].agent_path.as_deref(), Some("/root/research"));
        assert_eq!(agents[0].status, "open");

        store.close_child_agent(&child.id, "test close")?;
        let agents = store.list_child_agents(&parent.id)?;
        assert_eq!(agents[0].status, "closed");
        let child = store.load_session(&child.id)?.expect("child");
        assert_eq!(child.status, SessionStatus::Cancelled);
        let grandchild = store.load_session(&grandchild.id)?.expect("grandchild");
        assert_eq!(grandchild.status, SessionStatus::Cancelled);
        let descendants = store.list_child_agents(&child.id)?;
        assert_eq!(descendants[0].status, "open");
        Ok(())
    }

    #[test]
    fn updates_child_agent_status_without_copying_child_events() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let store = Store::open(temp.path())?;
        let parent = store.create_session(None, "/tmp")?;
        let child = store.create_child_session(
            &parent.id,
            "/tmp",
            Some("/root/research"),
            Some("research"),
            Some("explorer"),
        )?;
        store.set_child_agent_status(&child.id, "done")?;
        let agents = store.list_child_agents(&parent.id)?;
        assert_eq!(agents[0].status, "done");
        assert!(
            store.events_for_session(&parent.id)?.is_empty()
                || store.events_for_session(&parent.id)?.len() == 1
        );
        Ok(())
    }

    #[test]
    fn rejects_duplicate_child_agent_paths_per_parent() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let store = Store::open(temp.path())?;
        let parent = store.create_session(None, "/tmp")?;
        store.create_child_session(&parent.id, "/tmp", Some("/root/research"), None, None)?;
        let duplicate =
            store.create_child_session(&parent.id, "/tmp", Some("/root/research"), None, None);
        assert!(duplicate.is_err());
        let child = store.list_child_agents(&parent.id)?.remove(0);
        assert_eq!(
            store
                .agent_path_for_session(&child.child_session_id)?
                .as_deref(),
            Some("/root/research")
        );

        let other_parent = store.create_session(None, "/tmp")?;
        store.create_child_session(&other_parent.id, "/tmp", Some("/root/research"), None, None)?;
        Ok(())
    }

    #[test]
    fn sends_and_lists_agent_messages() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let store = Store::open(temp.path())?;
        let parent = store.create_session(None, "/tmp")?;
        let child = store.create_child_session(
            &parent.id,
            "/tmp",
            Some("/root/research"),
            Some("research"),
            Some("explorer"),
        )?;
        let message =
            store.send_agent_message(&parent.id, &child.id, "please inspect docs", true)?;
        let messages = store.messages_for_agent(&child.id)?;
        assert_eq!(messages, vec![message]);
        assert!(messages[0].trigger_turn);
        let drained = store.drain_agent_messages_for_agent(&child.id)?;
        assert_eq!(drained, messages);
        assert!(store.messages_for_agent(&child.id)?.is_empty());
        let retained: i64 = store.conn.query_row(
            "SELECT COUNT(*) FROM agent_messages WHERE target_session_id = ?1 AND consumed_ms IS NOT NULL",
            params![child.id],
            |row| row.get(0),
        )?;
        assert_eq!(retained, 1);
        assert!(store.drain_agent_messages_for_agent(&child.id)?.is_empty());
        Ok(())
    }

    #[test]
    fn persists_app_settings_in_sqlite() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let store = Store::open(temp.path())?;
        assert_eq!(store.get_setting("setup.complete")?, None);
        store.set_setting("setup.complete", "1")?;
        assert_eq!(store.get_setting("setup.complete")?.as_deref(), Some("1"));
        store.delete_setting("setup.complete")?;
        assert_eq!(store.get_setting("setup.complete")?, None);
        store.set_setting("setup.complete", "1")?;
        assert_eq!(
            store.list_settings()?,
            vec![("setup.complete".to_string(), "1".to_string())]
        );
        Ok(())
    }

    #[test]
    fn increments_u64_settings_atomically_across_connections() -> Result<()> {
        use std::sync::{Arc, Barrier};

        let temp = tempfile::tempdir()?;
        Store::open(temp.path())?;
        let state_dir = temp.path().to_path_buf();
        let thread_count = 8;
        let increments_per_thread = 25;
        let barrier = Arc::new(Barrier::new(thread_count));

        let handles = (0..thread_count)
            .map(|_| {
                let state_dir = state_dir.clone();
                let barrier = Arc::clone(&barrier);
                std::thread::spawn(move || -> Result<()> {
                    let store = Store::open(&state_dir)?;
                    barrier.wait();
                    for _ in 0..increments_per_thread {
                        store.increment_u64_setting("promo.count")?;
                    }
                    Ok(())
                })
            })
            .collect::<Vec<_>>();

        for handle in handles {
            handle.join().expect("worker thread panicked")?;
        }

        let store = Store::open(temp.path())?;
        let expected = (thread_count * increments_per_thread).to_string();
        assert_eq!(
            store.get_setting("promo.count")?.as_deref(),
            Some(expected.as_str())
        );
        Ok(())
    }
}
