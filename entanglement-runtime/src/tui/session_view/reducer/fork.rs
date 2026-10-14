//! `OutEvent::SessionForked` (#786, ADR-0223): a `/fork`'s view opens on a
//! lineage notice followed by the history it inherited, rebuilt from the
//! event's verbatim `context` — the same event a restored fork replays, so a
//! live and a resumed fork read alike.

use entanglement_core::{content_text, Message, MessageRole};

use super::*;

impl SessionView {
    pub(super) fn fold_forked(
        &mut self,
        seq: u64,
        forked_from: SessionId,
        at_turn: Option<u32>,
        context: &[Message],
    ) -> bool {
        if seq <= self.last_seen_seq {
            return false;
        }
        self.last_seen_seq = seq;
        let cut = match at_turn {
            Some(n) => format!(" through turn {n}"),
            None => String::new(),
        };
        self.transcript.push(TranscriptEntry::ToolOutput {
            tool: Some("fork".to_string()),
            output: format!(
                "Forked from {}{cut}. The source session stays live; file edits \
                 made there cannot be undone from this fork.",
                crate::tui::format::short_id(&forked_from)
            ),
        });
        self.forked_from = Some(forked_from);
        for message in context {
            self.push_inherited(message);
        }
        true
    }

    fn push_inherited(&mut self, message: &Message) {
        let text = content_text(&message.content);
        match message.role {
            MessageRole::User => {
                if self.first_prompt.is_none() {
                    self.first_prompt = Some(crate::session_store::first_prompt_snippet(&text));
                }
                self.transcript.push(TranscriptEntry::User {
                    text,
                    pending: false,
                });
            }
            MessageRole::Assistant => {
                if !text.is_empty() {
                    self.transcript.push(TranscriptEntry::TextDelta { text });
                }
                for call in &message.tool_calls {
                    self.transcript.push(TranscriptEntry::ToolCall {
                        request_id: Some(call.id.clone()),
                        tool: call.name.clone(),
                        input: call.input.clone(),
                        output: None,
                        is_error: false,
                        nested: Vec::new(),
                    });
                }
                self.transcript.push(TranscriptEntry::Done);
            }
            MessageRole::Tool => {
                let id = message.tool_call_id.as_deref();
                let slot = self.transcript.iter_mut().rev().find_map(|e| match e {
                    TranscriptEntry::ToolCall {
                        request_id, output, ..
                    } if request_id.as_deref() == id => Some(output),
                    _ => None,
                });
                match slot {
                    Some(output) => *output = Some(text),
                    None => self.transcript.push(TranscriptEntry::ToolOutput {
                        tool: None,
                        output: text,
                    }),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use entanglement_core::ToolCall;

    fn forked(context: Vec<Message>) -> OutEvent {
        OutEvent::SessionForked {
            session: SessionId::new("f"),
            seq: 1,
            forked_from: SessionId::new("src"),
            at_turn: Some(1),
            context,
        }
    }

    #[test]
    fn the_inherited_history_renders_under_a_lineage_notice() {
        let mut view = SessionView::new();
        let changed = view.apply_event(forked(vec![
            Message::user("fix it"),
            Message::assistant("on it", vec![ToolCall::new("c1", "read", "{}")]),
            Message::tool("c1", "contents"),
        ]));
        assert!(changed);
        assert_eq!(view.forked_from(), Some(&SessionId::new("src")));
        assert_eq!(view.first_prompt(), Some("fix it"));
        let t = view.transcript();
        assert!(
            matches!(&t[0], TranscriptEntry::ToolOutput { tool: Some(f), output }
            if f == "fork" && output.contains("src") && output.contains("turn 1"))
        );
        assert!(
            matches!(&t[1], TranscriptEntry::User { text, pending: false } if text == "fix it")
        );
        assert!(matches!(&t[2], TranscriptEntry::TextDelta { text } if text == "on it"));
        assert!(
            matches!(&t[3], TranscriptEntry::ToolCall { output: Some(o), .. } if o == "contents")
        );
    }

    #[test]
    fn a_replayed_duplicate_is_ignored() {
        let mut view = SessionView::new();
        view.apply_event(forked(vec![Message::user("a")]));
        let len = view.transcript().len();
        assert!(!view.apply_event(forked(vec![Message::user("a")])));
        assert_eq!(view.transcript().len(), len);
    }
}
