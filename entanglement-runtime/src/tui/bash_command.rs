//! `/bash` subcommand parsing + wire dispatch (#498): live-enable/disable the
//! `bash`/`bash_output` pair, graded by a `BashGrade` rather than a bare
//! on/off. Kept in its own module — the same "past the 400-line cap" reasoning
//! that split `mcp_command.rs`/`allow_command.rs` out of `commands.rs`/
//! `event_loop.rs`.

use entanglement_core::{BashGrade, Holly, InMsg};

use super::app::App;
use super::commands::Command;

/// One parsed `/bash` subcommand: `On` carries the grade to send with
/// `InMsg::BashEnable`; `Off` needs no further data.
#[derive(Debug, Clone, PartialEq)]
pub enum BashCommand {
    On(BashGrade),
    Off,
}

const BASH_USAGE: &str = "usage: /bash on [--allow [<pattern>] | --ask] | /bash off";

/// Parse `/bash <subcommand> ...` — the same raw-text re-parse pattern as
/// [`crate::tui::mcp_command::parse_mcp_args`], since [`crate::tui::commands::parse_command`]
/// only matches the command name and drops everything after it. A bare
/// `/bash` (no subcommand) defaults to `on` with the safe [`BashGrade::Ask`]
/// grade — every call still goes through the normal approval prompt — so a
/// plain Enter or a command-palette pick (which carries no trailing text)
/// never grants blanket access by accident.
pub fn parse_bash_args(text: &str) -> Result<BashCommand, String> {
    let rest = text
        .trim()
        .strip_prefix(&Command::Bash.slash_name())
        .map(str::trim)
        .unwrap_or("");
    let mut parts = rest.splitn(2, char::is_whitespace);
    let sub = parts.next().unwrap_or("").trim();
    let args = parts.next().unwrap_or("").trim();
    match sub {
        "off" => Ok(BashCommand::Off),
        "" | "on" => parse_bash_on(args),
        other => Err(format!("unknown /bash subcommand: {other} — {BASH_USAGE}")),
    }
}

/// The safe default grade for a bare `/bash on` — every call still goes
/// through the normal approval prompt. Also the command-palette pick's
/// default (`modal_events`, which carries no trailing text so it can't reach
/// [`parse_bash_on`]'s own bare-arg arm), so both paths share one definition
/// of "safe default" instead of a second hardcoded literal.
pub(super) fn default_grade() -> BashGrade {
    BashGrade::Ask
}

/// Parse `/bash on`'s arguments: bare (→ `Ask`), `--ask` (explicit), or
/// `--allow [<pattern>]` (blanket allow, or an argument-scoped
/// `bash(pattern): allow` rule when a pattern follows — the pattern is
/// whatever text trails `--allow`, rejoined verbatim with single spaces so a
/// multi-word glob like `git *` survives, matching how
/// [`crate::permission::permission_arg`]'s own `bash` command line is later
/// matched against it).
fn parse_bash_on(args: &str) -> Result<BashCommand, String> {
    let tokens: Vec<&str> = args.split_whitespace().collect();
    match tokens.first() {
        None => Ok(BashCommand::On(default_grade())),
        Some(&"--ask") => Ok(BashCommand::On(BashGrade::Ask)),
        Some(&"--allow") => {
            let pattern = (!tokens[1..].is_empty()).then(|| tokens[1..].join(" "));
            Ok(BashCommand::On(BashGrade::Allow { pattern }))
        }
        Some(other) => Err(format!("unknown /bash on flag: {other} — {BASH_USAGE}")),
    }
}

/// Send `/bash on|off ...`: both `InMsg::BashEnable`/`BashDisable` are
/// trusted-only (#472, ADR-0124) but the TUI sends over the privileged
/// `Holly::send`, same as `/mcp add`/`remove`. Their confirmation arrives as
/// `OutEvent::BashChanged`, rendered as a status line by
/// `App::handle_bash_changed`. A parse error (unknown subcommand/flag) is
/// rendered as a status line instead of hitting the engine, mirroring
/// `mcp_command::send_mcp`.
pub(super) async fn send_bash(app: &mut App, holly: &Holly, text: &str) {
    match parse_bash_args(text) {
        Ok(BashCommand::On(grade)) => {
            let _ = holly.send(InMsg::BashEnable { grade }).await;
        }
        Ok(BashCommand::Off) => {
            let _ = holly.send(InMsg::BashDisable).await;
        }
        Err(message) => app.record_bash_error(message),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_bash_args_bare_defaults_to_on_ask() {
        assert_eq!(
            parse_bash_args("/bash"),
            Ok(BashCommand::On(BashGrade::Ask))
        );
        assert_eq!(
            parse_bash_args("/bash on"),
            Ok(BashCommand::On(BashGrade::Ask))
        );
    }

    #[test]
    fn parse_bash_args_explicit_ask() {
        assert_eq!(
            parse_bash_args("/bash on --ask"),
            Ok(BashCommand::On(BashGrade::Ask))
        );
    }

    #[test]
    fn parse_bash_args_blanket_allow() {
        assert_eq!(
            parse_bash_args("/bash on --allow"),
            Ok(BashCommand::On(BashGrade::Allow { pattern: None }))
        );
    }

    #[test]
    fn parse_bash_args_allow_with_pattern() {
        assert_eq!(
            parse_bash_args("/bash on --allow git *"),
            Ok(BashCommand::On(BashGrade::Allow {
                pattern: Some("git *".to_string())
            }))
        );
    }

    #[test]
    fn parse_bash_args_off() {
        assert_eq!(parse_bash_args("/bash off"), Ok(BashCommand::Off));
    }

    #[test]
    fn parse_bash_args_unknown_subcommand() {
        assert!(parse_bash_args("/bash bogus")
            .unwrap_err()
            .contains("unknown"));
    }

    #[test]
    fn parse_bash_args_unknown_on_flag() {
        assert!(parse_bash_args("/bash on --bogus")
            .unwrap_err()
            .contains("unknown"));
    }
}
