use entanglement_core::TaskStatus;
use ratatui::{
    style::{Color, Style, Stylize},
    text::{Line, Span},
};

use crate::tui::app::App;
use crate::tui::diff::DiffRenderer;
use crate::tui::markdown::MarkdownRenderer;
use crate::tui::session_view::{ApprovalMode, TranscriptEntry};

/// Build every `Line` for the main chat panel: plan snapshot, task list, the
/// streamed transcript (user prompts + assistant deltas + tool I/O), and the
/// inline approval card when a tool call is pending.
///
/// Extracted from `ui::draw_body` so `ui.rs` stays under the 400-line cap and
/// the transcript rendering rules (markdown coalescing, blank-line fidelity,
/// user-message styling) live in one place.
pub(crate) fn render_body_lines<'a>(app: &'a App) -> Vec<Line<'a>> {
    let mut lines = Vec::new();
    let markdown_renderer = app.markdown_renderer();

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
        for task in tasks {
            let symbol = match task.status {
                TaskStatus::Pending => "○",
                TaskStatus::InProgress => "▶",
                TaskStatus::Completed => "✓",
                TaskStatus::Cancelled => "✗",
            };
            lines.push(Line::from(format!("  {} {}", symbol, task.content)));
        }
        lines.push(Line::from(""));
    }

    append_transcript(&mut lines, markdown_renderer, app);

    if let ApprovalMode::WaitingForApproval { .. } = app.approval_mode() {
        if let Some((_, tool, input)) = app.pending_tool_request() {
            lines.push(Line::from(""));
            lines.push(Line::from("─".repeat(60)).fg(Color::Yellow));
            lines.push(Line::from(vec![
                Span::styled("?", Style::default().fg(Color::Yellow).bold()),
                Span::raw(" "),
                Span::styled(tool, Style::default().fg(Color::Cyan).bold()),
            ]));

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

    lines
}

/// Append the transcript entries. Consecutive `TextDelta`s are streamed
/// token-by-token by the engine, so they're coalesced into one string before
/// markdown rendering — rendering each delta on its own would give every chunk
/// its own hard line break, wrecking word wrap.
fn append_transcript<'a>(
    lines: &mut Vec<Line<'a>>,
    markdown_renderer: &'a MarkdownRenderer,
    app: &'a App,
) {
    fn render_text_run<'b>(
        lines: &mut Vec<Line<'b>>,
        markdown_renderer: &'b MarkdownRenderer,
        run: &str,
    ) {
        if run.trim().is_empty() {
            return;
        }
        let rendered = markdown_renderer.render(run);
        // Keep every line the renderer emits, including the blank separators
        // between blocks — dropping them (the old `!spans.is_empty()` filter)
        // collapsed the spacing markdown relies on for readability.
        for line in rendered.lines {
            lines.push(line);
        }
    }

    let mut pending_text = String::new();
    for entry in app.transcript() {
        if let TranscriptEntry::TextDelta { text } = entry {
            pending_text.push_str(text);
            continue;
        }
        if !pending_text.is_empty() {
            render_text_run(lines, markdown_renderer, &pending_text);
            pending_text.clear();
        }

        match entry {
            TranscriptEntry::TextDelta { .. } => unreachable!(),
            TranscriptEntry::User { text } => {
                lines.push(Line::from(""));
                for line in text.lines() {
                    lines.push(Line::from(vec![
                        Span::styled("> ", Style::default().fg(Color::Blue).bold()),
                        Span::styled(line.to_string(), Style::default().fg(Color::Blue)),
                    ]));
                }
            }
            TranscriptEntry::ToolRequest { tool, input, .. } => {
                lines.push(Line::from(""));
                lines.push(Line::from(vec![
                    Span::styled("Tool Request: ", Style::default().fg(Color::Cyan)),
                    Span::styled(tool, Style::default().bold()),
                ]));
                for line in input.lines() {
                    lines.push(Line::from(format!("  {line}")));
                }
            }
            TranscriptEntry::ToolOutput { output } => {
                lines.push(Line::from(""));
                lines.push(Line::from("Tool Output:").fg(Color::DarkGray));

                // Diff outputs get first-class rendering; everything else is
                // dimmed verbatim. The `+`/`-` heuristic is loose on purpose —
                // `DiffRenderer::render_unified` falls back to plain text.
                if output.contains("---")
                    || output.contains("+++")
                    || output.contains("-")
                    || output.contains("+")
                {
                    let diff_text = DiffRenderer::render_unified(output);
                    for line in diff_text.lines {
                        lines.push(line);
                    }
                } else {
                    for line in output.lines() {
                        lines.push(Line::from(format!("  {line}")).fg(Color::DarkGray));
                    }
                }
            }
            TranscriptEntry::Error { message } => {
                lines.push(Line::from(""));
                lines.push(Line::from(vec![
                    Span::styled("Error: ", Style::default().fg(Color::Red).bold()),
                    Span::styled(message, Style::default().fg(Color::Red)),
                ]));
            }
            TranscriptEntry::Done => {
                lines.push(Line::from(""));
                lines.push(Line::from("─".repeat(40)).fg(Color::Blue));
            }
        }
    }
    if !pending_text.is_empty() {
        render_text_run(lines, markdown_renderer, &pending_text);
    }
}
