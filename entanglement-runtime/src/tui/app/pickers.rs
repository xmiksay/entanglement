use entanglement_core::SessionId;
use entanglement_provider::ModelInfo;
use ratatui::widgets::ListState;

use crate::session_store::{list_sessions, LogRecord, SessionMeta};

use super::{App, ProfileInfo};

/// The implicit Tab-cycle ring (`mode: primary` only, #322) derived from an
/// entry-agent roster: cross-vendor `all`-mode agents (ADR-0074) stay reachable
/// via the `/agent` picker but don't flood the ring. Falls back to the whole
/// roster if no primaries exist, so Tab never cycles an empty ring. Shared by
/// [`App::new`][super::construct] and [`App::refresh_profiles`] (#329) so a
/// definitions-watcher reload derives the ring identically to startup.
pub(super) fn primary_order(available_profiles: &[ProfileInfo]) -> Vec<String> {
    let primaries: Vec<String> = available_profiles
        .iter()
        .filter(|p| p.mode == entanglement_core::AgentMode::Primary)
        .map(|p| p.name.clone())
        .collect();
    if primaries.is_empty() {
        available_profiles.iter().map(|p| p.name.clone()).collect()
    } else {
        primaries
    }
}

impl App {
    pub fn showing_profile_picker(&self) -> bool {
        self.showing_profile_picker
    }

    pub fn profile_picker_state(&mut self) -> &mut ListState {
        &mut self.profile_picker_state
    }

    pub fn available_profiles(&self) -> &[ProfileInfo] {
        &self.available_profiles
    }

    pub fn toggle_profile_picker(&mut self) {
        self.showing_profile_picker = !self.showing_profile_picker;
        if self.showing_profile_picker {
            let agent = self.sessions.active_view().agent().to_string();
            let current_index = self
                .available_profiles
                .iter()
                .position(|p| p.name == agent)
                .unwrap_or(0);
            self.profile_picker_state.select(Some(current_index));
        }
        self.mark_dirty();
    }

    pub fn close_profile_picker(&mut self) {
        self.showing_profile_picker = false;
        self.mark_dirty();
    }

    pub fn select_profile_picker(&mut self) -> Option<String> {
        if let Some(selected) = self.profile_picker_state.selected() {
            if selected < self.available_profiles.len() {
                let profile_name = self.available_profiles[selected].name.clone();
                self.showing_profile_picker = false;
                self.mark_dirty();
                return Some(profile_name);
            }
        }
        None
    }

    pub fn profile_picker_next(&mut self) {
        if let Some(selected) = self.profile_picker_state.selected() {
            let next = (selected + 1) % self.available_profiles.len();
            self.profile_picker_state.select(Some(next));
            self.mark_dirty();
        }
    }

    pub fn profile_picker_prev(&mut self) {
        if let Some(selected) = self.profile_picker_state.selected() {
            let prev = if selected == 0 {
                self.available_profiles.len() - 1
            } else {
                selected - 1
            };
            self.profile_picker_state.select(Some(prev));
            self.mark_dirty();
        }
    }

    /// Advance the active session to the next agent in the Tab cycle ring
    /// (`mode: primary` only, #322). When the current agent is off-ring — an
    /// `all`-mode agent picked via the Ctrl+A picker — land on the first ring
    /// entry rather than the one after it.
    pub fn cycle_primary_profile(&mut self) -> Option<String> {
        let current = self.sessions.active_view().agent().to_string();
        let next_index = match self
            .primary_profile_order
            .iter()
            .position(|name| name == &current)
        {
            Some(idx) => (idx + 1) % self.primary_profile_order.len(),
            None => 0,
        };
        let new_agent = self.primary_profile_order[next_index].clone();
        self.sessions.active_view_mut().set_agent(new_agent.clone());
        self.mark_dirty();
        Some(new_agent)
    }

    /// Reverse of [`cycle_primary_profile`][Self::cycle_primary_profile]
    /// (Shift+Tab, #322). Off-ring current agent → the last ring entry.
    pub fn cycle_primary_profile_back(&mut self) -> Option<String> {
        let current = self.sessions.active_view().agent().to_string();
        let len = self.primary_profile_order.len();
        let prev_index = match self
            .primary_profile_order
            .iter()
            .position(|name| name == &current)
        {
            Some(idx) => (idx + len - 1) % len,
            None => len - 1,
        };
        let new_agent = self.primary_profile_order[prev_index].clone();
        self.sessions.active_view_mut().set_agent(new_agent.clone());
        self.mark_dirty();
        Some(new_agent)
    }

    pub fn showing_sessions_modal(&self) -> bool {
        self.sessions.showing_modal()
    }

    pub fn toggle_sessions_modal(&mut self) {
        self.sessions.toggle_modal();
        self.mark_dirty();
    }

    pub fn close_sessions_modal(&mut self) {
        self.sessions.close_modal();
        self.mark_dirty();
    }

    pub fn sessions_modal_state(&mut self) -> &mut ListState {
        self.sessions.modal_state()
    }

    pub fn sessions_modal_next(&mut self) {
        self.sessions.modal_next();
        self.mark_dirty();
    }

    pub fn sessions_modal_prev(&mut self) {
        self.sessions.modal_prev();
        self.mark_dirty();
    }

    pub fn select_session_from_modal(&mut self) {
        self.sessions.select_from_modal();
        self.mark_dirty();
    }

    pub fn showing_model_picker(&self) -> bool {
        self.showing_model_picker
    }

    pub fn model_picker_state(&mut self) -> &mut ListState {
        &mut self.model_picker_state
    }

    pub fn available_models(&self) -> &[(String, Vec<String>)] {
        &self.available_models
    }

    pub fn model_info(&self) -> &ModelInfo {
        &self.model_info
    }

    /// Set the active model, carrying the resolved `ModelInfo` (id, display
    /// name, context window) verbatim. The context window is already resolved on
    /// the incoming `ModelInfo` — re-deriving it from the catalog by id here
    /// would drop it (the id isn't always a catalog key), so we store as-is.
    pub fn set_model_info(&mut self, model_info: ModelInfo) {
        self.model_info = model_info;
        self.mark_dirty();
    }

    /// Install the managed per-agent model store (#323), threaded in from the head
    /// so a `/model` pick under an active profile persists back to disk. Shared
    /// (`Arc<Mutex<..>>`, #329) with the head's definitions watcher, which calls
    /// `reload()` on it directly — this handle always reads the current state.
    pub fn set_agent_models(
        &mut self,
        store: std::sync::Arc<std::sync::Mutex<crate::config::agent_models::AgentModelStore>>,
    ) {
        self.agent_models = Some(store);
    }

    /// Re-derive the `/agent` picker roster + Tab-cycle ring from a freshly
    /// reloaded registry (#329), the live-reload counterpart of the roster
    /// [`App::new`][super::construct] builds once at startup. The current
    /// picker selection index is left as-is (best-effort — a picker that
    /// happens to be open mid-reload may briefly point at a shifted row).
    pub fn refresh_profiles(&mut self, entry_profiles: Vec<ProfileInfo>) {
        // A reload that somehow yields no entry agent keeps the previous
        // roster rather than emptying the picker/ring it indexes unconditionally.
        if entry_profiles.is_empty() {
            return;
        }
        self.primary_profile_order = primary_order(&entry_profiles);
        self.available_profiles = entry_profiles;
        self.mark_dirty();
    }

    /// Record a pending persist when the `/model` picker confirms (#323): the
    /// active agent plus the picked `(provider, model)`. The matching
    /// `ModelChanged` for the active session commits it (see
    /// [`persist_model_if_pending`][Self::persist_model_if_pending]); an `Error`
    /// clears it. A `ModelChanged` from a `SetAgent` pin application has no pending
    /// recorded here, so it never writes.
    pub fn record_pending_model_persist(&mut self, provider: String, model: String) {
        let agent = self.agent().to_string();
        self.pending_model_persist = Some((agent, provider, model));
    }

    /// Commit a pending persist when its confirming `ModelChanged` arrives for the
    /// active session (#323). Matches the pending `(provider, model)` so a
    /// `ModelChanged` raced in by an interleaved `SetAgent` pin never commits the
    /// wrong pin. Writes via the store, drops the pending, and records a transcript
    /// status line. A write failure is logged and surfaced, never fatal.
    pub(super) fn persist_model_if_pending(
        &mut self,
        session: &SessionId,
        provider: &str,
        model: &str,
    ) {
        if session != self.active_session_id() {
            return;
        }
        let Some((agent, p, m)) = self.pending_model_persist.clone() else {
            return;
        };
        if p != provider || m != model {
            return;
        }
        self.pending_model_persist = None;
        let status = match self.agent_models.as_ref() {
            Some(store) => match store.lock().unwrap().set(&agent, &p, &m) {
                Ok(()) => format!("model for agent '{agent}' set to {p}/{m} (persisted)"),
                Err(e) => {
                    tracing::warn!("could not persist model pin for agent '{agent}': {e:#}");
                    format!("model for agent '{agent}' set to {p}/{m} (persist failed)")
                }
            },
            None => return,
        };
        self.sessions
            .active_view_mut()
            .record_status("model", status);
        self.mark_dirty();
    }

    /// Drop a pending persist on an `Error` for the active session (#323): the
    /// switch failed, so nothing is written.
    pub(super) fn clear_pending_model_persist_on_error(&mut self, session: &SessionId) {
        if self.pending_model_persist.is_some() && session == self.active_session_id() {
            self.pending_model_persist = None;
        }
    }

    /// Test accessor: the pending `(agent, provider, model)` persist, if any.
    #[cfg(test)]
    pub(crate) fn pending_model_persist(&self) -> Option<&(String, String, String)> {
        self.pending_model_persist.as_ref()
    }

    /// Test accessor: the persisted pin for `agent` in the installed store.
    #[cfg(test)]
    pub(crate) fn persisted_model_for(&self, agent: &str) -> Option<(String, String)> {
        self.agent_models.as_ref().and_then(|s| {
            s.lock()
                .unwrap()
                .get(agent)
                .map(|(p, m)| (p.to_string(), m.to_string()))
        })
    }

    /// Resolve the highlighted model-picker row to its `(provider, model)` pair
    /// and close the picker (#218). The selection is a flat index across the
    /// per-provider groups, so walk the groups the same way
    /// [`model_picker_next`][Self::model_picker_next] counts them. `None` when
    /// nothing is selected.
    pub fn select_model_picker(&mut self) -> Option<(String, String)> {
        let mut idx = self.model_picker_state.selected()?;
        for (provider, models) in &self.available_models {
            if idx < models.len() {
                let choice = (provider.clone(), models[idx].clone());
                self.showing_model_picker = false;
                self.mark_dirty();
                return Some(choice);
            }
            idx -= models.len();
        }
        None
    }

    pub fn toggle_model_picker(&mut self) {
        self.showing_model_picker = !self.showing_model_picker;
        if self.showing_model_picker {
            self.model_picker_state.select(Some(0));
        }
        self.mark_dirty();
    }

    pub fn close_model_picker(&mut self) {
        self.showing_model_picker = false;
        self.mark_dirty();
    }

    pub fn model_picker_next(&mut self) {
        let total_models: usize = self
            .available_models
            .iter()
            .map(|(_, models)| models.len())
            .sum();
        if let Some(selected) = self.model_picker_state.selected() {
            let next = (selected + 1) % total_models;
            self.model_picker_state.select(Some(next));
            self.mark_dirty();
        }
    }

    pub fn model_picker_prev(&mut self) {
        let total_models: usize = self
            .available_models
            .iter()
            .map(|(_, models)| models.len())
            .sum();
        if let Some(selected) = self.model_picker_state.selected() {
            let prev = if selected == 0 {
                total_models - 1
            } else {
                selected - 1
            };
            self.model_picker_state.select(Some(prev));
            self.mark_dirty();
        }
    }

    pub fn showing_resume_modal(&self) -> bool {
        self.showing_resume_modal
    }

    pub fn resume_state(&mut self) -> &mut ListState {
        &mut self.resume_state
    }

    pub fn toggle_resume_modal(&mut self) {
        self.showing_resume_modal = !self.showing_resume_modal;
        if self.showing_resume_modal {
            if let Ok(mut sessions) = list_sessions(&std::env::current_dir().unwrap_or_default()) {
                // Only root sessions are independently resumable; spawned
                // children live inside their root's file. Most-recent first.
                sessions.retain(|s| s.root);
                sessions.sort_by_key(|s| std::cmp::Reverse(s.last_active));
                self.available_sessions = sessions;
            }
            self.resume_state
                .select(if self.available_sessions.is_empty() {
                    None
                } else {
                    Some(0)
                });
        }
        self.mark_dirty();
    }

    pub fn close_resume_modal(&mut self) {
        self.showing_resume_modal = false;
        self.mark_dirty();
    }

    /// Rebuilds and switches to a session's view from persisted records,
    /// restoring its full visible transcript.
    pub fn restore_session(&mut self, id: SessionId, records: &[LogRecord]) {
        self.sessions.restore_from_records(id, records);
        self.mark_dirty();
    }

    pub fn resume_next(&mut self) {
        if self.available_sessions.is_empty() {
            return;
        }
        if let Some(selected) = self.resume_state.selected() {
            let next = (selected + 1) % self.available_sessions.len();
            self.resume_state.select(Some(next));
        }
    }

    pub fn resume_prev(&mut self) {
        if self.available_sessions.is_empty() {
            return;
        }
        if let Some(selected) = self.resume_state.selected() {
            let prev = if selected == 0 {
                self.available_sessions.len() - 1
            } else {
                selected - 1
            };
            self.resume_state.select(Some(prev));
        }
    }

    pub fn available_sessions(&self) -> &[SessionMeta] {
        &self.available_sessions
    }

    #[cfg(test)]
    pub(crate) fn set_available_sessions_for_test(&mut self, sessions: Vec<SessionMeta>) {
        self.available_sessions = sessions;
    }

    pub fn selected_resume_session(&self) -> Option<SessionMeta> {
        self.resume_state
            .selected()
            .and_then(|i| self.available_sessions.get(i).cloned())
    }
}
