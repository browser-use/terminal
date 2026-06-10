//! `web_search` tool: the HOSTED, provider-executed web-search capability.
//!
//! # Status: NOT REGISTERED (superseded by `search`)
//!
//! This handler is no longer wired into [`default_registry`] or the production
//! dispatcher: when registered, the OpenAI Responses builder encodes it as the
//! provider-side `web_search_preview` tool, which competes with — and the
//! model prefers over — the browser-use [`search`](super::search) tool. All
//! searches now go through `search`. The handler is kept as the codex-parity
//! model of the hosted capability (and as a pure registry-test fixture) should
//! a provider-side search ever need re-enabling.
//!
//! [`default_registry`]: crate::tools::registry::default_registry
//!
//! Unlike the other handlers in this module (`shell`, `apply_patch`,
//! `view_image`, `update_plan`, `tool_search`), `web_search`
//! is **not locally dispatched**. It is a *hosted tool*: the model provider runs
//! the search server-side and streams the result back inline. The client's only
//! responsibilities are:
//!
//! 1. **Declare** the tool's availability + configuration to the provider (so the
//!    provider knows it may execute web searches on this turn), and
//! 2. **Pass through** the provider-executed result the provider already produced.
//!
//! There is therefore **no real HTTP search in this crate** — and there must not
//! be. Implementing an actual fetch here would diverge from codex, where the
//! search runs entirely provider-side. This module mirrors codex's hosted-tool
//! modeling: a thin config + tool declaration, plus a `ToolRuntime::run` that is a
//! pure **passthrough / hosted marker** (it does *no* network I/O and does *not*
//! perform a search; it returns the already-provider-supplied result or, absent a
//! pre-supplied result, surfaces that this tool is provider-executed).
//!
//! # Parity grounding (file:line)
//!
//! * **Hosted config is thin** — codex `web_search` is a ~39-line hosted helper
//!   (`/home/exedev/repos/codex/codex-rs/core/src/web_search.rs`): it only renders
//!   provider-reported `WebSearchAction`s (`Search { query, queries }` /
//!   `OpenPage { url }` / `FindInPage { url, pattern }` / `Other`) into a
//!   human-readable detail string for display — it does NOT execute a search. That
//!   confirms the action is performed provider-side; the client only formats /
//!   passes through the provider's report. Our [`web_search_detail`] mirrors that
//!   display helper field-for-field, and our [`WebSearchAction`] reproduces the
//!   codex `WebSearchAction` variant shape (`codex_protocol::models::WebSearchAction`).
//! * **Enabled + mode config** — codex gates the hosted tool on a config flag
//!   (`Config.tools_web_search` / `ToolsToggle`,
//!   `core/src/config.rs`, `config_types.rs`): when enabled the `web_search` tool
//!   spec is emitted to the provider; when disabled it is omitted. The LEGACY
//!   provider layer modeled this as a `WebSearchToolConfig` carrying a
//!   `WebSearchMode { Disabled, ... }`
//!   (`terminal-decodex/crates/browser-use-providers`, web_search modes) and
//!   `browser-use-core/src/lib.rs::supports_hosted_web_search` decided whether the
//!   active provider/model can run the hosted search at all. Our
//!   [`WebSearchConfig`] + [`WebSearchMode`] reproduce that shape: a `mode`
//!   (`Disabled` / `Enabled`) plus optional `allowed_domains` scoping, with
//!   `enabled()` deriving from the mode.
//! * **Tool name** — the hosted tool surfaces to the provider under the name
//!   `"web_search"` (codex emits the hosted `web_search` tool spec). We expose it
//!   as [`WEB_SEARCH_TOOL_NAME`].

use crate::tools::runtime::{
    Approvable, ExecOutput, SandboxAttempt, Sandboxable, ToolCtx, ToolError, ToolRuntime,
};
use crate::tools::sandbox::{SandboxPermissions, SandboxPreference};

/// The hosted tool name surfaced to the provider.
///
/// Codex parity: the hosted web-search tool is declared under the name
/// `"web_search"` (codex emits the `web_search` tool spec when the capability is
/// enabled; the provider executes it server-side).
pub const WEB_SEARCH_TOOL_NAME: &str = "web_search";

/// The mode of the hosted web-search capability.
///
/// Codex/legacy parity: the legacy provider layer modeled the hosted toggle as a
/// `WebSearchMode { Disabled, ... }` inside `WebSearchToolConfig`
/// (`terminal-decodex/crates/browser-use-providers`). Codex itself gates the
/// hosted tool on a boolean config flag (`Config.tools_web_search`,
/// `core/src/config.rs`); the mode enum here carries that boolean plus room for
/// future provider-specific modes, exactly as the legacy `WebSearchMode` did.
///
/// Wire shape: `#[serde(rename_all = "snake_case")]` so the JSON strings are
/// `"disabled"` / `"enabled"`.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, Hash, Default, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum WebSearchMode {
    /// The hosted web-search tool is NOT offered to the provider (default).
    /// Mirrors legacy `WebSearchMode::Disabled`.
    #[default]
    Disabled,
    /// The hosted web-search tool IS offered to the provider; the provider runs
    /// the search server-side.
    Enabled,
}

impl WebSearchMode {
    /// Whether this mode offers the hosted tool to the provider.
    pub fn is_enabled(self) -> bool {
        matches!(self, WebSearchMode::Enabled)
    }
}

/// Configuration for the HOSTED web-search capability.
///
/// This is the CLIENT-side declaration the provider needs to decide whether (and
/// how) to run web searches server-side. It carries no execution logic — the
/// search itself is provider-executed (see the module doc).
///
/// Codex/legacy parity: codex's thin hosted config is essentially the on/off flag
/// (`Config.tools_web_search`, `core/src/config.rs`), and the legacy
/// `WebSearchToolConfig` (`terminal-decodex/crates/browser-use-providers`) carried
/// a `WebSearchMode` plus optional domain scoping. We reproduce both: a `mode`
/// (deriving `enabled`) and an optional `allowed_domains` allow-list. `enabled` is
/// a function of `mode` (it is NOT a separate wire field) so the config cannot
/// drift into an inconsistent "enabled but mode=Disabled" state.
#[derive(Clone, Debug, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub struct WebSearchConfig {
    /// The hosted web-search mode (off by default).
    #[serde(default)]
    pub mode: WebSearchMode,
    /// Optional allow-list of domains the hosted search may consult. `None` (the
    /// default) means "no client-imposed restriction" — the provider applies its
    /// own policy. Skipped on serialize when `None` to keep the wire shape tidy
    /// and match codex's thin config (which omits absent scoping).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allowed_domains: Option<Vec<String>>,
}

impl WebSearchConfig {
    /// A disabled config: the hosted tool is NOT offered to the provider.
    pub fn disabled() -> Self {
        Self {
            mode: WebSearchMode::Disabled,
            allowed_domains: None,
        }
    }

    /// An enabled config with no domain scoping: the hosted tool IS offered to the
    /// provider, which applies its own domain policy.
    pub fn enabled() -> Self {
        Self {
            mode: WebSearchMode::Enabled,
            allowed_domains: None,
        }
    }

    /// An enabled config scoped to `domains`.
    pub fn enabled_for<I, S>(domains: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            mode: WebSearchMode::Enabled,
            allowed_domains: Some(domains.into_iter().map(Into::into).collect()),
        }
    }

    /// Whether the hosted tool should be offered to the provider this turn.
    ///
    /// Derived purely from [`mode`](WebSearchConfig::mode) so it cannot disagree
    /// with the mode. Codex parity: the hosted `web_search` spec is emitted iff
    /// the capability flag is on (`core/src/config.rs`,
    /// `supports_hosted_web_search` in legacy `browser-use-core/src/lib.rs`).
    pub fn is_enabled(&self) -> bool {
        self.mode.is_enabled()
    }
}

/// A provider-reported web-search action, used only for DISPLAY/passthrough.
///
/// Codex parity: `codex_protocol::models::WebSearchAction` (rendered by
/// `core/src/web_search.rs:18-30`). The provider reports which action it took
/// server-side (a query, an open-page, a find-in-page, or other); the client only
/// formats it for display. We reproduce the variant shape so the passthrough is
/// faithful. Wire shape uses `#[serde(rename_all = "snake_case")]` and an
/// internal `type` tag mirroring the protocol enum.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WebSearchAction {
    /// A search was performed. `query` (single) and/or `queries` (multi) are the
    /// provider-reported search terms.
    Search {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        query: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        queries: Option<Vec<String>>,
    },
    /// A page was opened.
    OpenPage {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        url: Option<String>,
    },
    /// A find-in-page was performed.
    FindInPage {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        url: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pattern: Option<String>,
    },
    /// An action the protocol does not model specifically.
    Other,
}

/// Detail string for a [`WebSearchAction::Search`].
///
/// Codex parity: `search_action_detail` (`core/src/web_search.rs:3-16`): prefer
/// the single non-empty `query`; else the first of `queries`, suffixed `" ..."`
/// when there is more than one and the first is non-empty.
fn search_action_detail(query: &Option<String>, queries: &Option<Vec<String>>) -> String {
    query.clone().filter(|q| !q.is_empty()).unwrap_or_else(|| {
        let items = queries.as_ref();
        let first = items
            .and_then(|queries| queries.first())
            .cloned()
            .unwrap_or_default();
        if items.is_some_and(|queries| queries.len() > 1) && !first.is_empty() {
            format!("{first} ...")
        } else {
            first
        }
    })
}

/// Render a provider-reported [`WebSearchAction`] into a human-readable detail.
///
/// Codex parity: `web_search_action_detail` (`core/src/web_search.rs:18-30`),
/// reproduced branch-for-branch.
pub fn web_search_action_detail(action: &WebSearchAction) -> String {
    match action {
        WebSearchAction::Search { query, queries } => search_action_detail(query, queries),
        WebSearchAction::OpenPage { url } => url.clone().unwrap_or_default(),
        WebSearchAction::FindInPage { url, pattern } => match (pattern, url) {
            (Some(pattern), Some(url)) => format!("'{pattern}' in {url}"),
            (Some(pattern), None) => format!("'{pattern}'"),
            (None, Some(url)) => url.clone(),
            (None, None) => String::new(),
        },
        WebSearchAction::Other => String::new(),
    }
}

/// Render the display detail for a hosted web search, falling back to `query`.
///
/// Codex parity: `web_search_detail` (`core/src/web_search.rs:32-39`): use the
/// action's detail when non-empty, otherwise the raw `query`.
pub fn web_search_detail(action: Option<&WebSearchAction>, query: &str) -> String {
    let detail = action.map(web_search_action_detail).unwrap_or_default();
    if detail.is_empty() {
        query.to_string()
    } else {
        detail
    }
}

/// Typed request for the hosted `web_search` tool.
///
/// In a hosted tool the *request* models what the provider reports about the
/// search it ran server-side: the `query` it issued and, optionally, the
/// structured `action` it took plus the result text it already produced. The
/// client does NOT originate a search from these fields — they carry the
/// provider's report so [`WebSearchTool::run`] can pass it through.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct WebSearchRequest {
    /// The query the provider associated with this hosted search.
    pub query: String,
    /// The structured action the provider reported, when present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action: Option<WebSearchAction>,
    /// The provider-executed result text, when the provider already supplied it.
    /// When `None`, `run` emits a hosted-marker note instead of a fabricated
    /// result (it never performs a real search).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_result: Option<String>,
}

impl WebSearchRequest {
    /// Convenience constructor from a bare query (no action, no pre-supplied
    /// result).
    pub fn new(query: impl Into<String>) -> Self {
        Self {
            query: query.into(),
            action: None,
            provider_result: None,
        }
    }

    /// Convenience constructor carrying a provider-supplied result to pass
    /// through.
    pub fn with_provider_result(query: impl Into<String>, result: impl Into<String>) -> Self {
        Self {
            query: query.into(),
            action: None,
            provider_result: Some(result.into()),
        }
    }
}

/// Prefix on the [`ExecOutput::stdout`] payload identifying a hosted-passthrough
/// emission (so a later provider-aware layer can recognize it).
///
/// This is a property of our [`ExecOutput`] seam, NOT a codex/legacy wire
/// constant: codex never locally dispatches `web_search`, so it has no such
/// stdout. It marks that the text was passed through from the provider rather than
/// produced by a local search.
pub const WEB_SEARCH_HOSTED_PREFIX: &str = "web_search(hosted):";

/// The hosted `web_search` tool.
///
/// Holds the [`WebSearchConfig`] (the client-side declaration). It performs NO
/// network I/O: the search is provider-executed. `run` is a passthrough/marker
/// (see the module doc).
#[derive(Clone, Debug, Default)]
pub struct WebSearchTool {
    config: WebSearchConfig,
}

impl WebSearchTool {
    /// Construct the tool with an explicit config.
    pub fn new(config: WebSearchConfig) -> Self {
        Self { config }
    }

    /// Construct a disabled tool (the hosted capability is not offered).
    pub fn disabled() -> Self {
        Self::new(WebSearchConfig::disabled())
    }

    /// The tool's hosted config.
    pub fn config(&self) -> &WebSearchConfig {
        &self.config
    }

    /// The hosted tool name surfaced to the provider.
    pub fn name(&self) -> &'static str {
        WEB_SEARCH_TOOL_NAME
    }

    /// Whether this tool is a HOSTED (provider-executed) tool. Always `true`:
    /// `web_search` is never locally dispatched — the provider runs the search.
    pub fn is_hosted(&self) -> bool {
        true
    }

    /// Whether the hosted tool should be declared to the provider this turn.
    /// Delegates to the config's [`WebSearchConfig::is_enabled`].
    pub fn is_enabled(&self) -> bool {
        self.config.is_enabled()
    }
}

/// Approval key: the query identifies a hosted call for session caching, mirroring
/// the shape the other non-FS tools use (`tool_search.rs:354-358`,
/// `update_plan.rs:207-210`). A hosted tool never prompts (the provider already
/// ran it), so the key is rarely consulted; it exists to satisfy [`Approvable`]
/// uniformly.
#[derive(serde::Serialize, Clone, Debug, Eq, PartialEq, Hash)]
pub struct WebSearchApprovalKey {
    query: String,
}

impl Approvable<WebSearchRequest> for WebSearchTool {
    type ApprovalKey = WebSearchApprovalKey;

    fn approval_keys(&self, req: &WebSearchRequest) -> Vec<Self::ApprovalKey> {
        vec![WebSearchApprovalKey {
            query: req.query.clone(),
        }]
    }

    /// The hosted tool touches no local filesystem; request the default sandbox
    /// permissions (no escalation), mirroring the other non-FS tools
    /// (`tool_search.rs:373-375`, `update_plan.rs:236-238`).
    fn sandbox_permissions(&self, _req: &WebSearchRequest) -> SandboxPermissions {
        SandboxPermissions::UseDefault
    }

    // `exec_approval_requirement` is intentionally left at its trait default
    // (`None`): a hosted, provider-executed search needs no client approval gate
    // (codex's `web_search` is provider-run; there is no client approval logic in
    // `core/src/web_search.rs`). Returning `None` lets the orchestrator apply
    // `default_exec_approval_requirement`, which yields `Skip` under any
    // non-prompting policy.
}

impl Sandboxable for WebSearchTool {
    fn sandbox_preference(&self) -> SandboxPreference {
        // The tool does no local I/O (the search is provider-side), so the sandbox
        // is moot; `Auto` keeps the seam uniform with the other non-FS tools
        // (`tool_search.rs:386-392`, `update_plan.rs:249-255`).
        SandboxPreference::Auto
    }

    fn escalate_on_failure(&self) -> bool {
        // The tool never produces a sandbox denial (no local I/O), so this is
        // moot; `true` keeps it uniform (`tool_search.rs:394-399`,
        // `update_plan.rs:257-262`).
        true
    }
}

#[async_trait::async_trait]
impl ToolRuntime<WebSearchRequest, ExecOutput> for WebSearchTool {
    fn parallel_safe(&self, _req: &WebSearchRequest) -> bool {
        // A hosted, provider-executed search mutates no local shared state and the
        // client `run` is a pure passthrough, so it is safe to run concurrently
        // with other tools — matching `tool_search`'s parallel-safe stance
        // (`tool_search.rs:404-413`). `true`.
        true
    }

    /// PASSTHROUGH / HOSTED MARKER — performs NO real search and NO network I/O.
    ///
    /// `web_search` is provider-executed (see the module doc). The client `run`
    /// only:
    /// * passes through a [`provider_result`](WebSearchRequest::provider_result)
    ///   the provider already supplied, or
    /// * absent one, emits a hosted marker noting the tool is provider-executed
    ///   (never a fabricated/fake search result).
    ///
    /// It also rejects being invoked while [`disabled`](WebSearchConfig::disabled)
    /// — a disabled hosted tool should not have been declared to the provider, so
    /// a call is a configuration error rather than a search to run.
    async fn run(
        &self,
        req: &WebSearchRequest,
        attempt: &SandboxAttempt<'_>,
        _ctx: &ToolCtx,
    ) -> Result<ExecOutput, ToolError> {
        // No sandbox is exercised (no local I/O); acknowledge the attempt to make
        // the seam explicit, matching the other tools.
        let _ = attempt;

        // A disabled hosted tool should never have been offered to the provider;
        // a dispatch here is a config error, not a search to perform.
        if !self.config.is_enabled() {
            return Err(ToolError::Rejected(
                "web_search is disabled (hosted tool not offered to the provider)".to_string(),
            ));
        }

        // Display detail mirrors codex's provider-report formatting
        // (`core/src/web_search.rs`): the structured action detail, else the query.
        let detail = web_search_detail(req.action.as_ref(), &req.query);

        // PASSTHROUGH: emit the provider-executed result if present; otherwise a
        // hosted marker. We NEVER perform a real HTTP search here.
        let body = match req.provider_result.as_ref() {
            Some(result) => result.clone(),
            None => format!(
                "{WEB_SEARCH_HOSTED_PREFIX} provider-executed web search for {detail:?}; \
                 result is supplied by the provider (no local execution)"
            ),
        };

        Ok(ExecOutput {
            exit_code: 0,
            stdout: body,
            stderr: String::new(),
        })
    }
}
