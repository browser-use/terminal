//! `browser-use-agent` — the async agent engine (rearchitecture Milestone 3).
//!
//! Strategy **B (parallel rewrite)**: this crate is built alongside the legacy
//! synchronous `browser-use-core`, on top of the async, provider-neutral
//! `browser-use-llm`. Subsystems are ported here one at a time, each with
//! codex-parity tests, and the TUI/CLI are switched over to this engine only
//! once parity is reached. Until then `browser-use-core` remains the live engine.
//!
//! This module tree is the **frozen interface scaffold** (WP-A0): every type,
//! trait, and function signature is the contract later work packages fill in.
//! Bodies are `unimplemented!()` / trivial; only the shapes are load-bearing.
//!
//! Layering:
//! - [`decision`] — PURE sync decision core (the unit-test surface).
//! - [`events`]   — sync `EventSink` fan-out + pure stream-event mapper.
//! - [`context`]  — `ContextManager` + real token accounting (pure core + thin async).
//! - [`compact`]  — model-based context compaction (codex `compact.rs` parity).
//! - [`tools`]    — `ToolOrchestrator` + runtime/approval/sandbox seam.
//! - [`turn`]     — the async turn loop + in-turn tool dispatch.
//! - [`task`]     — async task driver / lifecycle.
//! - [`session`]  — lifecycle, resume-by-replay, fork/rollback.
//! - [`testkit`]  — deterministic fakes shared by every WP's tests.

pub mod compact;
pub mod config;
pub mod config_model;
pub mod config_overrides;
pub mod context;

pub mod decision;
pub mod entrypoint;
pub mod error;
pub mod events;
pub mod execpolicy;
pub mod goals;
pub mod guardian;
pub mod history;
pub mod hooks;
pub mod infra;
pub mod live_executor;
pub mod mcp;
pub mod network;
pub mod prompts;
pub mod rollout;
pub mod sandbox_backends;
pub mod session;
pub mod simple_harness;
pub mod skills;
pub mod subagents;
pub mod task;
pub mod testkit;
pub mod tools;
pub mod turn;

pub use config::AgentConfig;
pub use error::AgentError;

// Config-layer model resolution (Phase-E gap-fill): AGENTS.md / config-profile
// model + provider-id + catalog resolution for a cwd, ported from
// browser-use-core. Used by the tui/cli repoint.
pub use config_model::bundled_model_catalog;
pub use config_model::configured_model_for_cwd;
pub use config_model::configured_model_for_cwd_with_options;
pub use config_model::configured_model_provider_id_for_cwd;
pub use config_model::configured_model_provider_id_for_cwd_with_options;
pub use config_model::default_model_for_cwd;
pub use config_model::default_model_for_cwd_with_options;
pub use config_model::model_catalog_for_cwd;
pub use config_model::model_catalog_for_cwd_with_options;
pub use config_model::FakeAgentOptions;
pub use config_model::ModelCatalog;
pub use config_model::ModelCatalogEntry;

#[cfg(test)]
pub(crate) mod test_env {
    use std::sync::{Mutex, MutexGuard, OnceLock};

    static ENV_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

    pub(crate) fn lock() -> MutexGuard<'static, ()> {
        ENV_LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|err| err.into_inner())
    }
}

// The run-entrypoint facade (Wave-3 cutover): the binary-facing call tui/cli use
// to run a session on the new async engine. It assembles
// config -> provider/driver -> context seed -> turn loop -> store persistence and
// is the first production caller of `turn::model_path::build_sampling_driver`.
pub use entrypoint::{
    cleanup_all_unified_exec_managers, cleanup_unified_exec_manager_for_session_id,
    run_session_with_config, run_session_with_config_with_cancel,
    run_session_with_config_with_cancel_and_runtime,
};
pub use live_executor::{
    ensure_agent_attached as ensure_live_agent_attached, RuntimeAgentBackgroundCompletion,
    RuntimeAgentExecutor, RuntimeAgentExecutorConfig, RuntimeAgentRunRequest,
    RuntimeAgentRunResult,
};
#[allow(deprecated)]
pub use live_executor::{
    LiveAgentBackgroundCompletion, LiveAgentExecutor, LiveAgentExecutorConfig, LiveAgentRunRequest,
    LiveAgentRunResult,
};

#[cfg(test)]
mod tests {
    /// Scaffold smoke test: the crate builds and the async test harness runs.
    #[tokio::test]
    async fn crate_builds_and_async_harness_runs() {
        assert_eq!(2 + 2, 4);
    }
}
