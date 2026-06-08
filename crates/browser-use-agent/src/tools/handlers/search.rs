//! `search` tool: a web search via the browser-use search API.
//!
//! The client POSTs the query to `search.browser-use.com` — a thin proxy in
//! front of [Parallel](https://parallel.ai)'s Search API with browser-use auth
//! and billing — and formats the returned JSON results for the model. This
//! replaced the DuckDuckGo Lite scrape the tool was originally ported from:
//! the engine changed, the tool surface (name, request shape, output layout)
//! did not. Like the other handlers it implements the full trait stack
//! ([`Approvable`] + [`Sandboxable`] + [`ToolRuntime`]) so it can be driven by
//! the [`ToolOrchestrator`](crate::tools::orchestrator::ToolOrchestrator).
//!
//! # Relationship to [`web_search`](super::web_search)
//!
//! [`web_search`](super::web_search) is the HOSTED, provider-executed web search
//! (the model provider runs the search server-side; the client only declares +
//! passes through the result — it performs *no* local HTTP). This `search` tool
//! is the opposite: the client performs the API call itself, so it works
//! against any model provider.
//!
//! # API contract (verified against the `search` service source)
//!
//! * `POST {base}/search` with JSON `{"query": "…"}` and the
//!   [`X-Browser-Use-API-Key`](SEARCH_API_KEY_HEADER) header (a `bu_…` key,
//!   read from [`BROWSER_USE_API_KEY`](SEARCH_API_KEY_ENV) — the same variable
//!   the rest of the workspace uses for browser-use cloud auth). The base URL
//!   defaults to the production service and can be overridden via
//!   [`BROWSER_USE_SEARCH_URL`](SEARCH_BASE_URL_ENV) (e.g. a local dev
//!   instance, which runs as an open proxy without auth).
//! * `200` → `{"results": [{"title"?, "url", "published_date"?, "content"}]}`;
//!   `title` / `published_date` are omitted when the source lacks them, and
//!   `content` is multi-line markdown (whitespace-normalized here).
//! * Errors: `400` invalid query, `401` missing/invalid API key, `402`
//!   insufficient balance, `422` upstream rejected the request, `502` upstream
//!   failed, `503` auth/billing backend unavailable.
//!
//! # Network seam (testability)
//!
//! The HTTP call lives behind the [`SearchBackend`] trait, with the real
//! [`HttpSearchBackend`] (a `reqwest` client) injected by default and a fake
//! substitutable in tests. This mirrors how the `browser` / `python` / `mcp`
//! handlers inject their backends, so the tool's parsing/formatting logic is
//! unit-tested deterministically with fixture JSON — no network is touched.

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use base64::Engine as _;
use regex::Regex;

use crate::tools::runtime::{
    Approvable, ExecOutput, SandboxAttempt, Sandboxable, ToolCtx, ToolError, ToolRuntime,
};
use crate::tools::sandbox::{SandboxPermissions, SandboxPreference};

/// The tool name surfaced to the model.
pub const SEARCH_TOOL_NAME: &str = "search";

/// Whether search calls may run concurrently with other parallel-safe tools.
///
/// Search is a read-only API call. Keeping it parallel-safe lets the model issue
/// independent source-discovery queries in one turn instead of paying for extra
/// LLM/browser turns.
pub const SEARCH_PARALLEL_SAFE: bool = true;

/// The browser-use search service base URL.
const SEARCH_BASE_URL: &str = "https://search.browser-use.com";

/// Environment variable overriding the search service base URL (e.g. a local
/// dev instance, `http://localhost:8080`, which runs as an open proxy without
/// auth). Defaults to [`SEARCH_BASE_URL`].
const SEARCH_BASE_URL_ENV: &str = "BROWSER_USE_SEARCH_URL";

/// Environment variable holding the `bu_…` browser-use API key. The same
/// variable the rest of the workspace uses for browser-use cloud auth
/// (`.env.example`, `browser-use-browser`).
const SEARCH_API_KEY_ENV: &str = "BROWSER_USE_API_KEY";

/// Auth header the search service expects (service `internal/api/server.go` /
/// its README: `X-Browser-Use-API-Key: bu_…`).
const SEARCH_API_KEY_HEADER: &str = "X-Browser-Use-API-Key";

/// Client-side request timeout. The service's own upstream (Parallel) timeout
/// is 30s (`UPSTREAM_TIMEOUT`); 60s gives it room to answer — including with a
/// `502` — before we cut the connection.
const SEARCH_REQUEST_TIMEOUT_SECS: u64 = 60;

/// Fallback search endpoint used only when the browser-use search service has a
/// transport/server failure. The browser-use service remains the primary path
/// because it carries our normal auth, billing, and quality contract.
const BING_SEARCH_URL: &str = "https://www.bing.com/search";

/// Fallback HTTP timeout. Keep it lower than the primary service timeout so a
/// bad fallback cannot dominate a step.
const FALLBACK_SEARCH_TIMEOUT_SECS: u64 = 20;

/// Max fallback results to expose to the model.
const MAX_FALLBACK_RESULTS: usize = 8;

/// Max characters of a result title in the formatted output. Titles are trimmed
/// (with an ellipsis counted within the cap) to keep the model-facing text token
/// efficient.
const MAX_TITLE_CHARS: usize = 30;

/// Max characters of a result description (snippet) in the formatted output.
const MAX_DESCRIPTION_CHARS: usize = 125;

/// A single parsed search result.
///
/// Mirrors the service's result object; the wire `content` (multi-line
/// markdown) is whitespace-normalized into the single-line `description`.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SearchResult {
    /// The result's title; empty when the source provided none.
    pub title: String,
    /// The result's destination URL.
    pub url: String,
    /// `YYYY-MM-DD` publication date, when the source provides one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub published_date: Option<String>,
    /// The result's content/snippet, normalized to a single line.
    pub description: String,
}

/// Typed request for the `search` tool.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SearchRequest {
    /// The search query to look up on the web.
    pub query: String,
}

impl SearchRequest {
    /// Convenience constructor from a bare query.
    pub fn new(query: impl Into<String>) -> Self {
        Self {
            query: query.into(),
        }
    }
}

/// An error from the search backend's HTTP call.
///
/// The named variants mirror the service's documented statuses so the model
/// sees an actionable message instead of a bare code.
#[derive(Debug, thiserror::Error)]
pub enum SearchError {
    /// No API key was configured; the request was not attempted.
    #[error("BROWSER_USE_API_KEY is not set – the browser-use search API requires an API key")]
    MissingApiKey,
    /// The service rejected the API key (HTTP 401).
    #[error("invalid or missing browser-use API key (HTTP 401)")]
    Unauthorized,
    /// The project balance is exhausted (HTTP 402).
    #[error("insufficient browser-use balance (HTTP 402)")]
    InsufficientBalance,
    /// Any other client/server error status (400, 422, 502, 503, …).
    #[error("HTTP {status}: {snippet}")]
    Http {
        /// The HTTP status code.
        status: u16,
        /// The first 200 chars of the response body.
        snippet: String,
    },
    /// A `200` response whose body was not the documented JSON shape.
    #[error("unexpected response body: {0}")]
    Decode(String),
    /// A transport-level error (connection, timeout, decoding).
    #[error("{0}")]
    Request(String),
}

/// The network seam: fetch the raw search-API response body for a query.
///
/// Implemented for real by [`HttpSearchBackend`] and by a fake in tests, so the
/// tool's parsing/formatting can be exercised without a real network — mirroring
/// the `browser` / `python` / `mcp` backend seams.
#[async_trait::async_trait]
pub trait SearchBackend: Send + Sync {
    /// Fetch the search service's JSON response body for `query`.
    async fn fetch(&self, query: &str) -> Result<String, SearchError>;
}

/// The real [`SearchBackend`]: a `reqwest` client against the browser-use
/// search service.
pub struct HttpSearchBackend {
    client: reqwest::Client,
    base_url: String,
    api_key: Option<String>,
}

/// Lightweight HTML-search fallback used when the browser-use search service is
/// temporarily unreachable. It returns the same JSON shape as the primary
/// service so the tool's parser/formatter stays unchanged.
pub struct BingHtmlSearchBackend {
    client: reqwest::Client,
}

impl BingHtmlSearchBackend {
    /// Construct the fallback backend.
    pub fn new() -> Self {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(FALLBACK_SEARCH_TIMEOUT_SECS))
            .user_agent("Mozilla/5.0 (compatible; browser-use-terminal/search-fallback)")
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        Self { client }
    }
}

impl Default for BingHtmlSearchBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl HttpSearchBackend {
    /// Construct the backend from the environment: the base URL from
    /// [`BROWSER_USE_SEARCH_URL`](SEARCH_BASE_URL_ENV) (defaulting to the
    /// production [`SEARCH_BASE_URL`]) and the API key from
    /// [`BROWSER_USE_API_KEY`](SEARCH_API_KEY_ENV).
    pub fn new() -> Self {
        let base_url = std::env::var(SEARCH_BASE_URL_ENV)
            .ok()
            .map(|url| url.trim().trim_end_matches('/').to_string())
            .filter(|url| !url.is_empty())
            .unwrap_or_else(|| SEARCH_BASE_URL.to_string());
        let api_key = std::env::var(SEARCH_API_KEY_ENV)
            .ok()
            .map(|key| key.trim().to_string())
            .filter(|key| !key.is_empty());
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(SEARCH_REQUEST_TIMEOUT_SECS))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        Self {
            client,
            base_url,
            api_key,
        }
    }
}

impl Default for HttpSearchBackend {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl SearchBackend for HttpSearchBackend {
    async fn fetch(&self, query: &str) -> Result<String, SearchError> {
        // The production service always requires a key: fail fast with an
        // actionable message instead of a guaranteed 401 round-trip. A custom
        // endpoint (BROWSER_USE_SEARCH_URL, e.g. a local dev instance) may be
        // an open proxy, so keyless requests are allowed through there.
        if self.api_key.is_none() && self.base_url == SEARCH_BASE_URL {
            return Err(SearchError::MissingApiKey);
        }

        let mut request = self
            .client
            .post(format!("{}/search", self.base_url))
            .json(&serde_json::json!({ "query": query }));
        if let Some(api_key) = self.api_key.as_deref() {
            request = request.header(SEARCH_API_KEY_HEADER, api_key);
        }
        let response = request
            .send()
            .await
            .map_err(|err| SearchError::Request(err.to_string()))?;

        let status = response.status().as_u16();
        let body = response
            .text()
            .await
            .map_err(|err| SearchError::Request(err.to_string()))?;

        classify_response(status, &body)?;
        Ok(body)
    }
}

#[async_trait::async_trait]
impl SearchBackend for BingHtmlSearchBackend {
    async fn fetch(&self, query: &str) -> Result<String, SearchError> {
        let response = self
            .client
            .get(BING_SEARCH_URL)
            .query(&[("q", query)])
            .send()
            .await
            .map_err(|err| SearchError::Request(format!("Bing fallback request failed: {err}")))?;

        let status = response.status().as_u16();
        let body = response
            .text()
            .await
            .map_err(|err| SearchError::Request(format!("Bing fallback body failed: {err}")))?;

        classify_response(status, &body)?;
        let wire_results: Vec<serde_json::Value> = parse_bing_html_results(&body)
            .into_iter()
            .map(|result| {
                serde_json::json!({
                    "title": result.title,
                    "url": result.url,
                    "published_date": result.published_date,
                    "content": result.description,
                })
            })
            .collect();
        Ok(serde_json::json!({ "results": wire_results }).to_string())
    }
}

/// Classify an HTTP response per the service's documented statuses: `401` and
/// `402` get named, actionable errors; any other `>= 400` (400 invalid query,
/// 422 upstream rejected, 502 upstream failed, 503 auth backend down) carries
/// the status plus the first 200 chars of the body; everything else is success.
pub fn classify_response(status: u16, body: &str) -> Result<(), SearchError> {
    match status {
        401 => Err(SearchError::Unauthorized),
        402 => Err(SearchError::InsufficientBalance),
        s if s >= 400 => {
            let snippet: String = body.chars().take(200).collect();
            Err(SearchError::Http { status: s, snippet })
        }
        _ => Ok(()),
    }
}

/// The async `search` tool.
///
/// Holds the injected [`SearchBackend`]. Cheap to clone (the backend is behind
/// an `Arc`).
#[derive(Clone)]
pub struct SearchTool {
    backend: Arc<dyn SearchBackend>,
    fallback_backend: Option<Arc<dyn SearchBackend>>,
}

impl Default for SearchTool {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for SearchTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The backend is an opaque trait object; show only the tool identity.
        f.debug_struct("SearchTool").finish_non_exhaustive()
    }
}

impl SearchTool {
    /// Construct the tool backed by the real [`HttpSearchBackend`].
    pub fn new() -> Self {
        Self {
            backend: Arc::new(HttpSearchBackend::new()),
            fallback_backend: Some(Arc::new(BingHtmlSearchBackend::new())),
        }
    }

    /// Construct the tool with a custom backend (used by tests).
    pub fn with_backend(backend: Arc<dyn SearchBackend>) -> Self {
        Self {
            backend,
            fallback_backend: None,
        }
    }

    /// Construct the tool with custom primary and fallback backends (used by
    /// focused tests).
    pub fn with_backend_and_fallback(
        backend: Arc<dyn SearchBackend>,
        fallback_backend: Option<Arc<dyn SearchBackend>>,
    ) -> Self {
        Self {
            backend,
            fallback_backend,
        }
    }

    /// The tool name surfaced to the model.
    pub fn name(&self) -> &'static str {
        SEARCH_TOOL_NAME
    }
}

/// Approval key: the query identifies a call for session caching, mirroring the
/// shape the other non-FS tools use (`tool_search.rs`, `web_search.rs`). This
/// tool is read-only and benign, so the key is rarely consulted; it exists to
/// satisfy the [`Approvable`] contract uniformly.
#[derive(serde::Serialize, Clone, Debug, Eq, PartialEq, Hash)]
pub struct SearchApprovalKey {
    query: String,
}

impl Approvable<SearchRequest> for SearchTool {
    type ApprovalKey = SearchApprovalKey;

    fn approval_keys(&self, req: &SearchRequest) -> Vec<Self::ApprovalKey> {
        vec![SearchApprovalKey {
            query: req.query.clone(),
        }]
    }

    /// `search` touches no filesystem; request the default sandbox permissions
    /// (no escalation), mirroring the other non-FS tools.
    fn sandbox_permissions(&self, _req: &SearchRequest) -> SandboxPermissions {
        SandboxPermissions::UseDefault
    }

    // `exec_approval_requirement` is intentionally left at its trait default
    // (`None`): the search is a benign, read-only query against the browser-use
    // search API. Returning `None` lets the orchestrator apply
    // `default_exec_approval_requirement`, which yields `Skip` under any
    // non-prompting policy. The outbound request mirrors the crate's existing
    // network usage (the MCP HTTP client, analytics) which is likewise ungated.
}

impl Sandboxable for SearchTool {
    fn sandbox_preference(&self) -> SandboxPreference {
        // Let the provider decide (today everything resolves to
        // `SandboxType::None`). Keeps the seam uniform with the other non-FS
        // tools.
        SandboxPreference::Auto
    }

    fn escalate_on_failure(&self) -> bool {
        // The tool never produces a sandbox denial, so this is moot; `true` keeps
        // it uniform with the other tools.
        true
    }
}

#[async_trait::async_trait]
impl ToolRuntime<SearchRequest, ExecOutput> for SearchTool {
    fn parallel_safe(&self, _req: &SearchRequest) -> bool {
        SEARCH_PARALLEL_SAFE
    }

    async fn run(
        &self,
        req: &SearchRequest,
        attempt: &SandboxAttempt<'_>,
        _ctx: &ToolCtx,
    ) -> Result<ExecOutput, ToolError> {
        // No sandbox is exercised (the tool does no FS I/O); acknowledge the
        // attempt to make the seam explicit, matching the other tools.
        let _ = attempt;

        let query = req.query.trim();
        if query.is_empty() {
            return Err(ToolError::Rejected(
                "search query must not be empty".to_string(),
            ));
        }

        // A fetch/parse failure is surfaced to the model as a soft error
        // (nonzero exit with the message on stderr), mirroring the MCP
        // handler's model-facing error mapping — not a hard tool error.
        let results = match self
            .backend
            .fetch(query)
            .await
            .and_then(|body| parse_results(&body))
        {
            Ok(results) => Ok(results),
            Err(primary_error) => {
                if !search_error_allows_fallback(&primary_error) {
                    Err(primary_error)
                } else if let Some(fallback) = self.fallback_backend.as_ref() {
                    match fallback
                        .fetch(query)
                        .await
                        .and_then(|body| parse_results(&body))
                    {
                        Ok(results) => Ok(results),
                        Err(fallback_error) => Err(SearchError::Request(format!(
                            "{primary_error}; fallback search also failed: {fallback_error}"
                        ))),
                    }
                } else {
                    Err(primary_error)
                }
            }
        };

        match results {
            Ok(results) => {
                let stdout = if results.is_empty() {
                    format!("No results found for \"{query}\".")
                } else {
                    format_results(query, &results)
                };
                Ok(ExecOutput {
                    exit_code: 0,
                    stdout,
                    stderr: String::new(),
                })
            }
            Err(err) => Ok(ExecOutput {
                exit_code: 1,
                stdout: String::new(),
                stderr: format!("Search failed: {err}"),
            }),
        }
    }
}

// ---------------------------------------------------------------------------
// Pure helpers (parsing + formatting).
// ---------------------------------------------------------------------------

/// Wire shape of the service's `200` response: `{"results": [...]}`.
#[derive(serde::Deserialize)]
struct SearchResponseWire {
    #[serde(default)]
    results: Vec<SearchResultWire>,
}

/// Wire shape of one result. `title` / `published_date` are omitted when the
/// source lacks them; everything defaults so one sparse result cannot fail the
/// whole response.
#[derive(serde::Deserialize)]
struct SearchResultWire {
    #[serde(default)]
    title: String,
    #[serde(default)]
    url: String,
    #[serde(default)]
    published_date: Option<String>,
    #[serde(default)]
    content: String,
}

/// Parse the search service's JSON response body into results.
///
/// The wire `content` arrives as multi-line markdown; it is whitespace-
/// normalized into the single-line `description`. Results without a `url` are
/// dropped (the model cannot follow them). A body that is not the documented
/// JSON shape is a [`SearchError::Decode`].
pub fn parse_results(body: &str) -> Result<Vec<SearchResult>, SearchError> {
    let wire: SearchResponseWire =
        serde_json::from_str(body).map_err(|err| SearchError::Decode(err.to_string()))?;

    Ok(wire
        .results
        .into_iter()
        .filter(|result| !result.url.trim().is_empty())
        .map(|result| SearchResult {
            title: normalize_whitespace(&result.title),
            url: result.url.trim().to_string(),
            published_date: result
                .published_date
                .map(|date| date.trim().to_string())
                .filter(|date| !date.is_empty()),
            description: normalize_whitespace(&result.content),
        })
        .collect())
}

fn search_error_allows_fallback(err: &SearchError) -> bool {
    match err {
        SearchError::Request(_) => true,
        SearchError::Http { status, .. } if *status >= 500 => true,
        _ => false,
    }
}

/// Parse Bing HTML result blocks into normalized search results. This fallback
/// parser is intentionally small and skip-on-malformed; the primary search API
/// remains authoritative whenever it is reachable.
pub(crate) fn parse_bing_html_results(body: &str) -> Vec<SearchResult> {
    let mut results = Vec::new();
    let starts: Vec<usize> = bing_result_start_regex()
        .find_iter(body)
        .map(|m| m.start())
        .collect();
    for (index, start) in starts.iter().copied().enumerate() {
        let end = starts
            .get(index + 1)
            .copied()
            .or_else(|| body[start..].find("</ol>").map(|offset| start + offset))
            .unwrap_or(body.len());
        let block = &body[start..end];
        let Some(link) = bing_link_regex().captures(block) else {
            continue;
        };
        let Some(raw_href) = link.get(1).map(|m| m.as_str()) else {
            continue;
        };
        let Some(url) = normalize_bing_href(raw_href) else {
            continue;
        };
        let title = link
            .get(2)
            .map(|m| clean_html_text(m.as_str()))
            .filter(|text| !text.is_empty())
            .unwrap_or_else(|| url.clone());
        let description = bing_snippet_regex()
            .captures(block)
            .and_then(|snippet| snippet.get(1))
            .map(|m| clean_html_text(m.as_str()))
            .unwrap_or_default();

        results.push(SearchResult {
            title,
            url,
            published_date: None,
            description,
        });

        if results.len() >= MAX_FALLBACK_RESULTS {
            break;
        }
    }
    results
}

/// Format parsed results into the readable text block the model sees.
///
/// A header (count + the "you already have the results" guidance), then a
/// numbered list with each result's title (publication date appended when
/// known), `URL:` line, and optional snippet, blank-line separated. The title
/// and description are truncated ([`MAX_TITLE_CHARS`] /
/// [`MAX_DESCRIPTION_CHARS`]) for token efficiency; URLs are kept intact so
/// they remain usable.
pub fn format_results(query: &str, results: &[SearchResult]) -> String {
    let mut lines: Vec<String> = Vec::with_capacity(results.len() * 4 + 1);
    lines.push(format!(
        "Search results for \"{query}\" ({} results):\n\
         You already have the results below – do NOT navigate to a search engine.\n\
         If these snippets are not enough, navigate directly to the result URLs for more detail.\n",
        results.len()
    ));
    for (i, result) in results.iter().enumerate() {
        // Fall back to the URL when the source provided no title.
        let title = if result.title.is_empty() {
            result.url.as_str()
        } else {
            result.title.as_str()
        };
        let mut title_line = format!("{}. {}", i + 1, truncate_chars(title, MAX_TITLE_CHARS));
        if let Some(date) = result.published_date.as_deref() {
            title_line.push_str(&format!(" ({date})"));
        }
        lines.push(title_line);
        lines.push(format!("   URL: {}", result.url));
        if !result.description.is_empty() {
            lines.push(format!(
                "   {}",
                truncate_chars(&result.description, MAX_DESCRIPTION_CHARS)
            ));
        }
        lines.push(String::new());
    }
    lines.join("\n")
}

/// Truncate `text` to at most `max` characters (Unicode scalar values). When it
/// must cut, the last kept character is an ellipsis `…`, so the result is never
/// longer than `max` and the truncation is visible. Trailing whitespace before
/// the ellipsis is trimmed so the text reads cleanly.
fn truncate_chars(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    // Reserve one character for the ellipsis.
    let prefix: String = text.chars().take(max.saturating_sub(1)).collect();
    format!("{}…", prefix.trim_end())
}

/// Collapse runs of whitespace into a single space and trim the ends.
pub fn normalize_whitespace(text: &str) -> String {
    whitespace_regex()
        .replace_all(text.trim(), " ")
        .into_owned()
}

fn clean_html_text(text: &str) -> String {
    normalize_whitespace(&decode_html_entities(
        &html_tag_regex().replace_all(text, " "),
    ))
}

fn normalize_bing_href(raw_href: &str) -> Option<String> {
    let href = decode_html_entities(raw_href);
    if let Some(encoded) = query_param(&href, "u") {
        let decoded = percent_decode_query_component(&encoded);
        if let Some(rest) = decoded.strip_prefix("a1") {
            if let Ok(bytes) = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(rest) {
                if let Ok(url) = String::from_utf8(bytes) {
                    return normalize_search_url(&url);
                }
            }
        }
        if let Some(url) = normalize_search_url(&decoded) {
            return Some(url);
        }
    }
    normalize_search_url(&href)
}

fn normalize_search_url(url: &str) -> Option<String> {
    let url = url.trim();
    if url.starts_with("https://") || url.starts_with("http://") {
        Some(url.to_string())
    } else if url.starts_with("//") {
        Some(format!("https:{url}"))
    } else {
        None
    }
}

fn query_param(url: &str, name: &str) -> Option<String> {
    let query = url.split_once('?')?.1;
    for part in query.split('&') {
        let (key, value) = part.split_once('=').unwrap_or((part, ""));
        if key == name {
            return Some(value.to_string());
        }
    }
    None
}

fn percent_decode_query_component(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                if let (Some(high), Some(low)) = (hex_value(bytes[i + 1]), hex_value(bytes[i + 2]))
                {
                    out.push((high << 4) | low);
                    i += 3;
                } else {
                    out.push(bytes[i]);
                    i += 1;
                }
            }
            byte => {
                out.push(byte);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn decode_html_entities(text: &str) -> String {
    html_entity_regex()
        .replace_all(text, |captures: &regex::Captures<'_>| {
            decode_html_entity(captures.get(1).map(|m| m.as_str()).unwrap_or_default())
        })
        .into_owned()
}

fn decode_html_entity(entity: &str) -> String {
    match entity {
        "amp" => "&".to_string(),
        "quot" => "\"".to_string(),
        "apos" | "#39" => "'".to_string(),
        "lt" => "<".to_string(),
        "gt" => ">".to_string(),
        "nbsp" => " ".to_string(),
        _ if entity.starts_with("#x") || entity.starts_with("#X") => {
            u32::from_str_radix(&entity[2..], 16)
                .ok()
                .and_then(char::from_u32)
                .map(|ch| ch.to_string())
                .unwrap_or_else(|| format!("&{entity};"))
        }
        _ if entity.starts_with('#') => entity[1..]
            .parse::<u32>()
            .ok()
            .and_then(char::from_u32)
            .map(|ch| ch.to_string())
            .unwrap_or_else(|| format!("&{entity};")),
        _ => format!("&{entity};"),
    }
}

fn whitespace_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\s+").expect("valid whitespace regex"))
}

fn bing_result_start_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r#"(?is)<li\s+class="b_algo"[^>]*>"#).expect("valid Bing result start regex")
    })
}

fn bing_link_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r#"(?is)<h2[^>]*>.*?<a[^>]+href="([^"]+)"[^>]*>(.*?)</a>"#)
            .expect("valid Bing link regex")
    })
}

fn bing_snippet_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r#"(?is)<p[^>]*>(.*?)</p>"#).expect("valid snippet regex"))
}

fn html_tag_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?is)<[^>]+>").expect("valid HTML tag regex"))
}

fn html_entity_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"&(#x?[0-9A-Fa-f]+|[A-Za-z][A-Za-z0-9]+);").expect("valid HTML entity regex")
    })
}
