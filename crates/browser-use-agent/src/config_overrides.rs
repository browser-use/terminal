//! Config-override parsing + run-config construction types (Phase-D leaf port).
//!
//! Ported faithfully from the legacy `browser-use-core` so the TUI/CLI can build
//! the same provider/run configuration before driving a session on the new async
//! engine. The parse semantics and every default are byte-for-byte equivalent to
//! the originals — these values drive provider selection and must not drift.
//!
//! Source of truth (`crates/browser-use-core/src`):
//! - `parse_config_overrides` + helpers: `config_overrides.rs:9-47`
//! - `ProviderBackend`: `lib.rs:126-134` (redefined locally — the agent crate
//!   must not depend on `browser-use-core`)
//! - `RunConfigValueSource`: `lib.rs:136-140`
//! - `ProviderRunConfig` (+ impl): `lib.rs:142-176`
//! - `ConfigOverrides` alias: `lib.rs:178`
//! - `AgentRunOptions` (+ Default + builders): `lib.rs:202-280` / `380-489`
//! - `build_config_overrides_layer` / `apply_toml_override`: `lib.rs:14372-14445`
//! - `EnvironmentContext*` / `ChildAgentRun*`: `lib.rs:138-168` / `226-255`
//!
//! `CollaborationModeKind` is intentionally NOT redefined here — it already lives
//! in [`crate::prompts`] and is reused so the two engines agree on the mode set.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::OnceLock;

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::decision::AutoCompactTokenLimitScope;
use crate::mcp::McpServerConfig;
use crate::prompts::CollaborationModeKind;
use crate::subagents::role::{AgentRoleConfig, RoleOverrides};
use crate::tools::AskForApproval;

/// Legacy `browser-use-core::constants::DEFAULT_MAX_CONTEXT_CHARS`
/// (`constants.rs:9`). Reproduced verbatim so [`AgentRunOptions::default`]
/// matches the legacy engine exactly.
pub const DEFAULT_MAX_CONTEXT_CHARS: usize = 240_000;

/// Codex MultiAgentV2 defaults. These mirror Codex's `MultiAgentV2Config`
/// values, with v2 enabled by default for this terminal's model-visible toolset.
pub const DEFAULT_MULTI_AGENT_V2_MAX_CONCURRENT_THREADS_PER_SESSION: usize = 4;
pub const DEFAULT_MULTI_AGENT_V2_MIN_WAIT_TIMEOUT_MS: i64 = 1;
pub const DEFAULT_MULTI_AGENT_V2_MAX_WAIT_TIMEOUT_MS: i64 = 3_600_000;
pub const DEFAULT_MULTI_AGENT_V2_DEFAULT_WAIT_TIMEOUT_MS: i64 = 300_000;
pub const HARD_MIN_MULTI_AGENT_V2_TIMEOUT_MS: i64 = 1;
pub const HARD_MAX_MULTI_AGENT_V2_TIMEOUT_MS: i64 = DEFAULT_MULTI_AGENT_V2_MAX_WAIT_TIMEOUT_MS;

/// Parsed CLI/TUI `--config key=value` overrides: an ordered list of dotted TOML
/// paths paired with their parsed values.
///
/// Mirrors `browser-use-core` `pub type ConfigOverrides = Vec<(String, toml::Value)>;`
/// (`lib.rs:178`).
pub type ConfigOverrides = Vec<(String, toml::Value)>;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MultiAgentV2Options {
    pub enabled: bool,
    pub max_concurrent_threads_per_session: usize,
    pub min_wait_timeout_ms: i64,
    pub max_wait_timeout_ms: i64,
    pub default_wait_timeout_ms: i64,
    pub usage_hint_enabled: bool,
    pub usage_hint_text: Option<String>,
    pub root_agent_usage_hint_text: Option<String>,
    pub subagent_usage_hint_text: Option<String>,
    pub tool_namespace: Option<String>,
    pub hide_spawn_agent_metadata: bool,
    pub non_code_mode_only: bool,
}

impl Default for MultiAgentV2Options {
    fn default() -> Self {
        Self {
            enabled: true,
            max_concurrent_threads_per_session:
                DEFAULT_MULTI_AGENT_V2_MAX_CONCURRENT_THREADS_PER_SESSION,
            min_wait_timeout_ms: DEFAULT_MULTI_AGENT_V2_MIN_WAIT_TIMEOUT_MS,
            max_wait_timeout_ms: DEFAULT_MULTI_AGENT_V2_MAX_WAIT_TIMEOUT_MS,
            default_wait_timeout_ms: DEFAULT_MULTI_AGENT_V2_DEFAULT_WAIT_TIMEOUT_MS,
            usage_hint_enabled: true,
            usage_hint_text: None,
            root_agent_usage_hint_text: None,
            subagent_usage_hint_text: None,
            tool_namespace: None,
            hide_spawn_agent_metadata: false,
            non_code_mode_only: false,
        }
    }
}

/// The provider backend a run targets.
///
/// Redefined locally (the agent crate must not depend on `browser-use-core`);
/// kept in lock-step with `browser-use-core::ProviderBackend` (`lib.rs:126`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProviderBackend {
    Codex,
    BrowserUse,
    Openai,
    Anthropic,
    Google,
    Openrouter,
    Deepseek,
    Fake,
    None,
}

impl ProviderBackend {
    pub fn from_provider_id(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "codex" => Some(Self::Codex),
            "browser-use" | "browser_use" | "browseruse" => Some(Self::BrowserUse),
            "openai" => Some(Self::Openai),
            "anthropic" => Some(Self::Anthropic),
            "google" | "gemini" => Some(Self::Google),
            "openrouter" => Some(Self::Openrouter),
            "deepseek" => Some(Self::Deepseek),
            "fake" => Some(Self::Fake),
            "none" => Some(Self::None),
            _ => None,
        }
    }
}

/// Whether a run-config value was set explicitly or fell back to a default.
///
/// Mirrors `browser-use-core::RunConfigValueSource` (`lib.rs:136`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RunConfigValueSource {
    Explicit,
    Default,
}

/// A single named environment the agent may operate in.
///
/// Mirrors `browser-use-core::EnvironmentContextEnvironment` (`lib.rs:138`).
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct EnvironmentContextEnvironment {
    pub id: String,
    pub cwd: String,
    pub shell: String,
}

impl EnvironmentContextEnvironment {
    pub fn new(id: impl Into<String>, cwd: impl Into<String>, shell: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            cwd: cwd.into(),
            shell: shell.into(),
        }
    }
}

/// Network allow/deny lists surfaced in the environment context.
///
/// Mirrors `browser-use-core::EnvironmentNetworkContext` (`lib.rs:155`).
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
pub struct EnvironmentNetworkContext {
    pub allowed_domains: Vec<String>,
    pub denied_domains: Vec<String>,
}

impl EnvironmentNetworkContext {
    pub fn new(allowed_domains: Vec<String>, denied_domains: Vec<String>) -> Self {
        Self {
            allowed_domains,
            denied_domains,
        }
    }
}

/// A request to spawn a child agent, carried through [`ChildAgentRunner`].
///
/// Mirrors `browser-use-core::ChildAgentRunRequest` (`lib.rs:226`).
#[derive(Clone, Debug)]
pub struct ChildAgentRunRequest {
    pub parent_session_id: String,
    pub child_session_id: String,
    pub run_id: Option<String>,
    pub message: String,
    pub input_items: Option<serde_json::Value>,
    pub input_is_inter_agent_communication: bool,
    pub agent_path: Option<String>,
    pub nickname: Option<String>,
    pub role: Option<String>,
    pub fork_turns: Option<String>,
    pub model: Option<String>,
    pub reasoning_effort: Option<String>,
    pub service_tier: Option<String>,
    pub config_overrides: Vec<(String, toml::Value)>,
    pub completion_handler: Option<ChildAgentCompletionHandler>,
}

/// Terminal status reported by a child agent back to its parent run.
#[derive(Clone, Debug)]
pub struct ChildAgentRunCompletion {
    pub success: bool,
    pub summary: Option<String>,
}

impl ChildAgentRunCompletion {
    pub fn success(summary: impl Into<Option<String>>) -> Self {
        Self {
            success: true,
            summary: summary.into(),
        }
    }

    pub fn failure(error: impl Into<String>) -> Self {
        Self {
            success: false,
            summary: Some(error.into()),
        }
    }
}

/// Opaque, cloneable callback used by child runners to notify parent runs.
#[derive(Clone)]
pub struct ChildAgentCompletionHandler {
    notify: Arc<dyn Fn(ChildAgentRunCompletion) -> Result<()> + Send + Sync>,
}

impl std::fmt::Debug for ChildAgentCompletionHandler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChildAgentCompletionHandler")
            .finish_non_exhaustive()
    }
}

impl ChildAgentCompletionHandler {
    pub fn new(
        notify: impl Fn(ChildAgentRunCompletion) -> Result<()> + Send + Sync + 'static,
    ) -> Self {
        Self {
            notify: Arc::new(notify),
        }
    }

    pub fn notify(&self, completion: ChildAgentRunCompletion) -> Result<()> {
        (self.notify)(completion)
    }
}

/// Opaque, cloneable callback used to launch child agents.
///
/// Mirrors `browser-use-core::ChildAgentRunner` (`lib.rs:236`).
#[derive(Clone)]
pub struct ChildAgentRunner {
    run: Arc<dyn Fn(ChildAgentRunRequest) -> Result<()> + Send + Sync>,
}

impl std::fmt::Debug for ChildAgentRunner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChildAgentRunner").finish_non_exhaustive()
    }
}

impl ChildAgentRunner {
    pub fn new(run: impl Fn(ChildAgentRunRequest) -> Result<()> + Send + Sync + 'static) -> Self {
        Self { run: Arc::new(run) }
    }

    pub fn run(&self, request: ChildAgentRunRequest) -> Result<()> {
        (self.run)(request)
    }
}

/// Per-run knobs the TUI/CLI assemble before starting a session.
///
/// Mirrors `browser-use-core::AgentRunOptions` (`lib.rs:202`), including every
/// field and the exact default values.
#[derive(Clone, Debug)]
pub struct AgentRunOptions {
    pub max_turns: usize,
    pub max_context_chars: usize,
    pub browser_mode: Option<String>,
    pub dynamic_browser_mode_from_store: bool,
    pub browser_profile_id: Option<String>,
    pub browser_profile_label: Option<String>,
    pub browser_local_browser: Option<String>,
    pub collaboration_mode: CollaborationModeKind,
    pub include_environment_context: bool,
    pub include_permissions_instructions: bool,
    pub environment_context_environments: Vec<EnvironmentContextEnvironment>,
    pub environment_context_network: Option<EnvironmentNetworkContext>,
    pub config_profile: Option<String>,
    pub config_overrides: ConfigOverrides,
    pub session_thread_config: Option<toml::Value>,
    pub base_instructions: Option<String>,
    pub developer_instructions: Option<String>,
    pub compact_prompt: Option<String>,
    pub model_provider_id: Option<String>,
    pub model_provider_id_source: RunConfigValueSource,
    pub model_stream_idle_timeout_ms: Option<u64>,
    pub python_tool_timeout_seconds: u64,
    pub python_env: Vec<(String, String)>,
    pub child_agent_runner: Option<ChildAgentRunner>,
    pub final_output_json_schema: Option<Value>,
    pub final_output_json_schema_strict: bool,
    pub model_compaction_enabled: bool,
    pub model_auto_compact_token_limit: Option<i64>,
    pub model_auto_compact_token_limit_scope: AutoCompactTokenLimitScope,
    pub analytics_source: Option<String>,
    pub analytics_provider_kind: Option<String>,
    pub analytics_model: Option<String>,
    /// Persist exact provider input (system/messages/tool schemas) in
    /// `model.turn.request` events. Default `false` keeps local CLI/TUI history
    /// compact and avoids duplicating screenshots/prompt text every turn.
    pub full_llm_input_events: bool,
    /// MCP servers to connect to and expose via the model-callable `mcp` tool.
    ///
    /// Empty (the default) registers no `mcp` tool, preserving prior behavior.
    /// Each entry maps a logical server name (the `{server}` segment of an
    /// `mcp__{server}__{tool}` call) to its launch config. Populated by the
    /// TUI/CLI from a `[mcp_servers]` config table or explicit MCP config file.
    pub mcp_servers: HashMap<String, McpServerConfig>,
    /// How aggressively the agent asks before running a gated tool call.
    ///
    /// Drives the production tool dispatcher's approval routing
    /// ([`crate::entrypoint::provider::build_tool_dispatcher`]): the default
    /// [`AskForApproval::Never`] preserves the prior non-interactive behavior
    /// (tools auto-approve, no prompt), while any non-`Never` policy routes each
    /// gated call through the orchestrator's [`Approver`](crate::tools::runtime::Approver)
    /// seam, which can deny. (OS-level sandbox enforcement is intentionally NOT
    /// driven by this field — the production wiring uses a permissive sandbox seam.)
    pub approval_policy: AskForApproval,
    /// Whether the guardian LLM-reviewer safety gate is active for gated tool
    /// calls under a non-`Never` [`approval_policy`](Self::approval_policy).
    ///
    /// Default `false` keeps the permissive (allow-everything) reviewer; setting
    /// it `true` selects the fail-closed denying reviewer so a non-`Never` policy
    /// actually blocks gated calls (the guardian review path).
    pub use_guardian: bool,
    /// Codex-compatible MultiAgentV2 feature/config values used by the
    /// dispatcher and subagent tool handlers.
    pub multi_agent_v2: MultiAgentV2Options,
    /// Legacy Codex collaboration tool gate (`Feature::Collab`, canonical config
    /// key `features.multi_agent`, legacy alias `features.collab`).
    pub collab_enabled: bool,
    /// User-defined subagent roles loaded from `[agents.<name>]` config.
    pub agent_roles: BTreeMap<String, AgentRoleConfig>,
}

impl Default for AgentRunOptions {
    fn default() -> Self {
        Self {
            max_turns: 80,
            max_context_chars: DEFAULT_MAX_CONTEXT_CHARS,
            browser_mode: None,
            dynamic_browser_mode_from_store: false,
            browser_profile_id: None,
            browser_profile_label: None,
            browser_local_browser: None,
            collaboration_mode: CollaborationModeKind::Default,
            include_environment_context: true,
            include_permissions_instructions: true,
            environment_context_environments: Vec::new(),
            environment_context_network: None,
            config_profile: None,
            config_overrides: Vec::new(),
            session_thread_config: None,
            base_instructions: None,
            developer_instructions: None,
            compact_prompt: None,
            model_provider_id: None,
            model_provider_id_source: RunConfigValueSource::Default,
            model_stream_idle_timeout_ms: None,
            python_tool_timeout_seconds: 300,
            python_env: Vec::new(),
            child_agent_runner: None,
            final_output_json_schema: None,
            final_output_json_schema_strict: true,
            model_compaction_enabled: true,
            model_auto_compact_token_limit: None,
            model_auto_compact_token_limit_scope: AutoCompactTokenLimitScope::Total,
            analytics_source: None,
            analytics_provider_kind: None,
            analytics_model: None,
            full_llm_input_events: false,
            mcp_servers: HashMap::new(),
            // Default preserves the prior non-interactive behavior: tools
            // auto-approve, the approver is never consulted.
            approval_policy: AskForApproval::Never,
            use_guardian: false,
            multi_agent_v2: MultiAgentV2Options::default(),
            collab_enabled: false,
            agent_roles: BTreeMap::new(),
        }
    }
}

impl AgentRunOptions {
    pub fn with_browser_mode(mut self, mode: impl Into<String>) -> Self {
        self.browser_mode = Some(mode.into());
        self
    }

    pub fn with_dynamic_browser_mode_from_store(mut self, dynamic: bool) -> Self {
        self.dynamic_browser_mode_from_store = dynamic;
        self
    }

    pub fn with_browser_profile_id(mut self, profile_id: impl Into<String>) -> Self {
        self.browser_profile_id = Some(profile_id.into());
        self
    }

    pub fn with_browser_profile_label(mut self, profile_label: impl Into<String>) -> Self {
        self.browser_profile_label = Some(profile_label.into());
        self
    }

    pub fn with_browser_local_browser(mut self, browser: impl Into<String>) -> Self {
        self.browser_local_browser = Some(browser.into());
        self
    }

    pub fn with_collaboration_mode(mut self, mode: CollaborationModeKind) -> Self {
        self.collaboration_mode = match mode {
            CollaborationModeKind::Default | CollaborationModeKind::Plan => {
                CollaborationModeKind::Default
            }
        };
        self
    }

    pub fn with_include_environment_context(mut self, include: bool) -> Self {
        self.include_environment_context = include;
        self
    }

    pub fn with_include_permissions_instructions(mut self, include: bool) -> Self {
        self.include_permissions_instructions = include;
        self
    }

    pub fn with_environment_context_environments(
        mut self,
        environments: Vec<EnvironmentContextEnvironment>,
    ) -> Self {
        self.environment_context_environments = environments;
        self
    }

    pub fn with_environment_context_network(mut self, network: EnvironmentNetworkContext) -> Self {
        self.environment_context_network = Some(network);
        self
    }

    pub fn with_config_profile(mut self, profile: impl Into<String>) -> Self {
        self.config_profile = Some(profile.into());
        self
    }

    pub fn with_config_overrides(mut self, overrides: Vec<(String, toml::Value)>) -> Self {
        self.config_overrides = overrides;
        self
    }

    pub fn with_session_thread_config(mut self, config: toml::Value) -> Self {
        self.session_thread_config = Some(config);
        self
    }

    pub fn with_session_thread_config_overrides(
        mut self,
        overrides: Vec<(String, toml::Value)>,
    ) -> Self {
        self.session_thread_config = Some(build_config_overrides_layer(&overrides));
        self
    }

    pub fn with_base_instructions(mut self, instructions: impl Into<String>) -> Self {
        self.base_instructions = Some(instructions.into());
        self
    }

    pub fn with_developer_instructions(mut self, instructions: impl Into<String>) -> Self {
        self.developer_instructions = Some(instructions.into());
        self
    }

    pub fn with_compact_prompt(mut self, prompt: impl Into<String>) -> Self {
        self.compact_prompt = Some(prompt.into());
        self
    }

    pub fn with_model_provider_id(mut self, model_provider_id: impl Into<String>) -> Self {
        self.model_provider_id = Some(model_provider_id.into());
        self.model_provider_id_source = RunConfigValueSource::Explicit;
        self
    }

    pub fn with_default_model_provider_id(mut self, model_provider_id: impl Into<String>) -> Self {
        self.model_provider_id = Some(model_provider_id.into());
        self.model_provider_id_source = RunConfigValueSource::Default;
        self
    }

    pub fn with_python_tool_timeout_seconds(mut self, timeout_seconds: u64) -> Self {
        self.python_tool_timeout_seconds = timeout_seconds;
        self
    }

    pub fn with_python_env(mut self, env: Vec<(String, String)>) -> Self {
        self.python_env = env;
        self
    }

    pub fn with_child_agent_runner(mut self, runner: ChildAgentRunner) -> Self {
        self.child_agent_runner = Some(runner);
        self
    }

    pub fn with_final_output_json_schema(mut self, schema: Value, strict: bool) -> Self {
        self.final_output_json_schema = Some(schema);
        self.final_output_json_schema_strict = strict;
        self
    }

    pub fn with_model_compaction(mut self, enabled: bool) -> Self {
        self.model_compaction_enabled = enabled;
        self
    }

    pub fn with_model_auto_compact_token_limit(mut self, limit: Option<i64>) -> Self {
        self.model_auto_compact_token_limit = limit;
        self
    }

    pub fn with_model_auto_compact_token_limit_scope(
        mut self,
        scope: AutoCompactTokenLimitScope,
    ) -> Self {
        self.model_auto_compact_token_limit_scope = scope;
        self
    }

    pub fn with_analytics_source(mut self, source: impl Into<String>) -> Self {
        self.analytics_source = Some(source.into());
        self
    }

    /// Configure the MCP servers exposed via the model-callable `mcp` tool.
    pub fn with_mcp_servers(mut self, servers: HashMap<String, McpServerConfig>) -> Self {
        self.mcp_servers = servers;
        self
    }

    /// Set the tool approval policy (default [`AskForApproval::Never`]).
    pub fn with_approval_policy(mut self, policy: AskForApproval) -> Self {
        self.approval_policy = policy;
        self
    }

    /// Enable (or disable) the fail-closed guardian reviewer for gated tool
    /// calls under a non-`Never` approval policy.
    pub fn with_guardian(mut self, use_guardian: bool) -> Self {
        self.use_guardian = use_guardian;
        self
    }

    pub fn with_multi_agent_v2(mut self, options: MultiAgentV2Options) -> Self {
        self.multi_agent_v2 = options;
        self
    }

    pub fn with_collab_enabled(mut self, enabled: bool) -> Self {
        self.collab_enabled = enabled;
        self
    }

    pub fn with_agent_roles(mut self, roles: BTreeMap<String, AgentRoleConfig>) -> Self {
        self.agent_roles = roles;
        self
    }
}

/// Default model context-window budget in tokens, used when the caller does not
/// set one explicitly.
///
/// Drives the auto-compaction trigger (compaction fires at 90% of this — codex
/// `Session::auto_compact_token_limit`). Sized to the gpt-5/-codex family window;
/// callers that know the real model window should set it via
/// [`ProviderRunConfig::with_context_window_tokens`].
pub const DEFAULT_MODEL_CONTEXT_WINDOW_TOKENS: usize = 272_000;

const fn default_effective_context_window_percent() -> i64 {
    95
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ModelContextMetadata {
    pub context_window: Option<i64>,
    pub max_context_window: Option<i64>,
    pub auto_compact_token_limit: Option<i64>,
    pub effective_context_window_percent: i64,
}

impl Default for ModelContextMetadata {
    fn default() -> Self {
        Self {
            context_window: None,
            max_context_window: None,
            auto_compact_token_limit: None,
            effective_context_window_percent: default_effective_context_window_percent(),
        }
    }
}

impl ModelContextMetadata {
    pub fn resolved_context_window(&self) -> Option<i64> {
        self.context_window.or(self.max_context_window)
    }

    pub fn effective_context_window(&self) -> Option<i64> {
        self.resolved_context_window().map(|context_window| {
            context_window.saturating_mul(self.effective_context_window_percent) / 100
        })
    }

    pub fn auto_compact_token_limit(&self) -> Option<i64> {
        self.auto_compact_token_limit_with_override(None)
    }

    pub fn auto_compact_token_limit_with_override(&self, configured: Option<i64>) -> Option<i64> {
        let context_limit = self
            .resolved_context_window()
            .map(|context_window| (context_window * 9) / 10);
        let config_limit = configured.or(self.auto_compact_token_limit);
        match (context_limit, config_limit.filter(|limit| *limit > 0)) {
            (Some(context_limit), Some(configured)) => Some(configured.min(context_limit)),
            (Some(context_limit), None) => Some(context_limit),
            (None, configured) => configured,
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
struct BundledModels {
    models: Vec<BundledModelMetadata>,
}

#[derive(Clone, Debug, Deserialize)]
struct BundledModelMetadata {
    slug: String,
    #[serde(default)]
    context_window: Option<i64>,
    #[serde(default)]
    max_context_window: Option<i64>,
    #[serde(default)]
    auto_compact_token_limit: Option<i64>,
    #[serde(default = "default_effective_context_window_percent")]
    effective_context_window_percent: i64,
}

fn bundled_model_metadata() -> &'static [BundledModelMetadata] {
    static MODELS: OnceLock<Vec<BundledModelMetadata>> = OnceLock::new();
    MODELS
        .get_or_init(|| {
            serde_json::from_str::<BundledModels>(include_str!(
                "../../../prompts/codex-models.json"
            ))
            .map(|catalog| catalog.models)
            .unwrap_or_default()
        })
        .as_slice()
}

pub fn model_context_metadata_for_model(model: &str) -> ModelContextMetadata {
    bundled_model_metadata()
        .iter()
        .find(|metadata| metadata.slug == model)
        .map(|metadata| ModelContextMetadata {
            context_window: metadata.context_window,
            max_context_window: metadata.max_context_window,
            auto_compact_token_limit: metadata.auto_compact_token_limit,
            effective_context_window_percent: metadata.effective_context_window_percent,
        })
        .unwrap_or_default()
}

/// The fully-resolved provider/run configuration handed to the engine.
///
/// Mirrors `browser-use-core::ProviderRunConfig` (`lib.rs:142`).
#[derive(Clone, Debug)]
pub struct ProviderRunConfig {
    pub backend: ProviderBackend,
    pub model: String,
    pub model_source: RunConfigValueSource,
    pub options: AgentRunOptions,
    pub fake_result: Option<String>,
    /// The model's context-window budget in tokens. Drives the codex 90%
    /// auto-compaction trigger (`TokenStatus::from_usage`). `0` disables
    /// compaction (unknown budget). Defaults to
    /// [`DEFAULT_MODEL_CONTEXT_WINDOW_TOKENS`].
    pub context_window_tokens: usize,
}

impl ProviderRunConfig {
    pub fn new(backend: ProviderBackend, model: impl Into<String>) -> Self {
        let model = model.into();
        let metadata = model_context_metadata_for_model(&model);
        let context_window_tokens = metadata
            .resolved_context_window()
            .and_then(|tokens| usize::try_from(tokens).ok())
            .unwrap_or(DEFAULT_MODEL_CONTEXT_WINDOW_TOKENS);
        Self {
            backend,
            model,
            model_source: RunConfigValueSource::Explicit,
            options: AgentRunOptions::default(),
            fake_result: None,
            context_window_tokens,
        }
    }

    pub fn with_options(mut self, options: AgentRunOptions) -> Self {
        self.options = options;
        self
    }

    pub fn with_model_source(mut self, source: RunConfigValueSource) -> Self {
        self.model_source = source;
        self
    }

    pub fn with_fake_result(mut self, result: impl Into<String>) -> Self {
        self.fake_result = Some(result.into());
        self
    }

    /// Set the model context-window budget (tokens) driving the auto-compaction
    /// trigger. `0` disables compaction.
    pub fn with_context_window_tokens(mut self, tokens: usize) -> Self {
        self.context_window_tokens = tokens;
        self
    }

    pub fn model_context_metadata(&self) -> ModelContextMetadata {
        let mut metadata = model_context_metadata_for_model(&self.model);
        let context_window = self.context_window_tokens as i64;
        metadata.context_window = Some(
            metadata
                .max_context_window
                .map_or(context_window, |max_context_window| {
                    context_window.min(max_context_window)
                }),
        );
        metadata
    }
}

/// Re-apply runtime options that were snapshotted into a
/// [`ChildAgentRunRequest`]. Child wake/resume paths may run under a different
/// parent process/config than the original spawn, so these values must travel
/// with the child run marker rather than relying on the current parent config.
pub fn apply_child_request_runtime_config(
    config: &mut ProviderRunConfig,
    request: &ChildAgentRunRequest,
) -> Result<()> {
    apply_runtime_config_overrides(&mut config.options, &request.config_overrides)
}

/// Apply config keys that mutate in-memory runtime options.
///
/// The raw override list is still retained for downstream consumers that read
/// less common config keys directly, but options that are consulted before those
/// consumers run must be materialized here.
pub fn apply_runtime_config_overrides(
    options: &mut AgentRunOptions,
    overrides: &ConfigOverrides,
) -> Result<()> {
    if let Some(value) = config_override_u64(overrides, "max_turns") {
        options.max_turns = usize::try_from(value)
            .context("max_turns does not fit in usize")?
            .max(1);
    }
    if let Some(value) = config_override_str(overrides, "browser_mode") {
        options.browser_mode = Some(value);
    }
    if let Some(value) = config_override_str(overrides, "base_instructions") {
        options.base_instructions = Some(value);
    }
    if let Some(value) = config_override_str(overrides, "developer_instructions") {
        options.developer_instructions = Some(value);
    }
    if let Some(value) = config_override_str(overrides, "compact_prompt") {
        options.compact_prompt = Some(value);
    }
    if let Some(value) = config_override_u64(overrides, "python_tool_timeout_seconds") {
        options.python_tool_timeout_seconds = value;
    }
    if let Some(value) = config_override_u64(overrides, "model_stream_idle_timeout_ms") {
        options.model_stream_idle_timeout_ms = Some(value);
    }
    if let Some(value) = config_override_bool(overrides, "model_compaction_enabled") {
        options.model_compaction_enabled = value;
    }
    if let Some(value) = config_override_bool_any(
        overrides,
        &[
            "full_llm_input_events",
            "observability.full_llm_input_events",
        ],
    ) {
        options.full_llm_input_events = value;
    }
    if let Some(value) = config_override_i64(overrides, "model_auto_compact_token_limit") {
        options.model_auto_compact_token_limit = Some(value);
    }
    if let Some(value) = config_override_str(overrides, "model_auto_compact_token_limit_scope") {
        options.model_auto_compact_token_limit_scope =
            parse_auto_compact_token_limit_scope(&value)?;
    }
    if let Some(value) = config_override_str(overrides, "approval_policy")
        .or_else(|| config_override_str(overrides, "ask_for_approval"))
    {
        options.approval_policy = parse_approval_policy(&value)?;
    }
    if let Some(value) = config_override_bool(overrides, "use_guardian")
        .or_else(|| config_override_bool(overrides, "guardian"))
    {
        options.use_guardian = value;
    }
    Ok(())
}

/// Parse raw `key=value` override strings into an ordered [`ConfigOverrides`].
///
/// Behavior is byte-identical to `browser-use-core::parse_config_overrides`
/// (`config_overrides.rs:9`): each entry is split on the first `=`, the key is
/// trimmed (and canonicalized), and the value is parsed as a bare TOML value,
/// falling back to a quote-stripped string literal when that fails.
pub fn parse_config_overrides(raw_config_overrides: &[String]) -> Result<ConfigOverrides> {
    raw_config_overrides
        .iter()
        .map(|raw| {
            let mut parts = raw.splitn(2, '=');
            let key = parts.next().unwrap_or_default().trim();
            let value_str = parts
                .next()
                .ok_or_else(|| anyhow!("Invalid override (missing '='): {raw}"))?
                .trim();
            if key.is_empty() {
                bail!("Empty key in override: {raw}");
            }
            let value = parse_config_override_toml_value(value_str).unwrap_or_else(|| {
                toml::Value::String(
                    value_str
                        .trim()
                        .trim_matches(|candidate| candidate == '"' || candidate == '\'')
                        .to_string(),
                )
            });
            Ok((canonicalize_config_override_key(key), value))
        })
        .collect()
}

#[derive(Default, Deserialize)]
struct RuntimeConfigToml {
    #[serde(default)]
    mcp_servers: HashMap<String, McpServerConfig>,
    #[serde(default)]
    approval_policy: Option<String>,
    #[serde(default)]
    ask_for_approval: Option<String>,
    #[serde(default)]
    guardian: Option<bool>,
    #[serde(default)]
    use_guardian: Option<bool>,
    #[serde(default)]
    features: Option<toml::Value>,
    #[serde(default)]
    agents: Option<toml::Value>,
}

/// Load `[mcp_servers]` from `$BROWSER_USE_TERMINAL_HOME/config.toml`, the
/// active profile config, and explicit MCP config files. Later layers win.
pub fn load_mcp_servers_for_profile(
    config_profile: Option<&str>,
    explicit_paths: &[PathBuf],
) -> Result<HashMap<String, McpServerConfig>> {
    let mut servers = HashMap::new();
    for path in existing_runtime_config_paths(config_profile) {
        servers.extend(read_runtime_config_toml(&path)?.mcp_servers);
    }
    for path in explicit_paths {
        if !path.exists() {
            bail!("MCP config file does not exist: {}", path.display());
        }
        servers.extend(read_runtime_config_toml(path)?.mcp_servers);
    }
    Ok(servers)
}

/// Resolve the run approval policy from explicit CLI choice, `--config`
/// overrides, or the active config.toml layer. `None` means keep defaults.
pub fn resolve_approval_policy_for_profile(
    config_profile: Option<&str>,
    config_overrides: &ConfigOverrides,
    explicit: Option<AskForApproval>,
) -> Result<Option<AskForApproval>> {
    if explicit.is_some() {
        return Ok(explicit);
    }
    if let Some(value) = config_override_str(config_overrides, "approval_policy")
        .or_else(|| config_override_str(config_overrides, "ask_for_approval"))
    {
        return parse_approval_policy(&value).map(Some);
    }
    for path in existing_runtime_config_paths(config_profile)
        .into_iter()
        .rev()
    {
        let config = read_runtime_config_toml(&path)?;
        if let Some(value) = config.approval_policy.or(config.ask_for_approval) {
            return parse_approval_policy(&value).map(Some);
        }
    }
    Ok(None)
}

/// Resolve the guardian gate from explicit CLI choice, `--config` overrides, or
/// config.toml. `None` means keep defaults.
pub fn resolve_guardian_for_profile(
    config_profile: Option<&str>,
    config_overrides: &ConfigOverrides,
    explicit: Option<bool>,
) -> Result<Option<bool>> {
    if explicit.is_some() {
        return Ok(explicit);
    }
    if let Some(value) = config_override_bool(config_overrides, "guardian")
        .or_else(|| config_override_bool(config_overrides, "use_guardian"))
    {
        return Ok(Some(value));
    }
    for path in existing_runtime_config_paths(config_profile)
        .into_iter()
        .rev()
    {
        let config = read_runtime_config_toml(&path)?;
        if let Some(value) = config.guardian.or(config.use_guardian) {
            return Ok(Some(value));
        }
    }
    Ok(None)
}

pub fn resolve_multi_agent_v2_for_profile(
    config_profile: Option<&str>,
    config_overrides: &ConfigOverrides,
) -> Result<MultiAgentV2Options> {
    let mut options = MultiAgentV2Options::default();
    for path in existing_runtime_config_paths(config_profile) {
        let config = read_runtime_config_toml(&path)?;
        if let Some(value) = config
            .features
            .as_ref()
            .and_then(|features| features.get("multi_agent_v2"))
        {
            apply_multi_agent_v2_value(&mut options, value)?;
        }
    }
    let override_layer = build_config_overrides_layer(config_overrides);
    if let Some(value) = override_layer
        .get("features")
        .and_then(|features| features.get("multi_agent_v2"))
    {
        apply_multi_agent_v2_value(&mut options, value)?;
    }
    validate_multi_agent_v2_options(&options)?;
    Ok(options)
}

pub fn resolve_collab_for_profile(
    config_profile: Option<&str>,
    config_overrides: &ConfigOverrides,
) -> Result<bool> {
    let mut enabled = false;
    for path in existing_runtime_config_paths(config_profile) {
        let config = read_runtime_config_toml(&path)?;
        if let Some(features) = config.features.as_ref() {
            apply_collab_features_value(&mut enabled, features)?;
        }
    }
    let override_layer = build_config_overrides_layer(config_overrides);
    if let Some(features) = override_layer.get("features") {
        apply_collab_features_value(&mut enabled, features)?;
    }
    Ok(enabled)
}

pub fn resolve_agent_roles_for_profile(
    config_profile: Option<&str>,
    config_overrides: &ConfigOverrides,
) -> Result<BTreeMap<String, AgentRoleConfig>> {
    let mut roles = BTreeMap::new();
    for path in existing_runtime_config_paths(config_profile) {
        let config = read_runtime_config_toml(&path)?;
        if let Some(value) = config.agents.as_ref() {
            let base_dir = path.parent().unwrap_or_else(|| Path::new("."));
            apply_agent_roles_value(&mut roles, value, base_dir)?;
        }
        let base_dir = path.parent().unwrap_or_else(|| Path::new("."));
        discover_agent_roles_in_dir(&mut roles, &base_dir.join("agents"))?;
    }
    let override_layer = build_config_overrides_layer(config_overrides);
    if let Some(value) = override_layer.get("agents") {
        apply_agent_roles_value(&mut roles, value, Path::new("."))?;
    }
    Ok(roles)
}

fn discover_agent_roles_in_dir(
    roles: &mut BTreeMap<String, AgentRoleConfig>,
    agents_dir: &Path,
) -> Result<()> {
    if !agents_dir.exists() {
        return Ok(());
    }
    let mut files = Vec::new();
    collect_agent_role_files(agents_dir, &mut files)?;
    files.sort();
    for file in files {
        let contents = fs::read_to_string(&file)
            .with_context(|| format!("read agent role {}", file.display()))?;
        let role_file_base = file.parent().unwrap_or(agents_dir);
        let role_name_hint = file.file_stem().and_then(|stem| stem.to_str());
        let parsed = parse_agent_role_file(&contents, &file, role_file_base, role_name_hint)?;
        roles.entry(parsed.0).or_insert(parsed.1);
    }
    Ok(())
}

fn collect_agent_role_files(dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err.into()),
    };
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            collect_agent_role_files(&path, out)?;
        } else if file_type.is_file() && path.extension().is_some_and(|ext| ext == "toml") {
            out.push(path);
        }
    }
    Ok(())
}

fn parse_agent_role_file(
    contents: &str,
    file: &Path,
    base_dir: &Path,
    role_name_hint: Option<&str>,
) -> Result<(String, AgentRoleConfig)> {
    let mut value = contents
        .parse::<toml::Value>()
        .with_context(|| format!("parse agent role {}", file.display()))?;
    let table = value.as_table_mut().ok_or_else(|| {
        anyhow!(
            "agent role file {} must contain a TOML table",
            file.display()
        )
    })?;
    let name = table
        .remove("name")
        .and_then(|value| value.as_str().map(str::trim).map(ToOwned::to_owned))
        .filter(|value| !value.is_empty())
        .or_else(|| role_name_hint.map(ToOwned::to_owned))
        .ok_or_else(|| {
            anyhow!(
                "agent role file at {} must define a non-empty `name`",
                file.display()
            )
        })?;
    let description = optional_toml_string(
        table.remove("description").as_ref(),
        &format!("agent role file {}.description", file.display()),
    )?;
    let nickname_candidates = optional_toml_string_array(
        table.remove("nickname_candidates").as_ref(),
        &format!("agent role file {}.nickname_candidates", file.display()),
    )?;
    let overrides = role_overrides_from_table(
        table,
        &format!("agent role file {}", file.display()),
        base_dir,
    )?;
    Ok((
        name,
        AgentRoleConfig {
            description,
            config_file: Some(file.to_path_buf()),
            nickname_candidates,
            overrides,
        },
    ))
}

fn apply_agent_roles_value(
    roles: &mut BTreeMap<String, AgentRoleConfig>,
    value: &toml::Value,
    base_dir: &Path,
) -> Result<()> {
    let table = value
        .as_table()
        .ok_or_else(|| anyhow!("agents must be a table"))?;
    for (name, value) in table {
        let role_table = value
            .as_table()
            .ok_or_else(|| anyhow!("agents.{name} must be a table"))?;
        roles.insert(
            name.clone(),
            agent_role_from_table(name, role_table, base_dir)?,
        );
    }
    Ok(())
}

fn agent_role_from_table(
    name: &str,
    table: &toml::map::Map<String, toml::Value>,
    base_dir: &Path,
) -> Result<AgentRoleConfig> {
    let description = optional_toml_string(
        table.get("description"),
        &format!("agents.{name}.description"),
    )?;
    let nickname_candidates = optional_toml_string_array(
        table.get("nickname_candidates"),
        &format!("agents.{name}.nickname_candidates"),
    )?;
    let config_file = optional_toml_string(
        table.get("config_file"),
        &format!("agents.{name}.config_file"),
    )?
    .map(|path| {
        let path = PathBuf::from(path);
        if path.is_absolute() {
            path
        } else {
            base_dir.join(path)
        }
    });
    let mut overrides = role_overrides_from_table(table, &format!("agents.{name}"), base_dir)?;
    let mut description = description;
    let mut nickname_candidates = nickname_candidates;
    if let Some(config_file) = config_file.as_ref() {
        let file_contents = fs::read_to_string(config_file)
            .with_context(|| format!("read agents.{name}.config_file {}", config_file.display()))?;
        let (file_role_name, file_role) = parse_agent_role_file(
            &file_contents,
            config_file,
            config_file.parent().unwrap_or(base_dir),
            Some(name),
        )?;
        if file_role_name != name {
            bail!(
                "agents.{name}.config_file resolved role name `{file_role_name}`, expected `{name}`"
            );
        }
        description = file_role.description.or(description);
        nickname_candidates = file_role.nickname_candidates.or(nickname_candidates);
        overrides.merge(file_role.overrides);
    }
    Ok(AgentRoleConfig {
        description,
        config_file,
        nickname_candidates,
        overrides,
    })
}

fn optional_toml_string(value: Option<&toml::Value>, label: &str) -> Result<Option<String>> {
    value
        .map(|value| read_toml_string(value, label))
        .transpose()
}

fn optional_toml_string_array(
    value: Option<&toml::Value>,
    label: &str,
) -> Result<Option<Vec<String>>> {
    let Some(value) = value else {
        return Ok(None);
    };
    let values = value
        .as_array()
        .ok_or_else(|| anyhow!("{label} must be an array of strings"))?;
    let mut out = Vec::with_capacity(values.len());
    let mut seen = BTreeSet::new();
    for value in values {
        let value = read_toml_string(value, label)?.trim().to_string();
        if value.is_empty() {
            bail!("{label} cannot contain blank names");
        }
        if !value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, ' ' | '-' | '_'))
        {
            bail!(
                "{label} may only contain ASCII letters, digits, spaces, hyphens, and underscores"
            );
        }
        if !seen.insert(value.clone()) {
            bail!("{label} cannot contain duplicates");
        }
        out.push(value);
    }
    if out.is_empty() {
        bail!("{label} must not be empty");
    }
    Ok(Some(out))
}

fn role_overrides_from_table(
    table: &toml::map::Map<String, toml::Value>,
    label: &str,
    base_dir: &Path,
) -> Result<RoleOverrides> {
    Ok(RoleOverrides {
        model: optional_toml_string(table.get("model"), &format!("{label}.model"))?,
        reasoning_effort: optional_toml_string(
            table
                .get("model_reasoning_effort")
                .or_else(|| table.get("reasoning_effort")),
            &format!("{label}.reasoning_effort"),
        )?,
        instructions: optional_toml_string(
            table
                .get("developer_instructions")
                .or_else(|| table.get("instructions")),
            &format!("{label}.instructions"),
        )?,
        tool_allowlist: optional_toml_string_array(
            table.get("tool_allowlist").or_else(|| table.get("tools")),
            &format!("{label}.tool_allowlist"),
        )?,
        can_write: table
            .get("can_write")
            .map(|value| read_toml_bool(value, &format!("{label}.can_write")))
            .transpose()?,
        provider: optional_toml_string(
            table
                .get("model_provider")
                .or_else(|| table.get("provider")),
            &format!("{label}.model_provider"),
        )?,
        service_tier: optional_toml_string(
            table.get("service_tier"),
            &format!("{label}.service_tier"),
        )?,
        config_overrides: role_config_overrides_from_table(table, base_dir),
    })
}

fn role_config_overrides_from_table(
    table: &toml::map::Map<String, toml::Value>,
    base_dir: &Path,
) -> ConfigOverrides {
    let mut out = Vec::new();
    for (key, value) in table {
        if matches!(
            key.as_str(),
            "name" | "description" | "nickname_candidates" | "config_file"
        ) {
            continue;
        }
        flatten_role_config_override(key, value, base_dir, &mut out);
    }
    out
}

fn flatten_role_config_override(
    key: &str,
    value: &toml::Value,
    base_dir: &Path,
    out: &mut ConfigOverrides,
) {
    if let toml::Value::Table(table) = value {
        for (child_key, child_value) in table {
            let dotted = format!("{key}.{child_key}");
            flatten_role_config_override(&dotted, child_value, base_dir, out);
        }
        return;
    }
    out.push((
        canonicalize_config_override_key(key),
        resolve_role_config_value_paths(key, value.clone(), base_dir),
    ));
}

fn resolve_role_config_value_paths(key: &str, value: toml::Value, base_dir: &Path) -> toml::Value {
    let toml::Value::String(raw) = value else {
        return value;
    };
    if !is_role_path_like_key(key) {
        return toml::Value::String(raw);
    }
    let path = PathBuf::from(raw.trim());
    if path.as_os_str().is_empty() || path.is_absolute() {
        return toml::Value::String(path.to_string_lossy().to_string());
    }
    toml::Value::String(base_dir.join(path).to_string_lossy().to_string())
}

fn is_role_path_like_key(key: &str) -> bool {
    let leaf = key.rsplit('.').next().unwrap_or(key);
    leaf == "cwd"
        || leaf == "model_catalog_json"
        || leaf.ends_with("_path")
        || leaf.ends_with("_file")
        || leaf.ends_with("_dir")
}

fn apply_multi_agent_v2_value(
    options: &mut MultiAgentV2Options,
    value: &toml::Value,
) -> Result<()> {
    match value {
        toml::Value::Boolean(enabled) => {
            options.enabled = *enabled;
            Ok(())
        }
        toml::Value::Table(table) => {
            if let Some(value) = table.get("enabled") {
                options.enabled = read_toml_bool(value, "features.multi_agent_v2.enabled")?;
            }
            if let Some(value) = table.get("max_concurrent_threads_per_session") {
                let threads = read_toml_i64(
                    value,
                    "features.multi_agent_v2.max_concurrent_threads_per_session",
                )?;
                options.max_concurrent_threads_per_session =
                    usize::try_from(threads).map_err(|_| {
                        anyhow!(
                            "features.multi_agent_v2.max_concurrent_threads_per_session must be at least 1"
                        )
                    })?;
            }
            if let Some(value) = table.get("min_wait_timeout_ms") {
                options.min_wait_timeout_ms =
                    read_toml_i64(value, "features.multi_agent_v2.min_wait_timeout_ms")?;
            }
            if let Some(value) = table.get("max_wait_timeout_ms") {
                options.max_wait_timeout_ms =
                    read_toml_i64(value, "features.multi_agent_v2.max_wait_timeout_ms")?;
            }
            if let Some(value) = table.get("default_wait_timeout_ms") {
                options.default_wait_timeout_ms =
                    read_toml_i64(value, "features.multi_agent_v2.default_wait_timeout_ms")?;
            }
            if let Some(value) = table.get("usage_hint_enabled") {
                options.usage_hint_enabled =
                    read_toml_bool(value, "features.multi_agent_v2.usage_hint_enabled")?;
            }
            if let Some(value) = table.get("usage_hint_text") {
                options.usage_hint_text = Some(read_toml_string(
                    value,
                    "features.multi_agent_v2.usage_hint_text",
                )?);
            }
            if let Some(value) = table.get("root_agent_usage_hint_text") {
                options.root_agent_usage_hint_text = Some(read_toml_string(
                    value,
                    "features.multi_agent_v2.root_agent_usage_hint_text",
                )?);
            }
            if let Some(value) = table.get("subagent_usage_hint_text") {
                options.subagent_usage_hint_text = Some(read_toml_string(
                    value,
                    "features.multi_agent_v2.subagent_usage_hint_text",
                )?);
            }
            if let Some(value) = table.get("tool_namespace") {
                options.tool_namespace = Some(read_toml_string(
                    value,
                    "features.multi_agent_v2.tool_namespace",
                )?);
            }
            if let Some(value) = table.get("hide_spawn_agent_metadata") {
                options.hide_spawn_agent_metadata =
                    read_toml_bool(value, "features.multi_agent_v2.hide_spawn_agent_metadata")?;
            }
            if let Some(value) = table.get("non_code_mode_only") {
                options.non_code_mode_only =
                    read_toml_bool(value, "features.multi_agent_v2.non_code_mode_only")?;
            }
            Ok(())
        }
        _ => bail!("features.multi_agent_v2 must be a boolean or table"),
    }
}

fn apply_collab_features_value(enabled: &mut bool, features: &toml::Value) -> Result<()> {
    let Some(table) = features.as_table() else {
        return Ok(());
    };
    if let Some(value) = table.get("collab") {
        *enabled = read_toml_bool(value, "features.collab")?;
    }
    if let Some(value) = table.get("multi_agent") {
        *enabled = read_toml_bool(value, "features.multi_agent")?;
    }
    Ok(())
}

fn read_toml_bool(value: &toml::Value, label: &str) -> Result<bool> {
    value
        .as_bool()
        .ok_or_else(|| anyhow!("{label} must be a boolean"))
}

fn read_toml_i64(value: &toml::Value, label: &str) -> Result<i64> {
    value
        .as_integer()
        .ok_or_else(|| anyhow!("{label} must be an integer"))
}

fn read_toml_string(value: &toml::Value, label: &str) -> Result<String> {
    value
        .as_str()
        .map(str::to_string)
        .ok_or_else(|| anyhow!("{label} must be a string"))
}

fn validate_multi_agent_v2_options(options: &MultiAgentV2Options) -> Result<()> {
    if options.max_concurrent_threads_per_session == 0 {
        bail!("features.multi_agent_v2.max_concurrent_threads_per_session must be at least 1");
    }
    validate_multi_agent_v2_wait_timeout(
        "features.multi_agent_v2.min_wait_timeout_ms",
        options.min_wait_timeout_ms,
    )?;
    validate_multi_agent_v2_wait_timeout(
        "features.multi_agent_v2.max_wait_timeout_ms",
        options.max_wait_timeout_ms,
    )?;
    validate_multi_agent_v2_wait_timeout(
        "features.multi_agent_v2.default_wait_timeout_ms",
        options.default_wait_timeout_ms,
    )?;
    if options.min_wait_timeout_ms > options.max_wait_timeout_ms {
        bail!(
            "features.multi_agent_v2.min_wait_timeout_ms must be at most features.multi_agent_v2.max_wait_timeout_ms"
        );
    }
    if options.default_wait_timeout_ms < options.min_wait_timeout_ms {
        bail!(
            "features.multi_agent_v2.default_wait_timeout_ms must be at least features.multi_agent_v2.min_wait_timeout_ms"
        );
    }
    if options.default_wait_timeout_ms > options.max_wait_timeout_ms {
        bail!(
            "features.multi_agent_v2.default_wait_timeout_ms must be at most features.multi_agent_v2.max_wait_timeout_ms"
        );
    }
    validate_multi_agent_v2_tool_namespace(options.tool_namespace.as_deref())?;
    Ok(())
}

fn validate_multi_agent_v2_wait_timeout(label: &str, value: i64) -> Result<()> {
    if value < HARD_MIN_MULTI_AGENT_V2_TIMEOUT_MS {
        bail!("{label} must be at least {HARD_MIN_MULTI_AGENT_V2_TIMEOUT_MS}");
    }
    if value > HARD_MAX_MULTI_AGENT_V2_TIMEOUT_MS {
        bail!("{label} must be at most {HARD_MAX_MULTI_AGENT_V2_TIMEOUT_MS}");
    }
    Ok(())
}

fn validate_multi_agent_v2_tool_namespace(namespace: Option<&str>) -> Result<()> {
    const LABEL: &str = "features.multi_agent_v2.tool_namespace";
    const MAX_LEN: usize = 64;
    const RESERVED_RESPONSES_NAMESPACES: &[&str] = &[
        "api_tool",
        "browser",
        "computer",
        "container",
        "file_search",
        "functions",
        "image_gen",
        "multi_tool_use",
        "python",
        "python_user_visible",
        "submodel_delegator",
        "terminal",
        "tool_search",
        "web",
    ];

    let Some(namespace) = namespace else {
        return Ok(());
    };
    if namespace.is_empty() {
        bail!("{LABEL} must not be empty");
    }
    if namespace.trim() != namespace {
        bail!("{LABEL} must not have leading or trailing whitespace");
    }
    if !namespace
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
    {
        bail!("{LABEL} must match ^[a-zA-Z0-9_-]+$");
    }
    if namespace.chars().count() > MAX_LEN {
        bail!("{LABEL} must be at most {MAX_LEN} characters");
    }
    if namespace == "mcp"
        || namespace.starts_with("mcp__")
        || RESERVED_RESPONSES_NAMESPACES.contains(&namespace)
    {
        bail!("{LABEL} uses a reserved namespace: {namespace}");
    }
    Ok(())
}

pub fn parse_approval_policy(raw: &str) -> Result<AskForApproval> {
    let normalized = raw.trim().to_ascii_lowercase().replace(['_', ' '], "-");
    match normalized.as_str() {
        "never" => Ok(AskForApproval::Never),
        "on-failure" | "onfailure" => Ok(AskForApproval::OnFailure),
        "on-request" | "onrequest" => Ok(AskForApproval::OnRequest),
        "unless-trusted" | "unlesstrusted" => Ok(AskForApproval::UnlessTrusted),
        other => bail!(
            "invalid approval policy {other:?}; expected never, on-failure, on-request, or unless-trusted"
        ),
    }
}

fn read_runtime_config_toml(path: &Path) -> Result<RuntimeConfigToml> {
    let contents = fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    toml::from_str(&contents).with_context(|| format!("parse {}", path.display()))
}

fn existing_runtime_config_paths(config_profile: Option<&str>) -> Vec<PathBuf> {
    runtime_config_paths(config_profile)
        .into_iter()
        .filter(|path| path.exists())
        .collect()
}

fn runtime_config_paths(config_profile: Option<&str>) -> Vec<PathBuf> {
    let Some(home) = terminal_home_dir() else {
        return Vec::new();
    };
    let mut paths = vec![home.join("config.toml")];
    if let Some(profile) = config_profile
        .map(str::trim)
        .filter(|profile| !profile.is_empty())
    {
        paths.push(home.join(format!("{profile}.config.toml")));
    }
    paths
}

fn terminal_home_dir() -> Option<PathBuf> {
    std::env::var_os("BROWSER_USE_TERMINAL_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| home_dir().map(|home| home.join(".browser-use-terminal")))
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

fn config_override_str(overrides: &ConfigOverrides, key: &str) -> Option<String> {
    overrides
        .iter()
        .rev()
        .find(|(candidate, _)| candidate == key)
        .and_then(|(_, value)| value.as_str().map(str::to_string))
}

fn config_override_bool(overrides: &ConfigOverrides, key: &str) -> Option<bool> {
    overrides
        .iter()
        .rev()
        .find(|(candidate, _)| candidate == key)
        .and_then(|(_, value)| value.as_bool())
}

fn config_override_bool_any(overrides: &ConfigOverrides, keys: &[&str]) -> Option<bool> {
    overrides
        .iter()
        .rev()
        .find(|(candidate, _)| keys.iter().any(|key| candidate == key))
        .and_then(|(_, value)| value.as_bool())
}

fn config_override_i64(overrides: &ConfigOverrides, key: &str) -> Option<i64> {
    overrides
        .iter()
        .rev()
        .find(|(candidate, _)| candidate == key)
        .and_then(|(_, value)| value.as_integer())
}

fn config_override_u64(overrides: &ConfigOverrides, key: &str) -> Option<u64> {
    config_override_i64(overrides, key).and_then(|value| u64::try_from(value).ok())
}

fn parse_auto_compact_token_limit_scope(raw: &str) -> Result<AutoCompactTokenLimitScope> {
    match raw.trim().to_ascii_lowercase().replace('-', "_").as_str() {
        "total" => Ok(AutoCompactTokenLimitScope::Total),
        "body_after_prefix" | "bodyafterprefix" => Ok(AutoCompactTokenLimitScope::BodyAfterPrefix),
        other => bail!(
            "invalid model_auto_compact_token_limit_scope {other:?}; expected total or body_after_prefix"
        ),
    }
}

/// Mirrors `browser-use-core::canonicalize_config_override_key`
/// (`config_overrides.rs:35`).
fn canonicalize_config_override_key(key: &str) -> String {
    if key == "use_legacy_landlock" {
        "features.use_legacy_landlock".to_string()
    } else {
        key.to_string()
    }
}

/// Mirrors `browser-use-core::parse_config_override_toml_value`
/// (`config_overrides.rs:43`).
fn parse_config_override_toml_value(raw: &str) -> Option<toml::Value> {
    let wrapped = format!("_x_ = {raw}");
    let mut table = toml::from_str::<toml::Table>(&wrapped).ok()?;
    table.remove("_x_")
}

/// Collapse a flat list of dotted overrides into a nested TOML table.
///
/// Mirrors `browser-use-core::build_config_overrides_layer` (`lib.rs:14372`).
pub fn build_config_overrides_layer(config_overrides: &[(String, toml::Value)]) -> toml::Value {
    let mut root = toml::Value::Table(toml::map::Map::new());
    for (path, value) in config_overrides {
        apply_toml_override(&mut root, path, value.clone());
    }
    root
}

/// Insert `value` at the dotted `path` within `root`, creating intermediate
/// tables (and replacing non-table values) as needed.
///
/// Mirrors `browser-use-core::apply_toml_override` (`lib.rs:14417`).
fn apply_toml_override(root: &mut toml::Value, path: &str, value: toml::Value) {
    let mut current = root;
    let mut segments = path.split('.').peekable();
    while let Some(segment) = segments.next() {
        let is_last = segments.peek().is_none();
        if is_last {
            match current {
                toml::Value::Table(table) => {
                    table.insert(segment.to_string(), value);
                }
                _ => {
                    let mut table = toml::map::Map::new();
                    table.insert(segment.to_string(), value);
                    *current = toml::Value::Table(table);
                }
            }
            return;
        }

        let need_table = !matches!(current, toml::Value::Table(_));
        if need_table {
            *current = toml::Value::Table(toml::map::Map::new());
        }
        if let toml::Value::Table(table) = current {
            current = table
                .entry(segment.to_string())
                .or_insert_with(|| toml::Value::Table(toml::map::Map::new()));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ov(pairs: &[&str]) -> Vec<String> {
        pairs.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn parses_representative_override_strings() {
        let parsed = parse_config_overrides(&ov(&[
            "model=\"gpt-5\"",
            "max_turns=12",
            "model_compaction_enabled=true",
            "temperature=0.5",
        ]))
        .unwrap();

        assert_eq!(
            parsed,
            vec![
                (
                    "model".to_string(),
                    toml::Value::String("gpt-5".to_string())
                ),
                ("max_turns".to_string(), toml::Value::Integer(12)),
                (
                    "model_compaction_enabled".to_string(),
                    toml::Value::Boolean(true)
                ),
                ("temperature".to_string(), toml::Value::Float(0.5)),
            ]
        );
    }

    #[test]
    fn trims_keys_and_falls_back_to_bare_string() {
        // Whitespace around key/value is trimmed; an unquoted, non-TOML value
        // falls back to a quote-stripped string literal.
        let parsed = parse_config_overrides(&ov(&["  provider  =  anthropic  "])).unwrap();
        assert_eq!(
            parsed,
            vec![(
                "provider".to_string(),
                toml::Value::String("anthropic".to_string())
            )]
        );

        // Single/double quotes around a bare string fallback are stripped.
        let quoted = parse_config_overrides(&ov(&["name='hello world'"])).unwrap();
        assert_eq!(
            quoted,
            vec![(
                "name".to_string(),
                toml::Value::String("hello world".to_string())
            )]
        );
    }

    #[test]
    fn canonicalizes_legacy_landlock_key() {
        let parsed = parse_config_overrides(&ov(&["use_legacy_landlock=true"])).unwrap();
        assert_eq!(
            parsed,
            vec![(
                "features.use_legacy_landlock".to_string(),
                toml::Value::Boolean(true)
            )]
        );
    }

    #[test]
    fn rejects_missing_equals_and_empty_key() {
        assert!(parse_config_overrides(&ov(&["no_equals_here"])).is_err());
        assert!(parse_config_overrides(&ov(&["=value"])).is_err());
    }

    #[test]
    fn parses_approval_policy_names() {
        assert_eq!(
            parse_approval_policy("never").unwrap(),
            AskForApproval::Never
        );
        assert_eq!(
            parse_approval_policy("on_request").unwrap(),
            AskForApproval::OnRequest
        );
        assert_eq!(
            parse_approval_policy("unless-trusted").unwrap(),
            AskForApproval::UnlessTrusted
        );
        assert!(parse_approval_policy("sometimes").is_err());
    }

    #[test]
    fn runtime_config_loads_profile_mcp_approval_and_guardian() {
        let _guard = crate::test_env::lock();
        let temp = tempfile::tempdir().unwrap();
        let previous = std::env::var_os("BROWSER_USE_TERMINAL_HOME");
        unsafe {
            std::env::set_var("BROWSER_USE_TERMINAL_HOME", temp.path());
        }
        std::fs::write(
            temp.path().join("config.toml"),
            r#"
approval_policy = "on-failure"
guardian = false

[mcp_servers.base]
transport = "stdio"
command = "base-server"
"#,
        )
        .unwrap();
        std::fs::write(
            temp.path().join("work.config.toml"),
            r#"
approval_policy = "on-request"
use_guardian = true

[mcp_servers.profile]
transport = "stdio"
command = "profile-server"
"#,
        )
        .unwrap();

        let servers = load_mcp_servers_for_profile(Some("work"), &[]).unwrap();
        assert!(servers.contains_key("base"));
        assert!(servers.contains_key("profile"));
        assert_eq!(
            resolve_approval_policy_for_profile(Some("work"), &Vec::new(), None).unwrap(),
            Some(AskForApproval::OnRequest)
        );
        assert_eq!(
            resolve_guardian_for_profile(Some("work"), &Vec::new(), None).unwrap(),
            Some(true)
        );

        unsafe {
            match previous {
                Some(value) => std::env::set_var("BROWSER_USE_TERMINAL_HOME", value),
                None => std::env::remove_var("BROWSER_USE_TERMINAL_HOME"),
            }
        }
    }

    #[test]
    fn agent_run_options_defaults_match_core() {
        let options = AgentRunOptions::default();
        assert_eq!(options.max_turns, 80);
        assert_eq!(options.max_context_chars, DEFAULT_MAX_CONTEXT_CHARS);
        assert_eq!(options.max_context_chars, 240_000);
        assert!(options.browser_mode.is_none());
        assert_eq!(options.collaboration_mode, CollaborationModeKind::Default);
        assert!(options.include_environment_context);
        assert!(options.include_permissions_instructions);
        assert!(options.environment_context_environments.is_empty());
        assert!(options.environment_context_network.is_none());
        assert!(options.config_profile.is_none());
        assert!(options.config_overrides.is_empty());
        assert!(options.session_thread_config.is_none());
        assert!(options.base_instructions.is_none());
        assert!(options.developer_instructions.is_none());
        assert!(options.compact_prompt.is_none());
        assert!(options.model_provider_id.is_none());
        assert_eq!(
            options.model_provider_id_source,
            RunConfigValueSource::Default
        );
        assert_eq!(options.python_tool_timeout_seconds, 300);
        assert!(options.python_env.is_empty());
        assert!(options.child_agent_runner.is_none());
        assert!(options.final_output_json_schema.is_none());
        assert!(options.final_output_json_schema_strict);
        assert!(options.model_compaction_enabled);
        assert!(options.analytics_source.is_none());
        assert!(options.analytics_provider_kind.is_none());
        assert!(options.analytics_model.is_none());
        assert!(!options.full_llm_input_events);
        assert!(options.mcp_servers.is_empty());
        // Approval defaults preserve prior non-interactive behavior.
        assert_eq!(options.approval_policy, AskForApproval::Never);
        assert!(!options.use_guardian);
        assert_eq!(options.multi_agent_v2, MultiAgentV2Options::default());
        assert!(!options.collab_enabled);
        assert!(options.agent_roles.is_empty());
    }

    #[test]
    fn runtime_config_overrides_materialize_max_turns_and_browser_mode() {
        let overrides = parse_config_overrides(&ov(&[
            "max_turns=100",
            "browser_mode=\"remote-cdp\"",
            "python_tool_timeout_seconds=45",
            "model_compaction_enabled=false",
            "full_llm_input_events=true",
        ]))
        .unwrap();
        let mut options = AgentRunOptions::default();

        apply_runtime_config_overrides(&mut options, &overrides).unwrap();

        assert_eq!(options.max_turns, 100);
        assert_eq!(options.browser_mode.as_deref(), Some("remote-cdp"));
        assert_eq!(options.python_tool_timeout_seconds, 45);
        assert!(!options.model_compaction_enabled);
        assert!(options.full_llm_input_events);
    }

    #[test]
    fn runtime_config_overrides_materialize_observability_full_llm_input_alias() {
        let overrides = parse_config_overrides(&ov(&[
            "full_llm_input_events=false",
            "observability.full_llm_input_events=true",
        ]))
        .unwrap();
        let mut options = AgentRunOptions::default();

        apply_runtime_config_overrides(&mut options, &overrides).unwrap();

        assert!(options.full_llm_input_events);
    }

    #[test]
    fn provider_run_config_new_uses_explicit_source_and_default_options() {
        let config = ProviderRunConfig::new(ProviderBackend::Anthropic, "claude-x");
        assert_eq!(config.backend, ProviderBackend::Anthropic);
        assert_eq!(config.model, "claude-x");
        assert_eq!(config.model_source, RunConfigValueSource::Explicit);
        assert!(config.fake_result.is_none());
        // options default to AgentRunOptions::default()
        assert_eq!(config.options.max_turns, 80);
    }

    #[test]
    fn provider_run_config_uses_bundled_model_context_metadata() {
        let config = ProviderRunConfig::new(ProviderBackend::Codex, "gpt-5.4");
        assert_eq!(config.context_window_tokens, 272_000);
        let metadata = config.model_context_metadata();
        assert_eq!(metadata.context_window, Some(272_000));
        assert_eq!(metadata.max_context_window, Some(1_000_000));
        assert_eq!(metadata.effective_context_window_percent, 95);
        assert_eq!(metadata.effective_context_window(), Some(258_400));
        assert_eq!(metadata.auto_compact_token_limit(), Some(244_800));
    }

    #[test]
    fn provider_run_config_builders_apply() {
        let options = AgentRunOptions::default().with_browser_mode("dom");
        let config = ProviderRunConfig::new(ProviderBackend::Fake, "fake-model")
            .with_options(options)
            .with_model_source(RunConfigValueSource::Default)
            .with_fake_result("canned");
        assert_eq!(config.model_source, RunConfigValueSource::Default);
        assert_eq!(config.fake_result.as_deref(), Some("canned"));
        assert_eq!(config.options.browser_mode.as_deref(), Some("dom"));
    }

    #[test]
    fn provider_backend_covers_all_variants() {
        // Round-trip every backend through its debug name and back, asserting the
        // full variant set matches `browser-use-core::ProviderBackend`.
        let all = [
            ProviderBackend::Codex,
            ProviderBackend::BrowserUse,
            ProviderBackend::Openai,
            ProviderBackend::Anthropic,
            ProviderBackend::Openrouter,
            ProviderBackend::Deepseek,
            ProviderBackend::Fake,
            ProviderBackend::None,
        ];
        for backend in all {
            let name = format!("{backend:?}");
            let round_tripped = match name.as_str() {
                "Codex" => ProviderBackend::Codex,
                "BrowserUse" => ProviderBackend::BrowserUse,
                "Openai" => ProviderBackend::Openai,
                "Anthropic" => ProviderBackend::Anthropic,
                "Openrouter" => ProviderBackend::Openrouter,
                "Deepseek" => ProviderBackend::Deepseek,
                "Fake" => ProviderBackend::Fake,
                "None" => ProviderBackend::None,
                other => panic!("unexpected ProviderBackend debug name: {other}"),
            };
            assert_eq!(backend, round_tripped);
        }
        assert_eq!(all.len(), 8);
    }

    #[test]
    fn deprecated_plan_mode_normalizes_to_default() {
        assert_eq!(
            AgentRunOptions::default().collaboration_mode,
            CollaborationModeKind::Default
        );
        assert_eq!(
            AgentRunOptions::default()
                .with_collaboration_mode(CollaborationModeKind::Plan)
                .collaboration_mode,
            CollaborationModeKind::Default
        );
    }

    #[test]
    fn build_config_overrides_layer_nests_dotted_paths() {
        let overrides = parse_config_overrides(&ov(&[
            "features.use_legacy_landlock=true",
            "tools.web.enabled=false",
        ]))
        .unwrap();
        let layer = build_config_overrides_layer(&overrides);

        let features = layer
            .get("features")
            .and_then(|v| v.get("use_legacy_landlock"))
            .and_then(|v| v.as_bool());
        assert_eq!(features, Some(true));

        let web = layer
            .get("tools")
            .and_then(|v| v.get("web"))
            .and_then(|v| v.get("enabled"))
            .and_then(|v| v.as_bool());
        assert_eq!(web, Some(false));
    }

    #[test]
    fn resolves_multi_agent_v2_from_profile_and_overrides() {
        let _guard = crate::test_env::lock();
        let temp = tempfile::tempdir().unwrap();
        let previous = std::env::var_os("BROWSER_USE_TERMINAL_HOME");
        unsafe {
            std::env::set_var("BROWSER_USE_TERMINAL_HOME", temp.path());
        }
        std::fs::write(
            temp.path().join("config.toml"),
            r#"
[features.multi_agent_v2]
enabled = true
max_concurrent_threads_per_session = 5
min_wait_timeout_ms = 2500
max_wait_timeout_ms = 120000
default_wait_timeout_ms = 30000
usage_hint_enabled = false
usage_hint_text = "Custom delegation guidance."
tool_namespace = "agents"
hide_spawn_agent_metadata = true
"#,
        )
        .unwrap();
        let overrides = parse_config_overrides(&ov(&[
            "features.multi_agent_v2.max_concurrent_threads_per_session=2",
            "features.multi_agent_v2.enabled=false",
        ]))
        .unwrap();

        let options = resolve_multi_agent_v2_for_profile(None, &overrides).unwrap();
        assert!(!options.enabled);
        assert_eq!(options.max_concurrent_threads_per_session, 2);
        assert_eq!(options.min_wait_timeout_ms, 2500);
        assert_eq!(options.max_wait_timeout_ms, 120000);
        assert_eq!(options.default_wait_timeout_ms, 30000);
        assert!(!options.usage_hint_enabled);
        assert_eq!(
            options.usage_hint_text.as_deref(),
            Some("Custom delegation guidance.")
        );
        assert_eq!(options.tool_namespace.as_deref(), Some("agents"));
        assert!(options.hide_spawn_agent_metadata);

        unsafe {
            match previous {
                Some(value) => std::env::set_var("BROWSER_USE_TERMINAL_HOME", value),
                None => std::env::remove_var("BROWSER_USE_TERMINAL_HOME"),
            }
        }
    }

    #[test]
    fn resolves_legacy_collab_from_profile_and_overrides() {
        let _guard = crate::test_env::lock();
        let temp = tempfile::tempdir().unwrap();
        let previous = std::env::var_os("BROWSER_USE_TERMINAL_HOME");
        unsafe {
            std::env::set_var("BROWSER_USE_TERMINAL_HOME", temp.path());
        }
        std::fs::write(
            temp.path().join("config.toml"),
            r#"
[features]
collab = true
"#,
        )
        .unwrap();
        assert!(resolve_collab_for_profile(None, &Vec::new()).unwrap());

        let overrides = parse_config_overrides(&ov(&["features.multi_agent=false"])).unwrap();
        assert!(!resolve_collab_for_profile(None, &overrides).unwrap());

        unsafe {
            match previous {
                Some(value) => std::env::set_var("BROWSER_USE_TERMINAL_HOME", value),
                None => std::env::remove_var("BROWSER_USE_TERMINAL_HOME"),
            }
        }
    }

    #[test]
    fn resolves_agent_roles_from_config_files() {
        let _guard = crate::test_env::lock();
        let temp = tempfile::tempdir().unwrap();
        let previous = std::env::var_os("BROWSER_USE_TERMINAL_HOME");
        unsafe {
            std::env::set_var("BROWSER_USE_TERMINAL_HOME", temp.path());
        }
        std::fs::create_dir_all(temp.path().join("agents")).unwrap();
        std::fs::write(
            temp.path().join("agents/researcher.toml"),
            r#"
model = "gpt-5.1"
model_provider = "openai"
model_reasoning_effort = "low"
developer_instructions = "Research narrowly."
tool_allowlist = ["shell", "rg"]
can_write = false
service_tier = "priority"
model_catalog_json = "catalog.json"
"#,
        )
        .unwrap();
        std::fs::write(
            temp.path().join("config.toml"),
            r#"
[agents.researcher]
description = "Read-only researcher."
config_file = "./agents/researcher.toml"
nickname_candidates = ["Hypatia", "Noether"]
"#,
        )
        .unwrap();

        let roles = resolve_agent_roles_for_profile(None, &Vec::new()).unwrap();
        let role = roles.get("researcher").expect("researcher role");
        assert_eq!(role.description.as_deref(), Some("Read-only researcher."));
        assert_eq!(
            role.nickname_candidates.as_deref(),
            Some(&["Hypatia".to_string(), "Noether".to_string()][..])
        );
        assert_eq!(role.overrides.model.as_deref(), Some("gpt-5.1"));
        assert_eq!(role.overrides.provider.as_deref(), Some("openai"));
        assert_eq!(role.overrides.reasoning_effort.as_deref(), Some("low"));
        assert_eq!(
            role.overrides.instructions.as_deref(),
            Some("Research narrowly.")
        );
        assert_eq!(role.overrides.tool_allowlist.as_deref().unwrap().len(), 2);
        assert_eq!(role.overrides.can_write, Some(false));
        assert_eq!(role.overrides.service_tier.as_deref(), Some("priority"));
        assert!(role
            .overrides
            .config_overrides
            .iter()
            .any(|(key, value)| key == "developer_instructions"
                && value.as_str() == Some("Research narrowly.")));
        assert!(role
            .overrides
            .config_overrides
            .iter()
            .any(|(key, value)| key == "model_catalog_json"
                && value
                    .as_str()
                    .is_some_and(|path| path.ends_with("agents/catalog.json"))));

        unsafe {
            match previous {
                Some(value) => std::env::set_var("BROWSER_USE_TERMINAL_HOME", value),
                None => std::env::remove_var("BROWSER_USE_TERMINAL_HOME"),
            }
        }
    }

    #[test]
    fn rejects_invalid_multi_agent_v2_config() {
        let _guard = crate::test_env::lock();
        let overrides = parse_config_overrides(&ov(&[
            "features.multi_agent_v2.max_concurrent_threads_per_session=0",
            "features.multi_agent_v2.min_wait_timeout_ms=1",
            "features.multi_agent_v2.default_wait_timeout_ms=1",
            "features.multi_agent_v2.max_wait_timeout_ms=1000",
        ]))
        .unwrap();
        let err = resolve_multi_agent_v2_for_profile(None, &overrides).unwrap_err();
        assert!(err
            .to_string()
            .contains("max_concurrent_threads_per_session must be at least 1"));

        let overrides = parse_config_overrides(&ov(&[
            "features.multi_agent_v2.min_wait_timeout_ms=0",
            "features.multi_agent_v2.default_wait_timeout_ms=1",
            "features.multi_agent_v2.max_wait_timeout_ms=1000",
        ]))
        .unwrap();
        let err = resolve_multi_agent_v2_for_profile(None, &overrides).unwrap_err();
        assert!(err
            .to_string()
            .contains("min_wait_timeout_ms must be at least 1"));

        let overrides = parse_config_overrides(&ov(&[
            "features.multi_agent_v2.tool_namespace=functions",
            "features.multi_agent_v2.min_wait_timeout_ms=1",
            "features.multi_agent_v2.default_wait_timeout_ms=1",
            "features.multi_agent_v2.max_wait_timeout_ms=1000",
        ]))
        .unwrap();
        let err = resolve_multi_agent_v2_for_profile(None, &overrides).unwrap_err();
        assert!(err.to_string().contains("reserved namespace"));
    }

    #[test]
    fn child_agent_runner_invokes_callback() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let calls = Arc::new(AtomicUsize::new(0));
        let calls_clone = Arc::clone(&calls);
        let runner = ChildAgentRunner::new(move |_req| {
            calls_clone.fetch_add(1, Ordering::SeqCst);
            Ok(())
        });
        runner
            .run(ChildAgentRunRequest {
                parent_session_id: "parent".to_string(),
                child_session_id: "child".to_string(),
                run_id: None,
                message: "do work".to_string(),
                input_items: None,
                input_is_inter_agent_communication: false,
                agent_path: Some("/root/child".to_string()),
                nickname: None,
                role: None,
                fork_turns: None,
                model: None,
                reasoning_effort: None,
                service_tier: None,
                config_overrides: Vec::new(),
                completion_handler: None,
            })
            .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        // Debug is redacted (finish_non_exhaustive).
        assert_eq!(format!("{runner:?}"), "ChildAgentRunner { .. }");
    }
}
