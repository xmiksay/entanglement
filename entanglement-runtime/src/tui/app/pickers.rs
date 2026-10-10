use entanglement_core::SessionId;
use ratatui::widgets::ListState;

use super::{AgentInfo, App};

impl App {
    pub fn showing_profile_picker(&self) -> bool {
        self.showing_profile_picker
    }

    pub fn profile_picker_state(&mut self) -> &mut ListState {
        &mut self.profile_picker_state
    }

    pub fn available_profiles(&self) -> &[AgentInfo] {
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

    pub fn showing_mode_picker(&self) -> bool {
        self.showing_mode_picker
    }

    pub fn mode_picker_state(&mut self) -> &mut ListState {
        &mut self.mode_picker_state
    }

    pub fn available_modes(&self) -> &[AgentInfo] {
        &self.available_modes
    }

    /// Open the picker with the session's current mode highlighted, or
    /// close it — `/mode`'s own toggle (#560 P12, ADR-0207 §12).
    pub fn toggle_mode_picker(&mut self) {
        self.showing_mode_picker = !self.showing_mode_picker;
        if self.showing_mode_picker {
            let mode = self.mode().to_string();
            let current_index = self
                .available_modes
                .iter()
                .position(|m| m.name == mode)
                .unwrap_or(0);
            self.mode_picker_state.select(Some(current_index));
        }
        self.mark_dirty();
    }

    pub fn close_mode_picker(&mut self) {
        self.showing_mode_picker = false;
        self.mark_dirty();
    }

    pub fn mode_picker_next(&mut self) {
        if let Some(selected) = self.mode_picker_state.selected() {
            let next = (selected + 1) % self.available_modes.len();
            self.mode_picker_state.select(Some(next));
            self.mark_dirty();
        }
    }

    pub fn mode_picker_prev(&mut self) {
        if let Some(selected) = self.mode_picker_state.selected() {
            let prev = if selected == 0 {
                self.available_modes.len() - 1
            } else {
                selected - 1
            };
            self.mode_picker_state.select(Some(prev));
            self.mark_dirty();
        }
    }

    pub fn mode_picker_page_down(&mut self, n: usize) {
        if self.available_modes.is_empty() {
            return;
        }
        if let Some(selected) = self.mode_picker_state.selected() {
            let last = self.available_modes.len() - 1;
            self.mode_picker_state
                .select(Some((selected + n).min(last)));
            self.mark_dirty();
        }
    }

    pub fn mode_picker_page_up(&mut self, n: usize) {
        if self.available_modes.is_empty() {
            return;
        }
        if let Some(selected) = self.mode_picker_state.selected() {
            self.mode_picker_state
                .select(Some(selected.saturating_sub(n)));
            self.mark_dirty();
        }
    }

    /// Resolve the highlighted row to its mode name and close the picker.
    /// `None` when nothing is selected (an empty roster, which never
    /// happens for the fixed four-mode list but mirrors every other
    /// picker's defensive shape).
    pub fn select_mode_picker(&mut self) -> Option<String> {
        let name = self
            .mode_picker_state
            .selected()
            .and_then(|i| self.available_modes.get(i))
            .map(|m| m.name.clone())?;
        self.showing_mode_picker = false;
        self.mark_dirty();
        Some(name)
    }

    /// The mode `Tab`/`Shift+Tab` should switch to, or `None` when the roster
    /// has nothing to cycle to.
    ///
    /// `Tab` used to cycle *agents*. ADR-0207 fixed an agent for a session's
    /// life, so that binding lost its subject — but the muscle memory is worth
    /// keeping, and mode is now the axis a user can actually change mid-session.
    /// Pure: the caller sends the `SetMode`, so this stays testable without a
    /// live engine.
    pub fn cycle_mode(&self, forward: bool) -> Option<String> {
        let modes = &self.available_modes;
        if modes.len() < 2 {
            return None;
        }
        let here = modes
            .iter()
            .position(|m| m.name == self.mode())
            .unwrap_or(0);
        let next = if forward {
            (here + 1) % modes.len()
        } else {
            (here + modes.len() - 1) % modes.len()
        };
        Some(modes[next].name.clone())
    }
}
