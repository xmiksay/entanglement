//! `/aux-model` persist + status render (Issue 5), mirroring the `/allow`
//! command's direct-store-write pattern (`allow_command.rs`/ADR-0126): the pin
//! is head-side persistence, not an engine op, so there is no wire round-trip
//! and no confirm-on-event pairing (unlike `/model`'s `pending_model_persist`).

use crate::config::aux_models::Purpose;

use super::App;

impl App {
    /// Install the managed per-purpose aux-model store (Issue 5), threaded in
    /// from the head so `/aux-model` persists back to disk.
    pub fn set_aux_models(
        &mut self,
        store: std::sync::Arc<std::sync::Mutex<crate::config::aux_models::AuxModelStore>>,
    ) {
        self.aux_models = Some(store);
    }

    /// Records a `/aux-model` parse error (unknown purpose, missing slash,
    /// empty model) as a transcript status line (Issue 5) — no engine traffic,
    /// so nothing else to fold; mirrors `App::record_set_error`'s wrapper
    /// pattern.
    pub fn record_aux_error(&mut self, message: String) {
        self.sessions
            .active_view_mut()
            .record_status("aux-model", format!("error: {message}"));
        self.mark_dirty();
    }

    /// Render a transient toast confirming a persisted pin (Issue 5). The pin
    /// is effective immediately on the next aux call (the resolver re-reads the
    /// store), so this *is* the confirmation — no event to wait for.
    pub fn confirm_aux_pin(&mut self, purpose: Purpose, provider: &str, model: &str) {
        self.set_toast(format!(
            "aux-model: {purpose} → {provider}/{model} (persisted)"
        ));
        self.mark_dirty();
    }

    /// Render the current aux-model pins as a transcript status line (Issue 5):
    /// the discoverability affordance for bare `/aux-model` or `/aux-model list`,
    /// mirroring `/show`'s render-current-values pattern for generation params.
    pub fn show_aux_pins(&mut self) {
        // Clone the Arc out so the lock guard doesn't alias the `app` borrow
        // held across `record_status` (mirrors `send_aux_model`).
        let line = match self.aux_models().cloned() {
            Some(store) => {
                let s = store.lock().unwrap();
                let summarize = crate::tui::aux_command::pin_label(&s, Purpose::Summarize);
                let title = crate::tui::aux_command::pin_label(&s, Purpose::SessionTitle);
                let narrate = crate::tui::aux_command::pin_label(&s, Purpose::Narrate);
                format!(
                    "aux-models: summarize={summarize}  session_title={title}  narrate={narrate}"
                )
            }
            None => "aux-models: no store installed".to_string(),
        };
        self.sessions
            .active_view_mut()
            .record_status("aux-model", line);
        self.mark_dirty();
    }

    /// The installed aux-model store handle (Issue 5), or `None` when no
    /// config dir is available (tests / a head that didn't wire it).
    pub(crate) fn aux_models(
        &self,
    ) -> Option<&std::sync::Arc<std::sync::Mutex<crate::config::aux_models::AuxModelStore>>> {
        self.aux_models.as_ref()
    }
}
