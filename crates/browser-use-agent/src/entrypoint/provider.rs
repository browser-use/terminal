//! Provider resolution for the run-entrypoint facade.
//!
//! This turns a binary-facing [`ProviderRunConfig`] (its [`ProviderBackend`] +
//! model) into a concrete model [`Route`] and a ready-to-run
//! [`ModelSamplingDriver`] built via [`build_sampling_driver`] — the production
//! multi-provider path that `turn::model_path` ships.
//!
//! **This is the single place where [`build_sampling_driver`] is actually called
//! from a binary-facing entrypoint.** The real OpenAI / Anthropic / OpenAI-compatible
//! routes are *constructed* here (base-url + auth-header derivation, no network),
//! the transport is built over a [`ModelClient`], and the driver wraps it. No byte
//! goes on the wire until the returned driver's `run_sampling_request` awaits
//! `ModelClient::stream`. That is why a real driver can be CONSTRUCTED offline
//! (the facade tests assert exactly this) even though we have no API key to run it.
//!
//! ## Backend mapping (parity with the legacy provider-selection step)
//! The legacy `browser-use-core` run path picks a provider from the configured
//! backend + the standard env credentials. We mirror that:
//!   * [`ProviderBackend::Openai`]      → [`ProviderChoice::OpenAiResponses`]
//!     (key from `OPENAI_API_KEY` / `LLM_BROWSER_OPENAI_API_KEY`, optional
//!     `LLM_BROWSER_OPENAI_BASE_URL`),
//!   * [`ProviderBackend::BrowserUse`]  → [`ProviderChoice::OpenAiCompatibleCustom`]
//!     id `"browser-use"` (key from `BROWSER_USE_API_KEY`, base override
//!     `LLM_BROWSER_BROWSER_USE_BASE_URL`),
//!   * [`ProviderBackend::Anthropic`]   → [`ProviderChoice::Anthropic`]
//!     (key from `ANTHROPIC_API_KEY` / `LLM_BROWSER_ANTHROPIC_API_KEY`),
//!   * [`ProviderBackend::Openrouter`]  → [`ProviderChoice::OpenAiCompatibleProvider`]
//!     id `"openrouter"` (key from `OPENROUTER_API_KEY`),
//!   * [`ProviderBackend::Deepseek`]    → [`ProviderChoice::OpenAiCompatibleProvider`]
//!     id `"deepseek"` (key from `DEEPSEEK_API_KEY`),
//!   * [`ProviderBackend::Fake`]        → [`ResolvedProvider::Fake`] (no real
//!     provider; the facade drives it with an offline scripted driver),
//!   * [`ProviderBackend::Codex`]       → [`ProviderChoice::Codex`]: the codex
//!     (chatgpt.com) backend, resolved from the Codex CLI OAuth login. The access
//!     token + account id come from env (`CODEX_ACCESS_TOKEN`/`CODEX_ACCOUNT_ID`),
//!     else the credential store (`auth.codex.*`), else `~/.codex/auth.json` (via
//!     [`browser_use_llm::auth::load_codex_auth`]).
//!   * [`ProviderBackend::None`]        → a clear typed error (no provider chosen).
//!
//! ## Credential resolution (env, then store)
//! API keys are resolved env-first, then from the [`Store`] settings the legacy
//! `auth login <provider> --api-key` command writes (`auth.<provider>.api_key`),
//! matching the legacy `stored_or_env` precedence. A missing credential surfaces
//! as [`ProviderResolveError::MissingCredentials`] (honest, never a panic).

use std::collections::{hash_map::DefaultHasher, BTreeMap, BTreeSet, HashMap};
use std::future::Future;
use std::hash::{Hash, Hasher};
use std::pin::Pin;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use browser_use_llm::auth::{load_codex_auth, CodexAuth};
use browser_use_llm::route::ModelClient;
use browser_use_runtime::{
    AgentId as RuntimeAgentId, BrowserConfig as RuntimeBrowserConfig,
    BrowserId as RuntimeBrowserId, Durability as RuntimeDurability,
    MailboxDeliveryPhase as RuntimeMailboxDeliveryPhase, RuntimeHandle,
    SessionId as RuntimeSessionId,
};
use browser_use_store::Store;
use serde::Serialize;

use crate::config_overrides::ProviderBackend;
use crate::config_overrides::ProviderRunConfig;
use crate::config_overrides::{
    ChildAgentCompletionHandler, ChildAgentRunCompletion, ChildAgentRunRequest, ChildAgentRunner,
    ConfigOverrides,
};
use crate::events::EventSink;
use crate::events::PendingEvent;
use crate::events::TurnCtx;
use crate::guardian::approval::GuardianApprover;
use crate::guardian::reviewer::{GuardianReviewer, StaticReviewer};
use crate::guardian::Guardian;
use crate::mcp::McpConnectionManager;
use crate::session::{SessionId, SharedStore};
use crate::subagents::display_agent_path_for_session;
use crate::subagents::mailbox::Mailbox;
use crate::subagents::manager::{
    ChildHandle, ChildSpawner, ChildSpec, ParentContext, SubagentError, SubagentManager,
};
use crate::subagents::parent_link::{update_parent_from_child_run, ChildRunOutcome};
use crate::subagents::registry::AgentRegistry;
use crate::subagents::role::AgentConfigLayer;
use crate::tools::approval::AskForApproval;
use crate::tools::handlers::browser::BrowserBackend;
use crate::tools::handlers::mcp::{McpClient, McpTool};
use crate::tools::handlers::python::PythonBackend;
use crate::tools::orchestrator::{ToolOrchestrator, TurnEnv};
use crate::tools::runtime::ToolCtx;
use crate::tools::sandbox::{FileSystemSandboxPolicy, NoneSandboxProvider};
use crate::tools::UnifiedExecManager;
use crate::turn::dispatch::{RegistryRunner, ToolDispatcher};
use crate::turn::model_path::build_route;
use crate::turn::model_path::build_sampling_driver;
use crate::turn::model_path::build_transport;
use crate::turn::model_path::ModelPathError;
use crate::turn::model_path::ProviderChoice;
use crate::turn::sampling::FusionRecorder;
use crate::turn::sampling::MailboxPreemptionProbe;
use crate::turn::sampling::ModelClientTransport;
use crate::turn::sampling::ModelSamplingDriver;

/// The concrete real-backend sampling driver this facade builds.
///
/// [`build_sampling_driver`] returns the default-runner text-only sampler over a
/// live transport; the entrypoint then attaches FUSED tool dispatch via
/// [`ModelSamplingDriver::with_fusion`], which rebinds the runner type parameter to
/// the production [`RegistryRunner`]. This alias names the fused driver so the
/// entrypoint can hold it without spelling the generics each time. It still
/// implements [`SamplingDriver`](crate::turn::SamplingDriver) (the loop drives the
/// concrete type — the trait is not dyn-compatible, so the driver is held as the
/// concrete generic, not boxed as `dyn SamplingDriver`).
///
/// The runner's approver is the REAL [`GuardianApprover`] (over a permissive
/// [`NoneSandboxProvider`] — OS sandboxing is intentionally NOT enforced here).
/// The active [`AskForApproval`] policy (from the run config) decides whether the
/// approver is consulted at all: `Never` auto-approves without a prompt, any other
/// policy routes each gated call through the guardian review.
pub type RealSamplingDriver = ModelSamplingDriver<
    ModelClientTransport,
    RegistryRunner<NoneSandboxProvider, GuardianApprover>,
>;

/// The production tool dispatcher type: a [`RegistryRunner`] whose approver is the
/// REAL [`GuardianApprover`] (permissive sandbox seam). Named so the builder + the
/// fused driver agree on the runner's generic arguments.
pub type RealToolDispatcher = ToolDispatcher<RegistryRunner<NoneSandboxProvider, GuardianApprover>>;

static UNIFIED_EXEC_MANAGERS: OnceLock<Mutex<HashMap<String, UnifiedExecManager>>> =
    OnceLock::new();

const UNIFIED_EXEC_RESOURCE_KEY: &str = "tools.unified_exec_manager";
const BROWSER_BACKEND_RESOURCE_PREFIX: &str = "tools.browser_backend";
const PYTHON_BACKEND_RESOURCE_PREFIX: &str = "tools.python_backend";
const MCP_CLIENT_RESOURCE_PREFIX: &str = "tools.mcp_client";

struct RuntimeBrowserBackend {
    session_id: String,
    runtime: RuntimeHandle,
    agent_id: RuntimeAgentId,
    browser_id: RuntimeBrowserId,
    backend: Arc<dyn BrowserBackend>,
}

impl RuntimeBrowserBackend {
    fn with_browser_lease<T>(
        &self,
        action: impl FnOnce(&dyn BrowserBackend) -> anyhow::Result<T>,
    ) -> anyhow::Result<T> {
        self.runtime
            .with_browser_action(&self.browser_id, self.agent_id.clone(), || {
                action(self.backend.as_ref())
            })
    }

    fn record_browser_script_response(
        &self,
        session_id: &str,
        response: &browser_use_browser::BrowserScriptOutput,
        synthesize_start: bool,
    ) -> anyhow::Result<()> {
        let Some(run_id) = response
            .run_id
            .as_deref()
            .filter(|run_id| !run_id.trim().is_empty())
        else {
            return Ok(());
        };
        let runtime_session_id = RuntimeSessionId::from_string(session_id.to_string())?;
        let text = if response.text.trim().is_empty()
            && response.ok
            && response.status.as_deref() != Some("running")
            && response.outputs.is_empty()
            && response.summary.is_empty()
            && response.data.is_null()
            && response.images.is_empty()
            && response.artifacts.is_empty()
        {
            "browser_script completed".to_string()
        } else {
            response.text.clone()
        };
        let base_payload = serde_json::json!({
            "name": "browser_script",
            "run_id": run_id,
            "ok": response.ok,
            "status": response.status.clone(),
            "next_observe_ms": response.next_observe_ms,
            "text": text,
            "error": response.error.clone(),
            "data": response.data.clone(),
            "outputs": response.outputs.clone(),
            "summary": response.summary.clone(),
            "images": response.images.clone(),
            "artifacts": response.artifacts.clone(),
            "diagnosis": response.diagnosis.clone(),
        });

        if synthesize_start {
            self.runtime.append_observed_browser_session_event(
                runtime_session_id.clone(),
                self.browser_id.clone(),
                "browser_script.started",
                base_payload.clone(),
                RuntimeDurability::Barrier,
            )?;
        }

        if response.status.as_deref() == Some("running") {
            if !response.text.trim().is_empty() {
                self.runtime.append_observed_browser_session_event(
                    runtime_session_id,
                    self.browser_id.clone(),
                    "browser_script.output_delta",
                    base_payload,
                    RuntimeDurability::BestEffort,
                )?;
            }
            return Ok(());
        }

        let event_type = match response.status.as_deref() {
            Some("cancelled") => "browser_script.cancelled",
            Some("failed") => "browser_script.failed",
            Some("finished") => "browser_script.completed",
            _ if response.ok => "browser_script.completed",
            _ => "browser_script.failed",
        };
        self.runtime.append_observed_browser_session_event(
            runtime_session_id,
            self.browser_id.clone(),
            event_type,
            base_payload,
            RuntimeDurability::Barrier,
        )?;
        Ok(())
    }
}

impl BrowserBackend for RuntimeBrowserBackend {
    fn set_browser_mode(&self, browser_mode: Option<String>) {
        self.backend.set_browser_mode(browser_mode);
    }

    fn command(
        &self,
        session_id: &str,
        cwd: &std::path::Path,
        artifact_dir: &std::path::Path,
        command: &str,
    ) -> anyhow::Result<browser_use_browser::BrowserCommandOutput> {
        self.with_browser_lease(|backend| backend.command(session_id, cwd, artifact_dir, command))
    }

    fn run_script(
        &self,
        session_id: &str,
        cwd: &std::path::Path,
        artifact_dir: &std::path::Path,
        code: &str,
        timeout_secs: u64,
    ) -> anyhow::Result<browser_use_browser::BrowserScriptOutput> {
        self.with_browser_lease(|backend| {
            let output = backend.run_script(session_id, cwd, artifact_dir, code, timeout_secs)?;
            self.record_browser_script_response(session_id, &output, true)?;
            Ok(output)
        })
    }

    fn start_script(
        &self,
        session_id: &str,
        cwd: &std::path::Path,
        artifact_dir: &std::path::Path,
        code: &str,
        timeout_secs: u64,
    ) -> anyhow::Result<browser_use_browser::BrowserScriptOutput> {
        self.with_browser_lease(|backend| {
            let output = backend.start_script(session_id, cwd, artifact_dir, code, timeout_secs)?;
            self.record_browser_script_response(session_id, &output, true)?;
            Ok(output)
        })
    }

    fn observe_script(
        &self,
        session_id: &str,
        run_id: &str,
        observe_timeout_ms: u64,
    ) -> anyhow::Result<browser_use_browser::BrowserScriptOutput> {
        self.with_browser_lease(|backend| {
            let output = backend.observe_script(session_id, run_id, observe_timeout_ms)?;
            self.record_browser_script_response(session_id, &output, false)?;
            Ok(output)
        })
    }

    fn cancel_script(
        &self,
        session_id: &str,
        run_id: &str,
    ) -> anyhow::Result<browser_use_browser::BrowserScriptOutput> {
        self.with_browser_lease(|backend| {
            let output = backend.cancel_script(session_id, run_id)?;
            self.record_browser_script_response(session_id, &output, false)?;
            Ok(output)
        })
    }
}

struct RuntimePythonBackend {
    backend: Arc<dyn PythonBackend>,
}

struct RuntimeMcpClient {
    client: Arc<dyn McpClient>,
    startup_errors: Vec<(String, String)>,
}

fn unified_exec_managers() -> &'static Mutex<HashMap<String, UnifiedExecManager>> {
    UNIFIED_EXEC_MANAGERS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn runtime_resource_key<T: Serialize>(prefix: &str, value: &T) -> String {
    let serialized =
        serde_json::to_string(value).unwrap_or_else(|_| "<unserializable>".to_string());
    let mut hasher = DefaultHasher::new();
    serialized.hash(&mut hasher);
    format!("{prefix}:{:016x}", hasher.finish())
}

fn runtime_session_id_for_agent_session(session_id: &SessionId) -> Option<RuntimeSessionId> {
    RuntimeSessionId::from_string(session_id.as_str()).ok()
}

fn unified_exec_manager_for_session(session_id: Option<&SessionId>) -> UnifiedExecManager {
    let Some(session_id) = session_id else {
        return UnifiedExecManager::default();
    };
    let key = session_id.as_str().to_string();
    let Ok(mut managers) = unified_exec_managers().lock() else {
        return UnifiedExecManager::default();
    };
    managers.entry(key).or_default().clone()
}

fn unified_exec_manager_for_runtime_or_session(
    runtime_handle: Option<&RuntimeHandle>,
    session_id: Option<&SessionId>,
) -> Result<UnifiedExecManager, ProviderResolveError> {
    let Some(session_id) = session_id else {
        return if runtime_handle.is_some() {
            Err(ProviderResolveError::RuntimeResource(
                "runtime-backed exec manager requires an attached session".to_string(),
            ))
        } else {
            Ok(UnifiedExecManager::default())
        };
    };
    if let Some(runtime_handle) = runtime_handle {
        let runtime_session_id =
            RuntimeSessionId::from_string(session_id.as_str()).map_err(|e| {
                ProviderResolveError::RuntimeResource(format!(
                    "invalid runtime session id for exec manager {}: {e}",
                    session_id.as_str()
                ))
            })?;
        let manager = runtime_handle
            .get_or_insert_session_resource(
                &runtime_session_id,
                UNIFIED_EXEC_RESOURCE_KEY,
                UnifiedExecManager::default,
                |manager: Arc<UnifiedExecManager>| manager.terminate_all_best_effort(),
            )
            .map_err(|e| {
                ProviderResolveError::RuntimeResource(format!(
                    "failed to attach runtime exec manager for {}: {e}",
                    session_id.as_str()
                ))
            })?;
        return Ok((*manager).clone());
    }
    Ok(unified_exec_manager_for_session(Some(session_id)))
}

pub fn cleanup_unified_exec_manager_for_session_id(session_id: &str) -> usize {
    let manager = unified_exec_managers()
        .lock()
        .ok()
        .and_then(|mut managers| managers.remove(session_id));
    manager
        .map(|manager| manager.terminate_all_best_effort())
        .unwrap_or(0)
}

pub fn cleanup_all_unified_exec_managers() -> usize {
    let managers = unified_exec_managers()
        .lock()
        .ok()
        .map(|mut managers| {
            managers
                .drain()
                .map(|(_, manager)| manager)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    managers
        .into_iter()
        .map(|manager| manager.terminate_all_best_effort())
        .sum()
}

fn cleanup_session_runtime(
    runtime_handle: Option<RuntimeHandle>,
) -> Arc<dyn Fn(&str) -> usize + Send + Sync> {
    Arc::new(move |session_id| {
        runtime_handle
            .as_ref()
            .and_then(|runtime_handle| {
                RuntimeSessionId::from_string(session_id)
                    .ok()
                    .and_then(|runtime_session_id| {
                        runtime_handle
                            .cleanup_session_resources(&runtime_session_id)
                            .ok()
                    })
            })
            .unwrap_or_else(|| cleanup_unified_exec_manager_for_session_id(session_id))
    })
}

fn browser_backend_for_runtime_or_config(
    config: &ProviderRunConfig,
    runtime_handle: Option<&RuntimeHandle>,
    session_id: Option<&SessionId>,
) -> Result<Arc<dyn BrowserBackend>, ProviderResolveError> {
    let key = runtime_resource_key(
        BROWSER_BACKEND_RESOURCE_PREFIX,
        &config.options.browser_mode.clone(),
    );
    if let Some(handle) = runtime_handle {
        let session_id = session_id.ok_or_else(|| {
            ProviderResolveError::RuntimeResource(
                "runtime-backed browser backend requires an attached session".to_string(),
            )
        })?;
        let runtime_session_id =
            runtime_session_id_for_agent_session(session_id).ok_or_else(|| {
                ProviderResolveError::RuntimeResource(format!(
                    "invalid runtime session id for browser backend {}",
                    session_id.as_str()
                ))
            })?;
        let agent_id = handle
            .agent_id_for_session(&runtime_session_id)
            .map_err(|e| {
                ProviderResolveError::RuntimeResource(format!(
                    "failed to resolve runtime agent for browser backend {}: {e}",
                    session_id.as_str()
                ))
            })?;
        let resource = handle
            .try_get_or_insert_session_resource(
                &runtime_session_id,
                &key,
                || {
                    let browser_id = handle.create_browser_for_agent(
                        agent_id.clone(),
                        RuntimeBrowserConfig {
                            keep_alive: true,
                            headless: None,
                            profile_id: Some(session_id.as_str().to_string()),
                            ..RuntimeBrowserConfig::default()
                        },
                    )?;
                    let browser_registries = handle.browser_physical_registries(&browser_id)?;
                    Ok(RuntimeBrowserBackend {
                        session_id: session_id.as_str().to_string(),
                        runtime: handle.clone(),
                        agent_id,
                        browser_id,
                        backend: Arc::new(
                            crate::tools::handlers::browser::RealBackend::with_browser_mode_and_registries(
                                config.options.browser_mode.clone(),
                                browser_registries.session_registry(),
                                browser_registries.script_registry(),
                            ),
                        ),
                    })
                },
                |resource: Arc<RuntimeBrowserBackend>| {
                    let cleaned = resource.backend.cleanup_session(&resource.session_id);
                    let _ = resource
                        .runtime
                        .close_browser_for_agent(&resource.browser_id, &resource.agent_id);
                    cleaned
                },
            )
            .map_err(|e| {
                ProviderResolveError::RuntimeResource(format!(
                    "failed to attach runtime browser backend for {}: {e}",
                    session_id.as_str()
                ))
            })?;
        let backend: Arc<dyn BrowserBackend> = resource;
        return Ok(backend);
    }

    Ok(Arc::new(
        crate::tools::handlers::browser::RealBackend::with_browser_mode(
            config.options.browser_mode.clone(),
        ),
    ))
}

fn python_backend_for_runtime_or_config(
    config: &ProviderRunConfig,
    runtime_handle: Option<&RuntimeHandle>,
    session_id: Option<&SessionId>,
) -> Result<Arc<dyn PythonBackend>, ProviderResolveError> {
    let key = runtime_resource_key(
        PYTHON_BACKEND_RESOURCE_PREFIX,
        &(
            config.options.browser_mode.clone(),
            config.options.python_env.clone(),
        ),
    );
    if let (Some(handle), Some(session_id)) = (runtime_handle, session_id) {
        if let Some(runtime_session_id) = runtime_session_id_for_agent_session(session_id) {
            let resource = handle
                .try_get_or_insert_session_resource(
                    &runtime_session_id,
                    &key,
                    || {
                        start_python_backend(config)
                            .map(|backend| RuntimePythonBackend { backend })
                            .map_err(anyhow::Error::from)
                    },
                    |_| 1,
                )
                .map_err(|e| ProviderResolveError::PythonWorker(e.to_string()))?;
            return Ok(Arc::clone(&resource.backend));
        }
        return Err(ProviderResolveError::RuntimeResource(format!(
            "invalid runtime session id for python backend {}",
            session_id.as_str()
        )));
    } else if runtime_handle.is_some() {
        return Err(ProviderResolveError::RuntimeResource(
            "runtime-backed python backend requires an attached session".to_string(),
        ));
    }

    start_python_backend(config)
}

fn stable_mcp_servers(
    servers: &HashMap<String, crate::mcp::McpServerConfig>,
) -> BTreeMap<String, crate::mcp::McpServerConfig> {
    servers
        .iter()
        .map(|(name, config)| (name.clone(), config.clone()))
        .collect()
}

fn mcp_client_for_runtime_or_config(
    config: &ProviderRunConfig,
    runtime_handle: Option<&RuntimeHandle>,
    session_id: Option<&SessionId>,
) -> anyhow::Result<Arc<RuntimeMcpClient>> {
    let stable_servers = stable_mcp_servers(&config.options.mcp_servers);
    let key = runtime_resource_key(MCP_CLIENT_RESOURCE_PREFIX, &stable_servers);
    if let (Some(handle), Some(session_id)) = (runtime_handle, session_id) {
        if let Some(runtime_session_id) = runtime_session_id_for_agent_session(session_id) {
            return handle.try_get_or_insert_session_resource(
                &runtime_session_id,
                &key,
                || runtime_mcp_client_from_config(config.options.mcp_servers.clone()),
                |_| 1,
            );
        }
        anyhow::bail!(
            "invalid runtime session id for mcp client {}",
            session_id.as_str()
        );
    } else if runtime_handle.is_some() {
        anyhow::bail!("runtime-backed mcp client requires an attached session");
    }

    Ok(Arc::new(runtime_mcp_client_from_config(
        config.options.mcp_servers.clone(),
    )?))
}

fn runtime_mcp_client_from_config(
    servers: HashMap<String, crate::mcp::McpServerConfig>,
) -> anyhow::Result<RuntimeMcpClient> {
    let (manager, errors) = McpConnectionManager::connect_all(servers)?;
    let client: Arc<dyn McpClient> = Arc::new(manager);
    Ok(RuntimeMcpClient {
        client,
        startup_errors: errors
            .into_iter()
            .map(|(server, err)| (server, err.to_string()))
            .collect(),
    })
}

fn mailbox_preemption_probe(
    user_input: &Option<(SharedStore, SessionId)>,
    runtime_handle: Option<&RuntimeHandle>,
) -> Option<MailboxPreemptionProbe> {
    let (store, session_id) = user_input.as_ref()?;
    let store = Arc::clone(store);
    let session_id = session_id.as_str().to_string();
    let runtime_handle = runtime_handle.cloned();
    let probe: MailboxPreemptionProbe = Arc::new(
        move || -> Pin<Box<dyn Future<Output = bool> + Send + 'static>> {
            let store = Arc::clone(&store);
            let session_id = session_id.clone();
            let runtime_handle = runtime_handle.clone();
            Box::pin(async move {
                if let Some(runtime_handle) = runtime_handle {
                    let Ok(runtime_session_id) = RuntimeSessionId::from_string(session_id.clone())
                    else {
                        return false;
                    };
                    return runtime_handle
                        .has_pending_agent_mail_for_session(
                            &runtime_session_id,
                            RuntimeMailboxDeliveryPhase::CurrentTurn,
                        )
                        .unwrap_or(false);
                }
                let _ = (store, session_id);
                false
            })
        },
    );
    Some(probe)
}

#[cfg(test)]
struct NoopEventSink;

#[cfg(test)]
impl EventSink for NoopEventSink {
    fn emit(&self, _ev: PendingEvent) {}
}

/// Errors resolving a provider into a driver.
#[derive(Debug)]
pub enum ProviderResolveError {
    /// The configured backend has no real provider in this engine.
    ///
    /// Carries a human-readable reason. `None` means no backend was selected.
    UnsupportedBackend(String),
    /// No usable credential was found (env or store) for the chosen backend.
    MissingCredentials(&'static str),
    /// The model route could not be built (e.g. an unknown OpenAI-compatible
    /// provider id). Wraps the real [`ModelPathError`].
    Route(ModelPathError),
    /// The codex (chatgpt.com) login state exists but could not be resolved
    /// (e.g. a malformed `~/.codex/auth.json`). Carries the underlying message
    /// (never the token/file contents — the codex reader keeps secrets out of its
    /// error strings).
    Codex(String),
    /// The Python worker subprocess could not be started for the run.
    ///
    /// The legacy run path starts ONE [`PythonWorker`] per run (eager, via
    /// `start_with_browser_mode_and_env`) and threads it through dispatch; if
    /// that spawn fails we surface it as a typed error rather than silently
    /// dropping the `python` tool (which would be a hidden regression). Carries
    /// the underlying error's message.
    ///
    /// [`PythonWorker`]: browser_use_python_worker::PythonWorker
    PythonWorker(String),
    /// A runtime-backed provider could not attach a live resource to the runtime
    /// session resource bag. Runtime-owned runs must fail here rather than
    /// continuing with orphan exec/browser/MCP/Python resources.
    RuntimeResource(String),
}

impl std::fmt::Display for ProviderResolveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProviderResolveError::UnsupportedBackend(why) => {
                write!(f, "unsupported provider backend: {why}")
            }
            ProviderResolveError::MissingCredentials(which) => {
                write!(f, "no provider credentials found in environment ({which})")
            }
            ProviderResolveError::Route(e) => write!(f, "{e}"),
            ProviderResolveError::Codex(why) => {
                write!(f, "failed to resolve codex login: {why}")
            }
            ProviderResolveError::PythonWorker(why) => {
                write!(f, "failed to start python worker: {why}")
            }
            ProviderResolveError::RuntimeResource(why) => {
                write!(f, "failed to attach runtime resource: {why}")
            }
        }
    }
}

impl std::error::Error for ProviderResolveError {}

impl From<ModelPathError> for ProviderResolveError {
    fn from(e: ModelPathError) -> Self {
        ProviderResolveError::Route(e)
    }
}

/// What provider resolution produced for a config.
///
/// A real backend yields a built [`RealSamplingDriver`] (the live model path,
/// via [`build_sampling_driver`]); the `Fake` backend yields
/// [`ResolvedProvider::Fake`], the signal the entrypoint uses to drive the turn
/// with an offline scripted driver instead.
pub enum ResolvedProvider {
    /// A live model driver, built via [`build_sampling_driver`].
    Real(Box<RealSamplingDriver>),
    /// The fake/test backend: no real provider; caller drives offline.
    Fake,
}

// `ModelSamplingDriver` is not `Debug`, so derive is impossible. A by-hand impl
// lets callers `expect_err`/assert on a `Result<ResolvedProvider, _>` without
// leaking driver internals (it prints only the variant tag).
impl std::fmt::Debug for ResolvedProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ResolvedProvider::Real(_) => f.write_str("ResolvedProvider::Real(..)"),
            ResolvedProvider::Fake => f.write_str("ResolvedProvider::Fake"),
        }
    }
}

/// First non-empty env var among `keys`.
fn env_first(keys: &[&str]) -> Option<String> {
    keys.iter()
        .find_map(|k| std::env::var(k).ok().filter(|v| !v.trim().is_empty()))
}

/// Read a non-empty value from the [`Store`] settings, ignoring read errors
/// (a store read failure should not block an otherwise-resolvable env credential;
/// it degrades to "no stored value").
fn store_first(store: Option<&Store>, key: &str) -> Option<String> {
    store?
        .get_setting(key)
        .ok()
        .flatten()
        .filter(|v| !v.trim().is_empty())
}

/// Resolve a provider API key env-first, then from the [`Store`] settings the
/// legacy `auth login <provider> --api-key` command writes (`auth.<provider>.api_key`).
///
/// Precedence matches the legacy `stored_or_env`: env wins, store is the fallback.
fn key_env_then_store(
    env_keys: &[&str],
    store: Option<&Store>,
    store_provider: &str,
) -> Option<String> {
    env_first(env_keys).or_else(|| store_first(store, &format!("auth.{store_provider}.api_key")))
}

/// Map a [`ProviderBackend`] to a model-path [`ProviderChoice`], resolving the
/// backend's credentials env-first, then from the [`Store`] settings.
///
/// `store` is the (optional) credential store: when an env key is absent, the
/// stored `auth.<provider>.api_key` (and codex tokens) are consulted, matching the
/// legacy `auth login` write path. Pass `None` to resolve from env only.
///
/// Returns:
///   * `Ok(Some(choice))` for a real provider with credentials present,
///   * `Ok(None)` for [`ProviderBackend::Fake`] (no real provider),
///   * `Err(..)` for an absent backend or missing credentials.
///
/// Only file I/O is the codex `~/.codex/auth.json` fallback (and the store read);
/// no network I/O — this only assembles a [`ProviderChoice`].
pub fn provider_choice_for_backend(
    backend: ProviderBackend,
    store: Option<&Store>,
) -> Result<Option<ProviderChoice>, ProviderResolveError> {
    match backend {
        ProviderBackend::Openai => {
            let api_key = key_env_then_store(
                &["LLM_BROWSER_OPENAI_API_KEY", "OPENAI_API_KEY"],
                store,
                "openai",
            )
            .ok_or(ProviderResolveError::MissingCredentials(
                "set OPENAI_API_KEY (or run `auth login openai`) for the openai backend",
            ))?;
            Ok(Some(ProviderChoice::OpenAiResponses {
                api_key,
                base_url: env_first(&["LLM_BROWSER_OPENAI_BASE_URL"]),
            }))
        }
        ProviderBackend::BrowserUse => {
            let api_key = key_env_then_store(
                &["LLM_BROWSER_BROWSER_USE_API_KEY", "BROWSER_USE_API_KEY"],
                store,
                "browser_use_cloud",
            )
            .ok_or(ProviderResolveError::MissingCredentials(
                "set BROWSER_USE_API_KEY (or run `auth login browser-use-cloud`) for the browser-use backend",
            ))?;
            Ok(Some(ProviderChoice::OpenAiCompatibleCustom {
                provider_id: "browser-use".to_string(),
                base_url: env_first(&["LLM_BROWSER_BROWSER_USE_BASE_URL"])
                    .unwrap_or_else(|| "https://llm.api.browser-use.com/v1".to_string()),
                api_key,
                extra_headers: vec![(
                    "x-browser-use-request-type".to_string(),
                    env_first(&["LLM_BROWSER_BROWSER_USE_REQUEST_TYPE"])
                        .unwrap_or_else(|| "rust_agent".to_string()),
                )],
            }))
        }
        ProviderBackend::Anthropic => {
            let api_key = key_env_then_store(
                &["LLM_BROWSER_ANTHROPIC_API_KEY", "ANTHROPIC_API_KEY"],
                store,
                "anthropic",
            )
            .ok_or(ProviderResolveError::MissingCredentials(
                "set ANTHROPIC_API_KEY (or run `auth login anthropic`) for the anthropic backend",
            ))?;
            Ok(Some(ProviderChoice::Anthropic {
                api_key,
                base_url: env_first(&["LLM_BROWSER_ANTHROPIC_BASE_URL"]),
            }))
        }
        ProviderBackend::Openrouter => {
            let api_key = key_env_then_store(
                &["OPENROUTER_API_KEY", "LLM_BROWSER_OPENAI_COMPAT_API_KEY"],
                store,
                "openrouter",
            )
            .ok_or(ProviderResolveError::MissingCredentials(
                "set OPENROUTER_API_KEY (or run `auth login openrouter`) for the openrouter backend",
            ))?;
            Ok(Some(ProviderChoice::OpenAiCompatibleProvider {
                provider_id: "openrouter".to_string(),
                api_key,
            }))
        }
        ProviderBackend::Deepseek => {
            let api_key = key_env_then_store(
                &["DEEPSEEK_API_KEY", "LLM_BROWSER_OPENAI_COMPAT_API_KEY"],
                store,
                "deepseek",
            )
            .ok_or(ProviderResolveError::MissingCredentials(
                "set DEEPSEEK_API_KEY (or run `auth login deepseek`) for the deepseek backend",
            ))?;
            Ok(Some(ProviderChoice::OpenAiCompatibleProvider {
                provider_id: "deepseek".to_string(),
                api_key,
            }))
        }
        ProviderBackend::Fake => Ok(None),
        // Codex (chatgpt.com) login: resolve the OAuth access token + account id
        // from env, then the store, then `~/.codex/auth.json`.
        ProviderBackend::Codex => Ok(Some(resolve_codex_choice(store)?)),
        ProviderBackend::None => Err(ProviderResolveError::UnsupportedBackend(
            "no provider backend selected".to_string(),
        )),
    }
}

/// Resolve the codex (chatgpt.com) [`ProviderChoice`] from, in precedence order:
///   1. env `CODEX_ACCESS_TOKEN` + `CODEX_ACCOUNT_ID`,
///   2. the credential store (`auth.codex.access_token` + `auth.codex.account_id`),
///   3. the on-disk Codex CLI login `~/.codex/auth.json` (via [`load_codex_auth`]).
///
/// The base url honours `CODEX_BASE_URL` (env), else the stored `auth.codex.base_url`,
/// else the chatgpt.com default baked into the route builder.
fn resolve_codex_choice(store: Option<&Store>) -> Result<ProviderChoice, ProviderResolveError> {
    let base_url =
        env_first(&["CODEX_BASE_URL"]).or_else(|| store_first(store, "auth.codex.base_url"));

    // (1) explicit env token + account.
    if let (Some(access_token), Some(account_id)) = (
        env_first(&["CODEX_ACCESS_TOKEN"]),
        env_first(&["CODEX_ACCOUNT_ID"]),
    ) {
        return Ok(ProviderChoice::Codex {
            access_token,
            account_id,
            base_url,
        });
    }

    // (2) store-resolved token + account.
    if let (Some(access_token), Some(account_id)) = (
        store_first(store, "auth.codex.access_token"),
        store_first(store, "auth.codex.account_id"),
    ) {
        return Ok(ProviderChoice::Codex {
            access_token,
            account_id,
            base_url,
        });
    }

    // (3) on-disk Codex CLI login (`~/.codex/auth.json`).
    match load_codex_auth() {
        Ok(Some(CodexAuth {
            access_token,
            account_id,
        })) => Ok(ProviderChoice::Codex {
            access_token,
            account_id,
            base_url,
        }),
        // No login present anywhere → honest missing-credentials error.
        Ok(None) => Err(ProviderResolveError::MissingCredentials(
            "no codex login found: run `auth import-codex` or log in with the Codex CLI \
             (~/.codex/auth.json), or set CODEX_ACCESS_TOKEN + CODEX_ACCOUNT_ID",
        )),
        // A present-but-malformed auth.json is a typed route/resolution error.
        Err(e) => Err(ProviderResolveError::Codex(e.to_string())),
    }
}

/// Resolve a [`ProviderRunConfig`] into a ready sampling driver (or the Fake
/// signal). **This is the production call site for [`build_sampling_driver`].**
///
/// Steps (parity with the legacy provider-selection path):
/// 1. [`provider_choice_for_backend`] maps the backend → a credentialed
///    [`ProviderChoice`] (`Codex`/`None` → typed error, `Fake` → `None`).
/// 2. For a real choice: [`build_route`] derives the endpoint + auth (offline),
///    [`build_transport`] binds a fresh [`ModelClient`] + the per-turn request,
///    and [`build_sampling_driver`] wraps it into the live [`ModelSamplingDriver`].
/// 3. `Fake` short-circuits to [`ResolvedProvider::Fake`].
///
/// `store` is the (optional) credential store threaded through for env-then-store
/// key resolution (the caller's `SharedStore`); pass `None` to resolve from env
/// only. `sink` receives the driver's UI events; `ctx` carries the turn's
/// model/provider identity; `max_retries` is the codex-style stream retry budget;
/// `recorder` is the [`FusionRecorder`] the fused driver records the assistant
/// message + dispatched tool outputs through (it must point at the SAME
/// conversation buffer the loop's `TurnState` re-samples from). No network I/O
/// happens here.
pub fn resolve_provider(
    config: &ProviderRunConfig,
    store: Option<&Store>,
    sink: Arc<dyn EventSink>,
    ctx: TurnCtx,
    max_retries: u32,
    recorder: Arc<dyn FusionRecorder>,
    user_input: Option<(SharedStore, SessionId)>,
) -> Result<ResolvedProvider, ProviderResolveError> {
    let tool_cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    resolve_provider_with_tool_cwd(
        config,
        store,
        sink,
        ctx,
        max_retries,
        recorder,
        user_input,
        tool_cwd,
        None,
    )
}

#[allow(clippy::too_many_arguments)]
pub fn resolve_provider_with_tool_cwd(
    config: &ProviderRunConfig,
    store: Option<&Store>,
    sink: Arc<dyn EventSink>,
    ctx: TurnCtx,
    max_retries: u32,
    recorder: Arc<dyn FusionRecorder>,
    user_input: Option<(SharedStore, SessionId)>,
    tool_cwd: std::path::PathBuf,
    runtime_handle: Option<RuntimeHandle>,
) -> Result<ResolvedProvider, ProviderResolveError> {
    resolve_provider_with_tool_paths(
        config,
        store,
        sink,
        ctx,
        max_retries,
        recorder,
        user_input,
        tool_cwd.clone(),
        tool_cwd,
        runtime_handle,
    )
}

#[allow(clippy::too_many_arguments)]
pub fn resolve_provider_with_tool_paths(
    config: &ProviderRunConfig,
    store: Option<&Store>,
    sink: Arc<dyn EventSink>,
    ctx: TurnCtx,
    max_retries: u32,
    recorder: Arc<dyn FusionRecorder>,
    user_input: Option<(SharedStore, SessionId)>,
    tool_cwd: std::path::PathBuf,
    tool_artifact_root: std::path::PathBuf,
    runtime_handle: Option<RuntimeHandle>,
) -> Result<ResolvedProvider, ProviderResolveError> {
    // The Fake short-circuit lives in the inner builder (so we never spawn a
    // Python worker for a fake/cut/missing-credential run). For a real backend we
    // start the run's single Python worker EAGERLY here, then thread its backend
    // through. `start_python_backend` only runs once we know the route builds.
    //
    // `user_input` is the (SharedStore, SessionId) used by store-backed runtime
    // tools for this session. It is Some on the live run path and None for tests /
    // headless callers.
    resolve_provider_with_python(
        config,
        store,
        sink,
        ctx,
        max_retries,
        recorder,
        None,
        user_input,
        tool_cwd,
        tool_artifact_root,
        runtime_handle,
    )
}

/// Start the run's single Python worker subprocess (eager, matching legacy
/// `run_existing_session_from_config`, which spawns one
/// `PythonWorker::start_with_browser_mode_and_env` per run and threads it through
/// dispatch).
///
/// `browser_mode` + `python_env` come from the run config's [`AgentRunOptions`],
/// forwarded verbatim. A spawn failure is a typed [`ProviderResolveError::PythonWorker`]
/// (no silent drop of the `python` tool — that would be a hidden regression).
///
/// LIFECYCLE: the returned backend owns the [`PythonWorker`], which is reaped on
/// drop — `PythonWorker`'s `Drop` (python-worker `lib.rs:475`) sends a `shutdown`
/// request then force-kills + waits the child. The backend is held by the
/// `python` handler inside the dispatcher; when the dispatcher (owned by the
/// fused driver) drops at run end, the worker process is reaped — no leak.
///
/// [`AgentRunOptions`]: crate::config_overrides::AgentRunOptions
/// [`PythonWorker`]: browser_use_python_worker::PythonWorker
fn start_python_backend(
    config: &ProviderRunConfig,
) -> Result<Arc<dyn PythonBackend>, ProviderResolveError> {
    let backend = crate::tools::handlers::python::RealBackend::start(
        config.options.browser_mode.as_deref(),
        &config.options.python_env,
    )
    .map_err(|e| ProviderResolveError::PythonWorker(e.to_string()))?;
    Ok(Arc::new(backend))
}

/// Inner [`resolve_provider`] that accepts a pre-built Python backend.
///
/// `python_backend = None` means "start the real worker eagerly" (the production
/// path). Tests pass `Some(fake)` to exercise resolution WITHOUT spawning a real
/// Python process — the real-driver-constructs-offline assertion is about the
/// model transport, not the worker, so injecting a fake keeps it network/process
/// free while still wiring the `python` tool through the real dispatcher.
#[allow(clippy::too_many_arguments)]
fn resolve_provider_with_python(
    config: &ProviderRunConfig,
    store: Option<&Store>,
    sink: Arc<dyn EventSink>,
    ctx: TurnCtx,
    max_retries: u32,
    recorder: Arc<dyn FusionRecorder>,
    python_backend: Option<Arc<dyn PythonBackend>>,
    user_input: Option<(SharedStore, SessionId)>,
    tool_cwd: std::path::PathBuf,
    tool_artifact_root: std::path::PathBuf,
    runtime_handle: Option<RuntimeHandle>,
) -> Result<ResolvedProvider, ProviderResolveError> {
    // (1) backend → credentialed provider choice (env-then-store creds; codex from
    //     env/store/~/.codex; None → Err; Fake → None).
    let choice = match provider_choice_for_backend(config.backend, store)? {
        Some(choice) => choice,
        None => return Ok(ResolvedProvider::Fake),
    };

    // (2) build the real route (offline: URL + auth headers only).
    let route = build_route(&choice, &config.model)?;

    // (3) build the live transport over a fresh ModelClient, then the driver.
    //     The transport owns the per-turn request; we seed it with an empty
    //     message vec — the turn loop threads the real prompt through
    //     `run_sampling_request`, which rebuilds the request per attempt from
    //     `ctx` + the loop's input (the shape `build_transport` documents).
    let mut client = ModelClient::default();
    if let Some(timeout_ms) = config.options.model_stream_idle_timeout_ms {
        let timeout = Duration::from_millis(timeout_ms.max(1));
        client = client.with_stream_idle_timeout(timeout);
    }
    let client = Arc::new(client);
    let transport = build_transport(client, route, &ctx, Vec::new());

    // (3a) Resolve the Python backend for the run's `python` tool. Real path:
    //      start the single worker eagerly (only reached AFTER the `Fake`/`Codex`/
    //      missing-credential exits above, so those never spawn Python). Tests
    //      inject a fake.
    let python_backend = match python_backend {
        Some(backend) => backend,
        None => python_backend_for_runtime_or_config(
            config,
            runtime_handle.as_ref(),
            user_input.as_ref().map(|(_, session_id)| session_id),
        )?,
    };

    // *** build_sampling_driver is actually CALLED here (production path). ***
    // It yields the text-only sampler; we then attach the FUSED dispatch path so a
    // model tool-call actually EXECUTES (through the registry + orchestrator) and
    // its output re-enters the prompt via `recorder`, and the loop re-samples.
    let goal_store = build_goal_store(&user_input);
    let goals_enabled = goal_runtime_enabled(config, &user_input);
    let preemption_probe = mailbox_preemption_probe(&user_input, runtime_handle.as_ref());
    let mut driver = build_sampling_driver(transport, Arc::clone(&sink), ctx, max_retries)
        .with_full_llm_input_events(config.options.full_llm_input_events);
    if goals_enabled {
        driver = driver.with_goal_store(goal_store.clone());
    }
    let dispatcher = build_tool_dispatcher_with_cwd_and_goal_store(
        python_backend,
        config,
        user_input,
        tool_cwd,
        tool_artifact_root,
        sink,
        goal_store,
        runtime_handle,
    );
    let mut driver = driver.with_fusion(dispatcher?, recorder);
    if let Some(probe) = preemption_probe {
        driver = driver.with_mailbox_preemption_probe(probe);
    }
    Ok(ResolvedProvider::Real(Box::new(driver)))
}

/// Build the production fused tool dispatcher: a [`ToolRegistry`] behind the REAL
/// [`RegistryRunner`], over a REAL [`ToolOrchestrator`] (permissive
/// [`NoneSandboxProvider`] + a live [`GuardianApprover`]), wrapped in a
/// [`ToolDispatcher`].
///
/// This is the dispatcher the fused [`ModelSamplingDriver`] runs every model
/// tool-call through (codex `try_run_turn` -> router -> orchestrator). The runner
/// dispatches BY NAME through the registry, deserializing the call's `input` into
/// the matching handler's typed `Req` and running it under the orchestrator's
/// approval/sandbox policy, then renders the [`ExecOutput`](crate::tools::ExecOutput)
/// into the recorded tool-result message.
///
/// ## Which tools are wired here
/// The registry registers the backend-free handlers — `shell`, `apply_patch`,
/// `view_image`, `update_plan`, `done`, `tool_search` (catalog populated from the registered tools' defs),
/// `web_search` (ENABLED; the Responses builder encodes it as the hosted
/// `web_search_preview` tool), `search` (a locally-executed DuckDuckGo search,
/// distinct from the hosted `web_search`) — plus the two product-surface tools
/// that drive real subsystems:
///   * `browser` ([`BrowserTool::new`]): standalone — the production
///     [`RealBackend`](crate::tools::handlers::browser::RealBackend) wraps the
///     `browser-use-browser` crate and manages CDP sessions internally (keyed by
///     `session_id`), so no external handle is threaded in. Registered
///     `parallel_safe = false` (a single CDP connection).
///   * `python` ([`PythonTool::with_backend`]): backed by the `python_backend`
///     this builder receives — a [`RealBackend`](crate::tools::handlers::python::RealBackend)
///     wrapping the ONE [`PythonWorker`] [`resolve_provider`] started for the run
///     (eager, matching legacy). Registered `parallel_safe = false` (a single
///     interpreter process).
///
/// `mcp` ([`McpTool::new`] takes an
/// [`McpClient`](crate::tools::handlers::mcp::McpClient)) is registered ONLY when
/// the run config supplies one or more `mcp_servers`: this builder connects them
/// via [`McpConnectionManager::connect_all`] (per-server failure isolation) and
/// registers the `mcp` tool over the resulting manager. An EMPTY `mcp_servers`
/// map (the default) registers nothing, preserving prior behavior — a model call
/// to `mcp` then returns the registry's "unknown tool" tool-result.
///
/// [`BrowserTool::new`]: crate::tools::handlers::browser::BrowserTool::new
/// [`PythonTool::with_backend`]: crate::tools::handlers::python::PythonTool::with_backend
/// [`McpTool::new`]: crate::tools::handlers::mcp::McpTool::new
/// [`PythonWorker`]: browser_use_python_worker::PythonWorker
///
/// ## Approval / guardian wiring (LIVE)
/// - **Approval policy** is sourced from `config.options.approval_policy` (default
///   [`AskForApproval::Never`], preserving prior non-interactive behavior) and
///   threaded into the [`RegistryRunner`]. `Never` auto-approves (no prompt); any
///   other policy routes each gated call through the REAL [`GuardianApprover`],
///   which can deny.
/// - **Orchestrator** is [`build_real_orchestrator`] over a permissive
///   [`NoneSandboxProvider`] (OS-level sandbox enforcement is intentionally
///   SKIPPED — a permissive seam) + a live [`GuardianApprover`]. The guardian's
///   reviewer is selected by `config.options.use_guardian` (deny vs. allow).
/// - **`ToolCtx.cwd`** is the process cwd (best-effort); the per-call id/name are
///   placeholders for this headless dispatch path.
/// - **`supports_parallel_tool_calls = true`**: lets the registry's own per-tool
///   `parallel_safe` flag drive the parallel/serial gate (the conservative tools
///   are registered serial, so this is safe).
#[cfg(test)]
fn build_tool_dispatcher(
    python_backend: Arc<dyn PythonBackend>,
    config: &ProviderRunConfig,
    user_input: Option<(SharedStore, SessionId)>,
) -> Arc<RealToolDispatcher> {
    let tool_cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    build_tool_dispatcher_with_cwd(
        python_backend,
        config,
        user_input,
        tool_cwd.clone(),
        tool_cwd,
        Arc::new(NoopEventSink),
    )
}

#[cfg(test)]
fn build_tool_dispatcher_with_cwd(
    python_backend: Arc<dyn PythonBackend>,
    config: &ProviderRunConfig,
    user_input: Option<(SharedStore, SessionId)>,
    tool_cwd: std::path::PathBuf,
    tool_artifact_root: std::path::PathBuf,
    event_sink: Arc<dyn EventSink>,
) -> Arc<RealToolDispatcher> {
    let goal_store = build_goal_store(&user_input);
    build_tool_dispatcher_with_cwd_and_goal_store(
        python_backend,
        config,
        user_input,
        tool_cwd,
        tool_artifact_root,
        event_sink,
        goal_store,
        None,
    )
    .expect("test dispatcher should build")
}

fn build_tool_dispatcher_with_cwd_and_goal_store(
    python_backend: Arc<dyn PythonBackend>,
    config: &ProviderRunConfig,
    user_input: Option<(SharedStore, SessionId)>,
    tool_cwd: std::path::PathBuf,
    tool_artifact_root: std::path::PathBuf,
    event_sink: Arc<dyn EventSink>,
    goal_store: Arc<crate::tools::handlers::goal::GoalStore>,
    runtime_handle: Option<RuntimeHandle>,
) -> Result<Arc<RealToolDispatcher>, ProviderResolveError> {
    use crate::tools::handlers::apply_patch::{ApplyPatchRequest, ApplyPatchTool};
    use crate::tools::handlers::browser::{BrowserRequest, BrowserTool};
    use crate::tools::handlers::capture::{CaptureCurationRequest, CaptureCurationTool};
    use crate::tools::handlers::done::{DoneRequest, DoneTool};
    use crate::tools::handlers::mcp::McpToolCallRequest;
    use crate::tools::handlers::python::{PythonRequest, PythonTool};
    use crate::tools::handlers::search::{SearchRequest, SearchTool};
    use crate::tools::handlers::shell::{
        ExecCommandRequest, ExecCommandTool, ShellRequest, ShellTool, WriteStdinRequest,
        WriteStdinTool,
    };
    use crate::tools::handlers::tool_search::{ToolSearchEntry, ToolSearchRequest, ToolSearchTool};
    use crate::tools::handlers::update_plan::{UpdatePlanRequest, UpdatePlanTool};
    use crate::tools::handlers::view_image::{ViewImageRequest, ViewImageTool};
    use crate::tools::handlers::web_search::{WebSearchConfig, WebSearchRequest, WebSearchTool};
    use crate::tools::registry::{definitions, ToolRegistry};

    // The backend-free handlers, each with its parity-grounded definition + static
    // parallel_safe flag (matching `default_registry`'s presets), plus the
    // browser/python product tools. `mcp` is still absent (handled separately).
    // Typed with the production approver (`GuardianApprover`) so the registry, the
    // orchestrator, and the runner all agree on the `(S, A)` seams.
    let mut reg: ToolRegistry<NoneSandboxProvider, GuardianApprover> = ToolRegistry::new();
    let unified_exec = unified_exec_manager_for_runtime_or_session(
        runtime_handle.as_ref(),
        user_input.as_ref().map(|(_, session_id)| session_id),
    )?;
    let unified_exec_emitter = user_input.as_ref().map(|(_, session_id)| {
        Arc::new(crate::tools::UnifiedExecEventEmitter::new(
            Arc::clone(&event_sink),
            session_id.as_str().to_string(),
        ))
    });
    let shell_tool = match &unified_exec_emitter {
        Some(emitter) => {
            ShellTool::with_manager(unified_exec.clone()).with_event_emitter(Arc::clone(emitter))
        }
        None => ShellTool::with_manager(unified_exec.clone()),
    };
    let exec_command_tool = match &unified_exec_emitter {
        Some(emitter) => {
            ExecCommandTool::new(unified_exec.clone()).with_event_emitter(Arc::clone(emitter))
        }
        None => ExecCommandTool::new(unified_exec.clone()),
    };
    let write_stdin_tool = match &unified_exec_emitter {
        Some(emitter) => {
            WriteStdinTool::new(unified_exec.clone()).with_event_emitter(Arc::clone(emitter))
        }
        None => WriteStdinTool::new(unified_exec.clone()),
    };
    reg.register::<_, ShellRequest>("shell", definitions::shell(), false, shell_tool);
    reg.register::<_, ExecCommandRequest>(
        "exec_command",
        definitions::exec_command(),
        true,
        exec_command_tool,
    );
    reg.register::<_, WriteStdinRequest>(
        "write_stdin",
        definitions::write_stdin(),
        false,
        write_stdin_tool,
    );
    reg.register::<_, ApplyPatchRequest>(
        "apply_patch",
        definitions::apply_patch(),
        false,
        ApplyPatchTool::new(),
    );
    reg.register::<_, ViewImageRequest>(
        "view_image",
        definitions::view_image(),
        false,
        ViewImageTool::new(),
    );
    reg.register::<_, UpdatePlanRequest>(
        "update_plan",
        definitions::update_plan(),
        false,
        UpdatePlanTool::new(),
    );
    // `web_search` is ENABLED (hosted/provider-side). The OpenAI Responses
    // request builder encodes it as the hosted `{"type":"web_search_preview"}`
    // tool (see `browser-use-llm` `openai_responses.rs::lower_tool`).
    reg.register::<_, WebSearchRequest>(
        "web_search",
        definitions::web_search(),
        true,
        WebSearchTool::new(WebSearchConfig::enabled()),
    );
    // `search`: locally-executed DuckDuckGo (Lite) web search — the client runs
    // the HTTP request and parses the results itself (distinct from the hosted
    // `web_search` above). Read-only, so parallel_safe = true.
    reg.register::<_, SearchRequest>("search", definitions::search(), true, SearchTool::new());
    let browser_backend = browser_backend_for_runtime_or_config(
        config,
        runtime_handle.as_ref(),
        user_input.as_ref().map(|(_, session_id)| session_id),
    )?;
    let browser_tool = BrowserTool::with_backend(browser_backend)
        .with_selected_browser_mode(config.options.browser_mode.clone())
        .with_selected_browser_profile_id(config.options.browser_profile_id.clone())
        .with_selected_local_browser(config.options.browser_local_browser.clone())
        .with_default_script_timeout_secs(config.options.python_tool_timeout_seconds);
    let browser_tool = match &user_input {
        Some((store, session_id)) => {
            let tool = browser_tool
                .with_session_id(session_id.as_str().to_string())
                .with_persistence(store.clone(), session_id.as_str().to_string());
            if config.options.dynamic_browser_mode_from_store {
                tool.with_dynamic_browser_mode_from_store(true)
            } else {
                tool
            }
        }
        None => browser_tool,
    };
    // `browser`: standalone production backend (`browser-use-browser`, internal
    // session management). parallel_safe = false (single CDP connection).
    reg.register::<_, BrowserRequest>(
        "browser",
        definitions::browser(),
        false,
        browser_tool.clone(),
    );
    // `browser_script`: browser-use's page/data-plane surface. It routes through
    // the same handler, but the schema omits the internal session id and matches
    // the prompt contract used by current-main browser tasks.
    reg.register::<_, BrowserRequest>(
        "browser_script",
        definitions::browser_script(),
        false,
        browser_tool,
    );
    // Temporarily disable the agent-driven GIF curation pipeline. The
    // deterministic post-run fallback recording remains active.
    const AGENT_DRIVEN_GIF_CURATION_ENABLED: bool = false;
    if AGENT_DRIVEN_GIF_CURATION_ENABLED {
        if let Some((store, session_id)) = &user_input {
            reg.register::<_, CaptureCurationRequest>(
                "submit_capture_curation",
                definitions::submit_capture_curation(),
                false,
                CaptureCurationTool::with_store(store.clone(), session_id.as_str().to_string()),
            );
        }
    }
    // `python`: backed by the run's single PythonWorker (started eagerly by
    // `resolve_provider`). parallel_safe = false (single interpreter process).
    reg.register::<_, PythonRequest>(
        "python",
        definitions::python(),
        false,
        PythonTool::with_backend(python_backend),
    );
    // `done`: the completion tool the model calls to declare it has finished, with
    // its final summary. Serial (terminal; must not be reordered).
    reg.register::<_, DoneRequest>("done", definitions::done(), false, DoneTool::new());

    // Codex-style collaboration exposure: v2 is gated by
    // `features.multi_agent_v2`, but only advertise the subagent tools when this
    // run actually has a child runner. Otherwise the model can burn turns on an
    // unsupported tool that can never succeed.
    if subagent_tools_enabled_for_run(config) {
        register_subagent_tools(
            &mut reg,
            config,
            &user_input,
            &tool_cwd,
            runtime_handle.clone(),
        );
    } else if legacy_subagent_tools_enabled_for_run(config) {
        register_legacy_subagent_tools(
            &mut reg,
            config,
            &user_input,
            &tool_cwd,
            runtime_handle.clone(),
        );
    }

    // Goal tools (`get_goal` / `create_goal` / `update_goal`). All three share ONE
    // `GoalStore` (the event-sourced `GoalManager` + its durable `goal.*` event
    // sink), registered behind the same registry seam so a `create_goal` is
    // visible to a later `get_goal`/`update_goal`. Codex only exposes these for
    // persisted, non-plan, non-review turns; mirror that gate here.
    if goal_runtime_enabled(config, &user_input) {
        register_goal_tools(&mut reg, goal_store);
    }

    // `mcp`: register the MCP bridge ONLY when servers are configured. An empty
    // `mcp_servers` map (the default) registers nothing, preserving prior
    // behavior. Non-empty => connect all servers (per-server failure isolation
    // inside `connect_all`) and register the single `mcp` tool over the resulting
    // manager (which implements `McpClient`). Registered `parallel_safe = false`;
    // the handler's per-request read-only hint still drives its own gate.
    if !config.options.mcp_servers.is_empty() {
        let resource = mcp_client_for_runtime_or_config(
            config,
            runtime_handle.as_ref(),
            user_input.as_ref().map(|(_, session_id)| session_id),
        )
        .map_err(|e| ProviderResolveError::RuntimeResource(e.to_string()))?;
        for (server, err) in &resource.startup_errors {
            eprintln!("warning: MCP server '{server}' failed to connect: {err}");
        }
        reg.register::<_, McpToolCallRequest>(
            "mcp",
            definitions::mcp(),
            false,
            McpTool::new(Arc::clone(&resource.client)),
        );
    }

    apply_role_tool_policy(&mut reg, config);

    // `tool_search` catalog: populate it from the registry's model-visible
    // definitions so the model can discover the registered tools by free-text
    // query (legacy `deferred_tool_search_entries`; codex's `ToolSearchInfo`
    // catalog). We use the registered tools' definitions (name + description +
    // schema property names) as the searchable catalog — the obvious in-crate
    // source. (When a deferred MCP / dynamic-tool source lands, it extends this
    // catalog; for now the registered tools are the catalog. See REPORT.) We
    // register tool_search LAST so the catalog reflects every other tool, then
    // mirror the same entries as the registry's deferred set.
    let catalog: Vec<ToolSearchEntry> = reg
        .model_visible_definitions()
        .iter()
        .map(|def| {
            let props: Vec<String> = def
                .input_schema
                .get("properties")
                .and_then(|p| p.as_object())
                .map(|m| m.keys().cloned().collect())
                .unwrap_or_default();
            ToolSearchEntry::new(def.name.clone(), def.description.clone(), props)
        })
        .collect();
    reg.set_deferred_search_entries(catalog.clone());
    reg.register::<_, ToolSearchRequest>(
        "tool_search",
        definitions::tool_search(),
        true,
        ToolSearchTool::new(catalog),
    );
    apply_role_tool_policy(&mut reg, config);

    // Capture the model-visible tool definitions BEFORE `reg` is moved into the
    // runner's Arc, so the dispatcher can carry them to the sampling driver (which
    // sets `LlmRequest::tools` from them). Same Vec the registry advertises
    // (name-sorted, order-stable) — without this the model receives no tool
    // definitions and can never emit browser/python/shell tool calls.
    let specs = reg.model_visible_definitions();

    // The REAL approval policy, sourced from the run config (default `Never`,
    // which preserves prior non-interactive behavior).
    let policy: AskForApproval = config.options.approval_policy;
    // The REAL orchestrator: permissive `NoneSandboxProvider` (OS sandboxing is
    // intentionally skipped) + a live `GuardianApprover`. The approver is only
    // consulted when the policy routes to it (any non-`Never` policy on a gated
    // call); under `Never` the orchestrator bypasses the prompt entirely.
    let orchestrator = Arc::new(build_real_orchestrator(config.options.use_guardian));

    let runner = RegistryRunner::new(
        Arc::new(reg),
        orchestrator,
        // Per-turn ctx/env. The cwd is the durable session cwd supplied by the
        // caller; falling back to the process cwd only in test/headless helper paths.
        ToolCtx {
            call_id: user_input
                .as_ref()
                .map(|(_, session_id)| session_id.as_str().to_string())
                .unwrap_or_default(),
            tool_name: String::new(),
            cwd: tool_cwd,
            artifact_root: tool_artifact_root,
        },
        TurnEnv {
            file_system_sandbox_policy: FileSystemSandboxPolicy {
                restricted: false,
                denied_read: false,
            },
            managed_network_active: false,
            strict_auto_review: false,
            // Mirror the run config's guardian toggle for parity.
            use_guardian: config.options.use_guardian,
        },
        policy,
    );

    Ok(Arc::new(ToolDispatcher::with_runner_and_specs(
        runner, /* supports_parallel_tool_calls */ true, specs,
    )))
}

fn subagent_tools_enabled_for_run(config: &ProviderRunConfig) -> bool {
    config.options.multi_agent_v2.enabled && config.options.child_agent_runner.is_some()
}

fn legacy_subagent_tools_enabled_for_run(config: &ProviderRunConfig) -> bool {
    !config.options.multi_agent_v2.enabled
        && config.options.collab_enabled
        && config.options.child_agent_runner.is_some()
}

fn apply_role_tool_policy<S, A>(
    reg: &mut crate::tools::registry::ToolRegistry<S, A>,
    config: &ProviderRunConfig,
) where
    S: crate::tools::sandbox::SandboxProvider,
    A: crate::tools::runtime::Approver,
{
    let Some(policy) = ToolPolicy::from_config_overrides(&config.options.config_overrides) else {
        return;
    };
    reg.retain_registered_tools(|namespace, name| policy.allows(namespace, name));
}

struct ToolPolicy {
    allowlist: Option<BTreeSet<String>>,
    can_write: Option<bool>,
}

impl ToolPolicy {
    fn from_config_overrides(overrides: &[(String, toml::Value)]) -> Option<Self> {
        let allowlist = overrides
            .iter()
            .rev()
            .find(|(key, _)| key == "tool_allowlist")
            .and_then(|(_, value)| value.as_array())
            .map(|values| {
                values
                    .iter()
                    .filter_map(|value| value.as_str())
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .map(ToOwned::to_owned)
                    .collect::<BTreeSet<_>>()
            })
            .filter(|set| !set.is_empty());
        let can_write = overrides
            .iter()
            .rev()
            .find(|(key, _)| key == "can_write")
            .and_then(|(_, value)| value.as_bool());
        if allowlist.is_none() && can_write.is_none() {
            None
        } else {
            Some(Self {
                allowlist,
                can_write,
            })
        }
    }

    fn allows(&self, namespace: Option<&str>, name: &str) -> bool {
        if let Some(allowlist) = &self.allowlist {
            let display_name = tool_display_name(namespace, name);
            if !allowlist.contains(name) && !allowlist.contains(&display_name) {
                return false;
            }
        }
        if self.can_write == Some(false) && is_write_capable_tool(name) {
            return false;
        }
        true
    }
}

fn tool_display_name(namespace: Option<&str>, name: &str) -> String {
    match namespace {
        Some(namespace) => {
            let mut display = String::with_capacity(namespace.len() + name.len());
            display.push_str(namespace);
            display.push_str(name);
            display
        }
        None => name.to_string(),
    }
}

fn is_write_capable_tool(name: &str) -> bool {
    matches!(name, "shell" | "apply_patch" | "python")
}

/// Build the production tool orchestrator: a permissive [`NoneSandboxProvider`]
/// (OS-level sandbox enforcement is intentionally SKIPPED — this is a permissive
/// seam) paired with a live [`GuardianApprover`].
///
/// `use_guardian` selects the guardian's reviewer:
///   * `false` → [`StaticReviewer::allow`] — the permissive default. Under a
///     non-`Never` policy a gated call is reviewed and ALLOWED (the approver IS
///     consulted — the routing is live — it just permits).
///   * `true`  → [`StaticReviewer::deny`] — fail-closed. Under a non-`Never`
///     policy a gated call is reviewed and DENIED, proving the policy routes to a
///     real approver that can block.
///
/// Under [`AskForApproval::Never`] the orchestrator's pure decision table bypasses
/// the approval gate entirely, so the reviewer is never consulted regardless of
/// this flag — preserving the prior auto-approve behavior.
fn build_real_orchestrator(
    use_guardian: bool,
) -> ToolOrchestrator<NoneSandboxProvider, GuardianApprover> {
    let reviewer: Arc<dyn GuardianReviewer> = if use_guardian {
        Arc::new(StaticReviewer::deny(
            "guardian denied: non-interactive run with the guardian gate enabled",
        ))
    } else {
        Arc::new(StaticReviewer::allow())
    };
    let approver = GuardianApprover::new(Guardian::new(reviewer));
    ToolOrchestrator::new(NoneSandboxProvider, approver)
}

/// Bridges the run config's [`ChildAgentRunner`] (a `Fn(ChildAgentRunRequest) ->
/// Result<()>` that *launches* a child) into the subagents [`ChildSpawner`] seam.
///
/// The legacy `child_agent_runner` is fire-and-forget: it spawns the child run
/// (inheriting the parent's provider/model via the request fields) and returns.
/// The subagents [`SubagentManager`] tracks the child's lifecycle through the
/// registry + mailbox, so this adapter maps the [`ChildSpec`] onto a
/// [`ChildAgentRunRequest`], invokes the runner, and returns a [`ChildHandle`]
/// for the just-launched child. A runner error becomes a [`SubagentError`].
struct ChildAgentRunnerSpawner {
    runner: ChildAgentRunner,
    parent_session_id: String,
    parent_link: Arc<Mutex<Option<ChildAgentRunnerParentLink>>>,
    store: Option<SharedStore>,
    parent_run_config: ProviderRunConfig,
    runtime_authoritative: bool,
}

#[derive(Clone)]
struct ChildAgentRunnerParentLink {
    registry: Arc<AgentRegistry>,
    mailbox: Arc<Mailbox>,
}

#[async_trait::async_trait]
impl ChildSpawner for ChildAgentRunnerSpawner {
    async fn spawn_child(&self, spec: ChildSpec) -> Result<ChildHandle, SubagentError> {
        let manager_completion_handler = if self.runtime_authoritative {
            None
        } else {
            self.parent_link
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone()
                .map(|parent_link| {
                    let child_path = spec.agent_path.clone();
                    ChildAgentCompletionHandler::new(move |completion: ChildAgentRunCompletion| {
                        let outcome = if completion.success {
                            ChildRunOutcome::success(completion.summary)
                        } else {
                            ChildRunOutcome::failure(
                                completion
                                    .summary
                                    .unwrap_or_else(|| "child agent failed".to_string()),
                            )
                        };
                        let _ = update_parent_from_child_run(
                            &parent_link.registry,
                            &parent_link.mailbox,
                            &child_path,
                            &outcome,
                        );
                        Ok(())
                    })
                })
        };
        let store_completion_handler = if self.runtime_authoritative {
            None
        } else {
            self.store.as_ref().map(|store| {
                let run_id = Some(spec.run_id.clone());
                crate::tools::handlers::subagent::store_completion_handler(
                    Arc::clone(store),
                    self.parent_session_id.clone(),
                    spec.agent_id.clone(),
                    run_id,
                )
            })
        };
        let completion_handler = match (manager_completion_handler, store_completion_handler) {
            (None, None) => None,
            (Some(handler), None) | (None, Some(handler)) => Some(handler),
            (Some(manager_handler), Some(store_handler)) => {
                Some(ChildAgentCompletionHandler::new(move |completion| {
                    manager_handler.notify(completion.clone())?;
                    store_handler.notify(completion)
                }))
            }
        };
        let request = ChildAgentRunRequest {
            parent_session_id: self.parent_session_id.clone(),
            // The child's session id is its canonical agent id (unique per spawn).
            child_session_id: spec.agent_id.clone(),
            run_id: Some(spec.run_id.clone()),
            message: spec.message.clone(),
            input_items: spec.input_items.clone(),
            input_is_inter_agent_communication: spec.input_is_inter_agent_communication,
            agent_path: Some(spec.agent_path.clone()),
            nickname: spec.nickname.clone(),
            role: spec.role.clone(),
            fork_turns: spec.fork_turns.clone(),
            // Child inherits the resolved config (provider/tier folded into the
            // role layer); surface the model + reasoning/tier overrides the
            // legacy runner consumes.
            model: Some(spec.config.model.clone()),
            reasoning_effort: spec.config.reasoning_effort.clone(),
            service_tier: spec.config.service_tier.clone(),
            config_overrides: child_run_config_overrides(&self.parent_run_config, &spec.config),
            completion_handler,
        };
        self.runner
            .run(request)
            .map_err(|e| SubagentError(format!("child_agent_runner failed: {e}")))?;
        Ok(ChildHandle {
            agent_path: spec.agent_path,
            agent_id: spec.agent_id,
        })
    }
}

/// A [`ChildSpawner`] that always errors: the fallback when the run config
/// supplies no `child_agent_runner`. Spawning then returns an honest "subagents
/// not configured" error rather than the tools being silently absent — the model
/// still SEES `spawn_agent` (so it can attempt delegation) but gets a clear
/// failure when no runner is wired (e.g. headless/test runs).
struct UnconfiguredChildSpawner;

#[async_trait::async_trait]
impl ChildSpawner for UnconfiguredChildSpawner {
    async fn spawn_child(&self, _spec: ChildSpec) -> Result<ChildHandle, SubagentError> {
        Err(SubagentError(
            "subagents are not configured for this run (no child_agent_runner)".to_string(),
        ))
    }
}

/// A durable [`EventSink`] over a [`SharedStore`]: appends each `subagent.*`
/// lifecycle event under the shared lock so the TUI's subagent render sees the
/// transition. Best-effort (append errors are swallowed, matching
/// [`EventSink::emit`]'s infallible contract).
struct SubagentStoreSink {
    store: SharedStore,
}

impl EventSink for SubagentStoreSink {
    fn emit(&self, ev: PendingEvent) {
        if let Ok(store) = self.store.lock() {
            let _ = store.append_event(&ev.session_id, &ev.event_type, ev.payload);
        }
    }
}

/// Runtime-backed lifecycle sink for live subagent events.
///
/// The event payloads stay byte-compatible with the Store projection, but the
/// append and publish go through `BrowserUseRuntime`, so active TUI/SDK
/// subscribers see the same lifecycle facts that SQLite records.
struct SubagentRuntimeSink {
    runtime: RuntimeHandle,
}

impl EventSink for SubagentRuntimeSink {
    fn emit(&self, ev: PendingEvent) {
        let Ok(session_id) = RuntimeSessionId::from_string(ev.session_id) else {
            return;
        };
        let _ = self.runtime.append_observed_session_event(
            session_id,
            &ev.event_type,
            ev.payload,
            RuntimeDurability::Barrier,
        );
    }
}

/// A no-op [`EventSink`] for runs without a session store (tests / headless):
/// lifecycle events are dropped, but spawn/wait/send still function.
struct NoopSubagentSink;

impl EventSink for NoopSubagentSink {
    fn emit(&self, _ev: PendingEvent) {}
}

/// Register the subagent orchestration tools into `reg`, all sharing ONE
/// [`SubagentManager`].
///
/// The manager's [`ChildSpawner`] is bridged from
/// `config.options.child_agent_runner` via [`ChildAgentRunnerSpawner`]; spawned
/// children therefore inherit the parent's provider/model from whatever the
/// entrypoint wired into that runner. When the run config carries no runner,
/// [`UnconfiguredChildSpawner`] is used so a spawn attempt fails honestly.
/// Lifecycle events are persisted through the durable session journal when a
/// session store is available, else dropped via [`NoopSubagentSink`]
/// (tests/headless). Live mailbox/send/wait/close behavior is runtime-backed;
/// the store sink is the debug/replay projection.
fn register_subagent_tools<S, A>(
    reg: &mut crate::tools::registry::ToolRegistry<S, A>,
    config: &ProviderRunConfig,
    user_input: &Option<(SharedStore, SessionId)>,
    tool_cwd: &std::path::Path,
    runtime_handle: Option<RuntimeHandle>,
) where
    S: crate::tools::sandbox::SandboxProvider,
    A: crate::tools::runtime::Approver,
{
    use crate::subagents::spawn::SpawnAgentArgs;
    use crate::tools::handlers::subagent::{
        CloseAgentRequest, CloseAgentTool, FollowupTaskRequest, FollowupTaskTool,
        ListAgentsRequest, ListAgentsTool, SendMessageRequest, SendMessageTool, SpawnAgentTool,
        SubagentToolDeps, WaitAgentRequest, WaitAgentTimeoutOptions, WaitAgentTool,
    };
    use crate::tools::registry::definitions::{
        self, SpawnAgentDefinitionOptions, WaitAgentDefinitionOptions,
    };

    let parent_session_id = user_input
        .as_ref()
        .map(|(_, sid)| sid.as_str().to_string())
        .unwrap_or_default();

    // The child-runner seam (parent's provider/model inheritance) or an honest
    // error fallback when no runner is configured.
    let parent_link = Arc::new(Mutex::new(None));
    let spawner: Arc<dyn ChildSpawner> = match &config.options.child_agent_runner {
        Some(runner) => Arc::new(ChildAgentRunnerSpawner {
            runner: runner.clone(),
            parent_session_id,
            parent_link: Arc::clone(&parent_link),
            store: user_input.as_ref().map(|(store, _)| Arc::clone(store)),
            parent_run_config: config.clone(),
            runtime_authoritative: runtime_handle.is_some(),
        }),
        None => Arc::new(UnconfiguredChildSpawner),
    };
    let max_concurrent_threads_per_session = config
        .options
        .multi_agent_v2
        .max_concurrent_threads_per_session;
    let manager = Arc::new(SubagentManager::with_config_and_limits(
        spawner,
        crate::subagents::role::RoleRegistry::with_user_defined(config.options.agent_roles.clone()),
        crate::subagents::depth::DEFAULT_AGENT_MAX_DEPTH,
        Some(max_concurrent_threads_per_session),
    ));
    let spawn_gate = Arc::new(tokio::sync::Mutex::new(()));
    *parent_link
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(ChildAgentRunnerParentLink {
        registry: manager.registry(),
        mailbox: manager.mailbox(),
    });

    // The parent context the children hang off. On the live runtime path, use
    // the current session's durable agent path so nested spawns resolve beneath
    // the child, not always beneath `/root`.
    let parent_agent_path =
        parent_agent_path_from_store(user_input).unwrap_or_else(|| "/root".to_string());
    let parent = ParentContext {
        depth: agent_path_depth(&parent_agent_path),
        agent_path: parent_agent_path,
        base_config: parent_agent_config_layer(config, tool_cwd),
    };

    // Durable lifecycle sink + session scope: journal projection on the live
    // run path, a no-op when no session store is wired.
    let (sink, session_id): (Arc<dyn EventSink>, String) =
        match (runtime_handle.clone(), user_input) {
            (Some(runtime), Some((_, sid))) => (
                Arc::new(SubagentRuntimeSink { runtime }),
                sid.as_str().to_string(),
            ),
            (None, Some((store, sid))) => (
                Arc::new(SubagentStoreSink {
                    store: store.clone(),
                }),
                sid.as_str().to_string(),
            ),
            (Some(_), None) | (None, None) => (Arc::new(NoopSubagentSink), String::new()),
        };
    let is_spawned_subagent = user_input
        .as_ref()
        .is_some_and(|(store, sid)| session_is_spawned_subagent_for_tools(store, sid.as_str()));

    let deps = SubagentToolDeps {
        manager,
        parent,
        sink,
        session_id,
        store: user_input.as_ref().map(|(store, _)| Arc::clone(store)),
        child_runner: config.options.child_agent_runner.clone(),
        cleanup_session_runtime: Some(cleanup_session_runtime(runtime_handle.clone())),
        runtime_handle: runtime_handle.clone(),
        spawn_gate,
        wait_timeouts: WaitAgentTimeoutOptions {
            default_timeout_ms: config.options.multi_agent_v2.default_wait_timeout_ms,
            min_timeout_ms: config.options.multi_agent_v2.min_wait_timeout_ms,
            max_timeout_ms: config.options.multi_agent_v2.max_wait_timeout_ms,
        },
        hide_spawn_agent_metadata: config.options.multi_agent_v2.hide_spawn_agent_metadata,
        max_concurrent_threads_per_session: Some(max_concurrent_threads_per_session),
    };
    let tool_namespace = if provider_supports_tool_namespaces(config) {
        config.options.multi_agent_v2.tool_namespace.clone()
    } else {
        None
    };
    let namespace_description = tool_namespace
        .as_ref()
        .map(|_| "Tools for spawning and managing sub-agents.".to_string());
    let namespace_definition = |mut definition: browser_use_llm::schema::ToolDefinition| {
        definition.namespace = tool_namespace.clone();
        definition.namespace_description = namespace_description.clone();
        definition
    };

    reg.register::<_, SpawnAgentArgs>(
        "spawn_agent",
        namespace_definition(definitions::spawn_agent_with_options(
            SpawnAgentDefinitionOptions {
                agent_type_description: agent_type_description(&config.options.agent_roles),
                available_models_description: Some(spawn_agent_available_models_description(
                    config, tool_cwd,
                )),
                hide_agent_type_model_reasoning: config
                    .options
                    .multi_agent_v2
                    .hide_spawn_agent_metadata,
                include_usage_hint: config.options.multi_agent_v2.usage_hint_enabled,
                usage_hint_text: config.options.multi_agent_v2.usage_hint_text.clone(),
                max_concurrent_threads_per_session: Some(
                    config
                        .options
                        .multi_agent_v2
                        .max_concurrent_threads_per_session,
                ),
                is_spawned_subagent,
            },
        )),
        false,
        SpawnAgentTool::new(deps.clone()),
    );
    reg.register::<_, WaitAgentRequest>(
        "wait_agent",
        namespace_definition(definitions::wait_agent_with_timeouts(
            WaitAgentDefinitionOptions {
                default_timeout_ms: config.options.multi_agent_v2.default_wait_timeout_ms,
                min_timeout_ms: config.options.multi_agent_v2.min_wait_timeout_ms,
                max_timeout_ms: config.options.multi_agent_v2.max_wait_timeout_ms,
            },
        )),
        false,
        WaitAgentTool::new(deps.clone()),
    );
    // send_message: queue-only Codex v2 message delivery.
    reg.register::<_, SendMessageRequest>(
        "send_message",
        namespace_definition(definitions::send_message()),
        false,
        SendMessageTool::new(deps.clone()),
    );
    // followup_task: queue + trigger the target's next turn.
    reg.register::<_, FollowupTaskRequest>(
        "followup_task",
        namespace_definition(definitions::followup_task()),
        false,
        FollowupTaskTool::new(deps.clone()),
    );
    reg.register::<_, ListAgentsRequest>(
        "list_agents",
        namespace_definition(definitions::list_agents()),
        false,
        ListAgentsTool::new(deps.clone()),
    );
    // close_agent: marks a spawned agent subtree closed in this control plane.
    reg.register::<_, CloseAgentRequest>(
        "close_agent",
        namespace_definition(definitions::close_agent()),
        false,
        CloseAgentTool::new(deps),
    );
}

fn register_legacy_subagent_tools<S, A>(
    reg: &mut crate::tools::registry::ToolRegistry<S, A>,
    config: &ProviderRunConfig,
    user_input: &Option<(SharedStore, SessionId)>,
    tool_cwd: &std::path::Path,
    runtime_handle: Option<RuntimeHandle>,
) where
    S: crate::tools::sandbox::SandboxProvider,
    A: crate::tools::runtime::Approver,
{
    use crate::tools::handlers::subagent::{
        CloseAgentTool, CloseAgentV1Request, ResumeAgentRequest, ResumeAgentTool, SendInputRequest,
        SendInputTool, SpawnAgentV1Request, SpawnAgentV1Tool, SubagentToolDeps,
        WaitAgentTimeoutOptions, WaitAgentV1Request, WaitAgentV1Tool,
    };
    use crate::tools::registry::definitions::{
        self, SpawnAgentDefinitionOptions, WaitAgentDefinitionOptions,
    };

    let parent_session_id = user_input
        .as_ref()
        .map(|(_, sid)| sid.as_str().to_string())
        .unwrap_or_default();
    let parent_link = Arc::new(Mutex::new(None));
    let spawner: Arc<dyn ChildSpawner> = match &config.options.child_agent_runner {
        Some(runner) => Arc::new(ChildAgentRunnerSpawner {
            runner: runner.clone(),
            parent_session_id,
            parent_link: Arc::clone(&parent_link),
            store: user_input.as_ref().map(|(store, _)| Arc::clone(store)),
            parent_run_config: config.clone(),
            runtime_authoritative: runtime_handle.is_some(),
        }),
        None => Arc::new(UnconfiguredChildSpawner),
    };
    let max_concurrent_threads_per_session = config
        .options
        .multi_agent_v2
        .max_concurrent_threads_per_session;
    let manager = Arc::new(SubagentManager::with_config_and_limits(
        spawner,
        crate::subagents::role::RoleRegistry::with_user_defined(config.options.agent_roles.clone()),
        crate::subagents::depth::DEFAULT_AGENT_MAX_DEPTH,
        Some(max_concurrent_threads_per_session),
    ));
    let spawn_gate = Arc::new(tokio::sync::Mutex::new(()));
    *parent_link
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(ChildAgentRunnerParentLink {
        registry: manager.registry(),
        mailbox: manager.mailbox(),
    });

    let parent_agent_path =
        parent_agent_path_from_store(user_input).unwrap_or_else(|| "/root".to_string());
    let parent = ParentContext {
        depth: agent_path_depth(&parent_agent_path),
        agent_path: parent_agent_path,
        base_config: parent_agent_config_layer(config, tool_cwd),
    };
    let (sink, session_id): (Arc<dyn EventSink>, String) = match user_input {
        Some((store, sid)) => (
            Arc::new(SubagentStoreSink {
                store: store.clone(),
            }),
            sid.as_str().to_string(),
        ),
        None => (Arc::new(NoopSubagentSink), String::new()),
    };
    let deps = SubagentToolDeps {
        manager,
        parent,
        sink,
        session_id,
        store: user_input.as_ref().map(|(store, _)| Arc::clone(store)),
        child_runner: config.options.child_agent_runner.clone(),
        cleanup_session_runtime: Some(cleanup_session_runtime(runtime_handle.clone())),
        runtime_handle: runtime_handle.clone(),
        spawn_gate,
        wait_timeouts: WaitAgentTimeoutOptions {
            default_timeout_ms: config.options.multi_agent_v2.default_wait_timeout_ms,
            min_timeout_ms: config.options.multi_agent_v2.min_wait_timeout_ms,
            max_timeout_ms: config.options.multi_agent_v2.max_wait_timeout_ms,
        },
        hide_spawn_agent_metadata: config.options.multi_agent_v2.hide_spawn_agent_metadata,
        max_concurrent_threads_per_session: Some(max_concurrent_threads_per_session),
    };

    let spawn_options = SpawnAgentDefinitionOptions {
        agent_type_description: agent_type_description(&config.options.agent_roles),
        available_models_description: Some(spawn_agent_available_models_description(
            config, tool_cwd,
        )),
        hide_agent_type_model_reasoning: config.options.multi_agent_v2.hide_spawn_agent_metadata,
        include_usage_hint: config.options.multi_agent_v2.usage_hint_enabled,
        usage_hint_text: config.options.multi_agent_v2.usage_hint_text.clone(),
        max_concurrent_threads_per_session: Some(
            config
                .options
                .multi_agent_v2
                .max_concurrent_threads_per_session,
        ),
        is_spawned_subagent: user_input
            .as_ref()
            .is_some_and(|(store, sid)| session_is_spawned_subagent_for_tools(store, sid.as_str())),
    };
    let wait_options = WaitAgentDefinitionOptions {
        default_timeout_ms: WaitAgentTimeoutOptions::default().default_timeout_ms,
        min_timeout_ms: WaitAgentTimeoutOptions::default().min_timeout_ms,
        max_timeout_ms: WaitAgentTimeoutOptions::default().max_timeout_ms,
    };
    reg.register::<_, SpawnAgentV1Request>(
        "spawn_agent",
        definitions::spawn_agent_v1_with_options(spawn_options),
        false,
        SpawnAgentV1Tool::new(deps.clone()),
    );
    reg.register::<_, SendInputRequest>(
        "send_input",
        definitions::send_input(),
        false,
        SendInputTool::new(deps.clone()),
    );
    reg.register::<_, ResumeAgentRequest>(
        "resume_agent",
        definitions::resume_agent(),
        false,
        ResumeAgentTool::new(deps.clone()),
    );
    reg.register::<_, WaitAgentV1Request>(
        "wait_agent",
        definitions::wait_agent_v1_with_timeouts(wait_options),
        false,
        WaitAgentV1Tool::new(deps.clone()),
    );
    reg.register::<_, CloseAgentV1Request>(
        "close_agent",
        definitions::close_agent_v1(),
        false,
        CloseAgentTool::new_legacy(deps),
    );
}

fn provider_supports_tool_namespaces(config: &ProviderRunConfig) -> bool {
    matches!(
        config.backend,
        ProviderBackend::Openai | ProviderBackend::Codex
    )
}

fn agent_type_description(
    user_roles: &std::collections::BTreeMap<String, crate::subagents::role::AgentRoleConfig>,
) -> String {
    use std::collections::BTreeSet;

    let built_in_roles = crate::subagents::role::built_in_roles();
    let mut seen = BTreeSet::new();
    let mut formatted_roles = Vec::new();
    for (name, role) in user_roles {
        if seen.insert(name.as_str()) {
            formatted_roles.push(format_agent_role(name, role));
        }
    }
    for (name, role) in &built_in_roles {
        if seen.insert(name.as_str()) {
            formatted_roles.push(format_agent_role(name, role));
        }
    }
    format!(
        "Optional type name for the new agent. If omitted, `default` is used. For a normal full-history spawn, omit this field; do not send `agent_type: \"default\"`. Set this only to choose a non-default role, and then set `fork_turns` to `none` or a positive integer because full-history forks inherit this setting.\nAvailable roles:\n{}",
        formatted_roles.join("\n")
    )
}

fn format_agent_role(name: &str, role: &crate::subagents::role::AgentRoleConfig) -> String {
    let Some(description) = role
        .description
        .as_deref()
        .filter(|value| !value.trim().is_empty())
    else {
        return format!("{name}: no description");
    };
    format!(
        "{name}: {{\n{description}{}\n}}",
        locked_role_settings_note(role)
    )
}

fn locked_role_settings_note(role: &crate::subagents::role::AgentRoleConfig) -> String {
    let model = role.overrides.model.as_deref();
    let reasoning_effort = role.overrides.reasoning_effort.as_deref();
    let service_tier = role.overrides.service_tier.as_deref();
    let model_and_reasoning_note = match (model, reasoning_effort) {
        (Some(model), Some(reasoning_effort)) => format!(
            "\n- This role's model is set to `{model}` and its reasoning effort is set to `{reasoning_effort}`. These settings cannot be changed."
        ),
        (Some(model), None) => {
            format!("\n- This role's model is set to `{model}` and cannot be changed.")
        }
        (None, Some(reasoning_effort)) => {
            format!(
                "\n- This role's reasoning effort is set to `{reasoning_effort}` and cannot be changed."
            )
        }
        (None, None) => String::new(),
    };
    let service_tier_note = service_tier
        .map(|service_tier| {
            format!(
                "\n- This role's service tier is set to `{service_tier}`. If it is supported by the resolved model, it takes precedence over a valid spawn request service tier."
            )
        })
        .unwrap_or_default();
    format!("{model_and_reasoning_note}{service_tier_note}")
}

fn child_run_config_overrides(
    parent: &ProviderRunConfig,
    config: &AgentConfigLayer,
) -> Vec<(String, toml::Value)> {
    let mut overrides = parent.options.config_overrides.clone();
    push_string_override(
        &mut overrides,
        "browser_mode",
        parent.options.browser_mode.as_deref(),
    );
    push_string_override(
        &mut overrides,
        "base_instructions",
        parent.options.base_instructions.as_deref(),
    );
    push_string_override(
        &mut overrides,
        "compact_prompt",
        parent.options.compact_prompt.as_deref(),
    );
    push_u64_override(
        &mut overrides,
        "python_tool_timeout_seconds",
        parent.options.python_tool_timeout_seconds,
    );
    push_bool_override(
        &mut overrides,
        "model_compaction_enabled",
        parent.options.model_compaction_enabled,
    );
    if let Some(limit) = parent.options.model_auto_compact_token_limit {
        push_i64_override(&mut overrides, "model_auto_compact_token_limit", limit);
    }
    push_string_override(
        &mut overrides,
        "model_auto_compact_token_limit_scope",
        Some(match parent.options.model_auto_compact_token_limit_scope {
            crate::decision::AutoCompactTokenLimitScope::Total => "total",
            crate::decision::AutoCompactTokenLimitScope::BodyAfterPrefix => "body_after_prefix",
        }),
    );
    if let Some(policy) = approval_policy_config_value(parent.options.approval_policy) {
        push_string_override(&mut overrides, "approval_policy", Some(policy));
    }
    push_bool_override(&mut overrides, "use_guardian", parent.options.use_guardian);

    overrides.extend(config.config_overrides.clone());
    let instructions = config.instructions.trim();
    if !instructions.is_empty() {
        overrides.push((
            "developer_instructions".to_string(),
            toml::Value::String(instructions.to_string()),
        ));
    }
    let provider = config.provider.trim();
    if !provider.is_empty() {
        overrides.push((
            "model_provider".to_string(),
            toml::Value::String(provider.to_string()),
        ));
    }
    if let Some(service_tier) = config
        .service_tier
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        overrides.push((
            "service_tier".to_string(),
            toml::Value::String(service_tier.to_string()),
        ));
    }
    if !config.tool_allowlist.is_empty() {
        overrides.push((
            "tool_allowlist".to_string(),
            toml::Value::Array(
                config
                    .tool_allowlist
                    .iter()
                    .map(|tool| toml::Value::String(tool.clone()))
                    .collect(),
            ),
        ));
    }
    overrides.push((
        "can_write".to_string(),
        toml::Value::Boolean(config.can_write),
    ));
    overrides
}

fn push_string_override(
    overrides: &mut Vec<(String, toml::Value)>,
    key: &str,
    value: Option<&str>,
) {
    if let Some(value) = value.map(str::trim).filter(|value| !value.is_empty()) {
        overrides.push((key.to_string(), toml::Value::String(value.to_string())));
    }
}

fn push_bool_override(overrides: &mut Vec<(String, toml::Value)>, key: &str, value: bool) {
    overrides.push((key.to_string(), toml::Value::Boolean(value)));
}

fn push_i64_override(overrides: &mut Vec<(String, toml::Value)>, key: &str, value: i64) {
    overrides.push((key.to_string(), toml::Value::Integer(value)));
}

fn push_u64_override(overrides: &mut Vec<(String, toml::Value)>, key: &str, value: u64) {
    if let Ok(value) = i64::try_from(value) {
        push_i64_override(overrides, key, value);
    }
}

fn approval_policy_config_value(policy: AskForApproval) -> Option<&'static str> {
    match policy {
        AskForApproval::Never => Some("never"),
        AskForApproval::OnFailure => Some("on-failure"),
        AskForApproval::OnRequest => Some("on-request"),
        AskForApproval::UnlessTrusted => Some("unless-trusted"),
        AskForApproval::Granular(_) => None,
    }
}

fn parent_agent_path_from_store(user_input: &Option<(SharedStore, SessionId)>) -> Option<String> {
    let (store, session_id) = user_input.as_ref()?;
    let store = store.lock().ok()?;
    display_agent_path_for_session(&store, session_id.as_str()).ok()
}

fn session_is_spawned_subagent_for_tools(store: &SharedStore, session_id: &str) -> bool {
    let Ok(store) = store.lock() else {
        return false;
    };
    let has_parent = store
        .load_session(session_id)
        .ok()
        .flatten()
        .and_then(|session| session.parent_id)
        .is_some();
    has_parent
        && store
            .events_for_session(session_id)
            .map(|events| {
                events
                    .iter()
                    .any(|event| event.event_type == "agent.context")
            })
            .unwrap_or(false)
}

fn agent_path_depth(agent_path: &str) -> i32 {
    let trimmed = agent_path.trim().trim_matches('/');
    if trimmed.is_empty() || trimmed == "root" {
        return 0;
    }
    trimmed.split('/').count().saturating_sub(1) as i32
}

fn build_goal_store(
    user_input: &Option<(SharedStore, SessionId)>,
) -> Arc<crate::tools::handlers::goal::GoalStore> {
    use crate::tools::handlers::goal::GoalStore;

    match user_input {
        Some((store, sid)) => {
            let sink: Arc<dyn EventSink> = Arc::new(SubagentStoreSink {
                store: store.clone(),
            });
            Arc::new(GoalStore::from_shared_store(
                sid.as_str().to_string(),
                sink,
                store.clone(),
            ))
        }
        None => Arc::new(GoalStore::new(String::new(), Arc::new(NoopSubagentSink))),
    }
}

fn goal_runtime_enabled(
    _config: &ProviderRunConfig,
    user_input: &Option<(SharedStore, SessionId)>,
) -> bool {
    let Some((store, sid)) = user_input else {
        return false;
    };
    session_allows_goal_tools(store, sid.as_str())
}

fn session_allows_goal_tools(store: &SharedStore, session_id: &str) -> bool {
    let events = store
        .lock()
        .unwrap()
        .events_for_session(session_id)
        .unwrap_or_default();
    !events.iter().any(|event| {
        event.event_type == "session.review_mode"
            && event
                .payload
                .get("review_tool_restrictions")
                .and_then(|restrictions| restrictions.get("goals"))
                .and_then(serde_json::Value::as_bool)
                == Some(false)
    })
}

/// Register the goal tool family (`get_goal`, `create_goal`, `update_goal`) into
/// `reg`, all sharing ONE [`GoalStore`].
///
/// The store wraps a [`GoalManager`](crate::goals::GoalManager) whose injected
/// [`EventSink`] persists durable `goal.*` events: on the live run path it writes
/// the same journal projection as subagent lifecycle events, so TUI render /
/// resume-by-replay observe `goal.created` / `goal.updated`; in tests/headless it
/// is the no-op [`NoopSubagentSink`]. `create_goal` (and budget-threshold
/// crossings) emit through the manager's sink automatically; `update_goal` emits
/// `goal.updated` from its handler.
///
/// Mirrors [`register_subagent_tools`]: a shared durable journal sink plus the
/// session id from the threaded `(SharedStore, SessionId)`.
fn register_goal_tools<S, A>(
    reg: &mut crate::tools::registry::ToolRegistry<S, A>,
    store: Arc<crate::tools::handlers::goal::GoalStore>,
) where
    S: crate::tools::sandbox::SandboxProvider,
    A: crate::tools::runtime::Approver,
{
    use crate::tools::handlers::goal::{
        CreateGoalRequest, CreateGoalTool, GetGoalRequest, GetGoalTool, UpdateGoalRequest,
        UpdateGoalTool,
    };
    use crate::tools::registry::definitions;

    // get_goal: read-only snapshot (parallel-safe).
    reg.register::<_, GetGoalRequest>(
        "get_goal",
        definitions::get_goal(),
        true,
        GetGoalTool::new(store.clone()),
    );
    // create_goal: mutates the shared state (serial).
    reg.register::<_, CreateGoalRequest>(
        "create_goal",
        definitions::create_goal(),
        false,
        CreateGoalTool::new(store.clone()),
    );
    // update_goal: mutates the shared state (serial).
    reg.register::<_, UpdateGoalRequest>(
        "update_goal",
        definitions::update_goal(),
        false,
        UpdateGoalTool::new(store),
    );
}

/// Best-effort wire-provider label for the child's base config (mirrors the
/// entrypoint's backend->label derivation).
fn provider_label(config: &ProviderRunConfig) -> String {
    format!("{:?}", config.backend).to_ascii_lowercase()
}

fn effective_provider_id(config: &ProviderRunConfig) -> String {
    config
        .options
        .model_provider_id
        .clone()
        .unwrap_or_else(|| provider_label(config))
}

fn parent_agent_config_layer(
    config: &ProviderRunConfig,
    tool_cwd: &std::path::Path,
) -> AgentConfigLayer {
    let overrides = &config.options.config_overrides;
    let mut layer = AgentConfigLayer::base(config.model.clone(), effective_provider_id(config));
    if let Some(catalog) = spawn_agent_model_catalog(config, tool_cwd) {
        layer.available_models = catalog.presets(true);
        layer.model_catalog = Some(catalog);
    }
    if let Some(instructions) = config_override_string_any(overrides, &["developer_instructions"])
        .or_else(|| config.options.developer_instructions.clone())
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
    {
        layer.instructions = instructions;
    }
    layer.reasoning_effort =
        config_override_string_any(overrides, &["model_reasoning_effort", "reasoning_effort"])
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty());
    layer.service_tier = config_override_string_any(overrides, &["service_tier"])
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());
    if let Some(tool_allowlist) =
        config_override_string_array_any(overrides, &["tool_allowlist", "tools"])
    {
        layer.tool_allowlist = tool_allowlist;
    }
    if let Some(can_write) = config_override_bool_any(overrides, &["can_write"]) {
        layer.can_write = can_write;
    }
    layer
}

fn spawn_agent_available_models_description(
    config: &ProviderRunConfig,
    tool_cwd: &std::path::Path,
) -> String {
    if let Some(catalog) = spawn_agent_model_catalog(config, tool_cwd) {
        browser_use_providers::spawn_agent_model_overrides_description_for_catalog(&catalog, true)
    } else {
        browser_use_providers::spawn_agent_model_overrides_description()
    }
}

fn spawn_agent_model_catalog(
    config: &ProviderRunConfig,
    tool_cwd: &std::path::Path,
) -> Option<browser_use_providers::ModelCatalog> {
    let path = config
        .options
        .config_overrides
        .iter()
        .rev()
        .find(|(key, _)| key == "model_catalog_json")
        .and_then(|(_, value)| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())?;
    let path = std::path::PathBuf::from(path);
    let path = if path.is_absolute() {
        path
    } else {
        tool_cwd.join(path)
    };
    let json = std::fs::read_to_string(path).ok()?;
    serde_json::from_str::<browser_use_providers::ModelCatalog>(&json).ok()
}

fn config_override_string_any(overrides: &ConfigOverrides, keys: &[&str]) -> Option<String> {
    overrides
        .iter()
        .rev()
        .find(|(candidate, _)| keys.iter().any(|key| candidate == key))
        .and_then(|(_, value)| value.as_str().map(str::to_string))
}

fn config_override_bool_any(overrides: &ConfigOverrides, keys: &[&str]) -> Option<bool> {
    overrides
        .iter()
        .rev()
        .find(|(candidate, _)| keys.iter().any(|key| candidate == key))
        .and_then(|(_, value)| value.as_bool())
}

fn config_override_string_array_any(
    overrides: &ConfigOverrides,
    keys: &[&str],
) -> Option<Vec<String>> {
    overrides
        .iter()
        .rev()
        .find(|(candidate, _)| keys.iter().any(|key| candidate == key))
        .and_then(|(_, value)| {
            value.as_array().map(|items| {
                items
                    .iter()
                    .filter_map(|item| item.as_str())
                    .map(str::trim)
                    .filter(|item| !item.is_empty())
                    .map(ToOwned::to_owned)
                    .collect::<Vec<_>>()
            })
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config_overrides::ProviderRunConfig;
    use crate::events::PendingEvent;
    use std::sync::Mutex;

    /// Serializes tests that mutate process env (`set_var`/`remove_var`). Cargo
    /// runs tests in a binary in parallel, so unsynchronized env mutation across
    /// these credential tests would race; this lock keeps them serial.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    struct NullSink;
    impl EventSink for NullSink {
        fn emit(&self, _ev: PendingEvent) {}
    }

    /// A no-op [`FusionRecorder`] for resolution tests (they assert the driver
    /// CONSTRUCTS offline — they never sample, so nothing is ever recorded).
    struct NullRecorder;
    #[async_trait::async_trait]
    impl FusionRecorder for NullRecorder {
        async fn record(&self, _messages: &[browser_use_llm::schema::Message]) {}
    }

    fn recorder() -> Arc<dyn FusionRecorder> {
        Arc::new(NullRecorder)
    }

    /// A fake Python backend so resolution tests never spawn a real worker
    /// (network/process free). It records nothing and is never `run` — these
    /// tests only assert the driver CONSTRUCTS, they do not dispatch.
    struct FakePythonBackend;
    impl crate::tools::handlers::python::PythonBackend for FakePythonBackend {
        fn run(
            &self,
            _session_id: &str,
            _cwd: &std::path::Path,
            _artifact_dir: &std::path::Path,
            _code: &str,
            _timeout_secs: Option<f64>,
        ) -> anyhow::Result<browser_use_python_worker::RunPythonResponse> {
            anyhow::bail!("fake python backend: run() not used in resolution tests")
        }
    }

    fn fake_python() -> Arc<dyn PythonBackend> {
        Arc::new(FakePythonBackend)
    }

    fn ctx() -> TurnCtx {
        TurnCtx {
            session_id: "prov-test".to_string(),
            model: "m".to_string(),
            provider: "p".to_string(),
            base_instructions: crate::prompts::browser_agent_system_prompt(),
            browser_mode_instruction: None,
            turn_idx: 0,
            attempt: 0,
        }
    }

    /// A real OpenAI backend CONSTRUCTS the live driver offline (no network). We
    /// inject the key via env for the duration of the test, then assert
    /// resolution yields a Real driver. The key is removed afterwards.
    ///
    /// We go through `resolve_provider_with_python` with an injected FAKE Python
    /// backend so the test never spawns a real Python worker subprocess (the
    /// public `resolve_provider` starts the real worker eagerly; that is exercised
    /// in production, not here). The offline-construction assertion is about the
    /// model transport, not the worker.
    #[test]
    fn resolves_real_openai_driver_offline() {
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::set_var("OPENAI_API_KEY", "sk-test-entrypoint");
        let config = ProviderRunConfig::new(ProviderBackend::Openai, "gpt-x");
        let resolved = resolve_provider_with_python(
            &config,
            None,
            Arc::new(NullSink),
            ctx(),
            3,
            recorder(),
            Some(fake_python()),
            None,
            std::env::temp_dir(),
            std::env::temp_dir().join("artifacts"),
            None,
        )
        .expect("real openai driver must construct offline");
        std::env::remove_var("OPENAI_API_KEY");
        assert!(matches!(resolved, ResolvedProvider::Real(_)));
    }

    #[test]
    fn browser_use_backend_resolves_gateway_route_from_cloud_key() {
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::remove_var("BROWSER_USE_API_KEY");
        std::env::remove_var("LLM_BROWSER_BROWSER_USE_API_KEY");
        let dir = tempfile::tempdir().expect("tempdir");
        let store = Store::open(dir.path()).expect("store");
        store
            .set_setting("auth.browser_use_cloud.api_key", "stored-browser-use-key")
            .unwrap();

        let choice = provider_choice_for_backend(ProviderBackend::BrowserUse, Some(&store))
            .expect("resolves")
            .expect("browser-use is a real provider");

        match choice {
            ProviderChoice::OpenAiCompatibleCustom {
                provider_id,
                base_url,
                api_key,
                extra_headers,
            } => {
                assert_eq!(provider_id, "browser-use");
                assert_eq!(base_url, "https://llm.api.browser-use.com/v1");
                assert_eq!(api_key, "stored-browser-use-key");
                assert_eq!(
                    extra_headers,
                    vec![(
                        "x-browser-use-request-type".to_string(),
                        "rust_agent".to_string()
                    )]
                );
            }
            other => panic!("expected browser-use gateway choice, got {other:?}"),
        }
    }

    /// A real Anthropic backend also constructs offline given its key.
    #[test]
    fn resolves_real_anthropic_driver_offline() {
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::set_var("ANTHROPIC_API_KEY", "ak-test-entrypoint");
        let config = ProviderRunConfig::new(ProviderBackend::Anthropic, "claude-x");
        let resolved = resolve_provider_with_python(
            &config,
            None,
            Arc::new(NullSink),
            ctx(),
            3,
            recorder(),
            Some(fake_python()),
            None,
            std::env::temp_dir(),
            std::env::temp_dir().join("artifacts"),
            None,
        )
        .expect("real anthropic driver must construct offline");
        std::env::remove_var("ANTHROPIC_API_KEY");
        assert!(matches!(resolved, ResolvedProvider::Real(_)));
    }

    /// The fake backend resolves to the Fake signal (no real provider, no key).
    #[test]
    fn fake_backend_resolves_to_fake_signal() {
        let config = ProviderRunConfig::new(ProviderBackend::Fake, "fake-model");
        let resolved = resolve_provider(
            &config,
            None,
            Arc::new(NullSink),
            ctx(),
            3,
            recorder(),
            None,
        )
        .expect("fake must resolve");
        assert!(matches!(resolved, ResolvedProvider::Fake));
    }

    /// The codex backend is a REAL provider again: with env codex creds present it
    /// resolves a live driver offline (no network), targeting chatgpt.com.
    #[test]
    fn codex_backend_resolves_real_driver_from_env() {
        // Serialize with the other env-mutating tests: this sets CODEX_* vars,
        // and `codex_backend_resolves_choice_from_store` clears them, so without a
        // shared lock the two race (a flake surfaced under parallel test runs).
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::set_var("CODEX_ACCESS_TOKEN", "codex-access-test");
        std::env::set_var("CODEX_ACCOUNT_ID", "codex-acct-test");
        let config = ProviderRunConfig::new(ProviderBackend::Codex, "gpt-5.1-codex");
        let resolved = resolve_provider_with_python(
            &config,
            None,
            Arc::new(NullSink),
            ctx(),
            3,
            recorder(),
            Some(fake_python()),
            None,
            std::env::temp_dir(),
            std::env::temp_dir().join("artifacts"),
            None,
        );
        std::env::remove_var("CODEX_ACCESS_TOKEN");
        std::env::remove_var("CODEX_ACCOUNT_ID");
        assert!(matches!(
            resolved.expect("codex driver must construct offline"),
            ResolvedProvider::Real(_)
        ));
    }

    /// The codex backend also resolves its OAuth creds from the Store
    /// (`auth.codex.access_token` / `auth.codex.account_id`) when env is absent —
    /// proving the store-fallback path for codex.
    #[test]
    fn codex_backend_resolves_choice_from_store() {
        // Serialize with the env-setting codex test (see ENV_LOCK note there):
        // both touch CODEX_* process env, so they must not run concurrently.
        let _guard = ENV_LOCK.lock().unwrap();
        // Env codex creds must be absent for this to exercise the store path.
        std::env::remove_var("CODEX_ACCESS_TOKEN");
        std::env::remove_var("CODEX_ACCOUNT_ID");
        let dir = tempfile::tempdir().expect("tempdir");
        let store = Store::open(dir.path()).expect("store");
        store
            .set_setting("auth.codex.access_token", "stored-codex-access")
            .unwrap();
        store
            .set_setting("auth.codex.account_id", "stored-codex-acct")
            .unwrap();
        let choice = provider_choice_for_backend(ProviderBackend::Codex, Some(&store))
            .expect("codex resolves")
            .expect("codex is a real provider");
        match choice {
            ProviderChoice::Codex {
                access_token,
                account_id,
                ..
            } => {
                assert_eq!(access_token, "stored-codex-access");
                assert_eq!(account_id, "stored-codex-acct");
            }
            other => panic!("expected codex choice, got {other:?}"),
        }
    }

    /// A real backend with NO credentials in env AND none in the store is an honest
    /// typed error, not a panic.
    #[test]
    fn missing_credentials_is_typed_error() {
        // Ensure the relevant keys are unset for this backend.
        std::env::remove_var("OPENROUTER_API_KEY");
        std::env::remove_var("LLM_BROWSER_OPENAI_COMPAT_API_KEY");
        let config = ProviderRunConfig::new(ProviderBackend::Openrouter, "x");
        let err = resolve_provider(
            &config,
            None,
            Arc::new(NullSink),
            ctx(),
            3,
            recorder(),
            None,
        )
        .expect_err("missing credentials must error");
        assert!(matches!(err, ProviderResolveError::MissingCredentials(_)));
    }

    /// Env wins over the store: a provider key present in BOTH resolves to the env
    /// value (legacy `stored_or_env` precedence).
    #[test]
    fn env_key_wins_over_store() {
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::set_var("OPENAI_API_KEY", "env-openai-key");
        let dir = tempfile::tempdir().expect("tempdir");
        let store = Store::open(dir.path()).expect("store");
        store
            .set_setting("auth.openai.api_key", "stored-openai-key")
            .unwrap();
        let choice = provider_choice_for_backend(ProviderBackend::Openai, Some(&store))
            .expect("resolves")
            .expect("real provider");
        std::env::remove_var("OPENAI_API_KEY");
        match choice {
            ProviderChoice::OpenAiResponses { api_key, .. } => {
                assert_eq!(api_key, "env-openai-key", "env must win over store");
            }
            other => panic!("expected openai choice, got {other:?}"),
        }
    }

    /// Store is the fallback: with the env key absent, the stored
    /// `auth.<provider>.api_key` resolves the provider (fixes the env-only regression).
    #[test]
    fn store_key_is_fallback_when_env_absent() {
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::remove_var("ANTHROPIC_API_KEY");
        std::env::remove_var("LLM_BROWSER_ANTHROPIC_API_KEY");
        let dir = tempfile::tempdir().expect("tempdir");
        let store = Store::open(dir.path()).expect("store");
        store
            .set_setting("auth.anthropic.api_key", "stored-anthropic-key")
            .unwrap();
        let choice = provider_choice_for_backend(ProviderBackend::Anthropic, Some(&store))
            .expect("resolves")
            .expect("real provider");
        match choice {
            ProviderChoice::Anthropic { api_key, .. } => {
                assert_eq!(api_key, "stored-anthropic-key");
            }
            other => panic!("expected anthropic choice, got {other:?}"),
        }
    }

    // ---- browser/python tool wiring (network/process free) --------------------
    //
    // These prove the `browser` and `python` handlers are REGISTERED in the
    // production registry the dispatcher runs over, and REACHABLE through the real
    // dispatch path: a call deserializes into the typed Req, the runner looks the
    // handler up BY NAME (not "unknown tool"), and the FAKE backend's distinctive
    // marker flows out the rendered tool-result. We never start a real
    // PythonWorker or a real browser: browser uses a FAKE [`BrowserBackend`],
    // python a FAKE [`PythonBackend`]. Mirrors `browser_tests.rs` /
    // `python_tests.rs` (same orchestrator-driven seam) plus a registry
    // membership assertion.

    use crate::tools::handlers::browser::{BrowserBackend, BrowserRequest, BrowserTool};
    use crate::tools::handlers::python::{PythonRequest, PythonTool};
    use crate::tools::orchestrator::ToolOrchestrator;
    use crate::tools::registry::{definitions, ToolRegistry};
    use crate::tools::runtime::{AutoApprover, ToolCtx};
    use crate::tools::sandbox::{FileSystemSandboxPolicy, NoneSandboxProvider};
    use browser_use_browser::BrowserCommandOutput;
    use browser_use_python_worker::RunPythonResponse;

    /// A fake browser backend returning a marker on the `command` path (no real
    /// CDP/browser). Only `command` is exercised; the other methods are
    /// unreachable in these tests.
    struct MarkerBrowserBackend;
    impl BrowserBackend for MarkerBrowserBackend {
        fn command(
            &self,
            _session_id: &str,
            _cwd: &std::path::Path,
            _artifact_dir: &std::path::Path,
            _command: &str,
        ) -> anyhow::Result<BrowserCommandOutput> {
            Ok(BrowserCommandOutput {
                content: serde_json::json!({ "marker": "BROWSER_MARKER" }),
                events: vec![],
            })
        }
        fn run_script(
            &self,
            _session_id: &str,
            _cwd: &std::path::Path,
            _artifact_dir: &std::path::Path,
            _code: &str,
            _timeout_secs: u64,
        ) -> anyhow::Result<browser_use_browser::BrowserScriptOutput> {
            anyhow::bail!("run_script not used")
        }
        fn start_script(
            &self,
            _session_id: &str,
            _cwd: &std::path::Path,
            _artifact_dir: &std::path::Path,
            _code: &str,
            _timeout_secs: u64,
        ) -> anyhow::Result<browser_use_browser::BrowserScriptOutput> {
            anyhow::bail!("start_script not used")
        }
        fn observe_script(
            &self,
            _session_id: &str,
            _run_id: &str,
            _observe_timeout_ms: u64,
        ) -> anyhow::Result<browser_use_browser::BrowserScriptOutput> {
            anyhow::bail!("observe_script not used")
        }
        fn cancel_script(
            &self,
            _session_id: &str,
            _run_id: &str,
        ) -> anyhow::Result<browser_use_browser::BrowserScriptOutput> {
            anyhow::bail!("cancel_script not used")
        }
    }

    struct ScriptLifecycleBrowserBackend;
    impl BrowserBackend for ScriptLifecycleBrowserBackend {
        fn command(
            &self,
            _session_id: &str,
            _cwd: &std::path::Path,
            _artifact_dir: &std::path::Path,
            _command: &str,
        ) -> anyhow::Result<BrowserCommandOutput> {
            anyhow::bail!("command not used")
        }

        fn run_script(
            &self,
            _session_id: &str,
            _cwd: &std::path::Path,
            _artifact_dir: &std::path::Path,
            _code: &str,
            _timeout_secs: u64,
        ) -> anyhow::Result<browser_use_browser::BrowserScriptOutput> {
            anyhow::bail!("run_script not used")
        }

        fn start_script(
            &self,
            _session_id: &str,
            _cwd: &std::path::Path,
            _artifact_dir: &std::path::Path,
            _code: &str,
            _timeout_secs: u64,
        ) -> anyhow::Result<browser_use_browser::BrowserScriptOutput> {
            Ok(browser_use_browser::BrowserScriptOutput {
                ok: true,
                status: Some("running".to_string()),
                run_id: Some("script-1".to_string()),
                text: "first chunk".to_string(),
                ..Default::default()
            })
        }

        fn observe_script(
            &self,
            _session_id: &str,
            _run_id: &str,
            _observe_timeout_ms: u64,
        ) -> anyhow::Result<browser_use_browser::BrowserScriptOutput> {
            Ok(browser_use_browser::BrowserScriptOutput {
                ok: true,
                status: Some("finished".to_string()),
                run_id: Some("script-1".to_string()),
                outputs: vec![serde_json::json!({
                    "label": "page_info",
                    "value": { "url": "https://example.com", "title": "Example" }
                })],
                ..Default::default()
            })
        }

        fn cancel_script(
            &self,
            _session_id: &str,
            _run_id: &str,
        ) -> anyhow::Result<browser_use_browser::BrowserScriptOutput> {
            anyhow::bail!("cancel_script not used")
        }
    }

    /// A fake python backend returning a marker output (no real worker/process).
    struct MarkerPythonBackend;
    impl crate::tools::handlers::python::PythonBackend for MarkerPythonBackend {
        fn run(
            &self,
            _session_id: &str,
            _cwd: &std::path::Path,
            _artifact_dir: &std::path::Path,
            _code: &str,
            _timeout_secs: Option<f64>,
        ) -> anyhow::Result<RunPythonResponse> {
            Ok(RunPythonResponse {
                id: "py-marker".to_string(),
                ok: true,
                text: "PYTHON_MARKER".to_string(),
                error: None,
                data: serde_json::Value::Null,
                outputs: vec![],
                artifacts: vec![],
                images: vec![],
                browser_events: vec![],
                browser_harness_available: false,
                browser_harness_error: None,
            })
        }
    }

    fn turn_env() -> TurnEnv {
        TurnEnv {
            file_system_sandbox_policy: FileSystemSandboxPolicy {
                restricted: false,
                denied_read: false,
            },
            managed_network_active: false,
            strict_auto_review: false,
            use_guardian: false,
        }
    }

    fn tool_ctx(name: &str) -> ToolCtx {
        ToolCtx {
            call_id: format!("call-{name}"),
            tool_name: name.to_string(),
            cwd: std::env::temp_dir(),
            artifact_root: std::env::temp_dir().join("artifacts"),
        }
    }

    /// `browser` and `python` are REGISTERED in the production registry (exactly
    /// as `build_tool_dispatcher` registers them) — proving they are no longer the
    /// Phase-E "unknown tool" seam.
    #[test]
    fn browser_and_python_are_registered() {
        // Default seams (`NoneSandboxProvider`/`AutoApprover`) — this registry is
        // only inspected for membership, never handed to a runner that would infer
        // the seams, so annotate them explicitly.
        let mut reg: ToolRegistry<NoneSandboxProvider, AutoApprover> = ToolRegistry::new();
        reg.register::<_, BrowserRequest>(
            "browser",
            definitions::browser(),
            false,
            BrowserTool::with_backend(Arc::new(MarkerBrowserBackend)),
        );
        reg.register::<_, PythonRequest>(
            "python",
            definitions::python(),
            false,
            PythonTool::with_backend(Arc::new(MarkerPythonBackend)),
        );
        assert!(reg.contains("browser"), "browser must be registered");
        assert!(reg.contains("python"), "python must be registered");
    }

    /// A `browser` call REACHES the injected backend through the orchestrator
    /// (same seam the dispatcher's runner uses) and the backend's marker flows
    /// into the rendered output — not a stub, not "unknown tool".
    #[tokio::test]
    async fn browser_dispatch_reaches_injected_backend() {
        let tool = BrowserTool::with_backend(Arc::new(MarkerBrowserBackend));
        let orch = ToolOrchestrator::new(NoneSandboxProvider, AutoApprover);
        let req = BrowserRequest::command("sess-1", "click");
        let result = orch
            .run(
                &tool,
                &req,
                &tool_ctx("browser"),
                &turn_env(),
                AskForApproval::Never,
            )
            .await
            .expect("browser orchestration ok");
        assert!(
            result.output.stdout.contains("BROWSER_MARKER"),
            "browser must reach the backend, got: {:?}",
            result.output
        );
    }

    #[test]
    fn runtime_browser_backend_records_script_lifecycle() -> anyhow::Result<()> {
        use browser_use_runtime::{BrowserUseRuntime, CreateRootAgentRequest, JournalReader};

        let (runtime, journal) = BrowserUseRuntime::memory();
        let handle = runtime.handle();
        let root = handle.create_root_agent(CreateRootAgentRequest {
            cwd: std::env::temp_dir(),
            task: "root task".to_string(),
            max_concurrent_threads_per_session: 3,
        })?;
        let browser_id = handle.create_browser(RuntimeBrowserConfig::default());
        let backend = RuntimeBrowserBackend {
            session_id: root.session_id().as_str().to_string(),
            runtime: handle.clone(),
            agent_id: root.agent_id().clone(),
            browser_id: browser_id.clone(),
            backend: Arc::new(ScriptLifecycleBrowserBackend),
        };
        let cwd = std::env::temp_dir();
        let artifact_dir = std::env::temp_dir().join("browser-script-lifecycle");

        let started = backend.start_script(
            root.session_id().as_str(),
            &cwd,
            &artifact_dir,
            "await page.title()",
            10,
        )?;
        assert_eq!(started.run_id.as_deref(), Some("script-1"));
        let snapshot = handle.browsers().snapshot(&browser_id)?;
        assert_eq!(snapshot.active_scripts.len(), 1);
        assert_eq!(
            snapshot.active_scripts[0].last_delta.as_deref(),
            Some("first chunk")
        );

        let observed = backend.observe_script(root.session_id().as_str(), "script-1", 10)?;
        assert_eq!(observed.status.as_deref(), Some("finished"));
        assert!(handle
            .browsers()
            .snapshot(&browser_id)?
            .active_scripts
            .is_empty());

        let script_events = journal
            .events_for_session(root.session_id())?
            .into_iter()
            .filter(|event| event.event_type.starts_with("browser_script."))
            .collect::<Vec<_>>();
        let script_event_types = script_events
            .iter()
            .map(|event| event.event_type.clone())
            .collect::<Vec<_>>();
        assert_eq!(
            script_event_types,
            vec![
                "browser_script.started".to_string(),
                "browser_script.output_delta".to_string(),
                "browser_script.completed".to_string(),
            ]
        );
        let completed = script_events
            .iter()
            .find(|event| event.event_type == "browser_script.completed")
            .expect("completed browser_script event");
        assert_eq!(completed.payload["name"], "browser_script");
        assert_eq!(completed.payload["text"], "");
        assert_eq!(completed.payload["outputs"][0]["label"], "page_info");
        assert_eq!(
            completed.payload["outputs"][0]["value"]["url"],
            "https://example.com"
        );
        Ok(())
    }

    /// A `python` call REACHES the injected backend through the orchestrator and
    /// the backend's marker flows into the rendered output.
    #[tokio::test]
    async fn python_dispatch_reaches_injected_backend() {
        let tool = PythonTool::with_backend(Arc::new(MarkerPythonBackend));
        let orch = ToolOrchestrator::new(NoneSandboxProvider, AutoApprover);
        let req = PythonRequest::new("print('x')");
        let result = orch
            .run(
                &tool,
                &req,
                &tool_ctx("python"),
                &turn_env(),
                AskForApproval::Never,
            )
            .await
            .expect("python orchestration ok");
        assert!(
            result.output.stdout.contains("PYTHON_MARKER"),
            "python must reach the backend, got: {:?}",
            result.output
        );
    }

    /// The PRODUCTION builder `build_tool_dispatcher` accepts the injected python
    /// backend and constructs a real dispatcher (proving the signature wiring) —
    /// exercised with a FAKE backend so no real worker is started.
    #[test]
    fn production_builder_accepts_injected_python_backend() {
        // No MCP servers -> mcp tool absent.
        let config = ProviderRunConfig::new(ProviderBackend::Fake, "fake-model");
        let _dispatcher: Arc<RealToolDispatcher> =
            build_tool_dispatcher(Arc::new(MarkerPythonBackend), &config, None);
    }

    #[test]
    fn browser_use_api_tool_allowlist_hides_workspace_tools() {
        let options = crate::config_overrides::AgentRunOptions {
            config_overrides: vec![(
                "tool_allowlist".to_string(),
                toml::Value::Array(vec![
                    toml::Value::String("browser".to_string()),
                    toml::Value::String("browser_script".to_string()),
                    toml::Value::String("done".to_string()),
                ]),
            )],
            ..crate::config_overrides::AgentRunOptions::default()
        };
        let config =
            ProviderRunConfig::new(ProviderBackend::Fake, "fake-model").with_options(options);
        let dispatcher = build_tool_dispatcher(Arc::new(MarkerPythonBackend), &config, None);
        let names: Vec<&str> = dispatcher
            .tool_specs()
            .iter()
            .map(|s| s.name.as_str())
            .collect();

        assert_eq!(names, vec!["browser", "browser_script", "done"]);
        assert!(!names.contains(&"exec_command"));
        assert!(!names.contains(&"python"));
        assert!(!names.contains(&"tool_search"));
    }

    /// An empty `mcp_servers` map registers NO `mcp` tool (prior behavior).
    #[test]
    fn empty_mcp_servers_registers_no_mcp_tool() {
        let config = ProviderRunConfig::new(ProviderBackend::Fake, "fake-model");
        let dispatcher = build_tool_dispatcher(Arc::new(MarkerPythonBackend), &config, None);
        let names: Vec<&str> = dispatcher
            .tool_specs()
            .iter()
            .map(|s| s.name.as_str())
            .collect();
        assert!(
            !names.contains(&"mcp"),
            "no mcp tool without configured servers"
        );
        assert!(
            !names.contains(&"request_user_input"),
            "request_user_input is intentionally not exposed as a model tool"
        );
        // The other core tools are still present.
        assert!(names.contains(&"browser"));
        assert!(names.contains(&"done"));
        assert!(names.contains(&"update_plan"));
        // Both web searches are wired into the production dispatcher: the hosted
        // `web_search` and the locally-executed DuckDuckGo `search`.
        assert!(names.contains(&"web_search"));
        assert!(
            names.contains(&"search"),
            "the locally-executed `search` tool must be reachable by the live model"
        );
    }

    /// A non-empty `mcp_servers` map registers the `mcp` tool. The stdio server
    /// command (`true`) connects to nothing useful, but `connect_all`'s per-server
    /// failure isolation still yields a manager and the registration wiring
    /// surfaces the `mcp` tool in the dispatcher's specs.
    #[test]
    fn nonempty_mcp_servers_registers_mcp_tool() {
        use crate::mcp::{McpServerConfig, McpServerTransport};
        let mut servers = std::collections::HashMap::new();
        servers.insert(
            "echo".to_string(),
            McpServerConfig {
                transport: McpServerTransport::Stdio {
                    command: "true".to_string(),
                    args: Vec::new(),
                    env: std::collections::HashMap::new(),
                    cwd: None,
                },
                startup_timeout_ms: Some(200),
                tool_timeout_ms: Some(200),
                enabled_tools: None,
                disabled_tools: None,
            },
        );
        let options = crate::config_overrides::AgentRunOptions {
            mcp_servers: servers,
            ..crate::config_overrides::AgentRunOptions::default()
        };
        let config =
            ProviderRunConfig::new(ProviderBackend::Fake, "fake-model").with_options(options);
        let dispatcher = build_tool_dispatcher(Arc::new(MarkerPythonBackend), &config, None);
        let names: Vec<&str> = dispatcher
            .tool_specs()
            .iter()
            .map(|s| s.name.as_str())
            .collect();
        assert!(
            names.contains(&"mcp"),
            "mcp tool must be registered when servers are configured; got {names:?}"
        );
    }

    fn test_child_agent_runner() -> crate::config_overrides::ChildAgentRunner {
        crate::config_overrides::ChildAgentRunner::new(|_req| Ok(()))
    }

    /// The production dispatcher does not advertise subagent tools when there is
    /// no child runner wired for the run.
    #[test]
    fn subagent_tools_are_hidden_without_child_runner() {
        let options = crate::config_overrides::AgentRunOptions::default();
        let config =
            ProviderRunConfig::new(ProviderBackend::Fake, "fake-model").with_options(options);
        let dispatcher = build_tool_dispatcher(Arc::new(MarkerPythonBackend), &config, None);
        let names: Vec<&str> = dispatcher
            .tool_specs()
            .iter()
            .map(|s| s.name.as_str())
            .collect();
        for tool in [
            "spawn_agent",
            "wait_agent",
            "send_message",
            "followup_task",
            "list_agents",
            "close_agent",
        ] {
            assert!(
                !names.contains(&tool),
                "{tool} must be hidden without a configured child runner; got {names:?}"
            );
        }
    }

    #[test]
    fn subagent_tools_are_registered_in_the_dispatcher() {
        let options = crate::config_overrides::AgentRunOptions {
            child_agent_runner: Some(test_child_agent_runner()),
            multi_agent_v2: crate::config_overrides::MultiAgentV2Options {
                enabled: true,
                ..Default::default()
            },
            ..crate::config_overrides::AgentRunOptions::default()
        };
        let config =
            ProviderRunConfig::new(ProviderBackend::Fake, "fake-model").with_options(options);
        let dispatcher = build_tool_dispatcher(Arc::new(MarkerPythonBackend), &config, None);
        let names: Vec<&str> = dispatcher
            .tool_specs()
            .iter()
            .map(|s| s.name.as_str())
            .collect();
        for tool in [
            "spawn_agent",
            "wait_agent",
            "send_message",
            "followup_task",
            "list_agents",
            "close_agent",
        ] {
            assert!(
                names.contains(&tool),
                "{tool} must be registered in the production dispatcher; got {names:?}"
            );
        }
        assert!(
            !names.contains(&"send_input"),
            "send_input is a v1 tool and must not be exposed in the Codex-v2 flat tool surface"
        );
    }

    #[test]
    fn spawn_agent_agent_type_guidance_discourages_default_override() {
        let config = ProviderRunConfig::new(ProviderBackend::Fake, "fake-model").with_options(
            crate::config_overrides::AgentRunOptions {
                child_agent_runner: Some(test_child_agent_runner()),
                ..crate::config_overrides::AgentRunOptions::default()
            },
        );
        let dispatcher = build_tool_dispatcher(Arc::new(MarkerPythonBackend), &config, None);
        let spawn = dispatcher
            .tool_specs()
            .iter()
            .find(|spec| spec.name == "spawn_agent")
            .expect("spawn_agent tool");
        let description = spawn.input_schema["properties"]["agent_type"]
            .get("description")
            .and_then(serde_json::Value::as_str)
            .expect("agent_type description");
        assert!(description.contains("do not send `agent_type: \"default\"`"));
        assert!(description.contains("full-history forks inherit this setting"));
    }

    #[test]
    fn subagent_tools_are_hidden_when_multi_agent_features_disabled() {
        let options = crate::config_overrides::AgentRunOptions {
            multi_agent_v2: crate::config_overrides::MultiAgentV2Options {
                enabled: false,
                ..Default::default()
            },
            ..crate::config_overrides::AgentRunOptions::default()
        };
        let config =
            ProviderRunConfig::new(ProviderBackend::Fake, "fake-model").with_options(options);
        let dispatcher = build_tool_dispatcher(Arc::new(MarkerPythonBackend), &config, None);
        let names: Vec<&str> = dispatcher
            .tool_specs()
            .iter()
            .map(|s| s.name.as_str())
            .collect();
        for tool in ["send_message", "followup_task", "list_agents"] {
            assert!(
                !names.contains(&tool),
                "{tool} must be hidden when multi_agent_v2 is disabled; got {names:?}"
            );
        }
        for tool in [
            "spawn_agent",
            "send_input",
            "resume_agent",
            "wait_agent",
            "close_agent",
        ] {
            assert!(
                !names.contains(&tool),
                "{tool} must be hidden when both multi-agent feature gates are disabled; got {names:?}"
            );
        }
    }

    #[test]
    fn legacy_subagent_tools_are_exposed_when_collab_enabled() {
        let options = crate::config_overrides::AgentRunOptions {
            child_agent_runner: Some(test_child_agent_runner()),
            multi_agent_v2: crate::config_overrides::MultiAgentV2Options {
                enabled: false,
                ..Default::default()
            },
            collab_enabled: true,
            ..crate::config_overrides::AgentRunOptions::default()
        };
        let config =
            ProviderRunConfig::new(ProviderBackend::Fake, "fake-model").with_options(options);
        let dispatcher = build_tool_dispatcher(Arc::new(MarkerPythonBackend), &config, None);
        let names: Vec<&str> = dispatcher
            .tool_specs()
            .iter()
            .map(|s| s.name.as_str())
            .collect();
        for tool in [
            "spawn_agent",
            "send_input",
            "resume_agent",
            "wait_agent",
            "close_agent",
        ] {
            assert!(
                names.contains(&tool),
                "{tool} must be exposed through multi_agent_v1 when collab is enabled; got {names:?}"
            );
        }
    }

    #[test]
    fn subagent_tools_apply_multi_agent_v2_definition_options() {
        let options = crate::config_overrides::AgentRunOptions {
            child_agent_runner: Some(test_child_agent_runner()),
            multi_agent_v2: crate::config_overrides::MultiAgentV2Options {
                enabled: true,
                tool_namespace: Some("agents".to_string()),
                hide_spawn_agent_metadata: true,
                min_wait_timeout_ms: 1,
                default_wait_timeout_ms: 100,
                max_wait_timeout_ms: 1000,
                usage_hint_text: Some("Use sparingly.".to_string()),
                ..Default::default()
            },
            ..crate::config_overrides::AgentRunOptions::default()
        };
        let config =
            ProviderRunConfig::new(ProviderBackend::Fake, "fake-model").with_options(options);
        let dispatcher = build_tool_dispatcher(Arc::new(MarkerPythonBackend), &config, None);
        let spawn = dispatcher
            .tool_specs()
            .iter()
            .find(|spec| spec.name == "spawn_agent")
            .expect("spawn_agent tool");
        assert_eq!(spawn.namespace.as_deref(), None);
        assert!(spawn.description.contains("Use sparingly."));
        assert!(spawn.input_schema["properties"].get("model").is_none());
        assert_eq!(
            spawn.output_schema.as_ref().unwrap()["required"],
            serde_json::json!(["task_name"])
        );

        let wait = dispatcher
            .tool_specs()
            .iter()
            .find(|spec| spec.name == "wait_agent")
            .expect("wait_agent tool");
        assert_eq!(wait.namespace.as_deref(), None);
        assert_eq!(
            wait.input_schema["properties"]["timeout_ms"]["description"],
            serde_json::json!(
                "Optional timeout in milliseconds. Defaults to 100, min 1, max 1000."
            )
        );
    }

    #[test]
    fn subagent_namespace_is_only_applied_for_responses_backends() {
        let options = crate::config_overrides::AgentRunOptions {
            child_agent_runner: Some(test_child_agent_runner()),
            multi_agent_v2: crate::config_overrides::MultiAgentV2Options {
                enabled: true,
                tool_namespace: Some("agents".to_string()),
                ..Default::default()
            },
            ..crate::config_overrides::AgentRunOptions::default()
        };
        let openai =
            ProviderRunConfig::new(ProviderBackend::Openai, "gpt-x").with_options(options.clone());
        let anthropic =
            ProviderRunConfig::new(ProviderBackend::Anthropic, "claude-x").with_options(options);

        let openai_dispatcher = build_tool_dispatcher(Arc::new(MarkerPythonBackend), &openai, None);
        let anthropic_dispatcher =
            build_tool_dispatcher(Arc::new(MarkerPythonBackend), &anthropic, None);

        let openai_spawn = openai_dispatcher
            .tool_specs()
            .iter()
            .find(|spec| spec.name == "spawn_agent")
            .expect("openai spawn_agent");
        let anthropic_spawn = anthropic_dispatcher
            .tool_specs()
            .iter()
            .find(|spec| spec.name == "spawn_agent")
            .expect("anthropic spawn_agent");
        assert_eq!(openai_spawn.namespace.as_deref(), Some("agents"));
        assert_eq!(anthropic_spawn.namespace.as_deref(), None);
    }

    /// A `spawn_agent` call routes from the production registration into the
    /// `SubagentManager` and reaches the configured `child_agent_runner`, then
    /// returns the child's handle. This exercises the same registration
    /// `register_subagent_tools` installs, dispatched by NAME through the registry
    /// (the path the production `RegistryRunner` uses for a model tool-call), with
    /// a recording runner so it is offline.
    #[tokio::test]
    async fn spawn_agent_routes_through_registration_to_child_runner() {
        use crate::config_overrides::{AgentRunOptions, ChildAgentRunner};
        use crate::tools::orchestrator::ToolOrchestrator;
        use crate::tools::registry::ToolRegistry;
        use crate::tools::runtime::ToolCtx;
        use crate::tools::sandbox::FileSystemSandboxPolicy;
        use std::sync::atomic::{AtomicBool, Ordering};

        // A fire-and-forget child runner that records it was invoked and asserts
        // the child inherits the parent's model on the request (standing in for the
        // real task-driver-backed runner the live entrypoint wires).
        static INVOKED: AtomicBool = AtomicBool::new(false);
        INVOKED.store(false, Ordering::SeqCst);
        let runner = ChildAgentRunner::new(|req| {
            assert_eq!(req.model.as_deref(), Some("fake-model"));
            INVOKED.store(true, Ordering::SeqCst);
            Ok(())
        });
        let options = AgentRunOptions {
            child_agent_runner: Some(runner),
            multi_agent_v2: crate::config_overrides::MultiAgentV2Options {
                enabled: true,
                ..Default::default()
            },
            ..AgentRunOptions::default()
        };
        let config =
            ProviderRunConfig::new(ProviderBackend::Fake, "fake-model").with_options(options);

        // Build the production registration into a registry, then dispatch a
        // spawn_agent call BY NAME through it (the same `dispatch` the production
        // RegistryRunner calls), under the auto-approve stub orchestrator.
        let mut reg: ToolRegistry<NoneSandboxProvider, AutoApprover> = ToolRegistry::new();
        register_subagent_tools(&mut reg, &config, &None, &std::env::temp_dir(), None);
        assert!(reg.contains("spawn_agent"));

        let orch = ToolOrchestrator::stub();
        let env = TurnEnv {
            file_system_sandbox_policy: FileSystemSandboxPolicy {
                restricted: false,
                denied_read: false,
            },
            managed_network_active: false,
            strict_auto_review: false,
            use_guardian: false,
        };
        let ctx = ToolCtx {
            call_id: "c".to_string(),
            tool_name: "spawn_agent".to_string(),
            cwd: std::env::temp_dir(),
            artifact_root: std::env::temp_dir().join("artifacts"),
        };
        let out = reg
            .dispatch(
                "spawn_agent",
                &serde_json::json!({ "task_name": "explore", "message": "go" }),
                &ctx,
                &env,
                AskForApproval::Never,
                &orch,
            )
            .await
            .expect("spawn_agent should route and run");
        assert_eq!(out.exit_code, 0, "spawn should succeed: {out:?}");
        assert!(
            INVOKED.load(Ordering::SeqCst),
            "the configured child runner must be invoked"
        );
        let body: serde_json::Value = serde_json::from_str(&out.stdout).unwrap();
        assert_eq!(body["task_name"].as_str(), Some("/root/explore"));
        assert!(body.get("nickname").is_some());
    }

    #[tokio::test]
    async fn spawn_agent_registration_uses_root_inclusive_thread_limit_once() {
        use crate::config_overrides::{AgentRunOptions, ChildAgentRunner, MultiAgentV2Options};
        use crate::tools::orchestrator::ToolOrchestrator;
        use crate::tools::registry::ToolRegistry;
        use crate::tools::runtime::ToolCtx;
        use crate::tools::sandbox::FileSystemSandboxPolicy;

        let runner = ChildAgentRunner::new(|_req| Ok(()));
        let options = AgentRunOptions {
            child_agent_runner: Some(runner),
            multi_agent_v2: MultiAgentV2Options {
                enabled: true,
                max_concurrent_threads_per_session: 4,
                ..Default::default()
            },
            ..AgentRunOptions::default()
        };
        let config =
            ProviderRunConfig::new(ProviderBackend::Fake, "fake-model").with_options(options);
        let mut reg: ToolRegistry<NoneSandboxProvider, AutoApprover> = ToolRegistry::new();
        register_subagent_tools(&mut reg, &config, &None, &std::env::temp_dir(), None);

        let orch = ToolOrchestrator::stub();
        let env = TurnEnv {
            file_system_sandbox_policy: FileSystemSandboxPolicy {
                restricted: false,
                denied_read: false,
            },
            managed_network_active: false,
            strict_auto_review: false,
            use_guardian: false,
        };
        let ctx = ToolCtx {
            call_id: "c".to_string(),
            tool_name: "spawn_agent".to_string(),
            cwd: std::env::temp_dir(),
            artifact_root: std::env::temp_dir().join("artifacts"),
        };

        for task_name in ["one", "two", "three"] {
            reg.dispatch(
                "spawn_agent",
                &serde_json::json!({ "task_name": task_name, "message": "go" }),
                &ctx,
                &env,
                AskForApproval::Never,
                &orch,
            )
            .await
            .expect("cap 4 allows root plus three spawned agents");
        }

        let err = reg
            .dispatch(
                "spawn_agent",
                &serde_json::json!({ "task_name": "four", "message": "go" }),
                &ctx,
                &env,
                AskForApproval::Never,
                &orch,
            )
            .await
            .expect_err("fourth spawned agent should exceed root-inclusive cap 4");
        let error_text = format!("{err:?}");
        assert!(
            error_text.contains("agent limit reached: limit 3"),
            "unexpected error: {error_text}"
        );
    }

    #[tokio::test]
    async fn child_runner_completion_wakes_wait_agent() {
        use crate::config_overrides::{AgentRunOptions, ChildAgentRunCompletion, ChildAgentRunner};
        use crate::tools::orchestrator::ToolOrchestrator;
        use crate::tools::registry::ToolRegistry;
        use crate::tools::runtime::ToolCtx;
        use crate::tools::sandbox::FileSystemSandboxPolicy;

        let runner = ChildAgentRunner::new(|req| {
            req.completion_handler
                .as_ref()
                .expect("completion handler wired")
                .notify(ChildAgentRunCompletion::success(Some(
                    "child finished".to_string(),
                )))?;
            Ok(())
        });
        let options = AgentRunOptions {
            child_agent_runner: Some(runner),
            multi_agent_v2: crate::config_overrides::MultiAgentV2Options {
                enabled: true,
                ..Default::default()
            },
            ..AgentRunOptions::default()
        };
        let config =
            ProviderRunConfig::new(ProviderBackend::Fake, "fake-model").with_options(options);

        let mut reg: ToolRegistry<NoneSandboxProvider, AutoApprover> = ToolRegistry::new();
        register_subagent_tools(&mut reg, &config, &None, &std::env::temp_dir(), None);

        let orch = ToolOrchestrator::stub();
        let env = TurnEnv {
            file_system_sandbox_policy: FileSystemSandboxPolicy {
                restricted: false,
                denied_read: false,
            },
            managed_network_active: false,
            strict_auto_review: false,
            use_guardian: false,
        };
        let spawn_ctx = ToolCtx {
            call_id: "spawn".to_string(),
            tool_name: "spawn_agent".to_string(),
            cwd: std::env::temp_dir(),
            artifact_root: std::env::temp_dir().join("artifacts"),
        };
        reg.dispatch(
            "spawn_agent",
            &serde_json::json!({ "task_name": "explore", "message": "go" }),
            &spawn_ctx,
            &env,
            AskForApproval::Never,
            &orch,
        )
        .await
        .expect("spawn_agent should run");
        let wait_ctx = ToolCtx {
            call_id: "wait".to_string(),
            tool_name: "wait_agent".to_string(),
            cwd: std::env::temp_dir(),
            artifact_root: std::env::temp_dir().join("artifacts"),
        };
        let wait_out = reg
            .dispatch(
                "wait_agent",
                &serde_json::json!({ "timeout_ms": 10_000 }),
                &wait_ctx,
                &env,
                AskForApproval::Never,
                &orch,
            )
            .await
            .expect("wait_agent should run");
        let wait_body: serde_json::Value = serde_json::from_str(&wait_out.stdout).unwrap();
        assert_eq!(wait_body["message"], "Wait completed.");
        assert_eq!(wait_body["timed_out"], false);
    }

    #[tokio::test]
    async fn store_backed_child_runner_completion_writes_projection_once_but_wait_requires_runtime()
    {
        use crate::config_overrides::{AgentRunOptions, ChildAgentRunCompletion, ChildAgentRunner};
        use crate::tools::orchestrator::ToolOrchestrator;
        use crate::tools::registry::ToolRegistry;
        use crate::tools::runtime::ToolCtx;
        use crate::tools::sandbox::FileSystemSandboxPolicy;

        let temp = tempfile::tempdir().expect("tempdir");
        let store = Store::open(temp.path()).expect("store");
        let root = store
            .create_session(None, temp.path())
            .expect("root session");
        store
            .set_status(&root.id, browser_use_protocol::SessionStatus::Running)
            .expect("root running");
        let root_id = root.id.clone();
        let shared_store: SharedStore = Arc::new(Mutex::new(store));
        let runner_store = Arc::clone(&shared_store);
        let runner = ChildAgentRunner::new(move |req| {
            {
                let store = runner_store
                    .lock()
                    .map_err(|_| anyhow::anyhow!("store mutex poisoned"))?;
                if store.load_session(&req.child_session_id)?.is_none() {
                    let parent = store
                        .load_session(&req.parent_session_id)?
                        .ok_or_else(|| anyhow::anyhow!("missing parent session"))?;
                    store.create_child_session_with_id(
                        &req.parent_session_id,
                        std::path::Path::new(&parent.cwd),
                        req.agent_path.as_deref(),
                        req.nickname.as_deref(),
                        req.role.as_deref(),
                        req.child_session_id.clone(),
                    )?;
                }
                if let Some(run_id) = req.run_id.as_deref() {
                    store.append_event(
                        &req.child_session_id,
                        "agent.run.started",
                        serde_json::json!({ "run_id": run_id }),
                    )?;
                }
                store.set_status(
                    &req.child_session_id,
                    browser_use_protocol::SessionStatus::Done,
                )?;
            }
            req.completion_handler
                .as_ref()
                .expect("store completion handler wired")
                .notify(ChildAgentRunCompletion::success(Some(
                    "child finished".to_string(),
                )))?;
            Ok(())
        });
        let options = AgentRunOptions {
            child_agent_runner: Some(runner),
            multi_agent_v2: crate::config_overrides::MultiAgentV2Options {
                enabled: true,
                min_wait_timeout_ms: 1,
                default_wait_timeout_ms: 1,
                max_wait_timeout_ms: 1000,
                ..Default::default()
            },
            ..AgentRunOptions::default()
        };
        let config =
            ProviderRunConfig::new(ProviderBackend::Fake, "fake-model").with_options(options);

        let mut reg: ToolRegistry<NoneSandboxProvider, AutoApprover> = ToolRegistry::new();
        register_subagent_tools(
            &mut reg,
            &config,
            &Some((Arc::clone(&shared_store), SessionId(root_id.clone()))),
            temp.path(),
            None,
        );

        let orch = ToolOrchestrator::stub();
        let env = TurnEnv {
            file_system_sandbox_policy: FileSystemSandboxPolicy {
                restricted: false,
                denied_read: false,
            },
            managed_network_active: false,
            strict_auto_review: false,
            use_guardian: false,
        };
        let spawn_ctx = ToolCtx {
            call_id: "spawn".to_string(),
            tool_name: "spawn_agent".to_string(),
            cwd: temp.path().to_path_buf(),
            artifact_root: temp.path().join("artifacts"),
        };
        reg.dispatch(
            "spawn_agent",
            &serde_json::json!({ "task_name": "explore", "message": "go" }),
            &spawn_ctx,
            &env,
            AskForApproval::Never,
            &orch,
        )
        .await
        .expect("spawn_agent should run");
        let wait_ctx = ToolCtx {
            call_id: "wait".to_string(),
            tool_name: "wait_agent".to_string(),
            cwd: temp.path().to_path_buf(),
            artifact_root: temp.path().join("artifacts"),
        };
        let wait_err = reg
            .dispatch(
                "wait_agent",
                &serde_json::json!({ "timeout_ms": 1 }),
                &wait_ctx,
                &env,
                AskForApproval::Never,
                &orch,
            )
            .await
            .expect_err("wait_agent requires a live runtime mailbox");
        assert!(
            format!("{wait_err:?}").contains("wait_agent requires a live runtime mailbox"),
            "{wait_err:?}"
        );

        let store = shared_store.lock().unwrap();
        let parent_events = store.events_for_session(&root_id).unwrap();
        assert_eq!(
            parent_events
                .iter()
                .filter(|event| event.event_type == "agent.completed")
                .count(),
            1,
            "completion should be recorded once"
        );
        let parent_mail = store.messages_for_agent(&root_id).unwrap();
        assert!(
            parent_mail.is_empty(),
            "Store completion projection must not enqueue live mailbox rows"
        );
    }

    /// The production dispatcher advertises the three goal tools
    /// (`get_goal` / `create_goal` / `update_goal`) for persisted non-plan,
    /// non-review sessions.
    #[test]
    fn goal_tools_are_registered_for_persisted_default_session() {
        use browser_use_store::Store;

        let config = ProviderRunConfig::new(ProviderBackend::Fake, "fake-model");
        let dir = tempfile::tempdir().expect("tempdir");
        let store: SharedStore = Arc::new(std::sync::Mutex::new(
            Store::open(dir.path()).expect("open store"),
        ));
        let session_id = store
            .lock()
            .unwrap()
            .create_session(None, dir.path())
            .expect("create session row")
            .id;
        let dispatcher = build_tool_dispatcher(
            Arc::new(MarkerPythonBackend),
            &config,
            Some((store, SessionId(session_id))),
        );
        let names: Vec<&str> = dispatcher
            .tool_specs()
            .iter()
            .map(|s| s.name.as_str())
            .collect();
        for tool in ["get_goal", "create_goal", "update_goal"] {
            assert!(
                names.contains(&tool),
                "{tool} must be registered in the production dispatcher; got {names:?}"
            );
        }
    }

    #[test]
    fn parent_agent_config_layer_carries_live_parent_overrides() {
        let options = crate::config_overrides::AgentRunOptions {
            developer_instructions: Some("base developer".to_string()),
            config_overrides: vec![
                (
                    "developer_instructions".to_string(),
                    toml::Value::String("override developer".to_string()),
                ),
                (
                    "reasoning_effort".to_string(),
                    toml::Value::String("high".to_string()),
                ),
                (
                    "service_tier".to_string(),
                    toml::Value::String("priority".to_string()),
                ),
                (
                    "tool_allowlist".to_string(),
                    toml::Value::Array(vec![toml::Value::String("shell".to_string())]),
                ),
                ("can_write".to_string(), toml::Value::Boolean(false)),
            ],
            ..crate::config_overrides::AgentRunOptions::default()
        };
        let config =
            ProviderRunConfig::new(ProviderBackend::Openai, "gpt-5.5").with_options(options);

        let layer = parent_agent_config_layer(&config, &std::env::temp_dir());

        assert_eq!(layer.model, "gpt-5.5");
        assert_eq!(layer.provider, "openai");
        assert_eq!(layer.instructions, "override developer");
        assert_eq!(layer.reasoning_effort.as_deref(), Some("high"));
        assert_eq!(layer.service_tier.as_deref(), Some("priority"));
        assert_eq!(layer.tool_allowlist, vec!["shell"]);
        assert!(!layer.can_write);
    }

    #[test]
    fn child_run_config_overrides_snapshot_parent_runtime_and_child_role() {
        let options = crate::config_overrides::AgentRunOptions::default()
            .with_browser_mode("managed-headless")
            .with_compact_prompt("compact child this way")
            .with_model_auto_compact_token_limit(Some(1234))
            .with_model_auto_compact_token_limit_scope(
                crate::decision::AutoCompactTokenLimitScope::BodyAfterPrefix,
            )
            .with_approval_policy(AskForApproval::UnlessTrusted)
            .with_guardian(true)
            .with_config_overrides(vec![(
                "developer_instructions".to_string(),
                toml::Value::String("parent developer".to_string()),
            )]);
        let parent =
            ProviderRunConfig::new(ProviderBackend::Openai, "gpt-5.5").with_options(options);
        let mut child = parent_agent_config_layer(&parent, &std::env::temp_dir());
        child.instructions = "role developer".to_string();
        child.provider = "anthropic".to_string();
        child.service_tier = Some("priority".to_string());
        child.tool_allowlist = vec!["shell".to_string()];
        child.can_write = false;
        child.config_overrides.push((
            "custom_child_key".to_string(),
            toml::Value::String("custom".to_string()),
        ));

        let overrides = child_run_config_overrides(&parent, &child);
        let lookup = |key: &str| {
            overrides
                .iter()
                .rev()
                .find(|(candidate, _)| candidate == key)
                .map(|(_, value)| value)
        };

        assert_eq!(
            lookup("browser_mode").and_then(toml::Value::as_str),
            Some("managed-headless")
        );
        assert_eq!(
            lookup("compact_prompt").and_then(toml::Value::as_str),
            Some("compact child this way")
        );
        assert_eq!(
            lookup("model_auto_compact_token_limit").and_then(toml::Value::as_integer),
            Some(1234)
        );
        assert_eq!(
            lookup("model_auto_compact_token_limit_scope").and_then(toml::Value::as_str),
            Some("body_after_prefix")
        );
        assert_eq!(
            lookup("approval_policy").and_then(toml::Value::as_str),
            Some("unless-trusted")
        );
        assert_eq!(
            lookup("use_guardian").and_then(toml::Value::as_bool),
            Some(true)
        );
        assert_eq!(
            lookup("developer_instructions").and_then(toml::Value::as_str),
            Some("role developer")
        );
        assert_eq!(
            lookup("model_provider").and_then(toml::Value::as_str),
            Some("anthropic")
        );
        assert_eq!(
            lookup("service_tier").and_then(toml::Value::as_str),
            Some("priority")
        );
        assert_eq!(
            lookup("can_write").and_then(toml::Value::as_bool),
            Some(false)
        );
        assert_eq!(
            lookup("custom_child_key").and_then(toml::Value::as_str),
            Some("custom")
        );
    }

    #[test]
    fn goal_tools_are_hidden_without_persisted_session() {
        let config = ProviderRunConfig::new(ProviderBackend::Fake, "fake-model");
        let dispatcher = build_tool_dispatcher(Arc::new(MarkerPythonBackend), &config, None);
        let names: Vec<&str> = dispatcher
            .tool_specs()
            .iter()
            .map(|s| s.name.as_str())
            .collect();
        for tool in ["get_goal", "create_goal", "update_goal"] {
            assert!(
                !names.contains(&tool),
                "{tool} must not be registered without a persisted session; got {names:?}"
            );
        }
    }

    #[test]
    fn deprecated_plan_mode_keeps_goal_tools_available() {
        use browser_use_store::Store;

        let options = crate::config_overrides::AgentRunOptions::default()
            .with_collaboration_mode(crate::prompts::CollaborationModeKind::Plan);
        let config =
            ProviderRunConfig::new(ProviderBackend::Fake, "fake-model").with_options(options);
        let dir = tempfile::tempdir().expect("tempdir");
        let store: SharedStore = Arc::new(std::sync::Mutex::new(
            Store::open(dir.path()).expect("open store"),
        ));
        let session_id = store
            .lock()
            .unwrap()
            .create_session(None, dir.path())
            .expect("create session row")
            .id;
        let dispatcher = build_tool_dispatcher(
            Arc::new(MarkerPythonBackend),
            &config,
            Some((store, SessionId(session_id))),
        );
        let names: Vec<&str> = dispatcher
            .tool_specs()
            .iter()
            .map(|s| s.name.as_str())
            .collect();
        for tool in ["get_goal", "create_goal", "update_goal"] {
            assert!(
                names.contains(&tool),
                "{tool} must stay registered because plan mode is no longer a separate runtime; got {names:?}"
            );
        }
    }

    #[test]
    fn goal_tools_are_hidden_for_review_restricted_sessions() {
        use browser_use_store::Store;

        let config = ProviderRunConfig::new(ProviderBackend::Fake, "fake-model");
        let dir = tempfile::tempdir().expect("tempdir");
        let store: SharedStore = Arc::new(std::sync::Mutex::new(
            Store::open(dir.path()).expect("open store"),
        ));
        let session_id = store
            .lock()
            .unwrap()
            .create_session(None, dir.path())
            .expect("create session row")
            .id;
        store
            .lock()
            .unwrap()
            .append_event(
                &session_id,
                "session.review_mode",
                serde_json::json!({
                    "kind": "review",
                    "review_tool_restrictions": { "goals": false },
                }),
            )
            .expect("append review marker");
        let dispatcher = build_tool_dispatcher(
            Arc::new(MarkerPythonBackend),
            &config,
            Some((store, SessionId(session_id))),
        );
        let names: Vec<&str> = dispatcher
            .tool_specs()
            .iter()
            .map(|s| s.name.as_str())
            .collect();
        for tool in ["get_goal", "create_goal", "update_goal"] {
            assert!(
                !names.contains(&tool),
                "{tool} must not be registered for review-restricted sessions; got {names:?}"
            );
        }
    }

    /// A `create_goal` call routes from the PRODUCTION registration through the
    /// dispatcher (BY NAME via `dispatch_ordered`, the same path the turn loop
    /// uses for a model tool-call) into the shared `GoalStore`: a follow-up
    /// `get_goal` observes the goal, AND a durable `goal.created` event lands in
    /// the session journal (proving durable projection is wired, not just the
    /// tool listed). `update_goal` then emits a durable `goal.updated`.
    #[tokio::test]
    async fn create_goal_routes_into_store_and_emits_durable_event() {
        use browser_use_llm::schema::{ContentPart, MessageRole};
        use browser_use_store::Store;
        use tokio_util::sync::CancellationToken;

        // Dispatch a single tool call BY NAME and return its JSON tool output.
        async fn dispatch_one(
            dispatcher: &RealToolDispatcher,
            name: &str,
            input: serde_json::Value,
        ) -> serde_json::Value {
            let call = ContentPart::ToolCall {
                id: format!("call-{name}"),
                name: name.to_string(),
                input,
                provider_metadata: None,
            };
            let result = dispatcher
                .dispatch_ordered(vec![call], CancellationToken::new())
                .await;
            let msg = result
                .outputs_in_order
                .into_iter()
                .next()
                .expect("one tool output");
            assert_eq!(msg.role, MessageRole::Tool);
            let text = msg
                .content
                .iter()
                .find_map(|p| match p {
                    ContentPart::ToolResult {
                        content, is_error, ..
                    } => {
                        assert!(!*is_error, "tool call must succeed: {content:?}");
                        content.iter().find_map(|c| match c {
                            ContentPart::Text { text } => Some(text.clone()),
                            _ => None,
                        })
                    }
                    _ => None,
                })
                .expect("a tool-result text part");
            serde_json::from_str(&text).expect("tool output is JSON")
        }

        // Read every persisted `event_type` for the session from the store.
        fn event_types(store: &SharedStore, session_id: &str) -> Vec<String> {
            store
                .lock()
                .unwrap()
                .events_for_session(session_id)
                .expect("read events")
                .into_iter()
                .map(|e| e.event_type)
                .collect()
        }

        let config = ProviderRunConfig::new(ProviderBackend::Fake, "fake-model");
        // Real durable store on a tempdir; create the session ROW first (the
        // store MINTS the id) so the store-backed sink's `append_event`
        // satisfies the events FK on `sessions(id)`.
        let dir = tempfile::tempdir().expect("tempdir");
        let store: SharedStore = Arc::new(std::sync::Mutex::new(
            Store::open(dir.path()).expect("open store"),
        ));
        let session_id = store
            .lock()
            .unwrap()
            .create_session(None, dir.path())
            .expect("create session row")
            .id;
        let session = SessionId(session_id.clone());
        let dispatcher = build_tool_dispatcher(
            Arc::new(MarkerPythonBackend),
            &config,
            Some((store.clone(), session.clone())),
        );

        // create_goal routes into the goal store and returns the folded snapshot.
        let created = dispatch_one(
            &dispatcher,
            "create_goal",
            serde_json::json!({"objective": "ship the goals row", "token_budget": 1000}),
        )
        .await;
        assert_eq!(created["goal"]["objective"], "ship the goals row");
        assert_eq!(created["goal"]["status"], "active");
        assert_eq!(created["goal"]["tokenBudget"], 1000);

        // get_goal observes the SAME shared state (the store is shared).
        let fetched = dispatch_one(&dispatcher, "get_goal", serde_json::json!({})).await;
        assert_eq!(fetched["goal"]["objective"], "ship the goals row");
        assert_eq!(fetched["goal"]["status"], "active");

        // A durable `goal.created` event landed for this session (proving the
        // store-backed sink — not just the tool listing — is wired).
        let kinds = event_types(&store, &session_id);
        assert!(
            kinds.iter().any(|k| k == "goal.created"),
            "expected a durable goal.created event, got: {kinds:?}"
        );

        // update_goal folds + emits a durable `goal.updated` event.
        let updated = dispatch_one(
            &dispatcher,
            "update_goal",
            serde_json::json!({"status": "complete"}),
        )
        .await;
        assert_eq!(updated["goal"]["status"], "complete");
        let kinds = event_types(&store, &session_id);
        assert!(
            kinds.iter().any(|k| k == "goal.updated"),
            "expected a durable goal.updated event, got: {kinds:?}"
        );
    }

    // ---- approval-policy routing (the LIVE approval/guardian path) -------------
    //
    // These prove the production dispatcher HONORS `config.options.approval_policy`,
    // routing each gated tool call through the orchestrator's REAL `GuardianApprover`
    // (no OS sandbox — permissive seam). They dispatch a `shell` `echo` call BY NAME
    // through the SAME `dispatch_ordered` path the turn loop uses, asserting the
    // policy decides whether the approver is consulted / can deny.

    /// Dispatch a single tool call through the production dispatcher and return
    /// the recorded tool-result `(text, is_error)`.
    async fn dispatch_call(
        dispatcher: &RealToolDispatcher,
        name: &str,
        input: serde_json::Value,
    ) -> (String, bool) {
        use browser_use_llm::schema::{ContentPart, MessageRole};
        use tokio_util::sync::CancellationToken;

        let call = ContentPart::ToolCall {
            id: format!("call-{name}"),
            name: name.to_string(),
            input,
            provider_metadata: None,
        };
        let result = dispatcher
            .dispatch_ordered(vec![call], CancellationToken::new())
            .await;
        let msg = result
            .outputs_in_order
            .into_iter()
            .next()
            .expect("one tool output");
        assert_eq!(msg.role, MessageRole::Tool);
        msg.content
            .iter()
            .find_map(|p| match p {
                ContentPart::ToolResult {
                    content, is_error, ..
                } => {
                    let text = content
                        .iter()
                        .find_map(|c| match c {
                            ContentPart::Text { text } => Some(text.clone()),
                            _ => None,
                        })
                        .unwrap_or_default();
                    Some((text, *is_error))
                }
                _ => None,
            })
            .expect("a tool-result part")
    }

    #[tokio::test]
    async fn production_dispatcher_uses_supplied_session_cwd() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config = ProviderRunConfig::new(ProviderBackend::Fake, "fake-model");
        let dispatcher = build_tool_dispatcher_with_cwd(
            Arc::new(MarkerPythonBackend),
            &config,
            None,
            dir.path().to_path_buf(),
            dir.path().join("artifacts"),
            Arc::new(NoopEventSink),
        );

        let (text, is_error) = dispatch_call(
            &dispatcher,
            "shell",
            serde_json::json!({ "command": ["pwd"] }),
        )
        .await;
        assert!(!is_error, "pwd should run successfully: {text}");
        assert!(
            text.contains(&dir.path().display().to_string()),
            "tool cwd must be the supplied session cwd, got: {text}"
        );
    }

    #[tokio::test]
    async fn production_exec_command_calls_overlap() {
        use browser_use_llm::schema::{ContentPart, MessageRole};
        use std::time::{Duration, Instant};
        use tokio_util::sync::CancellationToken;

        let dir = tempfile::tempdir().expect("tempdir");
        let config = ProviderRunConfig::new(ProviderBackend::Fake, "fake-model");
        let dispatcher = build_tool_dispatcher_with_cwd(
            Arc::new(MarkerPythonBackend),
            &config,
            None,
            dir.path().to_path_buf(),
            dir.path().join("artifacts"),
            Arc::new(NoopEventSink),
        );

        let call = |id: &str, label: &str| ContentPart::ToolCall {
            id: id.to_string(),
            name: "exec_command".to_string(),
            input: serde_json::json!({
                "command": ["sh", "-c", format!("sleep 1; printf {label}")],
                "yield_time_ms": 1500,
            }),
            provider_metadata: None,
        };

        let started = Instant::now();
        let result = dispatcher
            .dispatch_ordered(
                vec![call("call-exec-a", "first"), call("call-exec-b", "second")],
                CancellationToken::new(),
            )
            .await;
        let elapsed = started.elapsed();

        assert_eq!(result.outputs_in_order.len(), 2);
        assert!(
            elapsed < Duration::from_millis(1800),
            "two 1s exec_command calls should overlap; elapsed={elapsed:?}"
        );

        for (msg, expected) in result.outputs_in_order.iter().zip(["first", "second"]) {
            assert_eq!(msg.role, MessageRole::Tool);
            let (text, is_error) = msg
                .content
                .iter()
                .find_map(|part| match part {
                    ContentPart::ToolResult {
                        content, is_error, ..
                    } => {
                        let text = content
                            .iter()
                            .find_map(|part| match part {
                                ContentPart::Text { text } => Some(text.clone()),
                                _ => None,
                            })
                            .unwrap_or_default();
                        Some((text, *is_error))
                    }
                    _ => None,
                })
                .expect("tool result");
            assert!(!is_error, "exec_command failed: {text}");
            assert!(
                text.contains(expected),
                "exec_command output should contain {expected:?}, got {text:?}"
            );
        }
    }

    #[tokio::test]
    async fn runtime_backed_unified_exec_manager_is_shared_by_session_resource() {
        let (runtime, _journal) = browser_use_runtime::BrowserUseRuntime::memory();
        let handle = runtime.handle();
        let tempdir = tempfile::tempdir().expect("tempdir");
        let root = handle
            .create_root_agent(browser_use_runtime::CreateRootAgentRequest {
                cwd: tempdir.path().to_path_buf(),
                task: "root task".to_string(),
                max_concurrent_threads_per_session: 3,
            })
            .expect("root agent");
        let session_id = SessionId(root.session_id().as_str().to_string());

        let first = unified_exec_manager_for_runtime_or_session(Some(&handle), Some(&session_id))
            .expect("runtime exec manager resource");
        let started = first
            .spawn_process(crate::tools::unified_exec::SpawnProcessRequest {
                argv: vec!["sh".to_string(), "-c".to_string(), "sleep 2".to_string()],
                cwd: tempdir.path().to_path_buf(),
                env: std::collections::HashMap::new(),
                tty: false,
                yield_time_ms: 250,
                max_output_tokens: None,
                timeout_ms: Some(5_000),
                kill_on_cancel: true,
                call_id: "call-start".to_string(),
                tool_name: "exec_command".to_string(),
                emitter: None,
                cancel: None,
            })
            .await
            .expect("spawn process");
        assert!(started.running);

        let second = unified_exec_manager_for_runtime_or_session(Some(&handle), Some(&session_id))
            .expect("runtime exec manager resource");
        assert_eq!(
            second.terminate_all().await,
            1,
            "second provider borrow must see the process created by the first borrow"
        );
    }

    #[tokio::test]
    async fn runtime_backed_unified_exec_manager_is_isolated_between_agents() {
        let (runtime, _journal) = browser_use_runtime::BrowserUseRuntime::memory();
        let handle = runtime.handle();
        let tempdir = tempfile::tempdir().expect("tempdir");
        let first = handle
            .create_root_agent(browser_use_runtime::CreateRootAgentRequest {
                cwd: tempdir.path().to_path_buf(),
                task: "first task".to_string(),
                max_concurrent_threads_per_session: 3,
            })
            .expect("first root agent");
        let second = handle
            .create_root_agent(browser_use_runtime::CreateRootAgentRequest {
                cwd: tempdir.path().to_path_buf(),
                task: "second task".to_string(),
                max_concurrent_threads_per_session: 3,
            })
            .expect("second root agent");
        let first_session_id = SessionId(first.session_id().as_str().to_string());
        let second_session_id = SessionId(second.session_id().as_str().to_string());

        let first_manager =
            unified_exec_manager_for_runtime_or_session(Some(&handle), Some(&first_session_id))
                .expect("first runtime exec manager resource");
        let second_manager =
            unified_exec_manager_for_runtime_or_session(Some(&handle), Some(&second_session_id))
                .expect("second runtime exec manager resource");
        let started = first_manager
            .spawn_process(crate::tools::unified_exec::SpawnProcessRequest {
                argv: vec!["sh".to_string(), "-c".to_string(), "sleep 2".to_string()],
                cwd: tempdir.path().to_path_buf(),
                env: std::collections::HashMap::new(),
                tty: false,
                yield_time_ms: 250,
                max_output_tokens: None,
                timeout_ms: Some(5_000),
                kill_on_cancel: true,
                call_id: "call-start".to_string(),
                tool_name: "exec_command".to_string(),
                emitter: None,
                cancel: None,
            })
            .await
            .expect("spawn process");
        assert!(started.running);

        let cross_agent = second_manager
            .write_stdin(crate::tools::unified_exec::WriteStdinRequest {
                session_id: started.session_id,
                chars: String::new(),
                yield_time_ms: 250,
                max_output_tokens: None,
                call_id: "call-cross-agent".to_string(),
                tool_name: "write_stdin".to_string(),
                emitter: None,
            })
            .await
            .expect_err("second agent must not access first agent process id");
        let cross_agent_debug = format!("{cross_agent:?}");
        assert!(
            cross_agent_debug.contains("unknown session"),
            "unexpected cross-agent write_stdin error: {cross_agent_debug}"
        );
        assert_eq!(
            first_manager.terminate_all().await,
            1,
            "first manager should still own the original process"
        );
        assert_eq!(
            second_manager.terminate_all().await,
            0,
            "second manager should not own first manager's process"
        );
    }

    #[test]
    fn runtime_unattached_unified_exec_does_not_use_global_fallback() {
        let (runtime, _journal) = browser_use_runtime::BrowserUseRuntime::memory();
        let handle = runtime.handle();
        let session_id = SessionId("missing-runtime-session".to_string());
        unified_exec_managers()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();

        let err = unified_exec_manager_for_runtime_or_session(Some(&handle), Some(&session_id))
            .expect_err("unattached runtime session should fail loudly");
        assert!(
            matches!(err, ProviderResolveError::RuntimeResource(_)),
            "expected runtime resource error, got {err:?}"
        );

        assert!(
            !unified_exec_managers()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .contains_key(session_id.as_str()),
            "runtime-backed lookup failures must not silently fall back to the legacy global manager"
        );
    }

    #[test]
    fn runtime_backed_browser_backend_is_shared_by_session_resource() {
        let (runtime, _journal) = browser_use_runtime::BrowserUseRuntime::memory();
        let handle = runtime.handle();
        let tempdir = tempfile::tempdir().expect("tempdir");
        let root = handle
            .create_root_agent(browser_use_runtime::CreateRootAgentRequest {
                cwd: tempdir.path().to_path_buf(),
                task: "root task".to_string(),
                max_concurrent_threads_per_session: 3,
            })
            .expect("root agent");
        let session_id = SessionId(root.session_id().as_str().to_string());
        let options = crate::config_overrides::AgentRunOptions {
            browser_mode: Some("managed-headless".to_string()),
            ..crate::config_overrides::AgentRunOptions::default()
        };
        let config =
            ProviderRunConfig::new(ProviderBackend::Fake, "fake-model").with_options(options);

        let first =
            browser_backend_for_runtime_or_config(&config, Some(&handle), Some(&session_id))
                .expect("runtime browser backend resource");
        let second =
            browser_backend_for_runtime_or_config(&config, Some(&handle), Some(&session_id))
                .expect("runtime browser backend resource");

        assert!(
            Arc::ptr_eq(&first, &second),
            "provider borrows for the same runtime session should reuse the browser backend"
        );
        first
            .command(
                session_id.as_str(),
                tempdir.path(),
                tempdir.path(),
                "browser status --json",
            )
            .expect("browser status should create a browser-use-browser session");
        assert!(
            !browser_use_browser::BrowserSessionRegistry::global()
                .contains_session(session_id.as_str()),
            "runtime-backed browser backend must not create sessions in the global browser registry"
        );
        let browser_snapshots = handle.snapshot().browsers;
        assert_eq!(browser_snapshots.len(), 1);
        assert_eq!(
            browser_snapshots[0].status,
            browser_use_runtime::BrowserStatus::Released,
            "browser calls should claim and release the runtime browser lease"
        );
        assert!(
            browser_snapshots[0].active_agent_id.is_none(),
            "browser lease should not remain active after a synchronous browser command"
        );
        assert_eq!(
            handle
                .cleanup_session_resources(root.session_id())
                .expect("cleanup browser runtime resource"),
            1,
            "runtime resource cleanup should close the underlying browser session"
        );
    }

    #[test]
    fn runtime_unattached_browser_backend_fails_loudly() {
        let (runtime, _journal) = browser_use_runtime::BrowserUseRuntime::memory();
        let handle = runtime.handle();
        let session_id = SessionId("missing-runtime-session".to_string());
        let config = ProviderRunConfig::new(ProviderBackend::Fake, "fake-model");

        let err = match browser_backend_for_runtime_or_config(
            &config,
            Some(&handle),
            Some(&session_id),
        ) {
            Ok(_) => panic!("unattached runtime session should fail loudly"),
            Err(err) => err,
        };

        assert!(
            matches!(err, ProviderResolveError::RuntimeResource(_)),
            "expected runtime resource error, got {err:?}"
        );
        assert!(
            handle.snapshot().browsers.is_empty(),
            "failed runtime browser resource attachment must not create browser handles"
        );
    }

    #[test]
    fn runtime_backed_mcp_client_is_shared_by_session_resource() {
        let (runtime, _journal) = browser_use_runtime::BrowserUseRuntime::memory();
        let handle = runtime.handle();
        let tempdir = tempfile::tempdir().expect("tempdir");
        let root = handle
            .create_root_agent(browser_use_runtime::CreateRootAgentRequest {
                cwd: tempdir.path().to_path_buf(),
                task: "root task".to_string(),
                max_concurrent_threads_per_session: 3,
            })
            .expect("root agent");
        let session_id = SessionId(root.session_id().as_str().to_string());
        let mut servers = std::collections::HashMap::new();
        servers.insert(
            "missing".to_string(),
            crate::mcp::McpServerConfig {
                transport: crate::mcp::McpServerTransport::Stdio {
                    command: "__browser_use_missing_mcp_test_command__".to_string(),
                    args: Vec::new(),
                    env: std::collections::HashMap::new(),
                    cwd: None,
                },
                startup_timeout_ms: Some(50),
                tool_timeout_ms: Some(50),
                enabled_tools: None,
                disabled_tools: None,
            },
        );
        let options = crate::config_overrides::AgentRunOptions {
            mcp_servers: servers,
            ..crate::config_overrides::AgentRunOptions::default()
        };
        let config =
            ProviderRunConfig::new(ProviderBackend::Fake, "fake-model").with_options(options);

        let first = mcp_client_for_runtime_or_config(&config, Some(&handle), Some(&session_id))
            .expect("first mcp resource");
        let second = mcp_client_for_runtime_or_config(&config, Some(&handle), Some(&session_id))
            .expect("second mcp resource");

        assert!(
            Arc::ptr_eq(&first, &second),
            "provider borrows for the same runtime session should reuse the MCP manager"
        );
        assert_eq!(
            first.startup_errors.len(),
            1,
            "startup errors should be retained with the runtime-owned MCP resource"
        );
    }

    #[test]
    fn runtime_unattached_mcp_client_fails_loudly() {
        let (runtime, _journal) = browser_use_runtime::BrowserUseRuntime::memory();
        let handle = runtime.handle();
        let session_id = SessionId("missing-runtime-session".to_string());
        let mut servers = std::collections::HashMap::new();
        servers.insert(
            "missing".to_string(),
            crate::mcp::McpServerConfig {
                transport: crate::mcp::McpServerTransport::Stdio {
                    command: "__browser_use_missing_mcp_test_command__".to_string(),
                    args: Vec::new(),
                    env: std::collections::HashMap::new(),
                    cwd: None,
                },
                startup_timeout_ms: Some(50),
                tool_timeout_ms: Some(50),
                enabled_tools: None,
                disabled_tools: None,
            },
        );
        let options = crate::config_overrides::AgentRunOptions {
            mcp_servers: servers,
            ..crate::config_overrides::AgentRunOptions::default()
        };
        let config =
            ProviderRunConfig::new(ProviderBackend::Fake, "fake-model").with_options(options);

        let err = match mcp_client_for_runtime_or_config(&config, Some(&handle), Some(&session_id))
        {
            Ok(_) => panic!("unattached runtime session should fail loudly"),
            Err(err) => err,
        };

        assert!(
            err.to_string().contains("unknown agent"),
            "unexpected mcp error: {err}"
        );
    }

    /// `AskForApproval::Never` (the default) AUTO-APPROVES: a gated `shell` call
    /// runs with NO prompt and NO denial — the approver is never consulted. This
    /// is the preserved-default-behavior proof.
    #[tokio::test]
    async fn never_policy_auto_approves_shell_call() {
        // Default config => approval_policy = Never, use_guardian = false.
        let config = ProviderRunConfig::new(ProviderBackend::Fake, "fake-model");
        assert_eq!(config.options.approval_policy, AskForApproval::Never);
        let dispatcher = build_tool_dispatcher(Arc::new(MarkerPythonBackend), &config, None);

        let (text, is_error) = dispatch_call(
            &dispatcher,
            "shell",
            serde_json::json!({ "command": ["echo", "hi"] }),
        )
        .await;
        assert!(
            !is_error,
            "Never policy must auto-approve (run the tool), got error: {text}"
        );
        assert!(
            text.contains("hi"),
            "the shell tool must have actually run under Never, got: {text}"
        );
    }

    /// A NON-`Never` policy ROUTES the gated call to the real approver, which —
    /// with the guardian gate enabled — DENIES it. Proves the policy is honored
    /// (the call is no longer hardcoded auto-approve) AND the approver can block.
    #[tokio::test]
    async fn non_never_policy_routes_to_approver_and_can_deny() {
        // UnlessTrusted always requires approval; use_guardian => the guardian's
        // reviewer denies, so the gated call is rejected.
        let options = crate::config_overrides::AgentRunOptions::default()
            .with_approval_policy(AskForApproval::UnlessTrusted)
            .with_guardian(true);
        let config =
            ProviderRunConfig::new(ProviderBackend::Fake, "fake-model").with_options(options);
        let dispatcher = build_tool_dispatcher(Arc::new(MarkerPythonBackend), &config, None);

        let (text, is_error) = dispatch_call(
            &dispatcher,
            "shell",
            serde_json::json!({ "command": ["echo", "hi"] }),
        )
        .await;
        assert!(
            is_error,
            "a non-Never policy with the guardian denying must reject the call; got ok: {text}"
        );
        assert!(
            text.contains("rejected"),
            "the rejection must surface the approver's denial, got: {text}"
        );
        assert!(
            !text.contains("hi"),
            "a denied shell call must NOT have executed, got: {text}"
        );
    }

    /// A NON-`Never` policy with the guardian DISABLED still routes to the real
    /// approver — which ALLOWS — so the gated call runs. This isolates "routing is
    /// live" from "guardian denies": the approver IS consulted (not bypassed), and
    /// the permissive reviewer permits.
    #[tokio::test]
    async fn non_never_policy_routes_to_approver_and_can_allow() {
        let options = crate::config_overrides::AgentRunOptions::default()
            .with_approval_policy(AskForApproval::UnlessTrusted)
            .with_guardian(false);
        let config =
            ProviderRunConfig::new(ProviderBackend::Fake, "fake-model").with_options(options);
        let dispatcher = build_tool_dispatcher(Arc::new(MarkerPythonBackend), &config, None);

        let (text, is_error) = dispatch_call(
            &dispatcher,
            "shell",
            serde_json::json!({ "command": ["echo", "hi"] }),
        )
        .await;
        assert!(
            !is_error,
            "the permissive guardian must approve the routed call, got error: {text}"
        );
        assert!(
            text.contains("hi"),
            "the approved shell call must have executed, got: {text}"
        );
    }
}
