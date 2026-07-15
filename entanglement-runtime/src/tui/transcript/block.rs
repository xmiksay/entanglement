use crate::tui::wrap;
use ratatui::{
    style::{Color, Style},
    text::{Line, Span},
};

use crate::tui::markdown::MarkdownRenderer;
use crate::tui::theme::{RoleColors, Theme};
use crate::tui::tool_render;

use super::render_run::{flush_reasoning, flush_text, flush_tool_call, padding_line};
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
            expanded,
            ..
        } => flush_tool_call(
            tool,
            input,
            output.as_deref(),
            theme,
            tool_req,
            available_width,
            *expanded,
        ),
        Block::User { text, pending } => render_user(text, *pending, theme, user, available_width),
        Block::ToolOutput { tool, output } => {
            render_tool_output(tool.as_deref(), output, theme, tool_out, available_width)
        }
        Block::Error { message } => render_error(message, theme, error, available_width),
        Block::Done => vec![Line::from("")],
    }
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
) -> Vec<Line<'static>> {
    let padding = padding_line(tool_out, available_width);
    let mut out = vec![padding.clone()];
    let header_text = match tool {
        Some(tool_name) => format!("Tool Output ({tool_name}):"),
        None => "Tool Output:".to_string(),
    };
    for wline in wrap::wrap_line(Line::from(header_text), available_width.saturating_sub(4)) {
        out.push(theme.decorate(wline, tool_out, available_width));
    }

    let rendered = tool_render::render_tool_output(tool, output, theme, available_width);
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
        Span::styled("Error: ", Style::default().fg(Color::Red).bold()),
        Span::styled(message.to_string(), Style::default().fg(Color::Red)),
    ]);
    for wline in wrap::wrap_line(error_line, available_width.saturating_sub(4)) {
        out.push(theme.decorate(wline, error, available_width));
    }
    out.push(padding);
    out
}
