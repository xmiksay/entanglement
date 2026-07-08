use entanglement_core::{OutEvent, SessionId};
use ratatui::widgets::ListState;
use std::collections::HashMap;

use crate::tui::session_view::SessionView;

/// Owns every `SessionView` the head has seen and which one is active.
/// Kept separate from `App` so the routing/lifecycle logic (switch, create,
/// auto-discover, sessions modal) can be unit-tested without the input/
/// profile-picker state that lives on `App`.
pub struct SessionRegistry {
    active: SessionId,
    order: Vec<SessionId>,
    views: HashMap<SessionId, SessionView>,
    base_name: String,
    next_ordinal: u64,
    showing_modal: bool,
    modal_state: ListState,
}

impl SessionRegistry {
    pub fn new(initial: SessionId) -> Self {
        let base_name = initial.to_string();
        let mut views = HashMap::new();
        views.insert(initial.clone(), SessionView::new());

        let mut modal_state = ListState::default();
        modal_state.select(Some(0));

        Self {
            active: initial.clone(),
            order: vec![initial],
            views,
            base_name,
            next_ordinal: 1,
            showing_modal: false,
            modal_state,
        }
    }

    pub fn active_id(&self) -> &SessionId {
        &self.active
    }

    pub fn active_view(&self) -> &SessionView {
        self.views
            .get(&self.active)
            .expect("active session always has a view")
    }

    pub fn active_view_mut(&mut self) -> &mut SessionView {
        self.views
            .get_mut(&self.active)
            .expect("active session always has a view")
    }

    fn view_or_insert(&mut self, id: &SessionId) -> &mut SessionView {
        if !self.views.contains_key(id) {
            self.views.insert(id.clone(), SessionView::new());
            self.order.push(id.clone());
        }
        self.views.get_mut(id).expect("just inserted")
    }

    pub fn switch_to(&mut self, id: SessionId) {
        if self.views.contains_key(&id) {
            self.active = id;
        }
    }

    /// Creates a new session view head-side; the engine spawns the matching
    /// task lazily on the session's first `InMsg` (holly.rs), so nothing is
    /// sent here.
    pub fn create(&mut self) -> SessionId {
        loop {
            self.next_ordinal += 1;
            let candidate = SessionId::new(format!("{}-{}", self.base_name, self.next_ordinal));
            if !self.views.contains_key(&candidate) {
                self.views.insert(candidate.clone(), SessionView::new());
                self.order.push(candidate.clone());
                self.switch_to(candidate.clone());
                return candidate;
            }
        }
    }

    pub fn all(&self) -> Vec<(&SessionId, &SessionView)> {
        self.order
            .iter()
            .filter_map(|id| self.views.get(id).map(|v| (id, v)))
            .collect()
    }

    /// Routes an event into its session's view, auto-discovering sessions
    /// seen for the first time on the broadcast. Background sessions keep
    /// accumulating state even while another session is active, so nothing
    /// is dropped when the user switches away. Returns whether anything changed.
    pub fn handle_out_event(&mut self, event: OutEvent) -> bool {
        let id = event.session().clone();
        self.view_or_insert(&id).apply_event(event)
    }

    pub fn showing_modal(&self) -> bool {
        self.showing_modal
    }

    pub fn toggle_modal(&mut self) {
        self.showing_modal = !self.showing_modal;
        if self.showing_modal {
            let current_index = self
                .order
                .iter()
                .position(|id| id == &self.active)
                .unwrap_or(0);
            self.modal_state.select(Some(current_index));
        }
    }

    pub fn close_modal(&mut self) {
        self.showing_modal = false;
    }

    pub fn modal_state(&mut self) -> &mut ListState {
        &mut self.modal_state
    }

    pub fn modal_next(&mut self) {
        if self.order.is_empty() {
            return;
        }
        if let Some(selected) = self.modal_state.selected() {
            self.modal_state
                .select(Some((selected + 1) % self.order.len()));
        }
    }

    pub fn modal_prev(&mut self) {
        if self.order.is_empty() {
            return;
        }
        if let Some(selected) = self.modal_state.selected() {
            let prev = if selected == 0 {
                self.order.len() - 1
            } else {
                selected - 1
            };
            self.modal_state.select(Some(prev));
        }
    }

    /// Switches to the highlighted session and closes the modal.
    pub fn select_from_modal(&mut self) {
        if let Some(selected) = self.modal_state.selected() {
            if let Some(id) = self.order.get(selected).cloned() {
                self.switch_to(id);
            }
        }
        self.showing_modal = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::session_view::{ApprovalMode, TranscriptEntry};

    fn event(session: &SessionId, seq: u64, text: &str) -> OutEvent {
        OutEvent::TextDelta {
            session: session.clone(),
            seq,
            text: text.to_string(),
        }
    }

    #[test]
    fn routes_events_to_the_right_session_without_cross_pollution() {
        let a = SessionId::new("a");
        let b = SessionId::new("b");
        let mut reg = SessionRegistry::new(a.clone());

        reg.handle_out_event(event(&a, 1, "hello-a"));
        reg.handle_out_event(event(&b, 1, "hello-b"));

        assert_eq!(reg.active_view().transcript().len(), 1);
        assert!(matches!(
            &reg.active_view().transcript()[0],
            TranscriptEntry::TextDelta { text } if text == "hello-a"
        ));

        let all = reg.all();
        assert_eq!(all.len(), 2);
        let b_view = all.iter().find(|(id, _)| **id == b).unwrap().1;
        assert_eq!(b_view.transcript().len(), 1);
    }

    #[test]
    fn per_session_seq_dedupe_is_independent() {
        let a = SessionId::new("a");
        let b = SessionId::new("b");
        let mut reg = SessionRegistry::new(a.clone());

        reg.handle_out_event(event(&a, 1, "a1"));
        reg.handle_out_event(event(&b, 1, "b1"));
        reg.switch_to(b);
        assert_eq!(reg.active_view().transcript().len(), 1);
    }

    #[test]
    fn background_approval_is_isolated_and_visible_in_sessions_list() {
        let a = SessionId::new("a");
        let b = SessionId::new("b");
        let mut reg = SessionRegistry::new(a.clone());

        reg.handle_out_event(OutEvent::ToolRequest {
            session: b.clone(),
            seq: 1,
            request_id: "t1".to_string(),
            tool: "read".to_string(),
            input: "{}".to_string(),
        });

        assert!(matches!(
            reg.active_view().approval_mode(),
            ApprovalMode::Normal
        ));

        let all = reg.all();
        let b_view = all.iter().find(|(id, _)| **id == b).unwrap().1;
        assert!(b_view.is_waiting_approval());

        reg.switch_to(b);
        assert!(matches!(
            reg.active_view().approval_mode(),
            ApprovalMode::WaitingForApproval { request_id } if request_id == "t1"
        ));
    }

    #[test]
    fn switch_round_trip_preserves_scroll_and_agent() {
        let a = SessionId::new("a");
        let mut reg = SessionRegistry::new(a.clone());
        let b = reg.create();

        reg.switch_to(a.clone());
        reg.active_view_mut().scroll_down(3);
        assert_eq!(reg.active_view().scroll_offset(), 3);

        reg.switch_to(b.clone());
        assert_eq!(reg.active_view().scroll_offset(), 0);

        reg.switch_to(a);
        assert_eq!(reg.active_view().scroll_offset(), 3);
    }

    #[test]
    fn create_generates_unique_incrementing_ids() {
        let mut reg = SessionRegistry::new(SessionId::new("tui"));
        let s2 = reg.create();
        let s3 = reg.create();
        assert_eq!(s2, SessionId::new("tui-2"));
        assert_eq!(s3, SessionId::new("tui-3"));
        assert_eq!(reg.active_id(), &s3);
    }

    #[test]
    fn create_skips_collisions_with_existing_sessions() {
        let a = SessionId::new("tui");
        let mut reg = SessionRegistry::new(a.clone());
        reg.handle_out_event(event(&SessionId::new("tui-2"), 1, "x"));

        let created = reg.create();
        assert_eq!(created, SessionId::new("tui-3"));
    }

    #[test]
    fn acceptance_multiple_sessions_visible_in_modal_switching_renders_right_transcript() {
        let a = SessionId::new("a");
        let b = SessionId::new("b");
        let c = SessionId::new("c");
        let mut reg = SessionRegistry::new(a.clone());

        reg.handle_out_event(event(&a, 1, "hello-a"));
        reg.handle_out_event(event(&b, 1, "hello-b"));
        reg.handle_out_event(event(&c, 1, "hello-c"));

        let all = reg.all();
        assert_eq!(all.len(), 3, "All sessions should be visible");

        assert_eq!(
            reg.active_view().transcript().len(),
            1,
            "Active session has 1 entry"
        );
        assert!(
            matches!(
                &reg.active_view().transcript()[0],
                crate::tui::session_view::TranscriptEntry::TextDelta { text } if text == "hello-a"
            ),
            "Active session 'a' shows correct transcript"
        );

        reg.switch_to(b.clone());
        assert_eq!(
            reg.active_view().transcript().len(),
            1,
            "After switch, active session has 1 entry"
        );
        assert!(
            matches!(
                &reg.active_view().transcript()[0],
                crate::tui::session_view::TranscriptEntry::TextDelta { text } if text == "hello-b"
            ),
            "After switch, session 'b' shows correct transcript"
        );

        reg.switch_to(c.clone());
        assert!(
            matches!(
                &reg.active_view().transcript()[0],
                crate::tui::session_view::TranscriptEntry::TextDelta { text } if text == "hello-c"
            ),
            "After switch to 'c', shows correct transcript"
        );

        reg.switch_to(a.clone());
        assert!(
            matches!(
                &reg.active_view().transcript()[0],
                crate::tui::session_view::TranscriptEntry::TextDelta { text } if text == "hello-a"
            ),
            "Switching back to 'a' still shows correct transcript"
        );
    }

    #[test]
    fn acceptance_new_session_created_on_first_prompt_and_appears_in_list() {
        let initial = SessionId::new("initial");
        let mut reg = SessionRegistry::new(initial.clone());

        reg.handle_out_event(event(&initial, 1, "first message"));

        let new_session = reg.create();
        assert!(new_session.to_string().starts_with("initial-"));

        let all = reg.all();
        assert_eq!(all.len(), 2, "New session appears in list");

        assert!(
            all.iter().any(|(id, _)| *id == &new_session),
            "New session ID is in the list"
        );

        reg.switch_to(new_session.clone());
        reg.handle_out_event(event(&new_session, 1, "new session message"));

        let all = reg.all();
        assert!(
            all.iter()
                .find(|(id, _)| **id == new_session)
                .map(|(_, view)| !view.transcript().is_empty())
                .unwrap_or(false),
            "New session transcript exists"
        );
    }

    #[test]
    fn acceptance_events_from_inactive_sessions_dont_pollute_active_view() {
        let active = SessionId::new("active");
        let background = SessionId::new("background");
        let mut reg = SessionRegistry::new(active.clone());

        reg.handle_out_event(event(&active, 1, "active-1"));

        reg.handle_out_event(event(&background, 1, "background-1"));
        reg.handle_out_event(event(&background, 2, "background-2"));

        assert_eq!(
            reg.active_view().transcript().len(),
            1,
            "Active session only has its own events"
        );
        assert!(
            matches!(
                &reg.active_view().transcript()[0],
                crate::tui::session_view::TranscriptEntry::TextDelta { text } if text == "active-1"
            ),
            "Active session not polluted by background events"
        );

        reg.switch_to(background.clone());
        assert_eq!(
            reg.active_view().transcript().len(),
            2,
            "Background session has its own events"
        );

        reg.handle_out_event(event(&active, 2, "active-2"));

        assert_eq!(
            reg.active_view().transcript().len(),
            2,
            "Background session not polluted by active events"
        );

        reg.switch_to(active.clone());
        assert_eq!(
            reg.active_view().transcript().len(),
            2,
            "Active session now has both its events"
        );
    }
}
