//! Browser Use adapter layer for embedding Codex app-server directly.
//!
//! This crate is intentionally small at first. It establishes the direct Codex
//! crate boundary, typed thread/turn request construction, and Browser Use event
//! projection before the TUI/SDK call sites are moved over.

mod mapper;
mod requests;
mod runner;

pub const OPENAI_API_PROVIDER_ID: &str = "browser_use_openai_api";

pub use mapper::{CodexEventMapper, CodexMappedEvent, CodexProjectedEvent, CodexTerminalOutcome};
pub use requests::{
    browser_harness_env_config, thread_start_params, turn_start_params, CodexThreadStartSpec,
    CodexTurnStartSpec,
};
pub use runner::{run_codex_engine_turn, CodexEngineRunResult, CodexEngineRunSpec};

/// Compile-time anchor proving the engine is built on Codex app-server crates,
/// not a `codex exec` subprocess boundary.
pub type CodexInProcessClient = codex_app_server_client::InProcessAppServerClient;
