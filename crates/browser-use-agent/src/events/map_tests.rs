//! Pure unit tests for the `LlmEvent` -> `PendingEvent` mapper, the usage /
//! payload helpers, and the `TeeSink` fan-out. Parity-pinned against
//! browser-use-core.

use super::map::{
    map_llm_event, session_done_payload, token_count_payload, usage_to_model_usage, ResultFilePtr,
};
use super::{names, EventSink, PendingEvent, TeeSink, TurnCtx};
use browser_use_llm::schema::{FinishReason, LlmEvent, Usage};
use browser_use_protocol::ModelUsage;
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};

fn ctx() -> TurnCtx {
    TurnCtx {
        session_id: "sess-1".to_string(),
        model: "gpt-test".to_string(),
        provider: "openai".to_string(),
        base_instructions: crate::prompts::browser_agent_system_prompt(),
        browser_mode_instruction: None,
        turn_idx: 3,
        attempt: 0,
    }
}

// ── map_llm_event: each LlmEvent variant -> exact Vec<PendingEvent> ──────────

#[test]
fn text_delta_maps_to_stream_delta() {
    let got = map_llm_event(
        &ctx(),
        &LlmEvent::TextDelta {
            id: "t0".to_string(),
            delta: "hello".to_string(),
        },
    );
    assert_eq!(
        got,
        vec![PendingEvent::new(
            "sess-1",
            names::MODEL_STREAM_DELTA,
            json!({ "text": "hello", "delta": "hello" }),
        )]
    );
}

#[test]
fn reasoning_delta_maps_to_thinking_delta() {
    let got = map_llm_event(
        &ctx(),
        &LlmEvent::ReasoningDelta {
            id: "r0".to_string(),
            delta: "thinking".to_string(),
        },
    );
    assert_eq!(
        got,
        vec![PendingEvent::new(
            "sess-1",
            names::MODEL_THINKING_DELTA,
            json!({ "text": "thinking", "delta": "thinking" }),
        )]
    );
}

#[test]
fn tool_call_maps_to_tool_started_with_parsed_arguments() {
    let got = map_llm_event(
        &ctx(),
        &LlmEvent::ToolCall {
            id: "c0".to_string(),
            name: "click".to_string(),
            namespace: None,
            input: json!({ "index": 5 }),
        },
    );
    assert_eq!(
        got,
        vec![PendingEvent::new(
            "sess-1",
            names::TOOL_STARTED,
            json!({ "name": "click", "tool_call_id": "c0", "arguments": { "index": 5 } }),
        )]
    );
}

#[test]
fn provider_error_maps_to_stream_error() {
    let got = map_llm_event(
        &ctx(),
        &LlmEvent::ProviderError {
            message: "boom".to_string(),
            retryable: true,
        },
    );
    assert_eq!(
        got,
        vec![PendingEvent::new(
            "sess-1",
            names::STREAM_ERROR,
            json!({ "message": "boom" }),
        )]
    );
}

#[test]
fn finish_maps_to_token_count_from_usage() {
    let usage = Usage {
        input_tokens: 100,
        cached_input_tokens: 10,
        cache_creation_input_tokens: 0,
        output_tokens: 20,
        reasoning_output_tokens: 5,
        total_tokens: 125,
    };
    let got = map_llm_event(
        &ctx(),
        &LlmEvent::Finish {
            usage,
            finish_reason: Some(FinishReason::Stop),
        },
    );
    let expected_usage = usage_to_model_usage(&usage);
    assert_eq!(
        got,
        vec![PendingEvent::new(
            "sess-1",
            names::TOKEN_COUNT,
            // prev_total = null, window = none, turn_idx from ctx (3).
            token_count_payload(&expected_usage, &Value::Null, None, 3),
        )]
    );
    // Spell out the resolved shape so the parity contract is visible.
    // prev_total = null -> total_token_usage == last_token_usage (this turn).
    assert_eq!(
        got[0].payload,
        json!({
            "info": {
                "total_token_usage": {
                    "input_tokens": 100,
                    "cached_input_tokens": 10,
                    "output_tokens": 20,
                    "reasoning_output_tokens": 5,
                    "total_tokens": 125,
                },
                "last_token_usage": {
                    "input_tokens": 100,
                    "cached_input_tokens": 10,
                    "output_tokens": 20,
                    "reasoning_output_tokens": 5,
                    "total_tokens": 125,
                },
                "model_context_window": Value::Null,
            },
            "turn_idx": 3,
        })
    );
}

#[test]
fn lifecycle_markers_map_to_nothing() {
    // codex/core records no per-marker UI event for these — assert they map to
    // empty. Note: Finish ALWAYS emits a token_count, so it is NOT in this list.
    let empties = [
        LlmEvent::StepStart,
        LlmEvent::TextStart { id: "t".into() },
        LlmEvent::TextEnd {
            id: "t".into(),
            phase: None,
        },
        LlmEvent::ReasoningStart { id: "r".into() },
        LlmEvent::ReasoningEnd { id: "r".into() },
        LlmEvent::ToolInputStart {
            id: "c".into(),
            name: "click".into(),
        },
        LlmEvent::ToolInputDelta {
            id: "c".into(),
            delta: "{".into(),
        },
        LlmEvent::ToolInputEnd { id: "c".into() },
        LlmEvent::StepFinish {
            usage: Usage::default(),
            finish_reason: None,
        },
        LlmEvent::StepFinish {
            usage: Usage {
                input_tokens: 1,
                ..Usage::default()
            },
            finish_reason: Some(FinishReason::ToolUse),
        },
    ];
    for ev in &empties {
        assert_eq!(
            map_llm_event(&ctx(), ev),
            Vec::<PendingEvent>::new(),
            "expected no PendingEvents for {ev:?}"
        );
    }
}

// ── usage_to_model_usage ────────────────────────────────────────────────────

#[test]
fn usage_total_zero_falls_back_to_computed_total() {
    let u = Usage {
        input_tokens: 100,
        cached_input_tokens: 40,
        cache_creation_input_tokens: 0,
        output_tokens: 20,
        reasoning_output_tokens: 5,
        total_tokens: 0, // provider didn't report an inclusive total
    };
    let mu = usage_to_model_usage(&u);
    assert_eq!(
        mu,
        ModelUsage {
            input_tokens: Some(100),
            input_cached_tokens: Some(40),
            output_tokens: Some(20),
            reasoning_output_tokens: Some(5),
            // computed_total() = input + output + reasoning = 125 (excludes cached).
            total_tokens: Some(125),
            ..Default::default()
        }
    );
}

#[test]
fn usage_total_nonzero_is_preserved() {
    let u = Usage {
        input_tokens: 100,
        cached_input_tokens: 40,
        cache_creation_input_tokens: 12,
        output_tokens: 20,
        reasoning_output_tokens: 5,
        total_tokens: 250, // explicit total wins over computed_total
    };
    let mu = usage_to_model_usage(&u);
    assert_eq!(mu.total_tokens, Some(250));
    assert_eq!(mu.input_tokens, Some(100));
    assert_eq!(mu.input_cached_tokens, Some(40));
    assert_eq!(mu.output_tokens, Some(20));
    assert_eq!(mu.reasoning_output_tokens, Some(5));
    assert_eq!(mu.input_cache_creation_tokens, Some(12));
    // Cost fields are unknown at this layer.
    assert_eq!(mu.cost_usd, None);
    assert_eq!(mu.cost_source, None);
}

// ── token_count_payload shape ───────────────────────────────────────────────

#[test]
fn token_count_payload_adds_prev_total_field_wise() {
    let mu = ModelUsage {
        input_tokens: Some(30),
        input_cached_tokens: Some(2),
        output_tokens: Some(40),
        reasoning_output_tokens: Some(10),
        total_tokens: Some(80),
        ..Default::default()
    };
    // prev_total is the previous cumulative usage object; it is summed field-wise.
    let prev = json!({
        "input_tokens": 100,
        "cached_input_tokens": 8,
        "output_tokens": 200,
        "reasoning_output_tokens": 20,
        "total_tokens": 320,
    });
    let payload = token_count_payload(&mu, &prev, Some(8192), 7);
    assert_eq!(
        payload,
        json!({
            "info": {
                "total_token_usage": {
                    "input_tokens": 130,
                    "cached_input_tokens": 10,
                    "output_tokens": 240,
                    "reasoning_output_tokens": 30,
                    "total_tokens": 400,
                },
                "last_token_usage": {
                    "input_tokens": 30,
                    "cached_input_tokens": 2,
                    "output_tokens": 40,
                    "reasoning_output_tokens": 10,
                    "total_tokens": 80,
                },
                "model_context_window": 8192,
            },
            "turn_idx": 7,
        })
    );
}

#[test]
fn token_count_payload_null_prev_total_equals_last() {
    let mu = ModelUsage {
        input_tokens: Some(7),
        output_tokens: Some(3),
        total_tokens: Some(10),
        ..Default::default()
    };
    let payload = token_count_payload(&mu, &Value::Null, None, 0);
    let expected_usage = json!({
        "input_tokens": 7,
        "cached_input_tokens": 0,
        "output_tokens": 3,
        "reasoning_output_tokens": 0,
        "total_tokens": 10,
    });
    assert_eq!(
        payload,
        json!({
            "info": {
                "total_token_usage": expected_usage,
                "last_token_usage": expected_usage,
                "model_context_window": Value::Null,
            },
            "turn_idx": 0,
        })
    );
}

#[test]
fn token_count_payload_missing_total_uses_breakdown_sum() {
    // total_tokens == None -> last_token_usage.total_tokens = input+output+reasoning.
    let mu = ModelUsage {
        input_tokens: Some(100),
        input_cached_tokens: Some(40),
        output_tokens: Some(20),
        reasoning_output_tokens: Some(5),
        total_tokens: None,
        ..Default::default()
    };
    let payload = token_count_payload(&mu, &Value::Null, None, 1);
    assert_eq!(
        payload["info"]["last_token_usage"]["total_tokens"],
        json!(125)
    );
}

// ── session_done_payload shape ──────────────────────────────────────────────

#[test]
fn session_done_payload_empty_when_no_inputs() {
    assert_eq!(session_done_payload(None, None), json!({}));
}

#[test]
fn session_done_payload_result_only() {
    assert_eq!(
        session_done_payload(Some("done"), None),
        json!({ "result": "done" })
    );
}

#[test]
fn session_done_payload_with_partial_result_file() {
    let rf = ResultFilePtr {
        url: Some("https://x/y".to_string()),
        path: None,
        bytes: Some(123),
    };
    assert_eq!(
        session_done_payload(Some("done"), Some(&rf)),
        json!({
            "result": "done",
            "result_file": { "url": "https://x/y", "bytes": 123 },
        })
    );
}

// ── TeeSink fan-out ─────────────────────────────────────────────────────────

/// Minimal `EventSink` that records everything it receives.
struct RecordingSink(Mutex<Vec<PendingEvent>>);

impl RecordingSink {
    fn new() -> Self {
        Self(Mutex::new(Vec::new()))
    }
    fn events(&self) -> Vec<PendingEvent> {
        self.0.lock().unwrap().clone()
    }
}

impl EventSink for RecordingSink {
    fn emit(&self, ev: PendingEvent) {
        self.0.lock().unwrap().push(ev);
    }
}

#[test]
fn tee_sink_fans_out_to_every_sink() {
    let a = Arc::new(RecordingSink::new());
    let b = Arc::new(RecordingSink::new());
    let tee = TeeSink(vec![
        a.clone() as Arc<dyn EventSink>,
        b.clone() as Arc<dyn EventSink>,
    ]);

    let ev1 = PendingEvent::new("s", names::MODEL_STREAM_DELTA, json!({ "delta": "x" }));
    let ev2 = PendingEvent::new("s", names::STREAM_ERROR, json!({ "message": "e" }));
    tee.emit(ev1.clone());
    tee.emit(ev2.clone());

    let expected = vec![ev1, ev2];
    assert_eq!(a.events(), expected);
    assert_eq!(b.events(), expected);
}

#[test]
fn tee_sink_with_no_sinks_is_a_noop() {
    let tee = TeeSink(vec![]);
    // Must not panic with zero downstream sinks.
    tee.emit(PendingEvent::new("s", names::TASK_COMPLETE, json!({})));
}
