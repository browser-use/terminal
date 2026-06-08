//! Tests for the async `read_url` tool ([`ReadUrlTool`]).
//!
//! No real network is touched: the run path is driven through a fake
//! [`ReadUrlBackend`].

use std::sync::Arc;
use std::time::Duration;

use super::read_url::{
    ReadUrlBackend, ReadUrlError, ReadUrlPage, ReadUrlRequest, ReadUrlTool, READ_URL_PARALLEL_SAFE,
    READ_URL_TOOL_NAME,
};
use crate::tools::runtime::{SandboxAttempt, ToolCtx, ToolError, ToolRuntime};
use crate::tools::sandbox::{SandboxLaunch, SandboxPermissions, SandboxType};

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
        tool_name: "read_url".to_string(),
        cwd: std::env::temp_dir(),
        artifact_root: std::env::temp_dir().join("artifacts"),
    }
}

struct StubReadUrlBackend;

#[async_trait::async_trait]
impl ReadUrlBackend for StubReadUrlBackend {
    async fn fetch(&self, url: &str, _timeout: Duration) -> Result<ReadUrlPage, ReadUrlError> {
        if url.contains("fail") {
            return Err(ReadUrlError::Request("network down".to_string()));
        }
        let body = if url.ends_with("/html") {
            br#"<!doctype html>
<html>
  <head><title>Example &amp; Source</title><style>.hidden{}</style></head>
  <body>
    <h1>Official price list</h1>
    <script>secret()</script>
    <p>Plan A costs $10.</p>
    <a href="/details">Details page</a>
  </body>
</html>"#
                .to_vec()
        } else {
            br#"{"status":"ok","items":[1,2]}"#.to_vec()
        };
        Ok(ReadUrlPage {
            status: 200,
            final_url: url.to_string(),
            content_type: Some(if url.ends_with("/html") {
                "text/html; charset=utf-8".to_string()
            } else {
                "application/json".to_string()
            }),
            body,
            truncated: false,
        })
    }
}

#[tokio::test]
async fn read_url_fetches_multiple_pages_and_extracts_text_links() {
    let launch = none_launch();
    let attempt = none_attempt(&launch);
    let tool = ReadUrlTool::with_backend(Arc::new(StubReadUrlBackend));
    let out = tool
        .run(
            &ReadUrlRequest {
                url: None,
                urls: vec![
                    "https://example.com/html".to_string(),
                    "https://api.example.com/data.json".to_string(),
                ],
                max_chars_per_url: Some(500),
                timeout_ms: Some(1_000),
            },
            &attempt,
            &ctx(),
        )
        .await
        .expect("read_url should run");

    assert_eq!(out.exit_code, 0);
    assert!(
        out.stdout.contains("read_url fetched 2 URL(s)"),
        "{}",
        out.stdout
    );
    assert!(
        out.stdout.contains("Title: Example & Source"),
        "{}",
        out.stdout
    );
    assert!(out.stdout.contains("Official price list"), "{}", out.stdout);
    assert!(out.stdout.contains("Plan A costs $10."), "{}", out.stdout);
    assert!(
        out.stdout
            .contains("- Details page -> https://example.com/details"),
        "{}",
        out.stdout
    );
    assert!(out.stdout.contains(r#"{"status":"ok""#), "{}", out.stdout);
}

#[tokio::test]
async fn read_url_reports_per_url_failures_without_hard_failing_mixed_batch() {
    let launch = none_launch();
    let attempt = none_attempt(&launch);
    let tool = ReadUrlTool::with_backend(Arc::new(StubReadUrlBackend));
    let out = tool
        .run(
            &ReadUrlRequest {
                url: None,
                urls: vec![
                    "https://example.com/html".to_string(),
                    "https://example.com/fail".to_string(),
                ],
                max_chars_per_url: None,
                timeout_ms: None,
            },
            &attempt,
            &ctx(),
        )
        .await
        .expect("mixed batch should be model-visible output");

    assert_eq!(out.exit_code, 0);
    assert!(out.stdout.contains("network down"), "{}", out.stdout);
    assert!(out.stderr.is_empty(), "{:?}", out.stderr);
}

#[tokio::test]
async fn read_url_rejects_missing_and_non_http_urls() {
    let launch = none_launch();
    let attempt = none_attempt(&launch);
    let tool = ReadUrlTool::with_backend(Arc::new(StubReadUrlBackend));

    let err = tool
        .run(&ReadUrlRequest::default(), &attempt, &ctx())
        .await
        .unwrap_err();
    assert!(matches!(err, ToolError::Rejected(_)));

    let err = tool
        .run(
            &ReadUrlRequest {
                url: Some("file:///tmp/nope".to_string()),
                ..Default::default()
            },
            &attempt,
            &ctx(),
        )
        .await
        .unwrap_err();
    assert!(matches!(err, ToolError::Rejected(_)));
}

#[test]
fn read_url_constants_match_tool_contract() {
    assert_eq!(READ_URL_TOOL_NAME, "read_url");
    assert!(READ_URL_PARALLEL_SAFE);
}
