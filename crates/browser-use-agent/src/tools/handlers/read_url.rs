//! `read_url` tool: fetch public URLs and return compact readable text.
//!
//! This is a cheap source-reading surface for research/list/pricing/document
//! tasks. It fills the gap between `search` (candidate URLs) and
//! `browser_script` (interactive browser work): if a URL is public/static, the
//! agent can read several pages in one parallel-safe tool call instead of
//! spending browser turns navigating and scraping page text.

use std::sync::Arc;
use std::time::Duration;

use regex::Regex;

use crate::tools::runtime::{
    Approvable, ExecOutput, SandboxAttempt, Sandboxable, ToolCtx, ToolError, ToolRuntime,
};
use crate::tools::sandbox::{SandboxPermissions, SandboxPreference};

/// The tool name surfaced to the model.
pub const READ_URL_TOOL_NAME: &str = "read_url";

/// Whether URL reads may run concurrently with other parallel-safe tools.
pub const READ_URL_PARALLEL_SAFE: bool = true;

const DEFAULT_TIMEOUT_MS: u64 = 20_000;
const MAX_TIMEOUT_MS: u64 = 60_000;
const DEFAULT_MAX_CHARS_PER_URL: usize = 8_000;
const MAX_CHARS_PER_URL: usize = 20_000;
const MAX_URLS_PER_CALL: usize = 10;
const MAX_BODY_BYTES: usize = 2_000_000;
const MAX_LINKS: usize = 30;

/// Typed request for the `read_url` tool.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ReadUrlRequest {
    /// Single URL to read.
    #[serde(default)]
    pub url: Option<String>,
    /// Several independent URLs to read in one tool call.
    #[serde(default)]
    pub urls: Vec<String>,
    /// Optional per-URL text cap. Defaults to 8000, maximum 20000.
    #[serde(default)]
    pub max_chars_per_url: Option<usize>,
    /// Optional per-request timeout. Defaults to 20000ms, maximum 60000ms.
    #[serde(default)]
    pub timeout_ms: Option<u64>,
}

impl ReadUrlRequest {
    fn normalized_urls(&self) -> Result<Vec<String>, ToolError> {
        let mut urls = Vec::new();
        if let Some(url) = self
            .url
            .as_deref()
            .map(str::trim)
            .filter(|url| !url.is_empty())
        {
            urls.push(url.to_string());
        }
        for url in &self.urls {
            let url = url.trim();
            if !url.is_empty() && !urls.iter().any(|existing| existing == url) {
                urls.push(url.to_string());
            }
        }
        if urls.is_empty() {
            return Err(ToolError::Rejected(
                "read_url requires `url` or at least one non-empty item in `urls`".to_string(),
            ));
        }
        if urls.len() > MAX_URLS_PER_CALL {
            return Err(ToolError::Rejected(format!(
                "read_url accepts at most {MAX_URLS_PER_CALL} URLs per call"
            )));
        }
        for url in &urls {
            let parsed = reqwest::Url::parse(url).map_err(|err| {
                ToolError::Rejected(format!("read_url invalid URL `{url}`: {err}"))
            })?;
            if !matches!(parsed.scheme(), "http" | "https") {
                return Err(ToolError::Rejected(format!(
                    "read_url only supports http/https URLs, got `{url}`"
                )));
            }
        }
        Ok(urls)
    }

    fn timeout(&self) -> Duration {
        Duration::from_millis(
            self.timeout_ms
                .unwrap_or(DEFAULT_TIMEOUT_MS)
                .min(MAX_TIMEOUT_MS),
        )
    }

    fn max_chars_per_url(&self) -> usize {
        self.max_chars_per_url
            .unwrap_or(DEFAULT_MAX_CHARS_PER_URL)
            .clamp(1, MAX_CHARS_PER_URL)
    }
}

/// The fetched content for one URL.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReadUrlPage {
    pub status: u16,
    pub final_url: String,
    pub content_type: Option<String>,
    pub body: Vec<u8>,
    pub truncated: bool,
}

/// A backend error for one URL.
#[derive(Debug, thiserror::Error)]
pub enum ReadUrlError {
    #[error("{0}")]
    Request(String),
}

/// Network seam for tests.
#[async_trait::async_trait]
pub trait ReadUrlBackend: Send + Sync {
    async fn fetch(&self, url: &str, timeout: Duration) -> Result<ReadUrlPage, ReadUrlError>;
}

/// Real HTTP backend.
pub struct HttpReadUrlBackend {
    client: reqwest::Client,
}

impl HttpReadUrlBackend {
    pub fn new() -> Self {
        let client = reqwest::Client::builder()
            .user_agent("browser-use-terminal/read_url")
            .redirect(reqwest::redirect::Policy::limited(10))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        Self { client }
    }
}

impl Default for HttpReadUrlBackend {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl ReadUrlBackend for HttpReadUrlBackend {
    async fn fetch(&self, url: &str, timeout: Duration) -> Result<ReadUrlPage, ReadUrlError> {
        let response = self
            .client
            .get(url)
            .timeout(timeout)
            .send()
            .await
            .map_err(|err| ReadUrlError::Request(err.to_string()))?;
        let status = response.status().as_u16();
        let final_url = response.url().to_string();
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .map(|value| value.to_string());
        let bytes = response
            .bytes()
            .await
            .map_err(|err| ReadUrlError::Request(err.to_string()))?;
        let truncated = bytes.len() > MAX_BODY_BYTES;
        let body = bytes
            .iter()
            .copied()
            .take(MAX_BODY_BYTES)
            .collect::<Vec<u8>>();
        Ok(ReadUrlPage {
            status,
            final_url,
            content_type,
            body,
            truncated,
        })
    }
}

/// The async `read_url` tool.
#[derive(Clone)]
pub struct ReadUrlTool {
    backend: Arc<dyn ReadUrlBackend>,
}

impl Default for ReadUrlTool {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for ReadUrlTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReadUrlTool").finish_non_exhaustive()
    }
}

impl ReadUrlTool {
    pub fn new() -> Self {
        Self::with_backend(Arc::new(HttpReadUrlBackend::new()))
    }

    pub fn with_backend(backend: Arc<dyn ReadUrlBackend>) -> Self {
        Self { backend }
    }
}

#[derive(serde::Serialize, Clone, Debug, Eq, PartialEq, Hash)]
pub struct ReadUrlApprovalKey {
    urls: Vec<String>,
}

impl Approvable<ReadUrlRequest> for ReadUrlTool {
    type ApprovalKey = ReadUrlApprovalKey;

    fn approval_keys(&self, req: &ReadUrlRequest) -> Vec<Self::ApprovalKey> {
        let urls = req.normalized_urls().unwrap_or_default();
        vec![ReadUrlApprovalKey { urls }]
    }

    fn sandbox_permissions(&self, _req: &ReadUrlRequest) -> SandboxPermissions {
        SandboxPermissions::UseDefault
    }
}

impl Sandboxable for ReadUrlTool {
    fn sandbox_preference(&self) -> SandboxPreference {
        SandboxPreference::Auto
    }

    fn escalate_on_failure(&self) -> bool {
        true
    }
}

#[async_trait::async_trait]
impl ToolRuntime<ReadUrlRequest, ExecOutput> for ReadUrlTool {
    fn parallel_safe(&self, _req: &ReadUrlRequest) -> bool {
        READ_URL_PARALLEL_SAFE
    }

    async fn run(
        &self,
        req: &ReadUrlRequest,
        attempt: &SandboxAttempt<'_>,
        _ctx: &ToolCtx,
    ) -> Result<ExecOutput, ToolError> {
        let _ = attempt;

        let urls = req.normalized_urls()?;
        let timeout = req.timeout();
        let max_chars = req.max_chars_per_url();
        let futures = urls.into_iter().map(|url| {
            let backend = Arc::clone(&self.backend);
            async move {
                let result = backend.fetch(&url, timeout).await;
                (url, result)
            }
        });
        let results = futures_util::future::join_all(futures).await;
        Ok(format_read_url_results(results, max_chars))
    }
}

fn format_read_url_results(
    results: Vec<(String, Result<ReadUrlPage, ReadUrlError>)>,
    max_chars: usize,
) -> ExecOutput {
    let mut success_count = 0usize;
    let mut lines = vec![format!(
        "read_url fetched {} URL(s). Use this text as evidence; do not re-open the same URL in the browser unless interaction, cookies, or visual verification are needed.",
        results.len()
    )];

    for (idx, (requested_url, result)) in results.into_iter().enumerate() {
        lines.push(String::new());
        lines.push(format!("[{}] {requested_url}", idx + 1));
        match result {
            Ok(page) => {
                success_count += 1;
                let rendered = render_page(&page, max_chars);
                lines.extend(rendered);
            }
            Err(err) => {
                lines.push(format!("Error: {err}"));
            }
        }
    }

    ExecOutput {
        exit_code: if success_count == 0 { 1 } else { 0 },
        stdout: lines.join("\n"),
        stderr: if success_count == 0 {
            "All read_url requests failed.".to_string()
        } else {
            String::new()
        },
    }
}

fn render_page(page: &ReadUrlPage, max_chars: usize) -> Vec<String> {
    let content_type = page.content_type.as_deref().unwrap_or("unknown");
    let mut lines = vec![
        format!("Final URL: {}", page.final_url),
        format!("Status: {}", page.status),
        format!("Content-Type: {content_type}"),
    ];

    if !is_text_like(content_type, &page.body) {
        lines.push(format!(
            "Body: binary/non-text content omitted ({} byte sample{}).",
            page.body.len(),
            if page.truncated { ", truncated" } else { "" }
        ));
        return lines;
    }

    let body = String::from_utf8_lossy(&page.body);
    let title = html_title(&body);
    if let Some(title) = title.as_deref().filter(|title| !title.is_empty()) {
        lines.push(format!("Title: {title}"));
    }

    let text = if looks_like_html(content_type, &body) {
        html_to_text(&body)
    } else {
        normalize_whitespace(&body)
    };
    let (text, text_truncated) = truncate_chars(&text, max_chars);
    lines.push("Text:".to_string());
    lines.push(if text.is_empty() {
        "(empty)".to_string()
    } else {
        text
    });
    if text_truncated || page.truncated {
        lines.push("[truncated]".to_string());
    }

    let links = if looks_like_html(content_type, &body) {
        extract_links(&body, &page.final_url)
    } else {
        Vec::new()
    };
    if !links.is_empty() {
        lines.push("Links:".to_string());
        for (text, href) in links {
            let label = if text.is_empty() {
                href.as_str()
            } else {
                text.as_str()
            };
            lines.push(format!("- {} -> {}", truncate_chars(label, 120).0, href));
        }
    }
    lines
}

fn is_text_like(content_type: &str, body: &[u8]) -> bool {
    let content_type = content_type.to_ascii_lowercase();
    content_type.starts_with("text/")
        || content_type.contains("json")
        || content_type.contains("xml")
        || content_type.contains("javascript")
        || content_type.contains("csv")
        || body.starts_with(b"<!DOCTYPE")
        || body.starts_with(b"<!doctype")
        || body.starts_with(b"<html")
}

fn looks_like_html(content_type: &str, body: &str) -> bool {
    let content_type = content_type.to_ascii_lowercase();
    content_type.contains("html") || body.trim_start().starts_with('<')
}

fn html_title(html: &str) -> Option<String> {
    static TITLE_RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    let re = TITLE_RE
        .get_or_init(|| Regex::new(r"(?is)<title[^>]*>(.*?)</title>").expect("valid title regex"));
    re.captures(html)
        .and_then(|captures| captures.get(1))
        .map(|match_| decode_basic_entities(&normalize_whitespace(match_.as_str())))
}

fn html_to_text(html: &str) -> String {
    static SCRIPT_STYLE_RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    static COMMENT_RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    static BLOCK_RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    static TAG_RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();

    let script_style_re = SCRIPT_STYLE_RE.get_or_init(|| {
        Regex::new(
            r"(?is)<script[^>]*>.*?</script>|<style[^>]*>.*?</style>|<noscript[^>]*>.*?</noscript>|<svg[^>]*>.*?</svg>|<canvas[^>]*>.*?</canvas>",
        )
            .expect("valid script/style regex")
    });
    let comment_re =
        COMMENT_RE.get_or_init(|| Regex::new(r"(?is)<!--.*?-->").expect("valid comment regex"));
    let block_re = BLOCK_RE.get_or_init(|| {
        Regex::new(r"(?i)</?(p|div|section|article|header|footer|main|aside|nav|br|li|tr|h[1-6]|table|ul|ol|pre|blockquote)[^>]*>")
            .expect("valid block regex")
    });
    let tag_re = TAG_RE.get_or_init(|| Regex::new(r"(?is)<[^>]+>").expect("valid tag regex"));

    let without_scripts = script_style_re.replace_all(html, " ");
    let without_comments = comment_re.replace_all(&without_scripts, " ");
    let with_breaks = block_re.replace_all(&without_comments, "\n");
    let without_tags = tag_re.replace_all(&with_breaks, " ");
    normalize_lines(&decode_basic_entities(&without_tags))
}

fn extract_links(html: &str, base_url: &str) -> Vec<(String, String)> {
    static LINK_RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    static INNER_TAG_RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    let link_re = LINK_RE.get_or_init(|| {
        Regex::new(r#"(?is)<a\b[^>]*\bhref\s*=\s*["']([^"']+)["'][^>]*>(.*?)</a>"#)
            .expect("valid link regex")
    });
    let inner_tag_re =
        INNER_TAG_RE.get_or_init(|| Regex::new(r"(?is)<[^>]+>").expect("valid inner tag regex"));
    let base = reqwest::Url::parse(base_url).ok();
    let mut links = Vec::new();
    for captures in link_re.captures_iter(html) {
        let href = captures
            .get(1)
            .map(|m| m.as_str().trim())
            .unwrap_or_default();
        if href.is_empty() || href.starts_with('#') || href.starts_with("javascript:") {
            continue;
        }
        let absolute = match &base {
            Some(base) => base
                .join(href)
                .map(|url| url.to_string())
                .unwrap_or_else(|_| href.to_string()),
            None => href.to_string(),
        };
        if links
            .iter()
            .any(|(_, existing): &(String, String)| existing == &absolute)
        {
            continue;
        }
        let raw_text = captures.get(2).map(|m| m.as_str()).unwrap_or_default();
        let text = inner_tag_re.replace_all(raw_text, " ");
        links.push((
            decode_basic_entities(&normalize_whitespace(&text)),
            absolute,
        ));
        if links.len() >= MAX_LINKS {
            break;
        }
    }
    links
}

fn normalize_lines(text: &str) -> String {
    let mut lines = Vec::new();
    for line in text.lines() {
        let normalized = normalize_whitespace(line);
        if !normalized.is_empty() {
            lines.push(normalized);
        }
    }
    lines.join("\n")
}

fn normalize_whitespace(text: &str) -> String {
    static WHITESPACE_RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    WHITESPACE_RE
        .get_or_init(|| Regex::new(r"\s+").expect("valid whitespace regex"))
        .replace_all(text.trim(), " ")
        .into_owned()
}

fn decode_basic_entities(text: &str) -> String {
    text.replace("&nbsp;", " ")
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&apos;", "'")
}

fn truncate_chars(text: &str, max: usize) -> (String, bool) {
    if text.chars().count() <= max {
        return (text.to_string(), false);
    }
    let prefix: String = text.chars().take(max.saturating_sub(1)).collect();
    (format!("{}...", prefix.trim_end()), true)
}
