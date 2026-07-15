use entanglement_core::AgentState;
use pulldown_cmark::{Event, Parser, Tag, TagEnd};
use ratatui::{
    style::{Style, Stylize},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Paragraph, Wrap},
    Frame,
};

use crate::tui::app::App;
use ratatui::layout::Rect;

pub(super) fn draw_sidebar(f: &mut Frame, area: Rect, app: &App) {
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
            AgentState::WaitingAnswer => "waiting",
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
                Event::Text(text) if current_level > 0 => {
                    let indent = "  ".repeat(current_level.min(3));
                    let prefix = match current_level {
                        1 => "# ",
                        2 => "## ",
                        _ => "• ",
                    };
                    let content = format!("{}{}{}", indent, prefix, text);
                    lines.push(Line::from(crate::tui::wrap::truncate(&content, 40)));
                }
                _ => {}
            }
        }
    }

    if let Some(tasks) = app.task_list() {
        lines.push(Line::from(""));
        lines.push(Line::from("Tasks").bold());
        // Render the checklist markdown through the same renderer the inline
        // transcript used, so the `- [x]`/`- [ ]` items keep their ☑/☐ glyphs.
        let rendered = app.markdown_renderer().render(tasks);
        for line in rendered.lines {
            lines.push(line);
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

    /// Renders `draw_sidebar` into a fresh backend and returns the visible text
    /// as one newline-joined string.
    fn render_sidebar(app: &App, width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|f| draw_sidebar(f, f.area(), app)).unwrap();
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
    fn sidebar_truncates_multibyte_heading_without_panic() {
        // A plan heading whose bytes exceed 40 but whose chars are multibyte
        // (CJK): a fixed byte slice at offset 37 lands mid-codepoint and panics.
        // Width-based truncation must render it and cap it with an ellipsis.
        let sid = SessionId::new("s1");
        let mut app = App::new_for_test(sid.clone());
        app.handle_out_event(OutEvent::Plan {
            session: sid.clone(),
            seq: 1,
            content: "# 日本語のとても長い見出しテキストで四十バイトを優に超える長さ".to_string(),
        });

        // Draw wide enough for the sidebar column; the assertion is that this
        // does not panic while building the truncated outline line.
        let text = render_sidebar(&app, 44, 12);
        assert!(text.contains("..."), "long heading should be truncated");
        assert!(
            text.contains("Plan Outline"),
            "sidebar should show the plan outline section"
        );
    }

    #[test]
    fn sidebar_renders_task_list_section() {
        // A stored `TaskList` must surface as a "Tasks" section with per-item
        // status glyphs (☑/☐) matching the inline transcript renderer (#325).
        let sid = SessionId::new("s1");
        let mut app = App::new_for_test(sid.clone());
        app.handle_out_event(OutEvent::TaskList {
            session: sid.clone(),
            seq: 1,
            content: "- [x] wire the sidebar\n- [ ] ship it".to_string(),
        });

        let text = render_sidebar(&app, 44, 12);
        assert!(
            text.contains("Tasks"),
            "sidebar should show the tasks section header"
        );
        assert!(text.contains("wire the sidebar"), "should list task items");
        assert!(text.contains("☑"), "completed item keeps its checked glyph");
        assert!(text.contains("☐"), "open item keeps its unchecked glyph");
    }
}
