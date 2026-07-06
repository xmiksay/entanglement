use entanglement_core::AgentState;
use pulldown_cmark::{Event, Parser, Tag, TagEnd};
use ratatui::{
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Style, Stylize},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Paragraph, Wrap},
    Frame,
};

use crate::tui::app::App;
use crate::tui::input_panel;
use crate::tui::keybindings::LeaderState;
use crate::tui::modals::{self, draw_model_picker, draw_profile_picker};

pub fn draw(f: &mut Frame, app: &mut App) {
    let size = f.area();

    let (main_area, sidebar_area) = if app.showing_sidebar() {
        let sidebar_width = app.sidebar_width();
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

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(0),
            Constraint::Length(1),
            Constraint::Length(2),
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
    let status = Line::from(spans);

    let paragraph = Paragraph::new(status).alignment(Alignment::Left);

    f.render_widget(paragraph, area);
}

fn draw_body(f: &mut Frame, area: Rect, app: &App) {
    let theme = app.theme();
    let margin = theme.chat_margin_left;

    let inner_area = if margin > 0 && area.width > margin {
        Rect::new(area.x + margin, area.y, area.width - margin, area.height)
    } else {
        area
    };

    let lines = crate::tui::transcript::render_body_lines(app, inner_area.width);

    let text = Text::from(lines);
    let paragraph = Paragraph::new(text).scroll((app.scroll_offset() as u16, 0));

    f.render_widget(paragraph, inner_area);
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
            Span::styled(agent, Style::default().fg(app.profile_color_for(agent))),
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
    let theme = app.theme();
    let sidebar_colors = theme.sidebar_colors();
    let sidebar_paragraph = Paragraph::new(sidebar_text)
        .wrap(Wrap { trim: false })
        .block(
            Block::new()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(sidebar_colors.bg)),
        )
        .style(Style::default().bg(sidebar_colors.bg));

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
}
