use diffy::Patch;
use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
};

pub struct DiffRenderer;

impl DiffRenderer {
    pub fn render_unified(diff: &str) -> Text<'_> {
        if diff.is_empty() {
            return Text::default();
        }

        let patch = Patch::from_str(diff).unwrap_or_else(|_| {
            Patch::from_str("").unwrap_or_else(|_| Patch::from_str("@@ -0,0 +0,0 @@\n").unwrap())
        });

        let mut lines = Vec::new();

        for hunk in patch.hunks() {
            for line in hunk.lines() {
                match line {
                    diffy::Line::Context(line) => {
                        lines.push(Line::from(vec![
                            Span::styled("  ", Style::default().fg(Color::DarkGray)),
                            Span::raw(line.to_string()),
                        ]));
                    }
                    diffy::Line::Insert(line) => {
                        lines.push(Line::from(vec![
                            Span::styled("+ ", Style::default().fg(Color::Green)),
                            Span::styled(line.to_string(), Style::default().fg(Color::Green)),
                        ]));
                    }
                    diffy::Line::Delete(line) => {
                        lines.push(Line::from(vec![
                            Span::styled("- ", Style::default().fg(Color::Red)),
                            Span::styled(line.to_string(), Style::default().fg(Color::Red)),
                        ]));
                    }
                }
            }
        }

        Text::from(lines)
    }

    #[allow(dead_code)]
    pub fn render_stacked<'a>(before: &'a str, after: &'a str) -> Text<'a> {
        let before_lines: Vec<&str> = before.lines().collect();
        let after_lines: Vec<&str> = after.lines().collect();

        let mut lines = Vec::new();

        let max_len = before_lines.len().max(after_lines.len());

        lines.push(Line::from(vec![
            Span::styled(
                "Before:",
                Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
            ),
            Span::raw("  "),
            Span::styled(
                "After:",
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD),
            ),
        ]));

        for i in 0..max_len {
            let before_line = before_lines
                .get(i)
                .map(|s| s.to_string())
                .unwrap_or_default();
            let after_line = after_lines
                .get(i)
                .map(|s| s.to_string())
                .unwrap_or_default();

            let is_diff = before_line != after_line;

            let before_preview = if before_line.len() > 30 {
                format!("─ {}", &before_line[..27])
            } else {
                format!("─ {}", before_line)
            };

            let after_preview = if after_line.len() > 30 {
                format!("+ {}", &after_line[..27])
            } else {
                format!("+ {}", after_line)
            };

            lines.push(Line::from(vec![
                Span::styled(
                    before_preview,
                    Style::default().fg(if is_diff { Color::Red } else { Color::DarkGray }),
                ),
                Span::raw("  "),
                Span::styled(
                    after_preview,
                    Style::default().fg(if is_diff {
                        Color::Green
                    } else {
                        Color::DarkGray
                    }),
                ),
            ]));
        }

        Text::from(lines)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_render_unified_diff() {
        let diff = "@@ -1,3 +1,3 @@
 line 1
-line 2
+line 2 modified
 line 3";
        let result = DiffRenderer::render_unified(diff);
        assert!(!result.lines.is_empty());
    }

    #[test]
    fn test_render_stacked_diff() {
        let before = "line 1\nline 2\nline 3";
        let after = "line 1\nline 2 modified\nline 3";
        let result = DiffRenderer::render_stacked(before, after);
        assert!(!result.lines.is_empty());
    }

    #[test]
    fn test_render_empty_diff() {
        let diff = "";
        let result = DiffRenderer::render_unified(diff);
        assert!(result.lines.is_empty());
    }
}
