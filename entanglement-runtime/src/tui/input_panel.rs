use entanglement_core::AgentState;
use ratatui::{
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Style},
    text::{Line, Span},
    widgets::Paragraph,
    Frame,
};

use crate::tui::app::App;
use crate::tui::modals;
use crate::tui::progress;
use crate::tui::session_view::ApprovalMode;

pub fn draw_top_padding(f: &mut Frame, area: Rect, app: &App) {
    let theme = app.theme();
    let paragraph = Paragraph::new("").style(Style::default().bg(theme.input_bg));
    f.render_widget(paragraph, area);
}

pub fn draw_profile_badge(f: &mut Frame, area: Rect, app: &App) {
    let theme = app.theme();
    let user_input = theme.user_input_colors(app.profile_color_for(app.agent()));

    let agent_color = app.profile_color_for(app.agent());
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

    let badge_top = Line::from(vec![Span::styled(
        app.agent(),
        Style::default().fg(agent_color).bold(),
    )]);

    let badge_bottom = Line::from(vec![Span::styled(
        state_text,
        Style::default().fg(state_color),
    )]);

    let vertical_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Length(1)])
        .split(area);

    let top_badge = Paragraph::new(badge_top)
        .alignment(Alignment::Center)
        .style(Style::default().bg(user_input.bg));
    f.render_widget(top_badge, vertical_chunks[0]);

    if let Some(since) = app.thinking_since() {
        progress::draw_ship_cruise(
            f,
            vertical_chunks[1],
            since,
            app.profile_color_for(app.agent()),
            app.theme(),
        );
    } else {
        let bottom_badge = Paragraph::new(badge_bottom)
            .alignment(Alignment::Center)
            .style(Style::default().bg(user_input.bg));
        f.render_widget(bottom_badge, vertical_chunks[1]);
    }
}

pub fn draw_input(f: &mut Frame, area: Rect, app: &mut App) {
    let approval_mode = app.approval_mode().clone();
    let theme = app.theme();

    // A pending `ask_user` question (ADR-0027) commandeers input; its own
    // placeholder wins over the approval-mode ones below.
    let question_placeholder = app.pending_question().map(|q| {
        if q.entering_free_form {
            "Type your answer... (Enter to submit, Esc to go back)"
        } else {
            "Answer the question above — [↑/↓] select, [1-9] pick, [Enter] choose, [Esc] interrupt"
        }
    });

    let placeholder_text = question_placeholder.unwrap_or(match &approval_mode {
        ApprovalMode::Normal => {
            "Type a message... | Shift+Enter: newline | Ctrl+J: newline | Enter: send"
        }
        ApprovalMode::WaitingForApproval { .. } => {
            "Waiting for approval... Use [y] approve, [n] reject, [e] edit reason, [Esc] interrupt"
        }
        ApprovalMode::EnteringRejectReason { .. } => {
            "Enter rejection reason... (Enter to send, Esc to cancel)"
        }
    });

    let input_text = app.input_text();
    let display_text = if input_text.is_empty() {
        placeholder_text
    } else {
        &input_text
    };

    let paragraph = Paragraph::new(display_text)
        .style(Style::default().fg(Color::White).bg(theme.input_bg))
        .scroll((app.input().scroll_offset(), 0));
    f.render_widget(paragraph, area);
    let cursor_pos = app.input().cursor_display_col() as u16;
    if cursor_pos < area.width {
        f.set_cursor_position((area.x + cursor_pos, area.y));
    }

    if matches!(approval_mode, ApprovalMode::Normal) && !app.is_asking() {
        modals::draw_slash_autocomplete(f, app, area);
        modals::draw_mention_popup(f, app, area);
    }
}

pub fn draw_input_info(f: &mut Frame, area: Rect, app: &App) {
    let theme = app.theme();
    let model_info = app.model_info();

    let model_display = if model_info.id.is_empty() {
        "unknown".to_string()
    } else {
        model_info.display_name.clone()
    };
    let tokens_display = format!("{} in / {} out", app.input_tokens(), app.output_tokens());
    let help_text = app.help_text();

    let info_line = Line::from(vec![
        Span::styled(model_display, Style::default().fg(Color::Cyan)),
        Span::raw(" | "),
        Span::styled(tokens_display, Style::default().fg(Color::Yellow)),
        Span::raw(" | "),
        Span::styled(help_text, Style::default().dim()),
    ]);

    let paragraph = Paragraph::new(info_line)
        .alignment(Alignment::Right)
        .style(Style::default().bg(theme.input_bg));
    f.render_widget(paragraph, area);
}
