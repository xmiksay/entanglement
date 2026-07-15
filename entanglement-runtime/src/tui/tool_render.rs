use ratatui::{
    style::{Color, Style},
    text::{Line, Span, Text},
};

use crate::tui::diff::DiffRenderer;
use crate::tui::theme::Theme;

pub fn render_tool_output(
    tool_name: Option<&str>,
    output: &str,
    theme: Theme,
    available_width: u16,
) -> Text<'static> {
    match tool_name {
        Some("edit") => render_edit_output(output, theme, available_width),
        Some("read") => render_read_output(output, theme, available_width),
        Some("glob") => render_glob_output(output, theme, available_width),
        Some("grep") => render_grep_output(output, theme, available_width),
        Some("bash") => render_plain_output(output, theme, available_width),
        _ => render_plain_output(output, theme, available_width),
    }
}

/// Build the expanded body of a tool block from **both** the call `input` and
/// its `output` (#341). Each operation gets a body that means something:
/// `read` → the file body, `edit` → a real diff, `write` → the new content,
/// `bash`/`call` → the command output (the command is already in the header),
/// and everything else → pretty-printed input followed by the output body. The
/// filename/command lives in the block header (#340), never re-printed here.
///
/// Wired into the live transcript by `flush_tool_call`'s expanded branch (#340).
pub fn render_expansion(
    tool: Option<&str>,
    input: &str,
    output: &str,
    theme: Theme,
    available_width: u16,
) -> Text<'static> {
    match tool {
        Some("read") => render_read_output(output, theme, available_width),
        Some("edit") => {
            let v: serde_json::Value =
                serde_json::from_str(input).unwrap_or(serde_json::Value::Null);
            let old = v.get("oldString").and_then(|s| s.as_str()).unwrap_or("");
            let new = v.get("newString").and_then(|s| s.as_str()).unwrap_or("");
            DiffRenderer::render_change(old, new)
        }
        Some("write") => {
            let v: serde_json::Value =
                serde_json::from_str(input).unwrap_or(serde_json::Value::Null);
            let content = v.get("content").and_then(|s| s.as_str()).unwrap_or("");
            Text::from(
                content
                    .lines()
                    .map(|line| Line::from(format!("  {line}")))
                    .collect::<Vec<_>>(),
            )
        }
        Some("bash") | Some("call") => render_plain_output(output, theme, available_width),
        _ => {
            let mut lines = Vec::new();
            match serde_json::from_str::<serde_json::Value>(input)
                .ok()
                .and_then(|v| serde_json::to_string_pretty(&v).ok())
            {
                Some(pretty) => {
                    for line in pretty.lines() {
                        lines.push(Line::from(format!("  {line}")));
                    }
                }
                None => {
                    for line in input.lines() {
                        lines.push(Line::from(format!("  {line}")));
                    }
                }
            }
            lines.extend(render_plain_output(output, theme, available_width).lines);
            Text::from(lines)
        }
    }
}

fn render_edit_output(output: &str, _theme: Theme, _available_width: u16) -> Text<'static> {
    if output.contains("created file:") {
        let line = Line::from(vec![
            Span::styled("✓ ", Style::default().fg(Color::Green)),
            Span::raw(output.to_string()),
        ]);
        return Text::from(vec![line]);
    }

    if output.contains("matches replaced") {
        let line = Line::from(vec![
            Span::styled("✓ ", Style::default().fg(Color::Green)),
            Span::raw(output.to_string()),
        ]);
        return Text::from(vec![line]);
    }

    Text::raw(output.to_string())
}

/// The file body of a `read`. The filename lives in the block header (#340), so
/// the expanded body is just the contents — indented like other tool output.
fn render_read_output(output: &str, _theme: Theme, _available_width: u16) -> Text<'static> {
    Text::from(
        output
            .lines()
            .map(|line| Line::from(format!("  {line}")))
            .collect::<Vec<_>>(),
    )
}

fn render_glob_output(output: &str, _theme: Theme, _available_width: u16) -> Text<'static> {
    let mut lines = Vec::new();

    let mut file_count = 0;
    let mut dir_count = 0;

    for line in output.lines() {
        if line.contains("matched") && line.contains("director") {
            dir_count = line
                .chars()
                .filter(|c| c.is_ascii_digit())
                .collect::<String>()
                .parse()
                .unwrap_or(0);
        } else if !line.trim().is_empty() && !line.starts_with("matched") {
            file_count += 1;
            lines.push(Line::from(format!("  {}", line)));
        }
    }

    let header = if file_count > 0 {
        let msg = format!("{} files matched", file_count);
        Line::from(vec![
            Span::styled("✓ ", Style::default().fg(Color::Green)),
            Span::styled(msg, Style::default().fg(Color::Cyan)),
        ])
    } else if dir_count > 0 {
        let msg = format!(
            "{} directories matched (use pattern/* to list files)",
            dir_count
        );
        Line::from(vec![
            Span::styled("⚠ ", Style::default().fg(Color::Yellow)),
            Span::styled(msg, Style::default().fg(Color::Yellow)),
        ])
    } else {
        Line::from(vec![
            Span::styled("✗ ", Style::default().fg(Color::Red)),
            Span::raw("No matches found"),
        ])
    };

    let mut result = vec![header];
    result.extend(lines);
    Text::from(result)
}

fn render_grep_output(output: &str, _theme: Theme, _available_width: u16) -> Text<'static> {
    let mut lines = Vec::new();
    let mut match_count = 0;

    for line in output.lines() {
        if line.contains(':') {
            match_count += 1;
            let parts: Vec<&str> = line.splitn(2, ':').collect();
            if parts.len() == 2 {
                lines.push(Line::from(vec![
                    Span::styled(format!("  {}:", parts[0]), Style::default().fg(Color::Cyan)),
                    Span::raw(parts[1].to_string()),
                ]));
            }
        }
    }

    let msg = format!("{} matches found", match_count);
    let header = Line::from(vec![
        Span::styled("✓ ", Style::default().fg(Color::Green)),
        Span::styled(msg, Style::default().fg(Color::Cyan)),
    ]);

    let mut result = vec![header];
    result.extend(lines);
    Text::from(result)
}

fn render_plain_output(output: &str, _theme: Theme, _available_width: u16) -> Text<'static> {
    let mut lines = Vec::new();
    for line in output.lines() {
        lines.push(Line::from(format!("  {}", line)));
    }
    Text::from(lines)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_glob_with_hyphenated_paths() {
        let output = "docs/adr/0001-actor-model-abi.md\ndocs/adr/0002-protocol.md\nsrc/main.rs\n";
        let theme = Theme::default();
        let result = render_glob_output(output, theme, 80);
        let text: String = result
            .lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .map(|s| s.content.as_ref())
            .collect();
        assert!(
            text.contains("0001-actor-model-abi.md"),
            "Should render hyphenated paths, got: {text}"
        );
        assert!(
            text.contains("0002-protocol.md"),
            "Should render hyphenated paths, got: {text}"
        );
        assert!(
            text.contains("3 files matched"),
            "Should show file count, got: {text}"
        );
    }

    #[test]
    fn test_glob_with_directories_only() {
        let output = "pattern `**` matched 3 directories but no files (files are filtered out). Try `**/*` to list files inside those directories.\n";
        let theme = Theme::default();
        let result = render_glob_output(output, theme, 80);
        let text: String = result
            .lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .map(|s| s.content.as_ref())
            .collect();
        assert!(
            text.contains("3 directories matched"),
            "Should show directory hint, got: {text}"
        );
    }

    #[test]
    fn test_grep_with_matches() {
        let output = "src/main.rs:10: fn main() {\nsrc/main.rs:20: println!(\"Hello\");\n";
        let theme = Theme::default();
        let result = render_grep_output(output, theme, 80);
        let text: String = result
            .lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .map(|s| s.content.as_ref())
            .collect();
        assert!(text.contains("2 matches found"), "Should show match count");
        assert!(text.contains("src/main.rs:10:"), "Should show file:line");
        assert!(text.contains("src/main.rs:20:"), "Should show file:line");
    }

    #[test]
    fn test_edit_creates_file() {
        let output = "created file: test.txt";
        let theme = Theme::default();
        let result = render_edit_output(output, theme, 80);
        let text: String = result
            .lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .map(|s| s.content.as_ref())
            .collect();
        assert!(text.contains("✓"), "Should show checkmark");
        assert!(
            text.contains("created file"),
            "Should show creation message"
        );
    }

    fn flatten(text: &Text<'_>) -> String {
        text.lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .map(|s| s.content.as_ref())
            .collect()
    }

    #[test]
    fn test_read_renders_body() {
        let output = "1: line 1\n2: line 2\n3: line 3\n";
        let result = render_read_output(output, Theme::default(), 80);
        let text = flatten(&result);
        assert!(text.contains("line 1"), "read should render the file body");
        assert!(text.contains("line 3"), "read should render the file body");
    }

    #[test]
    fn test_expansion_read_shows_body() {
        let result = render_expansion(
            Some("read"),
            r#"{"path":"src/main.rs"}"#,
            "fn main() {}\n",
            Theme::default(),
            80,
        );
        assert!(
            flatten(&result).contains("fn main() {}"),
            "read expansion should show the file body"
        );
    }

    #[test]
    fn test_expansion_edit_shows_diff() {
        let result = render_expansion(
            Some("edit"),
            r#"{"path":"a.rs","oldString":"a","newString":"b"}"#,
            "",
            Theme::default(),
            80,
        );
        let has_delete = result
            .lines
            .iter()
            .any(|l| l.spans.iter().any(|s| s.content == "- "));
        let has_insert = result
            .lines
            .iter()
            .any(|l| l.spans.iter().any(|s| s.content == "+ "));
        assert!(
            has_delete && has_insert,
            "edit expansion should render a `-`/`+` pair"
        );
    }

    #[test]
    fn test_expansion_write_shows_content() {
        let result = render_expansion(
            Some("write"),
            r#"{"path":"a.rs","content":"hello\nworld"}"#,
            "",
            Theme::default(),
            80,
        );
        let text = flatten(&result);
        assert!(
            text.contains("hello"),
            "write expansion should show the content"
        );
        assert!(
            text.contains("world"),
            "write expansion should show the content"
        );
    }
}
