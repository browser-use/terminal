use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use anyhow::{bail, Context, Result};
#[cfg(test)]
use browser_use_agent::config_overrides::ChildAgentCompletionHandler;
use browser_use_agent::config_overrides::{
    load_mcp_servers_for_profile, resolve_agent_roles_for_profile,
    resolve_approval_policy_for_profile, resolve_collab_for_profile, resolve_guardian_for_profile,
    resolve_multi_agent_v2_for_profile, AgentRunOptions, ChildAgentRunCompletion,
    ChildAgentRunRequest, ChildAgentRunner, ConfigOverrides, ProviderRunConfig,
};
use browser_use_agent::context::{
    typed_user_input_payload_from_items_for_cwd, typed_user_input_payload_from_text_for_cwd,
};
use browser_use_agent::live_executor::{
    ensure_agent_attached as ensure_runtime_agent_attached, RuntimeAgentExecutor,
    RuntimeAgentExecutorConfig, RuntimeAgentRunRequest,
};
use browser_use_agent::prompts::CollaborationModeKind;
use browser_use_agent::rollout::fork_events_by_turn;
use browser_use_agent::session::{
    provider_messages_from_events_for_fork, resume::provider_messages_to_fork_response_items,
    ForkMode,
};
use browser_use_agent::subagents::{display_agent_path_for_session, session_was_interrupted};
use browser_use_protocol::{failure_from_events, session_result_from_events, SessionMeta};
use browser_use_runtime::{
    spawn_local_runtime_server, AgentId, AgentThreadStatus, BrowserUseRuntime,
    CompleteAgentRequest, FailAgentRequest, LiveThreadPersistence, MailboxDeliveryPhase,
    MailboxItem, RunId as RuntimeRunId, RuntimeHandle, RuntimeSnapshot, SessionId,
    SpawnChildRequest, SqliteJournal, StateIndex, SubmitInputRequest,
};
use browser_use_store::{Store, StoreNotifier};

use crate::settings::{
    browser_use_cloud_env_key_present, AgentBackend, BROWSER_LOCAL_CHROME, BROWSER_USE_CLOUD,
    BROWSER_USE_CLOUD_API_KEY_ENV, BROWSER_USE_CLOUD_API_KEY_SETTING,
};
use crate::{LOCAL_CHROME_CLOUD_PROMO_EVENT, LOCAL_CHROME_CLOUD_PROMO_TEXT};

static TUI_LIVE_RUNTIMES: OnceLock<Mutex<HashMap<PathBuf, RuntimeHandle>>> = OnceLock::new();
static TUI_RUNTIME_AGENT_EXECUTORS: OnceLock<Mutex<HashMap<PathBuf, RuntimeAgentExecutor>>> =
    OnceLock::new();
const LOCAL_CHROME_CLOUD_PROMO_QUALIFIED_TASK_COUNT_SETTING: &str =
    "session.cloud_promo.local_chrome_qualified_task_count";

fn tui_live_runtimes() -> &'static Mutex<HashMap<PathBuf, RuntimeHandle>> {
    TUI_LIVE_RUNTIMES.get_or_init(|| Mutex::new(HashMap::new()))
}

fn tui_runtime_agent_executors() -> &'static Mutex<HashMap<PathBuf, RuntimeAgentExecutor>> {
    TUI_RUNTIME_AGENT_EXECUTORS.get_or_init(|| Mutex::new(HashMap::new()))
}

pub(crate) fn cancel_agent_run(state_dir: &Path, session_id: &str) -> bool {
    let Ok(Some(handle)) = existing_tui_runtime_handle(state_dir) else {
        return false;
    };
    let Ok(session_id) = SessionId::from_string(session_id.to_string()) else {
        return false;
    };
    handle.cancel_run(&session_id)
}

pub(crate) fn pending_runtime_agent_mailbox_count(
    state_dir: &Path,
    session_id: &str,
) -> Result<Option<usize>> {
    let handle = tui_runtime_handle_with_attached_agent(state_dir, session_id)?;
    let Some(handle) = handle else {
        return Ok(None);
    };
    let session_id = SessionId::from_string(session_id.to_string())?;
    match handle.pending_agent_mail_for_session(&session_id) {
        Ok(items) => Ok(Some(items.len())),
        Err(_) => Ok(None),
    }
}

pub(crate) fn pending_runtime_trigger_turn_agent_mailbox_count(
    state_dir: &Path,
    session_id: &str,
) -> Result<Option<usize>> {
    let handle = tui_runtime_handle_with_attached_agent(state_dir, session_id)?;
    let Some(handle) = handle else {
        return Ok(None);
    };
    let session_id = SessionId::from_string(session_id.to_string())?;
    match handle.pending_agent_mail_for_session(&session_id) {
        Ok(items) => Ok(Some(
            items.into_iter().filter(|item| item.trigger_turn).count(),
        )),
        Err(_) => Ok(None),
    }
}

pub(crate) fn runtime_active_child_session_count(
    state_dir: &Path,
    root_session_id: &str,
) -> Result<Option<usize>> {
    let handle = tui_runtime_handle_with_attached_agent(state_dir, root_session_id)?;
    let Some(handle) = handle else {
        return Ok(None);
    };
    Ok(Some(
        handle
            .snapshot()
            .agents
            .into_iter()
            .filter(|agent| {
                agent
                    .parent_session_id
                    .as_ref()
                    .is_some_and(|parent| parent.as_str() == root_session_id)
                    && runtime_agent_status_is_active(&agent.status)
            })
            .count(),
    ))
}

pub(crate) fn runtime_snapshot(state_dir: &Path) -> Result<Option<RuntimeSnapshot>> {
    let Some(handle) = existing_tui_runtime_handle(state_dir)? else {
        return Ok(None);
    };
    Ok(Some(handle.snapshot()))
}

fn runtime_agent_status_is_active(status: &AgentThreadStatus) -> bool {
    matches!(
        status,
        AgentThreadStatus::Created
            | AgentThreadStatus::Queued
            | AgentThreadStatus::Running
            | AgentThreadStatus::Cancelling
    )
}

pub(crate) fn has_live_runtime_agent(state_dir: &Path, session_id: &str) -> bool {
    let Ok(Some(handle)) = tui_runtime_handle_with_attached_agent(state_dir, session_id) else {
        return false;
    };
    let Ok(agent_id) = AgentId::from_string(session_id.to_string()) else {
        return false;
    };
    handle.agents().thread(&agent_id).is_ok()
}

fn existing_tui_runtime_handle(state_dir: &Path) -> Result<Option<RuntimeHandle>> {
    tui_live_runtimes()
        .lock()
        .map_err(|_| anyhow::anyhow!("TUI live runtime registry lock poisoned"))
        .map(|runtimes| runtimes.get(state_dir).cloned())
}

fn tui_runtime_handle_with_attached_agent(
    state_dir: &Path,
    session_id: &str,
) -> Result<Option<RuntimeHandle>> {
    let store = Store::open(state_dir)?;
    if store.load_session(session_id)?.is_none() {
        return Ok(None);
    }
    let handle = match existing_tui_runtime_handle(state_dir)? {
        Some(handle) => handle,
        None => tui_runtime_handle_with_notifier(state_dir, None)?,
    };
    ensure_tui_agent_attached(
        &handle,
        &store,
        session_id,
        browser_use_agent::config_overrides::DEFAULT_MULTI_AGENT_V2_MAX_CONCURRENT_THREADS_PER_SESSION,
    )?;
    Ok(Some(handle))
}

pub(crate) fn submit_runtime_user_input(
    state_dir: &Path,
    session_id: &str,
    content: String,
    input_items: Option<serde_json::Value>,
    trigger_turn: bool,
    delivery_phase: MailboxDeliveryPhase,
    payload: serde_json::Value,
) -> Result<MailboxItem> {
    let handle = existing_tui_runtime_handle(state_dir)?
        .context("no live TUI runtime is attached for this state dir")?;
    let store = Store::open(state_dir)?;
    ensure_tui_agent_attached(
        &handle,
        &store,
        session_id,
        browser_use_agent::config_overrides::DEFAULT_MULTI_AGENT_V2_MAX_CONCURRENT_THREADS_PER_SESSION,
    )?;
    let agent_id = AgentId::from_string(session_id.to_string())?;
    let response = handle.submit_followup(SubmitInputRequest {
        target_agent_id: agent_id,
        content,
        trigger_turn,
        delivery_phase,
        input_items,
        payload,
    })?;
    Ok(response.mailbox_item)
}

#[cfg(test)]
pub(crate) fn tui_runtime_handle(state_dir: &Path) -> Result<RuntimeHandle> {
    tui_runtime_handle_with_notifier(state_dir, None)
}

fn tui_runtime_handle_with_notifier(
    state_dir: &Path,
    notifier: Option<StoreNotifier>,
) -> Result<RuntimeHandle> {
    let state_dir = state_dir.to_path_buf();
    if let Some(handle) = tui_live_runtimes()
        .lock()
        .ok()
        .and_then(|runtimes| runtimes.get(&state_dir).cloned())
    {
        return Ok(handle);
    }

    let journal = Arc::new(SqliteJournal::from_store(
        Store::open_with_optional_notifier(&state_dir, notifier)?,
    ));
    let persistence: Arc<dyn LiveThreadPersistence> = journal.clone();
    let state_index: Arc<dyn StateIndex> = journal;
    let handle = BrowserUseRuntime::new(persistence, state_index).handle();
    spawn_local_runtime_server(&state_dir, handle.clone())?;
    let mut runtimes = tui_live_runtimes()
        .lock()
        .map_err(|_| anyhow::anyhow!("TUI live runtime registry lock poisoned"))?;
    Ok(runtimes.entry(state_dir).or_insert_with(|| handle).clone())
}

fn tui_runtime_agent_executor_with_notifier(
    state_dir: &Path,
    notifier: Option<StoreNotifier>,
) -> Result<RuntimeAgentExecutor> {
    let state_dir = state_dir.to_path_buf();
    if let Some(executor) = tui_runtime_agent_executors()
        .lock()
        .ok()
        .and_then(|executors| executors.get(&state_dir).cloned())
    {
        return Ok(executor);
    }
    let runtime = tui_runtime_handle_with_notifier(&state_dir, notifier.clone())?;
    let executor = RuntimeAgentExecutor::new(
        RuntimeAgentExecutorConfig::new(state_dir.clone(), runtime)
            .with_notifier(notifier)
            .with_worker_threads(2),
    )?;
    let mut executors = tui_runtime_agent_executors()
        .lock()
        .map_err(|_| anyhow::anyhow!("TUI live executor registry lock poisoned"))?;
    Ok(executors
        .entry(state_dir)
        .or_insert_with(|| executor)
        .clone())
}

fn ensure_tui_agent_attached(
    runtime: &RuntimeHandle,
    store: &Store,
    session_id: &str,
    max_concurrent_threads_per_session: usize,
) -> Result<()> {
    ensure_runtime_agent_attached(
        runtime,
        store,
        session_id,
        max_concurrent_threads_per_session,
    )
}

pub(crate) fn spawn_tui_agent_run(
    state_dir: PathBuf,
    session_id: String,
    backend: AgentBackend,
    model: String,
    model_provider_id: Option<String>,
    browser: String,
    browser_profile_id: Option<String>,
    browser_profile_label: Option<String>,
    browser_local_browser: Option<String>,
    collaboration_mode: CollaborationModeKind,
    config_profile: Option<String>,
    config_overrides: ConfigOverrides,
    notifier: Option<StoreNotifier>,
) -> Result<()> {
    let selected_browser = browser.clone();
    let local_chrome_cloud_promo_user_turn_seq = {
        let store = Store::open(&state_dir)?;
        local_chrome_cloud_promo_user_turn_seq(&store, &session_id, &selected_browser)?
    };
    let (executor, config) = prepare_tui_agent_run(
        state_dir.clone(),
        &session_id,
        backend,
        model,
        model_provider_id,
        browser,
        browser_profile_id,
        browser_profile_label,
        browser_local_browser,
        collaboration_mode,
        config_profile,
        config_overrides,
        notifier,
    )?;
    executor.spawn_background(
        format!("browser-use-agent-{session_id}"),
        RuntimeAgentRunRequest::new(session_id.clone(), config),
        move |completion| {
            if let Some(error) = completion.error_message() {
                eprintln!("tui agent failed: {error}");
                return;
            }
            if let Err(error) = Store::open(&state_dir).and_then(|store| {
                maybe_append_local_chrome_cloud_promo(
                    &store,
                    &session_id,
                    &selected_browser,
                    local_chrome_cloud_promo_user_turn_seq,
                )
            }) {
                eprintln!("tui local Chrome cloud promo append failed: {error:#}");
            }
        },
    )?;
    Ok(())
}

fn prepare_tui_agent_run(
    state_dir: PathBuf,
    session_id: &str,
    backend: AgentBackend,
    model: String,
    model_provider_id: Option<String>,
    browser: String,
    browser_profile_id: Option<String>,
    browser_profile_label: Option<String>,
    browser_local_browser: Option<String>,
    collaboration_mode: CollaborationModeKind,
    config_profile: Option<String>,
    config_overrides: ConfigOverrides,
    notifier: Option<StoreNotifier>,
) -> Result<(RuntimeAgentExecutor, ProviderRunConfig)> {
    let store = Store::open_with_optional_notifier(&state_dir, notifier.clone())?;
    let browser_use_cloud_api_key = if browser == BROWSER_USE_CLOUD {
        browser_use_cloud_api_key(&store)?
    } else {
        None
    };
    if browser == BROWSER_USE_CLOUD && browser_use_cloud_api_key.is_none() {
        let error = "Browser Use Cloud selected, but BROWSER_USE_API_KEY is not set";
        let _ = store.append_event(
            &session_id,
            "session.failed",
            serde_json::json!({ "error": error }),
        );
        bail!(error);
    }
    if let Some(api_key) = browser_use_cloud_api_key
        .as_deref()
        .filter(|value| !value.trim().is_empty())
    {
        // Browser runtime is Rust-owned now, so the cloud API key must also be
        // visible to Rust-side Browser Use API calls, not only the legacy Python worker.
        std::env::set_var(BROWSER_USE_CLOUD_API_KEY_ENV, api_key);
    }
    let mut config = ProviderRunConfig::new(backend.into(), model.clone())
        .with_options(tui_agent_options(
            &browser,
            &session_id,
            collaboration_mode,
            model_provider_id.as_deref(),
            browser_profile_id.as_deref(),
            browser_profile_label.as_deref(),
            browser_local_browser.as_deref(),
            browser_use_cloud_api_key.as_deref(),
            config_profile.clone(),
            config_overrides.clone(),
        )?)
        .with_fake_result("Fake result from the Rust TUI agent loop.");
    let executor = tui_runtime_agent_executor_with_notifier(&state_dir, notifier)?;
    let runtime_handle = executor.runtime_handle();
    ensure_tui_agent_attached(
        &runtime_handle,
        &store,
        session_id,
        config
            .options
            .multi_agent_v2
            .max_concurrent_threads_per_session,
    )?;
    attach_tui_child_agent_runner(
        runtime_handle.clone(),
        state_dir.clone(),
        backend,
        model,
        model_provider_id,
        browser,
        browser_profile_id,
        browser_profile_label,
        browser_local_browser,
        collaboration_mode,
        config_profile,
        config_overrides,
        &mut config,
    );
    Ok((executor, config))
}

fn maybe_append_local_chrome_cloud_promo(
    store: &Store,
    session_id: &str,
    browser: &str,
    user_turn_seq: Option<i64>,
) -> Result<()> {
    if browser != BROWSER_LOCAL_CHROME {
        return Ok(());
    }
    let Some(user_turn_seq) = user_turn_seq else {
        return Ok(());
    };
    let events = store.events_for_session(session_id)?;
    if !should_append_local_chrome_cloud_promo(&events, user_turn_seq) {
        return Ok(());
    }
    let qualified_count = increment_local_chrome_cloud_promo_qualified_task_count(store)?;
    if qualified_count % 5 != 1 {
        return Ok(());
    }
    store.append_event(
        session_id,
        LOCAL_CHROME_CLOUD_PROMO_EVENT,
        serde_json::json!({ "text": LOCAL_CHROME_CLOUD_PROMO_TEXT }),
    )?;
    Ok(())
}

fn local_chrome_cloud_promo_user_turn_seq(
    store: &Store,
    session_id: &str,
    browser: &str,
) -> Result<Option<i64>> {
    if browser != BROWSER_LOCAL_CHROME {
        return Ok(None);
    }
    let events = store.events_for_session(session_id)?;
    let latest_user_turn = events.iter().rev().find(|event| {
        event.event_type == "session.input" || event.event_type.starts_with("session.followup")
    });
    Ok(latest_user_turn
        .filter(|event| event.event_type == "session.input")
        .map(|event| event.seq))
}

fn increment_local_chrome_cloud_promo_qualified_task_count(store: &Store) -> Result<u64> {
    store.increment_u64_setting(LOCAL_CHROME_CLOUD_PROMO_QUALIFIED_TASK_COUNT_SETTING)
}

fn should_append_local_chrome_cloud_promo(
    events: &[browser_use_protocol::EventRecord],
    user_turn_seq: i64,
) -> bool {
    let Some(user_turn_index) = events.iter().position(|event| event.seq == user_turn_seq) else {
        return false;
    };
    if events[user_turn_index].event_type != "session.input" {
        return false;
    }
    let next_user_turn_index = events
        .iter()
        .enumerate()
        .skip(user_turn_index + 1)
        .find(|(_, event)| {
            event.event_type == "session.input" || event.event_type.starts_with("session.followup")
        })
        .map(|(index, _)| index)
        .unwrap_or(events.len());
    let current_user_message_events = &events[user_turn_index..next_user_turn_index];
    let has_browser_connection = current_user_message_events
        .iter()
        .any(event_indicates_browser_connected);
    let has_success = current_user_message_events
        .iter()
        .any(|event| event.event_type == "session.done");
    let has_terminal_failure = current_user_message_events.iter().any(|event| {
        matches!(
            event.event_type.as_str(),
            "session.failed" | "session.cancelled"
        )
    });
    let already_prompted = events
        .iter()
        .any(|event| event.event_type == LOCAL_CHROME_CLOUD_PROMO_EVENT);
    has_browser_connection && has_success && !has_terminal_failure && !already_prompted
}

fn event_indicates_browser_connected(event: &browser_use_protocol::EventRecord) -> bool {
    if event.event_type == "browser.connected" {
        return true;
    }
    if event.event_type != "tool.output" {
        return false;
    }
    if event
        .payload
        .get("name")
        .and_then(serde_json::Value::as_str)
        != Some("browser")
    {
        return false;
    }
    if event.payload.get("ok").and_then(serde_json::Value::as_bool) == Some(false) {
        return false;
    }
    let Some(text) = event
        .payload
        .get("text")
        .and_then(serde_json::Value::as_str)
    else {
        return false;
    };
    serde_json::from_str::<serde_json::Value>(text)
        .ok()
        .and_then(|value| {
            value
                .get("connection")
                .and_then(serde_json::Value::as_str)
                .map(|connection| connection == "connected")
        })
        .unwrap_or(false)
}

fn browser_use_cloud_api_key(store: &Store) -> Result<Option<String>> {
    if let Some(value) = store
        .get_setting(BROWSER_USE_CLOUD_API_KEY_SETTING)?
        .filter(|value| !value.trim().is_empty())
    {
        return Ok(Some(value));
    }
    if browser_use_cloud_env_key_present() {
        return Ok(std::env::var(BROWSER_USE_CLOUD_API_KEY_ENV).ok());
    }
    Ok(None)
}

fn attach_tui_child_agent_runner(
    runtime_handle: RuntimeHandle,
    state_dir: PathBuf,
    backend: AgentBackend,
    model: String,
    model_provider_id: Option<String>,
    browser: String,
    browser_profile_id: Option<String>,
    browser_profile_label: Option<String>,
    browser_local_browser: Option<String>,
    collaboration_mode: CollaborationModeKind,
    config_profile: Option<String>,
    config_overrides: ConfigOverrides,
    config: &mut ProviderRunConfig,
) {
    let runner = ChildAgentRunner::new(move |request| {
        spawn_tui_child_agent(
            runtime_handle.clone(),
            state_dir.clone(),
            backend,
            model.clone(),
            model_provider_id.clone(),
            browser.clone(),
            browser_profile_id.clone(),
            browser_profile_label.clone(),
            browser_local_browser.clone(),
            collaboration_mode,
            config_profile.clone(),
            config_overrides.clone(),
            request,
        )
    });
    config.options = config.options.clone().with_child_agent_runner(runner);
}

fn spawn_tui_child_agent(
    runtime_handle: RuntimeHandle,
    state_dir: PathBuf,
    backend: AgentBackend,
    model: String,
    model_provider_id: Option<String>,
    browser: String,
    browser_profile_id: Option<String>,
    browser_profile_label: Option<String>,
    browser_local_browser: Option<String>,
    collaboration_mode: CollaborationModeKind,
    config_profile: Option<String>,
    mut config_overrides: ConfigOverrides,
    request: ChildAgentRunRequest,
) -> Result<()> {
    let store = Store::open(&state_dir)?;
    ensure_tui_agent_attached(
        &runtime_handle,
        &store,
        &request.parent_session_id,
        config_overrides
            .iter()
            .rev()
            .find(|(key, _)| key == "features.multi_agent_v2.max_concurrent_threads_per_session")
            .and_then(|(_, value)| value.as_integer())
            .and_then(|value| usize::try_from(value).ok())
            .unwrap_or(browser_use_agent::config_overrides::DEFAULT_MULTI_AGENT_V2_MAX_CONCURRENT_THREADS_PER_SESSION),
    )?;
    let child = create_tui_child_session_from_request(&runtime_handle, &store, &request)?;
    let child_id = child.id.clone();
    record_child_run_marker_from_request(&store, &child_id, &request)?;
    let child_model = request
        .model
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .map(ToOwned::to_owned)
        .unwrap_or(model);
    let child_browser = child_request_browser(&request).unwrap_or(browser);
    let child_model_provider_id = child_request_provider_id(&request).or(model_provider_id);
    let child_backend = child_model_provider_id
        .as_deref()
        .and_then(AgentBackend::from_setting)
        .unwrap_or(backend);
    config_overrides.extend(request.config_overrides.clone());
    if let Some(reasoning) = request.reasoning_effort.clone() {
        config_overrides.push((
            "reasoning_effort".to_string(),
            toml::Value::String(reasoning),
        ));
    }
    if let Some(service_tier) = request.service_tier.clone() {
        config_overrides.push((
            "service_tier".to_string(),
            toml::Value::String(service_tier),
        ));
    }
    let (executor, child_config) = prepare_tui_agent_run(
        state_dir.clone(),
        &child_id,
        child_backend,
        child_model,
        child_model_provider_id,
        child_browser,
        browser_profile_id,
        browser_profile_label,
        browser_local_browser,
        collaboration_mode,
        config_profile,
        config_overrides,
        None,
    )?;
    let mut run_request = RuntimeAgentRunRequest::new(child_id.clone(), child_config);
    if let Some(run_id) = request.run_id.as_ref() {
        run_request = run_request.with_run_id(RuntimeRunId::from_string(run_id.clone())?);
    }
    executor.spawn_background(
        format!("browser-use-tui-child-{child_id}"),
        run_request,
        move |completion| {
            let error = completion.error_message();
            notify_tui_child_completion(
                &runtime_handle,
                &state_dir,
                &child_id,
                &request,
                error.as_deref(),
            );
            if let Some(error) = error {
                eprintln!("tui child agent failed: {error}");
            }
        },
    )?;
    Ok(())
}

fn notify_tui_child_completion(
    runtime_handle: &RuntimeHandle,
    state_dir: &Path,
    child_id: &str,
    _request: &ChildAgentRunRequest,
    run_error: Option<&str>,
) {
    let completion = match run_error {
        Some(error) => ChildAgentRunCompletion::failure(error.to_string()),
        None => {
            let events = Store::open(state_dir)
                .and_then(|store| store.events_for_session(child_id))
                .ok();
            if events
                .as_deref()
                .is_some_and(child_run_was_interrupted_from_events)
            {
                return;
            }
            let summary = events.and_then(|events| {
                session_result_from_events(&events).or_else(|| failure_from_events(&events))
            });
            ChildAgentRunCompletion::success(summary)
        }
    };
    let child_agent_id = match AgentId::from_string(child_id.to_string()) {
        Ok(child_agent_id) => child_agent_id,
        Err(error) => {
            eprintln!("tui child agent completion runtime id failed: {error:#}");
            return;
        }
    };
    let runtime_result = if completion.success {
        runtime_handle.complete_agent(CompleteAgentRequest {
            child_agent_id,
            result: completion.summary.clone().unwrap_or_default(),
        })
    } else {
        runtime_handle.fail_agent(FailAgentRequest {
            child_agent_id,
            error: completion
                .summary
                .clone()
                .unwrap_or_else(|| "child agent failed".to_string()),
        })
    };
    if let Err(error) = runtime_result {
        eprintln!("tui child agent completion runtime update failed: {error:#}");
    }
}

fn child_run_was_interrupted_from_events(events: &[browser_use_protocol::EventRecord]) -> bool {
    session_was_interrupted(events)
}

fn create_tui_child_session_from_request(
    runtime_handle: &RuntimeHandle,
    store: &Store,
    request: &ChildAgentRunRequest,
) -> Result<SessionMeta> {
    if let Some(existing) = store.load_session(&request.child_session_id)? {
        return Ok(existing);
    }
    store
        .load_session(&request.parent_session_id)?
        .with_context(|| format!("unknown parent session id: {}", request.parent_session_id))?;
    let parent_agent_id = AgentId::from_string(request.parent_session_id.clone())?;
    let child_agent_id = AgentId::from_string(request.child_session_id.clone())?;
    let child_session_id = SessionId::from_string(request.child_session_id.clone())?;
    runtime_handle.spawn_child(SpawnChildRequest {
        parent_agent_id,
        child_agent_id: Some(child_agent_id),
        child_session_id: Some(child_session_id),
        task_name: task_name_from_agent_path(request.agent_path.as_deref())
            .unwrap_or_else(|| request.child_session_id.clone()),
        message: request.message.clone(),
        nickname: request.nickname.clone(),
        role: request.role.clone(),
    })?;
    let child = store
        .load_session(&request.child_session_id)?
        .with_context(|| {
            format!(
                "runtime did not create child session {}",
                request.child_session_id
            )
        })?;
    let parent_events = store.events_for_session(&request.parent_session_id)?;
    store.append_event(
        &child.id,
        "agent.context",
        child_request_agent_context_payload(&parent_events, request)?,
    )?;
    store.append_event(
        &child.id,
        "workspace.context",
        serde_json::json!({
            "kind": "environment_context",
            "content": format!(
                "<environment_context>\n<cwd>{}</cwd>\n</environment_context>",
                child.cwd
            ),
        }),
    )?;
    seed_child_permissions_context_event(store, &child.id, request)?;
    append_child_initial_input_from_request(store, &child.id, &child.cwd, request)?;
    store.append_event(
        &request.parent_session_id,
        "agent.spawned",
        serde_json::json!({
            "child_session_id": child.id.clone(),
            "agent_path": request.agent_path.clone(),
            "nickname": request.nickname.clone(),
            "role": request.role.clone(),
        }),
    )?;
    Ok(child)
}

fn task_name_from_agent_path(agent_path: Option<&str>) -> Option<String> {
    agent_path
        .and_then(|path| path.rsplit('/').find(|segment| !segment.trim().is_empty()))
        .map(ToOwned::to_owned)
}

fn record_child_run_marker_from_request(
    store: &Store,
    child_id: &str,
    request: &ChildAgentRunRequest,
) -> Result<()> {
    let Some(run_id) = request.run_id.as_deref() else {
        return Ok(());
    };
    let config_overrides = request
        .config_overrides
        .iter()
        .map(|(key, value)| {
            serde_json::json!({
                "key": key,
                "value": value,
            })
        })
        .collect::<Vec<_>>();
    store.append_event(
        child_id,
        "agent.run.started",
        serde_json::json!({
            "run_id": run_id,
            "parent_session_id": request.parent_session_id.as_str(),
            "child_session_id": child_id,
            "agent_path": request.agent_path.as_deref(),
            "model": request.model.as_deref(),
            "reasoning_effort": request.reasoning_effort.as_deref(),
            "service_tier": request.service_tier.as_deref(),
            "config_overrides": config_overrides,
        }),
    )?;
    Ok(())
}

fn append_child_initial_input_from_request(
    store: &Store,
    child_id: &str,
    child_cwd: &str,
    request: &ChildAgentRunRequest,
) -> Result<()> {
    if request.input_is_inter_agent_communication {
        let author_path = display_agent_path_for_session(store, &request.parent_session_id)
            .unwrap_or_else(|_| "/root".to_string());
        let recipient_path = request.agent_path.clone().unwrap_or_else(|| {
            display_agent_path_for_session(store, child_id).unwrap_or_else(|_| child_id.to_string())
        });
        store.append_event(
            child_id,
            "agent.mailbox_input",
            serde_json::json!({
                "id": browser_use_store::new_thread_id(),
                "author_session_id": request.parent_session_id,
                "target_session_id": child_id,
                "author_path": author_path,
                "recipient_path": recipient_path,
                "content": request.message,
                "trigger_turn": true,
            }),
        )?;
    } else {
        let payload = if let Some(items) = request.input_items.as_ref() {
            typed_user_input_payload_from_items_for_cwd(items, child_cwd)?
        } else {
            typed_user_input_payload_from_text_for_cwd(&request.message, child_cwd)?
        };
        store.append_event(child_id, "session.input", payload)?;
    }
    Ok(())
}

fn seed_child_permissions_context_event(
    store: &Store,
    session_id: &str,
    request: &ChildAgentRunRequest,
) -> Result<()> {
    let Some(content) = child_request_developer_instructions(request) else {
        return Ok(());
    };
    store.append_event(
        session_id,
        "workspace.context",
        serde_json::json!({
            "kind": "permissions",
            "content": content,
        }),
    )?;
    Ok(())
}

fn child_request_developer_instructions(request: &ChildAgentRunRequest) -> Option<String> {
    request
        .config_overrides
        .iter()
        .rev()
        .find(|(key, _)| key == "developer_instructions")
        .and_then(|(_, value)| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

fn child_request_provider_id(request: &ChildAgentRunRequest) -> Option<String> {
    request
        .config_overrides
        .iter()
        .rev()
        .find(|(key, _)| {
            matches!(
                key.as_str(),
                "model_provider" | "model_provider_id" | "provider"
            )
        })
        .and_then(|(_, value)| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

fn child_request_browser(request: &ChildAgentRunRequest) -> Option<String> {
    let browser_mode = request
        .config_overrides
        .iter()
        .rev()
        .find(|(key, _)| key == "browser_mode")
        .and_then(|(_, value)| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())?;
    match browser_mode {
        "managed-headless" => Some("Headless Chromium".to_string()),
        "managed-headed" => Some("Managed Chromium".to_string()),
        "cloud" => Some(BROWSER_USE_CLOUD.to_string()),
        "local" => Some("Local Chrome".to_string()),
        _ => None,
    }
}

fn child_request_agent_context_payload(
    parent_events: &[browser_use_protocol::EventRecord],
    request: &ChildAgentRunRequest,
) -> Result<serde_json::Value> {
    let mode = child_request_fork_mode(request.fork_turns.as_deref())?;
    let forked = fork_events_by_turn(parent_events, &mode);
    let history = provider_messages_from_events_for_fork(&forked.carried);
    let response_items = provider_messages_to_fork_response_items(&history);
    let raw_mode = request
        .fork_turns
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("all");
    let mut payload = serde_json::json!({
        "from_session_id": request.parent_session_id.clone(),
        "fork_mode": raw_mode,
        "agent_path": request.agent_path.clone(),
        "nickname": request.nickname.clone(),
        "role": request.role.clone(),
    });
    if matches!(mode, ForkMode::None) {
        payload["history_mode"] = serde_json::json!("none");
    } else {
        payload["history_mode"] = serde_json::json!("fork_response_items");
        payload["fork_response_items"] = serde_json::Value::Array(response_items);
    }
    Ok(payload)
}

fn child_request_fork_mode(raw: Option<&str>) -> Result<ForkMode> {
    let value = raw
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("all");
    if value.eq_ignore_ascii_case("none") {
        return Ok(ForkMode::None);
    }
    if value.eq_ignore_ascii_case("all") {
        return Ok(ForkMode::All);
    }
    let turns = value
        .parse::<usize>()
        .with_context(|| "fork_turns must be `none`, `all`, or a positive integer string")?;
    if turns == 0 {
        bail!("fork_turns must be `none`, `all`, or a positive integer string");
    }
    Ok(ForkMode::LastN(turns))
}

fn tui_agent_options(
    browser: &str,
    _session_id: &str,
    collaboration_mode: CollaborationModeKind,
    model_provider_id: Option<&str>,
    browser_profile_id: Option<&str>,
    browser_profile_label: Option<&str>,
    browser_local_browser: Option<&str>,
    browser_use_cloud_api_key: Option<&str>,
    config_profile: Option<String>,
    config_overrides: ConfigOverrides,
) -> Result<AgentRunOptions> {
    let profile_ref = config_profile.as_deref();
    let mut options = match browser {
        "Headless Chromium" => AgentRunOptions::default()
            .with_collaboration_mode(collaboration_mode)
            .with_browser_mode("managed-headless")
            .with_model_compaction(true)
            .with_analytics_source("tui"),
        "Managed Chromium" => AgentRunOptions::default()
            .with_collaboration_mode(collaboration_mode)
            .with_browser_mode("managed-headed")
            .with_model_compaction(true)
            .with_analytics_source("tui"),
        BROWSER_USE_CLOUD => {
            let mut options = AgentRunOptions::default()
                .with_collaboration_mode(collaboration_mode)
                .with_browser_mode("cloud")
                .with_model_compaction(true)
                .with_analytics_source("tui");
            if let Some(api_key) =
                browser_use_cloud_api_key.filter(|value| !value.trim().is_empty())
            {
                options = options.with_python_env(vec![(
                    BROWSER_USE_CLOUD_API_KEY_ENV.to_string(),
                    api_key.to_string(),
                )]);
            }
            options
        }
        _ => AgentRunOptions::default()
            .with_collaboration_mode(collaboration_mode)
            .with_browser_mode("local")
            .with_model_compaction(true)
            .with_analytics_source("tui"),
    };
    if let Some(profile_id) = browser_profile_id
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        options = options.with_browser_profile_id(profile_id.to_string());
    }
    if let Some(profile_label) = browser_profile_label
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        options = options.with_browser_profile_label(profile_label.to_string());
    }
    if let Some(local_browser) = browser_local_browser
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        options = options.with_browser_local_browser(local_browser.to_string());
    }
    if let Some(policy) = resolve_approval_policy_for_profile(profile_ref, &config_overrides, None)?
    {
        options = options.with_approval_policy(policy);
    }
    if let Some(use_guardian) = resolve_guardian_for_profile(profile_ref, &config_overrides, None)?
    {
        options = options.with_guardian(use_guardian);
    }
    options = options.with_multi_agent_v2(resolve_multi_agent_v2_for_profile(
        profile_ref,
        &config_overrides,
    )?);
    options =
        options.with_collab_enabled(resolve_collab_for_profile(profile_ref, &config_overrides)?);
    options = options.with_agent_roles(resolve_agent_roles_for_profile(
        profile_ref,
        &config_overrides,
    )?);
    let mcp_servers = load_mcp_servers_for_profile(profile_ref, &[])?;
    if !mcp_servers.is_empty() {
        options = options.with_mcp_servers(mcp_servers);
    }
    if let Some(profile) = config_profile {
        options = options.with_config_profile(profile);
    }
    if !config_overrides.is_empty() {
        options = options.with_config_overrides(config_overrides);
    }
    if let Some(model_provider_id) = model_provider_id
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        Ok(options.with_model_provider_id(model_provider_id.to_string()))
    } else {
        Ok(options)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use browser_use_agent::tools::AskForApproval;
    use browser_use_protocol::EventRecord;
    use std::sync::{Mutex, OnceLock};

    static ENV_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

    fn env_value<'a>(options: &'a AgentRunOptions, key: &str) -> Option<&'a str> {
        options
            .python_env
            .iter()
            .find(|(candidate, _)| candidate == key)
            .map(|(_, value)| value.as_str())
    }

    fn event(seq: i64, event_type: &str) -> EventRecord {
        event_with_payload(seq, event_type, serde_json::json!({}))
    }

    fn event_with_payload(seq: i64, event_type: &str, payload: serde_json::Value) -> EventRecord {
        EventRecord {
            seq,
            id: format!("event-{seq}"),
            session_id: "session-1".to_string(),
            ts_ms: seq,
            event_type: event_type.to_string(),
            payload,
        }
    }

    fn browser_status_connected_event(seq: i64) -> EventRecord {
        event_with_payload(
            seq,
            "tool.output",
            serde_json::json!({
                "name": "browser",
                "ok": true,
                "text": "{\"connection\":\"connected\"}"
            }),
        )
    }

    #[test]
    fn tui_agent_runtime_supports_block_in_place() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .thread_name("browser-use-live-agent-runtime-test")
            .worker_threads(2)
            .build()
            .unwrap();
        let result = runtime.block_on(async { tokio::task::block_in_place(|| 42) });
        assert_eq!(result, 42);
    }

    #[test]
    fn local_chrome_overrides_cloud_dotenv_mode() {
        let options = tui_agent_options(
            "Local Chrome",
            "abc123",
            CollaborationModeKind::Default,
            Some("codex"),
            None,
            None,
            None,
            None,
            None,
            Vec::new(),
        )
        .unwrap();
        assert_eq!(options.browser_mode.as_deref(), Some("local"));
        assert!(!options.dynamic_browser_mode_from_store);
        assert!(options.python_env.is_empty());
    }

    #[test]
    fn local_chrome_options_carry_session_profile_snapshot() {
        let options = tui_agent_options(
            "Local Chrome",
            "abc123",
            CollaborationModeKind::Default,
            Some("codex"),
            Some("google-chrome:Profile 1"),
            Some("Work"),
            Some("Google Chrome"),
            None,
            None,
            Vec::new(),
        )
        .unwrap();
        assert_eq!(options.browser_mode.as_deref(), Some("local"));
        assert_eq!(
            options.browser_profile_id.as_deref(),
            Some("google-chrome:Profile 1")
        );
        assert_eq!(options.browser_profile_label.as_deref(), Some("Work"));
        assert_eq!(
            options.browser_local_browser.as_deref(),
            Some("Google Chrome")
        );
        assert!(!options.dynamic_browser_mode_from_store);
    }

    #[test]
    fn headless_chromium_uses_managed_browser_not_inherited_cdp() {
        let options = tui_agent_options(
            "Headless Chromium",
            "abc123",
            CollaborationModeKind::Default,
            Some("codex"),
            None,
            None,
            None,
            None,
            None,
            Vec::new(),
        )
        .unwrap();
        assert_eq!(options.browser_mode.as_deref(), Some("managed-headless"));
        assert!(!options.dynamic_browser_mode_from_store);
        assert!(options.python_env.is_empty());
    }

    #[test]
    fn tui_agent_options_normalize_deprecated_plan_mode() {
        let options = tui_agent_options(
            "Local Chrome",
            "abc123",
            CollaborationModeKind::Plan,
            Some("codex"),
            None,
            None,
            None,
            None,
            None,
            Vec::new(),
        )
        .unwrap();
        assert_eq!(options.collaboration_mode, CollaborationModeKind::Default);
        assert_eq!(options.model_provider_id.as_deref(), Some("codex"));
    }

    #[test]
    fn browser_use_cloud_keeps_cloud_mode() {
        let options = tui_agent_options(
            BROWSER_USE_CLOUD,
            "abc123",
            CollaborationModeKind::Default,
            Some("codex"),
            None,
            None,
            None,
            None,
            None,
            Vec::new(),
        )
        .unwrap();
        assert_eq!(options.browser_mode.as_deref(), Some("cloud"));
        assert!(!options.dynamic_browser_mode_from_store);
        assert!(options.python_env.is_empty());
    }

    #[test]
    fn browser_use_cloud_passes_stored_key_to_worker_env() {
        let options = tui_agent_options(
            BROWSER_USE_CLOUD,
            "abc123",
            CollaborationModeKind::Default,
            Some("codex"),
            None,
            None,
            None,
            Some("bu-test"),
            None,
            Vec::new(),
        )
        .unwrap();
        assert_eq!(options.browser_mode.as_deref(), Some("cloud"));
        assert_eq!(
            env_value(&options, BROWSER_USE_CLOUD_API_KEY_ENV),
            Some("bu-test")
        );
    }

    #[test]
    fn local_chrome_cloud_promo_requires_browser_connection() {
        let no_browser_events = vec![event(1, "session.input"), event(2, "session.done")];
        assert!(!should_append_local_chrome_cloud_promo(
            &no_browser_events,
            1
        ));

        let events = vec![
            event(1, "session.input"),
            event(2, "browser.connected"),
            event(3, "session.done"),
        ];
        assert!(should_append_local_chrome_cloud_promo(&events, 1));

        let tool_output_events = vec![
            event(1, "session.input"),
            browser_status_connected_event(2),
            event(3, "session.done"),
        ];
        assert!(should_append_local_chrome_cloud_promo(
            &tool_output_events,
            1
        ));
    }

    #[test]
    fn local_chrome_cloud_promo_requires_browser_connection_before_followup_and_skips_existing_promos(
    ) {
        let browser_connected_before_followup = vec![
            event(1, "session.input"),
            event(2, "browser.connected"),
            event(3, "session.done"),
            event(4, "session.followup"),
        ];
        assert!(should_append_local_chrome_cloud_promo(
            &browser_connected_before_followup,
            1
        ));

        let browser_connected_after_followup = vec![
            event(1, "session.input"),
            event(2, "session.followup"),
            event(3, "browser.connected"),
            event(4, "session.done"),
        ];
        assert!(!should_append_local_chrome_cloud_promo(
            &browser_connected_after_followup,
            1
        ));
        assert!(!should_append_local_chrome_cloud_promo(
            &browser_connected_after_followup,
            2
        ));

        let already_prompted = vec![
            event(1, "session.input"),
            event(2, "browser.connected"),
            event(3, "session.done"),
            event(4, LOCAL_CHROME_CLOUD_PROMO_EVENT),
        ];
        assert!(!should_append_local_chrome_cloud_promo(
            &already_prompted,
            1
        ));
    }

    #[test]
    fn local_chrome_cloud_promo_appends_on_first_and_every_fifth_browser_connected_initial_success(
    ) -> Result<()> {
        let _guard = ENV_LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .expect("env lock poisoned");
        let saved = std::env::var(BROWSER_USE_CLOUD_API_KEY_ENV).ok();
        unsafe {
            std::env::remove_var(BROWSER_USE_CLOUD_API_KEY_ENV);
        }

        let result = (|| -> Result<()> {
            let temp = tempfile::tempdir()?;
            let store = Store::open(temp.path())?;
            for idx in 1..=6 {
                let session = store.create_session(None, std::env::current_dir()?)?;
                store.append_event(
                    &session.id,
                    "session.input",
                    serde_json::json!({"text": format!("task {idx}")}),
                )?;
                let user_turn_seq = local_chrome_cloud_promo_user_turn_seq(
                    &store,
                    &session.id,
                    BROWSER_LOCAL_CHROME,
                )?;
                assert!(user_turn_seq.is_some(), "expected user turn at task {idx}");
                store.append_event(
                    &session.id,
                    "browser.connected",
                    serde_json::json!({"url": "https://example.com"}),
                )?;
                store.append_event(
                    &session.id,
                    "session.done",
                    serde_json::json!({"result": "done"}),
                )?;

                maybe_append_local_chrome_cloud_promo(
                    &store,
                    &session.id,
                    BROWSER_LOCAL_CHROME,
                    user_turn_seq,
                )?;
                let events = store.events_for_session(&session.id)?;
                let prompted = events
                    .iter()
                    .any(|event| event.event_type == LOCAL_CHROME_CLOUD_PROMO_EVENT);
                assert_eq!(
                    prompted,
                    idx == 1 || idx == 6,
                    "unexpected prompt state at task {idx}"
                );
            }

            let profile_session = store.create_session(None, std::env::current_dir()?)?;
            store.append_event(
                &profile_session.id,
                "session.input",
                serde_json::json!({"text": "use Hacker News"}),
            )?;
            store.append_event(
                &profile_session.id,
                "session.done",
                serde_json::json!({"result": "Which browser profile should I use?"}),
            )?;
            store.append_event(
                &profile_session.id,
                "session.followup",
                serde_json::json!({"text": "1"}),
            )?;
            let profile_user_turn_seq = local_chrome_cloud_promo_user_turn_seq(
                &store,
                &profile_session.id,
                BROWSER_LOCAL_CHROME,
            )?;
            assert!(
                profile_user_turn_seq.is_none(),
                "profile selection follow-up should not qualify"
            );
            store.append_event(
                &profile_session.id,
                "tool.output",
                serde_json::json!({
                    "name": "browser",
                    "ok": true,
                    "text": "{\"connection\":\"connected\"}"
                }),
            )?;
            store.append_event(
                &profile_session.id,
                "session.done",
                serde_json::json!({"result": "done"}),
            )?;
            maybe_append_local_chrome_cloud_promo(
                &store,
                &profile_session.id,
                BROWSER_LOCAL_CHROME,
                profile_user_turn_seq,
            )?;
            assert!(!store
                .events_for_session(&profile_session.id)?
                .iter()
                .any(|event| event.event_type == LOCAL_CHROME_CLOUD_PROMO_EVENT));

            assert_eq!(
                store
                    .get_setting(LOCAL_CHROME_CLOUD_PROMO_QUALIFIED_TASK_COUNT_SETTING)?
                    .as_deref(),
                Some("6")
            );
            Ok(())
        })();

        if let Some(value) = saved {
            unsafe {
                std::env::set_var(BROWSER_USE_CLOUD_API_KEY_ENV, value);
            }
        }
        result
    }

    #[test]
    fn local_chrome_cloud_promo_appends_even_when_cloud_key_is_stored() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let store = Store::open(temp.path())?;
        store.set_setting(BROWSER_USE_CLOUD_API_KEY_SETTING, "bu-test")?;
        let session = store.create_session(None, std::env::current_dir()?)?;
        store.append_event(
            &session.id,
            "session.input",
            serde_json::json!({"text": "research Hacker News"}),
        )?;
        let user_turn_seq =
            local_chrome_cloud_promo_user_turn_seq(&store, &session.id, BROWSER_LOCAL_CHROME)?;
        store.append_event(
            &session.id,
            "browser.connected",
            serde_json::json!({"status": "connected"}),
        )?;
        store.append_event(
            &session.id,
            "session.done",
            serde_json::json!({"result": "done"}),
        )?;

        maybe_append_local_chrome_cloud_promo(
            &store,
            &session.id,
            BROWSER_LOCAL_CHROME,
            user_turn_seq,
        )?;

        assert!(store
            .events_for_session(&session.id)?
            .iter()
            .any(|event| event.event_type == LOCAL_CHROME_CLOUD_PROMO_EVENT));
        Ok(())
    }

    #[test]
    fn tui_agent_options_leaves_provider_id_unset_for_config_resolution() {
        let options = tui_agent_options(
            "Local Chrome",
            "abc123",
            CollaborationModeKind::Default,
            None,
            None,
            None,
            None,
            None,
            None,
            Vec::new(),
        )
        .unwrap();
        assert_eq!(options.browser_mode.as_deref(), Some("local"));
        assert_eq!(options.model_provider_id, None);
    }

    #[test]
    fn tui_agent_options_pass_profile_and_config_overrides_to_core() {
        let config_overrides = browser_use_agent::config_overrides::parse_config_overrides(&[
            "developer_instructions=\"Stay precise.\"".to_string(),
        ])
        .expect("valid config override");
        let options = tui_agent_options(
            "Local Chrome",
            "abc123",
            CollaborationModeKind::Default,
            Some("codex"),
            None,
            None,
            None,
            None,
            Some("work".to_string()),
            config_overrides,
        )
        .unwrap();

        assert_eq!(options.config_profile.as_deref(), Some("work"));
        assert_eq!(
            options
                .config_overrides
                .iter()
                .find(|(key, _)| key == "developer_instructions")
                .and_then(|(_, value)| value.as_str()),
            Some("Stay precise.")
        );
    }

    #[test]
    fn tui_agent_options_apply_runtime_config_layer() {
        let _guard = crate::browser_use_terminal_home_test_lock();
        let temp = tempfile::tempdir().unwrap();
        let previous = std::env::var_os("BROWSER_USE_TERMINAL_HOME");
        unsafe {
            std::env::set_var("BROWSER_USE_TERMINAL_HOME", temp.path());
        }
        std::fs::write(
            temp.path().join("config.toml"),
            r#"
approval_policy = "unless-trusted"
use_guardian = true

[mcp_servers.local]
transport = "stdio"
command = "test-mcp"
"#,
        )
        .unwrap();

        let options = tui_agent_options(
            "Local Chrome",
            "abc123",
            CollaborationModeKind::Default,
            Some("codex"),
            None,
            None,
            None,
            None,
            None,
            Vec::new(),
        )
        .unwrap();

        assert_eq!(options.approval_policy, AskForApproval::UnlessTrusted);
        assert!(options.use_guardian);
        assert!(options.mcp_servers.contains_key("local"));

        unsafe {
            match previous {
                Some(value) => std::env::set_var("BROWSER_USE_TERMINAL_HOME", value),
                None => std::env::remove_var("BROWSER_USE_TERMINAL_HOME"),
            }
        }
    }

    #[test]
    fn tui_child_runner_request_creates_runtime_backed_store_child_session() {
        let temp = tempfile::tempdir().unwrap();
        let state_dir = temp.path().join("state");
        let cwd = temp.path().join("cwd");
        std::fs::create_dir_all(&cwd).unwrap();
        let store = Store::open(&state_dir).unwrap();
        let parent = store.create_session(None, &cwd).unwrap();
        let runtime = tui_runtime_handle(&state_dir).unwrap();
        ensure_tui_agent_attached(
            &runtime,
            &store,
            &parent.id,
            browser_use_agent::config_overrides::DEFAULT_MULTI_AGENT_V2_MAX_CONCURRENT_THREADS_PER_SESSION,
        )
        .unwrap();
        let request = ChildAgentRunRequest {
            parent_session_id: parent.id.clone(),
            child_session_id: "00000000dcba".to_string(),
            run_id: Some("run-1".to_string()),
            message: "Handle the child task".to_string(),
            input_items: None,
            input_is_inter_agent_communication: false,
            agent_path: Some("/root/worker_1".to_string()),
            nickname: Some("Worker".to_string()),
            role: Some("explorer".to_string()),
            fork_turns: Some("all".to_string()),
            model: Some("gpt-test".to_string()),
            reasoning_effort: Some("high".to_string()),
            service_tier: Some("priority".to_string()),
            config_overrides: vec![(
                "model_provider".to_string(),
                toml::Value::String("anthropic".to_string()),
            )],
            completion_handler: None,
        };

        let child = create_tui_child_session_from_request(&runtime, &store, &request).unwrap();
        record_child_run_marker_from_request(&store, &child.id, &request).unwrap();

        assert_eq!(child.id, "00000000dcba");
        assert_eq!(child.parent_id.as_deref(), Some(parent.id.as_str()));
        assert!(runtime
            .agents()
            .thread(&AgentId::from_string(child.id.clone()).unwrap())
            .is_ok());
        let child_events = store.events_for_session(&child.id).unwrap();
        assert!(child_events
            .iter()
            .any(|event| event.event_type == "workspace.context"));
        assert!(child_events
            .iter()
            .any(|event| event.event_type == "session.input"));
        let marker = child_events
            .iter()
            .find(|event| event.event_type == "agent.run.started")
            .expect("run marker");
        assert_eq!(marker.payload["model"], "gpt-test");
        assert_eq!(marker.payload["reasoning_effort"], "high");
        assert_eq!(marker.payload["service_tier"], "priority");
        let model_provider_override = marker.payload["config_overrides"]
            .as_array()
            .and_then(|items| {
                items.iter().find(|item| {
                    item.get("key").and_then(serde_json::Value::as_str) == Some("model_provider")
                })
            })
            .expect("model_provider override");
        assert_eq!(model_provider_override["value"], "anthropic");
        let parent_events = store.events_for_session(&parent.id).unwrap();
        assert!(parent_events
            .iter()
            .any(|event| event.event_type == "agent.spawned"
                && event.payload["child_session_id"] == child.id));
    }

    #[test]
    fn tui_child_completion_updates_runtime_parent_mailbox_without_legacy_handler() {
        let temp = tempfile::tempdir().unwrap();
        let state_dir = temp.path().join("state");
        let cwd = temp.path().join("cwd");
        std::fs::create_dir_all(&cwd).unwrap();
        let store = Store::open(&state_dir).unwrap();
        let parent = store.create_session(None, &cwd).unwrap();
        let runtime = tui_runtime_handle(&state_dir).unwrap();
        ensure_tui_agent_attached(
            &runtime,
            &store,
            &parent.id,
            browser_use_agent::config_overrides::DEFAULT_MULTI_AGENT_V2_MAX_CONCURRENT_THREADS_PER_SESSION,
        )
        .unwrap();
        let request = ChildAgentRunRequest {
            parent_session_id: parent.id.clone(),
            child_session_id: "00000000feed".to_string(),
            run_id: Some("run-2".to_string()),
            message: "Handle the child task".to_string(),
            input_items: None,
            input_is_inter_agent_communication: false,
            agent_path: Some("/root/worker_2".to_string()),
            nickname: None,
            role: None,
            fork_turns: Some("none".to_string()),
            model: None,
            reasoning_effort: None,
            service_tier: None,
            config_overrides: Vec::new(),
            completion_handler: None,
        };
        let child = create_tui_child_session_from_request(&runtime, &store, &request).unwrap();
        store
            .append_event(
                &child.id,
                "session.done",
                serde_json::json!({ "output": "child finished" }),
            )
            .unwrap();

        notify_tui_child_completion(&runtime, &state_dir, &child.id, &request, None);

        let root_agent = runtime
            .agents()
            .thread(&AgentId::from_string(parent.id.clone()).unwrap())
            .unwrap();
        let pending = root_agent.mailbox().pending_items();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].author_agent_id.as_str(), child.id);
        assert!(!pending[0].trigger_turn);
        assert_eq!(
            runtime
                .agents()
                .thread(&AgentId::from_string(child.id.clone()).unwrap())
                .unwrap()
                .snapshot()
                .status,
            browser_use_runtime::AgentThreadStatus::Completed
        );
    }

    #[test]
    fn tui_child_completion_runtime_failure_does_not_call_legacy_handler() {
        let temp = tempfile::tempdir().unwrap();
        let state_dir = temp.path().join("state");
        let cwd = temp.path().join("cwd");
        std::fs::create_dir_all(&cwd).unwrap();
        let store = Store::open(&state_dir).unwrap();
        let parent = store.create_session(None, &cwd).unwrap();
        let runtime = tui_runtime_handle(&state_dir).unwrap();
        ensure_tui_agent_attached(
            &runtime,
            &store,
            &parent.id,
            browser_use_agent::config_overrides::DEFAULT_MULTI_AGENT_V2_MAX_CONCURRENT_THREADS_PER_SESSION,
        )
        .unwrap();
        let handler_called = Arc::new(Mutex::new(false));
        let handler_called_for_callback = Arc::clone(&handler_called);
        let request = ChildAgentRunRequest {
            parent_session_id: parent.id.clone(),
            child_session_id: "00000000beef".to_string(),
            run_id: Some("run-3".to_string()),
            message: "Handle the missing child task".to_string(),
            input_items: None,
            input_is_inter_agent_communication: false,
            agent_path: Some("/root/missing_worker".to_string()),
            nickname: None,
            role: None,
            fork_turns: Some("none".to_string()),
            model: None,
            reasoning_effort: None,
            service_tier: None,
            config_overrides: Vec::new(),
            completion_handler: Some(ChildAgentCompletionHandler::new(move |_| {
                *handler_called_for_callback
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = true;
                Ok(())
            })),
        };

        notify_tui_child_completion(
            &runtime,
            &state_dir,
            &request.child_session_id,
            &request,
            None,
        );

        assert!(
            !*handler_called
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            "runtime-launched TUI child completion must not fall back to the legacy store handler"
        );
    }
}
