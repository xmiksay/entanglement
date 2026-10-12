//! `/enable` + `/disable` subcommand parsing + wire dispatch (#539, ADR-0149):
//! per-session tool-overlay editing — enable a single tool or pattern
//! (`/enable tool <name>`, an `mcp__<server>__*` glob included), disable it
//! the same way (`/disable ...` upserts a **deny** entry, withdrawing
//! even a profile-advertised tool for this session; bare `/disable` clears
//! the whole overlay), or open the session-tools checklist dialog (bare
//! `/enable`). An enable entry naming an always-registered tool like `bash`
//! (ADR-0195) is now a pure **grade override** — it exists either way; a deny
//! entry still withdraws it at dispatch for that session, same as any tool.
//! Kept in its own module mirroring `mcp_command.rs` (the raw-text re-parse
//! pattern), since `commands.rs`/`event_loop.rs` are past the 400-line cap.
//! MCP servers need no enabling (ADR-0226: registered = available), so the
//! old `mcp <server>` subcommand is gone. The `tool` subcommand grammar is clap-derived
//! ([`crate::tui::command_args::parse_enable_via_clap`]); this module maps the
//! parsed pieces onto [`EnableCommand`].

use entanglement_core::{Holly, InMsg, SessionId, ToolOverlayEntry};

use super::app::App;
use super::command_args::{parse_enable_via_clap, EnableTarget};

/// One parsed `/enable` or `/disable` subcommand. `pattern` is the overlay
/// entry it maps onto: `tool <x>` ⇒ `x` verbatim (a name or a `*`/`?`
/// pattern).
#[derive(Debug, Clone, PartialEq)]
pub enum EnableCommand {
    /// Bare `/enable`: open the session-tools checklist dialog.
    Show,
    /// Upsert one enable entry on the active session (drops a same-pattern
    /// deny entry — the two are mutually exclusive per pattern). `arg_pattern`
    /// (#611/ADR-0163) narrows `allow` to matching command arguments only —
    /// `--allow git *` on `/enable tool bash` — and is `None` for a flat
    /// grade or when `allow` is `false`.
    Enable {
        pattern: String,
        allow: bool,
        arg_pattern: Option<String>,
    },
    /// Upsert one **deny** entry (drops a same-pattern enable entry):
    /// matching tools stop existing for this session, even ones the profile
    /// advertises.
    Disable { pattern: String },
    /// Bare `/disable`: clear the whole overlay (back to the profile mask).
    Clear,
}

/// Parse `/enable ...` / `/disable ...` — Issue 2 delegates to clap
/// ([`parse_enable_via_clap`]) and maps the parsed `(target, allow,
/// arg_pattern)` onto [`EnableCommand`]. `enabling` selects which command's
/// grammar `text` is parsed under. A bare `/disable` (or its `/disable all`
/// alias) clears the whole overlay.
pub fn parse_enable_args(text: &str, enabling: bool) -> Result<EnableCommand, String> {
    // `/disable all` is an alias for bare `/disable` (Clear) — not a clap
    // subcommand, so intercept it before clap rejects `all` as unrecognized.
    if !enabling {
        let trimmed = text
            .trim()
            .strip_prefix("/disable")
            .map(str::trim)
            .unwrap_or("");
        if trimmed.is_empty() || trimmed == "all" {
            return Ok(EnableCommand::Clear);
        }
    }
    let (target, allow, arg_pattern) = parse_enable_via_clap(text, enabling)?;
    match (enabling, target) {
        (true, None) => Ok(EnableCommand::Show),
        (false, None) => Ok(EnableCommand::Clear),
        (_, Some(EnableTarget::Tool { name })) => {
            if enabling {
                Ok(EnableCommand::Enable {
                    pattern: name,
                    allow,
                    arg_pattern,
                })
            } else {
                Ok(EnableCommand::Disable { pattern: name })
            }
        }
    }
}

/// Send `/enable`/`/disable`: computes the active session's new full overlay
/// list from the app's tracked mirror and sends `InMsg::SetToolOverlay`
/// (full replacement — the confirmation folds back via
/// `App::handle_tool_overlay_changed`). A parse error renders as a status line
/// instead of hitting the engine, mirroring `send_mcp`.
pub(super) async fn send_enable(app: &mut App, holly: &Holly, text: &str, enabling: bool) {
    match parse_enable_args(text, enabling) {
        Ok(EnableCommand::Show) => app.open_session_tools_dialog(),
        Ok(EnableCommand::Enable {
            pattern,
            allow,
            arg_pattern,
        }) => {
            upsert_enable(app, holly, pattern, allow, arg_pattern).await;
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
/// Shared by the typed `/enable` and the `/tools` view. `arg_pattern`
/// (#611/ADR-0163) narrows `allow` to matching command arguments only.
pub(super) async fn upsert_enable(
    app: &mut App,
    holly: &Holly,
    pattern: String,
    allow: bool,
    arg_pattern: Option<String>,
) {
    // ADR-0199 part 4: report what the pattern matched, so a hand-typed glob
    // ("mcp__docs__*", "skill__research__*") gets immediate feedback on how
    // many currently-registered tools it actually reaches, before the
    // overlay confirmation round-trips back.
    app.record_enable_match_count(&pattern);
    let session = app.active_session_id().clone();
    let mut entries = app.overlay_entries(&session);
    entries.retain(|e| e.pattern != pattern);
    entries.push(ToolOverlayEntry {
        pattern,
        allow,
        deny: false,
        arg_pattern,
    });
    send_overlay(holly, session, entries).await;
}

/// Upsert a deny entry for `pattern` on the active session, dropping any
/// same-pattern entry — matching tools stop existing for this session even
/// when the profile advertises them.
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

    /// ADR-0226: MCP servers need no enabling — `mcp <server>` is gone.
    #[test]
    fn parse_enable_mcp_is_rejected() {
        assert!(parse_enable_args("/enable mcp chessbase", true).is_err());
        assert!(parse_enable_args("/disable mcp chessbase", false).is_err());
    }

    #[test]
    fn parse_enable_tool_verbatim_with_allow() {
        assert_eq!(
            parse_enable_args("/enable tool mcp__chessbase__evaluate --allow", true),
            Ok(EnableCommand::Enable {
                pattern: "mcp__chessbase__evaluate".to_string(),
                allow: true,
                arg_pattern: None,
            })
        );
        assert_eq!(
            parse_enable_args("/enable tool bash", true),
            Ok(EnableCommand::Enable {
                pattern: "bash".to_string(),
                allow: false,
                arg_pattern: None,
            })
        );
    }

    /// A `--allow`'s free-form trailing pattern (`/enable tool bash
    /// --allow git *`) narrows the grant to commands matching `git *` —
    /// the arg-scoped grade of a live enable. The multi-word pattern
    /// rejoins verbatim with single spaces (clap's `--allow` alone would
    /// only ever capture the first token).
    #[test]
    fn parse_enable_tool_allow_with_arg_pattern() {
        assert_eq!(
            parse_enable_args("/enable tool bash --allow git *", true),
            Ok(EnableCommand::Enable {
                pattern: "bash".to_string(),
                allow: true,
                arg_pattern: Some("git *".to_string()),
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
            parse_enable_args("/disable tool mcp__chessbase__*", false),
            Ok(EnableCommand::Disable {
                pattern: "mcp__chessbase__*".to_string(),
            })
        );
    }

    #[test]
    fn parse_rejects_malformed() {
        assert!(parse_enable_args("/enable tool", true).is_err());
        assert!(parse_enable_args("/enable bogus x", true).is_err());
        // `--allow` is an enable-only flag.
        assert!(parse_enable_args("/disable tool bash --allow", false).is_err());
    }
}
