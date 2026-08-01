//! `/enable` + `/disable` subcommand parsing + wire dispatch (#539, ADR-0149):
//! per-session tool-overlay editing — enable an MCP server (`/enable mcp
//! <server>`), a single tool or pattern (`/enable tool <name>`), disable
//! either the same way (`/disable ...` upserts a **deny** entry, withdrawing
//! even a profile-advertised tool for this session; bare `/disable` clears
//! the whole overlay), or open the session-tools checklist dialog (bare
//! `/enable`). Kept in its own module mirroring
//! `mcp_command.rs`/`bash_command.rs` (the raw-text re-parse pattern), since
//! `commands.rs`/`event_loop.rs` are past the 400-line cap.

use entanglement_core::{Holly, InMsg, SessionId, ToolOverlayEntry};

use super::app::App;
use super::commands::Command;

/// One parsed `/enable` or `/disable` subcommand. `pattern` is the overlay
/// entry it maps onto: `mcp <server>` ⇒ `mcp__<server>__*`, `tool <x>` ⇒ `x`
/// verbatim (a name or a `*`/`?` pattern).
#[derive(Debug, Clone, PartialEq)]
pub enum EnableCommand {
    /// Bare `/enable`: open the session-tools checklist dialog.
    Show,
    /// Upsert one enable entry on the active session (drops a same-pattern
    /// deny entry — the two are mutually exclusive per pattern).
    Enable { pattern: String, allow: bool },
    /// Upsert one **deny** entry (drops a same-pattern enable entry):
    /// matching tools stop existing for this session, even ones the profile
    /// advertises.
    Disable { pattern: String },
    /// Bare `/disable`: clear the whole overlay (back to the profile mask).
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
        Ok(EnableCommand::Show) => app.open_session_tools_dialog(),
        Ok(EnableCommand::Enable { pattern, allow }) => {
            upsert_enable(app, holly, pattern, allow).await;
        }
        Ok(EnableCommand::Disable { pattern }) => {
            upsert_deny(app, holly, pattern).await;
        }
        Ok(EnableCommand::Clear) => {
            let session = app.active_session_id().clone();
            send_overlay(holly, session, Vec::new()).await;
        }
        Err(message) => app.record_enable_error(message),
    }
}

/// Upsert an enable entry for `pattern` on the active session, dropping any
/// same-pattern entry (a re-enable re-grades in place; a prior deny flips).
/// Shared by the typed `/enable`, the `/mcp` panel's `e` key, and the
/// session-tools dialog.
pub(super) async fn upsert_enable(app: &mut App, holly: &Holly, pattern: String, allow: bool) {
    let session = app.active_session_id().clone();
    let mut entries = app.overlay_entries(&session);
    entries.retain(|e| e.pattern != pattern);
    entries.push(ToolOverlayEntry {
        pattern,
        allow,
        deny: false,
    });
    send_overlay(holly, session, entries).await;
}

/// Upsert a deny entry for `pattern` on the active session, dropping any
/// same-pattern entry — matching tools stop existing for this session even
/// when the profile advertises them. Shared by the typed `/disable` and the
/// `/mcp` panel's `d` key.
pub(super) async fn upsert_deny(app: &mut App, holly: &Holly, pattern: String) {
    let session = app.active_session_id().clone();
    let mut entries = app.overlay_entries(&session);
    entries.retain(|e| e.pattern != pattern);
    entries.push(ToolOverlayEntry::deny(pattern));
    send_overlay(holly, session, entries).await;
}

/// Send the full-replacement overlay update. Also shared by the session-tools
/// dialog's submit path (`modal_events::handle_session_tools_dialog_event`).
pub(super) async fn send_overlay(
    holly: &Holly,
    session: SessionId,
    entries: Vec<ToolOverlayEntry>,
) {
    let _ = holly.send(InMsg::SetToolOverlay { session, entries }).await;
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
