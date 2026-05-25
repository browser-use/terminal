use ratatui::style::{Color, Modifier, Style};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ThemeMode {
    Dark,
    Light,
}

#[derive(Clone, Copy)]
struct Palette {
    text: Color,
    muted: Color,
    dim: Color,
    accent: Color,
    link: Color,
    path_reference: Color,
    code: Color,
    code_background: Color,
    markdown_code_block: Color,
    heading: Color,
    quote: Color,
    border: Color,
    done: Color,
    running: Color,
    failed: Color,
    thought: Color,
    user_prompt_background: Color,
    activity_group: Color,
    activity_read: Color,
    activity_run: Color,
    activity_list: Color,
    activity_search: Color,
    activity_task: Color,
    selection: Color,
}

const DARK_PALETTE: Palette = Palette {
    text: Color::Rgb(205, 214, 244),
    muted: Color::Rgb(166, 173, 200),
    dim: Color::Rgb(108, 112, 134),
    accent: Color::Rgb(137, 180, 250),
    link: Color::Rgb(137, 220, 235),
    path_reference: Color::Rgb(250, 179, 135),
    code: Color::Rgb(180, 190, 254),
    code_background: Color::Rgb(49, 50, 68),
    markdown_code_block: Color::Rgb(186, 194, 222),
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
    selection: Color::Rgb(45, 52, 66),
};

const LIGHT_PALETTE: Palette = Palette {
    text: Color::Rgb(31, 41, 55),
    muted: Color::Rgb(75, 85, 99),
    dim: Color::Rgb(95, 104, 120),
    accent: Color::Rgb(47, 94, 158),
    link: Color::Rgb(15, 107, 120),
    path_reference: Color::Rgb(154, 75, 32),
    code: Color::Rgb(91, 79, 163),
    code_background: Color::Rgb(232, 234, 242),
    markdown_code_block: Color::Rgb(75, 85, 99),
    heading: Color::Rgb(154, 75, 32),
    quote: Color::Rgb(93, 102, 119),
    border: Color::Rgb(95, 104, 120),
    done: Color::Rgb(63, 111, 42),
    running: Color::Rgb(154, 75, 32),
    failed: Color::Rgb(177, 62, 90),
    thought: Color::Rgb(109, 78, 162),
    user_prompt_background: Color::Rgb(238, 241, 246),
    activity_group: Color::Rgb(63, 111, 42),
    activity_read: Color::Rgb(47, 94, 158),
    activity_run: Color::Rgb(154, 75, 32),
    activity_list: Color::Rgb(19, 112, 109),
    activity_search: Color::Rgb(115, 92, 15),
    activity_task: Color::Rgb(91, 79, 163),
    selection: Color::Rgb(221, 230, 243),
};

fn active_palette() -> Palette {
    palette_for_mode(active_theme_mode())
}

fn active_theme_mode() -> ThemeMode {
    match std::env::var("BUT_THEME") {
        Ok(value) => theme_mode_from_str(&value),
        Err(_) => ThemeMode::Dark,
    }
}

fn theme_mode_from_str(value: &str) -> ThemeMode {
    match value.trim().to_ascii_lowercase().as_str() {
        "light" => ThemeMode::Light,
        "auto" => auto_theme_mode(),
        _ => ThemeMode::Dark,
    }
}

fn auto_theme_mode() -> ThemeMode {
    std::env::var("CLITHEME")
        .ok()
        .map(|value| match value.trim().to_ascii_lowercase().as_str() {
            "light" => ThemeMode::Light,
            _ => ThemeMode::Dark,
        })
        .unwrap_or(ThemeMode::Dark)
}

fn palette_for_mode(mode: ThemeMode) -> Palette {
    match mode {
        ThemeMode::Dark => DARK_PALETTE,
        ThemeMode::Light => LIGHT_PALETTE,
    }
}

pub(crate) fn text() -> Color {
    active_palette().text
}

fn muted_color() -> Color {
    active_palette().muted
}

fn dim_color() -> Color {
    active_palette().dim
}

fn accent_color() -> Color {
    active_palette().accent
}

fn link_color() -> Color {
    active_palette().link
}

fn path_reference_color() -> Color {
    active_palette().path_reference
}

fn code_color() -> Color {
    active_palette().code
}

fn code_background_color() -> Color {
    active_palette().code_background
}

fn heading_color() -> Color {
    active_palette().heading
}

fn quote_color() -> Color {
    active_palette().quote
}

fn border_color() -> Color {
    active_palette().border
}

fn done_color() -> Color {
    active_palette().done
}

fn running_color() -> Color {
    active_palette().running
}

fn failed_color() -> Color {
    active_palette().failed
}

fn thought_color() -> Color {
    active_palette().thought
}

pub(crate) fn text_style() -> Style {
    Style::default().fg(text())
}

pub(crate) fn bold() -> Style {
    text_style().add_modifier(Modifier::BOLD)
}

pub(crate) fn muted() -> Style {
    Style::default().fg(muted_color())
}

pub(crate) fn dim() -> Style {
    Style::default().fg(dim_color())
}

pub(crate) fn accent() -> Style {
    Style::default()
        .fg(accent_color())
        .add_modifier(Modifier::BOLD)
}

pub(crate) fn border() -> Style {
    Style::default().fg(border_color())
}

fn user_prompt_background_color() -> Color {
    active_palette().user_prompt_background
}

/// Background fill for a user prompt block in the transcript, so the message
/// the user sent stands apart from the agent's replies.
pub(crate) fn user_prompt_text() -> Style {
    text_style().bg(user_prompt_background_color())
}

pub(crate) fn user_prompt_muted() -> Style {
    muted().bg(user_prompt_background_color())
}

/// The accent-colored `>` prefix on a user prompt, sharing the prompt's
/// highlight background.
pub(crate) fn user_prompt_accent() -> Style {
    accent().bg(user_prompt_background_color())
}

pub(crate) fn link() -> Style {
    Style::default()
        .fg(link_color())
        .add_modifier(Modifier::UNDERLINED)
}

pub(crate) fn path_reference() -> Style {
    Style::default().fg(path_reference_color())
}

pub(crate) fn markdown_code() -> Style {
    Style::default()
        .fg(code_color())
        .bg(code_background_color())
}

pub(crate) fn markdown_code_block() -> Style {
    Style::default().fg(active_palette().markdown_code_block)
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
        .fg(quote_color())
        .add_modifier(Modifier::ITALIC)
}

pub(crate) fn markdown_heading() -> Style {
    Style::default()
        .fg(heading_color())
        .add_modifier(Modifier::BOLD)
}

pub(crate) fn done() -> Style {
    Style::default().fg(done_color())
}

pub(crate) fn running() -> Style {
    Style::default().fg(running_color())
}

pub(crate) fn failed() -> Style {
    Style::default().fg(failed_color())
}

pub(crate) fn thought() -> Style {
    Style::default().fg(thought_color())
}

pub(crate) fn activity_group() -> Style {
    Style::default()
        .fg(active_palette().activity_group)
        .add_modifier(Modifier::BOLD)
}

pub(crate) fn activity_read() -> Style {
    Style::default()
        .fg(active_palette().activity_read)
        .add_modifier(Modifier::BOLD)
}

pub(crate) fn activity_run() -> Style {
    Style::default()
        .fg(active_palette().activity_run)
        .add_modifier(Modifier::BOLD)
}

pub(crate) fn activity_list() -> Style {
    Style::default()
        .fg(active_palette().activity_list)
        .add_modifier(Modifier::BOLD)
}

pub(crate) fn activity_search() -> Style {
    Style::default()
        .fg(active_palette().activity_search)
        .add_modifier(Modifier::BOLD)
}

pub(crate) fn activity_task() -> Style {
    Style::default()
        .fg(active_palette().activity_task)
        .add_modifier(Modifier::BOLD)
}

pub(crate) fn selection() -> Style {
    Style::default().bg(active_palette().selection)
}

pub(crate) fn status_style(status: &str) -> Style {
    match status {
        "done" => done(),
        "failed" => failed(),
        "running" | "created" | "starting" => running(),
        _ => muted(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIGHT_BACKGROUND: Color = Color::Rgb(252, 252, 250);

    #[test]
    fn default_theme_mode_stays_dark() {
        assert_eq!(theme_mode_from_str(""), ThemeMode::Dark);
        assert_eq!(theme_mode_from_str("dark"), ThemeMode::Dark);
        assert_eq!(theme_mode_from_str("unknown"), ThemeMode::Dark);
    }

    #[test]
    fn light_theme_mode_is_opt_in() {
        assert_eq!(theme_mode_from_str("light"), ThemeMode::Light);
        assert_eq!(theme_mode_from_str(" LIGHT "), ThemeMode::Light);
    }

    #[test]
    fn light_palette_has_readable_foreground_tokens() {
        let palette = LIGHT_PALETTE;
        for (name, color) in [
            ("text", palette.text),
            ("muted", palette.muted),
            ("dim", palette.dim),
            ("accent", palette.accent),
            ("link", palette.link),
            ("path_reference", palette.path_reference),
            ("code", palette.code),
            ("markdown_code_block", palette.markdown_code_block),
            ("heading", palette.heading),
            ("quote", palette.quote),
            ("border", palette.border),
            ("done", palette.done),
            ("running", palette.running),
            ("failed", palette.failed),
            ("thought", palette.thought),
            ("activity_group", palette.activity_group),
            ("activity_read", palette.activity_read),
            ("activity_run", palette.activity_run),
            ("activity_list", palette.activity_list),
            ("activity_search", palette.activity_search),
            ("activity_task", palette.activity_task),
        ] {
            assert!(
                contrast_ratio(color, LIGHT_BACKGROUND) >= 4.5,
                "{name} should contrast against light terminal backgrounds"
            );
        }
    }

    fn contrast_ratio(a: Color, b: Color) -> f64 {
        let a = relative_luminance(a);
        let b = relative_luminance(b);
        (a.max(b) + 0.05) / (a.min(b) + 0.05)
    }

    fn relative_luminance(color: Color) -> f64 {
        let Color::Rgb(r, g, b) = color else {
            panic!("test palettes should use RGB colors");
        };
        0.2126 * linear_component(r) + 0.7152 * linear_component(g) + 0.0722 * linear_component(b)
    }

    fn linear_component(value: u8) -> f64 {
        let value = f64::from(value) / 255.0;
        if value <= 0.03928 {
            value / 12.92
        } else {
            ((value + 0.055) / 1.055).powf(2.4)
        }
    }
}
