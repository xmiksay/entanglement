//! Small shared session-display helpers used by the sidebar, the sessions
//! modal, the status bar, and the attention panel.

use entanglement_core::SessionId;
use ratatui::style::Color;

use crate::tui::session_view::SessionView;

/// Human-scale session label: the first 8 chars of the id. A v4 UUID's first
/// group is unique enough to tell sessions apart in a list; short hand-picked
/// ids (tests, embedders) pass through unchanged. Char-indexed, not
/// byte-sliced, so an arbitrary id can never split a codepoint.
pub(crate) fn short_id(id: &SessionId) -> String {
    id.to_string().chars().take(8).collect()
}

/// The attention word (and its accent color) for a session that is parked on
/// user input: an approval prompt or an `ask_user` question. Derived from the
/// pending queues, not `AgentState` — `Status` briefly flaps to `Thinking`
/// between two parked requests (#273), so the queues are the reliable signal.
pub(crate) fn attention_word(view: &SessionView) -> Option<(&'static str, Color)> {
    if view.is_waiting_approval() {
        Some(("needs approval", Color::Yellow))
    } else if view.is_asking() {
        Some(("question", Color::Cyan))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use entanglement_core::{OutEvent, Question, Questions};

    #[test]
    fn short_id_takes_first_eight_chars_of_a_uuid() {
        let id = SessionId::new("a1b2c3d4-e5f6-7890-abcd-ef0123456789");
        assert_eq!(short_id(&id), "a1b2c3d4");
    }

    #[test]
    fn short_id_leaves_short_ids_unchanged() {
        assert_eq!(short_id(&SessionId::new("s1")), "s1");
    }

    #[test]
    fn short_id_is_multibyte_safe() {
        // Char-based take: a multibyte id must not panic or split a codepoint.
        assert_eq!(
            short_id(&SessionId::new("日本語のセッション識別子")),
            "日本語のセッショ"
        );
    }

    #[test]
    fn attention_word_distinguishes_approval_from_question() {
        let sid = SessionId::new("s1");
        let mut view = SessionView::new();
        assert_eq!(attention_word(&view), None);

        view.apply_event(OutEvent::ToolRequest {
            session: sid.clone(),
            seq: 1,
            request_id: "r1".to_string(),
            tool: "bash".to_string(),
            input: "{}".to_string(),
        });
        assert_eq!(
            attention_word(&view),
            Some(("needs approval", Color::Yellow))
        );

        let mut asking = SessionView::new();
        asking.apply_event(OutEvent::UserQuestion {
            session: sid,
            seq: 1,
            request_id: "q1".to_string(),
            questions: Questions(vec![Question {
                question: "pick one".to_string(),
                options: Vec::new(),
                multi_select: false,
            }]),
        });
        assert_eq!(attention_word(&asking), Some(("question", Color::Cyan)));
    }
}
