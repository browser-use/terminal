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
//! It does NOT itself force the turn loop to stop: the loop's termination signal
//! is "the model produced a final assistant message with NO tool calls"
//! ([`TurnRunOutcome::NoToolCalls`](crate::turn::loop::TurnRunOutcome)). A `done`
//! call is a tool call, so it is dispatched, recorded, and the loop re-samples;
//! the model then typically produces a final no-tool message and the loop stops.
//! Wiring the loop to treat a successful `done` [`ExecOutput`] as terminal (a
//! short-circuit) needs the loop's classifier (`turn/loop.rs` /
//! `turn/fusion.rs`) to inspect the dispatched tool name/output — those files are
//! outside this WP's owned set, so that deeper loop wiring is REPORTED, not
//! implemented here.
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
        _ctx: &ToolCtx,
    ) -> Result<ExecOutput, ToolError> {
        // No sandbox is exercised (the tool does no I/O); acknowledge the attempt
        // to make the seam explicit, matching the other tools.
        let _ = attempt;

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
