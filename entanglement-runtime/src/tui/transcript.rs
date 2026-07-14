use ratatui::{
    style::{Color, Style, Stylize},
    text::{Line, Span},
};

use crate::tui::app::App;
use crate::tui::session_view::ApprovalMode;

mod append;
mod question;
mod render_run;
#[cfg(test)]
mod tests;

use append::append_transcript;
use question::render_question;

/// Rendered transcript plus per-line provenance. `line_blocks[i]` holds the
/// reasoning-run id (transcript index of the run's first `ReasoningDelta`) that
/// produced rendered line `i`, or `None` for lines that aren't part of a
/// clickable block. Click hit-testing (`App::reasoning_block_at`) maps a
/// `row + scroll_offset` back to a block through this vector.
pub(crate) struct RenderedBody<'a> {
    pub lines: Vec<Line<'a>>,
    pub line_blocks: Vec<Option<usize>>,
}

pub(crate) fn render_body_lines<'a>(app: &'a App, available_width: u16) -> RenderedBody<'a> {
    let mut lines = Vec::new();
    // (block_id, start_line, end_line_exclusive) for each rendered reasoning run.
    let mut regions: Vec<(usize, usize, usize)> = Vec::new();
    let markdown_renderer = app.markdown_renderer();
    let theme = app.theme();
    let user = theme.user_colors(app.profile_color_for(app.agent()));

    if let Some(plan) = app.plan() {
        lines.push(Line::from(""));
        lines.push(Line::from("Plan:").bold());
        let rendered_plan = markdown_renderer.render(plan);
        for line in rendered_plan.lines {
            lines.push(line);
        }
        lines.push(Line::from(""));
    }

    if let Some(tasks) = app.task_list() {
        lines.push(Line::from("Tasks:").bold());
        let rendered_tasks = markdown_renderer.render(tasks);
        for line in rendered_tasks.lines {
            lines.push(line);
        }
        lines.push(Line::from(""));
    }

    append_transcript(
        &mut lines,
        &mut regions,
        markdown_renderer,
        app,
        theme,
        user,
        available_width,
    );

    if let ApprovalMode::WaitingForApproval { .. } = app.approval_mode() {
        if let Some((_, tool, input)) = app.pending_tool_request() {
            lines.push(Line::from(""));
            lines.push(Line::from("─".repeat(60)).fg(Color::Yellow));
            let mut header = vec![
                Span::styled("?", Style::default().fg(Color::Yellow).bold()),
                Span::raw(" "),
                Span::styled(tool, Style::default().fg(Color::Cyan).bold()),
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

    let mut line_blocks = vec![None; lines.len()];
    for (id, start, end) in regions {
        for slot in line_blocks.iter_mut().take(end).skip(start) {
            *slot = Some(id);
        }
    }

    RenderedBody { lines, line_blocks }
}
