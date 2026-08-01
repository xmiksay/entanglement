//! `/enable` + `/disable` subcommand parsing + wire dispatch (#539, ADR-0149):
//! per-session tool-overlay editing — enable an MCP server (`/enable mcp
//! <server>`), a single tool or pattern (`/enable tool <name>`), or retract
//! (`/disable ...`, bare `/disable` clears). Kept in its own module mirroring
//! `mcp_command.rs`/`bash_command.rs` (the raw-text re-parse pattern), since
//! `commands.rs`/`event_loop.rs` are past the 400-line cap.

use entanglement_core::{Holly, InMsg, ToolOverlayEntry};

use super::app::App;
use super::commands::Command;

/// One parsed `/enable` or `/disable` subcommand. `pattern` is the overlay
/// entry it maps onto: `mcp <server>` ⇒ `mcp__<server>__*`, `tool <x>` ⇒ `x`
/// verbatim (a name or a `*`/`?` pattern).
#[derive(Debug, Clone, PartialEq)]
pub enum EnableCommand {
    /// Bare `/enable`: show the session's overlay + the available roster.
    Show,
    /// Add (or re-grade) one overlay entry on the active session.
    Enable { pattern: String, allow: bool },
    /// Remove one overlay entry (by its exact stored pattern).
    Disable { pattern: String },
    /// Bare `/disable`: clear the whole overlay.
    Clear,
}

const ENABLE_USAGE: &str = "usage: /enable | /enable mcp <server> [--allow] | \
     /enable tool <name-or-pattern> [--allow]";
const DISABLE_USAGE: &str = "usage: /disable | /disable mcp <server> | /disable tool <name>";

/// Parse `/enable ...` / `/disable ...` — the same raw-text re-parse pattern as
/// [`crate::tui::mcp_command::parse_mcp_args`]. `enabling` selects which
/// command's grammar `text` is parsed under.
pub fn parse_enable_args(text: &str, enabling: bool) -> Result<EnableCommand, String> {
    let cmd = if enabling {
        Command::Enable
    } else {
        Command::Disable
    };
    let rest = text
        .trim()
        .strip_prefix(&cmd.slash_name())
        .map(str::trim)
        .unwrap_or("");
    let mut tokens = rest.split_whitespace();
    let sub = tokens.next().unwrap_or("");
    match (enabling, sub) {
        (true, "") => Ok(EnableCommand::Show),
        (false, "") | (false, "all") => Ok(EnableCommand::Clear),
        (_, "mcp") | (_, "tool") => {
            let target = tokens
                .next()
                .ok_or_else(|| usage(enabling).to_string())?
                .to_string();
            let pattern = if sub == "mcp" {
                format!("mcp__{target}__*")
            } else {
                target
            };
            let mut allow = false;
            for flag in tokens {
                match flag {
                    "--allow" if enabling => allow = true,
                    other => return Err(format!("unknown {} flag: {other}", cmd.slash_name())),
                }
            }
            if enabling {
                Ok(EnableCommand::Enable { pattern, allow })
            } else {
                Ok(EnableCommand::Disable { pattern })
            }
        }
        (_, other) => Err(format!(
            "unknown {} subcommand: {other} — {}",
            cmd.slash_name(),
            usage(enabling)
        )),
    }
}

fn usage(enabling: bool) -> &'static str {
    if enabling {
        ENABLE_USAGE
    } else {
        DISABLE_USAGE
    }
}

/// Send `/enable`/`/disable`: computes the active session's new full overlay
/// list from the app's tracked mirror and sends `InMsg::SetToolOverlay`
/// (full replacement — the confirmation folds back via
/// `App::handle_tool_overlay_changed`). A parse error renders as a status line
/// instead of hitting the engine, mirroring `send_mcp`/`send_bash`.
pub(super) async fn send_enable(app: &mut App, holly: &Holly, text: &str, enabling: bool) {
    match parse_enable_args(text, enabling) {
        Ok(EnableCommand::Show) => app.render_overlay_status(),
        Ok(cmd) => {
            let session = app.active_session_id().clone();
            let mut entries = app.overlay_entries(&session);
            match cmd {
                EnableCommand::Enable { pattern, allow } => {
                    // Re-enabling an existing pattern re-grades it in place.
                    entries.retain(|e| e.pattern != pattern);
                    entries.push(ToolOverlayEntry { pattern, allow });
                }
                EnableCommand::Disable { pattern } => {
                    let before = entries.len();
                    entries.retain(|e| e.pattern != pattern);
                    if entries.len() == before {
                        app.record_enable_error(format!(
                            "`{pattern}` is not in this session's overlay"
                        ));
                        return;
                    }
                }
                EnableCommand::Clear => entries.clear(),
                EnableCommand::Show => unreachable!("handled above"),
            }
            let _ = holly.send(InMsg::SetToolOverlay { session, entries }).await;
        }
        Err(message) => app.record_enable_error(message),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_enable_bare_shows() {
        assert_eq!(parse_enable_args("/enable", true), Ok(EnableCommand::Show));
    }

    #[test]
    fn parse_enable_mcp_maps_to_server_pattern() {
        assert_eq!(
            parse_enable_args("/enable mcp chessbase", true),
            Ok(EnableCommand::Enable {
                pattern: "mcp__chessbase__*".to_string(),
                allow: false,
            })
        );
    }

    #[test]
    fn parse_enable_tool_verbatim_with_allow() {
        assert_eq!(
            parse_enable_args("/enable tool mcp__chessbase__evaluate --allow", true),
            Ok(EnableCommand::Enable {
                pattern: "mcp__chessbase__evaluate".to_string(),
                allow: true,
            })
        );
        assert_eq!(
            parse_enable_args("/enable tool bash", true),
            Ok(EnableCommand::Enable {
                pattern: "bash".to_string(),
                allow: false,
            })
        );
    }

    #[test]
    fn parse_disable_forms() {
        assert_eq!(
            parse_enable_args("/disable", false),
            Ok(EnableCommand::Clear)
        );
        assert_eq!(
            parse_enable_args("/disable all", false),
            Ok(EnableCommand::Clear)
        );
        assert_eq!(
            parse_enable_args("/disable mcp chessbase", false),
            Ok(EnableCommand::Disable {
                pattern: "mcp__chessbase__*".to_string(),
            })
        );
    }

    #[test]
    fn parse_rejects_malformed() {
        assert!(parse_enable_args("/enable mcp", true).is_err());
        assert!(parse_enable_args("/enable bogus x", true).is_err());
        // `--allow` is an enable-only flag.
        assert!(parse_enable_args("/disable tool bash --allow", false).is_err());
    }
}
