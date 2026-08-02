//! `/enable` + `/disable` subcommand parsing + wire dispatch (#539, ADR-0149):
//! per-session tool-overlay editing — enable an MCP server (`/enable mcp
//! <server>`), a single tool or pattern (`/enable tool <name>`), disable
//! either the same way (`/disable ...` upserts a **deny** entry, withdrawing
//! even a profile-advertised tool for this session; bare `/disable` clears
//! the whole overlay), or open the session-tools checklist dialog (bare
//! `/enable`). Kept in its own module mirroring
//! `mcp_command.rs`/`bash_command.rs` (the raw-text re-parse pattern), since
//! `commands.rs`/`event_loop.rs` are past the 400-line cap. Issue 2: the
//! `mcp`/`tool` subcommand grammar is now clap-derived
//! ([`crate::tui::command_args::parse_enable_via_clap`]); this module maps the
//! parsed pieces onto [`EnableCommand`].

use entanglement_core::{Holly, InMsg, SessionId, ToolOverlayEntry};

use super::app::App;
use super::command_args::{parse_enable_via_clap, EnableTarget};

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

/// Parse `/enable ...` / `/disable ...` — Issue 2 delegates to clap
/// ([`parse_enable_via_clap`]) and maps the parsed `(target, allow)` onto
/// [`EnableCommand`]. `enabling` selects which command's grammar `text` is
/// parsed under. A bare `/disable` (or its `/disable all` alias) clears the
/// whole overlay.
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
    let (target, allow) = parse_enable_via_clap(text, enabling)?;
    match (enabling, target) {
        (true, None) => Ok(EnableCommand::Show),
        (false, None) => Ok(EnableCommand::Clear),
        (_, Some(EnableTarget::Mcp { server })) => {
            let pattern = format!("mcp__{server}__*");
            if enabling {
                Ok(EnableCommand::Enable { pattern, allow })
            } else {
                Ok(EnableCommand::Disable { pattern })
            }
        }
        (_, Some(EnableTarget::Tool { name })) => {
            if enabling {
                Ok(EnableCommand::Enable {
                    pattern: name,
                    allow,
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
/// session-tools dialog. For an `mcp__<server>__*` pattern naming an
/// *available* (`allowed`-state, #542) server, this first lazily connects it —
/// the same path the `mcp_enable` tool takes — and scopes its visibility to
/// this session; a connect failure renders as a status line and skips the
/// overlay entirely.
pub(super) async fn upsert_enable(app: &mut App, holly: &Holly, pattern: String, allow: bool) {
    if let Err(message) = lazy_enable_available(app, &pattern).await {
        app.record_enable_error(message);
        return;
    }
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

/// The `/enable` side of #542: when `pattern` targets an available MCP server,
/// connect it on demand and mark it enabled for the active session. A pattern
/// that isn't `mcp__<server>__*`, a TUI without handles (tests), or a server
/// that is neither available nor lazily connected (i.e. a startup-`enabled`
/// one, or an ordinary masked tool) all fall through as a no-op.
pub(super) async fn lazy_enable_available(app: &mut App, pattern: &str) -> Result<(), String> {
    let Some(server) = pattern
        .strip_prefix("mcp__")
        .and_then(|rest| rest.strip_suffix("__*"))
        .map(str::to_string)
    else {
        return Ok(());
    };
    let Some(handles) = app.mcp_handles().cloned() else {
        return Ok(());
    };
    if handles.avail.get(&server).is_none() && !handles.avail.is_lazy(&server) {
        return Ok(());
    }
    let session = app.active_session_id().clone();
    crate::mcp::available::enable_for_session(
        &handles.avail,
        &server,
        &session,
        &handles.registry,
        &handles.active,
    )
    .await
    .map(|_| ())
    .map_err(|e| format!("MCP server `{server}`: {e:#}"))
}

/// Upsert a deny entry for `pattern` on the active session, dropping any
/// same-pattern entry — matching tools stop existing for this session even
/// when the profile advertises them. Shared by the typed `/disable` and the
/// `/mcp` panel's `d` key. For a lazily-connected `allowed` server (#542) it
/// also withdraws this session's enablement mark — the symmetric,
/// session-scoped inverse; the connection itself stays up for other sessions.
pub(super) async fn upsert_deny(app: &mut App, holly: &Holly, pattern: String) {
    if let (Some(server), Some(handles)) = (
        pattern
            .strip_prefix("mcp__")
            .and_then(|rest| rest.strip_suffix("__*")),
        app.mcp_handles(),
    ) {
        let avail = handles.avail.clone();
        avail.mark_disabled(server, &app.active_session_id().clone());
    }
    let session = app.active_session_id().clone();
    let mut entries = app.overlay_entries(&session);
    entries.retain(|e| e.pattern != pattern);
    entries.push(ToolOverlayEntry::deny(pattern));
    send_overlay(holly, session, entries).await;
}

/// Lazy-connect every available server implied by `entries`' enable patterns
/// (#555): the session-tools dialog submits a whole diffed roster at once —
/// unlike the single-pattern `upsert_enable` path, any number of its rows may
/// name an `allowed` bundled server (`mcp__<server>__*`, #542) still
/// unconnected. Mirrors `upsert_enable`'s per-pattern connect for each; a deny
/// entry never targets a server to connect, so those are skipped. Stops and
/// reports the first failure so a bad row never lets the rest partially apply.
pub(super) async fn lazy_enable_entries(
    app: &mut App,
    entries: &[ToolOverlayEntry],
) -> Result<(), String> {
    for entry in entries.iter().filter(|e| !e.deny) {
        lazy_enable_available(app, &entry.pattern).await?;
    }
    Ok(())
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

    /// Builds `McpHandles` around a single ungated `allowed` stdio server
    /// pointed at a nonexistent binary — `enable_for_session` fails fast on
    /// spawn, mirroring `mcp::live::tests::mcp_add_surfaces_a_connect_failure_without_touching_state`.
    fn handles_with_unreachable_available_server(name: &str) -> crate::mcp::McpHandles {
        let mut user_mcp = std::collections::HashMap::new();
        user_mcp.insert(
            name.to_string(),
            crate::mcp::McpServerConfig {
                command: Some("definitely-not-a-real-binary-xyz".to_string()),
                args: vec![],
                env: std::collections::HashMap::new(),
                url: None,
                headers: std::collections::HashMap::new(),
                disabled: false,
                capabilities: std::collections::HashMap::new(),
                oauth: None,
                state: Some(crate::mcp::McpServerState::Allowed),
            },
        );
        let catalog = entanglement_core::Catalog { providers: vec![] };
        let (_startup, avail) = crate::mcp::AvailableMcp::partition(&catalog, &user_mcp, vec![]);
        crate::mcp::McpHandles {
            avail: std::sync::Arc::new(avail),
            registry: std::sync::Arc::new(std::sync::RwLock::new(crate::ToolRegistry::new())),
            active: std::sync::Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
        }
    }

    /// Ordinary tool patterns and deny entries never target a server to
    /// connect — a no-op success, with or without `mcp_handles` wired (#555).
    #[tokio::test]
    async fn lazy_enable_entries_skips_ordinary_and_deny_patterns() {
        let mut app = App::new_for_test(SessionId::new("s1"));
        let entries = vec![
            ToolOverlayEntry {
                pattern: "read".to_string(),
                allow: false,
                deny: false,
            },
            ToolOverlayEntry::deny("bash"),
        ];
        assert_eq!(lazy_enable_entries(&mut app, &entries).await, Ok(()));
    }

    /// The session-tools dialog can now check a row naming an available
    /// bundled server (#555): submitting it must attempt the same lazy
    /// connect `upsert_enable` runs for a single typed/panel pattern — a
    /// broken server surfaces its failure instead of silently applying an
    /// overlay entry that unmasks tools which were never registered.
    #[tokio::test]
    async fn lazy_enable_entries_surfaces_a_connect_failure() {
        let mut app = App::new_for_test(SessionId::new("s1"));
        app.set_mcp_handles(handles_with_unreachable_available_server("zread"));
        let entries = vec![ToolOverlayEntry {
            pattern: "mcp__zread__*".to_string(),
            allow: false,
            deny: false,
        }];
        let err = lazy_enable_entries(&mut app, &entries)
            .await
            .expect_err("a broken bundled server must surface, not silently no-op");
        assert!(err.contains("zread"), "unexpected error: {err}");
    }
}
