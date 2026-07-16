use crate::tui::commands::Command;
use crate::tui::keybindings::Action;

use super::{App, UiEffect};

impl App {
    pub fn execute_command(&mut self, command: Command) -> bool {
        match command {
            Command::Help => {
                self.toggle_help();
                false
            }
            Command::New => {
                self.create_session();
                false
            }
            Command::Exit => true,
            Command::Agent => {
                self.toggle_profile_picker();
                false
            }
            Command::Model => {
                self.toggle_model_picker();
                false
            }
            Command::Key => {
                self.open_key_dialog();
                false
            }
            Command::Plan => {
                self.show_sidebar();
                false
            }
            Command::Tasks => {
                self.show_sidebar();
                false
            }
            Command::Inspect => {
                self.toggle_inspect();
                false
            }
            Command::Editor => {
                self.request_effect(UiEffect::OpenEditor);
                false
            }
            Command::Export => {
                self.request_effect(UiEffect::Export);
                false
            }
            Command::Resume => {
                self.toggle_resume_modal();
                false
            }
            // Needs `holly` + (for the typed form) the trailing input text,
            // neither of which this sync dispatch has — both call sites
            // (`event_loop`'s Enter handler and the command palette) intercept
            // `Compact` before it reaches here (#324).
            Command::Compact => false,
            // Same shape as `Compact` (#376): `Set` needs the trailing `key
            // value` text and `Show` needs `holly` to query the live session,
            // neither available here — both call sites intercept them before
            // reaching this dispatch.
            Command::Set | Command::Show => false,
        }
    }

    pub fn dispatch_action(&mut self, action: Action) -> bool {
        match action {
            Action::Quit => true,
            Action::NewSession => {
                self.create_session();
                false
            }
            Action::ListSessions => {
                self.toggle_sessions_modal();
                false
            }
            Action::PickAgent => {
                self.toggle_profile_picker();
                false
            }
            Action::CycleAgent => {
                self.cycle_primary_profile();
                false
            }
            Action::PickModel => {
                self.toggle_model_picker();
                false
            }
            Action::ToggleSidebar => {
                self.toggle_sidebar();
                false
            }
            Action::OpenEditor => {
                self.request_effect(UiEffect::OpenEditor);
                false
            }
            Action::Export => {
                self.request_effect(UiEffect::Export);
                false
            }
            Action::Interrupt => false,
            Action::ScrollUp => {
                self.scroll_up(5);
                false
            }
            Action::ScrollDown => {
                self.scroll_down(5);
                false
            }
            Action::ShowHelp => {
                self.toggle_help();
                false
            }
            Action::CommandPalette => {
                self.toggle_command_palette();
                false
            }
            Action::ToggleReasoning => {
                self.toggle_last_block();
                false
            }
            Action::Inspect => {
                self.toggle_inspect();
                false
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use entanglement_core::SessionId;

    #[test]
    fn tasks_command_shows_the_sidebar() {
        let mut app = App::new_for_test(SessionId::new("s1"));
        assert!(app.showing_sidebar(), "sidebar starts visible");

        let quit = app.execute_command(Command::Tasks);
        assert!(!quit, "/tasks does not quit");
        assert!(app.showing_sidebar(), "/tasks reveals the sidebar");
    }

    #[test]
    fn plan_command_shows_the_sidebar() {
        let mut app = App::new_for_test(SessionId::new("s1"));
        app.execute_command(Command::Plan);
        assert!(app.showing_sidebar(), "/plan reveals the sidebar");
    }
}
