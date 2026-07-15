use crate::tui::wrap;
use ratatui::{
    style::{Color, Style},
    text::{Line, Span},
};

use crate::tui::app::App;
use crate::tui::markdown::MarkdownRenderer;
use crate::tui::session_view::TranscriptEntry;
use crate::tui::theme::{RoleColors, Theme};
use crate::tui::tool_render;

use super::render_run::{flush_reasoning, flush_text, flush_tool_call};

/// Append the transcript entries. Consecutive `TextDelta`s are streamed
/// token-by-token by the engine, so they're coalesced into one string before
/// markdown rendering — rendering each delta on its own would give every chunk
/// its own hard line break, wrecking word wrap.
pub(super) fn append_transcript<'a>(
    lines: &mut Vec<Line<'a>>,
    regions: &mut Vec<(usize, usize, usize)>,
    markdown_renderer: &'a MarkdownRenderer,
    app: &'a App,
    theme: Theme,
    user: RoleColors,
    available_width: u16,
) {
    let assistant = theme.assistant_colors();
    let reasoning = theme.reasoning_colors();
    let tool_req = theme.tool_req_colors();
    let tool_out = theme.tool_out_colors();
    let error = theme.error_colors();

    let mut pending_text = String::new();
    let mut pending_reasoning = String::new();
    // Transcript index of the first `ReasoningDelta` in the current run — its
    // stable click id, resolved once per coalesced run.
    let mut reasoning_start: Option<usize> = None;
    for (idx, entry) in app.transcript().iter().enumerate() {
        if let TranscriptEntry::TextDelta { text } = entry {
            // Switching text→reasoning would break arrival order, so commit any
            // reasoning run in progress before this text run starts.
            if !pending_reasoning.is_empty() {
                let block_id = reasoning_start.take().unwrap_or(idx);
                flush_reasoning(
                    lines,
                    regions,
                    markdown_renderer,
                    &pending_reasoning,
                    theme,
                    reasoning,
                    available_width,
                    block_id,
                    app.block_expanded(block_id),
                );
                pending_reasoning.clear();
            }
            pending_text.push_str(text);
            continue;
        }
        if let TranscriptEntry::ReasoningDelta { text } = entry {
            // Symmetric flush: commit the text run before the reasoning run so a
            // thinking block that arrives after text renders after it.
            if !pending_text.is_empty() {
                flush_text(
                    lines,
                    markdown_renderer,
                    &mut pending_text,
                    theme,
                    assistant,
                    available_width,
                    true,
                );
            }
            if pending_reasoning.is_empty() {
                reasoning_start = Some(idx);
            }
            pending_reasoning.push_str(text);
            continue;
        }
        // Flush-on-switch keeps the two accumulators mutually exclusive, so at a
        // non-delta boundary at most one is non-empty — flush order is immaterial.
        if !pending_text.is_empty() {
            flush_text(
                lines,
                markdown_renderer,
                &mut pending_text,
                theme,
                assistant,
                available_width,
                true,
            );
        }
        if !pending_reasoning.is_empty() {
            let block_id = reasoning_start.take().unwrap_or(idx);
            flush_reasoning(
                lines,
                regions,
                markdown_renderer,
                &pending_reasoning,
                theme,
                reasoning,
                available_width,
                block_id,
                app.block_expanded(block_id),
            );
            pending_reasoning.clear();
        }

        match entry {
            TranscriptEntry::TextDelta { .. } | TranscriptEntry::ReasoningDelta { .. } => {
                unreachable!()
            }
            TranscriptEntry::User { text, pending } => {
                let padding = Line::from(vec![
                    Span::styled("▌", Style::default().fg(user.fg).bg(user.bg)),
                    Span::raw(" ".repeat(available_width.saturating_sub(1) as usize)),
                ]);
                lines.push(padding.clone());
                for line in text.lines() {
                    let user_line = Line::from(vec![Span::styled(
                        line.to_string(),
                        if *pending {
                            Style::default().fg(user.fg).dim()
                        } else {
                            Style::default().fg(user.fg)
                        },
                    )]);
                    let wrapped = wrap::wrap_line(user_line, available_width.saturating_sub(4));
                    for wline in wrapped {
                        lines.push(theme.decorate(wline, user, available_width));
                    }
                }
                lines.push(padding);
            }
            TranscriptEntry::ToolCall {
                tool,
                input,
                output,
                ..
            } => {
                // One collapsible block per op — the op's index is its stable
                // block id, sharing the reasoning fold machinery (#340).
                flush_tool_call(
                    lines,
                    regions,
                    tool,
                    input,
                    output.as_deref(),
                    theme,
                    tool_req,
                    available_width,
                    idx,
                    app.block_expanded(idx),
                );
            }
            TranscriptEntry::ToolOutput { tool, output } => {
                let padding = Line::from(vec![
                    Span::styled("▌", Style::default().fg(tool_out.fg).bg(tool_out.bg)),
                    Span::raw(" ".repeat(available_width.saturating_sub(1) as usize)),
                ]);
                lines.push(padding.clone());
                let header_text = if let Some(tool_name) = tool {
                    format!("Tool Output ({tool_name}):")
                } else {
                    "Tool Output:".to_string()
                };
                let output_header = Line::from(header_text);
                let wrapped = wrap::wrap_line(output_header, available_width.saturating_sub(4));
                for wline in wrapped {
                    lines.push(theme.decorate(wline, tool_out, available_width));
                }

                let rendered = tool_render::render_tool_output(
                    tool.as_deref(),
                    output,
                    theme,
                    available_width,
                );
                for line in rendered.lines {
                    lines.push(theme.decorate(line, tool_out, available_width));
                }
                lines.push(padding);
            }
            TranscriptEntry::Error { message } => {
                let padding = Line::from(vec![
                    Span::styled("▌", Style::default().fg(error.fg).bg(error.bg)),
                    Span::raw(" ".repeat(available_width.saturating_sub(1) as usize)),
                ]);
                lines.push(padding.clone());
                let error_line = Line::from(vec![
                    Span::styled("Error: ", Style::default().fg(Color::Red).bold()),
                    Span::styled(message, Style::default().fg(Color::Red)),
                ]);
                let wrapped = wrap::wrap_line(error_line, available_width.saturating_sub(4));
                for wline in wrapped {
                    lines.push(theme.decorate(wline, error, available_width));
                }
                lines.push(padding);
            }
            TranscriptEntry::Done => {
                lines.push(Line::from(""));
            }
        }
    }
    // End of stream: at most one accumulator survives (flush-on-switch). The
    // trailing text run is still being streamed, so it renders bar-less.
    if !pending_text.is_empty() {
        flush_text(
            lines,
            markdown_renderer,
            &mut pending_text,
            theme,
            assistant,
            available_width,
            false,
        );
    }
    if !pending_reasoning.is_empty() {
        let block_id = reasoning_start.unwrap_or(app.transcript().len());
        flush_reasoning(
            lines,
            regions,
            markdown_renderer,
            &pending_reasoning,
            theme,
            reasoning,
            available_width,
            block_id,
            app.block_expanded(block_id),
        );
    }
}
