use ratatui::{
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Style},
    text::{Line, Span, Text},
    widgets::Paragraph,
    Frame,
};

use crate::tui::app::App;
use crate::tui::input_panel;
use crate::tui::keybindings::LeaderState;
use crate::tui::modals::{self, draw_model_picker, draw_profile_picker};

mod sidebar;
use sidebar::draw_sidebar;

pub fn draw(f: &mut Frame, app: &mut App) {
    let size = f.area();

    let (main_area, sidebar_area) = if app.showing_sidebar() {
        // The sidebar is at least 25% of the usable width so its Plan /
        // Tasks sections stay readable, but never below a floor so it
        // doesn't collapse to nothing on a narrow terminal. A stored
        // `sidebar_width` overrides up when the user has grown it.
        let pct = size.width / 4;
        let sidebar_width = app.sidebar_width().max(pct);
        if size.width > sidebar_width + 1 {
            let horizontal_chunks = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([
                    Constraint::Min(0),
                    Constraint::Length(1),
                    Constraint::Length(sidebar_width),
                ])
                .split(size);
            (
                horizontal_chunks[0],
                Some((horizontal_chunks[1], horizontal_chunks[2])),
            )
        } else {
            let horizontal_chunks = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Min(0), Constraint::Length(sidebar_width)])
                .split(size);
            (
                horizontal_chunks[0],
                Some((Rect::default(), horizontal_chunks[1])),
            )
        }
    } else {
        (size, None)
    };

    // D2: the input row grows with the buffer's line count so a multiline draft
    // is fully visible, capped at both 8 rows and one third of the terminal so a
    // long entry never eats the transcript body (`Min(0)` absorbs the slack).
    let input_rows = app.input().lines().len().max(1) as u16;
    let cap = (size.height / 3).max(2);
    let input_height = input_rows.clamp(2, cap.min(8));

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(0),
            Constraint::Length(1),
            Constraint::Length(input_height),
            Constraint::Length(1),
        ])
        .split(main_area);

    draw_status_bar(f, chunks[0], app);
    draw_body(f, chunks[1], app);
    input_panel::draw_top_padding(f, chunks[2], app);

    let input_horizontal_chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Length(app.agent().len() as u16 + 4),
            Constraint::Min(0),
        ])
        .split(chunks[3]);

    input_panel::draw_profile_badge(f, input_horizontal_chunks[0], app);
    input_panel::draw_input(f, input_horizontal_chunks[1], app);

    input_panel::draw_input_info(f, chunks[4], app);

    if let Some((gutter, sidebar)) = sidebar_area {
        if gutter.width > 0 {
            let gutter_para = Paragraph::new("").style(Style::default());
            f.render_widget(gutter_para, gutter);
        }
        draw_sidebar(f, sidebar, app);
    }

    if app.showing_profile_picker() {
        draw_profile_picker(f, app);
    }

    if app.showing_model_picker() {
        draw_model_picker(f, app);
    }

    if app.showing_key_dialog() {
        modals::draw_key_dialog(f, app);
    }

    if app.showing_tools_dialog() {
        modals::draw_tools_dialog(f, app);
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

    if app.showing_resume_modal() {
        modals::draw_resume_modal(f, app);
    }

    if app.showing_inspect() {
        modals::draw_inspect_overlay(f, app);
    }

    if app.showing_mcp_panel() {
        modals::draw_mcp_panel(f, app);
    }

    if matches!(app.leader_handler().state(), LeaderState::Pending { .. }) {
        modals::draw_which_key_popup(f, app.leader_handler().keymap());
    }

    app.clear_dirty();
}

fn draw_status_bar(f: &mut Frame, area: Rect, app: &App) {
    let sessions = app.sessions();
    let background_waiting = sessions.iter().any(|(id, view)| {
        *id != app.active_session_id() && (view.is_waiting_approval() || view.is_asking())
    });

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
    let status = Line::from(spans);

    let paragraph = Paragraph::new(status).alignment(Alignment::Left);

    f.render_widget(paragraph, area);
}

fn draw_body(f: &mut Frame, area: Rect, app: &mut App) {
    let theme = app.theme();
    let margin = theme.chat_margin_left;

    let inner_area = if margin > 0 && area.width > margin {
        Rect::new(area.x + margin, area.y, area.width - margin, area.height)
    } else {
        area
    };

    let body = crate::tui::transcript::render_body_lines(app, inner_area.width);
    let content_height = body.lines.len();
    let viewport_height = inner_area.height as usize;

    // Bottom-anchor: offset 0 is ratatui's *first* line, so following the
    // newest line means scrolling to `content - viewport`. Resolve here where
    // both counts exist; clamp a frozen offset to the same bound.
    let max_offset = content_height.saturating_sub(viewport_height);
    let offset = if app.auto_follow() {
        max_offset
    } else {
        app.scroll_offset().min(max_offset)
    };

    // Owned (no borrow of `app`), so it can outlive the `body.lines` borrow and
    // be handed back after rendering consumes the lines below.
    let line_blocks = body.line_blocks;

    let text = Text::from(body.lines);
    // Clamp rather than truncate: `Paragraph::scroll` takes u16, and a very
    // long transcript would otherwise wrap silently at 65 536 lines.
    let offset_y = offset.min(u16::MAX as usize) as u16;
    let paragraph = Paragraph::new(text).scroll((offset_y, 0));

    f.render_widget(paragraph, inner_area);

    // Record the geometry + line provenance so a mouse click can map a
    // (col, row) back to the transcript block it landed on; and feed the
    // measured metrics back so the next scroll clamps and follow re-arms. Both
    // run after the immutable `body.lines` borrow is consumed above.
    app.set_chat_hit_test(inner_area, offset, line_blocks);
    app.set_viewport_metrics(content_height, viewport_height);
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
        let mut app = App::new_for_test(sid.clone());
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
        terminal.draw(|f| draw_body(f, f.area(), &mut app)).unwrap();
        let buffer = terminal.backend().buffer().clone();

        let non_empty_rows = (0..10)
            .filter(|&y| {
                (0..30)
                    .map(|x| buffer[(x, y)].symbol())
                    .any(|sym| !sym.trim().is_empty())
            })
            .count();

        assert!(
            non_empty_rows <= 4,
            "expected a wrapped paragraph, got {non_empty_rows} non-empty rows"
        );
    }

    /// Feeds N distinctly-labelled one-line deltas so the transcript grows
    /// taller than any small viewport. Each delta carries a trailing newline,
    /// so the coalesced run renders one content line per delta.
    fn app_with_lines(sid: &SessionId, count: u64) -> App {
        let mut app = App::new_for_test(sid.clone());
        for i in 1..=count {
            app.handle_out_event(OutEvent::TextDelta {
                session: sid.clone(),
                seq: i,
                text: format!("row{i}.\n"),
            });
        }
        app
    }

    /// Renders `draw_body` into a fresh backend and returns the visible text as
    /// one newline-joined string (padding/gutter glyphs included).
    fn render_text(app: &mut App, width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|f| draw_body(f, f.area(), app)).unwrap();
        let buffer = terminal.backend().buffer().clone();
        (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn auto_follow_keeps_newest_line_visible() {
        let sid = SessionId::new("s1");
        let mut app = app_with_lines(&sid, 30);

        // Following pins the view to the newest line, not the oldest.
        let text = render_text(&mut app, 20, 8);
        assert!(text.contains("row30."), "newest line should be visible");
        assert!(
            !text.contains("row1."),
            "oldest line should be scrolled off"
        );

        // A further delta after a draw keeps following the newest line.
        app.handle_out_event(OutEvent::TextDelta {
            session: sid.clone(),
            seq: 31,
            text: "row31.\n".into(),
        });
        let text = render_text(&mut app, 20, 8);
        assert!(text.contains("row31."), "new delta should be visible");
    }

    #[test]
    fn scroll_up_freezes_view_across_new_events() {
        let sid = SessionId::new("s1");
        let mut app = app_with_lines(&sid, 30);
        // Prime metrics with one draw, then scroll up off the bottom.
        render_text(&mut app, 20, 8);
        app.scroll_up(5);
        assert!(!app.auto_follow());

        let before = render_text(&mut app, 20, 8);

        // New content must not shift the frozen viewport.
        app.handle_out_event(OutEvent::TextDelta {
            session: sid.clone(),
            seq: 31,
            text: "row31.\n".into(),
        });
        let after = render_text(&mut app, 20, 8);
        assert_eq!(before, after, "frozen view must not move on new events");
        assert!(!after.contains("row31."));
    }

    #[test]
    fn scroll_down_to_bottom_rearms_follow() {
        let sid = SessionId::new("s1");
        let mut app = app_with_lines(&sid, 30);
        render_text(&mut app, 20, 8);
        app.scroll_up(4);
        assert!(!app.auto_follow());
        render_text(&mut app, 20, 8);

        // Scrolling back down to the last line re-arms follow.
        app.scroll_down(4);
        assert!(app.auto_follow());
        let text = render_text(&mut app, 20, 8);
        assert!(text.contains("row30."), "bottom line visible after re-arm");
    }

    #[test]
    fn scroll_down_clamps_at_bottom() {
        let sid = SessionId::new("s1");
        let mut app = app_with_lines(&sid, 30);
        render_text(&mut app, 20, 8);
        app.scroll_up(2);
        render_text(&mut app, 20, 8);
        // Overscroll far past the end: it stops at the bottom and re-arms.
        app.scroll_down(9999);
        assert!(app.auto_follow());
        let text = render_text(&mut app, 20, 8);
        assert!(text.contains("row30."), "clamped at bottom, newest visible");
        assert!(!text.contains("row1."));
    }

    #[test]
    fn scroll_up_clamps_at_top() {
        let sid = SessionId::new("s1");
        let mut app = app_with_lines(&sid, 30);
        render_text(&mut app, 20, 8);
        // Overscroll up: never past the first line.
        app.scroll_up(9999);
        let text = render_text(&mut app, 20, 8);
        assert!(text.contains("row1."), "top line visible");
        assert!(!text.contains("row30."), "must not still show the bottom");
    }
}
