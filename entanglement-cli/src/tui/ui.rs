use entanglement_core::{AgentState, TaskStatus};
use pulldown_cmark::{Event, Parser, Tag, TagEnd};
use ratatui::{
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Style, Stylize},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Paragraph, Wrap},
    Frame,
};
use std::hash::Hasher;

use crate::tui::app::App;
use crate::tui::diff::DiffRenderer;
use crate::tui::keybindings::LeaderState;
use crate::tui::markdown::MarkdownRenderer;
use crate::tui::modals::{self, draw_model_picker, draw_profile_picker};
use crate::tui::session_view::{ApprovalMode, TranscriptEntry};

pub(crate) fn agent_color(name: &str) -> Color {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    std::hash::Hash::hash(name, &mut hasher);
    let hash = hasher.finish();

    let hue = (hash % 360) as u8;
    let saturation = 70;
    let value = 90;

    hsv_to_rgb(hue, saturation, value)
}

fn hsv_to_rgb(h: u8, s: u8, v: u8) -> Color {
    let h = h as f64 / 360.0;
    let s = s as f64 / 100.0;
    let v = v as f64 / 100.0;

    let c = v * s;
    let x = c * (1.0 - ((h * 6.0) % 2.0 - 1.0).abs());
    let m = v - c;

    let (r, g, b) = if h < 1.0 / 6.0 {
        (c, x, 0.0)
    } else if h < 2.0 / 6.0 {
        (x, c, 0.0)
    } else if h < 3.0 / 6.0 {
        (0.0, c, x)
    } else if h < 4.0 / 6.0 {
        (0.0, x, c)
    } else if h < 5.0 / 6.0 {
        (x, 0.0, c)
    } else {
        (c, 0.0, x)
    };

    Color::Rgb(
        ((r + m) * 255.0) as u8,
        ((g + m) * 255.0) as u8,
        ((b + m) * 255.0) as u8,
    )
}

pub fn draw(f: &mut Frame, app: &mut App) {
    let size = f.area();

    let (main_area, sidebar_area) = if app.showing_sidebar() {
        let horizontal_chunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Min(0), Constraint::Length(app.sidebar_width())])
            .split(size);

        (horizontal_chunks[0], Some(horizontal_chunks[1]))
    } else {
        (size, None)
    };

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(0),
            Constraint::Length(3),
        ])
        .split(main_area);

    draw_status_bar(f, chunks[0], app);
    draw_body(f, chunks[1], app);

    let input_chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Length(app.agent().len() as u16 + 4),
            Constraint::Min(0),
        ])
        .split(chunks[2]);

    draw_profile_badge(f, input_chunks[0], app);
    draw_input(f, input_chunks[1], app);

    if let Some(sidebar) = sidebar_area {
        draw_sidebar(f, sidebar, app);
    }

    if app.showing_profile_picker() {
        draw_profile_picker(f, app);
    }

    if app.showing_model_picker() {
        draw_model_picker(f, app);
    }

    if app.showing_sessions_modal() {
        modals::draw_sessions_modal(f, app);
    }

    if app.showing_help() {
        modals::draw_help_dialog(f, app.leader_handler().keymap());
    }

    if app.showing_command_palette() {
        modals::draw_command_palette(f, app);
    }

    if matches!(app.leader_handler().state(), LeaderState::Pending { .. }) {
        modals::draw_which_key_popup(f, app.leader_handler().keymap());
    }

    app.clear_dirty();
}

fn draw_status_bar(f: &mut Frame, area: Rect, app: &App) {
    let state_color = match app.state() {
        AgentState::Idle => Color::Green,
        AgentState::Thinking => Color::Yellow,
        AgentState::WaitingApproval => Color::Cyan,
        AgentState::Done => Color::Blue,
        AgentState::Error => Color::Red,
    };

    let state_text = match app.state() {
        AgentState::Idle => "Idle",
        AgentState::Thinking => "Thinking",
        AgentState::WaitingApproval => "WaitingApproval",
        AgentState::Done => "Done",
        AgentState::Error => "Error",
    };

    let agent_color = agent_color(app.agent());
    let sessions = app.sessions();
    let background_waiting = sessions
        .iter()
        .any(|(id, view)| *id != app.active_session_id() && view.is_waiting_approval());

    let model_info = app.model_info();
    let model_display = format!("{}/{}", model_info.provider, model_info.model);

    let mut spans = vec![
        Span::styled("skutter", Style::default().bold()),
        Span::raw(" | "),
        Span::styled(
            format!("Session: {}", app.active_session_id()),
            Style::default().dim(),
        ),
    ];
    if sessions.len() > 1 {
        spans.push(Span::styled(
            format!(" ({} sessions)", sessions.len()),
            Style::default().dim(),
        ));
    }
    if background_waiting {
        spans.push(Span::styled(
            " !",
            Style::default().fg(Color::Yellow).bold(),
        ));
    }
    spans.extend([
        Span::raw(" | "),
        Span::styled("[", Style::default().dim()),
        Span::styled(app.agent(), Style::default().fg(agent_color).bold()),
        Span::styled("]", Style::default().dim()),
        Span::raw(" | "),
        Span::styled(model_display, Style::default().fg(Color::Cyan)),
        Span::raw(" | "),
        Span::styled(state_text, Style::default().fg(state_color).bold()),
    ]);
    let status = Line::from(spans);

    let paragraph = Paragraph::new(status)
        .alignment(Alignment::Left)
        .block(Block::new().borders(Borders::BOTTOM));

    f.render_widget(paragraph, area);
}

fn draw_body(f: &mut Frame, area: Rect, app: &App) {
    let mut lines = Vec::new();
    let markdown_renderer = MarkdownRenderer::new();

    // Add plan if present
    if let Some(plan) = app.plan() {
        lines.push(Line::from(""));
        lines.push(Line::from("Plan:").bold());
        let rendered_plan = markdown_renderer.render(plan);
        for line in rendered_plan.lines {
            lines.push(line);
        }
        lines.push(Line::from(""));
    }

    // Add task list if present
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

    // Add transcript entries. Consecutive `TextDelta` entries are streamed
    // token-by-token by the engine, so they're coalesced into one string
    // before markdown rendering — rendering each delta on its own would give
    // every chunk its own hard line break, wrecking word wrap.
    fn render_text_run<'a>(
        lines: &mut Vec<Line<'a>>,
        markdown_renderer: &'a MarkdownRenderer,
        run: &str,
    ) {
        if !run.trim().is_empty() {
            let rendered_text = markdown_renderer.render(run);
            for line in rendered_text.lines {
                if !line.spans.is_empty() {
                    lines.push(line);
                }
            }
        }
    }

    let mut pending_text = String::new();
    for entry in app.transcript() {
        if let TranscriptEntry::TextDelta { text } = entry {
            pending_text.push_str(text);
            continue;
        }
        if !pending_text.is_empty() {
            render_text_run(&mut lines, &markdown_renderer, &pending_text);
            pending_text.clear();
        }

        match entry {
            TranscriptEntry::TextDelta { .. } => unreachable!(),
            TranscriptEntry::ToolRequest { tool, input, .. } => {
                lines.push(Line::from(""));
                lines.push(Line::from(vec![
                    Span::styled("Tool Request: ", Style::default().fg(Color::Cyan)),
                    Span::styled(tool, Style::default().bold()),
                ]));
                for line in input.lines() {
                    lines.push(Line::from(format!("  {}", line)));
                }
            }
            TranscriptEntry::ToolOutput { output } => {
                lines.push(Line::from(""));
                lines.push(Line::from("Tool Output:").fg(Color::DarkGray));

                // Check if this looks like a diff output from an edit tool
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
                        lines.push(Line::from(format!("  {}", line)).fg(Color::DarkGray));
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
        render_text_run(&mut lines, &markdown_renderer, &pending_text);
    }

    // Add approval card if waiting for approval
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
                        lines.push(Line::from(format!("  {}", line)));
                    }
                } else {
                    for line in input.lines() {
                        lines.push(Line::from(format!("  {}", line)));
                    }
                }
            } else {
                for line in input.lines() {
                    lines.push(Line::from(format!("  {}", line)));
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

    // Handle scrolling
    let text = Text::from(lines);
    let paragraph = Paragraph::new(text)
        .wrap(Wrap { trim: false })
        .block(Block::new().borders(Borders::ALL))
        .scroll((app.scroll_offset() as u16, 0));

    f.render_widget(paragraph, area);
}

fn draw_profile_badge(f: &mut Frame, area: Rect, app: &App) {
    let agent_color = agent_color(app.agent());

    let badge = Line::from(vec![
        Span::styled("[", Style::default().dim()),
        Span::styled(app.agent(), Style::default().fg(agent_color).bold()),
        Span::styled("]", Style::default().dim()),
    ]);

    let paragraph = Paragraph::new(badge)
        .alignment(Alignment::Left)
        .block(Block::new().borders(Borders::ALL));

    f.render_widget(paragraph, area);
}

fn draw_input(f: &mut Frame, area: Rect, app: &mut App) {
    let approval_mode = app.approval_mode().clone();
    let input = app.input();
    match &approval_mode {
        ApprovalMode::Normal => {
            input
                .set_placeholder_text("Type a message... (Enter to send, Shift+Enter for newline) | Tab: cycle agent | Ctrl+A: agent picker | Ctrl+L: sessions");
        }
        ApprovalMode::WaitingForApproval { .. } => {
            input.set_placeholder_text("Waiting for approval... Use [y] approve, [n] reject, [e] edit reason, [Esc] interrupt");
        }
        ApprovalMode::EnteringRejectReason { .. } => {
            input.set_placeholder_text("Enter rejection reason... (Enter to send, Esc to cancel)");
        }
    }
    input.set_block(ratatui::widgets::Block::new().borders(Borders::TOP));
    f.render_widget(&*input, area);

    if matches!(approval_mode, ApprovalMode::Normal) {
        modals::draw_slash_autocomplete(f, app, area);
    }
}

fn draw_sidebar(f: &mut Frame, area: Rect, app: &App) {
    let sessions = app.sessions();
    let active_id = app.active_session_id();
    let plan = app.plan();

    let mut lines = Vec::new();

    lines.push(Line::from("Sessions").bold());
    for (id, view) in sessions {
        let is_active = id.0 == active_id.0;
        let agent = view.agent();
        let state = match view.state() {
            AgentState::Idle => "idle",
            AgentState::Thinking => "thinking",
            AgentState::WaitingApproval => "waiting",
            AgentState::Done => "done",
            AgentState::Error => "error",
        };

        let prefix = if is_active { "* " } else { "  " };
        let line = Line::from(vec![
            Span::raw(prefix),
            Span::styled(
                format!("{}", id),
                if is_active {
                    Style::default().bold()
                } else {
                    Style::default()
                },
            ),
            Span::raw(" "),
            Span::styled(agent, Style::default().fg(agent_color(agent))),
            Span::raw(" "),
            Span::styled(state, Style::default().dim()),
        ]);
        lines.push(line);
    }

    lines.push(Line::from(""));

    if let Some(plan_content) = plan {
        lines.push(Line::from("Plan Outline").bold());

        let mut current_level = 0;
        let parser = Parser::new(plan_content);

        for event in parser {
            match event {
                Event::Start(Tag::Heading { level, .. }) => {
                    current_level = level as usize;
                }
                Event::End(TagEnd::Heading(_)) => {
                    current_level = 0;
                }
                Event::Text(text) => {
                    if current_level > 0 {
                        let indent = "  ".repeat(current_level.min(3));
                        let prefix = match current_level {
                            1 => "# ",
                            2 => "## ",
                            _ => "• ",
                        };
                        let content = format!("{}{}{}", indent, prefix, text);
                        let truncated = if content.len() > 40 {
                            format!("{}...", &content[..40 - 3])
                        } else {
                            content
                        };
                        lines.push(Line::from(truncated));
                    }
                }
                _ => {}
            }
        }
    }

    let sidebar_text = Text::from(lines);
    let sidebar_paragraph = Paragraph::new(sidebar_text)
        .wrap(Wrap { trim: false })
        .block(Block::new().borders(Borders::ALL));

    f.render_widget(sidebar_paragraph, area);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::app::App;
    use entanglement_core::{OutEvent, SessionId};
    use ratatui::{backend::TestBackend, Terminal};

    #[test]
    fn streamed_text_deltas_wrap_as_one_paragraph() {
        let sid = SessionId::new("s1");
        let mut app = App::new(sid.clone());
        let words = [
            "This", "is", "a", "fairly", "long", "sentence", "that", "should", "wrap", "nicely",
        ];
        for (i, word) in words.iter().enumerate() {
            app.handle_out_event(OutEvent::TextDelta {
                session: sid.clone(),
                seq: i as u64 + 1,
                text: format!("{} ", word),
            });
        }

        let backend = TestBackend::new(30, 10);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| draw_body(f, f.area(), &app)).unwrap();
        let buffer = terminal.backend().buffer().clone();

        // Exclude the bordered frame (top/bottom rows, left/right columns)
        // drawn by Block::Borders::ALL — it's non-blank regardless of content
        // and would otherwise mask the bug.
        let non_empty_rows = (1..9)
            .filter(|&y| {
                (1..29)
                    .map(|x| buffer[(x, y)].symbol())
                    .any(|sym| !sym.trim().is_empty())
            })
            .count();

        // 10 short words at width 30 wrap onto a couple of rows; rendering
        // each streamed delta as its own markdown blob put one word per row.
        assert!(
            non_empty_rows <= 3,
            "expected a wrapped paragraph, got {non_empty_rows} non-empty rows"
        );
    }
}
