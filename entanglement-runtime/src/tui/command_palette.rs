//! The `Ctrl+P` command palette: a filterable list over [`Command`], split out
//! of `commands.rs` (#376) once that file crossed the 400-line cap — a natural
//! seam, since the palette is a UI widget over the command set, not part of
//! defining/parsing commands.

use super::commands::{all_commands, filter_commands, Command};
use ratatui::widgets::ListState;

pub struct CommandPalette {
    commands: Vec<Command>,
    filtered: Vec<Command>,
    query: String,
    state: ListState,
    visible: bool,
}

impl CommandPalette {
    pub fn new() -> Self {
        let commands = all_commands();
        let filtered = commands.clone();
        let mut state = ListState::default();
        state.select(Some(0));

        Self {
            commands,
            filtered,
            query: String::new(),
            state,
            visible: false,
        }
    }

    pub fn visible(&self) -> bool {
        self.visible
    }

    pub fn show(&mut self) {
        self.visible = true;
        self.reset();
    }

    pub fn hide(&mut self) {
        self.visible = false;
        self.reset();
    }

    pub fn reset(&mut self) {
        self.query.clear();
        self.filtered = self.commands.clone();
        self.state.select(Some(0));
    }

    pub fn set_query(&mut self, query: String) {
        self.query = query;
        self.filtered = filter_commands(&self.query);
        if !self.filtered.is_empty() {
            self.state.select(Some(0));
        } else {
            self.state.select(None);
        }
    }

    pub fn query(&self) -> &str {
        &self.query
    }

    pub fn filtered_commands(&self) -> &[Command] {
        &self.filtered
    }

    pub fn state(&mut self) -> &mut ListState {
        &mut self.state
    }

    pub fn selected(&self) -> Option<&Command> {
        self.state.selected().and_then(|idx| self.filtered.get(idx))
    }

    pub fn select_next(&mut self) {
        if self.filtered.is_empty() {
            return;
        }
        let current = self.state.selected().unwrap_or(0);
        let next = (current + 1) % self.filtered.len();
        self.state.select(Some(next));
    }

    pub fn select_prev(&mut self) {
        if self.filtered.is_empty() {
            return;
        }
        let current = self.state.selected().unwrap_or(0);
        let prev = if current == 0 {
            self.filtered.len() - 1
        } else {
            current - 1
        };
        self.state.select(Some(prev));
    }

    pub fn execute_selected(&mut self) -> Option<Command> {
        self.selected().cloned().inspect(|_| {
            self.hide();
        })
    }
}

impl Default for CommandPalette {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_command_palette_navigation() {
        let mut palette = CommandPalette::new();
        palette.show();

        assert_eq!(palette.selected(), Some(&Command::Help));

        palette.select_next();
        assert_ne!(palette.selected(), Some(&Command::Help));

        palette.select_prev();
        assert_eq!(palette.selected(), Some(&Command::Help));
    }

    #[test]
    fn test_command_palette_filtering() {
        let mut palette = CommandPalette::new();
        palette.show();

        palette.set_query("hel".to_string());
        assert_eq!(palette.selected(), Some(&Command::Help));
        assert!(palette
            .filtered_commands()
            .iter()
            .any(|c| matches!(c, Command::Help)));
    }

    #[test]
    fn test_command_palette_execute() {
        let mut palette = CommandPalette::new();
        palette.show();

        let cmd = palette.execute_selected();
        assert_eq!(cmd, Some(Command::Help));
        assert!(!palette.visible());
    }
}
