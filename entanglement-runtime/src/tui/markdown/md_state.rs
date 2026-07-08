use pulldown_cmark::{Alignment, Event, Tag, TagEnd};
use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
};

use super::MarkdownRenderer;

/// One in-flight list. Ordered lists carry a running item counter.
struct ListState {
    ordered: bool,
    next: usize,
}

/// Mutable walk state for a single `render` call. Borrows the renderer only to
/// delegate fenced-code highlighting to syntect; every span it builds is owned
/// (`'static`), so the produced `Text` is `'static` and untied from the input
/// or renderer lifetimes.
pub(super) struct RenderState<'r> {
    renderer: &'r MarkdownRenderer,
    lines: Vec<Line<'static>>,
    cur: Vec<Span<'static>>,
    bold: u32,
    italic: u32,
    strike: u32,
    quote_depth: usize,
    list_stack: Vec<ListState>,
    in_code: bool,
    code_lang: String,
    code_buf: String,
    in_cell: bool,
    table_aligns: Vec<Alignment>,
    table_cell: String,
    table_row: Vec<String>,
    table_rows: Vec<Vec<String>>,
    table_next_is_header: bool,
    table_header_present: bool,
}

impl<'r> RenderState<'r> {
    pub(super) fn new(renderer: &'r MarkdownRenderer) -> Self {
        Self {
            renderer,
            lines: Vec::new(),
            cur: Vec::new(),
            bold: 0,
            italic: 0,
            strike: 0,
            quote_depth: 0,
            list_stack: Vec::new(),
            in_code: false,
            code_lang: String::new(),
            code_buf: String::new(),
            in_cell: false,
            table_aligns: Vec::new(),
            table_cell: String::new(),
            table_row: Vec::new(),
            table_rows: Vec::new(),
            table_next_is_header: false,
            table_header_present: false,
        }
    }

    pub(super) fn handle(&mut self, event: Event<'_>) {
        match event {
            Event::Start(tag) => self.start_tag(tag),
            Event::End(end) => self.end_tag(end),
            Event::Text(text) => self.push_text(&text),
            Event::Code(text) => {
                if self.in_cell {
                    self.table_cell.push_str(&text);
                } else {
                    let mut s = Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::ITALIC);
                    if self.bold > 0 {
                        s = s.add_modifier(Modifier::BOLD);
                    }
                    self.cur.push(Span::styled(format!("`{text}`"), s));
                }
            }
            // Soft breaks flush a new line so the model's intentional line
            // breaks survive — joining them with a space (CommonMark's render
            // choice) collapses multi-line prose into one run and wrecks the
            // structure the model authored.
            Event::SoftBreak | Event::HardBreak => {
                if self.in_cell {
                    self.table_cell.push(' ');
                } else if self.in_code {
                    self.code_buf.push('\n');
                } else {
                    self.flush();
                }
            }
            Event::Rule => {
                self.flush();
                self.lines.push(Line::from("─".repeat(40)));
            }
            Event::TaskListMarker(checked) => {
                let marker = if checked { "☑ " } else { "☐ " };
                self.cur.push(Span::raw(marker));
            }
            Event::FootnoteReference(name) => {
                self.cur
                    .push(Span::styled(format!("[^{name}]"), Style::default().dim()));
            }
            Event::Html(html) | Event::InlineHtml(html) => self.push_text(&html),
            Event::InlineMath(m) | Event::DisplayMath(m) => self.push_text(&m),
        }
    }

    fn push_text(&mut self, text: &str) {
        if self.in_code {
            self.code_buf.push_str(text);
        } else if self.in_cell {
            self.table_cell.push_str(text);
        } else {
            self.cur
                .push(Span::styled(text.to_string(), self.current_style()));
        }
    }

    fn current_style(&self) -> Style {
        let mut s = Style::default();
        if self.bold > 0 {
            s = s.add_modifier(Modifier::BOLD);
        }
        if self.italic > 0 {
            s = s.add_modifier(Modifier::ITALIC);
        }
        if self.strike > 0 {
            s = s.add_modifier(Modifier::CROSSED_OUT);
        }
        s
    }

    /// Commit `cur` as one visual line, prefixed with the blockquote bar when
    /// nested. Continuation lines (from soft breaks) re-apply the prefix so a
    /// multi-line quote stays visually grouped.
    fn flush(&mut self) {
        let prefix = self.quote_prefix();
        let mut spans: Vec<Span<'static>> = Vec::with_capacity(self.cur.len() + 2);
        if !prefix.is_empty() {
            spans.push(Span::raw(prefix));
        }
        spans.append(&mut self.cur);
        self.lines.push(Line::from(spans));
    }

    fn quote_prefix(&self) -> String {
        if self.quote_depth == 0 {
            String::new()
        } else {
            let pad = "  ".repeat(self.quote_depth - 1);
            format!("{pad}▌ ")
        }
    }

    fn blank(&mut self) {
        self.lines.push(Line::from(""));
    }

    fn start_tag(&mut self, tag: Tag<'_>) {
        match tag {
            Tag::Paragraph => {}
            Tag::Heading { level, .. } => {
                let prefix = "#".repeat(level as usize);
                self.cur
                    .push(Span::styled(format!("{prefix} "), Style::default().bold()));
                self.bold = self.bold.saturating_add(1);
            }
            Tag::BlockQuote(_) => self.quote_depth += 1,
            Tag::CodeBlock(kind) => {
                self.in_code = true;
                self.code_buf.clear();
                self.code_lang = match kind {
                    pulldown_cmark::CodeBlockKind::Fenced(lang) => lang.to_string(),
                    pulldown_cmark::CodeBlockKind::Indented => String::new(),
                };
            }
            Tag::List(number) => {
                let state = match number {
                    Some(n) => ListState {
                        ordered: true,
                        next: n as usize,
                    },
                    None => ListState {
                        ordered: false,
                        next: 0,
                    },
                };
                self.list_stack.push(state);
            }
            Tag::Item => {
                let depth = self.list_stack.len();
                let indent = "  ".repeat(depth.saturating_sub(1));
                if let Some(top) = self.list_stack.last_mut() {
                    let marker = if top.ordered {
                        let m = format!("{}. ", top.next);
                        top.next += 1;
                        m
                    } else {
                        "• ".to_string()
                    };
                    self.cur.push(Span::raw(format!("{indent}{marker}")));
                }
            }
            Tag::Emphasis => self.italic = self.italic.saturating_add(1),
            Tag::Strong => self.bold = self.bold.saturating_add(1),
            Tag::Strikethrough => self.strike = self.strike.saturating_add(1),
            Tag::Link { .. } | Tag::Image { .. } => {}
            Tag::Table(aligns) => {
                self.table_aligns = aligns;
                self.table_rows.clear();
                self.table_header_present = false;
            }
            Tag::TableHead => {
                self.table_next_is_header = true;
                self.table_row.clear();
            }
            Tag::TableRow => {
                self.table_next_is_header = false;
                self.table_row.clear();
            }
            Tag::TableCell => {
                self.in_cell = true;
                self.table_cell.clear();
            }
            _ => {}
        }
    }

    fn end_tag(&mut self, end: TagEnd) {
        match end {
            TagEnd::Paragraph => {
                if !self.cur.is_empty() {
                    self.flush();
                }
                self.blank();
            }
            TagEnd::Heading(_) => {
                if !self.cur.is_empty() {
                    self.flush();
                }
                self.bold = self.bold.saturating_sub(1);
                self.blank();
            }
            TagEnd::BlockQuote(_) => {
                if !self.cur.is_empty() {
                    self.flush();
                }
                self.quote_depth = self.quote_depth.saturating_sub(1);
                self.blank();
            }
            TagEnd::CodeBlock => {
                self.in_code = false;
                let highlighted = self
                    .renderer
                    .highlight_code(&self.code_lang, &self.code_buf);
                for line in highlighted.lines {
                    self.lines.push(line);
                }
                self.blank();
                self.code_lang.clear();
                self.code_buf.clear();
            }
            TagEnd::List(_) => {
                self.list_stack.pop();
                if self.list_stack.is_empty() {
                    self.blank();
                }
            }
            TagEnd::Item => {
                if !self.cur.is_empty() {
                    self.flush();
                }
            }
            TagEnd::Emphasis => self.italic = self.italic.saturating_sub(1),
            TagEnd::Strong => self.bold = self.bold.saturating_sub(1),
            TagEnd::Strikethrough => self.strike = self.strike.saturating_sub(1),
            TagEnd::Link | TagEnd::Image => {}
            TagEnd::TableCell => {
                self.in_cell = false;
                self.table_row.push(self.table_cell.clone());
            }
            TagEnd::TableHead | TagEnd::TableRow => {
                if matches!(end, TagEnd::TableHead) {
                    self.table_header_present = true;
                }
                self.table_rows.push(self.table_row.clone());
                self.table_row.clear();
            }
            TagEnd::Table => {
                let grid = render_table_grid(
                    &self.table_rows,
                    &self.table_aligns,
                    self.table_header_present,
                );
                for line in grid {
                    self.lines.push(line);
                }
                self.blank();
            }
            _ => {}
        }
    }

    pub(super) fn finish(mut self) -> Text<'static> {
        if !self.cur.is_empty() {
            self.flush();
        }
        Text::from(self.lines)
    }
}

/// Lay out buffered table rows as a full-width pipe grid. Column widths track
/// the widest cell so alignment is exact; overflow is not truncated (the user
/// opted to let the terminal scroll rather than clip cell content).
fn render_table_grid(
    rows: &[Vec<String>],
    aligns: &[Alignment],
    header_present: bool,
) -> Vec<Line<'static>> {
    if rows.is_empty() {
        return Vec::new();
    }
    let ncols = aligns.len().max(1);
    let mut widths = vec![1usize; ncols];
    for row in rows {
        for (i, cell) in row.iter().enumerate() {
            if i < ncols {
                widths[i] = widths[i].max(cell.chars().count().max(1));
            }
        }
    }

    let mut out = Vec::new();
    for (ri, row) in rows.iter().enumerate() {
        out.push(format_row(row, &widths, aligns));
        if header_present && ri == 0 {
            let mut spans = vec![Span::raw("|")];
            for w in &widths {
                spans.push(Span::raw(format!(" {} |", "-".repeat(*w))));
            }
            out.push(Line::from(spans));
        }
    }
    out
}

fn format_row(row: &[String], widths: &[usize], aligns: &[Alignment]) -> Line<'static> {
    let mut spans = vec![Span::raw("|")];
    for (i, cell) in row.iter().enumerate() {
        let w = widths.get(i).copied().unwrap_or(1);
        let a = aligns.get(i).cloned().unwrap_or(Alignment::None);
        spans.push(Span::raw(format!(" {} |", pad_cell(cell, a, w))));
    }
    Line::from(spans)
}

fn pad_cell(cell: &str, align: Alignment, w: usize) -> String {
    let len = cell.chars().count();
    if len >= w {
        return cell.to_string();
    }
    let pad = w - len;
    match align {
        Alignment::Right => format!("{}{}", " ".repeat(pad), cell),
        Alignment::Center => {
            let left = pad / 2;
            let right = pad - left;
            format!("{}{}{}", " ".repeat(left), cell, " ".repeat(right))
        }
        Alignment::None | Alignment::Left => format!("{}{}", cell, " ".repeat(pad)),
    }
}
