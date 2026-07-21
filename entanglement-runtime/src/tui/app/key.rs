//! `App` surface for the `/key` dialog (#304): open/close, navigation, and the
//! submit path that drives the shared [`crate::config::env_key::set_key`] writer,
//! primes the process env so the live model resolver picks the key up on the next
//! `/model` switch, and records a status line (never the key) into the transcript.

use ratatui::widgets::ListState;

use crate::tui::key_dialog::{KeyDialog, KeyStage};

use super::App;

/// Outcome of submitting the key dialog, so the event loop can react (e.g. keep
/// the dialog open on failure). The message is already recorded into the
/// transcript; this only signals success/failure.
pub enum KeySubmit {
    /// The key was written and the process env primed; the dialog closed.
    Saved,
    /// The write failed; the dialog stays on the entry stage for a retry.
    Failed,
    /// Nothing to submit (empty buffer / no provider selected).
    Noop,
}

impl App {
    pub fn showing_key_dialog(&self) -> bool {
        self.key_dialog.visible()
    }

    pub fn key_dialog(&self) -> &KeyDialog {
        &self.key_dialog
    }

    pub fn key_dialog_state(&mut self) -> &mut ListState {
        self.key_dialog.state()
    }

    pub fn key_dialog_stage(&self) -> KeyStage {
        self.key_dialog.stage()
    }

    pub fn open_key_dialog(&mut self) {
        self.key_dialog.show();
        self.mark_dirty();
    }

    pub fn close_key_dialog(&mut self) {
        self.key_dialog.hide();
        self.mark_dirty();
    }

    pub fn key_dialog_next(&mut self) {
        self.key_dialog.select_next();
        self.mark_dirty();
    }

    pub fn key_dialog_prev(&mut self) {
        self.key_dialog.select_prev();
        self.mark_dirty();
    }

    /// Advance from the provider list to the key-entry stage.
    pub fn key_dialog_confirm_provider(&mut self) {
        self.key_dialog.confirm_provider();
        self.mark_dirty();
    }

    /// `Esc` on the entry stage: back to the provider list, wiping the buffer.
    pub fn key_dialog_back(&mut self) {
        self.key_dialog.back_to_providers();
        self.mark_dirty();
    }

    pub fn key_dialog_push_char(&mut self, c: char) {
        self.key_dialog.push_char(c);
        self.mark_dirty();
    }

    pub fn key_dialog_pop_char(&mut self) {
        self.key_dialog.pop_char();
        self.mark_dirty();
    }

    /// Persist the typed key for the selected provider (#304): write it via the
    /// shared env-file writer, prime the process env (env > file) so the live
    /// model resolver binds it on the next `/model` switch — no restart — and
    /// record a status line (never the key) into the transcript.
    pub fn submit_key_dialog(&mut self) -> KeySubmit {
        let Some(provider) = self.key_dialog.selected_provider().cloned() else {
            return KeySubmit::Noop;
        };
        if self.key_dialog.buffer_is_empty() {
            return KeySubmit::Noop;
        }
        let value = self.key_dialog.take_buffer();

        match crate::config::env_key::set_key(&provider.key_env, &value) {
            Ok(path) => {
                // Prime the process env so the live resolver (#218) sees the new
                // key without a restart. `set_var` is the same channel `load()`
                // fills at startup.
                std::env::set_var(&provider.key_env, &value);
                self.record_status(
                    "/key",
                    format!("Saved {} to {}", provider.key_env, path.display()),
                );
                self.key_dialog.hide();
                self.mark_dirty();
                KeySubmit::Saved
            }
            Err(e) => {
                self.record_status(
                    "/key",
                    format!("Failed to save {}: {e:#}", provider.key_env),
                );
                self.mark_dirty();
                KeySubmit::Failed
            }
        }
    }

    /// Records a head-side status line into the active session's transcript.
    pub(crate) fn record_status(&mut self, label: &str, message: String) {
        self.sessions
            .active_view_mut()
            .record_status(label, message);
    }
}
