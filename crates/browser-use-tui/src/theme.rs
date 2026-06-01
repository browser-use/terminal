use std::sync::OnceLock;

use ratatui::style::{Color, Modifier, Style};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Variant {
    Dark,
    Light,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct Palette {
    pub variant: Variant,
    pub text: Color,
    pub muted: Color,
    pub dim: Color,
    pub accent: Color,
    pub link: Color,
    pub path_reference: Color,
    pub code: Color,
    pub code_background: Color,
    pub code_block_fg: Color,
    pub heading: Color,
    pub quote: Color,
    pub border: Color,
    pub done: Color,
    pub running: Color,
    pub failed: Color,
    pub thought: Color,
    pub user_prompt_background: Color,
    pub activity_group: Color,
    pub activity_read: Color,
    pub activity_run: Color,
    pub activity_list: Color,
    pub activity_search: Color,
    pub activity_task: Color,
    pub selection_background: Color,
}

impl Palette {
    /// Catppuccin Mocha — dark
    pub(crate) const fn mocha() -> Self {
        Self {
            variant: Variant::Dark,
            text: Color::Rgb(205, 214, 244),
            muted: Color::Rgb(166, 173, 200),
            dim: Color::Rgb(108, 112, 134),
            accent: Color::Rgb(137, 180, 250),
            link: Color::Rgb(137, 220, 235),
            path_reference: Color::Rgb(250, 179, 135),
            code: Color::Rgb(180, 190, 254),
            code_background: Color::Rgb(49, 50, 68),
            code_block_fg: Color::Rgb(186, 194, 222),
            heading: Color::Rgb(250, 179, 135),
            quote: Color::Rgb(147, 153, 178),
            border: Color::Rgb(69, 71, 90),
            done: Color::Rgb(166, 227, 161),
            running: Color::Rgb(250, 179, 135),
            failed: Color::Rgb(243, 139, 168),
            thought: Color::Rgb(203, 166, 247),
            user_prompt_background: Color::Rgb(49, 50, 68),
            activity_group: Color::Rgb(166, 227, 161),
            activity_read: Color::Rgb(137, 180, 250),
            activity_run: Color::Rgb(250, 179, 135),
            activity_list: Color::Rgb(148, 226, 213),
            activity_search: Color::Rgb(249, 226, 175),
            activity_task: Color::Rgb(180, 190, 254),
            selection_background: Color::Rgb(45, 52, 66),
        }
    }

    /// Catppuccin Latte — light
    pub(crate) const fn latte() -> Self {
        Self {
            variant: Variant::Light,
            text: Color::Rgb(76, 79, 105),
            muted: Color::Rgb(108, 111, 133),
            dim: Color::Rgb(156, 160, 176),
            accent: Color::Rgb(30, 102, 245),
            link: Color::Rgb(4, 165, 229),
            path_reference: Color::Rgb(254, 100, 11),
            code: Color::Rgb(114, 135, 253),
            code_background: Color::Rgb(230, 233, 239),
            code_block_fg: Color::Rgb(92, 95, 119),
            heading: Color::Rgb(254, 100, 11),
            quote: Color::Rgb(140, 143, 161),
            border: Color::Rgb(204, 208, 218),
            done: Color::Rgb(64, 160, 43),
            running: Color::Rgb(254, 100, 11),
            failed: Color::Rgb(210, 15, 57),
            thought: Color::Rgb(136, 57, 239),
            user_prompt_background: Color::Rgb(230, 233, 239),
            activity_group: Color::Rgb(64, 160, 43),
            activity_read: Color::Rgb(30, 102, 245),
            activity_run: Color::Rgb(254, 100, 11),
            activity_list: Color::Rgb(23, 146, 153),
            activity_search: Color::Rgb(223, 142, 29),
            activity_task: Color::Rgb(114, 135, 253),
            selection_background: Color::Rgb(220, 224, 232),
        }
    }
}

static ACTIVE: OnceLock<Palette> = OnceLock::new();

/// Install the active palette. First call wins — later calls are ignored.
pub(crate) fn init(palette: Palette) {
    let _ = ACTIVE.set(palette);
}

/// Pick a palette based on `BUT_THEME` (`light`/`dark`/`auto`, default `auto`)
/// and a terminal-background probe. Falls back to dark on any failure.
pub(crate) fn detect_palette() -> Palette {
    match std::env::var("BUT_THEME").ok().as_deref() {
        Some("light") | Some("LIGHT") => return Palette::latte(),
        Some("dark") | Some("DARK") => return Palette::mocha(),
        _ => {}
    }
    match terminal_colorsaurus::color_scheme(terminal_colorsaurus::QueryOptions::default()) {
        Ok(terminal_colorsaurus::ColorScheme::Light) => Palette::latte(),
        Ok(terminal_colorsaurus::ColorScheme::Dark) => Palette::mocha(),
        Err(_) => Palette::mocha(),
    }
}

pub(crate) fn palette() -> &'static Palette {
    ACTIVE.get_or_init(Palette::mocha)
}

pub(crate) fn variant() -> Variant {
    palette().variant
}

pub(crate) fn is_light() -> bool {
    variant() == Variant::Light
}

pub(crate) fn text() -> Color {
    palette().text
}

pub(crate) fn text_style() -> Style {
    Style::default().fg(text())
}

pub(crate) fn bold() -> Style {
    text_style().add_modifier(Modifier::BOLD)
}

pub(crate) fn muted() -> Style {
    Style::default().fg(palette().muted)
}

pub(crate) fn dim() -> Style {
    Style::default().fg(palette().dim)
}

pub(crate) fn accent() -> Style {
    Style::default()
        .fg(palette().accent)
        .add_modifier(Modifier::BOLD)
}

pub(crate) fn border() -> Style {
    Style::default().fg(palette().border)
}

/// Background fill for a user prompt block in the transcript, so the message
/// the user sent stands apart from the agent's replies.
pub(crate) fn user_prompt_text() -> Style {
    text_style().bg(palette().user_prompt_background)
}

pub(crate) fn user_prompt_muted() -> Style {
    muted().bg(palette().user_prompt_background)
}

/// The accent-colored `>` prefix on a user prompt, sharing the prompt's
/// highlight background.
pub(crate) fn user_prompt_accent() -> Style {
    accent().bg(palette().user_prompt_background)
}

pub(crate) fn link() -> Style {
    Style::default()
        .fg(palette().link)
        .add_modifier(Modifier::UNDERLINED)
}

pub(crate) fn path_reference() -> Style {
    Style::default().fg(palette().path_reference)
}

pub(crate) fn markdown_code() -> Style {
    Style::default()
        .fg(palette().code)
        .bg(palette().code_background)
}

pub(crate) fn markdown_code_block() -> Style {
    Style::default().fg(palette().code_block_fg)
}

pub(crate) fn markdown_emphasis() -> Style {
    muted().add_modifier(Modifier::ITALIC)
}

pub(crate) fn markdown_strong() -> Style {
    bold()
}

pub(crate) fn markdown_marker() -> Style {
    muted()
}

pub(crate) fn markdown_quote() -> Style {
    Style::default()
        .fg(palette().quote)
        .add_modifier(Modifier::ITALIC)
}

pub(crate) fn markdown_heading() -> Style {
    Style::default()
        .fg(palette().heading)
        .add_modifier(Modifier::BOLD)
}

pub(crate) fn done() -> Style {
    Style::default().fg(palette().done)
}

pub(crate) fn running() -> Style {
    Style::default().fg(palette().running)
}

pub(crate) fn failed() -> Style {
    Style::default().fg(palette().failed)
}

pub(crate) fn thought() -> Style {
    Style::default().fg(palette().thought)
}

pub(crate) fn activity_group() -> Style {
    Style::default()
        .fg(palette().activity_group)
        .add_modifier(Modifier::BOLD)
}

pub(crate) fn activity_read() -> Style {
    Style::default()
        .fg(palette().activity_read)
        .add_modifier(Modifier::BOLD)
}

pub(crate) fn activity_run() -> Style {
    Style::default()
        .fg(palette().activity_run)
        .add_modifier(Modifier::BOLD)
}

pub(crate) fn activity_list() -> Style {
    Style::default()
        .fg(palette().activity_list)
        .add_modifier(Modifier::BOLD)
}

pub(crate) fn activity_search() -> Style {
    Style::default()
        .fg(palette().activity_search)
        .add_modifier(Modifier::BOLD)
}

pub(crate) fn activity_task() -> Style {
    Style::default()
        .fg(palette().activity_task)
        .add_modifier(Modifier::BOLD)
}

pub(crate) fn selection() -> Style {
    Style::default().bg(palette().selection_background)
}

pub(crate) fn status_style(status: &str) -> Style {
    match status {
        "done" => done(),
        "failed" => failed(),
        "running" | "created" | "starting" => running(),
        _ => muted(),
    }
}
