//! Clap-derived help + arg parsing for every slash command that takes args
//! (Issue 2). Each `…Args` struct is a pure **parser + help generator** — never
//! wired into the binary's own clap CLI in `args.rs` (that CLI parses `skutter`'s
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

// Per-command clap structs live in the sibling `command_specs` module (split at
// the 400-line cap); re-exported so every `command_args::…Args` path still
// resolves.
pub use super::command_specs::*;

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
            Command::Stop => LifecycleArgs::command()
                .name("stop")
                .render_help()
                .to_string()
                .trim_end()
                .to_string(),
            // First line is the whisper's usage hint; the rest is `/help <cmd>`.
            Command::User(c) => format!(
                "{} — {}\n(user command, {}: {})",
                c.usage(),
                c.description,
                c.layer.label(),
                c.source
            ),
            // Arg-less commands have nothing for clap to render; the static
            // description is the whole usage hint.
            _ => self.description().to_string(),
        }
    }

    /// Whether this command accepts clap-renderable arguments (so `/cmd -h` is
    /// meaningful).
    pub fn has_args(&self) -> bool {
        matches!(
            self,
            Command::Compact | Command::Set | Command::Mcp | Command::Stop
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
            overrides.reasoning_effort =
                Some(ReasoningEffort::parse(&args.value).ok_or_else(|| {
                    format!(
                        "invalid effort value: {} (expected low|medium|high|xhigh|max)",
                        args.value
                    )
                })?);
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

/// Parse `/stop`'s `--all`/`-a` flag via clap.
pub(crate) fn parse_lifecycle_via_clap(text: &str, cmd: Command) -> Result<bool, String> {
    let argv = tokens_after_slash(text, &cmd).unwrap_or_default();
    let args: LifecycleArgs = parse_or_usage(argv)?;
    Ok(args.all)
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
            parse_lifecycle_via_clap("/stop -a", Command::Stop),
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
