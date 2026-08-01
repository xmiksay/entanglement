use entanglement_provider::GenerationParams;

// `CommandPalette` lives in a sibling module (#376, once this file crossed the
// 400-line cap) but stays reachable at its historical path for every call site.
pub use super::command_palette::CommandPalette;
// `/mcp`'s subcommand parsing + wire dispatch lives in its own sibling module
// (#373, `mcp_command.rs`, same "past the cap" reasoning as `CommandPalette`
// above) — call sites reach it at `crate::tui::mcp_command::…` directly.

#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    Help,
    New,
    Exit,
    Agent,
    Model,
    Key,
    Plan,
    Tasks,
    Inspect,
    Editor,
    Export,
    Resume,
    Compact,
    Set,
    Show,
    Mcp,
    Allow,
    Bash,
    Enable,
    Disable,
    Pause,
    Continue,
    Stop,
    Name,
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
            Command::Key => "key",
            Command::Plan => "plan",
            Command::Tasks => "tasks",
            Command::Inspect => "inspect",
            Command::Editor => "editor",
            Command::Export => "export",
            Command::Compact => "compact",
            Command::Set => "set",
            Command::Show => "show",
            Command::Mcp => "mcp",
            Command::Allow => "allow",
            Command::Bash => "bash",
            Command::Enable => "enable",
            Command::Disable => "disable",
            Command::Pause => "pause",
            Command::Continue => "continue",
            Command::Stop => "stop",
            Command::Name => "name",
        }
    }

    pub fn description(&self) -> &str {
        match self {
            Command::Help => "Show help and keybindings",
            Command::New => "Create a new session",
            Command::Exit => "Quit the application",
            Command::Agent => "Pick agent profile",
            Command::Model => "Pick model",
            Command::Key => "Set a provider API key",
            Command::Plan => "Open the bound plan file in $EDITOR",
            Command::Tasks => "Show the task list in the sidebar",
            Command::Inspect => "Inspect prompt, agents & skills",
            Command::Editor => "Open editor",
            Command::Export => "Export conversation",
            Command::Resume => "Continue a past session",
            Command::Compact => "Compact the conversation history (LLM summary, --keep N to preserve trailing messages)",
            Command::Set => {
                "Set a generation parameter (temperature, effort, thinking_budget, max_tokens)"
            }
            Command::Show => "Show the current effective generation parameters",
            Command::Mcp => "Manage MCP servers (list, add, remove)",
            Command::Allow => {
                "Allow a directory for read/grep/glob for the rest of this session"
            }
            Command::Bash => {
                "Live-enable/disable bash (on [--allow [<pattern>]|--ask] | off)"
            }
            Command::Enable => {
                "Enable tools for this session past the agent mask (bare = checklist dialog; mcp <server> | tool <name> [--allow])"
            }
            Command::Disable => {
                "Disable tools for this session (mcp <server> | tool <name>; bare = reset to profile defaults)"
            }
            Command::Pause => {
                "Pause the current session (--all for every live session)"
            }
            Command::Continue => {
                "Resume a paused session (--all for every live session)"
            }
            Command::Stop => {
                "Stop/cancel the current session's in-flight turn (--all for every live session)"
            }
            Command::Name => "Set a display name for the current session",
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
        Command::Key,
        Command::Plan,
        Command::Tasks,
        Command::Inspect,
        Command::Editor,
        Command::Export,
        Command::Compact,
        Command::Set,
        Command::Show,
        Command::Mcp,
        Command::Allow,
        Command::Bash,
        Command::Enable,
        Command::Disable,
        Command::Pause,
        Command::Continue,
        Command::Stop,
        Command::Name,
    ]
}

/// Parse `/set <key> <value>` into a partial [`GenerationParams`] override — only
/// the named field is `Some`, matching [`GenerationParams::apply_overrides`]'s
/// merge semantics. `text` is the raw input including the leading `/set` (the
/// `/compact` raw-text re-parse pattern, since [`parse_command`] only matches the
/// command name and drops everything after it). Recognised keys: `temperature`
/// (f32), `effort` (`low|medium|high`), `thinking_budget`/`thinking_budget_tokens`
/// (u32), `max_tokens`/`max_output_tokens` (u32). An unknown key or a value that
/// fails to parse for its key is a friendly `Err` message, not a panic.
///
/// Issue 2: the tokenization + key/value grammar is now clap-derived
/// ([`crate::tui::command_args::parse_set_via_clap`]); this thin wrapper keeps
/// the historical call-site path so `event_loop::send_set` is untouched.
pub fn parse_set_args(text: &str) -> Result<GenerationParams, String> {
    crate::tui::command_args::parse_set_via_clap(text)
}

/// Parse `/compact`'s trailing text into an optional keep-tail count plus the
/// remaining free-text instructions (#397). `text` is the raw input including
/// the leading `/compact` (the same raw-text re-parse pattern as
/// [`parse_set_args`], since [`parse_command`] drops everything after the
/// command name). A leading `--keep N` token is consumed and parsed as `u64`;
/// anything else is passed through unchanged as instructions. No `--keep` →
/// `kept: 0` (today's default: summarize the whole history).
///
/// Issue 2: the `--keep N` + free-text grammar is now clap-derived
/// ([`crate::tui::command_args::parse_compact_via_clap`]); this thin wrapper
/// keeps the historical call-site path so `event_loop::send_compact` is
/// untouched.
pub fn parse_compact_args(text: &str) -> Result<(u64, Option<String>), String> {
    crate::tui::command_args::parse_compact_via_clap(text)
}

/// Parse the `--all` flag shared by `/stop`, `/pause`, `/continue` (#6).
/// `text` is the raw input including the leading slash command. Returns
/// whether the flag was present; an unknown token is a friendly `Err`.
///
/// Issue 2: the `--all`/`-a` grammar is now clap-derived
/// ([`crate::tui::command_args::parse_lifecycle_via_clap`]); this thin wrapper
/// keeps the historical call-site path so the three `event_loop::send_*`
/// helpers are untouched.
pub fn parse_all_flag(text: &str, cmd: Command) -> Result<bool, String> {
    crate::tui::command_args::parse_lifecycle_via_clap(text, cmd)
}

/// Parse `/name <text>`'s trailing free text (the raw-text re-parse pattern,
/// like [`parse_compact_args`] — [`parse_command`] drops everything after the
/// command name). `None` when no text follows (the caller renders usage).
pub fn parse_name_args(text: &str) -> Option<String> {
    let rest = text.trim().strip_prefix("/name").unwrap_or("").trim();
    (!rest.is_empty()).then(|| rest.to_string())
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

#[cfg(test)]
mod tests {
    use super::*;
    use entanglement_provider::ReasoningEffort;

    #[test]
    fn test_parse_command_valid() {
        assert_eq!(parse_command("/help"), Some(Command::Help));
        assert_eq!(parse_command("/new"), Some(Command::New));
        assert_eq!(parse_command("/exit"), Some(Command::Exit));
        assert_eq!(parse_command("/agent"), Some(Command::Agent));
        assert_eq!(parse_command("/model"), Some(Command::Model));
        assert_eq!(parse_command("/key"), Some(Command::Key));
        assert_eq!(parse_command("/plan"), Some(Command::Plan));
        assert_eq!(parse_command("/tasks"), Some(Command::Tasks));
        assert_eq!(parse_command("/inspect"), Some(Command::Inspect));
        assert_eq!(parse_command("/editor"), Some(Command::Editor));
        assert_eq!(parse_command("/export"), Some(Command::Export));
        assert_eq!(parse_command("/compact"), Some(Command::Compact));
        assert_eq!(parse_command("/set"), Some(Command::Set));
        assert_eq!(parse_command("/show"), Some(Command::Show));
        assert_eq!(parse_command("/mcp"), Some(Command::Mcp));
        assert_eq!(parse_command("/allow"), Some(Command::Allow));
        assert_eq!(parse_command("/bash"), Some(Command::Bash));
    }

    #[test]
    fn test_parse_command_compact_with_trailing_instructions() {
        // The command name parses the same whether or not free text follows;
        // the trailing text is extracted separately (`event_loop::send_compact`),
        // not by `parse_command`.
        assert_eq!(
            parse_command("/compact keep the auth flow details"),
            Some(Command::Compact)
        );
    }

    #[test]
    fn test_parse_compact_args_bare() {
        assert_eq!(parse_compact_args("/compact"), Ok((0, None)));
    }

    #[test]
    fn test_parse_compact_args_instructions_only() {
        assert_eq!(
            parse_compact_args("/compact keep the auth flow details"),
            Ok((0, Some("keep the auth flow details".to_string())))
        );
    }

    #[test]
    fn test_parse_compact_args_keep_only() {
        assert_eq!(parse_compact_args("/compact --keep 3"), Ok((3, None)));
    }

    #[test]
    fn test_parse_compact_args_keep_and_instructions() {
        assert_eq!(
            parse_compact_args("/compact --keep 3 keep the auth flow details"),
            Ok((3, Some("keep the auth flow details".to_string())))
        );
    }

    #[test]
    fn test_parse_compact_args_keep_missing_value() {
        assert!(parse_compact_args("/compact --keep").is_err());
    }

    #[test]
    fn test_parse_compact_args_keep_invalid_value() {
        assert!(parse_compact_args("/compact --keep abc").is_err());
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
    fn test_command_slash_names() {
        assert_eq!(Command::Help.slash_name(), "/help");
        assert_eq!(Command::New.slash_name(), "/new");
        assert_eq!(Command::Exit.slash_name(), "/exit");
    }

    #[test]
    fn parse_set_args_valid_pairs() {
        assert_eq!(
            parse_set_args("/set temperature 0.7"),
            Ok(GenerationParams {
                temperature: Some(0.7),
                ..GenerationParams::default()
            })
        );
        assert_eq!(
            parse_set_args("/set effort high"),
            Ok(GenerationParams {
                reasoning_effort: Some(ReasoningEffort::High),
                ..GenerationParams::default()
            })
        );
        assert_eq!(
            parse_set_args("/set thinking_budget 4096"),
            Ok(GenerationParams {
                thinking_budget_tokens: Some(4096),
                ..GenerationParams::default()
            })
        );
        assert_eq!(
            parse_set_args("/set thinking_budget_tokens 2048"),
            Ok(GenerationParams {
                thinking_budget_tokens: Some(2048),
                ..GenerationParams::default()
            })
        );
        assert_eq!(
            parse_set_args("/set max_tokens 8192"),
            Ok(GenerationParams {
                max_output_tokens: Some(8192),
                ..GenerationParams::default()
            })
        );
        assert_eq!(
            parse_set_args("/set max_output_tokens 1024"),
            Ok(GenerationParams {
                max_output_tokens: Some(1024),
                ..GenerationParams::default()
            })
        );
        // Effort is case-insensitive.
        assert_eq!(
            parse_set_args("/set effort MEDIUM"),
            Ok(GenerationParams {
                reasoning_effort: Some(ReasoningEffort::Medium),
                ..GenerationParams::default()
            })
        );
    }

    #[test]
    fn parse_set_args_unknown_key() {
        assert!(parse_set_args("/set bogus 1")
            .unwrap_err()
            .contains("unknown"));
    }

    #[test]
    fn parse_set_args_malformed_value() {
        assert!(parse_set_args("/set temperature nope").is_err());
        assert!(parse_set_args("/set effort extreme").is_err());
        assert!(parse_set_args("/set max_tokens nope").is_err());
    }

    #[test]
    fn parse_set_args_missing_args() {
        assert!(parse_set_args("/set").is_err());
        assert!(parse_set_args("/set temperature").is_err());
    }

    #[test]
    fn new_lifecycle_commands_parse() {
        assert_eq!(parse_command("/pause"), Some(Command::Pause));
        assert_eq!(parse_command("/continue"), Some(Command::Continue));
        assert_eq!(parse_command("/stop"), Some(Command::Stop));
    }

    #[test]
    fn new_commands_appear_in_all_commands_and_palette_filter() {
        assert!(all_commands().iter().any(|c| matches!(c, Command::Pause)));
        assert!(all_commands()
            .iter()
            .any(|c| matches!(c, Command::Continue)));
        assert!(all_commands().iter().any(|c| matches!(c, Command::Stop)));
        // `filter_commands` is what the palette uses.
        assert!(filter_commands("pause")
            .iter()
            .any(|c| matches!(c, Command::Pause)));
        assert!(filter_commands("stop")
            .iter()
            .any(|c| matches!(c, Command::Stop)));
    }

    #[test]
    fn parse_name_command_and_args() {
        assert_eq!(parse_command("/name my session"), Some(Command::Name));
        assert_eq!(
            parse_name_args("/name my session"),
            Some("my session".to_string())
        );
        assert_eq!(parse_name_args("/name   "), None, "bare /name → usage");
        assert_eq!(parse_name_args("/name"), None);
    }

    #[test]
    fn parse_all_flag_bare_and_flagged() {
        assert_eq!(parse_all_flag("/stop", Command::Stop), Ok(false));
        assert_eq!(parse_all_flag("/stop --all", Command::Stop), Ok(true));
        assert_eq!(parse_all_flag("/pause -a", Command::Pause), Ok(true));
        assert_eq!(
            parse_all_flag("/continue --all", Command::Continue),
            Ok(true)
        );
    }

    #[test]
    fn parse_all_flag_rejects_unknown_token() {
        assert!(parse_all_flag("/stop frobnicate", Command::Stop).is_err());
    }
}
