//! Persistence hooks: recording Python/browser worker outputs into the `Store`.
//!
//! Ported faithfully from `browser-use-core`'s `persistence.rs`
//! (`crates/browser-use-core/src/persistence.rs`). These are the `record_*`
//! hooks the engine calls as a Python/browser tool produces output: they append
//! `tool.output` / `tool.image` / `artifact.created` events to the [`Store`] and
//! register artifacts.
//!
//! ## Helpers ported alongside
//!
//! Core's `record_python_response_events_inner` spills oversized text outputs to
//! an artifact via `write_tool_output_artifact`, `approx_token_count`, and
//! `tools::command::codex_formatted_truncate_text` (which delegates to
//! `truncate_for_context`). None of those exist in this crate yet, so the leaf
//! self-contained versions are ported here, matching the worktree core bodies:
//! - [`approx_token_count`] == core `approx_token_count`
//!   (`crates/browser-use-core/src/lib.rs:5176`): `len.div_ceil(4).max(1)`.
//! - [`codex_formatted_truncate_text`] == core `truncate_for_context`
//!   (`crates/browser-use-core/src/lib.rs:5165`): char budget = `tokens*4`.
//! - [`write_tool_output_artifact`] mirrors core's helper but writes under
//!   `state_dir/artifacts/<session_id>/` (the new `Store` does not expose an
//!   `artifacts_dir` accessor; `state_dir` is the stable public surface).
//!
//! The `DEFAULT_TOOL_OUTPUT_TEXT_TOKENS` budget is reused from this crate's
//! [`crate::events::names`] (== core's constant of the same name).

use base64::{engine::general_purpose, Engine as _};
use browser_use_browser::{BrowserCommandOutput, BrowserScriptOutput};
use browser_use_python_worker::{PythonWorkerEvent, RunPythonResponse};
use browser_use_store::Store;
use serde_json::Value;

use crate::events::names::DEFAULT_TOOL_OUTPUT_TEXT_TOKENS;

/// Record all host-side events for a Python tool response, then the final
/// `tool.output` event.
///
/// Mirrors `browser-use-core::persistence::record_python_response_events`
/// (`crates/browser-use-core/src/persistence.rs:14`).
pub fn record_python_response_events(
    store: &Store,
    session_id: &str,
    response: &RunPythonResponse,
) -> anyhow::Result<Option<Value>> {
    record_python_response_events_inner(
        store,
        session_id,
        response,
        true,
        DEFAULT_TOOL_OUTPUT_TEXT_TOKENS,
    )
}

/// Record only the final `tool.output` event for a Python tool response.
///
/// Mirrors `browser-use-core::persistence::record_python_response_final_event`
/// (`crates/browser-use-core/src/persistence.rs:28`).
pub fn record_python_response_final_event(
    store: &Store,
    session_id: &str,
    response: &RunPythonResponse,
) -> anyhow::Result<Option<Value>> {
    record_python_response_final_event_with_budget(
        store,
        session_id,
        response,
        DEFAULT_TOOL_OUTPUT_TEXT_TOKENS,
    )
}

/// Record the final `tool.output` event with an explicit token budget.
///
/// Mirrors
/// `browser-use-core::persistence::record_python_response_final_event_with_budget`
/// (`crates/browser-use-core/src/persistence.rs:41`).
pub(crate) fn record_python_response_final_event_with_budget(
    store: &Store,
    session_id: &str,
    response: &RunPythonResponse,
    tool_output_token_budget: usize,
) -> anyhow::Result<Option<Value>> {
    record_python_response_events_inner(
        store,
        session_id,
        response,
        false,
        tool_output_token_budget,
    )
}

/// Record all events for a `browser_script` tool response.
///
/// Mirrors
/// `browser-use-core::persistence::record_browser_script_response_events`
/// (`crates/browser-use-core/src/persistence.rs:56`).
pub fn record_browser_script_response_events(
    store: &Store,
    session_id: &str,
    tool_call_id: &str,
    response: &BrowserScriptOutput,
) -> anyhow::Result<()> {
    record_browser_script_response_events_for_tool(
        store,
        session_id,
        "browser_script",
        tool_call_id,
        response,
    )
}

/// Record all events for a browser script response using the actual advertised
/// tool name from the model call.
pub fn record_browser_script_response_events_for_tool(
    store: &Store,
    session_id: &str,
    tool_name: &str,
    tool_call_id: &str,
    response: &BrowserScriptOutput,
) -> anyhow::Result<()> {
    for browser_event in &response.browser_events {
        record_python_browser_event(store, session_id, browser_event)?;
    }
    let image_paths = response
        .images
        .iter()
        .filter_map(|image| image.get("path").and_then(Value::as_str))
        .collect::<std::collections::HashSet<_>>();
    for image in &response.images {
        record_tool_image_with_call_id(store, session_id, tool_name, Some(tool_call_id), image)?;
    }
    for artifact in &response.artifacts {
        let Some(path) = artifact.get("path").and_then(Value::as_str) else {
            continue;
        };
        if image_paths.contains(path) {
            continue;
        }
        record_tool_artifact_with_call_id(
            store,
            session_id,
            tool_name,
            Some(tool_call_id),
            artifact,
        )?;
    }
    let transcript_text = browser_script_transcript_text(response);
    let mut payload = serde_json::json!({
        "name": tool_name,
        "tool_call_id": tool_call_id,
        "ok": response.ok,
        "status": response.status,
        "run_id": response.run_id,
        "next_observe_ms": response.next_observe_ms,
        "text": transcript_text,
        "data": response.data,
        "outputs": response.outputs,
        "summary": response.summary,
        "images": response.images,
        "artifacts": response.artifacts,
        "diagnosis": response.diagnosis,
    });
    if response.ok {
        if let Some(content) = browser_script_output_content_parts(response, &transcript_text) {
            payload["content"] = content;
        }
        payload["error"] = serde_json::to_value(response.error.clone()).unwrap_or(Value::Null);
        store.append_event(session_id, "tool.output", payload)?;
    } else {
        let error = response
            .error
            .as_deref()
            .filter(|error| !error.trim().is_empty())
            .or_else(|| (!transcript_text.trim().is_empty()).then_some(transcript_text.as_str()))
            .unwrap_or("browser_script failed");
        let failed_text = format!("{tool_name} failed: {error}");
        if let Some(content) = browser_script_output_content_parts(response, &failed_text) {
            payload["content"] = content;
        }
        payload["error"] = Value::String(error.to_string());
        store.append_event(session_id, "tool.failed", payload)?;
    }
    Ok(())
}

fn browser_script_output_content_parts(
    response: &BrowserScriptOutput,
    text: &str,
) -> Option<Value> {
    if response.images.is_empty() {
        return None;
    }
    let mut image_parts = Vec::new();
    let mut warnings = Vec::new();
    for image in &response.images {
        let Some(path) = image.get("path").and_then(Value::as_str) else {
            continue;
        };
        let bytes = match std::fs::read(path) {
            Ok(bytes) => bytes,
            Err(error) => {
                warnings.push(format!(
                    "Warning: image artifact could not be read: {path} ({error})"
                ));
                continue;
            }
        };
        let mime_type = image
            .get("mime_type")
            .or_else(|| image.get("mime"))
            .and_then(Value::as_str)
            .unwrap_or("image/png");
        if !mime_type.starts_with("image/") {
            continue;
        };
        let mut part = serde_json::json!({
            "type": "input_image",
            "image_url": format!(
                "data:{mime_type};base64,{}",
                general_purpose::STANDARD.encode(bytes)
            ),
        });
        if let Some(detail) = image.get("detail").and_then(Value::as_str) {
            part["detail"] = Value::String(detail.to_string());
        }
        image_parts.push(part);
    }
    if image_parts.is_empty() && warnings.is_empty() {
        return None;
    }
    let text = append_browser_script_image_warnings(text.to_string(), &warnings);
    let mut parts = Vec::new();
    if !text.trim().is_empty() {
        parts.push(serde_json::json!({ "type": "input_text", "text": text }));
    }
    parts.extend(image_parts);
    Some(Value::Array(parts))
}

fn append_browser_script_image_warnings(mut text: String, warnings: &[String]) -> String {
    for warning in warnings {
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str(warning);
    }
    text
}

/// Record browser events emitted by a lifecycle/command browser call. The
/// command's model-facing output is still handled by the generic tool-result
/// event path; this preserves browser state/live-url events for replay/TUI.
pub fn record_browser_command_response_events(
    store: &Store,
    session_id: &str,
    _tool_name: &str,
    _tool_call_id: &str,
    response: &BrowserCommandOutput,
) -> anyhow::Result<()> {
    for browser_event in &response.events {
        record_python_browser_event(store, session_id, browser_event)?;
    }
    Ok(())
}

/// Strip transport/progress lines from a browser_script transcript.
///
/// Mirrors `browser-use-core::persistence::browser_script_transcript_text`
/// (`crates/browser-use-core/src/persistence.rs:118`).
fn browser_script_transcript_text(response: &BrowserScriptOutput) -> String {
    if response.status.as_deref() == Some("running") {
        return browser_script_running_transcript_text(response);
    }
    if response.text.trim().is_empty() {
        return String::new();
    }
    response
        .text
        .lines()
        .map(str::trim_end)
        .filter(|line| !line.trim().is_empty())
        .filter(|line| !is_browser_script_transport_line(line))
        .collect::<Vec<_>>()
        .join("\n")
}

fn browser_script_running_transcript_text(response: &BrowserScriptOutput) -> String {
    let mut lines = response
        .text
        .lines()
        .map(str::trim_end)
        .filter(|line| !line.trim().is_empty())
        .map(str::to_string)
        .collect::<Vec<_>>();
    if lines.is_empty() {
        lines.push("browser_script is still running.".to_string());
    }
    if let Some(run_id) = response.run_id.as_deref() {
        if !lines
            .iter()
            .any(|line| line.trim_start().starts_with("run_id:"))
        {
            lines.push(format!("run_id: {run_id}"));
        }
        if !lines.iter().any(|line| {
            let line = line.trim_start();
            line.starts_with("Next:") || line.starts_with("Next step:")
        }) {
            lines.push(format!(
				"Next step: call browser_script with action=\"observe\", run_id=\"{run_id}\", and observe_timeout_ms={}.",
				response.next_observe_ms.unwrap_or(30_000)
			));
        }
    }
    lines.join("\n")
}

/// Whether a transcript line is a transport/progress marker to be dropped.
///
/// Mirrors `browser-use-core::persistence::is_browser_script_transport_line`
/// (`crates/browser-use-core/src/persistence.rs:132`).
fn is_browser_script_transport_line(line: &str) -> bool {
    let line = line.trim_start();
    line == "browser_script is still running."
        || line.starts_with("run_id:")
        || line.starts_with("Next:")
        || line.starts_with("Next step:")
}

/// Dispatch a streamed Python worker event to the right recorder.
///
/// Mirrors `browser-use-core::persistence::record_python_worker_event`
/// (`crates/browser-use-core/src/persistence.rs:140`).
pub fn record_python_worker_event(
    store: &Store,
    session_id: &str,
    event: &PythonWorkerEvent,
) -> anyhow::Result<()> {
    match event.event.as_str() {
        "output" => record_python_output(store, session_id, &event.payload),
        "browser" => record_python_browser_event(store, session_id, &event.payload),
        "image" => record_python_image(store, session_id, &event.payload),
        "artifact" => record_python_artifact(store, session_id, &event.payload),
        _ => Ok(()),
    }
}

/// Shared core for the two `record_python_response_*` entry points.
///
/// Mirrors `browser-use-core::persistence::record_python_response_events_inner`
/// (`crates/browser-use-core/src/persistence.rs:154`).
fn record_python_response_events_inner(
    store: &Store,
    session_id: &str,
    response: &RunPythonResponse,
    include_host_records: bool,
    tool_output_token_budget: usize,
) -> anyhow::Result<Option<Value>> {
    if include_host_records {
        for output in &response.outputs {
            record_python_output(store, session_id, output)?;
        }

        for browser_event in &response.browser_events {
            record_python_browser_event(store, session_id, browser_event)?;
        }

        let image_paths = response
            .images
            .iter()
            .filter_map(|image| image.get("path").and_then(Value::as_str))
            .collect::<std::collections::HashSet<_>>();
        for image in &response.images {
            record_python_image(store, session_id, image)?;
        }

        for artifact in &response.artifacts {
            let Some(path) = artifact.get("path").and_then(Value::as_str) else {
                continue;
            };
            if image_paths.contains(path) {
                continue;
            }
            record_python_artifact(store, session_id, artifact)?;
        }
    }

    let (text, text_artifact) =
        spill_large_text_output(store, session_id, &response.text, tool_output_token_budget)?;
    let mut payload = serde_json::json!({
        "name": "python",
        "ok": response.ok,
        "text": text,
        "data": response.data,
        "images": response.images,
        "artifacts": response.artifacts,
        "browser_harness_available": response.browser_harness_available,
        "browser_harness_error": response.browser_harness_error,
    });
    if let Some(artifact) = text_artifact.as_ref() {
        payload["text_truncated"] = Value::Bool(true);
        payload["text_artifact"] = artifact.clone();
    }
    store.append_event(session_id, "tool.output", payload)?;
    Ok(text_artifact)
}

/// Spill an oversized text output to an artifact and return the truncated text.
///
/// Mirrors `browser-use-core::persistence::spill_large_text_output`
/// (`crates/browser-use-core/src/persistence.rs:210`).
fn spill_large_text_output(
    store: &Store,
    session_id: &str,
    text: &str,
    tool_output_token_budget: usize,
) -> anyhow::Result<(String, Option<Value>)> {
    if approx_token_count(text) <= tool_output_token_budget {
        return Ok((text.to_string(), None));
    }
    let artifact = write_tool_output_artifact(
        store,
        session_id,
        "python",
        None,
        text,
        tool_output_token_budget,
    )?;
    Ok((
        codex_formatted_truncate_text(text, tool_output_token_budget),
        Some(artifact),
    ))
}

/// Record a streamed Python stdout/stderr chunk.
///
/// Mirrors `browser-use-core::persistence::record_python_output`
/// (`crates/browser-use-core/src/persistence.rs:233`).
fn record_python_output(store: &Store, session_id: &str, output: &Value) -> anyhow::Result<()> {
    store.append_event(
        session_id,
        "tool.output",
        serde_json::json!({
            "name": "python",
            "stream": true,
            "text": output.get("text").and_then(Value::as_str).unwrap_or_default(),
        }),
    )?;
    Ok(())
}

/// Re-emit a browser event produced inside a Python tool run.
///
/// Mirrors `browser-use-core::persistence::record_python_browser_event`
/// (`crates/browser-use-core/src/persistence.rs:246`).
pub(crate) fn record_python_browser_event(
    store: &Store,
    session_id: &str,
    browser_event: &Value,
) -> anyhow::Result<()> {
    if let Some(event_type) = browser_event.get("type").and_then(Value::as_str) {
        let payload = browser_event
            .get("payload")
            .cloned()
            .unwrap_or_else(|| Value::Object(Default::default()));
        store.append_event(session_id, event_type, payload)?;
    }
    Ok(())
}

/// Record an image produced by a Python tool run.
///
/// Mirrors `browser-use-core::persistence::record_python_image`
/// (`crates/browser-use-core/src/persistence.rs:261`).
fn record_python_image(store: &Store, session_id: &str, image: &Value) -> anyhow::Result<()> {
    record_tool_image(store, session_id, "python", image)
}

/// Record a tool image without a call id.
///
/// Mirrors `browser-use-core::persistence::record_tool_image`
/// (`crates/browser-use-core/src/persistence.rs:265`).
fn record_tool_image(
    store: &Store,
    session_id: &str,
    name: &str,
    image: &Value,
) -> anyhow::Result<()> {
    record_tool_image_with_call_id(store, session_id, name, None, image)
}

/// Record a tool image (optionally associated with a tool call id) and register
/// it as an artifact when it has a path.
///
/// Mirrors `browser-use-core::persistence::record_tool_image_with_call_id`
/// (`crates/browser-use-core/src/persistence.rs:269`).
fn record_tool_image_with_call_id(
    store: &Store,
    session_id: &str,
    name: &str,
    tool_call_id: Option<&str>,
    image: &Value,
) -> anyhow::Result<()> {
    let mut payload = serde_json::json!({
        "name": name,
        "image": image,
    });
    if let Some(tool_call_id) = tool_call_id {
        payload["tool_call_id"] = Value::String(tool_call_id.to_string());
    }
    let event = store.append_event(session_id, "tool.image", payload)?;
    if let Some(path) = image.get("path").and_then(Value::as_str) {
        store.record_artifact(
            session_id,
            Some(event.seq),
            "image",
            path,
            image.get("mime_type").and_then(Value::as_str),
            image.clone(),
        )?;
    }
    Ok(())
}

/// Record an artifact produced by a Python tool run.
///
/// Mirrors `browser-use-core::persistence::record_python_artifact`
/// (`crates/browser-use-core/src/persistence.rs:297`).
fn record_python_artifact(store: &Store, session_id: &str, artifact: &Value) -> anyhow::Result<()> {
    record_tool_artifact(store, session_id, "python", artifact)
}

/// Record a tool artifact without a call id.
///
/// Mirrors `browser-use-core::persistence::record_tool_artifact`
/// (`crates/browser-use-core/src/persistence.rs:301`).
pub(crate) fn record_tool_artifact(
    store: &Store,
    session_id: &str,
    name: &str,
    artifact: &Value,
) -> anyhow::Result<()> {
    record_tool_artifact_with_call_id(store, session_id, name, None, artifact)
}

/// Record a tool artifact (optionally associated with a tool call id) and
/// register it in the [`Store`] when it has a path.
///
/// Mirrors `browser-use-core::persistence::record_tool_artifact_with_call_id`
/// (`crates/browser-use-core/src/persistence.rs:310`).
fn record_tool_artifact_with_call_id(
    store: &Store,
    session_id: &str,
    name: &str,
    tool_call_id: Option<&str>,
    artifact: &Value,
) -> anyhow::Result<()> {
    let Some(path) = artifact.get("path").and_then(Value::as_str) else {
        return Ok(());
    };
    let kind = artifact
        .get("kind")
        .and_then(Value::as_str)
        .unwrap_or("file");
    let mut payload = serde_json::json!({
        "name": name,
        "artifact": artifact,
    });
    if let Some(tool_call_id) = tool_call_id {
        payload["tool_call_id"] = Value::String(tool_call_id.to_string());
    }
    let event = store.append_event(session_id, "artifact.created", payload)?;
    store.record_artifact(
        session_id,
        Some(event.seq),
        kind,
        path,
        artifact.get("mime").and_then(Value::as_str),
        artifact.clone(),
    )?;
    Ok(())
}

/// Approximate token count of `text`.
///
/// Mirrors `browser-use-core`'s `approx_token_count`
/// (`crates/browser-use-core/src/lib.rs:5176`): `len.div_ceil(4).max(1)`.
fn approx_token_count(text: &str) -> usize {
    text.len().div_ceil(4).max(1)
}

/// Truncate `text` to a token budget, codex-formatted.
///
/// Mirrors `browser-use-core`'s `truncate_for_context`
/// (`crates/browser-use-core/src/lib.rs:5165`, the body
/// `tools::command::codex_formatted_truncate_text` delegates to): the char
/// budget is `token_budget * 4`; if the text fits it is returned verbatim,
/// otherwise the first `char_budget` characters are kept and an ellipsis line is
/// appended.
fn codex_formatted_truncate_text(text: &str, token_budget: usize) -> String {
    let char_budget = token_budget.saturating_mul(4);
    if text.len() <= char_budget {
        return text.to_string();
    }
    let mut truncated: String = text.chars().take(char_budget).collect();
    truncated.push_str("\n…\n");
    truncated
}

/// Write an oversized text output to an artifact file and return its metadata.
///
/// Mirrors `browser-use-core`'s `write_tool_output_artifact`
/// (`crates/browser-use-core/src/lib.rs:5183`). The new [`Store`] does not
/// expose an `artifacts_dir` accessor, so the file is written under
/// `state_dir/artifacts/<session_id>/`; the returned metadata shape matches the
/// legacy helper (`artifact_id`, `path`, `file_name`, `bytes`, `preview`,
/// `token_budget`).
fn write_tool_output_artifact(
    store: &Store,
    session_id: &str,
    tool_name: &str,
    _tool_call_id: Option<&str>,
    text: &str,
    token_budget: usize,
) -> anyhow::Result<Value> {
    let artifact_dir = store.state_dir().join("artifacts").join(session_id);
    std::fs::create_dir_all(&artifact_dir)?;
    let artifact_id = format!("tool-output-{tool_name}-{}", random_hex());
    let file_name = format!("{artifact_id}.txt");
    let path = artifact_dir.join(&file_name);
    std::fs::write(&path, text)?;
    let bytes = text.len() as u64;
    let preview = codex_formatted_truncate_text(text, token_budget.min(256));
    Ok(serde_json::json!({
        "artifact_id": artifact_id,
        "path": path.to_string_lossy(),
        "file_name": file_name,
        "bytes": bytes,
        "preview": preview,
        "token_budget": token_budget,
    }))
}

/// Generate a short random hex id (replaces core's `uuid::Uuid::new_v4`).
fn random_hex() -> String {
    use rand::RngCore;
    let mut bytes = [0u8; 16];
    rand::rng().fill_bytes(&mut bytes);
    let mut out = String::with_capacity(32);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use browser_use_python_worker::{PythonWorkerEvent, RunPythonResponse};
    use browser_use_store::Store;

    /// Open a tempdir-backed `Store` (same pattern as the store crate's own
    /// tests). The `TempDir` is returned so the caller keeps it alive.
    fn store() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = Store::open(dir.path()).expect("open store");
        (dir, store)
    }

    fn new_session(store: &Store) -> String {
        store
            .create_session(None, std::path::Path::new("/tmp"))
            .expect("create session")
            .id
    }

    /// `RunPythonResponse` has no `Default`; build it from JSON via its
    /// `Deserialize` impl (serde fills the `#[serde(default)]` fields).
    fn python_response(text: &str) -> RunPythonResponse {
        serde_json::from_value(serde_json::json!({
            "id": "resp-1",
            "ok": true,
            "text": text,
            "error": null,
        }))
        .expect("build RunPythonResponse")
    }

    /// `PythonWorkerEvent` has no `Default`; build it from JSON likewise.
    fn worker_event(event: &str, payload: Value) -> PythonWorkerEvent {
        serde_json::from_value(serde_json::json!({
            "id": "evt-1",
            "event": event,
            "payload": payload,
        }))
        .expect("build PythonWorkerEvent")
    }

    #[test]
    fn approx_token_count_matches_core() {
        assert_eq!(approx_token_count(""), 1);
        assert_eq!(approx_token_count("abcd"), 1);
        assert_eq!(approx_token_count("abcde"), 2);
    }

    #[test]
    fn truncate_keeps_short_text_and_clips_long_text() {
        assert_eq!(codex_formatted_truncate_text("hi", 10), "hi");
        let long = "x".repeat(100);
        let out = codex_formatted_truncate_text(&long, 5); // budget*4 = 20 chars
        assert!(out.starts_with(&"x".repeat(20)));
        assert!(out.ends_with("\n…\n"));
    }

    #[test]
    fn transport_lines_are_stripped() {
        let mut response = BrowserScriptOutput::default();
        response.text =
            "browser_script is still running.\nrun_id: 1\nNext: go\nNext step: x\nreal line\n"
                .to_string();
        let cleaned = browser_script_transcript_text(&response);
        assert_eq!(cleaned, "real line");
    }

    #[test]
    fn running_browser_script_preserves_observe_instruction() {
        let mut response = BrowserScriptOutput::default();
        response.ok = true;
        response.status = Some("running".to_string());
        response.run_id = Some("bs-123".to_string());
        response.next_observe_ms = Some(7_000);
        response.text = "browser_script is still running.".to_string();

        let text = browser_script_transcript_text(&response);

        assert!(text.contains("browser_script is still running."));
        assert!(text.contains("run_id: bs-123"));
        assert!(text.contains("action=\"observe\""));
        assert!(text.contains("observe_timeout_ms=7000"));
    }

    #[test]
    fn empty_transcript_is_empty() {
        let response = BrowserScriptOutput::default();
        assert_eq!(browser_script_transcript_text(&response), "");
    }

    #[test]
    fn record_python_response_appends_tool_output() {
        let (_dir, store) = store();
        let session = new_session(&store);
        let response = python_response("hello");
        let artifact = record_python_response_events(&store, &session, &response).unwrap();
        assert!(artifact.is_none(), "short text should not spill");
        let events = store.events_for_session(&session).unwrap();
        assert!(events.iter().any(|e| e.event_type == "tool.output"));
    }

    #[test]
    fn record_python_response_spills_large_text() {
        let (_dir, store) = store();
        let session = new_session(&store);
        // Exceed the default budget so the spill path runs.
        let response = python_response(&"y".repeat(DEFAULT_TOOL_OUTPUT_TEXT_TOKENS * 4 + 100));
        let artifact = record_python_response_events(&store, &session, &response).unwrap();
        let artifact = artifact.expect("large text should spill to an artifact");
        assert!(artifact.get("artifact_id").is_some());
        assert!(artifact.get("path").is_some());
        assert!(artifact.get("preview").is_some());
        // The tool.output event should mark the text as truncated.
        let events = store.events_for_session(&session).unwrap();
        let tool_output = events
            .iter()
            .find(|e| e.event_type == "tool.output")
            .expect("tool.output event");
        assert_eq!(tool_output.payload["text_truncated"], true);
    }

    #[test]
    fn record_browser_script_response_appends_output() {
        let (_dir, store) = store();
        let session = new_session(&store);
        let mut response = BrowserScriptOutput::default();
        response.ok = true;
        response.text = "browser_script is still running.\nclicked button".to_string();
        record_browser_script_response_events(&store, &session, "call-1", &response).unwrap();
        let events = store.events_for_session(&session).unwrap();
        let tool_output = events
            .iter()
            .find(|e| e.event_type == "tool.output")
            .expect("tool.output event");
        assert_eq!(tool_output.payload["name"], "browser_script");
        assert_eq!(tool_output.payload["tool_call_id"], "call-1");
        assert_eq!(tool_output.payload["text"], "clicked button");
    }

    #[test]
    fn record_browser_script_response_preserves_rich_payload_and_tool_name() {
        let (_dir, store) = store();
        let session = new_session(&store);
        let mut response = BrowserScriptOutput::default();
        response.ok = true;
        response.text = "browser_script is still running.\nvisible text".to_string();
        response.summary = vec![serde_json::json!({
            "kind": "page",
            "url": "https://example.com",
            "title": "Example",
        })];
        response.images = vec![serde_json::json!({
            "path": "/tmp/shot.png",
            "kind": "image",
            "mime_type": "image/png",
        })];
        response.artifacts = vec![serde_json::json!({
            "path": "/tmp/report.csv",
            "kind": "file",
            "mime": "text/csv",
        })];
        response.browser_events = vec![serde_json::json!({
            "type": "browser.state",
            "payload": {"url": "https://example.com"},
        })];

        record_browser_script_response_events_for_tool(
            &store,
            &session,
            "browser_script",
            "call-rich",
            &response,
        )
        .unwrap();

        let events = store.events_for_session(&session).unwrap();
        assert!(events.iter().any(|e| e.event_type == "browser.state"));
        assert!(events.iter().any(|e| e.event_type == "tool.image"));
        assert!(events.iter().any(|e| e.event_type == "artifact.created"));
        let output = events
            .iter()
            .find(|e| e.event_type == "tool.output")
            .expect("tool.output");
        assert_eq!(output.payload["name"], "browser_script");
        assert_eq!(output.payload["tool_call_id"], "call-rich");
        assert_eq!(output.payload["text"], "visible text");
        assert_eq!(output.payload["summary"][0]["title"], "Example");
        assert_eq!(store.artifacts_for_session(&session).unwrap().len(), 2);
    }

    #[test]
    fn record_browser_script_failure_persists_replayable_image_content() {
        let (dir, store) = store();
        let session = new_session(&store);
        let image_path = dir.path().join("failure.png");
        std::fs::write(&image_path, [0x89, b'P', b'N', b'G']).expect("write png");

        let mut response = BrowserScriptOutput::default();
        response.ok = false;
        response.error = Some("RuntimeError: failed after screenshot".to_string());
        response.images = vec![serde_json::json!({
            "path": image_path,
            "mime_type": "image/png",
            "detail": "high",
        })];

        record_browser_script_response_events_for_tool(
            &store,
            &session,
            "browser_script",
            "call-image-failed",
            &response,
        )
        .unwrap();

        let events = store.events_for_session(&session).unwrap();
        let failed = events
            .iter()
            .find(|e| e.event_type == "tool.failed")
            .expect("tool.failed");
        let content = failed.payload["content"].as_array().expect("content array");
        assert_eq!(content[0]["type"], "input_text");
        assert_eq!(content[1]["type"], "input_image");
        assert_eq!(content[1]["detail"], "high");
        assert!(content[1]["image_url"]
            .as_str()
            .is_some_and(|url| url.starts_with("data:image/png;base64,")));
    }

    #[test]
    fn record_browser_script_failure_persists_unreadable_image_warning() {
        let (dir, store) = store();
        let session = new_session(&store);
        let missing_path = dir.path().join("missing.png");

        let response = BrowserScriptOutput {
            ok: false,
            error: Some("RuntimeError: failed after screenshot".to_string()),
            images: vec![serde_json::json!({
                "path": missing_path,
                "mime_type": "image/png",
                "detail": "high",
            })],
            ..Default::default()
        };

        record_browser_script_response_events_for_tool(
            &store,
            &session,
            "browser_script",
            "call-missing-image",
            &response,
        )
        .unwrap();

        let events = store.events_for_session(&session).unwrap();
        let failed = events
            .iter()
            .find(|e| e.event_type == "tool.failed")
            .expect("tool.failed");
        let content = failed.payload["content"].as_array().expect("content array");
        assert_eq!(content.len(), 1);
        assert_eq!(content[0]["type"], "input_text");
        let text = content[0]["text"].as_str().expect("warning text");
        assert!(text.contains("browser_script failed: RuntimeError"));
        assert!(text.contains("Warning: image artifact could not be read:"));
        assert!(text.contains("missing.png"));
    }

    #[test]
    fn record_browser_script_response_failure_is_tool_failed_with_diagnosis() {
        let (_dir, store) = store();
        let session = new_session(&store);
        let mut response = BrowserScriptOutput::default();
        response.ok = false;
        response.text = "Traceback line".to_string();
        response.error = Some("RuntimeError: CDP timed out".to_string());
        response.diagnosis = Some(browser_use_browser::BrowserIssueDiagnosis {
            summary: "Browser remains usable.".to_string(),
            what_happened: "A CDP read timed out.".to_string(),
            next_step: "Observe again with a smaller script.".to_string(),
            browser_usable: true,
            page_usable: true,
            error_kind: "cdp-read-timeout".to_string(),
        });

        record_browser_script_response_events_for_tool(
            &store,
            &session,
            "browser_script",
            "call-failed",
            &response,
        )
        .unwrap();

        let events = store.events_for_session(&session).unwrap();
        assert!(!events.iter().any(|e| e.event_type == "tool.output"));
        let failed = events
            .iter()
            .find(|e| e.event_type == "tool.failed")
            .expect("tool.failed");
        assert_eq!(failed.payload["name"], "browser_script");
        assert_eq!(failed.payload["tool_call_id"], "call-failed");
        assert_eq!(failed.payload["error"], "RuntimeError: CDP timed out");
        assert_eq!(
            failed.payload["diagnosis"]["error_kind"],
            "cdp-read-timeout"
        );
    }

    #[test]
    fn record_python_worker_event_dispatches_output() {
        let (_dir, store) = store();
        let session = new_session(&store);
        let event = worker_event("output", serde_json::json!({ "text": "streamed" }));
        record_python_worker_event(&store, &session, &event).unwrap();
        let events = store.events_for_session(&session).unwrap();
        let streamed = events
            .iter()
            .find(|e| e.event_type == "tool.output")
            .expect("tool.output");
        assert_eq!(streamed.payload["stream"], true);
        assert_eq!(streamed.payload["text"], "streamed");
    }

    #[test]
    fn record_python_worker_event_ignores_unknown_kind() {
        let (_dir, store) = store();
        let session = new_session(&store);
        let event = worker_event("totally-unknown", serde_json::json!({}));
        record_python_worker_event(&store, &session, &event).unwrap();
        // No tool.output for an unknown event kind.
        let events = store.events_for_session(&session).unwrap();
        assert!(!events.iter().any(|e| e.event_type == "tool.output"));
    }

    #[test]
    fn random_hex_is_32_hex_chars() {
        let h = random_hex();
        assert_eq!(h.len(), 32);
        assert!(h.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(random_hex(), random_hex());
    }
}
