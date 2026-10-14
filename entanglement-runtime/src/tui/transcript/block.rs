use crate::tui::wrap;
use ratatui::{
    style::Style,
    text::{Line, Span},
};

use crate::tui::markdown::MarkdownRenderer;
use crate::tui::session_view::NestedLine;
use crate::tui::theme::{RoleColors, Slot, Theme};
use crate::tui::tool_render;

use super::render_run::{
    flush_reasoning, flush_text, flush_tool_call, padding_line, tool_header_spans,
};
use super::segment::Block;

/// Renders a single transcript [`Block`] to owned lines. This is the unit the
/// render cache memoizes: every line is `'static`, so a block's output survives
/// unchanged across frames until its content hash ([`Block::key`]) shifts.
pub(super) fn render_block(
    block: &Block,
    md: &MarkdownRenderer,
    theme: Theme,
    user: RoleColors,
    available_width: u16,
) -> Vec<Line<'static>> {
    let assistant = theme.assistant_colors();
    let reasoning = theme.reasoning_colors();
    let tool_req = theme.tool_req_colors();
    let tool_out = theme.tool_out_colors();
    let error = theme.error_colors();

    match block {
        Block::Text { run, with_padding } => {
            flush_text(md, run, theme, assistant, available_width, *with_padding)
        }
        Block::Reasoning { run, expanded, .. } => {
            flush_reasoning(md, run, theme, reasoning, available_width, *expanded)
        }
        Block::ToolCall {
            tool,
            input,
            output,
            is_error,
            nested,
            expanded,
            ..
        } => {
            let mut lines = flush_tool_call(
                md,
                tool,
                input,
                output.as_deref(),
                *is_error,
                theme,
                tool_req,
                available_width,
                *expanded,
                nested.iter().map(|n| n.calls).sum(),
            );
            // A `rhai` op's nested calls sit right under its header
            // (ADR-0233): all of them while expanded, only the latest while
            // collapsed — the header counts them. Lines 0/1 are padding +
            // header.
            let shown = if *expanded {
                &nested[..]
            } else {
                &nested[nested.len().saturating_sub(1)..]
            };
            let at = lines.len().min(2);
            let steps = shown
                .iter()
                .flat_map(|n| render_nested_call(n, theme, tool_req, available_width));
            lines.splice(at..at, steps);
            lines
        }
        Block::User { text, pending } => render_user(text, *pending, theme, user, available_width),
        Block::ToolOutput { tool, output } => render_tool_output(
            tool.as_deref(),
            output,
            theme,
            tool_out,
            available_width,
            md,
        ),
        Block::Error { message } => render_error(message, theme, error, available_width),
        Block::NestedCall(line) => render_nested_call(line, theme, tool_req, available_width),
        Block::Done => vec![Line::from("")],
    }
}

/// A script's nested call (ADR-0233): one dim `↳` line indented under the
/// `rhai` call's header, in the tool-call colors so it reads as part of it.
fn render_nested_call(
    nested: &NestedLine,
    theme: Theme,
    colors: RoleColors,
    available_width: u16,
) -> Vec<Line<'static>> {
    let (sigil, style) = if nested.is_error {
        ("  ✗ ", theme.fg(Slot::Error))
    } else {
        ("  ↳ ", Style::default().fg(colors.fg).dim())
    };
    let nested = Line::from(vec![
        Span::styled(sigil, style),
        Span::styled(nested.line.clone(), style),
    ]);
    wrap::wrap_line(nested, available_width.saturating_sub(4))
        .into_iter()
        .map(|wline| theme.decorate(wline, colors, available_width))
        .collect()
}

fn render_user(
    text: &str,
    pending: bool,
    theme: Theme,
    user: RoleColors,
    available_width: u16,
) -> Vec<Line<'static>> {
    let padding = padding_line(user, available_width);
    let mut out = vec![padding.clone()];
    for line in text.lines() {
        let user_line = Line::from(vec![Span::styled(
            line.to_string(),
            if pending {
                Style::default().fg(user.fg).dim()
            } else {
                Style::default().fg(user.fg)
            },
        )]);
        for wline in wrap::wrap_line(user_line, available_width.saturating_sub(4)) {
            out.push(theme.decorate(wline, user, available_width));
        }
    }
    out.push(padding);
    out
}

fn render_tool_output(
    tool: Option<&str>,
    output: &str,
    theme: Theme,
    tool_out: RoleColors,
    available_width: u16,
    md: &MarkdownRenderer,
) -> Vec<Line<'static>> {
    let padding = padding_line(tool_out, available_width);
    let mut out = vec![padding.clone()];
    // Same `▸ tool  arg` idiom as the block header and approval tail (#487);
    // there is no call `input` here (only the free-text `output`), so the arg
    // slot is empty and the header falls back to the bare tool name.
    let header = tool_header_spans(
        &theme,
        tool.unwrap_or("output"),
        "",
        '▸',
        tool_out.fg,
        available_width,
        None,
    );
    out.push(theme.decorate(Line::from(header), tool_out, available_width));

    let rendered = tool_render::render_tool_output(tool, output, theme, available_width, md);
    for line in rendered.lines {
        out.push(theme.decorate(line, tool_out, available_width));
    }
    out.push(padding);
    out
}

fn render_error(
    message: &str,
    theme: Theme,
    error: RoleColors,
    available_width: u16,
) -> Vec<Line<'static>> {
    let padding = padding_line(error, available_width);
    let mut out = vec![padding.clone()];
    let error_line = Line::from(vec![
        Span::styled("Error: ", theme.fg(Slot::Error).bold()),
        Span::styled(message.to_string(), theme.fg(Slot::Error)),
    ]);
    for wline in wrap::wrap_line(error_line, available_width.saturating_sub(4)) {
        out.push(theme.decorate(wline, error, available_width));
    }
    out.push(padding);
    out
}
