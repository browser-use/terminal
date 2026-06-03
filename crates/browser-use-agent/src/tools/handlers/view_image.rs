//! `view_image` tool: reads a LOCAL image file and returns it as model-visible
//! image content (a `data:<mime>;base64,...` URL).
//!
//! This is the async re-implementation of codex's `view_image` over our merged
//! [`ToolRuntime`](crate::tools::runtime::ToolRuntime) seam. It implements the
//! full trait stack ([`Approvable`] + [`Sandboxable`] + [`ToolRuntime`]) so it
//! can be driven by the [`ToolOrchestrator`](crate::tools::orchestrator::ToolOrchestrator),
//! mirroring the shell tool's structure (`tools/handlers/shell.rs`).
//!
//! # INTENTIONAL DIVERGENCE FROM CODEX (sanctioned design choice)
//!
//! Codex's `view_image` is async and **parallel-safe**
//! (`supports_parallel_tool_calls -> true`, codex
//! `core/src/tools/handlers/view_image.rs:76-78`). OURS is deliberately
//! **synchronous/blocking** and **NOT parallel-safe**
//! ([`parallel_safe`](ToolRuntime::parallel_safe)` -> false`).
//!
//! Rationale: this terminal is browser-first, and image observation is part of
//! the browser-interaction ordering. Screenshots / images must be observed in
//! strict order *after* the prior work that produced them — running `view_image`
//! concurrently with (or reordered relative to) other tool calls would let the
//! model observe a stale or future frame. Forcing the tool onto the serial /
//! write-lock path (like `shell` and `apply_patch`, which return
//! `parallel_safe == false`) guarantees an image is read at exactly the point in
//! the call sequence the model requested it. Because the read is serial and the
//! payload is a local file, we do the read with **blocking [`std::fs::read`]**
//! (NOT `tokio::fs`): it is fast and intentionally serial, so there is no benefit
//! to yielding the runtime for it. The `run` method keeps the trait's async
//! signature, but its body performs a blocking std read.
//!
//! This mirrors the legacy serial enforcement: legacy
//! `browser-use-core/src/tools/mod.rs::tool_supports_parallel` excludes
//! `ViewImage`, and legacy `view_image`
//! (`browser-use-core/src/tools/files.rs:284-360`) does a blocking `fs::read`.
//!
//! # Parity grounding
//!
//! * **Request shape + content return** — codex `ViewImageArgs { path, .. }`
//!   and the `input_image` data-URL content
//!   (`core/src/tools/handlers/view_image.rs:53-58, 175-176`); legacy
//!   `view_image` returns `{"type":"input_image","image_url": data_url, ..}`
//!   (`files.rs:353-359`).
//! * **Inline size cap** — legacy `MAX_INLINE_LOCAL_IMAGE_BYTES = 20 * 1024 * 1024`
//!   (`browser-use-core/src/lib.rs`); oversize images are rejected
//!   (legacy `files.rs:319-326`).
//! * **Path resolution** — relative paths are resolved against the request/context
//!   cwd, absolute paths are honored as-is, and the existing prefix is
//!   symlink-resolved. This intentionally does NOT enforce a workspace boundary.
//!
//! # Parity caveats / TODOs
//!
//! * **resize-to-fit / `detail: original`** — codex/legacy run the bytes through
//!   `load_for_prompt_bytes` (resize to fit `<= 2048` for `high`, or `Original`
//!   detail when the model allows it), normalizing the encoding and dimensions
//!   before inlining (legacy `prompt_image.rs`, codex
//!   `view_image.rs:160-176`). This WP intentionally does a faithful
//!   read + base64 + mime + size-cap only.
//!   TODO(WP-T-view_image-resize): port the `load_for_prompt_bytes` resize
//!   pipeline + `detail: high|original` selection (`original_image_detail.rs`).
//! * **richer image content seam** — codex returns a structured
//!   `FunctionCallOutputContentItem::InputImage` / `TurnItem::ImageView`; the
//!   present crate's `ToolRuntime` `Out` seam exposes only [`ExecOutput`]
//!   (`stdout`/`stderr`/`exit_code`; used by `shell` and `apply_patch`), and the
//!   workspace `browser-use-llm` / `browser-use-protocol` crates do not yet
//!   expose an image content part for the agent-engine seam. We therefore return
//!   the data URL in [`ExecOutput::stdout`] (sanctioned fallback), prefixed so a
//!   later content-aware layer can recognize and re-wrap it as an `input_image`
//!   part. TODO(WP-T-view_image-content): return a structured image content part
//!   once the seam grows one.

use std::path::{Path, PathBuf};

use base64::Engine as _;

use crate::tools::runtime::{
    Approvable, ExecOutput, SandboxAttempt, Sandboxable, ToolCtx, ToolError, ToolRuntime,
};
use crate::tools::sandbox::{SandboxPermissions, SandboxPreference};

/// Maximum number of bytes an image may occupy to be inlined.
///
/// Parity: legacy `MAX_INLINE_LOCAL_IMAGE_BYTES = 20 * 1024 * 1024`
/// (`browser-use-core/src/lib.rs`). Images above this are rejected.
pub const MAX_INLINE_LOCAL_IMAGE_BYTES: usize = 20 * 1024 * 1024;

/// Prefix on the [`ExecOutput::stdout`] data URL so a later content-aware layer
/// can recognize the payload and re-wrap it as an `input_image` content part.
///
/// This is a property of our [`ExecOutput`] fallback seam, NOT a codex/legacy
/// wire constant (see the module-doc "richer image content seam" caveat).
pub const VIEW_IMAGE_STDOUT_PREFIX: &str = "view_image:";

/// Typed request for the `view_image` tool.
///
/// Field shape follows codex `ViewImageArgs { path, .. }`
/// (`core/src/tools/handlers/view_image.rs:53-58`): a path to a local image.
/// `cwd` overrides [`ToolCtx::cwd`] as the base used for relative paths, mirroring
/// [`super::shell::ShellRequest`] / [`super::apply_patch::ApplyPatchRequest`].
///
/// # Wire shape (model-facing args)
///
/// ```json
/// { "path": "screenshots/page.png" }
/// ```
///
/// Deserializes directly from the model's argument object. The `path` field name
/// matches codex's `ViewImageArgs { path }`
/// (`core/src/tools/handlers/view_image.rs:53-58`) and the legacy `view_image`
/// arg (`browser-use-core/src/tools/files.rs`). `cwd` is not part of the model's
/// wire args (codex resolves under the session cwd); it defaults to `None` so
/// deserialization of `{ "path": ... }` succeeds.
#[derive(Clone, Debug, PartialEq, Eq, serde::Deserialize)]
pub struct ViewImageRequest {
    /// Path to the local image file. Absolute paths are used as-is; relative paths
    /// are resolved against `cwd`.
    pub path: PathBuf,
    /// Base directory for relative paths. When `None`, the [`ToolCtx::cwd`] is
    /// used.
    #[serde(default)]
    pub cwd: Option<PathBuf>,
}

impl ViewImageRequest {
    /// Convenience constructor from a path, using the context cwd as the root.
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            cwd: None,
        }
    }
}

/// The decoded result of a successful `view_image` read: the detected MIME type
/// and the `data:<mime>;base64,...` URL of the (un-resized) image bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ViewImageContent {
    /// The detected MIME type (e.g. `image/png`).
    pub mime: String,
    /// The full `data:<mime>;base64,<...>` URL.
    pub data_url: String,
    /// The number of raw image bytes that were inlined (pre-base64).
    pub byte_len: usize,
}

impl ViewImageContent {
    /// Render the content as an [`ExecOutput`] (exit 0) so the tool's `Out` type
    /// matches the shell / apply_patch tools' [`ExecOutput`] seam. The data URL
    /// is placed in `stdout`, prefixed with [`VIEW_IMAGE_STDOUT_PREFIX`] so a
    /// later content-aware layer can re-wrap it as an `input_image` part (see the
    /// module-doc "richer image content seam" caveat).
    pub fn into_exec_output(self) -> ExecOutput {
        ExecOutput {
            exit_code: 0,
            stdout: format!("{VIEW_IMAGE_STDOUT_PREFIX}{}", self.data_url),
            stderr: String::new(),
        }
    }
}

/// The async `view_image` tool.
///
/// Stateless; cheap to clone/construct.
#[derive(Clone, Debug, Default)]
pub struct ViewImageTool;

impl ViewImageTool {
    /// Construct a new `view_image` tool.
    pub fn new() -> Self {
        Self
    }
}

/// Approval key: the path + workspace root identify a call for caching.
///
/// Same shape the shell tool uses for command + cwd (`shell.rs:222-226`) and the
/// apply_patch tool uses for patch + cwd (`apply_patch.rs:192-196`).
#[derive(serde::Serialize, Clone, Debug, Eq, PartialEq, Hash)]
pub struct ViewImageApprovalKey {
    path: PathBuf,
    cwd: Option<PathBuf>,
}

impl Approvable<ViewImageRequest> for ViewImageTool {
    type ApprovalKey = ViewImageApprovalKey;

    fn approval_keys(&self, req: &ViewImageRequest) -> Vec<Self::ApprovalKey> {
        vec![ViewImageApprovalKey {
            path: req.path.clone(),
            cwd: req.cwd.clone(),
        }]
    }

    /// `view_image` only reads a file; request the default sandbox permissions
    /// (no escalation), mirroring the shell / apply_patch tools
    /// (`shell.rs:242-244`, `apply_patch.rs:211-213`).
    fn sandbox_permissions(&self, _req: &ViewImageRequest) -> SandboxPermissions {
        SandboxPermissions::UseDefault
    }
}

impl Sandboxable for ViewImageTool {
    fn sandbox_preference(&self) -> SandboxPreference {
        // Let the provider decide (today everything resolves to
        // `SandboxType::None`). Matches the shell / apply_patch tools
        // (`shell.rs:261-267`, `apply_patch.rs:217-221`).
        SandboxPreference::Auto
    }

    fn escalate_on_failure(&self) -> bool {
        // Matches the shell / apply_patch tools (`shell.rs:269-273`,
        // `apply_patch.rs:223-227`): a sandbox denial may be retried unsandboxed.
        true
    }
}

#[async_trait::async_trait]
impl ToolRuntime<ViewImageRequest, ExecOutput> for ViewImageTool {
    fn parallel_safe(&self, _req: &ViewImageRequest) -> bool {
        // INTENTIONAL DIVERGENCE FROM CODEX. Codex's view_image is parallel-safe
        // (codex `view_image.rs:76-78`); ours is forced serial so an image is
        // observed in strict order after the work that produced it (browser-
        // interaction ordering — see the module doc). DO NOT flip this to `true`.
        false
    }

    async fn run(
        &self,
        req: &ViewImageRequest,
        attempt: &SandboxAttempt<'_>,
        ctx: &ToolCtx,
    ) -> Result<ExecOutput, ToolError> {
        // Today the only sandbox is `None`; a real backend lands later behind
        // `attempt.sandbox`. Acknowledge the attempt to make the seam explicit
        // (matches the shell / apply_patch tools).
        let _ = attempt;

        let root = req.cwd.clone().unwrap_or_else(|| ctx.cwd.clone());

        // Resolve the path. Absolute paths are honored; relative paths use the
        // request/context cwd as their base.
        let real = resolve_local_path(&root, &req.path);

        // Detect the MIME from the extension BEFORE reading, so an unsupported
        // extension is a clean rejection rather than a wasted read.
        let mime = mime_from_extension(&real).ok_or_else(|| {
            ToolError::Rejected(format!(
                "view_image: unsupported image extension for {} (expected png/jpeg/gif/webp)",
                req.path.display()
            ))
        })?;

        let meta = std::fs::metadata(&real).map_err(|e| {
            ToolError::Other(anyhow::anyhow!(
                "view_image: cannot stat {}: {e}",
                req.path.display()
            ))
        })?;
        if !meta.file_type().is_file() {
            return Err(ToolError::Rejected(format!(
                "view_image refuses to read non-regular file {}",
                req.path.display()
            )));
        }
        if meta.len() > MAX_INLINE_LOCAL_IMAGE_BYTES as u64 {
            return Err(ToolError::Rejected(format!(
                "view_image cannot inline {}: image is {} bytes, above the {} byte inline limit",
                req.path.display(),
                meta.len(),
                MAX_INLINE_LOCAL_IMAGE_BYTES
            )));
        }

        // Blocking, deliberately-serial read (see the module doc: this tool is
        // not parallel-safe, so a blocking std read is the right choice — no
        // benefit to yielding the async runtime for a fast local file). A
        // missing/unreadable file is a clean error (not a panic).
        let bytes = std::fs::read(&real).map_err(|e| {
            ToolError::Other(anyhow::anyhow!(
                "view_image: cannot read {}: {e}",
                req.path.display()
            ))
        })?;

        // Enforce the inline size cap (legacy `files.rs:319-326`). We cap on the
        // raw bytes; the resize pipeline that could shrink an oversize image is a
        // TODO (see the module doc).
        if bytes.len() > MAX_INLINE_LOCAL_IMAGE_BYTES {
            return Err(ToolError::Rejected(format!(
                "view_image cannot inline {}: image is {} bytes, above the {} byte inline limit",
                req.path.display(),
                bytes.len(),
                MAX_INLINE_LOCAL_IMAGE_BYTES
            )));
        }

        let content = encode_data_url(mime, &bytes);
        Ok(content.into_exec_output())
    }
}

// ---- MIME detection (codex/legacy: detect from extension) ----

/// Detect the image MIME type from a path's extension.
///
/// Supports the four formats the workspace `image` crate is built with (png,
/// jpeg, gif, webp; see workspace `Cargo.toml`). Returns `None` for anything
/// else so the caller can reject it cleanly. Matches the codex/legacy approach
/// of keying the MIME off the extension.
pub fn mime_from_extension(path: &Path) -> Option<&'static str> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    let mime = match ext.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        _ => return None,
    };
    Some(mime)
}

/// Base64-encode `bytes` into a `data:<mime>;base64,<...>` URL.
///
/// Parity: legacy `PromptImage::into_data_url` shape
/// (`data:<mime>;base64,<standard-base64>`), minus the resize normalization.
fn encode_data_url(mime: &str, bytes: &[u8]) -> ViewImageContent {
    let b64 = base64::engine::general_purpose::STANDARD.encode(bytes);
    ViewImageContent {
        mime: mime.to_string(),
        data_url: format!("data:{mime};base64,{b64}"),
        byte_len: bytes.len(),
    }
}

// ---- Path resolution ----

/// Resolve `path` for local image reads.
///
/// Absolute paths are used as-is. Relative paths are joined to `root`, normalized
/// lexically, and then the longest existing prefix is canonicalized so symlink
/// targets are followed. No workspace-boundary check is applied.
fn resolve_local_path(root: &Path, path: &Path) -> PathBuf {
    let candidate = if path.is_absolute() {
        path.to_path_buf()
    } else {
        root.join(path)
    };
    let normalized = normalize_lexically(&candidate);
    resolve_existing_prefix(&normalized)
}

/// Normalize a path lexically, resolving `.` and `..` without touching the FS.
///
/// Parity: `apply_patch.rs::normalize_lexically` / legacy `files.rs:1628`.
fn normalize_lexically(path: &Path) -> PathBuf {
    use std::path::Component;
    let mut out = PathBuf::new();
    for comp in path.components() {
        match comp {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Resolve symlinks for the longest existing prefix of `path`.
///
/// Parity: `apply_patch.rs::resolve_existing_prefix` / legacy `files.rs:1644`.
fn resolve_existing_prefix(path: &Path) -> PathBuf {
    let mut existing = path;
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    loop {
        if existing.exists() {
            break;
        }
        match (existing.parent(), existing.file_name()) {
            (Some(parent), Some(name)) => {
                tail.push(name.to_os_string());
                existing = parent;
            }
            _ => break,
        }
    }

    let base = existing
        .canonicalize()
        .unwrap_or_else(|_| existing.to_path_buf());
    let mut out = base;
    for name in tail.into_iter().rev() {
        out.push(name);
    }

    out
}
