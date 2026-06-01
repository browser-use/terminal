use pulldown_cmark::{Alignment, CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use std::sync::OnceLock;
use syntect::easy::HighlightLines;
use syntect::highlighting::{FontStyle, Style as SyntectStyle, Theme};
use syntect::parsing::{SyntaxReference, SyntaxSet};
use syntect::util::LinesWithEndings;
use two_face::theme::EmbeddedThemeName;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::theme::{
    link, markdown_code, markdown_code_block, markdown_emphasis, markdown_heading, markdown_marker,
    markdown_quote, markdown_strong, muted, path_reference, text_style,
};

pub(crate) fn render_markdown_lines(markdown: &str, width: u16) -> Vec<Line<'static>> {
    let mut options = Options::empty();
    options.insert(Options::ENABLE_TABLES);
    options.insert(Options::ENABLE_STRIKETHROUGH);

    let mut writer = MarkdownWriter::new(width as usize);
    for event in Parser::new_ext(markdown, options) {
        writer.handle_event(event);
    }
    writer.finish()
}

pub(crate) fn render_code_lines(
    code: &str,
    language_hint: Option<&str>,
    width: u16,
) -> Vec<Line<'static>> {
    let width = usize::from(width.max(1)).max(24);
    let lines =
        highlight_code_to_lines(code, language_hint).unwrap_or_else(|| plain_code_lines(code));
    lines
        .into_iter()
        .flat_map(|line| wrap_line_preserving_spans(line, width))
        .collect()
}

#[derive(Clone, Debug)]
struct ListState {
    next: Option<u64>,
}

#[derive(Clone, Debug)]
struct TableRow {
    cells: Vec<Vec<Span<'static>>>,
    is_header: bool,
}

#[derive(Clone, Debug)]
struct TableState {
    alignments: Vec<Alignment>,
    rows: Vec<TableRow>,
    current_row: Option<Vec<Vec<Span<'static>>>>,
    current_cell: Option<Vec<Span<'static>>>,
    in_header: bool,
}

struct MarkdownWriter {
    lines: Vec<Line<'static>>,
    current: Vec<Span<'static>>,
    style_stack: Vec<Style>,
    list_stack: Vec<ListState>,
    link_stack: Vec<String>,
    table: Option<TableState>,
    pending_prefix: Option<Vec<Span<'static>>>,
    quote_depth: usize,
    in_code_block: bool,
    code_block_lang: Option<String>,
    code_block_buffer: String,
    needs_blank: bool,
    width: usize,
}

impl MarkdownWriter {
    fn new(width: usize) -> Self {
        Self {
            lines: Vec::new(),
            current: Vec::new(),
            style_stack: Vec::new(),
            list_stack: Vec::new(),
            link_stack: Vec::new(),
            table: None,
            pending_prefix: None,
            quote_depth: 0,
            in_code_block: false,
            code_block_lang: None,
            code_block_buffer: String::new(),
            needs_blank: false,
            width: width.max(24),
        }
    }

    fn handle_event(&mut self, event: Event<'_>) {
        match event {
            Event::Start(tag) => self.start_tag(tag),
            Event::End(tag) => self.end_tag(tag),
            Event::Text(text) => {
                if self.in_code_block {
                    self.push_code_text(&text);
                } else {
                    self.push_text(&text);
                }
            }
            Event::Code(code) => {
                self.push_inline_code(&code);
            }
            Event::SoftBreak | Event::HardBreak => self.flush_current(),
            Event::Rule => {
                self.flush_current();
                self.push_blank();
                self.lines.push(Line::from(Span::styled("---", muted())));
                self.push_blank();
            }
            Event::Html(html) | Event::InlineHtml(html) => self.push_text(&html),
            Event::FootnoteReference(text) => self.push_text(&format!("[{text}]")),
            Event::TaskListMarker(checked) => {
                self.push_span(Span::styled(if checked { "[x] " } else { "[ ] " }, muted()));
            }
        }
    }

    fn start_tag(&mut self, tag: Tag<'_>) {
        match tag {
            Tag::Paragraph => {
                if self.needs_blank && self.list_stack.is_empty() {
                    self.push_blank();
                }
                self.needs_blank = false;
            }
            Tag::Heading { level, .. } => {
                self.flush_current();
                if !self.lines.is_empty() {
                    self.push_blank();
                }
                self.push_style(heading_text_style(level));
            }
            Tag::BlockQuote => {
                self.flush_current();
                if self.needs_blank {
                    self.push_blank();
                    self.needs_blank = false;
                }
                self.quote_depth += 1;
            }
            Tag::Table(alignments) => {
                self.flush_current();
                if !self.lines.is_empty() && !self.last_line_is_blank() {
                    self.push_blank();
                }
                self.table = Some(TableState {
                    alignments,
                    rows: Vec::new(),
                    current_row: None,
                    current_cell: None,
                    in_header: false,
                });
                self.needs_blank = false;
            }
            Tag::TableHead => {
                if let Some(table) = self.table.as_mut() {
                    table.in_header = true;
                    table.current_row = Some(Vec::new());
                }
            }
            Tag::TableRow => {
                if let Some(table) = self.table.as_mut() {
                    table.current_row = Some(Vec::new());
                }
            }
            Tag::TableCell => {
                if let Some(table) = self.table.as_mut() {
                    table.current_cell = Some(Vec::new());
                }
            }
            Tag::CodeBlock(kind) => {
                self.flush_current();
                if !self.lines.is_empty() && !self.last_line_is_blank() {
                    self.push_blank();
                }
                self.code_block_lang = code_block_language(kind);
                self.code_block_buffer.clear();
                self.in_code_block = true;
                self.needs_blank = false;
            }
            Tag::List(start) => {
                self.flush_current();
                self.list_stack.push(ListState { next: start });
                self.needs_blank = false;
            }
            Tag::Item => {
                self.flush_current();
                self.pending_prefix = Some(self.next_list_marker());
            }
            Tag::Emphasis => self.push_style(markdown_emphasis()),
            Tag::Strong => self.push_style(markdown_strong()),
            Tag::Strikethrough => self.push_style(muted()),
            Tag::Link { dest_url, .. } => {
                self.link_stack.push(dest_url.to_string());
                self.push_style(link());
            }
            Tag::Image {
                title, dest_url, ..
            } => {
                let label = if title.is_empty() {
                    dest_url.to_string()
                } else {
                    title.to_string()
                };
                self.push_span(Span::styled(format!("[image: {label}]"), muted()));
            }
            Tag::FootnoteDefinition(_) | Tag::HtmlBlock | Tag::MetadataBlock(_) => {}
        }
    }

    fn end_tag(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph => {
                self.flush_current();
                self.needs_blank = true;
            }
            TagEnd::Heading(_) => {
                self.flush_current();
                self.pop_style();
                self.needs_blank = true;
            }
            TagEnd::BlockQuote => {
                self.flush_current();
                self.quote_depth = self.quote_depth.saturating_sub(1);
                self.needs_blank = true;
            }
            TagEnd::TableCell => {
                if let Some(table) = self.table.as_mut() {
                    let cell = table.current_cell.take().unwrap_or_default();
                    if let Some(row) = table.current_row.as_mut() {
                        row.push(cell);
                    }
                }
            }
            TagEnd::TableRow => {
                if let Some(table) = self.table.as_mut() {
                    let cells = table.current_row.take().unwrap_or_default();
                    table.rows.push(TableRow {
                        cells,
                        is_header: table.in_header,
                    });
                }
            }
            TagEnd::TableHead => {
                if let Some(table) = self.table.as_mut() {
                    let cells = table.current_row.take().unwrap_or_default();
                    if !cells.is_empty() {
                        table.rows.push(TableRow {
                            cells,
                            is_header: true,
                        });
                    }
                    table.in_header = false;
                }
            }
            TagEnd::Table => {
                self.flush_table();
                self.needs_blank = true;
            }
            TagEnd::CodeBlock => {
                self.flush_code_block();
                self.in_code_block = false;
                self.code_block_lang = None;
                self.needs_blank = true;
            }
            TagEnd::List(_) => {
                self.flush_current();
                self.list_stack.pop();
                self.needs_blank = true;
            }
            TagEnd::Item => {
                self.flush_current();
                self.pending_prefix = None;
            }
            TagEnd::Emphasis | TagEnd::Strong | TagEnd::Strikethrough => self.pop_style(),
            TagEnd::Link => {
                self.pop_style();
                if let Some(dest) = self.link_stack.pop() {
                    self.push_span(Span::raw(" ("));
                    self.push_span(Span::styled(dest, link()));
                    self.push_span(Span::raw(")"));
                }
            }
            TagEnd::Image
            | TagEnd::FootnoteDefinition
            | TagEnd::HtmlBlock
            | TagEnd::MetadataBlock(_) => {}
        }
    }

    fn push_text(&mut self, text: &str) {
        if self.in_table_cell() {
            let normalized = text.replace(['\n', '\r'], " ");
            if !normalized.is_empty() {
                let style = self.current_style();
                self.push_span(Span::styled(normalized, style));
            }
            return;
        }
        for (idx, line) in text.lines().enumerate() {
            if idx > 0 {
                self.flush_current();
            }
            let style = self.current_style();
            if looks_like_bare_link(line) {
                self.push_span(Span::styled(line.to_string(), link()));
            } else if looks_like_path(line) {
                self.push_span(Span::styled(line.to_string(), path_reference()));
            } else {
                self.push_span(Span::styled(line.to_string(), style));
            }
        }
    }

    fn push_code_text(&mut self, text: &str) {
        self.code_block_buffer.push_str(text);
    }

    fn push_inline_code(&mut self, code: &str) {
        let without_leading = code.trim_start();
        let leading_len = code.len().saturating_sub(without_leading.len());
        let trimmed = without_leading.trim_end();
        let trailing_len = without_leading.len().saturating_sub(trimmed.len());

        let leading = &code[..leading_len];
        let trailing_start = code.len().saturating_sub(trailing_len);
        let trailing = &code[trailing_start..];
        let style = if should_style_inline_code(trimmed) {
            markdown_code()
        } else {
            self.current_style()
        };

        if !leading.is_empty() {
            self.push_span(Span::styled(leading.to_string(), self.current_style()));
        }
        if !trimmed.is_empty() {
            self.push_span(Span::styled(trimmed.to_string(), style));
        }
        if !trailing.is_empty() {
            self.push_span(Span::styled(trailing.to_string(), self.current_style()));
        }
    }

    fn flush_code_block(&mut self) {
        let code = std::mem::take(&mut self.code_block_buffer);
        if code.is_empty() {
            return;
        }
        self.lines.extend(render_code_lines(
            &code,
            self.code_block_lang.as_deref(),
            self.width as u16,
        ));
    }

    fn ensure_prefix(&mut self) {
        if !self.current.is_empty() {
            return;
        }
        for _ in 0..self.quote_depth {
            self.current
                .push(Span::styled("| ".to_string(), markdown_quote()));
        }
        if let Some(prefix) = self.pending_prefix.take() {
            self.current.extend(prefix);
        }
    }

    fn push_span(&mut self, span: Span<'static>) {
        if let Some(cell) = self
            .table
            .as_mut()
            .and_then(|table| table.current_cell.as_mut())
        {
            cell.push(span);
            return;
        }
        self.ensure_prefix();
        self.current.push(span);
    }

    fn flush_current(&mut self) {
        if self.current.is_empty() {
            return;
        }
        let line = Line::from(std::mem::take(&mut self.current));
        self.push_rendered_line(line);
        self.pending_prefix = None;
    }

    fn push_rendered_line(&mut self, line: Line<'static>) {
        for wrapped in wrap_line_preserving_spans(line, self.width) {
            self.lines.push(wrapped);
        }
    }

    fn push_blank(&mut self) {
        if !self.last_line_is_blank() {
            self.lines.push(Line::from(""));
        }
    }

    fn last_line_is_blank(&self) -> bool {
        self.lines
            .last()
            .map(|line| line_plain_text(line).trim().is_empty())
            .unwrap_or(false)
    }

    fn next_list_marker(&mut self) -> Vec<Span<'static>> {
        let depth = self.list_stack.len().saturating_sub(1);
        let indent = "  ".repeat(depth);
        let Some(list) = self.list_stack.last_mut() else {
            return vec![
                Span::raw(indent),
                Span::styled("- ".to_string(), markdown_marker()),
            ];
        };
        match &mut list.next {
            Some(next) => {
                let marker = format!("{next}. ");
                *next += 1;
                vec![Span::raw(indent), Span::styled(marker, markdown_marker())]
            }
            None => vec![
                Span::raw(indent),
                Span::styled("- ".to_string(), markdown_marker()),
            ],
        }
    }

    fn push_style(&mut self, style: Style) {
        self.style_stack.push(style);
    }

    fn pop_style(&mut self) {
        self.style_stack.pop();
    }

    fn current_style(&self) -> Style {
        self.style_stack.last().copied().unwrap_or_else(text_style)
    }

    fn in_table_cell(&self) -> bool {
        self.table
            .as_ref()
            .is_some_and(|table| table.current_cell.is_some())
    }

    fn flush_table(&mut self) {
        let Some(table) = self.table.take() else {
            return;
        };
        let rendered = render_table_lines(table, self.width);
        for line in rendered {
            self.lines.push(line);
        }
    }

    fn finish(mut self) -> Vec<Line<'static>> {
        self.flush_current();
        if self.lines.is_empty() {
            return vec![Line::from("")];
        }
        while self.last_line_is_blank() && self.lines.len() > 1 {
            self.lines.pop();
        }
        self.lines
    }
}

fn wrap_line_preserving_spans(line: Line<'static>, width: usize) -> Vec<Line<'static>> {
    let plain = line_plain_text(&line);
    if plain.trim().is_empty() || display_width(&plain) <= width {
        return vec![line];
    }

    let continuation_indent = continuation_indent_width(&plain);
    let mut out = Vec::new();
    let mut builder = StyledLineBuilder::new(width);
    let mut pending_space = false;

    for span in line.spans {
        let style = span.style;
        for token in styled_tokens(&span.content) {
            match token {
                Token::Space(text) => {
                    if builder.is_empty() && out.is_empty() {
                        builder.push_text(text, style);
                    } else {
                        pending_space = true;
                    }
                }
                Token::Word(text) => {
                    let needed_space = usize::from(pending_space && !builder.is_empty());
                    let word_width = display_width(text);
                    if !builder.is_empty() && builder.width + needed_space + word_width > width {
                        out.push(builder.finish());
                        builder = StyledLineBuilder::with_indent(width, continuation_indent);
                    } else if pending_space && !builder.is_empty() {
                        builder.push_text(" ", style);
                    }
                    push_word(&mut out, &mut builder, text, style, continuation_indent);
                    pending_space = false;
                }
            }
        }
    }

    if !builder.is_empty() || out.is_empty() {
        out.push(builder.finish());
    }
    out
}

fn push_word(
    out: &mut Vec<Line<'static>>,
    builder: &mut StyledLineBuilder,
    word: &str,
    style: Style,
    continuation_indent: usize,
) {
    if display_width(word) <= builder.available_width() {
        builder.push_text(word, style);
        return;
    }

    for ch in word.chars() {
        let ch_width = ch.width().unwrap_or(0);
        if !builder.is_empty() && builder.width + ch_width > builder.max_width {
            out.push(builder.finish());
            *builder = StyledLineBuilder::with_indent(builder.max_width, continuation_indent);
        }
        builder.push_text(&ch.to_string(), style);
    }
}

#[derive(Clone, Copy)]
enum Token<'a> {
    Space(&'a str),
    Word(&'a str),
}

fn styled_tokens(text: &str) -> Vec<Token<'_>> {
    let mut tokens = Vec::new();
    let mut start = 0;
    let mut current_is_space: Option<bool> = None;

    for (idx, ch) in text.char_indices() {
        let is_space = ch.is_whitespace();
        match current_is_space {
            None => current_is_space = Some(is_space),
            Some(kind) if kind == is_space => {}
            Some(kind) => {
                tokens.push(if kind {
                    Token::Space(&text[start..idx])
                } else {
                    Token::Word(&text[start..idx])
                });
                start = idx;
                current_is_space = Some(is_space);
            }
        }
    }

    if let Some(kind) = current_is_space {
        tokens.push(if kind {
            Token::Space(&text[start..])
        } else {
            Token::Word(&text[start..])
        });
    }

    tokens
}

struct StyledLineBuilder {
    spans: Vec<Span<'static>>,
    width: usize,
    max_width: usize,
}

impl StyledLineBuilder {
    fn new(max_width: usize) -> Self {
        Self {
            spans: Vec::new(),
            width: 0,
            max_width,
        }
    }

    fn with_indent(max_width: usize, indent: usize) -> Self {
        let mut builder = Self::new(max_width);
        if indent > 0 {
            builder.push_text(
                &" ".repeat(indent.min(max_width.saturating_sub(1))),
                text_style(),
            );
        }
        builder
    }

    fn is_empty(&self) -> bool {
        self.spans.is_empty()
    }

    fn available_width(&self) -> usize {
        self.max_width.saturating_sub(self.width)
    }

    fn push_text(&mut self, text: &str, style: Style) {
        self.width += display_width(text);
        self.spans.push(Span::styled(text.to_string(), style));
    }

    fn finish(&mut self) -> Line<'static> {
        self.width = 0;
        Line::from(std::mem::take(&mut self.spans))
    }
}

fn continuation_indent_width(text: &str) -> usize {
    let leading = text.chars().take_while(|ch| ch.is_whitespace()).count();
    let trimmed = text.trim_start();
    let quote_prefix_width = if let Some(rest) = trimmed.strip_prefix("| ") {
        let nested = continuation_indent_width(rest);
        return leading + 2 + nested;
    } else {
        0
    };
    if let Some(rest) = trimmed.strip_prefix("- ") {
        return leading + quote_prefix_width + trimmed.len() - rest.len();
    }
    let marker_len = trimmed.chars().take_while(|ch| ch.is_ascii_digit()).count();
    if marker_len > 0 && trimmed.chars().nth(marker_len) == Some('.') {
        return leading + quote_prefix_width + marker_len + 2;
    }
    leading + quote_prefix_width
}

fn line_plain_text(line: &Line<'_>) -> String {
    line.spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect::<String>()
}

fn display_width(text: &str) -> usize {
    UnicodeWidthStr::width(text)
}

fn render_table_lines(table: TableState, width: usize) -> Vec<Line<'static>> {
    let column_count = table
        .rows
        .iter()
        .map(|row| row.cells.len())
        .max()
        .unwrap_or(0)
        .max(table.alignments.len());
    if column_count == 0 {
        return Vec::new();
    }

    let column_widths = table_column_widths(&table.rows, column_count, width);
    let mut out = Vec::new();
    let mut rendered_header_separator = false;

    for (idx, row) in table.rows.iter().enumerate() {
        if !row.is_header && idx > 0 && !rendered_header_separator {
            out.push(table_separator_line(&column_widths));
            rendered_header_separator = true;
        }
        out.push(table_row_line(
            row,
            &table.alignments,
            &column_widths,
            row.is_header,
        ));
    }

    out
}

fn table_column_widths(rows: &[TableRow], column_count: usize, max_width: usize) -> Vec<usize> {
    let gap_width = column_count.saturating_sub(1) * 2;
    let available = max_width.saturating_sub(gap_width).max(column_count);
    let mut widths = vec![3; column_count];

    for row in rows {
        for (idx, cell) in row.cells.iter().enumerate().take(column_count) {
            widths[idx] = widths[idx].max(cell_width(cell));
        }
    }

    while widths.iter().sum::<usize>() > available {
        let Some((idx, width)) = widths
            .iter()
            .copied()
            .enumerate()
            .filter(|(_, width)| *width > 3)
            .max_by_key(|(_, width)| *width)
        else {
            break;
        };
        widths[idx] = width.saturating_sub(1);
    }

    widths
}

fn table_row_line(
    row: &TableRow,
    alignments: &[Alignment],
    column_widths: &[usize],
    is_header: bool,
) -> Line<'static> {
    let mut spans = Vec::new();
    for (idx, width) in column_widths.iter().copied().enumerate() {
        if idx > 0 {
            spans.push(Span::raw("  "));
        }
        let cell = row.cells.get(idx).map(Vec::as_slice).unwrap_or(&[]);
        spans.extend(aligned_table_cell(
            cell,
            width,
            alignments.get(idx).copied().unwrap_or(Alignment::None),
            is_header,
        ));
    }
    Line::from(spans)
}

fn table_separator_line(column_widths: &[usize]) -> Line<'static> {
    let mut spans = Vec::new();
    for (idx, width) in column_widths.iter().copied().enumerate() {
        if idx > 0 {
            spans.push(Span::raw("  "));
        }
        spans.push(Span::styled("-".repeat(width), muted()));
    }
    Line::from(spans)
}

fn aligned_table_cell(
    cell: &[Span<'static>],
    width: usize,
    alignment: Alignment,
    is_header: bool,
) -> Vec<Span<'static>> {
    let content = truncate_table_cell(cell, width, is_header);
    let content_width = spans_width(&content);
    let padding = width.saturating_sub(content_width);
    let (left, right) = match alignment {
        Alignment::Right => (padding, 0),
        Alignment::Center => (padding / 2, padding - (padding / 2)),
        Alignment::None | Alignment::Left => (0, padding),
    };

    let mut spans = Vec::new();
    if left > 0 {
        spans.push(Span::raw(" ".repeat(left)));
    }
    spans.extend(content);
    if right > 0 {
        spans.push(Span::raw(" ".repeat(right)));
    }
    spans
}

fn truncate_table_cell(
    cell: &[Span<'static>],
    width: usize,
    is_header: bool,
) -> Vec<Span<'static>> {
    let mut remaining = width;
    let mut out = Vec::new();

    for span in cell {
        if remaining == 0 {
            break;
        }
        let mut text = String::new();
        let mut text_width = 0;
        for ch in span.content.chars() {
            let ch_width = ch.width().unwrap_or(0);
            if text_width + ch_width > remaining {
                break;
            }
            text.push(ch);
            text_width += ch_width;
        }
        if !text.is_empty() {
            let style = if is_header && span.style == text_style() {
                markdown_strong()
            } else {
                span.style
            };
            out.push(Span::styled(text, style));
            remaining = remaining.saturating_sub(text_width);
        }
    }

    out
}

fn cell_width(cell: &[Span<'static>]) -> usize {
    cell.iter()
        .map(|span| display_width(span.content.as_ref()))
        .sum()
}

fn spans_width(spans: &[Span<'static>]) -> usize {
    spans
        .iter()
        .map(|span| display_width(span.content.as_ref()))
        .sum()
}

fn heading_text_style(level: HeadingLevel) -> Style {
    match level {
        HeadingLevel::H1 | HeadingLevel::H2 | HeadingLevel::H3 => markdown_heading(),
        HeadingLevel::H4 | HeadingLevel::H5 | HeadingLevel::H6 => text_style(),
    }
}

fn looks_like_bare_link(value: &str) -> bool {
    value.starts_with("http://") || value.starts_with("https://") || value.starts_with("file://")
}

fn looks_like_path(value: &str) -> bool {
    value.starts_with('/') || value.starts_with("~/") || value.starts_with("./")
}

fn should_style_inline_code(value: &str) -> bool {
    let value = value.trim();
    if value.is_empty() {
        return false;
    }
    if !value.chars().any(char::is_whitespace) {
        return true;
    }
    if starts_with_common_command(value) {
        return true;
    }
    value.chars().any(is_inline_code_delimiter)
}

fn starts_with_common_command(value: &str) -> bool {
    let command = value.split_whitespace().next().unwrap_or_default();
    matches!(
        command,
        "awk"
            | "brew"
            | "bun"
            | "cargo"
            | "cat"
            | "cd"
            | "curl"
            | "deno"
            | "docker"
            | "gh"
            | "git"
            | "go"
            | "grep"
            | "just"
            | "kubectl"
            | "make"
            | "node"
            | "npm"
            | "npx"
            | "pnpm"
            | "python"
            | "python3"
            | "rg"
            | "sed"
            | "sqlite3"
            | "task"
            | "uv"
            | "yarn"
    )
}

fn is_inline_code_delimiter(ch: char) -> bool {
    matches!(
        ch,
        '/' | '\\'
            | '$'
            | '`'
            | '{'
            | '}'
            | '['
            | ']'
            | '('
            | ')'
            | '<'
            | '>'
            | '='
            | '|'
            | '&'
            | ';'
    )
}

fn code_block_language(kind: CodeBlockKind<'_>) -> Option<String> {
    match kind {
        CodeBlockKind::Fenced(language) => {
            let language = language.as_ref().trim();
            (!language.is_empty()).then(|| language.to_string())
        }
        CodeBlockKind::Indented => None,
    }
}

fn plain_code_lines(code: &str) -> Vec<Line<'static>> {
    code.lines()
        .map(|line| Line::from(Span::styled(line.to_string(), markdown_code_block())))
        .collect()
}

const MAX_HIGHLIGHT_BYTES: usize = 512 * 1024;
const MAX_HIGHLIGHT_LINES: usize = 10_000;

fn highlight_code_to_lines(code: &str, language_hint: Option<&str>) -> Option<Vec<Line<'static>>> {
    if code.len() > MAX_HIGHLIGHT_BYTES || code.lines().count() > MAX_HIGHLIGHT_LINES {
        return None;
    }
    let syntax = language_hint.and_then(find_syntax)?;
    let mut highlighter = HighlightLines::new(syntax, syntax_theme());
    let mut lines = Vec::new();

    for line in LinesWithEndings::from(code) {
        lines.push(Line::from(highlight_line_with(&mut highlighter, line)?));
    }

    (!lines.is_empty()).then_some(lines)
}

fn highlight_line_with(
    highlighter: &mut HighlightLines<'_>,
    line: &str,
) -> Option<Vec<Span<'static>>> {
    let ranges = highlighter.highlight_line(line, syntax_set()).ok()?;
    let mut spans = Vec::new();
    for (style, text) in ranges {
        let text = text.trim_end_matches(['\n', '\r']);
        if !text.is_empty() {
            spans.push(Span::styled(text.to_string(), convert_syntect_style(style)));
        }
    }
    Some(spans)
}

fn syntax_set() -> &'static SyntaxSet {
    static SYNTAX_SET: OnceLock<SyntaxSet> = OnceLock::new();
    SYNTAX_SET.get_or_init(two_face::syntax::extra_newlines)
}

fn syntax_theme() -> &'static Theme {
    static THEME: OnceLock<Theme> = OnceLock::new();
    THEME.get_or_init(|| {
        let name = if crate::theme::is_light() {
            EmbeddedThemeName::CatppuccinLatte
        } else {
            EmbeddedThemeName::CatppuccinMocha
        };
        two_face::theme::extra().get(name).clone()
    })
}

fn find_syntax(lang: &str) -> Option<&'static SyntaxReference> {
    if is_plain_text_language(lang) {
        return None;
    }
    let syntax_set = syntax_set();
    syntax_candidates(lang).into_iter().find_map(|candidate| {
        if is_plain_text_language(&candidate) {
            return None;
        }
        syntax_set
            .find_syntax_by_token(&candidate)
            .or_else(|| syntax_set.find_syntax_by_name(&candidate))
            .or_else(|| syntax_set.find_syntax_by_extension(&candidate))
            .or_else(|| {
                let lower = candidate.to_ascii_lowercase();
                syntax_set
                    .syntaxes()
                    .iter()
                    .find(|syntax| syntax.name.to_ascii_lowercase() == lower)
            })
    })
}

fn syntax_candidates(raw: &str) -> Vec<String> {
    let mut candidates = Vec::new();
    for token in raw.split([',', ' ', '\t']) {
        add_syntax_candidate(token, &mut candidates);
    }
    candidates
}

fn add_syntax_candidate(raw: &str, candidates: &mut Vec<String>) {
    let token = clean_syntax_token(raw);
    if token.is_empty() {
        return;
    }
    if let Some((key, value)) = token.split_once('=') {
        if matches!(key.to_ascii_lowercase().as_str(), "lang" | "language") {
            add_syntax_candidate(value, candidates);
        }
        return;
    }
    let token = token
        .strip_prefix("language-")
        .or_else(|| token.strip_prefix("lang-"))
        .unwrap_or(&token)
        .trim_start_matches('.');
    if token.is_empty() {
        return;
    }
    push_candidate(candidates, token);
    if let Some(alias) = syntax_alias(token) {
        push_candidate(candidates, alias);
    }
}

fn clean_syntax_token(raw: &str) -> String {
    raw.trim()
        .trim_matches(['{', '}', '[', ']', '(', ')', '"', '\'', '`'])
        .trim_start_matches('.')
        .trim()
        .to_ascii_lowercase()
}

fn push_candidate(candidates: &mut Vec<String>, candidate: &str) {
    let candidate = candidate.trim();
    if candidate.is_empty() || candidates.iter().any(|existing| existing == candidate) {
        return;
    }
    candidates.push(candidate.to_string());
}

fn syntax_alias(token: &str) -> Option<&'static str> {
    match token {
        "c++" | "cpp" | "cc" | "cxx" | "hpp" | "hxx" => Some("c++"),
        "c#" | "csharp" | "c-sharp" | "cs" => Some("c#"),
        "golang" => Some("go"),
        "py" | "python3" | "py3" | "python-repl" => Some("python"),
        "shell" | "sh" | "zsh" | "fish" | "ksh" => Some("bash"),
        "ps1" | "powershell" => Some("powershell"),
        "javascript" | "js" | "jsx" | "node" | "mjs" | "cjs" => Some("javascript"),
        "typescript" | "ts" | "tsx" => Some("typescript"),
        "jsonc" | "json5" | "geojson" => Some("json"),
        "yml" => Some("yaml"),
        "md" | "mdx" => Some("markdown"),
        "rb" => Some("ruby"),
        "rs" => Some("rust"),
        "kt" | "kts" => Some("kotlin"),
        "ex" | "exs" => Some("elixir"),
        "erl" | "hrl" => Some("erlang"),
        "fs" | "fsi" | "fsx" => Some("f#"),
        "hs" => Some("haskell"),
        "jl" => Some("julia"),
        "lua" => Some("lua"),
        "nim" => Some("nim"),
        "pl" | "pm" => Some("perl"),
        "r" | "rscript" => Some("r"),
        "scala" | "sc" => Some("scala"),
        "swift" => Some("swift"),
        "tf" | "tfvars" | "hcl" => Some("terraform"),
        "docker" | "containerfile" => Some("dockerfile"),
        "make" | "mk" => Some("makefile"),
        "sql" | "pgsql" | "postgres" | "postgresql" | "mysql" | "sqlite" => Some("sql"),
        "html" | "htm" | "xhtml" => Some("html"),
        "css" | "scss" | "sass" | "less" => Some("css"),
        "xml" | "svg" | "plist" => Some("xml"),
        "diff" | "patch" => Some("diff"),
        "toml" | "lock" => Some("toml"),
        _ => None,
    }
}

fn is_plain_text_language(lang: &str) -> bool {
    matches!(
        clean_syntax_token(lang).as_str(),
        "text" | "txt" | "plain" | "plaintext" | "none" | "nohighlight" | "no-highlight"
    )
}

fn convert_syntect_style(style: SyntectStyle) -> Style {
    let mut converted = markdown_code_block().fg(Color::Rgb(
        style.foreground.r,
        style.foreground.g,
        style.foreground.b,
    ));

    if style.font_style.contains(FontStyle::BOLD) {
        converted = converted.add_modifier(Modifier::BOLD);
    }
    if style.font_style.contains(FontStyle::ITALIC) {
        converted = converted.add_modifier(Modifier::ITALIC);
    }
    if style.font_style.contains(FontStyle::UNDERLINE) {
        converted = converted.add_modifier(Modifier::UNDERLINED);
    }
    converted
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::Modifier;

    fn plain(lines: &[Line<'static>]) -> String {
        lines
            .iter()
            .map(line_plain_text)
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn strips_markdown_markers_but_keeps_readable_text() {
        let lines = render_markdown_lines(
            "This is **important** with `coupon.json`.\n\n- [Example](https://example.com)",
            80,
        );
        let text = plain(&lines);
        assert!(text.contains("This is important with coupon.json."));
        assert!(text.contains("- Example (https://example.com)"));
        assert!(!text.contains("**important**"));
        assert!(!text.contains("`coupon.json`"));
    }

    #[test]
    fn inline_code_urls_and_paths_have_distinct_styles() {
        let lines = render_markdown_lines(
            "`@browser-use`\n\nhttps://example.com\n\n./Cargo.toml\n\n/in/reaganh",
            80,
        );
        assert!(lines.iter().any(|line| {
            line.spans
                .iter()
                .any(|span| span.content == "@browser-use" && span.style == markdown_code())
        }));
        assert!(lines.iter().any(|line| {
            line.spans
                .iter()
                .any(|span| span.content == "https://example.com" && span.style == link())
        }));
        assert!(lines.iter().any(|line| {
            line.spans
                .iter()
                .any(|span| span.content == "./Cargo.toml" && span.style == path_reference())
        }));
        assert!(lines.iter().any(|line| {
            line.spans
                .iter()
                .any(|span| span.content == "/in/reaganh" && span.style == path_reference())
        }));
    }

    #[test]
    fn inline_code_plain_language_phrases_render_as_plain_text() {
        let lines = render_markdown_lines("- `About LLMs at Zig Days` -", 80);
        assert_eq!(plain(&lines), "- About LLMs at Zig Days -");
        assert!(lines[0].spans.iter().any(|span| {
            span.content == "About LLMs at Zig Days" && span.style == text_style()
        }));
        assert!(!lines[0]
            .spans
            .iter()
            .any(|span| { span.content.contains("About LLMs") && span.style == markdown_code() }));
    }

    #[test]
    fn inline_code_does_not_highlight_padding_spaces() {
        let lines = render_markdown_lines("` About LLMs`", 80);
        assert_eq!(plain(&lines), " About LLMs");
        assert!(lines[0]
            .spans
            .iter()
            .any(|span| span.content == " " && span.style == text_style()));
        assert!(!lines[0]
            .spans
            .iter()
            .any(|span| span.content.starts_with(' ') && span.style == markdown_code()));
    }

    #[test]
    fn inline_code_commands_keep_code_style() {
        let lines = render_markdown_lines("Run `cargo test -p browser-use-tui`.", 80);
        assert!(lines[0].spans.iter().any(|span| {
            span.content == "cargo test -p browser-use-tui" && span.style == markdown_code()
        }));
    }

    #[test]
    fn code_fences_do_not_render_language_label_artifacts() {
        let lines = render_markdown_lines("```bash\ncargo test\n```\n", 80);
        let text = plain(&lines);
        assert!(text.contains("cargo test"));
        assert!(!text.contains("code bash"));
        assert!(!text.contains("```"));
    }

    #[test]
    fn known_code_fences_are_syntax_highlighted() {
        let lines =
            render_markdown_lines("```rust\nfn main() {\n    let url = \"x\";\n}\n```\n", 80);
        let text = plain(&lines);
        assert!(text.contains("fn main()"));
        assert!(lines.iter().any(|line| {
            line.spans.iter().any(|span| {
                span.content == "fn"
                    && span
                        .style
                        .fg
                        .is_some_and(|color| color != crate::theme::text())
            })
        }));
        let colors = lines
            .iter()
            .flat_map(|line| line.spans.iter().filter_map(|span| span.style.fg))
            .collect::<Vec<_>>();
        assert!(colors
            .first()
            .is_some_and(|first| colors.iter().any(|color| color != first)));
    }

    #[test]
    fn code_fence_language_aliases_and_language_metadata_are_highlighted() {
        let alias_lines = render_markdown_lines("```rs\nfn main() {}\n```\n", 80);
        assert!(has_multiple_code_colors(&alias_lines));

        let language_hint_lines =
            render_markdown_lines("```language=rust\nfn main() {}\n```\n", 80);
        assert!(has_multiple_code_colors(&language_hint_lines));

        let file_hint_only_lines = render_markdown_lines(
            "```filename=crates/browser-use-tui/src/main.rs\nfn main() {}\n```\n",
            80,
        );
        assert!(!has_multiple_code_colors(&file_hint_only_lines));
    }

    #[test]
    fn unlabeled_structured_code_blocks_use_plain_code_style() {
        let json_lines = render_markdown_lines(
            "```\n{\"name\":\"browser-use-terminal\",\"count\":2}\n```\n",
            80,
        );
        assert_eq!(
            plain(&json_lines),
            "{\"name\":\"browser-use-terminal\",\"count\":2}"
        );
        assert!(json_lines[0]
            .spans
            .iter()
            .all(|span| span.style.fg == markdown_code_block().fg));
    }

    #[test]
    fn plain_text_code_fences_do_not_syntax_highlight() {
        let lines = render_markdown_lines("```text\ncrates/\n  browser-use-tui\n```\n", 80);
        assert_eq!(plain(&lines), "crates/\n  browser-use-tui");
        assert!(lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .all(|span| span.style.fg == markdown_code_block().fg));
    }

    #[test]
    fn unknown_code_fences_fall_back_to_plain_code_style() {
        let lines = render_markdown_lines("```not-a-real-language\nhello\n```\n", 80);
        assert_eq!(plain(&lines), "hello");
        assert!(lines[0]
            .spans
            .iter()
            .all(|span| span.style.fg == markdown_code_block().fg));
    }

    #[test]
    fn headings_render_as_text_not_source_markers() {
        let lines = render_markdown_lines("### What it does\n\nBody", 80);
        let text = plain(&lines);
        assert!(text.contains("What it does"));
        assert!(!text.contains("###"));
    }

    #[test]
    fn blockquotes_render_as_quote_blocks_not_source_markers() {
        let lines = render_markdown_lines(
            "> Quote with **formatting**\n> - quoted list item\n>\n> > nested quote",
            80,
        );
        let text = plain(&lines);
        assert!(text.contains("| Quote with formatting"));
        assert!(text.contains("| - quoted list item"));
        assert!(text.contains("| | nested quote"));
        assert!(!text.contains("> Quote"));
    }

    #[test]
    fn tables_render_as_aligned_rows_not_pipe_source() {
        let lines = render_markdown_lines(
            "| Name | Count |\n| --- | ---: |\n| **Apples** | `12` |\n| Pears | 3 |",
            80,
        );
        let text = plain(&lines);
        assert!(text.contains("Name"), "{text:?}");
        assert!(text.contains("Count"));
        assert!(text.contains("Apples"));
        assert!(text.contains("12"));
        assert!(!text.contains("| Name | Count |"));
        assert!(!text.contains("---:"));
    }

    #[test]
    fn wraps_lists_without_losing_marker_or_span_styles() {
        let lines = render_markdown_lines(
            "- **browser-use-terminal** keeps state while a long sentence wraps cleanly",
            34,
        );
        let text = plain(&lines);
        assert!(text.starts_with("- browser-use-terminal keeps"));
        assert!(text.contains("\n  while a long sentence wraps"));
        let first = &lines[0];
        assert!(first
            .spans
            .iter()
            .any(|span| span.content == "browser-use-terminal"
                && span.style.add_modifier.contains(Modifier::BOLD)));
    }

    fn has_multiple_code_colors(lines: &[Line<'static>]) -> bool {
        let colors = lines
            .iter()
            .flat_map(|line| line.spans.iter().filter_map(|span| span.style.fg))
            .collect::<Vec<_>>();
        colors
            .first()
            .is_some_and(|first| colors.iter().any(|color| color != first))
    }
}
