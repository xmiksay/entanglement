//! Clap-derived help + arg parsing for every slash command that takes args
//! (Issue 2). Each `…Args` struct is a pure **parser + help generator** — never
//! wired into the binary's own clap CLI in `main.rs` (that CLI parses `skutter`'s
//! subcommands, not TUI slash commands). The TUI's raw `/cmd …` text is split on
//! whitespace, the leading `/cmd` name dropped, and the rest fed to
//! [`clap::Parser::try_parse_from`]; a [`clap::Error`] maps to the friendly
//! `Err(String)` status-line path every per-command parser already used.
//! `Command::help_text()` renders the clap struct's own help for arg-bearing
//! commands (so `-h`/`--help` and `/help <cmd>` agree), else the static
//! description the palette shows. **Core hygiene:** `clap` stays in
//! `entanglement-runtime` (`tui/`), never `entanglement-core` (`make tree`).

use clap::{CommandFactory, Parser};

use super::commands::Command;

// --- per-command clap structs ------------------------------------------------

/// `/compact [--keep N] [instructions…]` (#397, ADR-0102). `--keep` preserves the
/// trailing `N` messages verbatim before summarizing the rest; `instructions`
/// is free text passed through to the summarizer. A bare `/compact` summarizes
/// the whole history (`kept = 0`, no instructions).
#[derive(Parser, Debug)]
#[command(name = "/compact", disable_help_flag = false)]
pub struct CompactArgs {
    /// Number of trailing messages to preserve verbatim (default 0).
    #[arg(long)]
    pub keep: Option<u64>,
    /// Free-text instructions for the summarizer.
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub instructions: Vec<String>,
}

/// The recognised `/set` keys (Issue 2): each maps onto one field of
/// [`entanglement_provider::GenerationParams`]. Clap's `ValueEnum` kebab-cases
/// variant names by default; explicit `name`s + aliases accept every spelling
/// the pre-Issue-2 parser did: `thinking_budget` == `thinking_budget_tokens`,
/// `max_tokens` == `max_output_tokens` (both the underscore form, which clap's
/// kebab-casing would otherwise reject).
#[derive(clap::ValueEnum, Clone, Debug, PartialEq)]
pub enum SetKey {
    Temperature,
    Effort,
    #[value(name = "thinking_budget", alias = "thinking_budget_tokens")]
    ThinkingBudget,
    #[value(name = "max_tokens", alias = "max_output_tokens")]
    MaxTokens,
}

/// `/set <key> <value>` (#376): one partial generation-override. The `value` is
/// kept as a raw string here — the conversion to the field's typed value (f32 /
/// u32 / `ReasoningEffort`) happens in [`super::commands::parse_set_args`], which
/// preserves the original friendly per-key error messages.
#[derive(Parser, Debug)]
#[command(name = "/set")]
pub struct SetArgs {
    pub key: SetKey,
    pub value: String,
}

/// `/mcp add`'s streamable-HTTP header (`--header KEY:VALUE`, repeatable).
#[derive(Parser, Debug)]
#[command(name = "header")]
pub struct HeaderArgs {
    #[arg(long)]
    pub header: Vec<String>,
}

/// `/mcp add <name> -- <command> [args…]` (stdio) **or**
/// `/mcp add <name> --url <url> [--header KEY:VALUE]…` (streamable HTTP).
/// One of `--`/`--url` must be present; clap's `required_unless_present` keeps
/// the two forms mutually exclusive without a custom validator.
#[derive(Parser, Debug, PartialEq)]
#[command(name = "/mcp add")]
pub struct McpAddArgs {
    /// Server name (the `mcp__<name>__*` tool-namespace prefix).
    pub name: String,
    /// `--url <url>` selects the streamable-HTTP transport.
    #[arg(long)]
    pub url: Option<String>,
    /// Remaining tokens after `--` are the stdio command + its args.
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub command: Vec<String>,
}

/// `/mcp` subcommands (#373). A bare `/mcp` defaults to `list`, handled by the
/// caller before reaching clap (clap requires a subcommand).
#[derive(clap::Subcommand, Debug, PartialEq)]
pub enum McpSub {
    /// List configured MCP servers (the default).
    List,
    /// Add a server (stdio via `--` or HTTP via `--url`).
    Add(McpAddArgs),
    /// Remove a server by name.
    Remove { name: String },
}

#[derive(Parser, Debug)]
#[command(name = "/mcp")]
pub struct McpArgs {
    #[command(subcommand)]
    pub cmd: Option<McpSub>,
}

/// `/allow <path>` (#486, ADR-0126): a single path, normalized root-relative by
/// the caller.
#[derive(Parser, Debug)]
#[command(name = "/allow")]
pub struct AllowArgs {
    pub path: String,
}

/// `/bash on [--allow [<pattern>] | --ask] | /bash off` (#498, ADR-0133).
#[derive(Parser, Debug)]
#[command(name = "/bash")]
pub struct BashArgs {
    /// `on` (default) or `off`.
    pub state: BashState,
    /// `--ask` grades every call through the normal approval prompt.
    #[arg(long)]
    pub ask: bool,
    /// `--allow [<pattern>]` blanket-allows, or scopes to an argument glob.
    /// `Some(None)` = blanket; `Some(Some(pat))` = scoped; the trailing free-form
    /// pattern rejoins verbatim with single spaces.
    #[arg(long)]
    pub allow: Option<Option<String>>,
}

#[derive(clap::ValueEnum, Clone, Debug, PartialEq)]
pub enum BashState {
    On,
    Off,
}

/// `/enable`/`/disable` subcommand target (#539, ADR-0149): an MCP server
/// (`mcp <server>` ⇒ `mcp__<server>__*`) or a bare tool name / glob
/// (`tool <name>`).
#[derive(clap::Subcommand, Clone, Debug, PartialEq)]
pub enum EnableTarget {
    /// Enable/disable every tool from an MCP server.
    Mcp { server: String },
    /// Enable/disable a single tool name or glob pattern.
    Tool { name: String },
}

/// `/enable mcp <server> | tool <name> [--allow]` (bare `/enable` opens the
/// session-tools dialog; `--allow` is enable-only). `--allow` is `global` so it
/// parses after the `mcp`/`tool` subcommand (`/enable tool bash --allow`).
#[derive(Parser, Debug)]
#[command(name = "/enable")]
pub struct EnableArgs {
    #[command(subcommand)]
    pub target: Option<EnableTarget>,
    /// Only meaningful for `/enable` (ignored by `/disable`): grade the entry as
    /// `allow` instead of the default `ask`.
    #[arg(long, global = true)]
    pub allow: bool,
}

/// `/disable mcp <server> | tool <name>` (bare `/disable` clears the overlay).
#[derive(Parser, Debug)]
#[command(name = "/disable")]
pub struct DisableArgs {
    #[command(subcommand)]
    pub target: Option<EnableTarget>,
}

/// `/stop`, `/pause`, `/continue` shared `--all`/`-a` flag (#6). `Continue` is
/// the resume-a-paused-session command (named `Continue`, not `Resume`).
#[derive(Parser, Debug)]
#[command(name = "/lifecycle")]
pub struct LifecycleArgs {
    /// Fan out to every live session (default: active session only).
    #[arg(long, short)]
    pub all: bool,
}

// --- shared tokenization -----------------------------------------------------

/// Split `text` (a raw `/cmd …` line) into the argv slice clap expects: the
/// command name first (so clap's `Command::get_name()` matches), then every
/// whitespace-separated token that trailed it. Returns `None` when `text` isn't
/// a `/cmd` line at all (the caller's precondition).
pub(crate) fn tokens_after_slash(text: &str, cmd: &Command) -> Option<Vec<String>> {
    let rest = text
        .trim()
        .strip_prefix(&cmd.slash_name())
        .map(str::trim)
        .unwrap_or("");
    let mut argv = vec![cmd.name().to_string()];
    argv.extend(rest.split_whitespace().map(str::to_string));
    Some(argv)
}

/// Run [`Parser::try_parse_from`] and on failure return clap's short "usage"
/// rendering (the same text `-h` prints, minus the trailing blank line) as the
/// friendly `Err(String)` the status-line path expects.
fn parse_or_usage<T: Parser>(argv: Vec<String>) -> Result<T, String> {
    match T::try_parse_from(&argv) {
        Ok(parsed) => Ok(parsed),
        Err(err) => {
            // `Display` for a clap error includes the "Usage:" line + a short
            // tip; that's exactly what a status line should show.
            Err(err.to_string())
        }
    }
}

/// Render the full `-h`/`--help` text for a clap-derived arg struct. Used by
/// [`Command::help_text`] so `/help <cmd>` and `/cmd -h` agree with the parser.
fn render_help<T: CommandFactory>() -> String {
    T::command()
        .render_help()
        .to_string()
        .trim_end()
        .to_string()
}

/// One-line usage hint for a slash command (Issue 2). For arg-bearing commands
/// this is the clap struct's rendered help; for arg-less commands it's the
/// static [`Command::description`]. Used by the input-box whisper
/// ([`super::input_panel::draw_input`]) and `/help <cmd>` so they agree with the
/// parser.
impl Command {
    pub fn help_text(&self) -> String {
        match self {
            Command::Compact => render_help::<CompactArgs>(),
            Command::Set => render_help::<SetArgs>(),
            Command::Mcp => render_help::<McpArgs>(),
            Command::Allow => render_help::<AllowArgs>(),
            Command::Bash => render_help::<BashArgs>(),
            Command::Enable => render_help::<EnableArgs>(),
            Command::Disable => render_help::<DisableArgs>(),
            Command::Pause | Command::Continue | Command::Stop => {
                // Lifecycle structs share one shape; name it after the command
                // so the rendered usage reads `/stop`/`/pause`/`/continue`.
                let name = match self {
                    Command::Pause => "pause",
                    Command::Continue => "continue",
                    Command::Stop => "stop",
                    _ => unreachable!("only lifecycle commands reach here"),
                };
                LifecycleArgs::command()
                    .name(name)
                    .render_help()
                    .to_string()
                    .trim_end()
                    .to_string()
            }
            // Arg-less commands have nothing for clap to render; the static
            // description is the whole usage hint.
            _ => self.description().to_string(),
        }
    }

    /// Whether this command accepts clap-renderable arguments (so `/cmd -h` is
    /// meaningful). The lifecycle trio counts even though they share one struct.
    pub fn has_args(&self) -> bool {
        matches!(
            self,
            Command::Compact
                | Command::Set
                | Command::Mcp
                | Command::Allow
                | Command::Bash
                | Command::Enable
                | Command::Disable
                | Command::Pause
                | Command::Continue
                | Command::Stop
        )
    }
}

// --- public parse entry points (delegating to clap) --------------------------
//
// Each wraps [`tokens_after_slash`] + [`parse_or_usage`] so the caller still
// passes the raw `/cmd …` text it always did. The return types stay identical
// to the pre-Issue-2 parsers so `event_loop`/`send_*` are untouched.

/// Parse `/compact`'s trailing text via clap, preserving the original
/// `(kept, instructions)` shape.
pub(crate) fn parse_compact_via_clap(text: &str) -> Result<(u64, Option<String>), String> {
    let argv = tokens_after_slash(text, &Command::Compact).unwrap_or_default();
    let args: CompactArgs = parse_or_usage(argv)?;
    let instructions = (!args.instructions.is_empty())
        .then(|| args.instructions.join(" "))
        .filter(|s| !s.is_empty());
    Ok((args.keep.unwrap_or(0), instructions))
}

/// Parse `/set`'s trailing text via clap, mapping the typed `SetKey` + raw value
/// onto the partial [`GenerationParams`] override (same friendly per-key errors
/// the pre-clap parser produced). Clap's "invalid value" wording for an unknown
/// key is normalized to "unknown /set key" so the error matches the pre-Issue-2
/// message the status-line path rendered.
pub(crate) fn parse_set_via_clap(
    text: &str,
) -> Result<entanglement_provider::GenerationParams, String> {
    use entanglement_provider::{GenerationParams, ReasoningEffort};
    let argv = tokens_after_slash(text, &Command::Set).unwrap_or_default();
    let args: SetArgs = parse_or_usage(argv).map_err(|e| {
        // "invalid value 'bogus' for '<KEY>'" → "unknown /set key: bogus"
        if e.contains("invalid value") && e.contains("KEY") {
            let quoted = e.split('\'').nth(1).unwrap_or("?");
            format!("unknown /set key: {quoted}")
        } else {
            e
        }
    })?;
    let mut overrides = GenerationParams::default();
    match args.key {
        SetKey::Temperature => {
            overrides.temperature = Some(
                args.value
                    .parse::<f32>()
                    .map_err(|_| format!("invalid temperature value: {}", args.value))?,
            );
        }
        SetKey::Effort => {
            overrides.reasoning_effort = Some(match args.value.to_lowercase().as_str() {
                "low" => ReasoningEffort::Low,
                "medium" => ReasoningEffort::Medium,
                "high" => ReasoningEffort::High,
                _ => {
                    return Err(format!(
                        "invalid effort value: {} (expected low|medium|high)",
                        args.value
                    ))
                }
            });
        }
        SetKey::ThinkingBudget => {
            overrides.thinking_budget_tokens = Some(
                args.value
                    .parse::<u32>()
                    .map_err(|_| format!("invalid thinking_budget value: {}", args.value))?,
            );
        }
        SetKey::MaxTokens => {
            overrides.max_output_tokens = Some(
                args.value
                    .parse::<u32>()
                    .map_err(|_| format!("invalid max_tokens value: {}", args.value))?,
            );
        }
    }
    Ok(overrides)
}

/// Parse `/stop`/`/pause`/`/continue`'s shared `--all`/`-a` flag via clap.
pub(crate) fn parse_lifecycle_via_clap(text: &str, cmd: Command) -> Result<bool, String> {
    let argv = tokens_after_slash(text, &cmd).unwrap_or_default();
    let args: LifecycleArgs = parse_or_usage(argv)?;
    Ok(args.all)
}

/// Parse `/allow <path>` via clap — just validates that a path token is present.
/// The root-relative normalization itself stays in
/// [`crate::tui::allow_command::normalize_allow_dir`] (it needs the head's
/// `root`, which the clap layer doesn't see).
pub(crate) fn parse_allow_via_clap(text: &str) -> Result<String, String> {
    let argv = tokens_after_slash(text, &Command::Allow).unwrap_or_default();
    let args: AllowArgs = parse_or_usage(argv)?;
    Ok(args.path)
}

/// Parse `/enable`/`/disable`'s subcommand + optional `--allow` via clap.
/// `enabling` selects which command's grammar `text` is parsed under. Returns
/// the parsed clap struct; the caller ([`crate::tui::enable_command`]) maps it
/// onto [`EnableCommand`].
pub(crate) fn parse_enable_via_clap(
    text: &str,
    enabling: bool,
) -> Result<(Option<EnableTarget>, bool), String> {
    let cmd = if enabling {
        Command::Enable
    } else {
        Command::Disable
    };
    let argv = tokens_after_slash(text, &cmd).unwrap_or_default();
    if enabling {
        let args: EnableArgs = parse_or_usage(argv)?;
        Ok((args.target, args.allow))
    } else {
        let args: DisableArgs = parse_or_usage(argv)?;
        Ok((args.target, false))
    }
}

/// Parse `/mcp`'s subcommand via clap. Returns the parsed subcommand (or `None`
/// for a bare `/mcp` which defaults to `list`); the caller
/// ([`crate::tui::mcp_command`]) maps `McpSub` onto [`McpCommand`], building the
/// `McpServerSpec` from the parsed pieces.
pub(crate) fn parse_mcp_via_clap(text: &str) -> Result<Option<McpSub>, String> {
    let argv = tokens_after_slash(text, &Command::Mcp).unwrap_or_default();
    let args: McpArgs = parse_or_usage(argv)?;
    Ok(args.cmd)
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- CompactArgs ---------------------------------------------------------

    #[test]
    fn compact_bare_is_zero_keep_no_instructions() {
        assert_eq!(parse_compact_via_clap("/compact"), Ok((0, None)));
    }

    #[test]
    fn compact_instructions_only() {
        assert_eq!(
            parse_compact_via_clap("/compact keep the auth flow details"),
            Ok((0, Some("keep the auth flow details".to_string())))
        );
    }

    #[test]
    fn compact_keep_only() {
        assert_eq!(parse_compact_via_clap("/compact --keep 3"), Ok((3, None)));
    }

    #[test]
    fn compact_keep_and_instructions() {
        assert_eq!(
            parse_compact_via_clap("/compact --keep 3 keep the auth flow details"),
            Ok((3, Some("keep the auth flow details".to_string())))
        );
    }

    #[test]
    fn compact_keep_missing_value_is_an_error() {
        assert!(parse_compact_via_clap("/compact --keep").is_err());
    }

    #[test]
    fn compact_keep_invalid_value_is_an_error() {
        assert!(parse_compact_via_clap("/compact --keep abc").is_err());
    }

    #[test]
    fn compact_help_flag_renders_keep_option() {
        // `/compact -h` surfaces clap's rendered help; assert it names --keep.
        let argv = tokens_after_slash("/compact -h", &Command::Compact).unwrap_or_default();
        let err = CompactArgs::try_parse_from(&argv).unwrap_err();
        let help = err.to_string();
        assert!(
            help.contains("--keep"),
            "expected --keep in compact help, got: {help}"
        );
    }

    // --- SetArgs -------------------------------------------------------------

    #[test]
    fn set_temperature_parses() {
        let argv = tokens_after_slash("/set temperature 0.7", &Command::Set).unwrap_or_default();
        let args: SetArgs = SetArgs::try_parse_from(&argv).unwrap();
        assert_eq!(args.key, SetKey::Temperature);
        assert_eq!(args.value, "0.7");
    }

    #[test]
    fn set_effort_parses() {
        let argv = tokens_after_slash("/set effort high", &Command::Set).unwrap_or_default();
        let args: SetArgs = SetArgs::try_parse_from(&argv).unwrap();
        assert_eq!(args.key, SetKey::Effort);
        assert_eq!(args.value, "high");
    }

    #[test]
    fn set_thinking_budget_aliases() {
        for spelling in ["thinking_budget", "thinking_budget_tokens"] {
            let argv = tokens_after_slash(&format!("/set {spelling} 4096"), &Command::Set)
                .unwrap_or_default();
            let args: SetArgs = SetArgs::try_parse_from(&argv).unwrap();
            assert_eq!(args.key, SetKey::ThinkingBudget, "spelling={spelling}");
        }
    }

    #[test]
    fn set_max_tokens_aliases() {
        for spelling in ["max_tokens", "max_output_tokens"] {
            let argv = tokens_after_slash(&format!("/set {spelling} 8192"), &Command::Set)
                .unwrap_or_default();
            let args: SetArgs = SetArgs::try_parse_from(&argv).unwrap();
            assert_eq!(args.key, SetKey::MaxTokens, "spelling={spelling}");
        }
    }

    #[test]
    fn set_via_clap_round_trips_into_generation_params() {
        let parsed = parse_set_via_clap("/set temperature 0.7").unwrap();
        assert_eq!(parsed.temperature, Some(0.7));
    }

    #[test]
    fn set_via_clap_rejects_unknown_key() {
        assert!(parse_set_via_clap("/set bogus 1").is_err());
    }

    #[test]
    fn set_via_clap_rejects_bad_value() {
        assert!(parse_set_via_clap("/set temperature nope").is_err());
        assert!(parse_set_via_clap("/set effort extreme").is_err());
        assert!(parse_set_via_clap("/set max_tokens nope").is_err());
    }

    #[test]
    fn set_via_clap_rejects_missing_args() {
        assert!(parse_set_via_clap("/set").is_err());
        assert!(parse_set_via_clap("/set temperature").is_err());
    }

    #[test]
    fn set_help_flag_renders_key_value_usage() {
        let argv = tokens_after_slash("/set -h", &Command::Set).unwrap_or_default();
        let err = SetArgs::try_parse_from(&argv).unwrap_err();
        let help = err.to_string();
        assert!(
            help.contains("KEY") || help.contains("key"),
            "expected key/value usage in set help, got: {help}"
        );
    }

    // --- LifecycleArgs -------------------------------------------------------

    #[test]
    fn lifecycle_bare_is_not_all() {
        assert_eq!(parse_lifecycle_via_clap("/stop", Command::Stop), Ok(false));
    }

    #[test]
    fn lifecycle_all_flag_long() {
        assert_eq!(
            parse_lifecycle_via_clap("/stop --all", Command::Stop),
            Ok(true)
        );
    }

    #[test]
    fn lifecycle_all_flag_short() {
        assert_eq!(
            parse_lifecycle_via_clap("/pause -a", Command::Pause),
            Ok(true)
        );
    }

    #[test]
    fn lifecycle_continue_alias() {
        assert_eq!(
            parse_lifecycle_via_clap("/continue --all", Command::Continue),
            Ok(true)
        );
    }

    #[test]
    fn lifecycle_rejects_unknown_token() {
        assert!(parse_lifecycle_via_clap("/stop frobnicate", Command::Stop).is_err());
    }

    #[test]
    fn lifecycle_help_flag_renders_all_option() {
        let argv = tokens_after_slash("/stop -h", &Command::Stop).unwrap_or_default();
        let err = LifecycleArgs::try_parse_from(&argv).unwrap_err();
        let help = err.to_string();
        assert!(
            help.contains("--all"),
            "expected --all in lifecycle help, got: {help}"
        );
    }

    // --- render_help sanity --------------------------------------------------

    #[test]
    fn render_help_compact_names_keep() {
        let help = render_help::<CompactArgs>();
        assert!(help.contains("--keep"));
    }

    #[test]
    fn render_help_set_names_key() {
        let help = render_help::<SetArgs>();
        assert!(help.contains("KEY") || help.contains("key"));
    }
}
