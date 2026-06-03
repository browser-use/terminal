//! `apply_patch` tool: parses a V4A patch envelope and applies it to the
//! filesystem under a workspace root.
//!
//! This is the async re-implementation of codex's `apply_patch` over our merged
//! [`ToolRuntime`](crate::tools::runtime::ToolRuntime) seam. It implements the
//! full trait stack ([`Approvable`] + [`Sandboxable`] + [`ToolRuntime`]) so it
//! can be driven by the [`ToolOrchestrator`](crate::tools::orchestrator::ToolOrchestrator),
//! mirroring the shell tool's structure (`tools/handlers/shell.rs`).
//!
//! # V4A format
//!
//! A patch is wrapped in a `*** Begin Patch` / `*** End Patch` envelope and
//! contains one or more file sections:
//!
//! * `*** Add File: <path>` — body of `+`-prefixed content lines.
//! * `*** Delete File: <path>` — removes an existing file.
//! * `*** Update File: <path>` (optionally followed by `*** Move to: <dest>`)
//!   — one or more `@@` hunks, each a sequence of ` ` (context), `-` (removed),
//!   and `+` (added) lines. An `*** End of File` marker may terminate the final
//!   hunk (codex `END_OF_FILE_MARKER`).
//!
//! # Parity grounding
//!
//! * **Envelope + section markers** — codex apply-patch
//!   (`apply-patch/src/parser.rs:193-200`: `BEGIN_PATCH_MARKER`,
//!   `END_PATCH_MARKER`, `ADD_FILE_MARKER`, `UPDATE_FILE_MARKER`,
//!   `DELETE_FILE_MARKER`, `MOVE_FILE_MARKER`, `END_OF_FILE_MARKER`) and legacy
//!   `browser-use-core/src/tools/files.rs` (`PATCH_BEGIN`/`PATCH_END`/
//!   `ADD_FILE_PREFIX`/`UPDATE_FILE_PREFIX`/`DELETE_FILE_PREFIX`/`MOVE_TO_PREFIX`
//!   constants, `parse_patch`, `parse_add_file_body`, `parse_update_hunks`).
//! * **Hunk apply (context-anchored splice)** — legacy `apply_hunk_to_lines` +
//!   `find_subsequence`: build the "before" view (context + removed) and "after"
//!   view (context + added), locate the before-block as a contiguous
//!   subsequence, splice in the after-block. The original trailing-newline state
//!   is preserved.
//! * **Path resolution** — absolute patch paths are honored as-is; relative paths
//!   are resolved against `cwd`, normalized lexically, and symlink-resolved for
//!   the existing prefix. This intentionally does NOT enforce a workspace
//!   boundary.
//!
//! # Parity caveats / TODOs
//!
//! * **turn_diff_tracker** — codex feeds applied changes into a per-turn diff
//!   tracker (`TurnDiffTracker`) for the `turn.diff` event. That subsystem is
//!   OUT OF SCOPE for this WP. TODO(WP-apply_patch-diff): integrate once the
//!   turn-diff seam lands.
//! * **lark grammar / fuzzy context** — codex uses a lark grammar and a
//!   fuzzy `seek_sequence` (line-number hints, whitespace-tolerant matching).
//!   This is a focused parser with exact-match context location (legacy
//!   `find_subsequence`), which matches the legacy behavior faithfully.
//!   TODO(WP-apply_patch-fuzzy): port the fuzzy seek for full codex parity.

use std::path::{Path, PathBuf};

use crate::tools::runtime::{
    Approvable, ExecOutput, SandboxAttempt, Sandboxable, ToolCtx, ToolError, ToolRuntime,
};
use crate::tools::sandbox::{SandboxPermissions, SandboxPreference};

/// V4A patch envelope + section markers.
///
/// Parity: legacy `files.rs:21-29` and codex `apply-patch/src/parser.rs:193-200`.
const PATCH_BEGIN: &str = "*** Begin Patch";
const PATCH_END: &str = "*** End Patch";
const ADD_FILE_PREFIX: &str = "*** Add File: ";
const UPDATE_FILE_PREFIX: &str = "*** Update File: ";
const DELETE_FILE_PREFIX: &str = "*** Delete File: ";
const MOVE_TO_PREFIX: &str = "*** Move to: ";
const END_OF_FILE_MARKER: &str = "*** End of File";
const HUNK_MARKER: &str = "@@";

/// Typed request for the `apply_patch` tool.
///
/// `patch` is the full V4A envelope text; `cwd` overrides [`ToolCtx::cwd`] as the
/// base directory for relative patch paths.
///
/// # Wire shape (model-facing args)
///
/// ```json
/// { "patch": "*** Begin Patch\n*** Add File: a.txt\n+hi\n*** End Patch" }
/// ```
///
/// Deserializes directly from the model's argument object. The `patch` field name
/// matches codex's apply-patch argument (`core/src/tools/handlers/apply_patch.rs`
/// / `apply-patch/src/parser.rs`) and the legacy spec
/// (`browser-use-core/src/tools/mod.rs` / `files.rs`). `cwd` is not part of the
/// model's wire args (codex applies under the session cwd); it defaults to `None`
/// so deserialization of `{ "patch": ... }` succeeds.
#[derive(Clone, Debug, PartialEq, Eq, serde::Deserialize)]
pub struct ApplyPatchRequest {
    /// The full V4A patch text, including the `*** Begin Patch` / `*** End Patch`
    /// envelope.
    pub patch: String,
    /// Workspace root to apply under. When `None`, the [`ToolCtx::cwd`] is used.
    #[serde(default)]
    pub cwd: Option<PathBuf>,
}

impl ApplyPatchRequest {
    /// Convenience constructor from a patch string, using the context cwd.
    pub fn new(patch: impl Into<String>) -> Self {
        Self {
            patch: patch.into(),
            cwd: None,
        }
    }
}

/// A single file operation parsed from a V4A patch.
///
/// Parity: legacy `PatchOperation` (files.rs:32-43).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PatchOperation {
    /// Add a new file with the given contents.
    AddFile { path: String, contents: String },
    /// Delete an existing file.
    DeleteFile { path: String },
    /// Update an existing file with hunks, optionally moving it.
    UpdateFile {
        path: String,
        move_to: Option<String>,
        hunks: Vec<UpdateHunk>,
    },
}

/// A single hunk within an update operation.
///
/// Parity: legacy `UpdateHunk` (files.rs:46-53).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateHunk {
    /// Optional `@@ context_header` line that locates the hunk.
    pub context_header: Option<String>,
    /// The ordered lines making up the hunk (context / removed / added).
    pub lines: Vec<HunkLine>,
}

/// A single line within a hunk.
///
/// Parity: legacy `HunkLine` (files.rs:56-64).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HunkLine {
    /// A context line (unchanged) — must match the existing file.
    Context(String),
    /// A removed line — must match the existing file and is deleted.
    Removed(String),
    /// An added line — inserted into the file.
    Added(String),
}

/// The summary returned on a successful apply: the relative paths that changed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApplyPatchSummary {
    /// The relative paths (as written in the patch) that were created, updated,
    /// or deleted, in application order.
    pub changed: Vec<String>,
}

impl ApplyPatchSummary {
    /// Render the summary as an [`ExecOutput`] (exit 0) so the tool's `Out` type
    /// matches the shell tool's [`ExecOutput`] seam.
    pub fn into_exec_output(self) -> ExecOutput {
        let stdout = if self.changed.is_empty() {
            "apply_patch: no files changed".to_string()
        } else {
            let mut s = format!("apply_patch: {} file(s) changed\n", self.changed.len());
            for p in &self.changed {
                s.push_str(&format!("  {p}\n"));
            }
            s
        };
        ExecOutput {
            exit_code: 0,
            stdout,
            stderr: String::new(),
        }
    }
}

/// The async `apply_patch` tool.
///
/// Stateless; cheap to clone/construct.
#[derive(Clone, Debug, Default)]
pub struct ApplyPatchTool;

impl ApplyPatchTool {
    /// Construct a new apply_patch tool.
    pub fn new() -> Self {
        Self
    }
}

/// Approval key: the patch text + workspace root identify a call for caching.
///
/// Codex parity: apply_patch approvals are keyed on the change set; here we key
/// on the full patch + root (the same shape the shell tool uses for command +
/// cwd, `shell.rs:222-226`).
#[derive(serde::Serialize, Clone, Debug, Eq, PartialEq, Hash)]
pub struct ApplyPatchApprovalKey {
    patch: String,
    cwd: Option<PathBuf>,
}

impl Approvable<ApplyPatchRequest> for ApplyPatchTool {
    type ApprovalKey = ApplyPatchApprovalKey;

    fn approval_keys(&self, req: &ApplyPatchRequest) -> Vec<Self::ApprovalKey> {
        vec![ApplyPatchApprovalKey {
            patch: req.patch.clone(),
            cwd: req.cwd.clone(),
        }]
    }

    /// `apply_patch` writes under its workspace root; request the default
    /// sandbox permissions (no escalation), mirroring the shell tool
    /// (`shell.rs:242-244`).
    fn sandbox_permissions(&self, _req: &ApplyPatchRequest) -> SandboxPermissions {
        SandboxPermissions::UseDefault
    }
}

impl Sandboxable for ApplyPatchTool {
    fn sandbox_preference(&self) -> SandboxPreference {
        // Let the provider decide (today everything resolves to
        // `SandboxType::None`). Matches the shell tool (`shell.rs:261-267`).
        SandboxPreference::Auto
    }

    fn escalate_on_failure(&self) -> bool {
        // Matches the shell tool (`shell.rs:269-273`): a sandbox denial may be
        // retried unsandboxed.
        true
    }
}

#[async_trait::async_trait]
impl ToolRuntime<ApplyPatchRequest, ExecOutput> for ApplyPatchTool {
    fn parallel_safe(&self, _req: &ApplyPatchRequest) -> bool {
        // Mutates the filesystem; not safe to run concurrently (matches the
        // shell tool, `shell.rs:278-282`).
        false
    }

    async fn run(
        &self,
        req: &ApplyPatchRequest,
        attempt: &SandboxAttempt<'_>,
        ctx: &ToolCtx,
    ) -> Result<ExecOutput, ToolError> {
        // Today the only sandbox is `None`; a real backend lands later behind
        // `attempt.sandbox`. Acknowledge the attempt to make the seam explicit
        // (matches the shell tool, `shell.rs:296-297`).
        let _ = attempt;

        let root = req.cwd.clone().unwrap_or_else(|| ctx.cwd.clone());

        // Parse the V4A envelope. A malformed envelope is a hard rejection (not
        // a panic) — surfaced as `ToolError::Rejected`.
        let operations = parse_patch(&req.patch)
            .map_err(|e| ToolError::Rejected(format!("apply_patch parse error: {e}")))?;

        // Apply. I/O failures are `Other`; paths are resolved permissively.
        let summary = apply_patch_operations(&operations, &root)?;

        Ok(summary.into_exec_output())
    }
}

// ---- V4A parser (legacy files.rs:561-768 parity) ----

/// Parse a V4A patch envelope into a sequence of operations.
///
/// Parity: legacy `parse_patch` (files.rs:561). The patch starts with
/// `*** Begin Patch`, ends with `*** End Patch`, and contains one or more file
/// sections.
pub fn parse_patch(patch: &str) -> Result<Vec<PatchOperation>, String> {
    let lines: Vec<&str> = patch.lines().collect();
    let mut idx = 0;

    // Skip leading blank lines before the envelope.
    while idx < lines.len() && lines[idx].trim().is_empty() {
        idx += 1;
    }

    // Require the begin marker.
    if idx >= lines.len() || lines[idx].trim() != PATCH_BEGIN {
        return Err(format!(
            "patch must start with '{}' (got: {:?})",
            PATCH_BEGIN,
            lines.first().copied().unwrap_or("")
        ));
    }
    idx += 1;

    let mut operations = Vec::new();

    // Parse file sections until the end marker.
    loop {
        if idx >= lines.len() {
            return Err(format!("patch is missing '{PATCH_END}'"));
        }

        let line = lines[idx];
        let trimmed_end = line.trim_end();

        // End marker terminates the patch.
        if line.trim() == PATCH_END {
            break;
        }

        if let Some(rest) = trimmed_end.strip_prefix(ADD_FILE_PREFIX) {
            let path = rest.trim().to_string();
            idx += 1;
            let (contents, next) = parse_add_file_body(&lines, idx);
            operations.push(PatchOperation::AddFile { path, contents });
            idx = next;
        } else if let Some(rest) = trimmed_end.strip_prefix(DELETE_FILE_PREFIX) {
            let path = rest.trim().to_string();
            idx += 1;
            operations.push(PatchOperation::DeleteFile { path });
        } else if let Some(rest) = trimmed_end.strip_prefix(UPDATE_FILE_PREFIX) {
            let path = rest.trim().to_string();
            idx += 1;
            // Optional `*** Move to:` line.
            let mut move_to = None;
            if idx < lines.len() {
                if let Some(dest) = lines[idx].trim_end().strip_prefix(MOVE_TO_PREFIX) {
                    move_to = Some(dest.trim().to_string());
                    idx += 1;
                }
            }
            let (hunks, next) = parse_update_hunks(&lines, idx)?;
            operations.push(PatchOperation::UpdateFile {
                path,
                move_to,
                hunks,
            });
            idx = next;
        } else {
            return Err(format!("unexpected line in patch: {line:?}"));
        }
    }

    if operations.is_empty() {
        return Err("patch contains no file operations".to_string());
    }

    Ok(operations)
}

/// True if `line` begins a new file section or ends the patch (a body terminator).
///
/// Parity: the section-terminator checks in legacy `parse_add_file_body` /
/// `parse_update_hunks`.
fn is_section_boundary(line: &str) -> bool {
    line.trim() == PATCH_END
        || line.starts_with(ADD_FILE_PREFIX)
        || line.starts_with(UPDATE_FILE_PREFIX)
        || line.starts_with(DELETE_FILE_PREFIX)
}

/// Parse the body of an `*** Add File:` section: consecutive `+`-prefixed lines.
///
/// Parity: legacy `parse_add_file_body` (files.rs:638).
fn parse_add_file_body(lines: &[&str], start: usize) -> (String, usize) {
    let mut contents = String::new();
    let mut idx = start;
    let mut first = true;

    while idx < lines.len() {
        let line = lines[idx];
        // A new section or the end marker terminates the body.
        if is_section_boundary(line) {
            break;
        }
        // Add-file bodies use `+` prefixes for every content line.
        if let Some(content) = line.strip_prefix('+') {
            if !first {
                contents.push('\n');
            }
            contents.push_str(content);
            first = false;
        } else if line.trim().is_empty() {
            // Tolerate blank separator lines inside the body.
            if !first {
                contents.push('\n');
            }
            first = false;
        } else {
            break;
        }
        idx += 1;
    }

    (contents, idx)
}

/// Parse the `@@`-delimited hunks of an `*** Update File:` section.
///
/// Parity: legacy `parse_update_hunks` (files.rs:612). An `*** End of File`
/// marker (codex `END_OF_FILE_MARKER`) closes the current hunk and is consumed
/// without contributing a body line.
fn parse_update_hunks(lines: &[&str], start: usize) -> Result<(Vec<UpdateHunk>, usize), String> {
    let mut hunks = Vec::new();
    let mut idx = start;
    let mut current: Option<UpdateHunk> = None;

    while idx < lines.len() {
        let line = lines[idx];

        // Section terminators.
        if is_section_boundary(line) {
            break;
        }

        // The `*** End of File` marker closes the current hunk (codex parity:
        // `END_OF_FILE_MARKER`, parser.rs:199). It anchors the final hunk at EOF
        // but carries no body content for our exact-match applier.
        if line.trim() == END_OF_FILE_MARKER {
            idx += 1;
            continue;
        }

        // A `@@` line starts a new hunk (optionally carrying a context header).
        if let Some(header) = line.strip_prefix(HUNK_MARKER) {
            if let Some(h) = current.take() {
                hunks.push(h);
            }
            let header = header.trim();
            current = Some(UpdateHunk {
                context_header: if header.is_empty() {
                    None
                } else {
                    Some(header.to_string())
                },
                lines: Vec::new(),
            });
            idx += 1;
            continue;
        }

        // Otherwise it's a hunk body line: ` ` context, `-` removed, `+` added.
        let hunk = current.get_or_insert_with(|| UpdateHunk {
            context_header: None,
            lines: Vec::new(),
        });
        let parsed = if let Some(rest) = line.strip_prefix(' ') {
            HunkLine::Context(rest.to_string())
        } else if let Some(rest) = line.strip_prefix('-') {
            HunkLine::Removed(rest.to_string())
        } else if let Some(rest) = line.strip_prefix('+') {
            HunkLine::Added(rest.to_string())
        } else if line.is_empty() {
            HunkLine::Context(String::new())
        } else {
            return Err(format!("invalid hunk line (no +/-/space prefix): {line:?}"));
        };
        hunk.lines.push(parsed);
        idx += 1;
    }

    if let Some(h) = current.take() {
        hunks.push(h);
    }

    if hunks.is_empty() {
        return Err("update file section has no hunks".to_string());
    }

    Ok((hunks, idx))
}

// ---- Apply (legacy files.rs:698-848 parity) ----

/// Apply a sequence of parsed operations under `root`.
///
/// Parity: legacy `apply_patch_operations` (files.rs:698). Each path is resolved
/// under `root` for relative paths before any write. Absolute paths are used
/// as-is. I/O failures are [`ToolError::Other`].
pub fn apply_patch_operations(
    operations: &[PatchOperation],
    root: &Path,
) -> Result<ApplyPatchSummary, ToolError> {
    let mut changed = Vec::new();

    for op in operations {
        match op {
            PatchOperation::AddFile { path, contents } => {
                let real = resolve_patch_path(root, path);
                reject_existing_special_file(&real, path)?;
                if let Some(parent) = real.parent() {
                    std::fs::create_dir_all(parent).map_err(|e| {
                        ToolError::Other(anyhow::anyhow!("creating parent dirs for {path}: {e}"))
                    })?;
                }
                std::fs::write(&real, contents).map_err(|e| {
                    ToolError::Other(anyhow::anyhow!("writing new file {path}: {e}"))
                })?;
                changed.push(path.clone());
            }
            PatchOperation::DeleteFile { path } => {
                let real = resolve_patch_path(root, path);
                std::fs::remove_file(&real)
                    .map_err(|e| ToolError::Other(anyhow::anyhow!("deleting file {path}: {e}")))?;
                changed.push(path.clone());
            }
            PatchOperation::UpdateFile {
                path,
                move_to,
                hunks,
            } => {
                let real = resolve_patch_path(root, path);
                ensure_regular_file_for_read(&real, path)?;
                let original = std::fs::read_to_string(&real).map_err(|e| {
                    ToolError::Other(anyhow::anyhow!("reading file to update {path}: {e}"))
                })?;
                let updated = apply_hunk_to_lines(&original, hunks).map_err(|e| {
                    ToolError::Other(anyhow::anyhow!("applying hunks to {path}: {e}"))
                })?;

                // Honor an optional move/rename.
                let dest = if let Some(dest_rel) = move_to {
                    resolve_patch_path(root, dest_rel)
                } else {
                    real.clone()
                };
                reject_existing_special_file(&dest, move_to.as_deref().unwrap_or(path))?;
                if let Some(parent) = dest.parent() {
                    std::fs::create_dir_all(parent).map_err(|e| {
                        ToolError::Other(anyhow::anyhow!("creating parent dirs for {path}: {e}"))
                    })?;
                }
                std::fs::write(&dest, updated).map_err(|e| {
                    ToolError::Other(anyhow::anyhow!("writing updated file {path}: {e}"))
                })?;
                // If moved, remove the original.
                if move_to.is_some() && dest != real {
                    std::fs::remove_file(&real).map_err(|e| {
                        ToolError::Other(anyhow::anyhow!(
                            "removing original after move {path}: {e}"
                        ))
                    })?;
                }
                changed.push(path.clone());
            }
        }
    }

    Ok(ApplyPatchSummary { changed })
}

fn ensure_regular_file_for_read(real: &Path, display: &str) -> Result<(), ToolError> {
    let meta = std::fs::metadata(real)
        .map_err(|e| ToolError::Other(anyhow::anyhow!("reading metadata for {display}: {e}")))?;
    if !meta.file_type().is_file() {
        return Err(ToolError::Rejected(format!(
            "apply_patch refuses to read non-regular file {display}"
        )));
    }
    Ok(())
}

fn reject_existing_special_file(real: &Path, display: &str) -> Result<(), ToolError> {
    match std::fs::metadata(real) {
        Ok(meta) if !meta.file_type().is_file() => Err(ToolError::Rejected(format!(
            "apply_patch refuses to write non-regular file {display}"
        ))),
        Ok(_) | Err(_) => Ok(()),
    }
}

/// Apply update hunks to the original file contents, returning the new contents.
///
/// Parity: legacy `apply_hunk_to_lines` (files.rs:789). Each hunk is located by
/// matching its context/removed lines against the file as a contiguous
/// subsequence; the matched region is replaced with the context/added lines. The
/// original trailing-newline state is preserved.
fn apply_hunk_to_lines(original: &str, hunks: &[UpdateHunk]) -> Result<String, String> {
    // Work on a vector of lines (no trailing newline sentinel).
    let mut result: Vec<String> = original.lines().map(|s| s.to_string()).collect();
    let trailing_newline = original.ends_with('\n');

    for hunk in hunks {
        // Build the "before" (context + removed) and "after" (context + added) views.
        let before: Vec<&String> = hunk
            .lines
            .iter()
            .filter_map(|l| match l {
                HunkLine::Context(s) | HunkLine::Removed(s) => Some(s),
                HunkLine::Added(_) => None,
            })
            .collect();
        let after: Vec<String> = hunk
            .lines
            .iter()
            .filter_map(|l| match l {
                HunkLine::Context(s) | HunkLine::Added(s) => Some(s.clone()),
                HunkLine::Removed(_) => None,
            })
            .collect();

        // Locate the "before" block in the current result.
        if before.is_empty() {
            return Err("hunk has no context or removed lines to anchor".to_string());
        }

        let pos = find_subsequence(&result, &before)
            .ok_or_else(|| "could not locate hunk context in file".to_string())?;

        // Replace the matched region with the "after" lines.
        let end = pos + before.len();
        result.splice(pos..end, after.iter().cloned());
    }

    // Reassemble, preserving the original trailing-newline state.
    let mut out = result.join("\n");
    if trailing_newline {
        out.push('\n');
    }

    Ok(out)
}

/// Find the first index where `needle` appears as a contiguous subsequence.
///
/// Parity: legacy `find_subsequence` (files.rs ~1672).
fn find_subsequence(haystack: &[String], needle: &[&String]) -> Option<usize> {
    if needle.is_empty() || needle.len() > haystack.len() {
        return None;
    }
    for start in 0..=(haystack.len() - needle.len()) {
        if haystack[start..]
            .iter()
            .zip(needle.iter())
            .all(|(h, n)| h == *n)
        {
            return Some(start);
        }
    }
    None
}

// ---- Path resolution ----

/// Resolve a patch path.
///
/// Absolute paths are used as-is. Relative paths are joined to `root`, normalized
/// lexically, and then the longest existing prefix is canonicalized so symlink
/// targets are followed. No workspace-boundary check is applied.
fn resolve_patch_path(root: &Path, path: &str) -> PathBuf {
    let input = Path::new(path);
    let candidate = if input.is_absolute() {
        input.to_path_buf()
    } else {
        root.join(input)
    };
    let normalized = normalize_lexically(&candidate);
    resolve_existing_prefix(&normalized)
}

/// Normalize a path lexically, resolving `.` and `..` without touching the FS.
///
/// Parity: legacy `normalize_lexically` (files.rs ~1628).
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
/// Parity: legacy `resolve_existing_prefix` (files.rs ~1644).
fn resolve_existing_prefix(path: &Path) -> PathBuf {
    // Find the longest ancestor that exists, canonicalize it, then re-append the
    // remaining (non-existent) components.
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
