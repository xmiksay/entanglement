use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
};
use syntect::{
    highlighting::{Theme, ThemeSet},
    parsing::SyntaxSet,
};

use pulldown_cmark::{Event, Parser, Tag, TagEnd};

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

    pub fn render(&self, markdown: &str) -> Text<'_> {
        if markdown.trim().is_empty() {
            return Text::default();
        }

        let parser = Parser::new(markdown);
        let mut lines = Vec::new();
        let mut current_line = Vec::new();
        let mut in_code_block = false;
        let mut code_language = String::new();
        let mut code_content = String::new();

        for event in parser {
            match event {
                Event::Start(tag) => match tag {
                    Tag::Paragraph => {
                        current_line.push(Span::raw("  "));
                    }
                    Tag::Heading { level, .. } => {
                        let prefix = "#".repeat(level as usize);
                        current_line.push(Span::styled(
                            format!("{} ", prefix),
                            Style::default().add_modifier(Modifier::BOLD),
                        ));
                    }
                    Tag::Emphasis => {}
                    Tag::Strong => {}
                    Tag::CodeBlock(pulldown_cmark::CodeBlockKind::Fenced(lang)) => {
                        in_code_block = true;
                        code_language = lang.to_string();
                    }
                    Tag::List(_) => {}
                    Tag::Item => {
                        current_line.push(Span::raw("  • "));
                    }
                    Tag::Link { .. } => {}
                    _ => {}
                },
                Event::End(tag_end) => match tag_end {
                    TagEnd::Paragraph => {
                        if !current_line.is_empty() {
                            lines.push(Line::from(current_line.clone()));
                            current_line.clear();
                        }
                        lines.push(Line::from(""));
                    }
                    TagEnd::Heading(_) => {
                        if !current_line.is_empty() {
                            lines.push(Line::from(current_line.clone()));
                            current_line.clear();
                        }
                        lines.push(Line::from(""));
                    }
                    TagEnd::Emphasis => {}
                    TagEnd::Strong => {}
                    TagEnd::CodeBlock => {
                        in_code_block = false;
                        let highlighted = self.highlight_code(&code_language, &code_content);
                        for line in highlighted.lines {
                            lines.push(line);
                        }
                        lines.push(Line::from(""));
                        code_language.clear();
                        code_content.clear();
                    }
                    TagEnd::List(_) => {}
                    TagEnd::Item => {
                        if !current_line.is_empty() {
                            lines.push(Line::from(current_line.clone()));
                            current_line.clear();
                        }
                    }
                    TagEnd::Link => {}
                    _ => {}
                },
                Event::Text(text) => {
                    if in_code_block {
                        code_content.push_str(&text);
                    } else {
                        current_line.push(Span::raw(text.to_string()));
                    }
                }
                Event::Code(text) => {
                    current_line.push(Span::styled(
                        format!("`{}`", text),
                        Style::default()
                            .fg(Color::Yellow)
                            .add_modifier(Modifier::ITALIC),
                    ));
                }
                Event::SoftBreak => {
                    current_line.push(Span::raw(" "));
                }
                Event::HardBreak => {
                    if !current_line.is_empty() {
                        lines.push(Line::from(current_line.clone()));
                        current_line.clear();
                    }
                }
                Event::Rule => {
                    lines.push(Line::from("─".repeat(40)));
                }
                _ => {}
            }
        }

        if !current_line.is_empty() {
            lines.push(Line::from(current_line));
        }

        Text::from(lines)
    }

    fn highlight_code(&self, language: &str, code: &str) -> Text<'_> {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_render_plain_text() {
        let renderer = MarkdownRenderer::new();
        let result = renderer.render("Hello, world!");
        assert!(!result.lines.is_empty());
    }

    #[test]
    fn test_render_plain_text_no_word_duplication() {
        let renderer = MarkdownRenderer::new();
        let result = renderer.render("one two three four");
        let rendered: String = result
            .lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .map(|span| span.content.as_ref())
            .collect();
        assert_eq!(rendered.trim(), "one two three four");
    }

    #[test]
    fn test_render_bold() {
        let renderer = MarkdownRenderer::new();
        let result = renderer.render("**bold text**");
        assert!(!result.lines.is_empty());
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
}
