use crate::tui::wrap;
use ratatui::{
    style::{Color, Style},
    text::{Line, Span},
};
use std::borrow::Cow;
use unicode_width::UnicodeWidthStr;

use crate::tui::markdown::MarkdownRenderer;
use crate::tui::theme::{RoleColors, Theme};
use crate::tui::tool_render;

/// Reforge a `Line` whose spans the markdown renderer produced under `&self`'s
/// lifetime into an owned `Line<'static>`. Every span's content is already an
/// owned `String` behind the `Cow` (see [`MarkdownRenderer::render`]), so
/// `into_owned()` is a move, not a copy — this only relabels the lifetime so the
/// assembled line can live in the render cache ([`super::cache`]).
pub(super) fn to_static(line: Line<'_>) -> Line<'static> {
    let spans: Vec<Span<'static>> = line
        .spans
        .into_iter()
        .map(|s| Span {
            style: s.style,
            content: Cow::Owned(s.content.into_owned()),
        })
        .collect();
    let mut out = Line::from(spans);
    out.style = line.style;
    out.alignment = line.alignment;
    out
}

fn render_text_run(
    md: &MarkdownRenderer,
    run: &str,
    theme: Theme,
    colors: RoleColors,
    available_width: u16,
) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    if run.trim().is_empty() {
        return out;
    }
    let rendered = md.render(run);
    for line in rendered.lines {
        let line = to_static(line);
        let is_table = line
            .spans
            .first()
            .map(|s| s.content.as_ref().starts_with('|'))
            .unwrap_or(false);

        let is_code =
            line.spans.len() > 1 && line.spans.iter().skip(1).any(|s| s.style.fg.is_some());

        if is_table || is_code {
            out.push(theme.decorate(line, colors, available_width));
        } else {
            for wline in wrap::wrap_line(line, available_width.saturating_sub(4)) {
                out.push(theme.decorate(wline, colors, available_width));
            }
        }
    }
    out
}

fn render_reasoning_run(
    md: &MarkdownRenderer,
    run: &str,
    theme: Theme,
    colors: RoleColors,
    available_width: u16,
) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    if run.trim().is_empty() {
        return out;
    }
    let rendered = md.render(run);
    for line in rendered.lines {
        let line = to_static(line);
        let is_table = line
            .spans
            .first()
            .map(|s| s.content.as_ref().starts_with('|'))
            .unwrap_or(false);

        let is_code =
            line.spans.len() > 1 && line.spans.iter().skip(1).any(|s| s.style.fg.is_some());

        let styled_line = if is_table || is_code {
            line
        } else {
            let styled_spans: Vec<Span<'static>> = line
                .spans
                .iter()
                .map(|s| {
                    Span::styled(
                        s.content.clone().into_owned(),
                        Style::default().fg(colors.fg).italic(),
                    )
                })
                .collect();
            Line::from(styled_spans)
        };

        if is_table || is_code {
            out.push(theme.decorate(styled_line, colors, available_width));
        } else {
            for wline in wrap::wrap_line(styled_line, available_width.saturating_sub(4)) {
                out.push(theme.decorate(wline, colors, available_width));
            }
        }
    }
    out
}

/// The owned colored left-bar padding row that brackets a message block.
pub(super) fn padding_line(colors: RoleColors, available_width: u16) -> Line<'static> {
    Line::from(vec![
        Span::styled(
            "▌".to_string(),
            Style::default().fg(colors.fg).bg(colors.bg),
        ),
        Span::raw(" ".repeat(available_width.saturating_sub(1) as usize)),
    ])
}

/// Renders a coalesced assistant text run, optionally wrapped in the
/// colored left-bar padding. Streaming (uncommitted) trailing text renders
/// without padding; a run committed by a following entry gets the bar.
pub(super) fn flush_text(
    md: &MarkdownRenderer,
    run: &str,
    theme: Theme,
    colors: RoleColors,
    available_width: u16,
    with_padding: bool,
) -> Vec<Line<'static>> {
    let body = render_text_run(md, run, theme, colors, available_width);
    if with_padding {
        let padding = padding_line(colors, available_width);
        let mut out = Vec::with_capacity(body.len() + 2);
        out.push(padding.clone());
        out.extend(body);
        out.push(padding);
        out
    } else {
        body
    }
}

/// Renders a coalesced reasoning run as a collapsible block: a one-line
/// `▸ Thinking (N lines)` header (collapsed, the default) or `▾ …` plus the
/// italic body (expanded). The whole block maps to one clickable region; its
/// stable id is resolved by the caller ([`super::segment`]).
pub(super) fn flush_reasoning(
    md: &MarkdownRenderer,
    run: &str,
    theme: Theme,
    colors: RoleColors,
    available_width: u16,
    expanded: bool,
) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    if run.trim().is_empty() {
        return out;
    }
    let source_lines = run.lines().filter(|l| !l.trim().is_empty()).count().max(1);
    let padding = padding_line(colors, available_width);
    out.push(padding.clone());

    let arrow = if expanded { "▾" } else { "▸" };
    let header = Line::from(vec![Span::styled(
        format!("{arrow} Thinking ({source_lines} lines)"),
        Style::default().fg(colors.fg).italic(),
    )]);
    out.push(theme.decorate(header, colors, available_width));

    if expanded {
        out.extend(render_reasoning_run(
            md,
            run,
            theme,
            colors,
            available_width,
        ));
    }

    out.push(padding);
    out
}

/// The primary argument shown on a tool op's collapsed header: the path for
/// `read`/`edit`/`write`, the command line for `bash`/`call` (via the runtime's
/// [`permission_arg`][entanglement_runtime::permission::permission_arg]), or the
/// `pattern` for `glob`/`grep` (a local fallback, since `permission_arg` returns
/// `None` for those). `None` when nothing informative is available.
fn tool_primary_arg(tool: &str, input: &str) -> Option<String> {
    if let Some(arg) = entanglement_runtime::permission::permission_arg(tool, input) {
        return Some(arg);
    }
    let value: serde_json::Value = serde_json::from_str(input).ok()?;
    value.get("pattern")?.as_str().map(String::from)
}

/// Renders one tool op as a collapsible block, mirroring [`flush_reasoning`]:
/// a one-line `{arrow} {tool}  {primary_arg}  {status}` header (collapsed, the
/// default) plus — when expanded — the call args and its output. The whole block
/// maps to one clickable region so a click toggles it (#340).
pub(super) fn flush_tool_call(
    tool: &str,
    input: &str,
    output: Option<&str>,
    theme: Theme,
    colors: RoleColors,
    available_width: u16,
    expanded: bool,
) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    let padding = padding_line(colors, available_width);
    out.push(padding.clone());

    let arrow = if expanded { "▾" } else { "▸" };
    let mut header = vec![
        Span::styled(format!("{arrow} "), Style::default().fg(colors.fg)),
        Span::styled(tool.to_string(), Style::default().fg(Color::Cyan).bold()),
    ];
    if let Some(arg) = tool_primary_arg(tool, input) {
        // Budget the arg so the header stays on one line: strip the bar (2),
        // arrow (2), tool name, the two-space gaps, and the trailing status.
        let status_w = if output.is_some() { 2 } else { 0 };
        let fixed = 2 + 2 + UnicodeWidthStr::width(tool) + 2 + status_w;
        let budget = (available_width as usize).saturating_sub(fixed);
        header.push(Span::raw("  "));
        header.push(Span::styled(
            truncate_to_width(&arg, budget),
            Style::default().fg(colors.fg),
        ));
    }
    if output.is_some() {
        header.push(Span::styled(" ✓", Style::default().fg(Color::Green)));
    }
    out.push(theme.decorate(Line::from(header), colors, available_width));

    if expanded {
        for line in input.lines() {
            let content_line = Line::from(format!("  {line}"));
            for wline in wrap::wrap_line(content_line, available_width.saturating_sub(4)) {
                out.push(theme.decorate(wline, colors, available_width));
            }
        }
        if let Some(output) = output {
            let rendered =
                tool_render::render_tool_output(Some(tool), output, theme, available_width);
            for line in rendered.lines {
                out.push(theme.decorate(line, colors, available_width));
            }
        }
    }

    out.push(padding);
    out
}

/// Truncates `s` to `max` display columns, appending `…` when it had to cut.
fn truncate_to_width(s: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    if UnicodeWidthStr::width(s) <= max {
        return s.to_string();
    }
    let mut out = String::new();
    let mut width = 0;
    for ch in s.chars() {
        let w = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
        if width + w > max.saturating_sub(1) {
            break;
        }
        out.push(ch);
        width += w;
    }
    out.push('…');
    out
}
