use std::collections::HashMap;
use std::path::PathBuf;

use codex_app_server_protocol::{ThreadStartParams, TurnStartParams, UserInput};
use serde_json::{json, Value};

use crate::OPENAI_API_PROVIDER_ID;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CodexThreadStartSpec {
    pub model: String,
    pub model_provider: String,
    pub cwd: PathBuf,
    pub base_instructions: Option<String>,
    pub developer_instructions: Option<String>,
    pub browser_harness_env: HashMap<String, String>,
    pub ephemeral: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CodexTurnStartSpec {
    pub thread_id: String,
    pub user_text: String,
    pub cwd: Option<PathBuf>,
    pub client_user_message_id: Option<String>,
}

pub fn browser_harness_env_config(env: &HashMap<String, String>) -> HashMap<String, Value> {
    let mut config = HashMap::new();
    if !env.is_empty() {
        config.insert(
            "shell_environment_policy".to_string(),
            json!({
                "inherit": "all",
                "set": env,
            }),
        );
    }
    config
}

fn api_key_provider_config(model_provider: &str) -> HashMap<String, Value> {
    if model_provider != OPENAI_API_PROVIDER_ID {
        return HashMap::new();
    }
    HashMap::from([(
        format!("model_providers.{OPENAI_API_PROVIDER_ID}"),
        json!({
            "name": "OpenAI",
            "base_url": "https://api.openai.com/v1",
            "env_key": "OPENAI_API_KEY",
            "wire_api": "responses",
            "requires_openai_auth": false,
            "supports_websockets": true,
            "http_headers": {
                "version": env!("CARGO_PKG_VERSION"),
            },
        }),
    )])
}

pub fn thread_config_overrides(spec: &CodexThreadStartSpec) -> Option<HashMap<String, Value>> {
    let mut config = browser_harness_env_config(&spec.browser_harness_env);
    config.extend(api_key_provider_config(&spec.model_provider));
    (!config.is_empty()).then_some(config)
}

pub fn thread_start_params(spec: CodexThreadStartSpec) -> ThreadStartParams {
    let config = thread_config_overrides(&spec);
    ThreadStartParams {
        model: Some(spec.model),
        model_provider: Some(spec.model_provider),
        cwd: Some(spec.cwd.display().to_string()),
        config,
        base_instructions: spec.base_instructions,
        developer_instructions: spec.developer_instructions,
        ephemeral: Some(spec.ephemeral),
        experimental_raw_events: true,
        ..ThreadStartParams::default()
    }
}

pub fn turn_start_params(spec: CodexTurnStartSpec) -> TurnStartParams {
    TurnStartParams {
        thread_id: spec.thread_id,
        input: vec![UserInput::Text {
            text: spec.user_text,
            text_elements: Vec::new(),
        }],
        cwd: spec.cwd,
        ..TurnStartParams::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thread_start_enables_raw_events_and_shell_env() {
        let mut env = HashMap::new();
        env.insert("BH_MANAGER_SOCKET".to_string(), "/tmp/bh.sock".to_string());
        env.insert("BH_RUN_ID".to_string(), "run-1".to_string());

        let params = thread_start_params(CodexThreadStartSpec {
            model: "gpt-5.5".to_string(),
            model_provider: "codex".to_string(),
            cwd: PathBuf::from("/tmp/work"),
            base_instructions: Some("base".to_string()),
            developer_instructions: Some("dev".to_string()),
            browser_harness_env: env,
            ephemeral: true,
        });

        assert_eq!(params.model.as_deref(), Some("gpt-5.5"));
        assert_eq!(params.model_provider.as_deref(), Some("codex"));
        assert_eq!(params.cwd.as_deref(), Some("/tmp/work"));
        assert_eq!(params.base_instructions.as_deref(), Some("base"));
        assert_eq!(params.developer_instructions.as_deref(), Some("dev"));
        assert_eq!(params.ephemeral, Some(true));
        assert!(params.experimental_raw_events);

        let config = params.config.expect("env config");
        assert_eq!(
            config["shell_environment_policy"],
            json!({
                "inherit": "all",
                "set": {
                    "BH_MANAGER_SOCKET": "/tmp/bh.sock",
                    "BH_RUN_ID": "run-1",
                }
            })
        );
    }

    #[test]
    fn thread_start_injects_openai_api_provider_for_api_key_runs() {
        let params = thread_start_params(CodexThreadStartSpec {
            model: "gpt-5.5".to_string(),
            model_provider: OPENAI_API_PROVIDER_ID.to_string(),
            cwd: PathBuf::from("/tmp/work"),
            base_instructions: None,
            developer_instructions: None,
            browser_harness_env: HashMap::new(),
            ephemeral: true,
        });

        let config = params.config.expect("api provider config");
        let provider = &config[&format!("model_providers.{OPENAI_API_PROVIDER_ID}")];
        assert_eq!(provider["env_key"], json!("OPENAI_API_KEY"));
        assert_eq!(provider["requires_openai_auth"], json!(false));
        assert_eq!(provider["base_url"], json!("https://api.openai.com/v1"));
    }

    #[test]
    fn turn_start_uses_text_input() {
        let params = turn_start_params(CodexTurnStartSpec {
            thread_id: "thread-1".to_string(),
            user_text: "find the price".to_string(),
            cwd: Some(PathBuf::from("/tmp/task")),
            client_user_message_id: Some("msg-1".to_string()),
        });

        assert_eq!(params.thread_id, "thread-1");
        assert_eq!(
            params.cwd.as_deref(),
            Some(PathBuf::from("/tmp/task").as_path())
        );
        assert_eq!(params.input.len(), 1);
        match &params.input[0] {
            UserInput::Text {
                text,
                text_elements,
            } => {
                assert_eq!(text, "find the price");
                assert!(text_elements.is_empty());
            }
            other => panic!("expected text input, got {other:?}"),
        }
    }
}
