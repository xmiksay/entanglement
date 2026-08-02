use diffy::Patch;
use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
};

pub struct DiffRenderer;

impl DiffRenderer {
    /// Render an `oldString` → `newString` change as a unified diff, the way an
    /// `edit` tool block shows its edit expanded (#341). The hunks come out in
    /// the same `+`/`-` green/red style as [`render_unified`](Self::render_unified).
    /// Live via `render_expansion`'s `edit` arm (#341).
    pub fn render_change(old: &str, new: &str) -> Text<'static> {
        let patch = diffy::create_patch(old, new);
        Self::render_hunks(&patch)
    }

    /// Render a raw unified-diff patch (`apply_patch`'s `patch` field) as a
    /// real diff instead of the escaped-JSON-string fallback (#519). `diffy`'s
    /// own parser accepts the bare `@@ ... @@` hunks `apply_patch`'s
    /// hand-rolled `host::unified_diff` parser does — no `---`/`+++` file
    /// header required.
    pub fn render_unified(diff: &str) -> Text<'static> {
        if diff.is_empty() {
            return Text::default();
        }

        let patch = Patch::from_str(diff).unwrap_or_else(|_| {
            Patch::from_str("").unwrap_or_else(|_| Patch::from_str("@@ -0,0 +0,0 @@\n").unwrap())
        });

        Self::render_hunks(&patch)
    }

    /// Shared hunk → styled-line rendering for [`render_change`](Self::render_change)
    /// and [`render_unified`](Self::render_unified): inserts green `+`, deletes
    /// red `-`, context dimmed.
    fn render_hunks(patch: &Patch<'_, str>) -> Text<'static> {
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
                format!("─ {}", truncate_at_boundary(&before_line, 27))
            } else {
                format!("─ {}", before_line)
            };

            let after_preview = if after_line.len() > 30 {
                format!("+ {}", truncate_at_boundary(&after_line, 27))
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

/// Byte-safe prefix of `s` up to `max` bytes, backing off to the nearest
/// UTF-8 char boundary (#556): a multi-byte char straddling the byte cut must
/// not panic, mirroring `host/mod.rs`'s `truncate_output`.
fn truncate_at_boundary(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut cut = max;
    while !s.is_char_boundary(cut) {
        cut -= 1;
    }
    &s[..cut]
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
    fn render_stacked_does_not_panic_on_a_multibyte_boundary() {
        // 26 ASCII bytes then a 3-byte char starting at byte 26: a byte-27 cut
        // (the old `&line[..27]` slice) lands mid-char and panics.
        let before = format!("{}日本語 rest of the line", "a".repeat(26));
        let after = format!("{}異なる other content here", "a".repeat(26));
        let result = DiffRenderer::render_stacked(&before, &after);
        assert!(!result.lines.is_empty());
    }

    #[test]
    fn test_render_empty_diff() {
        let diff = "";
        let result = DiffRenderer::render_unified(diff);
        assert!(result.lines.is_empty());
    }

    #[test]
    fn test_render_change_produces_delete_insert_pair() {
        let result =
            DiffRenderer::render_change("line 1\nold line\nline 3\n", "line 1\nnew line\nline 3\n");
        let has_delete = result
            .lines
            .iter()
            .any(|l| l.spans.iter().any(|s| s.content == "- "));
        let has_insert = result
            .lines
            .iter()
            .any(|l| l.spans.iter().any(|s| s.content == "+ "));
        assert!(has_delete, "one-line change should render a `-` line");
        assert!(has_insert, "one-line change should render a `+` line");
    }
}
