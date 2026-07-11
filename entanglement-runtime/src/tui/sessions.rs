use entanglement_core::{InMsg, OutEvent, SessionId};
use ratatui::widgets::ListState;
use std::collections::HashMap;

use crate::session_store::{LogPayload, LogRecord};
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

    /// Adopt an externally-minted session id: create its view if absent and
    /// switch to it. Used by the `propose_plan` handoff (#141), which mints a
    /// fresh root `build` session head-side rather than through [`create`].
    pub fn adopt(&mut self, id: SessionId) {
        self.view_or_insert(&id);
        self.switch_to(id);
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

    /// Rebuilds a `SessionView` from persisted log records and switches to it,
    /// restoring the full visible transcript of a resumed session. The view is
    /// built fresh (seq-dedupe starts at 0) by folding `In(Prompt)` records as
    /// user messages and `Out` events through the normal `apply_event` path — the
    /// same reducers a live session uses.
    pub fn restore_from_records(&mut self, id: SessionId, records: &[LogRecord]) {
        let mut view = SessionView::new();
        for record in records {
            match &record.payload {
                LogPayload::In(InMsg::Prompt { text, .. }) => {
                    view.record_user_message(text.clone());
                }
                LogPayload::In(_) => {}
                LogPayload::Out(event) => {
                    view.apply_event(event.clone());
                }
                // A gap tombstone carries no transcript content. Resume refuses a
                // gapped log upstream, so this only guards a stray restore.
                LogPayload::Gap { .. } => {}
            }
        }

        if !self.order.contains(&id) {
            self.order.push(id.clone());
        }
        self.views.insert(id.clone(), view);
        self.switch_to(id);
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
mod tests;
