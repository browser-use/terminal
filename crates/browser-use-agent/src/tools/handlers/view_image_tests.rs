//! Tests for the async `view_image` tool ([`ViewImageTool`]).
//!
//! All tests use a `tempfile` workspace (no network). They drive the tool
//! directly through the [`ToolRuntime::run`] seam (with a `SandboxType::None`
//! attempt) and, for one case, through the [`ToolOrchestrator`] to prove seam
//! integration (like `shell_tests` / `apply_patch_tests`).

use base64::Engine as _;

use super::view_image::{
    mime_from_extension, ViewImageRequest, ViewImageTool, MAX_INLINE_LOCAL_IMAGE_BYTES,
    VIEW_IMAGE_STDOUT_PREFIX,
};
use crate::tools::approval::AskForApproval;
use crate::tools::orchestrator::{ToolOrchestrator, TurnEnv};
use crate::tools::runtime::{
    Approvable, AutoApprover, ExecOutput, SandboxAttempt, ToolCtx, ToolError, ToolRuntime,
};
use crate::tools::sandbox::{
    FileSystemSandboxPolicy, NoneSandboxProvider, SandboxLaunch, SandboxPermissions, SandboxType,
};

/// A minimal but valid 1x1 PNG (the canonical 67-byte transparent pixel). We
/// only need *real* image bytes the encoder can read; the tool itself does not
/// decode them (that is the resize-pipeline TODO), so a valid PNG header is
/// sufficient for an honest round-trip test.
const TINY_PNG: &[u8] = &[
    0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, // PNG signature
    0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44, 0x52, // IHDR chunk len + type
    0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, // width=1 height=1
    0x08, 0x06, 0x00, 0x00, 0x00, 0x1F, 0x15, 0xC4, // bit depth/color/...
    0x89, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x44, 0x41, // IDAT chunk len + type
    0x54, 0x78, 0x9C, 0x62, 0x00, 0x01, 0x00, 0x00, // zlib data
    0x05, 0x00, 0x01, 0x0D, 0x0A, 0x2D, 0xB4, 0x00, // ...
    0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE, // IEND chunk len + type
    0x42, 0x60, 0x82, // CRC
];

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
        tool_name: "view_image".to_string(),
        cwd: cwd.to_path_buf(),
        artifact_root: cwd.join("artifacts"),
    }
}

/// Run a view_image request directly through the runtime (no orchestrator).
async fn run_direct(req: &ViewImageRequest, ctx: &ToolCtx) -> Result<ExecOutput, ToolError> {
    let tool = ViewImageTool::new();
    let launch = none_launch();
    let attempt = none_attempt(&launch);
    tool.run(req, &attempt, ctx).await
}

/// Pull the bare data URL out of an [`ExecOutput`] produced by the tool.
fn data_url(out: &ExecOutput) -> &str {
    out.stdout
        .strip_prefix(VIEW_IMAGE_STDOUT_PREFIX)
        .expect("stdout should carry the view_image data-url prefix")
}

// (1) Reading a small valid PNG returns image content with the right mime/data.
#[tokio::test]
async fn reads_small_png_returns_image_content() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = ctx_in(dir.path());
    std::fs::write(dir.path().join("pixel.png"), TINY_PNG).unwrap();

    let req = ViewImageRequest::new("pixel.png");
    let out = run_direct(&req, &ctx).await.expect("png should be inlined");
    assert_eq!(out.exit_code, 0);
    assert!(out.stderr.is_empty());

    let url = data_url(&out);
    assert!(
        url.starts_with("data:image/png;base64,"),
        "data url should carry the png mime, got: {url}"
    );

    // The base64 payload must decode back to exactly the original bytes.
    let b64 = url
        .strip_prefix("data:image/png;base64,")
        .expect("data url prefix");
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(b64)
        .expect("payload should be valid base64");
    assert_eq!(
        decoded, TINY_PNG,
        "round-trip must preserve the image bytes"
    );
}

// (1b) A .jpg extension yields the image/jpeg mime.
#[tokio::test]
async fn jpeg_extension_yields_jpeg_mime() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = ctx_in(dir.path());
    // The tool keys mime off the extension and does not decode, so arbitrary
    // bytes under a .jpg name exercise the jpeg branch faithfully.
    std::fs::write(dir.path().join("photo.jpg"), b"\xFF\xD8\xFF\xE0jpegish").unwrap();

    let req = ViewImageRequest::new("photo.jpg");
    let out = run_direct(&req, &ctx).await.expect("jpg should be inlined");
    assert!(
        data_url(&out).starts_with("data:image/jpeg;base64,"),
        "got: {}",
        out.stdout
    );
}

// (2) Paths outside cwd are allowed: both ../escape and absolute.
#[tokio::test]
async fn relative_escape_path_is_allowed() {
    let outer = tempfile::tempdir().unwrap();
    let root = outer.path().join("root");
    std::fs::create_dir(&root).unwrap();
    let ctx = ctx_in(&root);
    std::fs::write(outer.path().join("outside.png"), TINY_PNG).unwrap();

    let req = ViewImageRequest::new("../outside.png");
    let out = run_direct(&req, &ctx)
        .await
        .expect("../escape image should be readable");
    assert!(data_url(&out).starts_with("data:image/png;base64,"));
}

#[tokio::test]
async fn absolute_path_is_allowed() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = ctx_in(dir.path());
    let image = dir.path().join("absolute.png");
    std::fs::write(&image, TINY_PNG).unwrap();

    let req = ViewImageRequest::new(image);
    let out = run_direct(&req, &ctx)
        .await
        .expect("absolute image path should be readable");
    assert!(data_url(&out).starts_with("data:image/png;base64,"));
}

// (3) A nonexistent file returns an error (not a panic).
#[tokio::test]
async fn nonexistent_file_errors() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = ctx_in(dir.path());

    let req = ViewImageRequest::new("does_not_exist.png");
    match run_direct(&req, &ctx).await {
        Err(ToolError::Other(e)) => {
            assert!(
                e.to_string().contains("cannot stat"),
                "should report it could not stat the file, got: {e}"
            );
        }
        other => panic!("expected Other for nonexistent file, got {other:?}"),
    }
}

#[tokio::test]
async fn directory_with_image_extension_is_rejected_before_read() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = ctx_in(dir.path());
    std::fs::create_dir(dir.path().join("not-a-file.png")).unwrap();

    let req = ViewImageRequest::new("not-a-file.png");
    match run_direct(&req, &ctx).await {
        Err(ToolError::Rejected(msg)) => {
            assert!(
                msg.contains("non-regular file"),
                "should reject directories before reading, got: {msg}"
            );
        }
        other => panic!("expected Rejected for directory, got {other:?}"),
    }
}

// (3b) An unsupported extension is rejected cleanly (not a panic).
#[tokio::test]
async fn unsupported_extension_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = ctx_in(dir.path());
    std::fs::write(dir.path().join("notes.txt"), b"hello").unwrap();

    let req = ViewImageRequest::new("notes.txt");
    match run_direct(&req, &ctx).await {
        Err(ToolError::Rejected(msg)) => {
            assert!(
                msg.contains("unsupported image extension"),
                "should reject unsupported extension, got: {msg}"
            );
        }
        other => panic!("expected Rejected for unsupported extension, got {other:?}"),
    }
}

// (4) An oversize file is rejected. We can't write 20 MiB cheaply in every CI,
// so we assert the cap value is the legacy one and that a file just over a small
// synthetic boundary trips the same code path by exceeding the real cap. To keep
// the test fast AND faithful, we verify the boundary arithmetic directly and
// exercise the real cap with a file that is provably over it only when feasible.
#[tokio::test]
async fn oversize_file_is_rejected() {
    // The cap matches the legacy constant.
    assert_eq!(MAX_INLINE_LOCAL_IMAGE_BYTES, 20 * 1024 * 1024);

    let dir = tempfile::tempdir().unwrap();
    let ctx = ctx_in(dir.path());

    // Write a file one byte over the cap. (~20 MiB; sparse-friendly write.)
    let path = dir.path().join("huge.png");
    let oversize = vec![0u8; MAX_INLINE_LOCAL_IMAGE_BYTES + 1];
    std::fs::write(&path, &oversize).unwrap();

    let req = ViewImageRequest::new("huge.png");
    match run_direct(&req, &ctx).await {
        Err(ToolError::Rejected(msg)) => {
            assert!(
                msg.contains("above the") && msg.contains("inline limit"),
                "should reject oversize image with the cap message, got: {msg}"
            );
        }
        other => panic!("expected Rejected for oversize image, got {other:?}"),
    }
}

// (5) `parallel_safe` returns FALSE. This is the INTENTIONAL DIVERGENCE from
// codex (whose view_image is parallel-safe): ours is forced serial so an image
// is observed in strict order after the work that produced it (browser-
// interaction ordering). This is a sanctioned design choice — see the module
// doc. DO NOT change this to expect `true`.
#[test]
fn view_image_is_not_parallel_safe_intentional_divergence() {
    let tool = ViewImageTool::new();
    let req = ViewImageRequest::new("pixel.png");
    assert!(
        !tool.parallel_safe(&req),
        "view_image MUST be serial (intentional divergence from codex's parallel-safe view_image)"
    );
}

// Pure mime detection: extensions map to the four supported mimes (case-insensitive).
#[test]
fn mime_detection_covers_supported_formats() {
    assert_eq!(
        mime_from_extension(std::path::Path::new("a.png")),
        Some("image/png")
    );
    assert_eq!(
        mime_from_extension(std::path::Path::new("a.JPG")),
        Some("image/jpeg")
    );
    assert_eq!(
        mime_from_extension(std::path::Path::new("a.jpeg")),
        Some("image/jpeg")
    );
    assert_eq!(
        mime_from_extension(std::path::Path::new("a.gif")),
        Some("image/gif")
    );
    assert_eq!(
        mime_from_extension(std::path::Path::new("a.webp")),
        Some("image/webp")
    );
    assert_eq!(mime_from_extension(std::path::Path::new("a.txt")), None);
    assert_eq!(mime_from_extension(std::path::Path::new("noext")), None);
}

// Approval/sandbox accessors: one key per call, default sandbox permissions.
#[test]
fn approval_accessors() {
    let tool = ViewImageTool::new();
    let req = ViewImageRequest::new("pixel.png");
    assert_eq!(
        tool.approval_keys(&req).len(),
        1,
        "one approval key per call"
    );
    assert_eq!(
        tool.sandbox_permissions(&req),
        SandboxPermissions::UseDefault
    );
    // A read defers to the policy default (no tool-intrinsic requirement).
    assert!(tool.exec_approval_requirement(&req).is_none());
}

// The request cwd overrides the ctx cwd as the base for relative paths.
#[tokio::test]
async fn request_cwd_overrides_ctx_cwd() {
    let ctx_dir = tempfile::tempdir().unwrap();
    let req_dir = tempfile::tempdir().unwrap();
    let ctx = ctx_in(ctx_dir.path());
    std::fs::write(req_dir.path().join("routed.png"), TINY_PNG).unwrap();

    let mut req = ViewImageRequest::new("routed.png");
    req.cwd = Some(req_dir.path().to_path_buf());

    let out = run_direct(&req, &ctx)
        .await
        .expect("view should route to req cwd");
    assert!(data_url(&out).starts_with("data:image/png;base64,"));
}

// --- Orchestrator integration: drive the view_image tool through the full seam. ---

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
async fn orchestrated_view_completes_under_none() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = ctx_in(dir.path());
    std::fs::write(dir.path().join("orchestrated.png"), TINY_PNG).unwrap();

    // `Never` => no approval prompt for a benign read.
    let orch = ToolOrchestrator::new(NoneSandboxProvider, AutoApprover);
    let tool = ViewImageTool::new();
    let req = ViewImageRequest::new("orchestrated.png");

    let result = orch
        .run(&tool, &req, &ctx, &turn_env(false), AskForApproval::Never)
        .await
        .expect("orchestration ok");

    assert_eq!(result.sandbox_used, SandboxType::None);
    assert_eq!(result.output.exit_code, 0);
    assert!(
        result.output.stdout.starts_with(VIEW_IMAGE_STDOUT_PREFIX),
        "orchestrated output should carry the image data url, got: {}",
        result.output.stdout
    );
}

#[tokio::test]
async fn orchestrated_escape_is_allowed() {
    let outer = tempfile::tempdir().unwrap();
    let root = outer.path().join("root");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(outer.path().join("orch_escape.png"), TINY_PNG).unwrap();
    let ctx = ctx_in(&root);
    let orch = ToolOrchestrator::new(NoneSandboxProvider, AutoApprover);
    let tool = ViewImageTool::new();
    let req = ViewImageRequest::new("../orch_escape.png");

    let result = orch
        .run(&tool, &req, &ctx, &turn_env(false), AskForApproval::Never)
        .await
        .expect("escaping read should complete through the orchestrator");
    assert_eq!(result.output.exit_code, 0);
    assert!(result.output.stdout.starts_with(VIEW_IMAGE_STDOUT_PREFIX));
}
