use pulldown_cmark::{Options, Parser};
use ratatui::{
    style::{Color, Style},
    text::{Line, Span, Text},
};
use syntect::{
    highlighting::{Theme, ThemeSet},
    parsing::SyntaxSet,
};

mod md_state;

use md_state::RenderState;

#[derive(Clone)]
pub struct MarkdownRenderer {
    syntax_set: SyntaxSet,
    theme: Theme,
}

impl MarkdownRenderer {
    pub fn new() -> Self {
        let syntax_set = SyntaxSet::load_defaults_newlines();
        let theme_set = ThemeSet::load_defaults();
        let theme = theme_set.themes["base16-ocean.dark"].clone();
        Self { syntax_set, theme }
    }

    /// Parse CommonMark + GFM (tables, strikethrough, task lists, footnotes,
    /// GitHub alerts) and render to a styled `Text`. The returned `Text`
    /// borrows from `&self` by convention (matching ratatui's `Text<'_>` idiom);
    /// every span is built from owned `String`s, so the borrow is nominal and
    /// the value is freely storable for any lifetime the caller needs.
    pub fn render(&self, markdown: &str) -> Text<'_> {
        if markdown.trim().is_empty() {
            return Text::default();
        }

        let opts = Options::ENABLE_TABLES
            | Options::ENABLE_STRIKETHROUGH
            | Options::ENABLE_TASKLISTS
            | Options::ENABLE_FOOTNOTES
            | Options::ENABLE_GFM;

        // Some models emit "tables" as pipe rows WITHOUT the GFM separator row
        // pulldown-cmark requires to parse a `Tag::Table` — those then render
        // as a plain paragraph (the "table shows as text" bug). Normalize such
        // loose-table runs into well-formed tables before parsing. Fenced code
        // is skipped so pasted source isn't mangled.
        let normalized = normalize_loose_tables(markdown);

        let parser = Parser::new_ext(&normalized, opts);
        let mut state = RenderState::new(self);
        for event in parser {
            state.handle(event);
        }
        state.finish()
    }

    /// Highlight a fenced code block via syntect. `pub(super)` so the state
    /// machine in `md_state` can delegate here without exposing syntect to the
    /// rest of the crate.
    pub(super) fn highlight_code(&self, language: &str, code: &str) -> Text<'static> {
        let syntax = self
            .syntax_set
            .find_syntax_by_token(language)
            .or_else(|| self.syntax_set.find_syntax_by_extension(language))
            .unwrap_or_else(|| self.syntax_set.find_syntax_plain_text());

        let mut highlighter = syntect::easy::HighlightLines::new(syntax, &self.theme);
        let mut lines = Vec::new();

        for line in code.lines() {
            let ranges = highlighter
                .highlight_line(line, &self.syntax_set)
                .unwrap_or_default();
            let spans: Vec<Span> = ranges
                .into_iter()
                .map(|(style, text)| {
                    let fg = Color::Rgb(style.foreground.r, style.foreground.g, style.foreground.b);
                    Span::styled(text.to_string(), Style::default().fg(fg))
                })
                .collect();

            lines.push(Line::from(spans));
        }

        Text::from(lines)
    }
}

impl Default for MarkdownRenderer {
    fn default() -> Self {
        Self::new()
    }
}

/// Heuristic pre-pass: detect "loose tables" — 2+ consecutive `|`-prefixed rows
/// with no GFM separator — and inject a separator after the header so pulldown-cmark
/// parses them as `Tag::Table` (otherwise they render as a plain paragraph).
///
/// Fenced code blocks (``` / ~~~) are skipped so source containing pipes isn't
/// rewritten. Conservative by design: a row must start with `|`, and a run is
/// only patched when it lacks any separator line — well-formed tables pass through
/// untouched.
fn normalize_loose_tables(markdown: &str) -> String {
    let lines: Vec<&str> = markdown.split('\n').collect();
    let mut out: Vec<String> = Vec::with_capacity(lines.len());
    let mut in_fence = false;
    let mut i = 0;

    while i < lines.len() {
        let line = lines[i];

        if is_fence_line(line) {
            in_fence = !in_fence;
            out.push(line.to_string());
            i += 1;
            continue;
        }
        if in_fence {
            out.push(line.to_string());
            i += 1;
            continue;
        }

        if is_table_row_line(line) {
            let start = i;
            while i < lines.len() && is_table_row_line(lines[i]) {
                i += 1;
            }
            let run = &lines[start..i];
            let has_sep = run.iter().any(|l| is_separator_line(l));
            if !has_sep && run.len() >= 2 {
                let ncols = count_cells(run[0]);
                out.push(run[0].to_string());
                out.push(make_separator(ncols));
                out.extend(run[1..].iter().map(|l| l.to_string()));
            } else {
                out.extend(run.iter().map(|l| l.to_string()));
            }
        } else {
            out.push(line.to_string());
            i += 1;
        }
    }

    out.join("\n")
}

fn is_fence_line(line: &str) -> bool {
    let t = line.trim_start();
    t.starts_with("```") || t.starts_with("~~~")
}

fn is_table_row_line(line: &str) -> bool {
    let t = line.trim();
    if !t.starts_with('|') {
        return false;
    }
    // needs at least one more pipe (cell delimiter or trailing) past the opener
    t[1..].contains('|')
}

fn is_separator_line(line: &str) -> bool {
    let t = line.trim();
    if t.is_empty() {
        return false;
    }
    // strip pipes, then the remainder must be only [-:space] with at least one dash
    let body: String = t.chars().filter(|&c| c != '|').collect();
    let body = body.trim();
    body.chars().any(|c| c == '-') && body.chars().all(|c| c == '-' || c == ':' || c == ' ')
}

fn count_cells(row: &str) -> usize {
    row.trim().matches('|').count().saturating_sub(1).max(1)
}

fn make_separator(ncols: usize) -> String {
    let mut s = String::from("|");
    for _ in 0..ncols.max(1) {
        s.push_str(" --- |");
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::Modifier;

    fn render_str(md: &str) -> String {
        let renderer = MarkdownRenderer::new();
        let result = renderer.render(md);
        result
            .lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .map(|span| span.content.as_ref())
            .collect()
    }

    #[test]
    fn test_render_plain_text() {
        let renderer = MarkdownRenderer::new();
        let result = renderer.render("Hello, world!");
        assert!(!result.lines.is_empty());
    }

    #[test]
    fn test_render_plain_text_no_word_duplication() {
        assert_eq!(
            render_str("one two three four").trim(),
            "one two three four"
        );
    }

    #[test]
    fn test_render_code_block() {
        let renderer = MarkdownRenderer::new();
        let result = renderer.render("```rust\nfn main() {}\n```");
        assert!(!result.lines.is_empty());
    }

    #[test]
    fn test_render_inline_code() {
        let renderer = MarkdownRenderer::new();
        let result = renderer.render("`inline code`");
        assert!(!result.lines.is_empty());
    }

    #[test]
    fn test_render_heading() {
        let renderer = MarkdownRenderer::new();
        let result = renderer.render("# Heading 1");
        assert!(!result.lines.is_empty());
    }

    #[test]
    fn test_render_list() {
        let renderer = MarkdownRenderer::new();
        let result = renderer.render("- Item 1\n- Item 2");
        assert!(!result.lines.is_empty());
    }

    #[test]
    fn soft_breaks_preserve_each_line() {
        // Regression for the "split by whitespace and joined with space" bug:
        // three source lines must render as three content lines, not one.
        let renderer = MarkdownRenderer::new();
        let result = renderer.render("line one\nline two\nline three");
        let non_empty = result.lines.iter().filter(|l| !l.spans.is_empty()).count();
        assert_eq!(non_empty, 3, "expected 3 lines, got {non_empty}");
    }

    #[test]
    fn bold_and_italic_apply_style_modifiers() {
        let renderer = MarkdownRenderer::new();
        let result = renderer.render("**bold** and *italic*");
        let modifiers: Vec<Modifier> = result
            .lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .map(|s| s.style.add_modifier)
            .collect();
        assert!(
            modifiers.iter().any(|m| m.contains(Modifier::BOLD)),
            "expected a bold span, got {modifiers:?}"
        );
        assert!(
            modifiers.iter().any(|m| m.contains(Modifier::ITALIC)),
            "expected an italic span, got {modifiers:?}"
        );
    }

    #[test]
    fn strikethrough_applies_crossed_out() {
        let renderer = MarkdownRenderer::new();
        let result = renderer.render("~~removed~~");
        let has_strike = result
            .lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .any(|s| s.style.add_modifier.contains(Modifier::CROSSED_OUT));
        assert!(has_strike, "expected a struck-through span");
    }

    #[test]
    fn table_renders_pipe_grid_with_separator() {
        let renderer = MarkdownRenderer::new();
        let md = "| name | role |\n| --- | --- |\n| holly | engine |\n| tui | head |";
        let result = renderer.render(md);

        let joined: String = result
            .lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .map(|s| s.content.as_ref())
            .collect::<String>();
        assert!(
            joined.contains('|'),
            "table rows should contain pipes: {joined}"
        );
        assert!(
            result.lines.iter().any(|l| {
                let s: String = l
                    .spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>();
                s.contains("---")
            }),
            "expected a dashed separator row after the header"
        );
        assert!(
            result.lines.iter().any(|l| {
                let s: String = l
                    .spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>();
                s.contains("holly")
            }),
            "expected a row containing 'holly'"
        );
    }

    #[test]
    fn blockquote_renders_with_quote_bar() {
        let renderer = MarkdownRenderer::new();
        let result = renderer.render("> quoted text");
        let joined: String = result
            .lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .map(|s| s.content.as_ref())
            .collect();
        assert!(joined.contains('▌'), "expected a blockquote bar: {joined}");
        assert!(joined.contains("quoted text"));
    }

    #[test]
    fn loose_table_without_separator_still_renders_as_grid() {
        // Regression: many models emit tables WITHOUT the GFM `| --- |` row.
        // pulldown-cmark then parses it as a paragraph ("just text"). The
        // normalize pre-pass must inject a separator so it becomes a real table.
        let renderer = MarkdownRenderer::new();
        let md = "| name | role |\n| holly | engine |\n| tui | head |";
        let result = renderer.render(md);

        let has_separator = result.lines.iter().any(|l| {
            let s: String = l.spans.iter().map(|sp| sp.content.as_ref()).collect();
            s.contains("---")
        });
        assert!(
            has_separator,
            "loose table should have been normalized into a grid with a separator"
        );
        let joined: String = result
            .lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .map(|s| s.content.as_ref())
            .collect();
        assert!(joined.contains("holly") && joined.contains("engine"));
    }

    #[test]
    fn loose_table_inside_code_block_is_left_alone() {
        // Pipes inside fenced code must NOT be rewritten as a table.
        let renderer = MarkdownRenderer::new();
        let md = "```\n| a | b |\n| 1 | 2 |\n```";
        let result = renderer.render(md);
        let joined: String = result
            .lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .map(|s| s.content.as_ref())
            .collect();
        assert!(
            !joined.contains("---"),
            "fenced code should not be normalized into a table: {joined}"
        );
        assert!(joined.contains("| 1 | 2 |"));
    }
}
