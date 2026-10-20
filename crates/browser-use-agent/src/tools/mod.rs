//! `tools/` — `ToolOrchestrator` + runtime/approval/sandbox seam.
//!
//! Initial scope runs with `sandbox = None` via `NoneSandboxProvider`; the real
//! Landlock/seccomp provider lands later behind the same trait surface.

pub mod approval;
mod command_guard;
pub mod handlers;
pub mod orchestrator;
pub mod registry;
pub mod runtime;
pub mod sandbox;
pub mod unified_exec;

pub use approval::{
    ApprovalStore, AskForApproval, ExecApprovalRequirement, GranularApprovalConfig, ReviewDecision,
};
pub use orchestrator::{OrchestratorRunResult, ToolOrchestrator, TurnEnv};
pub use registry::{DynTool, ToolAdapter, ToolRegistry};
pub use runtime::{
    build_denial_reason, default_exec_approval_requirement, map_decision, plan_attempts,
    sandbox_override_for_first_attempt, Approvable, ApprovalRequest, Approver, AttemptPlan,
    AutoApprover, DenialAction, ExecOutput, NetworkPolicyDecision, PermissionRequestPayload,
    SandboxAttempt, SandboxDenial, Sandboxable, ToolCtx, ToolError, ToolRuntime,
};
pub use sandbox::{
    FileSystemSandboxPolicy, NoneSandboxProvider, SandboxLaunch, SandboxOverride,
    SandboxPermissions, SandboxPreference, SandboxProvider, SandboxType,
};
pub use unified_exec::{UnifiedExecEventEmitter, UnifiedExecManager};

/// Re-export for callers that classify dispatch parallelism.
pub use crate::decision::ToolParallelism;

#[cfg(test)]
mod approval_tests;
#[cfg(test)]
mod orchestrator_tests;
#[cfg(test)]
mod sandbox_tests;
