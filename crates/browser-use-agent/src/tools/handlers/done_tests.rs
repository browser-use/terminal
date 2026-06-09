//! Tests for the async `done` (completion) tool ([`DoneTool`]).
//!
//! No network, no filesystem, no stdin: the tool is a pure accept-and-return
//! completion signal. Structure mirrors `update_plan_tests.rs` (the closest
//! analog) — direct `run` calls plus one drive through the orchestrator over the
//! seam.

use super::done::{audit_done_request, DoneRequest, DoneTool, DONE_STDOUT_PREFIX};
use crate::tools::approval::AskForApproval;
use crate::tools::orchestrator::{ToolOrchestrator, TurnEnv};
use crate::tools::runtime::{AutoApprover, SandboxAttempt, ToolCtx, ToolRuntime};
use crate::tools::sandbox::{
    FileSystemSandboxPolicy, NoneSandboxProvider, SandboxLaunch, SandboxPermissions, SandboxType,
};

/// A `SandboxType::None` launch + attempt for direct `run` calls (mirrors
/// `update_plan_tests::none_launch` / `none_attempt`).
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
        tool_name: "done".to_string(),
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

// ---- (1) a done call with a summary records it into the acknowledgement ----

#[tokio::test]
async fn done_with_text_records_the_summary() {
    let tool = DoneTool::new();
    let launch = none_launch();
    let attempt = none_attempt(&launch);

    let req = DoneRequest::with_text("All tests pass; shipped.");
    let out = tool.run(&req, &attempt, &ctx()).await.unwrap();

    assert_eq!(out.exit_code, 0);
    assert!(out.stderr.is_empty());
    assert!(out.stdout.starts_with(DONE_STDOUT_PREFIX));
    let summary = out.stdout.strip_prefix(DONE_STDOUT_PREFIX).unwrap();
    assert_eq!(summary, "All tests pass; shipped.");
}

// ---- (2) a done call WITHOUT text still succeeds (empty summary) ----

#[tokio::test]
async fn done_without_text_yields_empty_summary() {
    let tool = DoneTool::new();
    let launch = none_launch();
    let attempt = none_attempt(&launch);

    let out = tool
        .run(&DoneRequest::default(), &attempt, &ctx())
        .await
        .unwrap();
    assert_eq!(out.exit_code, 0);
    assert_eq!(out.stdout, DONE_STDOUT_PREFIX);
}

// ---- (3) the wire args deserialize from Browser Use-style and legacy payloads ----

#[test]
fn done_wire_args_round_trip() {
    // Browser Use-style final result form.
    let req: DoneRequest = serde_json::from_value(serde_json::json!({ "result": "done now" }))
        .expect("done deserialize");
    assert_eq!(req.result.as_deref(), Some("done now"));
    assert_eq!(req.summary(), "done now");

    // Legacy `text` remains accepted.
    let legacy: DoneRequest = serde_json::from_value(serde_json::json!({ "text": "legacy done" }))
        .expect("legacy done deserialize");
    assert_eq!(legacy.text.as_deref(), Some("legacy done"));
    assert_eq!(legacy.summary(), "legacy done");

    // `result` wins if both canonical and legacy fields are present.
    let both: DoneRequest =
        serde_json::from_value(serde_json::json!({ "result": "canonical", "text": "legacy" }))
            .expect("combined done deserialize");
    assert_eq!(both.summary(), "canonical");

    // File-only completion still produces a visible host summary.
    let file_only: DoneRequest =
        serde_json::from_value(serde_json::json!({ "result_file": "outputs/answer.json" }))
            .expect("file done deserialize");
    assert_eq!(file_only.summary(), "Result file: outputs/answer.json");

    // Minimal: fields omitted -> None (the model may declare done with no message).
    let empty: DoneRequest =
        serde_json::from_value(serde_json::json!({})).expect("empty done deserialize");
    assert_eq!(empty.result, None);
    assert_eq!(empty.text, None);
    assert_eq!(empty.result_file, None);
    assert_eq!(empty.summary(), "");

    // Empty fields are skipped on serialize.
    let json = serde_json::to_value(&DoneRequest::default()).unwrap();
    assert!(
        json.get("result").is_none(),
        "None result is skipped on serialize"
    );
    assert!(
        json.get("text").is_none(),
        "None text is skipped on serialize"
    );
    assert!(
        json.get("result_file").is_none(),
        "None result_file is skipped on serialize"
    );
}

// ---- (4) drive one call through the orchestrator over the seam ----

#[tokio::test]
async fn orchestrated_done_completes_under_none() {
    let orch = ToolOrchestrator::new(NoneSandboxProvider, AutoApprover);
    let tool = DoneTool::new();

    let result = orch
        .run(
            &tool,
            &DoneRequest::with_text("finished"),
            &ctx(),
            &turn_env(),
            AskForApproval::Never,
        )
        .await
        .expect("orchestration ok");

    assert_eq!(result.sandbox_used, SandboxType::None);
    assert_eq!(result.output.exit_code, 0);
    assert_eq!(
        result.output.stdout,
        format!("{DONE_STDOUT_PREFIX}finished")
    );
}

// ---- (5) parallel-safety: done is serial (terminal, never reordered) ----

#[test]
fn done_is_not_parallel_safe() {
    let tool = DoneTool::new();
    assert!(
        !tool.parallel_safe(&DoneRequest::default()),
        "done must be serial: completion is terminal and must not be reordered"
    );
}

// ---- (6) eval-mode completion audit catches weak finals before terminal stop ----

#[test]
fn eval_done_audit_rejects_explicit_partial_completion() {
    let err = audit_done_request(
        &DoneRequest::with_result("partial_incomplete: two fields were not checked"),
        &ctx(),
    )
    .expect_err("partial completion should be rejected");

    assert!(err.contains("partial_incomplete"), "got: {err}");
    assert!(err.contains("Continue working"), "got: {err}");
}

#[test]
fn eval_done_audit_rejects_missing_result_file() {
    let req = DoneRequest {
        result_file: Some("missing-result.json".to_string()),
        ..DoneRequest::default()
    };

    let err = audit_done_request(&req, &ctx()).expect_err("missing result file should reject");
    assert!(err.contains("result_file"), "got: {err}");
    assert!(err.contains("not readable"), "got: {err}");
}

#[test]
fn eval_done_audit_rejects_placeholder_heavy_json() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("answer.json");
    std::fs::write(
        &path,
        r#"{
            "items": [
                {"name": "unknown", "price": "not found", "url": ""},
                {"name": "missing", "price": "unavailable", "url": "not checked"},
                {"name": "ok", "price": "12", "url": "https://example.test"}
            ]
        }"#,
    )
    .unwrap();
    let req = DoneRequest {
        result_file: Some(path.to_string_lossy().to_string()),
        ..DoneRequest::default()
    };

    let err = audit_done_request(&req, &ctx()).expect_err("placeholder JSON should reject");
    assert!(err.contains("placeholder"), "got: {err}");
}

#[test]
fn eval_done_audit_accepts_non_empty_material_file() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("answer.json");
    std::fs::write(
        &path,
        r#"{"items":[{"name":"A","price":"12","url":"https://example.test/a"},{"name":"B","price":"14","url":"https://example.test/b"}]}"#,
    )
    .unwrap();
    let req = DoneRequest {
        result_file: Some(path.to_string_lossy().to_string()),
        ..DoneRequest::default()
    };

    audit_done_request(&req, &ctx()).expect("material JSON should pass");
}
