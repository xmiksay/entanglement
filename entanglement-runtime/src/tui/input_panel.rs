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

/// Compact token-count display with SI-style multipliers (k/M/G) so large
/// per-session totals stay readable in the bottom bar.
fn format_tokens(n: u64) -> String {
    if n < 1_000 {
        n.to_string()
    } else if n < 1_000_000 {
        format!("{:.1}k", n as f64 / 1_000.0)
    } else if n < 1_000_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else {
        format!("{:.1}G", n as f64 / 1_000_000_000.0)
    }
}

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
        AgentState::WaitingAnswer => Color::Cyan,
        AgentState::Done => Color::Blue,
        AgentState::Error => Color::Red,
    };

    let state_text = match app.state() {
        AgentState::Idle => "Idle",
        AgentState::Thinking => "Thinking",
        AgentState::WaitingApproval => "WaitingApproval",
        AgentState::WaitingAnswer => "WaitingAnswer",
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

    // Approval + question prompts render fully in the transcript (view area):
    // the `?` tool header, args, `[y]/[n]` hint, and numbered choices all live
    // there. The input box is only an active text field in the two modes where
    // the user is actually typing — a rejection reason or a free-form answer —
    // so it only carries a placeholder hint then. Otherwise it mirrors the
    // user's pending text (or stays blank), never duplicating the view prompt.
    let placeholder_text = if app.is_asking() {
        if app
            .pending_question()
            .map(|q| q.entering_free_form)
            .unwrap_or(false)
        {
            "Type your answer... (Enter to submit, Esc to go back)"
        } else {
            ""
        }
    } else {
        match &approval_mode {
            ApprovalMode::Normal => {
                "Type a message... | Shift+Enter: newline | Ctrl+J: newline | Enter: send"
            }
            ApprovalMode::WaitingForApproval { .. } => "",
            ApprovalMode::EnteringRejectReason { .. } => {
                "Enter rejection reason... (Enter to send, Esc to cancel)"
            }
        }
    };

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
    // Provider name comes from the resolved catalog entry / `ModelChanged`;
    // show it beside the model when known.
    let provider_display = app.active_provider().to_string();
    let tokens_display = if app.cost_usd() > 0.0 {
        format!(
            "{} in / {} out (${:.4})",
            format_tokens(app.input_tokens()),
            format_tokens(app.output_tokens()),
            app.cost_usd()
        )
    } else {
        format!(
            "{} in / {} out",
            format_tokens(app.input_tokens()),
            format_tokens(app.output_tokens())
        )
    };

    // `provider · model` pair, skipping the provider segment + separator when
    // it's unknown so we never leave a dangling `·`.
    let mut pm_spans: Vec<Span> = Vec::new();
    if !provider_display.is_empty() {
        pm_spans.push(Span::styled(
            provider_display,
            Style::default().fg(Color::Magenta),
        ));
        pm_spans.push(Span::raw(" · "));
    }
    pm_spans.push(Span::styled(
        model_display,
        Style::default().fg(Color::Cyan),
    ));

    // A pending two-stage quit (ADR-0087) replaces the help text with a
    // highlighted "press again" hint so the armed state is unmissable.
    let mut spans: Vec<Span> = pm_spans;
    spans.push(Span::raw(" | "));
    spans.push(Span::styled(
        tokens_display,
        Style::default().fg(Color::Yellow),
    ));
    if app.quit_pending() {
        spans.push(Span::raw(" | "));
        spans.push(Span::styled(
            "Press Ctrl+C again to quit",
            Style::default().fg(Color::Yellow).bold(),
        ));
    } else {
        spans.push(Span::raw(" | "));
        spans.push(Span::styled(
            app.help_text().to_string(),
            Style::default().dim(),
        ));
    }
    let info_line = Line::from(spans);

    let paragraph = Paragraph::new(info_line)
        .alignment(Alignment::Right)
        .style(Style::default().bg(theme.input_bg));
    f.render_widget(paragraph, area);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_tokens_uses_si_multipliers() {
        assert_eq!(format_tokens(0), "0");
        assert_eq!(format_tokens(999), "999");
        assert_eq!(format_tokens(1_000), "1.0k");
        assert_eq!(format_tokens(2_500), "2.5k");
        assert_eq!(format_tokens(1_000_000), "1.0M");
        assert_eq!(format_tokens(1_500_000), "1.5M");
        assert_eq!(format_tokens(1_000_000_000), "1.0G");
    }
}
