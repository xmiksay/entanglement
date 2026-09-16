//! The generic readable body (ADR-0204 §6) for every tool without a dedicated
//! renderer — MCP `mcp__<server>__<tool>`, `endpoint__*`/`skill__*`,
//! `mcp_enable`, unknown names — plus the output and error tails every
//! expanded call shares. Arguments and JSON outputs become indented
//! `key: value` lines ([`summary::readable_lines`]) wrapped to the panel,
//! never escaped JSON.

use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};

use crate::run::summary;
use crate::tui::wrap;

use super::collect_line;

pub(super) fn render_args(input: &str, available_width: u16) -> Vec<Line<'static>> {
    indented_wrapped(
        summary::readable_args(input),
        available_width,
        Style::default(),
    )
}

pub(super) fn render_output(output: &str, available_width: u16) -> Vec<Line<'static>> {
    indented_wrapped(
        summary::readable_output(output),
        available_width,
        Style::default(),
    )
}

/// A failed call's output (`is_error`, ADR-0176): a bold `✗ error` marker and
/// the output in the error color, so a denial or tool failure never reads
/// like an ordinary result once the block is expanded.
pub(super) fn render_error_output(
    output: &str,
    available_width: u16,
    color: Color,
) -> Vec<Line<'static>> {
    let style = Style::default().fg(color);
    let mut lines = vec![Line::from(Span::styled(
        "  ✗ error",
        style.add_modifier(Modifier::BOLD),
    ))];
    lines.extend(indented_wrapped(
        summary::readable_output(output),
        available_width,
        style,
    ));
    lines
}

/// Indent each line two columns and word-wrap it to the panel. A wrapped
/// continuation hangs two columns deeper than its own line, so a long value
/// inside nested `key: value` structure stays visibly attached to its key.
pub(super) fn indented_wrapped(
    lines: Vec<String>,
    available_width: u16,
    style: Style,
) -> Vec<Line<'static>> {
    let width = usize::from(available_width.saturating_sub(4));
    let mut out = Vec::new();
    for line in lines {
        let body = line.trim_start();
        let indent = line.len() - body.len() + 2;
        if body.is_empty() {
            out.push(Line::from(""));
            continue;
        }
        let wrap_width = u16::try_from(width.saturating_sub(indent + 2).max(1)).unwrap_or(1);
        for (i, wline) in wrap::wrap_line(Line::from(body.to_string()), wrap_width)
            .iter()
            .enumerate()
        {
            let pad = if i == 0 { indent } else { indent + 2 };
            out.push(Line::from(Span::styled(
                format!("{}{}", " ".repeat(pad), collect_line(wline)),
                style,
            )));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use crate::tui::markdown::MarkdownRenderer;
    use crate::tui::theme::Theme;
    use ratatui::text::Text;

    fn expand(tool: &str, input: &str, output: Option<&str>, is_error: bool) -> Text<'static> {
        super::super::render_expansion(
            Some(tool),
            input,
            output,
            is_error,
            Theme::default(),
            60,
            &MarkdownRenderer::new(),
        )
    }

    fn lines(text: &Text<'_>) -> Vec<String> {
        text.lines.iter().map(super::collect_line).collect()
    }

    #[test]
    fn mcp_call_renders_nested_args_and_json_output_as_key_value_lines() {
        let text = expand(
            "mcp__chess__board",
            r#"{"state":{"fen":"8/8","moves":["e4","e5"]},"note":"line one\nline two"}"#,
            Some(r#"{"result":{"legal":true,"count":2}}"#),
            false,
        );
        let rendered = lines(&text);
        for expected in [
            "  note:",
            "    line one",
            "  state:",
            "    fen: 8/8",
            "    moves:",
            "      - e4",
            "  result:",
            "    count: 2",
            "    legal: true",
        ] {
            assert!(
                rendered.iter().any(|l| l == expected),
                "missing {expected:?} in {rendered:#?}"
            );
        }
        let joined = rendered.join("\n");
        assert!(!joined.contains('{') && !joined.contains("\\n"), "{joined}");
    }

    #[test]
    fn long_string_argument_wraps_in_full() {
        let long = "word ".repeat(40);
        let input = serde_json::json!({ "query": long }).to_string();
        let text = expand("endpoint__search", &input, None, false);
        let rendered = lines(&text);
        assert!(rendered.len() > 2, "a long value must wrap: {rendered:#?}");
        assert!(
            rendered.iter().all(|l| l.chars().count() <= 60),
            "{rendered:#?}"
        );
        assert_eq!(rendered.join(" ").matches("word").count(), 40);
    }

    #[test]
    fn skill_tool_and_mcp_enable_use_the_readable_fallback() {
        let text = expand(
            "skill__arch__check",
            r#"{"strict":true}"#,
            Some("all good"),
            false,
        );
        assert_eq!(lines(&text), vec!["  strict: true", "  all good"]);
        let text = expand(
            "mcp_enable",
            r#"{"server":"github"}"#,
            Some("enabled github"),
            false,
        );
        assert_eq!(lines(&text), vec!["  server: github", "  enabled github"]);
        let text = expand("mystery", "{}", None, false);
        assert_eq!(lines(&text), vec!["  (no arguments)"]);
    }

    #[test]
    fn failed_call_output_is_marked_as_an_error() {
        let text = expand(
            "mcp__gh__issue",
            r#"{"title":"x"}"#,
            Some("Declined by profile"),
            true,
        );
        let rendered = lines(&text);
        assert!(rendered.contains(&"  ✗ error".to_string()), "{rendered:#?}");
        let error_line = text
            .lines
            .iter()
            .find(|l| super::collect_line(l).contains("Declined by profile"))
            .expect("error output line");
        assert!(
            error_line.spans.iter().all(|s| s.style.fg.is_some()),
            "error output must be colored"
        );
    }
}
