use ratatui::widgets::ListState;

#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    Help,
    New,
    Exit,
    Agent,
    Model,
    Plan,
    Tasks,
    Editor,
    Export,
    Resume,
}

impl Command {
    pub fn name(&self) -> &str {
        match self {
            Command::Help => "help",
            Command::New => "new",
            Command::Resume => "resume",
            Command::Exit => "exit",
            Command::Agent => "agent",
            Command::Model => "model",
            Command::Plan => "plan",
            Command::Tasks => "tasks",
            Command::Editor => "editor",
            Command::Export => "export",
        }
    }

    pub fn description(&self) -> &str {
        match self {
            Command::Help => "Show help and keybindings",
            Command::New => "Create a new session",
            Command::Exit => "Quit the application",
            Command::Agent => "Pick agent profile",
            Command::Model => "Pick model",
            Command::Plan => "Jump to plan panel",
            Command::Tasks => "Jump to task panel",
            Command::Editor => "Open editor",
            Command::Export => "Export conversation",
            Command::Resume => "Continue a past session",
        }
    }

    pub fn slash_name(&self) -> String {
        format!("/{}", self.name())
    }
}

pub fn all_commands() -> Vec<Command> {
    vec![
        Command::Help,
        Command::New,
        Command::Resume,
        Command::Exit,
        Command::Agent,
        Command::Model,
        Command::Plan,
        Command::Tasks,
        Command::Editor,
        Command::Export,
    ]
}

pub fn parse_command(input: &str) -> Option<Command> {
    let trimmed = input.trim();
    if !trimmed.starts_with('/') {
        return None;
    }

    let command_part = trimmed[1..].split_whitespace().next()?;
    all_commands()
        .into_iter()
        .find(|cmd| cmd.name() == command_part)
}

pub fn filter_commands(query: &str) -> Vec<Command> {
    let query = query.to_lowercase();
    all_commands()
        .into_iter()
        .filter(|cmd| {
            let name = cmd.name().to_lowercase();
            let description = cmd.description().to_lowercase();
            name.contains(&query) || description.contains(&query)
        })
        .collect()
}

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
    fn test_parse_command_valid() {
        assert_eq!(parse_command("/help"), Some(Command::Help));
        assert_eq!(parse_command("/new"), Some(Command::New));
        assert_eq!(parse_command("/exit"), Some(Command::Exit));
        assert_eq!(parse_command("/agent"), Some(Command::Agent));
        assert_eq!(parse_command("/model"), Some(Command::Model));
        assert_eq!(parse_command("/plan"), Some(Command::Plan));
        assert_eq!(parse_command("/tasks"), Some(Command::Tasks));
        assert_eq!(parse_command("/editor"), Some(Command::Editor));
        assert_eq!(parse_command("/export"), Some(Command::Export));
    }

    #[test]
    fn test_parse_command_with_args() {
        assert_eq!(parse_command("/help something"), Some(Command::Help));
        assert_eq!(parse_command("/new session"), Some(Command::New));
    }

    #[test]
    fn test_parse_command_invalid() {
        assert_eq!(parse_command("help"), None);
        assert_eq!(parse_command("/invalid"), None);
        assert_eq!(parse_command(""), None);
    }

    #[test]
    fn test_filter_commands_empty() {
        let filtered = filter_commands("");
        assert_eq!(filtered.len(), all_commands().len());
    }

    #[test]
    fn test_filter_commands_by_name() {
        let filtered = filter_commands("hel");
        assert!(filtered.iter().any(|c| matches!(c, Command::Help)));
        assert!(!filtered.iter().any(|c| matches!(c, Command::New)));
    }

    #[test]
    fn test_filter_commands_by_description() {
        let filtered = filter_commands("session");
        assert!(filtered.iter().any(|c| matches!(c, Command::New)));
    }

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

    #[test]
    fn test_command_slash_names() {
        assert_eq!(Command::Help.slash_name(), "/help");
        assert_eq!(Command::New.slash_name(), "/new");
        assert_eq!(Command::Exit.slash_name(), "/exit");
    }
}
