use crate::tui::wrap;
use ratatui::{
    style::{Color, Style},
    text::{Line, Span},
};
use unicode_width::UnicodeWidthStr;

use crate::tui::markdown::MarkdownRenderer;
use crate::tui::theme::{RoleColors, Theme};
use crate::tui::tool_render;

fn render_text_run<'a>(
    lines: &mut Vec<Line<'a>>,
    markdown_renderer: &'a MarkdownRenderer,
    run: &str,
    theme: Theme,
    colors: RoleColors,
    available_width: u16,
) {
    if run.trim().is_empty() {
        return;
    }
    let rendered = markdown_renderer.render(run);
    for line in rendered.lines {
        let is_table = line
            .spans
            .first()
            .map(|s| s.content.as_ref().starts_with('|'))
            .unwrap_or(false);

        let is_code =
            line.spans.len() > 1 && line.spans.iter().skip(1).any(|s| s.style.fg.is_some());

        if is_table || is_code {
            let decorated = theme.decorate(line, colors, available_width);
            lines.push(decorated);
        } else {
            let wrapped = wrap::wrap_line(line, available_width.saturating_sub(4));
            for wline in wrapped {
                let decorated = theme.decorate(wline, colors, available_width);
                lines.push(decorated);
            }
        }
    }
}

fn render_reasoning_run<'a>(
    lines: &mut Vec<Line<'a>>,
    markdown_renderer: &'a MarkdownRenderer,
    run: &str,
    theme: Theme,
    colors: RoleColors,
    available_width: u16,
) {
    if run.trim().is_empty() {
        return;
    }
    let rendered = markdown_renderer.render(run);
    for line in rendered.lines {
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
            let styled_spans: Vec<Span> = line
                .spans
                .iter()
                .map(|s| Span::styled(s.content.clone(), Style::default().fg(colors.fg).italic()))
                .collect();
            Line::from(styled_spans)
        };

        if is_table || is_code {
            let decorated = theme.decorate(styled_line, colors, available_width);
            lines.push(decorated);
        } else {
            let wrapped = wrap::wrap_line(styled_line, available_width.saturating_sub(4));
            for wline in wrapped {
                let decorated = theme.decorate(wline, colors, available_width);
                lines.push(decorated);
            }
        }
    }
}

/// Renders a coalesced assistant text run, optionally wrapped in the
/// colored left-bar padding. Streaming (uncommitted) trailing text renders
/// without padding; a run committed by a following entry gets the bar.
pub(super) fn flush_text<'a>(
    lines: &mut Vec<Line<'a>>,
    markdown_renderer: &'a MarkdownRenderer,
    run: &mut String,
    theme: Theme,
    colors: RoleColors,
    available_width: u16,
    with_padding: bool,
) {
    if with_padding {
        let padding = Line::from(vec![
            Span::styled("▌", Style::default().fg(colors.fg).bg(colors.bg)),
            Span::raw(" ".repeat(available_width.saturating_sub(1) as usize)),
        ]);
        lines.push(padding.clone());
        render_text_run(
            lines,
            markdown_renderer,
            run,
            theme,
            colors,
            available_width,
        );
        lines.push(padding);
    } else {
        render_text_run(
            lines,
            markdown_renderer,
            run,
            theme,
            colors,
            available_width,
        );
    }
    run.clear();
}

/// Renders a coalesced reasoning run as a collapsible block: a one-line
/// `▸ Thinking (N lines)` header (collapsed, the default) or `▾ …` plus the
/// italic body (expanded). Records the rendered line range under `block_id`
/// so a click anywhere in the block toggles it.
#[allow(clippy::too_many_arguments)]
pub(super) fn flush_reasoning<'a>(
    lines: &mut Vec<Line<'a>>,
    regions: &mut Vec<(usize, usize, usize)>,
    markdown_renderer: &'a MarkdownRenderer,
    run: &str,
    theme: Theme,
    colors: RoleColors,
    available_width: u16,
    block_id: usize,
    expanded: bool,
) {
    if run.trim().is_empty() {
        return;
    }
    let start = lines.len();
    let source_lines = run.lines().filter(|l| !l.trim().is_empty()).count().max(1);
    let padding = Line::from(vec![
        Span::styled("▌", Style::default().fg(colors.fg).bg(colors.bg)),
        Span::raw(" ".repeat(available_width.saturating_sub(1) as usize)),
    ]);
    lines.push(padding.clone());

    let arrow = if expanded { "▾" } else { "▸" };
    let header = Line::from(vec![Span::styled(
        format!("{arrow} Thinking ({source_lines} lines)"),
        Style::default().fg(colors.fg).italic(),
    )]);
    lines.push(theme.decorate(header, colors, available_width));

    if expanded {
        render_reasoning_run(
            lines,
            markdown_renderer,
            run,
            theme,
            colors,
            available_width,
        );
    }

    lines.push(padding);
    regions.push((block_id, start, lines.len()));
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
/// default) plus — when expanded — the call args and its output. Records the
/// rendered line range under `block_id` so a click toggles it (#340).
#[allow(clippy::too_many_arguments)]
pub(super) fn flush_tool_call<'a>(
    lines: &mut Vec<Line<'a>>,
    regions: &mut Vec<(usize, usize, usize)>,
    tool: &str,
    input: &str,
    output: Option<&str>,
    theme: Theme,
    colors: RoleColors,
    available_width: u16,
    block_id: usize,
    expanded: bool,
) {
    let start = lines.len();
    let padding = Line::from(vec![
        Span::styled("▌", Style::default().fg(colors.fg).bg(colors.bg)),
        Span::raw(" ".repeat(available_width.saturating_sub(1) as usize)),
    ]);
    lines.push(padding.clone());

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
    lines.push(theme.decorate(Line::from(header), colors, available_width));

    if expanded {
        for line in input.lines() {
            let content_line = Line::from(format!("  {line}"));
            for wline in wrap::wrap_line(content_line, available_width.saturating_sub(4)) {
                lines.push(theme.decorate(wline, colors, available_width));
            }
        }
        if let Some(output) = output {
            let rendered =
                tool_render::render_tool_output(Some(tool), output, theme, available_width);
            for line in rendered.lines {
                lines.push(theme.decorate(line, colors, available_width));
            }
        }
    }

    lines.push(padding);
    regions.push((block_id, start, lines.len()));
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
