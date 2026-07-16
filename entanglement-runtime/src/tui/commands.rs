use entanglement_provider::{GenerationParams, ReasoningEffort};

// `CommandPalette` lives in a sibling module (#376, once this file crossed the
// 400-line cap) but stays reachable at its historical path for every call site.
pub use super::command_palette::CommandPalette;

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
            Command::Plan => "Show the plan outline in the sidebar",
            Command::Tasks => "Show the task list in the sidebar",
            Command::Inspect => "Inspect prompt, agents & skills",
            Command::Editor => "Open editor",
            Command::Export => "Export conversation",
            Command::Resume => "Continue a past session",
            Command::Compact => "Compact the conversation history (LLM summary)",
            Command::Set => {
                "Set a generation parameter (temperature, effort, thinking_budget, max_tokens)"
            }
            Command::Show => "Show the current effective generation parameters",
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
pub fn parse_set_args(text: &str) -> Result<GenerationParams, String> {
    let rest = text
        .trim()
        .strip_prefix(&Command::Set.slash_name())
        .map(str::trim)
        .unwrap_or("");
    let mut parts = rest.splitn(2, char::is_whitespace);
    let key = parts.next().unwrap_or("").trim();
    let value = parts.next().unwrap_or("").trim();
    if key.is_empty() || value.is_empty() {
        return Err(
            "usage: /set <key> <value> — keys: temperature, effort, thinking_budget, max_tokens"
                .to_string(),
        );
    }

    let mut overrides = GenerationParams::default();
    match key {
        "temperature" => {
            overrides.temperature = Some(
                value
                    .parse::<f32>()
                    .map_err(|_| format!("invalid temperature value: {value}"))?,
            );
        }
        "effort" => {
            overrides.reasoning_effort = Some(match value.to_lowercase().as_str() {
                "low" => ReasoningEffort::Low,
                "medium" => ReasoningEffort::Medium,
                "high" => ReasoningEffort::High,
                _ => {
                    return Err(format!(
                        "invalid effort value: {value} (expected low|medium|high)"
                    ))
                }
            });
        }
        "thinking_budget" | "thinking_budget_tokens" => {
            overrides.thinking_budget_tokens = Some(
                value
                    .parse::<u32>()
                    .map_err(|_| format!("invalid thinking_budget value: {value}"))?,
            );
        }
        "max_tokens" | "max_output_tokens" => {
            overrides.max_output_tokens = Some(
                value
                    .parse::<u32>()
                    .map_err(|_| format!("invalid max_tokens value: {value}"))?,
            );
        }
        other => return Err(format!("unknown /set key: {other}")),
    }
    Ok(overrides)
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
}
