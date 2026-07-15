//! The `/key` dialog state machine (#304): a two-stage modal that persists a
//! provider API key to the managed env file, mirroring the `/model` picker.
//!
//! Stage 1 (`PickProvider`) lists the keyed providers; stage 2 (`EnterKey`)
//! reads the key into a buffer that renders as bullets only ([`masked`]) — the
//! key is never shown, logged, or echoed. Submitting drives the shared
//! [`crate::config::env_key::set_key`] writer; the buffer is wiped on `Esc`.
//!
//! [`masked`]: KeyDialog::masked

use ratatui::widgets::ListState;

/// One keyed provider offered in the picker (keyless providers are excluded).
#[derive(Debug, Clone, PartialEq)]
pub struct KeyProvider {
    pub name: String,
    pub key_env: String,
}

/// Which of the two stages the dialog is on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyStage {
    /// Choosing which provider's key to set.
    PickProvider,
    /// Typing the key for the chosen provider.
    EnterKey,
}

/// Two-stage `/key` modal state. Owns the provider roster, the selection, and the
/// (never-rendered-in-clear) key buffer.
pub struct KeyDialog {
    visible: bool,
    stage: KeyStage,
    providers: Vec<KeyProvider>,
    state: ListState,
    buffer: String,
}

impl KeyDialog {
    pub fn new(providers: Vec<KeyProvider>) -> Self {
        let mut state = ListState::default();
        state.select((!providers.is_empty()).then_some(0));
        Self {
            visible: false,
            stage: KeyStage::PickProvider,
            providers,
            state,
            buffer: String::new(),
        }
    }

    pub fn visible(&self) -> bool {
        self.visible
    }

    pub fn stage(&self) -> KeyStage {
        self.stage
    }

    pub fn providers(&self) -> &[KeyProvider] {
        &self.providers
    }

    pub fn state(&mut self) -> &mut ListState {
        &mut self.state
    }

    /// Open the dialog fresh on the provider-picking stage.
    pub fn show(&mut self) {
        self.visible = true;
        self.stage = KeyStage::PickProvider;
        self.buffer.clear();
        self.state.select((!self.providers.is_empty()).then_some(0));
    }

    /// Close and wipe the key buffer (never leave a key lingering in memory).
    pub fn hide(&mut self) {
        self.visible = false;
        self.stage = KeyStage::PickProvider;
        self.buffer.clear();
    }

    pub fn select_next(&mut self) {
        if self.providers.is_empty() {
            return;
        }
        let current = self.state.selected().unwrap_or(0);
        self.state
            .select(Some((current + 1) % self.providers.len()));
    }

    pub fn select_prev(&mut self) {
        if self.providers.is_empty() {
            return;
        }
        let current = self.state.selected().unwrap_or(0);
        let prev = if current == 0 {
            self.providers.len() - 1
        } else {
            current - 1
        };
        self.state.select(Some(prev));
    }

    pub fn selected_provider(&self) -> Option<&KeyProvider> {
        self.state.selected().and_then(|i| self.providers.get(i))
    }

    /// Advance from the provider list to the key-entry stage, if a provider is
    /// selected. Returns whether it advanced.
    pub fn confirm_provider(&mut self) -> bool {
        if self.selected_provider().is_some() {
            self.stage = KeyStage::EnterKey;
            self.buffer.clear();
            true
        } else {
            false
        }
    }

    /// Return from key entry to the provider list, wiping the buffer (`Esc`).
    pub fn back_to_providers(&mut self) {
        self.stage = KeyStage::PickProvider;
        self.buffer.clear();
    }

    pub fn push_char(&mut self, c: char) {
        self.buffer.push(c);
    }

    pub fn pop_char(&mut self) {
        self.buffer.pop();
    }

    pub fn buffer_is_empty(&self) -> bool {
        self.buffer.is_empty()
    }

    /// The buffer rendered as bullets — the key content is never shown in clear.
    pub fn masked(&self) -> String {
        "•".repeat(self.buffer.chars().count())
    }

    /// Take the entered key value, clearing the buffer.
    pub fn take_buffer(&mut self) -> String {
        std::mem::take(&mut self.buffer)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dialog() -> KeyDialog {
        KeyDialog::new(vec![
            KeyProvider {
                name: "zai".into(),
                key_env: "ZAI_API_KEY".into(),
            },
            KeyProvider {
                name: "openai".into(),
                key_env: "OPENAI_API_KEY".into(),
            },
        ])
    }

    #[test]
    fn opens_on_provider_stage_with_first_selected() {
        let mut d = dialog();
        d.show();
        assert!(d.visible());
        assert_eq!(d.stage(), KeyStage::PickProvider);
        assert_eq!(d.selected_provider().unwrap().name, "zai");
    }

    #[test]
    fn navigation_wraps() {
        let mut d = dialog();
        d.show();
        d.select_prev();
        assert_eq!(d.selected_provider().unwrap().name, "openai");
        d.select_next();
        assert_eq!(d.selected_provider().unwrap().name, "zai");
    }

    #[test]
    fn confirm_advances_to_entry_and_masks_input() {
        let mut d = dialog();
        d.show();
        assert!(d.confirm_provider());
        assert_eq!(d.stage(), KeyStage::EnterKey);
        d.push_char('s');
        d.push_char('k');
        d.push_char('1');
        assert_eq!(d.masked(), "•••", "key renders as bullets only");
        assert!(!d.buffer_is_empty());
    }

    #[test]
    fn back_to_providers_wipes_buffer() {
        let mut d = dialog();
        d.show();
        d.confirm_provider();
        d.push_char('x');
        d.back_to_providers();
        assert_eq!(d.stage(), KeyStage::PickProvider);
        assert!(d.buffer_is_empty());
    }

    #[test]
    fn hide_wipes_buffer_and_resets_stage() {
        let mut d = dialog();
        d.show();
        d.confirm_provider();
        d.push_char('x');
        d.hide();
        assert!(!d.visible());
        assert!(d.buffer_is_empty());
        assert_eq!(d.stage(), KeyStage::PickProvider);
    }

    #[test]
    fn take_buffer_returns_and_clears() {
        let mut d = dialog();
        d.show();
        d.confirm_provider();
        for c in "secret".chars() {
            d.push_char(c);
        }
        assert_eq!(d.take_buffer(), "secret");
        assert!(d.buffer_is_empty());
    }

    #[test]
    fn empty_provider_list_selects_nothing() {
        let mut d = KeyDialog::new(vec![]);
        d.show();
        assert!(d.selected_provider().is_none());
        assert!(!d.confirm_provider(), "cannot advance without a provider");
    }
}
