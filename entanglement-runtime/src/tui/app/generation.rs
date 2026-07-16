//! Persist-on-confirmation for the `/set`/`/show` generation-parameter commands
//! (#376), mirroring the `/model` picker's pin persistence
//! (`pickers.rs`/ADR-0081) but keyed off `OutEvent::GenerationChanged` instead of
//! `ModelChanged`.

use entanglement_provider::GenerationParams;

use entanglement_core::SessionId;

use super::App;

/// Does every `Some` field in `overrides` equal the corresponding field in
/// `generation`? This is the "reflects the pending overrides" test the
/// confirming `GenerationChanged` must pass before a pending `/set` commits —
/// an unrelated `GenerationChanged` (e.g. a `/show` query, or a `SetAgent`
/// reapplication racing in) must never be mistaken for it.
fn reflects(overrides: &GenerationParams, generation: &GenerationParams) -> bool {
    (overrides.temperature.is_none() || overrides.temperature == generation.temperature)
        && (overrides.max_output_tokens.is_none()
            || overrides.max_output_tokens == generation.max_output_tokens)
        && (overrides.thinking_budget_tokens.is_none()
            || overrides.thinking_budget_tokens == generation.thinking_budget_tokens)
        && (overrides.reasoning_effort.is_none()
            || overrides.reasoning_effort == generation.reasoning_effort)
}

impl App {
    /// Install the managed per-agent generation store (#376), threaded in from
    /// the head so a `/set` under an active profile persists back to disk.
    pub fn set_agent_generation(
        &mut self,
        store: std::sync::Arc<
            std::sync::Mutex<crate::config::agent_generation::AgentGenerationStore>,
        >,
    ) {
        self.agent_generation = Some(store);
    }

    /// Records a `/set` parse error (unknown key, malformed value) as a
    /// transcript status line (#376) — no engine traffic, so nothing else to
    /// fold; mirrors `App::record_reload_status`'s wrapper pattern.
    pub fn record_set_error(&mut self, message: String) {
        self.sessions
            .active_view_mut()
            .record_status("set", format!("error: {message}"));
        self.mark_dirty();
    }

    /// Record a pending persist when `/set`'s Enter sends `InMsg::SetGeneration`
    /// (#376): the active agent plus the overrides just sent. The matching
    /// `GenerationChanged` for the active session commits it (see
    /// [`handle_generation_changed`][Self::handle_generation_changed]); an
    /// `Error` clears it. A `/show` query sends no overrides here, so it never
    /// records a pending write; neither does a `SetAgent` reapplication.
    pub fn record_pending_generation_persist(&mut self, overrides: GenerationParams) {
        let agent = self.agent().to_string();
        self.pending_generation_persist = Some((agent, overrides));
    }

    /// Fold an incoming `GenerationChanged` for the active session (#376):
    /// always render a transient status line with the current effective params
    /// (this is what `/show` surfaces), and — if it reflects a pending `/set`
    /// write — commit it via the store and note the persisted profile+values.
    /// A `GenerationChanged` for another session, or one that doesn't reflect
    /// the pending overrides (an interleaved `/show`/`SetAgent`), is rendered
    /// but never clears or commits the pending write.
    pub(super) fn handle_generation_changed(
        &mut self,
        session: &SessionId,
        generation: GenerationParams,
    ) {
        if session != self.active_session_id() {
            return;
        }
        let agent = self.agent().to_string();
        let persisted =
            self.pending_generation_persist
                .as_ref()
                .is_some_and(|(pending_agent, overrides)| {
                    *pending_agent == agent && reflects(overrides, &generation)
                });
        let status = if persisted {
            self.pending_generation_persist = None;
            match self.agent_generation.as_ref() {
                Some(store) => {
                    match store.lock().unwrap().set(&agent, generation) {
                        Ok(()) => format!(
                            "generation for agent '{agent}' set to {generation:?} (persisted)"
                        ),
                        Err(e) => {
                            tracing::warn!(
                                "could not persist generation override for agent '{agent}': {e:#}"
                            );
                            format!("generation for agent '{agent}' set to {generation:?} (persist failed)")
                        }
                    }
                }
                None => format!("generation: {generation:?}"),
            }
        } else {
            format!("generation: {generation:?}")
        };
        self.sessions
            .active_view_mut()
            .record_status("generation", status);
        self.mark_dirty();
    }

    /// Drop a pending persist on an `Error` for the active session (#376): the
    /// `SetGeneration` failed, so nothing is written.
    pub(super) fn clear_pending_generation_persist_on_error(&mut self, session: &SessionId) {
        if self.pending_generation_persist.is_some() && session == self.active_session_id() {
            self.pending_generation_persist = None;
        }
    }

    /// Test accessor: the pending `(agent, overrides)` persist, if any.
    #[cfg(test)]
    pub(crate) fn pending_generation_persist(&self) -> Option<&(String, GenerationParams)> {
        self.pending_generation_persist.as_ref()
    }

    /// Test accessor: the persisted generation override for `agent` in the
    /// installed store.
    #[cfg(test)]
    pub(crate) fn persisted_generation_for(&self, agent: &str) -> Option<GenerationParams> {
        self.agent_generation
            .as_ref()
            .and_then(|s| s.lock().unwrap().get(agent))
    }
}
