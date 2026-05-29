use std::path::PathBuf;

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::text::{Line, Span};

use crate::theme::{accent, dim};

#[derive(Debug, Default)]
pub(crate) struct Composer {
    text: String,
    local_images: Vec<PathBuf>,
    cursor: usize,
    preferred_column: Option<usize>,
}

impl Composer {
    pub(crate) fn is_empty(&self) -> bool {
        self.text.is_empty() && self.local_images.is_empty()
    }

    pub(crate) fn clear(&mut self) {
        self.text.clear();
        self.local_images.clear();
        self.cursor = 0;
        self.preferred_column = None;
    }

    pub(crate) fn take_trimmed(&mut self) -> String {
        let text = self.text.trim().to_string();
        self.clear();
        text
    }

    pub(crate) fn take_submission(&mut self) -> (String, Vec<PathBuf>) {
        let text = self.text.trim().to_string();
        let local_images = std::mem::take(&mut self.local_images);
        self.clear();
        (text, local_images)
    }

    pub(crate) fn input(&self) -> &str {
        &self.text
    }

    pub(crate) fn has_local_images(&self) -> bool {
        !self.local_images.is_empty()
    }

    pub(crate) fn attach_image(&mut self, path: PathBuf) {
        self.local_images.push(path);
    }

    pub(crate) fn insert_paste(&mut self, text: &str) -> bool {
        if text.is_empty() {
            return false;
        }
        let normalized = normalize_pasted_text(text);
        self.insert_str(&normalized);
        true
    }

    #[cfg(test)]
    pub(crate) fn cursor(&self) -> usize {
        self.cursor
    }

    pub(crate) fn set_input(&mut self, value: String) {
        self.text = value;
        self.local_images.clear();
        self.cursor = self.input_len();
        self.preferred_column = None;
    }

    #[cfg(test)]
    pub(crate) fn set_cursor(&mut self, cursor: usize) {
        self.cursor = cursor.min(self.input_len());
        self.preferred_column = None;
    }

    pub(crate) fn input_len(&self) -> usize {
        self.text.chars().count()
    }

    pub(crate) fn cursor_is_at_text_boundary(&self) -> bool {
        self.cursor == 0 || self.cursor == self.input_len()
    }

    #[cfg(test)]
    pub(crate) fn height(&self) -> u16 {
        self.line_count().clamp(1, 10) as u16 + 2
    }

    pub(crate) fn render_lines(&self, max_lines: usize, placeholder: &str) -> Vec<Line<'static>> {
        if self.is_empty() {
            return vec![Line::from(vec![
                Span::styled("> ", dim()),
                Span::styled(placeholder.to_string(), dim()),
            ])];
        }
        visible_composer_lines(
            self.composer_input_lines(),
            self.visible_line_start(max_lines),
            max_lines,
        )
    }

    pub(crate) fn render_lines_wrapped(
        &self,
        max_lines: usize,
        width: usize,
        placeholder: &str,
    ) -> Vec<Line<'static>> {
        if self.is_empty() {
            return self.render_lines(max_lines, placeholder);
        }
        let lines = self.wrapped_composer_input_lines(width);
        visible_composer_lines(
            lines,
            self.visible_wrapped_line_start(max_lines, width),
            max_lines,
        )
    }

    pub(crate) fn visual_line_count_wrapped(&self, width: usize) -> usize {
        let content_width = width.saturating_sub(2).max(1);
        self.wrapped_line_count(content_width)
    }

    pub(crate) fn cursor_position_wrapped(&self, max_lines: usize, width: usize) -> (u16, u16) {
        if self.is_empty() {
            return (2, 0);
        }
        let content_width = width.saturating_sub(2).max(1);
        let (visual_row, visual_col) = self.cursor_visual_row_col_wrapped(content_width);
        let visible_start = self.visible_wrapped_line_start(max_lines, width);
        (
            visual_col.saturating_add(2) as u16,
            visual_row.saturating_sub(visible_start) as u16,
        )
    }

    pub(crate) fn set_cursor_from_wrapped_position(
        &mut self,
        max_lines: usize,
        width: usize,
        row: usize,
        col: usize,
    ) -> bool {
        if self.is_empty() {
            self.cursor = 0;
            self.preferred_column = None;
            return true;
        }
        let content_width = width.saturating_sub(2).max(1);
        let visible_start = self.visible_wrapped_line_start(max_lines, width);
        let target_row = visible_start.saturating_add(row);
        let target_col = col.saturating_sub(2);
        let image_width = self.image_width(!self.text.is_empty());
        let mut visual_row = 0usize;
        let mut line_start = 0usize;

        for (logical_idx, line) in self.text.split('\n').enumerate() {
            let line_len = line.chars().count();
            let wrap_width = if logical_idx == 0 {
                content_width.saturating_sub(image_width).max(1)
            } else {
                content_width
            };
            let line_visual_rows = wrapped_line_count(line, wrap_width);
            if target_row < visual_row.saturating_add(line_visual_rows) {
                let row_in_line = target_row.saturating_sub(visual_row);
                let col_in_line = if logical_idx == 0 && row_in_line == 0 {
                    target_col.saturating_sub(image_width)
                } else {
                    target_col
                };
                let line_col = row_in_line
                    .saturating_mul(wrap_width)
                    .saturating_add(col_in_line)
                    .min(line_len);
                self.cursor = line_start.saturating_add(line_col).min(self.input_len());
                self.preferred_column = None;
                return true;
            }
            visual_row = visual_row.saturating_add(line_visual_rows);
            line_start = line_start.saturating_add(line_len).saturating_add(1);
        }

        self.cursor = self.input_len();
        self.preferred_column = None;
        true
    }

    pub(crate) fn handle_key(&mut self, key: KeyEvent) -> bool {
        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            return false;
        }

        if is_insert_newline_key(key) {
            self.insert_str("\n");
            return true;
        }

        if matches!(normalize_key(key), (KeyCode::Backspace, _))
            && self.text.is_empty()
            && self.local_images.pop().is_some()
        {
            return true;
        }

        if key_pressed(key, KeyCode::Char('u'), KeyModifiers::CONTROL) {
            self.delete_to_line_start_or_remove_empty_line();
            return true;
        }

        if key_pressed(key, KeyCode::Char('w'), KeyModifiers::CONTROL) {
            self.delete_backward_word();
            return true;
        }

        // Cmd/Super/Meta + Backspace deletes the whole line (macOS-style).
        // This has to be checked before the ALT branch because some
        // terminals report Cmd as META instead of SUPER, and we don't want
        // Cmd+Backspace to fall through to the word-delete handler.
        if key
            .modifiers
            .intersects(KeyModifiers::SUPER | KeyModifiers::HYPER | KeyModifiers::META)
            && matches!(key.code, KeyCode::Backspace | KeyCode::Delete)
        {
            self.kill_current_line();
            return true;
        }

        // Option/Alt + Backspace deletes back to the previous blank, matching
        // the shell-style behavior users expect from word erase.
        if key_pressed(key, KeyCode::Backspace, KeyModifiers::ALT) {
            self.delete_backward_word();
            return true;
        }

        if key_pressed(key, KeyCode::Left, KeyModifiers::ALT)
            || key_pressed(key, KeyCode::Char('b'), KeyModifiers::ALT)
        {
            self.move_backward_word();
            return true;
        }

        if key_pressed(key, KeyCode::Right, KeyModifiers::ALT)
            || key_pressed(key, KeyCode::Char('f'), KeyModifiers::ALT)
        {
            self.move_forward_word();
            return true;
        }

        match normalize_key(key) {
            (KeyCode::Enter, KeyModifiers::NONE) => false,
            (KeyCode::Backspace, _) => {
                self.delete_backward_char();
                true
            }
            (KeyCode::Delete, _) => {
                self.delete_forward_char();
                true
            }
            (KeyCode::Left, _) => {
                self.move_left();
                true
            }
            (KeyCode::Right, _) => {
                self.move_right();
                true
            }
            (KeyCode::Up, _) | (KeyCode::Char('p'), KeyModifiers::CONTROL) => {
                self.move_line_up();
                true
            }
            (KeyCode::Down, _) | (KeyCode::Char('n'), KeyModifiers::CONTROL) => {
                self.move_line_down();
                true
            }
            (KeyCode::Home, _) | (KeyCode::Char('a'), KeyModifiers::CONTROL) => {
                self.move_to_line_start();
                true
            }
            (KeyCode::End, _) | (KeyCode::Char('e'), KeyModifiers::CONTROL) => {
                self.move_to_line_end();
                true
            }
            (KeyCode::Char(_), modifiers) if !has_ctrl_or_alt(modifiers) => {
                let Some(ch) = text_char_for_key(key) else {
                    return false;
                };
                self.insert_char(ch);
                true
            }
            _ => self.should_swallow_unmodified_key(key),
        }
    }

    fn composer_input_lines(&self) -> Vec<Line<'static>> {
        let image_spans = self.image_spans(!self.text.is_empty());
        if self.text.is_empty() {
            if image_spans.is_empty() {
                return vec![Line::from(vec![Span::styled("  ", accent())])];
            }
            let mut spans = vec![Span::styled("> ", accent())];
            spans.extend(image_spans);
            return vec![Line::from(spans)];
        }

        let mut lines = Vec::new();
        for (idx, source_line) in self.text.split('\n').enumerate() {
            let mut spans = vec![Span::styled(if idx == 0 { "> " } else { "  " }, accent())];
            if idx == 0 {
                spans.extend(image_spans.clone());
            }
            spans.push(Span::raw(source_line.to_string()));
            lines.push(Line::from(spans));
        }
        lines
    }

    fn wrapped_composer_input_lines(&self, width: usize) -> Vec<Line<'static>> {
        let content_width = width.saturating_sub(2).max(1);
        let image_spans = self.image_spans(!self.text.is_empty());
        let image_width = self.image_width(!self.text.is_empty());
        if self.text.is_empty() {
            if image_spans.is_empty() {
                return vec![Line::from(vec![Span::styled("  ", accent())])];
            }
            let mut spans = vec![Span::styled("> ", accent())];
            spans.extend(image_spans);
            return vec![Line::from(spans)];
        }
        let mut lines = Vec::new();
        for (logical_idx, source_line) in self.text.split('\n').enumerate() {
            let wrap_width = if logical_idx == 0 {
                content_width.saturating_sub(image_width).max(1)
            } else {
                content_width
            };
            let mut chunks = hard_wrap_line(source_line, wrap_width);
            if chunks.is_empty() {
                chunks.push(String::new());
            }
            for (chunk_idx, chunk) in chunks.into_iter().enumerate() {
                let prefix = if logical_idx == 0 && chunk_idx == 0 {
                    "> "
                } else {
                    "  "
                };
                let mut spans = vec![Span::styled(prefix, accent())];
                if logical_idx == 0 && chunk_idx == 0 {
                    spans.extend(image_spans.clone());
                }
                spans.push(Span::raw(chunk));
                lines.push(Line::from(spans));
            }
        }
        lines
    }

    fn image_spans(&self, trailing_space: bool) -> Vec<Span<'static>> {
        let last_idx = self.local_images.len().saturating_sub(1);
        self.local_images
            .iter()
            .enumerate()
            .flat_map(|(idx, _)| {
                let mut spans = vec![Span::styled(format!("[Image {}]", idx + 1), accent())];
                if idx != last_idx || trailing_space {
                    spans.push(Span::raw(" "));
                }
                spans
            })
            .collect()
    }

    fn image_width(&self, trailing_space: bool) -> usize {
        self.local_images
            .iter()
            .enumerate()
            .map(|(idx, _)| {
                image_label_width(idx + 1)
                    + usize::from(idx + 1 < self.local_images.len() || trailing_space)
            })
            .sum()
    }

    fn visible_line_start(&self, max_lines: usize) -> usize {
        if max_lines == 0 {
            return 0;
        }
        let line_count = self.display_line_count();
        if line_count <= max_lines {
            return 0;
        }
        let (cursor_row, _) = self.cursor_row_col();
        let max_start = line_count.saturating_sub(max_lines);
        cursor_row
            .saturating_sub(max_lines.saturating_sub(1))
            .min(max_start)
    }

    fn visible_wrapped_line_start(&self, max_lines: usize, width: usize) -> usize {
        if max_lines == 0 {
            return 0;
        }
        let content_width = width.saturating_sub(2).max(1);
        let line_count = self.wrapped_line_count(content_width);
        if line_count <= max_lines {
            return 0;
        }
        let (cursor_visual_row, _) = self.cursor_visual_row_col_wrapped(content_width);
        let max_start = line_count.saturating_sub(max_lines);
        cursor_visual_row
            .saturating_sub(max_lines.saturating_sub(1))
            .min(max_start)
    }

    fn wrapped_line_count(&self, content_width: usize) -> usize {
        if self.text.is_empty() {
            return 1;
        }
        let image_width = self.image_width(!self.text.is_empty());
        self.text
            .split('\n')
            .enumerate()
            .map(|(idx, line)| {
                let wrap_width = if idx == 0 {
                    content_width.saturating_sub(image_width).max(1)
                } else {
                    content_width
                };
                wrapped_line_count(line, wrap_width)
            })
            .sum::<usize>()
            .max(1)
    }

    fn cursor_visual_row_col_wrapped(&self, content_width: usize) -> (usize, usize) {
        let image_width = self.image_width(!self.text.is_empty());
        if self.text.is_empty() {
            return (0, image_width);
        }
        let (cursor_row, cursor_col) = self.cursor_row_col();
        let mut visual_row = 0usize;
        for (idx, line) in self.text.split('\n').take(cursor_row).enumerate() {
            let wrap_width = if idx == 0 {
                content_width.saturating_sub(image_width).max(1)
            } else {
                content_width
            };
            visual_row += wrapped_line_count(line, wrap_width);
        }
        let wrap_width = if cursor_row == 0 {
            content_width.saturating_sub(image_width).max(1)
        } else {
            content_width
        };
        let (cursor_line_row, cursor_line_col) = wrapped_cursor_row_col(cursor_col, wrap_width);
        visual_row += cursor_line_row;
        let visual_col = if cursor_row == 0 && cursor_line_row == 0 {
            image_width + cursor_line_col
        } else {
            cursor_line_col
        };
        (visual_row, visual_col)
    }

    fn should_swallow_unmodified_key(&self, key: KeyEvent) -> bool {
        if self.is_empty() {
            return false;
        }
        matches!(
            normalize_key(key),
            (
                KeyCode::Char('b' | 'd' | 'f' | 'h' | 'j' | 'k' | 'm' | 'y'),
                KeyModifiers::CONTROL
            ) | (KeyCode::Char('b' | 'd' | 'f' | 'h'), KeyModifiers::ALT)
        ) || key
            .modifiers
            .intersects(KeyModifiers::ALT | KeyModifiers::CONTROL)
    }

    fn insert_char(&mut self, ch: char) {
        self.insert_str(&ch.to_string());
    }

    fn insert_str(&mut self, value: &str) {
        let byte_idx = byte_index_for_char(&self.text, self.cursor);
        self.text.insert_str(byte_idx, value);
        self.cursor += value.chars().count();
        self.preferred_column = None;
    }

    fn delete_backward_char(&mut self) {
        if self.cursor == 0 {
            return;
        }
        self.delete_char_range(self.cursor - 1, self.cursor);
    }

    fn delete_forward_char(&mut self) {
        if self.cursor >= self.input_len() {
            return;
        }
        self.delete_char_range(self.cursor, self.cursor + 1);
    }

    fn delete_backward_word(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let chars = self.chars();
        let mut start = self.cursor;
        while start > 0 && chars[start - 1].is_whitespace() {
            start -= 1;
        }
        while start > 0 && !chars[start - 1].is_whitespace() {
            start -= 1;
        }
        self.delete_char_range(start, self.cursor);
    }

    fn delete_to_line_start_or_remove_empty_line(&mut self) {
        let start = self.current_line_start();
        if self.cursor == start && start > 0 {
            self.delete_char_range(start - 1, start);
            return;
        }
        self.delete_char_range(start, self.cursor);
    }

    fn kill_current_line(&mut self) {
        if self.text.is_empty() {
            return;
        }
        let start = self.current_line_start();
        let end = self.current_line_end();
        let line_count = self.line_count();
        let delete_start = if start > 0 { start - 1 } else { start };
        let delete_end = if end < self.input_len() && line_count > 1 {
            end + 1
        } else {
            end
        };
        self.delete_char_range(delete_start, delete_end);
    }

    fn delete_char_range(&mut self, start: usize, end: usize) {
        if start >= end {
            return;
        }
        let byte_start = byte_index_for_char(&self.text, start);
        let byte_end = byte_index_for_char(&self.text, end);
        self.text.replace_range(byte_start..byte_end, "");
        self.cursor = start.min(self.input_len());
        self.preferred_column = None;
    }

    fn move_left(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
        self.preferred_column = None;
    }

    fn move_right(&mut self) {
        self.cursor = (self.cursor + 1).min(self.input_len());
        self.preferred_column = None;
    }

    fn move_backward_word(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let chars = self.chars();
        let mut cursor = self.cursor;
        while cursor > 0 && chars[cursor - 1].is_whitespace() {
            cursor -= 1;
        }
        while cursor > 0 && !chars[cursor - 1].is_whitespace() {
            cursor -= 1;
        }
        self.cursor = cursor;
        self.preferred_column = None;
    }

    fn move_forward_word(&mut self) {
        let chars = self.chars();
        let mut cursor = self.cursor.min(chars.len());
        while cursor < chars.len() && chars[cursor].is_whitespace() {
            cursor += 1;
        }
        while cursor < chars.len() && !chars[cursor].is_whitespace() {
            cursor += 1;
        }
        self.cursor = cursor;
        self.preferred_column = None;
    }

    fn move_to_line_start(&mut self) {
        self.cursor = self.current_line_start();
        self.preferred_column = None;
    }

    fn move_to_line_end(&mut self) {
        self.cursor = self.current_line_end();
        self.preferred_column = None;
    }

    fn move_line_up(&mut self) {
        let (row, col) = self.cursor_row_col();
        if row == 0 {
            return;
        }
        let column = self.preferred_column.unwrap_or(col);
        self.preferred_column = Some(column);
        self.cursor = self.cursor_for_row_col(row - 1, column);
    }

    fn move_line_down(&mut self) {
        let (row, col) = self.cursor_row_col();
        if row + 1 >= self.line_count() {
            return;
        }
        let column = self.preferred_column.unwrap_or(col);
        self.preferred_column = Some(column);
        self.cursor = self.cursor_for_row_col(row + 1, column);
    }

    fn current_line_start(&self) -> usize {
        let chars = self.chars();
        let mut idx = self.cursor.min(chars.len());
        while idx > 0 && chars[idx - 1] != '\n' {
            idx -= 1;
        }
        idx
    }

    fn current_line_end(&self) -> usize {
        let chars = self.chars();
        let mut idx = self.cursor.min(chars.len());
        while idx < chars.len() && chars[idx] != '\n' {
            idx += 1;
        }
        idx
    }

    fn cursor_row_col(&self) -> (usize, usize) {
        let mut row = 0usize;
        let mut col = 0usize;
        for (idx, ch) in self.text.chars().enumerate() {
            if idx == self.cursor {
                break;
            }
            if ch == '\n' {
                row += 1;
                col = 0;
            } else {
                col += 1;
            }
        }
        (row, col)
    }

    fn cursor_for_row_col(&self, target_row: usize, target_col: usize) -> usize {
        let mut row = 0usize;
        let mut col = 0usize;
        for (idx, ch) in self.text.chars().enumerate() {
            if row == target_row && (col == target_col || ch == '\n') {
                return idx;
            }
            if ch == '\n' {
                if row == target_row {
                    return idx;
                }
                row += 1;
                col = 0;
            } else {
                col += 1;
            }
        }
        self.input_len()
    }

    fn line_count(&self) -> usize {
        self.text.split('\n').count()
    }

    fn display_line_count(&self) -> usize {
        self.line_count().max(1)
    }

    fn chars(&self) -> Vec<char> {
        self.text.chars().collect()
    }
}

fn image_label_width(index: usize) -> usize {
    format!("[Image {index}]").chars().count()
}

fn visible_composer_lines(
    mut lines: Vec<Line<'static>>,
    start: usize,
    max_lines: usize,
) -> Vec<Line<'static>> {
    if lines.len() <= max_lines {
        return lines;
    }
    lines.drain(0..start.min(lines.len()));
    lines.truncate(max_lines);
    lines
}

fn hard_wrap_line(line: &str, width: usize) -> Vec<String> {
    if line.is_empty() {
        return vec![String::new()];
    }
    let width = width.max(1);
    line.chars()
        .collect::<Vec<_>>()
        .chunks(width)
        .map(|chunk| chunk.iter().collect::<String>())
        .collect()
}

fn wrapped_line_count(line: &str, width: usize) -> usize {
    if line.is_empty() {
        return 1;
    }
    let width = width.max(1);
    line.chars().count().saturating_add(width.saturating_sub(1)) / width
}

fn wrapped_cursor_row_col(cursor_col: usize, width: usize) -> (usize, usize) {
    let width = width.max(1);
    if cursor_col == 0 {
        return (0, 0);
    }
    let previous_cell = cursor_col - 1;
    (previous_cell / width, previous_cell % width + 1)
}

fn normalize_pasted_text(value: &str) -> String {
    value.replace("\r\n", "\n").replace('\r', "\n")
}

fn byte_index_for_char(value: &str, char_index: usize) -> usize {
    value
        .char_indices()
        .nth(char_index)
        .map(|(idx, _)| idx)
        .unwrap_or(value.len())
}

fn key_pressed(event: KeyEvent, code: KeyCode, modifiers: KeyModifiers) -> bool {
    normalize_key(event) == normalize_key_parts(code, modifiers)
}

fn is_insert_newline_key(event: KeyEvent) -> bool {
    matches!(
        normalize_key(event),
        (KeyCode::Enter, KeyModifiers::SHIFT)
            | (KeyCode::Enter, KeyModifiers::ALT)
            | (KeyCode::Enter, KeyModifiers::META)
            | (KeyCode::Char('j'), KeyModifiers::CONTROL)
            | (KeyCode::Char('\n'), KeyModifiers::NONE)
            | (KeyCode::Char('\r'), KeyModifiers::NONE)
            | (KeyCode::Char('\n'), KeyModifiers::ALT)
            | (KeyCode::Char('\r'), KeyModifiers::ALT)
            | (KeyCode::Char('\n'), KeyModifiers::META)
            | (KeyCode::Char('\r'), KeyModifiers::META)
    )
}

fn normalize_key(event: KeyEvent) -> (KeyCode, KeyModifiers) {
    normalize_key_parts(event.code, event.modifiers)
}

fn normalize_key_parts(code: KeyCode, mut modifiers: KeyModifiers) -> (KeyCode, KeyModifiers) {
    let KeyCode::Char(ch) = code else {
        return (code, normalized_modifiers(modifiers));
    };
    if modifiers.is_empty() {
        if let Some(ctrl_char) = c0_control_char_to_ctrl_char(ch) {
            return (KeyCode::Char(ctrl_char), KeyModifiers::CONTROL);
        }
    }
    if ch.is_ascii_uppercase() {
        modifiers.insert(KeyModifiers::SHIFT);
        return (
            KeyCode::Char(ch.to_ascii_lowercase()),
            normalized_modifiers(modifiers),
        );
    }
    (code, normalized_modifiers(modifiers))
}

fn normalized_modifiers(modifiers: KeyModifiers) -> KeyModifiers {
    let mut out = modifiers;
    out.remove(KeyModifiers::SUPER);
    out.remove(KeyModifiers::HYPER);
    out
}

fn has_ctrl_or_alt(modifiers: KeyModifiers) -> bool {
    let modifiers = normalized_modifiers(modifiers);
    modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::META)
}

fn text_char_for_key(event: KeyEvent) -> Option<char> {
    let modifiers = normalized_modifiers(event.modifiers);
    if has_ctrl_or_alt(modifiers) {
        return None;
    }
    let KeyCode::Char(ch) = event.code else {
        return None;
    };
    if ch.is_control() {
        return None;
    }
    if modifiers.contains(KeyModifiers::SHIFT) && ch.is_ascii_lowercase() {
        return Some(ch.to_ascii_uppercase());
    }
    Some(ch)
}

fn c0_control_char_to_ctrl_char(ch: char) -> Option<char> {
    let code = u32::from(ch);
    match code {
        0x00 => Some(' '),
        0x01..=0x1a => char::from_u32(code - 0x01 + u32::from('a')),
        0x1c..=0x1f => char::from_u32(code - 0x1c + u32::from('4')),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain(lines: Vec<Line<'static>>) -> String {
        let mut out = String::new();
        for line in lines {
            for span in line.spans {
                out.push_str(&span.content);
            }
            out.push('\n');
        }
        out
    }

    #[test]
    fn shifted_letters_insert_as_uppercase() {
        let mut composer = Composer::default();

        for (code, modifiers) in [
            (KeyCode::Char('A'), KeyModifiers::SHIFT),
            (KeyCode::Char('b'), KeyModifiers::SHIFT),
            (KeyCode::Char('!'), KeyModifiers::SHIFT),
            (KeyCode::Char('c'), KeyModifiers::NONE),
        ] {
            assert!(composer.handle_key(KeyEvent::new(code, modifiers)));
        }

        assert_eq!(composer.input(), "AB!c");
    }

    #[test]
    fn alt_backspace_deletes_back_to_previous_blank() {
        let mut composer = Composer::default();

        composer.set_input("/stuff".to_string());
        assert!(composer.handle_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::ALT)));
        assert_eq!(composer.input(), "");

        composer.set_input("something-bla".to_string());
        assert!(composer.handle_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::ALT)));
        assert_eq!(composer.input(), "");

        composer.set_input("before something-bla".to_string());
        assert!(composer.handle_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::ALT)));
        assert_eq!(composer.input(), "before ");

        composer.set_input("alpha  ".to_string());
        assert!(composer.handle_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::ALT)));
        assert_eq!(composer.input(), "");

        composer.set_input("alpha  ".to_string());
        assert!(composer.handle_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::META)));
        assert_eq!(composer.input(), "");
    }

    #[test]
    fn alt_arrows_move_by_whitespace_delimited_words_across_lines() {
        let mut composer = Composer::default();

        composer.set_input("alpha beta\ngamma-delta epsilon".to_string());
        assert!(composer.handle_key(KeyEvent::new(KeyCode::Left, KeyModifiers::ALT)));
        assert_eq!(
            composer.cursor(),
            "alpha beta\ngamma-delta ".chars().count()
        );
        assert!(composer.handle_key(KeyEvent::new(KeyCode::Left, KeyModifiers::ALT)));
        assert_eq!(composer.cursor(), "alpha beta\n".chars().count());
        assert!(composer.handle_key(KeyEvent::new(KeyCode::Left, KeyModifiers::ALT)));
        assert_eq!(composer.cursor(), "alpha ".chars().count());

        assert!(composer.handle_key(KeyEvent::new(KeyCode::Right, KeyModifiers::ALT)));
        assert_eq!(composer.cursor(), "alpha beta".chars().count());
        assert!(composer.handle_key(KeyEvent::new(KeyCode::Right, KeyModifiers::ALT)));
        assert_eq!(composer.cursor(), "alpha beta\ngamma-delta".chars().count());
    }

    #[test]
    fn escape_word_keys_move_like_option_arrows() {
        let mut composer = Composer::default();

        composer.set_input("alpha beta gamma".to_string());
        assert!(composer.handle_key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::ALT)));
        assert_eq!(composer.cursor(), "alpha beta ".chars().count());
        assert!(composer.handle_key(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::ALT)));
        assert_eq!(composer.cursor(), "alpha beta gamma".chars().count());
    }

    #[test]
    fn click_position_maps_to_wrapped_multiline_cursor() {
        let mut composer = Composer::default();
        composer.set_input("first line\nsecond word\nthird".to_string());

        assert!(composer.set_cursor_from_wrapped_position(4, 40, 1, 2 + "second ".chars().count()));
        assert_eq!(composer.cursor(), "first line\nsecond ".chars().count());

        assert!(composer.set_cursor_from_wrapped_position(4, 40, 2, 2 + "thi".chars().count()));
        assert_eq!(
            composer.cursor(),
            "first line\nsecond word\nthi".chars().count()
        );
    }

    #[test]
    fn ctrl_w_keeps_whitespace_delimited_shell_behavior() {
        let mut composer = Composer::default();

        composer.set_input("something-bla".to_string());
        assert!(composer.handle_key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::CONTROL,)));
        assert_eq!(composer.input(), "");
    }

    #[test]
    fn wrapped_visual_line_count_matches_rendered_prompt_width() {
        let mut composer = Composer::default();
        composer.set_input("abcdefg".to_string());

        assert_eq!(composer.visual_line_count_wrapped(8), 2);
        assert_eq!(
            plain(composer.render_lines_wrapped(
                composer.visual_line_count_wrapped(8),
                8,
                "placeholder",
            )),
            "> abcdef\n  g\n"
        );
    }

    #[test]
    fn wrapped_cursor_stays_at_end_of_exactly_full_visual_line() {
        let mut composer = Composer::default();
        composer.set_input("abcdef".to_string());

        assert_eq!(composer.render_lines_wrapped(1, 8, "placeholder").len(), 1);
        assert_eq!(composer.cursor_position_wrapped(1, 8), (8, 0));

        composer.set_input("abcdefghijkl".to_string());
        assert_eq!(composer.render_lines_wrapped(2, 8, "placeholder").len(), 2);
        assert_eq!(composer.cursor_position_wrapped(2, 8), (8, 1));
    }

    #[test]
    fn attached_images_render_before_text_and_submit_with_paths() {
        let mut composer = Composer::default();
        composer.set_input("describe it".to_string());
        composer.attach_image(PathBuf::from("/tmp/shot.png"));
        composer.attach_image(PathBuf::from("/tmp/other.png"));

        assert_eq!(
            plain(composer.render_lines_wrapped(4, 80, "placeholder")),
            "> [Image 1] [Image 2] describe it\n"
        );

        let (text, images) = composer.take_submission();
        assert_eq!(text, "describe it");
        assert_eq!(
            images,
            vec![
                PathBuf::from("/tmp/shot.png"),
                PathBuf::from("/tmp/other.png")
            ]
        );
        assert!(composer.is_empty());
    }

    #[test]
    fn attached_images_without_text_render_inline_without_paths() {
        let mut composer = Composer::default();
        composer.attach_image(PathBuf::from("/tmp/shot.png"));
        composer.attach_image(PathBuf::from("/tmp/other.png"));
        composer.attach_image(PathBuf::from("/tmp/third.png"));

        let rendered = plain(composer.render_lines_wrapped(4, 40, "placeholder"));
        assert_eq!(rendered, "> [Image 1] [Image 2] [Image 3]\n");
        assert!(!rendered.contains("shot.png"));
        assert_eq!(composer.visual_line_count_wrapped(40), 1);
        assert_eq!(composer.cursor_position_wrapped(4, 40), (31, 0));
    }

    #[test]
    fn backspace_removes_image_when_text_is_empty() {
        let mut composer = Composer::default();
        composer.attach_image(PathBuf::from("/tmp/shot.png"));

        assert!(composer.handle_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE,)));
        assert!(composer.is_empty());
    }
}
