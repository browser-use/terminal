use std::any::Any;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use browser_use_protocol::EventRecord;
use browser_use_runtime::{
    AgentId, AttachChildAgentRequest, AttachRootAgentRequest, BrowserId as RuntimeBrowserId,
    MailboxDeliveryPhase as RuntimeMailboxDeliveryPhase, RunAgentRequest, RunId as RuntimeRunId,
    RuntimeHandle, SessionId as RuntimeSessionId,
};
use browser_use_store::{Store, StoreNotifier};
use serde_json::json;
use tokio_util::sync::CancellationToken;

use crate::config_overrides::ProviderRunConfig;
use crate::entrypoint::RuntimeTurnDriver;
use crate::session::SharedStore;
use crate::simple_harness;

#[derive(Clone)]
pub struct RuntimeAgentExecutor {
    inner: Arc<RuntimeAgentExecutorInner>,
}

struct RuntimeAgentExecutorInner {
    state_dir: PathBuf,
    runtime: RuntimeHandle,
    notifier: Option<StoreNotifier>,
    tokio: tokio::runtime::Runtime,
    background_active: Arc<(Mutex<usize>, Condvar)>,
}

#[derive(Clone)]
pub struct RuntimeAgentExecutorConfig {
    pub state_dir: PathBuf,
    pub runtime: RuntimeHandle,
    pub notifier: Option<StoreNotifier>,
    pub worker_threads: usize,
}

impl RuntimeAgentExecutorConfig {
    pub fn new(state_dir: impl Into<PathBuf>, runtime: RuntimeHandle) -> Self {
        Self {
            state_dir: state_dir.into(),
            runtime,
            notifier: None,
            worker_threads: 2,
        }
    }

    pub fn with_notifier(mut self, notifier: Option<StoreNotifier>) -> Self {
        self.notifier = notifier;
        self
    }

    pub fn with_worker_threads(mut self, worker_threads: usize) -> Self {
        self.worker_threads = worker_threads.max(1);
        self
    }
}

#[derive(Clone)]
pub struct RuntimeAgentRunRequest {
    pub session_id: String,
    pub config: ProviderRunConfig,
    pub run_id: Option<RuntimeRunId>,
    pub browser_id: Option<RuntimeBrowserId>,
    pub cancellation_token: Option<CancellationToken>,
}

impl RuntimeAgentRunRequest {
    pub fn new(session_id: impl Into<String>, config: ProviderRunConfig) -> Self {
        Self {
            session_id: session_id.into(),
            config,
            run_id: None,
            browser_id: None,
            cancellation_token: None,
        }
    }

    pub fn with_run_id(mut self, run_id: RuntimeRunId) -> Self {
        self.run_id = Some(run_id);
        self
    }

    pub fn with_browser_id(mut self, browser_id: RuntimeBrowserId) -> Self {
        self.browser_id = Some(browser_id);
        self
    }

    pub fn with_cancellation_token(mut self, cancellation_token: CancellationToken) -> Self {
        self.cancellation_token = Some(cancellation_token);
        self
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RuntimeAgentRunResult {
    pub session_id: String,
}

pub struct RuntimeAgentBackgroundCompletion {
    pub session_id: String,
    pub result: Result<RuntimeAgentRunResult>,
}

impl RuntimeAgentBackgroundCompletion {
    pub fn is_success(&self) -> bool {
        self.result.is_ok()
    }

    pub fn error_message(&self) -> Option<String> {
        self.result.as_ref().err().map(|error| format!("{error:#}"))
    }
}

impl RuntimeAgentExecutor {
    pub fn new(config: RuntimeAgentExecutorConfig) -> Result<Self> {
        let tokio = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .thread_name("browser-use-live-agent-runtime")
            .worker_threads(config.worker_threads)
            .build()
            .context("build live agent tokio runtime")?;
        Ok(Self {
            inner: Arc::new(RuntimeAgentExecutorInner {
                state_dir: config.state_dir,
                runtime: config.runtime,
                notifier: config.notifier,
                tokio,
                background_active: Arc::new((Mutex::new(0), Condvar::new())),
            }),
        })
    }

    pub fn state_dir(&self) -> &Path {
        &self.inner.state_dir
    }

    pub fn runtime_handle(&self) -> RuntimeHandle {
        self.inner.runtime.clone()
    }

    pub fn ensure_agent_attached(
        &self,
        session_id: &str,
        max_concurrent_threads_per_session: usize,
    ) -> Result<()> {
        let store = Store::open(&self.inner.state_dir)?;
        ensure_agent_attached(
            &self.inner.runtime,
            &store,
            session_id,
            max_concurrent_threads_per_session,
        )
    }

    pub fn run_blocking(&self, request: RuntimeAgentRunRequest) -> Result<RuntimeAgentRunResult> {
        let runtime_session_id = RuntimeSessionId::from_string(request.session_id.clone())?;
        let store =
            Store::open_with_optional_notifier(&self.inner.state_dir, self.inner.notifier.clone())?;
        let mut run_config = request.config.clone();
        let simple_harness_enabled = run_config.options.simple_harness;
        if simple_harness_enabled {
            simple_harness::prepare_existing_session(&store, &request.session_id, &mut run_config)?;
        }
        ensure_agent_attached(
            &self.inner.runtime,
            &store,
            &request.session_id,
            run_config
                .options
                .multi_agent_v2
                .max_concurrent_threads_per_session,
        )?;
        let initial_input =
            latest_runtime_durable_prompt_input_event(&self.inner.runtime, &runtime_session_id)?
                .map(|event| {
                    json!({
                        "source": "durable_prompt_input",
                        "event_type": event.event_type,
                        "source_event_seq": event.seq,
                        "payload": event.payload,
                    })
                });
        let session_meta = store
            .load_session(&request.session_id)?
            .with_context(|| format!("unknown session id: {}", request.session_id))?;
        let run_cwd = PathBuf::from(&session_meta.cwd);
        let shared_store: SharedStore = Arc::new(Mutex::new(store));
        let initial_cancel = request
            .cancellation_token
            .unwrap_or_else(CancellationToken::new);
        let provider_config = json!({
            "backend": format!("{:?}", run_config.backend),
            "model": run_config.model,
            "runtime_agent_executor": true,
        });
        let agent_id = AgentId::from_string(request.session_id.clone())?;
        let runtime = self.inner.runtime.clone();
        let request_session_id = request.session_id.clone();
        let config = run_config;
        let shared_store_for_loop = Arc::clone(&shared_store);
        let request_session_id_for_loop = request_session_id.clone();
        let result = self.inner.tokio.block_on(async move {
            let mut cancel = initial_cancel;
            let mut restart_count = 0usize;
            loop {
                let runtime_request = RunAgentRequest::new(runtime_session_id.clone())
                    .with_agent_id(agent_id.clone())
                    .with_provider_config(provider_config.clone())
                    .with_cwd(run_cwd.clone())
                    .with_input_source("runtime_agent_executor")
                    .with_cancellation_token(cancel.clone());
                let runtime_request = if let Some(run_id) = request.run_id.clone() {
                    runtime_request.with_run_id(run_id)
                } else {
                    runtime_request
                };
                let runtime_request = if let Some(initial_input) = initial_input.clone() {
                    runtime_request.with_initial_input(initial_input)
                } else {
                    runtime_request
                };
                let runtime_request = if let Some(browser_id) = request.browser_id.clone() {
                    runtime_request.with_browser_id(browser_id)
                } else {
                    runtime_request
                };
                let runner_runtime = runtime.clone();
                let shared_store_for_run = Arc::clone(&shared_store_for_loop);
                let request_session_id_for_run = request_session_id_for_loop.clone();
                let config_for_run = config.clone();
                let cancel_for_run = cancel.clone();
                let run_result = runtime
                    .run_agent(runtime_request, async move {
                        RuntimeTurnDriver::new(
                            shared_store_for_run,
                            request_session_id_for_run,
                            config_for_run,
                            cancel_for_run,
                            runner_runtime,
                        )
                        .run()
                        .await
                    })
                    .await;

                match run_result {
                    Ok(response) => break Ok(response),
                    Err(_)
                        if cancel.is_cancelled()
                            && restart_count < 16
                            && runtime_has_trigger_turn_mail(&runtime, &runtime_session_id) =>
                    {
                        restart_count = restart_count.saturating_add(1);
                        cancel = CancellationToken::new();
                        continue;
                    }
                    Err(error) => break Err(error),
                }
            }
        });
        match result {
            Ok(response) => {
                if simple_harness_enabled {
                    let mirror_result = mirror_session_outputs(&shared_store, &request_session_id);
                    cleanup_simple_harness_session(&shared_store, &request_session_id);
                    mirror_result?;
                }
                Ok(RuntimeAgentRunResult {
                    session_id: response.output.0,
                })
            }
            Err(error) => {
                append_session_failed_if_missing(
                    &shared_store,
                    &request_session_id,
                    &format!("{error:#}"),
                );
                if simple_harness_enabled {
                    let _ = mirror_session_outputs(&shared_store, &request_session_id);
                    cleanup_simple_harness_session(&shared_store, &request_session_id);
                }
                Err(error)
            }
        }
    }

    pub fn spawn_background(
        &self,
        thread_name: impl Into<String>,
        request: RuntimeAgentRunRequest,
        on_completion: impl FnOnce(RuntimeAgentBackgroundCompletion) + Send + 'static,
    ) -> Result<()> {
        let executor = self.clone();
        let session_id = request.session_id.clone();
        let active = BackgroundRunGuard::increment(Arc::clone(&self.inner.background_active));
        thread::Builder::new()
            .name(thread_name.into())
            .spawn(move || {
                let _active = active;
                let result = match catch_unwind(AssertUnwindSafe(|| executor.run_blocking(request)))
                {
                    Ok(result) => result,
                    Err(panic) => {
                        let message =
                            format!("agent executor panicked: {}", panic_payload_message(panic));
                        append_session_failed_to_state_dir(
                            executor.state_dir(),
                            &session_id,
                            &message,
                            executor.inner.notifier.clone(),
                        );
                        cleanup_simple_harness_session_in_state_dir(
                            executor.state_dir(),
                            &session_id,
                            executor.inner.notifier.clone(),
                        );
                        Err(anyhow!(message))
                    }
                };
                on_completion(RuntimeAgentBackgroundCompletion { session_id, result });
            })
            .context("spawn live agent executor thread")?;
        Ok(())
    }

    pub fn wait_for_background_idle(&self, timeout: Duration) -> bool {
        let (lock, cvar) = &*self.inner.background_active;
        let active = lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if *active == 0 {
            return true;
        }
        let (active, _) = cvar
            .wait_timeout_while(active, timeout, |active| *active > 0)
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *active == 0
    }
}

fn mirror_session_outputs(shared_store: &SharedStore, session_id: &str) -> Result<()> {
    let store = shared_store
        .lock()
        .map_err(|_| anyhow!("session store lock poisoned"))?;
    let mirror = simple_harness::mirror_existing_session(&store, session_id)?;
    simple_harness::record_mirror_event(&store, session_id, &mirror)?;
    Ok(())
}

fn cleanup_simple_harness_session(shared_store: &SharedStore, session_id: &str) {
    let Ok(store) = shared_store.lock() else {
        return;
    };
    let Ok(cleanup) = simple_harness::cleanup_existing_session(&store, session_id) else {
        return;
    };
    let _ = simple_harness::record_cleanup_event(&store, session_id, &cleanup);
}

fn cleanup_simple_harness_session_in_state_dir(
    state_dir: &Path,
    session_id: &str,
    notifier: Option<StoreNotifier>,
) {
    let Ok(store) = Store::open_with_optional_notifier(state_dir, notifier) else {
        return;
    };
    let Ok(cleanup) = simple_harness::cleanup_existing_session(&store, session_id) else {
        return;
    };
    let _ = simple_harness::record_cleanup_event(&store, session_id, &cleanup);
}

struct BackgroundRunGuard {
    active: Arc<(Mutex<usize>, Condvar)>,
}

impl BackgroundRunGuard {
    fn increment(active: Arc<(Mutex<usize>, Condvar)>) -> Self {
        let (lock, _) = &*active;
        let mut count = lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *count = count.saturating_add(1);
        drop(count);
        Self { active }
    }
}

impl Drop for BackgroundRunGuard {
    fn drop(&mut self) {
        let (lock, cvar) = &*self.active;
        let mut count = lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *count = count.saturating_sub(1);
        cvar.notify_all();
    }
}

fn latest_runtime_durable_prompt_input_event(
    runtime: &RuntimeHandle,
    runtime_session_id: &RuntimeSessionId,
) -> Result<Option<EventRecord>> {
    Ok(latest_durable_prompt_input_from_events(
        runtime.events_for_session(runtime_session_id)?,
    ))
}

fn latest_durable_prompt_input_from_events(events: Vec<EventRecord>) -> Option<EventRecord> {
    events.into_iter().rev().find(|event| {
        matches!(
            event.event_type.as_str(),
            "session.input" | "session.followup" | "agent.mailbox_input"
        )
    })
}

fn runtime_has_trigger_turn_mail(runtime: &RuntimeHandle, session_id: &RuntimeSessionId) -> bool {
    runtime
        .has_pending_trigger_turn_agent_mail_for_session(
            session_id,
            RuntimeMailboxDeliveryPhase::CurrentTurn,
        )
        .unwrap_or(false)
        || runtime
            .has_pending_trigger_turn_agent_mail_for_session(
                session_id,
                RuntimeMailboxDeliveryPhase::NextTurn,
            )
            .unwrap_or(false)
}

#[deprecated(note = "use RuntimeAgentExecutor; LiveAgentExecutor is a compatibility alias")]
pub type LiveAgentExecutor = RuntimeAgentExecutor;

#[deprecated(note = "use RuntimeAgentExecutorConfig")]
pub type LiveAgentExecutorConfig = RuntimeAgentExecutorConfig;

#[deprecated(note = "use RuntimeAgentRunRequest")]
pub type LiveAgentRunRequest = RuntimeAgentRunRequest;

#[deprecated(note = "use RuntimeAgentRunResult")]
pub type LiveAgentRunResult = RuntimeAgentRunResult;

#[deprecated(note = "use RuntimeAgentBackgroundCompletion")]
pub type LiveAgentBackgroundCompletion = RuntimeAgentBackgroundCompletion;

pub fn ensure_agent_attached(
    runtime: &RuntimeHandle,
    store: &Store,
    session_id: &str,
    max_concurrent_threads_per_session: usize,
) -> Result<()> {
    let agent_id = AgentId::from_string(session_id.to_string())?;
    if runtime.agents().thread(&agent_id).is_ok() {
        return Ok(());
    }

    let session = store
        .load_session(session_id)?
        .with_context(|| format!("unknown session id: {session_id}"))?;
    if let Some(parent_session_id) = session.parent_id.as_deref() {
        ensure_agent_attached(
            runtime,
            store,
            parent_session_id,
            max_concurrent_threads_per_session,
        )?;
        let summary = store
            .agent_summary_for_child(session_id)?
            .with_context(|| format!("missing agent edge for child session id: {session_id}"))?;
        runtime.attach_child_agent(AttachChildAgentRequest {
            parent_agent_id: AgentId::from_string(parent_session_id.to_string())?,
            child_agent_id: agent_id,
            child_session_id: RuntimeSessionId::from_string(session_id.to_string())?,
            cwd: PathBuf::from(&session.cwd),
            agent_path: summary
                .agent_path
                .unwrap_or_else(|| format!("/root/{session_id}")),
            nickname: summary.agent_nickname,
            role: summary.agent_role,
        })?;
    } else {
        runtime.attach_root_agent(AttachRootAgentRequest {
            session_id: RuntimeSessionId::from_string(session_id.to_string())?,
            cwd: PathBuf::from(&session.cwd),
            task: session_id.to_string(),
            max_concurrent_threads_per_session,
        })?;
    }
    Ok(())
}

fn append_session_failed_if_missing(shared_store: &SharedStore, session_id: &str, error: &str) {
    if let Ok(store) = shared_store.lock() {
        append_session_failed(&store, session_id, error);
    }
}

fn append_session_failed_to_state_dir(
    state_dir: &Path,
    session_id: &str,
    error: &str,
    notifier: Option<StoreNotifier>,
) {
    if let Ok(store) = Store::open_with_optional_notifier(state_dir, notifier) {
        append_session_failed(&store, session_id, error);
    }
}

fn append_session_failed(store: &Store, session_id: &str, error: &str) {
    let already_failed = store
        .events_for_session(session_id)
        .map(|events| {
            events
                .iter()
                .any(|event| event.event_type == "session.failed")
        })
        .unwrap_or(false);
    if already_failed {
        return;
    }
    let _ = store.append_event(
        session_id,
        "session.failed",
        serde_json::json!({ "error": error }),
    );
}

fn panic_payload_message(payload: Box<dyn Any + Send>) -> String {
    if let Some(message) = payload.downcast_ref::<&str>() {
        (*message).to_string()
    } else if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
    } else {
        "unknown panic payload".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config_overrides::AgentRunOptions;
    use crate::config_overrides::ProviderBackend;
    use browser_use_runtime::{
        BrowserUseRuntime, CreateRootAgentRequest, CreateThreadRequest, Durability, JournalAppend,
        JournalReader, JournalSink, LiveThreadPersistence, MemoryJournal, RuntimeEvent, SpawnEdge,
        SpawnEdgeStatus, SqliteJournal, StateIndex,
    };
    use serde_json::{json, Value};
    use tempfile::TempDir;

    fn sqlite_runtime(state_dir: &Path) -> Result<RuntimeHandle> {
        let journal = Arc::new(SqliteJournal::open(state_dir)?);
        let persistence: Arc<dyn LiveThreadPersistence> = journal.clone();
        let state_index: Arc<dyn StateIndex> = journal;
        Ok(BrowserUseRuntime::new(persistence, state_index).handle())
    }

    fn create_runtime_root(runtime: &RuntimeHandle, cwd: &Path) -> Result<RuntimeSessionId> {
        let root = runtime.create_root_agent(CreateRootAgentRequest {
            cwd: cwd.to_path_buf(),
            task: "runtime root".to_string(),
            max_concurrent_threads_per_session: 3,
        })?;
        Ok(root.session_id().clone())
    }

    #[derive(Clone, Default)]
    struct ReadFailingJournal {
        inner: MemoryJournal,
    }

    impl JournalSink for ReadFailingJournal {
        fn append_runtime_event(&self, event: &RuntimeEvent) -> Result<JournalAppend> {
            self.inner.append_runtime_event(event)
        }

        fn append_session_event(
            &self,
            session_id: &RuntimeSessionId,
            event_type: &str,
            payload: Value,
            durability: Durability,
        ) -> Result<JournalAppend> {
            self.inner
                .append_session_event(session_id, event_type, payload, durability)
        }

        fn flush(&self) -> Result<()> {
            self.inner.flush()
        }
    }

    impl JournalReader for ReadFailingJournal {
        fn load_session(
            &self,
            session_id: &RuntimeSessionId,
        ) -> Result<Option<browser_use_protocol::SessionMeta>> {
            self.inner.load_session(session_id)
        }

        fn list_sessions(&self) -> Result<Vec<browser_use_protocol::SessionMeta>> {
            self.inner.list_sessions()
        }

        fn events_for_session(&self, _session_id: &RuntimeSessionId) -> Result<Vec<EventRecord>> {
            Err(anyhow!("forced runtime read failure"))
        }

        fn events_after_seq(
            &self,
            _session_id: &RuntimeSessionId,
            _after_seq: i64,
        ) -> Result<Vec<EventRecord>> {
            Err(anyhow!("forced runtime read failure"))
        }
    }

    impl LiveThreadPersistence for ReadFailingJournal {
        fn create_thread(
            &self,
            request: CreateThreadRequest,
        ) -> Result<browser_use_protocol::SessionMeta> {
            self.inner.create_thread(request)
        }
    }

    impl StateIndex for ReadFailingJournal {
        fn open_spawn_edge(&self, edge: SpawnEdge) -> Result<()> {
            self.inner.open_spawn_edge(edge)
        }

        fn finish_spawn_edge(
            &self,
            child_session_id: &RuntimeSessionId,
            status: SpawnEdgeStatus,
        ) -> Result<()> {
            self.inner.finish_spawn_edge(child_session_id, status)
        }

        fn close_spawn_edge(
            &self,
            child_session_id: &RuntimeSessionId,
            reason: &str,
        ) -> Result<()> {
            self.inner.close_spawn_edge(child_session_id, reason)
        }

        fn list_children(&self, parent_session_id: &RuntimeSessionId) -> Result<Vec<SpawnEdge>> {
            self.inner.list_children(parent_session_id)
        }

        fn list_descendants(&self, root_session_id: &RuntimeSessionId) -> Result<Vec<SpawnEdge>> {
            self.inner.list_descendants(root_session_id)
        }
    }

    fn runtime_with_read_failing_journal() -> RuntimeHandle {
        let journal = Arc::new(ReadFailingJournal::default());
        let persistence: Arc<dyn LiveThreadPersistence> = journal.clone();
        let state_index: Arc<dyn StateIndex> = journal;
        BrowserUseRuntime::new(persistence, state_index).handle()
    }

    #[test]
    fn durable_prompt_input_prefers_runtime_journal_over_store() -> Result<()> {
        let store_dir = TempDir::new()?;
        let store = Store::open(store_dir.path())?;
        let session = store.create_session(None, Path::new("/work"))?;
        store.append_event(
            &session.id,
            "session.input",
            json!({ "text": "stale store input" }),
        )?;

        let (runtime, _journal) = BrowserUseRuntime::memory();
        let runtime = runtime.handle();
        let runtime_session_id = create_runtime_root(&runtime, Path::new("/work"))?;
        let runtime_input = runtime.append_observed_session_event(
            runtime_session_id.clone(),
            "session.input",
            json!({ "text": "runtime input" }),
            Durability::Barrier,
        )?;

        let selected = latest_runtime_durable_prompt_input_event(&runtime, &runtime_session_id)?
            .context("missing selected durable input")?;

        assert_eq!(selected.seq, runtime_input.seq.unwrap());
        assert_eq!(selected.payload["text"], "runtime input");
        Ok(())
    }

    #[test]
    fn durable_prompt_input_does_not_fall_back_to_store_when_runtime_has_none() -> Result<()> {
        let store_dir = TempDir::new()?;
        let store = Store::open(store_dir.path())?;
        let session = store.create_session(None, Path::new("/work"))?;
        store.append_event(
            &session.id,
            "session.input",
            json!({ "text": "store replay input" }),
        )?;

        let (runtime, _journal) = BrowserUseRuntime::memory();
        let runtime = runtime.handle();
        let runtime_session_id = create_runtime_root(&runtime, Path::new("/work"))?;

        let selected = latest_runtime_durable_prompt_input_event(&runtime, &runtime_session_id)?;

        assert!(
            selected.is_none(),
            "runtime executor must not resurrect Store-only input"
        );
        Ok(())
    }

    #[test]
    fn durable_prompt_input_runtime_read_failure_does_not_fall_back_to_store() -> Result<()> {
        let store_dir = TempDir::new()?;
        let store = Store::open(store_dir.path())?;
        let session = store.create_session(None, Path::new("/work"))?;
        store.append_event(
            &session.id,
            "session.input",
            json!({ "text": "store input must not be resurrected" }),
        )?;

        let runtime = runtime_with_read_failing_journal();
        let runtime_session_id = create_runtime_root(&runtime, Path::new("/work"))?;
        let error = latest_runtime_durable_prompt_input_event(&runtime, &runtime_session_id)
            .expect_err("runtime read failures must fail closed");

        assert!(
            format!("{error:#}").contains("forced runtime read failure"),
            "{error:#}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn durable_prompt_input_is_accepted_once_without_mailbox() -> Result<()> {
        let dir = TempDir::new()?;
        let store = Store::open(dir.path())?;
        let session = store.create_session(None, Path::new("/work"))?;
        let input = store.append_event(
            &session.id,
            "session.input",
            json!({ "text": "inspect the repo" }),
        )?;
        let runtime = sqlite_runtime(dir.path())?;
        ensure_agent_attached(&runtime, &store, &session.id, 3)?;

        let runtime_session_id = RuntimeSessionId::from_string(session.id.clone())?;
        let agent_id = AgentId::from_string(session.id.clone())?;
        let initial_input = json!({
            "source": "durable_prompt_input",
            "event_type": input.event_type,
            "source_event_seq": input.seq,
            "payload": input.payload,
        });
        let runtime_for_run = runtime.clone();
        let runtime_session_id_for_run = runtime_session_id.clone();
        runtime
            .run_agent(
                RunAgentRequest::new(runtime_session_id.clone())
                    .with_agent_id(agent_id.clone())
                    .with_initial_input(initial_input.clone()),
                async move {
                    let thread = runtime_for_run.agents().thread(&agent_id)?;
                    assert!(thread.mailbox().pending_items().is_empty());
                    let live = thread.live_state_snapshot();
                    assert_eq!(live.accepted_input_count, 1);
                    assert_eq!(live.pending_prompt_input_count, 1);
                    assert_eq!(live.last_accepted_prompt_input_seq, input.seq);
                    let consumed = runtime_for_run
                        .consume_prompt_input_for_session(&runtime_session_id_for_run)?;
                    assert!(consumed.consumed);
                    Ok::<_, anyhow::Error>(())
                },
            )
            .await?;

        let agent_id = AgentId::from_string(session.id.clone())?;
        let thread = runtime.agents().thread(&agent_id)?;
        assert!(thread.mailbox().pending_items().is_empty());

        let runtime_for_rerun = runtime.clone();
        let runtime_session_id_for_rerun = runtime_session_id.clone();
        runtime
            .run_agent(
                RunAgentRequest::new(runtime_session_id.clone())
                    .with_agent_id(agent_id.clone())
                    .with_initial_input(initial_input),
                async move {
                    let consumed = runtime_for_rerun
                        .consume_prompt_input_for_session(&runtime_session_id_for_rerun)?;
                    assert!(
                        !consumed.consumed,
                        "same durable input seq must not be re-accepted by run_agent"
                    );
                    Ok::<_, anyhow::Error>(())
                },
            )
            .await?;
        let live = thread.live_state_snapshot();
        assert_eq!(live.accepted_input_count, 1);
        assert_eq!(live.pending_prompt_input_count, 0);
        assert_eq!(live.last_consumed_prompt_input_seq, input.seq);

        let event_types = store
            .events_for_session(&session.id)?
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
        assert!(event_types
            .iter()
            .all(|event_type| event_type != "mailbox.enqueued"));
        Ok(())
    }

    #[test]
    fn run_blocking_prepares_simple_harness_and_mirrors_final_answer() -> Result<()> {
        let dir = TempDir::new()?;
        let cwd = dir.path().join("work");
        let artifact_root = dir.path().join("artifacts");
        let store = Store::open(dir.path())?;
        let session = store.create_session_with_id_and_artifact_root(
            None,
            &cwd,
            &artifact_root,
            "sess-1".to_string(),
        )?;
        store.append_event(
            &session.id,
            "session.input",
            json!({ "text": "return the canned fake result" }),
        )?;

        let runtime = sqlite_runtime(dir.path())?;
        let executor =
            RuntimeAgentExecutor::new(RuntimeAgentExecutorConfig::new(dir.path(), runtime))?;
        let config = ProviderRunConfig::new(ProviderBackend::Fake, "fake-model")
            .with_fake_result("mirrored fake answer")
            .with_options(AgentRunOptions::default().with_simple_harness(true));

        let result = executor.run_blocking(RuntimeAgentRunRequest::new(&session.id, config))?;

        assert_eq!(result.session_id, session.id);
        let paths = simple_harness::SimpleHarnessPaths::from_session(dir.path(), &session);
        assert!(paths.browser_skill_path.is_file());
        assert!(paths.agent_workspace.is_dir());
        assert!(paths.domain_skills_root.is_dir());
        assert_eq!(
            std::fs::read_to_string(&paths.final_txt)?,
            "mirrored fake answer"
        );
        let events_jsonl = std::fs::read_to_string(&paths.events_jsonl)?;
        assert!(events_jsonl.contains("\"harness.prepared\""));
        assert!(events_jsonl.contains("\"session.done\""));

        let event_types = store
            .events_for_session(&session.id)?
            .into_iter()
            .map(|event| event.event_type)
            .collect::<Vec<_>>();
        assert!(event_types.contains(&simple_harness::SIMPLE_HARNESS_PREPARED_EVENT.to_string()));
        assert!(event_types.contains(&simple_harness::SIMPLE_HARNESS_MIRRORED_EVENT.to_string()));
        assert!(event_types.contains(&simple_harness::SIMPLE_HARNESS_CLEANED_EVENT.to_string()));
        Ok(())
    }

    #[test]
    fn run_blocking_skips_simple_harness_when_disabled() -> Result<()> {
        let dir = TempDir::new()?;
        let cwd = dir.path().join("work");
        let artifact_root = dir.path().join("artifacts");
        let store = Store::open(dir.path())?;
        let session = store.create_session_with_id_and_artifact_root(
            None,
            &cwd,
            &artifact_root,
            "sess-legacy".to_string(),
        )?;
        store.append_event(
            &session.id,
            "session.input",
            json!({ "text": "return the canned fake result" }),
        )?;

        let runtime = sqlite_runtime(dir.path())?;
        let executor =
            RuntimeAgentExecutor::new(RuntimeAgentExecutorConfig::new(dir.path(), runtime))?;
        let config = ProviderRunConfig::new(ProviderBackend::Fake, "fake-model")
            .with_fake_result("legacy fake answer");

        let result = executor.run_blocking(RuntimeAgentRunRequest::new(&session.id, config))?;

        assert_eq!(result.session_id, session.id);
        let paths = simple_harness::SimpleHarnessPaths::from_session(dir.path(), &session);
        assert!(!paths.browser_skill_path.exists());
        assert!(!paths.final_txt.exists());

        let event_types = store
            .events_for_session(&session.id)?
            .into_iter()
            .map(|event| event.event_type)
            .collect::<Vec<_>>();
        assert!(event_types.iter().any(|event| event == "session.done"));
        assert!(!event_types.contains(&simple_harness::SIMPLE_HARNESS_PREPARED_EVENT.to_string()));
        assert!(!event_types.contains(&simple_harness::SIMPLE_HARNESS_MIRRORED_EVENT.to_string()));
        assert!(!event_types.contains(&simple_harness::SIMPLE_HARNESS_CLEANED_EVENT.to_string()));
        Ok(())
    }
}
