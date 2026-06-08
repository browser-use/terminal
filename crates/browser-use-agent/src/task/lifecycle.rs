//! Task lifecycle value types (codex `tasks/mod.rs` parity).
//!
//! These are the pure value types that the [`super::driver::TaskDriver`] emits and
//! consumes. They mirror codex's `tasks/mod.rs`:
//!
//! - [`TurnAbortReason`] — why an active turn was torn down (`tasks/mod.rs`).
//! - [`InterruptedTurnHistoryMarker`] — what marker (if any) is recorded into
//!   conversation history when a turn is *interrupted* (vs. replaced/errored),
//!   selected from config by [`InterruptedTurnHistoryMarker::from_config`]
//!   (`tasks/mod.rs:74`).
//! - [`TaskKind`] — the flavor of work an active turn is running.
//! - [`TurnLifecycleEvent`] — the start/complete/abort lifecycle signals the
//!   driver hands to an observer.

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TurnAbortReason {
    /// The user explicitly interrupted the active turn (records a history marker).
    Interrupted,
    /// A new task replaced the active one (`spawn_task`); no history marker.
    Replaced,
    /// The active turn stopped because an internal max-turn budget was exhausted.
    MaxTurns,
    /// The active turn was torn down because of an error.
    Error,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InterruptedTurnHistoryMarker {
    /// No interrupted-turn marker is recorded into history.
    Disabled,
    /// Record a *contextual user* marker (single-agent interrupt UX).
    ContextualUser,
    /// Record a *developer* marker (multi-agent v2 interrupt UX).
    Developer,
}

impl InterruptedTurnHistoryMarker {
    /// Select the interrupted-turn history marker from config (`tasks/mod.rs:74`).
    ///
    /// Truth table (codex parity):
    ///
    /// | `interrupt_enabled` | `multi_agent_v2` | marker            |
    /// |---------------------|------------------|-------------------|
    /// | `false`             | (either)         | `Disabled`        |
    /// | `true`              | `false`          | `ContextualUser`  |
    /// | `true`              | `true`           | `Developer`       |
    ///
    /// When interrupts are disabled no marker is recorded regardless of the
    /// multi-agent flag. When enabled, the multi-agent-v2 flavor records a
    /// developer marker; otherwise a contextual-user marker.
    pub fn from_config(interrupt_enabled: bool, multi_agent_v2: bool) -> Self {
        match (interrupt_enabled, multi_agent_v2) {
            (false, _) => Self::Disabled,
            (true, false) => Self::ContextualUser,
            (true, true) => Self::Developer,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaskKind {
    /// A normal user-driven turn.
    Regular,
    /// A history-compaction turn.
    Compact,
    /// A review turn.
    Review,
}

#[derive(Debug, Clone)]
pub enum TurnLifecycleEvent {
    /// Emitted right before an active turn's task future is spawned.
    TurnStarted { turn_id: String },
    /// Emitted when the active turn ran to normal completion (not cancelled).
    TurnComplete {
        turn_id: String,
        last_agent_message: Option<String>,
    },
    /// Emitted when the active turn was torn down (graceful-then-hard abort).
    TurnAborted {
        turn_id: String,
        reason: TurnAbortReason,
    },
}
