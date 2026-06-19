use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::anyhow;
use anyhow::Context;
use anyhow::Result;
use codex_app_server_client::legacy_core::config::Config;
use codex_app_server_client::legacy_core::config::ConfigBuilder;
use codex_app_server_client::EnvironmentManager;
use codex_app_server_client::InProcessAppServerClient;
use codex_app_server_client::InProcessClientStartArgs;
use codex_app_server_protocol::AskForApproval;
use codex_app_server_protocol::ClientRequest;
use codex_app_server_protocol::JSONRPCErrorError;
use codex_app_server_protocol::RequestId;
use codex_app_server_protocol::SandboxMode;
use codex_app_server_protocol::ServerRequest;
use codex_app_server_protocol::ThreadStartResponse;
use codex_app_server_protocol::TurnStartResponse;
use codex_arg0::Arg0DispatchPaths;
use codex_config::CloudRequirementsLoader;
use codex_config::LoaderOverrides;
use codex_feedback::CodexFeedback;
use codex_protocol::protocol::SessionSource;
use serde_json::json;
use tokio_util::sync::CancellationToken;

use crate::thread_start_params;
use crate::turn_start_params;
use crate::CodexEventMapper;
use crate::CodexProjectedEvent;
use crate::CodexTerminalOutcome;
use crate::CodexThreadStartSpec;
use crate::CodexTurnStartSpec;

#[derive(Clone, Debug)]
pub struct CodexEngineRunSpec {
    pub session_id: String,
    pub model: String,
    pub model_provider: String,
    pub cwd: PathBuf,
    pub codex_home: Option<PathBuf>,
    pub base_instructions: Option<String>,
    pub developer_instructions: Option<String>,
    pub browser_harness_env: HashMap<String, String>,
    pub user_text: String,
    pub cancellation_token: Option<CancellationToken>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CodexEngineRunResult {
    pub thread_id: String,
    pub turn_id: String,
    pub final_result: String,
}

pub async fn run_codex_engine_turn(
    spec: CodexEngineRunSpec,
    mut emit: impl FnMut(CodexProjectedEvent) -> Result<()>,
) -> Result<CodexEngineRunResult> {
    let cancellation_token = spec
        .cancellation_token
        .clone()
        .unwrap_or_else(CancellationToken::new);
    let mut client = start_in_process_client(spec.codex_home.as_deref())
        .await
        .context("start Codex in-process app-server")?;

    let thread_response: ThreadStartResponse = client
        .request_typed(ClientRequest::ThreadStart {
            request_id: RequestId::Integer(1),
            params: thread_start_params(CodexThreadStartSpec {
                model: spec.model.clone(),
                model_provider: spec.model_provider.clone(),
                cwd: spec.cwd.clone(),
                base_instructions: spec.base_instructions.clone(),
                developer_instructions: spec.developer_instructions.clone(),
                browser_harness_env: spec.browser_harness_env.clone(),
                ephemeral: false,
            })
            .with_cautious_eval_defaults(),
        })
        .await
        .context("Codex thread/start")?;
    let thread_id = thread_response.thread.id.clone();
    emit(CodexProjectedEvent {
        event_type: "codex.thread.started".to_string(),
        payload: json!({
            "session_id": spec.session_id,
            "thread_id": thread_id,
            "model": spec.model,
            "model_provider": spec.model_provider,
            "source": "codex_app_server",
        }),
    })?;

    let turn_response: TurnStartResponse = client
        .request_typed(ClientRequest::TurnStart {
            request_id: RequestId::Integer(2),
            params: turn_start_params(CodexTurnStartSpec {
                thread_id: thread_id.clone(),
                user_text: spec.user_text,
                cwd: Some(spec.cwd),
                client_user_message_id: None,
            }),
        })
        .await
        .context("Codex turn/start")?;
    let turn_id = turn_response.turn.id.clone();
    emit(CodexProjectedEvent {
        event_type: "codex.turn.started".to_string(),
        payload: json!({
            "session_id": spec.session_id,
            "thread_id": thread_id,
            "turn_id": turn_id,
            "source": "codex_app_server",
        }),
    })?;

    let mut mapper = CodexEventMapper::new();
    let final_result = loop {
        tokio::select! {
            _ = cancellation_token.cancelled() => {
                let _ = client.request_typed::<codex_app_server_protocol::TurnInterruptResponse>(
                    ClientRequest::TurnInterrupt {
                        request_id: RequestId::Integer(3),
                        params: codex_app_server_protocol::TurnInterruptParams {
                            thread_id: thread_id.clone(),
                            turn_id: turn_id.clone(),
                        },
                    },
                ).await;
                emit(CodexProjectedEvent {
                    event_type: "session.cancelled".to_string(),
                    payload: json!({
                        "reason": "cancelled",
                        "thread_id": thread_id,
                        "turn_id": turn_id,
                        "source": "codex_app_server",
                    }),
                })?;
                let _ = client.shutdown().await;
                return Err(anyhow!("CodexEngine run cancelled"));
            }
            event = client.next_event() => {
                let Some(event) = event else {
                    let _ = client.shutdown().await;
                    return Err(anyhow!("Codex app-server event stream ended before turn completion"));
                };
                let event: codex_app_server_client::AppServerEvent = event.into();
                emit(raw_projected_event(&event))?;
                if let codex_app_server_client::AppServerEvent::ServerRequest(request) = &event {
                    client
                        .reject_server_request(request.id().clone(), unsupported_server_request_error(request))
                        .await
                        .context("reject unsupported Codex app-server request")?;
                }
                let mapped = mapper.map_app_server_event(event);
                for projected in mapped.projected {
                    emit(projected)?;
                }
                if let Some(outcome) = mapped.terminal {
                    match outcome {
                        CodexTerminalOutcome::Completed { result } => {
                            break result;
                        }
                        CodexTerminalOutcome::Failed { error } => {
                            let _ = client.shutdown().await;
                            return Err(anyhow!(error));
                        }
                        CodexTerminalOutcome::Cancelled { reason } => {
                            let _ = client.shutdown().await;
                            return Err(anyhow!("CodexEngine run cancelled: {reason}"));
                        }
                    }
                }
            }
        }
    };

    client
        .shutdown()
        .await
        .context("shutdown Codex in-process app-server")?;

    Ok(CodexEngineRunResult {
        thread_id,
        turn_id,
        final_result,
    })
}

async fn start_in_process_client(codex_home: Option<&Path>) -> Result<InProcessAppServerClient> {
    let config = Arc::new(load_codex_config(codex_home).await?);
    let state_db = Some(
        codex_rollout::state_db::try_init(config.as_ref())
            .await
            .with_context(|| {
                format!(
                    "initialize Codex sqlite state under {}",
                    config.sqlite_home.display()
                )
            })?,
    );
    InProcessAppServerClient::start(InProcessClientStartArgs {
        arg0_paths: Arg0DispatchPaths::default(),
        config,
        cli_overrides: Vec::new(),
        loader_overrides: LoaderOverrides::default(),
        strict_config: false,
        cloud_requirements: CloudRequirementsLoader::default(),
        feedback: CodexFeedback::new(),
        log_db: None,
        state_db,
        environment_manager: Arc::new(EnvironmentManager::default_for_tests()),
        config_warnings: Vec::new(),
        session_source: SessionSource::Cli,
        enable_codex_api_key_env: true,
        client_name: "browser-use-terminal".to_string(),
        client_version: env!("CARGO_PKG_VERSION").to_string(),
        experimental_api: true,
        opt_out_notification_methods: Vec::new(),
        channel_capacity: 1024,
    })
    .await
    .map_err(Into::into)
}

async fn load_codex_config(codex_home: Option<&Path>) -> Result<Config> {
    let builder = match codex_home {
        Some(codex_home) => ConfigBuilder::default().codex_home(codex_home.to_path_buf()),
        None => ConfigBuilder::default(),
    };
    match builder.build().await {
        Ok(config) => Ok(config),
        Err(first_error) => match codex_home {
            Some(codex_home) => Config::load_default_with_cli_overrides_for_codex_home(
                codex_home.to_path_buf(),
                Vec::new(),
            )
            .await
            .with_context(|| format!("load Codex config after builder error: {first_error}")),
            None => Config::load_default_with_cli_overrides(Vec::new())
                .await
                .with_context(|| {
                    format!("load default Codex config after builder error: {first_error}")
                }),
        },
    }
}

fn raw_projected_event(event: &codex_app_server_client::AppServerEvent) -> CodexProjectedEvent {
    let payload = match event {
        codex_app_server_client::AppServerEvent::Lagged { skipped } => {
            json!({ "kind": "lagged", "skipped": skipped })
        }
        codex_app_server_client::AppServerEvent::ServerNotification(notification) => {
            json!({ "kind": "server_notification", "notification": notification })
        }
        codex_app_server_client::AppServerEvent::ServerRequest(request) => {
            json!({ "kind": "server_request", "request": request })
        }
        codex_app_server_client::AppServerEvent::Disconnected { message } => {
            json!({ "kind": "disconnected", "message": message })
        }
    };
    CodexProjectedEvent {
        event_type: "codex.raw_event".to_string(),
        payload,
    }
}

fn unsupported_server_request_error(request: &ServerRequest) -> JSONRPCErrorError {
    JSONRPCErrorError {
        code: -32001,
        message: format!("Browser Use CodexEngine does not handle server request {request:?}"),
        data: None,
    }
}

trait ThreadStartParamsExt {
    fn with_cautious_eval_defaults(self) -> codex_app_server_protocol::ThreadStartParams;
}

impl ThreadStartParamsExt for codex_app_server_protocol::ThreadStartParams {
    fn with_cautious_eval_defaults(mut self) -> codex_app_server_protocol::ThreadStartParams {
        self.approval_policy = Some(AskForApproval::Never);
        self.sandbox = Some(SandboxMode::DangerFullAccess);
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn loads_ephemeral_codex_config() {
        let dir = tempfile::tempdir().expect("temp dir");
        let config = load_codex_config(Some(dir.path()))
            .await
            .expect("config should load");
        assert_eq!(config.codex_home.as_ref(), dir.path());
    }

    #[test]
    fn thread_start_defaults_disable_approval_and_sandbox() {
        let params =
            codex_app_server_protocol::ThreadStartParams::default().with_cautious_eval_defaults();
        assert_eq!(params.approval_policy, Some(AskForApproval::Never));
        assert_eq!(params.sandbox, Some(SandboxMode::DangerFullAccess));
    }
}
