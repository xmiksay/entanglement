use ratatui::{
    style::{Color, Style, Stylize},
    text::{Line, Span},
};

use crate::tui::app::App;
use crate::tui::session_view::ApprovalMode;

mod block;
pub(crate) mod cache;
mod question;
mod render_run;
mod segment;
#[cfg(test)]
mod tests;

use question::render_question;

/// Rendered transcript plus per-line provenance. `line_blocks[i]` holds the
/// clickable-block id (a reasoning run's first `ReasoningDelta` index or a tool
/// op's `ToolCall` index) that produced rendered line `i`, or `None` for lines
/// that aren't part of a clickable block. Click hit-testing (`App::block_at`)
/// maps a `row + scroll_offset` back to a block through this vector.
///
/// The body is owned (`Line<'static>`) so it can be memoized in the per-session
/// [`cache::RenderCache`] and cloned into the `Paragraph` each frame (#342).
pub(crate) struct RenderedBody {
    pub lines: Vec<Line<'static>>,
    pub line_blocks: Vec<Option<usize>>,
}

pub(crate) fn render_body_lines(app: &mut App, available_width: u16) -> RenderedBody {
    let theme = app.theme();
    let user = theme.user_colors(app.profile_color_for(app.agent()));

    // The transcript body is cache-aware: only the block whose content hash
    // changed re-parses markdown; an idle redraw clones owned lines (#342).
    let (mut lines, mut line_blocks) = app.render_cached_body(available_width, theme, user);

    // Plan and task-list snapshots now live in the sidebar's "Plan Outline" /
    // "Tasks" sections (Ctrl+X s, or /plan · /tasks), not inline at the top of
    // the chat transcript (#325).

    // The approval/question tail is small and changes every frame while parked,
    // so it renders fresh after the cached body rather than through the cache.
    if let ApprovalMode::WaitingForApproval { .. } = app.approval_mode() {
        if let Some((_, tool, input)) = app.pending_tool_request() {
            lines.push(Line::from(""));
            lines.push(Line::from("─".repeat(60)).fg(Color::Yellow));
            let mut header = vec![
                Span::styled("?", Style::default().fg(Color::Yellow).bold()),
                Span::raw(" "),
                Span::styled(tool.clone(), Style::default().fg(Color::Cyan).bold()),
            ];
            // Core batch-emits tool calls (#270), so more approvals may be
            // parked behind this one (#273) — show how many are waiting.
            let queued = app.queued_approvals();
            if queued > 0 {
                header.push(Span::styled(
                    format!("  (+{queued} more queued)"),
                    Style::default().fg(Color::DarkGray),
                ));
            }
            lines.push(Line::from(header));

            if let Ok(json) = serde_json::from_str::<serde_json::Value>(input) {
                if let Ok(pretty) = serde_json::to_string_pretty(&json) {
                    for line in pretty.lines() {
                        lines.push(Line::from(format!("  {line}")));
                    }
                } else {
                    for line in input.lines() {
                        lines.push(Line::from(format!("  {line}")));
                    }
                }
            } else {
                for line in input.lines() {
                    lines.push(Line::from(format!("  {line}")));
                }
            }

            lines.push(Line::from(""));
            lines.push(Line::from(vec![
                Span::styled("[y]", Style::default().fg(Color::Green).bold()),
                Span::raw(" approve  "),
                Span::styled("[n]", Style::default().fg(Color::Red).bold()),
                Span::raw(" reject  "),
                Span::styled("[e]", Style::default().fg(Color::Yellow).bold()),
                Span::raw(" edit reason  "),
                Span::styled("[Esc]", Style::default().fg(Color::Gray).bold()),
                Span::raw(" interrupt"),
            ]));
            lines.push(Line::from("─".repeat(60)).fg(Color::Yellow));
        }
    }

    if let Some(q) = app.pending_question() {
        render_question(&mut lines, q, &app.input_text());
    }

    // The freshly-appended tail lines carry no clickable-block provenance.
    line_blocks.resize(lines.len(), None);

    RenderedBody { lines, line_blocks }
}
