use crate::tui::wrap;
use entanglement_core::TaskStatus;
use ratatui::{
    style::{Color, Style, Stylize},
    text::{Line, Span},
};

use crate::tui::app::App;
use crate::tui::diff::DiffRenderer;
use crate::tui::markdown::MarkdownRenderer;
use crate::tui::session_view::{ApprovalMode, TranscriptEntry};
use crate::tui::theme::{RoleColors, Theme};
pub(crate) fn render_body_lines<'a>(app: &'a App, available_width: u16) -> Vec<Line<'a>> {
    let mut lines = Vec::new();
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

    append_transcript(
        &mut lines,
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
    theme: Theme,
    user: RoleColors,
    available_width: u16,
) {
    fn render_text_run<'a>(
        lines: &mut Vec<Line<'a>>,
        markdown_renderer: &'a MarkdownRenderer,
        run: &str,
        theme: Theme,
        assistant: RoleColors,
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
                let decorated = theme.decorate(line, assistant, available_width);
                lines.push(decorated);
            } else {
                let wrapped = wrap::wrap_line(line, available_width.saturating_sub(4));
                for wline in wrapped {
                    let decorated = theme.decorate(wline, assistant, available_width);
                    lines.push(decorated);
                }
            }
        }
    }

    let assistant = theme.assistant_colors();
    let tool_req = theme.tool_req_colors();
    let tool_out = theme.tool_out_colors();
    let error = theme.error_colors();

    let mut pending_text = String::new();
    for entry in app.transcript() {
        if let TranscriptEntry::TextDelta { text } = entry {
            pending_text.push_str(text);
            continue;
        }
        if !pending_text.is_empty() {
            let padding = Line::from(vec![
                Span::styled("▌", Style::default().fg(assistant.fg).bg(assistant.bg)),
                Span::raw(" ".repeat((available_width - 1) as usize)),
            ]);
            lines.push(padding.clone());
            render_text_run(
                lines,
                markdown_renderer,
                &pending_text,
                theme,
                assistant,
                available_width,
            );
            lines.push(padding);
            pending_text.clear();
        }

        match entry {
            TranscriptEntry::TextDelta { .. } => unreachable!(),
            TranscriptEntry::User { text, pending } => {
                let padding = Line::from(vec![
                    Span::styled("▌", Style::default().fg(user.fg).bg(user.bg)),
                    Span::raw(" ".repeat((available_width - 1) as usize)),
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
            TranscriptEntry::ToolCall { tool, input, .. } => {
                let padding = Line::from(vec![
                    Span::styled("▌", Style::default().fg(tool_req.fg).bg(tool_req.bg)),
                    Span::raw(" ".repeat((available_width - 1) as usize)),
                ]);
                lines.push(padding.clone());
                let request_line = Line::from(vec![
                    Span::styled("Tool Call: ", Style::default().fg(Color::Cyan)),
                    Span::styled(tool, Style::default().bold()),
                ]);
                let wrapped = wrap::wrap_line(request_line, available_width.saturating_sub(4));
                for wline in wrapped {
                    lines.push(theme.decorate(wline, tool_req, available_width));
                }
                for line in input.lines() {
                    let content_line = Line::from(format!("  {line}"));
                    let wrapped = wrap::wrap_line(content_line, available_width.saturating_sub(4));
                    for wline in wrapped {
                        lines.push(theme.decorate(wline, tool_req, available_width));
                    }
                }
                lines.push(padding);
            }
            TranscriptEntry::ToolOutput { tool, output } => {
                let padding = Line::from(vec![
                    Span::styled("▌", Style::default().fg(tool_out.fg).bg(tool_out.bg)),
                    Span::raw(" ".repeat((available_width - 1) as usize)),
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

                if output.contains("---")
                    || output.contains("+++")
                    || output.contains("-")
                    || output.contains("+")
                {
                    let diff_text = DiffRenderer::render_unified(output);
                    for line in diff_text.lines {
                        lines.push(theme.decorate(line, tool_out, available_width));
                    }
                } else {
                    for line in output.lines() {
                        let content_line = Line::from(format!("  {line}"));
                        let wrapped =
                            wrap::wrap_line(content_line, available_width.saturating_sub(4));
                        for wline in wrapped {
                            lines.push(
                                theme
                                    .decorate(wline, tool_out, available_width)
                                    .fg(Color::DarkGray),
                            );
                        }
                    }
                }
                lines.push(padding);
            }
            TranscriptEntry::Error { message } => {
                let padding = Line::from(vec![
                    Span::styled("▌", Style::default().fg(error.fg).bg(error.bg)),
                    Span::raw(" ".repeat((available_width - 1) as usize)),
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
    if !pending_text.is_empty() {
        render_text_run(
            lines,
            markdown_renderer,
            &pending_text,
            theme,
            assistant,
            available_width,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::app::App;
    use crate::tui::theme::hash_profile_color;
    use entanglement_core::{OutEvent, SessionId};

    #[test]
    fn streamed_table_renders_as_grid_after_all_deltas() {
        let sid = SessionId::new("s1");
        let mut app = App::new(sid.clone());
        // A table streamed token-by-token, exactly as the engine emits it.
        let deltas = [
            "| name | role |\n",
            "| --- | --- |\n",
            "| holly | engine |\n",
            "| tui | head |\n",
        ];
        for (i, d) in deltas.iter().enumerate() {
            app.handle_out_event(OutEvent::TextDelta {
                session: sid.clone(),
                seq: i as u64 + 1,
                text: (*d).to_string(),
            });
        }

        let lines = render_body_lines(&app, 80);
        let joined: String = lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .map(|s| s.content.as_ref())
            .collect::<String>()
            .replace('\n', "\\n");
        println!("STREAMED TABLE LINES:");
        for l in &lines {
            let s: String = l.spans.iter().map(|sp| sp.content.as_ref()).collect();
            println!("  {s:?}");
        }
        let has_grid = lines.iter().any(|l| {
            let s: String = l.spans.iter().map(|sp| sp.content.as_ref()).collect();
            s.contains("---")
        });
        assert!(
            has_grid,
            "streamed table did not render as a grid: {joined}"
        );
    }

    #[test]
    fn user_messages_use_profile_colors() {
        let sid = SessionId::new("test");
        let mut app = App::new(sid.clone());
        app.record_user_message("Hello world".to_string());

        let lines = render_body_lines(&app, 80);
        let user_color = hash_profile_color("build");
        let theme = app.theme();
        let expected_user_bg = theme.user_colors(user_color).bg;

        let user_lines: Vec<_> = lines
            .iter()
            .filter(|l| {
                l.spans
                    .iter()
                    .any(|s| s.content.contains("Hello") || s.content.contains("world"))
            })
            .collect();

        assert!(!user_lines.is_empty(), "Should have user message lines");
        for line in user_lines {
            if let Some(bg) = line.style.bg {
                assert_eq!(
                    bg, expected_user_bg,
                    "User message should have message background"
                );
            }
        }
    }

    #[test]
    fn assistant_lines_use_theme_colors() {
        let sid = SessionId::new("test");
        let mut app = App::new(sid.clone());
        app.handle_out_event(OutEvent::TextDelta {
            session: sid.clone(),
            seq: 1,
            text: "Response".to_string(),
        });

        let lines = render_body_lines(&app, 80);
        let theme = app.theme();
        let expected_bg = theme.assistant_colors().bg;

        let assistant_lines: Vec<_> = lines
            .iter()
            .filter(|l| l.spans.iter().any(|s| s.content.contains("Response")))
            .collect();

        assert!(!assistant_lines.is_empty(), "Should have assistant lines");
        for line in assistant_lines {
            if let Some(bg) = line.style.bg {
                assert_eq!(
                    bg, expected_bg,
                    "Assistant lines should use theme message background"
                );
            }
        }
    }
}
