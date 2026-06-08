use anyhow::Result;
use browser_use_agent::{
    context::assembly::estimate_item_token_count, prompts::CollaborationModeKind,
    session::provider_messages_from_events,
};
use browser_use_protocol::{
    instruction_sources_from_events, startup_warnings_from_events, EventRecord, HistoryRow,
    TelemetrySummary, WorkbenchState,
};
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Margin, Position, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph, Widget, Wrap};
use ratatui::{Frame, Terminal};
use serde_json::Value;
use std::collections::BTreeMap;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::palette;
use crate::settings::{
    is_claude_code_account, ModelChoice, ACCOUNT_ANTHROPIC, ACCOUNT_CODEX, ACCOUNT_DEEPSEEK,
    ACCOUNT_OPENAI, ACCOUNT_OPENROUTER, AUTH_CHOICES, BROWSER_LOCAL_CHROME, BROWSER_USE_CLOUD,
};
use crate::theme::*;
use crate::transcript;

use super::{
    collaboration_mode_label, event_payload_text, format_goal_elapsed_seconds,
    format_goal_tokens_compact, goal_command_hint, goal_status_label,
    pending_active_followup_events_from_events, pending_queued_followup_events_from_events, App,
    BrowserSelectRow, CookieSyncStatus, DefaultProfileStatus, DomainFocus, DomainMode,
    FeedbackCategory, FeedbackStep, MessageActionKind, ModelSearchEntry, ProductState, SecretField,
    SecretFocus, SetupResultKind, Surface,
};

pub(crate) const APP_HORIZONTAL_MARGIN: u16 = 2;
const CONTENT_HORIZONTAL_MARGIN: u16 = 2;
pub(crate) const NATIVE_TRANSCRIPT_HORIZONTAL_MARGIN: u16 =
    APP_HORIZONTAL_MARGIN + CONTENT_HORIZONTAL_MARGIN;
pub(crate) fn render_dump(app: &mut App) -> Result<String> {
    app.drain_store_notifications()?;
    let backend = TestBackend::new(app.args.width, app.args.height);
    let mut terminal = Terminal::new(backend)?;
    terminal.draw(|frame| render(frame, app))?;
    Ok(buffer_to_string(terminal.backend().buffer()))
}

/// Like [`render_dump`] but also returns the terminal cursor position, so tests
/// can verify the blinking caret lands on the intended row/column.
#[cfg(test)]
pub(crate) fn render_dump_with_cursor(
    app: &mut App,
) -> Result<(String, Option<ratatui::layout::Position>)> {
    app.drain_store_notifications()?;
    let backend = TestBackend::new(app.args.width, app.args.height);
    let mut terminal = Terminal::new(backend)?;
    terminal.draw(|frame| render(frame, app))?;
    let cursor = terminal.get_cursor_position().ok();
    Ok((buffer_to_string(terminal.backend().buffer()), cursor))
}

/// The set of foreground colors used by filled block cells ("█"), so tests can
/// assert the context bar actually colors its segments per category.
#[cfg(test)]
pub(crate) fn render_filled_block_colors(
    app: &mut App,
) -> Result<std::collections::HashSet<ratatui::style::Color>> {
    app.drain_store_notifications()?;
    let backend = TestBackend::new(app.args.width, app.args.height);
    let mut terminal = Terminal::new(backend)?;
    terminal.draw(|frame| render(frame, app))?;
    let buffer = terminal.backend().buffer();
    let area = buffer.area;
    let mut colors = std::collections::HashSet::new();
    for y in area.y..area.y.saturating_add(area.height) {
        for x in area.x..area.x.saturating_add(area.width) {
            let cell = &buffer[(x, y)];
            if cell.symbol() == "█" {
                colors.insert(cell.fg);
            }
        }
    }
    Ok(colors)
}

#[cfg(test)]
pub(crate) fn render_text_foregrounds(
    app: &mut App,
    needle: &str,
) -> Result<Vec<ratatui::style::Color>> {
    app.drain_store_notifications()?;
    let backend = TestBackend::new(app.args.width, app.args.height);
    let mut terminal = Terminal::new(backend)?;
    terminal.draw(|frame| render(frame, app))?;
    let buffer = terminal.backend().buffer();
    let area = buffer.area;
    for y in area.y..area.y.saturating_add(area.height) {
        let mut text = String::new();
        let mut cells = Vec::new();
        for x in area.x..area.x.saturating_add(area.width) {
            let cell = &buffer[(x, y)];
            text.push_str(cell.symbol());
            cells.push(cell.fg);
        }
        if let Some(byte_start) = text.find(needle) {
            let start = text[..byte_start].chars().count();
            let end = start
                .saturating_add(needle.chars().count())
                .min(cells.len());
            return Ok(cells[start..end].to_vec());
        }
    }
    Ok(Vec::new())
}

fn buffer_to_string(buffer: &ratatui::buffer::Buffer) -> String {
    let area = buffer.area;
    let mut out = String::new();
    for y in area.y..area.y.saturating_add(area.height) {
        let mut line = String::new();
        for x in area.x..area.x.saturating_add(area.width) {
            line.push_str(buffer[(x, y)].symbol());
        }
        out.push_str(line.trim_end());
        out.push('\n');
    }
    out
}

pub(crate) fn native_scrollback_lines(app: &mut App, width: u16) -> Result<Vec<Line<'static>>> {
    app.drain_store_notifications()?;
    let state = app.workbench_state()?;
    let mut lines = transcript::transcript_model(app, &state)
        .map(|model| {
            transcript::all_terminal_scrollback_lines(&model, width.saturating_sub(4).max(1))
        })
        .unwrap_or_default();
    lines.push(Line::from(""));
    Ok(lines)
}

/// Strip trailing spaces from each line in place, so right-side column padding
/// stops counting toward the wrap budget. With this applied before a `Paragraph`
/// that has `Wrap` enabled, narrowing the terminal clips the empty tail off the
/// line rather than wrapping the padding to a new visual row.
fn trim_trailing_whitespace(lines: &mut Vec<Line<'static>>) {
    for line in lines.iter_mut() {
        while let Some(last) = line.spans.last_mut() {
            let trimmed_len = last.content.trim_end_matches(' ').len();
            if trimmed_len == 0 {
                line.spans.pop();
            } else {
                if trimmed_len != last.content.len() {
                    let style = last.style;
                    let trimmed = last.content[..trimmed_len].to_string();
                    *last = Span::styled(trimmed, style);
                }
                break;
            }
        }
    }
}

pub(crate) fn lines_plain_text(lines: &[Line<'static>]) -> String {
    let mut out = String::new();
    for line in lines {
        for span in &line.spans {
            out.push_str(&span.content);
        }
        out.push('\n');
    }
    out
}

pub(crate) fn render(frame: &mut Frame<'_>, app: &mut App) {
    let full_area = frame.area();
    let area = app_surface(full_area);
    // Reset each frame; only the composer status line re-records it when the
    // live URL is actually drawn. Prevents a stale link after disconnect.
    *app.live_link_overlay.borrow_mut() = None;

    if app.is_first_run_setup_visible().unwrap_or(false) || app.is_setup_cookie_sync_visible() {
        app.modal_background = None;
        let state = app
            .workbench_state()
            .unwrap_or_else(|_| app.empty_workbench_state_with_failure());
        // First-run setup always renders full-screen, whatever step it is on.
        let surface = if app.surface == Surface::Main {
            Surface::Setup
        } else {
            app.surface
        };
        render_surface(frame, area, app, &state, surface);
        return;
    }

    match app.surface {
        surface if surface.is_popup() => {
            if !app.native_scrollback_is_active() {
                if let Some(snapshot) = app
                    .modal_background
                    .as_ref()
                    .filter(|snapshot| snapshot.area == full_area)
                {
                    snapshot
                        .buffer
                        .merge_into(frame.buffer_mut(), snapshot.area);
                    let state = app.modal_overlay_state();
                    render_active_modal_overlay(frame, full_area, app, &state);
                    return;
                }
            }
            let state = main_render_state(app);
            let product_state = app.product_state(&state);
            render_main(frame, area, app, &state, product_state);
            if !app.native_scrollback_is_active() {
                app.modal_background = Some(super::ModalBackgroundSnapshot {
                    area: full_area,
                    buffer: frame.buffer_mut().clone(),
                });
                render_active_modal_overlay(frame, full_area, app, &state);
            }
        }
        surface if surface.uses_main_view() => {
            if (app.is_slash_palette_active() || app.prompt_history.search.is_some())
                && !app.native_scrollback_is_active()
            {
                if let Some(snapshot) = app
                    .modal_background
                    .as_ref()
                    .filter(|snapshot| snapshot.area == full_area)
                {
                    snapshot
                        .buffer
                        .merge_into(frame.buffer_mut(), snapshot.area);
                    let state = app.modal_overlay_state();
                    render_active_modal_overlay(frame, full_area, app, &state);
                    return;
                }
                let state = main_render_state(app);
                let product_state = app.product_state(&state);
                render_main(frame, area, app, &state, product_state);
                app.modal_background = Some(super::ModalBackgroundSnapshot {
                    area: full_area,
                    buffer: frame.buffer_mut().clone(),
                });
                render_active_modal_overlay(frame, full_area, app, &state);
            } else {
                let state = main_render_state(app);
                let product_state = app.product_state(&state);
                render_main(frame, area, app, &state, product_state);
                app.modal_background = Some(super::ModalBackgroundSnapshot {
                    area: full_area,
                    buffer: frame.buffer_mut().clone(),
                });
            }
        }
        Surface::FeedbackThanks => {
            app.modal_background = None;
            render_feedback_thanks(frame, area, app);
        }
        surface => {
            app.modal_background = None;
            let state = app
                .workbench_state()
                .unwrap_or_else(|_| app.empty_workbench_state_with_failure());
            render_surface(frame, area, app, &state, surface)
        }
    }
}

fn main_render_state(app: &mut App) -> WorkbenchState {
    if app.native_scrollback_is_active() {
        app.session_render_state()
    } else {
        app.workbench_state()
            .unwrap_or_else(|_| app.empty_workbench_state_with_failure())
    }
}

fn app_surface(area: Rect) -> Rect {
    area.inner(Margin {
        vertical: 0,
        horizontal: APP_HORIZONTAL_MARGIN,
    })
}

fn content_area(area: Rect) -> Rect {
    area.inner(Margin {
        vertical: 0,
        horizontal: CONTENT_HORIZONTAL_MARGIN,
    })
}

fn content_width(width: u16) -> u16 {
    width
        .saturating_sub(CONTENT_HORIZONTAL_MARGIN.saturating_mul(2))
        .max(1)
}

fn render_main(
    frame: &mut Frame<'_>,
    area: Rect,
    app: &mut App,
    state: &WorkbenchState,
    product_state: ProductState,
) {
    // Popup surfaces float over the main view; the underlying main layout
    // should ignore them and render as if no surface were open.
    let layout_surface = if app.surface.is_popup() {
        Surface::Main
    } else {
        app.surface
    };
    let body_width = content_width(area.width);
    let bottom_h = main_bottom_height_for(app, state, layout_surface, area, product_state);
    let modal_overlay_active = app.surface.is_popup() && !app.native_scrollback_is_active();
    let native_scrollback_active = app.native_scrollback_is_active() && !modal_overlay_active;
    let show_footer = layout_surface.is_bottom_pane()
        || app
            .quit_hint_until
            .is_some_and(|until| std::time::Instant::now() <= until)
        || app.escape_stop_is_pending();
    let footer_h = u16::from(show_footer && area.height > bottom_h);
    let max_body_h = area
        .height
        .saturating_sub(bottom_h)
        .saturating_sub(footer_h);
    let (body, reserve_live_status_row) = if native_scrollback_active {
        let (mut lines, reserve_live_status_row) =
            transcript::with_transcript_model(app, state, |model| {
                let stream_skip_lines = state
                    .current_session
                    .as_ref()
                    .map(|session| {
                        app.native_history
                            .live_stream_emitted_lines_for(&session.id, body_width)
                    })
                    .unwrap_or(0);
                let lines = transcript::active_viewport_lines_with_stream_skip(
                    Some(model),
                    body_width,
                    max_body_h,
                    stream_skip_lines,
                );
                (
                    lines,
                    transcript::active_viewport_needs_status_row_reserve(Some(model)),
                )
            })
            .unwrap_or_default();
        if let Some(notice) = app
            .status_notice
            .as_ref()
            .filter(|_| status_notice_needs_tail_visibility(app, product_state))
        {
            if !lines.is_empty() {
                lines.push(Line::from(""));
            }
            lines.push(Line::from(Span::styled(notice.clone(), muted())));
        }
        if lines.is_empty() {
            if let Some(next) = next_action_lines(state, app, product_state) {
                lines = next;
            }
        }
        (lines, reserve_live_status_row)
    } else {
        let lines = match product_state {
            ProductState::SetupNeeded => setup_lines(app, body_width as usize),
            ProductState::Ready => ready_lines(app, state, body_width, max_body_h),
            ProductState::Running
            | ProductState::Result
            | ProductState::Failed
            | ProductState::Cancelled => work_lines(state, app, body_width, product_state),
        };
        (lines, false)
    };
    let pin_bottom = should_pin_main_bottom(product_state, native_scrollback_active)
        && !layout_surface.is_bottom_pane();
    let attach_bottom_to_body =
        native_scrollback_active && !body.is_empty() && !layout_surface.is_bottom_pane();
    let reserve_live_status_row = attach_bottom_to_body && reserve_live_status_row;
    let layout_body_len = body
        .len()
        .saturating_add(usize::from(reserve_live_status_row));
    let (body_area, bottom_area, footer_area) = main_layout_areas(
        area,
        bottom_h,
        layout_body_len,
        show_footer,
        pin_bottom,
        attach_bottom_to_body,
    );
    let mut body = body;
    if body.len() > body_area.height as usize {
        body = visible_main_body_lines(body, body_area.height, product_state);
    }
    let body_render_area = if pin_bottom
        && !body.is_empty()
        && body.len() < body_area.height as usize
    {
        let empty_rows = body_area.height.saturating_sub(body.len() as u16);
        let top_gap = match product_state {
            ProductState::Result => empty_rows.saturating_sub(4).min(8),
            ProductState::Running | ProductState::Failed | ProductState::Cancelled => empty_rows,
            ProductState::Ready | ProductState::SetupNeeded => 0,
        };
        let top_gap = if native_scrollback_active
            && matches!(
                product_state,
                ProductState::Failed | ProductState::Cancelled
            ) {
            0
        } else {
            top_gap
        };
        Rect {
            y: body_area.y.saturating_add(top_gap),
            height: body_area.height.saturating_sub(top_gap),
            ..body_area
        }
    } else {
        body_area
    };
    trim_trailing_whitespace(&mut body);
    let body_content_rect = content_area(body_render_area);
    let logo_rect = if app.is_welcome_surface() {
        match product_state {
            ProductState::Ready => Some(crate::welcome::logo_screen_rect(
                body_content_rect,
                app.status_notice.is_some(),
                cloud_home_banner_lines(app, body_width).map_or(0, |lines| lines.len()),
            )),
            ProductState::SetupNeeded => Some(setup_logo_screen_rect(body_content_rect)),
            ProductState::Running
            | ProductState::Result
            | ProductState::Failed
            | ProductState::Cancelled => None,
        }
    } else {
        None
    };
    app.welcome_logo_rect.set(logo_rect);
    frame.render_widget(
        Paragraph::new(body)
            .style(Style::default().fg(text()))
            .wrap(Wrap { trim: false }),
        body_content_rect,
    );
    if layout_surface.is_bottom_pane() {
        app.composer_input_rect.set(None);
        render_bottom_pane(frame, bottom_area, app, state, layout_surface);
    } else if app.surface.is_text_input_popup() {
        // The popup itself is the input — don't render the composer under it,
        // or the user sees their typing duplicated. Clear the area so nothing
        // bleeds through behind the floating popup.
        app.composer_input_rect.set(None);
        frame.render_widget(Clear, bottom_area);
    } else {
        render_composer(frame, bottom_area, app, state, product_state);
    }
    if show_footer {
        render_footer(frame, footer_area, app, state, product_state);
    }
}

fn main_layout_areas(
    area: Rect,
    bottom_h: u16,
    body_len: usize,
    show_footer: bool,
    pin_bottom: bool,
    attach_bottom_to_body: bool,
) -> (Rect, Rect, Rect) {
    let footer_h = u16::from(show_footer && area.height > bottom_h);
    let max_body_h = area
        .height
        .saturating_sub(bottom_h)
        .saturating_sub(footer_h);
    let body_h = (body_len as u16).min(max_body_h);
    // The composer is always pinned to the bottom of the terminal; the
    // optional footer is the very last row. The body either sits at the
    // top with a flex spacer pushing the composer down (welcome / setup),
    // or sits at the bottom just above the composer with the spacer
    // above it so it grows downward toward the composer as content
    // arrives (active sessions).
    let chunks = if pin_bottom {
        Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Fill(1),
                Constraint::Length(body_h),
                Constraint::Length(bottom_h),
                Constraint::Length(footer_h),
            ])
            .split(area)
    } else if attach_bottom_to_body {
        Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(body_h),
                Constraint::Length(bottom_h),
                Constraint::Fill(1),
                Constraint::Length(footer_h),
            ])
            .split(area)
    } else {
        Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(body_h),
                Constraint::Fill(1),
                Constraint::Length(bottom_h),
                Constraint::Length(footer_h),
            ])
            .split(area)
    };
    if attach_bottom_to_body && !pin_bottom {
        (chunks[0], chunks[1], chunks[3])
    } else {
        let body_idx = if pin_bottom { 1 } else { 0 };
        (chunks[body_idx], chunks[2], chunks[3])
    }
}

fn should_pin_main_bottom(product_state: ProductState, native_scrollback_active: bool) -> bool {
    if native_scrollback_active {
        return false;
    }
    matches!(
        product_state,
        ProductState::Running
            | ProductState::Result
            | ProductState::Failed
            | ProductState::Cancelled
    )
}

pub(crate) fn main_viewport_height(app: &App, width: u16) -> u16 {
    // The palette is now a floating popup over the main view — it doesn't
    // grow the composer pane. So the composer's own height is the only
    // contributor to the bottom-pane reserve.
    composer_pane_height(app, ProductState::Ready, width)
}

fn main_bottom_height_for(
    app: &App,
    state: &WorkbenchState,
    surface: Surface,
    area: Rect,
    product_state: ProductState,
) -> u16 {
    if !surface.is_bottom_pane() {
        return composer_pane_height(app, product_state, area.width);
    }
    let line_count = surface_lines(
        surface,
        app,
        state,
        content_width(area.width) as usize,
        usize::MAX,
    )
    .len() as u16;
    let max_height = match surface {
        Surface::Model | Surface::History | Surface::Messages => {
            area.height.saturating_sub(2).max(6)
        }
        Surface::BrowserSelect | Surface::DefaultProfile | Surface::CookieSync => 22,
        Surface::Secrets | Surface::Domains => 24,
        _ => 18,
    };
    // Add room for the surface header, footer, borders, and content margins.
    let desired = line_count.saturating_add(10).clamp(8, max_height);
    let available = area.height.saturating_sub(2).max(4);
    desired.min(available)
}

fn composer_pane_height(app: &App, _product_state: ProductState, width: u16) -> u16 {
    let visual_input_lines = composer_visual_input_lines(app, width.saturating_sub(4).max(1));
    let preview_lines = pending_followup_preview_lines(
        app,
        app.selected_session_id.as_deref(),
        composer_preview_width(width),
    )
    .len()
    .min(PENDING_FOLLOWUP_PREVIEW_MAX_LINES) as u16;
    // top border + input rows + bottom border + status row beneath.
    preview_lines + visual_input_lines + 3
}

/// Visual rows the input area inside the fused composer should occupy.
/// Floored at 3 so the box has comfortable breathing room when empty, and
/// capped at 10 so a long pasted prompt doesn't push the rest of the UI
/// off-screen.
const COMPOSER_INPUT_MIN_ROWS: u16 = 3;
const COMPOSER_INPUT_MAX_ROWS: u16 = 10;
const PENDING_FOLLOWUP_PREVIEW_MAX_LINES: usize = 8;

fn composer_visual_input_lines(app: &App, input_area_width: u16) -> u16 {
    let visual_input_lines = app
        .composer
        .visual_line_count_wrapped(input_area_width as usize) as u16;
    visual_input_lines.clamp(COMPOSER_INPUT_MIN_ROWS, COMPOSER_INPUT_MAX_ROWS)
}

fn composer_preview_width(width: u16) -> u16 {
    width.saturating_sub(4).max(1)
}

fn pending_followup_preview_lines(
    app: &App,
    session_id: Option<&str>,
    width: u16,
) -> Vec<Line<'static>> {
    let Some(session_id) = session_id else {
        return Vec::new();
    };
    let events = app.cached_events_for_session(session_id);
    let pending_active = pending_active_followup_events_from_events(events)
        .into_iter()
        .filter_map(event_payload_text)
        .collect::<Vec<_>>();
    let pending_queued = pending_queued_followup_events_from_events(events)
        .into_iter()
        .filter_map(event_payload_text)
        .collect::<Vec<_>>();
    if pending_active.is_empty() && pending_queued.is_empty() {
        return Vec::new();
    }

    let mut lines = Vec::new();
    if !pending_active.is_empty() {
        push_wrapped_preview_line(
            &mut lines,
            "• ",
            "  ",
            "Messages to be submitted after next tool call (press esc to dequeue and edit)",
            width,
            muted(),
        );
        for message in pending_active {
            push_limited_wrapped_preview_line(&mut lines, "  ↳ ", "    ", &message, width, muted());
        }
    }

    if !pending_queued.is_empty() {
        if !lines.is_empty() {
            lines.push(Line::from(""));
        }
        for message in pending_queued {
            let label = format!("queued follow-up  {message}");
            push_limited_wrapped_preview_line(&mut lines, "• ", "  ", &label, width, muted());
        }
    }

    lines
}

fn push_limited_wrapped_preview_line(
    lines: &mut Vec<Line<'static>>,
    prefix: &str,
    continuation_prefix: &str,
    text: &str,
    width: u16,
    style: Style,
) {
    const MAX_LINES_PER_MESSAGE: usize = 3;
    let start = lines.len();
    push_wrapped_preview_line(lines, prefix, continuation_prefix, text, width, style);
    let added = lines.len().saturating_sub(start);
    if added > MAX_LINES_PER_MESSAGE {
        lines.truncate(start + MAX_LINES_PER_MESSAGE);
        if let Some(last) = lines.last_mut() {
            *last = Line::from(vec![
                Span::styled(continuation_prefix.to_string(), dim()),
                Span::styled("...".to_string(), style),
            ]);
        }
    }
}

fn push_wrapped_preview_line(
    lines: &mut Vec<Line<'static>>,
    prefix: &str,
    continuation_prefix: &str,
    text: &str,
    width: u16,
    style: Style,
) {
    let mut first_visual = true;
    let source_lines = if text.is_empty() {
        vec![""]
    } else {
        text.lines().collect::<Vec<_>>()
    };
    for source_line in source_lines {
        let chars = source_line.chars().collect::<Vec<_>>();
        if chars.is_empty() {
            let active_prefix = if first_visual {
                prefix
            } else {
                continuation_prefix
            };
            lines.push(Line::from(vec![Span::styled(
                active_prefix.to_string(),
                dim(),
            )]));
            first_visual = false;
            continue;
        }

        let mut offset = 0;
        while offset < chars.len() {
            let active_prefix = if first_visual {
                prefix
            } else {
                continuation_prefix
            };
            let budget = (width as usize)
                .saturating_sub(active_prefix.chars().count())
                .max(1);
            let end = offset.saturating_add(budget).min(chars.len());
            let chunk = chars[offset..end].iter().collect::<String>();
            lines.push(Line::from(vec![
                Span::styled(active_prefix.to_string(), dim()),
                Span::styled(chunk, style),
            ]));
            first_visual = false;
            offset = end;
        }
    }
}

fn render_bottom_pane(
    frame: &mut Frame<'_>,
    area: Rect,
    app: &App,
    state: &WorkbenchState,
    surface: Surface,
) {
    if area.width == 0 || area.height == 0 {
        app.composer_input_rect.set(None);
        return;
    }
    let header = surface_header_lines(app, surface, content_width(area.width));
    let header_h = header.len() as u16;
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(header_h), Constraint::Min(1)])
        .split(area);
    frame.render_widget(Paragraph::new(header), content_area(chunks[0]));
    let body_area = content_area(chunks[1]);
    let body_width = body_area.width as usize;
    let mut lines = surface_lines(surface, app, state, body_width, body_area.height as usize);
    // For surfaces whose body is a straight list of selectable rows indexed by
    // `selected_row`, keep the selection in view by
    // dropping rows from the top once it would otherwise scroll off the bottom.
    if matches!(surface, Surface::History | Surface::Messages) {
        let body_h = body_area.height as usize;
        let header_reserved = if surface == Surface::History && !app.history_filter().is_empty() {
            2.min(lines.len())
        } else {
            0
        };
        let data_h = body_h.saturating_sub(header_reserved);
        if data_h > 0 && app.selected_row >= data_h {
            let skip = app.selected_row + 1 - data_h;
            let head: Vec<Line<'static>> = lines.iter().take(header_reserved).cloned().collect();
            let tail: Vec<Line<'static>> = lines.into_iter().skip(header_reserved + skip).collect();
            lines = head;
            lines.extend(tail);
        }
    }
    trim_trailing_whitespace(&mut lines);
    frame.render_widget(
        Paragraph::new(lines)
            .style(Style::default().fg(text()))
            .wrap(Wrap { trim: false }),
        body_area,
    );
}

pub(crate) struct ModalOverlay {
    pub(crate) rect: Rect,
    pub(crate) buffer: Buffer,
    pub(crate) cursor: Option<Position>,
}

pub(crate) fn active_modal_overlay(
    app: &App,
    state: &WorkbenchState,
    area: Rect,
) -> Option<ModalOverlay> {
    if app.is_slash_palette_active() {
        return command_palette_overlay(app, area);
    }
    if app.prompt_history.search.is_some() {
        return prompt_history_search_overlay(app, area);
    }
    if app.surface.is_popup() {
        return surface_popup_overlay(app, state, area, app.surface);
    }
    None
}

fn render_active_modal_overlay(
    frame: &mut Frame<'_>,
    area: Rect,
    app: &App,
    state: &WorkbenchState,
) {
    let Some(overlay) = active_modal_overlay(app, state, area) else {
        return;
    };
    overlay.buffer.merge_into(frame.buffer_mut(), overlay.rect);
    if let Some(cursor) = overlay.cursor {
        frame.set_cursor_position(cursor);
    }
}

trait BufferOverlayExt {
    fn merge_into(&self, target: &mut Buffer, target_rect: Rect);
}

impl BufferOverlayExt for Buffer {
    fn merge_into(&self, target: &mut Buffer, target_rect: Rect) {
        for y in 0..self.area.height {
            for x in 0..self.area.width {
                if let Some(target_cell) = target.cell_mut((
                    target_rect.x.saturating_add(x),
                    target_rect.y.saturating_add(y),
                )) {
                    *target_cell = self[(x, y)].clone();
                }
            }
        }
    }
}

fn surface_popup_overlay(
    app: &App,
    state: &WorkbenchState,
    area: Rect,
    surface: Surface,
) -> Option<ModalOverlay> {
    let rect = surface_popup_rect(app, state, area, surface)?;
    let local_rect = Rect::new(0, 0, rect.width, rect.height);
    let mut buffer = Buffer::empty(local_rect);
    let local_cursor = render_surface_popup_box(&mut buffer, local_rect, app, state, surface);
    let cursor = local_cursor.map(|position| Position {
        x: rect.x.saturating_add(position.x),
        y: rect.y.saturating_add(position.y),
    });
    Some(ModalOverlay {
        rect,
        buffer,
        cursor,
    })
}

/// Centered floating popup overlay for slash-command-launched surfaces
/// (history, browser, model, auth, telemetry, developer). Responsive: shrinks
/// to fit small terminals and caps to a comfortable max on large ones.
fn surface_popup_rect(
    app: &App,
    state: &WorkbenchState,
    area: Rect,
    surface: Surface,
) -> Option<Rect> {
    if area.width == 0 || area.height == 0 {
        return None;
    }

    const MIN_W: u16 = 40;
    const MIN_H: u16 = 10;
    const MAX_W: u16 = 84;
    const H_MARGIN: u16 = 4;
    let max_h = if surface == Surface::Model {
        area.height
    } else {
        26
    };
    let v_margin: u16 = if surface == Surface::Model { 0 } else { 2 };

    let popup_w = if area.width <= MIN_W {
        area.width
    } else {
        area.width
            .saturating_sub(H_MARGIN.saturating_mul(2))
            .min(MAX_W)
            .max(MIN_W)
    };

    // Estimate desired height from body content length + chrome
    // (border 2 + header 4 + footer 2 = 8 lines).
    let body_inner_width = popup_w
        .saturating_sub(2 + CONTENT_HORIZONTAL_MARGIN * 2)
        .max(1) as usize;
    let body_line_count =
        surface_lines(surface, app, state, body_inner_width, usize::MAX).len() as u16;
    let desired_h = body_line_count.saturating_add(8);

    let popup_h = if area.height <= MIN_H {
        area.height
    } else {
        desired_h
            .clamp(MIN_H, max_h)
            .min(area.height.saturating_sub(v_margin.saturating_mul(2)))
            .max(MIN_H.min(area.height))
    };

    let popup_x = area.x + area.width.saturating_sub(popup_w) / 2;
    let popup_y = area.y + area.height.saturating_sub(popup_h) / 2;
    Some(Rect {
        x: popup_x,
        y: popup_y,
        width: popup_w,
        height: popup_h,
    })
}

/// Screen rows a plain-text line occupies under `Wrap { trim: false }` at
/// `width` columns (greedy word wrap, ASCII-width). Used to map a line index to
/// its rendered row when positioning the input caret.
fn wrapped_rows(text: &str, width: u16) -> u16 {
    let width = width.max(1) as usize;
    if text.chars().count() <= width {
        return 1;
    }
    let mut rows: u16 = 1;
    let mut col = 0usize;
    for word in text.split(' ') {
        let wlen = word.chars().count();
        if col == 0 {
            col = wlen;
        } else if col + 1 + wlen <= width {
            col += 1 + wlen;
        } else {
            rows = rows.saturating_add(1);
            col = wlen;
        }
        // A single word wider than the line spills onto further rows.
        while col > width {
            rows = rows.saturating_add(1);
            col -= width;
        }
    }
    rows.max(1)
}

fn render_surface_popup_box(
    buffer: &mut Buffer,
    popup_rect: Rect,
    app: &App,
    state: &WorkbenchState,
    surface: Surface,
) -> Option<Position> {
    Clear.render(popup_rect, buffer);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(border());
    let inner = block.inner(popup_rect);
    block.render(popup_rect, buffer);

    if inner.width == 0 || inner.height == 0 {
        return None;
    }

    // Layout inside the popup: header lines, body, footer line.
    let header = surface_header_lines(app, surface, inner.width);
    let header_h = (header.len() as u16).min(inner.height);
    let footer_text = surface_footer_for_app(surface, app);
    let footer_h: u16 = if footer_text.is_empty() { 0 } else { 1 };
    let body_h = inner
        .height
        .saturating_sub(header_h)
        .saturating_sub(footer_h);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(header_h),
            Constraint::Length(body_h),
            Constraint::Length(footer_h),
        ])
        .split(inner);

    Paragraph::new(header).render(content_area(chunks[0]), buffer);

    let body_area = content_area(chunks[1]);
    let mut lines = surface_lines(
        surface,
        app,
        state,
        body_area.width as usize,
        body_area.height as usize,
    );
    if matches!(surface, Surface::History | Surface::Messages) {
        let body_h = body_area.height as usize;
        // History reserves the first two lines for the live filter input when
        // the user has typed something — those must always stay pinned to the
        // top of the popup. Scroll only the data rows underneath.
        let header_reserved = if surface == Surface::History && !app.history_filter().is_empty() {
            2.min(lines.len())
        } else {
            0
        };
        let data_h = body_h.saturating_sub(header_reserved);
        if data_h > 0 && app.selected_row >= data_h {
            let skip = app.selected_row + 1 - data_h;
            let head: Vec<Line<'static>> = lines.iter().take(header_reserved).cloned().collect();
            let tail: Vec<Line<'static>> = lines.into_iter().skip(header_reserved + skip).collect();
            lines = head;
            lines.extend(tail);
        }
    }
    // For text-input popups, position the terminal cursor at the end of the
    // masked secret line so the user sees a blinking caret in the input field.
    let cursor_pos: Option<Position> = if surface.is_text_input_popup()
        && (surface != Surface::ModelSearch || app.model_search_has_filter_input())
    {
        let masked = match surface {
            Surface::Telemetry => masked_secret(app.composer.input()),
            Surface::ApiKey => {
                let account = app.api_key_account.as_deref().unwrap_or("");
                masked_secret_for_account(account, app.composer.input())
            }
            // The model search field is not a secret — show the raw query.
            Surface::ModelSearch => app.composer.input().to_string(),
            Surface::Secrets => secrets_input_field(app),
            Surface::Domains => domains_input_field(app),
            Surface::Email => email_input_field(app),
            _ => String::new(),
        };
        let target = format!("  {masked}");
        let cursor_col = target.chars().count() as u16;
        // Walk lines accumulating their *wrapped* screen height — the body
        // Paragraph wraps with `Wrap { trim: false }`, so a long line above the
        // input occupies several rows. Counting by line index lands the caret too
        // high (e.g. on the heading above the field).
        let mut screen_row: u16 = 0;
        let mut found = None;
        for line in lines.iter() {
            if screen_row >= body_area.height {
                break;
            }
            let plain: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
            if plain.starts_with(&target) {
                found = Some(Position {
                    x: body_area.x.saturating_add(cursor_col.min(body_area.width)),
                    y: body_area.y.saturating_add(screen_row),
                });
                break;
            }
            screen_row = screen_row.saturating_add(wrapped_rows(&plain, body_area.width));
        }
        found
    } else {
        None
    };
    trim_trailing_whitespace(&mut lines);
    Paragraph::new(lines)
        .style(Style::default().fg(text()))
        .wrap(Wrap { trim: false })
        .render(body_area, buffer);

    if footer_h > 0 {
        Paragraph::new(footer_text)
            .style(muted())
            .alignment(Alignment::Right)
            .render(content_area(chunks[2]), buffer);
    }
    cursor_pos
}

pub(crate) fn command_palette_overlay(app: &App, area: Rect) -> Option<ModalOverlay> {
    let rect = command_palette_popup_rect(app, area)?;
    let local_rect = Rect::new(0, 0, rect.width, rect.height);
    let mut buffer = Buffer::empty(local_rect);
    let local_cursor = render_command_palette_box(&mut buffer, local_rect, app)?;
    let cursor = Position {
        x: rect.x.saturating_add(local_cursor.x),
        y: rect.y.saturating_add(local_cursor.y),
    };
    Some(ModalOverlay {
        rect,
        buffer,
        cursor: Some(cursor),
    })
}

fn command_palette_popup_rect(app: &App, area: Rect) -> Option<Rect> {
    if area.width == 0 || area.height == 0 {
        return None;
    }
    const MIN_W: u16 = 40;
    const MIN_H: u16 = 10;
    const MAX_W: u16 = 72;
    const H_MARGIN: u16 = 4;
    const V_MARGIN: u16 = 2;

    // The popup size is fixed so the box never resizes as the user filters.
    // Completed native scrollback keeps one row shorter so the underlying
    // transcript remains visibly anchored while the palette layers over it.
    // Chrome: border(2) + input row(1) + blank(1) + footer(1) = 5.
    let item_rows = if app.native_scrollback_is_active() {
        (palette::max_item_count() as u16).min(6)
    } else {
        palette::max_item_count() as u16
    };
    let desired_h = item_rows.saturating_add(5);

    let popup_w = if area.width <= MIN_W {
        area.width
    } else {
        area.width
            .saturating_sub(H_MARGIN.saturating_mul(2))
            .min(MAX_W)
            .max(MIN_W)
    };
    let available_h = area
        .height
        .saturating_sub(V_MARGIN.saturating_mul(2))
        .max(MIN_H.min(area.height));
    let popup_h = desired_h.min(available_h).max(MIN_H.min(available_h));
    let popup_x = area.x + area.width.saturating_sub(popup_w) / 2;
    let popup_y = area.y + area.height.saturating_sub(popup_h) / 2;
    Some(Rect {
        x: popup_x,
        y: popup_y,
        width: popup_w,
        height: popup_h,
    })
}

fn render_command_palette_box(
    buffer: &mut Buffer,
    popup_rect: Rect,
    app: &App,
) -> Option<Position> {
    Clear.render(popup_rect, buffer);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(border());
    let inner = block.inner(popup_rect);
    block.render(popup_rect, buffer);
    if inner.width == 0 || inner.height == 0 {
        return None;
    }

    // Layout inside the popup:
    //   input row       — `> filter` (with cursor)
    //   blank
    //   items body      — filtered command rows
    //   footer hint
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // input
            Constraint::Length(1), // blank
            Constraint::Min(1),    // items
            Constraint::Length(1), // footer hint
        ])
        .split(inner);

    // Input row. The popup owns its own filter — the composer underneath
    // is untouched while the palette is open.
    let typed = app.palette_filter().to_string();
    let input_area = chunks[0];
    let input_inner = Rect {
        x: input_area.x.saturating_add(2),
        y: input_area.y,
        width: input_area.width.saturating_sub(2),
        height: 1,
    };
    let input_line = Line::from(vec![
        Span::styled("> ", accent()),
        Span::styled(typed.clone(), text_style()),
    ]);
    Paragraph::new(input_line).render(input_area, buffer);
    let cursor_offset = typed.chars().count() as u16;
    let mut cursor = None;
    if input_inner.width > 0 {
        cursor = Some(Position {
            x: input_inner
                .x
                .saturating_add(cursor_offset.min(input_inner.width)),
            y: input_inner.y,
        });
    }

    let body_chunk = chunks[2];
    let footer_chunk = chunks[3];
    let items = app.slash_palette_items();

    if items.is_empty() {
        Paragraph::new(Line::from(Span::styled("  No commands match.", muted())))
            .render(body_chunk, buffer);
    } else {
        let rows = slash_palette_rows(app, body_chunk.width as usize);
        let mut visible = rows;
        let body_h = body_chunk.height as usize;
        if body_h > 0 && app.selected_row >= body_h {
            let skip = app.selected_row + 1 - body_h;
            visible = visible.into_iter().skip(skip).collect();
        }
        Paragraph::new(visible)
            .style(Style::default().fg(text()))
            .wrap(Wrap { trim: false })
            .render(body_chunk, buffer);
    }

    Paragraph::new(Line::from(Span::styled(
        " ↑↓ navigate · ⏎ select · esc close",
        muted(),
    )))
    .alignment(Alignment::Right)
    .render(footer_chunk, buffer);
    cursor
}

fn prompt_history_search_overlay(app: &App, area: Rect) -> Option<ModalOverlay> {
    let rect = prompt_history_search_popup_rect(area)?;
    let local_rect = Rect::new(0, 0, rect.width, rect.height);
    let mut buffer = Buffer::empty(local_rect);
    let local_cursor = render_prompt_history_search_box(&mut buffer, local_rect, app)?;
    let cursor = Position {
        x: rect.x.saturating_add(local_cursor.x),
        y: rect.y.saturating_add(local_cursor.y),
    };
    Some(ModalOverlay {
        rect,
        buffer,
        cursor: Some(cursor),
    })
}

fn prompt_history_search_popup_rect(area: Rect) -> Option<Rect> {
    if area.width == 0 || area.height == 0 {
        return None;
    }
    const MIN_W: u16 = 40;
    const MIN_H: u16 = 9;
    const MAX_W: u16 = 84;
    const H_MARGIN: u16 = 4;
    const V_MARGIN: u16 = 2;

    let popup_w = if area.width <= MIN_W {
        area.width
    } else {
        area.width
            .saturating_sub(H_MARGIN.saturating_mul(2))
            .min(MAX_W)
            .max(MIN_W)
    };
    let available_h = area
        .height
        .saturating_sub(V_MARGIN.saturating_mul(2))
        .max(MIN_H.min(area.height));
    let popup_h = 11.min(available_h).max(MIN_H.min(available_h));
    let popup_x = area.x + area.width.saturating_sub(popup_w) / 2;
    let popup_y = area.y + area.height.saturating_sub(popup_h) / 2;
    Some(Rect {
        x: popup_x,
        y: popup_y,
        width: popup_w,
        height: popup_h,
    })
}

fn render_prompt_history_search_box(
    buffer: &mut Buffer,
    popup_rect: Rect,
    app: &App,
) -> Option<Position> {
    let search = app.prompt_history.search.as_ref()?;
    Clear.render(popup_rect, buffer);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(border());
    let inner = block.inner(popup_rect);
    block.render(popup_rect, buffer);
    if inner.width == 0 || inner.height == 0 {
        return None;
    }

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .split(inner);

    let input_area = chunks[0];
    let query = search.query.clone();
    Paragraph::new(Line::from(vec![
        Span::styled("> ", accent()),
        Span::styled(query.clone(), text_style()),
    ]))
    .render(input_area, buffer);
    let cursor = Position {
        x: input_area
            .x
            .saturating_add(2)
            .saturating_add((query.chars().count() as u16).min(input_area.width.saturating_sub(2))),
        y: input_area.y,
    };

    Paragraph::new(Line::from("")).render(chunks[1], buffer);

    let body_width = chunks[2].width as usize;
    let body_h = chunks[2].height as usize;
    let mut rows = prompt_history_search_rows(search, body_width);
    if rows.len() > body_h {
        rows.truncate(body_h);
    }
    Paragraph::new(rows)
        .style(Style::default().fg(text()))
        .wrap(Wrap { trim: false })
        .render(chunks[2], buffer);

    Paragraph::new(Line::from(Span::styled(
        " ctrl+r/↑ older · ctrl+s/↓ newer · enter accept · esc cancel",
        muted(),
    )))
    .alignment(Alignment::Right)
    .render(chunks[3], buffer);
    Some(cursor)
}

fn prompt_history_search_rows(
    search: &super::PromptHistorySearchState,
    width: usize,
) -> Vec<Line<'static>> {
    if search.query.trim().is_empty() {
        return vec![Line::from(Span::styled("  ", muted()))];
    }
    if search.matches.is_empty() {
        return vec![Line::from(Span::styled("  No matching prompts.", muted()))];
    }
    search
        .matches
        .iter()
        .enumerate()
        .map(|(idx, text)| {
            let is_selected = search.selected == Some(idx);
            let marker = if is_selected { "› " } else { "  " };
            let preview = truncate(&first_line(text), width.saturating_sub(6).max(4));
            let style = if is_selected { text_style() } else { muted() };
            highlight_selectable_row(
                vec![Span::styled(marker, accent()), Span::styled(preview, style)],
                is_selected,
                width,
            )
        })
        .collect()
}

fn visible_tail_lines(mut lines: Vec<Line<'static>>, height: u16) -> Vec<Line<'static>> {
    let height = height as usize;
    if height == 0 {
        return Vec::new();
    }
    if lines.len() > height {
        lines = lines.split_off(lines.len() - height);
    }
    lines
}

fn visible_head_lines(mut lines: Vec<Line<'static>>, height: u16) -> Vec<Line<'static>> {
    let height = height as usize;
    if height == 0 {
        return Vec::new();
    }
    if lines.len() > height {
        lines.truncate(height);
    }
    lines
}

fn visible_main_body_lines(
    lines: Vec<Line<'static>>,
    height: u16,
    product_state: ProductState,
) -> Vec<Line<'static>> {
    match product_state {
        ProductState::Ready | ProductState::SetupNeeded => visible_head_lines(lines, height),
        ProductState::Running
        | ProductState::Result
        | ProductState::Failed
        | ProductState::Cancelled => visible_tail_lines(lines, height),
    }
}

fn render_surface(
    frame: &mut Frame<'_>,
    area: Rect,
    app: &App,
    state: &WorkbenchState,
    surface: Surface,
) {
    frame.render_widget(Clear, frame.area());
    let header = surface_header_lines(app, surface, area.width);
    let chrome_h = header.len() as u16;
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(chrome_h),
            Constraint::Min(6),
            Constraint::Length(1),
        ])
        .split(area);
    frame.render_widget(Paragraph::new(header), chunks[0]);
    let body_area = content_area(chunks[1]);
    if surface == Surface::Setup {
        app.welcome_logo_rect
            .set(Some(setup_logo_screen_rect(body_area)));
    } else {
        app.welcome_logo_rect.set(None);
    }
    let body_width = body_area.width as usize;
    let mut lines = surface_lines(surface, app, state, body_width, body_area.height as usize);
    trim_trailing_whitespace(&mut lines);
    frame.render_widget(
        Paragraph::new(lines)
            .style(Style::default().fg(text()))
            .wrap(Wrap { trim: false }),
        body_area,
    );
    frame.render_widget(
        Paragraph::new(surface_footer_for_app(surface, app))
            .style(muted())
            .alignment(Alignment::Right),
        chunks[2],
    );
}

/// Title and one-line description for a dropdown/settings surface header.
fn surface_heading(app: &App, surface: Surface) -> (String, &'static str) {
    match surface {
        Surface::Setup => ("Setup".to_string(), "Choose how to run Browser Use"),
        Surface::SetupConfirm => ("Setup".to_string(), "Confirm provider"),
        Surface::SetupResult => ("Setup".to_string(), "Connection result"),
        Surface::SetupCloud => ("Setup".to_string(), "Choose your browser backend"),
        Surface::SetupCloudSuccess => ("Setup".to_string(), "Browser Use Cloud connected"),
        Surface::Account => ("Authenticate".to_string(), "Sign in to a model provider"),
        Surface::ApiKey => ("API key".to_string(), "Enter your provider API key"),
        Surface::Telemetry => ("Laminar".to_string(), "Configure Laminar telemetry"),
        Surface::Provider => (
            "Model".to_string(),
            "Pick a recommended model or choose a provider",
        ),
        Surface::OpenAiAuth => (
            app.provider_auth_label().to_string(),
            "Choose how to connect to this provider",
        ),
        Surface::Model => (
            "Model".to_string(),
            "Choose the model and provider for this session",
        ),
        Surface::ModelSearch => ("Model".to_string(), "Search this provider's models"),
        Surface::Mode => (
            "Mode".to_string(),
            "Choose the collaboration mode for the next turn",
        ),
        Surface::Browser => ("Browser".to_string(), "Change the browser backend"),
        Surface::BrowserSelect => ("Browser".to_string(), "Choose a local browser or backend"),
        Surface::DefaultProfile => (
            "Profile".to_string(),
            "Choose the default local Chrome profile",
        ),
        Surface::CookieSync => (
            "Cookie Sync".to_string(),
            "Import local browser cookies to Browser Use Cloud",
        ),
        Surface::Context => (
            "Context".to_string(),
            "Inspect current context window usage",
        ),
        Surface::Goal => ("Goal".to_string(), "Inspect or change the active task goal"),
        Surface::History => ("History".to_string(), "Browse and resume previous tasks"),
        Surface::Messages => (
            "Messages".to_string(),
            "Edit submitted prompts or cancel queued follow-ups",
        ),
        Surface::Developer => ("Developer".to_string(), "Developer tools and diagnostics"),
        Surface::Secrets => (
            "Secrets".to_string(),
            "Save passwords & 2FA codes the agent uses to log in",
        ),
        Surface::Domains => (
            "Domains".to_string(),
            "Allow or block which sites the agent may visit",
        ),
        Surface::Email => (
            "Email inbox".to_string(),
            "A disposable inbox the agent uses for sign-ups, links & codes",
        ),
        Surface::Feedback => ("Feedback".to_string(), "Report a bug or share feedback"),
        Surface::FeedbackThanks => ("Feedback".to_string(), ""),
        Surface::Main => ("".to_string(), ""),
    }
}

/// A surface header: a full-width accent rule, the colored title, and a muted
/// one-line description — the shared chrome for every dropdown/settings view.
fn surface_header_lines(app: &App, surface: Surface, width: u16) -> Vec<Line<'static>> {
    let (title, description) = surface_heading(app, surface);
    let indent = if matches!(surface, Surface::CookieSync | Surface::DefaultProfile) {
        String::new()
    } else {
        " ".repeat(CONTENT_HORIZONTAL_MARGIN as usize)
    };
    vec![
        Line::from(Span::styled("─".repeat(width as usize), accent())),
        Line::from(vec![
            Span::raw(indent.clone()),
            Span::styled(title, accent()),
        ]),
        Line::from(vec![
            Span::raw(indent),
            Span::styled(description.to_string(), muted()),
        ]),
        Line::from(""),
    ]
}

fn surface_footer(surface: Surface) -> &'static str {
    match surface {
        Surface::ApiKey => "Enter:save | Esc:cancel",
        Surface::Telemetry => "Enter:save | Esc:cancel",
        Surface::Email => "Enter:save | Esc:cancel",
        Surface::History => "Type to filter | Enter:open | Esc:close",
        Surface::Messages => "Enter:edit | Esc:close",
        Surface::Setup | Surface::SetupConfirm | Surface::SetupCloud => "Enter:continue | Esc:back",
        Surface::SetupCloudSuccess => "Enter:select | Esc:back",
        Surface::SetupResult => "Enter:select | Esc:back",
        Surface::Browser => "Enter:select | Esc:back",
        Surface::CookieSync => "Enter:select | Esc:close",
        Surface::Context => "Esc:close",
        Surface::Goal => "Esc:close",
        Surface::Developer => "Esc:close",
        Surface::Secrets => "Enter:next | Esc:close",
        Surface::Domains => "Enter:apply | Esc:close",
        Surface::Feedback => "Enter:next | Esc:back",
        Surface::FeedbackThanks => "",
        _ => "Enter:select | Esc:back",
    }
}

fn messages_footer(app: &App) -> &'static str {
    if app.selected_message_action_is_queued() {
        "Enter:edit | Del:cancel queued | Esc:close"
    } else {
        "Enter:edit | Esc:close"
    }
}

fn surface_footer_for_app(surface: Surface, app: &App) -> &'static str {
    match surface {
        Surface::Messages => messages_footer(app),
        Surface::Feedback => feedback_footer(app),
        _ => surface_footer(surface),
    }
}

fn feedback_footer(app: &App) -> &'static str {
    // The final step submits on Enter; earlier steps advance. On the home
    // screen (no selected session) the description step is the last one.
    let submits = matches!(app.feedback.step, FeedbackStep::UploadLogs)
        || (matches!(app.feedback.step, FeedbackStep::Description)
            && app.selected_session_id.is_none());
    if submits {
        "Enter:submit | Esc:back"
    } else {
        "Enter:next | Esc:back"
    }
}

fn surface_lines(
    surface: Surface,
    app: &App,
    state: &WorkbenchState,
    width: usize,
    height: usize,
) -> Vec<Line<'static>> {
    match surface {
        Surface::Setup => setup_lines(app, width),
        Surface::SetupConfirm => setup_confirm_lines(app),
        Surface::SetupResult => setup_result_lines(app, width),
        Surface::SetupCloud => setup_cloud_lines(app),
        Surface::SetupCloudSuccess => setup_cloud_connected_lines(app),
        Surface::Account => account_lines(app),
        Surface::ApiKey => api_key_lines(app),
        Surface::Telemetry => telemetry_key_lines(app),
        Surface::Provider => provider_lines(app),
        Surface::OpenAiAuth => openai_auth_lines(app),
        Surface::Model => model_lines(app, height),
        Surface::ModelSearch => model_search_lines(app, height),
        Surface::Mode => mode_lines(app),
        Surface::Browser => browser_panel_lines(app, state),
        Surface::BrowserSelect => browser_select_lines(app, width),
        Surface::DefaultProfile => default_profile_lines(app, width),
        Surface::CookieSync => cookie_sync_lines(app, width),
        Surface::Context => context_lines(app, state, width),
        Surface::Goal => goal_lines(app),
        Surface::History => history_lines(app, state, width),
        Surface::Messages => message_lines(app, width),
        Surface::Developer => developer_lines(app, state),
        Surface::Secrets => secrets_lines(app),
        Surface::Domains => domains_lines(app),
        Surface::Email => email_lines(app),
        Surface::Feedback => feedback_lines(app),
        Surface::FeedbackThanks => Vec::new(),
        Surface::Main => Vec::new(),
    }
}

/// Fused bordered composer: a single rounded box that contains the input area
/// and — when the slash palette is open — the dropdown rows sitting above the
/// input, separated by a thin dashed rule. Session metadata is punched
/// through the box's borders: model + browser on the top edge (or moves to
/// the bottom when the dropdown takes over the top), cwd on the bottom-left,
/// browser on the bottom-right. A single hint/status row renders just below
/// the box.
/// Bordered composer with the current browser punched into the
/// bottom border, and a single muted status row beneath showing the
/// active model and the context-fill bar. No cwd, no key hints — the
/// only ambient metadata is what the user explicitly asked to see.
fn render_composer(
    frame: &mut Frame<'_>,
    area: Rect,
    app: &App,
    state: &WorkbenchState,
    _product_state: ProductState,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let session_id = state
        .current_session
        .as_ref()
        .map(|session| session.id.as_str())
        .or(app.selected_session_id.as_deref());
    let mut preview_lines =
        pending_followup_preview_lines(app, session_id, composer_preview_width(area.width));
    if preview_lines.len() > PENDING_FOLLOWUP_PREVIEW_MAX_LINES {
        preview_lines.truncate(PENDING_FOLLOWUP_PREVIEW_MAX_LINES);
    }
    let min_composer_h = COMPOSER_INPUT_MIN_ROWS.saturating_add(2).min(area.height);
    let preview_h = (preview_lines.len() as u16)
        .min(area.height.saturating_sub(min_composer_h))
        .min(PENDING_FOLLOWUP_PREVIEW_MAX_LINES as u16);
    if preview_h > 0 {
        let preview_area = Rect {
            x: area.x.saturating_add(2),
            y: area.y,
            width: composer_preview_width(area.width),
            height: preview_h,
        };
        frame.render_widget(
            Paragraph::new(preview_lines)
                .style(Style::default().fg(text()))
                .wrap(Wrap { trim: false }),
            preview_area,
        );
    }

    let area = Rect {
        y: area.y.saturating_add(preview_h),
        height: area.height.saturating_sub(preview_h),
        ..area
    };
    if area.width == 0 || area.height == 0 {
        app.composer_input_rect.set(None);
        return;
    }
    let input_inner_w = area.width.saturating_sub(4).max(1);
    let input_h = composer_visual_input_lines(app, input_inner_w);
    let box_h = input_h.saturating_add(2).min(area.height);
    let status_h: u16 = if area.height > box_h { 1 } else { 0 };

    let box_area = Rect {
        x: area.x,
        y: area.y,
        width: area.width,
        height: box_h,
    };

    // Top + sides via Block, bottom border drawn manually so the browser
    // tag punches through it in white while the dashes/corners keep the
    // same gray border() color as the rest of the box.
    let block = Block::default()
        .borders(Borders::TOP | Borders::LEFT | Borders::RIGHT)
        .border_type(BorderType::Rounded)
        .border_style(border());
    let inner = block.inner(box_area);
    frame.render_widget(block, box_area);

    // IMPORTANT: render the input FIRST. Ratatui's Paragraph::render fills the
    // entire area with its base style (`Style::default().fg(text())` for the
    // composer input), which would otherwise paint over the bottom-border row
    // and bleach our dim border to bright white.
    if inner.width > 2 && inner.height > 0 {
        let input_area = Rect {
            x: inner.x.saturating_add(1),
            y: inner.y,
            width: inner.width.saturating_sub(2),
            height: inner.height.saturating_sub(1),
        };
        app.composer_input_rect.set(Some(input_area));
        render_composer_input(frame, input_area, app, state);
    } else {
        app.composer_input_rect.set(None);
    }

    let bottom_area = Rect {
        x: box_area.x,
        y: box_area.y + box_area.height.saturating_sub(1),
        width: box_area.width,
        height: 1,
    };
    let (bottom_border, bottom_live_link) = composer_bottom_border(box_area.width, app, state);
    frame.render_widget(Paragraph::new(bottom_border).style(border()), bottom_area);
    if let Some(link) = bottom_live_link {
        let col = bottom_area.x.saturating_add(link.col as u16);
        let max_visible = bottom_area.right().saturating_sub(col) as usize;
        let width = link.width.min(max_visible);
        if width > 0 {
            let text: String = link.text.chars().take(width).collect();
            *app.live_link_overlay.borrow_mut() = Some(LiveLinkOverlay {
                col,
                row: bottom_area.y,
                text,
                url: link.url,
                fg: accent().fg.unwrap_or(ratatui::style::Color::Reset),
                modifier: Modifier::UNDERLINED,
            });
        }
    }

    if status_h > 0 {
        let status_area = Rect {
            x: area.x,
            y: box_area.y + box_area.height,
            width: area.width,
            height: status_h,
        };
        let status_inner = status_area.inner(Margin {
            vertical: 0,
            horizontal: 2,
        });
        let status_line = composer_status_line(app, state, status_inner.width as usize);
        frame.render_widget(Paragraph::new(status_line), status_inner);
    }
}

/// Bottom border line for the composer, with the browser tag punched
/// through it on the right. Corners and dashes use the same gray
/// `border()` style as the rest of the box; the browser text is white.
fn composer_bottom_border(
    width: u16,
    app: &App,
    state: &WorkbenchState,
) -> (Line<'static>, Option<LiveLink>) {
    if width < 2 {
        return (Line::from(""), None);
    }
    let inner_w = width.saturating_sub(2) as usize;
    let mut spans: Vec<Span<'static>> = vec![Span::styled("╰", border())];
    let mut live_link = None;
    let browser = app.browser_status_label();
    let browser = browser.trim();
    let live_url = state
        .browser
        .live_url
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty());
    if !browser.is_empty() || live_url.is_some() {
        let live_label = LIVE_STATUS_LINK_TEXT;
        let live_segment_w = live_label.chars().count() + 3;
        let wants_live = live_url.is_some() && inner_w >= live_segment_w;
        let browser_budget = if wants_live {
            inner_w.saturating_sub(live_segment_w + 2)
        } else {
            inner_w.saturating_sub(4).max(1)
        };
        let browser_label = (!browser.is_empty())
            .then(|| truncate(browser, browser_budget.max(1)))
            .filter(|label| !label.is_empty());
        let browser_tag_w = browser_label
            .as_ref()
            .map(|label| label.chars().count() + 2)
            .unwrap_or(0);
        let tag_w = usize::from(wants_live) * live_segment_w + browser_tag_w;
        let trail = 2usize.min(inner_w.saturating_sub(tag_w));
        let lead = inner_w.saturating_sub(tag_w + trail);
        spans.push(Span::styled("─".repeat(lead), border()));
        let col = 1 + lead;
        if wants_live {
            spans.push(Span::raw(" "));
            spans.push(Span::styled(
                live_label,
                accent().add_modifier(Modifier::UNDERLINED),
            ));
            if let Some(url) = live_url {
                live_link = Some(LiveLink {
                    col: col + 1,
                    width: live_label.chars().count(),
                    text: live_label.to_string(),
                    url: url.to_string(),
                });
            }
            spans.push(Span::raw("  "));
        }
        if let Some(label) = browser_label {
            let tag_style = if app.browser == BROWSER_USE_CLOUD
                && !app.browser_use_cloud_key_ready().unwrap_or(false)
            {
                failed()
            } else {
                text_style()
            };
            spans.push(Span::raw(" "));
            spans.push(Span::styled(label, tag_style));
            spans.push(Span::raw(" "));
        }
        spans.push(Span::styled("─".repeat(trail), border()));
    } else {
        spans.push(Span::styled("─".repeat(inner_w), border()));
    }
    spans.push(Span::styled("╯", border()));
    (Line::from(spans), live_link)
}

/// Geometry of the clickable live-view link inside a rendered line. Columns are
/// relative to that line's origin;
/// the caller resolves them to absolute screen coordinates.
struct LiveLink {
    /// Column of the first live-link cell, relative to the status row origin.
    col: usize,
    /// Visible width (in cells) of the live-link text.
    width: usize,
    /// The visible live-link text actually drawn.
    text: String,
    /// The full, untruncated live-view URL to open on click.
    url: String,
}

/// On-screen placement of the live-view link, in absolute terminal
/// coordinates, recorded during render so the caller can paint an OSC-8
/// hyperlink over the already-drawn cells *after* the frame is flushed.
///
/// The link can't be bound inside the frame buffer: ratatui's draw diff
/// measures each cell's symbol display width, so escape bytes embedded in a
/// cell are counted as visible columns and corrupt the layout (skipped
/// cells, misaligned click region). Painting it post-draw, the way the
/// manual modal overlay does, sidesteps the diff entirely.
pub(crate) struct LiveLinkOverlay {
    /// Absolute column of the first live-link cell.
    pub(crate) col: u16,
    /// Absolute row of the status line.
    pub(crate) row: u16,
    /// The visible live-link text to reprint between the OSC-8 open/close.
    pub(crate) text: String,
    /// The full URL bound as the hyperlink target.
    pub(crate) url: String,
    /// Foreground color to reprint the text with.
    pub(crate) fg: ratatui::style::Color,
    /// Text modifier to reprint the text with.
    pub(crate) modifier: ratatui::style::Modifier,
}

/// Status row below the composer: active model and context-fill bar,
/// plus running cost when there is one. Browser and live-browser links live on
/// the box's bottom border, not here.
fn composer_status_line(app: &App, state: &WorkbenchState, width: usize) -> Line<'static> {
    let usage = session_usage(app, state);
    let mut spans = vec![Span::styled(app.model.clone(), accent())];
    spans.push(status_separator());
    spans.push(Span::styled(
        collaboration_mode_label(app.collaboration_mode).to_string(),
        muted(),
    ));

    let optional_budget = |spans: &[Span<'_>]| width.saturating_sub(spans_text_width(spans));

    if let Some(goal) = app.goal_status_indicator_for_state(state) {
        let available = optional_budget(&spans);
        push_optional_status_spans(
            &mut spans,
            vec![status_separator(), Span::styled(goal, muted())],
            available,
        );
    }
    if let (Some(context_tokens), Some(context_budget_tokens)) =
        (usage.context_tokens, usage.context_budget_tokens)
    {
        let mut context_spans = vec![status_separator()];
        context_spans.extend(context_bar_spans(context_tokens, context_budget_tokens));
        let available = optional_budget(&spans);
        push_optional_status_spans(&mut spans, context_spans, available);
    }
    if usage.cost_usd > 0.0 {
        let available = optional_budget(&spans);
        push_optional_status_spans(
            &mut spans,
            vec![
                status_separator(),
                Span::styled(format!("${:.4}", usage.cost_usd), muted()),
            ],
            available,
        );
    }
    Line::from(spans)
}

fn push_optional_status_spans(
    spans: &mut Vec<Span<'static>>,
    candidate: Vec<Span<'static>>,
    available: usize,
) {
    if spans_text_width(&candidate) <= available {
        spans.extend(candidate);
    }
}

const LIVE_STATUS_LINK_TEXT: &str = "live browser";

/// Dropdown rows used by the fused composer. No top/bottom rules and no
/// hint footer — those are provided by the box around it. Each row is
/// `marker · command · description` with the marker column reserved for the
/// `›` cursor on the active item.
fn slash_palette_rows(app: &App, width: usize) -> Vec<Line<'static>> {
    let items = app.slash_palette_items();
    let cmd_col = items
        .iter()
        .map(|item| item.command.chars().count())
        .max()
        .unwrap_or(0)
        .max(8);
    items
        .iter()
        .enumerate()
        .map(|(idx, item)| {
            let is_selected = idx == app.selected_row;
            let marker = if is_selected { "› " } else { "  " };
            let cmd_style = if is_selected { accent() } else { text_style() };
            let desc_style = if is_selected { text_style() } else { muted() };
            let desc_max = width.saturating_sub(cmd_col + 4).max(4);
            let description = truncate(slash_palette_item_description(app, item), desc_max);
            highlight_selectable_row(
                vec![
                    Span::styled(marker, accent()),
                    Span::styled(format!("{:<cmd_col$}", item.command), cmd_style),
                    Span::raw("  "),
                    Span::styled(description, desc_style),
                ],
                is_selected,
                width,
            )
        })
        .collect()
}

fn slash_palette_item_description(_app: &App, item: &palette::PaletteItem) -> &'static str {
    item.description
}

/// Fallback budget for context-surface attribution in older sessions that
/// predate Codex-style `token_count` events with model context-window metadata.
const FALLBACK_CONTEXT_BUDGET_TOKENS: i64 = 60_000;

/// Width, in cells, of the filled/empty context bar.
const CONTEXT_BAR_WIDTH: usize = 10;

/// A plain context bar — solid `█` fill over a `░` track — followed by the
/// `used/budget` token counts. Turns red as the conversation nears the
/// compaction budget.
fn context_bar_spans(used_tokens: i64, budget_tokens: i64) -> Vec<Span<'static>> {
    let used_tokens = used_tokens.max(0);
    let budget_tokens = budget_tokens.max(1);
    let ratio = (used_tokens as f64 / budget_tokens as f64).clamp(0.0, 1.0);
    let fill_style = if ratio >= 0.9 { failed() } else { accent() };

    let filled = ((ratio * CONTEXT_BAR_WIDTH as f64).round() as usize).min(CONTEXT_BAR_WIDTH);
    let pct_left = ((1.0 - ratio) * 100.0).round() as i64;
    vec![
        Span::styled("█".repeat(filled), fill_style),
        Span::styled("░".repeat(CONTEXT_BAR_WIDTH - filled), dim()),
        Span::raw("  "),
        Span::styled(
            format!(
                "{}/{}",
                format_token_count(used_tokens),
                format_token_count(budget_tokens)
            ),
            muted(),
        ),
        Span::raw("  "),
        Span::styled(format!("{pct_left}% context left"), muted()),
    ]
}

fn status_separator() -> Span<'static> {
    Span::styled("  ·  ", dim())
}

fn spans_text_width(spans: &[Span<'_>]) -> usize {
    spans.iter().map(|span| span.content.chars().count()).sum()
}

/// Per-session token and cost totals. Codex-style `token_count` events are the
/// source of truth for context occupancy; legacy `model.usage` remains only
/// the cost source.
struct SessionUsage {
    /// Prompt tokens of the most recent model turn — i.e. current context occupancy.
    context_tokens: Option<i64>,
    context_budget_tokens: Option<i64>,
    /// Accumulated estimated cost across the whole session, in USD.
    cost_usd: f64,
}

fn session_usage(app: &App, state: &WorkbenchState) -> SessionUsage {
    let mut usage = SessionUsage {
        context_tokens: None,
        context_budget_tokens: None,
        cost_usd: 0.0,
    };
    let Some(session) = state.current_session.as_ref() else {
        return usage;
    };
    for event in app.cached_events_for_session(&session.id) {
        match event.event_type.as_str() {
            "token_count" => {
                let Some(info) = event.payload.get("info").filter(|info| info.is_object()) else {
                    continue;
                };
                if let Some(input_tokens) = info
                    .get("last_token_usage")
                    .and_then(|usage| usage.get("input_tokens"))
                    .and_then(serde_json::Value::as_i64)
                {
                    usage.context_tokens = Some(input_tokens.max(0));
                }
                if let Some(model_context_window) = info
                    .get("model_context_window")
                    .and_then(serde_json::Value::as_i64)
                    .filter(|tokens| *tokens > 0)
                {
                    usage.context_budget_tokens = Some(model_context_window);
                }
            }
            "model.usage" => {
                if let Some(cost) = event
                    .payload
                    .get("cost_usd")
                    .and_then(serde_json::Value::as_f64)
                {
                    usage.cost_usd += cost;
                }
            }
            _ => {}
        }
    }
    usage
}

fn format_token_count(tokens: i64) -> String {
    let tokens = tokens.max(0);
    if tokens < 1_000 {
        return tokens.to_string();
    }
    let thousands = tokens as f64 / 1_000.0;
    if thousands.fract().abs() < 0.05 {
        format!("{}k", thousands.round() as i64)
    } else {
        format!("{thousands:.1}k")
    }
}

fn render_composer_input(frame: &mut Frame<'_>, area: Rect, app: &App, state: &WorkbenchState) {
    let current_session = state.current_session.as_ref();
    // Compute the home placeholder separately so it can own a String when
    // the typewriter produces a dynamic substring.
    let home_placeholder_owned: String;
    let placeholder: &str = if current_session.is_some_and(|session| session.status.is_active()) {
        "Type to steer the agent..."
    } else if current_session.is_some() {
        "Ask a follow-up..."
    } else {
        // Home screen — delegate to App which knows whether typewriter is active.
        home_placeholder_owned = app.home_placeholder();
        &home_placeholder_owned
    };
    let max_lines = area.height.max(1) as usize;
    // While the slash palette is open, the popup is the input — render the
    // composer as if it were empty (just the placeholder, no `/text`) and
    // skip cursor placement here so the popup owns it.
    let palette_owns_input = app.is_slash_palette_active();
    // During the Holding phase the full example is shown and the hint should
    // appear inline, clearly attached to the animated placeholder.
    let show_inline_hint =
        !palette_owns_input && app.is_home_examples_active() && app.is_typewriter_holding();
    let lines: Vec<Line<'static>> = if palette_owns_input {
        vec![Line::from(vec![
            Span::styled("> ", dim()),
            Span::styled(placeholder.to_string(), dim()),
        ])]
    } else if show_inline_hint {
        // Render manually so we can append the hint span without affecting
        // cursor_position_wrapped (composer is empty in this state).
        vec![Line::from(vec![
            Span::styled("> ", dim()),
            Span::styled(app.typewriter_placeholder_text().to_string(), dim()),
            Span::styled("  ⇥ tab to use", accent()),
        ])]
    } else {
        app.composer
            .render_lines_wrapped(max_lines, area.width as usize, placeholder)
    };
    frame.render_widget(
        Paragraph::new(lines)
            .style(Style::default().fg(text()))
            .wrap(Wrap { trim: false }),
        area,
    );
    if palette_owns_input {
        return;
    }
    if area.width > 0 && area.height > 0 {
        let (cursor_x, cursor_y) = app
            .composer
            .cursor_position_wrapped(max_lines, area.width as usize);
        frame.set_cursor_position(Position {
            x: area.x.saturating_add(cursor_x.min(area.width)),
            y: area
                .y
                .saturating_add(cursor_y.min(area.height.saturating_sub(1))),
        });
    }
}

fn render_footer(
    frame: &mut Frame<'_>,
    area: Rect,
    app: &App,
    state: &WorkbenchState,
    product_state: ProductState,
) {
    let label = if app
        .quit_hint_until
        .is_some_and(|until| std::time::Instant::now() <= until)
    {
        "ctrl+c again to quit"
    } else if app.escape_stop_is_pending() {
        "esc again to edit messages"
    } else if app.surface == Surface::Messages {
        messages_footer(app)
    } else if app.surface == Surface::Feedback {
        feedback_footer(app)
    } else if app.surface.is_bottom_pane() {
        surface_footer(app.surface)
    } else {
        let _ = (state, product_state);
        ""
    };
    frame.render_widget(
        Paragraph::new(label)
            .style(muted())
            .alignment(Alignment::Right),
        area,
    );
}

const SETUP_LOGO_W: usize = 18;
const SETUP_LOGO_H: usize = 7;
const SETUP_LOGO_GAP: usize = 8;
const SETUP_RIGHT_W: usize = 58;
const SETUP_CLICK_LABEL: &str = "click me!";
const SETUP_CLICK_PREFIX_W: usize = 11;
const SETUP_INTRO_MAX_W: usize = 74;
const SETUP_INTRO: &str = "Welcome to Browser Use Terminal, a Rust-based command line for running browser agents. Choose a provider below.";

fn setup_lines(app: &App, width: usize) -> Vec<Line<'static>> {
    let mut lines = Vec::new();

    let logo_rows = crate::welcome::render_braille_logo(
        SETUP_LOGO_W,
        SETUP_LOGO_H,
        11.0,
        1.1,
        app.welcome_anim.rx,
        app.welcome_anim.ry,
    );
    let right_lines = setup_account_lines(app);
    let side_by_side = setup_logo_is_side_by_side(width);

    if side_by_side {
        let total_w = setup_side_by_side_width().min(width);
        let left_pad = width.saturating_sub(total_w) / 2;
        let mut left_lines = logo_rows
            .into_iter()
            .map(|row| Line::from(Span::styled(row, text_style())))
            .collect::<Vec<_>>();
        left_lines.push(Line::from(""));
        left_lines.push(centered_line_in_width("Browser Use", SETUP_LOGO_W, bold()));
        left_lines.push(centered_line_in_width("Terminal", SETUP_LOGO_W, muted()));

        let row_count = left_lines.len().max(right_lines.len());
        for idx in 0..row_count {
            let show_click = idx == SETUP_LOGO_H / 2 && left_pad >= SETUP_CLICK_PREFIX_W;
            let mut spans = if show_click {
                vec![Span::raw(
                    " ".repeat(left_pad.saturating_sub(SETUP_CLICK_PREFIX_W)),
                )]
            } else {
                vec![Span::raw(" ".repeat(left_pad))]
            };
            if show_click {
                spans.extend([
                    Span::styled(SETUP_CLICK_LABEL.to_string(), accent()),
                    Span::raw("  "),
                ]);
            }
            if let Some(left) = left_lines.get(idx) {
                spans.extend(left.spans.clone());
            }
            let used_left = left_lines
                .get(idx)
                .map(line_width)
                .unwrap_or_default()
                .min(SETUP_LOGO_W);
            let gap_width = SETUP_LOGO_W
                .saturating_sub(used_left)
                .saturating_add(SETUP_LOGO_GAP);
            spans.push(Span::raw(" ".repeat(gap_width)));
            if let Some(right) = right_lines.get(idx) {
                spans.extend(right.spans.clone());
            }
            lines.push(Line::from(spans));
        }
    } else {
        if setup_stacked_logo_has_side_label(width) {
            let logo_pad = width.saturating_sub(SETUP_LOGO_W) / 2;
            for (idx, row) in logo_rows.into_iter().enumerate() {
                let show_click = idx == SETUP_LOGO_H / 2 && logo_pad >= SETUP_CLICK_PREFIX_W;
                let mut spans = if show_click {
                    vec![Span::raw(
                        " ".repeat(logo_pad.saturating_sub(SETUP_CLICK_PREFIX_W)),
                    )]
                } else {
                    vec![Span::raw(" ".repeat(logo_pad))]
                };
                if show_click {
                    spans.extend([
                        Span::styled(SETUP_CLICK_LABEL.to_string(), accent()),
                        Span::raw("  "),
                    ]);
                }
                spans.push(Span::styled(row, text_style()));
                lines.push(Line::from(spans));
            }
        } else {
            let logo_pad = " ".repeat(width.saturating_sub(SETUP_LOGO_W) / 2);
            for row in logo_rows {
                lines.push(Line::from(Span::styled(
                    format!("{logo_pad}{row}"),
                    text_style(),
                )));
            }
        }
        lines.push(Line::from(""));
        lines.push(centered_line("Browser Use", width, bold()));
        lines.push(centered_line("Terminal", width, muted()));
        lines.push(Line::from(""));
        lines.extend(setup_intro_lines(width));
        lines.push(Line::from(""));
        lines.extend(right_lines);
    }

    lines
}

fn setup_intro_lines(width: usize) -> Vec<Line<'static>> {
    let wrap_width = width.min(SETUP_INTRO_MAX_W).max(1);
    let mut rows = Vec::new();
    let mut current = String::new();

    for word in SETUP_INTRO.split_whitespace() {
        let next_len = if current.is_empty() {
            word.chars().count()
        } else {
            current.chars().count() + 1 + word.chars().count()
        };
        if !current.is_empty() && next_len > wrap_width {
            rows.push(centered_line(&current, width, muted()));
            current.clear();
        }
        if !current.is_empty() {
            current.push(' ');
        }
        current.push_str(word);
    }
    if !current.is_empty() {
        rows.push(centered_line(&current, width, muted()));
    }

    rows
}

fn setup_logo_is_side_by_side(_width: usize) -> bool {
    false
}

fn setup_side_by_side_width() -> usize {
    SETUP_LOGO_W + SETUP_LOGO_GAP + SETUP_RIGHT_W
}

fn setup_stacked_logo_has_side_label(width: usize) -> bool {
    width >= SETUP_LOGO_W
}

fn setup_logo_screen_rect(body_rect: Rect) -> Rect {
    let width = body_rect.width as usize;
    let x_offset = if setup_logo_is_side_by_side(width) {
        let total_w = setup_side_by_side_width().min(width);
        width.saturating_sub(total_w) / 2
    } else if setup_stacked_logo_has_side_label(width) {
        width.saturating_sub(SETUP_LOGO_W) / 2
    } else {
        width.saturating_sub(SETUP_LOGO_W) / 2
    };
    Rect {
        x: body_rect.x.saturating_add(x_offset as u16),
        y: body_rect.y,
        width: SETUP_LOGO_W as u16,
        height: (SETUP_LOGO_H as u16).min(body_rect.height),
    }
}

fn setup_account_lines(app: &App) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    lines.push(Line::from(Span::styled("PROVIDERS", muted())));
    lines.push(Line::from(""));

    for (idx, account) in app
        .setup_account_choices()
        .unwrap_or_else(|_| {
            vec![
                ACCOUNT_OPENAI,
                ACCOUNT_ANTHROPIC,
                ACCOUNT_OPENROUTER,
                ACCOUNT_DEEPSEEK,
            ]
        })
        .iter()
        .enumerate()
    {
        lines.push(setup_account_row(account, idx, app.selected_row));
    }
    lines.extend([
        Line::from(""),
        Line::from(Span::styled("enter select     esc quit", muted())),
    ]);
    lines
}

fn setup_account_row(label: &str, idx: usize, selected_row: usize) -> Line<'static> {
    let is_selected = idx == selected_row;
    let detected_codex = label == ACCOUNT_CODEX;
    let display = if detected_codex {
        "Continue with Codex login"
    } else {
        label
    };
    Line::from(vec![
        Span::styled(
            if is_selected { "> " } else { "  " },
            if is_selected { accent() } else { dim() },
        ),
        Span::styled(
            display.to_string(),
            if detected_codex {
                done()
            } else if is_selected {
                bold()
            } else {
                text_style()
            },
        ),
    ])
}

fn setup_confirm_lines(app: &App) -> Vec<Line<'static>> {
    let account = app
        .setup_pending_account
        .as_deref()
        .unwrap_or(ACCOUNT_CODEX);
    let title = if account == ACCOUNT_CODEX {
        "Continue with Codex login?".to_string()
    } else {
        format!("Use {account}?")
    };
    let mut lines = vec![Line::from(Span::styled(title, bold())), Line::from("")];
    if account == ACCOUNT_CODEX {
        lines.extend([
            Line::from("  A local Codex login is already available."),
            Line::from("  Continue to Browser Use Cloud setup."),
            Line::from("  No API key is required."),
        ]);
    } else if is_claude_code_account(account) {
        if app.account_ready(account).unwrap_or(false) {
            lines.push(Line::from("  Claude Code login found."));
        } else {
            lines.extend([
                Line::from("  Opens Anthropic OAuth sign-in in your browser."),
                Line::from("  Browser Use waits here for the localhost callback."),
                Line::from("  No API key or second terminal is required."),
            ]);
        }
    } else {
        lines.extend([
            Line::from("  Your key will be entered in the API key modal."),
            Line::from("  We confirm that the key was saved locally."),
        ]);
    }
    let primary_label =
        if is_claude_code_account(account) && !app.account_ready(account).unwrap_or(false) {
            "Open sign-in"
        } else if account == ACCOUNT_CODEX {
            "Continue"
        } else {
            "Continue"
        };
    lines.extend([
        Line::from(""),
        selected(primary_label, 0, app.selected_row),
        selected("Back", 1, app.selected_row),
    ]);
    lines
}

fn setup_cloud_lines(app: &App) -> Vec<Line<'static>> {
    let mut lines = vec![
        Line::from(Span::styled("One-Click Browser Use Cloud Setup?", bold())),
        Line::from(""),
        Line::from("  Cloud browsers are:"),
        Line::from(""),
        Line::from("  > free"),
        Line::from("  > automatically solve captchas"),
        Line::from("  > sync local cookies so you stay logged in"),
        Line::from("  > avoid local Chrome permission prompts"),
        Line::from(""),
        selected("Yes", 0, app.selected_row),
        selected("No", 1, app.selected_row),
    ];
    if app.browser_use_cloud_key_ready().unwrap_or(false) {
        lines.insert(
            2,
            Line::from(Span::styled(
                "  Browser Use Cloud is already connected.",
                done(),
            )),
        );
    }
    lines
}

fn setup_cloud_connected_lines(app: &App) -> Vec<Line<'static>> {
    vec![
        Line::from(Span::styled("Continue to cookie sync?", bold())),
        Line::from(""),
        selected("Yes", 0, app.selected_row),
        selected("Skip", 1, app.selected_row),
    ]
}

fn setup_result_lines(app: &App, width: usize) -> Vec<Line<'static>> {
    let Some(result) = app.setup_result.as_ref() else {
        return vec![
            Line::from(Span::styled("No setup result.", failed())),
            Line::from(""),
            selected("Back", 0, app.selected_row),
        ];
    };
    let is_success = result.kind == SetupResultKind::Success;
    let is_pending = result.kind == SetupResultKind::Pending;
    let mut lines = vec![
        Line::from(Span::styled(
            result.message.clone(),
            if is_success {
                done()
            } else if is_pending {
                muted()
            } else {
                failed()
            },
        )),
        Line::from(""),
        Line::from(format!("  {}", result.account)),
    ];
    if is_success {
        let next_message = if app.pending_model_after_auth.is_some() {
            "  Continue applies the selected model."
        } else if app.setup_complete {
            "  Continue keeps your current model."
        } else {
            "  Continue to Browser Use Cloud setup."
        };
        lines.extend([
            Line::from(Span::styled(next_message, muted())),
            Line::from(""),
            selected("Continue", 0, app.selected_row),
        ]);
    } else if is_pending {
        if result.account == ACCOUNT_CODEX {
            if let Some(seconds) = app.codex_login_elapsed_seconds() {
                lines.push(Line::from(Span::styled(
                    format!("  Waiting for device sign-in ({seconds}s)."),
                    muted(),
                )));
            }
            let output_lines = app.codex_login_output_lines();
            if output_lines.is_empty() {
                lines.push(Line::from("  Starting Codex device sign-in..."));
            } else {
                lines.push(Line::from(""));
                for line in output_lines
                    .into_iter()
                    .rev()
                    .take(8)
                    .collect::<Vec<_>>()
                    .into_iter()
                    .rev()
                {
                    push_wrapped_prefixed_text(&mut lines, "  ", &line, width);
                }
            }
        } else if result.account == BROWSER_USE_CLOUD {
            if let Some(seconds) = app.browser_use_cloud_login_elapsed_seconds() {
                lines.push(Line::from(Span::styled(
                    format!("  Waiting for cloud approval ({seconds}s)."),
                    muted(),
                )));
            }
            if let Some(error) = app.browser_use_cloud_open_error() {
                lines.push(Line::from(Span::styled(
                    format!("  Could not open browser automatically: {error}"),
                    failed(),
                )));
            } else {
                lines.push(Line::from("  Browser sign-in opened."));
            }
            if let Some(authorization) = app.browser_use_cloud_authorization() {
                lines.push(Line::from(""));
                lines.push(Line::from("  Browser authorization link:"));
                push_wrapped_prefixed_text(
                    &mut lines,
                    "    ",
                    &authorization.authorization_uri,
                    width,
                );
                lines.push(Line::from(""));
                lines.push(Line::from("  Callback listener:"));
                push_wrapped_prefixed_text(&mut lines, "    ", &authorization.redirect_uri, width);
            } else {
                lines.push(Line::from("  Requesting browser sign-in..."));
            }
        } else {
            if let Some(seconds) = app.claude_code_oauth_elapsed_seconds() {
                lines.push(Line::from(Span::styled(
                    format!("  Waiting for callback ({seconds}s)."),
                    muted(),
                )));
            }
            if let Some(error) = app.claude_code_oauth_open_error() {
                lines.push(Line::from(Span::styled(
                    format!("  Could not open browser automatically: {error}"),
                    failed(),
                )));
            } else {
                lines.push(Line::from("  Browser sign-in opened."));
            }
            if let Some(url) = app.claude_code_oauth_url() {
                lines.push(Line::from(""));
                lines.push(Line::from("  OAuth link:"));
                push_wrapped_prefixed_text(&mut lines, "    ", url, width);
            }
        }
        lines.extend([
            Line::from(""),
            selected(
                if result.account == ACCOUNT_CODEX || result.account == BROWSER_USE_CLOUD {
                    "Open sign-in page"
                } else {
                    "Open browser again"
                },
                0,
                app.selected_row,
            ),
            selected("Back", 1, app.selected_row),
        ]);
    } else {
        if is_claude_code_account(&result.account) {
            lines.extend([
                Line::from(""),
                Line::from("  Start the OAuth sign-in again from here."),
            ]);
        }
        lines.extend([
            Line::from(""),
            selected("Retry", 0, app.selected_row),
            selected("Back", 1, app.selected_row),
        ]);
    }
    lines
}

fn push_wrapped_prefixed_text(
    lines: &mut Vec<Line<'static>>,
    prefix: &str,
    text: &str,
    width: usize,
) {
    let available = width.saturating_sub(prefix.chars().count()).max(20);
    let mut start = 0;
    while start < text.len() {
        let mut end = (start + available).min(text.len());
        while !text.is_char_boundary(end) && end > start {
            end -= 1;
        }
        if end == start {
            end = text.len();
        }
        lines.push(Line::from(Span::styled(
            format!("{prefix}{}", &text[start..end]),
            text_style(),
        )));
        start = end;
    }
}

fn centered_line(text: &str, width: usize, style: Style) -> Line<'static> {
    Line::from(vec![
        Span::raw(" ".repeat(width.saturating_sub(text.chars().count()) / 2)),
        Span::styled(text.to_string(), style),
    ])
}

fn centered_line_in_width(text: &str, width: usize, style: Style) -> Line<'static> {
    Line::from(vec![
        Span::raw(" ".repeat(width.saturating_sub(text.chars().count()) / 2)),
        Span::styled(text.to_string(), style),
    ])
}

const FEEDBACK_THANKS_FACE_FRAME_0: &str = r"\(•◡•)/";
const FEEDBACK_THANKS_FACE_FRAME_1: &str = r"/(•◡•)\";
const FEEDBACK_THANKS_MESSAGE: &str = "Thanks for the feedback!";
const FEEDBACK_THANKS_HINT: &str = "press any key to continue";

fn render_success_character(
    frame: &mut Frame<'_>,
    area: Rect,
    started_at: Option<std::time::Instant>,
    message: &str,
    hint: &str,
) {
    frame.render_widget(Clear, frame.area());
    let elapsed_ms = started_at
        .map(|t| t.elapsed().as_millis() as u64)
        .unwrap_or(0);
    let frame_idx = (elapsed_ms / crate::FEEDBACK_THANKS_FRAME_MS) % 2;
    let face = if frame_idx == 0 {
        FEEDBACK_THANKS_FACE_FRAME_0
    } else {
        FEEDBACK_THANKS_FACE_FRAME_1
    };
    let w = area.width as usize;
    let content_lines: Vec<Line<'static>> = vec![
        centered_line(face, w, accent()),
        Line::from(""),
        centered_line(message, w, accent()),
        Line::from(""),
        centered_line(hint, w, muted()),
    ];
    let content_h = content_lines.len() as u16;
    let top_pad = area.height.saturating_sub(content_h) / 2;
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(top_pad),
            Constraint::Length(content_h),
            Constraint::Min(0),
        ])
        .split(area);
    frame.render_widget(Paragraph::new(content_lines), chunks[1]);
}

fn render_feedback_thanks(frame: &mut Frame<'_>, area: Rect, app: &App) {
    render_success_character(
        frame,
        area,
        app.feedback_thanks_started,
        FEEDBACK_THANKS_MESSAGE,
        FEEDBACK_THANKS_HINT,
    );
}

fn line_width(line: &Line<'_>) -> usize {
    line.spans
        .iter()
        .map(|span| span.content.chars().count())
        .sum()
}

fn account_lines(app: &App) -> Vec<Line<'static>> {
    let mut lines = vec![
        Line::from(Span::styled("Authenticate", bold())),
        Line::from(""),
    ];
    if let Some(notice) = app.status_notice.as_ref() {
        lines.push(Line::from(Span::styled(
            notice.clone(),
            status_style("failed"),
        )));
        lines.push(Line::from(""));
    }
    for (idx, account) in AUTH_CHOICES.iter().enumerate() {
        let status = if app.account_ready(account).unwrap_or(false) {
            "connected"
        } else if account.contains("API key") || *account == BROWSER_USE_CLOUD {
            "needs key"
        } else {
            "needs auth"
        };
        lines.push(selected(
            &format!("{account:<24} {status}"),
            idx,
            app.selected_row,
        ));
    }
    lines
}

fn api_key_lines(app: &App) -> Vec<Line<'static>> {
    let account = app.api_key_account.as_deref().unwrap_or("selected account");
    let mut lines = vec![Line::from(Span::styled(auth_secret_label(account), bold()))];
    lines.push(Line::from(""));
    lines.extend([
        Line::from(format!(
            "  {}",
            masked_secret_for_account(account, app.composer.input())
        )),
        Line::from(""),
        Line::from(Span::styled(
            if account == BROWSER_USE_CLOUD {
                "  Stored locally and passed to browser worker as BROWSER_USE_API_KEY."
            } else {
                "  This key is stored locally in browser-use state."
            },
            muted(),
        )),
        Line::from(""),
    ]);
    if let Some(notice) = app.status_notice.as_ref() {
        lines.push(Line::from(Span::styled(
            notice.clone(),
            status_style("failed"),
        )));
        lines.push(Line::from(""));
    }
    lines.push(selected("Save key", 0, app.selected_row));
    lines.push(selected("Cancel", 1, app.selected_row));
    lines
}

fn telemetry_key_lines(app: &App) -> Vec<Line<'static>> {
    let mut lines = vec![
        Line::from(Span::styled("Laminar API key", bold())),
        Line::from(""),
        Line::from(format!("  {}", masked_secret(app.composer.input()))),
        Line::from(""),
        Line::from(Span::styled(
            "  Stored locally and used by future agent runs.",
            muted(),
        )),
        Line::from(""),
    ];
    if let Some(notice) = app.status_notice.as_ref() {
        lines.push(Line::from(Span::styled(
            notice.clone(),
            status_style("failed"),
        )));
        lines.push(Line::from(""));
    }
    lines.push(selected("Save key", 0, app.selected_row));
    lines.push(selected("Cancel", 1, app.selected_row));
    lines
}

/// The provider screen: recommended quick-picks, then the provider/account list
/// with auth status. One flat `selected_row` spans recommended-then-provider rows.
fn provider_lines(app: &App) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    if let Some(notice) = app.status_notice.as_ref() {
        lines.push(Line::from(Span::styled(notice.clone(), failed())));
        lines.push(Line::from(""));
    }
    // The active model is shown current on its recommended row (if it's one of
    // the top picks) OR on its provider row below (so it's visible either way).
    let current_recommended = app.current_recommended_index();
    lines.push(Line::from(Span::styled("recommended", muted())));
    let recommended = app.recommended_models();
    for (idx, rec) in recommended.iter().enumerate() {
        lines.push(selectable_row(
            &format!("{:<22} {}", rec.display, access_label(rec.account)),
            idx,
            app.selected_row,
            current_recommended == Some(idx),
        ));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled("providers", muted())));
    let base = recommended.len();
    for (idx, row) in app.provider_rows().iter().enumerate() {
        let status = if app.provider_row_connected(row) {
            "connected"
        } else {
            "needs key"
        };
        // Mark the provider row current only when the active model isn't a
        // recommended pick (avoids double-marking top + bottom).
        let is_current = current_recommended.is_none() && app.provider_row_is_current(row);
        lines.push(selectable_row(
            &format!("{:<24} {status}", row.label),
            base + idx,
            app.selected_row,
            is_current,
        ));
    }
    lines
}

/// The provider auth sub-dialogue: connect via Codex / API key, with the current
/// method shown green.
fn openai_auth_lines(app: &App) -> Vec<Line<'static>> {
    let mut lines = vec![Line::from(Span::styled(
        format!("connect {}", app.provider_auth_label().to_ascii_lowercase()),
        muted(),
    ))];
    let current = app.current_provider_auth_method();
    for (idx, row) in app.provider_auth_rows().iter().enumerate() {
        lines.push(selectable_row(
            &row.label,
            idx,
            app.selected_row,
            current == Some(row.method),
        ));
    }
    lines
}

fn model_lines(app: &App, height: usize) -> Vec<Line<'static>> {
    let mut lines: Vec<(Option<usize>, Line<'static>)> = Vec::new();
    if let Some(notice) = app.status_notice.as_ref() {
        lines.push((None, Line::from(Span::styled(notice.clone(), failed()))));
        lines.push((None, Line::from("")));
    }
    let choices = app.model_surface_choices();
    let mut row_idx = 0usize;
    for choice in &choices {
        lines.push((Some(row_idx), model_row(choice, row_idx, app)));
        row_idx += 1;
    }
    if app.model_surface_has_custom_row() {
        let is_selected = row_idx == app.selected_row;
        lines.push((Some(row_idx), model_custom_row(is_selected)));
    }
    // Plain catalog list — no pinned header, scroll every row.
    crop_model_lines(lines, app.selected_row, height, 0)
}

/// The trailing "enter a custom model" row on the OpenRouter model screen.
fn model_custom_row(is_selected: bool) -> Line<'static> {
    let style = if is_selected { bold() } else { text_style() };
    highlight_selectable_row(
        vec![Span::styled(
            "+ Enter a custom OpenRouter model".to_string(),
            style,
        )],
        is_selected,
        68,
    )
}

/// The OpenRouter free-text search screen: a query input followed by typeahead
/// suggestions (and the raw typed id as the first row when not an exact match).
fn model_search_lines(app: &App, height: usize) -> Vec<Line<'static>> {
    let mut lines: Vec<(Option<usize>, Line<'static>)> = Vec::new();
    if app.model_search_has_filter_input() {
        // Query input line; the popup cursor logic positions the caret at its end.
        lines.push((None, Line::from(format!("  {}", app.composer.input()))));
        lines.push((None, Line::from("")));
    }
    if let Some(notice) = app.status_notice.as_ref() {
        lines.push((None, Line::from(Span::styled(notice.clone(), failed()))));
        lines.push((None, Line::from("")));
    }
    let entries = app.model_search_entries();
    if entries.is_empty() {
        let empty_message = if !app.model_search_has_filter_input() {
            "  No models available"
        } else if app.selected_provider == Some(ACCOUNT_OPENROUTER) {
            "  Type a model id, e.g. moonshotai/kimi-k2.5"
        } else {
            "  No matching models"
        };
        lines.push((
            None,
            Line::from(Span::styled(empty_message.to_string(), muted())),
        ));
    }
    // Items carry a running selectable index (matching `model_search_rows`);
    // headers are non-selectable section labels.
    let current_id = app.current_search_model_id();
    let mut item_idx = 0usize;
    for entry in &entries {
        match entry {
            ModelSearchEntry::Header(label) => {
                lines.push((None, Line::from(Span::styled(label.clone(), muted()))));
            }
            ModelSearchEntry::Item(id) => {
                let is_selected = item_idx == app.selected_row;
                let is_current = current_id == Some(id.as_str());
                let style = if is_current {
                    current()
                } else if is_selected {
                    bold()
                } else {
                    text_style()
                };
                let mut spans = vec![Span::styled(id.clone(), style)];
                if app.provider_model_is_vision(id) {
                    spans.push(Span::styled("  · vision".to_string(), muted()));
                }
                lines.push((
                    Some(item_idx),
                    highlight_selectable_row(spans, is_selected, 68),
                ));
                item_idx += 1;
            }
        }
    }
    // Pin the query-input line (index 0) only when the provider has one, so
    // plain curated lists scroll every row normally.
    let pinned_head = usize::from(app.model_search_has_filter_input());
    crop_model_lines(lines, app.selected_row, height, pinned_head)
}

/// Crop `lines` to `height` rows, centering on `selected_row`. `pinned_head`
/// leading lines are kept fixed at the top (used by the search surface to keep
/// its query-input line — and the popup caret — visible while the rows beneath
/// scroll); pass `0` for a plain list with no header to scroll normally.
fn crop_model_lines(
    lines: Vec<(Option<usize>, Line<'static>)>,
    selected_row: usize,
    height: usize,
    pinned_head: usize,
) -> Vec<Line<'static>> {
    if height == 0 {
        return Vec::new();
    }
    if lines.len() <= height {
        return lines.into_iter().map(|(_, line)| line).collect();
    }
    let pinned = pinned_head.min(height);
    let head: Vec<Line<'static>> = lines[..pinned]
        .iter()
        .map(|(_, line)| line.clone())
        .collect();
    let body = &lines[pinned..];
    let visible = height - pinned;
    if visible == 0 || body.len() <= visible {
        let mut out = head;
        out.extend(body.iter().map(|(_, line)| line.clone()));
        return out;
    }
    let selected_line = body
        .iter()
        .position(|(row, _)| *row == Some(selected_row))
        .unwrap_or(0);
    let mut start = selected_line.saturating_sub(visible / 2);
    start = start.min(body.len().saturating_sub(visible));
    let end = (start + visible).min(body.len());
    let mut data = body[start..end]
        .iter()
        .map(|(_, line)| line.clone())
        .collect::<Vec<_>>();
    if start > 0 {
        data[0] = Line::from(Span::styled("  ...", muted()));
    }
    if end < body.len() {
        let last = data.len().saturating_sub(1);
        data[last] = Line::from(Span::styled("  ...", muted()));
    }
    let mut out = head;
    out.extend(data);
    out
}

fn model_row(choice: &ModelChoice, row_idx: usize, app: &App) -> Line<'static> {
    let is_selected = row_idx == app.selected_row;
    let current =
        app.model_configured && app.model == choice.display && app.account == choice.account;
    let name_style = if is_selected { bold() } else { text_style() };
    let access = access_label(choice.account);
    let descriptor = choice.descriptor.as_str();
    let descriptor_style = if descriptor == "needs key" {
        dim()
    } else {
        muted()
    };
    // Truncate an over-long descriptor to the column width, but do not right-pad
    // a short one, so the current-model `*` sits immediately after the text
    // instead of floating at the far end of a fixed 22-char column.
    let descriptor_cell = if descriptor.chars().count() <= 22 {
        descriptor.to_string()
    } else {
        fixed_width_cell(descriptor, 22)
    };
    highlight_selectable_row(
        vec![
            Span::styled(fixed_width_cell(&choice.display, 20), name_style),
            Span::styled(format!("{:<22}", access), muted()),
            Span::styled(descriptor_cell, descriptor_style),
            Span::styled(if current { " *" } else { "" }.to_string(), done()),
        ],
        is_selected,
        // 2-space indent + 20 + 22 + up-to-22 descriptor + " *" — keep the
        // highlight padding target at the widest possible row so every selected
        // row still highlights to the same end column.
        68,
    )
}

fn fixed_width_cell(value: &str, width: usize) -> String {
    let char_count = value.chars().count();
    if char_count <= width {
        return format!("{value:<width$}");
    }
    if width == 0 {
        return String::new();
    }
    let mut out = value
        .chars()
        .take(width.saturating_sub(1))
        .collect::<String>();
    out.push('~');
    out
}

fn mode_lines(app: &App) -> Vec<Line<'static>> {
    vec![mode_row(
        0,
        app,
        CollaborationModeKind::Default,
        "Default",
        "execute tasks and update TODOs",
    )]
}

fn mode_row(
    row_idx: usize,
    app: &App,
    mode: CollaborationModeKind,
    label: &'static str,
    description: &'static str,
) -> Line<'static> {
    let is_selected = row_idx == app.selected_row;
    let current = app.collaboration_mode == mode;
    highlight_selectable_row(
        vec![
            Span::styled(
                format!("{label:<12}"),
                if is_selected { bold() } else { text_style() },
            ),
            Span::styled(format!("{description:<38}"), muted()),
            Span::styled(if current { " *" } else { "" }.to_string(), done()),
        ],
        is_selected,
        56,
    )
}

fn access_label(account: &'static str) -> &'static str {
    if account == ACCOUNT_CODEX {
        "Codex login"
    } else if is_claude_code_account(account) {
        "Claude Code sub"
    } else {
        account
    }
}

fn browser_select_lines(app: &App, width: usize) -> Vec<Line<'static>> {
    let body_width = cookie_sync_body_width(width);
    let mut lines = vec![
        Line::from(Span::styled("CHOOSE BROWSER", muted())),
        Line::from(""),
    ];
    lines.push(Line::from(Span::styled("BROWSERS", muted())));
    match &app.default_profile.status {
        DefaultProfileStatus::Loading => {
            lines.push(Line::from("  Scanning local browsers..."));
        }
        DefaultProfileStatus::Ready => {
            let rows = app.browser_select_rows();
            if rows
                .iter()
                .all(|row| matches!(row, BrowserSelectRow::Cloud))
            {
                lines.push(Line::from("  No local browsers found."));
            } else {
                for (idx, row) in rows.iter().enumerate() {
                    match row {
                        BrowserSelectRow::Local(browser) => {
                            let is_current = browser_select_local_row_is_current(app, browser);
                            let metadata = if browser.eq_ignore_ascii_case("Google Chrome") {
                                Some("recommended")
                            } else {
                                None
                            };
                            lines.push(browser_select_row_line(
                                &truncate(browser, body_width),
                                metadata,
                                idx,
                                app.selected_row,
                                is_current,
                            ));
                        }
                        BrowserSelectRow::ChromiumHeaded => {
                            lines.push(browser_select_row_line(
                                "  Mode: headed",
                                None,
                                idx,
                                app.selected_row,
                                app.browser == "Managed Chromium",
                            ));
                        }
                        BrowserSelectRow::ChromiumHeadless => {
                            lines.push(browser_select_row_line(
                                "  Mode: headless",
                                None,
                                idx,
                                app.selected_row,
                                app.browser == "Headless Chromium",
                            ));
                        }
                        BrowserSelectRow::Cloud => {}
                    }
                }
            }
        }
        DefaultProfileStatus::Failed(error) => {
            push_wrapped_cookie_sync_message(&mut lines, error, body_width);
        }
    }
    let non_local_start = app.browser_select_local_browser_count();
    let cloud_description = if !app.browser_use_cloud_key_ready().unwrap_or(false) {
        "needs Browser Use key"
    } else {
        "remote browser with live view"
    };
    lines.extend([Line::from(""), Line::from(Span::styled("REMOTE", muted()))]);
    lines.push(browser_select_row_line(
        BROWSER_USE_CLOUD,
        Some(cloud_description),
        non_local_start,
        app.selected_row,
        app.browser == BROWSER_USE_CLOUD,
    ));
    lines
}

fn browser_select_row_line(
    title: &str,
    metadata: Option<&str>,
    idx: usize,
    selected: usize,
    is_current: bool,
) -> Line<'static> {
    let cursor = idx == selected;
    let title_style = if is_current {
        current()
    } else if cursor {
        bold()
    } else {
        text_style()
    };
    let mut spans = vec![
        Span::styled(
            if cursor { "> " } else { "  " },
            if cursor { accent() } else { dim() },
        ),
        Span::styled(title.to_string(), title_style),
    ];
    if let Some(metadata) = metadata.filter(|value| !value.trim().is_empty()) {
        spans.push(Span::styled(format!("  {metadata}"), muted()));
    }
    Line::from(spans)
}

fn browser_select_local_row_is_current(app: &App, browser: &str) -> bool {
    if app.browser == "Headless Chromium" {
        return false;
    }
    if app.browser == "Managed Chromium" {
        return false;
    }
    app.browser == BROWSER_LOCAL_CHROME
        && app
            .current_local_browser_label()
            .as_deref()
            .is_some_and(|current| current.eq_ignore_ascii_case(browser))
}

fn goal_lines(app: &App) -> Vec<Line<'static>> {
    let Some(session_id) = app.selected_session_id.as_deref() else {
        return vec![Line::from("Open a task before using /goal.")];
    };
    let Some(goal) = app.current_goal_for_session(session_id) else {
        return vec![
            Line::from("No goal set for this task."),
            Line::from(""),
            Line::from(Span::styled("Commands", muted())),
            Line::from("  /goal <objective>"),
            Line::from("  /goal clear"),
        ];
    };
    let objective = goal
        .get("objective")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
    let status = goal
        .get("status")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("active");
    let tokens_used = goal
        .get("tokensUsed")
        .and_then(serde_json::Value::as_i64)
        .unwrap_or_default();
    let time_used_seconds = goal
        .get("timeUsedSeconds")
        .and_then(serde_json::Value::as_i64)
        .unwrap_or_default();
    let budget = goal.get("tokenBudget").and_then(serde_json::Value::as_i64);
    let token_text = match budget {
        Some(budget) => format!(
            "{}/{}",
            format_goal_tokens_compact(tokens_used),
            format_goal_tokens_compact(budget)
        ),
        None if tokens_used > 0 => format_goal_tokens_compact(tokens_used),
        None => "0".to_string(),
    };
    vec![
        Line::from(vec![
            Span::styled("Status  ", muted()),
            Span::raw(goal_status_label(status).to_string()),
        ]),
        Line::from(vec![
            Span::styled("Time    ", muted()),
            Span::raw(format_goal_elapsed_seconds(time_used_seconds)),
        ]),
        Line::from(vec![
            Span::styled("Tokens  ", muted()),
            Span::raw(token_text),
        ]),
        Line::from(""),
        Line::from(Span::styled("Objective", muted())),
        Line::from(format!("  {objective}")),
        Line::from(""),
        Line::from(Span::styled(goal_command_hint(status).to_string(), muted())),
    ]
}

/// System prompt + per-tool schema sizes the agent recorded for the most recent
/// request (`model.turn.request` `composition`). These are assembled inside the
/// agent and never persisted as message events, so without them the window
/// cannot be fully attributed.
#[derive(Debug, Default, Clone)]
struct ContextComposition {
    recorded: bool,
    system_prompt_tokens: i64,
    tools: Vec<(String, i64)>,
}

impl ContextComposition {
    fn tool_total(&self) -> i64 {
        self.tools.iter().map(|(_, tokens)| *tokens).sum()
    }
}

/// One row of the attribution breakdown.
#[derive(Debug, Clone)]
struct ContextComponent {
    label: &'static str,
    tokens: i64,
}

#[derive(Debug, Default, Clone)]
struct ContextUsageSummary {
    latest_input_tokens: Option<i64>,
    latest_cached_input_tokens: Option<i64>,
    total_input_tokens: Option<i64>,
    total_cached_input_tokens: Option<i64>,
    context_window: Option<i64>,
}

fn context_lines(app: &App, state: &WorkbenchState, width: usize) -> Vec<Line<'static>> {
    let Some(session) = state.current_session.as_ref() else {
        return vec![Line::from(Span::styled("No task selected.", dim()))];
    };
    let events = app.cached_events_for_session(&session.id);
    let usage = context_usage_from_events(events);
    let composition = context_composition_from_events(events);

    // What's actually in the window right now (provider-reported input tokens).
    let conversation = message_categories_from_events(events);
    let conversation_sum: i64 = conversation.iter().map(|component| component.tokens).sum();
    let window_used = usage.latest_input_tokens.unwrap_or(conversation_sum);

    // Build the breakdown so it always covers the WHOLE window — nothing hidden.
    // Conversation categories come from message events; the system prompt + tool
    // schemas come from the agent. When the agent recorded them we show them
    // split; otherwise the remaining window (everything that isn't conversation)
    // is exactly the system prompt + tool schemas, shown as one row.
    let mut components: Vec<ContextComponent> = Vec::new();
    if composition.recorded {
        if composition.system_prompt_tokens > 0 {
            components.push(ContextComponent {
                label: "System prompt",
                tokens: composition.system_prompt_tokens,
            });
        }
        let tool_total = composition.tool_total();
        if tool_total > 0 {
            components.push(ContextComponent {
                label: "Tool definitions",
                tokens: tool_total,
            });
        }
        components.extend(conversation);
    } else {
        components.extend(conversation);
        let overhead = window_used.saturating_sub(conversation_sum);
        if overhead > 0 {
            components.push(ContextComponent {
                label: "System prompt + tools",
                tokens: overhead,
            });
        }
    }

    // Reconcile the heuristic estimate to the real window so the rows sum to it.
    let attributed: i64 = components.iter().map(|component| component.tokens).sum();
    if window_used == 0 {
        components.clear();
    } else if attributed > 0 {
        for component in &mut components {
            component.tokens =
                ((component.tokens as i128 * window_used as i128) / attributed as i128) as i64;
        }
    }
    components.sort_by_key(|component| std::cmp::Reverse(component.tokens));
    let breakdown_total = components
        .iter()
        .map(|component| component.tokens)
        .sum::<i64>()
        .max(1);

    // ---- Window bar (segments colored per category) ----
    let mut lines = vec![Line::from(Span::styled("Window", bold())), Line::from("")];
    if usage.context_window.is_some() || window_used > 0 {
        let bar_width = width.saturating_sub(34).clamp(12, 42);
        let mut spans = vec![
            Span::raw("  "),
            Span::styled(format!("{:<10}", "context"), muted()),
        ];
        spans.extend(context_segmented_bar_spans(
            &components,
            window_used,
            usage.context_window,
            bar_width,
        ));
        lines.push(Line::from(spans));
    }

    let cache_lines = prompt_cache_lines(&usage);
    if !cache_lines.is_empty() {
        // ---- Provider prompt cache ----
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled("Prompt cache", bold())));
        lines.push(Line::from(""));
        lines.extend(cache_lines);
    }

    // ---- What's in it ----
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled("What's using it", bold())));
    lines.push(Line::from(""));
    if components.is_empty() {
        lines.push(Line::from(Span::styled("No context yet.", dim())));
    } else {
        for component in &components {
            lines.push(context_component_line(
                component.label,
                component.tokens,
                breakdown_total,
                width,
            ));
        }
    }

    lines
}

fn context_usage_from_events(events: &[EventRecord]) -> ContextUsageSummary {
    let mut summary = ContextUsageSummary::default();
    for event in events {
        if event.event_type != "token_count" {
            continue;
        }
        let Some(info) = event.payload.get("info").filter(|info| info.is_object()) else {
            continue;
        };
        if let Some(usage) = info
            .get("last_token_usage")
            .filter(|usage| usage.is_object())
        {
            if let Some(input) = usage.get("input_tokens").and_then(Value::as_i64) {
                summary.latest_input_tokens = Some(input.max(0));
            }
            summary.latest_cached_input_tokens = cached_input_from_usage(usage);
        }
        if let Some(usage) = info
            .get("total_token_usage")
            .filter(|usage| usage.is_object())
        {
            if let Some(input) = usage.get("input_tokens").and_then(Value::as_i64) {
                summary.total_input_tokens = Some(input.max(0));
            }
            summary.total_cached_input_tokens = cached_input_from_usage(usage);
        }
        if let Some(context_window) = info
            .get("model_context_window")
            .and_then(Value::as_i64)
            .filter(|tokens| *tokens > 0)
        {
            summary.context_window = Some(context_window);
        }
    }
    summary
}

fn cached_input_from_usage(usage: &Value) -> Option<i64> {
    usage
        .get("cached_input_tokens")
        .or_else(|| usage.get("input_cached_tokens"))
        .and_then(Value::as_i64)
        .map(|cached| cached.max(0))
}

fn prompt_cache_lines(usage: &ContextUsageSummary) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    if let (Some(input), Some(cached)) =
        (usage.latest_input_tokens, usage.latest_cached_input_tokens)
    {
        lines.push(prompt_cache_line("last turn", cached, input));
    }
    if let (Some(input), Some(cached)) = (usage.total_input_tokens, usage.total_cached_input_tokens)
    {
        lines.push(prompt_cache_line("session total", cached, input));
    }
    lines
}

fn prompt_cache_line(label: &str, cached: i64, input: i64) -> Line<'static> {
    let cached = cached.max(0);
    let input = input.max(0);
    let percent = context_percent(cached, input);
    let uncached = input.saturating_sub(cached);
    Line::from(vec![
        Span::raw("  "),
        Span::styled(format!("{label:<12}"), muted()),
        Span::styled(format!("{percent:>4}"), accent()),
        Span::raw("  "),
        Span::styled(format_token_count(cached), text_style()),
        Span::styled(" cached", muted()),
        Span::styled(" / ", dim()),
        Span::styled(format_token_count(input), text_style()),
        Span::styled(" input", muted()),
        Span::styled("  ", muted()),
        Span::styled(format_token_count(uncached), dim()),
        Span::styled(" uncached", dim()),
    ])
}

fn context_composition_from_events(events: &[EventRecord]) -> ContextComposition {
    let mut composition = ContextComposition::default();
    // Take the most recent recorded request; the loop overwrites earlier turns.
    for event in events {
        if event.event_type != "model.turn.request" {
            continue;
        }
        let Some(value) = event
            .payload
            .get("composition")
            .filter(|value| value.is_object())
        else {
            continue;
        };
        composition.recorded = true;
        composition.system_prompt_tokens = value
            .get("system_prompt_tokens")
            .and_then(Value::as_i64)
            .unwrap_or(0)
            .max(0);
        composition.tools = value
            .get("tools")
            .and_then(Value::as_array)
            .map(|tools| {
                tools
                    .iter()
                    .filter_map(|tool| {
                        let name = tool.get("name").and_then(Value::as_str)?.trim();
                        let tokens = tool
                            .get("tokens")
                            .and_then(Value::as_i64)
                            .unwrap_or(0)
                            .max(0);
                        (!name.is_empty()).then(|| (name.to_string(), tokens))
                    })
                    .collect()
            })
            .unwrap_or_default();
    }
    composition
}

fn message_categories_from_events(events: &[EventRecord]) -> Vec<ContextComponent> {
    let provider_items = provider_messages_from_events(events);
    let mut totals: BTreeMap<&'static str, i64> = BTreeMap::new();
    for item in &provider_items {
        let tokens = estimate_item_token_count(item).max(0);
        if tokens == 0 {
            continue;
        }
        *totals.entry(context_item_category(item)).or_default() += tokens;
    }
    totals
        .into_iter()
        .map(|(label, tokens)| ContextComponent { label, tokens })
        .collect()
}

fn context_item_category(item: &Value) -> &'static str {
    match item.get("type").and_then(Value::as_str) {
        Some("reasoning") => return "Reasoning",
        Some("compaction") => return "Compaction",
        Some("function_call") | Some("custom_tool_call") | Some("local_shell_call") => {
            return "Tool calls"
        }
        Some("function_call_output") | Some("custom_tool_call_output") => return "Tool outputs",
        _ => {}
    }

    match item.get("role").and_then(Value::as_str) {
        Some("system") => "System prompts",
        Some("user") => "User messages",
        Some("assistant") if item_has_tool_calls(item) && item_preview_text(item).is_none() => {
            "Tool calls"
        }
        Some("assistant") => "Assistant responses",
        Some("tool") => "Tool outputs",
        Some(_) => "Other context",
        None => "Other context",
    }
}

fn item_has_tool_calls(item: &Value) -> bool {
    item.get("tool_calls")
        .and_then(Value::as_array)
        .is_some_and(|calls| !calls.is_empty())
}

fn item_preview_text(item: &Value) -> Option<String> {
    for field in ["text", "content", "output"] {
        if let Some(value) = item.get(field) {
            if let Some(preview) = preview_text_value(value) {
                return Some(preview);
            }
        }
    }
    None
}

fn preview_text_value(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => trimmed_nonempty(text),
        Value::Array(parts) => parts.iter().find_map(preview_text_value),
        Value::Object(map) => ["text", "content", "output", "input"]
            .into_iter()
            .find_map(|field| map.get(field).and_then(preview_text_value)),
        _ => None,
    }
}

fn trimmed_nonempty(value: &str) -> Option<String> {
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_string())
}

fn context_component_line(label: &str, tokens: i64, total: i64, width: usize) -> Line<'static> {
    let token_text = format_token_count(tokens);
    let percent = context_percent(tokens, total);
    let color = context_category_color(label);
    let category = Style::default().fg(color);
    // Pad the label to a fixed column so the percent aligns down the list.
    let label_col = 22.min(width.saturating_sub(18).max(8));
    Line::from(vec![
        Span::raw("  "),
        // Swatch matches this row's bar segment.
        Span::styled("█ ", category),
        Span::styled(format!("{token_text:>6}"), category),
        Span::raw("  "),
        Span::styled(
            format!("{:<label_col$}", truncate(label, label_col)),
            category,
        ),
        Span::styled(format!("  {percent:>4}"), dim()),
    ])
}

/// A context bar whose filled portion is split into colored runs, one per
/// category, sized to each category's share of the window. The empty tail is
/// the remaining budget. Largest-remainder accumulation keeps the runs summing
/// to the filled width exactly.
fn context_segmented_bar_spans(
    components: &[ContextComponent],
    window_used: i64,
    budget: Option<i64>,
    bar_width: usize,
) -> Vec<Span<'static>> {
    let budget = budget
        .filter(|tokens| *tokens > 0)
        .unwrap_or(FALLBACK_CONTEXT_BUDGET_TOKENS);
    let used = window_used.clamp(0, budget);
    let used_blocks = ((used as f64 / budget as f64) * bar_width as f64).round() as usize;
    let used_blocks = used_blocks.min(bar_width);

    let mut spans = Vec::new();
    if window_used > 0 && used_blocks > 0 {
        let mut cumulative_tokens: i64 = 0;
        let mut filled: usize = 0;
        for component in components {
            cumulative_tokens = cumulative_tokens.saturating_add(component.tokens.max(0));
            let target = (((cumulative_tokens as f64 / window_used as f64) * used_blocks as f64)
                .round() as usize)
                .min(used_blocks);
            let blocks = target.saturating_sub(filled);
            filled = target;
            if blocks == 0 {
                continue;
            }
            spans.push(Span::styled(
                "█".repeat(blocks),
                Style::default().fg(context_category_color(component.label)),
            ));
        }
        if filled < used_blocks {
            spans.push(Span::styled("█".repeat(used_blocks - filled), muted()));
        }
    }
    let empty = bar_width.saturating_sub(used_blocks);
    if empty > 0 {
        spans.push(Span::styled("░".repeat(empty), dim()));
    }
    let pct_left = (((budget - used) as f64 / budget as f64) * 100.0).round() as i64;
    spans.push(Span::raw("  "));
    spans.push(Span::styled(
        format!(
            "{}/{}",
            format_token_count(window_used),
            format_token_count(budget)
        ),
        muted(),
    ));
    spans.push(Span::raw("  "));
    spans.push(Span::styled(format!("{pct_left}% left"), muted()));
    spans
}

fn context_percent(tokens: i64, total: i64) -> String {
    if total <= 0 {
        return "0%".to_string();
    }
    format!("{:.0}%", (tokens.max(0) as f64 / total as f64) * 100.0)
}

pub(crate) fn cookie_sync_lines(app: &App, width: usize) -> Vec<Line<'static>> {
    let body_width = cookie_sync_body_width(width);
    let mut lines = vec![
        Line::from(Span::styled("BROWSER USE CLOUD", muted())),
        Line::from(""),
    ];
    match &app.cookie_sync.status {
        CookieSyncStatus::NeedsAuth => {
            lines.push(Line::from("  Browser Use Cloud key is missing."));
            lines.push(Line::from(""));
            let label = if app.pending_setup_after_cookie_sync {
                "Sign in to Browser Use Cloud"
            } else {
                "Add Browser Use key"
            };
            lines.push(selected(label, 0, app.selected_row));
        }
        CookieSyncStatus::LoadingProfiles => {
            lines.push(Line::from("  Scanning local Chromium profiles..."));
        }
        CookieSyncStatus::Ready => {
            lines.push(kv_line("scope", "all cookies"));
            lines.push(kv_line("target", "new Browser Use Cloud profile"));
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled("LOCAL PROFILES", muted())));
            lines.push(Line::from(""));
            if app.cookie_sync.profiles.is_empty() {
                lines.push(Line::from("  No local Chromium profiles found."));
            } else {
                for (idx, profile) in app.cookie_sync.profiles.iter().enumerate() {
                    lines.push(selected(
                        &truncate(&profile.display_name, body_width),
                        idx,
                        app.selected_row,
                    ));
                }
            }
        }
        CookieSyncStatus::Syncing => {
            let profile = app
                .cookie_sync
                .selected_profile_label
                .as_deref()
                .unwrap_or("selected profile");
            let spinner = IMPORT_SPINNER[app.live_spinner_frame % IMPORT_SPINNER.len()];
            push_wrapped_cookie_sync_paragraph(
                &mut lines,
                &format!("{spinner} Syncing all cookies from {profile}..."),
                body_width,
            );
        }
        CookieSyncStatus::Completed(message) => {
            lines.push(Line::from(Span::styled("🎉 Complete", bold())));
            lines.push(Line::from(""));
            push_completed_cookie_sync_message(&mut lines, message, body_width);
            lines.push(Line::from(""));
            let label = if app.pending_setup_after_cookie_sync {
                "Continue"
            } else {
                "Close"
            };
            lines.push(selected(label, 0, app.selected_row));
        }
        CookieSyncStatus::Failed(error) => {
            lines.push(Line::from(Span::styled("Failed", bold())));
            lines.push(Line::from(""));
            push_wrapped_cookie_sync_message(&mut lines, error, body_width);
            lines.push(Line::from(""));
            let label = if app.pending_setup_after_cookie_sync {
                "Continue without cookie sync"
            } else {
                "Close"
            };
            lines.push(selected(label, 0, app.selected_row));
        }
    }
    lines
}

pub(crate) fn default_profile_lines(app: &App, width: usize) -> Vec<Line<'static>> {
    let body_width = cookie_sync_body_width(width);
    let mut lines = vec![
        Line::from(Span::styled("LOCAL CHROME", muted())),
        Line::from(""),
    ];
    match &app.default_profile.status {
        DefaultProfileStatus::Loading => {
            lines.push(Line::from("  Scanning local Chromium profiles..."));
        }
        DefaultProfileStatus::Ready => {
            let current = app
                .default_profile
                .current_profile_id
                .as_deref()
                .and_then(|current| {
                    app.default_profile
                        .profiles
                        .iter()
                        .find(|profile| profile.id == current)
                        .map(crate::human_profile_label)
                })
                .unwrap_or_else(|| "not set".to_string());
            lines.push(kv_line("default", &current));
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled("LOCAL PROFILES", muted())));
            lines.push(Line::from(""));
            if app.default_profile.profiles.is_empty() {
                lines.push(Line::from("  No local Chromium profiles found."));
            } else {
                for (idx, profile) in app.default_profile.profiles.iter().enumerate() {
                    let mut label = truncate(&crate::human_profile_label(profile), body_width);
                    if app
                        .default_profile
                        .current_profile_id
                        .as_deref()
                        .is_some_and(|current| current == profile.id)
                    {
                        label.push_str("  current");
                    }
                    lines.push(selected(&label, idx, app.selected_row));
                }
            }
        }
        DefaultProfileStatus::Failed(error) => {
            lines.push(Line::from(Span::styled("Failed", bold())));
            lines.push(Line::from(""));
            push_wrapped_cookie_sync_message(&mut lines, error, body_width);
            lines.push(Line::from(""));
            lines.push(selected("Close", 0, app.selected_row));
        }
    }
    lines
}

fn cookie_sync_body_width(width: usize) -> usize {
    width.saturating_sub(4).max(1).min(88)
}

fn push_completed_cookie_sync_message(lines: &mut Vec<Line<'static>>, message: &str, width: usize) {
    let mut paragraphs = message.lines();
    if let Some(first) = paragraphs.next() {
        if let Some((count, rest)) = synced_cookie_count_fragment(first) {
            lines.push(Line::from(vec![
                Span::raw("  Synced "),
                Span::styled(count.to_string(), bold()),
                Span::raw(rest.to_string()),
            ]));
        } else {
            push_wrapped_cookie_sync_paragraph(lines, first, width);
        }
    }
    for paragraph in paragraphs {
        push_wrapped_completed_cookie_sync_paragraph(lines, paragraph, width);
    }
}

fn synced_cookie_count_fragment(value: &str) -> Option<(&str, &str)> {
    let rest = value.strip_prefix("Synced ")?;
    let count_len = rest
        .char_indices()
        .find_map(|(idx, ch)| (!ch.is_ascii_digit() && ch != ',').then_some(idx))
        .unwrap_or(rest.len());
    if count_len == 0 {
        return None;
    }
    Some(rest.split_at(count_len))
}

fn push_wrapped_completed_cookie_sync_paragraph(
    lines: &mut Vec<Line<'static>>,
    message: &str,
    width: usize,
) {
    for line in wrap_cookie_sync_message(message, width) {
        if let Some((label, rest)) = cookie_sync_profile_label_fragment(&line) {
            lines.push(Line::from(vec![
                Span::raw("  "),
                Span::styled(label.to_string(), bold()),
                Span::raw(rest.to_string()),
            ]));
        } else {
            lines.push(Line::from(format!("  {line}")));
        }
    }
}

fn cookie_sync_profile_label_fragment(value: &str) -> Option<(&str, &str)> {
    ["Local profile", "Cloud profile"]
        .into_iter()
        .find_map(|label| value.strip_prefix(label).map(|rest| (label, rest)))
}

fn push_wrapped_cookie_sync_message(lines: &mut Vec<Line<'static>>, message: &str, width: usize) {
    for paragraph in message.lines() {
        push_wrapped_cookie_sync_paragraph(lines, paragraph, width);
    }
}

fn push_wrapped_cookie_sync_paragraph(lines: &mut Vec<Line<'static>>, message: &str, width: usize) {
    for line in wrap_cookie_sync_message(message, width) {
        lines.push(Line::from(format!("  {line}")));
    }
}

fn wrap_cookie_sync_message(message: &str, width: usize) -> Vec<String> {
    let mut lines = Vec::new();
    let mut current = String::new();
    for word in message.split_whitespace() {
        let word_len = word.chars().count();
        let current_len = current.chars().count();
        if !current.is_empty() && current_len + 1 + word_len > width {
            lines.push(std::mem::take(&mut current));
        }
        if !current.is_empty() {
            current.push(' ');
        }
        current.push_str(word);
    }
    if !current.is_empty() {
        lines.push(current);
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

fn ready_lines(app: &App, state: &WorkbenchState, width: u16, max_h: u16) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    if let Some(notice) = app.status_notice.as_ref() {
        lines.push(Line::from(Span::styled(notice.clone(), failed())));
        lines.push(Line::from(""));
    }
    let banner = cloud_home_banner_lines(app, width);
    // Pass the remaining body height to the welcome renderer so it can
    // balance the gap above the logo with the gap below the menu.
    let remaining = max_h.saturating_sub(lines.len() as u16);
    lines.extend(crate::welcome::welcome_lines(
        width,
        &app.welcome_anim,
        app.selected_row,
        remaining,
        banner,
    ));
    let _ = state;
    lines
}

fn cloud_home_banner_lines(app: &App, width: u16) -> Option<Vec<Line<'static>>> {
    if width == 0 || app.surface != Surface::Main || app.is_slash_palette_active() {
        return None;
    }
    if app.browser_use_cloud_key_ready().unwrap_or(true) {
        return None;
    }

    let wrap_width = (width as usize).saturating_sub(8).clamp(16, 64);
    let words = [
        ("Use", text_style()),
        ("a", text_style()),
        ("Cloud", text_style()),
        ("browser", text_style()),
        ("to", text_style()),
        ("avoid", text_style()),
        ("manual", text_style()),
        ("permissions", text_style()),
        ("and", text_style()),
        ("get", text_style()),
        ("automatic", text_style()),
        ("captcha-solving!", text_style()),
        ("[cloud.browser-use.com]", link()),
    ];
    Some(wrap_styled_words(&words, wrap_width))
}

fn wrap_styled_words(words: &[(&'static str, Style)], max_width: usize) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut current_width = 0usize;

    for (word, style) in words {
        let word_width = word.chars().count();
        let separator_width = usize::from(current_width > 0);
        if current_width > 0 && current_width + separator_width + word_width > max_width {
            lines.push(Line::from(std::mem::take(&mut spans)));
            current_width = 0;
        }
        if current_width > 0 {
            spans.push(Span::styled(" ", text_style()));
            current_width += 1;
        }
        spans.push(Span::styled(*word, *style));
        current_width += word_width;
    }

    if !spans.is_empty() {
        lines.push(Line::from(spans));
    }
    lines
}

fn work_lines(
    state: &WorkbenchState,
    app: &App,
    width: u16,
    product_state: ProductState,
) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    out.extend(crate::welcome::session_header_lines(width));
    let notice_at_tail = status_notice_needs_tail_visibility(app, product_state);
    if let Some(notice) = app.status_notice.as_ref().filter(|_| !notice_at_tail) {
        out.push(Line::from(Span::styled(notice.clone(), muted())));
        out.push(Line::from(""));
    }
    out.extend(
        transcript::transcript_model(app, state)
            .map(|model| {
                let mut lines = transcript::all_scrollback_lines(&model, width);
                if matches!(product_state, ProductState::Running) {
                    let active = transcript::active_viewport_lines(Some(&model), width, u16::MAX);
                    if !active.is_empty() {
                        if !lines.is_empty() {
                            for _ in 0..transcript::gap_before_active(&model) {
                                lines.push(Line::from(""));
                            }
                        }
                        lines.extend(active);
                    }
                }
                lines
            })
            .unwrap_or_default(),
    );
    if let Some(notice) = app.status_notice.as_ref().filter(|_| notice_at_tail) {
        if !out.is_empty() {
            out.push(Line::from(""));
        }
        out.push(Line::from(Span::styled(notice.clone(), muted())));
    }
    if out.is_empty() {
        append_task_section(&mut out, state);
    }
    if let Some(next) = next_action_lines(state, app, product_state) {
        out.push(Line::from(""));
        out.extend(next);
    }
    out
}

fn status_notice_needs_tail_visibility(app: &App, product_state: ProductState) -> bool {
    matches!(product_state, ProductState::Running)
        && matches!(
            app.status_notice.as_deref(),
            Some("Resumed previous session after reload.")
        )
}

fn next_action_lines(
    state: &WorkbenchState,
    app: &App,
    product_state: ProductState,
) -> Option<Vec<Line<'static>>> {
    let actions: Vec<&str> = match product_state {
        ProductState::Failed => {
            let error = state.failure.as_deref().unwrap_or("");
            let (primary, secondary) = failure_actions(error);
            vec![primary, secondary, "Retry", "New task"]
        }
        ProductState::Cancelled if app.selected_session_is_paused() => return None,
        ProductState::Cancelled => vec![
            "Continue with a follow-up",
            "Start a new task",
            "Previous work",
        ],
        _ => return None,
    };
    let effective_selection = if app.is_slash_palette_active() {
        usize::MAX
    } else {
        app.selected_row
    };
    let mut out = vec![event_marker_line("next")];
    for (idx, label) in actions.iter().enumerate() {
        out.push(prefix_block_line(
            "  ",
            selected(label, idx, effective_selection),
        ));
    }
    Some(out)
}

fn browser_panel_lines(app: &App, state: &WorkbenchState) -> Vec<Line<'static>> {
    let mut lines = vec![
        Line::from(Span::styled("Current browser", bold())),
        Line::from(""),
        kv_line("backend", &app.browser),
        kv_line("status", &state.browser.status),
        kv_line("title", state.browser.title.as_deref().unwrap_or("unknown")),
        kv_line(
            "page",
            state.browser.url.as_deref().unwrap_or("no page yet"),
        ),
        kv_line(
            "live view",
            state.browser.live_url.as_deref().unwrap_or("not available"),
        ),
        kv_line(
            "tabs",
            &state
                .browser
                .tabs
                .map(|tabs| format!("{tabs} open"))
                .unwrap_or_else(|| "unknown".to_string()),
        ),
        kv_line(
            "viewport",
            state.browser.viewport.as_deref().unwrap_or("unknown"),
        ),
    ];
    if let Some(issue) = latest_browser_issue(app, state) {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled("Last issue", bold())));
        lines.push(kv_line("issue", &truncate(&issue.summary, 86)));
        if let Some(next_step) = issue.next_step.as_ref() {
            lines.push(kv_line("next", &truncate(next_step, 86)));
        }
    }
    lines.extend([
        Line::from(""),
        selected("Open live browser", 0, app.selected_row),
        selected("Reconnect", 1, app.selected_row),
        selected("Change browser", 2, app.selected_row),
    ]);
    if let Some(notice) = app.browser_notice.as_ref() {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(notice.clone(), muted())));
    }
    lines
}

#[derive(Debug)]
struct BrowserIssueDisplay {
    summary: String,
    next_step: Option<String>,
}

fn latest_browser_issue(app: &App, state: &WorkbenchState) -> Option<BrowserIssueDisplay> {
    let session = state.current_session.as_ref()?;
    app.cached_events_for_session(&session.id)
        .iter()
        .rev()
        .find_map(|event| {
            event
                .payload
                .get("diagnosis")
                .or_else(|| event.payload.get("last_issue"))
                .and_then(browser_issue_display_from_value)
        })
}

fn browser_issue_display_from_value(value: &serde_json::Value) -> Option<BrowserIssueDisplay> {
    let summary = value
        .get("summary")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())?
        .to_string();
    let next_step = value
        .get("next_step")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned);
    Some(BrowserIssueDisplay { summary, next_step })
}

fn history_lines(app: &App, state: &WorkbenchState, width: usize) -> Vec<Line<'static>> {
    let mut lines: Vec<Line<'static>> = Vec::new();
    let filter = app.history_filter();
    // When the user is actively typing a filter, show it in a small input line
    // above the rows. Hidden until the first keystroke so the resting view of
    // the popup stays identical to before.
    if !filter.is_empty() {
        lines.push(Line::from(vec![
            Span::styled("  search: ", muted()),
            Span::styled(filter.to_string(), text_style()),
        ]));
        lines.push(Line::from(""));
    }
    if state.history.is_empty() {
        lines.push(Line::from(Span::styled("No previous work yet.", dim())));
        return lines;
    }
    let needle = filter.trim().to_ascii_lowercase();
    let visible: Vec<&HistoryRow> = if needle.is_empty() {
        state.history.iter().collect()
    } else {
        state
            .history
            .iter()
            .filter(|row| row.task.to_ascii_lowercase().contains(&needle))
            .collect()
    };
    if visible.is_empty() {
        lines.push(Line::from(Span::styled("No matching tasks.", dim())));
        return lines;
    }
    for (idx, row) in visible.iter().enumerate() {
        lines.push(history_overlay_line(row, idx, app.selected_row, width));
    }
    lines
}

fn message_lines(app: &App, width: usize) -> Vec<Line<'static>> {
    let rows = app.message_action_rows();
    if rows.is_empty() {
        return vec![Line::from(Span::styled(
            "No submitted messages yet.",
            dim(),
        ))];
    }
    rows.into_iter()
        .enumerate()
        .map(|(idx, row)| {
            let kind = match row.kind {
                MessageActionKind::Submitted if row.followup => "sent",
                MessageActionKind::Submitted => "start",
                MessageActionKind::Queued => "queued",
            };
            let content = vec![
                Span::styled(format!("{kind:<8}"), muted()),
                Span::styled(truncate(&row.text, width.saturating_sub(12)), text_style()),
            ];
            highlight_selectable_row(content, idx == app.selected_row, width)
        })
        .collect()
}

fn developer_lines(app: &App, state: &WorkbenchState) -> Vec<Line<'static>> {
    let mut lines = vec![
        Line::from(Span::styled("Laminar", bold())),
        Line::from(""),
        kv_line(
            "status",
            &app.laminar_status()
                .unwrap_or_else(|_| "settings unavailable".to_string()),
        ),
    ];
    if let Some(notice) = app.status_notice.as_ref() {
        lines.push(Line::from(Span::styled(notice.clone(), muted())));
    }
    lines.push(selected("Configure Laminar", 0, app.selected_row));
    lines.extend([
        Line::from(""),
        Line::from(Span::styled("Current task", bold())),
        Line::from(""),
    ]);
    let Some(session) = state.current_session.as_ref() else {
        lines.push(Line::from(Span::styled("No task selected.", dim())));
        return lines;
    };
    let events = app.cached_events_for_session(&session.id);
    let instruction_sources = instruction_sources_from_events(events);
    if !instruction_sources.is_empty() {
        lines.push(Line::from(Span::styled("Instruction sources", bold())));
        lines.push(Line::from(""));
        for source in instruction_sources.iter().take(8) {
            lines.push(kv_line("agents", source));
        }
        if instruction_sources.len() > 8 {
            lines.push(Line::from(Span::styled(
                format!("{} more source(s)", instruction_sources.len() - 8),
                dim(),
            )));
        }
        lines.push(Line::from(""));
    }
    let startup_warnings = startup_warnings_from_events(events);
    if !startup_warnings.is_empty() {
        lines.push(Line::from(Span::styled("Startup warnings", bold())));
        lines.push(Line::from(""));
        for warning in startup_warnings.iter().take(4) {
            lines.push(Line::from(Span::styled(truncate(warning, 80), muted())));
        }
        if startup_warnings.len() > 4 {
            lines.push(Line::from(Span::styled(
                format!("{} more warning(s)", startup_warnings.len() - 4),
                dim(),
            )));
        }
        lines.push(Line::from(""));
    }
    append_telemetry_detail_lines(&mut lines, &state.telemetry);
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled("Events", bold())));
    lines.push(Line::from(""));
    for event in events.iter().rev().take(12).rev() {
        let payload = truncate(&event.payload.to_string(), 44);
        lines.push(Line::from(vec![
            Span::styled(format!("{:>4}  ", event.seq), muted()),
            Span::styled(
                format!("{:<24}", truncate(&event.event_type, 24)),
                text_style(),
            ),
            Span::styled(payload, dim()),
        ]));
    }
    lines
}

fn append_task_section(lines: &mut Vec<Line<'static>>, state: &WorkbenchState) {
    lines.push(Line::from(vec![
        Span::styled("> ", accent()),
        Span::styled(
            state
                .task
                .clone()
                .unwrap_or_else(|| "browser task".to_string()),
            text_style(),
        ),
    ]));
}

fn event_marker_line(title: &str) -> Line<'static> {
    Line::from(vec![
        Span::styled("• ", event_marker_style(title)),
        Span::styled(title.to_string(), event_marker_style(title)),
    ])
}

fn event_marker_style(title: &str) -> Style {
    if title.starts_with("thought")
        || title.starts_with("thinking")
        || title.starts_with("status")
        || title.starts_with("edit")
    {
        thought()
    } else if title.starts_with("browser")
        || title == "run"
        || title == "image"
        || title == "plan"
        || title == "tool"
        || title == "python"
    {
        accent()
    } else if title.starts_with("answer")
        || title == "done"
        || title == "source"
        || title == "subagent"
        || title == "list"
        || title == "read"
        || title == "search"
    {
        done()
    } else if title == "error" || title == "stopped" {
        failed()
    } else {
        muted()
    }
}

fn prefix_block_line(prefix: &'static str, line: Line<'static>) -> Line<'static> {
    let mut spans = vec![Span::styled(prefix, dim())];
    spans.extend(line.spans);
    Line::from(spans)
}

fn kv_line(label: &str, value: &str) -> Line<'static> {
    Line::from(vec![
        Span::raw("  "),
        Span::styled(format!("{label:<10}"), muted()),
        Span::styled(value.to_string(), text_style()),
    ])
}

fn history_overlay_line(
    row: &HistoryRow,
    idx: usize,
    selected_row: usize,
    width: usize,
) -> Line<'static> {
    // Layout priority is left-to-right: the task is the leftmost and most
    // important column, then status, then the relative timestamp. When the
    // terminal gets squished we drop time first, then status, so the task
    // stays visible instead of being squeezed to zero. Each row must render
    // as exactly one visual line — wrapping would throw off the History pane's
    // scroll math, which counts data rows.
    const INDENT: usize = 2;
    const STATUS_COL_W: usize = 10;
    const TASK_FLOOR: usize = 6;
    let time_str = relative_time(row.updated_ms);
    let time_w = time_str.chars().count();
    let full_task_w = width.saturating_sub(INDENT + STATUS_COL_W + time_w);
    let no_time_task_w = width.saturating_sub(INDENT + STATUS_COL_W);
    let task_only_w = width.saturating_sub(INDENT);
    let (task_w, show_status, show_time) = if full_task_w >= TASK_FLOOR {
        (full_task_w, true, true)
    } else if no_time_task_w >= TASK_FLOOR {
        (no_time_task_w, true, false)
    } else {
        (task_only_w, false, false)
    };
    let mut content = vec![Span::styled(
        format!("{:<task_w$}", truncate(&row.task, task_w)),
        text_style(),
    )];
    if show_status {
        content.push(Span::styled(
            format!("{:<STATUS_COL_W$}", row.status.as_str()),
            status_style(row.status.as_str()),
        ));
    }
    if show_time {
        content.push(Span::styled(time_str, muted()));
    }
    highlight_selectable_row(content, idx == selected_row, width)
}

/// The single source of truth for selectable-row styling: a 2-space indent and,
/// when selected, a full-width background highlight. Shared by the slash palette
/// and the history list so selection looks identical everywhere.
fn highlight_selectable_row(
    content: Vec<Span<'static>>,
    is_selected: bool,
    width: usize,
) -> Line<'static> {
    let mut spans = vec![Span::raw("  ")];
    spans.extend(content);
    let mut line = Line::from(spans);
    if is_selected {
        let used: usize = line
            .spans
            .iter()
            .map(|span| span.content.chars().count())
            .sum();
        if used < width {
            line.spans.push(Span::raw(" ".repeat(width - used)));
        }
        line = line.style(selection());
    }
    line
}

fn selected(text: &str, idx: usize, selected: usize) -> Line<'static> {
    selectable_row(text, idx, selected, false)
}

/// A selectable list row. The cursor (the row being navigated) gets a blue `>`
/// and bold text; the item currently *in use* gets green text so the active
/// choice is visible even when the cursor has moved elsewhere.
fn selectable_row(text: &str, idx: usize, selected: usize, is_current: bool) -> Line<'static> {
    let cursor = idx == selected;
    let body = if is_current {
        current()
    } else if cursor {
        bold()
    } else {
        text_style()
    };
    Line::from(vec![
        Span::styled(
            if cursor { "> " } else { "  " },
            if cursor { accent() } else { dim() },
        ),
        Span::styled(text.to_string(), body),
    ])
}

fn append_telemetry_detail_lines(lines: &mut Vec<Line<'static>>, telemetry: &TelemetrySummary) {
    if telemetry.trace_id.is_none() && telemetry.failure.is_none() {
        lines.push(Line::from(Span::styled(
            "No Laminar event for this task.",
            dim(),
        )));
        return;
    }
    if let Some(trace_id) = telemetry.trace_id.as_ref() {
        lines.push(kv_line("trace", trace_id));
    }
    if let Some(backend) = telemetry.backend.as_ref() {
        lines.push(kv_line("backend", backend));
    }
    if let Some(endpoint) = telemetry.endpoint.as_ref() {
        lines.push(kv_line("endpoint", endpoint));
    }
    if let Some(error) = telemetry.failure.as_ref() {
        lines.push(kv_line(
            "status",
            &format!("disabled: {}", truncate(&first_line(error), 120)),
        ));
    }
}

fn masked_secret(value: &str) -> String {
    if value.is_empty() {
        "paste key here".to_string()
    } else {
        let count = value.chars().count();
        let visible = count.min(8);
        let hidden = count.saturating_sub(8);
        let prefix: String = value.chars().take(visible).collect();
        format!("{prefix}{}", "*".repeat(hidden))
    }
}

fn masked_secret_for_account(account: &str, value: &str) -> String {
    if value.is_empty() && is_claude_code_account(account) {
        "optional legacy access token".to_string()
    } else {
        masked_secret(value)
    }
}

fn help_line(text: &str) -> Line<'static> {
    Line::from(Span::styled(text.to_string(), muted()))
}

/// A "label  example  trailing" instruction row. None of these start with two
/// spaces, so the only `"  …"` line stays the input field (where the caret goes).
fn secret_field_label(field: SecretField) -> &'static str {
    match field {
        SecretField::Domain => "Domain",
        SecretField::Name => "Name",
        SecretField::Value => "Value",
    }
}

/// The rendered content of a form field WITHOUT the leading two-space indent:
/// `"Label   <value>"` (value masked for the Value field). The focused field
/// uses the live composer text; the others use their parked text.
fn secret_field_content(app: &App, field: SecretField) -> String {
    let form = app.secret_form.as_ref();
    let focused = form.is_some_and(|f| f.focused_field() == Some(field));
    let raw = if focused {
        app.composer.input().to_string()
    } else {
        form.map(|f| f.field(field).to_string()).unwrap_or_default()
    };
    let display = if matches!(field, SecretField::Value) {
        "•".repeat(raw.chars().count())
    } else {
        raw
    };
    format!("{:<8}{display}", secret_field_label(field))
}

/// The focused field's content — the string `render_surface_popup_box` uses to
/// place the caret (it looks for the line starting with `"  " + this`). Returns a
/// non-matching sentinel while a saved row is selected, so no caret is shown.
pub(crate) fn secrets_input_field(app: &App) -> String {
    match app.secret_form.as_ref().map(|form| form.focus) {
        Some(SecretFocus::Field(field)) => secret_field_content(app, field),
        _ => "\u{0}".to_string(),
    }
}

/// Braille spinner frames — same visual family as the BU logo.
const IMPORT_SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

fn plural(n: usize) -> &'static str {
    if n == 1 {
        ""
    } else {
        "s"
    }
}

/// Render the password-import banner (animated while running, result on finish).
fn import_banner_lines(import: &crate::SecretImport) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    match &import.outcome {
        None => {
            let ms = import.started.elapsed().as_millis();
            let frame = IMPORT_SPINNER[(ms / 90) as usize % IMPORT_SPINNER.len()];
            let dots = ".".repeat(1 + (ms / 350) as usize % 3);
            lines.push(Line::from(vec![
                Span::styled(format!("{frame} "), accent()),
                Span::styled(format!("Importing from {}{dots}", import.label), accent()),
                Span::styled(format!("   {}s", ms / 1000), muted()),
            ]));
        }
        Some(Ok(stats)) => {
            let message = if stats.changed_logins() == 0 {
                format!(
                    "Already up to date — {} login{} from {}, nothing new",
                    stats.unchanged_logins,
                    plural(stats.unchanged_logins),
                    import.label,
                )
            } else {
                format!(
                    "Synced {} — {} new, {} updated ({} unchanged)",
                    import.label, stats.new_logins, stats.updated_logins, stats.unchanged_logins,
                )
            };
            lines.push(Line::from(vec![
                Span::styled(
                    "✓ ".to_string(),
                    Style::default().fg(crate::theme::palette().done),
                ),
                Span::styled(message, Style::default().fg(text())),
            ]));
            if stats.skipped > 0 {
                lines.push(help_line(&format!(
                    "{} entr{} skipped (no domain or credentials).",
                    stats.skipped,
                    if stats.skipped == 1 { "y" } else { "ies" }
                )));
            }
        }
        Some(Err(message)) => {
            lines.push(Line::from(vec![
                Span::styled(
                    "✗ ".to_string(),
                    Style::default().fg(crate::theme::palette().failed),
                ),
                Span::styled(
                    format!("Import failed: {message}"),
                    Style::default().fg(crate::theme::palette().failed),
                ),
            ]));
        }
    }
    lines
}

fn secret_field_line(app: &App, field: SecretField) -> Line<'static> {
    let focused = app
        .secret_form
        .as_ref()
        .is_some_and(|f| f.focused_field() == Some(field));
    let content = secret_field_content(app, field);
    let style = if focused {
        accent()
    } else {
        Style::default().fg(text())
    };
    // Exactly one rendered line starts with two spaces — the focused field, where
    // the caret lands (its full content is the caret needle).
    Line::from(vec![Span::raw("  "), Span::styled(content, style)])
}

pub(crate) fn secrets_lines(app: &App) -> Vec<Line<'static>> {
    let mut lines = Vec::new();

    // Guidance specific to why a 1Password import can't run.
    if let Some(issue) = app.op_setup_hint {
        let url = browser_use_agent::tools::handlers::secrets_import::OP_DOWNLOAD_URL;
        match issue {
            crate::OpSetupIssue::NotInstalled => {
                lines.push(Line::from(Span::styled(
                    "1Password CLI not installed".to_string(),
                    accent(),
                )));
                lines.push(Line::from(""));
                lines.push(help_line(
                    "Importing from 1Password needs the `op` command-line tool. Install it:",
                ));
                lines.push(Line::from(vec![
                    Span::raw("  "),
                    Span::styled(
                        url.to_string(),
                        Style::default().fg(crate::theme::palette().link),
                    ),
                ]));
                lines.push(Line::from(""));
                lines.push(help_line(
                    "Then sign in with `op signin` and run /import-passwords again.    Esc: back",
                ));
            }
            crate::OpSetupIssue::NotSignedIn => {
                lines.push(Line::from(Span::styled(
                    "1Password CLI not signed in".to_string(),
                    accent(),
                )));
                lines.push(Line::from(""));
                lines.push(help_line(
                    "`op` is installed but can't read your vault non-interactively. Enable one:",
                ));
                lines.push(Line::from(vec![
                    Span::styled("  • ".to_string(), muted()),
                    Span::styled(
                        "1Password app → Settings → Developer → Integrate with 1Password CLI"
                            .to_string(),
                        Style::default().fg(text()),
                    ),
                ]));
                lines.push(Line::from(vec![
                    Span::styled("  • or set ".to_string(), muted()),
                    Span::styled(
                        "OP_SERVICE_ACCOUNT_TOKEN".to_string(),
                        Style::default().fg(text()),
                    ),
                    Span::styled(" before launching".to_string(), muted()),
                ]));
                lines.push(Line::from(""));
                lines.push(help_line(
                    "Verify with `op item list`, then run /import-passwords again.    Esc: back",
                ));
            }
        }
        return lines;
    }

    lines.push(help_line(
        "Logins the agent can use. Stored in an encrypted file; masked from the agent.",
    ));

    // Animated import banner (running) or its result.
    if let Some(import) = &app.secret_import {
        lines.push(Line::from(""));
        lines.extend(import_banner_lines(import));
    }
    lines.push(Line::from(""));

    let header = if app.secrets_list.is_empty() {
        "Saved secrets:".to_string()
    } else {
        format!("Saved secrets ({}):", app.secrets_list.len())
    };
    lines.push(Line::from(Span::styled(header, accent())));

    // Search box once the list is long enough (or a query is active).
    if app.secrets_search_active() {
        let focused = matches!(
            app.secret_form.as_ref().map(|f| f.focus),
            Some(SecretFocus::Search)
        );
        let label_style = if focused { accent() } else { muted() };
        let mut spans = vec![Span::styled("Search: ".to_string(), label_style)];
        if app.secrets_search.is_empty() && !focused {
            spans.push(Span::styled("(type to filter)".to_string(), muted()));
        } else {
            spans.push(Span::styled(
                app.secrets_search.clone(),
                Style::default().fg(text()),
            ));
            if focused {
                spans.push(Span::styled("▌".to_string(), accent()));
            }
        }
        lines.push(Line::from(spans));
    }

    let view = app.secrets_view();
    if app.secrets_list.is_empty() {
        lines.push(help_line("(none yet)"));
    } else if view.is_empty() {
        lines.push(help_line("(no matches)"));
    } else {
        // Window the list, keeping the highlighted row in view.
        let visible = crate::SECRETS_VISIBLE_ROWS;
        let total = view.len();
        let selected_idx = match app.secret_form.as_ref().map(|f| f.focus) {
            Some(SecretFocus::Saved(i)) => Some(i),
            _ => None,
        };
        let start = match selected_idx {
            Some(i) if i >= visible => i + 1 - visible,
            _ => 0,
        };
        let end = (start + visible).min(total);

        if start > 0 {
            lines.push(help_line(&format!("  ⋯ {} more above", start)));
        }
        for (offset, meta) in view[start..end].iter().enumerate() {
            let idx = start + offset;
            let selected = selected_idx == Some(idx);
            let extra = if meta.allowed_domains.is_empty() {
                String::new()
            } else {
                format!("  also: {}", meta.allowed_domains.join(", "))
            };
            // Markers avoid a two-space prefix so they can't collide with the
            // focused-field caret.
            let (marker, domain_style) = if selected {
                ("▶ ", accent())
            } else {
                ("· ", Style::default().fg(text()))
            };
            // Fixed-length dots (don't reveal length); "(2FA)" tags TOTP secrets.
            let value_label = if meta.kind.as_str() == "totp" {
                "•••••• (2FA)"
            } else {
                "••••••"
            };
            lines.push(Line::from(vec![
                Span::styled(format!("{marker}{:<22}", meta.domain), domain_style),
                Span::raw("  "),
                Span::styled(format!("{:<12}", meta.placeholder), accent()),
                Span::raw("  "),
                Span::styled(format!("{value_label}{extra}"), muted()),
            ]));
        }
        if end < total {
            lines.push(help_line(&format!("  ⋯ {} more below", total - end)));
        }
    }
    lines.push(Line::from(""));

    // The add form: three separate labeled fields (only the focused one is
    // double-indented, so the caret lands there).
    lines.push(Line::from(Span::styled(
        "Add a secret:".to_string(),
        accent(),
    )));
    lines.push(secret_field_line(app, SecretField::Domain));
    lines.push(secret_field_line(app, SecretField::Name));
    lines.push(secret_field_line(app, SecretField::Value));
    lines.push(Line::from(""));
    let move_hint = if app.secrets_search_active() {
        "↑/↓ Tab: move   type: search   Enter: edit/save   Del: remove   Esc: close"
    } else {
        "↑/↓ Tab: move   Enter: edit row / save form   Del: remove   Esc: close"
    };
    lines.push(help_line(move_hint));
    lines.push(help_line(
        "Ctrl-O: import logins from 1Password (or run /import-passwords)",
    ));
    lines.push(help_line(
        "Tip: name it \"otp\" for a 2FA code (paste the authenticator setup key).",
    ));
    if let Some(notice) = app.status_notice.as_deref() {
        lines.push(Line::from(Span::styled(notice.to_string(), accent())));
    }
    lines
}

/// Caret target for the `/domains` panel: the domain input line (label + text)
/// only when focused, so the cursor lands at the end of the typed domain.
pub(crate) fn domains_input_field(app: &App) -> String {
    if matches!(
        app.domain_form.as_ref().map(|form| form.focus),
        Some(DomainFocus::Input)
    ) {
        format!("{:<8}{}", "Domain", app.composer.input())
    } else {
        "\u{0}".to_string()
    }
}

pub(crate) fn domains_lines(app: &App) -> Vec<Line<'static>> {
    let mut lines = Vec::new();

    lines.push(help_line(
        "The agent may visit ONLY allowed sites (+ subdomains, + any site you've",
    ));
    lines.push(help_line(
        "saved a secret for). Empty allowed = every site. Blocked always takes precedence.",
    ));
    lines.push(Line::from(""));

    let rows = app.domain_rows();
    let header = if rows.is_empty() {
        "Rules:".to_string()
    } else {
        format!("Rules ({}):", rows.len())
    };
    lines.push(Line::from(Span::styled(header, accent())));
    if rows.is_empty() {
        lines.push(help_line("(none — every site allowed)"));
    } else {
        // Window the list, keeping the highlighted row in view.
        let visible = crate::SECRETS_VISIBLE_ROWS;
        let selected_idx = match app.domain_form.as_ref().map(|f| f.focus) {
            Some(DomainFocus::Saved(i)) => Some(i),
            _ => None,
        };
        let start = match selected_idx {
            Some(i) if i >= visible => i + 1 - visible,
            _ => 0,
        };
        let end = (start + visible).min(rows.len());
        if start > 0 {
            lines.push(help_line(&format!("  ⋯ {} more above", start)));
        }
        for (offset, (domain, is_allow)) in rows[start..end].iter().enumerate() {
            let idx = start + offset;
            let selected = selected_idx == Some(idx);
            let (marker, domain_style) = if selected {
                ("▶ ", accent())
            } else {
                ("· ", Style::default().fg(text()))
            };
            let (tag, tag_style) = if *is_allow {
                ("Allowed", Style::default().fg(crate::theme::palette().done))
            } else {
                (
                    "Blocked",
                    Style::default().fg(crate::theme::palette().failed),
                )
            };
            lines.push(Line::from(vec![
                Span::styled(format!("{marker}{:<28}", domain), domain_style),
                Span::raw("  "),
                Span::styled(tag.to_string(), tag_style),
            ]));
        }
        if end < rows.len() {
            lines.push(help_line(&format!("  ⋯ {} more below", rows.len() - end)));
        }
    }
    lines.push(Line::from(""));

    // Add-rule form: a Mode toggle + a Domain input (mirrors the secrets form).
    lines.push(Line::from(Span::styled(
        "Add a rule:".to_string(),
        accent(),
    )));
    // Domain input first (carries the caret when focused — matches
    // `domains_input_field`, so the cursor lands at the end of the text), then the
    // Mode toggle below it.
    let input_focused = matches!(
        app.domain_form.as_ref().map(|f| f.focus),
        Some(DomainFocus::Input)
    );
    let input_text = if input_focused {
        app.composer.input().to_string()
    } else {
        app.domain_form
            .as_ref()
            .map(|f| f.input.clone())
            .unwrap_or_default()
    };
    lines.push(Line::from(vec![
        Span::raw("  "),
        Span::styled(
            format!("{:<8}{}", "Domain", input_text),
            if input_focused {
                accent()
            } else {
                Style::default().fg(text())
            },
        ),
    ]));
    let mode_focused = matches!(
        app.domain_form.as_ref().map(|f| f.focus),
        Some(DomainFocus::Mode)
    );
    let allow_mode = matches!(
        app.domain_form.as_ref().map(|f| f.mode),
        Some(DomainMode::Allow)
    );
    let value = if allow_mode { "Allow" } else { "Block" };
    // Only bracket the value (and show the hint) when the Mode field is focused,
    // so the ‹ › reads as "you're here, use ←/→" rather than a stuck cursor.
    let mode_text = if mode_focused {
        format!("‹ {value} ›")
    } else {
        value.to_string()
    };
    let mut mode_spans = vec![
        Span::raw("  "),
        Span::styled(
            format!("{:<8}{}", "Mode", mode_text),
            if mode_focused {
                accent()
            } else {
                Style::default().fg(text())
            },
        ),
    ];
    if mode_focused {
        mode_spans.push(Span::styled("   (←/→ to switch)".to_string(), muted()));
    }
    lines.push(Line::from(mode_spans));
    lines.push(Line::from(""));
    lines.push(help_line(
        "↑/↓ Tab: move   ←/→: Allow/Block   Enter: add / toggle   Del: remove   Esc: close",
    ));
    if let Some(notice) = app.status_notice.as_deref() {
        lines.push(Line::from(Span::styled(notice.to_string(), accent())));
    }
    lines
}

/// The masked AgentMail key as it appears in the input field: dots (like the
/// `/secrets` value field), keyed by a `Key` label so the caret lands on it.
pub(crate) fn email_input_field(app: &App) -> String {
    let dots = "•".repeat(app.composer.input().chars().count());
    format!("{:<8}{dots}", "Key")
}

pub(crate) fn email_lines(app: &App) -> Vec<Line<'static>> {
    let mut lines = vec![Line::from(Span::styled("Disposable email inbox", bold()))];
    lines.push(Line::from(""));

    // Status / what it does — prose uses help_line (no leading spaces) so the
    // only "  …" line is the input field, where the caret goes.
    if app.email_configured {
        lines.push(Line::from(vec![
            Span::styled("✓ ", Style::default().fg(crate::theme::palette().done)),
            Span::styled(
                "Configured — the agent has its own inbox.",
                Style::default().fg(text()),
            ),
        ]));
        lines.push(help_line(
            "It can be the agent's email for any sign-up or service, and the agent",
        ));
        lines.push(help_line(
            "reads the verification / 2FA codes that arrive — no checking your email.",
        ));
        lines.push(Line::from(""));
        lines.push(help_line("Paste a new key below to replace it."));
    } else {
        lines.push(help_line(
            "Give the agent its own inbox to use as its email for sign-ups and any",
        ));
        lines.push(help_line(
            "service, and to read verification / 2FA codes itself — not just 2FA.",
        ));
        lines.push(Line::from(""));
        lines.push(help_line(
            "Get a free API key at https://agentmail.to, then paste it below.",
        ));
    }
    lines.push(Line::from(""));

    // Labeled, masked input field — styled like a /secrets field (accent + caret).
    lines.push(Line::from(vec![
        Span::raw("  "),
        Span::styled(email_input_field(app), accent()),
    ]));
    lines.push(Line::from(""));
    lines.push(help_line(
        "Stored locally & encrypted. Codes the agent reads are redacted from output.",
    ));
    lines.push(Line::from(""));

    if let Some(notice) = app.status_notice.as_ref() {
        lines.push(Line::from(Span::styled(
            notice.clone(),
            status_style("failed"),
        )));
        lines.push(Line::from(""));
    }
    lines.push(selected("Save key", 0, app.selected_row));
    lines.push(selected("Cancel", 1, app.selected_row));
    lines
}

fn auth_secret_label(account: &str) -> &'static str {
    match account {
        ACCOUNT_OPENAI => "OpenAI API key",
        ACCOUNT_OPENROUTER => "OpenRouter API key",
        ACCOUNT_DEEPSEEK => "DeepSeek API key",
        ACCOUNT_ANTHROPIC => "Anthropic API key",
        BROWSER_USE_CLOUD => "Browser Use Cloud key",
        account if is_claude_code_account(account) => "Claude Code OAuth token",
        _ => "Credential",
    }
}

fn failure_actions(error: &str) -> (&'static str, &'static str) {
    let lower = error.to_ascii_lowercase();
    if lower.contains("openrouter") {
        ("Authenticate with OpenRouter", "Choose a different model")
    } else if lower.contains("deepseek") {
        ("Authenticate with DeepSeek", "Choose a different model")
    } else if lower.contains("openai") {
        ("Authenticate with OpenAI", "Choose a different model")
    } else if lower.contains("anthropic") || lower.contains("claude") {
        ("Authenticate", "Choose a different model")
    } else if lower.contains("browser") || lower.contains("chrome") {
        ("Open browser settings", "Choose a different browser")
    } else {
        ("Retry", "Choose a different model")
    }
}

fn relative_time(ms: i64) -> String {
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(ms);
    let elapsed = now_ms.saturating_sub(ms);
    let seconds = elapsed / 1000;
    if seconds < 60 {
        return "recent".to_string();
    }
    let minutes = seconds / 60;
    if minutes < 60 {
        return format!("{minutes}m ago");
    }
    let hours = minutes / 60;
    if hours < 24 {
        return format!("{hours}h ago");
    }
    let days = hours / 24;
    if days == 1 {
        "yesterday".to_string()
    } else {
        format!("{days}d ago")
    }
}

fn truncate(value: &str, max: usize) -> String {
    if value.chars().count() <= max {
        return value.to_string();
    }
    if max <= 3 {
        return value.chars().take(max).collect();
    }
    let mut out = value.chars().take(max - 3).collect::<String>();
    out.push_str("...");
    out
}

fn feedback_lines(app: &App) -> Vec<Line<'static>> {
    let state = &app.feedback;
    match state.step {
        FeedbackStep::Category => {
            let mut lines: Vec<Line<'static>> = Vec::new();
            lines.push(Line::from(Span::styled(
                "  Choose a category:",
                text_style(),
            )));
            lines.push(Line::from(""));
            for (i, cat) in FeedbackCategory::ALL.iter().enumerate() {
                let number = format!("{}. ", i + 1);
                let label = cat.label();
                let desc = cat.description();
                let is_selected = i == state.category_index;
                let row_style = if is_selected { accent() } else { text_style() };
                let prefix_style = if is_selected { accent() } else { muted() };
                lines.push(Line::from(vec![
                    Span::styled(format!("  {number}"), prefix_style),
                    Span::styled(label.to_string(), row_style),
                    Span::styled(format!(" — {desc}"), muted()),
                ]));
            }
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                "  1-5 or ↑↓ to select, Enter to continue",
                dim(),
            )));
            lines
        }
        FeedbackStep::Description => {
            let category = FeedbackCategory::ALL
                .get(state.category_index)
                .copied()
                .unwrap_or(FeedbackCategory::Other);
            let label = format!("  Tell us more ({})", category.label());
            let mut lines: Vec<Line<'static>> = Vec::new();
            lines.push(Line::from(Span::styled(label, text_style())));
            lines.push(Line::from(""));
            let input = state.description.clone();
            let display = if input.is_empty() {
                Line::from(Span::styled("  (optional)", dim()))
            } else {
                Line::from(vec![
                    Span::raw("  "),
                    Span::styled(input, text_style()),
                    Span::styled("█", accent()),
                ])
            };
            lines.push(display);
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                "  Type your message, Enter to continue, Esc to go back",
                dim(),
            )));
            lines
        }
        FeedbackStep::UploadLogs => {
            let mut lines: Vec<Line<'static>> = Vec::new();
            lines.push(Line::from(Span::styled("  Upload logs?", text_style())));
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                "  Shares this session's full transcript and tool activity",
                muted(),
            )));
            lines.push(Line::from(Span::styled(
                "  (plus app version, OS, model). It may contain page or file",
                muted(),
            )));
            lines.push(Line::from(Span::styled(
                "  contents \u{2014} skip if anything here is sensitive.",
                muted(),
            )));
            lines.push(Line::from(""));
            let (yes_style, no_style) = if state.upload_yes {
                (accent(), muted())
            } else {
                (muted(), accent())
            };
            lines.push(Line::from(vec![
                Span::raw("  "),
                Span::styled("[ Yes ]", yes_style),
                Span::raw("   "),
                Span::styled("[ No ]", no_style),
            ]));
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                "  ↑↓ or y/n to choose, Enter to submit, Esc to go back",
                dim(),
            )));
            lines
        }
    }
}

fn first_line(value: &str) -> String {
    value.lines().next().unwrap_or(value).to_string()
}
