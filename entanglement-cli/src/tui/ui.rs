use entanglement_core::{AgentState, TaskStatus};
use ratatui::{
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Style, Stylize},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Paragraph, Wrap},
    Frame,
};

use crate::tui::app::{App, TranscriptEntry};

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
    draw_input(f, chunks[2], app);

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

    let status = Line::from(vec![
        Span::styled("skutter", Style::default().bold()),
        Span::raw(" | "),
        Span::styled(
            format!("Session: {}", app.session_id()),
            Style::default().dim(),
        ),
        Span::raw(" | "),
        Span::styled(format!("Agent: {}", app.agent()), Style::default()),
        Span::raw(" | "),
        Span::styled(state_text, Style::default().fg(state_color).bold()),
    ]);

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

    // Handle scrolling
    let text = Text::from(lines);
    let paragraph = Paragraph::new(text)
        .wrap(Wrap { trim: true })
        .block(Block::new().borders(Borders::ALL))
        .scroll((app.scroll_offset() as u16, 0));

    f.render_widget(paragraph, area);
}

fn draw_input(f: &mut Frame, area: Rect, app: &mut App) {
    let input = app.input();
    input.set_block(ratatui::widgets::Block::new().borders(Borders::TOP));
    f.render_widget(&*input, area);
}
