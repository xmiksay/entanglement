use entanglement_core::{AgentState, TaskStatus};
use ratatui::{
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Style, Stylize},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Paragraph, Wrap},
    Frame,
};
use std::hash::Hasher;

use crate::tui::app::App;
use crate::tui::keybindings::LeaderState;
use crate::tui::modals::{self, draw_profile_picker};
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

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(0),
            Constraint::Length(3),
        ])
        .split(size);

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

    if app.showing_profile_picker() {
        draw_profile_picker(f, app);
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

    // Add plan if present
    if let Some(plan) = app.plan() {
        lines.push(Line::from(""));
        lines.push(Line::from("Plan:").bold());
        for line in plan.lines() {
            lines.push(Line::from(format!("  {}", line)));
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

    // Add transcript entries
    for entry in app.transcript() {
        match entry {
            TranscriptEntry::TextDelta { text, .. } => {
                for line in text.lines() {
                    lines.push(Line::from(format!("  {}", line)));
                }
            }
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
            TranscriptEntry::ToolOutput { output, .. } => {
                lines.push(Line::from(""));
                lines.push(Line::from("Tool Output:").fg(Color::DarkGray));
                for line in output.lines() {
                    lines.push(Line::from(format!("  {}", line)).fg(Color::DarkGray));
                }
            }
            TranscriptEntry::Error { message, .. } => {
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
        .wrap(Wrap { trim: true })
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
