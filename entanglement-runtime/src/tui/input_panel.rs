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
                "Type a message... | Enter: send | Alt/Ctrl+J/Shift+Enter: newline | Ctrl+\u{2190}/\u{2192} word | Home/End line | Ctrl+Home/End doc"
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

    // The cursor (row, col) and a vertical/horizontal scroll that keep it in
    // view when content overflows the now-dynamic input box. We compute both
    // from `app.input()` once and freeze them before the mutable `&mut App`
    // borrow hands off to the modals below.
    let (cursor_row, _cursor_col) = app.input().cursor();
    let cursor_col = app.input().cursor_display_col();

    // Vertical: keep the cursor's row on a visible line by scrolling it up once
    // it would fall past the last visible row (`height - 1`).
    let vscroll = cursor_row.saturating_sub(area.height.saturating_sub(1) as usize) as u16;
    // Horizontal (cursor-following): advance the left column so the cursor stays
    // no further right than the last visible column (`width - 1`).
    let hscroll = cursor_col.saturating_sub(area.width.saturating_sub(1) as usize) as u16;

    let paragraph = Paragraph::new(display_text)
        .style(Style::default().fg(Color::White).bg(theme.input_bg))
        .scroll((vscroll, hscroll));
    f.render_widget(paragraph, area);

    // Always place the terminal cursor; the scroll math above guarantees it
    // lands inside `area` by construction (cursor row/col are in-view).
    let cursor_x = area.x + (cursor_col - hscroll as usize) as u16;
    let cursor_y = area.y + (cursor_row - vscroll as usize) as u16;
    f.set_cursor_position((cursor_x, cursor_y));

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
    use crate::tui::app::App;
    use entanglement_core::SessionId;
    use ratatui::{backend::TestBackend, Terminal};

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

    /// D2 + cursor-Y fix: with a 3-line input the terminal cursor must land on
    /// the cursor's row (here the last), not be pinned to the top row. Draw the
    /// input box alone into a TestBackend at a known area and read back the
    /// cursor position the backend recorded.
    #[test]
    fn multiline_input_places_cursor_on_cursor_row() {
        let mut app = App::new_for_test(SessionId::new("s1"));
        // "line1\nline2\nline3" — cursor ends on row 2, col 5.
        app.input().insert_str("line1");
        app.input().insert_newline();
        app.input().insert_str("line2");
        app.input().insert_newline();
        app.input().insert_str("line3");
        assert_eq!(app.input().cursor(), (2, "line3".len()));

        // A 1-row-tall area to prove the cursor Y is driven by `cursor_row`
        // (with vscroll) rather than a hardcoded `area.y`.
        let mut terminal = Terminal::new(TestBackend::new(40, 1)).unwrap();
        terminal
            .draw(|f| {
                let area = Rect::new(0, 0, 40, 1);
                draw_input(f, area, &mut app);
            })
            .unwrap();

        // The only visible row (area.y=0) shows the scrolled-to line, and the
        // terminal cursor sits on that same row at the cursor's column.
        let pos = terminal.backend().cursor_position();
        assert_eq!(pos.y, 0, "cursor Y should be on the visible (scrolled) row");
        assert_eq!(
            pos.x as usize,
            "line3".len(),
            "cursor X should be at the end of line3"
        );
    }

    /// Cursor on the middle row of a tall-enough box renders on that row, not
    /// the first — the core of the bug from complaint #2.
    #[test]
    fn cursor_second_row_renders_on_second_visible_row() {
        let mut app = App::new_for_test(SessionId::new("s1"));
        app.input().insert_str("aaa");
        app.input().insert_newline();
        app.input().insert_str("bbb");
        // cursor on row 1, col 2 (after the first two 'b's of "bbb")
        app.input().move_cursor_left();
        assert_eq!(app.input().cursor(), (1, 2));

        let mut terminal = Terminal::new(TestBackend::new(40, 2)).unwrap();
        terminal
            .draw(|f| {
                let area = Rect::new(0, 0, 40, 2);
                draw_input(f, area, &mut app);
            })
            .unwrap();

        let pos = terminal.backend().cursor_position();
        assert_eq!(pos.y, 1, "cursor on row 1 must render on the second row");
        assert_eq!(pos.x, 2, "cursor X tracks its column");
    }
}
