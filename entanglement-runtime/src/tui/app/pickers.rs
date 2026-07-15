use entanglement_core::SessionId;
use entanglement_provider::ModelInfo;
use ratatui::widgets::ListState;

use crate::session_store::{list_sessions, LogRecord, SessionMeta};

use super::{App, ProfileInfo};

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

    pub fn selected_resume_session(&self) -> Option<SessionMeta> {
        self.resume_state
            .selected()
            .and_then(|i| self.available_sessions.get(i).cloned())
    }
}
