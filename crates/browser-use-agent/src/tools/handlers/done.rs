//! `done` tool: the explicit completion tool the model calls to signal the task
//! is finished and to carry its final summary message.
//!
//! This is the async re-implementation of the codex/legacy completion (`done` /
//! finish) tool over our merged [`ToolRuntime`](crate::tools::runtime::ToolRuntime)
//! seam. It implements the full trait stack ([`Approvable`] + [`Sandboxable`] +
//! [`ToolRuntime`]) so it can be driven by the
//! [`ToolOrchestrator`](crate::tools::orchestrator::ToolOrchestrator), mirroring
//! the `update_plan` tool's structure: a non-FS, accept-and-return tool that
//! touches no filesystem and spawns no process.
//!
//! # What this tool does (and does NOT) do
//!
//! It RECORDS the model's final completion message and returns a deterministic
//! acknowledgement (prefixed with [`DONE_STDOUT_PREFIX`]) so the loop / host can
//! recognize that the agent declared itself finished, and so the final `text`
//! (the summary) is surfaced to the host.
//!
//! A successful `done` output is treated as terminal by the fused sampling loop.
//! In eval mode (`BROWSER_USE_EVAL_DONE_AUDIT=1`) this handler performs a small
//! completion audit first. If the final answer is obviously empty, placeholder
//! heavy, or explicitly partial, it rejects the call so the loop gives the model
//! one more repair turn instead of accepting a weak completion.
//!
//! # Parity grounding
//!
//! * **Tool name** — `done` (the completion tool key). Mirrors the codex/legacy
//!   completion/`done` tool the agent calls to declare it has finished.
//! * **Args** — `{ "result"?: string, "text"?: string, "result_file"?: string }`:
//!   an optional user-facing final answer, a legacy `text` alias, and an optional
//!   result file pointer. Codex's completion carries the final assistant text;
//!   Browser Use prompts call this `result`, so both names are accepted.
//! * **no approval / benign** — like `update_plan`, this is a pure state echo: it
//!   needs no approval and touches no sandbox. We leave
//!   [`exec_approval_requirement`](Approvable::exec_approval_requirement) at its
//!   default `None` so the orchestrator's policy-driven
//!   [`default_exec_approval_requirement`](crate::tools::runtime::default_exec_approval_requirement)
//!   applies (which yields `Skip` under any non-prompting policy).
//! * **parallel_safe = false** — completion is terminal and must not be reordered
//!   around other tools; it runs on the serial path (matching the trait default
//!   the codex completion handler inherits).

use crate::tools::runtime::{
    Approvable, ExecOutput, SandboxAttempt, Sandboxable, ToolCtx, ToolError, ToolRuntime,
};
use crate::tools::sandbox::{SandboxPermissions, SandboxPreference};
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};

const EVAL_DONE_AUDIT_ENV: &str = "BROWSER_USE_EVAL_DONE_AUDIT";
const DONE_AUDIT_TEXT_PREVIEW_BYTES: usize = 256 * 1024;

/// The tool name surfaced to the model.
///
/// Parity: the codex/legacy completion (`done`) tool key.
pub const DONE_TOOL_NAME: &str = "done";

/// Prefix on the [`ExecOutput::stdout`] acknowledgement so a later loop/host-aware
/// layer can recognize the completion signal (and the final summary text).
///
/// This is a property of our [`ExecOutput`] seam, NOT a codex/legacy wire
/// constant: the loop does not yet short-circuit on this (see the module doc),
/// so the prefix lets a host recognize the declared completion deterministically.
pub const DONE_STDOUT_PREFIX: &str = "done:";

/// Typed request for the `done` tool.
///
/// `result` is the canonical final answer. `text` remains accepted for legacy
/// callers, and `result_file` can point at a persisted artifact when the answer
/// is intentionally file-backed. All fields are optional so the model may still
/// declare done with no message.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DoneRequest {
    /// Canonical user-facing final answer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<String>,
    /// Legacy final summary alias.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// Optional relative or absolute result artifact path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result_file: Option<String>,
}

impl DoneRequest {
    /// Convenience constructor with a final summary message.
    pub fn with_text(text: impl Into<String>) -> Self {
        Self {
            text: Some(text.into()),
            ..Self::default()
        }
    }

    /// Convenience constructor with the canonical final answer field.
    pub fn with_result(result: impl Into<String>) -> Self {
        Self {
            result: Some(result.into()),
            ..Self::default()
        }
    }

    /// The user-facing final answer, trimmed.
    ///
    /// `result` wins over legacy `text`. If both are blank and only a
    /// `result_file` was supplied, expose a compact file-pointer summary so the
    /// host has a visible completion result.
    pub fn summary(&self) -> String {
        if let Some(result) = self
            .result
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            return result.to_string();
        }
        if let Some(text) = self
            .text
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            return text.to_string();
        }
        if let Some(result_file) = self
            .result_file
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            return format!("Result file: {result_file}");
        }
        String::new()
    }
}

/// The async `done` tool.
///
/// Stateless; cheap to clone/construct. Performs no I/O and spawns no process.
#[derive(Clone, Debug, Default)]
pub struct DoneTool;

impl DoneTool {
    /// Construct a new `done` tool.
    pub fn new() -> Self {
        Self
    }
}

/// Approval key: the final text identifies a call for session caching, mirroring
/// the shape the other non-FS tools use. This tool never prompts (it is benign),
/// so the key is rarely consulted; it exists to satisfy [`Approvable`] uniformly.
#[derive(serde::Serialize, Clone, Debug, Eq, PartialEq, Hash)]
pub struct DoneApprovalKey {
    result: Option<String>,
    text: Option<String>,
    result_file: Option<String>,
}

impl Approvable<DoneRequest> for DoneTool {
    type ApprovalKey = DoneApprovalKey;

    fn approval_keys(&self, req: &DoneRequest) -> Vec<Self::ApprovalKey> {
        vec![DoneApprovalKey {
            result: req.result.clone(),
            text: req.text.clone(),
            result_file: req.result_file.clone(),
        }]
    }

    /// `done` touches no filesystem; request the default sandbox permissions (no
    /// escalation), mirroring the other non-FS tools.
    fn sandbox_permissions(&self, _req: &DoneRequest) -> SandboxPermissions {
        SandboxPermissions::UseDefault
    }

    // `exec_approval_requirement` is left at its trait default (`None`): the
    // completion tool needs no approval. Returning `None` lets the orchestrator
    // apply `default_exec_approval_requirement`, which yields `Skip` under any
    // non-prompting policy. See the module doc.
}

impl Sandboxable for DoneTool {
    fn sandbox_preference(&self) -> SandboxPreference {
        // Let the provider decide (today everything resolves to
        // `SandboxType::None`). The tool does no I/O, so the sandbox is moot, but
        // `Auto` keeps the seam uniform with the other tools.
        SandboxPreference::Auto
    }

    fn escalate_on_failure(&self) -> bool {
        // The tool never produces a sandbox denial (it does no I/O); `true` keeps
        // it uniform with the other tools.
        true
    }
}

#[async_trait::async_trait]
impl ToolRuntime<DoneRequest, ExecOutput> for DoneTool {
    fn parallel_safe(&self, _req: &DoneRequest) -> bool {
        // Completion is terminal: it must run on the serial path so no other tool
        // reorders around the declared finish. Matches the trait default `false`
        // the codex completion handler inherits.
        false
    }

    async fn run(
        &self,
        req: &DoneRequest,
        attempt: &SandboxAttempt<'_>,
        ctx: &ToolCtx,
    ) -> Result<ExecOutput, ToolError> {
        // No sandbox is exercised (the tool does no I/O); acknowledge the attempt
        // to make the seam explicit, matching the other tools.
        let _ = attempt;

        if eval_done_audit_enabled() {
            audit_done_request(req, ctx).map_err(ToolError::Rejected)?;
        }

        // Record the final summary into a deterministic, prefixed acknowledgement
        // the loop/host can recognize as the declared completion. The summary may
        // be empty (the model can declare done without a message).
        let summary = req.summary();
        Ok(ExecOutput {
            exit_code: 0,
            stdout: format!("{DONE_STDOUT_PREFIX}{summary}"),
            stderr: String::new(),
        })
    }
}

fn eval_done_audit_enabled() -> bool {
    std::env::var(EVAL_DONE_AUDIT_ENV)
        .ok()
        .is_some_and(|value| env_flag_enabled(&value))
}

fn env_flag_enabled(value: &str) -> bool {
    let normalized = value.trim().to_ascii_lowercase();
    !normalized.is_empty()
        && !matches!(
            normalized.as_str(),
            "0" | "false" | "off" | "no" | "disabled"
        )
}

pub(crate) fn audit_done_request(req: &DoneRequest, ctx: &ToolCtx) -> Result<(), String> {
    let mut reasons = Vec::new();
    let mut audit_text = req.summary();
    let mut has_material_answer = !audit_text.trim().is_empty();

    if let Some(result_file) = req
        .result_file
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        match read_result_file_preview(result_file, ctx) {
            Ok(preview) => {
                has_material_answer = true;
                if let Some(preview) = preview {
                    reasons.extend(json_audit_reasons(&preview));
                    if !audit_text.is_empty() {
                        audit_text.push('\n');
                    }
                    audit_text.push_str(&preview);
                }
            }
            Err(reason) => reasons.push(reason),
        }
    }

    if !has_material_answer {
        reasons.push("no final answer text or readable non-empty result_file".to_string());
    }

    reasons.extend(text_audit_reasons(&audit_text));
    reasons.extend(json_audit_reasons(&audit_text));

    if reasons.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "done audit rejected the final answer: {}. Continue working, verify and fill the missing fields, then call done again. Only use an incomplete fallback if the run is genuinely out of turns.",
            reasons.join("; ")
        ))
    }
}

fn read_result_file_preview(path: &str, ctx: &ToolCtx) -> Result<Option<String>, String> {
    let resolved = resolve_result_file(path, ctx);
    let metadata = fs::metadata(&resolved).map_err(|error| {
        format!(
            "result_file `{}` is not readable at {} ({error})",
            path,
            resolved.display()
        )
    })?;
    if !metadata.is_file() {
        return Err(format!(
            "result_file `{}` is not a regular file ({})",
            path,
            resolved.display()
        ));
    }
    if metadata.len() == 0 {
        return Err(format!("result_file `{}` is empty", path));
    }

    let bytes = fs::read(&resolved).map_err(|error| {
        format!(
            "result_file `{}` could not be read at {} ({error})",
            path,
            resolved.display()
        )
    })?;
    let cap = bytes.len().min(DONE_AUDIT_TEXT_PREVIEW_BYTES);
    Ok(std::str::from_utf8(&bytes[..cap])
        .ok()
        .map(ToOwned::to_owned))
}

fn resolve_result_file(path: &str, ctx: &ToolCtx) -> PathBuf {
    let requested = Path::new(path);
    if requested.is_absolute() {
        return requested.to_path_buf();
    }

    let cwd_path = ctx.cwd.join(requested);
    if cwd_path.exists() {
        return cwd_path;
    }

    let artifact_path = ctx.artifact_root.join(requested);
    if artifact_path.exists() {
        return artifact_path;
    }

    cwd_path
}

fn text_audit_reasons(text: &str) -> Vec<String> {
    let normalized = text.to_ascii_lowercase();
    let markers = [
        ("partial_incomplete", "declares partial_incomplete"),
        ("partial / incomplete", "declares partial/incomplete"),
        ("partial/incomplete", "declares partial/incomplete"),
        ("not completed", "declares not completed"),
        ("not checked", "declares not checked"),
        ("not extracted", "declares not extracted"),
        ("could not verify", "declares unverified data"),
        ("couldn't verify", "declares unverified data"),
        ("could not access", "declares inaccessible source"),
        ("couldn't access", "declares inaccessible source"),
        ("unavailable due to", "declares unavailable data"),
        ("source unavailable", "declares unavailable source"),
        ("blocked by", "declares blocked source"),
        ("unable to complete", "declares incomplete task"),
        ("task is incomplete", "declares incomplete task"),
    ];

    markers
        .iter()
        .filter_map(|(needle, reason)| normalized.contains(needle).then(|| (*reason).to_string()))
        .collect()
}

fn json_audit_reasons(text: &str) -> Vec<String> {
    let Some(value) = parse_first_json_value(text) else {
        return Vec::new();
    };
    let mut reasons = Vec::new();
    if json_is_empty_result(&value) {
        reasons.push("JSON result is empty".to_string());
    }
    let mut stats = JsonPlaceholderStats::default();
    collect_json_placeholder_stats(&value, &mut stats);
    if stats.total_scalar_fields >= 8
        && stats.placeholder_fields * 100 >= stats.total_scalar_fields * 30
    {
        reasons.push(format!(
            "JSON result has too many placeholder fields ({}/{})",
            stats.placeholder_fields, stats.total_scalar_fields
        ));
    }
    reasons
}

fn parse_first_json_value(text: &str) -> Option<Value> {
    let trimmed = text.trim();
    serde_json::from_str(trimmed).ok()
}

fn json_is_empty_result(value: &Value) -> bool {
    match value {
        Value::Array(items) => items.is_empty(),
        Value::Object(map) => {
            if map.is_empty() {
                return true;
            }
            let array_values = map
                .values()
                .filter_map(|value| value.as_array())
                .collect::<Vec<_>>();
            !array_values.is_empty() && array_values.iter().all(|items| items.is_empty())
        }
        _ => false,
    }
}

#[derive(Default)]
struct JsonPlaceholderStats {
    total_scalar_fields: usize,
    placeholder_fields: usize,
}

fn collect_json_placeholder_stats(value: &Value, stats: &mut JsonPlaceholderStats) {
    match value {
        // A field explicitly set to null means the agent checked the source and
        // recorded a genuine absence. Many tasks REQUIRE null/empty for
        // unavailable fields, so count null toward the denominator but NOT as a
        // placeholder. Counting it as a placeholder pushed the agent to delete
        // required fields to satisfy the audit (real_v8 task 53 regression).
        Value::Null => stats.total_scalar_fields += 1,
        Value::Bool(_) | Value::Number(_) => stats.total_scalar_fields += 1,
        Value::String(text) => {
            stats.total_scalar_fields += 1;
            if is_placeholder_string(text) {
                stats.placeholder_fields += 1;
            }
        }
        Value::Array(items) => {
            for item in items {
                collect_json_placeholder_stats(item, stats);
            }
        }
        Value::Object(map) => {
            for value in map.values() {
                collect_json_placeholder_stats(value, stats);
            }
        }
    }
}

fn is_placeholder_string(text: &str) -> bool {
    let normalized = text.trim().to_ascii_lowercase();
    // An empty string is a deliberate "checked, genuinely absent" sentinel, and
    // many tasks explicitly MANDATE "" for missing values. Counting "" as a
    // placeholder rejected spec-correct answers and coerced literal placeholder
    // prose instead (real_v8 task 94). null is treated the same way.
    if normalized.is_empty() {
        return false;
    }
    matches!(
        normalized.as_str(),
        "unknown"
            | "n/a"
            | "na"
            | "none"
            | "null"
            | "missing"
            | "not found"
            | "unavailable"
            | "not available"
            | "not listed"
            | "not checked"
            | "not extracted"
            | "could not determine"
    ) || normalized.starts_with("could not ")
        || normalized.starts_with("unable to ")
}
