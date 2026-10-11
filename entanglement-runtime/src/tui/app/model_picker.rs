//! The `/model` picker and the per-agent model-pin persistence it drives.

use entanglement_core::SessionId;
use entanglement_provider::ModelInfo;
use ratatui::widgets::ListState;

use super::{AgentInfo, App};

/// The transcript notice for a session on the echo stub (#732).
pub(crate) const ECHO_NOTICE: &str = "no provider configured — echo mode: replies only echo \
     the request back. Add a key with /key, then pick a model with /model.";

impl App {
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
        // Echo is never silent (#732, ADR-0222): say so where the replies land.
        if provider == "echo" {
            self.record_notice("provider", ECHO_NOTICE.to_string());
        }
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

    /// Re-derive the `/agent` picker's (read-only) roster from a freshly
    /// reloaded registry (#329), the live-reload counterpart of the roster
    /// [`App::new`][super::construct] builds once at startup. The current
    /// picker selection index is left as-is (best-effort — a picker that
    /// happens to be open mid-reload may briefly point at a shifted row).
    pub fn refresh_profiles(&mut self, entry_profiles: Vec<AgentInfo>) {
        // A reload that somehow yields no entry agent keeps the previous
        // roster rather than emptying the picker it lists unconditionally.
        if entry_profiles.is_empty() {
            return;
        }
        self.available_profiles = entry_profiles;
        self.mark_dirty();
    }

    /// Record a pending persist when the `/model` picker confirms (#323): the
    /// active agent plus the picked `(provider, model)`. The matching
    /// `ModelChanged` for the active session commits it (see
    /// [`persist_model_if_pending`][Self::persist_model_if_pending]); an `Error`
    /// clears it.
    pub fn record_pending_model_persist(&mut self, provider: String, model: String) {
        let agent = self.agent().to_string();
        self.pending_model_persist = Some((agent, provider, model));
    }

    /// Commit a pending persist when its confirming `ModelChanged` arrives for the
    /// active session (#323). Matches the pending `(provider, model)` so an
    /// unrelated `ModelChanged` (e.g. the session-start pin re-announce) never
    /// commits the wrong pin. Writes via the store, drops the pending, and
    /// records a transcript status line. A write failure is logged and
    /// surfaced, never fatal.
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
        self.set_toast(status);
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
}

#[cfg(test)]
mod echo_tests {
    use super::*;
    use crate::tui::session_view::TranscriptEntry;

    #[test]
    fn an_echo_session_is_labeled_in_the_transcript() {
        let mut app = App::new_for_test(SessionId::new("s1"));
        app.set_active_provider("echo".into());
        assert!(matches!(
            app.transcript().last(),
            Some(TranscriptEntry::ToolOutput { tool: Some(t), output })
                if t == "provider" && output.starts_with("no provider configured — echo mode")
        ));
    }

    #[test]
    fn a_real_provider_records_no_notice() {
        let mut app = App::new_for_test(SessionId::new("s1"));
        let before = app.transcript().len();
        app.set_active_provider("zai".into());
        assert_eq!(app.transcript().len(), before);
    }
}
