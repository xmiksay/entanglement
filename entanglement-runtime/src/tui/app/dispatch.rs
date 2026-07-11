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
            Command::Plan => false,
            Command::Tasks => false,
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
                self.toggle_last_reasoning_block();
                false
            }
        }
    }
}
