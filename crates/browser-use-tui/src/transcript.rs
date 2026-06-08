use browser_use_protocol::{
    normalize_result_text, turn_streaming_text_from_events, EventRecord, SessionMeta,
    SessionStatus, WorkbenchState,
};
use browser_use_store::now_ms;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use std::cell::RefCell;
use std::path::{Path, PathBuf};
use unicode_width::UnicodeWidthChar;

use crate::markdown::render_markdown_lines;
use crate::theme::{
    accent, activity_group, activity_list, activity_read, activity_run, activity_search,
    activity_task, dim, failed, link, muted, path_reference, text_style, thought,
    user_prompt_accent, user_prompt_muted, user_prompt_text,
};

use super::{
    active_followup_is_after_next_tool_call, active_followup_is_cancelled_in_events,
    active_followup_is_pending_in_events, user_input_display_text_from_payload, App,
    LOCAL_CHROME_CLOUD_PROMO_EVENT, PENDING_FOLLOWUP_INTERRUPT_REASON,
    SESSION_MAILBOX_CONTINUATION_STARTED_EVENT, SESSION_PAUSED_REASON,
    SESSION_PENDING_ACTIVE_FOLLOWUP_EVENT, SESSION_QUEUED_FOLLOWUP_EVENT,
};

const GROUP_VALUE_RAIL_PREFIX: &str = "  │ ";
const GROUP_VALUE_LAST_PREFIX: &str = "  └ ";
const COMMAND_LINE_PREFIX: &str = "ran command: ";
const SHELL_SCRIPT_LINE_PREFIX: &str = "ran shell script";
const COMMAND_DISPLAY_MAX_ROWS: usize = 2;
const SHELL_GROUP_VISIBLE_VALUES: usize = 2;
const ACTIVE_FALLBACK_STATUS: &str = "running browser task";
const LIVE_STREAM_QUIET_STATUS_DELAY_MS: i64 = 500;
const SESSION_PAUSED_TITLE: &str = "Conversation paused";
const SESSION_PAUSED_TEXT: &str =
    "What should the model do differently? If something went wrong, please use /feedback :)";
/// Mirror of the agent crate's `pub(crate)` rollback event type. Used only to
/// decide whether the rollback-filtered event buffer can be extended in place
/// (append-only, no rollback) or must be rebuilt from scratch.
const SESSION_ROLLBACK_EVENT_TYPE: &str = "session.rollback";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DisplayMode {
    Scrollback,
    Active,
}

#[derive(Clone, Debug)]
pub(crate) struct TranscriptModel {
    pub(crate) session_id: String,
    pub(crate) committed: Vec<TranscriptNode>,
    terminal_committed: Vec<TranscriptNode>,
    pub(crate) active: Option<TranscriptNode>,
    pub(crate) last_event_seq: i64,
    live_phase: usize,
}

#[cfg(test)]
pub(crate) struct TerminalScrollbackEmission {
    pub(crate) lines: Vec<Line<'static>>,
    pub(crate) last_seq: i64,
}

pub(crate) struct TerminalNativeScrollbackEmission {
    pub(crate) lines: Vec<NativeLine>,
    pub(crate) last_seq: i64,
}

#[derive(Default)]
pub(crate) struct TranscriptModelCache {
    entry: RefCell<Option<CachedTranscriptModel>>,
}

struct CachedTranscriptModel {
    key: TranscriptModelCacheKey,
    model: TranscriptModel,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct TranscriptModelCacheKey {
    session_id: String,
    session_status: SessionStatus,
    state_revision: u64,
    event_count: usize,
    last_event_seq: i64,
    native_scrollback_active: bool,
    native_history_last_seq: i64,
}

impl TranscriptModelCache {
    fn with_model<R>(
        &self,
        key: TranscriptModelCacheKey,
        live_phase: usize,
        build: impl FnOnce() -> TranscriptModel,
        read: impl FnOnce(&TranscriptModel) -> R,
    ) -> R {
        {
            let mut cached = self.entry.borrow_mut();
            if let Some(entry) = cached.as_mut().filter(|entry| entry.key == key) {
                entry.model.live_phase = live_phase;
                return read(&entry.model);
            }
        }

        let model = build();
        let mut cached = self.entry.borrow_mut();
        *cached = Some(CachedTranscriptModel { key, model });
        let entry = cached.as_ref().expect("transcript cache populated");
        read(&entry.model)
    }
}

/// Caches the rollback-filtered, owned event buffer for the current session.
///
/// `build_transcript_model` used to clone *every* event in the session on every
/// frame (`rollback_filtered_event_records(..).cloned().collect()`). During a
/// streaming session the model cache misses each frame (new tokens bump the
/// event count), so that O(events) deep clone ran on every keystroke and grew
/// with session length — the dominant typing-latency cost.
///
/// Rollback filtering of an append-only event log is itself append-only as long
/// as no new event is a rollback: the filtered list for `raw[..n+k]` is the
/// filtered list for `raw[..n]` plus the new non-rollback events. So when the
/// raw log only grew (same prefix, no rollback in the new tail) we clone just
/// the new events and extend the cached buffer; otherwise we rebuild it.
#[derive(Default)]
pub(crate) struct FilteredEventCache {
    entry: RefCell<Option<FilteredEventEntry>>,
}

struct FilteredEventEntry {
    session_id: String,
    raw_len: usize,
    raw_last_seq: i64,
    filtered: Vec<EventRecord>,
}

impl FilteredEventCache {
    /// Refresh the cached buffer for `session_id`/`raw` and run `read` against
    /// the rollback-filtered events. Equivalent to passing
    /// `rollback_filtered_event_records(raw).cloned().collect()` but reusing the
    /// previous buffer when the log only grew without a rollback.
    fn with_filtered<R>(
        &self,
        session_id: &str,
        raw: &[EventRecord],
        read: impl FnOnce(&[EventRecord]) -> R,
    ) -> R {
        self.refresh(session_id, raw);
        let entry = self.entry.borrow();
        read(&entry.as_ref().expect("filtered cache populated").filtered)
    }

    fn refresh(&self, session_id: &str, raw: &[EventRecord]) {
        let mut slot = self.entry.borrow_mut();
        let can_extend = slot.as_ref().is_some_and(|entry| {
            entry.session_id == session_id
                && entry.raw_len > 0
                && raw.len() >= entry.raw_len
                // The event at the previous boundary is unchanged (the log is
                // append-only with unique, increasing seq), so the existing
                // filtered prefix still holds.
                && raw.get(entry.raw_len - 1).map(|event| event.seq) == Some(entry.raw_last_seq)
                // No rollback in the new tail — otherwise the filtered prefix
                // would be truncated and the in-place extend would be wrong.
                && raw[entry.raw_len..]
                    .iter()
                    .all(|event| event.event_type != SESSION_ROLLBACK_EVENT_TYPE)
        });

        if can_extend {
            let entry = slot.as_mut().expect("can_extend implies populated");
            if raw.len() > entry.raw_len {
                entry.filtered.extend(raw[entry.raw_len..].iter().cloned());
                entry.raw_len = raw.len();
                entry.raw_last_seq = raw
                    .last()
                    .map(|event| event.seq)
                    .unwrap_or(entry.raw_last_seq);
            }
            return;
        }

        let filtered =
            browser_use_agent::context::workspace_context::rollback_filtered_event_records(raw)
                .into_iter()
                .cloned()
                .collect::<Vec<_>>();
        *slot = Some(FilteredEventEntry {
            session_id: session_id.to_string(),
            raw_len: raw.len(),
            raw_last_seq: raw.last().map(|event| event.seq).unwrap_or_default(),
            filtered,
        });
    }
}

#[derive(Clone, Debug)]
pub(crate) struct TranscriptNode {
    id: String,
    seq: i64,
    revision: u64,
    kind: TranscriptKind,
}

#[derive(Clone, Debug)]
enum TranscriptKind {
    Stack {
        nodes: Vec<TranscriptNode>,
    },
    Prompt {
        text: String,
        followup: bool,
    },
    PendingStatus {
        status: String,
        detail: Option<String>,
    },
    Assistant {
        markdown: String,
        source: Option<String>,
    },
    Notice {
        text: String,
    },
    StreamingAssistant {
        markdown: String,
    },
    ResultFile {
        file_path: String,
        bytes: Option<u64>,
        mime: Option<String>,
        source: Option<String>,
    },
    Timeline {
        group: String,
        lines: Vec<String>,
        style: NodeStyle,
    },
    ToolImage {
        path: Option<String>,
        label: Option<String>,
        took_screenshot: bool,
    },
    ActiveStatus {
        group: String,
        lines: Vec<String>,
        style: NodeStyle,
    },
    Error {
        lines: Vec<String>,
    },
    Cancelled {
        title: String,
        text: String,
        style: NodeStyle,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum NodeStyle {
    Normal,
    Muted,
    Failed,
    Thought,
}

impl TranscriptNode {
    fn id(&self) -> &str {
        &self.id
    }

    fn seq(&self) -> i64 {
        self.seq
    }

    fn revision(&self) -> u64 {
        self.revision
    }

    fn display_lines(&self, width: u16, mode: DisplayMode) -> Vec<Line<'static>> {
        let width = width.max(1);
        match &self.kind {
            TranscriptKind::Stack { nodes } => cells_to_lines(nodes.iter(), width, mode),
            TranscriptKind::Prompt { text, followup } => prompt_lines(text, *followup, width),
            TranscriptKind::PendingStatus { status, detail } => {
                pending_status_lines(status, detail.as_deref(), ShimmerMode::Static)
            }
            TranscriptKind::Assistant { markdown, source } => {
                let mut lines = markdown_cell_lines(markdown, width, mode);
                if let Some(source) = source.as_deref() {
                    lines.extend(source_display_lines(source, width));
                }
                lines
            }
            TranscriptKind::Notice { text } => notice_lines(text, width),
            TranscriptKind::StreamingAssistant { markdown } => {
                markdown_cell_lines(markdown, width, mode)
            }
            TranscriptKind::ResultFile {
                file_path,
                bytes,
                mime,
                source,
            } => {
                let mut lines = result_file_lines(file_path, *bytes, mime.as_deref(), width);
                if let Some(source) = source.as_deref() {
                    lines.extend(source_display_lines(source, width));
                }
                lines
            }
            TranscriptKind::Timeline {
                group,
                lines,
                style,
            }
            | TranscriptKind::ActiveStatus {
                group,
                lines,
                style,
            } => grouped_lines(group, lines, *style, width),
            TranscriptKind::ToolImage {
                path,
                label,
                took_screenshot,
            } => {
                tool_image_display_lines(path.as_deref(), label.as_deref(), *took_screenshot, width)
            }
            TranscriptKind::Error { lines } => {
                grouped_lines("error", lines, NodeStyle::Failed, width)
            }
            TranscriptKind::Cancelled { title, text, style } => {
                grouped_lines(title, std::slice::from_ref(text), *style, width)
            }
        }
    }

    fn native_display_lines(&self, width: u16, mode: DisplayMode) -> Vec<NativeLine> {
        let width = width.max(1);
        match &self.kind {
            TranscriptKind::Stack { nodes } => cells_to_native_lines(nodes.iter(), width, mode),
            TranscriptKind::Prompt { text, followup } => {
                plain_native_lines(prompt_lines(text, *followup, width))
            }
            TranscriptKind::PendingStatus { status, detail } => plain_native_lines(
                pending_status_lines(status, detail.as_deref(), ShimmerMode::Static),
            ),
            TranscriptKind::Assistant { markdown, source } => {
                let mut lines = native_markdown_cell_lines(markdown, width, mode);
                if let Some(source) = source.as_deref() {
                    lines.extend(native_source_display_lines(source, width));
                }
                lines
            }
            TranscriptKind::Notice { text } => plain_native_lines(notice_lines(text, width)),
            TranscriptKind::StreamingAssistant { markdown } => {
                native_markdown_cell_lines(markdown, width, mode)
            }
            TranscriptKind::ResultFile {
                file_path,
                bytes,
                mime,
                source,
            } => {
                let mut lines = native_result_file_lines(file_path, *bytes, mime.as_deref(), width);
                if let Some(source) = source.as_deref() {
                    lines.extend(native_source_display_lines(source, width));
                }
                lines
            }
            TranscriptKind::Timeline {
                group,
                lines,
                style,
            }
            | TranscriptKind::ActiveStatus {
                group,
                lines,
                style,
            } => native_grouped_lines(group, lines, *style, width),
            TranscriptKind::ToolImage {
                path,
                label,
                took_screenshot,
            } => native_tool_image_display_lines(
                path.as_deref(),
                label.as_deref(),
                *took_screenshot,
                width,
            ),
            TranscriptKind::Error { lines } => {
                native_grouped_lines("error", lines, NodeStyle::Failed, width)
            }
            TranscriptKind::Cancelled { title, text, style } => {
                native_grouped_lines(title, std::slice::from_ref(text), *style, width)
            }
        }
    }

    fn plain_lines(&self) -> Vec<String> {
        match &self.kind {
            TranscriptKind::Stack { nodes } => {
                nodes.iter().flat_map(|node| node.plain_lines()).collect()
            }
            TranscriptKind::Prompt { text, .. } => prefixed_plain("> ", text),
            TranscriptKind::PendingStatus { status, detail } => {
                vec![pending_status_text(status, detail.as_deref())]
            }
            TranscriptKind::Assistant { markdown, source } => {
                let mut out = markdown.lines().map(str::to_string).collect::<Vec<_>>();
                if let Some(source) = source.as_ref() {
                    out.push(format!("source {source}"));
                }
                out
            }
            TranscriptKind::Notice { text } => text.lines().map(str::to_string).collect(),
            TranscriptKind::StreamingAssistant { markdown } => {
                markdown.lines().map(str::to_string).collect()
            }
            TranscriptKind::ResultFile {
                file_path,
                bytes,
                mime,
                source,
            } => {
                let mut out = result_file_plain_lines(file_path, *bytes, mime.as_deref());
                if let Some(source) = source.as_ref() {
                    out.push(format!("source {source}"));
                }
                out
            }
            TranscriptKind::Timeline { group, lines, .. }
            | TranscriptKind::ActiveStatus { group, lines, .. } => {
                let mut out = vec![format!("• {group}")];
                let last_idx = lines.len().saturating_sub(1);
                out.extend(lines.iter().enumerate().map(|(idx, line)| {
                    let prefix = if idx == last_idx {
                        GROUP_VALUE_LAST_PREFIX
                    } else {
                        GROUP_VALUE_RAIL_PREFIX
                    };
                    format!("{prefix}{line}")
                }));
                out
            }
            TranscriptKind::ToolImage {
                path,
                label,
                took_screenshot,
            } => tool_image_plain_lines(path.as_deref(), label.as_deref(), *took_screenshot),
            TranscriptKind::Error { lines } => {
                let mut out = vec!["• error".to_string()];
                let last_idx = lines.len().saturating_sub(1);
                out.extend(lines.iter().enumerate().map(|(idx, line)| {
                    let prefix = if idx == last_idx {
                        GROUP_VALUE_LAST_PREFIX
                    } else {
                        GROUP_VALUE_RAIL_PREFIX
                    };
                    format!("{prefix}{line}")
                }));
                out
            }
            TranscriptKind::Cancelled { title, text, .. } => {
                vec![
                    format!("• {title}"),
                    format!("{GROUP_VALUE_LAST_PREFIX}{text}"),
                ]
            }
        }
    }

    fn is_terminal_scrollback_transient(&self) -> bool {
        match &self.kind {
            TranscriptKind::PendingStatus { .. } => true,
            TranscriptKind::Timeline { group, style, .. }
                if group == "thinking"
                    || (*style == NodeStyle::Thought && group.starts_with("thought")) =>
            {
                true
            }
            _ => false,
        }
    }

    fn is_active_viewport_placeholder(&self) -> bool {
        match &self.kind {
            TranscriptKind::ActiveStatus {
                group,
                lines,
                style,
            } => {
                group == "status"
                    && *style == NodeStyle::Muted
                    && lines.len() == 1
                    && lines[0] == ACTIVE_FALLBACK_STATUS
            }
            TranscriptKind::Stack { nodes } => nodes
                .iter()
                .all(TranscriptNode::is_active_viewport_placeholder),
            _ => false,
        }
    }

    fn has_shimmering_live_status(&self) -> bool {
        match &self.kind {
            TranscriptKind::PendingStatus { .. } => true,
            TranscriptKind::Stack { nodes } => {
                nodes.iter().any(TranscriptNode::has_shimmering_live_status)
            }
            _ => false,
        }
    }

    fn needs_leading_status_padding(&self) -> bool {
        match &self.kind {
            TranscriptKind::PendingStatus { .. } => true,
            TranscriptKind::StreamingAssistant { .. } => true,
            TranscriptKind::Stack { nodes } => nodes
                .iter()
                .find(|node| !node.is_active_viewport_placeholder())
                .is_some_and(TranscriptNode::needs_leading_status_padding),
            _ => false,
        }
    }

    fn is_prompt(&self) -> bool {
        matches!(self.kind, TranscriptKind::Prompt { .. })
    }

    fn active_display_lines(
        &self,
        width: u16,
        shimmer_phase: usize,
        stream_skip_lines: Option<&mut usize>,
        allow_empty_stream: bool,
    ) -> Vec<Line<'static>> {
        let width = width.max(1);
        match &self.kind {
            TranscriptKind::Stack { nodes } => {
                let mut out = Vec::new();
                let mut previous_kind = None;
                let mut stream_skip_lines = stream_skip_lines;
                for (idx, node) in nodes.iter().enumerate() {
                    let _ = (node.id(), node.revision());
                    let child_allow_empty_stream =
                        matches!(node.kind, TranscriptKind::StreamingAssistant { .. })
                            && nodes[idx + 1..].iter().any(|node| {
                                matches!(node.kind, TranscriptKind::PendingStatus { .. })
                            });
                    let child_lines = node.active_display_lines(
                        width,
                        shimmer_phase,
                        stream_skip_lines.as_deref_mut(),
                        child_allow_empty_stream,
                    );
                    if child_lines.is_empty() {
                        continue;
                    }
                    if !out.is_empty() {
                        let gap = previous_kind
                            .map(|previous| gap_lines_between(previous, &node.kind))
                            .unwrap_or(0);
                        out.extend(std::iter::repeat_with(|| Line::from("")).take(gap));
                    }
                    out.extend(child_lines);
                    previous_kind = Some(&node.kind);
                }
                out
            }
            TranscriptKind::PendingStatus { status, detail } => pending_status_lines(
                status,
                detail.as_deref(),
                ShimmerMode::AnimatedAt(shimmer_phase),
            ),
            TranscriptKind::ActiveStatus {
                group,
                lines,
                style,
            } => grouped_lines(group, lines, *style, width),
            TranscriptKind::StreamingAssistant { markdown } => {
                let mut lines = streaming_markdown_cell_lines(markdown, width, DisplayMode::Active);
                if let Some(stream_skip_lines) = stream_skip_lines {
                    let max_skip = if allow_empty_stream {
                        lines.len()
                    } else {
                        lines.len().saturating_sub(1)
                    };
                    let skip = (*stream_skip_lines).min(max_skip);
                    *stream_skip_lines = (*stream_skip_lines).saturating_sub(skip);
                    if skip > 0 {
                        lines = lines.into_iter().skip(skip).collect();
                    }
                }
                lines
            }
            _ => self.display_lines(width, DisplayMode::Active),
        }
    }

    fn streaming_display_lines(&self, width: u16) -> Vec<Line<'static>> {
        let width = width.max(1);
        match &self.kind {
            TranscriptKind::Stack { nodes } => nodes
                .iter()
                .flat_map(|node| node.streaming_display_lines(width))
                .collect(),
            TranscriptKind::StreamingAssistant { markdown } => {
                streaming_markdown_cell_lines(markdown, width, DisplayMode::Active)
            }
            _ => Vec::new(),
        }
    }

    fn streaming_native_display_lines(&self, width: u16) -> Vec<NativeLine> {
        let width = width.max(1);
        match &self.kind {
            TranscriptKind::Stack { nodes } => nodes
                .iter()
                .flat_map(|node| node.streaming_native_display_lines(width))
                .collect(),
            TranscriptKind::StreamingAssistant { markdown } => {
                native_markdown_stream_lines(markdown, width, DisplayMode::Active)
            }
            _ => Vec::new(),
        }
    }

    fn streaming_native_commit_prefix_lines(&self, width: u16) -> Vec<NativeLine> {
        let width = width.max(1);
        match &self.kind {
            TranscriptKind::Stack { nodes } => nodes
                .iter()
                .flat_map(|node| node.streaming_native_commit_prefix_lines(width))
                .collect(),
            TranscriptKind::StreamingAssistant { markdown } => {
                native_markdown_stable_prefix_lines(markdown, width, DisplayMode::Active)
            }
            _ => Vec::new(),
        }
    }

    fn can_commit_full_live_stream(&self) -> bool {
        match &self.kind {
            TranscriptKind::Stack { nodes } => nodes.iter().enumerate().any(|(idx, node)| {
                matches!(node.kind, TranscriptKind::StreamingAssistant { .. })
                    && nodes[idx + 1..]
                        .iter()
                        .any(|node| matches!(node.kind, TranscriptKind::PendingStatus { .. }))
            }),
            _ => false,
        }
    }

    fn streaming_ends_on_line_boundary(&self) -> bool {
        match &self.kind {
            TranscriptKind::Stack { nodes } => nodes
                .iter()
                .rev()
                .find_map(|node| match &node.kind {
                    TranscriptKind::StreamingAssistant { .. } => {
                        Some(node.streaming_ends_on_line_boundary())
                    }
                    _ => None,
                })
                .unwrap_or(false),
            TranscriptKind::StreamingAssistant { markdown } => markdown.ends_with('\n'),
            _ => false,
        }
    }

    fn has_streaming_without_pending_status(&self) -> bool {
        match &self.kind {
            TranscriptKind::Stack { nodes } => {
                let has_streaming = nodes
                    .iter()
                    .any(|node| matches!(node.kind, TranscriptKind::StreamingAssistant { .. }));
                let has_pending_status = nodes
                    .iter()
                    .any(|node| matches!(node.kind, TranscriptKind::PendingStatus { .. }));
                has_streaming && !has_pending_status
                    || nodes
                        .iter()
                        .any(TranscriptNode::has_streaming_without_pending_status)
            }
            _ => false,
        }
    }

    fn has_streaming_table_holdback(&self) -> bool {
        match &self.kind {
            TranscriptKind::Stack { nodes } => nodes
                .iter()
                .any(TranscriptNode::has_streaming_table_holdback),
            TranscriptKind::StreamingAssistant { markdown } => {
                markdown_table_holdback_state(markdown).is_some()
            }
            _ => false,
        }
    }
}

pub(crate) fn transcript_model(app: &App, state: &WorkbenchState) -> Option<TranscriptModel> {
    with_transcript_model(app, state, |model| model.clone())
}

pub(crate) fn with_transcript_model<R>(
    app: &App,
    state: &WorkbenchState,
    read: impl FnOnce(&TranscriptModel) -> R,
) -> Option<R> {
    let session = state.current_session.as_ref()?;
    let raw_events = app.cached_events_for_session(&session.id);
    let last_event_seq = raw_events.last().map(|event| event.seq).unwrap_or_default();
    let native_scrollback_active = app.native_scrollback_is_active();
    let cache_key = TranscriptModelCacheKey {
        session_id: session.id.clone(),
        session_status: session.status.clone(),
        state_revision: app.state_cache.revision,
        event_count: raw_events.len(),
        last_event_seq,
        native_scrollback_active,
        native_history_last_seq: if native_scrollback_active {
            app.native_history.last_seq
        } else {
            0
        },
    };
    Some(app.transcript_model_cache.with_model(
        cache_key,
        app.live_spinner_frame,
        || build_transcript_model(app, state, session, raw_events, last_event_seq),
        read,
    ))
}

fn build_transcript_model(
    app: &App,
    state: &WorkbenchState,
    session: &SessionMeta,
    raw_events: &[EventRecord],
    last_event_seq: i64,
) -> TranscriptModel {
    // The rollback-filtered owned event buffer is cached and extended in place
    // (see `FilteredEventCache`) instead of re-cloning every event each frame.
    app.transcript_filtered_cache
        .with_filtered(&session.id, raw_events, |events| {
            build_transcript_model_from_events(app, state, session, events, last_event_seq)
        })
}

fn build_transcript_model_from_events(
    app: &App,
    state: &WorkbenchState,
    session: &SessionMeta,
    events: &[EventRecord],
    last_event_seq: i64,
) -> TranscriptModel {
    let mut committed = Vec::new();
    let mut terminal_committed = Vec::new();
    let commit_after_seq = if app.native_scrollback_is_active() {
        app.native_history.last_seq
    } else {
        0
    };

    for event in events.iter().filter(|event| event.seq > commit_after_seq) {
        if let Some(node) = committed_node_for_event(app, state, session, events, event) {
            terminal_committed.push(node.clone());
            push_committed_node(&mut committed, node);
        }
    }

    let has_live_subagent_work = active_child_session_count(app, &session.id) > 0
        || pending_agent_mailbox_count(app, &session.id) > 0;
    let active = if app.session_events_are_waiting_for_auth(&session.id, events) {
        None
    } else if session.status.is_active() || has_live_subagent_work {
        active_node_for_session(app, state, session, events)
    } else {
        None
    };

    TranscriptModel {
        session_id: session.id.clone(),
        committed,
        terminal_committed,
        active,
        last_event_seq,
        live_phase: app.live_spinner_frame,
    }
}

pub(crate) fn all_scrollback_lines(model: &TranscriptModel, width: u16) -> Vec<Line<'static>> {
    cells_to_lines(model.committed.iter(), width, DisplayMode::Scrollback)
}

pub(crate) fn all_terminal_scrollback_lines(
    model: &TranscriptModel,
    width: u16,
) -> Vec<Line<'static>> {
    cells_to_lines(
        model
            .committed
            .iter()
            .filter(|node| !node.is_terminal_scrollback_transient()),
        width,
        DisplayMode::Scrollback,
    )
}

#[derive(Clone, Debug)]
pub(crate) struct NativeLineLink {
    pub(crate) start_col: usize,
    pub(crate) width: usize,
    pub(crate) target: String,
}

#[derive(Clone, Debug)]
pub(crate) struct NativeLine {
    pub(crate) line: Line<'static>,
    pub(crate) links: Vec<NativeLineLink>,
}

impl NativeLine {
    pub(crate) fn plain(line: Line<'static>) -> Self {
        Self {
            line,
            links: Vec::new(),
        }
    }
}

#[cfg(test)]
pub(crate) fn terminal_scrollback_emission_since(
    model: &TranscriptModel,
    after_seq: i64,
    width: u16,
    defer_open_tail: bool,
) -> TerminalScrollbackEmission {
    let mut nodes = Vec::new();
    let mut last_seq = after_seq;
    for node in model
        .terminal_committed
        .iter()
        .filter(|node| node.seq() > after_seq)
        .filter(|node| !node.is_terminal_scrollback_transient())
    {
        last_seq = node.seq();
        push_committed_node(&mut nodes, node.clone());
    }
    if defer_open_tail && nodes.last().is_some_and(is_open_timeline_node) {
        nodes.pop();
        last_seq = nodes.last().map(TranscriptNode::seq).unwrap_or(after_seq);
    }
    TerminalScrollbackEmission {
        lines: cells_to_lines(nodes.iter(), width, DisplayMode::Scrollback),
        last_seq,
    }
}

pub(crate) fn terminal_scrollback_native_emission_since(
    model: &TranscriptModel,
    after_seq: i64,
    width: u16,
    defer_open_tail: bool,
) -> TerminalNativeScrollbackEmission {
    let mut nodes = Vec::new();
    let mut last_seq = after_seq;
    for node in model
        .terminal_committed
        .iter()
        .filter(|node| node.seq() > after_seq)
        .filter(|node| !node.is_terminal_scrollback_transient())
    {
        last_seq = node.seq();
        push_committed_node(&mut nodes, node.clone());
    }
    if defer_open_tail && nodes.last().is_some_and(is_open_timeline_node) {
        nodes.pop();
        last_seq = nodes.last().map(TranscriptNode::seq).unwrap_or(after_seq);
    }
    TerminalNativeScrollbackEmission {
        lines: cells_to_native_lines(nodes.iter(), width, DisplayMode::Scrollback),
        last_seq,
    }
}

pub(crate) fn active_viewport_lines(
    model: Option<&TranscriptModel>,
    width: u16,
    height: u16,
) -> Vec<Line<'static>> {
    let Some(active) = model.and_then(|model| model.active.as_ref()) else {
        return Vec::new();
    };
    if active.is_active_viewport_placeholder() {
        return Vec::new();
    }
    let mut lines = active.active_display_lines(
        width,
        model.map(|model| model.live_phase).unwrap_or(0),
        None,
        false,
    );
    if active.needs_leading_status_padding() && !lines.is_empty() {
        lines.insert(0, Line::from(""));
    }
    if lines.len() > height as usize {
        let start = lines.len().saturating_sub(height as usize);
        lines = lines.into_iter().skip(start).collect();
    }
    lines
}

pub(crate) fn active_viewport_lines_with_stream_skip(
    model: Option<&TranscriptModel>,
    width: u16,
    height: u16,
    stream_skip_lines: usize,
) -> Vec<Line<'static>> {
    let Some(active) = model.and_then(|model| model.active.as_ref()) else {
        return Vec::new();
    };
    if active.is_active_viewport_placeholder() {
        return Vec::new();
    }
    let mut skip = stream_skip_lines;
    let allow_empty_stream = active.has_streaming_table_holdback();
    let mut lines = active.active_display_lines(
        width,
        model.map(|model| model.live_phase).unwrap_or(0),
        Some(&mut skip),
        allow_empty_stream,
    );
    let consumed_stream_lines = stream_skip_lines > skip;
    if active.needs_leading_status_padding() && !lines.is_empty() && !consumed_stream_lines {
        lines.insert(0, Line::from(""));
    }
    if lines.len() > height as usize {
        let start = lines.len().saturating_sub(height as usize);
        lines = lines.into_iter().skip(start).collect();
    }
    lines
}

pub(crate) fn active_streaming_lines(
    model: Option<&TranscriptModel>,
    width: u16,
) -> Vec<Line<'static>> {
    model
        .and_then(|model| model.active.as_ref())
        .map(|active| active.streaming_display_lines(width))
        .unwrap_or_default()
}

pub(crate) fn active_streaming_native_lines(
    model: Option<&TranscriptModel>,
    width: u16,
) -> Vec<NativeLine> {
    model
        .and_then(|model| model.active.as_ref())
        .map(|active| active.streaming_native_display_lines(width))
        .unwrap_or_default()
}

pub(crate) fn active_streaming_native_commit_prefix_lines(
    model: Option<&TranscriptModel>,
    width: u16,
) -> Vec<NativeLine> {
    model
        .and_then(|model| model.active.as_ref())
        .map(|active| active.streaming_native_commit_prefix_lines(width))
        .unwrap_or_default()
}

pub(crate) fn active_streaming_can_commit_all(model: Option<&TranscriptModel>) -> bool {
    model
        .and_then(|model| model.active.as_ref())
        .is_some_and(TranscriptNode::can_commit_full_live_stream)
}

pub(crate) fn active_streaming_ends_on_line_boundary(model: Option<&TranscriptModel>) -> bool {
    model
        .and_then(|model| model.active.as_ref())
        .is_some_and(TranscriptNode::streaming_ends_on_line_boundary)
}

pub(crate) fn active_streaming_has_table_holdback(model: Option<&TranscriptModel>) -> bool {
    model
        .and_then(|model| model.active.as_ref())
        .is_some_and(TranscriptNode::has_streaming_table_holdback)
}

#[cfg(test)]
pub(crate) fn active_viewport_has_live_content(model: Option<&TranscriptModel>) -> bool {
    model
        .and_then(|model| model.active.as_ref())
        .is_some_and(|active| !active.is_active_viewport_placeholder())
}

pub(crate) fn has_shimmering_live_status(model: Option<&TranscriptModel>) -> bool {
    model
        .and_then(|model| model.active.as_ref())
        .is_some_and(TranscriptNode::has_shimmering_live_status)
}

pub(crate) fn active_viewport_needs_status_row_reserve(model: Option<&TranscriptModel>) -> bool {
    model
        .and_then(|model| model.active.as_ref())
        .is_some_and(TranscriptNode::has_streaming_without_pending_status)
}

pub(crate) fn model_plain_text(model: &TranscriptModel) -> String {
    let mut out = String::new();
    for node in &model.committed {
        for line in node.plain_lines() {
            out.push_str(&line);
            out.push('\n');
        }
        out.push('\n');
    }
    if let Some(active) = model.active.as_ref() {
        for line in active.plain_lines() {
            out.push_str(&line);
            out.push('\n');
        }
    }
    out
}

fn cells_to_lines<'a>(
    nodes: impl Iterator<Item = &'a TranscriptNode>,
    width: u16,
    mode: DisplayMode,
) -> Vec<Line<'static>> {
    let nodes = nodes.collect::<Vec<_>>();
    let mut out = Vec::new();
    let mut previous_kind = None;
    let mut idx = 0usize;
    while idx < nodes.len() {
        let node = nodes[idx];
        let _ = (node.id(), node.revision());
        if let Some(group) = collapsible_timeline_group(&node.kind) {
            let start = idx;
            idx += 1;
            while idx < nodes.len() && collapsible_timeline_group(&nodes[idx].kind) == Some(group) {
                idx += 1;
            }
            if !out.is_empty() {
                let gap = previous_kind
                    .map(|previous| gap_lines_between(previous, &node.kind))
                    .unwrap_or(0);
                if gap > 0 {
                    out.extend(std::iter::repeat_with(|| Line::from("")).take(gap));
                }
            }
            out.extend(collapsed_timeline_group_lines(
                &nodes[start..idx],
                group,
                width,
            ));
            previous_kind = Some(&nodes[idx - 1].kind);
            continue;
        }
        if !out.is_empty() {
            let gap = previous_kind
                .map(|previous| gap_lines_between(previous, &node.kind))
                .unwrap_or(0);
            if gap > 0 {
                out.extend(std::iter::repeat_with(|| Line::from("")).take(gap));
            }
        }
        out.extend(node.display_lines(width, mode));
        previous_kind = Some(&node.kind);
        idx += 1;
    }
    out
}

fn cells_to_native_lines<'a>(
    nodes: impl Iterator<Item = &'a TranscriptNode>,
    width: u16,
    mode: DisplayMode,
) -> Vec<NativeLine> {
    let nodes = nodes.collect::<Vec<_>>();
    let mut out = Vec::new();
    let mut previous_kind = None;
    let mut idx = 0usize;
    while idx < nodes.len() {
        let node = nodes[idx];
        let _ = (node.id(), node.revision());
        if let Some(group) = collapsible_timeline_group(&node.kind) {
            let start = idx;
            idx += 1;
            while idx < nodes.len() && collapsible_timeline_group(&nodes[idx].kind) == Some(group) {
                idx += 1;
            }
            if !out.is_empty() {
                let gap = previous_kind
                    .map(|previous| gap_lines_between(previous, &node.kind))
                    .unwrap_or(0);
                if gap > 0 {
                    out.extend(
                        std::iter::repeat_with(|| NativeLine::plain(Line::from(""))).take(gap),
                    );
                }
            }
            out.extend(collapsed_timeline_group_native_lines(
                &nodes[start..idx],
                group,
                width,
            ));
            previous_kind = Some(&nodes[idx - 1].kind);
            continue;
        }
        if !out.is_empty() {
            let gap = previous_kind
                .map(|previous| gap_lines_between(previous, &node.kind))
                .unwrap_or(0);
            if gap > 0 {
                out.extend(std::iter::repeat_with(|| NativeLine::plain(Line::from(""))).take(gap));
            }
        }
        out.extend(node.native_display_lines(width, mode));
        previous_kind = Some(&node.kind);
        idx += 1;
    }
    out
}

fn plain_native_lines(lines: Vec<Line<'static>>) -> Vec<NativeLine> {
    lines
        .into_iter()
        .map(native_line_from_styled_line)
        .collect()
}

fn native_line_from_styled_line(line: Line<'static>) -> NativeLine {
    let mut links = Vec::new();
    let mut col = 0usize;
    for span in &line.spans {
        let text = span.content.as_ref();
        let width = display_width(text);
        if (span.style == link() || span.style == path_reference())
            && !text.trim().is_empty()
            && width > 0
        {
            links.push(NativeLineLink {
                start_col: col,
                width,
                target: text.trim().to_string(),
            });
        }
        col = col.saturating_add(width);
    }
    NativeLine { line, links }
}

pub(crate) fn gap_before_active(model: &TranscriptModel) -> usize {
    let Some(previous) = model.committed.last() else {
        return 0;
    };
    let Some(active) = model.active.as_ref() else {
        return 0;
    };
    gap_lines_between(&previous.kind, &active.kind)
}

fn gap_lines_between(previous: &TranscriptKind, next: &TranscriptKind) -> usize {
    match (previous, next) {
        (TranscriptKind::StreamingAssistant { .. }, TranscriptKind::PendingStatus { .. }) => 0,
        (_, TranscriptKind::Prompt { .. } | TranscriptKind::PendingStatus { .. }) => 1,
        (
            TranscriptKind::Prompt { .. } | TranscriptKind::PendingStatus { .. },
            TranscriptKind::Timeline { .. } | TranscriptKind::ActiveStatus { .. },
        ) => 1,
        (
            TranscriptKind::Prompt { .. } | TranscriptKind::PendingStatus { .. },
            TranscriptKind::Assistant { .. }
            | TranscriptKind::ResultFile { .. }
            | TranscriptKind::StreamingAssistant { .. }
            | TranscriptKind::Error { .. }
            | TranscriptKind::Cancelled { .. }
            | TranscriptKind::Stack { .. },
        ) => 1,
        _ => 1,
    }
}

fn committed_node_for_event(
    app: &App,
    state: &WorkbenchState,
    root: &SessionMeta,
    events: &[EventRecord],
    event: &EventRecord,
) -> Option<TranscriptNode> {
    if event.session_id != root.id {
        return None;
    }
    let id = format!("{}:{}", event.session_id, event.seq);
    match event.event_type.as_str() {
        "session.input" | "session.followup" => {
            if event.event_type == "session.followup"
                && app.active_followup_is_pending(root.id.as_str(), event.seq)
            {
                return None;
            }
            if event.event_type == "session.followup"
                && active_followup_is_cancelled_in_events(events, event.seq)
            {
                return None;
            }
            let text = payload_string(event, "text")?;
            Some(TranscriptNode {
                id,
                seq: event.seq,
                revision: event.seq.max(0) as u64,
                kind: TranscriptKind::Prompt {
                    text,
                    followup: event.event_type == "session.followup",
                },
            })
        }
        SESSION_PENDING_ACTIVE_FOLLOWUP_EVENT => None,
        SESSION_QUEUED_FOLLOWUP_EVENT => None,
        // session.notice: a synthetic non-terminal assistant message (e.g. the
        // no-API-key nudge). Rendered like an assistant turn but does NOT count
        // as a terminal event, so the session stays resumable.
        "session.notice" => {
            let text = payload_string(event, "text")?;
            if text.trim().is_empty() {
                return None;
            }
            Some(TranscriptNode {
                id,
                seq: event.seq,
                revision: event.seq.max(0) as u64,
                kind: TranscriptKind::Assistant {
                    markdown: text,
                    source: source_for_state(state),
                },
            })
        }
        LOCAL_CHROME_CLOUD_PROMO_EVENT => {
            let text = payload_string(event, "text")?;
            if text.trim().is_empty() {
                return None;
            }
            Some(TranscriptNode {
                id,
                seq: event.seq,
                revision: event.seq.max(0) as u64,
                kind: TranscriptKind::Notice { text },
            })
        }
        "session.done" => {
            if let Some(result_file) = session_done_result_file(event, state) {
                return Some(TranscriptNode {
                    id,
                    seq: event.seq,
                    revision: event.seq.max(0) as u64,
                    kind: TranscriptKind::ResultFile {
                        file_path: result_file.file_path,
                        bytes: result_file.bytes,
                        mime: result_file.mime,
                        source: source_for_state(state),
                    },
                });
            }
            let result = session_done_result_text(event)?;
            if result.trim().is_empty() {
                return None;
            }
            Some(TranscriptNode {
                id,
                seq: event.seq,
                revision: event.seq.max(0) as u64,
                kind: TranscriptKind::Assistant {
                    markdown: result,
                    source: source_for_state(state),
                },
            })
        }
        "model.turn.response" => pre_tool_commentary_node(root, events, event),
        "model.response.continued" => continued_response_commentary_node(root, events, event),
        // `plan.proposed` is legacy: planning mode was removed, so nothing
        // emits these anymore. Old persisted sessions can still contain them,
        // though — render the text as a plain assistant turn so historical
        // plan content is preserved instead of vanishing from the transcript.
        "plan.proposed" => {
            let text = payload_string(event, "text")?;
            if text.trim().is_empty() {
                return None;
            }
            Some(TranscriptNode {
                id,
                seq: event.seq,
                revision: event.seq.max(0) as u64,
                kind: TranscriptKind::Assistant {
                    markdown: text,
                    source: source_for_state(state),
                },
            })
        }
        "session.failed" => {
            let lines = session_failed_error_lines(root, events, event);
            let node = TranscriptNode {
                id,
                seq: event.seq,
                revision: event.seq.max(0) as u64,
                kind: TranscriptKind::Error { lines },
            };
            Some(with_streaming_commentary_before_event(
                root, events, event, node,
            ))
        }
        "session.cancelled" => {
            if event
                .payload
                .get("reason")
                .and_then(serde_json::Value::as_str)
                == Some(PENDING_FOLLOWUP_INTERRUPT_REASON)
            {
                return Some(timeline_node(
                    event,
                    "interrupted",
                    vec!["Pending follow-up sent immediately.".to_string()],
                    NodeStyle::Muted,
                ));
            }
            let reason = event
                .payload
                .get("reason")
                .and_then(serde_json::Value::as_str);
            let (title, text, style) = if reason == Some(SESSION_PAUSED_REASON) {
                (SESSION_PAUSED_TITLE, SESSION_PAUSED_TEXT, NodeStyle::Failed)
            } else {
                ("stopped", "Progress is saved in history.", NodeStyle::Muted)
            };
            let node = TranscriptNode {
                id,
                seq: event.seq,
                revision: event.seq.max(0) as u64,
                kind: TranscriptKind::Cancelled {
                    title: title.to_string(),
                    text: text.to_string(),
                    style,
                },
            };
            Some(with_streaming_commentary_before_event(
                root, events, event, node,
            ))
        }
        "collab_agent_spawn_begin"
        | "collab_agent_interaction_begin"
        | "collab_close_begin"
        | "collab_resume_begin" => None,
        "collab_agent_spawn_end" => {
            if has_later_root_event(events, event, "agent.spawned") {
                None
            } else {
                Some(subagent_lifecycle_node(
                    app,
                    event,
                    "started",
                    NodeStyle::Normal,
                ))
            }
        }
        "collab_agent_interaction_end" => {
            if has_agent_message_for_collab_receiver(events, event) {
                None
            } else {
                Some(subagent_lifecycle_node(
                    app,
                    event,
                    "messaged",
                    NodeStyle::Muted,
                ))
            }
        }
        "collab_waiting_begin" | "collab_waiting_end" => None,
        "collab_close_end" => Some(subagent_lifecycle_node(
            app,
            event,
            "stopped",
            NodeStyle::Muted,
        )),
        "collab_resume_end" => {
            if has_later_root_event(events, event, "agent.resumed") {
                None
            } else {
                Some(subagent_lifecycle_node(
                    app,
                    event,
                    "resumed",
                    NodeStyle::Normal,
                ))
            }
        }
        "agent.spawned" => Some(subagent_lifecycle_node(
            app,
            event,
            "started",
            NodeStyle::Normal,
        )),
        "agent.spawn.queued" => Some(subagent_lifecycle_node(
            app,
            event,
            "queued",
            NodeStyle::Muted,
        )),
        "agent.spawn.queue_released" => Some(subagent_lifecycle_node(
            app,
            event,
            "starting",
            NodeStyle::Muted,
        )),
        "agent.message" => Some(subagent_lifecycle_node(
            app,
            event,
            if event
                .payload
                .get("trigger_turn")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false)
            {
                "sent task"
            } else {
                "messaged"
            },
            NodeStyle::Muted,
        )),
        "agent.wait.started" => None,
        "agent.wait.finished" => agent_wait_finished_node(event),
        SESSION_MAILBOX_CONTINUATION_STARTED_EVENT => Some(mailbox_continuation_node(event)),
        "agent.resumed" => {
            if is_replay_materialization_event(event) {
                None
            } else {
                Some(subagent_lifecycle_node(
                    app,
                    event,
                    "resumed",
                    NodeStyle::Normal,
                ))
            }
        }
        "agent.completed" => {
            subagent_terminal_lifecycle_node(app, event, "finished", NodeStyle::Normal)
        }
        "agent.failed" => subagent_terminal_lifecycle_node(app, event, "failed", NodeStyle::Failed),
        "agent.cancelled" => {
            subagent_terminal_lifecycle_node(app, event, "stopped", NodeStyle::Muted)
        }
        "model.tool_call" | "tool.started" | "tool.finished" => None,
        "tool.batch_started" | "tool.batch_result" | "tool.batch_finished" => None,
        "tool.output" => tool_output_node(event),
        "tool.image" => Some(tool_image_node(event)),
        "tool.failed" => tool_failed_node(event),
        "tool.aborted" => Some(timeline_node(
            event,
            "run",
            tool_failed_lines(event),
            NodeStyle::Muted,
        )),
        "tool.output_spilled" => {
            let path = event
                .payload
                .get("artifact")
                .and_then(|artifact| artifact.get("path"))
                .and_then(serde_json::Value::as_str)
                .map(|path| display_path(path, state))
                .unwrap_or_else(|| "artifact".to_string());
            Some(timeline_node(
                event,
                "run",
                vec![format!("Full output saved to {path}")],
                NodeStyle::Muted,
            ))
        }
        "file.list" => {
            let path = payload_string(event, "path")
                .map(|path| display_path(&path, state))
                .unwrap_or_else(|| ".".to_string());
            let item = event
                .payload
                .get("count")
                .and_then(serde_json::Value::as_u64)
                .map(|count| format!("listed {path} ({count} items)"))
                .unwrap_or_else(|| format!("listed {path}"));
            Some(timeline_node(
                event,
                "explored",
                vec![item],
                NodeStyle::Normal,
            ))
        }
        "file.read" => {
            let path = payload_string(event, "path").map(|path| display_path(&path, state))?;
            Some(timeline_node(
                event,
                "explored",
                vec![format!("read {path}")],
                NodeStyle::Normal,
            ))
        }
        "file.search" => {
            let query = payload_string(event, "query").unwrap_or_else(|| "files".to_string());
            let matches = event
                .payload
                .get("matches")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0);
            Some(timeline_node(
                event,
                "explored",
                vec![format!("searched {query:?} ({matches} matches)")],
                NodeStyle::Normal,
            ))
        }
        "exec_command.begin" => exec_command_begin_node(event),
        "terminal.interaction" => terminal_interaction_node(events, event),
        "command.started" => {
            let cmd = payload_string(event, "cmd").unwrap_or_else(|| "command".to_string());
            Some(timeline_node(
                event,
                "command",
                vec![format!("ran command: {cmd}")],
                NodeStyle::Normal,
            ))
        }
        "command.output" => None,
        "command.finished" => {
            let failed = event
                .payload
                .get("success")
                .and_then(serde_json::Value::as_bool)
                .is_some_and(|success| !success);
            failed.then(|| {
                let code = event
                    .payload
                    .get("exit_code")
                    .and_then(serde_json::Value::as_i64)
                    .map(|code| code.to_string())
                    .unwrap_or_else(|| "unknown".to_string());
                timeline_node(
                    event,
                    "command",
                    vec![format!("failed with exit {code}")],
                    NodeStyle::Failed,
                )
            })
        }
        "patch.file_changed" => {
            let kind = payload_string(event, "kind").unwrap_or_else(|| "changed".to_string());
            let path = payload_string(event, "path")
                .map(|path| display_path(&path, state))
                .unwrap_or_else(|| "file".to_string());
            Some(timeline_node(
                event,
                "edit",
                vec![format!("{kind} {path}")],
                NodeStyle::Normal,
            ))
        }
        "patch.started" | "patch.finished" => None,
        "artifact.created" => artifact_created_node(event, state),
        "browser.connected" | "browser.reconnected" | "browser.target_changed" => {
            Some(timeline_node(
                event,
                "browser",
                vec![browser_event_label(event)],
                NodeStyle::Normal,
            ))
        }
        "browser.disconnected" => Some(timeline_node(
            event,
            "browser",
            vec!["browser disconnected".to_string()],
            NodeStyle::Muted,
        )),
        "browser.live_url" => Some(timeline_node(
            event,
            "browser",
            vec![payload_string(event, "live_url")
                .or_else(|| payload_string(event, "url"))
                .map(|url| format!("live view {}", compact_url(&url)))
                .unwrap_or_else(|| "live view available".to_string())],
            NodeStyle::Normal,
        )),
        "browser.page" | "browser.state" => event
            .payload
            .get("url")
            .and_then(serde_json::Value::as_str)
            .map(|url| {
                timeline_node(
                    event,
                    "browser",
                    vec![format!("opened {}", compact_url(url))],
                    NodeStyle::Normal,
                )
            }),
        "browser.open_requested" | "browser.reconnect_requested" | "browser.cloud_shutdown" => None,
        "browser.cloud_shutdown_failed" => Some(timeline_node(
            event,
            "error",
            vec![payload_string(event, "error")
                .unwrap_or_else(|| "browser shutdown failed".to_string())],
            NodeStyle::Failed,
        )),
        "plan.updated" => Some(timeline_node(
            event,
            "plan",
            vec!["updated plan".to_string()],
            NodeStyle::Normal,
        )),
        "request_user_input.requested" | "request_user_input.response" => None,
        "session.deadline_warning" => Some(timeline_node(
            event,
            "warning",
            vec!["turn budget is nearly exhausted".to_string()],
            NodeStyle::Muted,
        )),
        "session.startup_warning" => Some(timeline_node(
            event,
            "warning",
            vec![payload_string(event, "message").unwrap_or_else(|| "startup warning".to_string())],
            NodeStyle::Muted,
        )),
        "session.final_answer_not_ready_at_max_turns" => Some(timeline_node(
            event,
            "error",
            vec![payload_string(event, "error")
                .unwrap_or_else(|| "final answer artifact is not ready".to_string())],
            NodeStyle::Failed,
        )),
        "model.turn.context_overflow" => Some(timeline_node(
            event,
            "context",
            vec!["provider context overflow; compacting history".to_string()],
            NodeStyle::Muted,
        )),
        "session.compaction_failed" => Some(timeline_node(
            event,
            "error",
            vec![payload_string(event, "error").unwrap_or_else(|| "compaction failed".to_string())],
            NodeStyle::Failed,
        )),
        "model.turn.error" => Some(timeline_node(
            event,
            "error",
            vec!["model request hit an error".to_string()],
            NodeStyle::Failed,
        )),
        "command.write_error" => Some(timeline_node(
            event,
            "error",
            vec![payload_string(event, "error")
                .unwrap_or_else(|| "failed to write to command".to_string())],
            NodeStyle::Failed,
        )),
        "model.turn.request"
        | "model.thinking_delta"
        | "model.turn.retry"
        | "model.stream_delta"
        | "model.delta"
        | "model.response.output_item.completed"
        | "model.config"
        | "model.usage"
        | "session.compaction_started"
        | "session.compacted"
        | "session.created"
        | "session.status"
        | "session.final_answer_ready"
        | "session.final_answer_used"
        | "session.cancel_requested"
        | "agent.context"
        | "agent.updated"
        | "telemetry.trace"
        | "telemetry.failed"
        | "command.cleaned_up" => None,
        _ => None,
    }
}

const MODEL_FAILURE_CHANGE_MODEL_HINT: &str =
    "Hint: use /model to choose a different model, then try again.";

fn session_failed_error_lines(
    root: &SessionMeta,
    events: &[EventRecord],
    event: &EventRecord,
) -> Vec<String> {
    let text = payload_string(event, "error").unwrap_or_else(|| "The task failed.".to_string());
    let mut lines = vec![friendly_error_message(&text)];
    if failed_after_model_request_without_response(root, events, event) {
        lines.push(MODEL_FAILURE_CHANGE_MODEL_HINT.to_string());
    }
    lines
}

fn failed_after_model_request_without_response(
    root: &SessionMeta,
    events: &[EventRecord],
    event: &EventRecord,
) -> bool {
    let Some(event_idx) = events.iter().position(|candidate| {
        candidate.session_id == event.session_id
            && candidate.seq == event.seq
            && candidate.event_type == event.event_type
    }) else {
        return false;
    };

    let prior = &events[..event_idx];
    let Some(turn_start_idx) = prior.iter().rposition(|candidate| {
        candidate.session_id == root.id
            && matches!(
                candidate.event_type.as_str(),
                "model.turn.request" | "model.turn.retry"
            )
    }) else {
        return false;
    };

    let turn_events = &prior[turn_start_idx..];
    !turn_events.iter().any(|candidate| {
        candidate.session_id == root.id && candidate.event_type == "model.turn.response"
    })
}

fn push_committed_node(committed: &mut Vec<TranscriptNode>, node: TranscriptNode) {
    if let Some(last) = committed.last_mut() {
        if merge_timeline_node(last, &node) {
            return;
        }
    }
    committed.push(node);
}

fn merge_timeline_node(last: &mut TranscriptNode, next: &TranscriptNode) -> bool {
    match (&mut last.kind, &next.kind) {
        (
            TranscriptKind::Timeline {
                group,
                lines,
                style,
            },
            TranscriptKind::Timeline {
                group: next_group,
                lines: next_lines,
                style: next_style,
            },
        ) if group == next_group && style == next_style => {
            if matches!(group.as_str(), "shell" | "command") {
                return false;
            }
            if *style == NodeStyle::Thought {
                *lines = next_lines.clone();
            } else {
                lines.extend(next_lines.clone());
                compact_repeated_read_lines(lines);
            }
            last.id = next.id.clone();
            last.seq = next.seq;
            last.revision = next.revision;
            true
        }
        _ => false,
    }
}

fn compact_repeated_read_lines(lines: &mut Vec<String>) {
    let mut compacted = Vec::with_capacity(lines.len());
    let mut reads = Vec::new();

    for line in lines.drain(..) {
        if let Some(path) = read_line_path(&line) {
            reads.push(path.to_string());
        } else {
            flush_read_lines(&mut compacted, &mut reads);
            compacted.push(line);
        }
    }
    flush_read_lines(&mut compacted, &mut reads);

    *lines = compacted;
}

fn read_line_path(line: &str) -> Option<&str> {
    line.strip_prefix("read ")
        .map(str::trim)
        .filter(|path| !path.is_empty())
}

fn flush_read_lines(out: &mut Vec<String>, reads: &mut Vec<String>) {
    match reads.len() {
        0 => {}
        1 => out.push(format!("read {}", reads[0])),
        _ => out.push(format!("read {}", reads.join(", "))),
    }
    reads.clear();
}

fn model_response_tool_call_count(event: &EventRecord) -> u64 {
    event
        .payload
        .get("tool_call_count")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0)
}

fn pre_tool_commentary_node(
    root: &SessionMeta,
    events: &[EventRecord],
    event: &EventRecord,
) -> Option<TranscriptNode> {
    if event.session_id != root.id || model_response_tool_call_count(event) == 0 {
        return None;
    }
    streaming_commentary_node_before_event(root, events, event)
}

fn continued_response_commentary_node(
    root: &SessionMeta,
    events: &[EventRecord],
    event: &EventRecord,
) -> Option<TranscriptNode> {
    let reason = event
        .payload
        .get("reason")
        .and_then(serde_json::Value::as_str)?;
    if reason != "active_turn_queue_drained" {
        return None;
    }
    streaming_commentary_node_before_event(root, events, event)
}

fn with_streaming_commentary_before_event(
    root: &SessionMeta,
    events: &[EventRecord],
    event: &EventRecord,
    node: TranscriptNode,
) -> TranscriptNode {
    let Some(commentary) = streaming_commentary_node_before_event(root, events, event) else {
        return node;
    };
    TranscriptNode {
        id: format!("{}:{}:stack", event.session_id, event.seq),
        seq: event.seq,
        revision: event.seq.max(0) as u64,
        kind: TranscriptKind::Stack {
            nodes: vec![commentary, node],
        },
    }
}

fn streaming_commentary_node_before_event(
    root: &SessionMeta,
    events: &[EventRecord],
    event: &EventRecord,
) -> Option<TranscriptNode> {
    if event.session_id != root.id {
        return None;
    }
    let event_idx = events.iter().position(|candidate| {
        candidate.session_id == event.session_id
            && candidate.seq == event.seq
            && candidate.event_type == event.event_type
    })?;
    let turn_start = events[..event_idx]
        .iter()
        .rposition(|candidate| {
            candidate.session_id == root.id
                && (matches!(
                    candidate.event_type.as_str(),
                    "model.turn.request" | "model.turn.retry" | "model.turn.error"
                ) || (candidate.event_type == "model.turn.response"
                    && model_response_tool_call_count(candidate) > 0))
        })
        .map(|idx| idx.saturating_add(1))
        .unwrap_or(0);
    let markdown = turn_streaming_text_from_events(&events[turn_start..event_idx])?;
    let markdown = markdown.trim_end().to_string();
    if markdown.trim().is_empty() {
        return None;
    }
    Some(TranscriptNode {
        id: format!("{}:{}:commentary", event.session_id, event.seq),
        seq: event.seq,
        revision: event.seq.max(0) as u64,
        kind: TranscriptKind::Assistant {
            markdown,
            source: None,
        },
    })
}

fn active_node_for_session(
    app: &App,
    state: &WorkbenchState,
    root: &SessionMeta,
    events: &[EventRecord],
) -> Option<TranscriptNode> {
    let live_events = current_turn_events(events);

    if let Some(pending_followup) = pending_followup_active_node(app, state, root, events) {
        return Some(pending_followup);
    }

    let active_child_count = active_child_session_count(app, &root.id);
    let pending_mailbox_count = pending_agent_mailbox_count(app, &root.id);
    let live_thinking_text = state
        .transcript
        .last()
        .and_then(|turn| turn.thinking_text.as_deref())
        .map(str::trim)
        .filter(|text| !text.is_empty());
    let live_streaming_text = state
        .transcript
        .last()
        .and_then(|turn| turn.streaming_text.as_deref())
        .filter(|text| !text.is_empty())
        .filter(|text| !text.trim().is_empty())
        .filter(|_| !live_stream_has_committed_successor(live_events));
    let live_turn_is_followup = state.transcript.last().is_some_and(|turn| turn.is_followup);

    let mut active_nodes = Vec::new();
    let live_status = live_status_for_session(active_child_count, live_thinking_text, live_events);

    if let Some(text) = live_streaming_text {
        let seq = events.last().map(|event| event.seq).unwrap_or_default();
        if !text.trim().is_empty() {
            active_nodes.push(TranscriptNode {
                id: format!("{}:active-stream", root.id),
                seq,
                revision: seq.max(0) as u64,
                kind: TranscriptKind::StreamingAssistant {
                    markdown: text.to_string(),
                },
            });
        }
    }

    if app.native_scrollback_is_active() && live_streaming_text.is_none() {
        if let Some(node) = active_timeline_tail_node(app, state, root, live_events) {
            active_nodes.push(node);
        }
    }

    if !app.native_scrollback_is_active() && live_streaming_text.is_none() {
        if let Some(event) = live_events.iter().rev().find(|event| {
            matches!(
                event.event_type.as_str(),
                "command.waiting"
                    | "tool.started"
                    | "browser.page"
                    | "browser.state"
                    | "plan.updated"
            )
        }) {
            if let Some(node) = active_node_for_event(root, events, event) {
                active_nodes.push(node);
            }
        }
    }
    if pending_mailbox_count > 0 && live_streaming_text.is_none() {
        active_nodes.push(pending_status_node(
            root,
            events,
            "subagent results ready",
            pending_mailbox_summary(pending_mailbox_count).as_deref(),
        ));
    }
    if pending_mailbox_count == 0
        && (live_streaming_text.is_none()
            || (live_status == "Thinking..."
                && live_stream_pending_status_allowed(live_events, !live_turn_is_followup)))
    {
        active_nodes.push(pending_status_node(
            root,
            events,
            live_status,
            active_subagent_summary(active_child_count).as_deref(),
        ));
    }

    if !active_nodes.is_empty() {
        let seq = events.last().map(|event| event.seq).unwrap_or_default();
        return Some(TranscriptNode {
            id: format!("{}:active-stack", root.id),
            seq,
            revision: seq.max(0) as u64,
            kind: TranscriptKind::Stack {
                nodes: active_nodes,
            },
        });
    }

    Some(pending_status_node(
        root,
        events,
        live_status,
        active_subagent_summary(active_child_count).as_deref(),
    ))
}

fn live_status_for_session(
    active_child_count: usize,
    live_thinking_text: Option<&str>,
    live_events: &[EventRecord],
) -> &'static str {
    if active_child_count > 0 {
        return "Working...";
    }
    if live_events
        .iter()
        .rev()
        .any(|event| event.event_type == "model.turn.retry")
    {
        return "Retrying...";
    }
    if live_thinking_text.is_some()
        || live_events
            .iter()
            .rev()
            .any(|event| event.event_type == "model.turn.request")
    {
        return "Thinking...";
    }
    "Working..."
}

fn active_child_session_count(app: &App, root_id: &str) -> usize {
    let store_count = app
        .state_cache
        .sessions
        .iter()
        .filter(|session| {
            session.parent_id.as_deref() == Some(root_id) && session.status.is_active()
        })
        .count();
    let runtime_count =
        crate::runtime::runtime_active_child_session_count(&app.args.state_dir, root_id)
            .ok()
            .flatten()
            .unwrap_or(0);
    store_count.max(runtime_count)
}

fn pending_agent_mailbox_count(app: &App, session_id: &str) -> usize {
    crate::runtime::pending_runtime_agent_mailbox_count(&app.args.state_dir, session_id)
        .ok()
        .flatten()
        .unwrap_or(0)
}

fn active_subagent_summary(active_child_count: usize) -> Option<String> {
    if active_child_count == 0 {
        return None;
    }
    let noun = if active_child_count == 1 {
        "subagent"
    } else {
        "subagents"
    };
    Some(format!("({active_child_count} {noun} running)"))
}

fn pending_mailbox_summary(pending_mailbox_count: usize) -> Option<String> {
    if pending_mailbox_count == 0 {
        return None;
    }
    let noun = if pending_mailbox_count == 1 {
        "result"
    } else {
        "results"
    };
    Some(format!("({pending_mailbox_count} subagent {noun} queued)"))
}

fn active_timeline_tail_node(
    app: &App,
    state: &WorkbenchState,
    root: &SessionMeta,
    live_events: &[EventRecord],
) -> Option<TranscriptNode> {
    let after_seq = app.native_history.last_seq;
    let nodes = live_events
        .iter()
        .filter(|event| event.seq > after_seq)
        .filter_map(|event| committed_node_for_event(app, state, root, live_events, event))
        .filter(|node| !node.is_terminal_scrollback_transient())
        .collect::<Vec<_>>();
    let last = nodes.last()?;
    let key = timeline_merge_key(last)?;
    if !is_open_timeline_node(last) {
        return None;
    }

    let mut start = nodes.len().saturating_sub(1);
    while start > 0 && timeline_merge_key(&nodes[start - 1]) == Some(key) {
        start -= 1;
    }

    let mut tail = Vec::new();
    for node in nodes[start..].iter().cloned() {
        push_committed_node(&mut tail, node);
    }
    tail.into_iter().next()
}

fn is_open_timeline_node(node: &TranscriptNode) -> bool {
    matches!(
        &node.kind,
        TranscriptKind::Timeline { style, .. } if *style != NodeStyle::Failed
    )
}

fn timeline_merge_key(node: &TranscriptNode) -> Option<(&str, NodeStyle)> {
    match &node.kind {
        TranscriptKind::Timeline { group, style, .. } => Some((group.as_str(), *style)),
        _ => None,
    }
}

fn pending_followup_active_node(
    app: &App,
    state: &WorkbenchState,
    root: &SessionMeta,
    events: &[EventRecord],
) -> Option<TranscriptNode> {
    let latest_followup = events.iter().rev().find(|event| {
        event.session_id == root.id
            && event.event_type == "session.followup"
            && !active_followup_is_after_next_tool_call(event)
    })?;
    let has_prior_scrollback = events
        .iter()
        .filter(|event| event.seq < latest_followup.seq)
        .filter_map(|event| committed_node_for_event(app, state, root, events, event))
        .any(|node| !node.is_terminal_scrollback_transient());
    if !has_prior_scrollback {
        return None;
    }
    let has_committed_output_after = events
        .iter()
        .filter(|event| event.seq > latest_followup.seq)
        .filter_map(|event| committed_node_for_event(app, state, root, events, event))
        .filter(|node| !node.is_terminal_scrollback_transient())
        .any(|node| !node.is_prompt());
    if has_committed_output_after {
        return None;
    }
    let has_live_output_after = events
        .iter()
        .filter(|event| event.seq > latest_followup.seq)
        .any(is_live_output_event);
    if has_live_output_after {
        return None;
    }
    let status = pending_followup_status(events, latest_followup.seq);
    Some(TranscriptNode {
        id: format!("{}:active-followup:{}", root.id, latest_followup.seq),
        seq: latest_followup.seq,
        revision: latest_followup.seq.max(0) as u64,
        kind: TranscriptKind::PendingStatus {
            status,
            detail: None,
        },
    })
}

fn live_stream_has_committed_successor(live_events: &[EventRecord]) -> bool {
    let segment_start = live_events
        .iter()
        .rposition(|event| {
            matches!(
                event.event_type.as_str(),
                "model.turn.request" | "model.turn.retry" | "model.turn.error"
            )
        })
        .map(|idx| idx.saturating_add(1))
        .unwrap_or(0);
    let segment = live_events.get(segment_start..).unwrap_or_default();
    let Some(latest_stream_seq) = segment
        .iter()
        .rev()
        .find(|event| {
            event.event_type == "model.stream_delta"
                && event
                    .payload
                    .get("text")
                    .and_then(serde_json::Value::as_str)
                    .is_some_and(|text| !text.trim().is_empty())
        })
        .map(|event| event.seq)
    else {
        return false;
    };
    segment.iter().any(|event| {
        event.seq > latest_stream_seq
            && (matches!(
                event.event_type.as_str(),
                "session.done" | "session.failed" | "session.cancelled"
            ) || (event.event_type == "model.turn.response"
                && model_response_tool_call_count(event) > 0))
    })
}

fn live_stream_pending_status_allowed(
    live_events: &[EventRecord],
    allow_quiet_status: bool,
) -> bool {
    let Some(latest_stream) = latest_nonempty_stream_event(live_events) else {
        return false;
    };
    let later_events = live_events
        .iter()
        .filter(|event| event.seq > latest_stream.seq)
        .filter(|event| !is_replay_materialization_event(event))
        .collect::<Vec<_>>();
    if later_events.iter().any(|event| {
        event.event_type != "model.stream_delta" && event.event_type != "goal.accounted"
    }) {
        return true;
    }
    if later_events
        .iter()
        .any(|event| event.event_type == "goal.accounted")
    {
        return false;
    }
    allow_quiet_status
        && now_ms().saturating_sub(latest_stream.ts_ms) >= LIVE_STREAM_QUIET_STATUS_DELAY_MS
}

fn latest_nonempty_stream_event(live_events: &[EventRecord]) -> Option<&EventRecord> {
    live_events.iter().rev().find(|event| {
        event.event_type == "model.stream_delta"
            && event
                .payload
                .get("text")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|text| !text.trim().is_empty())
    })
}

fn is_live_output_event(event: &EventRecord) -> bool {
    match event.event_type.as_str() {
        "model.stream_delta" | "model.thinking_delta" => event
            .payload
            .get("text")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|text| !text.trim().is_empty()),
        "command.waiting" | "tool.output_delta" | "tool.started" | "browser.page"
        | "browser.state" | "plan.updated" => true,
        _ => false,
    }
}

fn pending_followup_status(events: &[EventRecord], after_seq: i64) -> String {
    events
        .iter()
        .filter(|event| event.seq > after_seq)
        .rev()
        .find_map(|event| match event.event_type.as_str() {
            "model.turn.request" => Some("thinking".to_string()),
            "model.turn.retry" => Some("retrying model request".to_string()),
            "command.waiting" => Some("running command".to_string()),
            "tool.started" => payload_string(event, "name")
                .map(|name| format!("running {name}"))
                .or_else(|| Some("running tool".to_string())),
            _ => None,
        })
        .unwrap_or_else(|| "sending".to_string())
}

fn current_turn_events(events: &[EventRecord]) -> &[EventRecord] {
    let start = events
        .iter()
        .rposition(|event| event_starts_visible_turn(events, event))
        .map(|idx| idx.saturating_add(1))
        .unwrap_or(0);
    events.get(start..).unwrap_or_default()
}

fn event_starts_visible_turn(events: &[EventRecord], event: &EventRecord) -> bool {
    match event.event_type.as_str() {
        "session.input" => true,
        "session.followup" => {
            !active_followup_is_pending_in_events(events, event.seq)
                && !active_followup_is_cancelled_in_events(events, event.seq)
        }
        _ => false,
    }
}

fn active_node_for_event(
    root: &SessionMeta,
    events: &[EventRecord],
    event: &EventRecord,
) -> Option<TranscriptNode> {
    match event.event_type.as_str() {
        "model.turn.request" => None,
        "model.turn.retry" => Some(active_status_node(
            root,
            events,
            "thinking",
            vec!["retrying model request".to_string()],
            NodeStyle::Muted,
        )),
        "collab_agent_spawn_begin" => Some(active_status_node(
            root,
            events,
            "subagent",
            vec!["spawning".to_string()],
            NodeStyle::Muted,
        )),
        "agent.spawn.queued" => Some(active_status_node(
            root,
            events,
            "subagent",
            vec!["queued for capacity".to_string()],
            NodeStyle::Muted,
        )),
        "agent.spawn.queue_released" => Some(active_status_node(
            root,
            events,
            "subagent",
            vec!["starting queued task".to_string()],
            NodeStyle::Muted,
        )),
        "collab_agent_interaction_begin" => Some(active_status_node(
            root,
            events,
            "subagent",
            vec!["messaging".to_string()],
            NodeStyle::Muted,
        )),
        "agent.wait.started" => Some(active_status_node(
            root,
            events,
            "subagent",
            vec!["waiting".to_string()],
            NodeStyle::Muted,
        )),
        "collab_waiting_begin" => Some(active_status_node(
            root,
            events,
            "subagent",
            vec!["waiting".to_string()],
            NodeStyle::Muted,
        )),
        "collab_close_begin" => Some(active_status_node(
            root,
            events,
            "subagent",
            vec!["stopping".to_string()],
            NodeStyle::Muted,
        )),
        "collab_resume_begin" => Some(active_status_node(
            root,
            events,
            "subagent",
            vec!["resuming".to_string()],
            NodeStyle::Muted,
        )),
        "command.waiting" => Some(active_status_node(
            root,
            events,
            "run",
            vec!["command still running".to_string()],
            NodeStyle::Muted,
        )),
        "tool.started" => {
            let name = payload_string(event, "name").unwrap_or_else(|| "tool".to_string());
            active_tool_status(&name).map(|(group, line)| {
                active_status_node(
                    root,
                    events,
                    group,
                    vec![line.to_string()],
                    NodeStyle::Muted,
                )
            })
        }
        "browser.page" | "browser.state" => event
            .payload
            .get("url")
            .and_then(serde_json::Value::as_str)
            .map(|url| {
                active_status_node(
                    root,
                    events,
                    "browser",
                    vec![format!("opened {}", compact_url(url))],
                    NodeStyle::Muted,
                )
            }),
        "plan.updated" => Some(active_status_node(
            root,
            events,
            "plan",
            vec!["updated plan".to_string()],
            NodeStyle::Muted,
        )),
        _ => None,
    }
}

fn active_status_node(
    root: &SessionMeta,
    events: &[EventRecord],
    group: &str,
    lines: Vec<String>,
    style: NodeStyle,
) -> TranscriptNode {
    let seq = events.last().map(|event| event.seq).unwrap_or_default();
    TranscriptNode {
        id: format!("{}:active:{group}", root.id),
        seq,
        revision: seq.max(0) as u64,
        kind: TranscriptKind::ActiveStatus {
            group: group.to_string(),
            lines,
            style,
        },
    }
}

fn pending_status_node(
    root: &SessionMeta,
    events: &[EventRecord],
    status: &str,
    detail: Option<&str>,
) -> TranscriptNode {
    let seq = events.last().map(|event| event.seq).unwrap_or_default();
    TranscriptNode {
        id: format!("{}:active-status", root.id),
        seq,
        revision: seq.max(0) as u64,
        kind: TranscriptKind::PendingStatus {
            status: status.to_string(),
            detail: detail.map(str::to_string),
        },
    }
}

fn tool_output_node(event: &EventRecord) -> Option<TranscriptNode> {
    let name = payload_string(event, "name").unwrap_or_else(|| "tool".to_string());
    if is_subagent_management_tool(&name) || name == "request_user_input" {
        return None;
    }
    if is_command_tool_output(&name) {
        return None;
    }
    let mut lines = Vec::new();
    if name == "browser" {
        lines.extend(browser_command_output_lines(event));
    }
    let browser_script_summary_lines = if name == "browser_script" {
        browser_script_summary_lines(event)
    } else {
        Vec::new()
    };
    let has_browser_script_summary = !browser_script_summary_lines.is_empty();
    lines.extend(browser_script_summary_lines);
    if name == "browser_script" && !has_browser_script_summary {
        lines.extend(browser_script_structured_output_lines(event));
    }
    if should_show_generic_tool_output_text(&name)
        && name != "browser"
        && !(name == "browser_script" && has_browser_script_summary)
    {
        if let Some(text) = payload_string(event, "text").filter(|text| !text.trim().is_empty()) {
            if name == "browser_script" {
                lines.extend(browser_script_text_preview_lines(&text));
            } else {
                lines.extend(preview_lines(&text, 3));
            }
        }
    }
    if event
        .payload
        .get("text_truncated")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
    {
        if let Some(path) = event
            .payload
            .get("text_artifact")
            .and_then(|artifact| artifact.get("path"))
            .and_then(serde_json::Value::as_str)
        {
            lines.push(format!("full output saved to {path}"));
        }
    }
    if let Some(images) = event
        .payload
        .get("images")
        .and_then(serde_json::Value::as_array)
    {
        if !images.is_empty() {
            lines.push(format!(
                "{} image artifact{}",
                images.len(),
                plural(images.len())
            ));
        }
    }
    if let Some(artifacts) = event
        .payload
        .get("artifacts")
        .and_then(serde_json::Value::as_array)
    {
        if !artifacts.is_empty() {
            lines.push(format!(
                "{} file artifact{}",
                artifacts.len(),
                plural(artifacts.len())
            ));
        }
    }
    if lines.is_empty() {
        return None;
    }
    Some(timeline_node(
        event,
        tool_output_group(&name),
        lines,
        NodeStyle::Muted,
    ))
}

fn tool_failed_node(event: &EventRecord) -> Option<TranscriptNode> {
    let name = payload_string(event, "name").unwrap_or_else(|| "tool".to_string());
    let group = if is_command_tool_output(&name) {
        tool_output_group(&name)
    } else {
        "error"
    };
    Some(timeline_node(
        event,
        group,
        tool_failed_lines(event),
        NodeStyle::Failed,
    ))
}

fn exec_command_begin_node(event: &EventRecord) -> Option<TranscriptNode> {
    let command = event_command_display(event).unwrap_or_else(|| "command".to_string());
    let name = payload_string(event, "name").unwrap_or_else(|| "exec_command".to_string());
    let activity = command_activity(&name, &command);
    Some(timeline_node(
        event,
        activity.group,
        vec![activity.line],
        NodeStyle::Normal,
    ))
}

struct CommandActivity {
    group: &'static str,
    line: String,
}

fn command_activity(name: &str, command: &str) -> CommandActivity {
    if let Some(line) = exploration_command_summary(command) {
        return CommandActivity {
            group: "explored",
            line,
        };
    }
    if let Some(line) = shell_script_activity_summary(command) {
        return CommandActivity {
            group: command_event_group(name),
            line,
        };
    }
    CommandActivity {
        group: command_event_group(name),
        line: format!("{COMMAND_LINE_PREFIX}{}", inline_command_display(command)),
    }
}

fn inline_command_display(command: &str) -> String {
    command.replace(['\r', '\n'], " ")
}

fn exploration_command_summary(command: &str) -> Option<String> {
    let segments = shell_command_segments(command);
    if segments.is_empty() {
        return None;
    }

    if segments
        .iter()
        .any(|segment| segment_is_execution_or_mutation(segment))
    {
        return None;
    }

    if let Some(summary) = read_only_git_summary(&segments) {
        return Some(summary.to_string());
    }
    if segments
        .iter()
        .any(|segment| segment_reads_toolchain_metadata(segment))
    {
        return Some("read toolchain metadata".to_string());
    }

    if segments.iter().any(|segment| segment_lists_files(segment)) {
        return Some("listed files".to_string());
    }
    if segments
        .iter()
        .any(|segment| segment_lists_directories(segment))
    {
        return Some("listed directories".to_string());
    }
    if segments
        .iter()
        .any(|segment| segment_program(segment).as_deref() == Some("ls"))
    {
        return Some(list_command_summary(&segments));
    }

    if segments.iter().any(|segment| segment_searches(segment)) {
        return Some("searched repository".to_string());
    }

    if segments
        .iter()
        .any(|segment| segment_reads_repository_metadata(segment))
    {
        return Some("read repository metadata".to_string());
    }

    if segments.iter().any(|segment| segment_reads_files(segment)) {
        let paths = command_read_paths(&segments);
        if !paths.is_empty() {
            return Some(format!("read {}", paths.join(", ")));
        }
        return Some("read files".to_string());
    }

    if segments
        .iter()
        .all(|segment| segment_is_read_only_probe(segment))
    {
        return Some("read repository metadata".to_string());
    }

    None
}

fn shell_command_segments(command: &str) -> Vec<Vec<String>> {
    let mut segments = Vec::new();
    let mut segment = Vec::new();
    let mut token = String::new();
    let mut chars = command.trim().chars().peekable();
    let mut in_single = false;
    let mut in_double = false;
    let mut escaped = false;

    while let Some(ch) = chars.next() {
        if escaped {
            token.push(ch);
            escaped = false;
            continue;
        }

        if ch == '\\' {
            escaped = true;
            continue;
        }

        if ch == '\'' && !in_double {
            in_single = !in_single;
            continue;
        }
        if ch == '"' && !in_single {
            in_double = !in_double;
            continue;
        }

        if !in_single && !in_double {
            if ch.is_whitespace() {
                flush_shell_token(&mut segment, &mut token);
                continue;
            }
            if matches!(ch, ';' | '|') {
                if ch == '|' && chars.peek() == Some(&'|') {
                    chars.next();
                }
                flush_shell_token(&mut segment, &mut token);
                flush_shell_segment(&mut segments, &mut segment);
                continue;
            }
            if ch == '&' {
                if chars.peek() == Some(&'&') {
                    chars.next();
                }
                flush_shell_token(&mut segment, &mut token);
                flush_shell_segment(&mut segments, &mut segment);
                continue;
            }
        }

        token.push(ch);
    }

    flush_shell_token(&mut segment, &mut token);
    flush_shell_segment(&mut segments, &mut segment);
    segments
}

fn flush_shell_token(segment: &mut Vec<String>, token: &mut String) {
    let cleaned = clean_command_token(token).trim();
    if !cleaned.is_empty() {
        segment.push(cleaned.to_string());
    }
    token.clear();
}

fn flush_shell_segment(segments: &mut Vec<Vec<String>>, segment: &mut Vec<String>) {
    if !segment.is_empty() {
        segments.push(std::mem::take(segment));
    }
}

fn segment_program(segment: &[String]) -> Option<String> {
    segment.first().map(|program| {
        program
            .rsplit('/')
            .next()
            .unwrap_or(program)
            .to_ascii_lowercase()
    })
}

fn segment_is_execution_or_mutation(segment: &[String]) -> bool {
    let Some(program) = segment_program(segment) else {
        return false;
    };
    let first = segment.first().map(String::as_str).unwrap_or_default();

    if first.starts_with("scripts/") || first.starts_with("./") || first.starts_with("../") {
        return true;
    }
    if segment_has_stdout_redirection(segment) {
        return true;
    }
    if program == "find"
        && segment
            .iter()
            .any(|token| matches!(token.as_str(), "-delete" | "-exec" | "-execdir"))
    {
        return true;
    }

    match program.as_str() {
        "bash" | "sh" | "zsh" | "fish" => true,
        "npm" | "pnpm" | "yarn" | "bun" | "python" | "python3" | "pytest" | "node" | "perl"
        | "ruby" | "task" | "make" | "docker" | "open" | "curl" | "rm" | "mv" | "cp" | "mkdir"
        | "touch" | "chmod" | "chown" | "kill" | "pkill" | "tee" | "truncate" | "xargs" => true,
        "sed" => segment.iter().any(|token| token == "-i"),
        "uv" => matches!(segment.get(1).map(String::as_str), Some("run")),
        "cargo" => matches!(
            segment.get(1).map(String::as_str),
            Some("test" | "run" | "fmt" | "check" | "build" | "clippy" | "install")
        ),
        "git" => matches!(
            segment.get(1).map(String::as_str),
            Some(
                "add"
                    | "apply"
                    | "checkout"
                    | "clean"
                    | "commit"
                    | "fetch"
                    | "merge"
                    | "pull"
                    | "push"
                    | "rebase"
                    | "reset"
                    | "restore"
                    | "stash"
                    | "switch"
            )
        ),
        _ => false,
    }
}

fn segment_has_stdout_redirection(segment: &[String]) -> bool {
    segment
        .iter()
        .any(|token| token_is_stdout_redirection(token))
}

fn token_is_stdout_redirection(token: &str) -> bool {
    if matches!(token, ">" | ">>" | "1>" | "1>>" | "&>") {
        return true;
    }
    if token.starts_with("2>") {
        return false;
    }
    token.starts_with(">")
        || token.starts_with(">>")
        || token.starts_with("1>")
        || token.starts_with("1>>")
        || token.starts_with("&>")
}

fn read_only_git_summary(segments: &[Vec<String>]) -> Option<&'static str> {
    for segment in segments {
        if segment_program(segment).as_deref() != Some("git") {
            continue;
        }
        return match segment.get(1).map(String::as_str) {
            Some("status") => Some("read git status"),
            Some("log") => Some("read git log"),
            Some("diff") => Some("read git diff"),
            Some("show") => Some("read git show"),
            Some("branch" | "rev-parse") => Some("read git metadata"),
            _ => None,
        };
    }
    None
}

fn segment_reads_toolchain_metadata(segment: &[String]) -> bool {
    match segment_program(segment).as_deref() {
        Some("rustc") => segment.iter().any(|token| token == "--version"),
        Some("cargo") => matches!(
            segment.get(1).map(String::as_str),
            Some("--version" | "metadata")
        ),
        _ => false,
    }
}

fn segment_lists_files(segment: &[String]) -> bool {
    match segment_program(segment).as_deref() {
        Some("rg") => segment.iter().any(|token| token == "--files"),
        Some("find") => {
            has_arg_pair(segment, "-type", "f")
                || segment
                    .iter()
                    .any(|token| token == "-name" || token == "-iname")
        }
        _ => false,
    }
}

fn segment_lists_directories(segment: &[String]) -> bool {
    segment_program(segment).as_deref() == Some("find") && has_arg_pair(segment, "-type", "d")
}

fn list_command_summary(segments: &[Vec<String>]) -> String {
    let Some(segment) = segments
        .iter()
        .find(|segment| segment_program(segment).as_deref() == Some("ls"))
    else {
        return "listed files".to_string();
    };

    segment
        .iter()
        .skip(1)
        .find(|token| {
            !token.is_empty()
                && !token.starts_with('-')
                && !is_shell_operator(token)
                && !is_shell_redirection_token(token)
        })
        .map(|path| format!("listed {path}"))
        .unwrap_or_else(|| "listed files".to_string())
}

fn segment_reads_files(segment: &[String]) -> bool {
    matches!(
        segment_program(segment).as_deref(),
        Some("cat" | "sed" | "head" | "tail" | "nl" | "wc")
    )
}

fn command_read_paths(segments: &[Vec<String>]) -> Vec<String> {
    let mut paths = Vec::new();
    for segment in segments {
        for token in segment {
            if !is_display_read_path(token) || paths.iter().any(|existing| existing == token) {
                continue;
            }
            paths.push(token.to_string());
            if paths.len() == 3 {
                return paths;
            }
        }
    }
    paths
}

fn segment_searches(segment: &[String]) -> bool {
    match segment_program(segment).as_deref() {
        Some("rg") => !segment.iter().any(|token| token == "--files"),
        Some("grep") => true,
        _ => false,
    }
}

fn segment_reads_repository_metadata(segment: &[String]) -> bool {
    matches!(
        segment_program(segment).as_deref(),
        Some("pwd" | "du" | "whoami")
    )
}

fn segment_is_read_only_probe(segment: &[String]) -> bool {
    match segment_program(segment).as_deref() {
        Some(
            "ls" | "cat" | "pwd" | "echo" | "whoami" | "head" | "tail" | "wc" | "true" | "grep"
            | "rg" | "find" | "sed" | "du" | "printf" | "sort",
        ) => true,
        Some("git") => matches!(
            segment.get(1).map(String::as_str),
            Some("status" | "log" | "diff" | "show" | "branch" | "rev-parse")
        ),
        Some("rustc") => segment.iter().any(|token| token == "--version"),
        Some("cargo") => matches!(
            segment.get(1).map(String::as_str),
            Some("--version" | "metadata")
        ),
        _ => false,
    }
}

fn has_arg_pair(segment: &[String], key: &str, value: &str) -> bool {
    segment
        .windows(2)
        .any(|pair| pair[0] == key && pair[1] == value)
}

fn shell_script_activity_summary(command: &str) -> Option<String> {
    if command.chars().count() < 180 || !looks_like_shell_script(command) {
        return None;
    }
    let programs = shell_script_summary_programs(command);
    if programs.is_empty() {
        return Some(SHELL_SCRIPT_LINE_PREFIX.to_string());
    }
    Some(format!(
        "{SHELL_SCRIPT_LINE_PREFIX}: {}",
        programs.join(", ")
    ))
}

fn looks_like_shell_script(command: &str) -> bool {
    command.contains('\n')
        || command.contains("\r")
        || [
            " for ", " while ", " if ", " do ", " done ", " then ", " fi ",
        ]
        .iter()
        .any(|needle| command.contains(needle))
        || command.trim_start().starts_with("set ")
}

fn shell_script_summary_programs(command: &str) -> Vec<String> {
    let mut programs = Vec::new();
    for segment in shell_command_segments(command) {
        for token in segment {
            let program = clean_command_token(&token)
                .rsplit('/')
                .next()
                .unwrap_or_default()
                .to_ascii_lowercase();
            if !is_shell_script_summary_program(&program)
                || programs.iter().any(|existing| existing == &program)
            {
                continue;
            }
            programs.push(program);
            if programs.len() == 4 {
                return programs;
            }
        }
    }
    programs
}

fn is_shell_script_summary_program(program: &str) -> bool {
    matches!(
        program,
        "awk"
            | "cat"
            | "chmod"
            | "cp"
            | "curl"
            | "find"
            | "git"
            | "grep"
            | "head"
            | "ls"
            | "mkdir"
            | "mv"
            | "python"
            | "python3"
            | "rm"
            | "sed"
            | "sort"
            | "tail"
            | "touch"
            | "wc"
    )
}

fn clean_command_token(token: &str) -> &str {
    token.trim_matches(|ch: char| {
        matches!(
            ch,
            '\'' | '"' | '`' | ';' | ',' | ')' | '(' | '{' | '}' | '[' | ']' | ':'
        )
    })
}

fn is_display_read_path(token: &str) -> bool {
    if token.is_empty()
        || token.starts_with('-')
        || is_shell_operator(token)
        || matches!(
            token,
            "cat"
                | "sed"
                | "head"
                | "tail"
                | "nl"
                | "wc"
                | "grep"
                | "rg"
                | "find"
                | "sort"
                | "xargs"
                | "echo"
                | "printf"
                | "do"
                | "done"
                | "then"
                | "fi"
                | "for"
                | "in"
        )
    {
        return false;
    }

    source_extension(token).is_some()
        || matches!(
            token,
            "README" | "README.md" | "AGENTS.md" | "Cargo.toml" | "pyproject.toml"
        )
}

fn is_shell_operator(token: &str) -> bool {
    matches!(
        token,
        "|" | "||" | "&" | "&&" | ";" | "\\" | ">" | ">>" | "<"
    )
}

fn is_shell_redirection_token(token: &str) -> bool {
    token.contains('>') || token.contains('<')
}

fn terminal_interaction_node(
    events: &[EventRecord],
    event: &EventRecord,
) -> Option<TranscriptNode> {
    let stdin = raw_payload_string(event, "stdin")?;
    let command = command_for_process(events, event);
    let line = if stdin.is_empty() {
        match command {
            Some(command) => format!("checked output from {command}"),
            None => "checked command output".to_string(),
        }
    } else {
        let input = visible_terminal_input(&stdin);
        match command {
            Some(command) => format!("wrote input to {command}: {input}"),
            None => format!("wrote input: {input}"),
        }
    };
    Some(timeline_node(
        event,
        "command",
        vec![line],
        NodeStyle::Muted,
    ))
}

fn command_for_process(events: &[EventRecord], event: &EventRecord) -> Option<String> {
    let process_id = event_process_id(event)?;
    events
        .iter()
        .rev()
        .filter(|candidate| candidate.seq <= event.seq)
        .find(|candidate| {
            candidate.event_type == "exec_command.begin"
                && event_process_id(candidate).as_deref() == Some(process_id.as_str())
        })
        .and_then(event_command_display)
}

fn event_process_id(event: &EventRecord) -> Option<String> {
    event
        .payload
        .get("process_id")
        .and_then(serde_json::Value::as_str)
        .map(ToOwned::to_owned)
        .or_else(|| {
            event
                .payload
                .get("session_id")
                .and_then(serde_json::Value::as_i64)
                .map(|session_id| session_id.to_string())
        })
}

fn event_command_display(event: &EventRecord) -> Option<String> {
    let argv = event
        .payload
        .get("command")
        .and_then(serde_json::Value::as_array)?
        .iter()
        .map(|part| part.as_str().map(ToOwned::to_owned))
        .collect::<Option<Vec<_>>>()?;
    (!argv.is_empty()).then(|| command_display_from_argv(&argv))
}

fn command_display_from_argv(argv: &[String]) -> String {
    if argv.len() >= 3 && is_shell_program(&argv[0]) && matches!(argv[1].as_str(), "-c" | "-lc") {
        return argv[2].clone();
    }
    argv.iter()
        .map(|part| shell_quote_display_arg(part))
        .collect::<Vec<_>>()
        .join(" ")
}

fn is_shell_program(program: &str) -> bool {
    matches!(
        program.rsplit('/').next().unwrap_or(program),
        "bash" | "sh" | "zsh" | "fish"
    )
}

fn shell_quote_display_arg(arg: &str) -> String {
    if !arg.is_empty()
        && arg
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | '.' | '/' | ':' | '='))
    {
        return arg.to_string();
    }
    format!("'{}'", arg.replace('\'', "'\\''"))
}

fn visible_terminal_input(stdin: &str) -> String {
    stdin.chars().flat_map(char::escape_default).collect()
}

fn command_event_group(name: &str) -> &'static str {
    match name {
        "shell" => "shell",
        _ => "command",
    }
}

fn provider_text_content(text: &str) -> Option<String> {
    let value = serde_json::from_str::<serde_json::Value>(text).ok()?;
    let content = value.as_array()?;
    let chunks = content
        .iter()
        .filter_map(provider_text_content_part)
        .collect::<Vec<_>>();
    Some(chunks.join("\n"))
}

fn provider_text_content_part(value: &serde_json::Value) -> Option<String> {
    if value.get("type").and_then(serde_json::Value::as_str) != Some("text") {
        return None;
    }
    value
        .get("text")
        .and_then(serde_json::Value::as_str)
        .map(ToOwned::to_owned)
}

fn command_output_summary_lines(text: &str) -> Vec<String> {
    let visible = text
        .lines()
        .map(str::trim_end)
        .filter(|line| !line.trim().is_empty())
        .collect::<Vec<_>>();
    let Some(first) = visible.first() else {
        return Vec::new();
    };
    let first = truncate_inline(first, 140);
    if visible.len() == 1 {
        vec![format!("output: {first}")]
    } else {
        let omitted = visible.len().saturating_sub(1);
        vec![format!(
            "output: {first} (+{omitted} line{})",
            plural(omitted)
        )]
    }
}

fn command_failed_summary_lines(error: &str) -> Vec<String> {
    let plain_text = provider_text_content(error).unwrap_or_else(|| error.to_string());
    let lines = command_output_summary_lines(&plain_text);
    if lines.is_empty() {
        return vec!["failed".to_string()];
    }
    lines
        .into_iter()
        .map(|line| match line.strip_prefix("output: ") {
            Some(rest) => format!("failed: {rest}"),
            None => line,
        })
        .collect()
}

fn browser_command_output_lines(event: &EventRecord) -> Vec<String> {
    let Some(text) = payload_string(event, "text") else {
        return Vec::new();
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
        return Vec::new();
    };
    browser_command_value_lines(&value)
}

fn browser_command_value_lines(value: &serde_json::Value) -> Vec<String> {
    if is_routine_browser_status_output(value) {
        return Vec::new();
    }

    let mut lines = Vec::new();
    lines.push(browser_command_headline(value));
    if let Some(reason) = summary_value_string(value, "reason")
        .or_else(|| summary_value_string(value, "error"))
        .or_else(|| summary_value_string(value, "raw_error"))
    {
        lines.push(truncate_inline(&reason, 180));
    }
    lines.extend(browser_profile_choice_lines(value));
    if let Some(profile_id) = summary_value_string(value, "profile_id") {
        lines.push(format!("profile: {}", truncate_inline(&profile_id, 120)));
    }
    if let Some(live_url) = summary_value_string(value, "live_url") {
        lines.push(format!("live view {}", compact_url(&live_url)));
    } else if let Some(url) = summary_value_string(value, "url") {
        lines.push(format!("open {}", compact_url(&url)));
    }
    if let Some(active_scripts) = value
        .get("active_scripts")
        .and_then(serde_json::Value::as_array)
        .filter(|scripts| !scripts.is_empty())
    {
        lines.push(format!(
            "{} active browser script{}",
            active_scripts.len(),
            plural(active_scripts.len())
        ));
    }
    if let Some(next_step) = summary_value_string(value, "next_step") {
        lines.push(format!("Next: {}", truncate_inline(&next_step, 180)));
    }
    lines
}

fn is_routine_browser_status_output(value: &serde_json::Value) -> bool {
    if browser_command_output_has_visible_result(value) {
        return false;
    }
    if let Some(connection) = summary_value_string(value, "connection") {
        return matches!(
            connection.as_str(),
            "connected" | "not-configured" | "disconnected"
        );
    }
    if let Some(status) = summary_value_string(value, "status") {
        return matches!(
            status.as_str(),
            "connected" | "ok" | "not-configured" | "disconnected"
        );
    }
    false
}

fn browser_command_output_has_visible_result(value: &serde_json::Value) -> bool {
    browser_profiles(value).is_some()
        || summary_value_string(value, "profile_id").is_some()
        || matches!(
            summary_value_string(value, "status").as_deref(),
            Some("needs-user-action" | "failed")
        )
        || summary_value_string(value, "reason").is_some()
        || summary_value_string(value, "error").is_some()
        || summary_value_string(value, "raw_error").is_some()
        || value
            .get("active_scripts")
            .and_then(serde_json::Value::as_array)
            .is_some_and(|scripts| !scripts.is_empty())
}

fn browser_command_headline(value: &serde_json::Value) -> String {
    if let Some(connection) = summary_value_string(value, "connection") {
        let browser =
            summary_value_string(value, "browser").unwrap_or_else(|| "browser".to_string());
        return match connection.as_str() {
            "connected" => format!("connected to {browser}"),
            "disconnected" => format!("{browser} disconnected"),
            "not-configured" => "browser not configured".to_string(),
            other => format!("{browser} {other}"),
        };
    }
    let status = summary_value_string(value, "status");
    match status.as_deref() {
        Some("needs-user-action") => {
            if browser_profiles(value).is_some() {
                "choose browser profile".to_string()
            } else if value.get("url").is_some() {
                "browser needs permission".to_string()
            } else {
                "browser needs user action".to_string()
            }
        }
        Some("ok") => {
            if browser_profiles(value).is_some() {
                "browser profiles found".to_string()
            } else if value.get("profile_id").is_some() {
                "browser profile selected".to_string()
            } else {
                "browser ok".to_string()
            }
        }
        Some("failed") => "browser failed".to_string(),
        Some(status) => format!("browser {status}"),
        None => "browser updated".to_string(),
    }
}

fn browser_profile_choice_lines(value: &serde_json::Value) -> Vec<String> {
    let Some(profiles) = browser_profiles(value) else {
        return Vec::new();
    };
    let mut lines = profiles
        .iter()
        .take(5)
        .enumerate()
        .map(|(idx, profile)| browser_profile_line(idx + 1, profile))
        .collect::<Vec<_>>();
    if profiles.len() > lines.len() {
        lines.push(format!("... +{} profiles", profiles.len() - lines.len()));
    }
    lines
}

fn browser_profiles(value: &serde_json::Value) -> Option<&Vec<serde_json::Value>> {
    value
        .get("local_profiles")
        .or_else(|| value.get("profiles"))
        .or_else(|| value.get("available_profiles"))
        .and_then(serde_json::Value::as_array)
        .filter(|profiles| !profiles.is_empty())
}

fn browser_profile_line(index: usize, profile: &serde_json::Value) -> String {
    let display = summary_value_string(profile, "display_name")
        .or_else(|| summary_value_string(profile, "profile_name"))
        .or_else(|| summary_value_string(profile, "id"))
        .unwrap_or_else(|| "profile".to_string());
    let detail = summary_value_string(profile, "profile_dir")
        .filter(|detail| !display.contains(&format!("({detail})")));
    match detail {
        Some(detail) => format!(
            "{index}. {} ({})",
            truncate_inline(&display, 100),
            truncate_inline(&detail, 60)
        ),
        None => format!("{index}. {}", truncate_inline(&display, 120)),
    }
}

fn browser_script_summary_lines(event: &EventRecord) -> Vec<String> {
    let Some(summary) = event
        .payload
        .get("summary")
        .and_then(serde_json::Value::as_array)
    else {
        return Vec::new();
    };
    let mut lines = summary
        .iter()
        .filter_map(browser_script_summary_record_line)
        .take(6)
        .collect::<Vec<_>>();
    if summary.len() > lines.len() {
        lines.push(format!("... +{} summaries", summary.len() - lines.len()));
    }
    lines
}

fn browser_script_summary_record_line(value: &serde_json::Value) -> Option<String> {
    let kind = summary_value_string(value, "kind").unwrap_or_else(|| "summary".to_string());
    let message = summary_value_string(value, "message");
    match kind.as_str() {
        "page" | "opened" | "navigation" | "navigated" => {
            let mut line = if let Some(url) = summary_value_string(value, "url") {
                format!("page: {}", compact_url(&url))
            } else if let Some(message) = message.as_deref() {
                format!("page: {}", truncate_inline(message, 140))
            } else {
                "page: updated".to_string()
            };
            if let Some(title) = summary_value_string(value, "title") {
                line.push_str(" - ");
                line.push_str(&truncate_inline(&title, 80));
            }
            Some(line)
        }
        "click" | "clicked" => {
            let target = summary_value_string(value, "text")
                .or_else(|| summary_value_string(value, "label"))
                .or_else(|| summary_value_string(value, "selector"))
                .or(message)
                .unwrap_or_else(|| "target".to_string());
            let mut line = format!("clicked: {}", truncate_inline(&target, 100));
            if let Some(url) =
                summary_value_string(value, "href").or_else(|| summary_value_string(value, "url"))
            {
                line.push_str(" -> ");
                line.push_str(&compact_url(&url));
            }
            Some(line)
        }
        "input" | "typed" | "fill" | "filled" => {
            let target = summary_value_string(value, "label")
                .or_else(|| summary_value_string(value, "selector"))
                .or(message)
                .unwrap_or_else(|| "field".to_string());
            Some(format!("filled: {}", truncate_inline(&target, 120)))
        }
        "extract" | "extracted" => {
            if let Some(message) = message {
                return Some(truncate_inline(&message, 140));
            }
            if let Some(count) = summary_value_string(value, "count") {
                return Some(format!("extracted: {count} items"));
            }
            Some("extracted: data".to_string())
        }
        "screenshot" | "image" => {
            let label = summary_value_string(value, "label")
                .or(message)
                .unwrap_or_else(|| "screenshot".to_string());
            Some(format!("screenshot: {}", truncate_inline(&label, 120)))
        }
        _ => {
            if let Some(message) = message {
                return Some(truncate_inline(&message, 140));
            }
            if let Some(url) = summary_value_string(value, "url") {
                return Some(format!("{kind}: {}", compact_url(&url)));
            }
            compact_summary_json(value).map(|summary| format!("{kind}: {summary}"))
        }
    }
}

fn browser_script_structured_output_lines(event: &EventRecord) -> Vec<String> {
    let Some(outputs) = event
        .payload
        .get("outputs")
        .and_then(serde_json::Value::as_array)
        .filter(|outputs| !outputs.is_empty())
    else {
        return Vec::new();
    };
    let labels = outputs
        .iter()
        .filter_map(|output| summary_value_string(output, "label"))
        .take(3)
        .collect::<Vec<_>>();
    if labels.is_empty() {
        return vec![format!(
            "{} structured output{}",
            outputs.len(),
            plural(outputs.len())
        )];
    }
    let mut line = format!("structured output: {}", labels.join(", "));
    if outputs.len() > labels.len() {
        line.push_str(&format!(" (+{})", outputs.len() - labels.len()));
    }
    vec![line]
}

fn browser_script_text_preview_lines(text: &str) -> Vec<String> {
    let trimmed = text.trim();
    if trimmed.starts_with("browser_script is still running.") {
        return Vec::new();
    }
    let visible = text
        .lines()
        .map(str::trim_end)
        .filter(|line| !line.is_empty())
        .filter(|line| !is_browser_script_runtime_instruction_line(line))
        .collect::<Vec<_>>();
    let mut out = visible
        .iter()
        .take(2)
        .map(|line| truncate_inline(line, 180))
        .collect::<Vec<_>>();
    if visible.len() > out.len() {
        out.push(format!("... +{} lines", visible.len() - out.len()));
    }
    out
}

fn is_browser_script_runtime_instruction_line(line: &str) -> bool {
    let line = line.trim_start();
    line.starts_with("run_id:")
        || line.starts_with("Next:")
        || line.starts_with("Next step:")
        || line == "browser_script is still running."
}

fn summary_value_string(value: &serde_json::Value, key: &str) -> Option<String> {
    value.get(key).and_then(|value| match value {
        serde_json::Value::String(text) => {
            let text = text.trim();
            (!text.is_empty()).then(|| text.to_string())
        }
        serde_json::Value::Number(_) | serde_json::Value::Bool(_) => Some(value.to_string()),
        _ => None,
    })
}

fn compact_summary_json(value: &serde_json::Value) -> Option<String> {
    serde_json::to_string(value)
        .ok()
        .map(|text| truncate_inline(&text, 160))
        .filter(|text| !text.is_empty())
}

fn artifact_created_node(event: &EventRecord, _state: &WorkbenchState) -> Option<TranscriptNode> {
    let artifact = event.payload.get("artifact")?;
    let path = artifact
        .get("path")
        .and_then(serde_json::Value::as_str)
        .map(ToOwned::to_owned)?;
    let kind = artifact
        .get("kind")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("artifact");
    Some(timeline_node(
        event,
        "artifacts created",
        vec![format!("{kind} {path}")],
        NodeStyle::Normal,
    ))
}

fn tool_image_node(event: &EventRecord) -> TranscriptNode {
    let image = event.payload.get("image");
    let path = image
        .and_then(|image| image.get("path"))
        .and_then(serde_json::Value::as_str)
        .map(ToOwned::to_owned);
    let label = image
        .and_then(|image| image.get("label"))
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|label| !label.is_empty())
        .map(ToOwned::to_owned);
    TranscriptNode {
        id: format!("{}:{}", event.session_id, event.seq),
        seq: event.seq,
        revision: event.seq.max(0) as u64,
        kind: TranscriptKind::ToolImage {
            path,
            label,
            took_screenshot: image
                .and_then(|image| image.get("source"))
                .and_then(serde_json::Value::as_str)
                == Some("screenshot"),
        },
    }
}

fn tool_image_display_lines(
    path: Option<&str>,
    label: Option<&str>,
    took_screenshot: bool,
    width: u16,
) -> Vec<Line<'static>> {
    let line = path
        .map(ToOwned::to_owned)
        .or_else(|| label.map(|label| format!("image: {label}")))
        .unwrap_or_else(|| "image attached".to_string());
    let group = if took_screenshot {
        "took screenshot"
    } else {
        "read image"
    };
    grouped_lines(group, &[line], NodeStyle::Normal, width)
}

fn native_tool_image_display_lines(
    path: Option<&str>,
    label: Option<&str>,
    took_screenshot: bool,
    width: u16,
) -> Vec<NativeLine> {
    let line = path
        .map(ToOwned::to_owned)
        .or_else(|| label.map(|label| format!("image: {label}")))
        .unwrap_or_else(|| "image attached".to_string());
    let group = if took_screenshot {
        "took screenshot"
    } else {
        "read image"
    };
    native_grouped_lines(group, &[line], NodeStyle::Normal, width)
}

fn tool_image_plain_lines(
    path: Option<&str>,
    label: Option<&str>,
    took_screenshot: bool,
) -> Vec<String> {
    let line = path
        .map(ToOwned::to_owned)
        .or_else(|| label.map(|label| format!("image: {label}")))
        .unwrap_or_else(|| "image attached".to_string());
    let header = if took_screenshot {
        "• took screenshot"
    } else {
        "• read image"
    };
    vec![
        header.to_string(),
        format!("{GROUP_VALUE_LAST_PREFIX}{line}"),
    ]
}

fn active_tool_status(name: &str) -> Option<(&'static str, &'static str)> {
    match name {
        "browser" => Some(("browser", "running browser command")),
        "browser_script" => Some(("browser", "running browser script")),
        "python" => Some(("python", "running browser Python")),
        "shell" => Some(("shell", "running command")),
        "exec_command" => Some(("command", "running command")),
        "write_stdin" => Some(("command", "writing to command")),
        "apply_patch" => Some(("edit", "applying patch")),
        "view_image" => Some(("image", "inspecting image")),
        "update_plan" => Some(("plan", "updating plan")),
        _ => None,
    }
}

fn should_show_generic_tool_output_text(name: &str) -> bool {
    !is_known_tool_with_domain_events(name)
}

fn is_command_tool_output(name: &str) -> bool {
    matches!(name, "shell" | "exec_command" | "write_stdin")
}

fn tool_output_group(name: &str) -> &str {
    match name {
        "browser" | "browser_script" => "browser",
        "python" => "python",
        "shell" => "shell",
        "exec_command" | "write_stdin" => "command",
        _ => "tool",
    }
}

fn is_known_tool_with_domain_events(name: &str) -> bool {
    matches!(
        name,
        "done"
            | "python"
            | "apply_patch"
            | "read_file"
            | "search_files"
            | "list_files"
            | "view_image"
            | "update_plan"
            | "spawn_agent"
            | "wait_agent"
            | "send_input"
            | "send_message"
            | "followup_task"
            | "list_agents"
            | "close_agent"
            | "resume_agent"
    )
}

fn timeline_node(
    event: &EventRecord,
    group: &str,
    lines: Vec<String>,
    style: NodeStyle,
) -> TranscriptNode {
    TranscriptNode {
        id: format!("{}:{}", event.session_id, event.seq),
        seq: event.seq,
        revision: event.seq.max(0) as u64,
        kind: TranscriptKind::Timeline {
            group: group.to_string(),
            lines,
            style,
        },
    }
}

fn prompt_lines(text: &str, followup: bool, width: u16) -> Vec<Line<'static>> {
    prompt_lines_with_status(text, followup, width, None)
}

#[derive(Clone, Copy)]
enum ShimmerMode {
    Static,
    AnimatedAt(usize),
}

fn pending_status_lines(
    status: &str,
    detail: Option<&str>,
    shimmer: ShimmerMode,
) -> Vec<Line<'static>> {
    let mut spans = vec![Span::styled("• ".to_string(), dim())];
    spans.extend(match shimmer {
        ShimmerMode::Static => vec![Span::styled(status.to_string(), muted())],
        ShimmerMode::AnimatedAt(phase) => shimmer_spans(status, phase, muted()),
    });
    if let Some(detail) = detail.filter(|detail| !detail.trim().is_empty()) {
        spans.push(Span::styled("  ".to_string(), dim()));
        spans.push(Span::styled(detail.to_string(), muted()));
    }
    vec![Line::from(spans)]
}

fn pending_status_text(status: &str, detail: Option<&str>) -> String {
    match detail.filter(|detail| !detail.trim().is_empty()) {
        Some(detail) => format!("• {status}  {detail}"),
        None => format!("• {status}"),
    }
}

fn shimmer_spans(text: &str, phase: usize, base: Style) -> Vec<Span<'static>> {
    if text.is_empty() {
        return Vec::new();
    }
    let chars = text.chars().collect::<Vec<_>>();
    let center = (phase % chars.len().max(1)) as isize;
    let mut spans = Vec::new();
    let mut pending = String::new();
    let mut pending_style = base;
    let mut have_pending = false;

    for (idx, ch) in chars.into_iter().enumerate() {
        let distance = (idx as isize - center).unsigned_abs();
        let style = if distance <= 1 {
            accent()
        } else if distance <= 3 {
            text_style()
        } else {
            base
        };
        if have_pending && style == pending_style {
            pending.push(ch);
        } else {
            if have_pending {
                spans.push(Span::styled(std::mem::take(&mut pending), pending_style));
            }
            pending.push(ch);
            pending_style = style;
            have_pending = true;
        }
    }
    if have_pending {
        spans.push(Span::styled(pending, pending_style));
    }
    spans
}

fn prompt_lines_with_status(
    text: &str,
    _followup: bool,
    width: u16,
    status: Option<&str>,
) -> Vec<Line<'static>> {
    let content_width = width.saturating_sub(2).max(1) as usize;
    // Pad the content to the full width so the highlight background reads as a
    // solid block rather than only wrapping the glyphs.
    let pad_to_width = |value: &str| -> String {
        let used = display_width(value);
        let mut out = value.to_string();
        out.extend(std::iter::repeat(' ').take(content_width.saturating_sub(used)));
        out
    };
    let mut rows = Vec::new();
    for (idx, source) in text.lines().enumerate() {
        let prefix = if idx == 0 { "> " } else { "  " };
        for (wrap_idx, wrapped) in wrap_plain(source, content_width as u16) {
            let visible_prefix = if wrap_idx == 0 { prefix } else { "  " };
            rows.push((visible_prefix.to_string(), wrapped));
        }
    }
    if rows.is_empty() {
        rows.push(("> ".to_string(), String::new()));
    }
    let last_idx = rows.len().saturating_sub(1);
    rows.into_iter()
        .enumerate()
        .map(|(idx, (prefix, wrapped))| {
            let mut spans = vec![Span::styled(prefix, user_prompt_accent())];
            let can_fit_status = status.is_some_and(|status| {
                let status_width = display_width(status).saturating_add(2);
                display_width(&wrapped).saturating_add(status_width) <= content_width
            });
            if idx == last_idx && can_fit_status {
                let status = status.unwrap_or_default();
                let content_used = display_width(&wrapped);
                let status_gap = 2usize;
                let status_width = display_width(status);
                let tail_gap =
                    content_width.saturating_sub(content_used + status_gap + status_width);
                spans.push(Span::styled(wrapped, user_prompt_text()));
                spans.push(Span::styled(" ".repeat(status_gap), user_prompt_text()));
                spans.push(Span::styled(status.to_string(), user_prompt_muted()));
                spans.push(Span::styled(" ".repeat(tail_gap), user_prompt_text()));
            } else {
                spans.push(Span::styled(pad_to_width(&wrapped), user_prompt_text()));
            }
            Line::from(spans)
        })
        .collect()
}

fn display_width(value: &str) -> usize {
    value.chars().map(|ch| ch.width().unwrap_or(0).max(1)).sum()
}

fn markdown_cell_lines(markdown: &str, width: u16, mode: DisplayMode) -> Vec<Line<'static>> {
    let _ = mode;
    let mut lines = render_markdown_lines(markdown.trim_end(), width);
    if lines.is_empty() {
        lines.push(Line::from(""));
    }
    lines
}

fn native_markdown_cell_lines(markdown: &str, width: u16, mode: DisplayMode) -> Vec<NativeLine> {
    plain_native_lines(markdown_cell_lines(markdown, width, mode))
}

fn streaming_markdown_cell_lines(
    markdown: &str,
    width: u16,
    mode: DisplayMode,
) -> Vec<Line<'static>> {
    let Some(holdback_start) = markdown_table_holdback_start(markdown) else {
        return markdown_cell_lines(markdown, width, mode);
    };

    let mut lines = markdown_stream_stable_prefix_lines(markdown, holdback_start, width, mode);
    lines.extend(raw_markdown_cell_lines(
        &markdown[holdback_start.min(markdown.len())..],
        width,
    ));
    lines
}

fn native_markdown_stream_lines(markdown: &str, width: u16, mode: DisplayMode) -> Vec<NativeLine> {
    plain_native_lines(streaming_markdown_cell_lines(markdown, width, mode))
}

fn native_markdown_stable_prefix_lines(
    markdown: &str,
    width: u16,
    mode: DisplayMode,
) -> Vec<NativeLine> {
    let Some(holdback_start) = markdown_table_holdback_start(markdown) else {
        return native_markdown_cell_lines(markdown, width, mode);
    };

    plain_native_lines(markdown_stream_stable_prefix_lines(
        markdown,
        holdback_start,
        width,
        mode,
    ))
}

fn markdown_stream_stable_prefix_lines(
    markdown: &str,
    holdback_start: usize,
    width: u16,
    mode: DisplayMode,
) -> Vec<Line<'static>> {
    let stable_source = &markdown[..holdback_start.min(markdown.len())];
    if stable_source.trim().is_empty() {
        Vec::new()
    } else {
        markdown_cell_lines(stable_source, width, mode)
    }
}

fn raw_markdown_cell_lines(markdown: &str, width: u16) -> Vec<Line<'static>> {
    let source = markdown.trim_end();
    if source.is_empty() {
        return Vec::new();
    }

    let mut lines = Vec::new();
    for raw_line in source.split('\n') {
        let raw_line = raw_line.strip_suffix('\r').unwrap_or(raw_line);
        for (_, wrapped) in wrap_plain(raw_line, width) {
            lines.push(Line::from(vec![Span::styled(wrapped, text_style())]));
        }
    }
    lines
}

fn markdown_table_holdback_start(markdown: &str) -> Option<usize> {
    markdown_table_holdback_state(markdown).map(|state| match state {
        MarkdownTableHoldbackState::PendingHeader { header_start } => header_start,
        MarkdownTableHoldbackState::Confirmed { table_start } => table_start,
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MarkdownTableHoldbackState {
    PendingHeader { header_start: usize },
    Confirmed { table_start: usize },
}

fn markdown_table_holdback_state(markdown: &str) -> Option<MarkdownTableHoldbackState> {
    let lines = markdown_line_spans(markdown);
    let mut fence_tracker = MarkdownFenceTracker::new();
    let mut previous_line: Option<PreviousMarkdownLine> = None;
    let mut pending_header_start = None;
    let mut active_table_start = None;

    for line in lines {
        let fence_kind = fence_tracker.kind();
        let can_scan_table = fence_kind == MarkdownFenceKind::Outside
            && !is_markdown_table_block_interrupt(line.text);
        let candidate = if can_scan_table {
            markdown_table_candidate_text(line.text)
        } else {
            None
        };
        let is_header = candidate.is_some_and(is_markdown_table_header);
        let is_delimiter = candidate.is_some_and(is_markdown_table_delimiter);
        let is_delimiter_prefix = candidate.is_some_and(is_markdown_table_delimiter_prefix);
        let continues_active_table =
            active_table_start.is_some() && !line.text.trim().is_empty() && candidate.is_some();

        if active_table_start.is_some() && !continues_active_table {
            active_table_start = None;
        }

        if let Some(previous) = previous_line {
            if previous.fence_kind == MarkdownFenceKind::Outside
                && fence_kind == MarkdownFenceKind::Outside
                && can_scan_table
                && previous.is_header
                && is_delimiter
            {
                active_table_start = Some(previous.start);
            }
        }

        if active_table_start.is_some() {
            pending_header_start = None;
        } else if !line.text.trim().is_empty() {
            pending_header_start = if can_scan_table && is_header {
                if is_delimiter_prefix {
                    previous_line
                        .filter(|previous| {
                            previous.fence_kind == MarkdownFenceKind::Outside && previous.is_header
                        })
                        .map(|previous| previous.start)
                        .or(Some(line.start))
                } else {
                    Some(line.start)
                }
            } else {
                None
            };
        } else {
            pending_header_start = None;
        }

        previous_line = Some(PreviousMarkdownLine {
            start: line.start,
            fence_kind,
            is_header,
        });
        fence_tracker.advance(line.text);
    }

    if let Some(table_start) = active_table_start {
        Some(MarkdownTableHoldbackState::Confirmed { table_start })
    } else {
        pending_header_start
            .map(|header_start| MarkdownTableHoldbackState::PendingHeader { header_start })
    }
}

#[derive(Clone, Copy)]
struct PreviousMarkdownLine {
    start: usize,
    fence_kind: MarkdownFenceKind,
    is_header: bool,
}

#[derive(Clone, Copy)]
struct MarkdownLineSpan<'a> {
    start: usize,
    text: &'a str,
}

fn markdown_line_spans(markdown: &str) -> Vec<MarkdownLineSpan<'_>> {
    let mut spans = Vec::new();
    let mut start = 0usize;
    for (idx, ch) in markdown.char_indices() {
        if ch == '\n' {
            let mut end = idx;
            if end > start && markdown.as_bytes()[end - 1] == b'\r' {
                end -= 1;
            }
            spans.push(MarkdownLineSpan {
                start,
                text: &markdown[start..end],
            });
            start = idx + 1;
        }
    }
    if start < markdown.len() {
        spans.push(MarkdownLineSpan {
            start,
            text: &markdown[start..],
        });
    }
    spans
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MarkdownFenceKind {
    Outside,
    Markdown,
    Other,
}

struct MarkdownFenceTracker {
    state: Option<(char, usize, MarkdownFenceKind)>,
}

impl MarkdownFenceTracker {
    fn new() -> Self {
        Self { state: None }
    }

    fn kind(&self) -> MarkdownFenceKind {
        self.state
            .map_or(MarkdownFenceKind::Outside, |(_, _, kind)| kind)
    }

    fn advance(&mut self, raw_line: &str) {
        let leading_spaces = raw_line
            .as_bytes()
            .iter()
            .take_while(|byte| **byte == b' ')
            .count();
        if leading_spaces > 3 {
            return;
        }

        let trimmed = &raw_line[leading_spaces..];
        let fence_scan_text = strip_markdown_blockquote_prefix(trimmed);
        let Some((marker, len)) = parse_markdown_fence_marker(fence_scan_text) else {
            return;
        };

        if let Some((open_marker, open_len, _)) = self.state {
            if marker == open_marker && len >= open_len && fence_scan_text[len..].trim().is_empty()
            {
                self.state = None;
            }
            return;
        }

        let kind = if is_markdown_fence_info(fence_scan_text, len) {
            MarkdownFenceKind::Markdown
        } else {
            MarkdownFenceKind::Other
        };
        self.state = Some((marker, len, kind));
    }
}

fn parse_markdown_fence_marker(line: &str) -> Option<(char, usize)> {
    let first = line.as_bytes().first().copied()?;
    if first != b'`' && first != b'~' {
        return None;
    }
    let len = line.bytes().take_while(|byte| *byte == first).count();
    (len >= 3).then_some((first as char, len))
}

fn is_markdown_fence_info(trimmed_line: &str, marker_len: usize) -> bool {
    let info = trimmed_line[marker_len..]
        .split_whitespace()
        .next()
        .unwrap_or_default();
    info.eq_ignore_ascii_case("md") || info.eq_ignore_ascii_case("markdown")
}

fn markdown_table_candidate_text(line: &str) -> Option<&str> {
    let stripped = strip_markdown_blockquote_prefix(line).trim();
    parse_markdown_table_segments(stripped).map(|_| stripped)
}

fn strip_markdown_blockquote_prefix(line: &str) -> &str {
    let mut rest = line.trim_start();
    loop {
        let Some(stripped) = rest.strip_prefix('>') else {
            return rest;
        };
        rest = stripped.strip_prefix(' ').unwrap_or(stripped).trim_start();
    }
}

fn parse_markdown_table_segments(line: &str) -> Option<Vec<&str>> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return None;
    }

    let has_outer_pipe = trimmed.starts_with('|') || trimmed.ends_with('|');
    let content = trimmed.strip_prefix('|').unwrap_or(trimmed);
    let content = content.strip_suffix('|').unwrap_or(content);
    let raw_segments = split_unescaped_markdown_pipe(content);
    if !has_outer_pipe && raw_segments.len() <= 1 {
        return None;
    }

    let segments: Vec<&str> = raw_segments.into_iter().map(str::trim).collect();
    (!segments.is_empty()).then_some(segments)
}

fn split_unescaped_markdown_pipe(content: &str) -> Vec<&str> {
    let mut segments = Vec::with_capacity(8);
    let mut start = 0;
    let bytes = content.as_bytes();
    let mut idx = 0;
    while idx < bytes.len() {
        if bytes[idx] == b'\\' {
            idx += 2;
        } else if bytes[idx] == b'|' {
            segments.push(&content[start..idx]);
            start = idx + 1;
            idx += 1;
        } else {
            idx += 1;
        }
    }
    segments.push(&content[start..]);
    segments
}

fn is_markdown_table_header(line: &str) -> bool {
    parse_markdown_table_segments(line)
        .is_some_and(|segments| segments.iter().any(|segment| !segment.trim().is_empty()))
}

fn is_markdown_table_delimiter(line: &str) -> bool {
    parse_markdown_table_segments(line).is_some_and(|segments| {
        segments
            .into_iter()
            .all(is_markdown_table_delimiter_segment)
    })
}

fn is_markdown_table_delimiter_prefix(line: &str) -> bool {
    let trimmed = line.trim();
    !trimmed.is_empty()
        && trimmed.contains('-')
        && trimmed
            .chars()
            .all(|ch| ch == '|' || ch == ':' || ch == '-' || ch.is_whitespace())
}

fn is_markdown_table_block_interrupt(line: &str) -> bool {
    let trimmed = line.trim_start();
    is_markdown_heading_start(trimmed)
        || trimmed.starts_with('>')
        || parse_markdown_fence_marker(trimmed).is_some()
}

fn is_markdown_heading_start(trimmed: &str) -> bool {
    let marker_len = trimmed.bytes().take_while(|byte| *byte == b'#').count();
    (1..=6).contains(&marker_len)
        && trimmed
            .as_bytes()
            .get(marker_len)
            .is_some_and(u8::is_ascii_whitespace)
}

fn is_markdown_table_delimiter_segment(segment: &str) -> bool {
    let trimmed = segment.trim();
    if trimmed.is_empty() {
        return false;
    }
    let inner = trimmed.strip_prefix(':').unwrap_or(trimmed);
    let inner = inner.strip_suffix(':').unwrap_or(inner);
    inner.len() >= 3 && inner.chars().all(|ch| ch == '-')
}

fn notice_lines(text: &str, width: u16) -> Vec<Line<'static>> {
    wrap_plain(text.trim_end(), width)
        .into_iter()
        .map(|(_, row)| Line::from(styled_notice_spans(&row, activity_task())))
        .collect()
}

fn source_display_lines(source: &str, width: u16) -> Vec<Line<'static>> {
    let prefix = "source ";
    let first_width = width.saturating_sub(prefix.chars().count() as u16).max(1);
    let continuation_prefix = "       ";
    let continuation_width = width
        .saturating_sub(continuation_prefix.chars().count() as u16)
        .max(1);
    let mut lines = Vec::new();
    for (idx, wrapped) in wrap_plain(source, first_width) {
        let prefix_text = if idx == 0 {
            prefix
        } else {
            continuation_prefix
        };
        let available = if idx == 0 {
            first_width
        } else {
            continuation_width
        };
        let fragments = if idx == 0 {
            vec![wrapped]
        } else {
            wrap_plain(&wrapped, available)
                .into_iter()
                .map(|(_, line)| line)
                .collect()
        };
        for (fragment_idx, fragment) in fragments.into_iter().enumerate() {
            let visible_prefix = if idx == 0 && fragment_idx == 0 {
                prefix_text
            } else {
                continuation_prefix
            };
            lines.push(Line::from(vec![
                Span::styled(visible_prefix.to_string(), muted()),
                Span::styled(fragment, link()),
            ]));
        }
    }
    lines
}

fn collapsible_timeline_group(kind: &TranscriptKind) -> Option<&str> {
    match kind {
        TranscriptKind::Timeline { group, .. } if matches!(group.as_str(), "shell" | "command") => {
            Some(group.as_str())
        }
        _ => None,
    }
}

fn collapsed_timeline_group_lines(
    nodes: &[&TranscriptNode],
    group: &str,
    width: u16,
) -> Vec<Line<'static>> {
    let header_style = nodes
        .iter()
        .find_map(|node| match &node.kind {
            TranscriptKind::Timeline { style, .. } => Some(*style),
            _ => None,
        })
        .unwrap_or(NodeStyle::Normal);
    let mut lines = vec![Line::from(vec![
        Span::styled("• ", dim()),
        Span::styled(group.to_string(), group_label_style(group, header_style)),
    ])];

    let prefix_width = display_width(GROUP_VALUE_LAST_PREFIX) as u16;
    let content_width = width.saturating_sub(prefix_width).max(1);
    let values = compact_shell_cluster_values(nodes);
    let value_rows = values
        .iter()
        .flat_map(|value| {
            styled_wrapped_value_rows(group, &value.text, body_style(value.style), content_width)
                .into_iter()
        })
        .collect::<Vec<_>>();
    let last_idx = value_rows.len().saturating_sub(1);
    for (idx, wrapped) in value_rows.into_iter().enumerate() {
        let prefix = if idx == last_idx {
            GROUP_VALUE_LAST_PREFIX
        } else {
            GROUP_VALUE_RAIL_PREFIX
        };
        let mut spans = vec![Span::styled(prefix.to_string(), dim())];
        spans.extend(wrapped);
        lines.push(Line::from(spans));
    }
    lines
}

fn collapsed_timeline_group_native_lines(
    nodes: &[&TranscriptNode],
    group: &str,
    width: u16,
) -> Vec<NativeLine> {
    let header_style = nodes
        .iter()
        .find_map(|node| match &node.kind {
            TranscriptKind::Timeline { style, .. } => Some(*style),
            _ => None,
        })
        .unwrap_or(NodeStyle::Normal);
    let mut lines = vec![NativeLine::plain(Line::from(vec![
        Span::styled("• ", dim()),
        Span::styled(group.to_string(), group_label_style(group, header_style)),
    ]))];

    let prefix_width = display_width(GROUP_VALUE_LAST_PREFIX) as u16;
    let content_width = width.saturating_sub(prefix_width).max(1);
    let values = compact_shell_cluster_values(nodes);
    let value_rows = values
        .iter()
        .flat_map(|value| {
            native_wrapped_value_fragments(
                group,
                &value.text,
                body_style(value.style),
                content_width,
            )
        })
        .collect::<Vec<_>>();
    let last_idx = value_rows.len().saturating_sub(1);
    for (idx, row) in value_rows.into_iter().enumerate() {
        let prefix = if idx == last_idx {
            GROUP_VALUE_LAST_PREFIX
        } else {
            GROUP_VALUE_RAIL_PREFIX
        };
        lines.push(native_line_from_prefixed_fragments(prefix, row));
    }
    lines
}

fn native_source_display_lines(source: &str, width: u16) -> Vec<NativeLine> {
    explicit_target_lines(source_display_lines(source, width), source)
}

fn explicit_target_lines(lines: Vec<Line<'static>>, target: &str) -> Vec<NativeLine> {
    lines
        .into_iter()
        .map(|line| explicit_target_line(line, target))
        .collect()
}

fn explicit_target_line(line: Line<'static>, target: &str) -> NativeLine {
    let mut links = Vec::new();
    let mut col = 0usize;
    for span in &line.spans {
        let text = span.content.as_ref();
        let width = display_width(text);
        if (span.style == link() || span.style == path_reference())
            && !text.trim().is_empty()
            && width > 0
        {
            links.push(NativeLineLink {
                start_col: col,
                width,
                target: target.to_string(),
            });
        }
        col = col.saturating_add(width);
    }
    NativeLine { line, links }
}

fn grouped_lines(
    group: &str,
    values: &[String],
    style: NodeStyle,
    width: u16,
) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    lines.push(Line::from(vec![
        Span::styled("• ", dim()),
        Span::styled(group.to_string(), group_label_style(group, style)),
    ]));
    let value_style = body_style(style);
    let prefix_width = display_width(GROUP_VALUE_LAST_PREFIX) as u16;
    let content_width = width.saturating_sub(prefix_width).max(1);
    let display_values = compact_shell_group_values(group, values);
    let value_rows = display_values
        .iter()
        .flat_map(|value| {
            styled_wrapped_value_rows(group, value, value_style, content_width).into_iter()
        })
        .collect::<Vec<_>>();
    let last_idx = value_rows.len().saturating_sub(1);
    for (idx, wrapped) in value_rows.into_iter().enumerate() {
        let prefix = if idx == last_idx {
            GROUP_VALUE_LAST_PREFIX
        } else {
            GROUP_VALUE_RAIL_PREFIX
        };
        let mut spans = vec![Span::styled(prefix.to_string(), dim())];
        spans.extend(wrapped);
        lines.push(Line::from(spans));
    }
    lines
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ShellClusterValueKind {
    Command,
    Output,
    Other,
}

#[derive(Clone, Debug)]
struct ShellClusterValue {
    text: String,
    style: NodeStyle,
    kind: ShellClusterValueKind,
}

fn compact_shell_cluster_values(nodes: &[&TranscriptNode]) -> Vec<ShellClusterValue> {
    let values = nodes
        .iter()
        .flat_map(|node| match &node.kind {
            TranscriptKind::Timeline { lines, style, .. } => lines
                .iter()
                .map(|line| ShellClusterValue {
                    text: line.clone(),
                    style: *style,
                    kind: shell_cluster_value_kind(line),
                })
                .collect::<Vec<_>>(),
            _ => Vec::new(),
        })
        .collect::<Vec<_>>();

    let command_count = values
        .iter()
        .filter(|value| value.kind == ShellClusterValueKind::Command)
        .count();
    let output_count = values
        .iter()
        .filter(|value| value.kind == ShellClusterValueKind::Output)
        .count();
    let other_count = values
        .iter()
        .filter(|value| value.kind == ShellClusterValueKind::Other)
        .count();

    let mut compacted = Vec::new();
    let mut shown_commands = 0usize;
    let mut shown_outputs = 0usize;
    let mut shown_others = 0usize;
    let mut omitted_commands = false;
    let mut omitted_outputs = false;
    let mut omitted_others = false;
    for value in values {
        match value.kind {
            ShellClusterValueKind::Command => {
                if shown_commands < SHELL_GROUP_VISIBLE_VALUES {
                    shown_commands += 1;
                    compacted.push(value);
                } else if !omitted_commands {
                    omitted_commands = true;
                    compacted.push(omitted_cluster_value(
                        command_count.saturating_sub(SHELL_GROUP_VISIBLE_VALUES),
                        "command",
                    ));
                }
            }
            ShellClusterValueKind::Output => {
                if shown_outputs < SHELL_GROUP_VISIBLE_VALUES {
                    shown_outputs += 1;
                    compacted.push(value);
                } else if !omitted_outputs {
                    omitted_outputs = true;
                    compacted.push(omitted_cluster_value(
                        output_count.saturating_sub(SHELL_GROUP_VISIBLE_VALUES),
                        "line",
                    ));
                }
            }
            ShellClusterValueKind::Other => {
                if shown_others < SHELL_GROUP_VISIBLE_VALUES {
                    shown_others += 1;
                    compacted.push(value);
                } else if !omitted_others {
                    omitted_others = true;
                    compacted.push(omitted_cluster_value(
                        other_count.saturating_sub(SHELL_GROUP_VISIBLE_VALUES),
                        "event",
                    ));
                }
            }
        }
    }
    compacted
}

fn shell_cluster_value_kind(value: &str) -> ShellClusterValueKind {
    if value.starts_with(COMMAND_LINE_PREFIX) || value.starts_with(SHELL_SCRIPT_LINE_PREFIX) {
        ShellClusterValueKind::Command
    } else if value.starts_with("output: ") || value.starts_with("failed: ") {
        ShellClusterValueKind::Output
    } else {
        ShellClusterValueKind::Other
    }
}

fn omitted_cluster_value(count: usize, noun: &str) -> ShellClusterValue {
    ShellClusterValue {
        text: format!("... +{count} {noun}{}", plural(count)),
        style: NodeStyle::Muted,
        kind: ShellClusterValueKind::Other,
    }
}

fn compact_shell_group_values(group: &str, values: &[String]) -> Vec<String> {
    if !matches!(group, "shell" | "command") || values.len() <= SHELL_GROUP_VISIBLE_VALUES {
        return values.to_vec();
    }

    if values
        .iter()
        .all(|value| value.starts_with(COMMAND_LINE_PREFIX))
    {
        return compact_group_values(values, "command");
    }

    if values.iter().all(|value| value.starts_with("output: ")) {
        return compact_group_values(values, "line");
    }

    values.to_vec()
}

fn compact_group_values(values: &[String], noun: &str) -> Vec<String> {
    let omitted = values.len().saturating_sub(SHELL_GROUP_VISIBLE_VALUES);
    let mut compacted = values
        .iter()
        .take(SHELL_GROUP_VISIBLE_VALUES)
        .cloned()
        .collect::<Vec<_>>();
    compacted.push(format!("... +{omitted} {noun}{}", plural(omitted)));
    compacted
}

fn native_grouped_lines(
    group: &str,
    values: &[String],
    style: NodeStyle,
    width: u16,
) -> Vec<NativeLine> {
    let mut lines = Vec::new();
    lines.push(NativeLine::plain(Line::from(vec![
        Span::styled("• ", dim()),
        Span::styled(group.to_string(), group_label_style(group, style)),
    ])));
    let value_style = body_style(style);
    let prefix_width = display_width(GROUP_VALUE_LAST_PREFIX) as u16;
    let content_width = width.saturating_sub(prefix_width).max(1);
    let display_values = compact_shell_group_values(group, values);
    let value_rows = display_values
        .iter()
        .flat_map(|value| native_wrapped_value_fragments(group, value, value_style, content_width))
        .collect::<Vec<_>>();
    let last_idx = value_rows.len().saturating_sub(1);
    for (idx, row) in value_rows.into_iter().enumerate() {
        let prefix = if idx == last_idx {
            GROUP_VALUE_LAST_PREFIX
        } else {
            GROUP_VALUE_RAIL_PREFIX
        };
        lines.push(native_line_from_prefixed_fragments(prefix, row));
    }
    lines
}

#[derive(Clone, Debug)]
struct NativeStyledFragment {
    text: String,
    style: Style,
    target: Option<String>,
}

fn native_wrapped_value_fragments(
    group: &str,
    value: &str,
    fallback: Style,
    width: u16,
) -> Vec<Vec<NativeStyledFragment>> {
    if matches!(group, "shell" | "command") {
        if let Some(command) = value.strip_prefix(COMMAND_LINE_PREFIX) {
            return native_shell_command_rows(command, fallback, width);
        }
        return wrap_plain(value, width)
            .into_iter()
            .map(|(_, row)| native_fragments_from_spans(styled_path_tokens(&row, fallback)))
            .collect();
    }
    let fragments = native_fragments_from_spans(styled_value_spans(group, value, fallback));
    wrap_native_fragments(fragments, width)
}

fn styled_wrapped_value_rows(
    group: &str,
    value: &str,
    fallback: Style,
    width: u16,
) -> Vec<Vec<Span<'static>>> {
    if matches!(group, "shell" | "command") {
        if let Some(command) = value.strip_prefix(COMMAND_LINE_PREFIX) {
            return styled_shell_command_rows(command, fallback, width);
        }
        return wrap_plain(value, width)
            .into_iter()
            .map(|(_, row)| styled_path_tokens(&row, fallback))
            .collect();
    }
    wrap_plain(value, width)
        .into_iter()
        .map(|(_, row)| styled_value_spans(group, &row, fallback))
        .collect()
}

fn styled_shell_command_rows(
    command: &str,
    fallback: Style,
    content_width: u16,
) -> Vec<Vec<Span<'static>>> {
    let label_width = display_width(COMMAND_LINE_PREFIX) as u16;
    let command_width = content_width.saturating_sub(label_width).max(1);
    let wrapped = wrap_shell_command(command, command_width);
    let visible_rows = wrapped.len().min(COMMAND_DISPLAY_MAX_ROWS);
    let mut rows = Vec::new();
    for (idx, row) in wrapped.iter().take(visible_rows).enumerate() {
        let mut spans = Vec::new();
        if idx == 0 {
            spans.push(Span::styled(COMMAND_LINE_PREFIX.to_string(), fallback));
        } else {
            spans.push(Span::styled(" ".repeat(label_width as usize), fallback));
        }
        spans.extend(styled_path_tokens(row, fallback));
        rows.push(spans);
    }
    if wrapped.len() > visible_rows {
        let omitted_chars = wrapped
            .iter()
            .skip(visible_rows)
            .map(|row| row.chars().count())
            .sum::<usize>();
        rows.push(vec![Span::styled(
            format!(
                "{}... command truncated (+{omitted_chars} chars)",
                " ".repeat(label_width as usize)
            ),
            dim(),
        )]);
    }
    rows
}

fn native_shell_command_rows(
    command: &str,
    fallback: Style,
    content_width: u16,
) -> Vec<Vec<NativeStyledFragment>> {
    styled_shell_command_rows(command, fallback, content_width)
        .into_iter()
        .map(native_fragments_from_spans)
        .collect()
}

fn native_fragments_from_spans(spans: Vec<Span<'static>>) -> Vec<NativeStyledFragment> {
    spans
        .into_iter()
        .map(|span| {
            let text = span.content.into_owned();
            let target = ((span.style == link() || span.style == path_reference())
                && !text.trim().is_empty())
            .then(|| text.trim().to_string());
            NativeStyledFragment {
                text,
                style: span.style,
                target,
            }
        })
        .collect()
}

fn wrap_shell_command(command: &str, width: u16) -> Vec<String> {
    let width = width.max(1) as usize;
    let mut rows = Vec::new();
    let mut line = String::new();
    let mut line_width = 0usize;
    for chunk in shell_wrap_chunks(command) {
        let chunk_width = display_width(chunk);
        if chunk.chars().all(char::is_whitespace) {
            if line.is_empty() {
                continue;
            }
            if line_width + chunk_width <= width {
                line.push_str(chunk);
                line_width += chunk_width;
            }
            continue;
        }
        if !line.is_empty() && line_width + chunk_width > width {
            rows.push(line.trim_end().to_string());
            line.clear();
            line_width = 0;
        }
        if chunk_width > width {
            for (_, wrapped) in wrap_plain(chunk, width as u16) {
                if !line.is_empty() {
                    rows.push(line.trim_end().to_string());
                    line.clear();
                    line_width = 0;
                }
                rows.push(wrapped);
            }
            continue;
        }
        line.push_str(chunk);
        line_width += chunk_width;
    }
    if !line.trim().is_empty() {
        rows.push(line.trim_end().to_string());
    }
    if rows.is_empty() {
        rows.push(String::new());
    }
    rows
}

fn shell_wrap_chunks(value: &str) -> Vec<&str> {
    let mut chunks = Vec::new();
    let mut start = 0usize;
    let mut in_whitespace = None;
    for (idx, ch) in value.char_indices() {
        let whitespace = ch.is_whitespace();
        match in_whitespace {
            None => in_whitespace = Some(whitespace),
            Some(current) if current != whitespace => {
                chunks.push(&value[start..idx]);
                start = idx;
                in_whitespace = Some(whitespace);
            }
            _ => {}
        }
    }
    if start < value.len() {
        chunks.push(&value[start..]);
    }
    chunks
}

fn wrap_native_fragments(
    fragments: Vec<NativeStyledFragment>,
    width: u16,
) -> Vec<Vec<NativeStyledFragment>> {
    let width = width.max(1) as usize;
    let mut rows = vec![Vec::new()];
    let mut line_width = 0usize;
    for fragment in fragments {
        for ch in fragment.text.chars() {
            let ch_width = ch.width().unwrap_or(0).max(1);
            if line_width > 0 && line_width + ch_width > width {
                rows.push(Vec::new());
                line_width = 0;
            }
            append_native_fragment_char(
                rows.last_mut()
                    .expect("native fragment rows are never empty"),
                ch,
                fragment.style,
                fragment.target.clone(),
            );
            line_width += ch_width;
        }
    }
    rows
}

fn append_native_fragment_char(
    row: &mut Vec<NativeStyledFragment>,
    ch: char,
    style: Style,
    target: Option<String>,
) {
    if let Some(last) = row
        .last_mut()
        .filter(|last| last.style == style && last.target == target)
    {
        last.text.push(ch);
        return;
    }
    row.push(NativeStyledFragment {
        text: ch.to_string(),
        style,
        target,
    });
}

fn native_line_from_prefixed_fragments(
    prefix: &str,
    fragments: Vec<NativeStyledFragment>,
) -> NativeLine {
    let mut spans = vec![Span::styled(prefix.to_string(), dim())];
    let mut links = Vec::new();
    let mut col = display_width(prefix);
    for fragment in fragments {
        let width = display_width(&fragment.text);
        if let Some(target) = fragment.target.as_ref().filter(|_| width > 0) {
            links.push(NativeLineLink {
                start_col: col,
                width,
                target: target.clone(),
            });
        }
        col = col.saturating_add(width);
        spans.push(Span::styled(fragment.text, fragment.style));
    }
    NativeLine {
        line: Line::from(spans),
        links,
    }
}

fn styled_value_spans(_group: &str, text: &str, fallback: Style) -> Vec<Span<'static>> {
    if text.starts_with("https://") || text.starts_with("http://") {
        return vec![Span::styled(text.to_string(), link())];
    }
    if let Some(spans) = styled_activity_line_spans(text, fallback) {
        return spans;
    }
    styled_path_tokens(text, fallback)
}

fn styled_notice_spans(text: &str, fallback: Style) -> Vec<Span<'static>> {
    let Some(start) = text.find("[cloud.browser-use.com]") else {
        return styled_path_tokens(text, fallback);
    };
    let end = start + "[cloud.browser-use.com]".len();
    let mut spans = Vec::new();
    if start > 0 {
        spans.extend(styled_path_tokens(&text[..start], fallback));
    }
    spans.push(Span::styled("[".to_string(), fallback));
    spans.push(Span::styled("cloud.browser-use.com".to_string(), link()));
    spans.push(Span::styled("]".to_string(), fallback));
    if end < text.len() {
        spans.extend(styled_path_tokens(&text[end..], fallback));
    }
    spans
}

fn styled_activity_line_spans(text: &str, fallback: Style) -> Option<Vec<Span<'static>>> {
    let (leading, action, rest) = split_activity_line(text)?;
    if action == "run" && looks_like_command_line(rest) {
        let mut spans = Vec::new();
        if !leading.is_empty() {
            spans.push(Span::styled(leading.to_string(), fallback));
        }
        spans.push(Span::styled(
            action.to_string(),
            activity_action_style(action),
        ));
        spans.push(Span::styled(" ".to_string(), fallback));
        spans.extend(styled_path_tokens(rest, fallback));
        return Some(spans);
    }

    if matches!(
        action,
        "read"
            | "list"
            | "listed"
            | "search"
            | "searched"
            | "task"
            | "follow-up"
            | "waiting"
            | "working"
            | "artifact"
            | "command"
    ) {
        let mut spans = Vec::new();
        if !leading.is_empty() {
            spans.push(Span::styled(leading.to_string(), fallback));
        }
        spans.push(Span::styled(
            action.to_string(),
            activity_action_style(action),
        ));
        if !rest.is_empty() {
            spans.push(Span::styled(" ".to_string(), fallback));
            spans.extend(styled_path_tokens(rest, fallback));
        }
        return Some(spans);
    }

    None
}

fn split_activity_line(text: &str) -> Option<(&str, &str, &str)> {
    let leading_len = text
        .chars()
        .take_while(|ch| ch.is_whitespace())
        .map(char::len_utf8)
        .sum::<usize>();
    let leading = &text[..leading_len];
    let body = &text[leading_len..];
    if body.is_empty() {
        return None;
    }
    if body == "working" {
        return Some((leading, body, ""));
    }
    let (action, rest) = body.split_once(' ')?;
    Some((leading, action, rest))
}

fn activity_action_style(action: &str) -> Style {
    match action {
        "read" => activity_read(),
        "run" | "command" => activity_run(),
        "list" | "listed" => activity_list(),
        "search" | "searched" => activity_search(),
        "artifact" | "task" | "follow-up" => activity_task(),
        "working" | "waiting" => thought(),
        _ => group_style(NodeStyle::Normal),
    }
}

fn group_label_style(group: &str, style: NodeStyle) -> Style {
    match group.split_whitespace().next() {
        Some("subagent") => thought(),
        Some("run" | "shell" | "command") => activity_run(),
        Some("explored") => activity_group(),
        Some("browser") => activity_search(),
        Some("edit") | Some("plan") | Some("context") => activity_task(),
        _ => group_style(style),
    }
}

fn styled_path_tokens(text: &str, fallback: Style) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    let mut token_start = None;
    for (idx, ch) in text.char_indices() {
        if ch.is_whitespace() {
            if let Some(start) = token_start.take() {
                push_maybe_path_token(&mut spans, &text[start..idx], fallback);
            }
            spans.push(Span::styled(ch.to_string(), fallback));
        } else if token_start.is_none() {
            token_start = Some(idx);
        }
    }
    if let Some(start) = token_start {
        push_maybe_path_token(&mut spans, &text[start..], fallback);
    }
    if spans.is_empty() {
        spans.push(Span::styled(text.to_string(), fallback));
    }
    spans
}

fn push_maybe_path_token(spans: &mut Vec<Span<'static>>, token: &str, fallback: Style) {
    let leading = token
        .chars()
        .take_while(|ch| matches!(ch, '"' | '\'' | '`' | '(' | '[' | '{' | '<'))
        .map(char::len_utf8)
        .sum::<usize>();
    let trailing = token
        .chars()
        .rev()
        .take_while(|ch| {
            matches!(
                ch,
                '"' | '\'' | '`' | ')' | ']' | '}' | '>' | ',' | ':' | ';'
            )
        })
        .map(char::len_utf8)
        .sum::<usize>();
    let core_end = token.len().saturating_sub(trailing);
    if leading >= core_end {
        spans.push(Span::styled(token.to_string(), fallback));
        return;
    }
    let (prefix, rest) = token.split_at(leading);
    let (core, suffix) = rest.split_at(core_end - leading);
    if looks_like_path_token(core) {
        if !prefix.is_empty() {
            spans.push(Span::styled(prefix.to_string(), fallback));
        }
        spans.push(Span::styled(core.to_string(), reference_token_style(core)));
        if !suffix.is_empty() {
            spans.push(Span::styled(suffix.to_string(), fallback));
        }
    } else {
        spans.push(Span::styled(token.to_string(), fallback));
    }
}

fn looks_like_command_line(text: &str) -> bool {
    matches!(
        text.trim_start()
            .trim_start_matches("$ ")
            .split_whitespace()
            .next(),
        Some(
            "cargo"
                | "git"
                | "rg"
                | "grep"
                | "find"
                | "sed"
                | "awk"
                | "cat"
                | "ls"
                | "cd"
                | "pwd"
                | "uv"
                | "python"
                | "python3"
                | "node"
                | "npm"
                | "pnpm"
                | "yarn"
                | "bun"
                | "curl"
                | "ssh"
                | "docker"
                | "task"
                | "sqlite3"
        )
    )
}

fn looks_like_path_token(token: &str) -> bool {
    if looks_like_url_token(token) {
        return true;
    }
    let has_path_character = token
        .chars()
        .any(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-');
    token.starts_with('/')
        || (has_path_character
            && (token.starts_with("~/") || token.starts_with("./") || token.starts_with("../")))
        || source_extension(token).is_some()
}

fn reference_token_style(token: &str) -> Style {
    if looks_like_url_token(token) || looks_like_absolute_path_token(token) {
        link()
    } else {
        path_reference()
    }
}

fn looks_like_url_token(token: &str) -> bool {
    token.starts_with("http://") || token.starts_with("https://") || token.starts_with("file://")
}

fn looks_like_absolute_path_token(token: &str) -> bool {
    token.starts_with('/')
}

fn source_extension(token: &str) -> Option<&str> {
    let extension = token.rsplit_once('.')?.1;
    matches!(
        extension,
        "rs" | "toml"
            | "lock"
            | "md"
            | "py"
            | "json"
            | "jsonl"
            | "yaml"
            | "yml"
            | "ts"
            | "tsx"
            | "js"
            | "jsx"
            | "css"
            | "scss"
            | "html"
            | "sql"
            | "sh"
            | "zsh"
            | "fish"
            | "txt"
            | "log"
            | "xml"
            | "svg"
            | "png"
            | "jpg"
            | "jpeg"
            | "gif"
            | "webp"
            | "pdf"
            | "diff"
            | "patch"
    )
    .then_some(extension)
}

fn group_style(style: NodeStyle) -> Style {
    match style {
        NodeStyle::Normal => activity_group(),
        NodeStyle::Muted => muted(),
        NodeStyle::Failed => failed(),
        NodeStyle::Thought => thought(),
    }
}

fn body_style(style: NodeStyle) -> Style {
    match style {
        NodeStyle::Normal => text_style(),
        NodeStyle::Muted => muted(),
        NodeStyle::Failed => failed(),
        NodeStyle::Thought => muted(),
    }
}

fn wrap_plain(value: &str, width: u16) -> Vec<(usize, String)> {
    let width = width.max(1) as usize;
    if value.is_empty() {
        return vec![(0, String::new())];
    }
    let mut out = Vec::new();
    let mut line = String::new();
    let mut line_width = 0usize;
    let mut wrap_idx = 0usize;
    for ch in value.chars() {
        let ch_width = ch.width().unwrap_or(0).max(1);
        if line_width > 0 && line_width + ch_width > width {
            out.push((wrap_idx, std::mem::take(&mut line)));
            wrap_idx += 1;
            line_width = 0;
        }
        line.push(ch);
        line_width += ch_width;
    }
    out.push((wrap_idx, line));
    out
}

fn prefixed_plain(prefix: &str, text: &str) -> Vec<String> {
    text.lines()
        .enumerate()
        .map(|(idx, line)| {
            if idx == 0 {
                format!("{prefix}{line}")
            } else {
                format!("  {line}")
            }
        })
        .collect()
}

fn payload_string(event: &EventRecord, key: &str) -> Option<String> {
    if key == "text" {
        if let Some(text) = user_input_display_text_from_payload(&event.payload) {
            return Some(text);
        }
    }
    event
        .payload
        .get(key)
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

fn raw_payload_string(event: &EventRecord, key: &str) -> Option<String> {
    event
        .payload
        .get(key)
        .and_then(serde_json::Value::as_str)
        .map(ToOwned::to_owned)
}

fn source_for_state(state: &WorkbenchState) -> Option<String> {
    if let Some(source) = state
        .browser
        .url
        .as_deref()
        .filter(|source| is_useful_source_for_backend(source, &state.browser.backend))
    {
        return Some(source.to_string());
    }
    state
        .browser
        .live_url
        .as_deref()
        .filter(|_| browser_backend_supports_live_url(&state.browser.backend))
        .filter(|source| is_useful_source(source))
        .map(ToOwned::to_owned)
}

fn is_useful_source(source: &str) -> bool {
    let source = source.trim();
    !source.is_empty() && source != "about:blank"
}

fn is_useful_source_for_backend(source: &str, backend: &str) -> bool {
    is_useful_source(source)
        && (browser_backend_supports_live_url(backend) || !is_cloud_live_url(source))
}

fn browser_backend_supports_live_url(backend: &str) -> bool {
    let backend = backend.to_ascii_lowercase();
    backend.contains("cloud")
        || backend.contains("managed")
        || backend.contains("headless chromium")
        || backend.contains("managed chromium")
}

fn is_cloud_live_url(source: &str) -> bool {
    source
        .trim()
        .to_ascii_lowercase()
        .starts_with("https://live.browser-use.com/")
}

fn tool_failed_lines(event: &EventRecord) -> Vec<String> {
    let name = payload_string(event, "name").unwrap_or_else(|| "tool".to_string());
    let Some(diagnosis) = event
        .payload
        .get("diagnosis")
        .filter(|value| value.is_object())
    else {
        let error = payload_string(event, "error").unwrap_or_else(|| "tool failed".to_string());
        if is_command_tool_output(&name) {
            return command_failed_summary_lines(&error);
        }
        return vec![format!("{name} failed: {}", friendly_error_message(&error))];
    };

    let mut lines = vec![format!("{name} failed")];
    if let Some(summary) = diagnosis_text(diagnosis, "summary") {
        lines.push(summary);
    }
    if let Some(what_happened) = diagnosis_text(diagnosis, "what_happened") {
        lines.push(format!("What happened: {what_happened}"));
    }
    if let Some(next_step) = diagnosis_text(diagnosis, "next_step") {
        lines.push(format!("Next: {next_step}"));
    }
    if let Some(error) = payload_string(event, "error") {
        let detail = last_error_line(&error);
        if !detail.is_empty() {
            lines.push(format!("Details: {}", truncate_inline(&detail, 180)));
        }
    }
    lines
}

fn diagnosis_text(diagnosis: &serde_json::Value, key: &str) -> Option<String> {
    diagnosis
        .get(key)
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

fn last_error_line(error: &str) -> String {
    error
        .lines()
        .rev()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or(error.trim())
        .to_string()
}

fn truncate_inline(value: &str, max_chars: usize) -> String {
    if value.chars().count() <= max_chars {
        return value.to_string();
    }
    let mut out = value
        .chars()
        .take(max_chars.saturating_sub(3))
        .collect::<String>();
    out.push_str("...");
    out
}

fn preview_lines(text: &str, limit: usize) -> Vec<String> {
    let mut out = text
        .lines()
        .map(str::trim_end)
        .filter(|line| !line.is_empty())
        .take(limit)
        .map(ToOwned::to_owned)
        .collect::<Vec<_>>();
    if text
        .lines()
        .filter(|line| !line.trim_end().is_empty())
        .count()
        > out.len()
    {
        out.push(format!(
            "... +{} lines",
            text.lines().count().saturating_sub(out.len())
        ));
    }
    out
}

fn friendly_error_message(error: &str) -> String {
    let error = error.trim();
    if error.is_empty() {
        "The task failed.".to_string()
    } else {
        error.to_string()
    }
}

fn display_path(path: &str, state: &WorkbenchState) -> String {
    let Some(session) = state.current_session.as_ref() else {
        return path.to_string();
    };
    let cwd = session.cwd.trim_end_matches('/');
    path.strip_prefix(cwd)
        .and_then(|path| path.strip_prefix('/').or(Some(path)))
        .filter(|path| !path.is_empty())
        .unwrap_or(path)
        .to_string()
}

#[derive(Clone, Debug)]
struct ResultFileDisplay {
    file_path: String,
    bytes: Option<u64>,
    mime: Option<String>,
}

fn session_done_result_text(event: &EventRecord) -> Option<String> {
    payload_string(event, "result").map(|result| normalize_result_text(&result))
}

fn session_done_result_file(
    event: &EventRecord,
    state: &WorkbenchState,
) -> Option<ResultFileDisplay> {
    event.payload.get("result_file")?;
    let file_path = payload_string(event, "result_file_path")
        .or_else(|| resolved_result_file_path(event, state).map(|path| path.display().to_string()))
        .or_else(|| payload_string(event, "result_file"))
        .unwrap_or_else(|| "<unknown>".to_string());
    let bytes = event
        .payload
        .get("result_file_bytes")
        .and_then(serde_json::Value::as_u64);
    let mime = payload_string(event, "result_file_mime");

    Some(ResultFileDisplay {
        file_path,
        bytes,
        mime,
    })
}

fn result_file_lines(
    file_path: &str,
    bytes: Option<u64>,
    mime: Option<&str>,
    width: u16,
) -> Vec<Line<'static>> {
    let mut lines = vec![
        Line::from(Span::styled("Saved result file", text_style())),
        Line::from(""),
    ];
    let path_style = result_file_path_style(file_path);
    lines.extend(
        wrap_plain(file_path, width)
            .into_iter()
            .map(|(_, line)| Line::from(Span::styled(line, path_style))),
    );
    if let Some(metadata) = result_file_metadata(bytes, mime) {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(metadata, muted())));
    }
    lines
}

fn native_result_file_lines(
    file_path: &str,
    bytes: Option<u64>,
    mime: Option<&str>,
    width: u16,
) -> Vec<NativeLine> {
    let mut lines = vec![
        NativeLine::plain(Line::from(Span::styled("Saved result file", text_style()))),
        NativeLine::plain(Line::from("")),
    ];
    let path_style = result_file_path_style(file_path);
    lines.extend(wrap_plain(file_path, width).into_iter().map(|(_, line)| {
        explicit_target_line(Line::from(Span::styled(line, path_style)), file_path)
    }));
    if let Some(metadata) = result_file_metadata(bytes, mime) {
        lines.push(NativeLine::plain(Line::from("")));
        lines.push(NativeLine::plain(Line::from(Span::styled(
            metadata,
            muted(),
        ))));
    }
    lines
}

fn result_file_plain_lines(file_path: &str, bytes: Option<u64>, mime: Option<&str>) -> Vec<String> {
    let mut lines = vec![
        "Saved result file".to_string(),
        String::new(),
        file_path.to_string(),
    ];
    if let Some(metadata) = result_file_metadata(bytes, mime) {
        lines.push(String::new());
        lines.push(metadata);
    }
    lines
}

fn result_file_path_style(file_path: &str) -> Style {
    if file_path.starts_with('/') || file_path.starts_with("file://") {
        link()
    } else {
        path_reference()
    }
}

fn result_file_metadata(bytes: Option<u64>, mime: Option<&str>) -> Option<String> {
    let mime = mime.and_then(display_result_file_mime);
    match (bytes, mime) {
        (Some(bytes), Some(mime)) => Some(format!("{} · {mime}", format_bytes(bytes))),
        (Some(bytes), None) => Some(format_bytes(bytes)),
        (None, Some(mime)) => Some(mime.to_string()),
        (None, None) => None,
    }
}

fn display_result_file_mime(mime: &str) -> Option<&str> {
    (mime != "application/octet-stream").then_some(mime)
}

fn resolved_result_file_path(event: &EventRecord, state: &WorkbenchState) -> Option<PathBuf> {
    if let Some(path) = payload_string(event, "result_file_path") {
        return Some(PathBuf::from(path));
    }
    let requested = payload_string(event, "result_file")?;
    let requested_path = Path::new(&requested);
    if requested_path.is_absolute() {
        return Some(requested_path.to_path_buf());
    }
    let session = state.current_session.as_ref()?;
    let candidates = [
        Path::new(&session.cwd).join(&requested),
        Path::new(&session.artifact_root).join(&requested),
        requested_path.to_path_buf(),
    ];
    candidates
        .iter()
        .find(|candidate| candidate.exists())
        .cloned()
        .or_else(|| Some(Path::new(&session.artifact_root).join(&requested)))
}

fn format_bytes(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = 1024.0 * 1024.0;
    const GB: f64 = 1024.0 * 1024.0 * 1024.0;
    let bytes_f = bytes as f64;
    if bytes_f >= GB {
        format!("{:.1} GB", bytes_f / GB)
    } else if bytes_f >= MB {
        format!("{:.1} MB", bytes_f / MB)
    } else if bytes_f >= KB {
        format!("{:.1} KB", bytes_f / KB)
    } else {
        format!("{bytes} B")
    }
}

fn compact_url(url: &str) -> String {
    const MAX: usize = 72;
    let compact = url
        .trim()
        .strip_prefix("https://")
        .or_else(|| url.trim().strip_prefix("http://"))
        .unwrap_or_else(|| url.trim())
        .trim_end_matches('/');
    let compact = if let Some((prefix, _)) = compact.split_once('?') {
        format!("{prefix}?...")
    } else {
        compact.to_string()
    };
    if compact.chars().count() <= MAX {
        return compact;
    }
    let mut out = compact
        .chars()
        .take(MAX.saturating_sub(1))
        .collect::<String>();
    out.push_str("...");
    out
}

fn helper_label_for_child(app: &App, parent_id: &str, child_id: &str) -> String {
    app.cached_events_for_session(parent_id)
        .iter()
        .find(|event| {
            event.event_type == "agent.spawned"
                && event
                    .payload
                    .get("child_session_id")
                    .and_then(serde_json::Value::as_str)
                    == Some(child_id)
        })
        .and_then(|event| {
            event
                .payload
                .get("nickname")
                .and_then(serde_json::Value::as_str)
                .or_else(|| {
                    event
                        .payload
                        .get("role")
                        .and_then(serde_json::Value::as_str)
                })
        })
        .map(str::trim)
        .filter(|label| !label.is_empty())
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| helper_label_for_session(app, child_id))
}

fn helper_label_for_session(app: &App, session_id: &str) -> String {
    if let Some(label) = app
        .cached_events_for_session(session_id)
        .iter()
        .find_map(|event| {
            if event.event_type != "agent.context" {
                return None;
            }
            event
                .payload
                .get("nickname")
                .and_then(serde_json::Value::as_str)
                .or_else(|| {
                    event
                        .payload
                        .get("role")
                        .and_then(serde_json::Value::as_str)
                })
                .or_else(|| {
                    event
                        .payload
                        .get("agent_path")
                        .and_then(serde_json::Value::as_str)
                })
                .map(str::trim)
                .filter(|label| !label.is_empty())
                .map(ToOwned::to_owned)
        })
    {
        return label;
    }

    if let Some(label) = app
        .state_cache
        .sessions
        .iter()
        .find(|session| session.id == session_id)
        .and_then(|session| session.parent_id.as_deref())
        .and_then(|parent_id| {
            app.cached_events_for_session(parent_id)
                .iter()
                .find(|event| {
                    event.event_type == "agent.spawned"
                        && event
                            .payload
                            .get("child_session_id")
                            .and_then(serde_json::Value::as_str)
                            == Some(session_id)
                })
        })
        .and_then(|event| {
            event
                .payload
                .get("nickname")
                .and_then(serde_json::Value::as_str)
                .or_else(|| {
                    event
                        .payload
                        .get("role")
                        .and_then(serde_json::Value::as_str)
                })
        })
        .map(str::trim)
        .filter(|label| !label.is_empty())
        .map(ToOwned::to_owned)
    {
        return label;
    }

    app.state_cache
        .sessions
        .iter()
        .find(|session| session.id == session_id)
        .and_then(|session| {
            session
                .parent_id
                .as_ref()
                .map(|_| session.id.chars().take(6).collect::<String>())
        })
        .unwrap_or_else(|| "subagent".to_string())
}

fn is_subagent_management_tool(name: &str) -> bool {
    matches!(
        name,
        "spawn_agent"
            | "wait_agent"
            | "send_input"
            | "send_message"
            | "followup_task"
            | "close_agent"
            | "resume_agent"
            | "list_agents"
    )
}

fn has_later_root_event(events: &[EventRecord], event: &EventRecord, event_type: &str) -> bool {
    events.iter().any(|candidate| {
        candidate.session_id == event.session_id
            && candidate.seq > event.seq
            && candidate.event_type == event_type
            && !is_replay_materialization_event(candidate)
    })
}

fn is_replay_materialization_event(event: &EventRecord) -> bool {
    event
        .payload
        .get("materialized_from_replay")
        .or_else(|| {
            event
                .payload
                .get("payload")
                .and_then(serde_json::Value::as_object)
                .and_then(|payload| payload.get("materialized_from_replay"))
        })
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
}

fn has_agent_message_for_collab_receiver(events: &[EventRecord], event: &EventRecord) -> bool {
    let Some(receiver_thread_id) = event
        .payload
        .get("receiver_thread_id")
        .and_then(serde_json::Value::as_str)
    else {
        return false;
    };
    events.iter().any(|candidate| {
        candidate.session_id == event.session_id
            && candidate.event_type == "agent.message"
            && (candidate
                .payload
                .get("target_session_id")
                .and_then(serde_json::Value::as_str)
                == Some(receiver_thread_id)
                || candidate
                    .payload
                    .get("child_session_id")
                    .and_then(serde_json::Value::as_str)
                    == Some(receiver_thread_id))
    })
}

fn subagent_lifecycle_node(
    app: &App,
    event: &EventRecord,
    status: &str,
    style: NodeStyle,
) -> TranscriptNode {
    let group = subagent_label_for_event(app, event)
        .map(|label| format!("subagent {label} {status}"))
        .unwrap_or_else(|| format!("subagent {status}"));
    timeline_node(event, &group, Vec::new(), style)
}

fn subagent_terminal_lifecycle_node(
    app: &App,
    event: &EventRecord,
    status: &str,
    style: NodeStyle,
) -> Option<TranscriptNode> {
    if event_child_session_id(event).is_none() {
        return None;
    }
    Some(subagent_lifecycle_node(app, event, status, style))
}

fn event_child_session_id(event: &EventRecord) -> Option<&str> {
    event
        .payload
        .get("child_session_id")
        .and_then(serde_json::Value::as_str)
        .or_else(|| {
            event
                .payload
                .get("payload")
                .and_then(|payload| payload.get("child_session_id"))
                .and_then(serde_json::Value::as_str)
        })
}

fn subagent_label_for_event(app: &App, event: &EventRecord) -> Option<String> {
    if let Some(child_id) = event_child_session_id(event)
        .or_else(|| {
            event
                .payload
                .get("new_thread_id")
                .and_then(serde_json::Value::as_str)
        })
        .or_else(|| {
            event
                .payload
                .get("receiver_thread_id")
                .and_then(serde_json::Value::as_str)
        })
    {
        if let Some(label) =
            normalize_subagent_label(&helper_label_for_child(app, &event.session_id, child_id))
        {
            return Some(label);
        }
    }

    if let Some(label) = [
        "nickname",
        "role",
        "task_name",
        "agent_path",
        "recipient_path",
        "new_agent_nickname",
        "new_agent_role",
        "receiver_agent_nickname",
        "receiver_agent_role",
    ]
    .into_iter()
    .find_map(|key| {
        event
            .payload
            .get(key)
            .and_then(serde_json::Value::as_str)
            .and_then(normalize_subagent_label)
            .or_else(|| {
                event
                    .payload
                    .get("payload")
                    .and_then(|payload| payload.get(key))
                    .and_then(serde_json::Value::as_str)
                    .and_then(normalize_subagent_label)
            })
    }) {
        return Some(label);
    }

    event
        .payload
        .get("receiver_agents")
        .and_then(serde_json::Value::as_array)
        .and_then(|agents| {
            if agents.len() == 1 {
                agents.first().and_then(|agent| {
                    agent
                        .get("agent_nickname")
                        .and_then(serde_json::Value::as_str)
                        .or_else(|| agent.get("agent_role").and_then(serde_json::Value::as_str))
                        .or_else(|| agent.get("thread_id").and_then(serde_json::Value::as_str))
                        .and_then(normalize_subagent_label)
                })
            } else if agents.len() > 1 {
                Some(format!("{} subagents", agents.len()))
            } else {
                None
            }
        })
}

fn normalize_subagent_label(value: &str) -> Option<String> {
    let label = value
        .trim()
        .trim_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or(value)
        .trim();
    (!label.is_empty() && label != "root" && label != "subagent").then(|| label.to_string())
}

fn plural(count: usize) -> &'static str {
    if count == 1 {
        ""
    } else {
        "s"
    }
}

fn browser_event_label(event: &EventRecord) -> String {
    match event.event_type.as_str() {
        "browser.reconnected" => "browser reconnected",
        "browser.target_changed" => "browser target changed",
        _ => "browser connected",
    }
    .to_string()
}

fn agent_wait_finished_node(event: &EventRecord) -> Option<TranscriptNode> {
    if !event
        .payload
        .get("timed_out")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
    {
        return None;
    }
    Some(timeline_node(
        event,
        "subagent wait timed out",
        Vec::new(),
        NodeStyle::Muted,
    ))
}

fn mailbox_continuation_node(event: &EventRecord) -> TranscriptNode {
    let count = event
        .payload
        .get("mailbox_messages")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0) as usize;
    let lines = if count == 0 {
        Vec::new()
    } else {
        vec![format!("{count} mailbox update{} queued", plural(count))]
    };
    timeline_node(event, "subagent results ready", lines, NodeStyle::Normal)
}

#[cfg(test)]
mod tests {
    use super::*;
    use browser_use_agent::context::workspace_context::rollback_filtered_event_records;

    fn ev(seq: i64, event_type: &str, text: &str) -> EventRecord {
        EventRecord {
            seq,
            id: format!("e{seq}"),
            session_id: "s".to_string(),
            ts_ms: 0,
            event_type: event_type.to_string(),
            payload: serde_json::json!({ "text": text }),
        }
    }

    fn filtered_seqs(events: &[EventRecord]) -> Vec<i64> {
        events.iter().map(|event| event.seq).collect()
    }

    #[test]
    fn successful_agent_wait_finished_is_hidden_from_transcript() {
        let event = EventRecord {
            seq: 1,
            id: "wait-finished".to_string(),
            session_id: "s".to_string(),
            ts_ms: 0,
            event_type: "agent.wait.finished".to_string(),
            payload: serde_json::json!({ "timed_out": false }),
        };

        assert!(agent_wait_finished_node(&event).is_none());
    }

    #[test]
    fn timed_out_agent_wait_finished_stays_visible() {
        let event = EventRecord {
            seq: 1,
            id: "wait-timeout".to_string(),
            session_id: "s".to_string(),
            ts_ms: 0,
            event_type: "agent.wait.finished".to_string(),
            payload: serde_json::json!({ "timed_out": true }),
        };

        let node = agent_wait_finished_node(&event).expect("timeout should render");
        let text = node
            .display_lines(120, DisplayMode::Scrollback)
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("subagent wait timed out"), "{text}");
    }

    // The cache's extend path must produce exactly what a full rebuild would,
    // including truncating on a rollback in the appended tail.
    #[test]
    fn filtered_event_cache_extend_matches_full_rebuild() {
        let cache = FilteredEventCache::default();
        let mut raw = vec![
            ev(1, "session.input", "hi"),
            ev(2, "model.turn.response", "a"),
        ];
        let baseline = |raw: &[EventRecord]| {
            rollback_filtered_event_records(raw)
                .into_iter()
                .map(|event| event.seq)
                .collect::<Vec<_>>()
        };

        cache.with_filtered("s", &raw, |events| {
            assert_eq!(filtered_seqs(events), baseline(&raw));
        });

        // Append-only growth -> extend path.
        raw.push(ev(3, "agent.message", "b"));
        raw.push(ev(4, "model.turn.response", "c"));
        cache.with_filtered("s", &raw, |events| {
            assert_eq!(filtered_seqs(events), baseline(&raw));
        });

        // A rollback in the tail must force a rebuild, not a naive append.
        raw.push(ev(5, SESSION_ROLLBACK_EVENT_TYPE, ""));
        cache.with_filtered("s", &raw, |events| {
            assert_eq!(filtered_seqs(events), baseline(&raw));
        });

        // Switching sessions rebuilds for the new id.
        let other = vec![ev(1, "session.input", "other")];
        cache.with_filtered("other", &other, |events| {
            assert_eq!(filtered_seqs(events), baseline(&other));
        });
    }

    // Run with: cargo test -p browser-use-tui filtered_event_cache_streaming_cost -- --ignored --nocapture
    #[test]
    #[ignore]
    fn filtered_event_cache_streaming_cost() {
        use std::hint::black_box;
        use std::time::Instant;

        let body = "x".repeat(400);
        let mut raw: Vec<EventRecord> = (0..3000)
            .map(|i| {
                let ty = if i % 2 == 0 {
                    "model.turn.response"
                } else {
                    "agent.message"
                };
                ev(i + 1, ty, &format!("{i}: {body}"))
            })
            .collect();

        let reps = 50;
        // Old behavior: clone every filtered event, every frame.
        let t = Instant::now();
        for _ in 0..reps {
            let cloned: Vec<EventRecord> = rollback_filtered_event_records(&raw)
                .into_iter()
                .cloned()
                .collect();
            black_box(cloned);
        }
        let full_clone_us = t.elapsed().as_micros() as f64 / reps as f64;

        // New behavior: prime once, then each streaming frame appends one event
        // and the cache extends in place.
        let cache = FilteredEventCache::default();
        cache.with_filtered("s", &raw, |events| black_box(events.len()));
        let t = Instant::now();
        for i in 0..reps {
            raw.push(ev(
                raw.last().unwrap().seq + 1,
                "model.turn.response",
                &body,
            ));
            cache.with_filtered("s", &raw, |events| black_box(events.len()));
            black_box(i);
        }
        let extend_us = t.elapsed().as_micros() as f64 / reps as f64;

        eprintln!(
            "TIMING2 events={} full_clone_per_frame={full_clone_us:.0}us \
             incremental_extend_per_frame={extend_us:.1}us",
            raw.len()
        );
    }

    fn line_text(line: &Line<'static>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>()
            .trim_end()
            .to_string()
    }

    fn native_lines_text(lines: &[NativeLine]) -> String {
        lines
            .iter()
            .map(|line| line_text(&line.line))
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn streaming_model_for(markdown: &str) -> TranscriptModel {
        TranscriptModel {
            session_id: "session".to_string(),
            committed: Vec::new(),
            terminal_committed: Vec::new(),
            active: Some(TranscriptNode {
                id: "stream".to_string(),
                seq: 1,
                revision: 1,
                kind: TranscriptKind::StreamingAssistant {
                    markdown: markdown.to_string(),
                },
            }),
            last_event_seq: 1,
            live_phase: 0,
        }
    }

    fn exec_begin_event(seq: i64, tool_name: &str, command: &str) -> EventRecord {
        EventRecord {
            seq,
            id: format!("event-{seq}"),
            session_id: "session".to_string(),
            ts_ms: 0,
            event_type: "exec_command.begin".to_string(),
            payload: serde_json::json!({
                "name": tool_name,
                "process_id": seq.to_string(),
                "session_id": seq,
                "command": ["bash", "-lc", command]
            }),
        }
    }

    fn rendered_exec_command_text(tool_name: &str, command: &str, width: u16) -> String {
        let event = exec_begin_event(8, tool_name, command);
        let node = exec_command_begin_node(&event).expect("exec begin node");
        node.display_lines(width, DisplayMode::Scrollback)
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn prompt_output_pairs_use_one_blank_line() {
        let prompt = TranscriptNode {
            id: "prompt".to_string(),
            seq: 1,
            revision: 1,
            kind: TranscriptKind::Prompt {
                text: "go to gusto".to_string(),
                followup: false,
            },
        };
        let answer = TranscriptNode {
            id: "answer".to_string(),
            seq: 2,
            revision: 2,
            kind: TranscriptKind::Assistant {
                markdown: "Please open Chrome first.".to_string(),
                source: None,
            },
        };

        let lines = cells_to_lines([&prompt, &answer].into_iter(), 80, DisplayMode::Scrollback);

        assert_eq!(line_text(&lines[0]), "> go to gusto");
        assert_eq!(line_text(&lines[1]), "");
        assert_eq!(line_text(&lines[2]), "Please open Chrome first.");
    }

    #[test]
    fn prompt_streaming_output_uses_one_blank_line() {
        let prompt = TranscriptNode {
            id: "prompt".to_string(),
            seq: 1,
            revision: 1,
            kind: TranscriptKind::Prompt {
                text: "whats up".to_string(),
                followup: false,
            },
        };
        let answer = TranscriptNode {
            id: "answer".to_string(),
            seq: 2,
            revision: 2,
            kind: TranscriptKind::StreamingAssistant {
                markdown: "Not much. I'm ready to work.".to_string(),
            },
        };

        let lines = cells_to_lines([&prompt, &answer].into_iter(), 80, DisplayMode::Active);

        assert_eq!(line_text(&lines[0]), "> whats up");
        assert_eq!(line_text(&lines[1]), "");
        assert_eq!(line_text(&lines[2]), "Not much. I'm ready to work.");
    }

    #[test]
    fn streaming_with_pending_status_keeps_prompt_separator() {
        let active = TranscriptNode {
            id: "active-stack".to_string(),
            seq: 3,
            revision: 3,
            kind: TranscriptKind::Stack {
                nodes: vec![
                    TranscriptNode {
                        id: "stream".to_string(),
                        seq: 2,
                        revision: 2,
                        kind: TranscriptKind::StreamingAssistant {
                            markdown: "Not much. I'm ready to work.".to_string(),
                        },
                    },
                    TranscriptNode {
                        id: "status".to_string(),
                        seq: 3,
                        revision: 3,
                        kind: TranscriptKind::PendingStatus {
                            status: "Thinking...".to_string(),
                            detail: None,
                        },
                    },
                ],
            },
        };
        let model = TranscriptModel {
            session_id: "session".to_string(),
            committed: Vec::new(),
            terminal_committed: Vec::new(),
            active: Some(active),
            last_event_seq: 3,
            live_phase: 0,
        };

        let lines = active_viewport_lines(Some(&model), 80, 10);

        assert_eq!(line_text(&lines[0]), "");
        assert_eq!(line_text(&lines[1]), "Not much. I'm ready to work.");
        assert_eq!(line_text(&lines[2]), "• Thinking...");
    }

    #[test]
    fn active_streaming_moves_separator_with_emitted_prefix() {
        fn model_for(markdown: &str) -> TranscriptModel {
            TranscriptModel {
                session_id: "session".to_string(),
                committed: Vec::new(),
                terminal_committed: Vec::new(),
                active: Some(TranscriptNode {
                    id: "stream".to_string(),
                    seq: 1,
                    revision: 1,
                    kind: TranscriptKind::StreamingAssistant {
                        markdown: markdown.to_string(),
                    },
                }),
                last_event_seq: 1,
                live_phase: 0,
            }
        }

        let first = model_for("Not much. I'm ready to work.");
        let first_native_stream = active_streaming_lines(Some(&first), 80);
        assert_eq!(first_native_stream.len(), 1);

        let first_viewport = active_viewport_lines_with_stream_skip(Some(&first), 80, 100, 0);
        assert_eq!(line_text(&first_viewport[0]), "");
        assert_eq!(
            line_text(&first_viewport[1]),
            "Not much. I'm ready to work."
        );

        let second = model_for("Not much. I'm ready to work.\n\nSend me the command.");
        let second_native_stream = active_streaming_lines(Some(&second), 80);
        let emitted_lines = second_native_stream.len().saturating_sub(1);
        let second_viewport =
            active_viewport_lines_with_stream_skip(Some(&second), 80, 100, emitted_lines);

        assert_eq!(line_text(&second_viewport[0]), "Send me the command.");
    }

    #[test]
    fn streaming_table_waits_for_block_boundary_before_native_commit() {
        let model =
            streaming_model_for("Intro.\n\n| Name | Count |\n| --- | ---: |\n| Apples | 12 |\n");

        let full_text = native_lines_text(&active_streaming_native_lines(Some(&model), 80));
        assert!(full_text.contains("Name"), "{full_text}");
        assert!(full_text.contains("Apples"), "{full_text}");

        let commit_text = native_lines_text(&active_streaming_native_commit_prefix_lines(
            Some(&model),
            80,
        ));
        assert!(commit_text.contains("Intro."), "{commit_text}");
        assert!(!commit_text.contains("Name"), "{commit_text}");
        assert!(!commit_text.contains("Apples"), "{commit_text}");
    }

    #[test]
    fn streaming_table_header_alone_waits_before_native_commit() {
        let model = streaming_model_for("Rendered:\n\n| ID | Name | Role |");

        let commit_text = native_lines_text(&active_streaming_native_commit_prefix_lines(
            Some(&model),
            80,
        ));
        assert!(commit_text.contains("Rendered:"), "{commit_text}");
        assert!(
            !commit_text.contains("| ID | Name | Role |"),
            "{commit_text}"
        );
    }

    #[test]
    fn streaming_table_header_waits_while_delimiter_is_partial() {
        let model = streaming_model_for("Intro.\n\n| Name | Count |\n| --");

        let commit_text = native_lines_text(&active_streaming_native_commit_prefix_lines(
            Some(&model),
            80,
        ));
        assert!(commit_text.contains("Intro."), "{commit_text}");
        assert!(!commit_text.contains("| Name | Count |"), "{commit_text}");
    }

    #[test]
    fn markdown_code_fence_table_does_not_trigger_streaming_holdback() {
        let model = streaming_model_for("```markdown\n| Name | Count |\n| --- | ---: |\n");

        let full_text = native_lines_text(&active_streaming_native_lines(Some(&model), 80));
        let commit_text = native_lines_text(&active_streaming_native_commit_prefix_lines(
            Some(&model),
            80,
        ));

        assert!(full_text.contains("| Name | Count |"), "{full_text}");
        assert!(commit_text.contains("| Name | Count |"), "{commit_text}");
        assert!(commit_text.contains("| --- | ---: |"), "{commit_text}");
    }

    #[test]
    fn blank_line_clears_pending_streaming_table_header() {
        let model = streaming_model_for("Intro.\n\n| Name | Count |\n\n");

        let commit_text = native_lines_text(&active_streaming_native_commit_prefix_lines(
            Some(&model),
            80,
        ));

        assert!(commit_text.contains("Intro."), "{commit_text}");
        assert!(commit_text.contains("| Name | Count |"), "{commit_text}");
    }

    #[test]
    fn closed_streaming_table_releases_following_paragraph() {
        let model = streaming_model_for(
            "Intro.\n\n| Name | Count |\n| --- | ---: |\n| Apples | 12 |\n\nNext paragraph\n",
        );

        let commit_text = native_lines_text(&active_streaming_native_commit_prefix_lines(
            Some(&model),
            80,
        ));
        assert!(commit_text.contains("Intro."), "{commit_text}");
        assert!(commit_text.contains("Name"), "{commit_text}");
        assert!(commit_text.contains("Apples"), "{commit_text}");
        assert!(commit_text.contains("Next paragraph"), "{commit_text}");
        assert!(!commit_text.contains("| Name | Count |"), "{commit_text}");
    }

    #[test]
    fn block_start_after_streaming_table_releases_holdback() {
        let model = streaming_model_for(
            "Intro.\n\n| Name | Count |\n| --- | ---: |\n| Apples | 12 |\n# Details\nMore text\n",
        );

        let commit_text = native_lines_text(&active_streaming_native_commit_prefix_lines(
            Some(&model),
            80,
        ));
        assert!(commit_text.contains("Intro."), "{commit_text}");
        assert!(commit_text.contains("Name"), "{commit_text}");
        assert!(commit_text.contains("Apples"), "{commit_text}");
        assert!(commit_text.contains("Details"), "{commit_text}");
        assert!(commit_text.contains("More text"), "{commit_text}");
        assert!(!commit_text.contains("| Name | Count |"), "{commit_text}");
    }

    #[test]
    fn prompt_tool_rows_use_one_blank_line() {
        let prompt = TranscriptNode {
            id: "prompt".to_string(),
            seq: 1,
            revision: 1,
            kind: TranscriptKind::Prompt {
                text: "whats this repo about".to_string(),
                followup: false,
            },
        };
        let run = TranscriptNode {
            id: "run".to_string(),
            seq: 2,
            revision: 2,
            kind: TranscriptKind::Timeline {
                group: "run".to_string(),
                lines: vec!["pwd && rg --files".to_string()],
                style: NodeStyle::Muted,
            },
        };

        let lines = cells_to_lines([&prompt, &run].into_iter(), 80, DisplayMode::Scrollback);

        assert_eq!(line_text(&lines[0]), "> whats this repo about");
        assert_eq!(line_text(&lines[1]), "");
        assert_eq!(line_text(&lines[2]), "• run");
    }

    #[test]
    fn followup_prompts_keep_a_gap_after_previous_output() {
        let answer = TranscriptNode {
            id: "answer".to_string(),
            seq: 1,
            revision: 1,
            kind: TranscriptKind::Assistant {
                markdown: "First answer.".to_string(),
                source: None,
            },
        };
        let followup = TranscriptNode {
            id: "followup".to_string(),
            seq: 2,
            revision: 2,
            kind: TranscriptKind::Prompt {
                text: "which chrome profiles do i have".to_string(),
                followup: true,
            },
        };

        let lines = cells_to_lines(
            [&answer, &followup].into_iter(),
            80,
            DisplayMode::Scrollback,
        );

        assert_eq!(line_text(&lines[0]), "First answer.");
        assert_eq!(line_text(&lines[1]), "");
        assert_eq!(line_text(&lines[2]), "> which chrome profiles do i have");
    }

    #[test]
    fn merging_timeline_nodes_compacts_consecutive_reads() {
        let mut last = TranscriptNode {
            id: "first".to_string(),
            seq: 1,
            revision: 1,
            kind: TranscriptKind::Timeline {
                group: "explored".to_string(),
                lines: vec!["read README.md".to_string()],
                style: NodeStyle::Normal,
            },
        };
        let next = TranscriptNode {
            id: "second".to_string(),
            seq: 2,
            revision: 2,
            kind: TranscriptKind::Timeline {
                group: "explored".to_string(),
                lines: vec![
                    "read Cargo.toml".to_string(),
                    "list . (10 items)".to_string(),
                    "read Taskfile.yml".to_string(),
                ],
                style: NodeStyle::Normal,
            },
        };

        assert!(merge_timeline_node(&mut last, &next));
        let TranscriptKind::Timeline { lines, .. } = &last.kind else {
            panic!("expected timeline node");
        };
        assert_eq!(
            lines,
            &[
                "read README.md, Cargo.toml".to_string(),
                "list . (10 items)".to_string(),
                "read Taskfile.yml".to_string(),
            ]
        );
    }

    #[test]
    fn browser_script_failures_render_compact_diagnosis() {
        let event = EventRecord {
            seq: 7,
            id: "event-7".to_string(),
            session_id: "session".to_string(),
            ts_ms: 0,
            event_type: "tool.failed".to_string(),
            payload: serde_json::json!({
                "name": "browser_script",
                "error": "Traceback (most recent call last):\nRuntimeError: read CDP Runtime.evaluate: IO error",
                "diagnosis": {
                    "summary": "Browser is still connected; the same page should still be usable.",
                    "what_happened": "A CDP read timed out while waiting for Chrome.",
                    "next_step": "Continue on the same page with a smaller chunk.",
                    "browser_usable": true,
                    "page_usable": true,
                    "error_kind": "cdp-read-timeout"
                }
            }),
        };

        let lines = tool_failed_lines(&event);

        assert_eq!(lines[0], "browser_script failed");
        assert!(lines.iter().any(|line| line.contains("same page")));
        assert!(lines
            .iter()
            .any(|line| line.contains("Next: Continue on the same page")));
        assert!(lines
            .iter()
            .any(|line| line.contains("Details: RuntimeError")));
        assert!(!lines
            .iter()
            .any(|line| line.contains("Traceback (most recent call last)")));
    }

    #[test]
    fn shell_tool_failures_render_as_compact_shell_output() {
        let event = EventRecord {
            seq: 8,
            id: "event-8".to_string(),
            session_id: "session".to_string(),
            ts_ms: 0,
            event_type: "tool.failed".to_string(),
            payload: serde_json::json!({
                "name": "shell",
                "error": format!("--- README.md{}\nCargo.toml\nsrc/main.rs", "x".repeat(400))
            }),
        };

        let node = tool_failed_node(&event).expect("tool failed node");
        let TranscriptKind::Timeline {
            group,
            lines,
            style,
        } = &node.kind
        else {
            panic!("expected timeline node");
        };

        assert_eq!(group, "shell");
        assert_eq!(*style, NodeStyle::Failed);
        assert_eq!(lines.len(), 1);
        assert!(lines[0].starts_with("failed: --- README.md"), "{lines:?}");
        assert!(lines[0].contains("(+2 lines)"), "{lines:?}");
        assert!(!lines[0].contains("Cargo.toml"), "{lines:?}");

        let rendered = node
            .display_lines(100, DisplayMode::Scrollback)
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(rendered.contains("• shell"), "{rendered}");
    }

    #[test]
    fn successful_shell_tool_output_is_hidden() {
        let event = EventRecord {
            seq: 8,
            id: "event-8".to_string(),
            session_id: "session".to_string(),
            ts_ms: 0,
            event_type: "tool.output".to_string(),
            payload: serde_json::json!({
                "name": "shell",
                "text": "hello from command\nsecond line"
            }),
        };

        assert!(tool_output_node(&event).is_none());
    }

    #[test]
    fn adjacent_shell_command_and_successful_output_render_command_only() {
        let begin = EventRecord {
            seq: 8,
            id: "event-8".to_string(),
            session_id: "session".to_string(),
            ts_ms: 0,
            event_type: "exec_command.begin".to_string(),
            payload: serde_json::json!({
                "name": "shell",
                "process_id": "23817",
                "session_id": 23817,
                "command": ["bash", "-lc", "cargo test -p browser-use-tui"]
            }),
        };
        let output = EventRecord {
            seq: 9,
            id: "event-9".to_string(),
            session_id: "session".to_string(),
            ts_ms: 0,
            event_type: "tool.output".to_string(),
            payload: serde_json::json!({
                "name": "shell",
                "text": "/tmp/project\nCargo.toml\nREADME.md\nsrc/main.rs"
            }),
        };
        let mut committed = Vec::new();

        push_committed_node(
            &mut committed,
            exec_command_begin_node(&begin).expect("exec begin node"),
        );
        if let Some(node) = tool_output_node(&output) {
            push_committed_node(&mut committed, node);
        }
        let lines = cells_to_lines(committed.iter(), 120, DisplayMode::Scrollback);
        let text = lines.iter().map(line_text).collect::<Vec<_>>().join("\n");
        let shell_headers = lines
            .iter()
            .filter(|line| line_text(line) == "• shell")
            .count();

        assert_eq!(committed.len(), 1, "{text}");
        assert_eq!(shell_headers, 1, "{text}");
        assert!(
            text.contains("ran command: cargo test -p browser-use-tui"),
            "{text}"
        );
        assert!(!text.contains("output:"), "{text}");
        assert!(!text.contains("/tmp/project"), "{text}");
        assert!(!text.contains("Cargo.toml"), "{text}");
    }

    #[test]
    fn terminal_shell_delta_keeps_failed_tool_header() {
        let command = TranscriptNode {
            id: "command".to_string(),
            seq: 1,
            revision: 1,
            kind: TranscriptKind::Timeline {
                group: "shell".to_string(),
                lines: vec!["ran command: pwd".to_string()],
                style: NodeStyle::Normal,
            },
        };
        let failure = TranscriptNode {
            id: "failure".to_string(),
            seq: 2,
            revision: 2,
            kind: TranscriptKind::Timeline {
                group: "shell".to_string(),
                lines: vec!["failed: /tmp/project".to_string()],
                style: NodeStyle::Failed,
            },
        };
        let mut committed = Vec::new();
        push_committed_node(&mut committed, command.clone());
        push_committed_node(&mut committed, failure.clone());
        let model = TranscriptModel {
            session_id: "session".to_string(),
            committed,
            terminal_committed: vec![command, failure],
            active: None,
            last_event_seq: 2,
            live_phase: 0,
        };

        let full = terminal_scrollback_emission_since(&model, 0, 120, false);
        let full_text = full
            .lines
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(
            full.lines
                .iter()
                .filter(|line| line_text(line) == "• shell")
                .count(),
            1,
            "{full_text}"
        );

        let delta = terminal_scrollback_emission_since(&model, 1, 120, false);
        let delta_text = delta
            .lines
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(delta_text.contains("• shell"), "{delta_text}");
        assert!(delta_text.contains("failed: /tmp/project"), "{delta_text}");
    }

    enum ExpectedCommandRender {
        Explored(&'static str),
        ShellCommand(&'static str),
        CommandTool(&'static str),
        ShellScript(&'static str),
    }

    struct CommandRenderCase {
        name: &'static str,
        tool_name: &'static str,
        command: &'static str,
        expected: ExpectedCommandRender,
        forbidden: &'static [&'static str],
    }

    #[test]
    fn shell_command_rendering_classifies_common_real_commands() {
        let cases = [
            CommandRenderCase {
                name: "plain pwd",
                tool_name: "shell",
                command: "pwd",
                expected: ExpectedCommandRender::Explored("read repository metadata"),
                forbidden: &[COMMAND_LINE_PREFIX],
            },
            CommandRenderCase {
                name: "list files pipeline",
                tool_name: "shell",
                command: "pwd && find . -maxdepth 2 -type f | sed 's#^./##' | sort | head -200",
                expected: ExpectedCommandRender::Explored("listed files"),
                forbidden: &[COMMAND_LINE_PREFIX],
            },
            CommandRenderCase {
                name: "list directories pipeline",
                tool_name: "shell",
                command: "find . -maxdepth 2 -type d | sed 's#^./##' | sort | head -100",
                expected: ExpectedCommandRender::Explored("listed directories"),
                forbidden: &[COMMAND_LINE_PREFIX],
            },
            CommandRenderCase {
                name: "ripgrep file listing",
                tool_name: "shell",
                command: "rg --files -g 'README*' -g Cargo.toml -g pyproject.toml",
                expected: ExpectedCommandRender::Explored("listed files"),
                forbidden: &[COMMAND_LINE_PREFIX],
            },
            CommandRenderCase {
                name: "read one file",
                tool_name: "shell",
                command: "sed -n '1,220p' README.md",
                expected: ExpectedCommandRender::Explored("read README.md"),
                forbidden: &[COMMAND_LINE_PREFIX],
            },
            CommandRenderCase {
                name: "read multiple files",
                tool_name: "shell",
                command: "cat Cargo.toml && printf '\\n--- pyproject ---\\n' && cat pyproject.toml",
                expected: ExpectedCommandRender::Explored("read Cargo.toml, pyproject.toml"),
                forbidden: &[COMMAND_LINE_PREFIX],
            },
            CommandRenderCase {
                name: "repo search",
                tool_name: "shell",
                command: "grep -R \"terminal-ui\" docs crates | head -20",
                expected: ExpectedCommandRender::Explored("searched repository"),
                forbidden: &[COMMAND_LINE_PREFIX],
            },
            CommandRenderCase {
                name: "git status",
                tool_name: "shell",
                command: "git status --short --branch",
                expected: ExpectedCommandRender::Explored("read git status"),
                forbidden: &[COMMAND_LINE_PREFIX],
            },
            CommandRenderCase {
                name: "git log",
                tool_name: "shell",
                command: "git log --oneline -10",
                expected: ExpectedCommandRender::Explored("read git log"),
                forbidden: &[COMMAND_LINE_PREFIX],
            },
            CommandRenderCase {
                name: "git diff",
                tool_name: "shell",
                command: "git diff --stat",
                expected: ExpectedCommandRender::Explored("read git diff"),
                forbidden: &[COMMAND_LINE_PREFIX],
            },
            CommandRenderCase {
                name: "toolchain metadata",
                tool_name: "shell",
                command: "rustc --version; cargo --version; cargo metadata --no-deps --format-version 1 | head -40",
                expected: ExpectedCommandRender::Explored("read toolchain metadata"),
                forbidden: &[COMMAND_LINE_PREFIX],
            },
            CommandRenderCase {
                name: "disk usage probe",
                tool_name: "shell",
                command: "du -sh ./* 2>/dev/null | sort -h | tail -20",
                expected: ExpectedCommandRender::Explored("read repository metadata"),
                forbidden: &[COMMAND_LINE_PREFIX],
            },
            CommandRenderCase {
                name: "stderr-only redirection probe",
                tool_name: "shell",
                command: "find . -type f 2>/dev/null | head -20",
                expected: ExpectedCommandRender::Explored("listed files"),
                forbidden: &[COMMAND_LINE_PREFIX],
            },
            CommandRenderCase {
                name: "cargo test",
                tool_name: "shell",
                command: "cargo test -p browser-use-tui transcript::tests::",
                expected: ExpectedCommandRender::ShellCommand("ran command: cargo test -p browser-use-tui transcript::tests::"),
                forbidden: &["• explored"],
            },
            CommandRenderCase {
                name: "cargo check",
                tool_name: "shell",
                command: "cargo check -p browser-use-tui",
                expected: ExpectedCommandRender::ShellCommand("ran command: cargo check -p browser-use-tui"),
                forbidden: &["• explored"],
            },
            CommandRenderCase {
                name: "npm run",
                tool_name: "shell",
                command: "npm run dev",
                expected: ExpectedCommandRender::ShellCommand("ran command: npm run dev"),
                forbidden: &["• explored"],
            },
            CommandRenderCase {
                name: "uv pytest",
                tool_name: "shell",
                command: "uv run --with pytest python -m pytest -q",
                expected: ExpectedCommandRender::ShellCommand("ran command: uv run --with pytest python -m pytest -q"),
                forbidden: &["• explored"],
            },
            CommandRenderCase {
                name: "python heredoc",
                tool_name: "shell",
                command: "python3 - <<'PY'\nprint('hello')\nPY",
                expected: ExpectedCommandRender::ShellCommand("ran command: python3 - <<'PY'"),
                forbidden: &["• explored", "\nprint"],
            },
            CommandRenderCase {
                name: "repo script",
                tool_name: "shell",
                command: "scripts/verify-terminal-ui.sh",
                expected: ExpectedCommandRender::ShellCommand("ran command: scripts/verify-terminal-ui.sh"),
                forbidden: &["• explored"],
            },
            CommandRenderCase {
                name: "open browser",
                tool_name: "shell",
                command: "open -na \"Google Chrome\" --args --profile-directory=\"Default\"",
                expected: ExpectedCommandRender::ShellCommand("ran command: open -na \"Google Chrome\" --args --profile-directory=\"Default\""),
                forbidden: &["• explored"],
            },
            CommandRenderCase {
                name: "stdout write redirection",
                tool_name: "shell",
                command: "printf 'Edited random line' > random-note-123.txt",
                expected: ExpectedCommandRender::ShellCommand("ran command: printf"),
                forbidden: &["• explored"],
            },
            CommandRenderCase {
                name: "append redirection",
                tool_name: "shell",
                command: "cat README.md >> notes.txt",
                expected: ExpectedCommandRender::ShellCommand("ran command: cat README.md >> notes.txt"),
                forbidden: &["• explored"],
            },
            CommandRenderCase {
                name: "tee write",
                tool_name: "shell",
                command: "sed -n '1p' README.md | tee random-note-123.txt",
                expected: ExpectedCommandRender::ShellCommand("ran command: sed -n '1p' README.md"),
                forbidden: &["• explored"],
            },
            CommandRenderCase {
                name: "find delete",
                tool_name: "shell",
                command: "find tmp -type f -delete",
                expected: ExpectedCommandRender::ShellCommand("ran command: find tmp -type f -delete"),
                forbidden: &["• explored", "listed files"],
            },
            CommandRenderCase {
                name: "find exec",
                tool_name: "shell",
                command: "find tmp -type f -exec rm {} \\;",
                expected: ExpectedCommandRender::ShellCommand("ran command: find tmp -type f -exec rm"),
                forbidden: &["• explored", "listed files"],
            },
            CommandRenderCase {
                name: "xargs mutation",
                tool_name: "shell",
                command: "find tmp -type f -print0 | xargs -0 rm",
                expected: ExpectedCommandRender::ShellCommand("ran command: find tmp -type f -print0 | xargs -0 rm"),
                forbidden: &["• explored", "listed files"],
            },
            CommandRenderCase {
                name: "apply patch through shell",
                tool_name: "shell",
                command: "apply_patch <<'PATCH'\n*** Begin Patch\n*** End Patch\nPATCH",
                expected: ExpectedCommandRender::ShellCommand("ran command: apply_patch <<'PATCH'"),
                forbidden: &["• explored", "\n*** Begin Patch"],
            },
            CommandRenderCase {
                name: "generic exec command tool",
                tool_name: "exec_command",
                command: "npm run dev",
                expected: ExpectedCommandRender::CommandTool("ran command: npm run dev"),
                forbidden: &["• shell", "• explored"],
            },
            CommandRenderCase {
                name: "long mutating shell script",
                tool_name: "shell",
                command: concat!(
                    "set -euo pipefail\n",
                    "DEMO_DIR=\"tmp-random-files-demo-$(date +%s)-$$\"\n",
                    "mkdir \"$DEMO_DIR\"\n",
                    "for i in 1 2 3 4 5; do\n",
                    "  head -c $((64 + i * 17)) /dev/urandom > \"$DEMO_DIR/file-$i.bin\"\n",
                    "done\n",
                    "ls -lh \"$DEMO_DIR\"\n",
                    "rm -rf \"$DEMO_DIR\"\n",
                    "test ! -e \"$DEMO_DIR\""
                ),
                expected: ExpectedCommandRender::ShellScript("ran shell script: mkdir, head, ls, rm"),
                forbidden: &["tmp-random-files-demo", "command truncated", "• explored"],
            },
        ];

        for case in cases {
            let text = rendered_exec_command_text(case.tool_name, case.command, 120);
            match case.expected {
                ExpectedCommandRender::Explored(expected) => {
                    assert!(text.contains("• explored"), "{}\n{text}", case.name);
                    assert!(text.contains(expected), "{}\n{text}", case.name);
                }
                ExpectedCommandRender::ShellCommand(expected) => {
                    assert!(text.contains("• shell"), "{}\n{text}", case.name);
                    assert!(text.contains(COMMAND_LINE_PREFIX), "{}\n{text}", case.name);
                    assert!(text.contains(expected), "{}\n{text}", case.name);
                }
                ExpectedCommandRender::CommandTool(expected) => {
                    assert!(text.contains("• command"), "{}\n{text}", case.name);
                    assert!(text.contains(expected), "{}\n{text}", case.name);
                }
                ExpectedCommandRender::ShellScript(expected) => {
                    assert!(text.contains("• shell"), "{}\n{text}", case.name);
                    assert!(text.contains(expected), "{}\n{text}", case.name);
                    assert!(!text.contains(COMMAND_LINE_PREFIX), "{}\n{text}", case.name);
                }
            }
            for forbidden in case.forbidden {
                assert!(!text.contains(forbidden), "{}\n{text}", case.name);
            }
        }
    }

    #[test]
    fn consecutive_shell_commands_keep_separate_events_but_render_capped_cluster() {
        let commands = [
            "cargo test -p browser-use-tui",
            "npm run dev",
            "uv run --with pytest python -m pytest -q",
            "scripts/verify-terminal-ui.sh",
        ];
        let mut committed = Vec::new();
        for (idx, command) in commands.iter().enumerate() {
            let seq = idx as i64 + 8;
            let event = EventRecord {
                seq,
                id: format!("event-{seq}"),
                session_id: "session".to_string(),
                ts_ms: 0,
                event_type: "exec_command.begin".to_string(),
                payload: serde_json::json!({
                    "name": "shell",
                    "process_id": seq.to_string(),
                    "session_id": seq,
                    "command": ["bash", "-lc", command]
                }),
            };
            push_committed_node(
                &mut committed,
                exec_command_begin_node(&event).expect("exec begin node"),
            );
        }

        let lines = cells_to_lines(committed.iter(), 120, DisplayMode::Scrollback);
        let text = lines.iter().map(line_text).collect::<Vec<_>>().join("\n");

        assert_eq!(committed.len(), 4, "{text}");
        assert_eq!(
            lines
                .iter()
                .filter(|line| line_text(line) == "• shell")
                .count(),
            1,
            "{text}"
        );
        assert!(
            text.contains("ran command: cargo test -p browser-use-tui"),
            "{text}"
        );
        assert!(text.contains("ran command: npm run dev"), "{text}");
        assert!(!text.contains("python -m pytest"), "{text}");
        assert!(!text.contains("scripts/verify-terminal-ui.sh"), "{text}");
        assert!(text.contains("... +2 commands"), "{text}");
    }

    #[test]
    fn exploration_shell_commands_render_as_explored_activity() {
        let commands = [
            "find . -maxdepth 2 -type f | sed 's#^./##' | sort | head -200",
            "sed -n '1,220p' README.md",
            "cat Cargo.toml",
            "rg --files -g 'README*' -g Cargo.toml",
        ];
        let mut committed = Vec::new();
        for (idx, command) in commands.iter().enumerate() {
            let seq = idx as i64 + 8;
            let event = EventRecord {
                seq,
                id: format!("event-{seq}"),
                session_id: "session".to_string(),
                ts_ms: 0,
                event_type: "exec_command.begin".to_string(),
                payload: serde_json::json!({
                    "name": "shell",
                    "process_id": seq.to_string(),
                    "session_id": seq,
                    "command": ["bash", "-lc", command]
                }),
            };
            push_committed_node(
                &mut committed,
                exec_command_begin_node(&event).expect("exec begin node"),
            );
        }

        let lines = cells_to_lines(committed.iter(), 120, DisplayMode::Scrollback);
        let text = lines.iter().map(line_text).collect::<Vec<_>>().join("\n");

        assert!(text.contains("• explored"), "{text}");
        assert!(!text.contains("• shell"), "{text}");
        assert!(text.contains("listed files"), "{text}");
        assert!(text.contains("read README.md, Cargo.toml"), "{text}");
        assert!(!text.contains("ran command: find"), "{text}");
    }

    #[test]
    fn read_only_git_commands_render_as_explored_activity() {
        let commands = [
            ("git status --short", "read git status"),
            ("git log --oneline -10", "read git log"),
            ("git diff --stat", "read git diff"),
        ];
        for (idx, (command, expected)) in commands.iter().enumerate() {
            let seq = idx as i64 + 8;
            let event = EventRecord {
                seq,
                id: format!("event-{seq}"),
                session_id: "session".to_string(),
                ts_ms: 0,
                event_type: "exec_command.begin".to_string(),
                payload: serde_json::json!({
                    "name": "shell",
                    "process_id": seq.to_string(),
                    "session_id": seq,
                    "command": ["bash", "-lc", command]
                }),
            };

            let node = exec_command_begin_node(&event).expect("exec begin node");
            let text = node
                .display_lines(120, DisplayMode::Scrollback)
                .iter()
                .map(line_text)
                .collect::<Vec<_>>()
                .join("\n");
            assert!(text.contains("• explored"), "{text}");
            assert!(text.contains(expected), "{text}");
            assert!(!text.contains(COMMAND_LINE_PREFIX), "{text}");
        }
    }

    #[test]
    fn compound_repo_probe_with_du_dash_sh_renders_as_explored() {
        let command = concat!(
            "printf 'PWD: '; pwd; ",
            "printf '\\nTop-level files:\\n'; ls -la; ",
            "printf '\\nDisk usage top dirs:\\n'; du -sh ./* 2>/dev/null | sort -h | tail -20"
        );
        let event = EventRecord {
            seq: 8,
            id: "event-8".to_string(),
            session_id: "session".to_string(),
            ts_ms: 0,
            event_type: "exec_command.begin".to_string(),
            payload: serde_json::json!({
                "name": "shell",
                "process_id": "23817",
                "session_id": 23817,
                "command": ["bash", "-lc", command]
            }),
        };

        let node = exec_command_begin_node(&event).expect("exec begin node");
        let text = node
            .display_lines(120, DisplayMode::Scrollback)
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n");

        assert!(text.contains("• explored"), "{text}");
        assert!(text.contains("listed files"), "{text}");
        assert!(!text.contains("• shell"), "{text}");
        assert!(!text.contains(COMMAND_LINE_PREFIX), "{text}");
    }

    #[test]
    fn toolchain_metadata_probe_renders_as_explored() {
        let command = concat!(
            "printf 'Rust toolchain:\\n'; rustc --version; cargo --version; ",
            "printf '\\nCargo workspace metadata packages:\\n'; ",
            "cargo metadata --no-deps --format-version 1 | head -40"
        );
        let event = EventRecord {
            seq: 8,
            id: "event-8".to_string(),
            session_id: "session".to_string(),
            ts_ms: 0,
            event_type: "exec_command.begin".to_string(),
            payload: serde_json::json!({
                "name": "shell",
                "process_id": "23817",
                "session_id": 23817,
                "command": ["bash", "-lc", command]
            }),
        };

        let node = exec_command_begin_node(&event).expect("exec begin node");
        let text = node
            .display_lines(120, DisplayMode::Scrollback)
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n");

        assert!(text.contains("• explored"), "{text}");
        assert!(text.contains("read toolchain metadata"), "{text}");
        assert!(!text.contains("• shell"), "{text}");
        assert!(!text.contains(COMMAND_LINE_PREFIX), "{text}");
    }

    #[test]
    fn regular_shell_commands_stay_shell() {
        for command in [
            "cargo test -p browser-use-tui",
            "npm run dev",
            "scripts/verify-terminal-ui.sh",
        ] {
            let event = EventRecord {
                seq: 8,
                id: "event-8".to_string(),
                session_id: "session".to_string(),
                ts_ms: 0,
                event_type: "exec_command.begin".to_string(),
                payload: serde_json::json!({
                    "name": "shell",
                    "process_id": "23817",
                    "session_id": 23817,
                    "command": ["bash", "-lc", command]
                }),
            };

            let node = exec_command_begin_node(&event).expect("exec begin node");
            let text = node
                .display_lines(120, DisplayMode::Scrollback)
                .iter()
                .map(line_text)
                .collect::<Vec<_>>()
                .join("\n");
            assert!(text.contains("• shell"), "{text}");
            assert!(
                text.contains(&format!("{COMMAND_LINE_PREFIX}{command}")),
                "{text}"
            );
        }
    }

    #[test]
    fn write_redirection_commands_stay_shell() {
        for command in [
            "printf 'Edited random line' > random-note-123.txt",
            "cat README.md > random-note-copy.txt",
            "sed -n '1p' README.md | tee random-note-123.txt",
        ] {
            let event = EventRecord {
                seq: 8,
                id: "event-8".to_string(),
                session_id: "session".to_string(),
                ts_ms: 0,
                event_type: "exec_command.begin".to_string(),
                payload: serde_json::json!({
                    "name": "shell",
                    "process_id": "23817",
                    "session_id": 23817,
                    "command": ["bash", "-lc", command]
                }),
            };

            let node = exec_command_begin_node(&event).expect("exec begin node");
            let text = node
                .display_lines(120, DisplayMode::Scrollback)
                .iter()
                .map(line_text)
                .collect::<Vec<_>>()
                .join("\n");
            assert!(text.contains("• shell"), "{text}");
            assert!(text.contains(COMMAND_LINE_PREFIX), "{text}");
            assert!(!text.contains("• explored"), "{text}");
        }
    }

    #[test]
    fn consecutive_successful_shell_outputs_are_hidden() {
        let outputs = ["/tmp/project", "Cargo.toml", "README.md", "src/main.rs"];
        for (idx, output) in outputs.iter().enumerate() {
            let seq = idx as i64 + 8;
            let event = EventRecord {
                seq,
                id: format!("event-{seq}"),
                session_id: "session".to_string(),
                ts_ms: 0,
                event_type: "tool.output".to_string(),
                payload: serde_json::json!({
                    "name": "shell",
                    "text": output
                }),
            };
            assert!(tool_output_node(&event).is_none());
        }
    }

    #[test]
    fn exec_command_begin_renders_shell_command_input() {
        let event = EventRecord {
            seq: 8,
            id: "event-8".to_string(),
            session_id: "session".to_string(),
            ts_ms: 0,
            event_type: "exec_command.begin".to_string(),
            payload: serde_json::json!({
                "name": "shell",
                "process_id": "23817",
                "session_id": 23817,
                "command": [
                    "bash",
                    "-lc",
                    "open -na \"Google Chrome\" --args --profile-directory=\"Default\""
                ]
            }),
        };

        let node = exec_command_begin_node(&event).expect("exec begin node");
        let text = node
            .display_lines(120, DisplayMode::Scrollback)
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n");

        assert!(text.contains("• shell"), "{text}");
        assert!(
            text.contains(
                "ran command: open -na \"Google Chrome\" --args --profile-directory=\"Default\""
            ),
            "{text}"
        );
        assert!(!text.contains("bash -lc"), "{text}");
    }

    #[test]
    fn long_shell_command_is_plain_and_truncated() {
        let command = concat!(
            "cargo test -p browser-use-tui --features long-output-check && ",
            "scripts/verify-terminal-ui.sh --state-dir /tmp/browser-use-terminal-long-command && ",
            "npm run dev -- --host 127.0.0.1 --port 3000 --strict"
        );
        let event = EventRecord {
            seq: 8,
            id: "event-8".to_string(),
            session_id: "session".to_string(),
            ts_ms: 0,
            event_type: "exec_command.begin".to_string(),
            payload: serde_json::json!({
                "name": "shell",
                "process_id": "23817",
                "session_id": 23817,
                "command": ["bash", "-lc", command]
            }),
        };

        let node = exec_command_begin_node(&event).expect("exec begin node");
        let lines = node.display_lines(96, DisplayMode::Scrollback);
        let text = lines.iter().map(line_text).collect::<Vec<_>>().join("\n");
        let spans = lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .collect::<Vec<_>>();

        assert!(text.contains("• shell"), "{text}");
        assert!(text.contains("ran command: cargo test"), "{text}");
        assert!(text.contains("command truncated"), "{text}");
        assert!(!spans
            .iter()
            .any(|span| span.content.as_ref() == "cargo" && span.style == activity_run()));
        assert!(!spans
            .iter()
            .any(|span| span.content.as_ref() == "--features" && span.style == activity_task()));
        assert!(!spans
            .iter()
            .any(|span| span.content.contains("long-output-check")
                && span.style == activity_search()));
    }

    #[test]
    fn long_mutating_shell_script_renders_compact_script_summary() {
        let command = concat!(
            "set -euo pipefail\n",
            "DEMO_DIR=\"tmp-random-files-demo-$(date +%s)-$$\"\n",
            "mkdir \"$DEMO_DIR\"\n",
            "for i in 1 2 3 4 5; do\n",
            "  head -c $((64 + i * 17)) /dev/urandom > \"$DEMO_DIR/file-$i.bin\"\n",
            "done\n",
            "ls -lh \"$DEMO_DIR\"\n",
            "rm -rf \"$DEMO_DIR\"\n",
            "test ! -e \"$DEMO_DIR\""
        );
        let event = EventRecord {
            seq: 8,
            id: "event-8".to_string(),
            session_id: "session".to_string(),
            ts_ms: 0,
            event_type: "exec_command.begin".to_string(),
            payload: serde_json::json!({
                "name": "shell",
                "process_id": "23817",
                "session_id": 23817,
                "command": ["bash", "-lc", command]
            }),
        };

        let node = exec_command_begin_node(&event).expect("exec begin node");
        let text = node
            .display_lines(96, DisplayMode::Scrollback)
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n");

        assert!(text.contains("• shell"), "{text}");
        assert!(
            text.contains("ran shell script: mkdir, head, ls, rm"),
            "{text}"
        );
        assert!(!text.contains("tmp-random-files-demo"), "{text}");
        assert!(!text.contains("command truncated"), "{text}");
    }

    #[test]
    fn shell_command_paths_are_styled_without_syntax_highlighting() {
        let event = EventRecord {
            seq: 8,
            id: "event-8".to_string(),
            session_id: "session".to_string(),
            ts_ms: 0,
            event_type: "exec_command.begin".to_string(),
            payload: serde_json::json!({
                "name": "shell",
                "process_id": "23817",
                "session_id": 23817,
                "command": [
                    "bash",
                    "-lc",
                    "cargo test --manifest-path /Users/reagan/project/Cargo.toml --config Cargo.toml"
                ]
            }),
        };

        let node = exec_command_begin_node(&event).expect("exec begin node");
        let lines = node.display_lines(160, DisplayMode::Scrollback);
        let text = lines.iter().map(line_text).collect::<Vec<_>>().join("\n");
        let spans = lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .collect::<Vec<_>>();

        assert!(
            text.contains(
                "ran command: cargo test --manifest-path /Users/reagan/project/Cargo.toml --config Cargo.toml"
            ),
            "{text}"
        );
        assert!(spans.iter().any(|span| span.content.as_ref()
            == "/Users/reagan/project/Cargo.toml"
            && span.style == link()));
        assert!(spans
            .iter()
            .any(|span| span.content.as_ref() == "Cargo.toml" && span.style == path_reference()));
        assert!(!spans
            .iter()
            .any(|span| span.content.as_ref() == "cargo" && span.style == activity_run()));
        assert!(!spans.iter().any(
            |span| span.content.as_ref() == "--manifest-path" && span.style == activity_task()
        ));
        assert!(!spans
            .iter()
            .any(|span| span.content.as_ref() == "test" && span.style == activity_search()));
    }

    #[test]
    fn terminal_interaction_renders_exact_input_with_command() {
        let begin = EventRecord {
            seq: 8,
            id: "event-8".to_string(),
            session_id: "session".to_string(),
            ts_ms: 0,
            event_type: "exec_command.begin".to_string(),
            payload: serde_json::json!({
                "name": "exec_command",
                "process_id": "42",
                "session_id": 42,
                "command": ["bash", "-lc", "npm run dev"]
            }),
        };
        let interaction = EventRecord {
            seq: 9,
            id: "event-9".to_string(),
            session_id: "session".to_string(),
            ts_ms: 0,
            event_type: "terminal.interaction".to_string(),
            payload: serde_json::json!({
                "process_id": "42",
                "session_id": 42,
                "stdin": "q\n"
            }),
        };
        let events = vec![begin, interaction.clone()];

        let node = terminal_interaction_node(&events, &interaction).expect("interaction node");
        let text = node
            .display_lines(120, DisplayMode::Scrollback)
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n");

        assert!(text.contains("• command"), "{text}");
        assert!(text.contains(r"wrote input to npm run dev: q\n"), "{text}");
    }

    #[test]
    fn terminal_interaction_empty_input_renders_poll_with_command() {
        let begin = EventRecord {
            seq: 8,
            id: "event-8".to_string(),
            session_id: "session".to_string(),
            ts_ms: 0,
            event_type: "exec_command.begin".to_string(),
            payload: serde_json::json!({
                "name": "exec_command",
                "process_id": "42",
                "session_id": 42,
                "command": ["bash", "-lc", "npm run dev"]
            }),
        };
        let interaction = EventRecord {
            seq: 9,
            id: "event-9".to_string(),
            session_id: "session".to_string(),
            ts_ms: 0,
            event_type: "terminal.interaction".to_string(),
            payload: serde_json::json!({
                "process_id": "42",
                "session_id": 42,
                "stdin": ""
            }),
        };
        let events = vec![begin, interaction.clone()];

        let node = terminal_interaction_node(&events, &interaction).expect("interaction node");
        let text = node
            .display_lines(120, DisplayMode::Scrollback)
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n");

        assert!(text.contains("• command"), "{text}");
        assert!(text.contains("checked output from npm run dev"), "{text}");
        assert!(!text.contains("wrote input"), "{text}");
    }

    #[test]
    fn empty_provider_wrapped_shell_output_is_hidden() {
        let event = EventRecord {
            seq: 8,
            id: "event-8".to_string(),
            session_id: "session".to_string(),
            ts_ms: 0,
            event_type: "tool.output".to_string(),
            payload: serde_json::json!({
                "name": "shell",
                "ok": true,
                "text": r#"[{"type":"text","text":""}]"#
            }),
        };

        assert!(tool_output_node(&event).is_none());
    }

    #[test]
    fn provider_wrapped_shell_output_is_hidden() {
        let event = EventRecord {
            seq: 8,
            id: "event-8".to_string(),
            session_id: "session".to_string(),
            ts_ms: 0,
            event_type: "tool.output".to_string(),
            payload: serde_json::json!({
                "name": "shell",
                "ok": true,
                "text": r#"[{"type":"text","text":"opened profile window"}]"#
            }),
        };

        assert!(tool_output_node(&event).is_none());
    }

    #[test]
    fn browser_profile_choice_renders_as_browser_summary() {
        let event = EventRecord {
            seq: 8,
            id: "event-8".to_string(),
            session_id: "session".to_string(),
            ts_ms: 0,
            event_type: "tool.output".to_string(),
            payload: serde_json::json!({
                "name": "browser",
                "text": serde_json::json!({
                    "status": "needs-user-action",
                    "reason": "Multiple local Chromium profiles are available. Ask the user which profile to use before connecting.",
                    "local_profiles": [
                        {
                            "id": "google-chrome:Default",
                            "display_name": "Google Chrome - Reagan",
                            "profile_dir": "Default",
                            "profile_path": "/Users/reagan/Library/Application Support/Google/Chrome/Default"
                        },
                        {
                            "id": "google-chrome:System Profile",
                            "display_name": "Google Chrome - System Profile",
                            "profile_dir": "System Profile",
                            "profile_path": "/Users/reagan/Library/Application Support/Google/Chrome/System Profile"
                        }
                    ],
                    "next_step": "Ask the user which profile to use, then run browser profile use <profile-id> before browser connect local."
                }).to_string()
            }),
        };

        let node = tool_output_node(&event).expect("tool output node");
        let text = node
            .display_lines(120, DisplayMode::Scrollback)
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n");

        assert!(text.contains("• browser"), "{text}");
        assert!(text.contains("choose browser profile"), "{text}");
        assert!(text.contains("Google Chrome - Reagan (Default)"), "{text}");
        assert!(
            text.contains("Google Chrome - System Profile (System Profile)"),
            "{text}"
        );
        assert!(
            text.contains("Next: Ask the user which profile to use"),
            "{text}"
        );
        assert!(!text.contains("local_profiles"), "{text}");
        assert!(!text.contains("Application Support"), "{text}");
    }

    #[test]
    fn routine_browser_status_output_is_hidden() {
        let event = EventRecord {
            seq: 8,
            id: "event-8".to_string(),
            session_id: "session".to_string(),
            ts_ms: 0,
            event_type: "tool.output".to_string(),
            payload: serde_json::json!({
                "name": "browser",
                "text": serde_json::json!({
                    "mode": "local",
                    "connection": "connected",
                    "browser": "Google Chrome",
                    "active_scripts": [],
                    "live_url": "https://live.browser-use.com/?wss=secret",
                    "endpoint": {
                        "http_url": "http://127.0.0.1:49385",
                        "ws_url": "ws://127.0.0.1:49385/devtools/browser/secret"
                    }
                }).to_string()
            }),
        };

        assert!(tool_output_node(&event).is_none());
    }

    #[test]
    fn routine_not_configured_browser_status_output_is_hidden() {
        let event = EventRecord {
            seq: 8,
            id: "event-8".to_string(),
            session_id: "session".to_string(),
            ts_ms: 0,
            event_type: "tool.output".to_string(),
            payload: serde_json::json!({
                "name": "browser",
                "text": serde_json::json!({
                    "mode": "none",
                    "connection": "not-configured",
                    "browser_task_blocked": true,
                    "next_step": "browser connect local",
                    "model_instruction": "Follow next_step."
                }).to_string()
            }),
        };

        assert!(tool_output_node(&event).is_none());
    }

    #[test]
    fn browser_profile_command_result_stays_visible() {
        let event = EventRecord {
            seq: 8,
            id: "event-8".to_string(),
            session_id: "session".to_string(),
            ts_ms: 0,
            event_type: "tool.output".to_string(),
            payload: serde_json::json!({
                "name": "browser",
                "text": serde_json::json!({
                    "status": "ok",
                    "profile_id": "google-chrome:Default"
                }).to_string()
            }),
        };

        let node = tool_output_node(&event).expect("tool output node");
        let text = node
            .display_lines(120, DisplayMode::Scrollback)
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n");

        assert!(text.contains("• browser"), "{text}");
        assert!(text.contains("browser profile selected"), "{text}");
        assert!(text.contains("profile: google-chrome:Default"), "{text}");
    }

    #[test]
    fn browser_script_summary_hides_raw_page_info_text() {
        let event = EventRecord {
            seq: 8,
            id: "event-8".to_string(),
            session_id: "session".to_string(),
            ts_ms: 0,
            event_type: "tool.output".to_string(),
            payload: serde_json::json!({
                "name": "browser_script",
                "text": "{'url': 'https://login.gusto.com/realms/zenpayroll/protocol/openid-connect/auth?client_id=zenpayroll&device_uuid=secret', 'title': 'Gusto Login - Payroll, Benefits, HR | Gusto', 'readyState': 'complete', 'target': {'targetId': 'B6CDD9676BD0503360290CD36A12A4D1'}}",
                "summary": [{
                    "kind": "page",
                    "url": "https://login.gusto.com/realms/zenpayroll/protocol/openid-connect/auth?client_id=zenpayroll&device_uuid=secret",
                    "title": "Gusto Login - Payroll, Benefits, HR | Gusto"
                }],
                "images": [{"path": "/tmp/page.png"}],
                "artifacts": [{"path": "/tmp/result.json"}]
            }),
        };

        let node = tool_output_node(&event).expect("tool output node");
        let text = node
            .display_lines(120, DisplayMode::Scrollback)
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n");

        assert!(text.contains("• browser"), "{text}");
        assert!(
            text.contains(
                "page: login.gusto.com/realms/zenpayroll/protocol/openid-connect/auth?..."
            ),
            "{text}"
        );
        assert!(text.contains("Gusto Login - Payroll"), "{text}");
        assert!(text.contains("1 image artifact"), "{text}");
        assert!(text.contains("1 file artifact"), "{text}");
        assert!(!text.contains("targetId"), "{text}");
        assert!(!text.contains("client_id=zenpayroll"), "{text}");
        assert!(!text.contains("readyState"), "{text}");
    }

    #[test]
    fn browser_script_summary_suppresses_running_transport_text() {
        let event = EventRecord {
            seq: 9,
            id: "event-9".to_string(),
            session_id: "session".to_string(),
            ts_ms: 0,
            event_type: "tool.output".to_string(),
            payload: serde_json::json!({
                "name": "browser_script",
                "text": "browser_script is still running.\nrun_id: bs-secret\nNext: observe this run again.",
                "summary": [{
                    "kind": "inspected",
                    "message": "Sampled 5 comments from current thread"
                }]
            }),
        };

        let node = tool_output_node(&event).expect("tool output node");
        let text = node
            .display_lines(120, DisplayMode::Scrollback)
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n");

        assert!(
            text.contains("Sampled 5 comments from current thread"),
            "{text}"
        );
        assert!(!text.contains("inspected Sampled"), "{text}");
        assert!(!text.contains("browser_script is still running"), "{text}");
        assert!(!text.contains("bs-secret"), "{text}");
    }

    #[test]
    fn browser_script_raw_text_fallback_is_bounded() {
        let line = format!(
            "{{'url': 'https://login.example.test/realms/acme/protocol/openid-connect/auth?client_id=zenpayroll&state={}', 'target': {{'targetId': '{}'}}}}",
            "x".repeat(240),
            "y".repeat(240)
        );

        let preview = browser_script_text_preview_lines(&line);

        assert_eq!(preview.len(), 1);
        assert!(preview[0].chars().count() <= 180, "{preview:?}");
        assert!(preview[0].ends_with("..."), "{preview:?}");
    }

    #[test]
    fn browser_script_running_text_fallback_hides_run_id() {
        let preview = browser_script_text_preview_lines(
            "browser_script is still running.\nNo new output in the last 50 ms.\nrun_id: bs-secret\nNext: observe this run again.",
        );

        assert!(preview.is_empty(), "{preview:?}");
        assert!(!preview.join("\n").contains("bs-secret"));
    }

    #[test]
    fn browser_script_partial_text_fallback_drops_runtime_instructions() {
        let preview = browser_script_text_preview_lines(
            "chunk one\n\nbrowser_script is still running.\nrun_id: bs-secret\nNext: observe this run again.",
        );

        assert_eq!(preview, vec!["chunk one"]);
        assert!(!preview.join("\n").contains("bs-secret"));
    }

    #[test]
    fn terminal_scrollback_emits_only_new_timeline_delta() {
        let raw_nodes = vec![
            TranscriptNode {
                id: "first".to_string(),
                seq: 1,
                revision: 1,
                kind: TranscriptKind::Timeline {
                    group: "explored".to_string(),
                    lines: vec!["read README.md".to_string()],
                    style: NodeStyle::Normal,
                },
            },
            TranscriptNode {
                id: "second".to_string(),
                seq: 2,
                revision: 2,
                kind: TranscriptKind::Timeline {
                    group: "explored".to_string(),
                    lines: vec!["read Cargo.toml".to_string()],
                    style: NodeStyle::Normal,
                },
            },
            TranscriptNode {
                id: "third".to_string(),
                seq: 3,
                revision: 3,
                kind: TranscriptKind::Timeline {
                    group: "explored".to_string(),
                    lines: vec!["read Taskfile.yml".to_string()],
                    style: NodeStyle::Normal,
                },
            },
        ];
        let mut committed = Vec::new();
        for node in raw_nodes.clone() {
            push_committed_node(&mut committed, node);
        }
        let model = TranscriptModel {
            session_id: "session".to_string(),
            committed,
            terminal_committed: raw_nodes,
            active: None,
            last_event_seq: 3,
            live_phase: 0,
        };

        let full = terminal_scrollback_emission_since(&model, 0, 120, false);
        let full_text = full
            .lines
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(full_text.contains("read README.md, Cargo.toml, Taskfile.yml"));

        let delta = terminal_scrollback_emission_since(&model, 1, 120, false);
        let delta_text = delta
            .lines
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(delta_text.contains("read Cargo.toml, Taskfile.yml"));
        assert!(!delta_text.contains("README.md"), "{delta_text}");
    }

    #[test]
    fn grouped_timeline_values_are_visually_nested_under_header() {
        let node = TranscriptNode {
            id: "test".to_string(),
            seq: 1,
            revision: 1,
            kind: TranscriptKind::Timeline {
                group: "explored".to_string(),
                lines: vec![
                    "read Taskfile.yml Cargo.toml README.md".to_string(),
                    "list . (200 items)".to_string(),
                ],
                style: NodeStyle::Normal,
            },
        };

        let lines = node.display_lines(24, DisplayMode::Scrollback);
        assert_eq!(line_text(&lines[0]), "• explored");
        assert!(line_text(&lines[1]).starts_with(GROUP_VALUE_RAIL_PREFIX));
        assert!(line_text(&lines[1]).contains("read"));
        assert!(line_text(&lines[2]).starts_with(GROUP_VALUE_RAIL_PREFIX));
        assert!(line_text(&lines[3]).starts_with(GROUP_VALUE_LAST_PREFIX));
        assert!(line_text(&lines[3]).contains("list"));
    }

    #[test]
    fn url_lines_keep_link_style_after_wrapping() {
        let node = TranscriptNode {
            id: "test".to_string(),
            seq: 1,
            revision: 1,
            kind: TranscriptKind::Timeline {
                group: "link".to_string(),
                lines: vec!["https://example.com/some/very/long/path".to_string()],
                style: NodeStyle::Normal,
            },
        };
        let lines = node.display_lines(20, DisplayMode::Scrollback);
        assert!(lines.iter().any(|line| {
            line.spans
                .iter()
                .any(|span| span.content.contains("https://") && span.style == link())
        }));
    }

    #[test]
    fn native_source_wraps_each_visible_fragment_with_original_target() {
        let source = "https://live.browser-use.com?wss=https%3A%2F%2Fexample.cdp.browser-use.com";
        let lines = native_source_display_lines(source, 42);
        let linked_targets = lines
            .iter()
            .flat_map(|line| line.links.iter().map(|link| link.target.as_str()))
            .collect::<Vec<_>>();

        assert!(linked_targets.len() > 1, "{linked_targets:?}");
        assert!(linked_targets.iter().all(|target| *target == source));
    }

    #[test]
    fn native_wrapped_links_keep_separate_targets_for_following_paths() {
        let url = "https://example.com/docs/";
        let path = "README.md";
        let lines = native_grouped_lines(
            "source",
            &[url.to_string(), path.to_string()],
            NodeStyle::Normal,
            32,
        );
        let linked_targets = lines
            .iter()
            .flat_map(|line| line.links.iter().map(|link| link.target.as_str()))
            .collect::<Vec<_>>();

        assert!(linked_targets.contains(&url), "{linked_targets:?}");
        assert!(linked_targets.contains(&path), "{linked_targets:?}");
        assert_eq!(
            linked_targets
                .iter()
                .filter(|target| **target == url)
                .count(),
            1
        );
        assert_eq!(
            linked_targets
                .iter()
                .filter(|target| **target == path)
                .count(),
            1
        );
    }

    #[test]
    fn native_activity_artifact_path_keeps_full_target_after_wrapping() {
        let path = "/Users/example/.browser-use-terminal/artifacts/session/capture-summary.gif";
        let lines = native_grouped_lines(
            "artifacts created",
            &[format!("summary_gif {path}")],
            NodeStyle::Normal,
            46,
        );
        let linked_targets = lines
            .iter()
            .flat_map(|line| line.links.iter().map(|link| link.target.as_str()))
            .collect::<Vec<_>>();

        assert!(linked_targets.len() > 1, "{linked_targets:?}");
        assert!(linked_targets.iter().all(|target| *target == path));
    }

    #[test]
    fn result_file_lines_render_full_path_as_clickable_text() {
        let path = "/tmp/browser use/artifacts/session/result.json";
        let lines = result_file_lines(path, Some(2048), Some("application/octet-stream"), 120);
        let text = lines.iter().map(line_text).collect::<Vec<_>>().join("\n");

        assert!(text.contains("Saved result file"), "{text}");
        assert!(text.contains(path), "{text}");
        assert!(text.contains("2.0 KB"), "{text}");
        assert!(!text.contains("file://"), "{text}");
        assert!(!text.contains("application/octet-stream"), "{text}");
        assert!(lines.iter().any(|line| {
            line.spans
                .iter()
                .any(|span| span.content.as_ref() == path && span.style == link())
        }));
    }

    #[test]
    fn run_values_style_paths_without_command_syntax_highlighting() {
        let command_spans = styled_value_spans(
            "run",
            "find crates -maxdepth 3 -type f | sort",
            text_style(),
        );
        assert!(command_spans
            .iter()
            .any(|span| span.content.as_ref() == "find" && span.style == text_style()));

        let path_spans = styled_value_spans(
            "run",
            "crates/browser-use-tui/src/markdown.rs",
            text_style(),
        );
        assert!(path_spans
            .iter()
            .any(|span| span.content.contains("markdown.rs") && span.style == path_reference()));
        assert!(!path_spans
            .iter()
            .any(|span| span.content.contains("markdown.rs") && span.style == link()));
    }

    #[test]
    fn activity_values_style_image_artifact_paths() {
        let spans = styled_value_spans(
            "took screenshot",
            "80677223915_search_results_sf_food.png",
            text_style(),
        );
        assert!(spans.iter().any(|span| {
            span.content.contains("search_results_sf_food.png") && span.style == path_reference()
        }));
    }

    #[test]
    fn nested_activity_run_lines_style_action_but_not_command_syntax() {
        let spans = styled_value_spans(
            "subagent repo explorer",
            "run pwd && find . -maxdepth 2 -type f | sed 's# ./##' | sort | head -200",
            text_style(),
        );
        assert!(spans
            .iter()
            .any(|span| span.content.as_ref() == "run" && span.style == activity_run()));
        assert!(spans
            .iter()
            .any(|span| span.content.as_ref() == "find" && span.style == text_style()));
        assert!(!spans
            .iter()
            .any(|span| span.content.contains("./##") && span.style == link()));
        assert!(!spans
            .iter()
            .any(|span| span.content.contains("./##") && span.style == path_reference()));
    }

    #[test]
    fn prose_slash_tokens_do_not_become_paths() {
        let spans = styled_value_spans(
            "subagent repo explorer",
            "task Inspect the repo: languages/frameworks...",
            text_style(),
        );
        assert!(spans
            .iter()
            .any(|span| span.content.as_ref() == "task" && span.style == activity_task()));
        assert!(!spans
            .iter()
            .any(|span| span.content.contains("languages/frameworks") && span.style == link()));
        assert!(!spans
            .iter()
            .any(|span| span.content.contains("languages/frameworks")
                && span.style == path_reference()));
    }

    #[test]
    fn child_activity_state_words_are_highlighted() {
        for (line, expected_style) in [
            ("working", thought()),
            ("list .", activity_list()),
            ("listed files", activity_list()),
            ("read Taskfile.yml", activity_read()),
            ("searched repository", activity_search()),
        ] {
            let spans = styled_value_spans("subagent repo explorer", line, text_style());
            let action = line.split_whitespace().next().unwrap_or(line);
            assert!(
                spans
                    .iter()
                    .any(|span| span.content.as_ref() == action && span.style == expected_style),
                "{line:?} did not highlight {action:?}"
            );
        }
    }

    #[test]
    fn activity_roles_use_distinct_styles() {
        let group_style = group_style(NodeStyle::Normal);
        for style in [
            activity_read(),
            activity_run(),
            activity_list(),
            activity_search(),
            activity_task(),
        ] {
            assert_ne!(group_style, style);
        }
        assert_ne!(activity_read(), activity_run());
        assert_ne!(activity_read(), activity_list());
        assert_ne!(activity_read(), activity_search());
        assert_ne!(activity_read(), activity_task());
        assert_ne!(activity_run(), activity_list());
        assert_ne!(activity_run(), activity_search());
        assert_ne!(activity_run(), activity_task());
        assert_ne!(activity_list(), activity_search());
        assert_ne!(activity_list(), activity_task());
        assert_ne!(activity_search(), activity_task());
    }

    #[test]
    fn timeline_group_labels_use_domain_styles() {
        assert_eq!(
            group_label_style("subagent repo_explorer started", NodeStyle::Normal),
            thought()
        );
        assert_eq!(group_label_style("run", NodeStyle::Normal), activity_run());
        assert_eq!(group_label_style("run", NodeStyle::Muted), activity_run());
        assert_eq!(group_label_style("shell", NodeStyle::Muted), activity_run());
        assert_eq!(
            group_label_style("command", NodeStyle::Muted),
            activity_run()
        );
        assert_eq!(
            group_label_style("explored", NodeStyle::Normal),
            activity_group()
        );
        assert_ne!(
            group_label_style("subagent repo_explorer started", NodeStyle::Normal),
            group_label_style("explored", NodeStyle::Normal)
        );
        assert_ne!(
            group_label_style("run", NodeStyle::Normal),
            group_label_style("explored", NodeStyle::Normal)
        );
    }

    #[test]
    fn source_for_state_keeps_headless_chromium_live_preview() {
        let live_url = "file:///tmp/browser-use-terminal/.capture.frames/live.html";
        let state = WorkbenchState {
            setup_complete: true,
            current_session: None,
            task: None,
            result: None,
            failure: None,
            activity: Vec::new(),
            transcript: Vec::new(),
            browser: browser_use_protocol::BrowserSummary {
                backend: "Headless Chromium".to_string(),
                status: "connected".to_string(),
                title: None,
                url: None,
                live_url: Some(live_url.to_string()),
                tabs: None,
                viewport: None,
            },
            telemetry: browser_use_protocol::TelemetrySummary::default(),
            history: Vec::new(),
        };

        assert_eq!(source_for_state(&state).as_deref(), Some(live_url));
    }

    #[test]
    fn cloud_promo_notice_body_and_link_are_colored() {
        let lines = notice_lines(
            "[tip] Use a Cloud browser to avoid manual permissions and get automatic captcha-solving! [cloud.browser-use.com]",
            120,
        );
        let spans = lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .collect::<Vec<_>>();

        assert!(spans
            .iter()
            .any(|span| span.content.as_ref() == "[tip]" && span.style == activity_task()));
        assert!(spans
            .iter()
            .any(|span| span.content.as_ref() == "Cloud" && span.style == activity_task()));
        assert!(spans
            .iter()
            .any(|span| span.content.as_ref() == "cloud.browser-use.com" && span.style == link()));
    }
}
