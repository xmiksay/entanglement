use crate::tui::wrap;
use ratatui::{
    style::Style,
    text::{Line, Span},
};

use crate::tui::markdown::MarkdownRenderer;
use crate::tui::theme::{RoleColors, Theme};

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
