//! Generic free-text transcript-body rendering, shared by every plain-text
//! tool-output path (`bash`/`call` output, a `read`'s file body, a head-local
//! status notice) that previously bypassed wrapping entirely and let long
//! lines run off the right edge of the panel.
//!
//! Two transforms, tried in order:
//! - a body that is JSON **as a whole** (an object or array — a bare scalar
//!   isn't worth reformatting) is pretty-printed and syntax-highlighted
//!   through the same `syntect` machinery `markdown.rs` uses for fenced code
//!   blocks (no new dependency);
//! - everything else word-wraps to the panel width via [`wrap::wrap_line`],
//!   which already hard-breaks a token with no whitespace (a long path, URL,
//!   hash or base64 blob) and measures in Unicode display columns.
//!
//! `body + width -> lines` is a pure function so it's cheap to unit-test
//! without a terminal; the transcript's [`super::super::cache`] memoizes the
//! *caller's* rendered lines per block, keyed on width, so this never reruns
//! on an idle redraw — only on a content or width change.

use ratatui::text::{Line, Text};

use crate::tui::markdown::MarkdownRenderer;
use crate::tui::wrap;

/// `trimmed` parses as JSON *and* is a container (object/array) worth
/// reformatting — a bare scalar (`42`, `"x"`, `true`) or invalid JSON falls
/// through to plain-text wrapping instead.
fn parse_json_container(trimmed: &str) -> Option<serde_json::Value> {
    let value: serde_json::Value = serde_json::from_str(trimmed).ok()?;
    matches!(
        value,
        serde_json::Value::Object(_) | serde_json::Value::Array(_)
    )
    .then_some(value)
}

/// Render a free-text transcript body: JSON pretty-printed + highlighted when
/// the whole (trimmed) body parses as a JSON object/array, else word-wrapped
/// plain text. Every line — JSON or prose — is wrapped to `available_width`
/// so nothing overflows the panel.
pub(super) fn render_wrapped_body(
    body: &str,
    available_width: u16,
    md: &MarkdownRenderer,
) -> Vec<Line<'static>> {
    let trimmed = body.trim();
    if let Some(value) = parse_json_container(trimmed) {
        // `value` just parsed from `trimmed`, so re-serializing it can't fail.
        if let Ok(pretty) = serde_json::to_string_pretty(&value) {
            return wrap_highlighted(&pretty, available_width, md);
        }
    }
    wrap_plain(body, available_width)
}

fn wrap_highlighted(
    pretty: &str,
    available_width: u16,
    md: &MarkdownRenderer,
) -> Vec<Line<'static>> {
    let highlighted: Text<'static> = md.highlight_code("json", pretty);
    let mut out = Vec::with_capacity(highlighted.lines.len());
    for line in highlighted.lines {
        out.extend(wrap::wrap_line(line, available_width));
    }
    out
}

fn wrap_plain(body: &str, available_width: u16) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    for line in body.lines() {
        out.extend(wrap::wrap_line(
            Line::from(line.to_string()),
            available_width,
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use unicode_width::UnicodeWidthStr;

    fn joined(lines: &[Line<'_>]) -> String {
        lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect::<Vec<String>>()
            .join("\n")
    }

    fn line_widths(lines: &[Line<'_>]) -> Vec<usize> {
        lines
            .iter()
            .map(|l| {
                let s: String = l.spans.iter().map(|s| s.content.as_ref()).collect();
                UnicodeWidthStr::width(s.as_str())
            })
            .collect()
    }

    #[test]
    fn json_object_is_pretty_printed_and_highlighted() {
        let md = MarkdownRenderer::new();
        let out = render_wrapped_body(r#"{"ok":true,"count":2}"#, 80, &md);
        let text = joined(&out);
        // Pretty-printing spreads keys onto their own indented lines — the
        // one-line-JSON symptom is gone.
        assert!(
            out.len() > 1,
            "expected multiple pretty-printed lines: {out:?}"
        );
        assert!(text.contains("\"ok\": true"), "{text:?}");
        assert!(text.contains("\"count\": 2"), "{text:?}");
        // Highlighting assigns an `fg` to at least one span (syntect colors
        // every token it recognizes).
        assert!(
            out.iter()
                .any(|l| l.spans.iter().any(|s| s.style.fg.is_some())),
            "expected at least one highlighted span: {out:?}"
        );
    }

    #[test]
    fn json_array_is_pretty_printed_and_highlighted() {
        let md = MarkdownRenderer::new();
        let out = render_wrapped_body(r#"[{"a":1},{"a":2}]"#, 80, &md);
        let text = joined(&out);
        assert!(
            out.len() > 1,
            "expected multiple pretty-printed lines: {out:?}"
        );
        assert!(text.contains("\"a\": 1"), "{text:?}");
        assert!(text.contains("\"a\": 2"), "{text:?}");
        assert!(
            out.iter()
                .any(|l| l.spans.iter().any(|s| s.style.fg.is_some())),
            "expected at least one highlighted span: {out:?}"
        );
    }

    #[test]
    fn prose_containing_a_brace_is_not_reformatted() {
        let md = MarkdownRenderer::new();
        let prose = "the config block starts with { and ends much later";
        let out = render_wrapped_body(prose, 80, &md);
        assert_eq!(
            joined(&out),
            prose,
            "prose with a brace must render verbatim"
        );
    }

    #[test]
    fn shell_one_liner_is_not_reformatted() {
        let md = MarkdownRenderer::new();
        let cmd = "for f in *.rs; do echo $f; done";
        let out = render_wrapped_body(cmd, 80, &md);
        assert_eq!(joined(&out), cmd);
    }

    #[test]
    fn bare_json_scalar_is_not_reformatted() {
        let md = MarkdownRenderer::new();
        for scalar in ["42", "\"x\"", "true"] {
            let out = render_wrapped_body(scalar, 80, &md);
            assert_eq!(
                joined(&out),
                scalar,
                "bare scalar {scalar:?} must pass through"
            );
        }
    }

    #[test]
    fn invalid_json_falls_through_to_plain_text() {
        let md = MarkdownRenderer::new();
        let broken = r#"{"unterminated": "oops"#;
        let out = render_wrapped_body(broken, 80, &md);
        assert_eq!(joined(&out), broken);
    }

    #[test]
    fn long_line_wraps_to_panel_width() {
        let md = MarkdownRenderer::new();
        let line = "word ".repeat(40);
        let out = render_wrapped_body(&line, 20, &md);
        assert!(out.len() > 1, "expected the line to wrap: {out:?}");
        for w in line_widths(&out) {
            assert!(w <= 20, "wrapped line exceeds panel width: {w}");
        }
    }

    #[test]
    fn long_unbroken_token_hard_breaks() {
        let md = MarkdownRenderer::new();
        let token = "/very/long/path/with/no/spaces".repeat(5);
        let out = render_wrapped_body(&token, 20, &md);
        assert!(out.len() > 1, "expected the token to hard-break: {out:?}");
        for w in line_widths(&out) {
            assert!(w <= 20, "hard-broken line exceeds panel width: {w}");
        }
        // No character is lost across the break.
        assert_eq!(joined(&out).replace('\n', ""), token);
    }

    #[test]
    fn wide_characters_never_exceed_width() {
        let md = MarkdownRenderer::new();
        let s = "日本語のとても長いテキストです🚀🚀🚀🚀🚀🚀🚀🚀🚀🚀";
        let out = render_wrapped_body(s, 10, &md);
        for w in line_widths(&out) {
            assert!(w <= 10, "wide-char line exceeds display width: {w}");
        }
    }

    #[test]
    fn width_change_reflows_the_same_body() {
        let md = MarkdownRenderer::new();
        let line = "word ".repeat(40);
        let at_80 = render_wrapped_body(&line, 80, &md);
        let at_20 = render_wrapped_body(&line, 20, &md);
        assert!(
            at_20.len() > at_80.len(),
            "narrower width must produce more wrapped lines: {} vs {}",
            at_20.len(),
            at_80.len()
        );
        for w in line_widths(&at_20) {
            assert!(w <= 20, "re-wrapped line exceeds new width: {w}");
        }
    }
}
