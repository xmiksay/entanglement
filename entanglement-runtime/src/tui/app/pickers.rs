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

    /// Page the profile-picker selection forward by `n`, clamping at the last
    /// profile (no wrap, unlike [`profile_picker_next`][Self::profile_picker_next]).
    pub fn profile_picker_page_down(&mut self, n: usize) {
        if self.available_profiles.is_empty() {
            return;
        }
        if let Some(selected) = self.profile_picker_state.selected() {
            let last = self.available_profiles.len() - 1;
            self.profile_picker_state
                .select(Some((selected + n).min(last)));
            self.mark_dirty();
        }
    }

    /// Page the profile-picker selection backward by `n`, clamping at the
    /// first profile.
    pub fn profile_picker_page_up(&mut self, n: usize) {
        if self.available_profiles.is_empty() {
            return;
        }
        if let Some(selected) = self.profile_picker_state.selected() {
            self.profile_picker_state
                .select(Some(selected.saturating_sub(n)));
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

    /// Page the sessions-modal selection forward by `n`, clamping at the last
    /// session (no wrap).
    pub fn sessions_modal_page_down(&mut self, n: usize) {
        self.sessions.modal_page_down(n);
        self.mark_dirty();
    }

    /// Page the sessions-modal selection backward by `n`, clamping at the
    /// first session.
    pub fn sessions_modal_page_up(&mut self, n: usize) {
        self.sessions.modal_page_up(n);
        self.mark_dirty();
    }

    pub fn select_session_from_modal(&mut self) {
        self.sessions.select_from_modal();
        self.mark_dirty();
    }

    /// The session id highlighted in the open sessions modal, if any (#6) —
    /// used by the modal's quick keys (`s`/`p`/`r`) to act on that session.
    pub fn modal_selected_session_id(&self) -> Option<SessionId> {
        self.sessions.modal_selected_id()
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

    /// The resolved active model (#218).
    pub fn model_info(&self) -> &ModelInfo {
        &self.model_info
    }

    /// Active provider name, tracked from the initial selection and every
    /// `ModelChanged`. Shown in the bottom bar beside the model name.
    pub fn active_provider(&self) -> &str {
        &self.active_provider
    }

    /// Set the active model, carrying the resolved `ModelInfo` (id, display
    /// name, context window) verbatim. The context window is already resolved on
    /// the incoming `ModelInfo` — re-deriving it from the catalog by id here
    /// would drop it (the id isn't always a catalog key), so we store as-is.
    pub fn set_model_info(&mut self, model_info: ModelInfo) {
        self.model_info = model_info;
        self.mark_dirty();
    }

    /// Seed the active provider name at head startup (the catalog's resolved
    /// entry). `ModelChanged` keeps it current afterwards.
    pub fn set_active_provider(&mut self, provider: String) {
        self.active_provider = provider;
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

    /// Page the model-picker selection forward by `n`, clamping at the last
    /// model across all provider groups (no wrap, unlike
    /// [`model_picker_next`][Self::model_picker_next]).
    pub fn model_picker_page_down(&mut self, n: usize) {
        let total_models: usize = self
            .available_models
            .iter()
            .map(|(_, models)| models.len())
            .sum();
        if total_models == 0 {
            return;
        }
        if let Some(selected) = self.model_picker_state.selected() {
            let last = total_models - 1;
            self.model_picker_state
                .select(Some((selected + n).min(last)));
            self.mark_dirty();
        }
    }

    /// Page the model-picker selection backward by `n`, clamping at the first
    /// model.
    pub fn model_picker_page_up(&mut self, n: usize) {
        let total_models: usize = self
            .available_models
            .iter()
            .map(|(_, models)| models.len())
            .sum();
        if total_models == 0 {
            return;
        }
        if let Some(selected) = self.model_picker_state.selected() {
            self.model_picker_state
                .select(Some(selected.saturating_sub(n)));
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
            // `self.root()` (the canonicalized cwd wired in at startup) is the
            // same cwd `session_store::delete`/`prune` key off, so the modal
            // lists exactly the logs a `d` press can reach.
            if let Ok(mut sessions) = list_sessions(self.root()) {
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

    /// Page the resume-modal selection forward by `n`, clamping at the last
    /// resumable session (no wrap).
    pub fn resume_page_down(&mut self, n: usize) {
        if self.available_sessions.is_empty() {
            return;
        }
        if let Some(selected) = self.resume_state.selected() {
            let last = self.available_sessions.len() - 1;
            self.resume_state.select(Some((selected + n).min(last)));
        }
    }

    /// Page the resume-modal selection backward by `n`, clamping at the first
    /// resumable session.
    pub fn resume_page_up(&mut self, n: usize) {
        if self.available_sessions.is_empty() {
            return;
        }
        if let Some(selected) = self.resume_state.selected() {
            self.resume_state.select(Some(selected.saturating_sub(n)));
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

    /// Delete the session highlighted in the **sessions modal** (Issue 4, Phase
    /// 4.1). The modal lists the *live* set (sessions with an in-memory view),
    /// so this refuses a live id with a status line rather than deleting its
    /// on-disk log underneath it. A past/non-live id is deleted from disk and
    /// its view (if any still hangs around) is left alone — the live set is the
    /// source of truth, not the on-disk log. The modal stays open.
    pub fn delete_session_from_modal(&mut self) {
        let Some(id) = self.modal_selected_session_id() else {
            return;
        };
        // Live-set guard: never delete a session the engine still holds. The
        // modal's `order` is the live set (`SessionRegistry::order`).
        let is_live = self.sessions().iter().any(|(sid, _)| **sid == id);
        if is_live {
            self.record_notice("/sessions", "cannot delete a live session".to_string());
            return;
        }
        let cwd = self.root().to_path_buf();
        match crate::session_store::delete(&cwd, &id) {
            Ok(()) => self.record_notice("/sessions", format!("deleted session {}", id.0)),
            Err(e) => self.record_notice(
                "/sessions",
                format!("could not delete session {}: {e:#}", id.0),
            ),
        }
    }

    /// Delete the session highlighted in the **resume modal** (Issue 4, Phase
    /// 4.1). The resume modal only lists past (persisted, non-live) sessions, so
    /// `d` here always deletes — no live-set guard needed. The deleted entry is
    /// dropped from the modal's list and the selection clamped; the modal stays
    /// open so several can be deleted in a row.
    pub fn delete_resume_session(&mut self) {
        let Some(meta) = self.selected_resume_session() else {
            return;
        };
        let id = meta.id.clone();
        let cwd = self.root().to_path_buf();
        match crate::session_store::delete(&cwd, &id) {
            Ok(()) => {
                // Drop the deleted entry from the modal list and clamp the
                // selection so the highlight stays in range.
                self.available_sessions.retain(|s| s.id != id);
                if let Some(selected) = self.resume_state.selected() {
                    if selected >= self.available_sessions.len()
                        && !self.available_sessions.is_empty()
                    {
                        self.resume_state
                            .select(Some(self.available_sessions.len() - 1));
                    } else if self.available_sessions.is_empty() {
                        self.resume_state.select(None);
                    }
                }
                self.record_notice("/resume", format!("deleted session {}", id.0));
            }
            Err(e) => self.record_notice(
                "/resume",
                format!("could not delete session {}: {e:#}", id.0),
            ),
        }
    }
}
