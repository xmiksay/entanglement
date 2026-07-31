use entanglement_core::{AgentState, SessionId};
use pulldown_cmark::{Event, Parser, Tag, TagEnd};
use ratatui::{
    style::{Style, Stylize},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Paragraph, Wrap},
    Frame,
};
use unicode_width::UnicodeWidthStr;

use crate::tui::app::App;
use crate::tui::format::{attention_word, short_id};
use ratatui::layout::Rect;

/// Detaches a rendered line from its renderer borrow: the hit-test capture
/// needs `&mut App` after the lines are built, so nothing in `lines` may keep
/// borrowing `App`.
fn own_line(line: Line<'_>) -> Line<'static> {
    let spans: Vec<Span<'static>> = line
        .spans
        .into_iter()
        .map(|s| Span::styled(s.content.into_owned(), s.style))
        .collect();
    Line::from(spans).style(line.style)
}

/// Keeps a session row on one rendered line: the row map below assumes one
/// line per pushed entry, so a line the `Wrap` widget would fold must be
/// flattened and truncated instead (styling is lost only in that overflow
/// case).
fn fit_line(line: Line<'static>, max_width: usize) -> Line<'static> {
    let width: usize = line
        .spans
        .iter()
        .map(|s| UnicodeWidthStr::width(s.content.as_ref()))
        .sum();
    if width <= max_width {
        return line;
    }
    let flat: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
    Line::from(crate::tui::wrap::truncate(&flat, max_width))
}

pub(super) fn draw_sidebar(f: &mut Frame, area: Rect, app: &mut App) {
    let inner_width = area.width.saturating_sub(2) as usize;

    let mut lines: Vec<Line<'static>> = Vec::new();
    // Parallel per-line session map for click hit-testing: a session row and
    // its description line both map to the session; header/blank lines don't.
    let mut row_map: Vec<Option<SessionId>> = Vec::new();

    lines.push(Line::from("Sessions").bold());
    row_map.push(None);
    let active_id = app.active_session_id().clone();
    for (id, view) in app.sessions() {
        let is_active = id.0 == active_id.0;
        // Attention states come from the pending queues (not the flappy
        // `Status`, #273) and stay visually distinct: an approval is not the
        // same ask as a question.
        let (state_word, state_style) = match attention_word(view) {
            Some((word, color)) => (word, Style::default().fg(color).bold()),
            None => (
                match view.state() {
                    AgentState::Idle => "idle",
                    AgentState::Thinking => "thinking",
                    AgentState::Working => "working",
                    AgentState::WaitingAgent => "waiting for agent",
                    AgentState::WaitingApproval => "waiting",
                    AgentState::WaitingAnswer => "waiting",
                    AgentState::Done => "done",
                    AgentState::Error => "error",
                },
                Style::default().dim(),
            ),
        };

        let agent = view.agent().to_string();
        let agent_color = app.profile_color_for(&agent);
        let prefix = if is_active { "* " } else { "  " };
        let line = Line::from(vec![
            Span::raw(prefix),
            Span::styled(
                short_id(id),
                if is_active {
                    Style::default().bold()
                } else {
                    Style::default()
                },
            ),
            Span::raw(" "),
            Span::styled(agent, Style::default().fg(agent_color)),
            Span::raw(" "),
            Span::styled(state_word, state_style),
        ]);
        lines.push(fit_line(line, inner_width));
        row_map.push(Some(id.clone()));

        // One dim description line under the row: the session's first prompt.
        if let Some(desc) = view.first_prompt() {
            let text = crate::tui::wrap::truncate(&format!("    {desc}"), inner_width);
            lines.push(Line::from(Span::styled(text, Style::default().dim())));
            row_map.push(Some(id.clone()));
        }
    }

    lines.push(Line::from(""));

    if let Some(plan_content) = app.plan() {
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
            lines.push(own_line(line));
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

    // Record the inner (border-adjusted) geometry + row map so a click maps
    // back to a session row. Mapped lines were width-fitted above, so the row
    // index equals the rendered line index (nothing above them can wrap).
    let inner = Rect {
        x: area.x + 1,
        y: area.y + 1,
        width: area.width.saturating_sub(2),
        height: (row_map.len() as u16).min(area.height.saturating_sub(2)),
    };
    app.set_sidebar_hit_test(inner, row_map);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::app::App;
    use entanglement_core::{OutEvent, SessionId};
    use ratatui::{backend::TestBackend, Terminal};

    /// Renders `draw_sidebar` into a fresh backend and returns the visible text
    /// as one newline-joined string.
    fn render_sidebar(app: &mut App, width: u16, height: u16) -> String {
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
        let text = render_sidebar(&mut app, 44, 12);
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

        let text = render_sidebar(&mut app, 44, 12);
        assert!(
            text.contains("Tasks"),
            "sidebar should show the tasks section header"
        );
        assert!(text.contains("wire the sidebar"), "should list task items");
        assert!(text.contains("☑"), "completed item keeps its checked glyph");
        assert!(text.contains("☐"), "open item keeps its unchecked glyph");
    }

    #[test]
    fn sidebar_shows_short_id_and_description_not_full_uuid() {
        let sid = SessionId::new("a1b2c3d4-e5f6-7890-abcd-ef0123456789");
        let mut app = App::new_for_test(sid.clone());
        app.record_user_message("fix the login bug in the auth module".to_string());

        let text = render_sidebar(&mut app, 44, 12);
        assert!(text.contains("a1b2c3d4"), "short id shown: {text}");
        assert!(
            !text.contains("a1b2c3d4-e5f6"),
            "full uuid must not render: {text}"
        );
        assert!(
            text.contains("fix the login bug"),
            "first-prompt description shown: {text}"
        );
    }

    #[test]
    fn sidebar_distinguishes_approval_from_question() {
        let active = SessionId::new("active-1");
        let mut app = App::new_for_test(active);
        let approving = SessionId::new("bg-approve");
        let asking = SessionId::new("bg-ask");
        app.handle_out_event(OutEvent::ToolRequest {
            session: approving,
            seq: 1,
            request_id: "r1".to_string(),
            tool: "bash".to_string(),
            input: "{}".to_string(),
        });
        app.handle_out_event(OutEvent::UserQuestion {
            session: asking,
            seq: 1,
            request_id: "q1".to_string(),
            questions: entanglement_core::Questions(vec![entanglement_core::Question {
                question: "pick".to_string(),
                options: Vec::new(),
                multi_select: false,
            }]),
        });

        let text = render_sidebar(&mut app, 44, 12);
        assert!(text.contains("needs approval"), "{text}");
        assert!(text.contains("question"), "{text}");
        assert!(!text.contains("waiting"), "collapsed word replaced: {text}");
    }

    #[test]
    fn click_on_a_session_row_or_its_description_resolves_the_session() {
        let active = SessionId::new("active-1");
        let mut app = App::new_for_test(active.clone());
        app.record_user_message("first prompt".to_string());
        let other = SessionId::new("other-session");
        app.handle_out_event(OutEvent::Status {
            session: other.clone(),
            state: entanglement_core::AgentState::Thinking,
        });

        render_sidebar(&mut app, 30, 12);
        // Inner rows: 0 = "Sessions" header, 1 = active row, 2 = its
        // description, 3 = other row. Border offsets both axes by 1.
        assert_eq!(app.session_at(2, 1), None, "header row maps to nothing");
        assert_eq!(app.session_at(2, 2), Some(active.clone()), "session row");
        assert_eq!(app.session_at(2, 3), Some(active), "description row");
        assert_eq!(app.session_at(2, 4), Some(other), "second session row");
        assert_eq!(app.session_at(0, 2), None, "border column maps to nothing");
        assert_eq!(
            app.session_at(2, 11),
            None,
            "below the list maps to nothing"
        );
    }
}
