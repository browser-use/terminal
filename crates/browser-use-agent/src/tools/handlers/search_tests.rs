//! Tests for the async `search` tool ([`SearchTool`]).
//!
//! No real network is touched: the pure parsing/formatting helpers are
//! exercised against fixture JSON, and the `run` path is driven through a fake
//! [`SearchBackend`] (mirroring `update_plan_tests` / `tool_search_tests`).

use std::sync::Arc;

use super::search::{
    classify_response, format_results, normalize_whitespace, parse_results, SearchBackend,
    SearchError, SearchRequest, SearchResult, SearchTool, SEARCH_PARALLEL_SAFE, SEARCH_TOOL_NAME,
};
use crate::tools::approval::AskForApproval;
use crate::tools::orchestrator::{ToolOrchestrator, TurnEnv};
use crate::tools::runtime::{
    Approvable, AutoApprover, SandboxAttempt, ToolCtx, ToolError, ToolRuntime,
};
use crate::tools::sandbox::{
    FileSystemSandboxPolicy, NoneSandboxProvider, SandboxLaunch, SandboxPermissions, SandboxType,
};

// ---- test scaffolding (mirrors update_plan_tests) -------------------------

fn none_launch() -> SandboxLaunch {
    SandboxLaunch {
        sandbox: SandboxType::None,
        cancel: None,
    }
}

fn none_attempt(launch: &SandboxLaunch) -> SandboxAttempt<'_> {
    SandboxAttempt {
        sandbox: SandboxType::None,
        permissions: SandboxPermissions::UseDefault,
        enforce_managed_network: false,
        launch,
        cancel: None,
    }
}

fn ctx() -> ToolCtx {
    ToolCtx {
        call_id: "test-call".to_string(),
        tool_name: "search".to_string(),
        cwd: std::env::temp_dir(),
        artifact_root: std::env::temp_dir().join("artifacts"),
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

/// A fake backend returning a canned response body (no network).
struct StubBackend(String);

#[async_trait::async_trait]
impl SearchBackend for StubBackend {
    async fn fetch(&self, _query: &str) -> Result<String, SearchError> {
        Ok(self.0.clone())
    }
}

/// A fake backend failing with a 401 (no network).
struct UnauthorizedBackend;

#[async_trait::async_trait]
impl SearchBackend for UnauthorizedBackend {
    async fn fetch(&self, _query: &str) -> Result<String, SearchError> {
        Err(SearchError::Unauthorized)
    }
}

/// A realistic search-service response fixture exercising: a full result
/// (title + date + multi-line markdown content), a result without a
/// `published_date`, a result without a `title` (URL fallback), and a result
/// without a `url` (dropped).
const FIXTURE: &str = r##"{
  "results": [
    {
      "title": "Genpact and Parallel Web Systems Partner to Drive Tangible Efficiency from AI Systems",
      "url": "https://www.prnewswire.com/news-releases/genpact-parallel-302736563.html",
      "published_date": "2026-04-08",
      "content": "# Genpact and Parallel\n## Share this article\nIntegrating  Parallel's   API helps\nGenpact automate research workflows."
    },
    {
      "title": "Parallel raises $100M",
      "url": "https://www.linkedin.com/posts/example-activity",
      "content": "Nov 12, 2025 · The startup secured a $100 million Series A round."
    },
    {
      "url": "https://untitled.example.com/page",
      "published_date": "2026-05-19",
      "content": "A result whose source provided no title."
    },
    {
      "title": "No URL – must be dropped",
      "content": "this result has no url and is filtered out"
    }
  ]
}"##;

// ---- pure helpers: normalize_whitespace -----------------------------------

#[test]
fn normalize_whitespace_collapses_and_trims() {
    assert_eq!(normalize_whitespace("  a \n\t b   c \r\n"), "a b c");
    assert_eq!(normalize_whitespace("single"), "single");
    assert_eq!(normalize_whitespace("   "), "");
}

// ---- pure helpers: parse_results -------------------------------------------

#[test]
fn parse_results_maps_wire_results() {
    let results = parse_results(FIXTURE).unwrap();

    // The url-less result is dropped; the other three are kept in order.
    assert_eq!(results.len(), 3);

    // Full result: title, url, date, and content normalized to one line.
    assert_eq!(
        results[0].title,
        "Genpact and Parallel Web Systems Partner to Drive Tangible Efficiency from AI Systems"
    );
    assert_eq!(
        results[0].url,
        "https://www.prnewswire.com/news-releases/genpact-parallel-302736563.html"
    );
    assert_eq!(results[0].published_date.as_deref(), Some("2026-04-08"));
    assert_eq!(
        results[0].description,
        "# Genpact and Parallel ## Share this article Integrating Parallel's API helps Genpact automate research workflows."
    );

    // Date is optional.
    assert_eq!(results[1].title, "Parallel raises $100M");
    assert_eq!(results[1].published_date, None);

    // Title is optional (empty when the source provided none).
    assert_eq!(results[2].title, "");
    assert_eq!(results[2].url, "https://untitled.example.com/page");
    assert_eq!(results[2].published_date.as_deref(), Some("2026-05-19"));
}

#[test]
fn parse_results_handles_empty_and_missing_results() {
    assert!(parse_results(r#"{"results": []}"#).unwrap().is_empty());
    // `results` defaults when absent.
    assert!(parse_results("{}").unwrap().is_empty());
}

/// The formatted output instructs the model to navigate to result URLs, so
/// only `http(s)://` destinations may surface; unsafe schemes are dropped.
#[test]
fn parse_results_drops_non_http_urls() {
    let body = r##"{"results": [
        {"title": "ok https", "url": "https://safe.example.com/", "content": "keep"},
        {"title": "ok http", "url": "http://plain.example.com/", "content": "keep"},
        {"title": "xss", "url": "javascript:alert(1)", "content": "drop"},
        {"title": "data", "url": "data:text/html,hi", "content": "drop"},
        {"title": "relative", "url": "/relative/path", "content": "drop"},
        {"title": "empty", "url": "", "content": "drop"}
    ]}"##;
    let results = parse_results(body).unwrap();
    let urls: Vec<&str> = results.iter().map(|r| r.url.as_str()).collect();
    assert_eq!(
        urls,
        vec!["https://safe.example.com/", "http://plain.example.com/"]
    );
}

#[test]
fn parse_results_rejects_malformed_bodies() {
    for body in ["not json", "", r#"{"results": "nope"}"#, "[1,2,3]"] {
        let err = parse_results(body).unwrap_err();
        assert!(
            matches!(err, SearchError::Decode(_)),
            "expected Decode for {body:?}, got {err:?}"
        );
    }
}

// ---- pure helpers: format_results -----------------------------------------

#[test]
fn format_results_renders_header_and_numbered_entries() {
    let results = vec![
        SearchResult {
            title: "First".to_string(),
            url: "https://a.example/".to_string(),
            published_date: Some("2026-04-08".to_string()),
            description: "first snippet".to_string(),
        },
        SearchResult {
            title: "Second".to_string(),
            url: "https://b.example/".to_string(),
            published_date: None,
            description: String::new(),
        },
    ];
    let out = format_results("my query", &results);

    assert!(
        out.contains("Search results for \"my query\" (2 results):"),
        "got: {out}"
    );
    assert!(
        out.contains("do NOT navigate to a search engine"),
        "got: {out}"
    );
    // The publication date is appended to the title line when known.
    assert!(out.contains("1. First (2026-04-08)"), "got: {out}");
    assert!(out.contains("   URL: https://a.example/"), "got: {out}");
    assert!(out.contains("   first snippet"), "got: {out}");
    // No date -> bare title line.
    assert!(out.contains("2. Second\n"), "got: {out}");
    assert!(out.contains("   URL: https://b.example/"), "got: {out}");
}

#[test]
fn format_results_falls_back_to_url_for_untitled_results() {
    let results = vec![SearchResult {
        title: String::new(),
        url: "https://untitled.example.com/page".to_string(),
        published_date: None,
        description: "snippet".to_string(),
    }];
    let out = format_results("q", &results);
    // The fallback title is the URL, subject to the same 30-char cap.
    assert!(
        out.contains("1. https://untitled.example.com/…"),
        "untitled result should show its URL as the title: {out}"
    );
}

#[test]
fn format_results_truncates_long_title_and_description() {
    let results = vec![SearchResult {
        title: "ThisIsAVeryLongResultTitleThatExceedsThirtyCharacters".to_string(),
        url: "https://example.com/keep/this/whole/url".to_string(),
        published_date: None,
        description: "d".repeat(250),
    }];
    let out = format_results("q", &results);

    // Title capped at 30 characters including the ellipsis.
    let title = out
        .lines()
        .find_map(|l| l.strip_prefix("1. "))
        .expect("title line");
    assert_eq!(title.chars().count(), 30, "title capped at 30: {title:?}");
    assert!(title.ends_with('…'), "title ellipsized: {title:?}");
    assert!(
        title.starts_with("ThisIsAVeryLong"),
        "title prefix: {title:?}"
    );
    assert!(!out.contains("Characters"), "tail must be dropped: {out}");

    // URL is kept intact (not truncated).
    assert!(
        out.contains("https://example.com/keep/this/whole/url"),
        "url kept: {out}"
    );

    // Description capped at 125 characters including the ellipsis.
    let desc_line = out.lines().find(|l| l.starts_with("   d")).expect("desc");
    let desc = desc_line.strip_prefix("   ").unwrap();
    assert_eq!(desc.chars().count(), 125, "description capped at 125");
    assert!(desc.ends_with('…'), "description ellipsized: {desc:?}");
}

// ---- pure helpers: classify_response --------------------------------------

#[test]
fn classify_response_names_auth_and_billing_errors() {
    assert!(matches!(
        classify_response(401, "unauthorized"),
        Err(SearchError::Unauthorized)
    ));
    assert!(matches!(
        classify_response(402, "payment required"),
        Err(SearchError::InsufficientBalance)
    ));
}

#[test]
fn classify_response_flags_other_errors_with_snippet() {
    // 400 invalid query, 429 rate limited, 502/503 upstream down — all
    // carry the status + body snippet.
    for status in [400u16, 429, 502, 503] {
        match classify_response(status, "boom") {
            Err(SearchError::Http {
                status: got,
                snippet,
            }) => {
                assert_eq!(got, status);
                assert_eq!(snippet, "boom");
            }
            other => panic!("expected Http for {status}, got {other:?}"),
        }
    }
    // The snippet is truncated to 200 chars.
    let body = "x".repeat(500);
    match classify_response(500, &body) {
        Err(SearchError::Http { snippet, .. }) => {
            assert_eq!(snippet.chars().count(), 200, "snippet truncated");
        }
        other => panic!("expected Http error, got {other:?}"),
    }
}

#[test]
fn classify_response_accepts_ok_and_pins_the_400_boundary() {
    assert!(classify_response(200, r#"{"results":[]}"#).is_ok());
    // The 399-ok / 400-error boundary pins against an off-by-one in `>= 400`.
    assert!(classify_response(399, "ok").is_ok());
    assert!(matches!(
        classify_response(400, "bad"),
        Err(SearchError::Http { status: 400, .. })
    ));
}

// ---- run() through the fake backend ---------------------------------------

#[tokio::test]
async fn run_formats_results_from_backend_json() {
    let tool = SearchTool::with_backend(Arc::new(StubBackend(FIXTURE.to_string())));
    let launch = none_launch();
    let attempt = none_attempt(&launch);
    let out = tool
        .run(&SearchRequest::new("parallel"), &attempt, &ctx())
        .await
        .unwrap();

    assert_eq!(out.exit_code, 0);
    assert!(out.stderr.is_empty());
    assert!(
        out.stdout
            .contains("Search results for \"parallel\" (3 results):"),
        "got: {}",
        out.stdout
    );
    // Title truncated to 30 chars (incl. ellipsis) with the date appended.
    assert!(
        out.stdout
            .contains("1. Genpact and Parallel Web Syst… (2026-04-08)"),
        "got: {}",
        out.stdout
    );
    // URLs are kept intact.
    assert!(
        out.stdout
            .contains("https://www.prnewswire.com/news-releases/genpact-parallel-302736563.html"),
        "got: {}",
        out.stdout
    );
    // Multi-line markdown content arrives normalized to one line.
    assert!(
        out.stdout
            .contains("# Genpact and Parallel ## Share this article"),
        "got: {}",
        out.stdout
    );
}

#[tokio::test]
async fn run_reports_no_results() {
    let tool = SearchTool::with_backend(Arc::new(StubBackend(r#"{"results":[]}"#.to_string())));
    let launch = none_launch();
    let attempt = none_attempt(&launch);
    let out = tool
        .run(&SearchRequest::new("obscure"), &attempt, &ctx())
        .await
        .unwrap();

    assert_eq!(out.exit_code, 0);
    assert_eq!(out.stdout, "No results found for \"obscure\".");
}

#[tokio::test]
async fn run_rejects_empty_query() {
    let tool = SearchTool::with_backend(Arc::new(StubBackend(String::new())));
    let launch = none_launch();
    let attempt = none_attempt(&launch);
    let err = tool
        .run(&SearchRequest::new("   "), &attempt, &ctx())
        .await
        .unwrap_err();
    let ToolError::Rejected(msg) = err else {
        panic!("expected Rejected, got {err:?}");
    };
    assert!(msg.contains("must not be empty"), "got: {msg}");
}

#[tokio::test]
async fn run_surfaces_backend_failure_as_soft_error() {
    let tool = SearchTool::with_backend(Arc::new(UnauthorizedBackend));
    let launch = none_launch();
    let attempt = none_attempt(&launch);
    let out = tool
        .run(&SearchRequest::new("parallel"), &attempt, &ctx())
        .await
        .unwrap();

    // A fetch failure is a soft, model-visible error (nonzero exit + stderr),
    // not a hard tool error.
    assert_eq!(out.exit_code, 1);
    assert!(out.stdout.is_empty());
    assert!(
        out.stderr.contains("Search failed:") && out.stderr.contains("API key"),
        "got: {}",
        out.stderr
    );
}

#[tokio::test]
async fn run_surfaces_malformed_body_as_soft_error() {
    let tool = SearchTool::with_backend(Arc::new(StubBackend("not json".to_string())));
    let launch = none_launch();
    let attempt = none_attempt(&launch);
    let out = tool
        .run(&SearchRequest::new("parallel"), &attempt, &ctx())
        .await
        .unwrap();

    assert_eq!(out.exit_code, 1);
    assert!(
        out.stderr.contains("Search failed:") && out.stderr.contains("unexpected response body"),
        "got: {}",
        out.stderr
    );
}

// ---- accessors + parallel-safety ------------------------------------------

#[test]
fn approval_accessors() {
    let tool = SearchTool::with_backend(Arc::new(StubBackend(String::new())));
    let req = SearchRequest::new("parallel");
    assert_eq!(tool.approval_keys(&req).len(), 1, "one key per call");
    assert_eq!(
        tool.sandbox_permissions(&req),
        SandboxPermissions::UseDefault
    );
    assert!(tool.exec_approval_requirement(&req).is_none());
}

#[test]
fn search_is_serial_by_default() {
    // A conservative scheduling default for a billed API call.
    let tool = SearchTool::with_backend(Arc::new(StubBackend(String::new())));
    let req = SearchRequest::new("parallel");
    assert_eq!(tool.parallel_safe(&req), SEARCH_PARALLEL_SAFE);
    assert!(!tool.parallel_safe(&req), "search must be serial");
}

#[test]
fn tool_name_is_search() {
    assert_eq!(SEARCH_TOOL_NAME, "search");
    let tool = SearchTool::with_backend(Arc::new(StubBackend(String::new())));
    assert_eq!(tool.name(), "search");
}

#[test]
fn request_round_trips_wire_shape() {
    let json = r#"{"query":"hello world"}"#;
    let req: SearchRequest = serde_json::from_str(json).unwrap();
    assert_eq!(req.query, "hello world");
    let out = serde_json::to_string(&req).unwrap();
    assert_eq!(out, json);
}

// ---- drive a call through the orchestrator over the seam -------------------

#[tokio::test]
async fn orchestrated_search_completes_under_none() {
    let orch = ToolOrchestrator::new(NoneSandboxProvider, AutoApprover);
    let tool = SearchTool::with_backend(Arc::new(StubBackend(FIXTURE.to_string())));

    let result = orch
        .run(
            &tool,
            &SearchRequest::new("parallel"),
            &ctx(),
            &turn_env(),
            AskForApproval::Never,
        )
        .await
        .expect("orchestration ok");

    assert_eq!(result.sandbox_used, SandboxType::None);
    assert_eq!(result.output.exit_code, 0);
    assert!(
        result.output.stdout.contains("Genpact and Parallel"),
        "got: {}",
        result.output.stdout
    );
}

// ---- live smoke (ignored: hits the real browser-use search API) -----------

/// End-to-end check against the REAL `search.browser-use.com` service via the
/// default [`HttpSearchBackend`]. Ignored by default (network, billing, and a
/// `BROWSER_USE_API_KEY` requirement). Run it manually with:
///
/// ```text
/// cargo test -p browser-use-agent --lib -- --ignored --nocapture search_live_smoke
/// ```
#[tokio::test]
#[ignore = "hits the live browser-use search API (requires BROWSER_USE_API_KEY, \
            or BROWSER_USE_SEARCH_URL pointing at an open dev instance)"]
async fn search_live_smoke() {
    let has_key = std::env::var("BROWSER_USE_API_KEY").is_ok_and(|key| !key.trim().is_empty());
    let has_url = std::env::var("BROWSER_USE_SEARCH_URL").is_ok_and(|url| !url.trim().is_empty());
    if !has_key && !has_url {
        eprintln!(
            "skipping live smoke: neither BROWSER_USE_API_KEY nor BROWSER_USE_SEARCH_URL is set"
        );
        return;
    }

    let tool = SearchTool::new();
    let launch = none_launch();
    let attempt = none_attempt(&launch);
    let out = tool
        .run(
            &SearchRequest::new("Parallel Web Systems latest announcements"),
            &attempt,
            &ctx(),
        )
        .await
        .expect("run ok");

    eprintln!(
        "exit_code={}\n--- stdout ---\n{}\n--- stderr ---\n{}",
        out.exit_code, out.stdout, out.stderr
    );
    // An auth/billing rejection is a legitimate live outcome (exit 1 + message);
    // only assert hard on the success shape so the test documents both paths.
    if out.exit_code == 0 {
        assert!(
            out.stdout.contains("Search results for") || out.stdout.contains("No results found"),
            "unexpected stdout: {}",
            out.stdout
        );
    } else {
        assert!(
            out.stderr.contains("Search failed:"),
            "unexpected stderr: {}",
            out.stderr
        );
    }
}
