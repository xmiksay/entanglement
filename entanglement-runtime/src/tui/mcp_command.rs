//! `/mcp` subcommand parsing + wire dispatch (#373): kept in its own module
//! rather than folded into `tui/commands.rs` (name/description/dispatch) or
//! `tui/event_loop.rs` (the `Enter`-key send helpers) — both already past the
//! 400-line cap — mirroring how `CommandPalette` was split out of
//! `commands.rs` once it crossed the cap (#376).

use std::collections::HashMap;

use entanglement_core::{Holly, InMsg, McpServerSpec, SessionId};

use super::app::App;
use super::commands::Command;

/// One parsed `/mcp` subcommand: `List` needs no further wire data; `Add`/
/// `Remove` carry exactly what `InMsg::McpAdd`/`McpRemove` need.
#[derive(Debug, Clone, PartialEq)]
pub enum McpCommand {
    List,
    Add { name: String, spec: McpServerSpec },
    Remove { name: String },
}

const MCP_USAGE: &str = "usage: /mcp list | /mcp add <name> -- <command> [args...] | \
     /mcp add <name> --url <url> [--header KEY:VALUE]... | /mcp remove <name>";

/// Parse `/mcp <subcommand> ...` — the same raw-text re-parse pattern as
/// [`crate::tui::commands::parse_set_args`], since [`crate::tui::commands::parse_command`]
/// only matches the command name and drops everything after it. A bare `/mcp`
/// (no subcommand) defaults to `list`, so both a plain Enter and a
/// command-palette pick (which carries no trailing text) do something useful.
pub fn parse_mcp_args(text: &str) -> Result<McpCommand, String> {
    let rest = text
        .trim()
        .strip_prefix(&Command::Mcp.slash_name())
        .map(str::trim)
        .unwrap_or("");
    let mut parts = rest.splitn(2, char::is_whitespace);
    let sub = parts.next().unwrap_or("").trim();
    let args = parts.next().unwrap_or("").trim();
    match sub {
        "" | "list" => Ok(McpCommand::List),
        "remove" => {
            let name = args
                .split_whitespace()
                .next()
                .ok_or_else(|| "usage: /mcp remove <name>".to_string())?;
            Ok(McpCommand::Remove {
                name: name.to_string(),
            })
        }
        "add" => parse_mcp_add(args),
        other => Err(format!("unknown /mcp subcommand: {other} — {MCP_USAGE}")),
    }
}

/// Parse `/mcp add`'s arguments: `<name> -- <command> [args...]` (stdio) or
/// `<name> --url <url> [--header KEY:VALUE]...` (streamable HTTP). Whitespace
/// splitting only — no quoting — matching every other raw-text re-parse in the
/// TUI.
fn parse_mcp_add(args: &str) -> Result<McpCommand, String> {
    let mut tokens = args.split_whitespace();
    let name = tokens
        .next()
        .ok_or_else(|| MCP_USAGE.to_string())?
        .to_string();
    let rest: Vec<&str> = tokens.collect();
    match rest.first() {
        Some(&"--") => {
            let command = rest
                .get(1)
                .ok_or_else(|| "usage: /mcp add <name> -- <command> [args...]".to_string())?
                .to_string();
            let args = rest[2..].iter().map(|s| s.to_string()).collect();
            Ok(McpCommand::Add {
                name,
                spec: McpServerSpec {
                    command: Some(command),
                    args,
                    env: HashMap::new(),
                    url: None,
                    headers: HashMap::new(),
                    disabled: false,
                },
            })
        }
        Some(&"--url") => {
            let url = rest
                .get(1)
                .ok_or_else(|| "usage: /mcp add <name> --url <url>".to_string())?
                .to_string();
            let mut headers = HashMap::new();
            let mut i = 2;
            while i < rest.len() {
                if rest[i] != "--header" {
                    return Err(format!("unknown /mcp add flag: {}", rest[i]));
                }
                let kv = rest
                    .get(i + 1)
                    .ok_or_else(|| "usage: --header KEY:VALUE".to_string())?;
                let (k, v) = kv
                    .split_once(':')
                    .ok_or_else(|| format!("invalid --header value: {kv} (expected KEY:VALUE)"))?;
                headers.insert(k.to_string(), v.to_string());
                i += 2;
            }
            Ok(McpCommand::Add {
                name,
                spec: McpServerSpec {
                    command: None,
                    args: Vec::new(),
                    env: HashMap::new(),
                    url: Some(url),
                    headers,
                    disabled: false,
                },
            })
        }
        _ => Err(MCP_USAGE.to_string()),
    }
}

/// Send `/mcp list|add|remove ...`: `list` records the query's correlation id
/// (so only its own reply opens the panel, `App::handle_mcp_list`) and sends
/// `InMsg::McpList`; `add`/`remove` send `InMsg::McpAdd`/`McpRemove` directly —
/// their confirmation arrives as `OutEvent::McpChanged`, rendered as a status
/// line by `App::handle_mcp_changed`. A parse error (unknown subcommand,
/// malformed add/remove) is rendered as a status line instead of hitting the
/// engine, mirroring `event_loop::send_set`.
pub(super) async fn send_mcp(app: &mut App, holly: &Holly, text: &str) {
    match parse_mcp_args(text) {
        Ok(McpCommand::List) => send_mcp_list(app, holly).await,
        Ok(McpCommand::Add { name, spec }) => {
            let _ = holly.send(InMsg::McpAdd { name, config: spec }).await;
        }
        Ok(McpCommand::Remove { name }) => {
            let _ = holly.send(InMsg::McpRemove { name }).await;
        }
        Err(message) => app.record_mcp_error(message),
    }
}

/// Send an `InMsg::McpList` query. Shared by the typed `/mcp list` path above
/// and the command-palette pick
/// (`modal_events::handle_command_palette_event`), which carries no trailing
/// text and so always runs `list`.
pub(super) async fn send_mcp_list(app: &mut App, holly: &Holly) {
    let correlation_id = SessionId::new_uuid().0;
    app.record_pending_mcp_list(correlation_id.clone());
    let _ = holly.send(InMsg::McpList { correlation_id }).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_mcp_args_list() {
        assert_eq!(parse_mcp_args("/mcp list"), Ok(McpCommand::List));
        // Bare `/mcp` defaults to list.
        assert_eq!(parse_mcp_args("/mcp"), Ok(McpCommand::List));
    }

    #[test]
    fn parse_mcp_args_add_stdio() {
        assert_eq!(
            parse_mcp_args("/mcp add myserver -- node server.js --flag"),
            Ok(McpCommand::Add {
                name: "myserver".to_string(),
                spec: McpServerSpec {
                    command: Some("node".to_string()),
                    args: vec!["server.js".to_string(), "--flag".to_string()],
                    env: HashMap::new(),
                    url: None,
                    headers: HashMap::new(),
                    disabled: false,
                },
            })
        );
    }

    #[test]
    fn parse_mcp_args_add_http() {
        assert_eq!(
            parse_mcp_args(
                "/mcp add myserver --url http://localhost:1234 --header Authorization:Bearer123"
            ),
            Ok(McpCommand::Add {
                name: "myserver".to_string(),
                spec: McpServerSpec {
                    command: None,
                    args: Vec::new(),
                    env: HashMap::new(),
                    url: Some("http://localhost:1234".to_string()),
                    headers: HashMap::from([(
                        "Authorization".to_string(),
                        "Bearer123".to_string()
                    )]),
                    disabled: false,
                },
            })
        );
    }

    #[test]
    fn parse_mcp_args_add_http_no_headers() {
        assert_eq!(
            parse_mcp_args("/mcp add myserver --url http://localhost:1234"),
            Ok(McpCommand::Add {
                name: "myserver".to_string(),
                spec: McpServerSpec {
                    command: None,
                    args: Vec::new(),
                    env: HashMap::new(),
                    url: Some("http://localhost:1234".to_string()),
                    headers: HashMap::new(),
                    disabled: false,
                },
            })
        );
    }

    #[test]
    fn parse_mcp_args_remove() {
        assert_eq!(
            parse_mcp_args("/mcp remove myserver"),
            Ok(McpCommand::Remove {
                name: "myserver".to_string()
            })
        );
    }

    #[test]
    fn parse_mcp_args_unknown_subcommand() {
        assert!(parse_mcp_args("/mcp bogus")
            .unwrap_err()
            .contains("unknown"));
    }

    #[test]
    fn parse_mcp_args_malformed() {
        assert!(parse_mcp_args("/mcp remove").is_err());
        assert!(parse_mcp_args("/mcp add").is_err());
        assert!(parse_mcp_args("/mcp add myserver").is_err());
        assert!(parse_mcp_args("/mcp add myserver --").is_err());
        assert!(parse_mcp_args("/mcp add myserver --url").is_err());
        assert!(parse_mcp_args("/mcp add myserver --url http://x --header bad").is_err());
        assert!(parse_mcp_args("/mcp add myserver --url http://x --bogus x:y").is_err());
    }
}
