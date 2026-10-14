//! Cross-session attention surfaces: the status bar (whose row content is
//! [`crate::tui::statusline`]) and the attention panel.
//!
//! A background session that parks on a permission Ask or an `ask_user`
//! question used to be invisible — the approval/question UI reads only the
//! active view, so the user had a bell and a bare `!` to go on. The panel
//! (one line above the input box, absent while nothing waits) names the
//! oldest waiting session and what it asks; `Ctrl+G` or a click jumps there,
//! where the existing approval/question flow takes over unchanged. The active
//! session's own pending request already renders as the transcript tail, so
//! the panel covers background sessions only — between the two, every pending
//! request is visible somewhere.

use entanglement_core::SessionId;
use ratatui::{
    layout::Rect,
    style::Style,
    text::{Line, Span},
    widgets::Paragraph,
    Frame,
};

use crate::tui::app::App;
use crate::tui::format::{attention_word, short_id};
use crate::tui::keybindings::Action;
use crate::tui::theme::Slot;

/// One background session waiting on user input.
pub(crate) struct AttentionItem {
    pub session: SessionId,
    pub agent: String,
    /// `needs approval: <tool> <arg>` or `question: <text>`.
    pub summary: String,
}

/// Every non-active session parked on an approval or question, in registry
/// (session-creation) order — the same order [`App::jump_to_next_attention`]
/// picks its target from, so the panel always describes where `Ctrl+G` goes.
pub(crate) fn background_attention(app: &App) -> Vec<AttentionItem> {
    let active = app.active_session_id().clone();
    app.sessions()
        .into_iter()
        .filter(|(id, _)| **id != active)
        .filter_map(|(id, view)| {
            let (word, _) = attention_word(view)?;
            let summary = if let Some(req) = view.pending_tool_request() {
                let tool = &req.tool;
                let arg = crate::tui::transcript::render_run::tool_primary_arg(tool, &req.input)
                    .map(|a| format!(" {a}"))
                    .unwrap_or_default();
                format!("{word}: {tool}{arg}")
            } else if let Some(q) = view.pending_question() {
                if q.is_reviewing() {
                    format!("{word}: review your answers")
                } else {
                    format!("{word}: {}", q.current_question().question)
                }
            } else {
                word.to_string()
            };
            Some(AttentionItem {
                session: id.clone(),
                agent: view.agent().to_string(),
                summary,
            })
        })
        .collect()
}

/// The attention panel's layout height: one row while anything waits, else 0
/// (a `Length(0)` constraint renders nothing, so the panel is structurally
/// absent rather than blanked).
pub(super) fn panel_height(app: &App) -> u16 {
    if background_attention(app).is_empty() {
        0
    } else {
        1
    }
}

/// Renders the one-line attention panel and records its rect for click
/// hit-testing. The paragraph doesn't wrap, so an over-long line clips at the
/// panel edge instead of pushing the layout.
pub(super) fn draw_attention_panel(f: &mut Frame, area: Rect, app: &mut App) {
    let items = background_attention(app);
    let Some(first) = items.first() else {
        app.set_attention_area(Rect::default());
        return;
    };

    let theme = app.theme();
    let mut spans = vec![Span::styled("⚠ ", theme.fg(Slot::Warning).bold())];
    if items.len() > 1 {
        spans.push(Span::styled(
            format!("{} waiting · ", items.len()),
            theme.fg(Slot::Warning),
        ));
    }
    spans.extend([
        Span::styled(short_id(&first.session), Style::default().bold()),
        Span::raw(" "),
        Span::styled("[", Style::default().dim()),
        Span::styled(
            first.agent.clone(),
            Style::default().fg(app.profile_color_for(&first.agent)),
        ),
        Span::styled("]", Style::default().dim()),
        Span::raw(" "),
        Span::styled(first.summary.clone(), theme.fg(Slot::Warning)),
    ]);
    if let Some(key) = app.keymap().first_key(Action::JumpAttention) {
        spans.push(Span::styled(
            format!("  — {key}: jump"),
            Style::default().dim(),
        ));
    }

    f.render_widget(Paragraph::new(Line::from(spans)), area);
    app.set_attention_area(area);
}

/// The top row: the project/path status line (#762) — repo, path, branch,
/// worktree left, session right, then the session-count and waiting-count
/// badges (the latter sourced from the same aggregation as the panel).
pub(super) fn draw_status_bar(f: &mut Frame, area: Rect, app: &App) {
    let line = crate::tui::statusline::status_line(app, area.width);
    f.render_widget(Paragraph::new(line), area);
}

#[cfg(test)]
mod tests {
    use super::*;
    use entanglement_core::{OutEvent, Question, Questions};
    use ratatui::{backend::TestBackend, Terminal};

    fn park_approval(app: &mut App, sid: &SessionId, seq: u64, tool: &str, input: &str) {
        app.handle_out_event(OutEvent::ToolRequest {
            session: sid.clone(),
            seq,
            request_id: format!("r-{sid}-{seq}"),
            tool: tool.to_string(),
            input: input.to_string(),
            reason: None,
            scopes: Vec::new(),
        });
    }

    fn park_question(app: &mut App, sid: &SessionId, text: &str) {
        app.handle_out_event(OutEvent::UserQuestion {
            session: sid.clone(),
            seq: 1,
            request_id: format!("q-{sid}"),
            questions: Questions(vec![Question {
                question: text.to_string(),
                options: Vec::new(),
                multi_select: false,
            }]),
        });
    }

    #[test]
    fn aggregation_excludes_the_active_session() {
        let active = SessionId::new("active-1");
        let mut app = App::new_for_test(active.clone());
        // Active session parks too — it must NOT appear (its own transcript
        // tail already shows the prompt).
        park_approval(&mut app, &active, 1, "bash", r#"{"command":"ls"}"#);
        let bg = SessionId::new("bg-session-1");
        park_approval(&mut app, &bg, 1, "bash", r#"{"command":"cargo test"}"#);

        let items = background_attention(&app);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].session, bg);
        assert_eq!(items[0].summary, "needs approval: bash cargo test");
    }

    #[test]
    fn aggregation_covers_questions_and_orders_by_registry() {
        let active = SessionId::new("active-1");
        let mut app = App::new_for_test(active);
        let b1 = SessionId::new("bg-1");
        let b2 = SessionId::new("bg-2");
        park_approval(&mut app, &b1, 1, "write", r#"{"path":"src/x.rs"}"#);
        park_question(&mut app, &b2, "which database?");

        let items = background_attention(&app);
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].session, b1);
        assert_eq!(items[1].session, b2);
        assert_eq!(items[1].summary, "question: which database?");
    }

    #[test]
    fn resolved_session_drops_out_of_the_aggregation() {
        let active = SessionId::new("active-1");
        let mut app = App::new_for_test(active);
        let bg = SessionId::new("bg-1");
        park_approval(&mut app, &bg, 1, "bash", r#"{"command":"ls"}"#);
        assert_eq!(background_attention(&app).len(), 1);

        // A terminal Status drops the queues (reducer) — the panel must clear.
        app.handle_out_event(OutEvent::Status {
            session: bg,
            state: entanglement_core::AgentState::Done,
        });
        assert!(background_attention(&app).is_empty());
    }

    fn render(app: &mut App, width: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, 1)).unwrap();
        terminal
            .draw(|f| draw_attention_panel(f, f.area(), app))
            .unwrap();
        let buffer = terminal.backend().buffer().clone();
        (0..width).map(|x| buffer[(x, 0)].symbol()).collect()
    }

    #[test]
    fn panel_absent_while_nothing_waits() {
        let mut app = App::new_for_test(SessionId::new("active-1"));
        assert_eq!(panel_height(&app), 0);
        // Draw records a zero rect so a stale click can't hit a ghost panel.
        render(&mut app, 60);
        assert!(!app.attention_at(0, 0));
    }

    #[test]
    fn panel_shows_count_short_id_and_summary() {
        let active = SessionId::new("active-1");
        let mut app = App::new_for_test(active);
        let b1 = SessionId::new("a1b2c3d4-e5f6-7890-abcd-ef0123456789");
        let b2 = SessionId::new("bg-2");
        park_approval(&mut app, &b1, 1, "bash", r#"{"command":"cargo test"}"#);
        park_question(&mut app, &b2, "pick one");

        assert_eq!(panel_height(&app), 1);
        let text = render(&mut app, 80);
        assert!(text.contains("2 waiting"), "count shown: {text}");
        assert!(text.contains("a1b2c3d4"), "short id shown: {text}");
        assert!(
            !text.contains("a1b2c3d4-e5f6"),
            "full uuid must not render: {text}"
        );
        assert!(text.contains("needs approval: bash cargo test"), "{text}");
        assert!(app.attention_at(0, 0), "panel rect recorded for clicks");
    }
}
