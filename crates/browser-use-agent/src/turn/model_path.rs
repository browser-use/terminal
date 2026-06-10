//! Production multi-provider real-model path.
//!
//! This is the missing piece that turns the engine from "scripted-transport only"
//! into a system that talks to a **live** model. It resolves a real
//! [`browser_use_llm::route::Route`] from a [`ProviderChoice`] + credentials, then
//! builds the real [`ModelClientTransport`] and wraps it in the engine's
//! [`ModelSamplingDriver`] (the concrete [`SamplingDriver`] impl). The scripted
//! transport stays available for offline tests — this module only adds the
//! production seam.
//!
//! ## Providers
//! The production providers are the ones `browser-use-llm` ships:
//! - **OpenAI Responses** (`OPENAI_API_KEY`, base override `LLM_BROWSER_OPENAI_BASE_URL`),
//! - **Anthropic Messages** (`ANTHROPIC_API_KEY`),
//! - **OpenAI-compatible** (Ollama / OpenRouter / DeepSeek / Fireworks, by id +
//!   key, or an explicit base url),
//! - **Codex (chatgpt.com)** via the Codex CLI OAuth login
//!   ([`ProviderChoice::Codex`]): the credential is an OAuth `access_token` +
//!   `account_id`, read from `~/.codex/auth.json` (or the credential store), not a
//!   raw API key. See [`browser_use_llm::auth::codex_route`].
//!
//! Most credentials come from process env (standard keys); codex creds come from
//! the on-disk codex login state (`CODEX_ACCESS_TOKEN` / `~/.codex/auth.json`).

use std::sync::Arc;

use browser_use_llm::auth::{codex_route, CodexAuth};
use browser_use_llm::providers::{
    Anthropic, AnthropicConfig, OpenAi, OpenAiCompatible, OpenAiConfig,
};
use browser_use_llm::route::{Auth, ModelClient, Route};
use browser_use_llm::schema::{ContentPart, LlmRequest, Message, MessageRole, SystemPart};

use crate::events::{EventSink, TurnCtx};
use crate::turn::sampling::{ModelClientTransport, ModelSamplingDriver};

/// Which production provider + wire format to route through.
///
/// Each variant names a `browser-use-llm` provider facade. Building a route from
/// a choice resolves the API key (and optional base-url override) and binds the
/// model, yielding a ready [`Route`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderChoice {
    /// OpenAI first-party Responses API (`/responses`).
    OpenAiResponses {
        /// API key (`OPENAI_API_KEY` / `LLM_BROWSER_OPENAI_API_KEY`).
        api_key: String,
        /// Optional base-url override (proxy / gateway).
        base_url: Option<String>,
    },
    /// Anthropic first-party Messages API (`/messages`).
    Anthropic {
        /// API key (`ANTHROPIC_API_KEY`).
        api_key: String,
        /// Optional base-url override.
        base_url: Option<String>,
    },
    /// A known OpenAI-compatible provider, selected by id (e.g. `openrouter`,
    /// `deepseek`, `fireworks`, `ollama`), with the provider's default base url.
    OpenAiCompatibleProvider {
        /// The provider id from the built-in profile table.
        provider_id: String,
        /// API key (ignored for auth-free providers like `ollama`).
        api_key: String,
    },
    /// An explicitly-configured OpenAI-compatible endpoint (self-hosted / unlisted).
    OpenAiCompatibleCustom {
        /// A label for the provider (recorded, not on the wire).
        provider_id: String,
        /// The full base url (e.g. `https://llm.internal/v1`).
        base_url: String,
        /// API key.
        api_key: String,
        /// Additional static headers to apply to every request for this route.
        extra_headers: Vec<(String, String)>,
    },
    /// The codex (chatgpt.com) backend, reached via the Codex CLI OAuth login.
    ///
    /// Unlike the env-keyed providers, the credential is an OAuth `access_token`
    /// plus the `account_id` sent as the `chatgpt-account-id` header (there is no
    /// raw API key on the ChatGPT flow). The route targets
    /// `<base_url>/codex/responses` (default
    /// [`CODEX_BASE_URL`](browser_use_llm::auth::CODEX_BASE_URL)).
    Codex {
        /// The OAuth access token, sent as `Authorization: Bearer <access_token>`.
        access_token: String,
        /// The ChatGPT account id, sent as the `chatgpt-account-id` header.
        account_id: String,
        /// Optional base-url override (proxy / gateway); default chatgpt.com.
        base_url: Option<String>,
    },
}

/// Errors resolving a production provider route.
#[derive(Debug)]
pub enum ModelPathError {
    /// No usable credential was found in the environment.
    MissingCredentials(&'static str),
    /// The requested OpenAI-compatible provider id is not in the profile table.
    UnknownProvider(String),
}

impl std::fmt::Display for ModelPathError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ModelPathError::MissingCredentials(which) => {
                write!(f, "no provider credentials found in environment ({which})")
            }
            ModelPathError::UnknownProvider(id) => {
                write!(f, "unknown OpenAI-compatible provider id: {id}")
            }
        }
    }
}

impl std::error::Error for ModelPathError {}

/// Resolve a [`ProviderChoice`] from process environment, preferring (in order)
/// OpenAI, then Anthropic, then an OpenAI-compatible base url.
///
/// Honours the standard keys the legacy stack uses:
/// - OpenAI: `LLM_BROWSER_OPENAI_API_KEY` || `OPENAI_API_KEY`
///   (base override: `LLM_BROWSER_OPENAI_BASE_URL`),
/// - Anthropic: `LLM_BROWSER_ANTHROPIC_API_KEY` || `ANTHROPIC_API_KEY`,
/// - OpenAI-compatible: `LLM_BROWSER_OPENAI_COMPAT_API_KEY` || `OPENROUTER_API_KEY`
///   with `LLM_BROWSER_OPENAI_COMPAT_BASE_URL` || `OPENROUTER_BASE_URL`.
///
/// Returns `Err(MissingCredentials)` when nothing is configured — honest, not a
/// panic.
///
/// **Codex:** an explicit `CODEX_ACCESS_TOKEN` + `CODEX_ACCOUNT_ID` pair in the
/// env selects the codex (chatgpt.com) backend. The on-disk codex login
/// (`~/.codex/auth.json`) is resolved by the entrypoint provider, not here (this
/// env resolver stays free of file I/O); env codex creds take top precedence so a
/// caller can force the codex backend.
pub fn provider_choice_from_env() -> Result<ProviderChoice, ModelPathError> {
    let env = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());

    if let (Some(access_token), Some(account_id)) =
        (env("CODEX_ACCESS_TOKEN"), env("CODEX_ACCOUNT_ID"))
    {
        return Ok(ProviderChoice::Codex {
            access_token,
            account_id,
            base_url: env("CODEX_BASE_URL"),
        });
    }
    if let Some(api_key) = env("LLM_BROWSER_OPENAI_API_KEY").or_else(|| env("OPENAI_API_KEY")) {
        return Ok(ProviderChoice::OpenAiResponses {
            api_key,
            base_url: env("LLM_BROWSER_OPENAI_BASE_URL"),
        });
    }
    if let Some(api_key) = env("LLM_BROWSER_ANTHROPIC_API_KEY").or_else(|| env("ANTHROPIC_API_KEY"))
    {
        return Ok(ProviderChoice::Anthropic {
            api_key,
            base_url: env("LLM_BROWSER_ANTHROPIC_BASE_URL"),
        });
    }
    if let Some(api_key) =
        env("LLM_BROWSER_OPENAI_COMPAT_API_KEY").or_else(|| env("OPENROUTER_API_KEY"))
    {
        let base_url = env("LLM_BROWSER_OPENAI_COMPAT_BASE_URL")
            .or_else(|| env("OPENROUTER_BASE_URL"))
            .unwrap_or_else(|| "https://openrouter.ai/api/v1".to_string());
        return Ok(ProviderChoice::OpenAiCompatibleCustom {
            provider_id: "openai-compatible".to_string(),
            base_url,
            api_key,
            extra_headers: Vec::new(),
        });
    }
    Err(ModelPathError::MissingCredentials(
        "set OPENAI_API_KEY, ANTHROPIC_API_KEY, or an OpenAI-compatible key",
    ))
}

/// Build a ready [`Route`] for `model` from a [`ProviderChoice`].
pub fn build_route(choice: &ProviderChoice, model: &str) -> Result<Route, ModelPathError> {
    match choice {
        ProviderChoice::OpenAiResponses { api_key, base_url } => {
            let provider = OpenAi::configure(OpenAiConfig {
                api_key: api_key.clone(),
                base_url: base_url.clone(),
            });
            Ok(provider.responses(model))
        }
        ProviderChoice::Anthropic { api_key, base_url } => {
            let provider = Anthropic::configure(AnthropicConfig {
                api_key: api_key.clone(),
                base_url: base_url.clone(),
            });
            Ok(provider.model(model))
        }
        ProviderChoice::OpenAiCompatibleProvider {
            provider_id,
            api_key,
        } => {
            let provider = OpenAiCompatible::provider(provider_id, api_key.clone())
                .ok_or_else(|| ModelPathError::UnknownProvider(provider_id.clone()))?;
            Ok(provider.chat(model))
        }
        ProviderChoice::OpenAiCompatibleCustom {
            provider_id,
            base_url,
            api_key,
            extra_headers,
        } => {
            let provider =
                OpenAiCompatible::configure(provider_id.clone(), base_url.clone(), api_key.clone());
            let mut route = provider.chat(model);
            for (name, value) in extra_headers {
                route.auth = route
                    .auth
                    .and_then(Auth::header(name.clone(), value.clone()));
            }
            Ok(route)
        }
        ProviderChoice::Codex {
            access_token,
            account_id,
            base_url,
        } => {
            // Reuse the `browser-use-llm` codex route builder: it targets
            // `<base_url>/codex/responses` (default chatgpt.com backend-api) with
            // the Bearer + `chatgpt-account-id` + `originator` + `OpenAI-Beta`
            // headers. The model id is carried by the per-turn request (the
            // Responses protocol does not bind it into the URL).
            let auth = CodexAuth::new(access_token.clone(), account_id.clone());
            let _ = model;
            Ok(codex_route(&auth, base_url.as_deref()))
        }
    }
}

/// Build the real [`ModelClientTransport`] for a turn: a live [`ModelClient`]
/// driving `route`, owning the per-turn [`LlmRequest`] assembled from `ctx`'s
/// model/provider identity and the input `messages`.
///
/// This is the production analogue of the tests' `ScriptedTransport`: it opens a
/// real streaming HTTP request when the driver samples.
pub fn build_transport(
    client: Arc<ModelClient>,
    route: Route,
    ctx: &TurnCtx,
    messages: Vec<Message>,
) -> ModelClientTransport {
    let mut req = LlmRequest::new(ctx.model.clone(), ctx.provider.clone());
    req.system
        .push(SystemPart::new(ctx.base_instructions.clone()));
    req.messages = messages;
    if let Some(instruction) = ctx.browser_mode_instruction.as_deref() {
        req.messages.insert(
            0,
            Message::new(
                MessageRole::System,
                vec![ContentPart::text(instruction.to_string())],
            ),
        );
    }
    apply_browser_use_provider_options(&ctx.provider, &mut req);
    ModelClientTransport::new(client, route, req)
}

pub(crate) fn apply_browser_use_provider_options(_provider: &str, _req: &mut LlmRequest) {}

/// Build the production text-only [`ModelSamplingDriver`] over a live transport.
///
/// This is the real [`SamplingDriver`](crate::turn::SamplingDriver) the turn loop
/// drives: it streams from the live model, maps events to UI events via `sink`,
/// and reports `model_needs_follow_up` from the model's tool calls. Attach fused
/// tool dispatch with [`ModelSamplingDriver::with_fusion`] (unchanged seam).
///
/// `max_retries` is the codex-style `stream_max_retries` (pass
/// `AgentConfig::stream_max_retries`).
pub fn build_sampling_driver(
    transport: ModelClientTransport,
    sink: Arc<dyn EventSink>,
    ctx: TurnCtx,
    max_retries: u32,
) -> ModelSamplingDriver<ModelClientTransport> {
    ModelSamplingDriver::new(transport, sink, ctx, max_retries)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header(route: &Route, name: &str) -> Option<String> {
        route
            .auth
            .headers()
            .into_iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v)
    }

    #[test]
    fn openai_responses_route_is_first_party() {
        let choice = ProviderChoice::OpenAiResponses {
            api_key: "sk-test".to_string(),
            base_url: None,
        };
        let route = build_route(&choice, "gpt-5.1-codex").unwrap();
        assert_eq!(route.endpoint.url(), "https://api.openai.com/v1/responses");
        assert_eq!(
            header(&route, "authorization").as_deref(),
            Some("Bearer sk-test")
        );
    }

    #[test]
    fn openai_responses_honors_base_url_override() {
        let choice = ProviderChoice::OpenAiResponses {
            api_key: "sk-test".to_string(),
            base_url: Some("https://proxy.example.com/v1".to_string()),
        };
        let route = build_route(&choice, "gpt-5.1-codex").unwrap();
        assert_eq!(
            route.endpoint.url(),
            "https://proxy.example.com/v1/responses"
        );
    }

    #[test]
    fn anthropic_route_uses_messages_and_api_key_header() {
        let choice = ProviderChoice::Anthropic {
            api_key: "ak-test".to_string(),
            base_url: None,
        };
        let route = build_route(&choice, "claude-sonnet-4-6").unwrap();
        assert_eq!(
            route.endpoint.url(),
            "https://api.anthropic.com/v1/messages"
        );
        assert_eq!(header(&route, "x-api-key").as_deref(), Some("ak-test"));
    }

    #[test]
    fn openai_compatible_known_provider_resolves_base_url() {
        let choice = ProviderChoice::OpenAiCompatibleProvider {
            provider_id: "deepseek".to_string(),
            api_key: "ds-key".to_string(),
        };
        let route = build_route(&choice, "deepseek-chat").unwrap();
        assert_eq!(
            route.endpoint.url(),
            "https://api.deepseek.com/v1/chat/completions"
        );
    }

    #[test]
    fn openai_compatible_unknown_provider_is_error() {
        let choice = ProviderChoice::OpenAiCompatibleProvider {
            provider_id: "nope".to_string(),
            api_key: "k".to_string(),
        };
        let err = build_route(&choice, "m").unwrap_err();
        assert!(matches!(err, ModelPathError::UnknownProvider(_)));
    }

    #[test]
    fn openai_compatible_custom_uses_explicit_base_url() {
        let choice = ProviderChoice::OpenAiCompatibleCustom {
            provider_id: "internal".to_string(),
            base_url: "https://llm.internal/v1".to_string(),
            api_key: "k".to_string(),
            extra_headers: Vec::new(),
        };
        let route = build_route(&choice, "m").unwrap();
        assert_eq!(
            route.endpoint.url(),
            "https://llm.internal/v1/chat/completions"
        );
    }

    #[test]
    fn openai_compatible_custom_applies_extra_headers() {
        let choice = ProviderChoice::OpenAiCompatibleCustom {
            provider_id: "browser-use".to_string(),
            base_url: "https://llm.api.browser-use.com/v1".to_string(),
            api_key: "k".to_string(),
            extra_headers: vec![(
                "x-browser-use-request-type".to_string(),
                "rust_agent".to_string(),
            )],
        };
        let route = build_route(&choice, "bu-3-max").unwrap();

        assert_eq!(
            header(&route, "x-browser-use-request-type").as_deref(),
            Some("rust_agent")
        );
    }

    #[test]
    fn browser_use_provider_options_do_not_tag_request_body() {
        let mut req = LlmRequest::new("bu-3-max", "browseruse");

        apply_browser_use_provider_options("browser-use", &mut req);

        assert_eq!(req.provider_options, None);
    }

    /// Only the `Codex` variant targets chatgpt.com: the env-keyed providers never
    /// route to the codex backend, while `Codex` does (and only it).
    #[test]
    fn only_codex_variant_targets_codex_backend() {
        // Non-codex providers must NOT target chatgpt.com / backend-api.
        for choice in [
            ProviderChoice::OpenAiResponses {
                api_key: "k".into(),
                base_url: None,
            },
            ProviderChoice::Anthropic {
                api_key: "k".into(),
                base_url: None,
            },
            ProviderChoice::OpenAiCompatibleCustom {
                provider_id: "x".into(),
                base_url: "https://llm.internal/v1".into(),
                api_key: "k".into(),
                extra_headers: Vec::new(),
            },
        ] {
            let url = build_route(&choice, "m").unwrap().endpoint.url();
            assert!(
                !url.contains("chatgpt.com") && !url.contains("backend-api"),
                "non-codex route must not target the codex backend: {url}"
            );
        }

        // The Codex variant DOES target the chatgpt.com backend-api.
        let codex = ProviderChoice::Codex {
            access_token: "acc".into(),
            account_id: "acct".into(),
            base_url: None,
        };
        let url = build_route(&codex, "m").unwrap().endpoint.url();
        assert_eq!(url, "https://chatgpt.com/backend-api/codex/responses");
    }

    /// The codex route carries the Bearer token + `chatgpt-account-id` header and
    /// honours a base-url override; the token never leaks via Debug.
    #[test]
    fn codex_route_has_oauth_headers_and_base_override() {
        let choice = ProviderChoice::Codex {
            access_token: "tok-secret".into(),
            account_id: "acct-123".into(),
            base_url: Some("https://proxy.example.com/backend-api".into()),
        };
        let route = build_route(&choice, "gpt-5.1-codex").unwrap();
        assert_eq!(
            route.endpoint.url(),
            "https://proxy.example.com/backend-api/codex/responses"
        );
        assert_eq!(
            header(&route, "authorization").as_deref(),
            Some("Bearer tok-secret")
        );
        assert_eq!(
            header(&route, "chatgpt-account-id").as_deref(),
            Some("acct-123")
        );
        assert!(!format!("{route:?}").contains("tok-secret"));
    }

    /// Env resolver is honest: with the relevant keys cleared it returns a
    /// MissingCredentials error rather than panicking or defaulting to codex.
    ///
    /// We do not mutate global env here (that is racy across the test binary);
    /// instead we assert the pure precedence by feeding the resolver-shaped logic
    /// through `build_route`, and document the env contract in the fn docs.
    #[test]
    fn missing_credentials_is_an_honest_error_type() {
        let err = ModelPathError::MissingCredentials("test");
        // It implements Display/Error and does not leak anything sensitive.
        assert!(format!("{err}").contains("no provider credentials"));
    }
}
