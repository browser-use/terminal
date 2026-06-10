use anyhow::{anyhow, bail, Context, Result};
use base64::{
    engine::general_purpose::{self, URL_SAFE_NO_PAD},
    Engine as _,
};
use browser_use_protocol::{
    CreditsSnapshot, ModelEvent, ModelUsage, ModelVerification, RateLimitSnapshot, RateLimitWindow,
    ToolCall, ToolSpec,
};
use chrono::{DateTime, Datelike, Local, TimeZone, Utc};
use rand::Rng as _;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::fs;
use std::fs::OpenOptions;
use std::io::{BufRead, BufReader, Cursor, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant, SystemTime};
use uuid::Uuid;

pub mod openrouter;

const X_CODEX_TURN_STATE_HEADER: &str = "x-codex-turn-state";
const CODEX_OAUTH_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const CODEX_AUTHORIZE_URL: &str = "https://auth.openai.com/oauth/authorize";
const CODEX_REFRESH_TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
const CODEX_REFRESH_TOKEN_URL_OVERRIDE_ENV_VAR: &str = "CODEX_REFRESH_TOKEN_URL_OVERRIDE";
pub const CODEX_CALLBACK_HOST: &str = "localhost";
pub const CODEX_CALLBACK_PORT: u16 = 1455;
pub const CODEX_CALLBACK_PATH: &str = "/auth/callback";
pub const CODEX_REDIRECT_URI: &str = "http://localhost:1455/auth/callback";
const CODEX_OAUTH_SCOPES: &str = "openid profile email offline_access";
const PROVIDER_MAX_IMAGE_DIMENSION: u32 = 2000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProviderErrorKind {
    Stream,
    Retryable,
    Unauthorized,
    ContextWindowExceeded,
    QuotaExceeded,
    UsageLimitReached,
    UsageNotIncluded,
    InvalidRequest,
    InvalidImage,
    RetryLimit,
    InternalServerError,
    UnexpectedStatus,
    RequestTimeout,
    ServerOverloaded,
    CyberPolicy,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ProviderError {
    kind: ProviderErrorKind,
    message: String,
    retry_delay: Option<Duration>,
    http_status_code: Option<u16>,
    rate_limits: Option<RateLimitSnapshot>,
}

impl ProviderError {
    pub fn stream(message: impl Into<String>) -> Self {
        Self {
            kind: ProviderErrorKind::Stream,
            message: message.into(),
            retry_delay: None,
            http_status_code: None,
            rate_limits: None,
        }
    }

    pub fn retryable(message: impl Into<String>, retry_delay: Option<Duration>) -> Self {
        Self {
            kind: ProviderErrorKind::Retryable,
            message: message.into(),
            retry_delay,
            http_status_code: None,
            rate_limits: None,
        }
    }

    pub fn non_retryable(kind: ProviderErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            retry_delay: None,
            http_status_code: None,
            rate_limits: None,
        }
    }

    pub fn kind(&self) -> ProviderErrorKind {
        self.kind
    }

    pub fn is_retryable(&self) -> bool {
        matches!(
            self.kind,
            ProviderErrorKind::Stream
                | ProviderErrorKind::Retryable
                | ProviderErrorKind::InternalServerError
                | ProviderErrorKind::UnexpectedStatus
                | ProviderErrorKind::RequestTimeout
                | ProviderErrorKind::ServerOverloaded
        )
    }

    pub fn retry_delay(&self) -> Option<Duration> {
        self.retry_delay
    }

    pub fn http_status_code(&self) -> Option<u16> {
        self.http_status_code
    }

    pub fn rate_limits(&self) -> Option<&RateLimitSnapshot> {
        self.rate_limits.as_ref()
    }

    fn with_http_status_code(mut self, status: reqwest::StatusCode) -> Self {
        self.http_status_code = Some(status.as_u16());
        self
    }

    fn with_rate_limits(mut self, rate_limits: Option<RateLimitSnapshot>) -> Self {
        self.rate_limits = rate_limits;
        self
    }
}

impl std::fmt::Display for ProviderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ProviderError {}

#[derive(Clone, Debug)]
pub struct ProviderTurn {
    pub instructions: Option<String>,
    pub model_settings: ModelRequestSettings,
    pub model_request_info: Option<ModelRequestInfo>,
    pub request_max_retries: Option<usize>,
    pub stream_max_retries: Option<usize>,
    pub stream_idle_timeout_ms: Option<u64>,
    pub messages: Vec<Value>,
    pub previous_response_id: Option<String>,
    pub tools: Vec<ToolSpec>,
    pub hosted_tools: Vec<HostedToolSpec>,
    pub output_schema: Option<Value>,
    pub output_schema_strict: bool,
    pub prompt_cache_key: Option<String>,
    pub client_metadata: Option<HashMap<String, String>>,
    pub extra_headers: Option<HashMap<String, String>>,
    pub turn_state: Option<Arc<Mutex<Option<String>>>>,
}

impl ProviderTurn {
    fn instructions_or_default<'a>(&'a self, default: &'a str) -> &'a str {
        self.instructions.as_deref().unwrap_or(default)
    }
}

impl Default for ProviderTurn {
    fn default() -> Self {
        Self {
            instructions: None,
            model_settings: ModelRequestSettings::default(),
            model_request_info: None,
            request_max_retries: None,
            stream_max_retries: None,
            stream_idle_timeout_ms: None,
            messages: Vec::new(),
            previous_response_id: None,
            tools: Vec::new(),
            hosted_tools: Vec::new(),
            output_schema: None,
            output_schema_strict: true,
            prompt_cache_key: None,
            client_metadata: None,
            extra_headers: None,
            turn_state: None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HostedToolSpec {
    WebSearch {
        external_web_access: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        filters: Option<WebSearchFilters>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        user_location: Option<WebSearchUserLocation>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        search_context_size: Option<WebSearchContextSize>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        search_content_types: Option<Vec<String>>,
    },
    ImageGeneration {
        output_format: String,
    },
}

#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(rename_all = "lowercase")]
pub enum WebSearchMode {
    Disabled,
    #[default]
    Cached,
    Live,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WebSearchContextSize {
    Low,
    Medium,
    High,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WebSearchLocation {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub country: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub region: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub city: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timezone: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WebSearchToolConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_size: Option<WebSearchContextSize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allowed_domains: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub location: Option<WebSearchLocation>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WebSearchFilters {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allowed_domains: Option<Vec<String>>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WebSearchUserLocation {
    #[serde(default)]
    pub r#type: WebSearchUserLocationType,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub country: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub region: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub city: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timezone: Option<String>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WebSearchUserLocationType {
    #[default]
    Approximate,
}

impl HostedToolSpec {
    pub fn web_search(
        mode: WebSearchMode,
        config: Option<&WebSearchToolConfig>,
        tool_type: WebSearchToolType,
    ) -> Option<Self> {
        let external_web_access = match mode {
            WebSearchMode::Disabled => return None,
            WebSearchMode::Cached => false,
            WebSearchMode::Live => true,
        };
        let search_content_types = match tool_type {
            WebSearchToolType::Text => None,
            WebSearchToolType::TextAndImage => Some(vec!["text".to_string(), "image".to_string()]),
        };
        Some(Self::WebSearch {
            external_web_access,
            filters: config.and_then(|config| {
                config
                    .allowed_domains
                    .as_ref()
                    .map(|allowed_domains| WebSearchFilters {
                        allowed_domains: Some(allowed_domains.clone()),
                    })
            }),
            user_location: config.and_then(|config| {
                config
                    .location
                    .as_ref()
                    .map(|location| WebSearchUserLocation {
                        r#type: WebSearchUserLocationType::Approximate,
                        country: location.country.clone(),
                        region: location.region.clone(),
                        city: location.city.clone(),
                        timezone: location.timezone.clone(),
                    })
            }),
            search_context_size: config.and_then(|config| config.context_size),
            search_content_types,
        })
    }

    pub fn image_generation_png() -> Self {
        Self::ImageGeneration {
            output_format: "png".to_string(),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelShellType {
    Default,
    Local,
    UnifiedExec,
    Disabled,
    #[default]
    ShellCommand,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WebSearchToolType {
    #[default]
    Text,
    TextAndImage,
}

#[derive(Clone, Debug, PartialEq)]
struct PreviousResponsesState {
    response_id: String,
    request_input: Vec<Value>,
    response_items: Vec<Value>,
    non_input_request: Value,
}

#[derive(Clone, Debug)]
struct ResponsesRequestBaseline {
    request_input: Vec<Value>,
    non_input_request: Value,
    allow_state_update: bool,
}

type SharedPreviousResponsesState = Arc<Mutex<Option<PreviousResponsesState>>>;

const MAX_REQUEST_MAX_RETRIES: usize = 100;
const DEFAULT_STREAM_IDLE_TIMEOUT_MS: u64 = 300_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ProviderRequestRetryConfig {
    max_retries: usize,
    base_delay: Duration,
    retry_429: bool,
    retry_5xx: bool,
    retry_transport: bool,
}

impl Default for ProviderRequestRetryConfig {
    fn default() -> Self {
        Self {
            max_retries: 4,
            base_delay: Duration::from_millis(200),
            retry_429: false,
            retry_5xx: true,
            retry_transport: true,
        }
    }
}

impl ProviderRequestRetryConfig {
    fn for_turn(self, turn: &ProviderTurn) -> Self {
        match turn.request_max_retries {
            Some(max_retries) => Self {
                max_retries: max_retries.min(MAX_REQUEST_MAX_RETRIES),
                ..self
            },
            None => self,
        }
    }

    #[cfg(test)]
    fn without_retries() -> Self {
        Self {
            max_retries: 0,
            ..Self::default()
        }
    }

    #[cfg(test)]
    fn without_delay() -> Self {
        Self {
            base_delay: Duration::ZERO,
            ..Self::default()
        }
    }

    fn should_retry_status(self, status: reqwest::StatusCode, attempt: usize) -> bool {
        attempt < self.max_retries
            && ((self.retry_429 && status.as_u16() == 429)
                || (self.retry_5xx && status.is_server_error()))
    }

    fn should_retry_send_error(self, error: &reqwest::Error, attempt: usize) -> bool {
        attempt < self.max_retries && self.retry_transport && !error.is_builder()
    }

    fn delay(self, retry_number: usize) -> Duration {
        if self.base_delay.is_zero() {
            return Duration::ZERO;
        }
        let exp = 2_u64.saturating_pow(retry_number.saturating_sub(1).min(63) as u32);
        let raw_ms = self.base_delay.as_millis() as u64;
        let raw = raw_ms.saturating_mul(exp);
        let jitter = rand::rng().random_range(0.9..1.1);
        Duration::from_millis((raw as f64 * jitter) as u64)
    }
}

fn send_provider_request(
    operation: &str,
    retry: ProviderRequestRetryConfig,
    mut make_request: impl FnMut() -> reqwest::blocking::RequestBuilder,
) -> Result<reqwest::blocking::Response, ProviderError> {
    for attempt in 0..=retry.max_retries {
        match make_request().send() {
            Ok(response) => {
                if retry.should_retry_status(response.status(), attempt) {
                    std::thread::sleep(retry.delay(attempt + 1));
                    continue;
                }
                return Ok(response);
            }
            Err(error) => {
                if retry.should_retry_send_error(&error, attempt) {
                    std::thread::sleep(retry.delay(attempt + 1));
                    continue;
                }
                return Err(provider_send_error(operation, &error));
            }
        }
    }
    Err(ProviderError::non_retryable(
        ProviderErrorKind::RetryLimit,
        format!("{operation}: retry limit reached"),
    ))
}

fn send_provider_text_request(
    send_operation: &str,
    read_operation: &str,
    retry: ProviderRequestRetryConfig,
    mut make_request: impl FnMut() -> reqwest::blocking::RequestBuilder,
) -> Result<(reqwest::StatusCode, reqwest::header::HeaderMap, String), ProviderError> {
    for attempt in 0..=retry.max_retries {
        match make_request().send() {
            Ok(response) => {
                let status = response.status();
                let headers = response.headers().clone();
                match response.text() {
                    Ok(body_text) => {
                        if retry.should_retry_status(status, attempt) {
                            std::thread::sleep(retry.delay(attempt + 1));
                            continue;
                        }
                        return Ok((status, headers, body_text));
                    }
                    Err(error) => {
                        if retry.should_retry_send_error(&error, attempt) {
                            std::thread::sleep(retry.delay(attempt + 1));
                            continue;
                        }
                        return Err(provider_send_error(read_operation, &error));
                    }
                }
            }
            Err(error) => {
                if retry.should_retry_send_error(&error, attempt) {
                    std::thread::sleep(retry.delay(attempt + 1));
                    continue;
                }
                return Err(provider_send_error(send_operation, &error));
            }
        }
    }
    Err(ProviderError::non_retryable(
        ProviderErrorKind::RetryLimit,
        format!("{send_operation}: retry limit reached"),
    ))
}

fn prepare_previous_response_request_body(
    body: &mut Value,
    state: &SharedPreviousResponsesState,
    explicit_previous_response_id: Option<&str>,
    auto_previous_response_reuse: bool,
) -> Result<ResponsesRequestBaseline> {
    let request_input = body
        .get("input")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let non_input_request = responses_request_without_input(body);
    if let Some(previous_response_id) = explicit_previous_response_id.filter(|id| !id.is_empty()) {
        body["previous_response_id"] = Value::String(previous_response_id.to_string());
        return Ok(ResponsesRequestBaseline {
            request_input,
            non_input_request,
            allow_state_update: false,
        });
    }

    if !auto_previous_response_reuse {
        return Ok(ResponsesRequestBaseline {
            request_input,
            non_input_request,
            allow_state_update: false,
        });
    }

    let previous_state = state
        .lock()
        .map_err(|_| anyhow!("previous response state lock poisoned"))?
        .clone();
    if let Some(previous_state) = previous_state {
        let mut reusable_prefix = previous_state.request_input.clone();
        reusable_prefix.extend(previous_state.response_items.clone());
        if previous_state.non_input_request == non_input_request
            && input_starts_with(&request_input, &reusable_prefix)
        {
            body["previous_response_id"] = Value::String(previous_state.response_id);
            body["input"] = Value::Array(request_input[reusable_prefix.len()..].to_vec());
        }
    }

    Ok(ResponsesRequestBaseline {
        request_input,
        non_input_request,
        allow_state_update: true,
    })
}

fn responses_request_without_input(body: &Value) -> Value {
    let mut without_input = body.clone();
    if let Some(object) = without_input.as_object_mut() {
        object.insert("input".to_string(), Value::Array(Vec::new()));
        object.remove("previous_response_id");
    }
    without_input
}

fn input_starts_with(input: &[Value], prefix: &[Value]) -> bool {
    input.len() >= prefix.len()
        && input
            .iter()
            .zip(prefix.iter())
            .take(prefix.len())
            .all(|(input, prefix)| input == prefix)
}

fn update_previous_response_state(
    state: &SharedPreviousResponsesState,
    baseline: ResponsesRequestBaseline,
    response_id: Option<String>,
    response_items: Vec<Value>,
) -> Result<()> {
    let mut guard = state
        .lock()
        .map_err(|_| anyhow!("previous response state lock poisoned"))?;
    if baseline.allow_state_update {
        if let Some(response_id) = response_id.filter(|id| !id.is_empty()) {
            *guard = Some(PreviousResponsesState {
                response_id,
                request_input: baseline.request_input,
                response_items,
                non_input_request: baseline.non_input_request,
            });
            return Ok(());
        }
    }
    *guard = None;
    Ok(())
}

fn clear_previous_response_state(state: &SharedPreviousResponsesState) -> Result<()> {
    *state
        .lock()
        .map_err(|_| anyhow!("previous response state lock poisoned"))? = None;
    Ok(())
}

fn previous_response_result_from_events(events: &[ModelEvent]) -> (Option<String>, Vec<Value>) {
    let mut response_id = None;
    let mut response_items = Vec::new();
    for event in events {
        match event {
            ModelEvent::ResponseOutputItem { item } => response_items.push(item.clone()),
            ModelEvent::ResponseCompleted {
                response_id: completed_response_id,
                ..
            } => {
                if let Some(completed_response_id) =
                    completed_response_id.as_deref().filter(|id| !id.is_empty())
                {
                    response_id = Some(completed_response_id.to_string());
                }
            }
            _ => {}
        }
    }
    (response_id, response_items)
}

fn previous_response_prefix_items_for_model(
    response_items: Vec<Value>,
    model_info: ModelRequestInfo,
) -> Vec<Value> {
    let mut items = response_items
        .into_iter()
        .filter_map(|item| raw_response_item_for_responses_input(&item))
        .collect::<Vec<_>>();
    strip_images_when_unsupported(&mut items, model_info.supports_image_input);
    items
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ModelRequestSettings {
    pub reasoning_effort: Option<String>,
    pub reasoning_summary: Option<String>,
    pub model_supports_reasoning_summaries: Option<bool>,
    pub text_verbosity: Option<String>,
    pub service_tier: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModelPersonality {
    None,
    Friendly,
    Pragmatic,
}

const CODEX_FALLBACK_BASE_MODEL_INSTRUCTIONS: &str =
    include_str!("../../../prompts/codex-model-fallback-prompt.md");
const NEUTRAL_FALLBACK_BASE_MODEL_INSTRUCTIONS: &str =
    include_str!("../../../prompts/model-fallback-prompt.md");
const CODEX_FRIENDLY_PERSONALITY_MESSAGE: &str =
    "You optimize for team morale and being a supportive teammate as much as code quality.";
const CODEX_PRAGMATIC_PERSONALITY_MESSAGE: &str =
    "You are a deeply pragmatic, effective software engineer.";
const TERMINAL_AGENT_TOOLING_AMENDMENT: &str = r#"## Agent Tooling Reliability

- When repository search is needed, use `rg` or `rg --files` first. If `rg` fails, diagnose the exact agent shell failure before saying it is not installed: distinguish not on `PATH`, present but not executable, wrapper or launcher interpreter missing, and no executable found in checked locations. If you continue with fallback tools, say that tooling is degraded and keep the answer scoped.
- If the latest user message asks you to stop, pause, or cancel, do not launch more tools. Acknowledge the stop and wait for the user to resume.
- After an interruption or rapid follow-up message, do not start parallel tool batches until the latest instruction is clearly stable. Prefer a short acknowledgement first, then continue only when the user asks for more work."#;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct StaticModelRequestInfo {
    supports_reasoning_summaries: bool,
    default_reasoning_effort: Option<&'static str>,
    supported_reasoning_efforts: &'static [&'static str],
    default_reasoning_summary: &'static str,
    support_verbosity: bool,
    default_verbosity: Option<&'static str>,
    supported_service_tiers: &'static [&'static str],
    supports_parallel_tool_calls: bool,
    supports_search_tool: bool,
    supports_image_input: bool,
    supports_image_detail_original: bool,
    shell_type: ModelShellType,
    web_search_tool_type: WebSearchToolType,
    truncation_policy: ModelTruncationPolicyInfo,
}

impl StaticModelRequestInfo {
    const fn unknown() -> Self {
        Self {
            supports_reasoning_summaries: false,
            default_reasoning_effort: None,
            supported_reasoning_efforts: &[],
            default_reasoning_summary: "auto",
            support_verbosity: false,
            default_verbosity: None,
            supported_service_tiers: &[],
            supports_parallel_tool_calls: false,
            supports_search_tool: false,
            supports_image_input: false,
            supports_image_detail_original: false,
            shell_type: ModelShellType::ShellCommand,
            web_search_tool_type: WebSearchToolType::Text,
            truncation_policy: default_model_truncation_policy(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelRequestInfo {
    pub supports_reasoning_summaries: bool,
    pub default_reasoning_effort: Option<String>,
    pub supported_reasoning_efforts: Vec<String>,
    pub default_reasoning_summary: String,
    pub support_verbosity: bool,
    pub default_verbosity: Option<String>,
    pub supported_service_tiers: Vec<String>,
    pub supports_parallel_tool_calls: bool,
    pub supports_search_tool: bool,
    pub supports_image_input: bool,
    pub supports_image_detail_original: bool,
    pub shell_type: ModelShellType,
    pub web_search_tool_type: WebSearchToolType,
    pub experimental_supported_tools: Vec<String>,
    pub context_window: Option<i64>,
    pub max_context_window: Option<i64>,
    pub auto_compact_token_limit: Option<i64>,
    pub truncation_policy: ModelTruncationPolicyInfo,
}

impl ModelRequestInfo {
    pub fn unknown() -> Self {
        StaticModelRequestInfo::unknown().into()
    }

    pub fn resolved_context_window(&self) -> Option<i64> {
        self.context_window.or(self.max_context_window)
    }

    pub fn auto_compact_token_limit(&self) -> Option<i64> {
        let context_limit = self
            .resolved_context_window()
            .map(|context_window| (context_window * 9) / 10);
        match (context_limit, self.auto_compact_token_limit) {
            (Some(context_limit), Some(configured)) => Some(configured.min(context_limit)),
            (Some(context_limit), None) => Some(context_limit),
            (None, configured) => configured,
        }
    }

    pub fn tool_output_token_budget(&self) -> usize {
        self.truncation_policy.token_budget()
    }
}

impl ModelTruncationPolicyInfo {
    pub fn token_budget(&self) -> usize {
        let limit = usize::try_from(self.limit.max(0)).unwrap_or(usize::MAX);
        match self.mode {
            ModelTruncationPolicyMode::Bytes => limit.div_ceil(4),
            ModelTruncationPolicyMode::Tokens => limit,
        }
    }

    pub fn with_token_limit(self, token_limit: usize) -> Self {
        match self.mode {
            ModelTruncationPolicyMode::Bytes => Self {
                mode: ModelTruncationPolicyMode::Bytes,
                limit: i64::try_from(token_limit.saturating_mul(4)).unwrap_or(i64::MAX),
            },
            ModelTruncationPolicyMode::Tokens => Self {
                mode: ModelTruncationPolicyMode::Tokens,
                limit: i64::try_from(token_limit).unwrap_or(i64::MAX),
            },
        }
    }
}

impl From<StaticModelRequestInfo> for ModelRequestInfo {
    fn from(info: StaticModelRequestInfo) -> Self {
        Self {
            supports_reasoning_summaries: info.supports_reasoning_summaries,
            default_reasoning_effort: info.default_reasoning_effort.map(str::to_string),
            supported_reasoning_efforts: info
                .supported_reasoning_efforts
                .iter()
                .map(|value| (*value).to_string())
                .collect(),
            default_reasoning_summary: info.default_reasoning_summary.to_string(),
            support_verbosity: info.support_verbosity,
            default_verbosity: info.default_verbosity.map(str::to_string),
            supported_service_tiers: info
                .supported_service_tiers
                .iter()
                .map(|value| (*value).to_string())
                .collect(),
            supports_parallel_tool_calls: info.supports_parallel_tool_calls,
            supports_search_tool: info.supports_search_tool,
            supports_image_input: info.supports_image_input,
            supports_image_detail_original: info.supports_image_detail_original,
            shell_type: info.shell_type,
            web_search_tool_type: info.web_search_tool_type,
            experimental_supported_tools: Vec::new(),
            context_window: None,
            max_context_window: None,
            auto_compact_token_limit: None,
            truncation_policy: info.truncation_policy,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelPresetInfo {
    pub id: String,
    pub display_name: String,
    pub description: String,
    pub default_reasoning_effort: Option<String>,
    pub supported_reasoning_efforts: Vec<String>,
    pub supported_service_tiers: Vec<String>,
    pub supports_personality: bool,
    pub show_in_picker: bool,
    pub is_default: bool,
    pub supported_in_api: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelCatalog {
    pub models: Vec<ModelCatalogEntryInfo>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelCatalogEntryInfo {
    pub slug: String,
    pub display_name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub default_reasoning_level: Option<String>,
    #[serde(default)]
    pub supported_reasoning_levels: Vec<ModelReasoningEffortInfo>,
    #[serde(default = "default_model_visibility")]
    pub visibility: String,
    #[serde(default = "default_supported_in_api")]
    pub supported_in_api: bool,
    #[serde(default)]
    pub priority: i32,
    #[serde(default)]
    pub service_tiers: Vec<ModelServiceTierInfo>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_service_tier: Option<String>,
    #[serde(default)]
    pub base_instructions: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_messages: Option<ModelMessagesInfo>,
    #[serde(default)]
    pub supports_reasoning_summaries: bool,
    #[serde(default = "default_reasoning_summary")]
    pub default_reasoning_summary: String,
    #[serde(default)]
    pub support_verbosity: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_verbosity: Option<String>,
    #[serde(default)]
    pub supports_parallel_tool_calls: bool,
    #[serde(default)]
    pub supports_search_tool: bool,
    #[serde(default)]
    pub supports_image_detail_original: bool,
    #[serde(default)]
    pub shell_type: ModelShellType,
    #[serde(default)]
    pub web_search_tool_type: WebSearchToolType,
    #[serde(default)]
    pub experimental_supported_tools: Vec<String>,
    #[serde(default = "default_input_modalities")]
    pub input_modalities: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_context_window: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_compact_token_limit: Option<i64>,
    #[serde(default = "default_model_truncation_policy")]
    pub truncation_policy: ModelTruncationPolicyInfo,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelTruncationPolicyInfo {
    #[serde(default = "default_truncation_policy_mode")]
    pub mode: ModelTruncationPolicyMode,
    pub limit: i64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelTruncationPolicyMode {
    #[default]
    Bytes,
    Tokens,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelReasoningEffortInfo {
    pub effort: String,
    #[serde(default)]
    pub description: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelServiceTierInfo {
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub description: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelMessagesInfo {
    pub instructions_template: Option<String>,
    pub instructions_variables: Option<ModelInstructionsVariablesInfo>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelInstructionsVariablesInfo {
    pub personality_default: Option<String>,
    pub personality_friendly: Option<String>,
    pub personality_pragmatic: Option<String>,
}

impl ModelCatalog {
    pub fn presets(&self, chatgpt_mode: bool) -> Vec<ModelPresetInfo> {
        let mut entries = self.models.iter().collect::<Vec<_>>();
        entries.sort_by_key(|entry| entry.priority);
        let mut presets = entries
            .into_iter()
            .filter(|entry| chatgpt_mode || entry.supported_in_api)
            .map(ModelCatalogEntryInfo::to_preset)
            .collect::<Vec<_>>();
        mark_default_by_picker_visibility(&mut presets);
        presets
    }

    pub fn entry_for_model(&self, model: &str) -> Option<&ModelCatalogEntryInfo> {
        catalog_entry_by_longest_prefix(model, &self.models)
            .or_else(|| catalog_entry_by_namespaced_suffix(model, &self.models))
    }

    pub fn request_info_for_model(&self, model: &str) -> ModelRequestInfo {
        self.entry_for_model(model)
            .map(ModelCatalogEntryInfo::request_info)
            .unwrap_or_else(ModelRequestInfo::unknown)
    }
}

impl ModelCatalogEntryInfo {
    fn to_preset(&self) -> ModelPresetInfo {
        ModelPresetInfo {
            id: self.slug.clone(),
            display_name: self.display_name.clone(),
            description: self.description.clone().unwrap_or_default(),
            default_reasoning_effort: self.default_reasoning_level.clone(),
            supported_reasoning_efforts: self
                .supported_reasoning_levels
                .iter()
                .map(|level| level.effort.clone())
                .collect(),
            supported_service_tiers: self
                .service_tiers
                .iter()
                .map(|tier| tier.id.clone())
                .collect(),
            supports_personality: self
                .model_messages
                .as_ref()
                .is_some_and(ModelMessagesInfo::supports_personality),
            show_in_picker: self.visibility == "list",
            is_default: false,
            supported_in_api: self.supported_in_api,
        }
    }

    pub fn request_info(&self) -> ModelRequestInfo {
        ModelRequestInfo {
            supports_reasoning_summaries: self.supports_reasoning_summaries,
            default_reasoning_effort: self.default_reasoning_level.clone(),
            supported_reasoning_efforts: self
                .supported_reasoning_levels
                .iter()
                .map(|level| level.effort.clone())
                .collect(),
            default_reasoning_summary: self.default_reasoning_summary.clone(),
            support_verbosity: self.support_verbosity,
            default_verbosity: self.default_verbosity.clone(),
            supported_service_tiers: self
                .service_tiers
                .iter()
                .map(|tier| tier.id.clone())
                .collect(),
            supports_parallel_tool_calls: self.supports_parallel_tool_calls,
            supports_search_tool: self.supports_search_tool,
            supports_image_input: self
                .input_modalities
                .iter()
                .any(|modality| modality == "image"),
            supports_image_detail_original: self.supports_image_detail_original,
            shell_type: self.shell_type,
            web_search_tool_type: self.web_search_tool_type,
            experimental_supported_tools: self.experimental_supported_tools.clone(),
            context_window: self.context_window,
            max_context_window: self.max_context_window,
            auto_compact_token_limit: self.auto_compact_token_limit,
            truncation_policy: self.truncation_policy,
        }
    }

    pub fn get_model_instructions(&self, personality: Option<ModelPersonality>) -> String {
        if let Some(model_messages) = &self.model_messages {
            if let Some(template) = model_messages.instructions_template.as_deref() {
                let personality_message = model_messages
                    .get_personality_message(personality)
                    .unwrap_or_default();
                return template.replace(PERSONALITY_PLACEHOLDER, &personality_message);
            }
        }
        self.base_instructions.clone()
    }
}

impl ModelMessagesInfo {
    fn has_personality_placeholder(&self) -> bool {
        self.instructions_template
            .as_deref()
            .is_some_and(|template| template.contains(PERSONALITY_PLACEHOLDER))
    }

    fn supports_personality(&self) -> bool {
        self.has_personality_placeholder()
            && self
                .instructions_variables
                .as_ref()
                .is_some_and(ModelInstructionsVariablesInfo::is_complete)
    }

    fn get_personality_message(&self, personality: Option<ModelPersonality>) -> Option<String> {
        self.instructions_variables
            .as_ref()
            .and_then(|variables| variables.get_personality_message(personality))
    }
}

impl ModelInstructionsVariablesInfo {
    fn is_complete(&self) -> bool {
        self.personality_default.is_some()
            && self.personality_friendly.is_some()
            && self.personality_pragmatic.is_some()
    }

    fn get_personality_message(&self, personality: Option<ModelPersonality>) -> Option<String> {
        match personality {
            Some(ModelPersonality::None) => Some(String::new()),
            Some(ModelPersonality::Friendly) => self.personality_friendly.clone(),
            Some(ModelPersonality::Pragmatic) => self.personality_pragmatic.clone(),
            None => self.personality_default.clone(),
        }
    }
}

fn default_model_visibility() -> String {
    "none".to_string()
}

fn default_supported_in_api() -> bool {
    true
}

fn default_reasoning_summary() -> String {
    "auto".to_string()
}

fn default_input_modalities() -> Vec<String> {
    vec!["text".to_string(), "image".to_string()]
}

fn default_truncation_policy_mode() -> ModelTruncationPolicyMode {
    ModelTruncationPolicyMode::Bytes
}

pub const fn default_model_truncation_policy() -> ModelTruncationPolicyInfo {
    ModelTruncationPolicyInfo {
        mode: ModelTruncationPolicyMode::Bytes,
        limit: 10_000,
    }
}

pub fn bundled_model_presets() -> Vec<ModelPresetInfo> {
    bundled_model_catalog().presets(true)
}

pub fn bundled_model_catalog() -> ModelCatalog {
    codex_bundled_model_catalog().clone()
}

fn codex_bundled_model_catalog() -> &'static ModelCatalog {
    static CATALOG: OnceLock<ModelCatalog> = OnceLock::new();
    CATALOG.get_or_init(|| {
        serde_json::from_str(include_str!("../../../prompts/codex-models.json"))
            .expect("bundled Codex models.json should parse")
    })
}

fn bundled_model_catalog_entry(model: &str) -> Option<&'static ModelCatalogEntryInfo> {
    codex_bundled_model_catalog().entry_for_model(model)
}

fn mark_default_by_picker_visibility(presets: &mut [ModelPresetInfo]) {
    for preset in presets.iter_mut() {
        preset.is_default = false;
    }
    if let Some(default) = presets.iter_mut().find(|preset| preset.show_in_picker) {
        default.is_default = true;
    } else if let Some(default) = presets.first_mut() {
        default.is_default = true;
    }
}

pub fn spawn_agent_model_overrides_description() -> String {
    spawn_agent_model_overrides_description_for_presets(bundled_model_presets())
}

pub fn spawn_agent_model_overrides_description_for_catalog(
    catalog: &ModelCatalog,
    chatgpt_mode: bool,
) -> String {
    spawn_agent_model_overrides_description_for_presets(catalog.presets(chatgpt_mode))
}

fn spawn_agent_model_overrides_description_for_presets(presets: Vec<ModelPresetInfo>) -> String {
    let model_descriptions = presets
        .into_iter()
        .filter(|preset| preset.show_in_picker)
        .take(5)
        .map(|preset| {
            let reasoning_efforts_suffix = spawn_agent_reasoning_efforts_suffix(
                &preset.supported_reasoning_efforts,
                preset.default_reasoning_effort.as_deref(),
            );
            let service_tiers_suffix =
                spawn_agent_service_tiers_suffix(&preset.supported_service_tiers);
            format!(
                "- `{}`: {}{}{}",
                preset.id, preset.description, reasoning_efforts_suffix, service_tiers_suffix
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    if model_descriptions.is_empty() {
        "No picker-visible model overrides are currently loaded.".to_string()
    } else {
        format!(
            "Available model overrides (optional; inherited parent model is preferred):\n{model_descriptions}"
        )
    }
}

fn spawn_agent_reasoning_efforts_suffix(
    supported_efforts: &[String],
    default_effort: Option<&str>,
) -> String {
    let efforts = supported_efforts
        .iter()
        .map(|effort| {
            if Some(effort.as_str()) == default_effort {
                format!("{effort} (default)")
            } else {
                effort.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join(", ");
    if efforts.is_empty() {
        String::new()
    } else {
        format!(" Reasoning efforts: {efforts}.")
    }
}

fn spawn_agent_service_tiers_suffix(supported_tiers: &[String]) -> String {
    let tiers = supported_tiers.join(", ");
    if tiers.is_empty() {
        String::new()
    } else {
        format!(" Service tiers: {tiers}.")
    }
}

pub fn model_switch_request_settings_for_model(
    model: &str,
    settings: &ModelRequestSettings,
) -> ModelRequestSettings {
    model_switch_request_settings_for_model_with_catalog(model, settings, None)
}

pub fn model_switch_request_settings_for_model_with_catalog(
    model: &str,
    settings: &ModelRequestSettings,
    catalog: Option<&ModelCatalog>,
) -> ModelRequestSettings {
    let model_info = model_request_info_for_catalog(model, catalog);
    let mut settings = settings.clone();
    settings.reasoning_effort =
        reasoning_effort_for_model_switch(settings.reasoning_effort.as_deref(), &model_info);
    settings.service_tier = service_tier_for_model(settings.service_tier.as_deref(), &model_info);
    settings
}

pub fn model_supported_service_tiers(model: &str) -> Vec<String> {
    model_supported_service_tiers_for_catalog(model, None)
}

pub fn model_supported_service_tiers_for_catalog(
    model: &str,
    catalog: Option<&ModelCatalog>,
) -> Vec<String> {
    model_request_info_for_catalog(model, catalog).supported_service_tiers
}

pub fn model_supports_service_tier(model: &str, service_tier: &str) -> bool {
    model_supports_service_tier_for_catalog(model, service_tier, None)
}

pub fn model_supports_service_tier_for_catalog(
    model: &str,
    service_tier: &str,
    catalog: Option<&ModelCatalog>,
) -> bool {
    model_supported_service_tiers_for_catalog(model, catalog)
        .iter()
        .any(|supported| supported == service_tier)
}

pub fn model_supports_original_image_detail(model: &str) -> bool {
    model_supports_original_image_detail_for_catalog(model, None)
}

pub fn model_supports_original_image_detail_for_catalog(
    model: &str,
    catalog: Option<&ModelCatalog>,
) -> bool {
    model_request_info_for_catalog(model, catalog).supports_image_detail_original
}

pub fn model_supports_personality(model: &str) -> bool {
    model_supports_personality_for_catalog(model, None)
}

pub fn model_supports_personality_for_catalog(model: &str, catalog: Option<&ModelCatalog>) -> bool {
    if let Some(catalog) = catalog {
        if let Some(entry) = catalog.entry_for_model(model) {
            return entry
                .model_messages
                .as_ref()
                .is_some_and(ModelMessagesInfo::supports_personality);
        }
        return fallback_model_messages_for_slug(model)
            .is_some_and(LocalModelMessages::supports_personality);
    }
    if let Some(entry) = bundled_model_catalog_entry(model) {
        return entry
            .model_messages
            .as_ref()
            .is_some_and(ModelMessagesInfo::supports_personality);
    }
    fallback_model_messages_for_slug(model).is_some_and(LocalModelMessages::supports_personality)
}

pub trait ModelProvider {
    fn provider_name(&self) -> &str {
        "unknown"
    }

    fn model_name(&self) -> &str {
        "unknown"
    }

    fn supports_namespace_tools(&self) -> bool {
        true
    }

    fn supports_hosted_web_search(&self) -> bool {
        false
    }

    fn supports_hosted_image_generation(&self) -> bool {
        false
    }

    fn start_turn(&self, turn: ProviderTurn) -> Result<Vec<ModelEvent>>;

    fn stream_turn(
        &self,
        turn: ProviderTurn,
        on_event: &mut dyn FnMut(ModelEvent) -> Result<()>,
    ) -> Result<()> {
        for event in self.start_turn(turn)? {
            on_event(event)?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Default)]
pub struct ProviderRequestOptions {
    pub query_params: Vec<(String, String)>,
    pub headers: HashMap<String, String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProviderCommandAuthConfig {
    pub command: String,
    pub args: Vec<String>,
    pub timeout_ms: u64,
    pub refresh_interval_ms: u64,
    pub cwd: PathBuf,
}

#[derive(Clone, Debug)]
pub struct ProviderCommandAuth {
    config: ProviderCommandAuthConfig,
    cached: Arc<Mutex<Option<CachedProviderCommandToken>>>,
}

#[derive(Clone, Debug)]
struct CachedProviderCommandToken {
    access_token: String,
    fetched_at: Instant,
}

impl ProviderCommandAuth {
    pub fn new(config: ProviderCommandAuthConfig) -> Self {
        Self {
            config,
            cached: Arc::new(Mutex::new(None)),
        }
    }

    pub fn access_token(&self) -> Result<String> {
        let mut cached = self
            .cached
            .lock()
            .map_err(|_| anyhow!("provider auth token cache is poisoned"))?;
        if let Some(cached_token) = cached.as_ref() {
            let should_use_cached_token = self.config.refresh_interval_ms == 0
                || cached_token.fetched_at.elapsed()
                    < Duration::from_millis(self.config.refresh_interval_ms);
            if should_use_cached_token {
                return Ok(cached_token.access_token.clone());
            }
        }

        let access_token = run_provider_auth_command(&self.config)?;
        *cached = Some(CachedProviderCommandToken {
            access_token: access_token.clone(),
            fetched_at: Instant::now(),
        });
        Ok(access_token)
    }

    pub fn refresh_access_token(&self) -> Result<String> {
        let access_token = run_provider_auth_command(&self.config)?;
        let mut cached = self
            .cached
            .lock()
            .map_err(|_| anyhow!("provider auth token cache is poisoned"))?;
        *cached = Some(CachedProviderCommandToken {
            access_token: access_token.clone(),
            fetched_at: Instant::now(),
        });
        Ok(access_token)
    }
}

fn run_provider_auth_command(config: &ProviderCommandAuthConfig) -> Result<String> {
    if config.timeout_ms == 0 {
        bail!(
            "provider auth command `{}` timeout_ms must be non-zero",
            config.command
        );
    }
    let program = resolve_provider_auth_program(&config.command, &config.cwd);
    let mut child = Command::new(&program)
        .args(&config.args)
        .current_dir(&config.cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("provider auth command `{}` failed to start", config.command))?;

    let timeout = Duration::from_millis(config.timeout_ms);
    let started = Instant::now();
    loop {
        if child
            .try_wait()
            .with_context(|| format!("provider auth command `{}` wait failed", config.command))?
            .is_some()
        {
            break;
        }
        if started.elapsed() >= timeout {
            let _ = child.kill();
            let _ = child.wait();
            bail!(
                "provider auth command `{}` timed out after {} ms",
                config.command,
                config.timeout_ms
            );
        }
        thread::sleep(Duration::from_millis(10));
    }

    let output = child
        .wait_with_output()
        .with_context(|| format!("provider auth command `{}` wait failed", config.command))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let stderr_suffix = if stderr.is_empty() {
            String::new()
        } else {
            format!(": {stderr}")
        };
        bail!(
            "provider auth command `{}` exited with status {}{}",
            config.command,
            output.status,
            stderr_suffix
        );
    }
    let stdout = String::from_utf8(output.stdout).map_err(|_| {
        anyhow!(
            "provider auth command `{}` wrote non-UTF-8 data to stdout",
            config.command
        )
    })?;
    let access_token = stdout.trim().to_string();
    if access_token.is_empty() {
        bail!(
            "provider auth command `{}` produced an empty token",
            config.command
        );
    }
    Ok(access_token)
}

fn resolve_provider_auth_program(command: &str, cwd: &Path) -> PathBuf {
    let path = Path::new(command);
    if path.is_absolute() {
        return path.to_path_buf();
    }
    if path.components().count() > 1 {
        return cwd.join(path);
    }
    PathBuf::from(command)
}

fn provider_command_auth_error(error: anyhow::Error) -> ProviderError {
    ProviderError::non_retryable(ProviderErrorKind::InvalidRequest, format!("{error:#}"))
}

#[derive(Clone, Debug)]
pub struct FakeProvider {
    events: Vec<ModelEvent>,
}

impl FakeProvider {
    pub fn new(events: Vec<ModelEvent>) -> Self {
        Self { events }
    }

    pub fn with_text(text: impl Into<String>) -> Self {
        Self {
            events: vec![
                ModelEvent::TextDelta { text: text.into() },
                ModelEvent::Done,
            ],
        }
    }
}

impl Default for FakeProvider {
    fn default() -> Self {
        Self::with_text("ok")
    }
}

impl ModelProvider for FakeProvider {
    fn provider_name(&self) -> &str {
        "fake"
    }

    fn model_name(&self) -> &str {
        "fake"
    }

    fn start_turn(&self, _turn: ProviderTurn) -> Result<Vec<ModelEvent>> {
        Ok(self.events.clone())
    }
}

#[derive(Debug)]
pub struct ScriptedProvider {
    turns: Mutex<VecDeque<Vec<ModelEvent>>>,
}

impl ScriptedProvider {
    pub fn new(turns: Vec<Vec<ModelEvent>>) -> Self {
        Self {
            turns: Mutex::new(VecDeque::from(turns)),
        }
    }
}

impl ModelProvider for ScriptedProvider {
    fn provider_name(&self) -> &str {
        "scripted"
    }

    fn model_name(&self) -> &str {
        "scripted"
    }

    fn start_turn(&self, _turn: ProviderTurn) -> Result<Vec<ModelEvent>> {
        Ok(self
            .turns
            .lock()
            .expect("scripted provider lock")
            .pop_front()
            .unwrap_or_else(|| vec![ModelEvent::Done]))
    }
}

#[derive(Clone, Debug)]
pub struct OpenAIResponsesProvider {
    api_key: Option<String>,
    command_auth: Option<ProviderCommandAuth>,
    model: String,
    base_url: String,
    provider_name: String,
    instructions: String,
    client: reqwest::blocking::Client,
    previous_response_state: SharedPreviousResponsesState,
    request_retry: ProviderRequestRetryConfig,
    request_options: ProviderRequestOptions,
}

impl OpenAIResponsesProvider {
    pub fn new(api_key: impl Into<String>, model: impl Into<String>) -> Self {
        Self::with_base_url(api_key, model, "https://api.openai.com/v1")
    }

    pub fn with_base_url(
        api_key: impl Into<String>,
        model: impl Into<String>,
        base_url: impl Into<String>,
    ) -> Self {
        Self::with_optional_api_key(Some(api_key.into()), model, base_url)
    }

    pub fn with_optional_api_key(
        api_key: Option<String>,
        model: impl Into<String>,
        base_url: impl Into<String>,
    ) -> Self {
        Self {
            api_key,
            command_auth: None,
            model: model.into(),
            base_url: base_url.into().trim_end_matches('/').to_string(),
            provider_name: "openai".to_string(),
            instructions: default_instructions(),
            client: reqwest::blocking::Client::new(),
            previous_response_state: Arc::new(Mutex::new(None)),
            request_retry: ProviderRequestRetryConfig::default(),
            request_options: ProviderRequestOptions::default(),
        }
    }

    pub fn from_env(model: impl Into<String>) -> Result<Self> {
        let api_key = std::env::var("LLM_BROWSER_OPENAI_API_KEY")
            .or_else(|_| std::env::var("OPENAI_API_KEY"))
            .context("set LLM_BROWSER_OPENAI_API_KEY or OPENAI_API_KEY")?;
        let base_url = std::env::var("LLM_BROWSER_OPENAI_BASE_URL")
            .unwrap_or_else(|_| "https://api.openai.com/v1".to_string());
        Ok(Self::with_base_url(api_key, model, base_url))
    }

    pub fn with_instructions(mut self, instructions: impl Into<String>) -> Self {
        self.instructions = instructions.into();
        self
    }

    pub fn with_provider_name(mut self, provider_name: impl Into<String>) -> Self {
        let provider_name = provider_name.into();
        if !provider_name.trim().is_empty() {
            self.provider_name = provider_name;
        }
        self
    }

    pub fn with_request_options(mut self, request_options: ProviderRequestOptions) -> Self {
        self.request_options = request_options;
        self
    }

    pub fn with_command_auth_config(mut self, auth: ProviderCommandAuthConfig) -> Self {
        self.command_auth = Some(ProviderCommandAuth::new(auth));
        self
    }
}

impl ModelProvider for OpenAIResponsesProvider {
    fn provider_name(&self) -> &str {
        &self.provider_name
    }

    fn model_name(&self) -> &str {
        &self.model
    }

    fn supports_hosted_web_search(&self) -> bool {
        true
    }

    fn start_turn(&self, turn: ProviderTurn) -> Result<Vec<ModelEvent>> {
        let stream_idle_timeout = stream_idle_timeout_for_turn(&turn);
        let turn_state = turn.turn_state.clone();
        let (response, baseline, model_info) = self.send_turn_request(turn)?;
        record_turn_state_from_headers(response.headers(), turn_state.as_ref())?;
        match parse_responses_sse(response, &self.model, stream_idle_timeout) {
            Ok(events) => {
                let (response_id, response_items) = previous_response_result_from_events(&events);
                let response_items =
                    previous_response_prefix_items_for_model(response_items, model_info);
                update_previous_response_state(
                    &self.previous_response_state,
                    baseline,
                    response_id,
                    response_items,
                )?;
                Ok(events)
            }
            Err(error) => {
                clear_previous_response_state(&self.previous_response_state)?;
                Err(error)
            }
        }
    }

    fn stream_turn(
        &self,
        turn: ProviderTurn,
        on_event: &mut dyn FnMut(ModelEvent) -> Result<()>,
    ) -> Result<()> {
        let stream_idle_timeout = stream_idle_timeout_for_turn(&turn);
        let turn_state = turn.turn_state.clone();
        let (response, baseline, model_info) = self.send_turn_request(turn)?;
        record_turn_state_from_headers(response.headers(), turn_state.as_ref())?;
        let mut response_items = Vec::new();
        let mut response_id = None;
        let result =
            parse_responses_sse_stream(response, &self.model, stream_idle_timeout, &mut |event| {
                match &event {
                    ModelEvent::ResponseOutputItem { item } => response_items.push(item.clone()),
                    ModelEvent::ResponseCompleted {
                        response_id: completed_response_id,
                        ..
                    } => {
                        if let Some(completed_response_id) =
                            completed_response_id.as_deref().filter(|id| !id.is_empty())
                        {
                            response_id = Some(completed_response_id.to_string());
                        }
                    }
                    _ => {}
                }
                on_event(event)
            });
        match result {
            Ok(()) => {
                let response_items =
                    previous_response_prefix_items_for_model(response_items, model_info);
                update_previous_response_state(
                    &self.previous_response_state,
                    baseline,
                    response_id,
                    response_items,
                )
            }
            Err(error) => {
                clear_previous_response_state(&self.previous_response_state)?;
                Err(error)
            }
        }
    }
}

impl OpenAIResponsesProvider {
    fn send_turn_request(
        &self,
        turn: ProviderTurn,
    ) -> Result<(
        reqwest::blocking::Response,
        ResponsesRequestBaseline,
        ModelRequestInfo,
    )> {
        let request_retry = self.request_retry.for_turn(&turn);
        let extra_headers =
            extra_headers_with_turn_state(turn.extra_headers.clone(), turn.turn_state.as_ref())?;
        let instructions = turn.instructions_or_default(&self.instructions).to_string();
        let model_info = model_request_info_for_turn(&self.model, &turn);
        let input = messages_to_responses_input_for_model(&turn.messages, &model_info)?;
        let tools = tool_specs_to_responses_tools_with_hosted(&turn.tools, &turn.hosted_tools);
        let mut body = json!({
            "model": self.model,
            "input": input,
            "instructions": instructions,
            "store": is_azure_responses_base_url(&self.base_url),
            "stream": true,
            "tool_choice": "auto",
            "include": [],
            "parallel_tool_calls": model_info.supports_parallel_tool_calls,
        });
        apply_model_request_settings(
            &mut body,
            &turn.model_settings,
            &model_info,
            turn.output_schema.as_ref(),
            turn.output_schema_strict,
        );
        if let Some(prompt_cache_key) = turn
            .prompt_cache_key
            .as_deref()
            .filter(|key| !key.is_empty())
        {
            body["prompt_cache_key"] = Value::String(prompt_cache_key.to_string());
        }
        if let Some(client_metadata) = non_empty_client_metadata(turn.client_metadata) {
            body["client_metadata"] = Value::Object(client_metadata);
        }
        if !tools.is_empty() {
            body["tools"] = Value::Array(tools);
        }
        let baseline = prepare_previous_response_request_body(
            &mut body,
            &self.previous_response_state,
            turn.previous_response_id.as_deref(),
            /*auto_previous_response_reuse*/ false,
        )?;

        let command_auth = self.command_auth.clone();
        let command_auth_token = command_auth
            .as_ref()
            .map(ProviderCommandAuth::access_token)
            .transpose()
            .map_err(provider_command_auth_error)?;
        let build_request = |auth_token: Option<&str>| {
            let mut request = self
                .client
                .post(format!("{}/responses", self.base_url))
                .header("Accept", "text/event-stream");
            request = apply_query_params(request, &self.request_options.query_params);
            request = apply_extra_headers(request, Some(&self.request_options.headers));
            if let Some(api_key) = auth_token
                .or(self.api_key.as_deref())
                .filter(|key| !key.is_empty())
            {
                request = request.bearer_auth(api_key);
            }
            apply_extra_headers(request, extra_headers.as_ref()).json(&body)
        };
        let send_with_auth = |auth_token: Option<&str>| {
            send_provider_request("send OpenAI Responses request", request_retry, || {
                build_request(auth_token)
            })
        };

        let mut response = match send_with_auth(command_auth_token.as_deref()) {
            Ok(response) => response,
            Err(error) => {
                clear_previous_response_state(&self.previous_response_state)?;
                return Err(error.into());
            }
        };
        if response.status() == reqwest::StatusCode::UNAUTHORIZED {
            if let Some(command_auth) = command_auth.as_ref() {
                let refreshed = command_auth
                    .refresh_access_token()
                    .map_err(provider_command_auth_error)?;
                response = match send_with_auth(Some(&refreshed)) {
                    Ok(response) => response,
                    Err(error) => {
                        clear_previous_response_state(&self.previous_response_state)?;
                        return Err(error.into());
                    }
                };
            }
        }
        let status = response.status();
        if !status.is_success() {
            let headers = response.headers().clone();
            let body = response.text().unwrap_or_default();
            clear_previous_response_state(&self.previous_response_state)?;
            return Err(provider_http_status_error(
                "OpenAI Responses",
                status,
                &body,
                Some(&headers),
            )
            .into());
        }
        Ok((response, baseline, model_info))
    }
}

#[derive(Clone, Debug)]
pub struct OpenAICompatibleChatProvider {
    api_key: Option<String>,
    command_auth: Option<ProviderCommandAuth>,
    model: String,
    base_url: String,
    provider_name: String,
    instructions: String,
    include_image_content: bool,
    include_parallel_tool_calls: bool,
    include_tool_choice: bool,
    include_usage_request: bool,
    thinking: Option<Value>,
    reasoning_effort: Option<String>,
    client: reqwest::blocking::Client,
    request_retry: ProviderRequestRetryConfig,
    request_options: ProviderRequestOptions,
}

impl OpenAICompatibleChatProvider {
    pub fn new(api_key: impl Into<String>, model: impl Into<String>) -> Self {
        Self::with_base_url(api_key, model, "https://openrouter.ai/api/v1")
    }

    pub fn with_base_url(
        api_key: impl Into<String>,
        model: impl Into<String>,
        base_url: impl Into<String>,
    ) -> Self {
        Self::with_optional_api_key(Some(api_key.into()), model, base_url)
    }

    pub fn with_optional_api_key(
        api_key: Option<String>,
        model: impl Into<String>,
        base_url: impl Into<String>,
    ) -> Self {
        let model = model.into();
        Self {
            api_key,
            command_auth: None,
            include_image_content: !is_deepseek_v4_model(&model),
            model,
            base_url: base_url.into().trim_end_matches('/').to_string(),
            provider_name: "openai-compatible".to_string(),
            instructions: default_instructions(),
            include_parallel_tool_calls: true,
            include_tool_choice: true,
            include_usage_request: true,
            thinking: None,
            reasoning_effort: None,
            client: reqwest::blocking::Client::new(),
            request_retry: ProviderRequestRetryConfig::default(),
            request_options: ProviderRequestOptions::default(),
        }
    }

    pub fn deepseek(
        api_key: impl Into<String>,
        model: impl Into<String>,
        base_url: impl Into<String>,
    ) -> Self {
        let mut provider = Self::with_base_url(api_key, model, base_url);
        provider.provider_name = "deepseek".to_string();
        provider.include_image_content = false;
        provider.include_parallel_tool_calls = false;
        provider.include_tool_choice = false;
        provider.include_usage_request = false;
        provider.thinking = Some(json!({ "type": "enabled" }));
        provider.reasoning_effort = Some(
            std::env::var("LLM_BROWSER_DEEPSEEK_REASONING_EFFORT")
                .ok()
                .filter(|value| !value.trim().is_empty())
                .unwrap_or_else(|| "high".to_string()),
        );
        provider
    }

    pub fn from_env(model: impl Into<String>) -> Result<Self> {
        let api_key = std::env::var("LLM_BROWSER_OPENAI_COMPAT_API_KEY")
            .or_else(|_| std::env::var("OPENROUTER_API_KEY"))
            .context("set LLM_BROWSER_OPENAI_COMPAT_API_KEY or OPENROUTER_API_KEY")?;
        let base_url = std::env::var("LLM_BROWSER_OPENAI_COMPAT_BASE_URL")
            .or_else(|_| std::env::var("OPENROUTER_BASE_URL"))
            .unwrap_or_else(|_| "https://openrouter.ai/api/v1".to_string());
        Ok(Self::with_base_url(api_key, model, base_url))
    }

    pub fn with_instructions(mut self, instructions: impl Into<String>) -> Self {
        self.instructions = instructions.into();
        self
    }

    pub fn with_provider_name(mut self, provider_name: impl Into<String>) -> Self {
        let provider_name = provider_name.into();
        if !provider_name.trim().is_empty() {
            self.provider_name = provider_name;
        }
        self
    }

    pub fn with_request_options(mut self, request_options: ProviderRequestOptions) -> Self {
        self.request_options = request_options;
        self
    }

    pub fn with_command_auth_config(mut self, auth: ProviderCommandAuthConfig) -> Self {
        self.command_auth = Some(ProviderCommandAuth::new(auth));
        self
    }

    fn chat_request_body(&self, turn: &ProviderTurn, stream: bool) -> Result<Value> {
        let instructions = turn.instructions_or_default(&self.instructions).to_string();
        let mut messages = vec![json!({
            "role": "system",
            "content": instructions,
        })];
        messages.extend(messages_to_chat_messages(
            &turn.messages,
            self.include_image_content,
        )?);
        let tools = tool_specs_to_chat_tools(&turn.tools);
        let model_info = model_request_info_for_turn(&self.model, turn);
        let mut body = json!({
            "model": self.model,
            "messages": messages,
        });
        if self.include_parallel_tool_calls {
            body["parallel_tool_calls"] = json!(model_info.supports_parallel_tool_calls);
        }
        if let Some(thinking) = &self.thinking {
            body["thinking"] = thinking.clone();
        }
        if let Some(reasoning_effort) = &self.reasoning_effort {
            body["reasoning_effort"] = json!(reasoning_effort);
        }
        let include_usage = (self.include_usage_request && self.base_url.contains("openrouter.ai"))
            || include_openai_compatible_usage();
        if include_usage {
            body["usage"] = json!({ "include": true });
        }
        if stream {
            body["stream"] = json!(true);
            if include_usage {
                body["stream_options"] = json!({ "include_usage": true });
            }
        }
        if !tools.is_empty() {
            body["tools"] = Value::Array(tools);
            if self.include_tool_choice {
                body["tool_choice"] = json!("auto");
            }
        }
        Ok(body)
    }

    fn build_chat_request(
        &self,
        body: &Value,
        auth_token: Option<&str>,
        accept: Option<&str>,
    ) -> reqwest::blocking::RequestBuilder {
        let mut request = self
            .client
            .post(format!("{}/chat/completions", self.base_url));
        if let Some(accept) = accept {
            request = request.header("Accept", accept);
        }
        request = apply_query_params(request, &self.request_options.query_params);
        request = apply_extra_headers(request, Some(&self.request_options.headers));
        if let Some(api_key) = auth_token
            .or(self.api_key.as_deref())
            .filter(|key| !key.trim().is_empty())
        {
            request = request.bearer_auth(api_key);
        }
        request.json(body)
    }

    fn send_chat_stream_request(
        &self,
        body: &Value,
        request_retry: ProviderRequestRetryConfig,
    ) -> Result<reqwest::blocking::Response> {
        let command_auth = self.command_auth.clone();
        let command_auth_token = command_auth
            .as_ref()
            .map(ProviderCommandAuth::access_token)
            .transpose()
            .map_err(provider_command_auth_error)?;
        let send_with_auth = |auth_token: Option<&str>| {
            send_provider_request("send OpenAI-compatible chat request", request_retry, || {
                self.build_chat_request(body, auth_token, Some("text/event-stream"))
            })
        };
        let mut response = send_with_auth(command_auth_token.as_deref())?;
        if response.status() == reqwest::StatusCode::UNAUTHORIZED {
            if let Some(command_auth) = command_auth.as_ref() {
                let refreshed = command_auth
                    .refresh_access_token()
                    .map_err(provider_command_auth_error)?;
                response = send_with_auth(Some(&refreshed))?;
            }
        }
        Ok(response)
    }
}

impl ModelProvider for OpenAICompatibleChatProvider {
    fn provider_name(&self) -> &str {
        &self.provider_name
    }

    fn model_name(&self) -> &str {
        &self.model
    }

    fn supports_namespace_tools(&self) -> bool {
        false
    }

    fn start_turn(&self, turn: ProviderTurn) -> Result<Vec<ModelEvent>> {
        let request_retry = self.request_retry.for_turn(&turn);
        let body = self.chat_request_body(&turn, false)?;
        let command_auth = self.command_auth.clone();
        let command_auth_token = command_auth
            .as_ref()
            .map(ProviderCommandAuth::access_token)
            .transpose()
            .map_err(provider_command_auth_error)?;
        let send_with_auth = |auth_token: Option<&str>| {
            send_provider_text_request(
                "send OpenAI-compatible chat request",
                "read OpenAI-compatible chat response body",
                request_retry,
                || self.build_chat_request(&body, auth_token, None),
            )
        };
        let (mut status, mut headers, mut body_text) =
            send_with_auth(command_auth_token.as_deref())?;
        if status == reqwest::StatusCode::UNAUTHORIZED {
            if let Some(command_auth) = command_auth.as_ref() {
                let refreshed = command_auth
                    .refresh_access_token()
                    .map_err(provider_command_auth_error)?;
                (status, headers, body_text) = send_with_auth(Some(&refreshed))?;
            }
        }
        if !status.is_success() {
            return Err(provider_http_status_error(
                "OpenAI-compatible chat",
                status,
                &body_text,
                Some(&headers),
            )
            .into());
        }
        let body: Value = serde_json::from_str(&body_text).map_err(|error| {
            ProviderError::retryable(format!("parse OpenAI-compatible chat JSON: {error}"), None)
        })?;
        parse_chat_completion_output(&body, &self.model, &turn.tools)
    }

    fn stream_turn(
        &self,
        turn: ProviderTurn,
        on_event: &mut dyn FnMut(ModelEvent) -> Result<()>,
    ) -> Result<()> {
        let request_retry = self.request_retry.for_turn(&turn);
        let stream_idle_timeout = stream_idle_timeout_for_turn(&turn);
        let body = self.chat_request_body(&turn, true)?;
        let response = self.send_chat_stream_request(&body, request_retry)?;
        let status = response.status();
        if !status.is_success() {
            let headers = response.headers().clone();
            let body_text = response.text().unwrap_or_default();
            return Err(provider_http_status_error(
                "OpenAI-compatible chat",
                status,
                &body_text,
                Some(&headers),
            )
            .into());
        }
        parse_chat_completion_sse_stream(
            response,
            &self.model,
            &turn.tools,
            stream_idle_timeout,
            on_event,
        )
    }
}

type ClaudeCodeOAuthRefreshFn =
    Arc<dyn Fn(&str) -> Result<ClaudeCodeOAuthCredential> + Send + Sync>;
type ClaudeCodeOAuthPersistFn = Arc<dyn Fn(&ClaudeCodeOAuthCredential) -> Result<()> + Send + Sync>;

fn is_deepseek_v4_model(model: &str) -> bool {
    let normalized = model.trim().to_ascii_lowercase().replace('_', "-");
    normalized.contains("deepseek")
        && (normalized.contains("v4-pro") || normalized.contains("v4-flash"))
}

#[derive(Clone, Debug)]
pub struct AnthropicMessagesProvider {
    credential: Arc<Mutex<AnthropicCredential>>,
    oauth_refresh: Option<ClaudeCodeOAuthRefresh>,
    model: String,
    base_url: String,
    instructions: String,
    client: reqwest::blocking::Client,
    request_retry: ProviderRequestRetryConfig,
}

#[derive(Clone, Debug)]
enum AnthropicCredential {
    ApiKey(String),
    AuthToken(String),
}

impl AnthropicCredential {
    fn is_oauth(&self) -> bool {
        match self {
            AnthropicCredential::AuthToken(_) => true,
            AnthropicCredential::ApiKey(value) => is_claude_code_oauth_token(value),
        }
    }
}

#[derive(Clone)]
struct ClaudeCodeOAuthRefresh {
    refresh_token: Arc<Mutex<String>>,
    refresh_fn: ClaudeCodeOAuthRefreshFn,
    on_refresh: Option<ClaudeCodeOAuthPersistFn>,
}

impl std::fmt::Debug for ClaudeCodeOAuthRefresh {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClaudeCodeOAuthRefresh")
            .field("refresh_token", &"<redacted>")
            .field("on_refresh", &self.on_refresh.is_some())
            .finish()
    }
}

impl ClaudeCodeOAuthRefresh {
    fn new(
        refresh_token: impl Into<String>,
        refresh_fn: ClaudeCodeOAuthRefreshFn,
        on_refresh: Option<ClaudeCodeOAuthPersistFn>,
    ) -> Self {
        Self {
            refresh_token: Arc::new(Mutex::new(refresh_token.into())),
            refresh_fn,
            on_refresh,
        }
    }

    fn refresh(&self) -> Result<ClaudeCodeOAuthCredential> {
        let refresh_token = self
            .refresh_token
            .lock()
            .map_err(|_| anyhow!("Claude Code OAuth refresh token cache is poisoned"))?
            .clone();
        let credential = (self.refresh_fn)(refresh_token.trim())?;
        if let Some(on_refresh) = &self.on_refresh {
            on_refresh(&credential)?;
        }
        *self
            .refresh_token
            .lock()
            .map_err(|_| anyhow!("Claude Code OAuth refresh token cache is poisoned"))? =
            credential.refresh_token.trim().to_string();
        Ok(credential)
    }
}

const CLAUDE_CODE_CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
const CLAUDE_CODE_AUTHORIZE_URL: &str = "https://claude.ai/oauth/authorize";
const CLAUDE_CODE_TOKEN_URL: &str = "https://platform.claude.com/v1/oauth/token";
pub const CLAUDE_CODE_CALLBACK_HOST: &str = "127.0.0.1";
pub const CLAUDE_CODE_CALLBACK_PORT: u16 = 53692;
pub const CLAUDE_CODE_CALLBACK_PATH: &str = "/callback";
pub const CLAUDE_CODE_REDIRECT_URI: &str = "http://localhost:53692/callback";
const CLAUDE_CODE_SCOPES: &str =
    "org:create_api_key user:profile user:inference user:sessions:claude_code user:mcp_servers user:file_upload";
const CLAUDE_CODE_VERSION: &str = "2.1.75";
const ANTHROPIC_BETA_FEATURES: &[&str] = &[
    "fine-grained-tool-streaming-2025-05-14",
    "interleaved-thinking-2025-05-14",
];

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClaudeCodeOAuthCredential {
    pub access_token: String,
    pub refresh_token: String,
    pub expires_ms: i64,
}

#[derive(Debug, Deserialize)]
struct ClaudeCodeTokenResponse {
    access_token: Option<String>,
    refresh_token: Option<String>,
    expires_in: Option<i64>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ClaudeCodeAuthorization {
    pub code: Option<String>,
    pub state: Option<String>,
}

impl AnthropicMessagesProvider {
    pub fn new(api_key: impl Into<String>, model: impl Into<String>) -> Self {
        Self::with_base_url(api_key, model, "https://api.anthropic.com/v1")
    }

    pub fn with_base_url(
        api_key: impl Into<String>,
        model: impl Into<String>,
        base_url: impl Into<String>,
    ) -> Self {
        Self::with_credential(AnthropicCredential::ApiKey(api_key.into()), model, base_url)
    }

    pub fn with_auth_token(
        auth_token: impl Into<String>,
        model: impl Into<String>,
        base_url: impl Into<String>,
    ) -> Self {
        Self::with_credential(
            AnthropicCredential::AuthToken(auth_token.into()),
            model,
            base_url,
        )
    }

    pub fn with_claude_code_oauth_credential(
        credential: ClaudeCodeOAuthCredential,
        model: impl Into<String>,
        base_url: impl Into<String>,
    ) -> Self {
        Self::with_claude_code_oauth_refresh_handler(
            credential,
            model,
            base_url,
            Arc::new(refresh_claude_code_oauth),
            None,
        )
    }

    pub fn with_claude_code_oauth_persistence(
        credential: ClaudeCodeOAuthCredential,
        model: impl Into<String>,
        base_url: impl Into<String>,
        on_refresh: impl Fn(&ClaudeCodeOAuthCredential) -> Result<()> + Send + Sync + 'static,
    ) -> Self {
        Self::with_claude_code_oauth_refresh_handler(
            credential,
            model,
            base_url,
            Arc::new(refresh_claude_code_oauth),
            Some(Arc::new(on_refresh)),
        )
    }

    fn with_claude_code_oauth_refresh_handler(
        credential: ClaudeCodeOAuthCredential,
        model: impl Into<String>,
        base_url: impl Into<String>,
        refresh_fn: ClaudeCodeOAuthRefreshFn,
        on_refresh: Option<ClaudeCodeOAuthPersistFn>,
    ) -> Self {
        let refresh =
            ClaudeCodeOAuthRefresh::new(credential.refresh_token.clone(), refresh_fn, on_refresh);
        let mut provider = Self::with_credential(
            AnthropicCredential::AuthToken(credential.access_token),
            model,
            base_url,
        );
        provider.oauth_refresh = Some(refresh);
        provider
    }

    #[cfg(test)]
    fn with_claude_code_oauth_refresh_for_test(
        credential: ClaudeCodeOAuthCredential,
        model: impl Into<String>,
        base_url: impl Into<String>,
        refresh_fn: impl Fn(&str) -> Result<ClaudeCodeOAuthCredential> + Send + Sync + 'static,
    ) -> Self {
        Self::with_claude_code_oauth_refresh_handler(
            credential,
            model,
            base_url,
            Arc::new(refresh_fn),
            None,
        )
    }

    fn with_credential(
        credential: AnthropicCredential,
        model: impl Into<String>,
        base_url: impl Into<String>,
    ) -> Self {
        Self {
            credential: Arc::new(Mutex::new(credential)),
            oauth_refresh: None,
            model: model.into(),
            base_url: base_url.into().trim_end_matches('/').to_string(),
            instructions: default_instructions(),
            client: reqwest::blocking::Client::new(),
            request_retry: ProviderRequestRetryConfig::default(),
        }
    }

    pub fn from_env(model: impl Into<String>) -> Result<Self> {
        let base_url = std::env::var("LLM_BROWSER_ANTHROPIC_BASE_URL")
            .unwrap_or_else(|_| "https://api.anthropic.com/v1".to_string());
        if let Ok(api_key) = std::env::var("LLM_BROWSER_ANTHROPIC_API_KEY")
            .or_else(|_| std::env::var("ANTHROPIC_API_KEY"))
        {
            if !api_key.trim().is_empty() {
                return Ok(Self::with_base_url(api_key, model, base_url));
            }
        }
        if let Ok(auth_token) = std::env::var("LLM_BROWSER_CLAUDE_CODE_OAUTH_TOKEN")
            .or_else(|_| std::env::var("CLAUDE_CODE_OAUTH_TOKEN"))
            .or_else(|_| std::env::var("LLM_BROWSER_ANTHROPIC_OAUTH_TOKEN"))
            .or_else(|_| std::env::var("ANTHROPIC_OAUTH_TOKEN"))
            .or_else(|_| std::env::var("ANTHROPIC_AUTH_TOKEN"))
        {
            if !auth_token.trim().is_empty() {
                return Ok(Self::with_auth_token(auth_token, model, base_url));
            }
        }
        bail!("set LLM_BROWSER_ANTHROPIC_API_KEY, ANTHROPIC_API_KEY, CLAUDE_CODE_OAUTH_TOKEN, or ANTHROPIC_AUTH_TOKEN")
    }

    pub fn with_instructions(mut self, instructions: impl Into<String>) -> Self {
        self.instructions = instructions.into();
        self
    }

    fn current_credential(&self) -> Result<AnthropicCredential> {
        self.credential
            .lock()
            .map_err(|_| anyhow!("Anthropic credential cache is poisoned"))
            .map(|credential| credential.clone())
    }

    fn replace_oauth_access_token(&self, access_token: impl Into<String>) -> Result<()> {
        *self
            .credential
            .lock()
            .map_err(|_| anyhow!("Anthropic credential cache is poisoned"))? =
            AnthropicCredential::AuthToken(access_token.into());
        Ok(())
    }

    fn send_messages_request(
        &self,
        body: &Value,
        credential: &AnthropicCredential,
        request_retry: ProviderRequestRetryConfig,
    ) -> Result<(reqwest::StatusCode, reqwest::header::HeaderMap, String), ProviderError> {
        send_provider_text_request(
            "send Anthropic Messages request",
            "read Anthropic Messages response body",
            request_retry,
            || self.build_messages_request(body, credential, "application/json"),
        )
    }

    fn send_messages_stream_request(
        &self,
        body: &Value,
        credential: &AnthropicCredential,
        request_retry: ProviderRequestRetryConfig,
    ) -> Result<reqwest::blocking::Response, ProviderError> {
        send_provider_request("send Anthropic Messages request", request_retry, || {
            self.build_messages_request(body, credential, "text/event-stream")
        })
    }

    fn build_messages_request(
        &self,
        body: &Value,
        credential: &AnthropicCredential,
        accept: &'static str,
    ) -> reqwest::blocking::RequestBuilder {
        let is_oauth = credential.is_oauth();
        let request = self
            .client
            .post(format!("{}/messages", self.base_url))
            .header("accept", accept)
            .header("content-type", "application/json")
            .header("anthropic-version", "2023-06-01")
            .header("anthropic-dangerous-direct-browser-access", "true");
        match credential {
            AnthropicCredential::ApiKey(api_key) if !is_oauth => request
                .header("x-api-key", api_key)
                .header("anthropic-beta", ANTHROPIC_BETA_FEATURES.join(","))
                .json(body),
            AnthropicCredential::ApiKey(auth_token)
            | AnthropicCredential::AuthToken(auth_token) => {
                let mut beta = vec!["claude-code-20250219", "oauth-2025-04-20"];
                beta.extend_from_slice(ANTHROPIC_BETA_FEATURES);
                request
                    .bearer_auth(auth_token)
                    .header("anthropic-beta", beta.join(","))
                    .header("user-agent", format!("claude-cli/{CLAUDE_CODE_VERSION}"))
                    .header("x-app", "cli")
                    .json(body)
            }
        }
    }

    fn messages_request_body(
        &self,
        turn: &ProviderTurn,
        is_oauth: bool,
        stream: bool,
    ) -> Result<Value> {
        let tools = tool_specs_to_anthropic_tools(&turn.tools, is_oauth);
        let instructions = turn.instructions_or_default(&self.instructions).to_string();
        let mut body = json!({
            "model": self.model,
            "max_tokens": 16000,
            "cache_control": { "type": "ephemeral" },
            "system": anthropic_system_blocks_with_developer_context(&instructions, &turn.messages, is_oauth),
            "messages": messages_to_anthropic_messages(&turn.messages, is_oauth)?,
        });
        if stream {
            body["stream"] = json!(true);
        }
        if !tools.is_empty() {
            body["tools"] = Value::Array(tools);
            body["tool_choice"] = json!({"type": "auto"});
        }
        Ok(body)
    }
}

impl ModelProvider for AnthropicMessagesProvider {
    fn provider_name(&self) -> &str {
        "anthropic"
    }

    fn model_name(&self) -> &str {
        &self.model
    }

    fn supports_namespace_tools(&self) -> bool {
        false
    }

    fn start_turn(&self, turn: ProviderTurn) -> Result<Vec<ModelEvent>> {
        let request_retry = self.request_retry.for_turn(&turn);
        let mut credential = self.current_credential()?;
        let is_oauth = credential.is_oauth();
        let body = self.messages_request_body(&turn, is_oauth, false)?;
        let (mut status, mut headers, mut body_text) =
            self.send_messages_request(&body, &credential, request_retry)?;
        if status == reqwest::StatusCode::UNAUTHORIZED && is_oauth {
            if let Some(refresh) = self.oauth_refresh.as_ref() {
                let refreshed = refresh.refresh().map_err(|error| {
                    ProviderError::non_retryable(
                        ProviderErrorKind::Unauthorized,
                        format!("refresh Claude Code OAuth token after 401: {error:#}"),
                    )
                })?;
                self.replace_oauth_access_token(refreshed.access_token.clone())?;
                credential = AnthropicCredential::AuthToken(refreshed.access_token);
                (status, headers, body_text) =
                    self.send_messages_request(&body, &credential, request_retry)?;
            }
        }
        if !status.is_success() {
            return Err(provider_http_status_error(
                "Anthropic Messages",
                status,
                &body_text,
                Some(&headers),
            )
            .into());
        }
        let body: Value = serde_json::from_str(&body_text).map_err(|error| {
            ProviderError::retryable(format!("parse Anthropic Messages JSON: {error}"), None)
        })?;
        parse_anthropic_messages_output(&body, &self.model, &turn.tools, is_oauth)
    }

    fn stream_turn(
        &self,
        turn: ProviderTurn,
        on_event: &mut dyn FnMut(ModelEvent) -> Result<()>,
    ) -> Result<()> {
        let request_retry = self.request_retry.for_turn(&turn);
        let stream_idle_timeout = stream_idle_timeout_for_turn(&turn);
        let mut credential = self.current_credential()?;
        let is_oauth = credential.is_oauth();
        let body = self.messages_request_body(&turn, is_oauth, true)?;
        let mut response = self.send_messages_stream_request(&body, &credential, request_retry)?;
        if response.status() == reqwest::StatusCode::UNAUTHORIZED && is_oauth {
            if let Some(refresh) = self.oauth_refresh.as_ref() {
                let refreshed = refresh.refresh().map_err(|error| {
                    ProviderError::non_retryable(
                        ProviderErrorKind::Unauthorized,
                        format!("refresh Claude Code OAuth token after 401: {error:#}"),
                    )
                })?;
                self.replace_oauth_access_token(refreshed.access_token.clone())?;
                credential = AnthropicCredential::AuthToken(refreshed.access_token);
                response = self.send_messages_stream_request(&body, &credential, request_retry)?;
            }
        }
        let status = response.status();
        if !status.is_success() {
            let headers = response.headers().clone();
            let body_text = response.text().unwrap_or_default();
            return Err(provider_http_status_error(
                "Anthropic Messages",
                status,
                &body_text,
                Some(&headers),
            )
            .into());
        }
        parse_anthropic_messages_sse_stream(
            response,
            &self.model,
            &turn.tools,
            is_oauth,
            stream_idle_timeout,
            on_event,
        )
    }
}

pub fn claude_code_oauth_pkce() -> (String, String) {
    let mut verifier_bytes = Vec::with_capacity(32);
    verifier_bytes.extend_from_slice(Uuid::new_v4().as_bytes());
    verifier_bytes.extend_from_slice(Uuid::new_v4().as_bytes());
    let verifier = URL_SAFE_NO_PAD.encode(verifier_bytes);
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    (verifier, challenge)
}

pub fn codex_oauth_pkce() -> (String, String) {
    let chars = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-._~";
    let mut rng = rand::rng();
    let verifier = (0..43)
        .map(|_| chars[rng.random_range(0..chars.len())] as char)
        .collect::<String>();
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    (verifier, challenge)
}

pub fn codex_oauth_state() -> String {
    let mut bytes = [0_u8; 32];
    rand::rng().fill(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

pub fn codex_oauth_authorize_url(challenge: &str, state: &str) -> String {
    form_url(
        CODEX_AUTHORIZE_URL,
        &[
            ("response_type", "code"),
            ("client_id", CODEX_OAUTH_CLIENT_ID),
            ("redirect_uri", CODEX_REDIRECT_URI),
            ("scope", CODEX_OAUTH_SCOPES),
            ("code_challenge", challenge),
            ("code_challenge_method", "S256"),
            ("id_token_add_organizations", "true"),
            ("codex_cli_simplified_flow", "true"),
            ("state", state),
            ("originator", "browser-use-terminal"),
        ],
    )
}

pub fn claude_code_oauth_authorize_url(verifier: &str, challenge: &str) -> String {
    form_url(
        CLAUDE_CODE_AUTHORIZE_URL,
        &[
            ("code", "true"),
            ("client_id", CLAUDE_CODE_CLIENT_ID),
            ("response_type", "code"),
            ("redirect_uri", CLAUDE_CODE_REDIRECT_URI),
            ("scope", CLAUDE_CODE_SCOPES),
            ("code_challenge", challenge),
            ("code_challenge_method", "S256"),
            ("state", verifier),
        ],
    )
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CodexAuthorization {
    pub code: Option<String>,
    pub state: Option<String>,
    pub error: Option<String>,
    pub error_description: Option<String>,
}

pub fn parse_codex_authorization_input(value: &str) -> CodexAuthorization {
    let mut stripped = value.trim();
    if stripped.is_empty() {
        return CodexAuthorization::default();
    }
    if let Some((_, query)) = stripped.split_once('?') {
        stripped = query.split('#').next().unwrap_or(query);
    }
    let mut authorization = CodexAuthorization::default();
    for (key, value) in parse_form_pairs(stripped) {
        match key.as_str() {
            "code" => authorization.code = Some(value),
            "state" => authorization.state = Some(value),
            "error" => authorization.error = Some(value),
            "error_description" => authorization.error_description = Some(value),
            _ => {}
        }
    }
    authorization
}

pub fn codex_callback_status(
    path: &str,
    expected_state: &str,
    authorization: &CodexAuthorization,
) -> u16 {
    if !path.starts_with(CODEX_CALLBACK_PATH) {
        404
    } else if authorization.error.is_some() {
        400
    } else if authorization.code.is_none() || authorization.state.as_deref() != Some(expected_state)
    {
        400
    } else {
        200
    }
}

pub struct CodexCallbackPage {
    pub status_text: &'static str,
    pub body: String,
    pub message: String,
}

pub fn codex_callback_page(status: u16, authorization: &CodexAuthorization) -> CodexCallbackPage {
    let error_message = authorization
        .error_description
        .as_deref()
        .or(authorization.error.as_deref())
        .unwrap_or("Codex authentication failed: missing code or state mismatch.");
    let (title, message) = match status {
        200 => (
            "Authorization Successful",
            "You can close this window and return to Terminal.",
        ),
        400 => ("Authorization Failed", error_message),
        _ => (
            "Callback Not Found",
            "This browser tab reached Browser Use, but the callback path was not recognized.",
        ),
    };

    CodexCallbackPage {
        status_text: http_status_text(status),
        body: codex_callback_html(status, title, message),
        message: message.to_string(),
    }
}

fn http_status_text(status: u16) -> &'static str {
    match status {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        _ => "Error",
    }
}

fn codex_callback_html(status: u16, title: &str, message: &str) -> String {
    let success = status == 200;
    let title = escape_html(title);
    let message = escape_html(message);
    let body_copy = if success {
        message.as_str()
    } else {
        "Return to the terminal and try signing in again."
    };
    let detail = if success {
        String::new()
    } else {
        format!(r#"<div class="error">{message}</div>"#)
    };
    let script = if success {
        r#"<script>setTimeout(() => window.close(), 2000);</script>"#
    } else {
        ""
    };

    format!(
        r#"<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <title>{title}</title>
  <link rel="preconnect" href="https://fonts.googleapis.com">
  <link rel="preconnect" href="https://fonts.gstatic.com" crossorigin>
  <link href="https://fonts.googleapis.com/css2?family=Geist:wght@400;500;650&family=Geist+Mono:wght@400;500&display=swap" rel="stylesheet">
  <style>
    :root {{
      color-scheme: light dark;
      --bg: #ffffff;
      --text: #111111;
      --muted: #666666;
      --detail-bg: #f5f5f5;
      --detail-text: #444444;
    }}
    @media (prefers-color-scheme: dark) {{
      :root {{
        --bg: #000000;
        --text: #eeeeee;
        --muted: #9ca3af;
        --detail-bg: #111111;
        --detail-text: #b8b8b8;
      }}
    }}
    html, body {{
      width: 100%;
      height: 100%;
      overflow: hidden;
    }}
    body {{
      margin: 0;
      background: var(--bg);
      color: var(--text);
      font: 14px/1.5 "Geist", ui-sans-serif, -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif;
    }}
    main {{
      position: fixed;
      top: 50%;
      left: 50%;
      width: min(460px, 100vw);
      padding: 32px;
      text-align: center;
      transform: translate(-50%, -50%);
    }}
    h1 {{
      margin: 0 0 8px;
      color: var(--text);
      font-size: 24px;
      font-weight: 650;
      line-height: 1.2;
      letter-spacing: 0;
    }}
    p {{
      margin: 0;
      color: var(--muted);
      font-size: 14px;
    }}
    .error {{
      margin-top: 16px;
      padding: 12px 14px;
      border-radius: 8px;
      background: var(--detail-bg);
      color: var(--detail-text);
      font: 12px/1.45 "Geist Mono", ui-monospace, SFMono-Regular, Menlo, Consolas, monospace;
      overflow-wrap: anywhere;
    }}
  </style>
</head>
<body>
  <main>
    <h1>{title}</h1>
    <p>{body_copy}</p>
    {detail}
  </main>
  {script}
</body>
</html>"#
    )
}

fn escape_html(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for c in value.chars() {
        match c {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&#39;"),
            _ => escaped.push(c),
        }
    }
    escaped
}

pub fn parse_claude_code_authorization_input(value: &str) -> ClaudeCodeAuthorization {
    let mut stripped = value.trim();
    if stripped.is_empty() {
        return ClaudeCodeAuthorization::default();
    }
    if let Some((_, query)) = stripped.split_once('?') {
        stripped = query.split('#').next().unwrap_or(query);
    }
    if stripped.contains("code=") || stripped.contains("state=") {
        let mut authorization = ClaudeCodeAuthorization::default();
        for (key, value) in parse_form_pairs(stripped) {
            match key.as_str() {
                "code" => authorization.code = Some(value),
                "state" => authorization.state = Some(value),
                _ => {}
            }
        }
        return authorization;
    }
    if let Some((code, state)) = stripped.split_once('#') {
        return ClaudeCodeAuthorization {
            code: Some(code.trim().to_string()),
            state: Some(state.trim().to_string()),
        };
    }
    ClaudeCodeAuthorization {
        code: Some(stripped.to_string()),
        state: None,
    }
}

pub fn exchange_codex_authorization_code(code: &str, verifier: &str) -> Result<CodexManagedAuth> {
    let body = form_body(&[
        ("grant_type", "authorization_code"),
        ("code", code),
        ("redirect_uri", CODEX_REDIRECT_URI),
        ("client_id", CODEX_OAUTH_CLIENT_ID),
        ("code_verifier", verifier),
    ]);
    let response = reqwest::blocking::Client::new()
        .post(CODEX_REFRESH_TOKEN_URL)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(body)
        .send()
        .context("send Codex OAuth token request")?;
    let status = response.status();
    let text = response.text().context("read Codex OAuth token response")?;
    if !status.is_success() {
        bail!(
            "Codex OAuth token request failed ({status}): {}",
            truncate_error_body(&text)
        );
    }
    let payload: CodexOAuthTokenResponse =
        serde_json::from_str(&text).context("parse Codex OAuth token response")?;
    let access_token = payload
        .access_token
        .filter(|value| !value.trim().is_empty())
        .context("Codex OAuth response missing access_token")?;
    let refresh_token = payload
        .refresh_token
        .filter(|value| !value.trim().is_empty())
        .context("Codex OAuth response missing refresh_token")?;
    let account_id = payload
        .id_token
        .as_deref()
        .and_then(account_id_from_id_token)
        .or_else(|| account_id_from_id_token(&access_token))
        .context("Codex OAuth response missing account id")?;
    Ok(CodexManagedAuth::new(CodexManagedAuthSnapshot {
        access_token,
        account_id,
        id_token: payload.id_token,
        refresh_token: Some(refresh_token),
        source_path: None,
        last_refresh: Some(Utc::now()),
    }))
}

pub fn exchange_claude_code_authorization_code(
    code: &str,
    state: &str,
    verifier: &str,
) -> Result<ClaudeCodeOAuthCredential> {
    post_claude_code_oauth_token(json!({
        "grant_type": "authorization_code",
        "client_id": CLAUDE_CODE_CLIENT_ID,
        "code": code,
        "state": state,
        "redirect_uri": CLAUDE_CODE_REDIRECT_URI,
        "code_verifier": verifier,
    }))
}

pub fn refresh_claude_code_oauth(refresh_token: &str) -> Result<ClaudeCodeOAuthCredential> {
    if refresh_token.trim().is_empty() {
        bail!("missing Claude Code refresh token");
    }
    post_claude_code_oauth_token(json!({
        "grant_type": "refresh_token",
        "client_id": CLAUDE_CODE_CLIENT_ID,
        "refresh_token": refresh_token.trim(),
    }))
}

pub fn is_claude_code_oauth_token(token: &str) -> bool {
    token.starts_with("sk-ant-oat") || token.contains("sk-ant-oat")
}

fn post_claude_code_oauth_token(body: Value) -> Result<ClaudeCodeOAuthCredential> {
    let client = reqwest::blocking::Client::new();
    let response = client
        .post(CLAUDE_CODE_TOKEN_URL)
        .header("content-type", "application/json")
        .header("accept", "application/json")
        .json(&body)
        .send()
        .context("send Anthropic OAuth token request")?;
    let status = response.status();
    let text = response
        .text()
        .context("read Anthropic OAuth token response")?;
    if !status.is_success() {
        bail!(
            "Anthropic OAuth token request failed ({status}): {}",
            truncate_error_body(&text)
        );
    }
    let payload: ClaudeCodeTokenResponse =
        serde_json::from_str(&text).context("parse Anthropic OAuth token response")?;
    let access_token = payload
        .access_token
        .filter(|value| !value.trim().is_empty())
        .context("Anthropic OAuth response missing access_token")?;
    let refresh_token = payload
        .refresh_token
        .filter(|value| !value.trim().is_empty())
        .context("Anthropic OAuth response missing refresh_token")?;
    let expires_in = payload
        .expires_in
        .filter(|value| *value > 0)
        .context("Anthropic OAuth response missing expires_in")?;
    Ok(ClaudeCodeOAuthCredential {
        access_token,
        refresh_token,
        expires_ms: unix_ms_now() + expires_in.saturating_mul(1000) - 5 * 60 * 1000,
    })
}

fn unix_ms_now() -> i64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(i64::MAX as u128) as i64
}

fn form_url(base: &str, params: &[(&str, &str)]) -> String {
    format!("{base}?{}", form_body(params))
}

fn form_body(params: &[(&str, &str)]) -> String {
    params
        .iter()
        .map(|(key, value)| format!("{}={}", percent_encode(key), percent_encode(value)))
        .collect::<Vec<_>>()
        .join("&")
}

fn parse_form_pairs(value: &str) -> Vec<(String, String)> {
    value
        .split('&')
        .filter_map(|pair| {
            let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
            Some((percent_decode(key)?, percent_decode(value)?))
        })
        .collect()
}

fn percent_encode(value: &str) -> String {
    let mut encoded = String::new();
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                encoded.push(byte as char)
            }
            b' ' => encoded.push('+'),
            _ => encoded.push_str(&format!("%{byte:02X}")),
        }
    }
    encoded
}

fn percent_decode(value: &str) -> Option<String> {
    let mut bytes = Vec::with_capacity(value.len());
    let mut iter = value.as_bytes().iter().copied();
    while let Some(byte) = iter.next() {
        match byte {
            b'+' => bytes.push(b' '),
            b'%' => {
                let hi = iter.next()?;
                let lo = iter.next()?;
                let hex = [hi, lo];
                let hex = std::str::from_utf8(&hex).ok()?;
                bytes.push(u8::from_str_radix(hex, 16).ok()?);
            }
            _ => bytes.push(byte),
        }
    }
    String::from_utf8(bytes).ok()
}

fn truncate_error_body(value: &str) -> String {
    let mut out = value.chars().take(1000).collect::<String>();
    if value.chars().count() > 1000 {
        out.push_str("...");
    }
    out
}

const CYBER_POLICY_FALLBACK_MESSAGE: &str =
    "This request has been flagged for possible cybersecurity risk.";

#[derive(Default)]
struct ParsedProviderHttpError {
    code: Option<String>,
    error_type: Option<String>,
    message: Option<String>,
    plan_type: Option<String>,
    resets_at: Option<i64>,
}

fn provider_send_error(operation: &str, error: &reqwest::Error) -> ProviderError {
    let message = format!("{operation}: {error}");
    if error.is_timeout() {
        ProviderError {
            kind: ProviderErrorKind::RequestTimeout,
            message,
            retry_delay: None,
            http_status_code: None,
            rate_limits: None,
        }
    } else {
        ProviderError::stream(message)
    }
}

fn provider_http_status_error(
    operation: &str,
    status: reqwest::StatusCode,
    body: &str,
    headers: Option<&reqwest::header::HeaderMap>,
) -> ProviderError {
    let parsed = parse_provider_http_error_body(body);
    let message = format!(
        "{operation} request failed ({status}): {}",
        truncate_error_body(body)
    );
    let code = parsed.code.as_deref();
    let error_type = parsed.error_type.as_deref();

    if status == reqwest::StatusCode::UNAUTHORIZED {
        return ProviderError::non_retryable(ProviderErrorKind::Unauthorized, message)
            .with_http_status_code(status);
    }

    if status == reqwest::StatusCode::SERVICE_UNAVAILABLE
        && matches!(code, Some("server_is_overloaded" | "slow_down"))
    {
        return ProviderError::non_retryable(ProviderErrorKind::ServerOverloaded, message)
            .with_http_status_code(status);
    }

    if status == reqwest::StatusCode::BAD_REQUEST {
        if code == Some("invalid_image")
            || invalid_image_error_message(parsed.message.as_deref(), body)
        {
            return ProviderError::non_retryable(ProviderErrorKind::InvalidImage, message)
                .with_http_status_code(status);
        }
        if code == Some("cyber_policy") {
            return ProviderError::non_retryable(
                ProviderErrorKind::CyberPolicy,
                parsed
                    .message
                    .filter(|message| !message.trim().is_empty())
                    .unwrap_or_else(|| CYBER_POLICY_FALLBACK_MESSAGE.to_string()),
            )
            .with_http_status_code(status);
        }
        return ProviderError::non_retryable(ProviderErrorKind::InvalidRequest, message)
            .with_http_status_code(status);
    }

    if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
        let error = match error_type {
            Some("usage_limit_reached") => ProviderError::non_retryable(
                ProviderErrorKind::UsageLimitReached,
                usage_limit_message(&parsed, headers),
            )
            .with_rate_limits(headers.and_then(|headers| {
                let active_limit = parse_header_str(headers, "x-codex-active-limit");
                parse_rate_limit_for_limit(headers, active_limit.as_deref())
            })),
            Some("usage_not_included") => {
                ProviderError::non_retryable(ProviderErrorKind::UsageNotIncluded, message)
            }
            _ => ProviderError::non_retryable(ProviderErrorKind::RetryLimit, message),
        };
        return error.with_http_status_code(status);
    }

    if status == reqwest::StatusCode::INTERNAL_SERVER_ERROR {
        return ProviderError {
            kind: ProviderErrorKind::InternalServerError,
            message,
            retry_delay: None,
            http_status_code: Some(status.as_u16()),
            rate_limits: None,
        };
    }

    ProviderError {
        kind: ProviderErrorKind::UnexpectedStatus,
        message,
        retry_delay: None,
        http_status_code: Some(status.as_u16()),
        rate_limits: None,
    }
}

fn parse_provider_http_error_body(body: &str) -> ParsedProviderHttpError {
    let Ok(value) = serde_json::from_str::<Value>(body) else {
        return ParsedProviderHttpError::default();
    };
    let Some(error) = value.get("error") else {
        return ParsedProviderHttpError::default();
    };
    ParsedProviderHttpError {
        code: error
            .get("code")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned),
        error_type: error
            .get("type")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned),
        message: error
            .get("message")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned),
        plan_type: error
            .get("plan_type")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned),
        resets_at: error.get("resets_at").and_then(Value::as_i64),
    }
}

fn usage_limit_message(
    parsed: &ParsedProviderHttpError,
    headers: Option<&reqwest::header::HeaderMap>,
) -> String {
    let rate_limit_snapshot = headers.and_then(|headers| {
        let active_limit = parse_header_str(headers, "x-codex-active-limit");
        parse_rate_limit_for_limit(headers, active_limit.as_deref())
    });
    if let Some(limit_name) = rate_limit_snapshot
        .as_ref()
        .and_then(|snapshot| snapshot.limit_name.as_deref())
        .map(str::trim)
        .filter(|name| !name.is_empty())
    {
        if !limit_name.eq_ignore_ascii_case("codex") {
            return format!(
                "You've hit your usage limit for {limit_name}. Switch to another model now,{}",
                retry_suffix_after_or(parsed.resets_at)
            );
        }
    }

    if let Some(rate_limit_reached_type) =
        headers.and_then(|headers| parse_header_str(headers, "x-codex-rate-limit-reached-type"))
    {
        match rate_limit_reached_type.trim() {
            "workspace_owner_credits_depleted" => {
                return "Your workspace is out of credits. Add credits to continue.".to_string();
            }
            "workspace_member_credits_depleted" => {
                return "Your workspace is out of credits. Ask your workspace owner to refill in order to continue.".to_string();
            }
            "workspace_owner_usage_limit_reached" => {
                return "You hit your spend cap set in your workspace. Increase your spend cap to continue.".to_string();
            }
            "workspace_member_usage_limit_reached" => {
                return "You hit your spend cap set by the owner of your workspace. Ask an owner to increase your spend cap to continue.".to_string();
            }
            "rate_limit_reached" => {}
            _ => {}
        }
    }

    let promo = headers
        .and_then(|headers| parse_header_str(headers, "x-codex-promo-message"))
        .filter(|message| !message.trim().is_empty());
    if let Some(promo) = promo.as_deref() {
        return format!(
            "You've hit your usage limit. {promo},{}",
            retry_suffix_after_or(parsed.resets_at)
        );
    }

    match parsed
        .plan_type
        .as_deref()
        .map(|plan| plan.to_ascii_lowercase())
        .as_deref()
    {
        Some("plus") => format!(
            "You've hit your usage limit. Upgrade to Pro (https://chatgpt.com/explore/pro), visit https://chatgpt.com/codex/settings/usage to purchase more credits{}",
            retry_suffix_after_or(parsed.resets_at)
        ),
        Some("team")
        | Some("self_serve_business_usage_based")
        | Some("business")
        | Some("enterprise_cbp_usage_based") => format!(
            "You've hit your usage limit. To get more access now, send a request to your admin{}",
            retry_suffix_after_or(parsed.resets_at)
        ),
        Some("free") | Some("go") => format!(
            "You've hit your usage limit. Upgrade to Plus to continue using Codex (https://chatgpt.com/explore/plus),{}",
            retry_suffix_after_or(parsed.resets_at)
        ),
        Some("pro") | Some("prolite") => format!(
            "You've hit your usage limit. Visit https://chatgpt.com/codex/settings/usage to purchase more credits{}",
            retry_suffix_after_or(parsed.resets_at)
        ),
        Some("enterprise") | Some("hc") | Some("education") | Some("edu") => {
            format!("You've hit your usage limit.{}", retry_suffix(parsed.resets_at))
        }
        Some(_) | None => {
            format!("You've hit your usage limit.{}", retry_suffix(parsed.resets_at))
        }
    }
}

fn retry_suffix(resets_at: Option<i64>) -> String {
    if let Some(resets_at) = resets_at.and_then(|seconds| Local.timestamp_opt(seconds, 0).single())
    {
        let formatted = format_retry_timestamp(&resets_at);
        format!(" Try again at {formatted}.")
    } else {
        " Try again later.".to_string()
    }
}

fn retry_suffix_after_or(resets_at: Option<i64>) -> String {
    if let Some(resets_at) = resets_at.and_then(|seconds| Local.timestamp_opt(seconds, 0).single())
    {
        let formatted = format_retry_timestamp(&resets_at);
        format!(" or try again at {formatted}.")
    } else {
        " or try again later.".to_string()
    }
}

fn format_retry_timestamp(resets_at: &DateTime<Local>) -> String {
    let local_now = Local::now();
    if resets_at.date_naive() == local_now.date_naive() {
        resets_at.format("%-I:%M %p").to_string()
    } else {
        let suffix = day_suffix(resets_at.day());
        resets_at
            .format(&format!("%b %-d{suffix}, %Y %-I:%M %p"))
            .to_string()
    }
}

fn day_suffix(day: u32) -> &'static str {
    match day {
        11..=13 => "th",
        _ => match day % 10 {
            1 => "st",
            2 => "nd",
            3 => "rd",
            _ => "th",
        },
    }
}

fn parse_all_rate_limits(headers: &reqwest::header::HeaderMap) -> Vec<RateLimitSnapshot> {
    let mut snapshots = Vec::new();
    if let Some(snapshot) = parse_rate_limit_for_limit(headers, None) {
        snapshots.push(snapshot);
    }

    let mut limit_ids = BTreeSet::new();
    for name in headers.keys() {
        if let Some(limit_id) = header_name_to_limit_id(name.as_str()) {
            if limit_id != "codex" {
                limit_ids.insert(limit_id);
            }
        }
    }
    snapshots.extend(limit_ids.into_iter().filter_map(|limit_id| {
        let snapshot = parse_rate_limit_for_limit(headers, Some(&limit_id))?;
        has_rate_limit_data(&snapshot).then_some(snapshot)
    }));
    snapshots
}

fn parse_rate_limit_for_limit(
    headers: &reqwest::header::HeaderMap,
    limit_id: Option<&str>,
) -> Option<RateLimitSnapshot> {
    let normalized_limit = limit_id
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .unwrap_or("codex")
        .to_ascii_lowercase()
        .replace('_', "-");
    let prefix = format!("x-{normalized_limit}");
    let primary = parse_rate_limit_window(
        headers,
        &format!("{prefix}-primary-used-percent"),
        &format!("{prefix}-primary-window-minutes"),
        &format!("{prefix}-primary-reset-at"),
    );
    let secondary = parse_rate_limit_window(
        headers,
        &format!("{prefix}-secondary-used-percent"),
        &format!("{prefix}-secondary-window-minutes"),
        &format!("{prefix}-secondary-reset-at"),
    );
    let limit_name = parse_header_str(headers, &format!("{prefix}-limit-name"))
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());
    Some(RateLimitSnapshot {
        limit_id: Some(normalize_limit_id(normalized_limit)),
        limit_name,
        primary,
        secondary,
        credits: parse_credits_snapshot(headers),
        plan_type: None,
        rate_limit_reached_type: None,
    })
}

fn parse_rate_limit_window(
    headers: &reqwest::header::HeaderMap,
    used_percent_header: &str,
    window_minutes_header: &str,
    resets_at_header: &str,
) -> Option<RateLimitWindow> {
    let used_percent = parse_header_f64(headers, used_percent_header)?;
    let window_minutes = parse_header_i64(headers, window_minutes_header);
    let resets_at = parse_header_i64(headers, resets_at_header);
    let has_data = used_percent != 0.0
        || window_minutes.is_some_and(|minutes| minutes != 0)
        || resets_at.is_some();
    has_data.then_some(RateLimitWindow {
        used_percent,
        window_minutes,
        resets_at,
    })
}

fn parse_credits_snapshot(headers: &reqwest::header::HeaderMap) -> Option<CreditsSnapshot> {
    Some(CreditsSnapshot {
        has_credits: parse_header_bool(headers, "x-codex-credits-has-credits")?,
        unlimited: parse_header_bool(headers, "x-codex-credits-unlimited")?,
        balance: parse_header_str(headers, "x-codex-credits-balance")
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty()),
    })
}

fn parse_header_f64(headers: &reqwest::header::HeaderMap, name: &str) -> Option<f64> {
    parse_header_str(headers, name)?
        .parse::<f64>()
        .ok()
        .filter(|value| value.is_finite())
}

fn parse_header_i64(headers: &reqwest::header::HeaderMap, name: &str) -> Option<i64> {
    parse_header_str(headers, name)?.parse::<i64>().ok()
}

fn parse_header_bool(headers: &reqwest::header::HeaderMap, name: &str) -> Option<bool> {
    let raw = parse_header_str(headers, name)?;
    if raw.eq_ignore_ascii_case("true") || raw == "1" {
        Some(true)
    } else if raw.eq_ignore_ascii_case("false") || raw == "0" {
        Some(false)
    } else {
        None
    }
}

fn parse_header_str(headers: &reqwest::header::HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string)
}

fn header_name_to_limit_id(header_name: &str) -> Option<String> {
    let suffix = "-primary-used-percent";
    let prefix = header_name.to_ascii_lowercase();
    let prefix = prefix.strip_suffix(suffix)?;
    let limit = prefix.strip_prefix("x-")?;
    Some(normalize_limit_id(limit))
}

fn normalize_limit_id(name: impl Into<String>) -> String {
    name.into().trim().to_ascii_lowercase().replace('-', "_")
}

fn has_rate_limit_data(snapshot: &RateLimitSnapshot) -> bool {
    snapshot.primary.is_some() || snapshot.secondary.is_some() || snapshot.credits.is_some()
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CodexAuth {
    pub access_token: String,
    pub account_id: String,
}

impl CodexAuth {
    pub fn new(access_token: impl Into<String>, account_id: impl Into<String>) -> Self {
        Self {
            access_token: access_token.into(),
            account_id: account_id.into(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CodexManagedAuthSnapshot {
    pub access_token: String,
    pub account_id: String,
    pub id_token: Option<String>,
    pub refresh_token: Option<String>,
    pub source_path: Option<PathBuf>,
    pub last_refresh: Option<DateTime<Utc>>,
}

impl CodexManagedAuthSnapshot {
    pub fn current_auth(&self) -> CodexAuth {
        CodexAuth::new(self.access_token.clone(), self.account_id.clone())
    }

    fn has_refresh_token(&self) -> bool {
        self.refresh_token
            .as_deref()
            .is_some_and(|token| !token.trim().is_empty())
    }
}

#[derive(Clone, Debug)]
pub struct CodexManagedAuth {
    snapshot: Arc<Mutex<CodexManagedAuthSnapshot>>,
    client: reqwest::blocking::Client,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CodexManagedReloadOutcome {
    ReloadedChanged,
    ReloadedNoChange,
}

impl CodexManagedAuth {
    pub fn new(snapshot: CodexManagedAuthSnapshot) -> Self {
        Self {
            snapshot: Arc::new(Mutex::new(snapshot)),
            client: reqwest::blocking::Client::new(),
        }
    }

    pub fn from_stored_parts(
        access_token: impl Into<String>,
        account_id: impl Into<String>,
        id_token: Option<String>,
        refresh_token: Option<String>,
        source_path: Option<PathBuf>,
        last_refresh: Option<String>,
    ) -> Self {
        Self::new(CodexManagedAuthSnapshot {
            access_token: access_token.into(),
            account_id: account_id.into(),
            id_token,
            refresh_token,
            source_path,
            last_refresh: last_refresh
                .as_deref()
                .and_then(|value| DateTime::parse_from_rfc3339(value.trim()).ok())
                .map(|value| value.with_timezone(&Utc)),
        })
    }

    pub fn current_snapshot(&self) -> Result<CodexManagedAuthSnapshot> {
        self.snapshot
            .lock()
            .map_err(|_| anyhow!("Codex managed auth cache is poisoned"))
            .map(|guard| guard.clone())
    }

    pub fn current_auth(&self) -> Result<CodexAuth> {
        Ok(self.current_snapshot()?.current_auth())
    }

    pub fn reload_if_account_id_matches(
        &self,
        expected_account_id: &str,
    ) -> Result<CodexManagedReloadOutcome> {
        let source_path = self
            .current_snapshot()?
            .source_path
            .context("Codex managed auth has no source path to reload")?;
        let next = load_codex_managed_auth_snapshot_from_file(&source_path)?;
        if next.account_id.trim() != expected_account_id.trim() {
            bail!(
                "Codex auth reload skipped because account id changed from {} to {}",
                expected_account_id,
                next.account_id
            );
        }
        let mut guard = self
            .snapshot
            .lock()
            .map_err(|_| anyhow!("Codex managed auth cache is poisoned"))?;
        let changed = *guard != next;
        *guard = next;
        Ok(if changed {
            CodexManagedReloadOutcome::ReloadedChanged
        } else {
            CodexManagedReloadOutcome::ReloadedNoChange
        })
    }

    pub fn refresh_from_authority(&self) -> Result<CodexManagedAuthSnapshot> {
        let current = self.current_snapshot()?;
        let refresh_token = current
            .refresh_token
            .as_deref()
            .filter(|token| !token.trim().is_empty())
            .context("Codex managed auth has no refresh token")?
            .trim()
            .to_string();
        let refreshed = request_codex_token_refresh(&self.client, refresh_token)?;
        let mut next = current.clone();
        if let Some(id_token) = refreshed.id_token {
            next.id_token = Some(id_token);
        }
        if let Some(access_token) = refreshed.access_token {
            next.access_token = access_token;
        }
        if let Some(refresh_token) = refreshed.refresh_token {
            next.refresh_token = Some(refresh_token);
        }
        next.last_refresh = Some(Utc::now());
        if let Some(path) = next.source_path.as_ref() {
            persist_codex_managed_auth_snapshot(path, &next)?;
            next = load_codex_managed_auth_snapshot_from_file(path)?;
        }
        let mut guard = self
            .snapshot
            .lock()
            .map_err(|_| anyhow!("Codex managed auth cache is poisoned"))?;
        *guard = next.clone();
        Ok(next)
    }

    pub fn refresh_if_stale(&self) -> Result<Option<CodexManagedAuthSnapshot>> {
        if !codex_managed_auth_is_stale(&self.current_snapshot()?) {
            return Ok(None);
        }
        self.refresh_from_authority().map(Some)
    }
}

#[derive(Clone, Debug)]
pub struct CodexResponsesProvider {
    auth: Arc<Mutex<CodexAuth>>,
    managed_auth: Option<CodexManagedAuth>,
    model: String,
    base_url: String,
    instructions: String,
    client: reqwest::blocking::Client,
    previous_response_state: SharedPreviousResponsesState,
    request_retry: ProviderRequestRetryConfig,
}

impl CodexResponsesProvider {
    pub fn new(auth: CodexAuth, model: impl Into<String>) -> Self {
        Self::with_base_url(auth, model, "https://chatgpt.com/backend-api")
    }

    pub fn with_base_url(
        auth: CodexAuth,
        model: impl Into<String>,
        base_url: impl Into<String>,
    ) -> Self {
        Self {
            auth: Arc::new(Mutex::new(auth)),
            managed_auth: None,
            model: model.into(),
            base_url: base_url.into().trim_end_matches('/').to_string(),
            instructions: default_instructions(),
            client: reqwest::blocking::Client::new(),
            previous_response_state: Arc::new(Mutex::new(None)),
            request_retry: ProviderRequestRetryConfig::default(),
        }
    }

    pub fn with_managed_base_url(
        auth: CodexManagedAuth,
        model: impl Into<String>,
        base_url: impl Into<String>,
    ) -> Result<Self> {
        let current_auth = auth.current_auth()?;
        Ok(Self {
            auth: Arc::new(Mutex::new(current_auth)),
            managed_auth: Some(auth),
            model: model.into(),
            base_url: base_url.into().trim_end_matches('/').to_string(),
            instructions: default_instructions(),
            client: reqwest::blocking::Client::new(),
            previous_response_state: Arc::new(Mutex::new(None)),
            request_retry: ProviderRequestRetryConfig::default(),
        })
    }

    pub fn from_env(model: impl Into<String>) -> Result<Self> {
        let model = model.into();
        let base_url = std::env::var("LLM_BROWSER_CODEX_BASE_URL")
            .unwrap_or_else(|_| "https://chatgpt.com/backend-api".to_string());
        if std::env::var("LLM_BROWSER_CODEX_ACCESS_TOKEN").is_err() {
            if let Ok(managed_auth) = load_codex_managed_auth() {
                return Self::with_managed_base_url(managed_auth, model, base_url);
            }
        }
        let auth = load_codex_auth()?;
        Ok(Self::with_base_url(auth, model, base_url))
    }

    pub fn with_instructions(mut self, instructions: impl Into<String>) -> Self {
        self.instructions = instructions.into();
        self
    }
}

impl ModelProvider for CodexResponsesProvider {
    fn provider_name(&self) -> &str {
        "codex"
    }

    fn model_name(&self) -> &str {
        &self.model
    }

    fn supports_hosted_web_search(&self) -> bool {
        true
    }

    fn supports_hosted_image_generation(&self) -> bool {
        true
    }

    fn start_turn(&self, turn: ProviderTurn) -> Result<Vec<ModelEvent>> {
        let stream_idle_timeout = stream_idle_timeout_for_turn(&turn);
        let turn_state = turn.turn_state.clone();
        let (response, baseline, model_info) = self.send_turn_request(turn)?;
        record_turn_state_from_headers(response.headers(), turn_state.as_ref())?;
        match parse_responses_sse(response, &self.model, stream_idle_timeout) {
            Ok(events) => {
                let (response_id, response_items) = previous_response_result_from_events(&events);
                let response_items =
                    previous_response_prefix_items_for_model(response_items, model_info);
                update_previous_response_state(
                    &self.previous_response_state,
                    baseline,
                    response_id,
                    response_items,
                )?;
                Ok(events)
            }
            Err(error) => {
                clear_previous_response_state(&self.previous_response_state)?;
                Err(error)
            }
        }
    }

    fn stream_turn(
        &self,
        turn: ProviderTurn,
        on_event: &mut dyn FnMut(ModelEvent) -> Result<()>,
    ) -> Result<()> {
        let stream_idle_timeout = stream_idle_timeout_for_turn(&turn);
        let turn_state = turn.turn_state.clone();
        let (response, baseline, model_info) = self.send_turn_request(turn)?;
        record_turn_state_from_headers(response.headers(), turn_state.as_ref())?;
        let mut response_items = Vec::new();
        let mut response_id = None;
        let result =
            parse_responses_sse_stream(response, &self.model, stream_idle_timeout, &mut |event| {
                match &event {
                    ModelEvent::ResponseOutputItem { item } => response_items.push(item.clone()),
                    ModelEvent::ResponseCompleted {
                        response_id: completed_response_id,
                        ..
                    } => {
                        if let Some(completed_response_id) =
                            completed_response_id.as_deref().filter(|id| !id.is_empty())
                        {
                            response_id = Some(completed_response_id.to_string());
                        }
                    }
                    _ => {}
                }
                on_event(event)
            });
        match result {
            Ok(()) => {
                let response_items =
                    previous_response_prefix_items_for_model(response_items, model_info);
                update_previous_response_state(
                    &self.previous_response_state,
                    baseline,
                    response_id,
                    response_items,
                )
            }
            Err(error) => {
                clear_previous_response_state(&self.previous_response_state)?;
                Err(error)
            }
        }
    }
}

impl CodexResponsesProvider {
    fn current_auth(&self) -> Result<CodexAuth> {
        self.auth
            .lock()
            .map_err(|_| anyhow!("Codex auth cache is poisoned"))
            .map(|guard| guard.clone())
    }

    fn replace_auth(&self, auth: CodexAuth) -> Result<()> {
        *self
            .auth
            .lock()
            .map_err(|_| anyhow!("Codex auth cache is poisoned"))? = auth;
        Ok(())
    }

    fn send_turn_request(
        &self,
        turn: ProviderTurn,
    ) -> Result<(
        reqwest::blocking::Response,
        ResponsesRequestBaseline,
        ModelRequestInfo,
    )> {
        let request_retry = self.request_retry.for_turn(&turn);
        let extra_headers =
            extra_headers_with_turn_state(turn.extra_headers.clone(), turn.turn_state.as_ref())?;
        let instructions = turn.instructions_or_default(&self.instructions).to_string();
        let model_info = model_request_info_for_turn(&self.model, &turn);
        let input = messages_to_responses_input_for_model(&turn.messages, &model_info)?;
        let tools = tool_specs_to_responses_tools_with_hosted(&turn.tools, &turn.hosted_tools);
        let mut body = json!({
            "model": self.model,
            "input": input,
            "instructions": instructions,
            "store": false,
            "stream": true,
            "tool_choice": "auto",
            "include": [],
            "parallel_tool_calls": model_info.supports_parallel_tool_calls,
        });
        apply_model_request_settings(
            &mut body,
            &turn.model_settings,
            &model_info,
            turn.output_schema.as_ref(),
            turn.output_schema_strict,
        );
        if let Some(prompt_cache_key) = turn
            .prompt_cache_key
            .as_deref()
            .filter(|key| !key.is_empty())
        {
            body["prompt_cache_key"] = Value::String(prompt_cache_key.to_string());
        }
        if let Some(client_metadata) = non_empty_client_metadata(turn.client_metadata) {
            body["client_metadata"] = Value::Object(client_metadata);
        }
        if !tools.is_empty() {
            body["tools"] = Value::Array(tools);
        }
        let baseline = prepare_previous_response_request_body(
            &mut body,
            &self.previous_response_state,
            turn.previous_response_id.as_deref(),
            /*auto_previous_response_reuse*/ false,
        )?;

        let expected_account_id = self.current_auth()?.account_id;
        let mut response = self.send_codex_request(&body, extra_headers.as_ref(), request_retry)?;
        if response.status() == reqwest::StatusCode::UNAUTHORIZED {
            if let Some(managed_auth) = self.managed_auth.as_ref() {
                drop(response);
                match managed_auth.reload_if_account_id_matches(&expected_account_id) {
                    Ok(_) => {
                        self.replace_auth(managed_auth.current_auth()?)?;
                        response =
                            self.send_codex_request(&body, extra_headers.as_ref(), request_retry)?;
                    }
                    Err(error) => {
                        clear_previous_response_state(&self.previous_response_state)?;
                        return Err(ProviderError::non_retryable(
                            ProviderErrorKind::Unauthorized,
                            format!("reload Codex auth after 401: {error:#}"),
                        )
                        .into());
                    }
                }
                if response.status() == reqwest::StatusCode::UNAUTHORIZED {
                    drop(response);
                    match managed_auth.refresh_from_authority() {
                        Ok(snapshot) => {
                            self.replace_auth(snapshot.current_auth())?;
                            response = self.send_codex_request(
                                &body,
                                extra_headers.as_ref(),
                                request_retry,
                            )?;
                        }
                        Err(error) => {
                            clear_previous_response_state(&self.previous_response_state)?;
                            return Err(ProviderError::non_retryable(
                                ProviderErrorKind::Unauthorized,
                                format!("refresh Codex auth after 401: {error:#}"),
                            )
                            .into());
                        }
                    }
                }
            }
        }
        let status = response.status();
        if !status.is_success() {
            let headers = response.headers().clone();
            let body = response.text().unwrap_or_default();
            clear_previous_response_state(&self.previous_response_state)?;
            return Err(provider_http_status_error(
                "Codex Responses",
                status,
                &body,
                Some(&headers),
            )
            .into());
        }
        Ok((response, baseline, model_info))
    }

    fn send_codex_request(
        &self,
        body: &Value,
        extra_headers: Option<&HashMap<String, String>>,
        request_retry: ProviderRequestRetryConfig,
    ) -> Result<reqwest::blocking::Response> {
        let auth = self.current_auth()?;
        match send_provider_request("send Codex Responses request", request_retry, || {
            let request = self
                .client
                .post(codex_responses_url(&self.base_url))
                .bearer_auth(&auth.access_token)
                .header("chatgpt-account-id", &auth.account_id)
                .header("originator", "browser-use-terminal")
                .header("Accept", "text/event-stream");
            apply_extra_headers(request, extra_headers).json(body)
        }) {
            Ok(response) => Ok(response),
            Err(error) => {
                clear_previous_response_state(&self.previous_response_state)?;
                Err(error.into())
            }
        }
    }
}

fn codex_responses_url(base_url: &str) -> String {
    let normalized = base_url.trim_end_matches('/');
    if normalized.ends_with("/codex/responses") {
        normalized.to_string()
    } else if normalized.ends_with("/codex") {
        format!("{normalized}/responses")
    } else {
        format!("{normalized}/codex/responses")
    }
}

fn is_azure_responses_base_url(base_url: &str) -> bool {
    let base_url = base_url.to_ascii_lowercase();
    const AZURE_MARKERS: [&str; 6] = [
        "openai.azure.",
        "cognitiveservices.azure.",
        "aoai.azure.",
        "azure-api.",
        "azurefd.",
        "windows.net/openai",
    ];
    AZURE_MARKERS.iter().any(|marker| base_url.contains(marker))
}

fn non_empty_client_metadata(
    client_metadata: Option<HashMap<String, String>>,
) -> Option<serde_json::Map<String, Value>> {
    let metadata = client_metadata?;
    let mut out = serde_json::Map::new();
    for (key, value) in metadata {
        if !key.is_empty() && !value.is_empty() {
            out.insert(key, Value::String(value));
        }
    }
    (!out.is_empty()).then_some(out)
}

fn apply_extra_headers(
    mut request: reqwest::blocking::RequestBuilder,
    extra_headers: Option<&HashMap<String, String>>,
) -> reqwest::blocking::RequestBuilder {
    let Some(extra_headers) = extra_headers else {
        return request;
    };
    let mut headers = extra_headers.iter().collect::<Vec<_>>();
    headers.sort_by(|(left, _), (right, _)| left.cmp(right));
    for (name, value) in headers {
        if name.is_empty() || value.is_empty() {
            continue;
        }
        let Ok(name) = reqwest::header::HeaderName::from_bytes(name.as_bytes()) else {
            continue;
        };
        let Ok(value) = reqwest::header::HeaderValue::from_str(value) else {
            continue;
        };
        request = request.header(name, value);
    }
    request
}

fn apply_query_params(
    request: reqwest::blocking::RequestBuilder,
    query_params: &[(String, String)],
) -> reqwest::blocking::RequestBuilder {
    if query_params.is_empty() {
        request
    } else {
        request.query(query_params)
    }
}

fn extra_headers_with_turn_state(
    mut extra_headers: Option<HashMap<String, String>>,
    turn_state: Option<&Arc<Mutex<Option<String>>>>,
) -> Result<Option<HashMap<String, String>>> {
    let Some(turn_state) = turn_state else {
        return Ok(extra_headers);
    };
    let state = turn_state
        .lock()
        .map_err(|_| anyhow!("turn state lock poisoned"))?
        .clone();
    let Some(state) = state.filter(|state| !state.is_empty()) else {
        return Ok(extra_headers);
    };
    extra_headers
        .get_or_insert_with(HashMap::new)
        .entry(X_CODEX_TURN_STATE_HEADER.to_string())
        .or_insert(state);
    Ok(extra_headers)
}

fn record_turn_state_from_headers(
    headers: &reqwest::header::HeaderMap,
    turn_state: Option<&Arc<Mutex<Option<String>>>>,
) -> Result<()> {
    let Some(turn_state) = turn_state else {
        return Ok(());
    };
    let Some(header_value) = headers
        .get(X_CODEX_TURN_STATE_HEADER)
        .and_then(|value| value.to_str().ok())
        .filter(|value| !value.is_empty())
    else {
        return Ok(());
    };
    let mut guard = turn_state
        .lock()
        .map_err(|_| anyhow!("turn state lock poisoned"))?;
    if guard.is_none() {
        *guard = Some(header_value.to_string());
    }
    Ok(())
}

fn apply_model_request_settings(
    body: &mut Value,
    settings: &ModelRequestSettings,
    model_info: &ModelRequestInfo,
    output_schema: Option<&Value>,
    output_schema_strict: bool,
) {
    apply_reasoning_request(body, settings, model_info);
    apply_service_tier_request(body, settings, model_info);
    apply_text_format(body, output_schema, output_schema_strict);
    if model_info.support_verbosity {
        apply_text_verbosity(
            body,
            settings
                .text_verbosity
                .as_deref()
                .or(model_info.default_verbosity.as_deref()),
        );
    }
}

fn apply_reasoning_request(
    body: &mut Value,
    settings: &ModelRequestSettings,
    model_info: &ModelRequestInfo,
) {
    let supports_reasoning_summaries = if settings.model_supports_reasoning_summaries == Some(true)
    {
        true
    } else {
        model_info.supports_reasoning_summaries
    };
    if !supports_reasoning_summaries {
        return;
    }

    let effort = settings
        .reasoning_effort
        .as_deref()
        .or(model_info.default_reasoning_effort.as_deref());
    let summary = settings
        .reasoning_summary
        .as_deref()
        .unwrap_or(model_info.default_reasoning_summary.as_str());
    let mut reasoning = serde_json::Map::new();
    if let Some(effort) = effort {
        reasoning.insert("effort".to_string(), Value::String(effort.to_string()));
    }
    if summary != "none" {
        reasoning.insert("summary".to_string(), Value::String(summary.to_string()));
    }
    if !reasoning.is_empty() {
        body["reasoning"] = Value::Object(reasoning);
        body["include"] = json!(["reasoning.encrypted_content"]);
    }
}

fn apply_service_tier_request(
    body: &mut Value,
    settings: &ModelRequestSettings,
    model_info: &ModelRequestInfo,
) {
    if let Some(service_tier) = service_tier_for_model(settings.service_tier.as_deref(), model_info)
    {
        body["service_tier"] = Value::String(service_tier);
    }
}

fn reasoning_effort_for_model_switch(
    current_effort: Option<&str>,
    model_info: &ModelRequestInfo,
) -> Option<String> {
    if let Some(current_effort) = current_effort {
        if model_info
            .supported_reasoning_efforts
            .iter()
            .any(|effort| effort == current_effort)
        {
            return Some(current_effort.to_string());
        }
    }
    model_info
        .supported_reasoning_efforts
        .get(
            model_info
                .supported_reasoning_efforts
                .len()
                .saturating_sub(1)
                / 2,
        )
        .map(String::as_str)
        .or(model_info.default_reasoning_effort.as_deref())
        .map(ToString::to_string)
}

fn service_tier_for_model(
    service_tier: Option<&str>,
    model_info: &ModelRequestInfo,
) -> Option<String> {
    service_tier
        .filter(|service_tier| *service_tier != "default")
        .filter(|service_tier| {
            model_info
                .supported_service_tiers
                .iter()
                .any(|supported| supported == service_tier)
        })
        .map(ToString::to_string)
}

fn apply_text_verbosity(body: &mut Value, verbosity: Option<&str>) {
    let Some(verbosity) = verbosity else {
        return;
    };
    if !body.get("text").is_some_and(Value::is_object) {
        body["text"] = json!({});
    }
    body["text"]["verbosity"] = Value::String(verbosity.to_string());
}

fn apply_text_format(body: &mut Value, output_schema: Option<&Value>, strict: bool) {
    let Some(schema) = output_schema else {
        return;
    };
    if !body.get("text").is_some_and(Value::is_object) {
        body["text"] = json!({});
    }
    body["text"]["format"] = json!({
        "type": "json_schema",
        "strict": strict,
        "schema": schema,
        "name": "codex_output_schema",
    });
}

fn model_request_info(model: &str) -> ModelRequestInfo {
    model_request_info_for_catalog(model, None)
}

fn model_request_info_for_turn(model: &str, turn: &ProviderTurn) -> ModelRequestInfo {
    turn.model_request_info
        .clone()
        .unwrap_or_else(|| model_request_info(model))
}

pub fn model_request_info_for_catalog(
    model: &str,
    catalog: Option<&ModelCatalog>,
) -> ModelRequestInfo {
    if let Some(catalog) = catalog {
        return catalog.request_info_for_model(model);
    }
    codex_bundled_model_catalog().request_info_for_model(model)
}

fn catalog_entry_by_namespaced_suffix<'a>(
    model: &str,
    entries: &'a [ModelCatalogEntryInfo],
) -> Option<&'a ModelCatalogEntryInfo> {
    let (namespace, suffix) = model.split_once('/')?;
    if suffix.contains('/') {
        return None;
    }
    if namespace.is_empty()
        || !namespace
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        return None;
    }
    catalog_entry_by_longest_prefix(suffix, entries)
}

fn catalog_entry_by_longest_prefix<'a>(
    model: &str,
    entries: &'a [ModelCatalogEntryInfo],
) -> Option<&'a ModelCatalogEntryInfo> {
    entries
        .iter()
        .filter(|entry| model.starts_with(&entry.slug))
        .max_by_key(|entry| entry.slug.len())
}

pub fn load_codex_auth() -> Result<CodexAuth> {
    if let Ok(access_token) = std::env::var("LLM_BROWSER_CODEX_ACCESS_TOKEN") {
        let account_id = std::env::var("LLM_BROWSER_CODEX_ACCOUNT_ID")
            .context("set LLM_BROWSER_CODEX_ACCOUNT_ID with LLM_BROWSER_CODEX_ACCESS_TOKEN")?;
        return Ok(CodexAuth::new(access_token, account_id));
    }
    let path = codex_auth_path().context("could not resolve Codex auth path")?;
    load_codex_auth_file(path)
}

fn codex_auth_path() -> Option<PathBuf> {
    if let Ok(path) = std::env::var("LLM_BROWSER_CODEX_AUTH_FILE") {
        return Some(PathBuf::from(path));
    }
    if let Ok(home) = std::env::var("CODEX_HOME") {
        return Some(PathBuf::from(home).join("auth.json"));
    }
    std::env::var("HOME")
        .ok()
        .map(|home| PathBuf::from(home).join(".codex").join("auth.json"))
}

pub fn load_codex_auth_file(path: impl AsRef<Path>) -> Result<CodexAuth> {
    Ok(load_codex_managed_auth_file(path)?.current_auth()?)
}

pub fn load_codex_managed_auth() -> Result<CodexManagedAuth> {
    let path = codex_auth_path().context("could not resolve Codex auth path")?;
    load_codex_managed_auth_file(path)
}

pub fn load_codex_managed_auth_file(path: impl AsRef<Path>) -> Result<CodexManagedAuth> {
    Ok(CodexManagedAuth::new(
        load_codex_managed_auth_snapshot_from_file(path.as_ref())?,
    ))
}

fn load_codex_managed_auth_snapshot_from_file(path: &Path) -> Result<CodexManagedAuthSnapshot> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("read Codex auth file {}", path.display()))?;
    let file: CodexAuthFile =
        serde_json::from_str(&raw).with_context(|| format!("parse {}", path.display()))?;
    let access_token = file
        .tokens
        .as_ref()
        .and_then(|tokens| tokens.access_token.clone())
        .or(file.access_token)
        .context("Codex auth missing access token")?;
    let account_id = file
        .tokens
        .as_ref()
        .and_then(|tokens| tokens.account_id.clone())
        .or(file.account_id)
        .or(file.chatgpt_account_id)
        .or_else(|| {
            file.tokens
                .as_ref()
                .and_then(|tokens| tokens.id_token.as_deref())
                .and_then(account_id_from_id_token)
        })
        .context("Codex auth missing account id")?;
    Ok(CodexManagedAuthSnapshot {
        access_token,
        account_id,
        id_token: file
            .tokens
            .as_ref()
            .and_then(|tokens| tokens.id_token.clone()),
        refresh_token: file
            .tokens
            .as_ref()
            .and_then(|tokens| tokens.refresh_token.clone()),
        source_path: Some(path.to_path_buf()),
        last_refresh: file
            .last_refresh
            .as_deref()
            .and_then(|value| DateTime::parse_from_rfc3339(value.trim()).ok())
            .map(|value| value.with_timezone(&Utc)),
    })
}

fn persist_codex_managed_auth_snapshot(
    path: &Path,
    snapshot: &CodexManagedAuthSnapshot,
) -> Result<()> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("read Codex auth file {}", path.display()))?;
    let mut file: CodexAuthFile =
        serde_json::from_str(&raw).with_context(|| format!("parse {}", path.display()))?;
    let tokens = file.tokens.get_or_insert_with(CodexAuthTokens::default);
    if let Some(id_token) = snapshot.id_token.as_ref() {
        tokens.id_token = Some(id_token.clone());
    }
    tokens.access_token = Some(snapshot.access_token.clone());
    if let Some(refresh_token) = snapshot.refresh_token.as_ref() {
        tokens.refresh_token = Some(refresh_token.clone());
    }
    if !snapshot.account_id.trim().is_empty() {
        tokens.account_id = Some(snapshot.account_id.clone());
    }
    file.last_refresh = snapshot.last_refresh.map(|value| value.to_rfc3339());
    let json = serde_json::to_string_pretty(&file)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut options = OpenOptions::new();
    options.truncate(true).write(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(json.as_bytes())?;
    file.flush()?;
    Ok(())
}

fn codex_managed_auth_is_stale(snapshot: &CodexManagedAuthSnapshot) -> bool {
    if !snapshot.has_refresh_token() {
        return false;
    }
    if jwt_expiration_timestamp(&snapshot.access_token)
        .is_some_and(|expires_at| expires_at <= Utc::now().timestamp())
    {
        return true;
    }
    snapshot
        .last_refresh
        .is_some_and(|last_refresh| last_refresh < Utc::now() - chrono::Duration::days(8))
}

fn request_codex_token_refresh(
    client: &reqwest::blocking::Client,
    refresh_token: String,
) -> Result<CodexRefreshResponse> {
    let endpoint = std::env::var(CODEX_REFRESH_TOKEN_URL_OVERRIDE_ENV_VAR)
        .unwrap_or_else(|_| CODEX_REFRESH_TOKEN_URL.to_string());
    let response = client
        .post(endpoint)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(form_body(&[
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token.as_str()),
            ("client_id", CODEX_OAUTH_CLIENT_ID),
        ]))
        .send()
        .context("send Codex OAuth refresh request")?;
    let status = response.status();
    let body = response.text().unwrap_or_default();
    if status.is_success() {
        return serde_json::from_str(&body).context("parse Codex OAuth refresh response");
    }
    if status == reqwest::StatusCode::UNAUTHORIZED {
        bail!(
            "Codex OAuth refresh token rejected: {}",
            codex_refresh_failure_message(&body)
        );
    }
    bail!("Codex OAuth refresh failed ({status}): {body}");
}

fn codex_refresh_failure_message(body: &str) -> String {
    let code = extract_codex_refresh_error_code(body)
        .unwrap_or_else(|| "unknown_refresh_error".to_string())
        .to_ascii_lowercase();
    match code.as_str() {
        "refresh_token_expired" => "refresh token expired".to_string(),
        "refresh_token_reused" => "refresh token was already used".to_string(),
        "refresh_token_invalidated" => "refresh token was revoked".to_string(),
        _ => format!("unrecognized refresh error `{code}`"),
    }
}

fn extract_codex_refresh_error_code(body: &str) -> Option<String> {
    let Value::Object(map) = serde_json::from_str::<Value>(body).ok()? else {
        return None;
    };
    if let Some(error) = map.get("error") {
        match error {
            Value::Object(object) => {
                if let Some(code) = object.get("code").and_then(Value::as_str) {
                    return Some(code.to_string());
                }
            }
            Value::String(code) => return Some(code.to_string()),
            _ => {}
        }
    }
    map.get("code")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
}

#[derive(Debug, Deserialize)]
struct CodexRefreshResponse {
    id_token: Option<String>,
    access_token: Option<String>,
    refresh_token: Option<String>,
}

#[derive(Debug, Deserialize)]
struct CodexOAuthTokenResponse {
    id_token: Option<String>,
    access_token: Option<String>,
    refresh_token: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
struct CodexAuthFile {
    tokens: Option<CodexAuthTokens>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    auth_mode: Option<String>,
    #[serde(
        rename = "OPENAI_API_KEY",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    openai_api_key: Option<String>,
    access_token: Option<String>,
    account_id: Option<String>,
    chatgpt_account_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_refresh: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    agent_identity: Option<String>,
}

#[derive(Default, Debug, Deserialize, Serialize)]
struct CodexAuthTokens {
    access_token: Option<String>,
    account_id: Option<String>,
    id_token: Option<String>,
    refresh_token: Option<String>,
}

fn account_id_from_id_token(id_token: &str) -> Option<String> {
    let payload = id_token.split('.').nth(1)?;
    let decoded = general_purpose::URL_SAFE_NO_PAD.decode(payload).ok()?;
    let value: Value = serde_json::from_slice(&decoded).ok()?;
    value
        .get("chatgpt_account_id")
        .or_else(|| {
            value
                .get("https://api.openai.com/auth")
                .and_then(|auth| auth.get("chatgpt_account_id"))
        })
        .or_else(|| value.get("account_id"))
        .or_else(|| {
            value
                .get("organizations")
                .and_then(Value::as_array)
                .and_then(|organizations| organizations.first())
                .and_then(|organization| organization.get("id"))
        })
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
}

fn jwt_expiration_timestamp(jwt: &str) -> Option<i64> {
    let payload = jwt.split('.').nth(1)?;
    let decoded = general_purpose::URL_SAFE_NO_PAD.decode(payload).ok()?;
    let value: Value = serde_json::from_slice(&decoded).ok()?;
    value.get("exp").and_then(Value::as_i64)
}

fn parse_responses_sse(
    response: reqwest::blocking::Response,
    model: &str,
    stream_idle_timeout: Duration,
) -> Result<Vec<ModelEvent>> {
    let mut events = Vec::new();
    parse_responses_sse_stream(response, model, stream_idle_timeout, &mut |event| {
        events.push(event);
        Ok(())
    })?;
    Ok(events)
}

fn parse_responses_sse_stream(
    response: reqwest::blocking::Response,
    model: &str,
    stream_idle_timeout: Duration,
    on_event: &mut dyn FnMut(ModelEvent) -> Result<()>,
) -> Result<()> {
    let mut stream_state = CodexSseStreamState::default();
    emit_responses_header_events(response.headers(), &mut stream_state, on_event)?;
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(|value| value.to_ascii_lowercase())
        .unwrap_or_default();
    let is_json = content_type.contains("application/json") || content_type.contains("+json");
    if is_json {
        let body_text = response.text().context("read Responses JSON body")?;
        let body: Value = serde_json::from_str(&body_text).map_err(|error| {
            ProviderError::retryable(format!("parse Responses JSON: {error}"), None)
        })?;
        for event in parse_responses_output(&body, model)? {
            on_event(event)?;
        }
        return Ok(());
    }

    let mut data_lines = Vec::new();
    let mut emitted_done = false;
    let (line_tx, line_rx) = std::sync::mpsc::channel();
    thread::spawn(move || {
        for line in BufReader::new(response).lines() {
            if line_tx.send(line).is_err() {
                return;
            }
        }
    });
    loop {
        let line = match line_rx.recv_timeout(stream_idle_timeout) {
            Ok(line) => line,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                return Err(ProviderError::stream("idle timeout waiting for SSE").into());
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        };
        let line = match line {
            Ok(line) => line,
            Err(error) if is_sse_idle_timeout_error(&error) => {
                return Err(ProviderError::stream("idle timeout waiting for SSE").into());
            }
            Err(error) => return Err(error).context("read Codex SSE line"),
        };
        if line.is_empty() {
            flush_sse_event(
                &mut data_lines,
                &mut stream_state,
                model,
                on_event,
                &mut emitted_done,
            )?;
        } else if let Some(data) = line.strip_prefix("data:") {
            data_lines.push(data.trim().to_string());
        }
    }
    flush_sse_event(
        &mut data_lines,
        &mut stream_state,
        model,
        on_event,
        &mut emitted_done,
    )?;
    if !emitted_done {
        return Err(ProviderError::stream("stream closed before response.completed").into());
    }
    Ok(())
}

fn stream_idle_timeout_for_turn(turn: &ProviderTurn) -> Duration {
    Duration::from_millis(
        turn.stream_idle_timeout_ms
            .unwrap_or(DEFAULT_STREAM_IDLE_TIMEOUT_MS),
    )
}

fn is_sse_idle_timeout_error(error: &std::io::Error) -> bool {
    if error.kind() == std::io::ErrorKind::TimedOut {
        return true;
    }
    let message = error.to_string().to_ascii_lowercase();
    message.contains("timed out") || message.contains("operation timed out")
}

fn response_is_json(response: &reqwest::blocking::Response) -> bool {
    response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|content_type| content_type.contains("application/json"))
}

fn read_sse_data_stream(
    response: reqwest::blocking::Response,
    stream_idle_timeout: Duration,
    read_error_context: &'static str,
    on_data: &mut dyn FnMut(&str) -> Result<()>,
) -> Result<()> {
    let mut data_lines = Vec::new();
    let (line_tx, line_rx) = std::sync::mpsc::channel();
    thread::spawn(move || {
        for line in BufReader::new(response).lines() {
            if line_tx.send(line).is_err() {
                return;
            }
        }
    });
    loop {
        let line = match line_rx.recv_timeout(stream_idle_timeout) {
            Ok(line) => line,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                return Err(ProviderError::stream("idle timeout waiting for SSE").into());
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        };
        let line = match line {
            Ok(line) => line,
            Err(error) if is_sse_idle_timeout_error(&error) => {
                return Err(ProviderError::stream("idle timeout waiting for SSE").into());
            }
            Err(error) => return Err(error).context(read_error_context),
        };
        if line.is_empty() {
            flush_generic_sse_data(&mut data_lines, on_data)?;
        } else if let Some(data) = line.strip_prefix("data:") {
            data_lines.push(data.trim().to_string());
        } else if line.starts_with("event:") || line.starts_with(':') {
            continue;
        } else {
            data_lines.push(line.trim().to_string());
        }
    }
    flush_generic_sse_data(&mut data_lines, on_data)
}

fn flush_generic_sse_data(
    data_lines: &mut Vec<String>,
    on_data: &mut dyn FnMut(&str) -> Result<()>,
) -> Result<()> {
    if data_lines.is_empty() {
        return Ok(());
    }
    let data = data_lines.join("\n");
    data_lines.clear();
    if data.trim().is_empty() {
        return Ok(());
    }
    on_data(data.trim())
}

fn emit_responses_header_events(
    headers: &reqwest::header::HeaderMap,
    stream_state: &mut CodexSseStreamState,
    on_event: &mut dyn FnMut(ModelEvent) -> Result<()>,
) -> Result<()> {
    if let Some(model) = parse_header_str(headers, "openai-model") {
        emit_server_model(model, stream_state, on_event)?;
    }
    for snapshot in parse_all_rate_limits(headers) {
        on_event(ModelEvent::ModelRateLimits { snapshot })?;
    }
    if let Some(etag) = parse_header_str(headers, "x-models-etag") {
        on_event(ModelEvent::ModelsEtag { etag })?;
    }
    if headers.get("x-reasoning-included").is_some() {
        on_event(ModelEvent::ServerReasoningIncluded { included: true })?;
    }
    Ok(())
}

fn emit_server_model(
    model: String,
    stream_state: &mut CodexSseStreamState,
    on_event: &mut dyn FnMut(ModelEvent) -> Result<()>,
) -> Result<()> {
    if stream_state.last_server_model.as_deref() != Some(model.as_str()) {
        stream_state.last_server_model = Some(model.clone());
        on_event(ModelEvent::ServerModel { model })?;
    }
    Ok(())
}

fn response_model_from_event(event: &Value) -> Option<String> {
    event
        .get("response")
        .and_then(|response| response.get("headers"))
        .and_then(header_openai_model_value_from_json)
        .or_else(|| {
            event
                .get("headers")
                .and_then(header_openai_model_value_from_json)
        })
}

fn header_openai_model_value_from_json(value: &Value) -> Option<String> {
    let headers = value.as_object()?;
    headers.iter().find_map(|(name, value)| {
        if name.eq_ignore_ascii_case("openai-model") || name.eq_ignore_ascii_case("x-openai-model")
        {
            json_value_as_string(value)
        } else {
            None
        }
    })
}

fn json_value_as_string(value: &Value) -> Option<String> {
    match value {
        Value::String(value) => Some(value.clone()),
        Value::Array(values) => values.first().and_then(json_value_as_string),
        _ => None,
    }
}

fn model_verifications_from_event(event: &Value) -> Option<Vec<ModelVerification>> {
    if event.get("type").and_then(Value::as_str) != Some("response.metadata") {
        return None;
    }
    let recommendations = event
        .get("metadata")
        .and_then(|metadata| metadata.get("openai_verification_recommendation"))?;
    let verifications = recommendations
        .as_array()
        .map(|items| {
            let mut verifications = Vec::new();
            for verification in items
                .iter()
                .filter_map(Value::as_str)
                .filter_map(parse_model_verification)
            {
                if !verifications.contains(&verification) {
                    verifications.push(verification);
                }
            }
            verifications
        })
        .unwrap_or_default();
    (!verifications.is_empty()).then_some(verifications)
}

fn parse_model_verification(value: &str) -> Option<ModelVerification> {
    match value {
        "trusted_access_for_cyber" => Some(ModelVerification::TrustedAccessForCyber),
        _ => None,
    }
}

fn flush_sse_event(
    data_lines: &mut Vec<String>,
    stream_state: &mut CodexSseStreamState,
    model: &str,
    on_event: &mut dyn FnMut(ModelEvent) -> Result<()>,
    emitted_done: &mut bool,
) -> Result<()> {
    if data_lines.is_empty() {
        return Ok(());
    }
    let data = data_lines.join("\n");
    data_lines.clear();
    if data.trim().is_empty() || data.trim() == "[DONE]" {
        return Ok(());
    }
    let event: Value = match serde_json::from_str(&data) {
        Ok(event) => event,
        Err(_) => return Ok(()),
    };
    if let Some(model) = response_model_from_event(&event) {
        emit_server_model(model, stream_state, on_event)?;
    }
    if let Some(verifications) = model_verifications_from_event(&event) {
        emit_codex_model_event(
            ModelEvent::ModelVerifications { verifications },
            on_event,
            emitted_done,
        )?;
    }
    match event.get("type").and_then(Value::as_str) {
        Some("response.output_text.delta") => {
            if let Some(delta) = event.get("delta").and_then(Value::as_str) {
                emit_codex_model_event(
                    ModelEvent::TextDelta {
                        text: delta.to_string(),
                    },
                    on_event,
                    emitted_done,
                )?;
            }
        }
        Some("response.reasoning_summary_text.delta") => {
            if let Some(delta) = event.get("delta").and_then(Value::as_str) {
                emit_codex_model_event(
                    ModelEvent::ThinkingDelta {
                        text: delta.to_string(),
                        label: Some("reasoning summary".to_string()),
                    },
                    on_event,
                    emitted_done,
                )?;
            }
        }
        Some("response.output_item.done") => {
            if let Some(item) = event.get("item") {
                let mut item_events = Vec::new();
                maybe_push_codex_output_item(item, stream_state, &mut item_events)?;
                for item_event in item_events {
                    emit_codex_model_event(item_event, on_event, emitted_done)?;
                }
            }
        }
        Some("response.output_item.added") => {
            if let Some(item) = event.get("item") {
                stream_state.remember_custom_tool_call_item(item);
            }
        }
        Some("response.custom_tool_call_input.delta") => {
            let item_id = event
                .get("item_id")
                .or_else(|| event.get("output_index").and_then(|_| event.get("item_id")))
                .and_then(Value::as_str);
            if let Some(item_id) = item_id {
                if let Some((call_id, name)) = stream_state.custom_tool_call_for_item(item_id) {
                    if let Some(delta) = event.get("delta").and_then(Value::as_str) {
                        emit_codex_model_event(
                            ModelEvent::CustomToolCallInputDelta {
                                call_id: call_id.to_string(),
                                name: name.to_string(),
                                delta: delta.to_string(),
                            },
                            on_event,
                            emitted_done,
                        )?;
                    }
                }
            }
        }
        Some("response.created")
        | Some("response.reasoning_summary_part.added")
        | Some("response.metadata") => {}
        Some("response.reasoning_text.delta") => {
            if let Some(delta) = event.get("delta").and_then(Value::as_str) {
                emit_codex_model_event(
                    ModelEvent::ThinkingDelta {
                        text: delta.to_string(),
                        label: Some("reasoning".to_string()),
                    },
                    on_event,
                    emitted_done,
                )?;
            }
        }
        Some("response.failed") => return Err(response_failed_error(&event).into()),
        Some("response.incomplete") => {
            let reason = event
                .get("response")
                .and_then(|response| response.get("incomplete_details"))
                .and_then(|details| details.get("reason"))
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            bail!("Incomplete response returned, reason: {reason}");
        }
        Some("response.completed") => {
            if let Some(response) = event.get("response") {
                if let Some(items) = response.get("output").and_then(Value::as_array) {
                    for item in items {
                        let mut item_events = Vec::new();
                        maybe_push_codex_output_item(item, stream_state, &mut item_events)?;
                        for item_event in item_events {
                            emit_codex_model_event(item_event, on_event, emitted_done)?;
                        }
                    }
                }
                emit_codex_model_event(
                    response_completed_event(response)?,
                    on_event,
                    emitted_done,
                )?;
                if let Some(usage) = parse_usage(response.get("usage"), model) {
                    emit_codex_model_event(ModelEvent::Usage { usage }, on_event, emitted_done)?;
                }
            } else {
                bail!("response.completed missing response");
            }
            emit_codex_model_event(ModelEvent::Done, on_event, emitted_done)?;
        }
        Some("error") => bail!("Codex stream error: {event}"),
        _ => {}
    }
    Ok(())
}

fn emit_codex_model_event(
    event: ModelEvent,
    on_event: &mut dyn FnMut(ModelEvent) -> Result<()>,
    emitted_done: &mut bool,
) -> Result<()> {
    if matches!(event, ModelEvent::Done) {
        *emitted_done = true;
    }
    on_event(event)
}

fn response_failed_error_message(event: &Value) -> String {
    let error = event
        .get("response")
        .and_then(|response| response.get("error"))
        .or_else(|| event.get("error"));
    let message = error
        .and_then(|error| error.get("message"))
        .and_then(Value::as_str)
        .filter(|message| !message.trim().is_empty());
    let code = error
        .and_then(|error| error.get("code"))
        .and_then(Value::as_str)
        .filter(|code| !code.trim().is_empty());

    match (code, message) {
        (Some(code), Some(message)) => format!("response.failed ({code}): {message}"),
        (None, Some(message)) => format!("response.failed: {message}"),
        (Some(code), None) => format!("response.failed ({code})"),
        (None, None) => "response.failed event received".to_string(),
    }
}

fn response_failed_error(event: &Value) -> ProviderError {
    let message = response_failed_error_message(event);
    let error = event
        .get("response")
        .and_then(|response| response.get("error"))
        .or_else(|| event.get("error"));
    let code = error
        .and_then(|error| error.get("code"))
        .and_then(Value::as_str)
        .filter(|code| !code.trim().is_empty());
    match code {
        Some("context_length_exceeded") => {
            ProviderError::non_retryable(ProviderErrorKind::ContextWindowExceeded, message)
        }
        Some("insufficient_quota") => {
            ProviderError::non_retryable(ProviderErrorKind::QuotaExceeded, message)
        }
        Some("usage_not_included") => {
            ProviderError::non_retryable(ProviderErrorKind::UsageNotIncluded, message)
        }
        Some("invalid_prompt") => {
            ProviderError::non_retryable(ProviderErrorKind::InvalidRequest, message)
        }
        Some("invalid_image") => {
            ProviderError::non_retryable(ProviderErrorKind::InvalidImage, message)
        }
        Some("cyber_policy") => {
            ProviderError::non_retryable(ProviderErrorKind::CyberPolicy, message)
        }
        Some("server_is_overloaded") | Some("slow_down") => {
            ProviderError::non_retryable(ProviderErrorKind::ServerOverloaded, message)
        }
        Some("rate_limit_exceeded") => {
            let retry_delay = error.and_then(try_parse_retry_after);
            ProviderError::retryable(message, retry_delay)
        }
        Some(_) => ProviderError::retryable(message, None),
        None => ProviderError::stream(message),
    }
}

fn invalid_image_error_message(parsed_message: Option<&str>, body: &str) -> bool {
    parsed_message
        .unwrap_or(body)
        .contains("The image data you provided does not represent a valid image")
}

fn response_completed_event(response: &Value) -> Result<ModelEvent> {
    let response_id = response
        .get("id")
        .and_then(Value::as_str)
        .filter(|id| !id.trim().is_empty())
        .ok_or_else(|| anyhow!("failed to parse ResponseCompleted: missing field `id`"))?;
    Ok(ModelEvent::ResponseCompleted {
        response_id: Some(response_id.to_string()),
        end_turn: response.get("end_turn").and_then(Value::as_bool),
    })
}

fn try_parse_retry_after(error: &Value) -> Option<Duration> {
    if error.get("code").and_then(Value::as_str) != Some("rate_limit_exceeded") {
        return None;
    }
    let message = error.get("message").and_then(Value::as_str)?;
    let lower = message.to_ascii_lowercase();
    let marker = "try again in ";
    let start = lower.find(marker)? + marker.len();
    let remainder = &message[start..];
    let mut chars = remainder.char_indices().peekable();
    let mut end = 0;
    let mut saw_digit = false;
    while let Some((idx, ch)) = chars.peek().copied() {
        if ch.is_ascii_digit() || ch == '.' {
            saw_digit |= ch.is_ascii_digit();
            end = idx + ch.len_utf8();
            chars.next();
        } else {
            break;
        }
    }
    if !saw_digit {
        return None;
    }
    let value = remainder[..end].parse::<f64>().ok()?;
    let unit_start = end;
    let unit_remainder = remainder[unit_start..].trim_start();
    let unit: String = unit_remainder
        .chars()
        .take_while(|ch| ch.is_ascii_alphabetic())
        .collect::<String>()
        .to_ascii_lowercase();
    if unit == "ms" {
        Some(Duration::from_millis(value as u64))
    } else if unit == "s" || unit.starts_with("second") {
        Some(Duration::from_secs_f64(value))
    } else {
        None
    }
}

#[derive(Default)]
struct CodexSseStreamState {
    seen_tool_calls: HashSet<String>,
    seen_output_items: HashSet<String>,
    last_server_model: Option<String>,
    custom_tool_calls_by_item_id: HashMap<String, (String, String)>,
}

impl CodexSseStreamState {
    fn remember_custom_tool_call_item(&mut self, item: &Value) {
        if item.get("type").and_then(Value::as_str) != Some("custom_tool_call") {
            return;
        }
        let Some(item_id) = item.get("id").and_then(Value::as_str) else {
            return;
        };
        let call_id = item
            .get("call_id")
            .or_else(|| item.get("id"))
            .and_then(Value::as_str)
            .unwrap_or(item_id)
            .to_string();
        let name = item
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("custom_tool")
            .to_string();
        self.custom_tool_calls_by_item_id
            .insert(item_id.to_string(), (call_id, name));
    }

    fn custom_tool_call_for_item(&self, item_id: &str) -> Option<(&str, &str)> {
        self.custom_tool_calls_by_item_id
            .get(item_id)
            .map(|(call_id, name)| (call_id.as_str(), name.as_str()))
    }
}

fn maybe_push_codex_output_item(
    item: &Value,
    stream_state: &mut CodexSseStreamState,
    events: &mut Vec<ModelEvent>,
) -> Result<()> {
    let output_item_key = response_output_item_key(item);
    if stream_state.seen_output_items.insert(output_item_key) {
        events.push(ModelEvent::ResponseOutputItem { item: item.clone() });
    }
    if matches!(
        item.get("type").and_then(Value::as_str),
        Some("function_call") | Some("custom_tool_call") | Some("tool_search_call")
    ) {
        let call_id = item
            .get("call_id")
            .or_else(|| item.get("id"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        if stream_state.seen_tool_calls.insert(call_id) {
            parse_response_output_item(item, events)?;
        }
    } else {
        parse_response_output_item(item, events)?;
    }
    Ok(())
}

fn response_output_item_key(item: &Value) -> String {
    item.get("id")
        .or_else(|| item.get("call_id"))
        .and_then(Value::as_str)
        .map(|id| format!("id:{id}"))
        .unwrap_or_else(|| {
            serde_json::to_string(item)
                .map(|serialized| format!("value:{serialized}"))
                .unwrap_or_else(|_| format!("ptr:{:p}", item))
        })
}

pub fn default_agent_instructions() -> String {
    default_agent_instructions_for_model_and_personality("gpt-5.4", ModelPersonality::Pragmatic)
}

pub fn default_agent_instructions_for_personality(personality: ModelPersonality) -> String {
    default_agent_instructions_for_model_and_personality("gpt-5.4", personality)
}

pub fn default_agent_instructions_for_model_and_personality(
    model: &str,
    personality: ModelPersonality,
) -> String {
    default_agent_instructions_for_model_and_personality_with_catalog(model, personality, None)
}

pub fn default_agent_instructions_for_model_and_personality_with_catalog(
    model: &str,
    personality: ModelPersonality,
    catalog: Option<&ModelCatalog>,
) -> String {
    browser_agent_instructions_for_model_and_personality_with_catalog(model, personality, catalog)
}

pub fn default_terminal_agent_instructions() -> String {
    default_terminal_agent_instructions_for_model_and_personality(
        "gpt-5.4",
        ModelPersonality::Pragmatic,
    )
}

pub fn default_terminal_agent_instructions_for_personality(
    personality: ModelPersonality,
) -> String {
    default_terminal_agent_instructions_for_model_and_personality("gpt-5.4", personality)
}

pub fn default_terminal_agent_instructions_for_model_and_personality(
    model: &str,
    personality: ModelPersonality,
) -> String {
    default_terminal_agent_instructions_for_model_and_personality_with_catalog(
        model,
        personality,
        None,
    )
}

pub fn default_terminal_agent_instructions_for_model_and_personality_with_catalog(
    model: &str,
    personality: ModelPersonality,
    catalog: Option<&ModelCatalog>,
) -> String {
    append_terminal_agent_tooling_amendment(
        codex_model_instructions_for_model_and_personality_with_catalog(
            model,
            Some(personality),
            catalog,
        ),
    )
}

pub fn browser_agent_instructions_for_model_and_personality_with_catalog(
    model: &str,
    personality: ModelPersonality,
    catalog: Option<&ModelCatalog>,
) -> String {
    let mut instructions =
        prepend_browser_agent_identity_preamble(append_terminal_agent_tooling_amendment(
            codex_model_instructions_for_model_and_personality_with_catalog(
                model,
                Some(personality),
                catalog,
            ),
        ));
    instructions.push_str("\n\n## Browser Agent Contract\n\n");
    instructions.push_str(include_str!("../../../prompts/browser-agent-system.md").trim());
    instructions.push_str("\n\n## Loaded Browser-Harness Interaction Skills");
    instructions.push_str(
        "\n\nThese are the same interaction-skill playbooks from browser-harness. Apply the relevant section when the page mechanic appears.",
    );
    for (path, content) in browser_harness_interaction_skills() {
        instructions.push_str("\n\n### ");
        instructions.push_str(path);
        instructions.push_str("\n\n");
        instructions.push_str(content.trim());
    }
    instructions
}

fn prepend_browser_agent_identity_preamble(instructions: String) -> String {
    format!("{BROWSER_AGENT_IDENTITY_PREAMBLE}\n\n{instructions}")
}

fn append_terminal_agent_tooling_amendment(mut instructions: String) -> String {
    if !instructions.contains("## Agent Tooling Reliability") {
        instructions.push_str("\n\n");
        instructions.push_str(TERMINAL_AGENT_TOOLING_AMENDMENT);
    }
    instructions
}

const BROWSER_AGENT_IDENTITY_PREAMBLE: &str = concat!(
    "You are Browser Use Terminal, a web agent that operates a real browser for the user. ",
    "You can navigate websites, inspect pages, click, type, scroll, upload and download files, ",
    "take screenshots, extract data, and verify results from live web pages. ",
    "You also have terminal and coding tools for supporting work, but when the user asks what you are ",
    "or what you can do, describe your web-browsing abilities first."
);

pub fn default_personality_message_for_personality(personality: ModelPersonality) -> &'static str {
    codex_personality_message(personality)
}

pub fn default_personality_message_for_model_and_personality(
    model: &str,
    personality: ModelPersonality,
) -> Option<String> {
    default_personality_message_for_model_and_personality_with_catalog(model, personality, None)
}

pub fn default_personality_message_for_model_and_personality_with_catalog(
    model: &str,
    personality: ModelPersonality,
    catalog: Option<&ModelCatalog>,
) -> Option<String> {
    if let Some(catalog) = catalog {
        if let Some(entry) = catalog.entry_for_model(model) {
            return entry
                .model_messages
                .as_ref()?
                .get_personality_message(Some(personality));
        }
        return fallback_model_messages_for_slug(model)?.get_personality_message(Some(personality));
    }
    if let Some(entry) = bundled_model_catalog_entry(model) {
        return entry
            .model_messages
            .as_ref()?
            .get_personality_message(Some(personality));
    }
    fallback_model_messages_for_slug(model)?.get_personality_message(Some(personality))
}

fn codex_model_instructions_for_model_and_personality_with_catalog(
    model: &str,
    personality: Option<ModelPersonality>,
    catalog: Option<&ModelCatalog>,
) -> String {
    if let Some(catalog) = catalog {
        if let Some(entry) = catalog.entry_for_model(model) {
            return entry.get_model_instructions(personality);
        }
        return fallback_model_instructions_for_model_and_personality(model, personality);
    }
    if let Some(entry) = bundled_model_catalog_entry(model) {
        return entry.get_model_instructions(personality);
    }
    fallback_model_instructions_for_model_and_personality(model, personality)
}

fn fallback_model_instructions_for_model_and_personality(
    model: &str,
    personality: Option<ModelPersonality>,
) -> String {
    fallback_model_messages_for_slug(model)
        .map(|messages| {
            messages.get_model_instructions(personality, CODEX_FALLBACK_BASE_MODEL_INSTRUCTIONS)
        })
        .unwrap_or_else(|| NEUTRAL_FALLBACK_BASE_MODEL_INSTRUCTIONS.to_string())
}

fn fallback_model_messages_for_slug(slug: &str) -> Option<LocalModelMessages> {
    match slug {
        "gpt-5.2-codex" | "exp-codex-personality" => Some(LOCAL_PERSONALITY_MODEL_MESSAGES),
        _ => None,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct LocalModelMessages {
    instructions_template: Option<&'static str>,
    instructions_variables: Option<LocalModelInstructionsVariables>,
}

impl LocalModelMessages {
    fn supports_personality(self) -> bool {
        self.has_personality_placeholder()
            && self
                .instructions_variables
                .is_some_and(LocalModelInstructionsVariables::is_complete)
    }

    fn has_personality_placeholder(self) -> bool {
        self.instructions_template
            .is_some_and(|template| template.contains(PERSONALITY_PLACEHOLDER))
    }

    fn get_personality_message(self, personality: Option<ModelPersonality>) -> Option<String> {
        self.instructions_variables
            .and_then(|variables| variables.get_personality_message(personality))
            .map(ToString::to_string)
    }

    fn get_model_instructions(
        self,
        personality: Option<ModelPersonality>,
        base_instructions: &str,
    ) -> String {
        if let Some(template) = self.instructions_template {
            let personality_message = self
                .get_personality_message(personality)
                .unwrap_or_default();
            template
                .replace(PERSONALITY_PLACEHOLDER, personality_message.as_str())
                .replace(BASE_INSTRUCTIONS_PLACEHOLDER, base_instructions)
        } else {
            base_instructions.to_string()
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct LocalModelInstructionsVariables {
    personality_default: Option<&'static str>,
    personality_friendly: Option<&'static str>,
    personality_pragmatic: Option<&'static str>,
}

impl LocalModelInstructionsVariables {
    fn is_complete(self) -> bool {
        self.personality_default.is_some()
            && self.personality_friendly.is_some()
            && self.personality_pragmatic.is_some()
    }

    fn get_personality_message(
        self,
        personality: Option<ModelPersonality>,
    ) -> Option<&'static str> {
        match personality {
            Some(ModelPersonality::None) => Some(""),
            Some(ModelPersonality::Friendly) => self.personality_friendly,
            Some(ModelPersonality::Pragmatic) => self.personality_pragmatic,
            None => self.personality_default,
        }
    }
}

const PERSONALITY_PLACEHOLDER: &str = "{{ personality }}";
const BASE_INSTRUCTIONS_PLACEHOLDER: &str = "{{ base_instructions }}";
const LOCAL_MODEL_MESSAGES_TEMPLATE: &str = concat!(
    "You are Codex, a coding agent based on GPT-5. You and the user share the same workspace and collaborate to achieve the user's goals.\n\n",
    "{{ personality }}\n\n",
    "{{ base_instructions }}"
);
const LOCAL_PERSONALITY_MODEL_MESSAGES: LocalModelMessages = LocalModelMessages {
    instructions_template: Some(LOCAL_MODEL_MESSAGES_TEMPLATE),
    instructions_variables: Some(LocalModelInstructionsVariables {
        personality_default: Some(""),
        personality_friendly: Some(CODEX_FRIENDLY_PERSONALITY_MESSAGE),
        personality_pragmatic: Some(CODEX_PRAGMATIC_PERSONALITY_MESSAGE),
    }),
};

fn codex_personality_message(personality: ModelPersonality) -> &'static str {
    match personality {
        ModelPersonality::None => "",
        ModelPersonality::Friendly => CODEX_FRIENDLY_PERSONALITY_MESSAGE,
        ModelPersonality::Pragmatic => CODEX_PRAGMATIC_PERSONALITY_MESSAGE,
    }
}

pub fn default_permissions_instructions() -> &'static str {
    concat!(
        "<permissions instructions>",
        "Filesystem sandboxing defines which files can be read or written. ",
        "`sandbox_mode` is `danger-full-access`: No filesystem sandboxing - all commands are permitted. ",
        "Network access is enabled.\n\n",
        "Approval policy is currently never. Do not provide the `sandbox_permissions` for any reason, commands will be rejected.\n",
        "</permissions instructions>"
    )
}

fn default_instructions() -> String {
    default_agent_instructions()
}

fn browser_harness_interaction_skills() -> &'static [(&'static str, &'static str)] {
    &[
        (
            "interaction-skills/connection.md",
            include_str!("../../../prompts/interaction-skills/connection.md"),
        ),
        (
            "interaction-skills/cookies.md",
            include_str!("../../../prompts/interaction-skills/cookies.md"),
        ),
        (
            "interaction-skills/cross-origin-iframes.md",
            include_str!("../../../prompts/interaction-skills/cross-origin-iframes.md"),
        ),
        (
            "interaction-skills/dialogs.md",
            include_str!("../../../prompts/interaction-skills/dialogs.md"),
        ),
        (
            "interaction-skills/downloads.md",
            include_str!("../../../prompts/interaction-skills/downloads.md"),
        ),
        (
            "interaction-skills/drag-and-drop.md",
            include_str!("../../../prompts/interaction-skills/drag-and-drop.md"),
        ),
        (
            "interaction-skills/dropdowns.md",
            include_str!("../../../prompts/interaction-skills/dropdowns.md"),
        ),
        (
            "interaction-skills/forms.md",
            include_str!("../../../prompts/interaction-skills/forms.md"),
        ),
        (
            "interaction-skills/iframes.md",
            include_str!("../../../prompts/interaction-skills/iframes.md"),
        ),
        (
            "interaction-skills/network-requests.md",
            include_str!("../../../prompts/interaction-skills/network-requests.md"),
        ),
        (
            "interaction-skills/print-as-pdf.md",
            include_str!("../../../prompts/interaction-skills/print-as-pdf.md"),
        ),
        (
            "interaction-skills/profile-sync.md",
            include_str!("../../../prompts/interaction-skills/profile-sync.md"),
        ),
        (
            "interaction-skills/screenshots.md",
            include_str!("../../../prompts/interaction-skills/screenshots.md"),
        ),
        (
            "interaction-skills/scrolling.md",
            include_str!("../../../prompts/interaction-skills/scrolling.md"),
        ),
        (
            "interaction-skills/shadow-dom.md",
            include_str!("../../../prompts/interaction-skills/shadow-dom.md"),
        ),
        (
            "interaction-skills/tabs.md",
            include_str!("../../../prompts/interaction-skills/tabs.md"),
        ),
        (
            "interaction-skills/uploads.md",
            include_str!("../../../prompts/interaction-skills/uploads.md"),
        ),
        (
            "interaction-skills/viewport.md",
            include_str!("../../../prompts/interaction-skills/viewport.md"),
        ),
    ]
}

#[cfg(test)]
fn tool_specs_to_responses_tools(tools: &[ToolSpec]) -> Vec<Value> {
    tool_specs_to_responses_tools_with_hosted(tools, &[])
}

fn tool_specs_to_responses_tools_with_hosted(
    tools: &[ToolSpec],
    hosted_tools: &[HostedToolSpec],
) -> Vec<Value> {
    let mut output = Vec::new();
    let mut namespace_indices = HashMap::<String, usize>::new();
    for tool in tools {
        if tool.name == "tool_search" && tool.namespace.is_none() && tool.freeform.is_none() {
            output.push(json!({
                "type": "tool_search",
                "execution": "client",
                "description": tool.description,
                "parameters": tool.input_schema,
            }));
            continue;
        }
        let function_tool = if let Some(format) = &tool.freeform {
            json!({
                "type": "custom",
                "name": tool.name,
                "description": tool.description,
                "format": {
                    "type": format.kind,
                    "syntax": format.syntax,
                    "definition": format.definition,
                },
            })
        } else {
            json!({
                "type": "function",
                "name": tool.name,
                "description": tool.description,
                "strict": false,
                "parameters": tool.input_schema,
            })
        };
        let Some(namespace) = tool.namespace.as_deref() else {
            output.push(function_tool);
            continue;
        };
        if let Some(index) = namespace_indices.get(namespace).copied() {
            if let Some(tools) = output[index].get_mut("tools").and_then(Value::as_array_mut) {
                tools.push(function_tool);
            }
            continue;
        }
        namespace_indices.insert(namespace.to_string(), output.len());
        output.push(json!({
            "type": "namespace",
            "name": namespace,
            "description": tool
                .namespace_description
                .clone()
                .unwrap_or_else(|| format!("Tools in the {namespace} namespace.")),
            "tools": [function_tool],
        }));
    }
    for tool in &mut output {
        if tool.get("type").and_then(Value::as_str) == Some("namespace") {
            if let Some(tools) = tool.get_mut("tools").and_then(Value::as_array_mut) {
                tools.sort_by(|left, right| {
                    left.get("name")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .cmp(
                            right
                                .get("name")
                                .and_then(Value::as_str)
                                .unwrap_or_default(),
                        )
                });
            }
        }
    }
    output.extend(hosted_tools.iter().map(hosted_tool_to_responses_tool));
    output
}

fn hosted_tool_to_responses_tool(tool: &HostedToolSpec) -> Value {
    match tool {
        HostedToolSpec::WebSearch {
            external_web_access,
            filters,
            user_location,
            search_context_size,
            search_content_types,
        } => {
            let mut value = json!({
                "type": "web_search",
                "external_web_access": external_web_access,
            });
            if let Some(filters) = filters {
                value["filters"] = serde_json::to_value(filters).unwrap_or(Value::Null);
            }
            if let Some(user_location) = user_location {
                value["user_location"] = serde_json::to_value(user_location).unwrap_or(Value::Null);
            }
            if let Some(search_context_size) = search_context_size {
                value["search_context_size"] =
                    serde_json::to_value(search_context_size).unwrap_or(Value::Null);
            }
            if let Some(search_content_types) = search_content_types {
                value["search_content_types"] = json!(search_content_types);
            }
            value
        }
        HostedToolSpec::ImageGeneration { output_format } => json!({
            "type": "image_generation",
            "output_format": output_format,
        }),
    }
}

fn tool_specs_to_chat_tools(tools: &[ToolSpec]) -> Vec<Value> {
    tools
        .iter()
        .map(|tool| {
            json!({
                "type": "function",
                "function": {
                    "name": tool.name,
                    "description": chat_tool_description(tool),
                    "parameters": tool.input_schema,
                }
            })
        })
        .collect()
}

fn chat_tool_description(tool: &ToolSpec) -> String {
    downgraded_freeform_tool_description(tool).unwrap_or_else(|| tool.description.clone())
}

fn tool_specs_to_anthropic_tools(tools: &[ToolSpec], is_oauth: bool) -> Vec<Value> {
    let mut anthropic_tools: Vec<Value> = tools
        .iter()
        .map(|tool| {
            json!({
                "name": anthropic_request_tool_name(&tool.name, is_oauth),
                "description": anthropic_tool_description(tool),
                "input_schema": tool.input_schema,
            })
        })
        .collect();
    if let Some(Value::Object(last_tool)) = anthropic_tools.last_mut() {
        last_tool.insert("cache_control".to_string(), json!({ "type": "ephemeral" }));
    }
    anthropic_tools
}

fn anthropic_tool_description(tool: &ToolSpec) -> String {
    downgraded_freeform_tool_description(tool).unwrap_or_else(|| tool.description.clone())
}

fn downgraded_freeform_tool_description(tool: &ToolSpec) -> Option<String> {
    let format = tool.freeform.as_ref()?;
    let Some(field_name) = freeform_tool_payload_field(tool) else {
        return Some(format!(
            "{}\n\nThis provider requires JSON tool arguments. Encode the freeform {} payload in the JSON arguments accepted by this tool.",
            tool.description, format.syntax
        ));
    };
    Some(format!(
        "{}\n\nThis provider requires JSON tool arguments. Put the raw {} payload in the `{field_name}` string property exactly as it should be passed to the tool.",
        tool.description, format.syntax
    ))
}

fn freeform_tool_payload_field(tool: &ToolSpec) -> Option<&str> {
    let required = tool.input_schema.get("required")?.as_array()?;
    if required.len() != 1 {
        return None;
    }
    let field = required.first()?.as_str()?;
    let field_schema = tool.input_schema.get("properties")?.get(field)?;
    (field_schema.get("type").and_then(Value::as_str) == Some("string")).then_some(field)
}

const IMAGE_CONTENT_OMITTED_PLACEHOLDER: &str =
    "image content omitted because you do not support image input";

#[cfg(test)]
fn messages_to_responses_input(messages: &[Value]) -> Result<Vec<Value>> {
    messages_to_responses_input_for_model(messages, &ModelRequestInfo::unknown())
}

fn messages_to_responses_input_for_model(
    messages: &[Value],
    model_info: &ModelRequestInfo,
) -> Result<Vec<Value>> {
    let mut input = Vec::new();
    let mut seen_tool_calls = HashMap::<String, ResponsesToolCallKind>::new();
    for message in messages {
        if let Some(response_item) = raw_response_item_for_responses_input(message) {
            if let Some(call_id) = response_item_call_id(&response_item) {
                seen_tool_calls
                    .insert(call_id.to_string(), response_tool_call_kind(&response_item));
            }
            input.push(response_item);
            continue;
        }

        let role = message
            .get("role")
            .and_then(Value::as_str)
            .unwrap_or("user");
        match role {
            "tool" => {
                let call_id = message
                    .get("tool_call_id")
                    .and_then(Value::as_str)
                    .context("tool message missing tool_call_id")?;
                if let Some(kind) = seen_tool_calls.get(call_id).copied() {
                    input.push(tool_output_to_responses_input(message, call_id, kind));
                    if tool_name(message) != "view_image" {
                        if let Some(visual_context) =
                            responses_visual_context_message(message, call_id)
                        {
                            input.push(visual_context);
                        }
                    }
                } else if let Some(orphan_context) =
                    responses_orphan_tool_context_message(message, call_id)
                {
                    input.push(orphan_context);
                }
            }
            "system" => {
                let content = message_content_as_text(message);
                if !content.is_empty() {
                    input.push(json!({
                        "type": "message",
                        "role": "user",
                        "content": [{
                            "type": "input_text",
                            "text": format!("System context:\n{content}"),
                        }],
                    }));
                }
            }
            _ => {
                let content = message_content_parts(message, role);
                if !content.is_empty() {
                    input.push(json!({
                        "type": "message",
                        "role": role,
                        "content": content,
                    }));
                }
                if role == "assistant" {
                    for call in message
                        .get("tool_calls")
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                    {
                        let response_item = tool_call_to_responses_input(call)?;
                        if let Some(call_id) = call
                            .get("id")
                            .or_else(|| call.get("call_id"))
                            .and_then(Value::as_str)
                        {
                            seen_tool_calls.insert(
                                call_id.to_string(),
                                response_tool_call_kind(&response_item),
                            );
                        }
                        input.push(response_item);
                    }
                }
            }
        }
    }
    normalize_responses_input_items(&mut input, model_info.supports_image_input);
    Ok(input)
}

fn raw_response_item_for_responses_input(message: &Value) -> Option<Value> {
    let item_type = message.get("type").and_then(Value::as_str)?;
    if !response_item_type_is_responses_input_item(item_type, message) {
        return None;
    }

    let mut item = message.clone();
    strip_codex_skipped_response_item_fields(&mut item, item_type);
    Some(item)
}

fn response_item_type_is_responses_input_item(item_type: &str, item: &Value) -> bool {
    match item_type {
        "message" => item.get("role").and_then(Value::as_str) != Some("system"),
        "function_call_output"
        | "function_call"
        | "tool_search_call"
        | "tool_search_output"
        | "custom_tool_call"
        | "custom_tool_call_output"
        | "local_shell_call"
        | "reasoning"
        | "web_search_call"
        | "image_generation_call"
        | "compaction"
        | "compaction_summary"
        | "context_compaction" => true,
        "compaction_trigger" | "other" => false,
        _ => false,
    }
}

fn strip_codex_skipped_response_item_fields(item: &mut Value, item_type: &str) {
    if item_type != "image_generation_call" {
        if let Some(object) = item.as_object_mut() {
            object.remove("id");
        }
    }

    if item_type == "reasoning"
        && item
            .get("content")
            .and_then(Value::as_array)
            .is_some_and(|content| {
                content
                    .iter()
                    .any(|part| part.get("type").and_then(Value::as_str) == Some("reasoning_text"))
            })
    {
        if let Some(object) = item.as_object_mut() {
            object.remove("content");
        }
    }
}

fn response_item_call_id(item: &Value) -> Option<&str> {
    match item.get("type").and_then(Value::as_str) {
        Some("function_call")
        | Some("custom_tool_call")
        | Some("local_shell_call")
        | Some("tool_search_call") => item.get("call_id").and_then(Value::as_str),
        _ => None,
    }
}

fn normalize_responses_input_items(items: &mut Vec<Value>, supports_image_input: bool) {
    ensure_raw_call_outputs_present(items);
    remove_orphan_raw_outputs(items);
    strip_images_when_unsupported(items, supports_image_input);
}

fn ensure_raw_call_outputs_present(items: &mut Vec<Value>) {
    let mut missing_outputs = Vec::<(usize, Value)>::new();
    for (index, item) in items.iter().enumerate() {
        match item.get("type").and_then(Value::as_str) {
            Some("function_call") => {
                let Some(call_id) = item.get("call_id").and_then(Value::as_str) else {
                    continue;
                };
                if !items.iter().any(|existing| {
                    existing.get("type").and_then(Value::as_str) == Some("function_call_output")
                        && existing.get("call_id").and_then(Value::as_str) == Some(call_id)
                }) {
                    missing_outputs.push((
                        index,
                        json!({
                            "type": "function_call_output",
                            "call_id": call_id,
                            "output": "aborted",
                        }),
                    ));
                }
            }
            Some("local_shell_call") => {
                let Some(call_id) = item.get("call_id").and_then(Value::as_str) else {
                    continue;
                };
                if !items.iter().any(|existing| {
                    existing.get("type").and_then(Value::as_str) == Some("function_call_output")
                        && existing.get("call_id").and_then(Value::as_str) == Some(call_id)
                }) {
                    missing_outputs.push((
                        index,
                        json!({
                            "type": "function_call_output",
                            "call_id": call_id,
                            "output": "aborted",
                        }),
                    ));
                }
            }
            Some("custom_tool_call") => {
                let Some(call_id) = item.get("call_id").and_then(Value::as_str) else {
                    continue;
                };
                if !items.iter().any(|existing| {
                    existing.get("type").and_then(Value::as_str) == Some("custom_tool_call_output")
                        && existing.get("call_id").and_then(Value::as_str) == Some(call_id)
                }) {
                    missing_outputs.push((
                        index,
                        json!({
                            "type": "custom_tool_call_output",
                            "call_id": call_id,
                            "output": "aborted",
                        }),
                    ));
                }
            }
            Some("tool_search_call") => {
                let Some(call_id) = item.get("call_id").and_then(Value::as_str) else {
                    continue;
                };
                if !items.iter().any(|existing| {
                    existing.get("type").and_then(Value::as_str) == Some("tool_search_output")
                        && existing.get("call_id").and_then(Value::as_str) == Some(call_id)
                }) {
                    missing_outputs.push((
                        index,
                        json!({
                            "type": "tool_search_output",
                            "call_id": call_id,
                            "status": "completed",
                            "execution": "client",
                            "tools": [],
                        }),
                    ));
                }
            }
            _ => {}
        }
    }

    for (index, output) in missing_outputs.into_iter().rev() {
        items.insert(index + 1, output);
    }
}

fn remove_orphan_raw_outputs(items: &mut Vec<Value>) {
    let function_call_ids = response_item_call_ids_for_type(items, "function_call");
    let local_shell_call_ids = response_item_call_ids_for_type(items, "local_shell_call");
    let custom_tool_call_ids = response_item_call_ids_for_type(items, "custom_tool_call");
    let tool_search_call_ids = response_item_call_ids_for_type(items, "tool_search_call");

    items.retain(|item| match item.get("type").and_then(Value::as_str) {
        Some("function_call_output") => {
            item.get("call_id")
                .and_then(Value::as_str)
                .is_some_and(|call_id| {
                    function_call_ids.contains(call_id) || local_shell_call_ids.contains(call_id)
                })
        }
        Some("custom_tool_call_output") => item
            .get("call_id")
            .and_then(Value::as_str)
            .is_some_and(|call_id| custom_tool_call_ids.contains(call_id)),
        Some("tool_search_output") => {
            if item.get("execution").and_then(Value::as_str) == Some("server") {
                true
            } else if let Some(call_id) = item.get("call_id").and_then(Value::as_str) {
                tool_search_call_ids.contains(call_id)
            } else {
                true
            }
        }
        _ => true,
    });
}

fn response_item_call_ids_for_type(items: &[Value], item_type: &str) -> HashSet<String> {
    items
        .iter()
        .filter(|item| item.get("type").and_then(Value::as_str) == Some(item_type))
        .filter_map(|item| item.get("call_id").and_then(Value::as_str))
        .map(ToString::to_string)
        .collect()
}

fn strip_images_when_unsupported(items: &mut [Value], supports_image_input: bool) {
    if supports_image_input {
        return;
    }

    for item in items {
        match item.get("type").and_then(Value::as_str) {
            Some("message") => {
                if let Some(content) = item.get_mut("content").and_then(Value::as_array_mut) {
                    replace_input_image_parts_with_placeholder(content);
                }
            }
            Some("function_call_output") | Some("custom_tool_call_output") => {
                if let Some(output) = item.get_mut("output").and_then(Value::as_array_mut) {
                    replace_input_image_parts_with_placeholder(output);
                }
            }
            Some("image_generation_call") => {
                if let Some(object) = item.as_object_mut() {
                    object.insert("result".to_string(), Value::String(String::new()));
                }
            }
            _ => {}
        }
    }
}

fn replace_input_image_parts_with_placeholder(parts: &mut [Value]) {
    for part in parts {
        if part.get("type").and_then(Value::as_str) == Some("input_image") {
            *part = json!({
                "type": "input_text",
                "text": IMAGE_CONTENT_OMITTED_PLACEHOLDER,
            });
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ResponsesToolCallKind {
    Function,
    Custom,
}

fn response_tool_call_kind(item: &Value) -> ResponsesToolCallKind {
    if item.get("type").and_then(Value::as_str) == Some("custom_tool_call") {
        ResponsesToolCallKind::Custom
    } else {
        ResponsesToolCallKind::Function
    }
}

fn tool_output_to_responses_input(
    message: &Value,
    call_id: &str,
    kind: ResponsesToolCallKind,
) -> Value {
    match kind {
        ResponsesToolCallKind::Function => {
            let output = if tool_name(message) == "view_image" {
                let images = tool_output_images(message);
                if images.is_empty() {
                    Value::String(tool_output_text(message))
                } else {
                    Value::Array(images)
                }
            } else {
                Value::String(tool_output_text(message))
            };
            json!({
                "type": "function_call_output",
                "call_id": call_id,
                "output": output,
            })
        }
        ResponsesToolCallKind::Custom => json!({
            "type": "custom_tool_call_output",
            "call_id": call_id,
            "output": tool_output_text(message),
        }),
    }
}

fn responses_orphan_tool_context_message(message: &Value, call_id: &str) -> Option<Value> {
    let text = tool_output_text(message);
    let images = tool_output_images(message);
    if text.trim().is_empty() && images.is_empty() {
        return None;
    }
    let mut content = vec![json!({
        "type": "input_text",
        "text": format!(
            "Tool output retained as context after history compaction. Original tool call {call_id} ({}):\n{}",
            tool_name(message),
            text,
        ),
    })];
    content.extend(images);
    Some(json!({
        "type": "message",
        "role": "user",
        "content": content,
    }))
}

fn messages_to_chat_messages(
    messages: &[Value],
    include_image_content: bool,
) -> Result<Vec<Value>> {
    let mut out = Vec::new();
    for message in messages {
        if should_skip_raw_response_item_for_fallback_provider(message) {
            continue;
        }
        let role = message
            .get("role")
            .and_then(Value::as_str)
            .unwrap_or("user");
        match role {
            "tool" => {
                let call_id = message
                    .get("tool_call_id")
                    .and_then(Value::as_str)
                    .context("tool message missing tool_call_id")?;
                out.push(json!({
                    "role": "tool",
                    "tool_call_id": call_id,
                    "content": chat_tool_output_text(message, include_image_content),
                }));
                if include_image_content {
                    if let Some(visual_context) = chat_visual_context_message(message, call_id) {
                        out.push(visual_context);
                    }
                } else if let Some(omitted_context) =
                    chat_omitted_visual_context_message(message, call_id)
                {
                    out.push(omitted_context);
                }
            }
            "assistant" => {
                let mut item = json!({
                    "role": "assistant",
                    "content": message_content_as_text(message),
                });
                if let Some(reasoning_content) = message
                    .get("reasoning_content")
                    .and_then(Value::as_str)
                    .filter(|text| !text.is_empty())
                {
                    item["reasoning_content"] = json!(reasoning_content);
                }
                let tool_calls = message
                    .get("tool_calls")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .map(chat_tool_call)
                    .collect::<Result<Vec<_>>>()?;
                if !tool_calls.is_empty() {
                    item["tool_calls"] = Value::Array(tool_calls);
                }
                out.push(item);
            }
            "system" => out.push(json!({
                "role": "system",
                "content": message_content_as_text(message),
            })),
            "developer" => out.push(json!({
                "role": "system",
                "content": format!("Developer context:\n{}", message_content_as_text(message)),
            })),
            _ => out.push(json!({
                "role": "user",
                "content": chat_content(message, include_image_content),
            })),
        }
    }
    Ok(out)
}

fn chat_content(message: &Value, include_image_content: bool) -> Value {
    match message.get("content") {
        Some(Value::Array(parts)) => Value::Array(
            parts
                .iter()
                .filter_map(|part| match part.get("type").and_then(Value::as_str) {
                    Some("input_image") => {
                        let image_url = part.get("image_url").and_then(Value::as_str)?;
                        if include_image_content {
                            Some(json!({
                                "type": "image_url",
                                "image_url": { "url": image_url },
                            }))
                        } else {
                            Some(json!({
                                "type": "text",
                                "text": "[image omitted: selected model endpoint does not accept image content]",
                            }))
                        }
                    }
                    Some("input_text") | Some("output_text") | Some("text") | None => part
                        .get("text")
                        .and_then(Value::as_str)
                        .filter(|text| !text.is_empty())
                        .map(|text| json!({ "type": "text", "text": text })),
                    _ => None,
                })
                .collect(),
        ),
        _ => Value::String(message_content_as_text(message)),
    }
}

fn chat_tool_call(call: &Value) -> Result<Value> {
    let call_id = call
        .get("id")
        .or_else(|| call.get("call_id"))
        .and_then(Value::as_str)
        .context("assistant tool call missing id")?;
    let name = call
        .get("name")
        .and_then(Value::as_str)
        .context("assistant tool call missing name")?;
    let arguments = call.get("arguments").cloned().unwrap_or_else(|| json!({}));
    Ok(json!({
        "id": call_id,
        "type": "function",
        "function": {
            "name": name,
            "arguments": json_string(arguments)?,
        }
    }))
}

fn messages_to_anthropic_messages(messages: &[Value], is_oauth: bool) -> Result<Vec<Value>> {
    let mut out = Vec::new();
    for message in messages {
        if should_skip_raw_response_item_for_fallback_provider(message) {
            continue;
        }
        let role = message
            .get("role")
            .and_then(Value::as_str)
            .unwrap_or("user");
        match role {
            "assistant" => out.push(json!({
                "role": "assistant",
                "content": anthropic_assistant_content(message, is_oauth)?,
            })),
            "tool" => {
                let call_id = message
                    .get("tool_call_id")
                    .and_then(Value::as_str)
                    .context("tool message missing tool_call_id")?;
                out.push(json!({
                    "role": "user",
                    "content": [{
                        "type": "tool_result",
                        "tool_use_id": call_id,
                        "content": tool_output_text(message),
                    }],
                }));
                if let Some(visual_context) = anthropic_visual_context_message(message, call_id) {
                    out.push(visual_context);
                }
            }
            "system" => out.push(json!({
                "role": "user",
                "content": [{
                    "type": "text",
                    "text": format!("System context:\n{}", message_content_as_text(message)),
                }],
            })),
            "developer" => {}
            _ => out.push(json!({
                "role": "user",
                "content": anthropic_user_content(message),
            })),
        }
    }
    drop_trailing_anthropic_assistant_prefill(&mut out);
    Ok(out)
}

fn drop_trailing_anthropic_assistant_prefill(messages: &mut Vec<Value>) {
    while messages
        .last()
        .and_then(|message| message.get("role"))
        .and_then(Value::as_str)
        == Some("assistant")
    {
        messages.pop();
    }
}

fn should_skip_raw_response_item_for_fallback_provider(message: &Value) -> bool {
    let Some(item_type) = message.get("type").and_then(Value::as_str) else {
        return false;
    };
    item_type != "message" && response_item_type_is_responses_input_item(item_type, message)
}

fn anthropic_user_content(message: &Value) -> Vec<Value> {
    match message.get("content") {
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|part| match part.get("type").and_then(Value::as_str) {
                Some("input_image") => {
                    let image_url = part.get("image_url").and_then(Value::as_str)?;
                    let safe_image_url = provider_safe_image_url(image_url);
                    data_url_source(&safe_image_url).map(|(media_type, data)| {
                        json!({
                            "type": "image",
                            "source": {
                                "type": "base64",
                                "media_type": media_type,
                                "data": data,
                            }
                        })
                    })
                }
                Some("input_text") | Some("output_text") | Some("text") | None => part
                    .get("text")
                    .and_then(Value::as_str)
                    .filter(|text| !text.is_empty())
                    .map(|text| json!({ "type": "text", "text": text })),
                _ => None,
            })
            .collect(),
        _ => vec![json!({
            "type": "text",
            "text": message_content_as_text(message),
        })],
    }
}

fn anthropic_assistant_content(message: &Value, is_oauth: bool) -> Result<Vec<Value>> {
    let mut blocks = Vec::new();
    let text = message_content_as_text(message);
    if !text.is_empty() {
        blocks.push(json!({ "type": "text", "text": text }));
    }
    for call in message
        .get("tool_calls")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        blocks.push(anthropic_tool_use_block(call, is_oauth)?);
    }
    Ok(blocks)
}

fn anthropic_tool_use_block(call: &Value, is_oauth: bool) -> Result<Value> {
    let call_id = call
        .get("id")
        .or_else(|| call.get("call_id"))
        .and_then(Value::as_str)
        .context("assistant tool call missing id")?;
    let name = call
        .get("name")
        .and_then(Value::as_str)
        .context("assistant tool call missing name")?;
    let input = call.get("arguments").cloned().unwrap_or_else(|| json!({}));
    Ok(json!({
        "type": "tool_use",
        "id": call_id,
        "name": anthropic_request_tool_name(name, is_oauth),
        "input": input,
    }))
}

fn anthropic_system_blocks(instructions: &str, is_oauth: bool) -> Value {
    let mut blocks = Vec::new();
    if is_oauth {
        blocks.push(json!({
            "type": "text",
            "text": "You are Claude Code, Anthropic's official CLI for Claude.",
            "cache_control": { "type": "ephemeral" },
        }));
    }
    blocks.push(json!({
        "type": "text",
        "text": instructions,
        "cache_control": { "type": "ephemeral" },
    }));
    Value::Array(blocks)
}

fn anthropic_system_blocks_with_developer_context(
    instructions: &str,
    messages: &[Value],
    is_oauth: bool,
) -> Value {
    let Value::Array(mut blocks) = anthropic_system_blocks(instructions, is_oauth) else {
        return anthropic_system_blocks(instructions, is_oauth);
    };
    for message in messages {
        if message.get("role").and_then(Value::as_str) != Some("developer") {
            continue;
        }
        let text = message_content_as_text(message);
        if text.is_empty() {
            continue;
        }
        blocks.push(json!({
            "type": "text",
            "text": format!("Developer context:\n{text}"),
            "cache_control": { "type": "ephemeral" },
        }));
    }
    Value::Array(blocks)
}

fn anthropic_request_tool_name(name: &str, is_oauth: bool) -> String {
    if !is_oauth {
        return name.to_string();
    }
    match name.to_ascii_lowercase().as_str() {
        "read" => "Read".to_string(),
        "write" => "Write".to_string(),
        "edit" => "Edit".to_string(),
        "shell" | "bash" => "Bash".to_string(),
        "grep" => "Grep".to_string(),
        "glob" => "Glob".to_string(),
        "todo_write" | "todowrite" => "TodoWrite".to_string(),
        "web_fetch" | "webfetch" => "WebFetch".to_string(),
        "web_search" | "websearch" => "WebSearch".to_string(),
        _ => name.to_string(),
    }
}

fn anthropic_response_tool_name(name: &str, tools: &[ToolSpec], is_oauth: bool) -> String {
    if !is_oauth {
        return name.to_string();
    }
    let lower = name.to_ascii_lowercase();
    for tool in tools {
        if lower == tool.name.to_ascii_lowercase()
            || lower == anthropic_request_tool_name(&tool.name, true).to_ascii_lowercase()
        {
            return tool.name.clone();
        }
    }
    if lower == "bash" {
        return "shell".to_string();
    }
    name.to_string()
}

fn data_url_source(image_url: &str) -> Option<(String, String)> {
    let rest = image_url.strip_prefix("data:")?;
    let (header, data) = rest.split_once(',')?;
    let media_type = header.split(';').next()?.to_string();
    Some((media_type, data.to_string()))
}

fn provider_safe_image_url(image_url: &str) -> String {
    downsample_oversized_data_url_image(image_url).unwrap_or_else(|| image_url.to_string())
}

fn downsample_oversized_data_url_image(image_url: &str) -> Option<String> {
    let (media_type, data) = data_url_source(image_url)?;
    if !media_type.starts_with("image/") {
        return None;
    }
    let bytes = general_purpose::STANDARD.decode(data.as_bytes()).ok()?;
    let image = image::load_from_memory(&bytes).ok()?;
    let width = image.width();
    let height = image.height();
    if width <= PROVIDER_MAX_IMAGE_DIMENSION && height <= PROVIDER_MAX_IMAGE_DIMENSION {
        return None;
    }

    let scale = (PROVIDER_MAX_IMAGE_DIMENSION as f32 / width as f32)
        .min(PROVIDER_MAX_IMAGE_DIMENSION as f32 / height as f32);
    let resized_width = ((width as f32 * scale).round() as u32).max(1);
    let resized_height = ((height as f32 * scale).round() as u32).max(1);
    let resized = image.resize(
        resized_width,
        resized_height,
        image::imageops::FilterType::Lanczos3,
    );

    let mut out = Cursor::new(Vec::new());
    resized.write_to(&mut out, image::ImageFormat::Png).ok()?;
    let encoded = general_purpose::STANDARD.encode(out.into_inner());
    Some(format!("data:image/png;base64,{encoded}"))
}

fn input_text_type_for_role(role: &str) -> &'static str {
    if role == "assistant" {
        "output_text"
    } else {
        "input_text"
    }
}

fn message_content_parts(message: &Value, role: &str) -> Vec<Value> {
    match message.get("content") {
        Some(Value::String(content)) if !content.is_empty() => vec![json!({
            "type": input_text_type_for_role(role),
            "text": content,
        })],
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|part| normalize_content_part(part, role))
            .collect(),
        Some(other) if !other.is_null() => vec![json!({
            "type": input_text_type_for_role(role),
            "text": other.to_string(),
        })],
        _ => Vec::new(),
    }
}

fn normalize_content_part(part: &Value, role: &str) -> Option<Value> {
    match part.get("type").and_then(Value::as_str) {
        Some("input_image") => {
            let image_url = part.get("image_url").and_then(Value::as_str)?;
            let mut out = json!({
                "type": "input_image",
                "image_url": provider_safe_image_url(image_url),
            });
            if let Some(detail) = part.get("detail").and_then(Value::as_str) {
                out["detail"] = json!(detail);
            }
            Some(out)
        }
        Some("input_text") | Some("output_text") | Some("text") | None => part
            .get("text")
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty())
            .map(|text| {
                json!({
                    "type": input_text_type_for_role(role),
                    "text": text,
                })
            }),
        _ => None,
    }
}

fn message_content_as_text(message: &Value) -> String {
    match message.get("content") {
        Some(Value::String(content)) => content.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|part| part.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        Some(other) if !other.is_null() => other.to_string(),
        _ => String::new(),
    }
}

#[derive(Clone, Copy)]
enum ToolImageStatus {
    Attached,
    Omitted,
}

fn tool_output_text(message: &Value) -> String {
    tool_output_text_with_image_status(message, ToolImageStatus::Attached)
}

fn chat_tool_output_text(message: &Value, include_image_content: bool) -> String {
    let image_status = if include_image_content {
        ToolImageStatus::Attached
    } else {
        ToolImageStatus::Omitted
    };
    tool_output_text_with_image_status(message, image_status)
}

fn tool_output_text_with_image_status(message: &Value, image_status: ToolImageStatus) -> String {
    let Some(Value::Array(parts)) = message.get("content") else {
        return message_content_as_text(message);
    };
    let mut text_parts = Vec::new();
    let mut image_count = 0;
    for part in parts {
        match part.get("type").and_then(Value::as_str) {
            Some("input_image") => image_count += 1,
            Some("output_text") | Some("input_text") | Some("text") | None => {
                if let Some(text) = part.get("text").and_then(Value::as_str) {
                    if !text.is_empty() {
                        text_parts.push(text.to_string());
                    }
                }
            }
            _ => {}
        }
    }
    if image_count > 0 {
        match image_status {
            ToolImageStatus::Attached => text_parts.push(format!(
                "[{image_count} screenshot image(s) attached in the following visual context message]"
            )),
            ToolImageStatus::Omitted => text_parts.push(format!(
                "[{image_count} screenshot image(s) omitted because this model endpoint does not accept image content]"
            )),
        }
    }
    text_parts.join("\n")
}

fn tool_output_images(message: &Value) -> Vec<Value> {
    match message.get("content") {
        Some(Value::Array(parts)) => parts.iter().filter_map(normalize_tool_image_part).collect(),
        _ => Vec::new(),
    }
}

fn normalize_tool_image_part(part: &Value) -> Option<Value> {
    if part.get("type").and_then(Value::as_str) != Some("input_image") {
        return None;
    }
    let image_url = part.get("image_url").and_then(Value::as_str)?;
    let mut out = json!({
        "type": "input_image",
        "image_url": provider_safe_image_url(image_url),
    });
    if let Some(detail) = part.get("detail").and_then(Value::as_str) {
        out["detail"] = json!(detail);
    }
    Some(out)
}

fn visual_context_text(call_id: &str, tool_name: &str) -> String {
    format!(
        "Visual context from tool call {call_id} ({tool_name}). Use these screenshots to verify the browser state before continuing. Do not call screenshot again unless the page changed or you need a different visual region."
    )
}

fn omitted_visual_context_text(call_id: &str, tool_name: &str) -> String {
    format!(
        "Visual output from tool call {call_id} ({tool_name}) was omitted because this model endpoint does not accept image content. Continue using text, DOM state, and browser_script inspection instead."
    )
}

fn tool_name(message: &Value) -> &str {
    message
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("tool")
}

fn responses_visual_context_message(message: &Value, call_id: &str) -> Option<Value> {
    let images = tool_output_images(message);
    if images.is_empty() {
        return None;
    }
    let mut content = vec![json!({
        "type": "input_text",
        "text": visual_context_text(call_id, tool_name(message)),
    })];
    content.extend(images);
    Some(json!({
        "type": "message",
        "role": "user",
        "content": content,
    }))
}

fn chat_visual_context_message(message: &Value, call_id: &str) -> Option<Value> {
    let images = tool_output_images(message);
    if images.is_empty() {
        return None;
    }
    let mut content = vec![json!({
        "type": "input_text",
        "text": visual_context_text(call_id, tool_name(message)),
    })];
    content.extend(images);
    let visual_message = json!({ "content": content });
    Some(json!({
        "role": "user",
        "content": chat_content(&visual_message, true),
    }))
}

fn chat_omitted_visual_context_message(message: &Value, call_id: &str) -> Option<Value> {
    if tool_output_images(message).is_empty() {
        return None;
    }
    Some(json!({
        "role": "user",
        "content": omitted_visual_context_text(call_id, tool_name(message)),
    }))
}

fn anthropic_visual_context_message(message: &Value, call_id: &str) -> Option<Value> {
    let images = tool_output_images(message);
    if images.is_empty() {
        return None;
    }
    let mut content = vec![json!({
        "type": "input_text",
        "text": visual_context_text(call_id, tool_name(message)),
    })];
    content.extend(images);
    let visual_message = json!({ "content": content });
    Some(json!({
        "role": "user",
        "content": anthropic_user_content(&visual_message),
    }))
}

fn tool_call_to_responses_input(call: &Value) -> Result<Value> {
    let call_id = call
        .get("id")
        .or_else(|| call.get("call_id"))
        .and_then(Value::as_str)
        .context("assistant tool call missing id")?;
    let name = call
        .get("name")
        .and_then(Value::as_str)
        .context("assistant tool call missing name")?;
    let namespace = call.get("namespace").and_then(Value::as_str);
    let arguments = call.get("arguments").cloned().unwrap_or_else(|| json!({}));
    if let Some(input) = arguments.as_str() {
        let mut item = json!({
            "type": "custom_tool_call",
            "call_id": call_id,
            "name": name,
            "input": input,
        });
        if let Some(namespace) = namespace {
            item["namespace"] = Value::String(namespace.to_string());
        }
        return Ok(item);
    }
    let mut item = json!({
        "type": "function_call",
        "call_id": call_id,
        "name": name,
        "arguments": json_string(arguments)?,
    });
    if let Some(namespace) = namespace {
        item["namespace"] = Value::String(namespace.to_string());
    }
    Ok(item)
}

fn json_string(value: Value) -> Result<String> {
    match value {
        Value::String(raw) => Ok(raw),
        other => serde_json::to_string(&other).context("serialize tool call arguments"),
    }
}

fn parse_responses_output(body: &Value, model: &str) -> Result<Vec<ModelEvent>> {
    let mut events = Vec::new();
    let mut stream_state = CodexSseStreamState::default();
    if let Some(items) = body.get("output").and_then(Value::as_array) {
        for item in items {
            maybe_push_codex_output_item(item, &mut stream_state, &mut events)?;
        }
    }
    if events
        .iter()
        .all(|event| !matches!(event, ModelEvent::TextDelta { .. }))
    {
        if let Some(text) = body.get("output_text").and_then(Value::as_str) {
            if !text.is_empty() {
                events.push(ModelEvent::TextDelta {
                    text: text.to_string(),
                });
            }
        }
    }
    if let Some(usage) = parse_usage(body.get("usage"), model) {
        events.push(response_completed_event(body)?);
        events.push(ModelEvent::Usage { usage });
    } else {
        events.push(response_completed_event(body)?);
    }
    events.push(ModelEvent::Done);
    Ok(events)
}

fn parse_chat_completion_output(
    body: &Value,
    model: &str,
    tools: &[ToolSpec],
) -> Result<Vec<ModelEvent>> {
    let mut events = Vec::new();
    let Some(message) = body
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|choices| choices.first())
        .and_then(|choice| choice.get("message"))
    else {
        bail!("OpenAI-compatible chat response missing choices[0].message");
    };
    if let Some(reasoning_content) = message.get("reasoning_content").and_then(Value::as_str) {
        if !reasoning_content.is_empty() {
            events.push(ModelEvent::ThinkingDelta {
                text: reasoning_content.to_string(),
                label: Some("reasoning".to_string()),
            });
        }
    }
    if let Some(content) = message.get("content").and_then(Value::as_str) {
        if !content.is_empty() {
            events.push(ModelEvent::TextDelta {
                text: content.to_string(),
            });
        }
    }
    for call in message
        .get("tool_calls")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let Some(function) = call.get("function") else {
            continue;
        };
        let name = function
            .get("name")
            .and_then(Value::as_str)
            .context("chat tool call missing function.name")?;
        let call_id = call
            .get("id")
            .and_then(Value::as_str)
            .context("chat tool call missing id")?;
        let arguments = chat_tool_call_arguments(name, function, tools);
        events.push(ModelEvent::ToolCall {
            call: ToolCall {
                id: call_id.to_string(),
                name: name.to_string(),
                namespace: None,
                arguments,
            },
        });
    }
    if let Some(usage) = parse_chat_usage(body.get("usage"), model) {
        events.push(ModelEvent::Usage { usage });
    }
    events.push(ModelEvent::Done);
    Ok(events)
}

fn parse_chat_completion_sse_stream(
    response: reqwest::blocking::Response,
    model: &str,
    tools: &[ToolSpec],
    stream_idle_timeout: Duration,
    on_event: &mut dyn FnMut(ModelEvent) -> Result<()>,
) -> Result<()> {
    if response_is_json(&response) {
        let body_text = response
            .text()
            .context("read OpenAI-compatible chat JSON body")?;
        let body: Value = serde_json::from_str(&body_text).map_err(|error| {
            ProviderError::retryable(format!("parse OpenAI-compatible chat JSON: {error}"), None)
        })?;
        for event in parse_chat_completion_output(&body, model, tools)? {
            on_event(event)?;
        }
        return Ok(());
    }

    let mut tool_calls = BTreeMap::<usize, ChatStreamToolCall>::new();
    let mut emitted_done = false;
    read_sse_data_stream(
        response,
        stream_idle_timeout,
        "read OpenAI-compatible chat SSE line",
        &mut |data| {
            if data.trim() == "[DONE]" {
                flush_chat_stream_tool_calls(&mut tool_calls, tools, on_event)?;
                on_event(ModelEvent::Done)?;
                emitted_done = true;
                return Ok(());
            }
            let chunk: Value = serde_json::from_str(data).map_err(|error| {
                ProviderError::retryable(
                    format!("parse OpenAI-compatible chat SSE JSON: {error}"),
                    None,
                )
            })?;
            if chunk
                .get("choices")
                .and_then(Value::as_array)
                .and_then(|choices| choices.first())
                .and_then(|choice| choice.get("message"))
                .is_some()
            {
                for event in parse_chat_completion_output(&chunk, model, tools)? {
                    on_event(event)?;
                }
                emitted_done = true;
                return Ok(());
            }
            if let Some(usage) = parse_chat_usage(chunk.get("usage"), model) {
                on_event(ModelEvent::Usage { usage })?;
            }
            for choice in chunk
                .get("choices")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                let Some(delta) = choice.get("delta") else {
                    continue;
                };
                if let Some(content) = delta.get("content").and_then(Value::as_str) {
                    if !content.is_empty() {
                        on_event(ModelEvent::TextDelta {
                            text: content.to_string(),
                        })?;
                    }
                }
                if let Some(text) = delta
                    .get("reasoning_content")
                    .or_else(|| delta.get("reasoning"))
                    .and_then(Value::as_str)
                    .filter(|text| !text.is_empty())
                {
                    on_event(ModelEvent::ThinkingDelta {
                        text: text.to_string(),
                        label: Some("reasoning".to_string()),
                    })?;
                }
                for tool_delta in delta
                    .get("tool_calls")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    let index = tool_delta
                        .get("index")
                        .and_then(Value::as_u64)
                        .unwrap_or(tool_calls.len() as u64)
                        as usize;
                    let entry = tool_calls.entry(index).or_default();
                    if let Some(id) = tool_delta.get("id").and_then(Value::as_str) {
                        entry.id = Some(id.to_string());
                    }
                    if let Some(function) = tool_delta.get("function") {
                        if let Some(name) = function.get("name").and_then(Value::as_str) {
                            entry.name = Some(name.to_string());
                        }
                        if let Some(arguments) = function.get("arguments").and_then(Value::as_str) {
                            entry.arguments.push_str(arguments);
                        }
                    }
                }
            }
            Ok(())
        },
    )?;
    if !emitted_done {
        return Err(
            ProviderError::stream("OpenAI-compatible chat stream closed before [DONE]").into(),
        );
    }
    Ok(())
}

#[derive(Default)]
struct ChatStreamToolCall {
    id: Option<String>,
    name: Option<String>,
    arguments: String,
}

fn flush_chat_stream_tool_calls(
    tool_calls: &mut BTreeMap<usize, ChatStreamToolCall>,
    tools: &[ToolSpec],
    on_event: &mut dyn FnMut(ModelEvent) -> Result<()>,
) -> Result<()> {
    let pending = std::mem::take(tool_calls);
    for (_, call) in pending {
        let name = call
            .name
            .filter(|name| !name.trim().is_empty())
            .context("chat streamed tool call missing function.name")?;
        let call_id = call
            .id
            .filter(|id| !id.trim().is_empty())
            .context("chat streamed tool call missing id")?;
        let function = json!({ "arguments": call.arguments });
        on_event(ModelEvent::ToolCall {
            call: ToolCall {
                id: call_id,
                name: name.clone(),
                namespace: None,
                arguments: chat_tool_call_arguments(&name, &function, tools),
            },
        })?;
    }
    Ok(())
}

fn chat_tool_call_arguments(name: &str, function: &Value, tools: &[ToolSpec]) -> Value {
    let raw = function
        .get("arguments")
        .and_then(Value::as_str)
        .unwrap_or_default();
    serde_json::from_str::<Value>(raw)
        .unwrap_or_else(|_| freeform_tool_arguments_from_raw(name, raw, tools))
}

fn freeform_tool_arguments_from_raw(name: &str, raw: &str, tools: &[ToolSpec]) -> Value {
    let Some(tool) = tools
        .iter()
        .find(|tool| tool.name == name && tool.freeform.is_some())
    else {
        return json!({});
    };
    if let Some(field_name) = freeform_tool_payload_field(tool) {
        json!({ field_name: raw })
    } else {
        Value::String(raw.to_string())
    }
}

fn parse_anthropic_messages_output(
    body: &Value,
    model: &str,
    tools: &[ToolSpec],
    is_oauth: bool,
) -> Result<Vec<ModelEvent>> {
    let mut events = Vec::new();
    for block in body
        .get("content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        match block.get("type").and_then(Value::as_str) {
            Some("text") => {
                if let Some(text) = block.get("text").and_then(Value::as_str) {
                    events.push(ModelEvent::TextDelta {
                        text: text.to_string(),
                    });
                }
            }
            Some("thinking") => {
                if let Some(text) = block
                    .get("thinking")
                    .or_else(|| block.get("text"))
                    .and_then(Value::as_str)
                    .filter(|text| !text.trim().is_empty())
                {
                    events.push(ModelEvent::ThinkingDelta {
                        text: text.to_string(),
                        label: Some("thinking".to_string()),
                    });
                }
            }
            Some("tool_use") => {
                let name = block
                    .get("name")
                    .and_then(Value::as_str)
                    .context("Anthropic tool_use missing name")?;
                let call_id = block
                    .get("id")
                    .and_then(Value::as_str)
                    .context("Anthropic tool_use missing id")?;
                events.push(ModelEvent::ToolCall {
                    call: ToolCall {
                        id: call_id.to_string(),
                        name: anthropic_response_tool_name(name, tools, is_oauth),
                        namespace: None,
                        arguments: anthropic_tool_use_arguments(
                            &anthropic_response_tool_name(name, tools, is_oauth),
                            block.get("input"),
                            tools,
                        ),
                    },
                });
            }
            _ => {}
        }
    }
    if let Some(usage) = parse_usage(body.get("usage"), model) {
        events.push(ModelEvent::Usage { usage });
    }
    events.push(ModelEvent::Done);
    Ok(events)
}

fn parse_anthropic_messages_sse_stream(
    response: reqwest::blocking::Response,
    model: &str,
    tools: &[ToolSpec],
    is_oauth: bool,
    stream_idle_timeout: Duration,
    on_event: &mut dyn FnMut(ModelEvent) -> Result<()>,
) -> Result<()> {
    if response_is_json(&response) {
        let body_text = response
            .text()
            .context("read Anthropic Messages JSON body")?;
        let body: Value = serde_json::from_str(&body_text).map_err(|error| {
            ProviderError::retryable(format!("parse Anthropic Messages JSON: {error}"), None)
        })?;
        for event in parse_anthropic_messages_output(&body, model, tools, is_oauth)? {
            on_event(event)?;
        }
        return Ok(());
    }

    let mut state = AnthropicStreamState::default();
    let mut emitted_done = false;
    read_sse_data_stream(
        response,
        stream_idle_timeout,
        "read Anthropic Messages SSE line",
        &mut |data| {
            let event: Value = serde_json::from_str(data).map_err(|error| {
                ProviderError::retryable(
                    format!("parse Anthropic Messages SSE JSON: {error}"),
                    None,
                )
            })?;
            if event.get("content").and_then(Value::as_array).is_some() {
                for parsed in parse_anthropic_messages_output(&event, model, tools, is_oauth)? {
                    on_event(parsed)?;
                }
                emitted_done = true;
                return Ok(());
            }
            match event.get("type").and_then(Value::as_str) {
                Some("message_start") => {
                    state.merge_usage(
                        event
                            .get("message")
                            .and_then(|message| message.get("usage")),
                    );
                }
                Some("content_block_start") => {
                    let index = event
                        .get("index")
                        .and_then(Value::as_u64)
                        .unwrap_or(state.tool_blocks.len() as u64)
                        as usize;
                    if let Some(block) = event.get("content_block") {
                        state.remember_content_block(index, block);
                    }
                }
                Some("content_block_delta") => {
                    let index = event.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
                    if let Some(delta) = event.get("delta") {
                        match delta.get("type").and_then(Value::as_str) {
                            Some("text_delta") => {
                                if let Some(text) = delta.get("text").and_then(Value::as_str) {
                                    if !text.is_empty() {
                                        on_event(ModelEvent::TextDelta {
                                            text: text.to_string(),
                                        })?;
                                    }
                                }
                            }
                            Some("thinking_delta") => {
                                if let Some(text) = delta.get("thinking").and_then(Value::as_str) {
                                    if !text.is_empty() {
                                        on_event(ModelEvent::ThinkingDelta {
                                            text: text.to_string(),
                                            label: Some("thinking".to_string()),
                                        })?;
                                    }
                                }
                            }
                            Some("input_json_delta") => {
                                if let Some(partial) =
                                    delta.get("partial_json").and_then(Value::as_str)
                                {
                                    state
                                        .tool_blocks
                                        .entry(index)
                                        .or_default()
                                        .input_json
                                        .push_str(partial);
                                }
                            }
                            _ => {}
                        }
                    }
                }
                Some("content_block_stop") => {
                    let index = event.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
                    if let Some(call) = state.finish_tool_block(index, tools, is_oauth)? {
                        on_event(ModelEvent::ToolCall { call })?;
                    }
                }
                Some("message_delta") => {
                    state.merge_usage(event.get("usage"));
                }
                Some("message_stop") => {
                    for call in state.finish_all_tool_blocks(tools, is_oauth)? {
                        on_event(ModelEvent::ToolCall { call })?;
                    }
                    if let Some(usage) = state.usage(model) {
                        on_event(ModelEvent::Usage { usage })?;
                    }
                    on_event(ModelEvent::Done)?;
                    emitted_done = true;
                }
                Some("ping") => {}
                Some("error") => bail!("Anthropic stream error: {event}"),
                _ => {}
            }
            Ok(())
        },
    )?;
    if !emitted_done {
        return Err(ProviderError::stream("Anthropic stream closed before message_stop").into());
    }
    Ok(())
}

#[derive(Default)]
struct AnthropicStreamState {
    input_tokens: Option<i64>,
    output_tokens: Option<i64>,
    tool_blocks: BTreeMap<usize, AnthropicStreamToolBlock>,
}

impl AnthropicStreamState {
    fn merge_usage(&mut self, usage: Option<&Value>) {
        if let Some(input_tokens) = usage
            .and_then(|usage| usage.get("input_tokens"))
            .and_then(Value::as_i64)
        {
            self.input_tokens = Some(input_tokens);
        }
        if let Some(output_tokens) = usage
            .and_then(|usage| usage.get("output_tokens"))
            .and_then(Value::as_i64)
        {
            self.output_tokens = Some(output_tokens);
        }
    }

    fn remember_content_block(&mut self, index: usize, block: &Value) {
        if block.get("type").and_then(Value::as_str) != Some("tool_use") {
            return;
        }
        let entry = self.tool_blocks.entry(index).or_default();
        if let Some(id) = block.get("id").and_then(Value::as_str) {
            entry.id = Some(id.to_string());
        }
        if let Some(name) = block.get("name").and_then(Value::as_str) {
            entry.name = Some(name.to_string());
        }
        if let Some(input) = block.get("input").filter(|input| !input.is_null()) {
            entry.input = Some(input.clone());
        }
    }

    fn finish_tool_block(
        &mut self,
        index: usize,
        tools: &[ToolSpec],
        is_oauth: bool,
    ) -> Result<Option<ToolCall>> {
        let Some(block) = self.tool_blocks.remove(&index) else {
            return Ok(None);
        };
        block.into_tool_call(tools, is_oauth).map(Some)
    }

    fn finish_all_tool_blocks(
        &mut self,
        tools: &[ToolSpec],
        is_oauth: bool,
    ) -> Result<Vec<ToolCall>> {
        let pending = std::mem::take(&mut self.tool_blocks);
        pending
            .into_values()
            .map(|block| block.into_tool_call(tools, is_oauth))
            .collect()
    }

    fn usage(&self, model: &str) -> Option<ModelUsage> {
        if self.input_tokens.is_none() && self.output_tokens.is_none() {
            return None;
        }
        parse_usage(
            Some(&json!({
                "input_tokens": self.input_tokens,
                "output_tokens": self.output_tokens,
            })),
            model,
        )
    }
}

#[derive(Default)]
struct AnthropicStreamToolBlock {
    id: Option<String>,
    name: Option<String>,
    input: Option<Value>,
    input_json: String,
}

impl AnthropicStreamToolBlock {
    fn into_tool_call(self, tools: &[ToolSpec], is_oauth: bool) -> Result<ToolCall> {
        let name = self
            .name
            .filter(|name| !name.trim().is_empty())
            .context("Anthropic streamed tool_use missing name")?;
        let call_id = self
            .id
            .filter(|id| !id.trim().is_empty())
            .context("Anthropic streamed tool_use missing id")?;
        let local_name = anthropic_response_tool_name(&name, tools, is_oauth);
        let input = if self.input_json.trim().is_empty() {
            self.input.unwrap_or_else(|| json!({}))
        } else {
            serde_json::from_str::<Value>(&self.input_json).unwrap_or_else(|_| {
                freeform_tool_arguments_from_raw(&local_name, &self.input_json, tools)
            })
        };
        Ok(ToolCall {
            id: call_id,
            name: local_name.clone(),
            namespace: None,
            arguments: anthropic_tool_use_arguments(&local_name, Some(&input), tools),
        })
    }
}

fn anthropic_tool_use_arguments(name: &str, input: Option<&Value>, tools: &[ToolSpec]) -> Value {
    match input {
        Some(Value::String(raw)) => freeform_tool_arguments_from_raw(name, raw, tools),
        Some(input) => input.clone(),
        None => json!({}),
    }
}

fn parse_response_output_item(item: &Value, events: &mut Vec<ModelEvent>) -> Result<()> {
    match item.get("type").and_then(Value::as_str) {
        Some("message") => {
            if let Some(parts) = item.get("content").and_then(Value::as_array) {
                for part in parts {
                    if matches!(
                        part.get("type").and_then(Value::as_str),
                        Some("output_text") | Some("text")
                    ) {
                        if let Some(text) = part.get("text").and_then(Value::as_str) {
                            events.push(ModelEvent::TextDelta {
                                text: text.to_string(),
                            });
                        }
                    }
                }
            }
        }
        Some("reasoning") => {
            for summary in item
                .get("summary")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                if let Some(text) = summary
                    .get("text")
                    .and_then(Value::as_str)
                    .filter(|text| !text.trim().is_empty())
                {
                    events.push(ModelEvent::ThinkingDelta {
                        text: text.to_string(),
                        label: Some("reasoning summary".to_string()),
                    });
                }
            }
        }
        Some("function_call") => {
            let name = item
                .get("name")
                .and_then(Value::as_str)
                .context("function_call missing name")?;
            let call_id = item
                .get("call_id")
                .or_else(|| item.get("id"))
                .and_then(Value::as_str)
                .context("function_call missing call_id")?;
            let arguments = item
                .get("arguments")
                .and_then(Value::as_str)
                .and_then(|raw| serde_json::from_str::<Value>(raw).ok())
                .unwrap_or_else(|| json!({}));
            events.push(ModelEvent::ToolCall {
                call: ToolCall {
                    id: call_id.to_string(),
                    name: name.to_string(),
                    namespace: item
                        .get("namespace")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    arguments,
                },
            });
        }
        Some("custom_tool_call") => {
            let name = item
                .get("name")
                .and_then(Value::as_str)
                .context("custom_tool_call missing name")?;
            let call_id = item
                .get("call_id")
                .or_else(|| item.get("id"))
                .and_then(Value::as_str)
                .context("custom_tool_call missing call_id")?;
            let input = item
                .get("input")
                .and_then(Value::as_str)
                .unwrap_or_default();
            events.push(ModelEvent::ToolCall {
                call: ToolCall {
                    id: call_id.to_string(),
                    name: name.to_string(),
                    namespace: item
                        .get("namespace")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    arguments: Value::String(input.to_string()),
                },
            });
        }
        Some("tool_search_call") => {
            let call_id = item
                .get("call_id")
                .or_else(|| item.get("id"))
                .and_then(Value::as_str)
                .context("tool_search_call missing call_id")?;
            events.push(ModelEvent::ToolCall {
                call: ToolCall {
                    id: call_id.to_string(),
                    name: "tool_search".to_string(),
                    namespace: None,
                    arguments: item.get("arguments").cloned().unwrap_or_else(|| json!({})),
                },
            });
        }
        _ => {}
    }
    Ok(())
}

fn parse_usage(usage: Option<&Value>, model: &str) -> Option<ModelUsage> {
    let usage = usage?;
    let native_cost = usage
        .get("cost")
        .or_else(|| usage.get("total_cost"))
        .or_else(|| usage.get("cost_usd"))
        .and_then(value_f64);
    let raw_input_tokens = usage
        .get("input_tokens")
        .or_else(|| usage.get("prompt_tokens"))
        .and_then(Value::as_i64);
    let cached_input_tokens = usage
        .get("input_tokens_details")
        .and_then(|details| details.get("cached_tokens"))
        .or_else(|| {
            usage
                .get("prompt_tokens_details")
                .and_then(|details| details.get("cached_tokens"))
        })
        .or_else(|| usage.get("cache_read_input_tokens"))
        .and_then(Value::as_i64);
    let cache_creation_tokens = usage
        .get("cache_creation_input_tokens")
        .or_else(|| usage.get("prompt_cache_creation_tokens"))
        .and_then(Value::as_i64);
    let input_tokens = raw_input_tokens.map(|tokens| {
        if usage.get("cache_read_input_tokens").is_some()
            || usage.get("cache_creation_input_tokens").is_some()
        {
            tokens + cached_input_tokens.unwrap_or(0)
        } else {
            tokens
        }
    });
    let output_tokens = usage
        .get("output_tokens")
        .or_else(|| usage.get("completion_tokens"))
        .and_then(Value::as_i64);
    let reasoning_output_tokens = usage
        .get("output_tokens_details")
        .and_then(|details| details.get("reasoning_tokens"))
        .or_else(|| {
            usage
                .get("completion_tokens_details")
                .and_then(|details| details.get("reasoning_tokens"))
        })
        .and_then(Value::as_i64);
    let has_anthropic_cache_fields = usage.get("cache_read_input_tokens").is_some()
        || usage.get("cache_creation_input_tokens").is_some();
    let computed_total_tokens = input_tokens? + cache_creation_tokens.unwrap_or(0) + output_tokens?;
    let total_tokens = if has_anthropic_cache_fields {
        Some(computed_total_tokens)
    } else {
        usage
            .get("total_tokens")
            .and_then(Value::as_i64)
            .or(Some(computed_total_tokens))
    };
    let usage = ModelUsage {
        input_tokens,
        input_cached_tokens: cached_input_tokens,
        input_cache_creation_tokens: cache_creation_tokens,
        output_tokens,
        reasoning_output_tokens,
        total_tokens,
        input_cost_usd: None,
        input_cached_cost_usd: None,
        input_cache_creation_cost_usd: None,
        output_cost_usd: None,
        cost_usd: native_cost,
        cost_source: native_cost.map(|_| "native".to_string()),
    };
    Some(add_usage_cost(model, usage))
}

fn value_f64(value: &Value) -> Option<f64> {
    value
        .as_f64()
        .or_else(|| value.as_str().and_then(|raw| raw.parse::<f64>().ok()))
}

fn parse_chat_usage(usage: Option<&Value>, model: &str) -> Option<ModelUsage> {
    parse_usage(usage, model)
}

#[derive(Clone, Debug, Default)]
struct ModelPricing {
    input_cost_per_token: Option<f64>,
    output_cost_per_token: Option<f64>,
    cache_read_input_token_cost: Option<f64>,
    cache_creation_input_token_cost: Option<f64>,
}

fn add_usage_cost(model: &str, mut usage: ModelUsage) -> ModelUsage {
    if usage.cost_usd.is_some() {
        usage
            .cost_source
            .get_or_insert_with(|| "native".to_string());
        return usage;
    }
    if !calculate_cost_enabled() {
        return usage;
    }
    let Some(pricing) = model_pricing(model) else {
        return usage;
    };
    let input_tokens = usage.input_tokens.unwrap_or(0).max(0);
    let cached_tokens = usage.input_cached_tokens.unwrap_or(0).max(0);
    let cache_creation_tokens = usage.input_cache_creation_tokens.unwrap_or(0).max(0);
    let output_tokens = usage.output_tokens.unwrap_or(0).max(0);
    let uncached_input_tokens = input_tokens.saturating_sub(cached_tokens);

    usage.input_cost_usd = pricing
        .input_cost_per_token
        .map(|price| uncached_input_tokens as f64 * price);
    usage.input_cached_cost_usd = if cached_tokens > 0 {
        pricing
            .cache_read_input_token_cost
            .map(|price| cached_tokens as f64 * price)
    } else {
        None
    };
    usage.input_cache_creation_cost_usd = if cache_creation_tokens > 0 {
        pricing
            .cache_creation_input_token_cost
            .map(|price| cache_creation_tokens as f64 * price)
    } else {
        None
    };
    usage.output_cost_usd = pricing
        .output_cost_per_token
        .map(|price| output_tokens as f64 * price);

    let total = usage.input_cost_usd.unwrap_or(0.0)
        + usage.input_cached_cost_usd.unwrap_or(0.0)
        + usage.input_cache_creation_cost_usd.unwrap_or(0.0)
        + usage.output_cost_usd.unwrap_or(0.0);
    if total > 0.0 {
        usage.cost_usd = Some(total);
        usage.cost_source = Some("estimated".to_string());
    }
    usage
}

fn include_openai_compatible_usage() -> bool {
    std::env::var("LLM_BROWSER_OPENAI_COMPAT_INCLUDE_USAGE")
        .map(|value| {
            matches!(
                value.to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
        .unwrap_or(false)
}

fn calculate_cost_enabled() -> bool {
    std::env::var("BU_USE_CALCULATE_COST")
        .map(|value| {
            matches!(
                value.to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
        .unwrap_or(false)
}

fn model_pricing(model: &str) -> Option<ModelPricing> {
    custom_model_pricing(model).or_else(|| pricing_from_litellm(model))
}

fn custom_model_pricing(model: &str) -> Option<ModelPricing> {
    let pricing = match model {
        "accounts/fireworks/models/glm-4p7" => ModelPricing {
            input_cost_per_token: Some(0.60 / 1_000_000.0),
            output_cost_per_token: Some(2.20 / 1_000_000.0),
            ..Default::default()
        },
        "accounts/fireworks/models/glm-4p7-flash" => ModelPricing {
            input_cost_per_token: Some(0.07 / 1_000_000.0),
            output_cost_per_token: Some(0.40 / 1_000_000.0),
            ..Default::default()
        },
        "accounts/fireworks/models/glm-5" => ModelPricing {
            input_cost_per_token: Some(1.00 / 1_000_000.0),
            output_cost_per_token: Some(3.20 / 1_000_000.0),
            cache_read_input_token_cost: Some(0.20 / 1_000_000.0),
            ..Default::default()
        },
        "accounts/fireworks/models/kimi-k2p5" | "kimi-k2.5" => ModelPricing {
            input_cost_per_token: Some(0.60 / 1_000_000.0),
            output_cost_per_token: Some(3.00 / 1_000_000.0),
            cache_read_input_token_cost: Some(0.10 / 1_000_000.0),
            ..Default::default()
        },
        "accounts/fireworks/models/minimax-m2p5" => ModelPricing {
            input_cost_per_token: Some(0.30 / 1_000_000.0),
            output_cost_per_token: Some(1.20 / 1_000_000.0),
            cache_read_input_token_cost: Some(0.029 / 1_000_000.0),
            ..Default::default()
        },
        _ => return None,
    };
    Some(pricing)
}

fn pricing_from_litellm(model: &str) -> Option<ModelPricing> {
    let pricing_data = litellm_pricing_data()?;
    let model_data = find_model_pricing_data(pricing_data, model)?;
    Some(ModelPricing {
        input_cost_per_token: model_data
            .get("input_cost_per_token")
            .and_then(Value::as_f64),
        output_cost_per_token: model_data
            .get("output_cost_per_token")
            .and_then(Value::as_f64),
        cache_read_input_token_cost: model_data
            .get("cache_read_input_token_cost")
            .and_then(Value::as_f64),
        cache_creation_input_token_cost: model_data
            .get("cache_creation_input_token_cost")
            .and_then(Value::as_f64),
    })
}

fn find_model_pricing_data<'a>(
    pricing_data: &'a HashMap<String, Value>,
    model: &str,
) -> Option<&'a Value> {
    if let Some(data) = pricing_data.get(model) {
        return Some(data);
    }
    if let Some(mapped) = model_to_litellm(model) {
        if let Some(data) = pricing_data.get(mapped) {
            return Some(data);
        }
    }
    for prefix in ["anthropic/", "openai/", "google/", "azure/"] {
        let prefixed = format!("{prefix}{model}");
        if let Some(data) = pricing_data.get(&prefixed) {
            return Some(data);
        }
    }
    if let Some((_, bare)) = model.split_once('/') {
        if let Some(data) = pricing_data.get(bare) {
            return Some(data);
        }
    }
    None
}

fn model_to_litellm(model: &str) -> Option<&'static str> {
    match model {
        "gemini-flash-latest" => Some("gemini/gemini-flash-latest"),
        _ => None,
    }
}

fn litellm_pricing_data() -> Option<&'static HashMap<String, Value>> {
    static PRICING_DATA: OnceLock<Option<HashMap<String, Value>>> = OnceLock::new();
    PRICING_DATA.get_or_init(load_litellm_pricing_data).as_ref()
}

fn load_litellm_pricing_data() -> Option<HashMap<String, Value>> {
    let cache_path = pricing_cache_dir().join("model_prices_and_context_window.json");
    if cache_path.exists() && cache_file_fresh(&cache_path) {
        if let Ok(raw) = fs::read_to_string(&cache_path) {
            if let Ok(data) = serde_json::from_str::<HashMap<String, Value>>(&raw) {
                return Some(data);
            }
        }
    }

    let response = reqwest::blocking::Client::new()
        .get("https://raw.githubusercontent.com/BerriAI/litellm/main/model_prices_and_context_window.json")
        .send()
        .ok()?;
    if !response.status().is_success() {
        return None;
    }
    let raw = response.text().ok()?;
    let data = serde_json::from_str::<HashMap<String, Value>>(&raw).ok()?;
    if let Some(parent) = cache_path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let _ = fs::write(cache_path, raw);
    Some(data)
}

fn pricing_cache_dir() -> PathBuf {
    if let Ok(path) = std::env::var("XDG_CACHE_HOME") {
        let path = PathBuf::from(path);
        if path.is_absolute() {
            return path.join("bu_use").join("token_cost");
        }
    }
    std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."))
        .join(".cache")
        .join("bu_use")
        .join("token_cost")
}

fn cache_file_fresh(path: &Path) -> bool {
    let Ok(modified) = path.metadata().and_then(|metadata| metadata.modified()) else {
        return false;
    };
    SystemTime::now()
        .duration_since(modified)
        .unwrap_or(Duration::MAX)
        < Duration::from_secs(24 * 60 * 60)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::mpsc;
    use std::thread;
    use std::time::Instant;

    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    struct EnvVarGuard {
        key: &'static str,
        old_value: Option<std::ffi::OsString>,
    }

    impl EnvVarGuard {
        fn set_str(key: &'static str, value: &str) -> Self {
            let old_value = std::env::var_os(key);
            std::env::set_var(key, value);
            Self { key, old_value }
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            if let Some(value) = &self.old_value {
                std::env::set_var(self.key, value);
            } else {
                std::env::remove_var(self.key);
            }
        }
    }

    fn image_capable_model_info() -> ModelRequestInfo {
        ModelRequestInfo {
            supports_image_input: true,
            ..ModelRequestInfo::unknown()
        }
    }

    #[test]
    fn fake_provider_returns_scripted_events() -> Result<()> {
        let provider = FakeProvider::with_text("hello");
        let events = provider.start_turn(ProviderTurn::default())?;
        assert_eq!(
            events,
            vec![
                ModelEvent::TextDelta {
                    text: "hello".to_string()
                },
                ModelEvent::Done
            ]
        );
        Ok(())
    }

    #[test]
    fn scripted_provider_returns_one_turn_at_a_time() -> Result<()> {
        let provider = ScriptedProvider::new(vec![
            vec![ModelEvent::TextDelta {
                text: "first".to_string(),
            }],
            vec![ModelEvent::TextDelta {
                text: "second".to_string(),
            }],
        ]);
        assert_eq!(provider.start_turn(ProviderTurn::default())?.len(), 1);
        let second = provider.start_turn(ProviderTurn::default())?;
        assert_eq!(
            second,
            vec![ModelEvent::TextDelta {
                text: "second".to_string()
            }]
        );
        assert_eq!(
            provider.start_turn(ProviderTurn::default())?,
            vec![ModelEvent::Done]
        );
        Ok(())
    }

    #[test]
    fn responses_input_preserves_user_image_parts() -> Result<()> {
        let image_model = ModelRequestInfo {
            supports_image_input: true,
            ..ModelRequestInfo::unknown()
        };
        let input = messages_to_responses_input_for_model(
            &[json!({
                "role": "user",
                "content": [
                    {"type": "input_text", "text": "look"},
                    {"type": "input_image", "image_url": "data:image/png;base64,abc", "detail": "high"}
                ]
            })],
            &image_model,
        )?;
        assert_eq!(input[0]["content"][0]["type"], "input_text");
        assert_eq!(input[0]["content"][1]["type"], "input_image");
        assert_eq!(
            input[0]["content"][1]["image_url"],
            "data:image/png;base64,abc"
        );
        assert_eq!(input[0]["content"][1]["detail"], "high");
        Ok(())
    }

    #[test]
    fn responses_input_strips_images_when_model_does_not_support_images_like_codex() -> Result<()> {
        let text_only_model = ModelRequestInfo {
            supports_image_input: false,
            ..ModelRequestInfo::unknown()
        };
        let input = messages_to_responses_input_for_model(
            &[
                json!({
                    "role": "user",
                    "content": [
                        {"type": "input_text", "text": "look"},
                        {"type": "input_image", "image_url": "data:image/png;base64,abc", "detail": "high"},
                        {"type": "input_text", "text": "caption"}
                    ]
                }),
                json!({
                    "type": "function_call",
                    "call_id": "call_image",
                    "name": "view_image",
                    "arguments": "{}"
                }),
                json!({
                    "type": "function_call_output",
                    "call_id": "call_image",
                    "output": [
                        {"type": "input_text", "text": "image result"},
                        {"type": "input_image", "image_url": "data:image/png;base64,result", "detail": "auto"}
                    ]
                }),
                json!({
                    "type": "custom_tool_call",
                    "call_id": "call_custom",
                    "name": "apply_patch",
                    "input": "*** Begin Patch\n*** End Patch"
                }),
                json!({
                    "type": "custom_tool_call_output",
                    "call_id": "call_custom",
                    "output": [
                        {"type": "input_text", "text": "custom result"},
                        {"type": "input_image", "image_url": "data:image/png;base64,custom", "detail": "high"}
                    ]
                }),
                json!({
                    "type": "image_generation_call",
                    "id": "ig_123",
                    "status": "completed",
                    "result": "Zm9v"
                }),
            ],
            &text_only_model,
        )?;

        assert_eq!(input[0]["content"][1]["type"], "input_text");
        assert_eq!(
            input[0]["content"][1]["text"],
            IMAGE_CONTENT_OMITTED_PLACEHOLDER
        );
        assert!(input[0]["content"][1].get("image_url").is_none());
        assert_eq!(input[2]["output"][1]["type"], "input_text");
        assert_eq!(
            input[2]["output"][1]["text"],
            IMAGE_CONTENT_OMITTED_PLACEHOLDER
        );
        assert_eq!(input[4]["output"][1]["type"], "input_text");
        assert_eq!(
            input[4]["output"][1]["text"],
            IMAGE_CONTENT_OMITTED_PLACEHOLDER
        );
        assert_eq!(input[5]["type"], "image_generation_call");
        assert_eq!(input[5]["result"], "");
        Ok(())
    }

    #[test]
    fn responses_input_moves_tool_output_images_to_visual_context_message() -> Result<()> {
        let input = messages_to_responses_input_for_model(
            &[
                json!({
                    "role": "assistant",
                    "tool_calls": [{
                        "id": "call_1",
                        "name": "python",
                        "arguments": {"code": "screenshot('x')"}
                    }]
                }),
                json!({
                    "role": "tool",
                    "tool_call_id": "call_1",
                    "name": "python",
                    "content": [
                        {"type": "output_text", "text": "screenshot"},
                        {"type": "input_image", "image_url": "data:image/png;base64,abc", "detail": "auto"}
                    ]
                }),
            ],
            &image_capable_model_info(),
        )?;
        assert_eq!(input[0]["type"], "function_call");
        assert_eq!(input[1]["type"], "function_call_output");
        assert_eq!(input[1]["call_id"], "call_1");
        assert!(input[1]["output"]
            .as_str()
            .unwrap_or_default()
            .contains("following visual context message"));
        assert_eq!(input[2]["type"], "message");
        assert_eq!(input[2]["role"], "user");
        assert_eq!(input[2]["content"][0]["type"], "input_text");
        assert_eq!(input[2]["content"][1]["type"], "input_image");
        assert_eq!(
            input[2]["content"][1]["image_url"],
            "data:image/png;base64,abc"
        );
        assert_eq!(input[2]["content"][1]["detail"], "auto");
        Ok(())
    }

    #[test]
    fn responses_input_returns_view_image_as_function_call_output_content_like_codex() -> Result<()>
    {
        let input = messages_to_responses_input_for_model(
            &[
                json!({
                    "role": "assistant",
                    "tool_calls": [{
                        "id": "call_view",
                        "name": "view_image",
                        "arguments": {"path": "shot.png"}
                    }]
                }),
                json!({
                    "role": "tool",
                    "tool_call_id": "call_view",
                    "name": "view_image",
                    "content": [
                        {"type": "input_image", "image_url": "data:image/png;base64,abc", "detail": "high"}
                    ]
                }),
            ],
            &image_capable_model_info(),
        )?;
        assert_eq!(input.len(), 2);
        assert_eq!(input[1]["type"], "function_call_output");
        assert_eq!(input[1]["call_id"], "call_view");
        assert_eq!(input[1]["output"][0]["type"], "input_image");
        assert_eq!(input[1]["output"][0]["detail"], "high");
        Ok(())
    }

    #[test]
    fn responses_input_round_trips_custom_apply_patch_calls() -> Result<()> {
        let input = messages_to_responses_input(&[
            json!({
                "role": "assistant",
                "tool_calls": [{
                    "id": "call_patch",
                    "name": "apply_patch",
                    "arguments": "*** Begin Patch\n*** Add File: hello.txt\n+hello\n*** End Patch"
                }]
            }),
            json!({
                "role": "tool",
                "tool_call_id": "call_patch",
                "name": "apply_patch",
                "content": "Success. Updated the following files:\nA hello.txt\n"
            }),
        ])?;

        assert_eq!(input[0]["type"], "custom_tool_call");
        assert_eq!(input[0]["call_id"], "call_patch");
        assert_eq!(input[0]["name"], "apply_patch");
        assert!(input[0]["input"]
            .as_str()
            .unwrap_or_default()
            .starts_with("*** Begin Patch"));
        assert_eq!(input[1]["type"], "custom_tool_call_output");
        assert_eq!(input[1]["call_id"], "call_patch");
        assert_eq!(
            input[1]["output"],
            "Success. Updated the following files:\nA hello.txt\n"
        );
        assert!(input[1].get("success").is_none());
        Ok(())
    }

    #[test]
    fn responses_input_converts_orphan_tool_output_to_context_message() -> Result<()> {
        let input = messages_to_responses_input_for_model(
            &[
                json!({
                    "role": "system",
                    "content": "compacted context"
                }),
                json!({
                    "role": "tool",
                    "tool_call_id": "missing_call",
                    "name": "python",
                    "content": [
                        {"type": "output_text", "text": "screenshot"},
                        {"type": "input_image", "image_url": "data:image/png;base64,abc", "detail": "auto"}
                    ]
                }),
            ],
            &image_capable_model_info(),
        )?;
        assert_eq!(input.len(), 2);
        assert_eq!(input[1]["type"], "message");
        assert_eq!(input[1]["role"], "user");
        assert_eq!(input[1]["content"][0]["type"], "input_text");
        assert!(input[1]["content"][0]["text"]
            .as_str()
            .unwrap_or_default()
            .contains("Tool output retained as context"));
        assert_eq!(input[1]["content"][1]["type"], "input_image");
        assert!(!input.iter().any(|item| {
            item.get("type").and_then(Value::as_str) == Some("function_call_output")
                && item.get("call_id").and_then(Value::as_str) == Some("missing_call")
        }));
        Ok(())
    }

    #[test]
    fn responses_tools_encode_freeform_apply_patch_as_custom() {
        let tools = tool_specs_to_responses_tools(&[ToolSpec {
            name: "apply_patch".to_string(),
            namespace: None,
            namespace_description: None,
            description: "Use the `apply_patch` tool to edit files.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": { "patch": { "type": "string" } },
                "required": ["patch"],
                "additionalProperties": false
            }),
            output_schema: None,
            freeform: Some(browser_use_protocol::FreeformToolFormat {
                kind: "grammar".to_string(),
                syntax: "lark".to_string(),
                definition: "start: begin_patch hunk+ end_patch".to_string(),
            }),
        }]);

        assert_eq!(tools[0]["type"], "custom");
        assert_eq!(tools[0]["name"], "apply_patch");
        assert_eq!(tools[0]["format"]["type"], "grammar");
        assert_eq!(tools[0]["format"]["syntax"], "lark");
        assert!(tools[0].get("parameters").is_none());
    }

    #[test]
    fn chat_tools_downgrade_freeform_apply_patch_to_json_wrapper() {
        let tools = tool_specs_to_chat_tools(&[ToolSpec {
            name: "apply_patch".to_string(),
            namespace: None,
            namespace_description: None,
            description: "FREEFORM patch tool; do not wrap in JSON.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {"patch": {"type": "string"}},
                "required": ["patch"],
                "additionalProperties": false
            }),
            output_schema: None,
            freeform: Some(browser_use_protocol::FreeformToolFormat {
                kind: "grammar".to_string(),
                syntax: "lark".to_string(),
                definition: "start: begin_patch hunk+ end_patch".to_string(),
            }),
        }]);

        let function = &tools[0]["function"];
        assert_eq!(function["name"], "apply_patch");
        assert_eq!(function["parameters"]["required"][0], "patch");
        assert!(function["description"]
            .as_str()
            .unwrap_or_default()
            .contains("Put the raw lark payload in the `patch` string property"));
    }

    #[test]
    fn responses_tools_include_strict_false_for_function_tools_like_codex() {
        let tools = tool_specs_to_responses_tools(&[ToolSpec {
            name: "done".to_string(),
            namespace: None,
            namespace_description: None,
            description: "finish".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false,
            }),
            output_schema: Some(json!({
                "type": "object",
                "properties": {"ok": {"type": "boolean"}},
                "additionalProperties": false,
            })),
            freeform: None,
        }]);

        assert_eq!(tools[0]["type"], "function");
        assert_eq!(tools[0]["strict"], false);
        assert!(tools[0].get("output_schema").is_none());
    }

    #[test]
    fn responses_tools_encode_tool_search_like_codex() {
        let tools = tool_specs_to_responses_tools(&[ToolSpec {
            name: "tool_search".to_string(),
            namespace: None,
            namespace_description: None,
            description: "Search deferred tools.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "query": {"type": "string"}
                },
                "required": ["query"],
                "additionalProperties": false,
            }),
            output_schema: None,
            freeform: None,
        }]);

        assert_eq!(
            tools[0],
            json!({
                "type": "tool_search",
                "execution": "client",
                "description": "Search deferred tools.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "query": {"type": "string"}
                    },
                    "required": ["query"],
                    "additionalProperties": false,
                },
            })
        );
    }

    #[test]
    fn responses_tools_encode_hosted_web_search_and_image_generation_like_codex() {
        let hosted = vec![
            HostedToolSpec::web_search(
                WebSearchMode::Live,
                Some(&WebSearchToolConfig {
                    context_size: Some(WebSearchContextSize::High),
                    allowed_domains: Some(vec!["example.com".to_string()]),
                    location: Some(WebSearchLocation {
                        country: Some("US".to_string()),
                        region: Some("CA".to_string()),
                        city: Some("San Francisco".to_string()),
                        timezone: Some("America/Los_Angeles".to_string()),
                    }),
                }),
                WebSearchToolType::TextAndImage,
            )
            .expect("web search spec"),
            HostedToolSpec::image_generation_png(),
        ];
        let tools = tool_specs_to_responses_tools_with_hosted(&[], &hosted);

        assert_eq!(
            tools[0],
            json!({
                "type": "web_search",
                "external_web_access": true,
                "filters": {"allowed_domains": ["example.com"]},
                "user_location": {
                    "type": "approximate",
                    "country": "US",
                    "region": "CA",
                    "city": "San Francisco",
                    "timezone": "America/Los_Angeles",
                },
                "search_context_size": "high",
                "search_content_types": ["text", "image"],
            })
        );
        assert_eq!(
            tools[1],
            json!({
                "type": "image_generation",
                "output_format": "png",
            })
        );
    }

    #[test]
    fn responses_tools_coalesce_namespaced_function_tools_like_codex() {
        let tools = tool_specs_to_responses_tools(&[
            ToolSpec {
                name: "wait_agent".to_string(),
                namespace: Some("agents".to_string()),
                namespace_description: Some(
                    "Tools for spawning and managing sub-agents.".to_string(),
                ),
                description: "Wait for agents.".to_string(),
                input_schema: json!({
                    "type": "object",
                    "properties": {},
                    "additionalProperties": false,
                }),
                output_schema: None,
                freeform: None,
            },
            ToolSpec {
                name: "spawn_agent".to_string(),
                namespace: Some("agents".to_string()),
                namespace_description: Some(
                    "Tools for spawning and managing sub-agents.".to_string(),
                ),
                description: "Create a new agent.".to_string(),
                input_schema: json!({
                    "type": "object",
                    "properties": {},
                    "additionalProperties": false,
                }),
                output_schema: Some(json!({
                    "type": "object",
                    "properties": {},
                    "additionalProperties": false,
                })),
                freeform: None,
            },
            ToolSpec {
                name: "done".to_string(),
                namespace: None,
                namespace_description: None,
                description: "Finish.".to_string(),
                input_schema: json!({
                    "type": "object",
                    "properties": {},
                    "additionalProperties": false,
                }),
                output_schema: None,
                freeform: None,
            },
        ]);

        assert_eq!(tools.len(), 2);
        assert_eq!(tools[0]["type"], "namespace");
        assert_eq!(tools[0]["name"], "agents");
        assert_eq!(
            tools[0]["description"],
            "Tools for spawning and managing sub-agents."
        );
        let namespace_tools = tools[0]["tools"].as_array().expect("namespace tools");
        assert_eq!(namespace_tools.len(), 2);
        assert_eq!(namespace_tools[0]["type"], "function");
        assert_eq!(namespace_tools[0]["name"], "spawn_agent");
        assert_eq!(namespace_tools[0]["strict"], false);
        assert!(namespace_tools[0].get("output_schema").is_none());
        assert_eq!(namespace_tools[1]["type"], "function");
        assert_eq!(namespace_tools[1]["name"], "wait_agent");
        assert_eq!(tools[1]["type"], "function");
        assert_eq!(tools[1]["name"], "done");
    }

    #[test]
    fn responses_output_item_parses_tool_namespace_like_codex() -> Result<()> {
        let mut events = Vec::new();
        parse_response_output_item(
            &json!({
                "type": "function_call",
                "call_id": "call_spawn",
                "name": "spawn_agent",
                "namespace": "agents",
                "arguments": "{\"message\":\"inspect\"}",
            }),
            &mut events,
        )?;

        assert_eq!(
            events,
            vec![ModelEvent::ToolCall {
                call: ToolCall {
                    id: "call_spawn".to_string(),
                    name: "spawn_agent".to_string(),
                    namespace: Some("agents".to_string()),
                    arguments: json!({"message": "inspect"}),
                },
            }]
        );
        Ok(())
    }

    #[test]
    fn responses_input_converts_system_messages_to_user_context() -> Result<()> {
        let input = messages_to_responses_input(&[json!({
            "role": "system",
            "content": "compact summary"
        })])?;
        assert_eq!(input[0]["role"], "user");
        assert_eq!(input[0]["content"][0]["type"], "input_text");
        assert_eq!(
            input[0]["content"][0]["text"],
            "System context:\ncompact summary"
        );
        Ok(())
    }

    #[test]
    fn responses_input_preserves_developer_context_role() -> Result<()> {
        let input = messages_to_responses_input(&[json!({
            "role": "developer",
            "content": [{
                "type": "input_text",
                "text": "<permissions instructions>Approval policy is currently never.</permissions instructions>"
            }]
        })])?;
        assert_eq!(input[0]["role"], "developer");
        assert_eq!(input[0]["content"][0]["type"], "input_text");
        assert!(input[0]["content"][0]["text"]
            .as_str()
            .unwrap_or_default()
            .contains("Approval policy is currently never"));
        Ok(())
    }

    #[test]
    fn responses_input_passes_raw_response_items_through_like_codex() -> Result<()> {
        let input = messages_to_responses_input(&[
            json!({
                "type": "reasoning",
                "id": "rs_123",
                "summary": [{"type": "summary_text", "text": "checked context"}],
                "content": [{"type": "reasoning_text", "text": "hidden chain"}],
                "encrypted_content": "encrypted-reasoning"
            }),
            json!({
                "type": "web_search_call",
                "id": "ws_123",
                "status": "completed",
                "action": {"type": "search", "query": "Codex parity"}
            }),
            json!({
                "type": "image_generation_call",
                "id": "ig_123",
                "status": "completed",
                "result": "image-bytes"
            }),
            json!({
                "type": "compaction_trigger"
            }),
        ])?;

        assert_eq!(input.len(), 3);
        assert_eq!(input[0]["type"], "reasoning");
        assert!(input[0].get("id").is_none());
        assert!(input[0].get("content").is_none());
        assert_eq!(input[0]["encrypted_content"], "encrypted-reasoning");
        assert_eq!(input[1]["type"], "web_search_call");
        assert!(input[1].get("id").is_none());
        assert_eq!(input[1]["status"], "completed");
        assert_eq!(input[2]["type"], "image_generation_call");
        assert_eq!(input[2]["id"], "ig_123");
        Ok(())
    }

    #[test]
    fn responses_input_preserves_namespaced_assistant_tool_calls_like_codex() -> Result<()> {
        let input = messages_to_responses_input(&[json!({
            "role": "assistant",
            "content": "",
            "tool_calls": [
                {
                    "id": "call_spawn",
                    "name": "spawn_agent",
                    "namespace": "agents",
                    "arguments": {"message": "inspect"}
                },
                {
                    "id": "call_patch",
                    "name": "apply_patch",
                    "namespace": "tools",
                    "arguments": "*** Begin Patch\n*** End Patch"
                }
            ]
        })])?;

        let spawn = input
            .iter()
            .find(|item| item.get("call_id").and_then(Value::as_str) == Some("call_spawn"))
            .context("spawn call")?;
        assert_eq!(spawn["type"], "function_call");
        assert_eq!(spawn["namespace"], "agents");
        let patch = input
            .iter()
            .find(|item| item.get("call_id").and_then(Value::as_str) == Some("call_patch"))
            .context("patch call")?;
        assert_eq!(patch["type"], "custom_tool_call");
        assert_eq!(patch["namespace"], "tools");
        Ok(())
    }

    #[test]
    fn responses_input_synthesizes_missing_raw_call_outputs_like_codex() -> Result<()> {
        let input = messages_to_responses_input(&[
            json!({
                "type": "function_call",
                "call_id": "call_fn",
                "name": "exec_command",
                "arguments": "{\"cmd\":\"true\"}"
            }),
            json!({
                "type": "local_shell_call",
                "call_id": "call_shell",
                "status": "completed",
                "action": {"type": "exec", "command": "true"}
            }),
            json!({
                "type": "custom_tool_call",
                "call_id": "call_custom",
                "name": "apply_patch",
                "input": "*** Begin Patch\n*** End Patch"
            }),
            json!({
                "type": "tool_search_call",
                "call_id": "call_search",
                "status": "completed",
                "execution": "client",
                "arguments": {"query": "tools"}
            }),
        ])?;

        assert_eq!(input[1]["type"], "function_call_output");
        assert_eq!(input[1]["call_id"], "call_fn");
        assert_eq!(input[1]["output"], "aborted");
        assert_eq!(input[3]["type"], "function_call_output");
        assert_eq!(input[3]["call_id"], "call_shell");
        assert_eq!(input[3]["output"], "aborted");
        assert_eq!(input[5]["type"], "custom_tool_call_output");
        assert_eq!(input[5]["call_id"], "call_custom");
        assert_eq!(input[5]["output"], "aborted");
        assert_eq!(input[7]["type"], "tool_search_output");
        assert_eq!(input[7]["call_id"], "call_search");
        assert_eq!(input[7]["status"], "completed");
        assert_eq!(input[7]["execution"], "client");
        assert_eq!(input[7]["tools"], json!([]));
        Ok(())
    }

    #[test]
    fn responses_input_removes_orphan_raw_outputs_like_codex() -> Result<()> {
        let input = messages_to_responses_input(&[
            json!({
                "type": "function_call_output",
                "call_id": "orphan_fn",
                "output": "stale"
            }),
            json!({
                "type": "custom_tool_call_output",
                "call_id": "orphan_custom",
                "output": "stale"
            }),
            json!({
                "type": "tool_search_output",
                "call_id": "orphan_search",
                "status": "completed",
                "execution": "client",
                "tools": []
            }),
            json!({
                "type": "tool_search_output",
                "call_id": "server_search",
                "status": "completed",
                "execution": "server",
                "tools": [{"name": "search"}]
            }),
            json!({
                "type": "tool_search_output",
                "status": "completed",
                "execution": "client",
                "tools": [{"name": "no_call_id"}]
            }),
        ])?;

        assert_eq!(input.len(), 2);
        assert!(input.iter().any(|item| {
            item.get("type").and_then(Value::as_str) == Some("tool_search_output")
                && item.get("call_id").and_then(Value::as_str) == Some("server_search")
        }));
        assert!(input.iter().any(|item| {
            item.get("type").and_then(Value::as_str) == Some("tool_search_output")
                && item.get("call_id").is_none()
        }));
        assert!(!input.iter().any(|item| {
            item.get("call_id").and_then(Value::as_str) == Some("orphan_fn")
                || item.get("call_id").and_then(Value::as_str) == Some("orphan_custom")
                || item.get("call_id").and_then(Value::as_str) == Some("orphan_search")
        }));
        Ok(())
    }

    #[test]
    fn fallback_providers_skip_raw_response_items_they_cannot_express() -> Result<()> {
        let messages = [
            json!({
                "type": "reasoning",
                "summary": [],
                "encrypted_content": "encrypted-reasoning"
            }),
            json!({
                "role": "user",
                "content": "next prompt"
            }),
        ];

        let chat = messages_to_chat_messages(&messages, true)?;
        assert_eq!(chat.len(), 1);
        assert_eq!(chat[0]["role"], "user");
        assert_eq!(chat[0]["content"], "next prompt");

        let anthropic = messages_to_anthropic_messages(&messages, false)?;
        assert_eq!(anthropic.len(), 1);
        assert_eq!(anthropic[0]["role"], "user");
        assert_eq!(anthropic[0]["content"][0]["text"], "next prompt");
        Ok(())
    }

    #[test]
    fn anthropic_messages_drop_trailing_assistant_prefill() -> Result<()> {
        let messages = [
            json!({
                "role": "user",
                "content": "do the browser task"
            }),
            json!({
                "role": "assistant",
                "content": "premature final answer"
            }),
        ];

        let anthropic = messages_to_anthropic_messages(&messages, false)?;

        assert_eq!(anthropic.len(), 1);
        assert_eq!(anthropic[0]["role"], "user");
        assert_eq!(anthropic[0]["content"][0]["text"], "do the browser task");
        Ok(())
    }

    #[test]
    fn chat_messages_map_developer_context_to_system_priority() -> Result<()> {
        let messages = messages_to_chat_messages(
            &[json!({
                "role": "developer",
                "content": [{
                    "type": "input_text",
                    "text": "<permissions instructions>Do not provide sandbox permissions.</permissions instructions>"
                }]
            })],
            true,
        )?;

        assert_eq!(messages[0]["role"], "system");
        assert!(messages[0]["content"]
            .as_str()
            .unwrap_or_default()
            .contains("Developer context:\n<permissions instructions>"));
        Ok(())
    }

    #[test]
    fn anthropic_messages_move_developer_context_to_system_blocks() -> Result<()> {
        let developer = json!({
            "role": "developer",
            "content": [{
                "type": "input_text",
                "text": "<permissions instructions>Do not provide sandbox permissions.</permissions instructions>"
            }]
        });
        let system = anthropic_system_blocks_with_developer_context(
            "base instructions",
            &[developer.clone()],
            false,
        );
        let blocks = system.as_array().expect("system blocks");

        assert_eq!(blocks.len(), 2);
        assert!(blocks[1]["text"]
            .as_str()
            .unwrap_or_default()
            .contains("Developer context:\n<permissions instructions>"));
        assert!(messages_to_anthropic_messages(&[developer], false)?.is_empty());
        Ok(())
    }

    #[test]
    fn anthropic_messages_downsamples_oversized_tool_images() -> Result<()> {
        let image = image::RgbImage::new(10, PROVIDER_MAX_IMAGE_DIMENSION + 500);
        let mut png = Cursor::new(Vec::new());
        image::DynamicImage::ImageRgb8(image).write_to(&mut png, image::ImageFormat::Png)?;
        let image_url = format!(
            "data:image/png;base64,{}",
            general_purpose::STANDARD.encode(png.into_inner())
        );

        let messages = messages_to_anthropic_messages(
            &[
                json!({
                    "role": "assistant",
                    "tool_calls": [{
                        "id": "call_screenshot",
                        "name": "browser_script",
                        "arguments": {"code": "screenshot('tall')"}
                    }]
                }),
                json!({
                    "role": "tool",
                    "tool_call_id": "call_screenshot",
                    "name": "browser_script",
                    "content": [
                        {"type": "output_text", "text": "captured"},
                        {"type": "input_image", "image_url": image_url, "detail": "auto"}
                    ]
                }),
            ],
            false,
        )?;

        let visual_image = messages
            .iter()
            .flat_map(|message| message.get("content").and_then(Value::as_array).into_iter())
            .flatten()
            .find(|part| part.get("type").and_then(Value::as_str) == Some("image"))
            .expect("visual image block");
        let data = visual_image
            .pointer("/source/data")
            .and_then(Value::as_str)
            .expect("base64 image data");
        let bytes = general_purpose::STANDARD.decode(data)?;
        let resized = image::load_from_memory(&bytes)?;

        assert_eq!(
            visual_image
                .pointer("/source/media_type")
                .and_then(Value::as_str),
            Some("image/png")
        );
        assert!(resized.width() <= PROVIDER_MAX_IMAGE_DIMENSION);
        assert!(resized.height() <= PROVIDER_MAX_IMAGE_DIMENSION);
        assert_eq!(resized.height(), PROVIDER_MAX_IMAGE_DIMENSION);
        Ok(())
    }

    #[test]
    fn loads_codex_auth_file_with_nested_tokens() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let path = temp.path().join("auth.json");
        std::fs::write(
            &path,
            json!({
                "auth_mode": "chatgpt",
                "tokens": {
                    "id_token": "header.payload.signature",
                    "access_token": "access-123",
                    "account_id": "account-123",
                    "refresh_token": "refresh-123"
                },
                "last_refresh": "2026-05-25T00:00:00Z"
            })
            .to_string(),
        )?;
        assert_eq!(
            load_codex_auth_file(path)?,
            CodexAuth {
                access_token: "access-123".to_string(),
                account_id: "account-123".to_string(),
            }
        );
        let managed = load_codex_managed_auth_file(temp.path().join("auth.json"))?;
        let snapshot = managed.current_snapshot()?;
        assert_eq!(snapshot.access_token, "access-123");
        assert_eq!(snapshot.account_id, "account-123");
        assert_eq!(
            snapshot.id_token.as_deref(),
            Some("header.payload.signature")
        );
        assert_eq!(snapshot.refresh_token.as_deref(), Some("refresh-123"));
        let expected_path = temp.path().join("auth.json");
        assert_eq!(
            snapshot.source_path.as_deref(),
            Some(expected_path.as_path())
        );
        assert!(snapshot.last_refresh.is_some());
        Ok(())
    }

    #[test]
    fn codex_responses_provider_parses_sse_text_tool_call_and_usage() -> Result<()> {
        let sse = concat!(
            "data: {\"type\":\"response.reasoning_summary_text.delta\",\"delta\":\"Checking context.\"}\n\n",
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"Working\\n\"}\n\n",
            "data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"function_call\",\"call_id\":\"call_123\",\"name\":\"done\",\"arguments\":\"{\\\"result\\\":\\\"ok\\\"}\"}}\n\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_123\",\"end_turn\":false,\"output\":[],\"usage\":{\"input_tokens\":3,\"output_tokens\":4,\"total_tokens\":7}}}\n\n",
        );
        let (base_url, handle) = spawn_mock_server(sse.to_string(), "text/event-stream")?;
        let mut provider = CodexResponsesProvider::with_base_url(
            CodexAuth {
                access_token: "chatgpt-token".to_string(),
                account_id: "account-123".to_string(),
            },
            "gpt-test",
            format!("{}/backend-api", base_url.trim_end_matches("/v1")),
        );
        provider.request_retry = ProviderRequestRetryConfig::without_retries();
        let events = provider.start_turn(ProviderTurn {
            instructions: None,
            model_settings: ModelRequestSettings::default(),
            messages: vec![json!({"role": "user", "content": "finish"})],
            tools: vec![ToolSpec {
                name: "done".to_string(),
                namespace: None,
                namespace_description: None,
                description: "finish".to_string(),
                input_schema: json!({
                    "type": "object",
                    "properties": { "result": { "type": "string" } },
                    "required": ["result"],
                    "additionalProperties": false
                }),
                output_schema: None,
                freeform: None,
            }],
            ..ProviderTurn::default()
        })?;
        handle.join().expect("mock server thread");
        assert!(events.contains(&ModelEvent::ThinkingDelta {
            text: "Checking context.".to_string(),
            label: Some("reasoning summary".to_string()),
        }));
        assert!(events.contains(&ModelEvent::TextDelta {
            text: "Working\n".to_string(),
        }));
        assert!(events.contains(&ModelEvent::ToolCall {
            call: ToolCall {
                id: "call_123".to_string(),
                name: "done".to_string(),
                namespace: None,
                arguments: json!({"result": "ok"}),
            },
        }));
        assert!(events.contains(&ModelEvent::ResponseOutputItem {
            item: json!({
                "type": "function_call",
                "call_id": "call_123",
                "name": "done",
                "arguments": "{\"result\":\"ok\"}"
            }),
        }));
        assert!(events.contains(&ModelEvent::Usage {
            usage: ModelUsage {
                input_tokens: Some(3),
                output_tokens: Some(4),
                total_tokens: Some(7),
                cost_usd: None,
                ..Default::default()
            },
        }));
        assert!(events.contains(&ModelEvent::ResponseCompleted {
            response_id: Some("resp_123".to_string()),
            end_turn: Some(false),
        }));
        assert!(events.contains(&ModelEvent::Done));
        Ok(())
    }

    #[test]
    fn codex_responses_provider_parses_sse_without_content_type_like_codex_backend() -> Result<()> {
        let sse = concat!(
            "event: response.created\n",
            "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_no_content_type\",\"output\":[]}}\n\n",
            "event: response.output_text.delta\n",
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"Paris\"}\n\n",
            "event: response.completed\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_no_content_type\",\"output\":[],\"usage\":{\"input_tokens\":1,\"output_tokens\":1,\"total_tokens\":2}}}\n\n",
        );
        let (base_url, handle) = spawn_mock_status_sequence_server(vec![MockHttpResponse::new(
            200,
            "OK",
            sse,
            "text/event-stream",
        )
        .without_content_type()])?;
        let mut provider = CodexResponsesProvider::with_base_url(
            CodexAuth {
                access_token: "chatgpt-token".to_string(),
                account_id: "account-123".to_string(),
            },
            "gpt-test",
            format!("{}/backend-api", base_url.trim_end_matches("/v1")),
        );
        provider.request_retry = ProviderRequestRetryConfig::without_retries();

        let events = provider.start_turn(ProviderTurn {
            messages: vec![json!({"role": "user", "content": "finish"})],
            ..ProviderTurn::default()
        })?;
        handle.join().expect("mock server thread");

        assert!(events.contains(&ModelEvent::TextDelta {
            text: "Paris".to_string(),
        }));
        assert!(events.contains(&ModelEvent::Done));
        Ok(())
    }

    #[test]
    fn codex_responses_provider_ignores_response_processed_without_completing_turn() -> Result<()> {
        let sse = concat!(
            "data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"web_search_call\",\"id\":\"ws_1\",\"status\":\"completed\",\"action\":{\"type\":\"search\",\"query\":\"Codex\"}}}\n\n",
            "data: {\"type\":\"response.processed\",\"response_id\":\"resp_processed\"}\n\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_processed\",\"output\":[{\"type\":\"web_search_call\",\"id\":\"ws_1\",\"status\":\"completed\",\"action\":{\"type\":\"search\",\"query\":\"Codex\"}}],\"usage\":{\"input_tokens\":1,\"output_tokens\":1,\"total_tokens\":2}}}\n\n",
        );
        let (base_url, handle) = spawn_mock_server(sse.to_string(), "text/event-stream")?;
        let mut provider = CodexResponsesProvider::with_base_url(
            CodexAuth {
                access_token: "chatgpt-token".to_string(),
                account_id: "account-123".to_string(),
            },
            "gpt-test",
            format!("{}/backend-api", base_url.trim_end_matches("/v1")),
        );
        provider.request_retry = ProviderRequestRetryConfig::without_retries();

        let events = provider.start_turn(ProviderTurn {
            messages: vec![json!({"role": "user", "content": "search"})],
            ..ProviderTurn::default()
        })?;
        handle.join().expect("mock server thread");

        let raw_items = events
            .iter()
            .filter(|event| {
                matches!(
                    event,
                    ModelEvent::ResponseOutputItem { item }
                        if item.get("type").and_then(Value::as_str) == Some("web_search_call")
                            && item.get("id").and_then(Value::as_str) == Some("ws_1")
                )
            })
            .count();
        assert_eq!(raw_items, 1);
        assert!(events.contains(&ModelEvent::ResponseCompleted {
            response_id: Some("resp_processed".to_string()),
            end_turn: None,
        }));
        assert!(events.contains(&ModelEvent::Done));
        Ok(())
    }

    #[test]
    fn codex_responses_provider_response_processed_alone_is_not_completion() -> Result<()> {
        let sse = "data: {\"type\":\"response.processed\",\"response_id\":\"resp_processed\"}\n\n";
        let (base_url, handle) = spawn_mock_server(sse.to_string(), "text/event-stream")?;
        let mut provider = CodexResponsesProvider::with_base_url(
            CodexAuth {
                access_token: "chatgpt-token".to_string(),
                account_id: "account-123".to_string(),
            },
            "gpt-test",
            format!("{}/backend-api", base_url.trim_end_matches("/v1")),
        );
        provider.request_retry = ProviderRequestRetryConfig::without_retries();

        let err = provider
            .start_turn(ProviderTurn {
                messages: vec![json!({"role": "user", "content": "search"})],
                ..ProviderTurn::default()
            })
            .expect_err("response.processed alone should not complete the turn");
        handle.join().expect("mock server thread");
        assert!(format!("{err:#}").contains("stream closed before response.completed"));
        Ok(())
    }

    #[test]
    fn responses_output_item_parses_tool_search_call_like_codex() -> Result<()> {
        let mut events = Vec::new();
        parse_response_output_item(
            &json!({
                "type": "tool_search_call",
                "call_id": "search_1",
                "status": "completed",
                "execution": "client",
                "arguments": {
                    "query": "spawn subagent",
                    "limit": 3
                }
            }),
            &mut events,
        )?;

        assert_eq!(
            events,
            vec![ModelEvent::ToolCall {
                call: ToolCall {
                    id: "search_1".to_string(),
                    name: "tool_search".to_string(),
                    namespace: None,
                    arguments: json!({
                        "query": "spawn subagent",
                        "limit": 3
                    }),
                },
            }]
        );
        Ok(())
    }

    #[test]
    fn codex_managed_auth_reloads_same_account_after_401_like_codex() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let auth_path = temp.path().join("auth.json");
        write_codex_auth_file(
            &auth_path,
            "stale-chatgpt-token",
            "account-123",
            "refresh-token",
        )?;
        let managed_auth = load_codex_managed_auth_file(&auth_path)?;
        write_codex_auth_file(
            &auth_path,
            "reloaded-chatgpt-token",
            "account-123",
            "refresh-token",
        )?;
        let (base_url, headers_rx, handle) = spawn_request_header_capture_server_sequence(vec![
            MockHttpResponse::new(401, "Unauthorized", "unauthorized", "text/plain"),
            MockHttpResponse::new(
                200,
                "OK",
                codex_completed_sse("resp_reloaded"),
                "text/event-stream",
            ),
        ])?;
        let provider = CodexResponsesProvider::with_managed_base_url(
            managed_auth,
            "gpt-test",
            format!("{}/backend-api", base_url.trim_end_matches("/v1")),
        )?;

        provider.start_turn(ProviderTurn {
            messages: vec![json!({"role": "user", "content": "finish"})],
            ..ProviderTurn::default()
        })?;
        handle.join().expect("mock server thread");
        let first = headers_rx.recv().expect("first request headers");
        let second = headers_rx.recv().expect("second request headers");

        assert!(first
            .to_ascii_lowercase()
            .contains("\r\nauthorization: bearer stale-chatgpt-token\r\n"));
        assert!(second
            .to_ascii_lowercase()
            .contains("\r\nauthorization: bearer reloaded-chatgpt-token\r\n"));
        assert!(second
            .to_ascii_lowercase()
            .contains("\r\nchatgpt-account-id: account-123\r\n"));
        Ok(())
    }

    #[test]
    fn codex_managed_auth_refreshes_after_reload_nochange_401_like_codex() -> Result<()> {
        let _env_lock = ENV_LOCK.lock().expect("env lock");
        let temp = tempfile::tempdir()?;
        let auth_path = temp.path().join("auth.json");
        write_codex_auth_file(
            &auth_path,
            "stale-chatgpt-token",
            "account-123",
            "refresh-token",
        )?;
        let managed_auth = load_codex_managed_auth_file(&auth_path)?;
        let (base_url, headers_rx, handle) = spawn_request_header_capture_server_sequence(vec![
            MockHttpResponse::new(401, "Unauthorized", "unauthorized", "text/plain"),
            MockHttpResponse::new(401, "Unauthorized", "still unauthorized", "text/plain"),
            MockHttpResponse::new(
                200,
                "OK",
                codex_completed_sse("resp_refreshed"),
                "text/event-stream",
            ),
        ])?;
        let (refresh_url, refresh_rx, refresh_handle) = spawn_codex_refresh_capture_server(
            json!({
                "access_token": "fresh-chatgpt-token",
                "refresh_token": "fresh-refresh-token"
            })
            .to_string(),
            200,
        )?;
        let _refresh_guard =
            EnvVarGuard::set_str(CODEX_REFRESH_TOKEN_URL_OVERRIDE_ENV_VAR, &refresh_url);
        let provider = CodexResponsesProvider::with_managed_base_url(
            managed_auth,
            "gpt-test",
            format!("{}/backend-api", base_url.trim_end_matches("/v1")),
        )?;

        provider.start_turn(ProviderTurn {
            messages: vec![json!({"role": "user", "content": "finish"})],
            ..ProviderTurn::default()
        })?;
        handle.join().expect("mock server thread");
        refresh_handle.join().expect("refresh server thread");
        let refresh_request = refresh_rx.recv().expect("refresh request body");
        let first = headers_rx.recv().expect("first request headers");
        let second = headers_rx.recv().expect("second request headers");
        let third = headers_rx.recv().expect("third request headers");

        assert_eq!(refresh_request["client_id"], CODEX_OAUTH_CLIENT_ID);
        assert_eq!(refresh_request["grant_type"], "refresh_token");
        assert_eq!(refresh_request["refresh_token"], "refresh-token");
        assert!(first
            .to_ascii_lowercase()
            .contains("\r\nauthorization: bearer stale-chatgpt-token\r\n"));
        assert!(second
            .to_ascii_lowercase()
            .contains("\r\nauthorization: bearer stale-chatgpt-token\r\n"));
        assert!(third
            .to_ascii_lowercase()
            .contains("\r\nauthorization: bearer fresh-chatgpt-token\r\n"));
        let persisted: Value = serde_json::from_str(&std::fs::read_to_string(auth_path)?)?;
        assert_eq!(persisted["tokens"]["access_token"], "fresh-chatgpt-token");
        assert_eq!(persisted["tokens"]["refresh_token"], "fresh-refresh-token");
        assert!(persisted["last_refresh"].as_str().is_some());
        Ok(())
    }

    #[test]
    fn codex_managed_auth_account_mismatch_does_not_refresh_like_codex() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let auth_path = temp.path().join("auth.json");
        write_codex_auth_file(
            &auth_path,
            "stale-chatgpt-token",
            "account-123",
            "refresh-token",
        )?;
        let managed_auth = load_codex_managed_auth_file(&auth_path)?;
        write_codex_auth_file(
            &auth_path,
            "other-chatgpt-token",
            "other-account",
            "other-refresh-token",
        )?;
        let (base_url, handle) = spawn_timed_status_sequence_server(vec![
            MockHttpResponse::new(401, "Unauthorized", "unauthorized", "text/plain"),
            MockHttpResponse::new(
                200,
                "OK",
                codex_completed_sse("unexpected"),
                "text/event-stream",
            ),
        ])?;
        let provider = CodexResponsesProvider::with_managed_base_url(
            managed_auth,
            "gpt-test",
            format!("{}/backend-api", base_url.trim_end_matches("/v1")),
        )?;

        let err = provider
            .start_turn(ProviderTurn {
                messages: vec![json!({"role": "user", "content": "finish"})],
                ..ProviderTurn::default()
            })
            .expect_err("account mismatch should make 401 terminal");
        let served = handle.join().expect("mock server thread");
        let provider_error = err
            .downcast_ref::<ProviderError>()
            .expect("typed provider error");

        assert_eq!(served, 1);
        assert_eq!(provider_error.kind(), ProviderErrorKind::Unauthorized);
        Ok(())
    }

    #[test]
    fn codex_responses_provider_parses_sse_custom_apply_patch_call() -> Result<()> {
        let patch = "*** Begin Patch\\n*** Add File: hello.txt\\n+hello\\n*** End Patch";
        let sse = format!(
            "data: {{\"type\":\"response.output_item.done\",\"item\":{{\"type\":\"custom_tool_call\",\"call_id\":\"call_patch\",\"name\":\"apply_patch\",\"input\":\"{patch}\"}}}}\n\n\
             data: {{\"type\":\"response.completed\",\"response\":{{\"id\":\"resp_patch\",\"output\":[],\"usage\":{{\"input_tokens\":1,\"output_tokens\":1,\"total_tokens\":2}}}}}}\n\n"
        );
        let (base_url, handle) = spawn_mock_server(sse, "text/event-stream")?;
        let mut provider = CodexResponsesProvider::with_base_url(
            CodexAuth {
                access_token: "chatgpt-token".to_string(),
                account_id: "account-123".to_string(),
            },
            "gpt-test",
            format!("{}/backend-api", base_url.trim_end_matches("/v1")),
        );
        provider.request_retry = ProviderRequestRetryConfig::without_retries();
        let events = provider.start_turn(ProviderTurn {
            instructions: None,
            model_settings: ModelRequestSettings::default(),
            messages: vec![json!({"role": "user", "content": "patch"})],
            tools: Vec::new(),
            ..ProviderTurn::default()
        })?;
        handle.join().expect("mock server thread");
        assert!(events.contains(&ModelEvent::ToolCall {
            call: ToolCall {
                id: "call_patch".to_string(),
                name: "apply_patch".to_string(),
                namespace: None,
                arguments: Value::String(
                    "*** Begin Patch\n*** Add File: hello.txt\n+hello\n*** End Patch".to_string(),
                ),
            },
        }));
        Ok(())
    }

    #[test]
    fn codex_responses_provider_ignores_custom_input_delta_for_payload_and_dedupes() -> Result<()> {
        let patch = "*** Begin Patch\n*** Add File: streamed.txt\n+hello\n+world\n*** End Patch";
        let sse = [
            json!({
                "type": "response.output_item.added",
                "item": {
                    "type": "custom_tool_call",
                    "id": "ctc_1",
                    "call_id": "call_patch",
                    "name": "apply_patch",
                    "input": "",
                }
            }),
            json!({
                "type": "response.custom_tool_call_input.delta",
                "item_id": "ctc_1",
                "delta": "live progress only",
            }),
            json!({
                "type": "response.custom_tool_call_input.delta",
                "item_id": "ctc_1",
                "delta": " and not executable payload",
            }),
            json!({
                "type": "response.output_item.done",
                "item": {
                    "type": "custom_tool_call",
                    "id": "ctc_1",
                    "call_id": "call_patch",
                    "name": "apply_patch",
                    "input": patch,
                }
            }),
            json!({
                "type": "response.completed",
                "response": {
                    "id": "resp_patch",
                    "output": [{
                        "type": "custom_tool_call",
                        "id": "ctc_1",
                        "call_id": "call_patch",
                        "name": "apply_patch",
                        "input": patch,
                    }],
                    "usage": {
                        "input_tokens": 1,
                        "output_tokens": 1,
                        "total_tokens": 2,
                    }
                }
            }),
        ]
        .into_iter()
        .map(|event| format!("data: {event}\n\n"))
        .collect::<String>();
        let (base_url, handle) = spawn_mock_server(sse, "text/event-stream")?;
        let mut provider = CodexResponsesProvider::with_base_url(
            CodexAuth {
                access_token: "chatgpt-token".to_string(),
                account_id: "account-123".to_string(),
            },
            "gpt-test",
            format!("{}/backend-api", base_url.trim_end_matches("/v1")),
        );
        provider.request_retry = ProviderRequestRetryConfig::without_retries();
        let events = provider.start_turn(ProviderTurn {
            instructions: None,
            model_settings: ModelRequestSettings::default(),
            messages: vec![json!({"role": "user", "content": "patch"})],
            tools: Vec::new(),
            ..ProviderTurn::default()
        })?;
        handle.join().expect("mock server thread");

        let tool_calls = events
            .iter()
            .filter(|event| matches!(event, ModelEvent::ToolCall { .. }))
            .count();
        let response_output_items = events
            .iter()
            .filter(|event| matches!(event, ModelEvent::ResponseOutputItem { .. }))
            .count();
        let input_deltas = events
            .iter()
            .filter(|event| matches!(event, ModelEvent::CustomToolCallInputDelta { .. }))
            .count();
        assert_eq!(
            tool_calls, 1,
            "custom tool call repeated in response.completed should be deduped"
        );
        assert_eq!(
            response_output_items, 1,
            "raw response item repeated in response.completed should be deduped"
        );
        assert_eq!(
            input_deltas, 2,
            "custom input deltas should be exposed only as progress events"
        );
        assert!(events.contains(&ModelEvent::CustomToolCallInputDelta {
            call_id: "call_patch".to_string(),
            name: "apply_patch".to_string(),
            delta: "live progress only".to_string(),
        }));
        assert!(events.contains(&ModelEvent::ToolCall {
            call: ToolCall {
                id: "call_patch".to_string(),
                name: "apply_patch".to_string(),
                namespace: None,
                arguments: Value::String(patch.to_string()),
            },
        }));
        Ok(())
    }

    #[test]
    fn openai_responses_provider_parses_text_tool_call_and_usage() -> Result<()> {
        let (base_url, handle) = spawn_mock_server(
            json!({
                "id": "resp_123",
                "object": "response",
                "output": [
                    {
                        "type": "reasoning",
                        "summary": [
                            {
                                "type": "summary_text",
                                "text": "Checking whether a tool is needed."
                            }
                        ]
                    },
                    {
                        "type": "message",
                        "role": "assistant",
                        "content": [
                            {
                                "type": "output_text",
                                "text": "Need the browser.\n"
                            }
                        ]
                    },
                    {
                        "type": "function_call",
                        "call_id": "call_123",
                        "name": "done",
                        "arguments": "{\"result\":\"ok\"}",
                        "status": "completed"
                    }
                ],
                "usage": {
                    "input_tokens": 11,
                    "output_tokens": 7,
                    "total_tokens": 18
                }
            })
            .to_string(),
            "application/json",
        )?;
        let provider = OpenAIResponsesProvider::with_base_url("test-key", "gpt-5.5", base_url);
        let events = provider.start_turn(ProviderTurn {
            instructions: None,
            model_settings: ModelRequestSettings::default(),
            messages: vec![json!({
                "role": "user",
                "content": "finish"
            })],
            tools: vec![ToolSpec {
                name: "done".to_string(),
                namespace: None,
                namespace_description: None,
                description: "finish".to_string(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "result": { "type": "string" }
                    },
                    "required": ["result"],
                    "additionalProperties": false
                }),
                output_schema: None,
                freeform: None,
            }],
            ..ProviderTurn::default()
        })?;
        handle.join().expect("mock server thread");
        assert!(events.contains(&ModelEvent::ThinkingDelta {
            text: "Checking whether a tool is needed.".to_string(),
            label: Some("reasoning summary".to_string()),
        }));
        assert!(events.contains(&ModelEvent::TextDelta {
            text: "Need the browser.\n".to_string()
        }));
        assert!(events.contains(&ModelEvent::ToolCall {
            call: ToolCall {
                id: "call_123".to_string(),
                name: "done".to_string(),
                namespace: None,
                arguments: json!({"result": "ok"}),
            }
        }));
        assert!(events.contains(&ModelEvent::ResponseOutputItem {
            item: json!({
                "type": "function_call",
                "call_id": "call_123",
                "name": "done",
                "arguments": "{\"result\":\"ok\"}",
                "status": "completed"
            }),
        }));
        assert!(events.contains(&ModelEvent::Usage {
            usage: ModelUsage {
                input_tokens: Some(11),
                output_tokens: Some(7),
                total_tokens: Some(18),
                cost_usd: None,
                ..Default::default()
            }
        }));
        assert!(events.contains(&ModelEvent::ResponseCompleted {
            response_id: Some("resp_123".to_string()),
            end_turn: None,
        }));
        assert!(events.contains(&ModelEvent::Done));
        Ok(())
    }

    #[test]
    fn openai_compatible_chat_provider_parses_tool_call_and_usage() -> Result<()> {
        let (base_url, handle) = spawn_mock_server(
            json!({
                "id": "chatcmpl_123",
                "choices": [{
                    "message": {
                        "role": "assistant",
                        "content": "Need a tool.\n",
                        "tool_calls": [{
                            "id": "call_123",
                            "type": "function",
                            "function": {
                                "name": "done",
                                "arguments": "{\"result\":\"ok\"}"
                            }
                        }]
                    }
                }],
                "usage": {
                    "prompt_tokens": 5,
                    "completion_tokens": 6,
                    "total_tokens": 11,
                    "cost": 0.0123
                }
            })
            .to_string(),
            "application/json",
        )?;
        let provider =
            OpenAICompatibleChatProvider::with_base_url("test-key", "openrouter/test", base_url);
        let events = provider.start_turn(ProviderTurn {
            instructions: None,
            model_settings: ModelRequestSettings::default(),
            messages: vec![json!({"role": "user", "content": "finish"})],
            tools: vec![ToolSpec {
                name: "done".to_string(),
                namespace: None,
                namespace_description: None,
                description: "finish".to_string(),
                input_schema: json!({
                    "type": "object",
                    "properties": { "result": { "type": "string" } },
                    "required": ["result"],
                    "additionalProperties": false
                }),
                output_schema: None,
                freeform: None,
            }],
            ..ProviderTurn::default()
        })?;
        handle.join().expect("mock server thread");
        assert!(events.contains(&ModelEvent::TextDelta {
            text: "Need a tool.\n".to_string()
        }));
        assert!(events.contains(&ModelEvent::ToolCall {
            call: ToolCall {
                id: "call_123".to_string(),
                name: "done".to_string(),
                namespace: None,
                arguments: json!({"result": "ok"}),
            }
        }));
        assert!(events.contains(&ModelEvent::Usage {
            usage: ModelUsage {
                input_tokens: Some(5),
                output_tokens: Some(6),
                total_tokens: Some(11),
                cost_usd: Some(0.0123),
                cost_source: Some("native".to_string()),
                ..Default::default()
            }
        }));
        Ok(())
    }

    #[test]
    fn openai_compatible_chat_provider_wraps_raw_freeform_tool_arguments() -> Result<()> {
        let raw_patch = "*** Begin Patch\n*** Add File: hello.txt\n+hello\n*** End Patch";
        let (base_url, handle) = spawn_mock_server(
            json!({
                "id": "chatcmpl_freeform",
                "choices": [{
                    "message": {
                        "role": "assistant",
                        "tool_calls": [{
                            "id": "call_patch",
                            "type": "function",
                            "function": {
                                "name": "apply_patch",
                                "arguments": raw_patch
                            }
                        }]
                    }
                }]
            })
            .to_string(),
            "application/json",
        )?;
        let provider =
            OpenAICompatibleChatProvider::with_base_url("test-key", "openrouter/test", base_url);
        let events = provider.start_turn(ProviderTurn {
            messages: vec![json!({"role": "user", "content": "patch"})],
            tools: vec![ToolSpec {
                name: "apply_patch".to_string(),
                namespace: None,
                namespace_description: None,
                description: "Use a patch.".to_string(),
                input_schema: json!({
                    "type": "object",
                    "properties": {"patch": {"type": "string"}},
                    "required": ["patch"],
                    "additionalProperties": false
                }),
                output_schema: None,
                freeform: Some(browser_use_protocol::FreeformToolFormat {
                    kind: "grammar".to_string(),
                    syntax: "lark".to_string(),
                    definition: "start: begin_patch hunk+ end_patch".to_string(),
                }),
            }],
            ..ProviderTurn::default()
        })?;
        handle.join().expect("mock server thread");
        assert!(events.contains(&ModelEvent::ToolCall {
            call: ToolCall {
                id: "call_patch".to_string(),
                name: "apply_patch".to_string(),
                namespace: None,
                arguments: json!({"patch": raw_patch}),
            }
        }));
        Ok(())
    }

    #[test]
    fn chat_parser_surfaces_deepseek_reasoning_content() -> Result<()> {
        let events = parse_chat_completion_output(
            &json!({
                "choices": [{
                    "message": {
                        "role": "assistant",
                        "reasoning_content": "I need a tool.",
                        "content": "",
                        "tool_calls": [{
                            "id": "call_123",
                            "type": "function",
                            "function": {
                                "name": "done",
                                "arguments": "{\"result\":\"ok\"}"
                            }
                        }]
                    }
                }]
            }),
            "deepseek-v4-pro",
            &[],
        )?;

        assert!(events.contains(&ModelEvent::ThinkingDelta {
            text: "I need a tool.".to_string(),
            label: Some("reasoning".to_string()),
        }));
        assert!(events.contains(&ModelEvent::ToolCall {
            call: ToolCall {
                id: "call_123".to_string(),
                name: "done".to_string(),
                namespace: None,
                arguments: json!({"result": "ok"}),
            }
        }));
        Ok(())
    }

    #[test]
    fn chat_messages_preserve_deepseek_reasoning_content() -> Result<()> {
        let messages = messages_to_chat_messages(
            &[json!({
                "role": "assistant",
                "content": "",
                "reasoning_content": "Need page state.",
                "tool_calls": [{
                    "id": "call_123",
                    "name": "python",
                    "arguments": {"code": "print('ok')"},
                }],
            })],
            true,
        )?;

        assert_eq!(messages[0]["role"], "assistant");
        assert_eq!(messages[0]["reasoning_content"], "Need page state.");
        assert_eq!(messages[0]["tool_calls"][0]["id"], "call_123");
        Ok(())
    }

    #[test]
    fn deepseek_v4_model_ids_disable_chat_image_content() {
        let direct = OpenAICompatibleChatProvider::with_base_url(
            "test-key",
            "deepseek-v4-pro",
            "https://example.test",
        );
        let openrouter = OpenAICompatibleChatProvider::with_base_url(
            "test-key",
            "deepseek/deepseek-v4-flash",
            "https://openrouter.ai/api/v1",
        );
        let non_deepseek = OpenAICompatibleChatProvider::with_base_url(
            "test-key",
            "openai/gpt-4o",
            "https://example.test",
        );

        assert!(!direct.include_image_content);
        assert!(!openrouter.include_image_content);
        assert!(non_deepseek.include_image_content);
    }

    #[test]
    fn direct_deepseek_provider_disables_chat_image_content_for_any_model() {
        let provider = OpenAICompatibleChatProvider::deepseek(
            "test-key",
            "deepseek-chat",
            "https://api.deepseek.com",
        );

        assert!(!provider.include_image_content);
    }

    #[test]
    fn chat_messages_can_omit_user_image_content() -> Result<()> {
        let messages = messages_to_chat_messages(
            &[json!({
                "role": "user",
                "content": [
                    { "type": "input_text", "text": "What is shown?" },
                    { "type": "input_image", "image_url": "data:image/png;base64,abc" }
                ],
            })],
            false,
        )?;
        let serialized = serde_json::to_string(&messages)?;

        assert!(!serialized.contains("image_url"));
        assert_eq!(messages[0]["content"][0]["text"], "What is shown?");
        assert_eq!(
            messages[0]["content"][1]["text"],
            "[image omitted: selected model endpoint does not accept image content]"
        );
        Ok(())
    }

    #[test]
    fn chat_messages_can_omit_tool_image_context() -> Result<()> {
        let messages = messages_to_chat_messages(
            &[json!({
                "role": "tool",
                "tool_call_id": "call_123",
                "name": "browser",
                "content": [
                    { "type": "output_text", "text": "browser connected" },
                    { "type": "input_image", "image_url": "data:image/png;base64,abc" }
                ],
            })],
            false,
        )?;
        let serialized = serde_json::to_string(&messages)?;

        assert_eq!(messages.len(), 2);
        assert!(!serialized.contains("image_url"));
        assert!(messages[0]["content"]
            .as_str()
            .unwrap_or_default()
            .contains("omitted because this model endpoint does not accept image content"));
        assert!(messages[1]["content"]
            .as_str()
            .unwrap_or_default()
            .contains("Visual output from tool call call_123 (browser) was omitted"));
        Ok(())
    }

    #[test]
    fn chat_messages_keep_tool_image_context_when_supported() -> Result<()> {
        let messages = messages_to_chat_messages(
            &[json!({
                "role": "tool",
                "tool_call_id": "call_123",
                "name": "browser",
                "content": [
                    { "type": "output_text", "text": "browser connected" },
                    { "type": "input_image", "image_url": "data:image/png;base64,abc" }
                ],
            })],
            true,
        )?;
        let serialized = serde_json::to_string(&messages)?;

        assert_eq!(messages.len(), 2);
        assert!(serialized.contains("image_url"));
        assert!(messages[0]["content"]
            .as_str()
            .unwrap_or_default()
            .contains("attached in the following visual context message"));
        Ok(())
    }

    #[test]
    fn openai_compatible_chat_provider_streams_text_tool_calls_and_usage() -> Result<()> {
        let sse = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"Working.\\n\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_done\",\"type\":\"function\",\"function\":{\"name\":\"done\",\"arguments\":\"{\\\"result\\\"\"}}]}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\":\\\"ok\\\"}\"}}]}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}],\"usage\":{\"prompt_tokens\":5,\"completion_tokens\":6,\"total_tokens\":11}}\n\n",
            "data: [DONE]\n\n",
        );
        let (base_url, handle) = spawn_mock_server(sse.to_string(), "text/event-stream")?;
        let provider =
            OpenAICompatibleChatProvider::with_base_url("test-key", "openrouter/test", base_url);
        let mut events = Vec::new();
        provider.stream_turn(
            ProviderTurn {
                messages: vec![json!({"role": "user", "content": "finish"})],
                tools: vec![ToolSpec {
                    name: "done".to_string(),
                    namespace: None,
                    namespace_description: None,
                    description: "finish".to_string(),
                    input_schema: json!({
                        "type": "object",
                        "properties": { "result": { "type": "string" } },
                        "required": ["result"],
                        "additionalProperties": false
                    }),
                    output_schema: None,
                    freeform: None,
                }],
                ..ProviderTurn::default()
            },
            &mut |event| {
                events.push(event);
                Ok(())
            },
        )?;
        handle.join().expect("mock server thread");
        assert!(events.contains(&ModelEvent::TextDelta {
            text: "Working.\n".to_string()
        }));
        assert!(events.contains(&ModelEvent::ToolCall {
            call: ToolCall {
                id: "call_done".to_string(),
                name: "done".to_string(),
                namespace: None,
                arguments: json!({"result": "ok"}),
            }
        }));
        assert!(events.contains(&ModelEvent::Usage {
            usage: ModelUsage {
                input_tokens: Some(5),
                output_tokens: Some(6),
                total_tokens: Some(11),
                cost_usd: None,
                ..Default::default()
            }
        }));
        assert!(matches!(events.last(), Some(ModelEvent::Done)));
        Ok(())
    }

    #[test]
    fn anthropic_messages_provider_parses_text_tool_use_and_usage() -> Result<()> {
        let (base_url, handle) = spawn_mock_server(
            json!({
                "id": "msg_123",
                "type": "message",
                "role": "assistant",
                "content": [
                    { "type": "text", "text": "Working.\n" },
                    {
                        "type": "tool_use",
                        "id": "toolu_123",
                        "name": "done",
                        "input": { "result": "ok" }
                    }
                ],
                "usage": {
                    "input_tokens": 7,
                    "output_tokens": 8
                }
            })
            .to_string(),
            "application/json",
        )?;
        let provider =
            AnthropicMessagesProvider::with_base_url("anthropic-key", "claude-test", base_url);
        let events = provider.start_turn(ProviderTurn {
            instructions: None,
            model_settings: ModelRequestSettings::default(),
            messages: vec![json!({"role": "user", "content": "finish"})],
            tools: vec![ToolSpec {
                name: "done".to_string(),
                namespace: None,
                namespace_description: None,
                description: "finish".to_string(),
                input_schema: json!({
                    "type": "object",
                    "properties": { "result": { "type": "string" } },
                    "required": ["result"],
                    "additionalProperties": false
                }),
                output_schema: None,
                freeform: None,
            }],
            ..ProviderTurn::default()
        })?;
        handle.join().expect("mock server thread");
        assert!(events.contains(&ModelEvent::TextDelta {
            text: "Working.\n".to_string()
        }));
        assert!(events.contains(&ModelEvent::ToolCall {
            call: ToolCall {
                id: "toolu_123".to_string(),
                name: "done".to_string(),
                namespace: None,
                arguments: json!({"result": "ok"}),
            }
        }));
        assert!(events.contains(&ModelEvent::Usage {
            usage: ModelUsage {
                input_tokens: Some(7),
                output_tokens: Some(8),
                total_tokens: Some(15),
                cost_usd: None,
                ..Default::default()
            }
        }));
        Ok(())
    }

    #[test]
    fn anthropic_messages_provider_streams_text_tool_use_and_usage() -> Result<()> {
        let sse = concat!(
            "event: message_start\n",
            "data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":7,\"output_tokens\":1}}}\n\n",
            "event: content_block_start\n",
            "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Working.\\n\"}}\n\n",
            "event: content_block_start\n",
            "data: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_123\",\"name\":\"done\",\"input\":{}}}\n\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"result\\\":\\\"ok\\\"}\"}}\n\n",
            "event: content_block_stop\n",
            "data: {\"type\":\"content_block_stop\",\"index\":1}\n\n",
            "event: message_delta\n",
            "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"},\"usage\":{\"output_tokens\":8}}\n\n",
            "event: message_stop\n",
            "data: {\"type\":\"message_stop\"}\n\n",
        );
        let (base_url, handle) = spawn_mock_server(sse.to_string(), "text/event-stream")?;
        let provider =
            AnthropicMessagesProvider::with_base_url("anthropic-key", "claude-test", base_url);
        let mut events = Vec::new();
        provider.stream_turn(
            ProviderTurn {
                instructions: None,
                model_settings: ModelRequestSettings::default(),
                messages: vec![json!({"role": "user", "content": "finish"})],
                tools: vec![ToolSpec {
                    name: "done".to_string(),
                    namespace: None,
                    namespace_description: None,
                    description: "finish".to_string(),
                    input_schema: json!({
                        "type": "object",
                        "properties": { "result": { "type": "string" } },
                        "required": ["result"],
                        "additionalProperties": false
                    }),
                    output_schema: None,
                    freeform: None,
                }],
                ..ProviderTurn::default()
            },
            &mut |event| {
                events.push(event);
                Ok(())
            },
        )?;
        handle.join().expect("mock server thread");
        assert!(events.contains(&ModelEvent::TextDelta {
            text: "Working.\n".to_string()
        }));
        assert!(events.contains(&ModelEvent::ToolCall {
            call: ToolCall {
                id: "toolu_123".to_string(),
                name: "done".to_string(),
                namespace: None,
                arguments: json!({"result": "ok"}),
            }
        }));
        assert!(events.contains(&ModelEvent::Usage {
            usage: ModelUsage {
                input_tokens: Some(7),
                output_tokens: Some(8),
                total_tokens: Some(15),
                cost_usd: None,
                ..Default::default()
            }
        }));
        assert!(matches!(events.last(), Some(ModelEvent::Done)));
        Ok(())
    }

    #[test]
    fn anthropic_messages_request_marks_last_tool_cacheable() -> Result<()> {
        let provider = AnthropicMessagesProvider::new("anthropic-key", "claude-test");
        let body = provider.messages_request_body(
            &ProviderTurn {
                instructions: Some("Stable system prompt".to_string()),
                messages: vec![json!({"role": "user", "content": "finish"})],
                tools: vec![
                    ToolSpec {
                        name: "browser".to_string(),
                        namespace: None,
                        namespace_description: None,
                        description: "Inspect a page".to_string(),
                        input_schema: json!({"type": "object"}),
                        output_schema: None,
                        freeform: None,
                    },
                    ToolSpec {
                        name: "done".to_string(),
                        namespace: None,
                        namespace_description: None,
                        description: "Finish the task".to_string(),
                        input_schema: json!({"type": "object"}),
                        output_schema: None,
                        freeform: None,
                    },
                ],
                ..ProviderTurn::default()
            },
            false,
            true,
        )?;

        assert!(body["tools"][0].get("cache_control").is_none());
        assert_eq!(body["cache_control"], json!({"type": "ephemeral"}));
        assert_eq!(
            body["tools"][1]["cache_control"],
            json!({"type": "ephemeral"})
        );
        assert_eq!(
            body["system"][0]["cache_control"],
            json!({"type": "ephemeral"})
        );
        Ok(())
    }

    #[test]
    fn openai_compatible_chat_retries_5xx_inside_provider_like_codex_request_layer() -> Result<()> {
        let success = json!({
            "id": "chatcmpl_retry",
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": "Recovered.\n"
                }
            }],
            "usage": {
                "prompt_tokens": 1,
                "completion_tokens": 2,
                "total_tokens": 3
            }
        })
        .to_string();
        let (base_url, handle) = spawn_mock_status_sequence_server(vec![
            MockHttpResponse::new(502, "Bad Gateway", "temporary gateway", "text/plain"),
            MockHttpResponse::new(200, "OK", success, "application/json"),
        ])?;
        let mut provider =
            OpenAICompatibleChatProvider::with_base_url("test-key", "openrouter/test", base_url);
        provider.request_retry = ProviderRequestRetryConfig::without_delay();

        let events = provider.start_turn(ProviderTurn {
            messages: vec![json!({"role": "user", "content": "finish"})],
            ..ProviderTurn::default()
        })?;
        handle.join().expect("mock server thread");

        assert!(events.contains(&ModelEvent::TextDelta {
            text: "Recovered.\n".to_string()
        }));
        Ok(())
    }

    #[test]
    fn openai_compatible_chat_turn_request_max_retries_zero_disables_hidden_retry_like_codex_config(
    ) -> Result<()> {
        let success = json!({
            "id": "chatcmpl_no_retry",
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": "Should not be reached.\n"
                }
            }]
        })
        .to_string();
        let (base_url, handle) = spawn_timed_status_sequence_server(vec![
            MockHttpResponse::new(
                500,
                "Internal Server Error",
                "temporary failure",
                "text/plain",
            ),
            MockHttpResponse::new(200, "OK", success, "application/json"),
        ])?;
        let provider =
            OpenAICompatibleChatProvider::with_base_url("test-key", "openrouter/test", base_url);

        let err = provider
            .start_turn(ProviderTurn {
                request_max_retries: Some(0),
                messages: vec![json!({"role": "user", "content": "finish"})],
                ..ProviderTurn::default()
            })
            .expect_err("request_max_retries=0 should disable hidden retry");
        let served = handle.join().expect("mock server thread");
        let provider_error = err
            .downcast_ref::<ProviderError>()
            .expect("typed provider error");

        assert_eq!(served, 1);
        assert_eq!(
            provider_error.kind(),
            ProviderErrorKind::InternalServerError
        );
        Ok(())
    }

    #[test]
    fn openai_compatible_chat_turn_request_max_retries_one_serves_two_attempts_like_codex_config(
    ) -> Result<()> {
        let (base_url, handle) = spawn_timed_status_sequence_server(vec![
            MockHttpResponse::new(
                500,
                "Internal Server Error",
                "temporary failure one",
                "text/plain",
            ),
            MockHttpResponse::new(
                500,
                "Internal Server Error",
                "temporary failure two",
                "text/plain",
            ),
        ])?;
        let mut provider =
            OpenAICompatibleChatProvider::with_base_url("test-key", "openrouter/test", base_url);
        provider.request_retry = ProviderRequestRetryConfig::without_delay();

        let err = provider
            .start_turn(ProviderTurn {
                request_max_retries: Some(1),
                messages: vec![json!({"role": "user", "content": "finish"})],
                ..ProviderTurn::default()
            })
            .expect_err("request_max_retries=1 should allow exactly one hidden retry");
        let served = handle.join().expect("mock server thread");
        let provider_error = err
            .downcast_ref::<ProviderError>()
            .expect("typed provider error");

        assert_eq!(served, 2);
        assert_eq!(
            provider_error.kind(),
            ProviderErrorKind::InternalServerError
        );
        Ok(())
    }

    #[test]
    fn openai_compatible_chat_turn_request_max_retries_caps_at_100_like_codex_config() -> Result<()>
    {
        let responses = (0..101)
            .map(|_| {
                MockHttpResponse::new(
                    500,
                    "Internal Server Error",
                    "temporary failure",
                    "text/plain",
                )
            })
            .collect();
        let (base_url, handle) = spawn_timed_status_sequence_server(responses)?;
        let mut provider =
            OpenAICompatibleChatProvider::with_base_url("test-key", "openrouter/test", base_url);
        provider.request_retry = ProviderRequestRetryConfig::without_delay();

        let err = provider
            .start_turn(ProviderTurn {
                request_max_retries: Some(150),
                messages: vec![json!({"role": "user", "content": "finish"})],
                ..ProviderTurn::default()
            })
            .expect_err("request_max_retries above cap should exhaust at Codex cap");
        let served = handle.join().expect("mock server thread");
        let provider_error = err
            .downcast_ref::<ProviderError>()
            .expect("typed provider error");

        assert_eq!(served, 101);
        assert_eq!(
            provider_error.kind(),
            ProviderErrorKind::InternalServerError
        );
        Ok(())
    }

    #[test]
    fn hidden_request_retry_does_not_retry_reqwest_builder_errors_like_codex() {
        let error = reqwest::blocking::Client::new()
            .get("http://[::1")
            .send()
            .expect_err("invalid URL should produce a builder error");

        assert!(error.is_builder(), "{error}");
        assert!(!ProviderRequestRetryConfig::default().should_retry_send_error(&error, 0));
    }

    #[test]
    fn anthropic_messages_retries_5xx_inside_provider_like_codex_request_layer() -> Result<()> {
        let success = json!({
            "id": "msg_retry",
            "type": "message",
            "role": "assistant",
            "content": [{ "type": "text", "text": "Recovered." }],
            "usage": {
                "input_tokens": 1,
                "output_tokens": 2
            }
        })
        .to_string();
        let (base_url, handle) = spawn_mock_status_sequence_server(vec![
            MockHttpResponse::new(
                500,
                "Internal Server Error",
                "temporary failure",
                "text/plain",
            ),
            MockHttpResponse::new(200, "OK", success, "application/json"),
        ])?;
        let mut provider =
            AnthropicMessagesProvider::with_base_url("anthropic-key", "claude-test", base_url);
        provider.request_retry = ProviderRequestRetryConfig::without_delay();

        let events = provider.start_turn(ProviderTurn {
            messages: vec![json!({"role": "user", "content": "finish"})],
            ..ProviderTurn::default()
        })?;
        handle.join().expect("mock server thread");

        assert!(events.contains(&ModelEvent::TextDelta {
            text: "Recovered.".to_string()
        }));
        Ok(())
    }

    #[test]
    fn openai_compatible_chat_retries_body_read_error_like_codex_request_layer() -> Result<()> {
        let success = json!({
            "id": "chatcmpl_body_retry",
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": "Recovered after body read.\n"
                }
            }],
            "usage": {
                "prompt_tokens": 1,
                "completion_tokens": 2,
                "total_tokens": 3
            }
        })
        .to_string();
        let (base_url, handle) = spawn_incomplete_body_then_status_server(MockHttpResponse::new(
            200,
            "OK",
            success,
            "application/json",
        ))?;
        let mut provider =
            OpenAICompatibleChatProvider::with_base_url("test-key", "openrouter/test", base_url);
        provider.request_retry = ProviderRequestRetryConfig::without_delay();

        let events = provider.start_turn(ProviderTurn {
            messages: vec![json!({"role": "user", "content": "finish"})],
            ..ProviderTurn::default()
        })?;
        handle.join().expect("mock server thread");

        assert!(events.contains(&ModelEvent::TextDelta {
            text: "Recovered after body read.\n".to_string()
        }));
        Ok(())
    }

    #[test]
    fn anthropic_messages_retries_body_read_error_like_codex_request_layer() -> Result<()> {
        let success = json!({
            "id": "msg_body_retry",
            "type": "message",
            "role": "assistant",
            "content": [{ "type": "text", "text": "Recovered after body read." }],
            "usage": {
                "input_tokens": 1,
                "output_tokens": 2
            }
        })
        .to_string();
        let (base_url, handle) = spawn_incomplete_body_then_status_server(MockHttpResponse::new(
            200,
            "OK",
            success,
            "application/json",
        ))?;
        let mut provider =
            AnthropicMessagesProvider::with_base_url("anthropic-key", "claude-test", base_url);
        provider.request_retry = ProviderRequestRetryConfig::without_delay();

        let events = provider.start_turn(ProviderTurn {
            messages: vec![json!({"role": "user", "content": "finish"})],
            ..ProviderTurn::default()
        })?;
        handle.join().expect("mock server thread");

        assert!(events.contains(&ModelEvent::TextDelta {
            text: "Recovered after body read.".to_string()
        }));
        Ok(())
    }

    #[test]
    fn anthropic_messages_provider_accepts_oauth_auth_token() -> Result<()> {
        let (base_url, handle) = spawn_mock_server(
            json!({
                "id": "msg_123",
                "type": "message",
                "role": "assistant",
                "content": [
                    { "type": "text", "text": "OAuth ok." },
                    {
                        "type": "tool_use",
                        "id": "toolu_bash",
                        "name": "Bash",
                        "input": { "cmd": "pwd" }
                    }
                ],
                "usage": { "input_tokens": 1, "output_tokens": 2 }
            })
            .to_string(),
            "application/json",
        )?;
        let provider = AnthropicMessagesProvider::with_auth_token(
            "claude-oauth-token",
            "claude-test",
            base_url,
        );
        let events = provider.start_turn(ProviderTurn {
            instructions: None,
            model_settings: ModelRequestSettings::default(),
            messages: vec![json!({"role": "user", "content": "finish"})],
            tools: vec![ToolSpec {
                name: "shell".to_string(),
                namespace: None,
                namespace_description: None,
                description: "run shell".to_string(),
                input_schema: json!({
                    "type": "object",
                    "properties": { "cmd": { "type": "string" } },
                    "required": ["cmd"],
                    "additionalProperties": false
                }),
                output_schema: None,
                freeform: None,
            }],
            ..ProviderTurn::default()
        })?;
        handle.join().expect("mock server thread");
        assert!(events.contains(&ModelEvent::TextDelta {
            text: "OAuth ok.".to_string()
        }));
        assert!(events.contains(&ModelEvent::ToolCall {
            call: ToolCall {
                id: "toolu_bash".to_string(),
                name: "shell".to_string(),
                namespace: None,
                arguments: json!({"cmd": "pwd"}),
            }
        }));
        assert!(events.contains(&ModelEvent::Usage {
            usage: ModelUsage {
                input_tokens: Some(1),
                output_tokens: Some(2),
                total_tokens: Some(3),
                cost_usd: None,
                ..Default::default()
            }
        }));
        Ok(())
    }

    #[test]
    fn anthropic_oauth_refreshes_once_after_401_like_codex_auth_recovery() -> Result<()> {
        let success = json!({
            "id": "msg_refresh",
            "type": "message",
            "role": "assistant",
            "content": [{ "type": "text", "text": "Recovered." }],
            "usage": { "input_tokens": 1, "output_tokens": 2 }
        })
        .to_string();
        let (base_url, headers_rx, handle) = spawn_request_header_capture_server_sequence(vec![
            MockHttpResponse::new(
                401,
                "Unauthorized",
                r#"{"error":{"message":"stale oauth token"}}"#,
                "application/json",
            ),
            MockHttpResponse::new(200, "OK", success, "application/json"),
        ])?;
        let refresh_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let refresh_count_for_fn = refresh_count.clone();
        let provider = AnthropicMessagesProvider::with_claude_code_oauth_refresh_for_test(
            ClaudeCodeOAuthCredential {
                access_token: "stale-claude-token".to_string(),
                refresh_token: "refresh-token".to_string(),
                expires_ms: 9_999_999_999_999,
            },
            "claude-test",
            base_url,
            move |refresh_token| {
                assert_eq!(refresh_token, "refresh-token");
                refresh_count_for_fn.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(ClaudeCodeOAuthCredential {
                    access_token: "fresh-claude-token".to_string(),
                    refresh_token: "fresh-refresh-token".to_string(),
                    expires_ms: 9_999_999_999_999,
                })
            },
        );

        let events = provider.start_turn(ProviderTurn {
            messages: vec![json!({"role": "user", "content": "finish"})],
            ..ProviderTurn::default()
        })?;
        handle.join().expect("mock server thread");
        let first = headers_rx.recv().expect("first request headers");
        let second = headers_rx.recv().expect("second request headers");

        assert_eq!(refresh_count.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert!(first
            .to_ascii_lowercase()
            .contains("\r\nauthorization: bearer stale-claude-token\r\n"));
        assert!(second
            .to_ascii_lowercase()
            .contains("\r\nauthorization: bearer fresh-claude-token\r\n"));
        assert!(events.contains(&ModelEvent::TextDelta {
            text: "Recovered.".to_string()
        }));
        Ok(())
    }

    #[test]
    fn anthropic_static_oauth_401_stays_terminal_like_codex_without_recovery() -> Result<()> {
        let (base_url, handle) = spawn_timed_status_sequence_server(vec![
            MockHttpResponse::new(
                401,
                "Unauthorized",
                r#"{"error":{"message":"stale static token"}}"#,
                "application/json",
            ),
            MockHttpResponse::new(
                200,
                "OK",
                r#"{"content":[{"type":"text","text":"unexpected"}],"usage":{"input_tokens":1,"output_tokens":1}}"#,
                "application/json",
            ),
        ])?;
        let provider = AnthropicMessagesProvider::with_auth_token(
            "claude-oauth-token",
            "claude-test",
            base_url,
        );

        let err = provider
            .start_turn(ProviderTurn {
                messages: vec![json!({"role": "user", "content": "finish"})],
                ..ProviderTurn::default()
            })
            .expect_err("static OAuth 401 should be terminal");
        let served = handle.join().expect("mock server thread");
        let provider_error = err
            .downcast_ref::<ProviderError>()
            .expect("typed provider error");

        assert_eq!(served, 1);
        assert_eq!(provider_error.kind(), ProviderErrorKind::Unauthorized);
        Ok(())
    }

    #[test]
    fn claude_code_oauth_url_and_callback_parser_match_main_contract() {
        let (verifier, challenge) = claude_code_oauth_pkce();
        let url = claude_code_oauth_authorize_url(&verifier, &challenge);
        assert!(url.starts_with("https://claude.ai/oauth/authorize?"));
        assert!(url.contains("client_id=9d1c250a-e61b-44d9-88ed-5944d1962f5e"));
        assert!(url.contains("response_type=code"));
        assert!(url.contains("code_challenge_method=S256"));
        assert!(url.contains("user%3Asessions%3Aclaude_code"));
        let parsed = parse_claude_code_authorization_input(&format!(
            "http://localhost:53692/callback?code=abc123&state={verifier}"
        ));
        assert_eq!(parsed.code.as_deref(), Some("abc123"));
        assert_eq!(parsed.state.as_deref(), Some(verifier.as_str()));
        let parsed = parse_claude_code_authorization_input("abc123#state456");
        assert_eq!(parsed.code.as_deref(), Some("abc123"));
        assert_eq!(parsed.state.as_deref(), Some("state456"));
    }

    #[test]
    fn openai_responses_provider_serializes_model_request_settings() -> Result<()> {
        let response_body = json!({
            "id": "resp_123",
            "output": [],
            "usage": {
                "input_tokens": 1,
                "output_tokens": 1,
                "total_tokens": 2
            }
        })
        .to_string();
        let (base_url, body_rx, handle) =
            spawn_request_capture_server(response_body, "application/json")?;
        let provider = OpenAIResponsesProvider::with_base_url("test-key", "gpt-5.5", base_url);

        provider.start_turn(ProviderTurn {
            instructions: Some("base".to_string()),
            model_settings: ModelRequestSettings {
                reasoning_effort: Some("high".to_string()),
                reasoning_summary: Some("detailed".to_string()),
                model_supports_reasoning_summaries: None,
                text_verbosity: Some("high".to_string()),
                service_tier: None,
            },
            messages: vec![json!({"role": "user", "content": "finish"})],
            tools: Vec::new(),
            output_schema: Some(json!({
                "type": "object",
                "properties": {
                    "ok": { "type": "boolean" }
                },
                "required": ["ok"],
                "additionalProperties": false
            })),
            output_schema_strict: false,
            ..ProviderTurn::default()
        })?;
        handle.join().expect("mock server thread");
        let request = body_rx.recv().expect("request body");

        assert_eq!(request["reasoning"]["effort"], "high");
        assert_eq!(request["reasoning"]["summary"], "detailed");
        assert_eq!(request["include"], json!(["reasoning.encrypted_content"]));
        assert_eq!(request["text"]["verbosity"], "high");
        assert_eq!(request["text"]["format"]["type"], "json_schema");
        assert_eq!(request["text"]["format"]["strict"], false);
        assert_eq!(request["text"]["format"]["name"], "codex_output_schema");
        assert_eq!(
            request["text"]["format"]["schema"]["required"],
            json!(["ok"])
        );
        Ok(())
    }

    #[test]
    fn openai_responses_provider_serializes_supported_service_tier_like_codex() -> Result<()> {
        let response_body = json!({
            "id": "resp_123",
            "output": [],
            "usage": {
                "input_tokens": 1,
                "output_tokens": 1,
                "total_tokens": 2
            }
        })
        .to_string();
        let (base_url, body_rx, handle) =
            spawn_request_capture_server(response_body, "application/json")?;
        let provider = OpenAIResponsesProvider::with_base_url("test-key", "gpt-5.4", base_url);

        provider.start_turn(ProviderTurn {
            instructions: Some("base".to_string()),
            model_settings: ModelRequestSettings {
                service_tier: Some("priority".to_string()),
                ..Default::default()
            },
            messages: vec![json!({"role": "user", "content": "finish"})],
            tools: Vec::new(),
            ..ProviderTurn::default()
        })?;
        handle.join().expect("mock server thread");
        let request = body_rx.recv().expect("request body");

        assert_eq!(request["service_tier"], "priority");
        Ok(())
    }

    #[test]
    fn openai_responses_provider_omits_unsupported_mini_service_tier_like_codex() -> Result<()> {
        let response_body = json!({
            "id": "resp_123",
            "output": [],
            "usage": {
                "input_tokens": 1,
                "output_tokens": 1,
                "total_tokens": 2
            }
        })
        .to_string();
        let (base_url, body_rx, handle) =
            spawn_request_capture_server(response_body, "application/json")?;
        let provider = OpenAIResponsesProvider::with_base_url("test-key", "gpt-5.4-mini", base_url);

        provider.start_turn(ProviderTurn {
            instructions: Some("base".to_string()),
            model_settings: ModelRequestSettings {
                service_tier: Some("priority".to_string()),
                ..Default::default()
            },
            messages: vec![json!({"role": "user", "content": "finish"})],
            tools: Vec::new(),
            ..ProviderTurn::default()
        })?;
        handle.join().expect("mock server thread");
        let request = body_rx.recv().expect("request body");

        assert!(request.get("service_tier").is_none());
        Ok(())
    }

    #[test]
    fn spawn_agent_model_overrides_description_matches_codex_catalog_slice() {
        let description = spawn_agent_model_overrides_description();
        assert!(description.contains(
            "Available model overrides (optional; inherited parent model is preferred):"
        ));
        assert!(description.contains(
            "- `gpt-5.5`: Frontier model for complex coding, research, and real-world work. Reasoning efforts: low, medium (default), high, xhigh. Service tiers: priority."
        ));
        assert!(description.contains(
            "- `gpt-5.4`: Strong model for everyday coding. Reasoning efforts: low, medium (default), high, xhigh. Service tiers: priority."
        ));
        assert!(description.contains(
            "- `gpt-5.4-mini`: Small, fast, and cost-efficient model for simpler coding tasks. Reasoning efforts: low, medium (default), high, xhigh."
        ));
        assert!(description.contains(
            "- `gpt-5.3-codex`: Coding-optimized model. Reasoning efforts: low, medium (default), high, xhigh."
        ));
        assert!(description.contains(
            "- `gpt-5.2`: Optimized for professional work and long-running agents. Reasoning efforts: low, medium (default), high, xhigh."
        ));
        assert!(!description.contains("codex-auto-review"));
    }

    #[test]
    fn bundled_model_catalog_uses_codex_models_json_prompt_shapes() {
        let catalog = bundled_model_catalog();
        assert_eq!(
            catalog
                .models
                .iter()
                .map(|entry| entry.slug.as_str())
                .collect::<Vec<_>>(),
            vec![
                "gpt-5.5",
                "gpt-5.4",
                "gpt-5.4-mini",
                "gpt-5.3-codex",
                "gpt-5.2",
                "codex-auto-review"
            ]
        );

        let gpt_5_4 = catalog.entry_for_model("gpt-5.4").expect("gpt-5.4");
        assert_eq!(gpt_5_4.base_instructions.chars().count(), 14731);
        let model_messages = gpt_5_4.model_messages.as_ref().expect("model messages");
        assert_eq!(
            model_messages
                .instructions_template
                .as_deref()
                .unwrap_or_default()
                .chars()
                .count(),
            12896
        );

        let rendered = gpt_5_4.get_model_instructions(Some(ModelPersonality::Pragmatic));
        assert!(rendered.starts_with(
            "You are Codex, a coding agent based on GPT-5. You and the user share the same workspace"
        ));
        assert!(rendered.contains("# Personality\n\nYou are a deeply pragmatic"));
        assert!(!rendered.contains("{{ base_instructions }}"));
        assert!(catalog
            .entry_for_model("gpt-5.2")
            .expect("gpt-5.2")
            .model_messages
            .is_none());
    }

    #[test]
    fn bundled_model_presets_are_catalog_derived_like_codex() {
        let presets = bundled_model_presets();
        let picker_ids = presets
            .iter()
            .filter(|preset| preset.show_in_picker)
            .map(|preset| preset.id.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            picker_ids,
            vec![
                "gpt-5.5",
                "gpt-5.4",
                "gpt-5.4-mini",
                "gpt-5.3-codex",
                "gpt-5.2"
            ]
        );
        assert_eq!(
            presets
                .iter()
                .filter(|preset| preset.is_default)
                .map(|preset| preset.id.as_str())
                .collect::<Vec<_>>(),
            vec!["gpt-5.5"]
        );
        let auto_review = presets
            .iter()
            .find(|preset| preset.id == "codex-auto-review")
            .expect("auto review preset");
        assert!(!auto_review.show_in_picker);
        assert!(auto_review.supports_personality);
    }

    #[test]
    fn dynamic_model_catalog_drives_presets_requests_and_instructions_like_codex() {
        let catalog: ModelCatalog = serde_json::from_value(json!({
            "models": [
                {
                    "slug": "catalog-hidden",
                    "display_name": "Catalog Hidden",
                    "description": "Hidden but request-capable.",
                    "default_reasoning_level": "low",
                    "supported_reasoning_levels": [{"effort": "low"}],
                    "visibility": "hide",
                    "supported_in_api": true,
                    "priority": 0,
                    "base_instructions": "Hidden base",
                    "supports_reasoning_summaries": true,
                    "default_reasoning_summary": "none",
                    "supports_parallel_tool_calls": false,
                    "input_modalities": ["text"]
                },
                {
                    "slug": "catalog-model",
                    "display_name": "Catalog Model",
                    "description": "Catalog-defined picker model.",
                    "default_reasoning_level": "high",
                    "supported_reasoning_levels": [{"effort": "low"}, {"effort": "high"}],
                    "visibility": "list",
                    "supported_in_api": true,
                    "priority": 2,
                    "service_tiers": [{"id": "priority", "name": "", "description": ""}],
                    "base_instructions": "Catalog base",
                    "model_messages": {
                        "instructions_template": "Catalog template {{ personality }} :: {{ base_instructions }}",
                        "instructions_variables": {
                            "personality_default": "default personality",
                            "personality_friendly": "friendly personality",
                            "personality_pragmatic": "pragmatic personality"
                        }
                    },
                    "supports_reasoning_summaries": true,
                    "default_reasoning_summary": "detailed",
                    "support_verbosity": true,
                    "default_verbosity": "high",
                    "supports_parallel_tool_calls": true,
                    "supports_search_tool": true,
                    "supports_image_detail_original": true,
                    "shell_type": "disabled",
                    "web_search_tool_type": "text_and_image",
                    "experimental_supported_tools": ["image_generation"],
                    "context_window": 1000,
                    "max_context_window": 2000,
                    "auto_compact_token_limit": 800,
                    "truncation_policy": {"mode": "tokens", "limit": 123}
                }
            ]
        }))
        .expect("catalog json");

        let presets = catalog.presets(true);
        assert_eq!(
            presets
                .iter()
                .filter(|preset| preset.is_default)
                .map(|preset| preset.id.as_str())
                .collect::<Vec<_>>(),
            vec!["catalog-model"]
        );
        let info = model_request_info_for_catalog("catalog-model-2026", Some(&catalog));
        assert_eq!(info.default_reasoning_effort.as_deref(), Some("high"));
        assert_eq!(info.default_verbosity.as_deref(), Some("high"));
        assert!(info.supports_parallel_tool_calls);
        assert!(info.supports_search_tool);
        assert!(info.supports_image_detail_original);
        assert_eq!(info.shell_type, ModelShellType::Disabled);
        assert_eq!(info.web_search_tool_type, WebSearchToolType::TextAndImage);
        assert_eq!(info.experimental_supported_tools, vec!["image_generation"]);
        assert_eq!(info.resolved_context_window(), Some(1000));
        assert_eq!(info.auto_compact_token_limit(), Some(800));
        assert_eq!(info.tool_output_token_budget(), 123);

        let hidden = model_request_info_for_catalog("catalog-hidden", Some(&catalog));
        assert!(!hidden.supports_image_input);
        assert!(!hidden.supports_parallel_tool_calls);
        assert!(!hidden.supports_search_tool);
        assert_eq!(hidden.tool_output_token_budget(), 2500);

        let description = spawn_agent_model_overrides_description_for_catalog(&catalog, true);
        assert!(description.contains("- `catalog-model`: Catalog-defined picker model."));
        assert!(!description.contains("catalog-hidden"));

        let instructions = default_agent_instructions_for_model_and_personality_with_catalog(
            "catalog-model",
            ModelPersonality::Pragmatic,
            Some(&catalog),
        );
        assert!(instructions.starts_with("You are Browser Use Terminal, a web agent"));
        assert!(instructions
            .contains("Catalog template pragmatic personality :: {{ base_instructions }}"));
    }

    #[test]
    fn openai_responses_provider_uses_model_defaults_for_known_models() -> Result<()> {
        let response_body = json!({
            "id": "resp_123",
            "output": [],
            "usage": {
                "input_tokens": 1,
                "output_tokens": 1,
                "total_tokens": 2
            }
        })
        .to_string();
        let (base_url, body_rx, handle) =
            spawn_request_capture_server(response_body, "application/json")?;
        let provider = OpenAIResponsesProvider::with_base_url("test-key", "gpt-5.5", base_url);

        provider.start_turn(ProviderTurn {
            instructions: Some("base".to_string()),
            model_settings: ModelRequestSettings::default(),
            messages: vec![json!({"role": "user", "content": "finish"})],
            tools: Vec::new(),
            ..ProviderTurn::default()
        })?;
        handle.join().expect("mock server thread");
        let request = body_rx.recv().expect("request body");

        assert_eq!(request["reasoning"]["effort"], "medium");
        assert!(request["reasoning"].get("summary").is_none());
        assert_eq!(request["include"], json!(["reasoning.encrypted_content"]));
        assert_eq!(request["text"]["verbosity"], "low");
        assert_eq!(request["parallel_tool_calls"], true);
        Ok(())
    }

    #[test]
    fn openai_responses_provider_uses_codex_streaming_request_fields() -> Result<()> {
        let response_body = "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_123\",\"output\":[],\"usage\":{\"input_tokens\":1,\"output_tokens\":1,\"total_tokens\":2}}}\n\n";
        let (base_url, body_rx, handle) =
            spawn_request_capture_server(response_body.to_string(), "text/event-stream")?;
        let provider = OpenAIResponsesProvider::with_base_url("test-key", "gpt-5.5", base_url);

        provider.start_turn(ProviderTurn {
            instructions: Some("base".to_string()),
            model_settings: ModelRequestSettings::default(),
            messages: vec![json!({"role": "user", "content": "finish"})],
            tools: Vec::new(),
            prompt_cache_key: Some("thread-123".to_string()),
            previous_response_id: Some("resp_previous".to_string()),
            client_metadata: Some(HashMap::from([(
                "x-codex-installation-id".to_string(),
                "install-123".to_string(),
            )])),
            ..ProviderTurn::default()
        })?;
        handle.join().expect("mock server thread");
        let request = body_rx.recv().expect("request body");

        assert_eq!(request["stream"], true);
        assert_eq!(request["tool_choice"], "auto");
        assert_eq!(request["store"], false);
        assert_eq!(request["include"], json!(["reasoning.encrypted_content"]));
        assert_eq!(request["prompt_cache_key"], "thread-123");
        assert_eq!(request["previous_response_id"], "resp_previous");
        assert_eq!(
            request["client_metadata"]["x-codex-installation-id"],
            "install-123"
        );
        assert_eq!(request["parallel_tool_calls"], true);
        Ok(())
    }

    #[test]
    fn openai_responses_provider_sends_codex_identity_headers() -> Result<()> {
        let response_body = "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_123\",\"output\":[],\"usage\":{\"input_tokens\":1,\"output_tokens\":1,\"total_tokens\":2}}}\n\n";
        let (base_url, headers_rx, handle) = spawn_request_header_capture_server(
            MockHttpResponse::new(200, "OK", response_body, "text/event-stream"),
        )?;
        let provider = OpenAIResponsesProvider::with_base_url("test-key", "gpt-5.5", base_url);

        provider.start_turn(ProviderTurn {
            instructions: Some("base".to_string()),
            model_settings: ModelRequestSettings::default(),
            messages: vec![json!({"role": "user", "content": "finish"})],
            tools: Vec::new(),
            extra_headers: Some(HashMap::from([
                ("session-id".to_string(), "session-123".to_string()),
                ("thread-id".to_string(), "session-123".to_string()),
                ("x-client-request-id".to_string(), "session-123".to_string()),
                ("x-codex-window-id".to_string(), "window-123".to_string()),
                (
                    "x-codex-beta-features".to_string(),
                    "remote_compaction_v2".to_string(),
                ),
                ("x-openai-subagent".to_string(), "collab_spawn".to_string()),
                (
                    "x-codex-parent-thread-id".to_string(),
                    "parent-123".to_string(),
                ),
            ])),
            ..ProviderTurn::default()
        })?;
        handle.join().expect("mock server thread");
        let headers = headers_rx.recv().expect("request headers");
        let headers = headers.to_ascii_lowercase();

        assert!(headers.contains("\r\nsession-id: session-123\r\n"));
        assert!(headers.contains("\r\nthread-id: session-123\r\n"));
        assert!(headers.contains("\r\nx-client-request-id: session-123\r\n"));
        assert!(headers.contains("\r\nx-codex-window-id: window-123\r\n"));
        assert!(headers.contains("\r\nx-codex-beta-features: remote_compaction_v2\r\n"));
        assert!(headers.contains("\r\nx-openai-subagent: collab_spawn\r\n"));
        assert!(headers.contains("\r\nx-codex-parent-thread-id: parent-123\r\n"));
        Ok(())
    }

    #[test]
    fn openai_responses_provider_applies_provider_registry_headers_and_query_params() -> Result<()>
    {
        let response_body = "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_123\",\"output\":[],\"usage\":{\"input_tokens\":1,\"output_tokens\":1,\"total_tokens\":2}}}\n\n";
        let (base_url, headers_rx, handle) = spawn_request_header_capture_server(
            MockHttpResponse::new(200, "OK", response_body, "text/event-stream"),
        )?;
        let provider = OpenAIResponsesProvider::with_base_url("test-key", "gpt-5.5", base_url)
            .with_provider_name("openai-custom")
            .with_request_options(ProviderRequestOptions {
                query_params: vec![("api-version".to_string(), "2026-05-25".to_string())],
                headers: HashMap::from([("x-provider-test".to_string(), "yes".to_string())]),
            });

        assert_eq!(provider.provider_name(), "openai-custom");
        provider.start_turn(ProviderTurn {
            instructions: Some("base".to_string()),
            messages: vec![json!({"role": "user", "content": "finish"})],
            ..ProviderTurn::default()
        })?;
        handle.join().expect("mock server thread");
        let headers = headers_rx.recv().expect("request headers");

        assert!(headers.starts_with("POST /v1/responses?api-version=2026-05-25 "));
        assert!(headers
            .to_ascii_lowercase()
            .contains("\r\nx-provider-test: yes\r\n"));
        Ok(())
    }

    #[test]
    fn openai_responses_provider_command_auth_refreshes_after_401_like_codex() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let response_body = "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_123\",\"output\":[],\"usage\":{\"input_tokens\":1,\"output_tokens\":1,\"total_tokens\":2}}}\n\n";
        let (base_url, headers_rx, handle) = spawn_request_header_capture_server_sequence(vec![
            MockHttpResponse::new(
                401,
                "Unauthorized",
                r#"{"error":{"message":"stale token"}}"#,
                "application/json",
            ),
            MockHttpResponse::new(200, "OK", response_body, "text/event-stream"),
        ])?;
        let provider = OpenAIResponsesProvider::with_optional_api_key(None, "gpt-5.5", base_url)
            .with_command_auth_config(ProviderCommandAuthConfig {
                command: "sh".to_string(),
                args: vec![
                    "-c".to_string(),
                    "if [ -f token_counter ]; then echo ' fresh-token '; else touch token_counter; echo stale-token; fi".to_string(),
                ],
                timeout_ms: 5_000,
                refresh_interval_ms: 300_000,
                cwd: temp.path().to_path_buf(),
            });

        provider.start_turn(ProviderTurn {
            instructions: Some("base".to_string()),
            messages: vec![json!({"role": "user", "content": "finish"})],
            ..ProviderTurn::default()
        })?;
        handle.join().expect("mock server thread");
        let first = headers_rx.recv().expect("first request headers");
        let second = headers_rx.recv().expect("second request headers");

        assert!(first
            .to_ascii_lowercase()
            .contains("\r\nauthorization: bearer stale-token\r\n"));
        assert!(second
            .to_ascii_lowercase()
            .contains("\r\nauthorization: bearer fresh-token\r\n"));
        Ok(())
    }

    #[test]
    fn codex_responses_http_omits_websocket_beta_header_like_codex() -> Result<()> {
        let response_body = "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_123\",\"output\":[],\"usage\":{\"input_tokens\":1,\"output_tokens\":1,\"total_tokens\":2}}}\n\n";
        let (base_url, headers_rx, handle) = spawn_request_header_capture_server(
            MockHttpResponse::new(200, "OK", response_body, "text/event-stream"),
        )?;
        let provider = CodexResponsesProvider::with_base_url(
            CodexAuth {
                access_token: "chatgpt-token".to_string(),
                account_id: "account-123".to_string(),
            },
            "gpt-test",
            format!("{}/backend-api", base_url.trim_end_matches("/v1")),
        );

        provider.start_turn(ProviderTurn {
            messages: vec![json!({"role": "user", "content": "finish"})],
            ..ProviderTurn::default()
        })?;
        handle.join().expect("mock server thread");
        let headers = headers_rx
            .recv()
            .expect("request headers")
            .to_ascii_lowercase();

        assert!(!headers.contains("\r\nopenai-beta: responses=experimental\r\n"));
        Ok(())
    }

    #[test]
    fn openai_responses_provider_replays_sticky_turn_state_like_codex() -> Result<()> {
        let response_body = "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_123\",\"output\":[],\"usage\":{\"input_tokens\":1,\"output_tokens\":1,\"total_tokens\":2}}}\n\n";
        let (base_url, headers_rx, handle) = spawn_request_header_capture_server_sequence(vec![
            MockHttpResponse::new(200, "OK", response_body, "text/event-stream")
                .with_header("x-codex-turn-state", "sticky-a"),
            MockHttpResponse::new(200, "OK", response_body, "text/event-stream")
                .with_header("x-codex-turn-state", "sticky-b"),
            MockHttpResponse::new(200, "OK", response_body, "text/event-stream"),
        ])?;
        let provider = OpenAIResponsesProvider::with_base_url("test-key", "gpt-5.5", base_url);
        let turn_state = Arc::new(Mutex::new(None));

        for _ in 0..3 {
            provider.start_turn(ProviderTurn {
                instructions: Some("base".to_string()),
                model_settings: ModelRequestSettings::default(),
                messages: vec![json!({"role": "user", "content": "finish"})],
                tools: Vec::new(),
                turn_state: Some(Arc::clone(&turn_state)),
                ..ProviderTurn::default()
            })?;
        }
        handle.join().expect("mock server thread");
        let first_headers = headers_rx
            .recv()
            .expect("first request headers")
            .to_ascii_lowercase();
        let second_headers = headers_rx
            .recv()
            .expect("second request headers")
            .to_ascii_lowercase();
        let third_headers = headers_rx
            .recv()
            .expect("third request headers")
            .to_ascii_lowercase();

        assert!(!first_headers.contains("\r\nx-codex-turn-state:"));
        assert!(second_headers.contains("\r\nx-codex-turn-state: sticky-a\r\n"));
        assert!(third_headers.contains("\r\nx-codex-turn-state: sticky-a\r\n"));
        assert_eq!(
            turn_state.lock().expect("turn state lock").as_deref(),
            Some("sticky-a")
        );
        Ok(())
    }

    #[test]
    fn openai_responses_provider_does_not_auto_reuse_previous_response_over_http_like_codex(
    ) -> Result<()> {
        let assistant_item = json!({
            "id": "msg_1",
            "type": "message",
            "role": "assistant",
            "content": [{ "type": "output_text", "text": "first answer" }]
        });
        let first_response = format!(
            "data: {}\n\n",
            json!({
                "type": "response.completed",
                "response": {
                    "id": "resp_1",
                    "output": [assistant_item.clone()],
                    "usage": { "input_tokens": 1, "output_tokens": 1, "total_tokens": 2 }
                }
            })
        );
        let second_response = "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_2\",\"output\":[],\"usage\":{\"input_tokens\":1,\"output_tokens\":1,\"total_tokens\":2}}}\n\n".to_string();
        let (base_url, body_rx, handle) = spawn_request_capture_server_sequence(
            vec![first_response, second_response],
            "text/event-stream",
        )?;
        let provider = OpenAIResponsesProvider::with_base_url("test-key", "gpt-test", base_url);

        provider.start_turn(ProviderTurn {
            instructions: Some("base".to_string()),
            messages: vec![json!({"role": "user", "content": "first"})],
            prompt_cache_key: Some("thread-123".to_string()),
            ..ProviderTurn::default()
        })?;
        provider.start_turn(ProviderTurn {
            instructions: Some("base".to_string()),
            messages: vec![
                json!({"role": "user", "content": "first"}),
                assistant_item,
                json!({"role": "user", "content": "second"}),
            ],
            prompt_cache_key: Some("thread-123".to_string()),
            ..ProviderTurn::default()
        })?;
        handle.join().expect("mock server thread");
        let first_request = body_rx.recv().expect("first request body");
        let second_request = body_rx.recv().expect("second request body");

        assert!(first_request.get("previous_response_id").is_none());
        assert!(second_request.get("previous_response_id").is_none());
        let second_input = second_request["input"].as_array().expect("input array");
        assert_eq!(second_input.len(), 3);
        assert_eq!(second_input[0]["role"], "user");
        assert_eq!(second_input[0]["content"][0]["text"], "first");
        assert_eq!(second_input[2]["role"], "user");
        assert_eq!(second_input[2]["content"][0]["text"], "second");
        Ok(())
    }

    #[test]
    fn openai_responses_provider_sends_full_http_body_when_non_input_differs() -> Result<()> {
        let first_response = "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"output\":[],\"usage\":{\"input_tokens\":1,\"output_tokens\":1,\"total_tokens\":2}}}\n\n".to_string();
        let second_response = "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_2\",\"output\":[],\"usage\":{\"input_tokens\":1,\"output_tokens\":1,\"total_tokens\":2}}}\n\n".to_string();
        let (base_url, body_rx, handle) = spawn_request_capture_server_sequence(
            vec![first_response, second_response],
            "text/event-stream",
        )?;
        let provider = OpenAIResponsesProvider::with_base_url("test-key", "gpt-test", base_url);

        provider.start_turn(ProviderTurn {
            instructions: Some("base-a".to_string()),
            messages: vec![json!({"role": "user", "content": "first"})],
            ..ProviderTurn::default()
        })?;
        provider.start_turn(ProviderTurn {
            instructions: Some("base-b".to_string()),
            messages: vec![
                json!({"role": "user", "content": "first"}),
                json!({"role": "user", "content": "second"}),
            ],
            ..ProviderTurn::default()
        })?;
        handle.join().expect("mock server thread");
        let _first_request = body_rx.recv().expect("first request body");
        let second_request = body_rx.recv().expect("second request body");

        assert!(second_request.get("previous_response_id").is_none());
        assert_eq!(
            second_request["input"]
                .as_array()
                .expect("input array")
                .len(),
            2
        );
        Ok(())
    }

    #[test]
    fn openai_responses_provider_clears_previous_response_after_stream_error() -> Result<()> {
        let assistant_item = json!({
            "id": "msg_1",
            "type": "message",
            "role": "assistant",
            "content": [{ "type": "output_text", "text": "first answer" }]
        });
        let first_response = format!(
            "data: {}\n\n",
            json!({
                "type": "response.completed",
                "response": {
                    "id": "resp_1",
                    "output": [assistant_item.clone()],
                    "usage": { "input_tokens": 1, "output_tokens": 1, "total_tokens": 2 }
                }
            })
        );
        let failed_response =
            "data: {\"type\":\"response.incomplete\",\"response\":{\"incomplete_details\":{\"reason\":\"max_output_tokens\"}}}\n\n"
                .to_string();
        let third_response = "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_3\",\"output\":[],\"usage\":{\"input_tokens\":1,\"output_tokens\":1,\"total_tokens\":2}}}\n\n".to_string();
        let (base_url, body_rx, handle) = spawn_request_capture_server_sequence(
            vec![first_response, failed_response, third_response],
            "text/event-stream",
        )?;
        let provider = OpenAIResponsesProvider::with_base_url("test-key", "gpt-test", base_url);
        let full_history = vec![
            json!({"role": "user", "content": "first"}),
            assistant_item,
            json!({"role": "user", "content": "second"}),
        ];

        provider.start_turn(ProviderTurn {
            messages: vec![json!({"role": "user", "content": "first"})],
            ..ProviderTurn::default()
        })?;
        let error = provider
            .start_turn(ProviderTurn {
                messages: full_history.clone(),
                ..ProviderTurn::default()
            })
            .expect_err("second turn should fail");
        assert!(format!("{error:#}").contains("Incomplete response returned"));
        provider.start_turn(ProviderTurn {
            messages: full_history,
            ..ProviderTurn::default()
        })?;
        handle.join().expect("mock server thread");
        let _first_request = body_rx.recv().expect("first request body");
        let second_request = body_rx.recv().expect("second request body");
        let third_request = body_rx.recv().expect("third request body");

        assert!(second_request.get("previous_response_id").is_none());
        assert_eq!(
            second_request["input"]
                .as_array()
                .expect("input array")
                .len(),
            3
        );
        assert!(third_request.get("previous_response_id").is_none());
        assert_eq!(
            third_request["input"]
                .as_array()
                .expect("input array")
                .len(),
            3
        );
        Ok(())
    }

    #[test]
    fn openai_responses_hidden_request_retry_preserves_full_http_body_like_codex() -> Result<()> {
        let assistant_item = json!({
            "id": "msg_1",
            "type": "message",
            "role": "assistant",
            "content": [{ "type": "output_text", "text": "first answer" }]
        });
        let first_response = format!(
            "data: {}\n\n",
            json!({
                "type": "response.completed",
                "response": {
                    "id": "resp_1",
                    "output": [assistant_item.clone()],
                    "usage": { "input_tokens": 1, "output_tokens": 1, "total_tokens": 2 }
                }
            })
        );
        let retry_success = "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_2\",\"output\":[],\"usage\":{\"input_tokens\":1,\"output_tokens\":1,\"total_tokens\":2}}}\n\n";
        let (base_url, body_rx, handle) = spawn_request_capture_http_sequence(vec![
            MockHttpResponse::new(200, "OK", first_response, "text/event-stream"),
            MockHttpResponse::new(
                500,
                "Internal Server Error",
                "temporary failure",
                "text/plain",
            ),
            MockHttpResponse::new(200, "OK", retry_success, "text/event-stream"),
        ])?;
        let mut provider = OpenAIResponsesProvider::with_base_url("test-key", "gpt-test", base_url);
        provider.request_retry = ProviderRequestRetryConfig::without_delay();
        let full_history = vec![
            json!({"role": "user", "content": "first"}),
            assistant_item,
            json!({"role": "user", "content": "second"}),
        ];

        provider.start_turn(ProviderTurn {
            messages: vec![json!({"role": "user", "content": "first"})],
            ..ProviderTurn::default()
        })?;
        provider.start_turn(ProviderTurn {
            messages: full_history,
            ..ProviderTurn::default()
        })?;
        handle.join().expect("mock server thread");
        let first_request = body_rx.recv().expect("first request body");
        let first_attempt = body_rx.recv().expect("first retrying request body");
        let second_attempt = body_rx.recv().expect("second retrying request body");

        assert!(first_request.get("previous_response_id").is_none());
        for request in [first_attempt, second_attempt] {
            assert!(request.get("previous_response_id").is_none());
            let input = request["input"].as_array().expect("input array");
            assert_eq!(input.len(), 3);
            assert_eq!(input[0]["role"], "user");
            assert_eq!(input[0]["content"][0]["text"], "first");
            assert_eq!(input[2]["role"], "user");
            assert_eq!(input[2]["content"][0]["text"], "second");
        }
        Ok(())
    }

    #[test]
    fn codex_responses_provider_serializes_previous_response_id() -> Result<()> {
        let response_body = "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_123\",\"output\":[],\"usage\":{\"input_tokens\":1,\"output_tokens\":1,\"total_tokens\":2}}}\n\n";
        let (base_url, body_rx, handle) =
            spawn_request_capture_server(response_body.to_string(), "text/event-stream")?;
        let mut provider = CodexResponsesProvider::with_base_url(
            CodexAuth {
                access_token: "chatgpt-token".to_string(),
                account_id: "account-123".to_string(),
            },
            "gpt-test",
            format!("{}/backend-api", base_url.trim_end_matches("/v1")),
        );
        provider.request_retry = ProviderRequestRetryConfig::without_retries();

        provider.start_turn(ProviderTurn {
            messages: vec![json!({"role": "user", "content": "delta only"})],
            previous_response_id: Some("resp_previous".to_string()),
            ..ProviderTurn::default()
        })?;
        handle.join().expect("mock server thread");
        let request = body_rx.recv().expect("request body");

        assert_eq!(request["previous_response_id"], "resp_previous");
        assert_eq!(request["store"], false);
        Ok(())
    }

    #[test]
    fn azure_responses_base_url_sets_store_like_codex() {
        assert!(is_azure_responses_base_url(
            "https://foo.openai.azure.com/openai"
        ));
        assert!(is_azure_responses_base_url(
            "https://foo.openai.azure.us/openai/deployments/bar"
        ));
        assert!(is_azure_responses_base_url(
            "https://foo.cognitiveservices.azure.cn/openai"
        ));
        assert!(!is_azure_responses_base_url(
            "https://myproxy.azurewebsites.net/openai"
        ));
    }

    #[test]
    fn openai_responses_provider_omits_unsupported_model_settings_for_unknown_models() -> Result<()>
    {
        let response_body = json!({
            "id": "resp_123",
            "output": [],
            "usage": {
                "input_tokens": 1,
                "output_tokens": 1,
                "total_tokens": 2
            }
        })
        .to_string();
        let (base_url, body_rx, handle) =
            spawn_request_capture_server(response_body, "application/json")?;
        let provider = OpenAIResponsesProvider::with_base_url("test-key", "gpt-test", base_url);

        provider.start_turn(ProviderTurn {
            instructions: Some("base".to_string()),
            model_settings: ModelRequestSettings {
                reasoning_effort: Some("high".to_string()),
                reasoning_summary: Some("detailed".to_string()),
                model_supports_reasoning_summaries: None,
                text_verbosity: Some("high".to_string()),
                service_tier: None,
            },
            messages: vec![json!({"role": "user", "content": "finish"})],
            tools: Vec::new(),
            ..ProviderTurn::default()
        })?;
        handle.join().expect("mock server thread");
        let request = body_rx.recv().expect("request body");

        assert!(request.get("reasoning").is_none());
        assert_eq!(request["include"], json!([]));
        assert!(request.get("text").is_none());
        Ok(())
    }

    #[test]
    fn responses_sse_errors_before_done_like_codex() -> Result<()> {
        for (sse, expected) in [
            (
                "data: {\"type\":\"response.failed\",\"response\":{\"error\":{\"code\":\"context_length_exceeded\",\"message\":\"Your input exceeds the context window.\"}}}\n\n",
                "response.failed (context_length_exceeded): Your input exceeds the context window.",
            ),
            (
                "data: {\"type\":\"response.incomplete\",\"response\":{\"incomplete_details\":{\"reason\":\"max_output_tokens\"}}}\n\n",
                "Incomplete response returned, reason: max_output_tokens",
            ),
            (
                "data: {\"type\":\"response.output_text.delta\",\"delta\":\"partial\"}\n\n",
                "stream closed before response.completed",
            ),
            (
                "data: {\"type\":\"response.done\",\"response\":{\"output\":[]}}\n\n",
                "stream closed before response.completed",
            ),
        ] {
            let (base_url, handle) = spawn_mock_server(sse.to_string(), "text/event-stream")?;
            let provider = CodexResponsesProvider::with_base_url(
                CodexAuth {
                    access_token: "chatgpt-token".to_string(),
                    account_id: "account-123".to_string(),
                },
                "gpt-test",
                format!("{}/backend-api", base_url.trim_end_matches("/v1")),
            );
            let err = provider
                .start_turn(ProviderTurn {
                    instructions: None,
                    model_settings: ModelRequestSettings::default(),
                    messages: vec![json!({"role": "user", "content": "finish"})],
                    tools: Vec::new(),
                    ..ProviderTurn::default()
                })
                .expect_err("stream should fail before completion");
            handle.join().expect("mock server thread");
            assert!(
                format!("{err:#}").contains(expected),
                "expected {expected:?}, got {err:#}"
            );
            if expected == "stream closed before response.completed" {
                let provider_error = err
                    .downcast_ref::<ProviderError>()
                    .expect("EOF before response.completed should be a typed stream error");
                assert_eq!(provider_error.kind(), ProviderErrorKind::Stream);
                assert!(provider_error.is_retryable());
            }
        }
        Ok(())
    }

    #[test]
    fn responses_sse_without_content_type_is_streamed_like_codex() -> Result<()> {
        let sse = concat!(
            "event: response.created\n",
            "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_123\"}}\n\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_123\",\"output\":[],\"usage\":{\"input_tokens\":1,\"output_tokens\":1,\"total_tokens\":2}}}\n\n",
        );
        let (base_url, handle) = spawn_mock_server(sse.to_string(), "")?;
        let provider = CodexResponsesProvider::with_base_url(
            CodexAuth {
                access_token: "chatgpt-token".to_string(),
                account_id: "account-123".to_string(),
            },
            "gpt-test",
            format!("{}/backend-api", base_url.trim_end_matches("/v1")),
        );

        let events = provider.start_turn(ProviderTurn {
            messages: vec![json!({"role": "user", "content": "finish"})],
            ..ProviderTurn::default()
        })?;
        handle.join().expect("mock server thread");
        assert!(events.iter().any(|event| {
            matches!(
                event,
                ModelEvent::ResponseCompleted {
                    response_id: Some(response_id),
                    ..
                } if response_id == "resp_123"
            )
        }));
        Ok(())
    }

    #[test]
    fn responses_sse_malformed_event_does_not_mask_later_completion_like_codex() -> Result<()> {
        let sse = concat!(
            "data: {not json}\n\n",
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"ok\"}\n\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_ok\",\"output\":[],\"usage\":{\"input_tokens\":1,\"output_tokens\":1,\"total_tokens\":2}}}\n\n",
        );
        let (base_url, handle) = spawn_mock_server(sse.to_string(), "text/event-stream")?;
        let mut provider = CodexResponsesProvider::with_base_url(
            CodexAuth {
                access_token: "chatgpt-token".to_string(),
                account_id: "account-123".to_string(),
            },
            "gpt-test",
            format!("{}/backend-api", base_url.trim_end_matches("/v1")),
        );
        provider.request_retry = ProviderRequestRetryConfig::without_retries();

        let events = provider.start_turn(ProviderTurn {
            messages: vec![json!({"role": "user", "content": "finish"})],
            ..ProviderTurn::default()
        })?;
        handle.join().expect("mock server thread");
        assert!(events
            .iter()
            .any(|event| { matches!(event, ModelEvent::TextDelta { text } if text == "ok") }));
        assert!(events.iter().any(|event| matches!(event, ModelEvent::Done)));
        Ok(())
    }

    #[test]
    fn responses_sse_header_metadata_events_match_codex() -> Result<()> {
        let sse = concat!(
            "data: {\"type\":\"response.created\",\"response\":{\"headers\":{\"x-openai-model\":[\"gpt-event-routed\"]}}}\n\n",
            "data: {\"type\":\"response.metadata\",\"metadata\":{\"openai_verification_recommendation\":[\"trusted_access_for_cyber\",\"unknown\",\"trusted_access_for_cyber\"]}}\n\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_header_metadata\",\"output\":[],\"usage\":{\"input_tokens\":1,\"output_tokens\":1,\"total_tokens\":2}}}\n\n",
        );
        let (base_url, handle) = spawn_mock_status_sequence_server(vec![MockHttpResponse::new(
            200,
            "OK",
            sse,
            "text/event-stream",
        )
        .with_header("openai-model", "gpt-server-routed")
        .with_header("x-codex-primary-used-percent", "12.5")
        .with_header("x-codex-primary-window-minutes", "300")
        .with_header("x-codex-primary-reset-at", "1770000000")
        .with_header("x-codex-credits-has-credits", "true")
        .with_header("x-codex-credits-unlimited", "false")
        .with_header("x-codex-credits-balance", "42")
        .with_header("x-models-etag", "models-etag-1")
        .with_header("x-reasoning-included", "true")])?;
        let provider = CodexResponsesProvider::with_base_url(
            CodexAuth {
                access_token: "chatgpt-token".to_string(),
                account_id: "account-123".to_string(),
            },
            "gpt-test",
            format!("{}/backend-api", base_url.trim_end_matches("/v1")),
        );

        let events = provider.start_turn(ProviderTurn {
            messages: vec![json!({"role": "user", "content": "finish"})],
            ..ProviderTurn::default()
        })?;
        handle.join().expect("mock server thread");

        assert!(events.contains(&ModelEvent::ServerModel {
            model: "gpt-server-routed".to_string(),
        }));
        assert!(events.contains(&ModelEvent::ServerModel {
            model: "gpt-event-routed".to_string(),
        }));
        assert!(events.contains(&ModelEvent::ModelVerifications {
            verifications: vec![ModelVerification::TrustedAccessForCyber],
        }));
        let snapshot = events
            .iter()
            .find_map(|event| match event {
                ModelEvent::ModelRateLimits { snapshot }
                    if snapshot.limit_id.as_deref() == Some("codex") =>
                {
                    Some(snapshot)
                }
                _ => None,
            })
            .context("missing Codex rate-limit header event")?;
        let primary = snapshot.primary.as_ref().context("primary rate limit")?;
        assert_eq!(primary.used_percent, 12.5);
        assert_eq!(primary.window_minutes, Some(300));
        assert_eq!(primary.resets_at, Some(1770000000));
        let credits = snapshot.credits.as_ref().context("credits snapshot")?;
        assert!(credits.has_credits);
        assert!(!credits.unlimited);
        assert_eq!(credits.balance.as_deref(), Some("42"));
        assert!(events.contains(&ModelEvent::ModelsEtag {
            etag: "models-etag-1".to_string(),
        }));
        assert!(events.contains(&ModelEvent::ServerReasoningIncluded { included: true }));
        Ok(())
    }

    #[test]
    fn responses_json_body_decode_error_is_retryable_like_codex() -> Result<()> {
        let (base_url, handle) = spawn_mock_server("{not json}".to_string(), "application/json")?;
        let mut provider = CodexResponsesProvider::with_base_url(
            CodexAuth {
                access_token: "chatgpt-token".to_string(),
                account_id: "account-123".to_string(),
            },
            "gpt-test",
            format!("{}/backend-api", base_url.trim_end_matches("/v1")),
        );
        provider.request_retry = ProviderRequestRetryConfig::without_retries();

        let err = provider
            .start_turn(ProviderTurn {
                messages: vec![json!({"role": "user", "content": "finish"})],
                ..ProviderTurn::default()
            })
            .expect_err("malformed JSON body should fail this attempt");
        handle.join().expect("mock server thread");
        let provider_error = err
            .downcast_ref::<ProviderError>()
            .expect("typed provider error");
        assert_eq!(provider_error.kind(), ProviderErrorKind::Retryable);
        assert!(provider_error.is_retryable());
        assert!(provider_error.to_string().contains("parse Responses JSON"));
        Ok(())
    }

    #[test]
    fn responses_failed_rate_limit_carries_requested_retry_delay_like_codex() -> Result<()> {
        for (message, expected_delay) in [
            (
                "Rate limit reached. Please try again in 28ms.",
                Duration::from_millis(28),
            ),
            (
                "Rate limit reached. Please try again in 11.054s.",
                Duration::from_secs_f64(11.054),
            ),
            (
                "Rate limit reached. Please try again in 35 seconds.",
                Duration::from_secs(35),
            ),
        ] {
            let sse = format!(
                "data: {}\n\n",
                json!({
                    "type": "response.failed",
                    "response": {
                        "error": {
                            "code": "rate_limit_exceeded",
                            "message": message
                        }
                    }
                })
            );
            let (base_url, handle) = spawn_mock_server(sse, "text/event-stream")?;
            let provider = CodexResponsesProvider::with_base_url(
                CodexAuth {
                    access_token: "chatgpt-token".to_string(),
                    account_id: "account-123".to_string(),
                },
                "gpt-test",
                format!("{}/backend-api", base_url.trim_end_matches("/v1")),
            );
            let err = provider
                .start_turn(ProviderTurn {
                    messages: vec![json!({"role": "user", "content": "finish"})],
                    ..ProviderTurn::default()
                })
                .expect_err("rate limit should fail this turn");
            handle.join().expect("mock server thread");
            let provider_error = err
                .downcast_ref::<ProviderError>()
                .expect("typed provider error");
            assert_eq!(provider_error.kind(), ProviderErrorKind::Retryable);
            assert!(provider_error.is_retryable());
            assert_eq!(provider_error.retry_delay(), Some(expected_delay));
        }
        Ok(())
    }

    #[test]
    fn responses_failed_terminal_codes_are_not_retryable_like_codex() {
        for (code, expected_kind) in [
            (
                "context_length_exceeded",
                ProviderErrorKind::ContextWindowExceeded,
            ),
            ("insufficient_quota", ProviderErrorKind::QuotaExceeded),
            ("usage_not_included", ProviderErrorKind::UsageNotIncluded),
            ("invalid_prompt", ProviderErrorKind::InvalidRequest),
            ("invalid_image", ProviderErrorKind::InvalidImage),
            ("cyber_policy", ProviderErrorKind::CyberPolicy),
        ] {
            let error = response_failed_error(&json!({
                "type": "response.failed",
                "response": {
                    "error": {
                        "code": code,
                        "message": "terminal"
                    }
                }
            }));
            assert_eq!(error.kind(), expected_kind);
            assert!(!error.is_retryable(), "expected {code} to be terminal");
        }
    }

    #[test]
    fn responses_failed_server_overloaded_is_retryable_for_transient_capacity() -> Result<()> {
        for code in ["server_is_overloaded", "slow_down"] {
            let sse = format!(
                "data: {}\n\n",
                json!({
                    "type": "response.failed",
                    "response": {
                        "error": {
                            "code": code,
                            "message": "Server is overloaded."
                        }
                    }
                })
            );
            let (base_url, handle) = spawn_mock_server(sse, "text/event-stream")?;
            let provider = CodexResponsesProvider::with_base_url(
                CodexAuth {
                    access_token: "chatgpt-token".to_string(),
                    account_id: "account-123".to_string(),
                },
                "gpt-test",
                format!("{}/backend-api", base_url.trim_end_matches("/v1")),
            );
            let err = provider
                .start_turn(ProviderTurn {
                    messages: vec![json!({"role": "user", "content": "finish"})],
                    ..ProviderTurn::default()
                })
                .expect_err("server overloaded should fail this turn");
            handle.join().expect("mock server thread");
            let provider_error = err
                .downcast_ref::<ProviderError>()
                .expect("typed provider error");
            assert_eq!(provider_error.kind(), ProviderErrorKind::ServerOverloaded);
            assert!(provider_error.is_retryable());
        }
        Ok(())
    }

    #[test]
    fn responses_http_503_server_overloaded_is_retryable_for_transient_capacity() -> Result<()> {
        let body = json!({
            "error": {
                "code": "server_is_overloaded",
                "message": "Server is overloaded."
            }
        })
        .to_string();
        let (base_url, handle) =
            spawn_mock_status_server(503, "Service Unavailable", body, "application/json")?;
        let mut provider = CodexResponsesProvider::with_base_url(
            CodexAuth {
                access_token: "chatgpt-token".to_string(),
                account_id: "account-123".to_string(),
            },
            "gpt-test",
            format!("{}/backend-api", base_url.trim_end_matches("/v1")),
        );
        provider.request_retry = ProviderRequestRetryConfig::without_retries();

        let err = provider
            .start_turn(ProviderTurn {
                messages: vec![json!({"role": "user", "content": "finish"})],
                ..ProviderTurn::default()
            })
            .expect_err("server overloaded HTTP response should fail this turn");
        handle.join().expect("mock server thread");
        let provider_error = err
            .downcast_ref::<ProviderError>()
            .expect("typed provider error");
        assert_eq!(provider_error.kind(), ProviderErrorKind::ServerOverloaded);
        assert!(provider_error.is_retryable());
        Ok(())
    }

    #[test]
    fn responses_http_401_is_terminal_unauthorized_like_codex() -> Result<()> {
        let body = json!({
            "error": {
                "message": "Unauthorized"
            }
        })
        .to_string();
        let (base_url, handle) = spawn_timed_status_sequence_server(vec![
            MockHttpResponse::new(401, "Unauthorized", body, "application/json"),
            MockHttpResponse::new(
                200,
                "OK",
                "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_123\",\"output\":[]}}\n\n",
                "text/event-stream",
            ),
        ])?;
        let provider = OpenAIResponsesProvider::with_base_url("test-key", "gpt-test", base_url);

        let err = provider
            .start_turn(ProviderTurn {
                messages: vec![json!({"role": "user", "content": "finish"})],
                ..ProviderTurn::default()
            })
            .expect_err("401 should fail without generic retry");
        let served = handle.join().expect("mock server thread");
        let provider_error = err
            .downcast_ref::<ProviderError>()
            .expect("typed provider error");
        assert_eq!(provider_error.kind(), ProviderErrorKind::Unauthorized);
        assert_eq!(provider_error.http_status_code(), Some(401));
        assert!(!provider_error.is_retryable());
        assert_eq!(served, 1, "401 must not use hidden generic request retry");
        Ok(())
    }

    #[test]
    fn responses_http_400_cyber_policy_uses_typed_fallback_like_codex() -> Result<()> {
        let body = json!({
            "error": {
                "code": "cyber_policy"
            }
        })
        .to_string();
        let (base_url, handle) =
            spawn_mock_status_server(400, "Bad Request", body, "application/json")?;
        let provider = CodexResponsesProvider::with_base_url(
            CodexAuth {
                access_token: "chatgpt-token".to_string(),
                account_id: "account-123".to_string(),
            },
            "gpt-test",
            format!("{}/backend-api", base_url.trim_end_matches("/v1")),
        );

        let err = provider
            .start_turn(ProviderTurn {
                messages: vec![json!({"role": "user", "content": "finish"})],
                ..ProviderTurn::default()
            })
            .expect_err("cyber policy HTTP response should fail this turn");
        handle.join().expect("mock server thread");
        let provider_error = err
            .downcast_ref::<ProviderError>()
            .expect("typed provider error");
        assert_eq!(provider_error.kind(), ProviderErrorKind::CyberPolicy);
        assert_eq!(
            provider_error.to_string(),
            "This request has been flagged for possible cybersecurity risk."
        );
        assert!(!provider_error.is_retryable());
        Ok(())
    }

    #[test]
    fn responses_http_429_rate_limit_is_retry_limit_like_codex() -> Result<()> {
        let body = json!({
            "error": {
                "type": "rate_limit_exceeded",
                "message": "Too many requests."
            }
        })
        .to_string();
        let (base_url, handle) =
            spawn_mock_status_server(429, "Too Many Requests", body, "application/json")?;
        let provider = CodexResponsesProvider::with_base_url(
            CodexAuth {
                access_token: "chatgpt-token".to_string(),
                account_id: "account-123".to_string(),
            },
            "gpt-test",
            format!("{}/backend-api", base_url.trim_end_matches("/v1")),
        );

        let err = provider
            .start_turn(ProviderTurn {
                messages: vec![json!({"role": "user", "content": "finish"})],
                ..ProviderTurn::default()
            })
            .expect_err("HTTP 429 should be terminal after request-level retry handling");
        handle.join().expect("mock server thread");
        let provider_error = err
            .downcast_ref::<ProviderError>()
            .expect("typed provider error");
        assert_eq!(provider_error.kind(), ProviderErrorKind::RetryLimit);
        assert!(!provider_error.is_retryable());
        Ok(())
    }

    #[test]
    fn responses_http_429_usage_limit_reached_carries_rate_limits_like_codex() -> Result<()> {
        let body = json!({
            "error": {
                "type": "usage_limit_reached",
                "message": "server body message should not override Codex display",
                "plan_type": "plus"
            }
        })
        .to_string();
        let (base_url, handle) = spawn_mock_status_sequence_server(vec![MockHttpResponse::new(
            429,
            "Too Many Requests",
            body,
            "application/json",
        )
        .with_header("x-codex-active-limit", "gpt-5")
        .with_header("x-gpt-5-limit-name", "gpt-5")
        .with_header("x-gpt-5-primary-used-percent", "100")
        .with_header("x-gpt-5-primary-window-minutes", "300")
        .with_header("x-gpt-5-primary-reset-at", "1770000000")
        .with_header("x-codex-credits-has-credits", "false")
        .with_header("x-codex-credits-unlimited", "false")])?;
        let provider = CodexResponsesProvider::with_base_url(
            CodexAuth {
                access_token: "chatgpt-token".to_string(),
                account_id: "account-123".to_string(),
            },
            "gpt-test",
            format!("{}/backend-api", base_url.trim_end_matches("/v1")),
        );

        let err = provider
            .start_turn(ProviderTurn {
                messages: vec![json!({"role": "user", "content": "finish"})],
                ..ProviderTurn::default()
            })
            .expect_err("usage_limit_reached HTTP response should fail this turn");
        handle.join().expect("mock server thread");
        let provider_error = err
            .downcast_ref::<ProviderError>()
            .expect("typed provider error");
        assert_eq!(provider_error.kind(), ProviderErrorKind::UsageLimitReached);
        assert_eq!(
            provider_error.to_string(),
            "You've hit your usage limit for gpt-5. Switch to another model now, or try again later."
        );
        let snapshot = provider_error
            .rate_limits()
            .context("usage limit rate limits")?;
        assert_eq!(snapshot.limit_id.as_deref(), Some("gpt_5"));
        assert_eq!(snapshot.limit_name.as_deref(), Some("gpt-5"));
        let primary = snapshot.primary.as_ref().context("primary rate limit")?;
        assert_eq!(primary.used_percent, 100.0);
        assert_eq!(primary.window_minutes, Some(300));
        assert_eq!(primary.resets_at, Some(1770000000));
        let credits = snapshot.credits.as_ref().context("credits snapshot")?;
        assert!(!credits.has_credits);
        assert!(!credits.unlimited);
        assert!(!provider_error.is_retryable());
        Ok(())
    }

    #[test]
    fn responses_http_429_usage_not_included_is_terminal_like_codex() -> Result<()> {
        let body = json!({
            "error": {
                "type": "usage_not_included"
            }
        })
        .to_string();
        let (base_url, handle) =
            spawn_mock_status_server(429, "Too Many Requests", body, "application/json")?;
        let provider = CodexResponsesProvider::with_base_url(
            CodexAuth {
                access_token: "chatgpt-token".to_string(),
                account_id: "account-123".to_string(),
            },
            "gpt-test",
            format!("{}/backend-api", base_url.trim_end_matches("/v1")),
        );

        let err = provider
            .start_turn(ProviderTurn {
                messages: vec![json!({"role": "user", "content": "finish"})],
                ..ProviderTurn::default()
            })
            .expect_err("usage_not_included HTTP response should fail this turn");
        handle.join().expect("mock server thread");
        let provider_error = err
            .downcast_ref::<ProviderError>()
            .expect("typed provider error");
        assert_eq!(provider_error.kind(), ProviderErrorKind::UsageNotIncluded);
        assert!(!provider_error.is_retryable());
        Ok(())
    }

    #[test]
    fn responses_http_500_is_retryable_internal_server_error_like_codex() -> Result<()> {
        let (base_url, handle) = spawn_mock_status_server(
            500,
            "Internal Server Error",
            "temporary failure".to_string(),
            "text/plain",
        )?;
        let mut provider = CodexResponsesProvider::with_base_url(
            CodexAuth {
                access_token: "chatgpt-token".to_string(),
                account_id: "account-123".to_string(),
            },
            "gpt-test",
            format!("{}/backend-api", base_url.trim_end_matches("/v1")),
        );
        provider.request_retry = ProviderRequestRetryConfig::without_retries();

        let err = provider
            .start_turn(ProviderTurn {
                messages: vec![json!({"role": "user", "content": "finish"})],
                ..ProviderTurn::default()
            })
            .expect_err("HTTP 500 should fail this attempt");
        handle.join().expect("mock server thread");
        let provider_error = err
            .downcast_ref::<ProviderError>()
            .expect("typed provider error");
        assert_eq!(
            provider_error.kind(),
            ProviderErrorKind::InternalServerError
        );
        assert!(provider_error.is_retryable());
        Ok(())
    }

    #[test]
    fn responses_http_5xx_retries_inside_provider_like_codex_request_layer() -> Result<()> {
        let completed = "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_after_retry\",\"output\":[],\"usage\":{\"input_tokens\":1,\"output_tokens\":1,\"total_tokens\":2}}}\n\n";
        let (base_url, handle) = spawn_mock_status_sequence_server(vec![
            MockHttpResponse::new(
                500,
                "Internal Server Error",
                "temporary failure",
                "text/plain",
            ),
            MockHttpResponse::new(200, "OK", completed, "text/event-stream"),
        ])?;
        let mut provider = CodexResponsesProvider::with_base_url(
            CodexAuth {
                access_token: "chatgpt-token".to_string(),
                account_id: "account-123".to_string(),
            },
            "gpt-test",
            format!("{}/backend-api", base_url.trim_end_matches("/v1")),
        );
        provider.request_retry = ProviderRequestRetryConfig::without_delay();

        let events = provider.start_turn(ProviderTurn {
            messages: vec![json!({"role": "user", "content": "finish"})],
            ..ProviderTurn::default()
        })?;
        handle.join().expect("mock server thread");

        assert!(events.iter().any(|event| matches!(event, ModelEvent::Done)));
        Ok(())
    }

    #[test]
    fn responses_transport_error_retries_inside_provider_like_codex_request_layer() -> Result<()> {
        let completed = "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_after_transport_retry\",\"output\":[],\"usage\":{\"input_tokens\":1,\"output_tokens\":1,\"total_tokens\":2}}}\n\n";
        let (base_url, handle) = spawn_drop_then_status_server(MockHttpResponse::new(
            200,
            "OK",
            completed,
            "text/event-stream",
        ))?;
        let mut provider = CodexResponsesProvider::with_base_url(
            CodexAuth {
                access_token: "chatgpt-token".to_string(),
                account_id: "account-123".to_string(),
            },
            "gpt-test",
            format!("{}/backend-api", base_url.trim_end_matches("/v1")),
        );
        provider.request_retry = ProviderRequestRetryConfig::without_delay();

        let events = provider.start_turn(ProviderTurn {
            messages: vec![json!({"role": "user", "content": "finish"})],
            ..ProviderTurn::default()
        })?;
        handle.join().expect("mock server thread");

        assert!(events.iter().any(|event| matches!(event, ModelEvent::Done)));
        Ok(())
    }

    #[test]
    fn responses_http_5xx_exhaustion_returns_final_http_error_like_codex() -> Result<()> {
        let (base_url, handle) = spawn_timed_status_sequence_server(
            (0..5)
                .map(|_| {
                    MockHttpResponse::new(
                        500,
                        "Internal Server Error",
                        "temporary failure",
                        "text/plain",
                    )
                })
                .collect(),
        )?;
        let mut provider = CodexResponsesProvider::with_base_url(
            CodexAuth {
                access_token: "chatgpt-token".to_string(),
                account_id: "account-123".to_string(),
            },
            "gpt-test",
            format!("{}/backend-api", base_url.trim_end_matches("/v1")),
        );
        provider.request_retry = ProviderRequestRetryConfig::without_delay();

        let err = provider
            .start_turn(ProviderTurn {
                messages: vec![json!({"role": "user", "content": "finish"})],
                ..ProviderTurn::default()
            })
            .expect_err("exhausted HTTP 500 retries should return final HTTP error");
        let served = handle.join().expect("mock server thread");
        assert_eq!(served, 5);
        let provider_error = err
            .downcast_ref::<ProviderError>()
            .expect("typed provider error");
        assert_eq!(
            provider_error.kind(),
            ProviderErrorKind::InternalServerError
        );
        assert!(provider_error.is_retryable());
        Ok(())
    }

    #[test]
    fn responses_sse_idle_timeout_matches_codex_error() -> Result<()> {
        let (base_url, handle) = spawn_sse_delayed_body_server(Duration::from_millis(200))?;
        let provider = OpenAIResponsesProvider::with_base_url("test-key", "gpt-test", base_url);

        let err = provider
            .start_turn(ProviderTurn {
                messages: vec![json!({"role": "user", "content": "finish"})],
                stream_idle_timeout_ms: Some(50),
                request_max_retries: Some(0),
                ..ProviderTurn::default()
            })
            .expect_err("idle SSE stream should fail like Codex");
        handle.join().expect("mock server thread");
        let provider_error = err
            .downcast_ref::<ProviderError>()
            .expect("typed provider error");
        assert_eq!(provider_error.kind(), ProviderErrorKind::Stream);
        assert_eq!(provider_error.to_string(), "idle timeout waiting for SSE");
        Ok(())
    }

    #[test]
    fn responses_turn_request_max_retries_zero_disables_hidden_retry_like_codex_config(
    ) -> Result<()> {
        let completed = "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_no_retry\",\"output\":[],\"usage\":{\"input_tokens\":1,\"output_tokens\":1,\"total_tokens\":2}}}\n\n";
        let (base_url, handle) = spawn_timed_status_sequence_server(vec![
            MockHttpResponse::new(
                500,
                "Internal Server Error",
                "temporary failure",
                "text/plain",
            ),
            MockHttpResponse::new(200, "OK", completed, "text/event-stream"),
        ])?;
        let provider = CodexResponsesProvider::with_base_url(
            CodexAuth {
                access_token: "chatgpt-token".to_string(),
                account_id: "account-123".to_string(),
            },
            "gpt-test",
            format!("{}/backend-api", base_url.trim_end_matches("/v1")),
        );

        let err = provider
            .start_turn(ProviderTurn {
                request_max_retries: Some(0),
                messages: vec![json!({"role": "user", "content": "finish"})],
                ..ProviderTurn::default()
            })
            .expect_err("request_max_retries=0 should disable hidden retry");
        let served = handle.join().expect("mock server thread");
        let provider_error = err
            .downcast_ref::<ProviderError>()
            .expect("typed provider error");

        assert_eq!(served, 1);
        assert_eq!(
            provider_error.kind(),
            ProviderErrorKind::InternalServerError
        );
        Ok(())
    }

    #[test]
    fn responses_http_400_context_length_stays_invalid_request_like_codex() -> Result<()> {
        let body = json!({
            "error": {
                "code": "context_length_exceeded",
                "message": "Too many tokens."
            }
        })
        .to_string();
        let (base_url, handle) =
            spawn_mock_status_server(400, "Bad Request", body, "application/json")?;
        let provider = CodexResponsesProvider::with_base_url(
            CodexAuth {
                access_token: "chatgpt-token".to_string(),
                account_id: "account-123".to_string(),
            },
            "gpt-test",
            format!("{}/backend-api", base_url.trim_end_matches("/v1")),
        );

        let err = provider
            .start_turn(ProviderTurn {
                messages: vec![json!({"role": "user", "content": "finish"})],
                ..ProviderTurn::default()
            })
            .expect_err("HTTP 400 context body maps through invalid request in Codex");
        handle.join().expect("mock server thread");
        let provider_error = err
            .downcast_ref::<ProviderError>()
            .expect("typed provider error");
        assert_eq!(provider_error.kind(), ProviderErrorKind::InvalidRequest);
        assert!(!provider_error.is_retryable());
        Ok(())
    }

    #[test]
    fn responses_http_400_invalid_image_is_typed_like_codex() -> Result<()> {
        let body = json!({
            "error": {
                "code": "invalid_image",
                "message": "The image data you provided does not represent a valid image."
            }
        })
        .to_string();
        let (base_url, handle) =
            spawn_mock_status_server(400, "Bad Request", body, "application/json")?;
        let provider = CodexResponsesProvider::with_base_url(
            CodexAuth {
                access_token: "chatgpt-token".to_string(),
                account_id: "account-123".to_string(),
            },
            "gpt-test",
            format!("{}/backend-api", base_url.trim_end_matches("/v1")),
        );

        let err = provider
            .start_turn(ProviderTurn {
                messages: vec![json!({"role": "user", "content": "finish"})],
                ..ProviderTurn::default()
            })
            .expect_err("invalid image should be a typed recoverable terminal error");
        handle.join().expect("mock server thread");
        let provider_error = err
            .downcast_ref::<ProviderError>()
            .expect("typed provider error");
        assert_eq!(provider_error.kind(), ProviderErrorKind::InvalidImage);
        assert!(!provider_error.is_retryable());
        Ok(())
    }

    #[test]
    fn response_completed_requires_id_like_codex() -> Result<()> {
        let sse = concat!(
            "data: {\"type\":\"response.completed\",\"response\":{\"output\":[],\"usage\":{\"input_tokens\":1,\"output_tokens\":1,\"total_tokens\":2}}}\n\n",
        );
        let (base_url, handle) = spawn_mock_server(sse.to_string(), "text/event-stream")?;
        let provider = CodexResponsesProvider::with_base_url(
            CodexAuth {
                access_token: "chatgpt-token".to_string(),
                account_id: "account-123".to_string(),
            },
            "gpt-test",
            format!("{}/backend-api", base_url.trim_end_matches("/v1")),
        );
        let err = provider
            .start_turn(ProviderTurn {
                messages: vec![json!({"role": "user", "content": "finish"})],
                ..ProviderTurn::default()
            })
            .expect_err("missing completed response id should fail");
        handle.join().expect("mock server thread");
        assert!(format!("{err:#}").contains("missing field `id`"));
        Ok(())
    }

    #[test]
    fn responses_usage_parses_reasoning_output_tokens_like_codex() -> Result<()> {
        let sse = concat!(
            "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_123\",\"output\":[],\"usage\":{\"input_tokens\":3,\"output_tokens\":9,\"output_tokens_details\":{\"reasoning_tokens\":5},\"total_tokens\":12}}}\n\n",
        );
        let (base_url, handle) = spawn_mock_server(sse.to_string(), "text/event-stream")?;
        let provider = CodexResponsesProvider::with_base_url(
            CodexAuth {
                access_token: "chatgpt-token".to_string(),
                account_id: "account-123".to_string(),
            },
            "gpt-test",
            format!("{}/backend-api", base_url.trim_end_matches("/v1")),
        );
        let events = provider.start_turn(ProviderTurn {
            instructions: None,
            model_settings: ModelRequestSettings::default(),
            messages: vec![json!({"role": "user", "content": "finish"})],
            tools: Vec::new(),
            ..ProviderTurn::default()
        })?;
        handle.join().expect("mock server thread");
        assert!(events.contains(&ModelEvent::Usage {
            usage: ModelUsage {
                input_tokens: Some(3),
                output_tokens: Some(9),
                reasoning_output_tokens: Some(5),
                total_tokens: Some(12),
                cost_usd: None,
                ..Default::default()
            },
        }));
        Ok(())
    }

    #[test]
    fn openai_responses_provider_force_enables_reasoning_summaries_like_codex() -> Result<()> {
        let response_body = json!({
            "id": "resp_123",
            "output": [],
            "usage": {
                "input_tokens": 1,
                "output_tokens": 1,
                "total_tokens": 2
            }
        })
        .to_string();
        let (base_url, body_rx, handle) =
            spawn_request_capture_server(response_body, "application/json")?;
        let provider = OpenAIResponsesProvider::with_base_url("test-key", "gpt-test", base_url);

        provider.start_turn(ProviderTurn {
            instructions: Some("base".to_string()),
            model_settings: ModelRequestSettings {
                reasoning_effort: Some("high".to_string()),
                reasoning_summary: None,
                model_supports_reasoning_summaries: Some(true),
                text_verbosity: Some("high".to_string()),
                service_tier: None,
            },
            messages: vec![json!({"role": "user", "content": "finish"})],
            tools: Vec::new(),
            ..ProviderTurn::default()
        })?;
        handle.join().expect("mock server thread");
        let request = body_rx.recv().expect("request body");

        assert_eq!(request["reasoning"]["effort"], "high");
        assert_eq!(request["reasoning"]["summary"], "auto");
        assert_eq!(request["include"], json!(["reasoning.encrypted_content"]));
        assert!(request.get("text").is_none());
        Ok(())
    }

    #[test]
    fn model_request_info_matches_codex_longest_prefix_and_namespaced_suffix() {
        assert_eq!(
            model_request_info("gpt-5.4-mini-latest").default_verbosity,
            Some("medium".to_string())
        );
        assert_eq!(
            model_request_info("gpt-5.4-custom").default_verbosity,
            Some("low".to_string())
        );
        assert_eq!(
            model_request_info("openai/gpt-5.3-codex").default_reasoning_effort,
            Some("medium".to_string())
        );
        for model in [
            "gpt-5.5",
            "gpt-5.4",
            "gpt-5.4-mini",
            "gpt-5.3-codex",
            "gpt-5.2",
            "codex-auto-review",
        ] {
            assert!(
                model_request_info(model).supports_parallel_tool_calls,
                "{model} should match Codex bundled catalog"
            );
        }
        assert!(!model_request_info("openai/nested/gpt-5.5").supports_reasoning_summaries);
        assert!(model_request_info("gpt-5.2-codex").supports_reasoning_summaries);
        assert!(!model_supports_personality("gpt-5.2-codex"));
        assert!(model_supports_personality("exp-codex-personality"));
    }

    #[test]
    fn model_switch_request_settings_clamp_reasoning_effort_like_codex() {
        let unsupported = ModelRequestSettings {
            reasoning_effort: Some("minimal".to_string()),
            reasoning_summary: Some("none".to_string()),
            model_supports_reasoning_summaries: None,
            text_verbosity: Some("high".to_string()),
            service_tier: Some("priority".to_string()),
        };
        let clamped = model_switch_request_settings_for_model("gpt-5.4", &unsupported);
        assert_eq!(clamped.reasoning_effort.as_deref(), Some("medium"));
        assert_eq!(clamped.reasoning_summary.as_deref(), Some("none"));
        assert_eq!(clamped.text_verbosity.as_deref(), Some("high"));
        assert_eq!(clamped.service_tier.as_deref(), Some("priority"));

        let supported = ModelRequestSettings {
            reasoning_effort: Some("xhigh".to_string()),
            ..ModelRequestSettings::default()
        };
        let preserved = model_switch_request_settings_for_model("gpt-5.4", &supported);
        assert_eq!(preserved.reasoning_effort.as_deref(), Some("xhigh"));

        let defaulted =
            model_switch_request_settings_for_model("openai/gpt-5.3-codex", &Default::default());
        assert_eq!(defaulted.reasoning_effort.as_deref(), Some("medium"));

        let unsupported_service_tier = ModelRequestSettings {
            service_tier: Some("priority".to_string()),
            ..ModelRequestSettings::default()
        };
        let cleared =
            model_switch_request_settings_for_model("gpt-5.3-codex", &unsupported_service_tier);
        assert_eq!(cleared.service_tier, None);

        let unknown = ModelRequestSettings {
            reasoning_effort: Some("high".to_string()),
            ..ModelRequestSettings::default()
        };
        let unsupported_unknown = model_switch_request_settings_for_model("custom-model", &unknown);
        assert_eq!(unsupported_unknown.reasoning_effort, None);
        assert_eq!(unsupported_unknown.service_tier, None);
    }

    #[test]
    fn codex_responses_provider_keeps_default_low_verbosity_and_can_disable_summary() -> Result<()>
    {
        let response_body = "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_123\",\"output\":[],\"usage\":{\"input_tokens\":1,\"output_tokens\":1,\"total_tokens\":2}}}\n\n".to_string();
        let (base_url, body_rx, handle) =
            spawn_request_capture_server(response_body, "text/event-stream")?;
        let provider = CodexResponsesProvider::with_base_url(
            CodexAuth {
                access_token: "chatgpt-token".to_string(),
                account_id: "account-123".to_string(),
            },
            "gpt-5.5",
            format!("{}/backend-api", base_url.trim_end_matches("/v1")),
        );

        provider.start_turn(ProviderTurn {
            instructions: Some("base".to_string()),
            model_settings: ModelRequestSettings {
                reasoning_effort: Some("minimal".to_string()),
                reasoning_summary: Some("none".to_string()),
                model_supports_reasoning_summaries: None,
                text_verbosity: None,
                service_tier: None,
            },
            messages: vec![json!({"role": "user", "content": "finish"})],
            tools: Vec::new(),
            ..ProviderTurn::default()
        })?;
        handle.join().expect("mock server thread");
        let request = body_rx.recv().expect("request body");

        assert_eq!(request["reasoning"]["effort"], "minimal");
        assert!(request["reasoning"].get("summary").is_none());
        assert_eq!(request["include"], json!(["reasoning.encrypted_content"]));
        assert_eq!(request["text"]["verbosity"], "low");
        assert_eq!(request["parallel_tool_calls"], true);
        Ok(())
    }

    #[test]
    fn default_instructions_preserve_bitter_cdp_browser_harness_contract() {
        let instructions = default_instructions();
        assert!(instructions.starts_with("You are Browser Use Terminal, a web agent"));
        for expected in [
            "describe your web-browsing abilities first",
            "You are Codex, a coding agent based on GPT-5",
            "deeply pragmatic, effective software engineer",
            "As an expert coding agent",
            "Persist until the task is fully handled end-to-end",
            "Always use apply_patch for manual code edits",
            "NEVER revert existing changes you did not make",
            "**NEVER** use destructive commands like `git reset --hard`",
            "If the user asks for a \"review\", default to a code review mindset",
            "bitter lesson",
            "Raw CDP is the center",
            "source of truth",
            "new_tab(url)",
            "not `goto_url(url)`",
            "Prefer coordinate clicks",
            "Chrome hit-testing handles iframes",
            "Never bulk-fill a live form by setting DOM values",
            "screenshot(\"label\")",
            "input_image",
            "agent_helpers.py",
            "Browser interaction tool",
            "Loaded Browser-Harness Interaction Skills",
            "Use helper agents only when the user explicitly asks",
            "detailed codebase analysis do not by themselves authorize spawning",
            "interaction-skills/screenshots.md",
            "interaction-skills/tabs.md",
            "interaction-skills/dialogs.md",
            "interaction-skills/forms.md",
            "JS may inspect forms; browser input actions mutate forms",
            "Do not build manager layers",
            "Do not import or install Playwright",
        ] {
            assert!(
                instructions.contains(expected),
                "missing {expected:?} from default instructions:\n{instructions}"
            );
        }
        let permissions = default_permissions_instructions();
        assert!(permissions.contains("<permissions instructions>"));
        assert!(permissions.contains("`sandbox_mode` is `danger-full-access`"));
        assert!(permissions.contains("Approval policy is currently never"));
        assert!(permissions.contains("Do not provide the `sandbox_permissions` for any reason"));
        assert!(!instructions.contains("spawn a read-only helper with role `explorer` unless"));
    }

    #[test]
    fn default_instructions_render_codex_personality_variants() {
        let pragmatic = default_agent_instructions_for_personality(ModelPersonality::Pragmatic);
        assert!(pragmatic.contains("deeply pragmatic, effective software engineer"));
        assert!(!pragmatic.contains("supportive teammate as much as code quality"));

        let friendly = default_agent_instructions_for_personality(ModelPersonality::Friendly);
        assert!(friendly.contains("supportive teammate as much as code quality"));
        assert!(!friendly.contains("deeply pragmatic, effective software engineer"));

        let none = default_agent_instructions_for_personality(ModelPersonality::None);
        assert!(none.contains("You are Codex, a coding agent based on GPT-5"));
        assert!(!none.contains("supportive teammate as much as code quality"));
        assert!(!none.contains("deeply pragmatic, effective software engineer"));
        assert!(none.contains("Browser Agent Contract"));

        let unsupported = default_agent_instructions_for_model_and_personality(
            "custom-model",
            ModelPersonality::Friendly,
        );
        assert!(unsupported
            .contains("You are a coding agent running in a terminal-based agent harness"));
        assert!(!unsupported.contains("Codex CLI is an open source project led by OpenAI"));
        assert!(!unsupported.contains("supportive teammate as much as code quality"));

        let namespaced = default_agent_instructions_for_model_and_personality(
            "openai/gpt-5.4",
            ModelPersonality::Friendly,
        );
        assert!(namespaced.contains("supportive teammate as much as code quality"));
        assert!(model_supports_personality("gpt-5.4"));
        assert!(model_supports_personality("openai/gpt-5.4"));
        assert!(!model_supports_personality("gpt-5.2"));
        assert!(!model_supports_personality("gpt-5.2-codex"));
        assert!(model_supports_personality("exp-codex-personality"));
    }

    #[test]
    fn local_model_messages_follow_codex_template_semantics() {
        let messages = LocalModelMessages {
            instructions_template: Some("Hello {{ personality }} {{ base_instructions }}"),
            instructions_variables: Some(LocalModelInstructionsVariables {
                personality_default: Some("default"),
                personality_friendly: Some("friendly"),
                personality_pragmatic: None,
            }),
        };

        assert!(!messages.supports_personality());
        assert_eq!(
            messages.get_model_instructions(Some(ModelPersonality::Friendly), "base"),
            "Hello friendly base"
        );
        assert_eq!(
            messages.get_model_instructions(Some(ModelPersonality::Pragmatic), "base"),
            "Hello  base"
        );
        assert_eq!(
            messages.get_model_instructions(Some(ModelPersonality::None), "base"),
            "Hello  base"
        );
        assert_eq!(
            messages.get_model_instructions(None, "base"),
            "Hello default base"
        );

        let no_template = LocalModelMessages {
            instructions_template: None,
            instructions_variables: Some(LocalModelInstructionsVariables {
                personality_default: Some("default"),
                personality_friendly: Some("friendly"),
                personality_pragmatic: Some("pragmatic"),
            }),
        };
        assert_eq!(
            no_template.get_model_instructions(Some(ModelPersonality::Friendly), "base"),
            "base"
        );
    }

    fn spawn_mock_server(
        body: String,
        content_type: &'static str,
    ) -> Result<(String, thread::JoinHandle<()>)> {
        spawn_mock_status_server(200, "OK", body, content_type)
    }

    fn spawn_mock_status_server(
        status: u16,
        reason: &'static str,
        body: String,
        content_type: &'static str,
    ) -> Result<(String, thread::JoinHandle<()>)> {
        spawn_mock_status_sequence_server(vec![MockHttpResponse::new(
            status,
            reason,
            body,
            content_type,
        )])
    }

    struct MockHttpResponse {
        status: u16,
        reason: &'static str,
        body: String,
        content_type: Option<&'static str>,
        headers: Vec<(&'static str, &'static str)>,
    }

    impl MockHttpResponse {
        fn new(
            status: u16,
            reason: &'static str,
            body: impl Into<String>,
            content_type: &'static str,
        ) -> Self {
            Self {
                status,
                reason,
                body: body.into(),
                content_type: Some(content_type),
                headers: Vec::new(),
            }
        }

        fn without_content_type(mut self) -> Self {
            self.content_type = None;
            self
        }

        fn with_header(mut self, name: &'static str, value: &'static str) -> Self {
            self.headers.push((name, value));
            self
        }
    }

    fn mock_http_response_bytes(response: &MockHttpResponse) -> String {
        let content_type_header = response
            .content_type
            .map(|content_type| format!("Content-Type: {content_type}\r\n"))
            .unwrap_or_default();
        let extra_headers = response
            .headers
            .iter()
            .map(|(name, value)| format!("{name}: {value}\r\n"))
            .collect::<String>();
        format!(
            "HTTP/1.1 {} {}\r\n{}{}Content-Length: {}\r\nConnection: close\r\n\r\n{}",
            response.status,
            response.reason,
            content_type_header,
            extra_headers,
            response.body.len(),
            response.body
        )
    }

    fn spawn_mock_status_sequence_server(
        responses: Vec<MockHttpResponse>,
    ) -> Result<(String, thread::JoinHandle<()>)> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let addr = listener.local_addr()?;
        let handle = thread::spawn(move || {
            for response in responses {
                let (mut stream, _) = listener.accept().expect("accept request");
                let mut request = Vec::new();
                let mut buf = [0_u8; 1024];
                loop {
                    let read = stream.read(&mut buf).expect("read request");
                    if read == 0 {
                        break;
                    }
                    request.extend_from_slice(&buf[..read]);
                    if request.windows(4).any(|window| window == b"\r\n\r\n") {
                        break;
                    }
                }
                let request_text = String::from_utf8_lossy(&request);
                let request_text_lower = request_text.to_ascii_lowercase();
                assert!(
                    request_text.starts_with("POST /v1/responses")
                        || request_text.starts_with("POST /backend-api/codex/responses")
                        || request_text.starts_with("POST /v1/chat/completions")
                        || request_text.starts_with("POST /v1/messages")
                );
                assert!(
                    request_text_lower.contains("authorization: bearer test-key")
                        || request_text_lower.contains("authorization: bearer chatgpt-token")
                        || request_text_lower.contains("authorization: bearer claude-oauth-token")
                        || request_text_lower.contains("x-api-key: anthropic-key")
                );
                let wire_response = mock_http_response_bytes(&response);
                stream
                    .write_all(wire_response.as_bytes())
                    .expect("write response");
            }
        });
        Ok((format!("http://{addr}/v1"), handle))
    }

    fn spawn_timed_status_sequence_server(
        responses: Vec<MockHttpResponse>,
    ) -> Result<(String, thread::JoinHandle<usize>)> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        listener.set_nonblocking(true)?;
        let addr = listener.local_addr()?;
        let handle = thread::spawn(move || {
            let mut served = 0;
            for response in responses {
                let start = Instant::now();
                let (mut stream, _) = loop {
                    match listener.accept() {
                        Ok(accepted) => break accepted,
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            if start.elapsed() > Duration::from_secs(2) {
                                return served;
                            }
                            thread::sleep(Duration::from_millis(5));
                        }
                        Err(error) => panic!("accept request: {error}"),
                    }
                };
                read_request_headers(&mut stream);
                let wire_response = mock_http_response_bytes(&response);
                stream
                    .write_all(wire_response.as_bytes())
                    .expect("write response");
                served += 1;
            }
            served
        });
        Ok((format!("http://{addr}/v1"), handle))
    }

    fn spawn_drop_then_status_server(
        response: MockHttpResponse,
    ) -> Result<(String, thread::JoinHandle<()>)> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let addr = listener.local_addr()?;
        let handle = thread::spawn(move || {
            let (mut first_stream, _) = listener.accept().expect("accept dropped request");
            read_request_headers(&mut first_stream);
            drop(first_stream);

            let (mut stream, _) = listener.accept().expect("accept retry request");
            read_request_headers(&mut stream);
            let wire_response = mock_http_response_bytes(&response);
            stream
                .write_all(wire_response.as_bytes())
                .expect("write response");
        });
        Ok((format!("http://{addr}/v1"), handle))
    }

    fn spawn_incomplete_body_then_status_server(
        response: MockHttpResponse,
    ) -> Result<(String, thread::JoinHandle<()>)> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let addr = listener.local_addr()?;
        let handle = thread::spawn(move || {
            let (mut first_stream, _) = listener.accept().expect("accept incomplete-body request");
            read_request_headers(&mut first_stream);
            let incomplete_response = concat!(
                "HTTP/1.1 200 OK\r\n",
                "Content-Type: application/json\r\n",
                "Content-Length: 64\r\n",
                "Connection: close\r\n\r\n",
                "{"
            );
            first_stream
                .write_all(incomplete_response.as_bytes())
                .expect("write incomplete response");
            drop(first_stream);

            let (mut stream, _) = listener.accept().expect("accept retry request");
            read_request_headers(&mut stream);
            let wire_response = mock_http_response_bytes(&response);
            stream
                .write_all(wire_response.as_bytes())
                .expect("write response");
        });
        Ok((format!("http://{addr}/v1"), handle))
    }

    fn spawn_sse_delayed_body_server(delay: Duration) -> Result<(String, thread::JoinHandle<()>)> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let addr = listener.local_addr()?;
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept request");
            read_request_headers(&mut stream);
            let headers = concat!(
                "HTTP/1.1 200 OK\r\n",
                "Content-Type: text/event-stream\r\n",
                "Connection: close\r\n\r\n"
            );
            stream
                .write_all(headers.as_bytes())
                .expect("write response headers");
            thread::sleep(delay);
            let body = "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_late\",\"output\":[],\"usage\":{\"input_tokens\":1,\"output_tokens\":1,\"total_tokens\":2}}}\n\n";
            let _ = stream.write_all(body.as_bytes());
        });
        Ok((format!("http://{addr}/v1"), handle))
    }

    fn read_request_headers(stream: &mut TcpStream) {
        let mut request = Vec::new();
        let mut buf = [0_u8; 1024];
        let start = Instant::now();
        loop {
            match stream.read(&mut buf) {
                Ok(0) => break,
                Ok(read) => {
                    request.extend_from_slice(&buf[..read]);
                    if request.windows(4).any(|window| window == b"\r\n\r\n") {
                        break;
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    if start.elapsed() > Duration::from_secs(2) {
                        panic!("read request timed out");
                    }
                    thread::sleep(Duration::from_millis(5));
                }
                Err(error) => panic!("read request: {error}"),
            }
        }
        let request_text = String::from_utf8_lossy(&request);
        assert!(
            request_text.starts_with("POST /v1/responses")
                || request_text.starts_with("POST /backend-api/codex/responses")
                || request_text.starts_with("POST /v1/chat/completions")
                || request_text.starts_with("POST /v1/messages")
        );
    }

    fn spawn_request_capture_server(
        body: String,
        content_type: &'static str,
    ) -> Result<(String, mpsc::Receiver<Value>, thread::JoinHandle<()>)> {
        spawn_request_capture_server_sequence(vec![body], content_type)
    }

    fn spawn_request_capture_server_sequence(
        bodies: Vec<String>,
        content_type: &'static str,
    ) -> Result<(String, mpsc::Receiver<Value>, thread::JoinHandle<()>)> {
        spawn_request_capture_http_sequence(
            bodies
                .into_iter()
                .map(|body| MockHttpResponse::new(200, "OK", body, content_type))
                .collect(),
        )
    }

    fn spawn_request_header_capture_server(
        response: MockHttpResponse,
    ) -> Result<(String, mpsc::Receiver<String>, thread::JoinHandle<()>)> {
        spawn_request_header_capture_server_sequence(vec![response])
    }

    fn spawn_request_header_capture_server_sequence(
        responses: Vec<MockHttpResponse>,
    ) -> Result<(String, mpsc::Receiver<String>, thread::JoinHandle<()>)> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let addr = listener.local_addr()?;
        let (tx, rx) = mpsc::channel();
        let handle = thread::spawn(move || {
            for response in responses {
                let (mut stream, _) = listener.accept().expect("accept request");
                let mut request = Vec::new();
                let mut buf = [0_u8; 1024];
                let header_end = loop {
                    let read = stream.read(&mut buf).expect("read request");
                    if read == 0 {
                        panic!("request ended before headers");
                    }
                    request.extend_from_slice(&buf[..read]);
                    if let Some(pos) = request
                        .windows(4)
                        .position(|window| window == b"\r\n\r\n")
                        .map(|pos| pos + 4)
                    {
                        break pos;
                    }
                };
                let headers = String::from_utf8_lossy(&request[..header_end]).to_string();
                assert!(
                    headers.starts_with("POST /v1/responses")
                        || headers.starts_with("POST /backend-api/codex/responses")
                        || headers.starts_with("POST /v1/messages")
                );
                let request_text_lower = headers.to_ascii_lowercase();
                let content_length = request_text_lower
                    .lines()
                    .find_map(|line| line.strip_prefix("content-length: "))
                    .and_then(|value| value.trim().parse::<usize>().ok())
                    .expect("content-length header");
                while request.len() < header_end + content_length {
                    let read = stream.read(&mut buf).expect("read body");
                    if read == 0 {
                        break;
                    }
                    request.extend_from_slice(&buf[..read]);
                }
                tx.send(headers).expect("send request headers");
                let wire_response = mock_http_response_bytes(&response);
                stream
                    .write_all(wire_response.as_bytes())
                    .expect("write response");
            }
        });
        Ok((format!("http://{addr}/v1"), rx, handle))
    }

    fn codex_completed_sse(response_id: &str) -> String {
        format!(
            "data: {{\"type\":\"response.completed\",\"response\":{{\"id\":\"{response_id}\",\"output\":[],\"usage\":{{\"input_tokens\":1,\"output_tokens\":1,\"total_tokens\":2}}}}}}\n\n"
        )
    }

    fn write_codex_auth_file(
        path: &Path,
        access_token: &str,
        account_id: &str,
        refresh_token: &str,
    ) -> Result<()> {
        std::fs::write(
            path,
            serde_json::to_string_pretty(&json!({
                "auth_mode": "chatgpt",
                "tokens": {
                    "id_token": "header.payload.signature",
                    "access_token": access_token,
                    "refresh_token": refresh_token,
                    "account_id": account_id
                },
                "last_refresh": "2026-05-25T00:00:00Z"
            }))?,
        )?;
        Ok(())
    }

    fn spawn_codex_refresh_capture_server(
        body: String,
        status: u16,
    ) -> Result<(String, mpsc::Receiver<Value>, thread::JoinHandle<()>)> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let addr = listener.local_addr()?;
        let (tx, rx) = mpsc::channel();
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept refresh request");
            let mut request = Vec::new();
            let mut buf = [0_u8; 1024];
            let header_end = loop {
                let read = stream.read(&mut buf).expect("read refresh request");
                if read == 0 {
                    panic!("refresh request ended before headers");
                }
                request.extend_from_slice(&buf[..read]);
                if let Some(pos) = request
                    .windows(4)
                    .position(|window| window == b"\r\n\r\n")
                    .map(|pos| pos + 4)
                {
                    break pos;
                }
            };
            let headers = String::from_utf8_lossy(&request[..header_end]);
            assert!(headers.starts_with("POST /oauth/token"));
            assert!(headers
                .to_ascii_lowercase()
                .contains("\r\ncontent-type: application/x-www-form-urlencoded"));
            let content_length = headers
                .to_ascii_lowercase()
                .lines()
                .find_map(|line| line.strip_prefix("content-length: "))
                .and_then(|value| value.trim().parse::<usize>().ok())
                .expect("content-length header");
            while request.len() < header_end + content_length {
                let read = stream.read(&mut buf).expect("read refresh body");
                if read == 0 {
                    break;
                }
                request.extend_from_slice(&buf[..read]);
            }
            let request_text =
                String::from_utf8_lossy(&request[header_end..header_end + content_length]);
            let request_body = Value::Object(
                parse_form_pairs(&request_text)
                    .into_iter()
                    .map(|(key, value)| (key, Value::String(value)))
                    .collect(),
            );
            tx.send(request_body).expect("send refresh body");
            let response = format!(
                "HTTP/1.1 {status} OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            stream
                .write_all(response.as_bytes())
                .expect("write refresh response");
        });
        Ok((format!("http://{addr}/oauth/token"), rx, handle))
    }

    fn spawn_request_capture_http_sequence(
        responses: Vec<MockHttpResponse>,
    ) -> Result<(String, mpsc::Receiver<Value>, thread::JoinHandle<()>)> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let addr = listener.local_addr()?;
        let (tx, rx) = mpsc::channel();
        let handle = thread::spawn(move || {
            for response in responses {
                let (mut stream, _) = listener.accept().expect("accept request");
                let mut request = Vec::new();
                let mut buf = [0_u8; 1024];
                let header_end = loop {
                    let read = stream.read(&mut buf).expect("read request");
                    if read == 0 {
                        panic!("request ended before headers");
                    }
                    request.extend_from_slice(&buf[..read]);
                    if let Some(pos) = request
                        .windows(4)
                        .position(|window| window == b"\r\n\r\n")
                        .map(|pos| pos + 4)
                    {
                        break pos;
                    }
                };
                let headers = String::from_utf8_lossy(&request[..header_end]);
                let request_text_lower = headers.to_ascii_lowercase();
                let content_length = request_text_lower
                    .lines()
                    .find_map(|line| line.strip_prefix("content-length: "))
                    .and_then(|value| value.trim().parse::<usize>().ok())
                    .expect("content-length header");
                while request.len() < header_end + content_length {
                    let read = stream.read(&mut buf).expect("read body");
                    if read == 0 {
                        break;
                    }
                    request.extend_from_slice(&buf[..read]);
                }
                let body_start = header_end;
                let body_end = header_end + content_length;
                let request_body: Value = serde_json::from_slice(&request[body_start..body_end])
                    .expect("json request body");
                tx.send(request_body).expect("send request body");
                let wire_response = mock_http_response_bytes(&response);
                stream
                    .write_all(wire_response.as_bytes())
                    .expect("write response");
            }
        });
        Ok((format!("http://{addr}/v1"), rx, handle))
    }
}
