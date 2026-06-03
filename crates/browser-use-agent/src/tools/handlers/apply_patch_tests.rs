//! Tests for the async `apply_patch` tool ([`ApplyPatchTool`]).
//!
//! All tests use a `tempfile` workspace (no network). They drive the tool
//! directly through the [`ToolRuntime::run`] seam (with a `SandboxType::None`
//! attempt) and, for one case, through the [`ToolOrchestrator`] to prove seam
//! integration (like `shell_tests`).

use super::apply_patch::{
    apply_patch_operations, parse_patch, ApplyPatchRequest, ApplyPatchTool, PatchOperation,
};
use crate::tools::approval::AskForApproval;
use crate::tools::orchestrator::{ToolOrchestrator, TurnEnv};
use crate::tools::runtime::{
    Approvable, AutoApprover, ExecOutput, SandboxAttempt, ToolCtx, ToolError, ToolRuntime,
};
use crate::tools::sandbox::{
    FileSystemSandboxPolicy, NoneSandboxProvider, SandboxLaunch, SandboxPermissions, SandboxType,
};

/// A `SandboxType::None` launch + attempt for direct `run` calls.
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

/// A minimal ctx rooted at a given cwd.
fn ctx_in(cwd: &std::path::Path) -> ToolCtx {
    ToolCtx {
        call_id: "test-call".to_string(),
        tool_name: "apply_patch".to_string(),
        cwd: cwd.to_path_buf(),
        artifact_root: cwd.join("artifacts"),
    }
}

/// Run an apply_patch request directly through the runtime (no orchestrator).
async fn run_direct(req: &ApplyPatchRequest, ctx: &ToolCtx) -> Result<ExecOutput, ToolError> {
    let tool = ApplyPatchTool::new();
    let launch = none_launch();
    let attempt = none_attempt(&launch);
    tool.run(req, &attempt, ctx).await
}

// (1) Add File creates a new file with the given contents.
#[tokio::test]
async fn add_file_creates_new_file() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = ctx_in(dir.path());
    let patch = "\
*** Begin Patch
*** Add File: hello.txt
+line one
+line two
*** End Patch
";
    let req = ApplyPatchRequest::new(patch);
    let out = run_direct(&req, &ctx).await.expect("add file should apply");
    assert_eq!(out.exit_code, 0);

    let written = std::fs::read_to_string(dir.path().join("hello.txt")).unwrap();
    assert_eq!(written, "line one\nline two");
    assert!(
        out.stdout.contains("hello.txt"),
        "summary should mention the changed file, got: {:?}",
        out.stdout
    );
}

// (1b) Add File can create nested directories.
#[tokio::test]
async fn add_file_creates_nested_dirs() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = ctx_in(dir.path());
    let patch = "\
*** Begin Patch
*** Add File: a/b/c.txt
+nested
*** End Patch
";
    let req = ApplyPatchRequest::new(patch);
    run_direct(&req, &ctx)
        .await
        .expect("nested add should apply");
    let written = std::fs::read_to_string(dir.path().join("a/b/c.txt")).unwrap();
    assert_eq!(written, "nested");
}

#[tokio::test]
async fn add_file_over_directory_is_rejected_before_write() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = ctx_in(dir.path());
    std::fs::create_dir(dir.path().join("existing.txt")).unwrap();

    let patch = "\
*** Begin Patch
*** Add File: existing.txt
+replacement
*** End Patch
";
    let req = ApplyPatchRequest::new(patch);
    match run_direct(&req, &ctx).await {
        Err(ToolError::Rejected(msg)) => {
            assert!(
                msg.contains("non-regular file"),
                "should reject directories before writing, got: {msg}"
            );
        }
        other => panic!("expected Rejected for directory add, got {other:?}"),
    }
}

// (2) Update File applies a hunk (context + +/- lines) correctly.
#[tokio::test]
async fn update_file_applies_hunk() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = ctx_in(dir.path());
    // Seed an existing file.
    std::fs::write(dir.path().join("code.txt"), "alpha\nbeta\ngamma\ndelta\n").unwrap();

    // Replace `beta` with `BETA`, keeping `alpha`/`gamma` as context anchors.
    let patch = "\
*** Begin Patch
*** Update File: code.txt
@@
 alpha
-beta
+BETA
 gamma
*** End Patch
";
    let req = ApplyPatchRequest::new(patch);
    let out = run_direct(&req, &ctx).await.expect("update should apply");
    assert_eq!(out.exit_code, 0);

    let result = std::fs::read_to_string(dir.path().join("code.txt")).unwrap();
    assert_eq!(
        result, "alpha\nBETA\ngamma\ndelta\n",
        "hunk should replace beta with BETA and preserve trailing newline"
    );
}

#[tokio::test]
async fn update_directory_is_rejected_before_read() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = ctx_in(dir.path());
    std::fs::create_dir(dir.path().join("not-a-file.txt")).unwrap();

    let patch = "\
*** Begin Patch
*** Update File: not-a-file.txt
@@
 anything
*** End Patch
";
    let req = ApplyPatchRequest::new(patch);
    match run_direct(&req, &ctx).await {
        Err(ToolError::Rejected(msg)) => {
            assert!(
                msg.contains("non-regular file"),
                "should reject directories before reading, got: {msg}"
            );
        }
        other => panic!("expected Rejected for directory update, got {other:?}"),
    }
}

// (2b) Update File with an *** End of File marker terminating the hunk.
#[tokio::test]
async fn update_file_with_end_of_file_marker() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = ctx_in(dir.path());
    std::fs::write(dir.path().join("eof.txt"), "one\ntwo\n").unwrap();

    let patch = "\
*** Begin Patch
*** Update File: eof.txt
@@
 one
-two
+TWO
*** End of File
*** End Patch
";
    let req = ApplyPatchRequest::new(patch);
    run_direct(&req, &ctx).await.expect("update should apply");
    let result = std::fs::read_to_string(dir.path().join("eof.txt")).unwrap();
    assert_eq!(result, "one\nTWO\n");
}

// (2c) Update File with *** Move to: relocates the file.
#[tokio::test]
async fn update_file_with_move_relocates() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = ctx_in(dir.path());
    std::fs::write(dir.path().join("old.txt"), "keep\nremove\n").unwrap();

    let patch = "\
*** Begin Patch
*** Update File: old.txt
*** Move to: new.txt
@@
 keep
-remove
+REMOVE
*** End Patch
";
    let req = ApplyPatchRequest::new(patch);
    run_direct(&req, &ctx)
        .await
        .expect("move-update should apply");

    assert!(
        !dir.path().join("old.txt").exists(),
        "original should be removed after move"
    );
    let moved = std::fs::read_to_string(dir.path().join("new.txt")).unwrap();
    assert_eq!(moved, "keep\nREMOVE\n");
}

// (3) Delete File removes the file.
#[tokio::test]
async fn delete_file_removes_file() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = ctx_in(dir.path());
    let target = dir.path().join("gone.txt");
    std::fs::write(&target, "doomed\n").unwrap();
    assert!(target.exists());

    let patch = "\
*** Begin Patch
*** Delete File: gone.txt
*** End Patch
";
    let req = ApplyPatchRequest::new(patch);
    let out = run_direct(&req, &ctx).await.expect("delete should apply");
    assert_eq!(out.exit_code, 0);
    assert!(!target.exists(), "file should be deleted");
}

// (4a) A patch targeting a `../escape` path writes outside the cwd.
#[tokio::test]
async fn relative_escape_path_is_allowed() {
    let outer = tempfile::tempdir().unwrap();
    let root = outer.path().join("root");
    std::fs::create_dir(&root).unwrap();
    let ctx = ctx_in(&root);
    let patch = "\
*** Begin Patch
*** Add File: ../escape.txt
+pwned
*** End Patch
";
    let req = ApplyPatchRequest::new(patch);
    run_direct(&req, &ctx)
        .await
        .expect("../escape patch should apply");
    assert_eq!(
        std::fs::read_to_string(outer.path().join("escape.txt")).unwrap(),
        "pwned"
    );
}

// (4b) A patch targeting an absolute path writes to that path.
#[tokio::test]
async fn absolute_path_is_allowed() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = ctx_in(dir.path());
    let target = dir.path().join("abs_escape.txt");
    let patch = format!(
        "\
*** Begin Patch
*** Add File: {}
+pwned
*** End Patch
",
        target.display()
    );
    let req = ApplyPatchRequest::new(patch);
    run_direct(&req, &ctx)
        .await
        .expect("absolute path patch should apply");
    assert_eq!(std::fs::read_to_string(target).unwrap(), "pwned");
}

// (4c) A deep `..` traversal that lands back inside the root is allowed.
#[tokio::test]
async fn traversal_back_into_root_is_allowed() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = ctx_in(dir.path());
    std::fs::create_dir(dir.path().join("sub")).unwrap();
    let patch = "\
*** Begin Patch
*** Add File: sub/../inside.txt
+ok
*** End Patch
";
    let req = ApplyPatchRequest::new(patch);
    run_direct(&req, &ctx)
        .await
        .expect("traversal that stays under root should be allowed");
    assert_eq!(
        std::fs::read_to_string(dir.path().join("inside.txt")).unwrap(),
        "ok"
    );
}

// (5) A malformed envelope returns an error (not a panic).
#[tokio::test]
async fn malformed_envelope_errors() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = ctx_in(dir.path());

    // Missing the *** Begin Patch marker.
    let req = ApplyPatchRequest::new("not a patch at all\njust text\n");
    match run_direct(&req, &ctx).await {
        Err(ToolError::Rejected(msg)) => {
            assert!(
                msg.contains("parse error"),
                "should be a parse-error rejection, got: {msg}"
            );
        }
        other => panic!("expected Rejected for malformed envelope, got {other:?}"),
    }
}

// (5b) An envelope missing the *** End Patch marker errors (not a panic).
#[tokio::test]
async fn missing_end_marker_errors() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = ctx_in(dir.path());
    let patch = "\
*** Begin Patch
*** Add File: x.txt
+content
";
    let req = ApplyPatchRequest::new(patch);
    assert!(
        matches!(run_direct(&req, &ctx).await, Err(ToolError::Rejected(_))),
        "missing end marker must be a rejection, not a panic"
    );
}

// (5c) A hunk whose context cannot be located errors (mapped to Other).
#[tokio::test]
async fn unlocatable_hunk_errors() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = ctx_in(dir.path());
    std::fs::write(dir.path().join("f.txt"), "real\ncontent\n").unwrap();
    let patch = "\
*** Begin Patch
*** Update File: f.txt
@@
 nonexistent
-line
+replacement
*** End Patch
";
    let req = ApplyPatchRequest::new(patch);
    match run_direct(&req, &ctx).await {
        Err(ToolError::Other(e)) => {
            assert!(
                e.to_string().contains("locate hunk context"),
                "should report it could not locate the hunk, got: {e}"
            );
        }
        other => panic!("expected Other for unlocatable hunk, got {other:?}"),
    }
    // The file must be unchanged.
    assert_eq!(
        std::fs::read_to_string(dir.path().join("f.txt")).unwrap(),
        "real\ncontent\n"
    );
}

// Pure parser: a well-formed multi-op patch parses into the expected operations.
#[test]
fn parse_patch_yields_operations() {
    let patch = "\
*** Begin Patch
*** Add File: a.txt
+hi
*** Delete File: b.txt
*** End Patch
";
    let ops = parse_patch(patch).expect("should parse");
    assert_eq!(ops.len(), 2);
    assert!(matches!(
        &ops[0],
        PatchOperation::AddFile { path, contents }
            if path == "a.txt" && contents == "hi"
    ));
    assert!(matches!(&ops[1], PatchOperation::DeleteFile { path } if path == "b.txt"));
}

// Pure apply: applying ops under a tempdir reports the changed paths.
#[test]
fn apply_operations_reports_changed_paths() {
    let dir = tempfile::tempdir().unwrap();
    let ops = vec![PatchOperation::AddFile {
        path: "made.txt".to_string(),
        contents: "x".to_string(),
    }];
    let summary = apply_patch_operations(&ops, dir.path()).expect("apply ok");
    assert_eq!(summary.changed, vec!["made.txt".to_string()]);
    assert_eq!(
        std::fs::read_to_string(dir.path().join("made.txt")).unwrap(),
        "x"
    );
}

// The tool is not parallel-safe (mutates the filesystem).
#[test]
fn apply_patch_is_not_parallel_safe() {
    let tool = ApplyPatchTool::new();
    let req = ApplyPatchRequest::new("*** Begin Patch\n*** End Patch\n");
    assert!(!tool.parallel_safe(&req));
}

// Approval/sandbox accessors: one key per call, default sandbox permissions.
#[test]
fn approval_accessors() {
    let tool = ApplyPatchTool::new();
    let req = ApplyPatchRequest::new("*** Begin Patch\n*** End Patch\n");
    assert_eq!(
        tool.approval_keys(&req).len(),
        1,
        "one approval key per call"
    );
    assert_eq!(
        tool.sandbox_permissions(&req),
        SandboxPermissions::UseDefault
    );
    // A benign call defers to the policy default (no tool-intrinsic requirement).
    assert!(tool.exec_approval_requirement(&req).is_none());
}

// The request cwd overrides the ctx cwd as the base for relative paths.
#[tokio::test]
async fn request_cwd_overrides_ctx_cwd() {
    let ctx_dir = tempfile::tempdir().unwrap();
    let req_dir = tempfile::tempdir().unwrap();
    let ctx = ctx_in(ctx_dir.path());

    let mut req = ApplyPatchRequest::new(
        "\
*** Begin Patch
*** Add File: r.txt
+routed
*** End Patch
",
    );
    req.cwd = Some(req_dir.path().to_path_buf());

    run_direct(&req, &ctx)
        .await
        .expect("apply should route to req cwd");
    assert!(
        req_dir.path().join("r.txt").exists(),
        "file should land under the request cwd, not the ctx cwd"
    );
    assert!(
        !ctx_dir.path().join("r.txt").exists(),
        "file should NOT land under the ctx cwd"
    );
}

// --- Orchestrator integration: drive the apply_patch tool through the full seam. ---

fn turn_env(restricted: bool) -> TurnEnv {
    TurnEnv {
        file_system_sandbox_policy: FileSystemSandboxPolicy {
            restricted,
            denied_read: false,
        },
        managed_network_active: false,
        strict_auto_review: false,
        use_guardian: false,
    }
}

#[tokio::test]
async fn orchestrated_add_file_completes_under_none() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = ctx_in(dir.path());
    // `Never` => no approval prompt for a benign apply.
    let orch = ToolOrchestrator::new(NoneSandboxProvider, AutoApprover);
    let tool = ApplyPatchTool::new();
    let req = ApplyPatchRequest::new(
        "\
*** Begin Patch
*** Add File: orchestrated.txt
+via orchestrator
*** End Patch
",
    );

    let result = orch
        .run(&tool, &req, &ctx, &turn_env(false), AskForApproval::Never)
        .await
        .expect("orchestration ok");

    assert_eq!(result.sandbox_used, SandboxType::None);
    assert_eq!(result.output.exit_code, 0);
    assert_eq!(
        std::fs::read_to_string(dir.path().join("orchestrated.txt")).unwrap(),
        "via orchestrator"
    );
}

#[tokio::test]
async fn orchestrated_escape_is_allowed() {
    let outer = tempfile::tempdir().unwrap();
    let root = outer.path().join("root");
    std::fs::create_dir(&root).unwrap();
    let ctx = ctx_in(&root);
    let orch = ToolOrchestrator::new(NoneSandboxProvider, AutoApprover);
    let tool = ApplyPatchTool::new();
    let req = ApplyPatchRequest::new(
        "\
*** Begin Patch
*** Add File: ../orch_escape.txt
+nope
*** End Patch
",
    );

    let result = orch
        .run(&tool, &req, &ctx, &turn_env(false), AskForApproval::Never)
        .await
        .expect("escaping patch should complete through the orchestrator");
    assert_eq!(result.output.exit_code, 0);
    assert_eq!(
        std::fs::read_to_string(outer.path().join("orch_escape.txt")).unwrap(),
        "nope"
    );
}
