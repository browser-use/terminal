//! `events/` — sync `EventSink` fan-out + PURE `LlmEvent` -> protocol mapper.
//!
//! This is the seam the turn loop calls WITHOUT `.await` so its logic stays
//! testable. The only `EventSink` that touches the DB is `StoreSink`.

pub mod map;
pub mod names;
pub mod store_sink;

#[cfg(test)]
mod map_tests;
#[cfg(test)]
mod store_sink_tests;

use serde_json::Value;

#[derive(Clone, Debug, PartialEq)]
pub struct PendingEvent {
    pub session_id: String,
    pub event_type: String,
    pub payload: Value,
}

impl PendingEvent {
    pub fn new(session_id: impl Into<String>, ty: impl Into<String>, payload: Value) -> Self {
        Self {
            session_id: session_id.into(),
            event_type: ty.into(),
            payload,
        }
    }
}

/// Synchronous, infallible fan-out. Real impl = `StoreSink`; tests = `Vec` recorder.
pub trait EventSink: Send + Sync {
    fn emit(&self, ev: PendingEvent);
}

pub struct TeeSink(pub Vec<std::sync::Arc<dyn EventSink>>);

impl EventSink for TeeSink {
    fn emit(&self, ev: PendingEvent) {
        for s in &self.0 {
            s.emit(ev.clone());
        }
    }
}

#[derive(Clone, Debug)]
pub struct TurnCtx {
    pub session_id: String,
    pub model: String,
    pub provider: String,
    pub base_instructions: String,
    pub browser_mode_instruction: Option<String>,
    pub turn_idx: usize,
    pub attempt: usize,
}

pub use map::{
    map_llm_event, session_done_payload, token_count_payload, usage_to_model_usage, ResultFilePtr,
};
pub use store_sink::{ArtifactSpec, StoreSink};
