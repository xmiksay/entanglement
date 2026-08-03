//! `/mcp` subcommand parsing + wire dispatch (#373): kept in its own module
//! rather than folded into `tui/commands.rs` (name/description/dispatch) or
//! `tui/event_loop.rs` (the `Enter`-key send helpers) — both already past the
//! 400-line cap — mirroring how `CommandPalette` was split out of
//! `commands.rs` once it crossed the cap (#376). Issue 2: the `list`/`add`/
//! `remove` subcommand grammar is now clap-derived
//! ([`crate::tui::command_args::parse_mcp_via_clap`]); this module maps the
//! parsed `McpSub` onto the [`McpCommand`] + `McpServerSpec` the wire path
//! needs.

use std::collections::HashMap;

use entanglement_core::{Holly, IdKind, InMsg, McpAuthAction, McpServerSpec};

use super::app::App;
use super::command_args::{parse_mcp_via_clap, McpSub};

/// One parsed `/mcp` subcommand: `List` needs no further wire data; `Add`/
/// `Remove` carry exactly what `InMsg::McpAdd`/`McpRemove` need.
#[derive(Debug, Clone, PartialEq)]
pub enum McpCommand {
    List,
    Add {
        name: String,
        spec: McpServerSpec,
    },
    Remove {
        name: String,
    },
    /// OAuth management (ADR-0153) — `connect`/`check`/`disconnect`, all three
    /// carried by the single trusted-only `InMsg::McpAuth`.
    Auth {
        name: String,
        action: McpAuthAction,
    },
}

const MCP_USAGE: &str = "usage: /mcp list | /mcp add <name> -- <command> [args...] | \
     /mcp add <name> --url <url> [--header KEY:VALUE]... | /mcp remove <name> | \
     /mcp connect|check|disconnect <name>";

/// Parse `/mcp <subcommand> ...` — Issue 2 delegates to clap
/// ([`parse_mcp_via_clap`]) and maps the parsed [`McpSub`] onto [`McpCommand`],
/// building the [`McpServerSpec`] for `add`. A bare `/mcp` (no subcommand)
/// defaults to `list`, so both a plain Enter and a command-palette pick (which
/// carries no trailing text) do something useful. Clap's "unrecognized
/// subcommand" wording is normalized to the friendlier "unknown subcommand"
/// the pre-Issue-2 parser used.
pub fn parse_mcp_args(text: &str) -> Result<McpCommand, String> {
    let sub = parse_mcp_via_clap(text)
        .map_err(|e| e.replace("unrecognized subcommand", "unknown subcommand"))?;
    match sub {
        None | Some(McpSub::List) => Ok(McpCommand::List),
        Some(McpSub::Remove { name }) => Ok(McpCommand::Remove { name }),
        Some(McpSub::Add(add)) => build_mcp_add_spec(add),
        Some(McpSub::Connect { name }) => Ok(McpCommand::Auth {
            name,
            action: McpAuthAction::Connect,
        }),
        Some(McpSub::Check { name }) => Ok(McpCommand::Auth {
            name,
            action: McpAuthAction::Check,
        }),
        Some(McpSub::Disconnect { name }) => Ok(McpCommand::Auth {
            name,
            action: McpAuthAction::Disconnect,
        }),
    }
}

/// Build the [`McpServerSpec`] from clap's parsed `McpAddArgs`: the `command`
/// vec carries the stdio command + args (the tokens after `--`); `url` selects
/// the streamable-HTTP transport with optional repeated `--header KEY:VALUE`s
/// — but clap's standard `Vec<String>` can't collect a repeatable `--header`
/// alongside a `trailing_var_arg` command, so headers are re-extracted from the
/// raw tokens here (mirroring the pre-Issue-2 behavior exactly).
fn build_mcp_add_spec(add: super::command_args::McpAddArgs) -> Result<McpCommand, String> {
    let name = add.name;
    if let Some(url) = add.url {
        // HTTP form: re-scan the original tokens for `--header KEY:VALUE` pairs.
        // clap captured `name` + `url`; headers ride the same raw text.
        let headers = parse_headers_from_remaining(&add.command)?;
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
    } else if add.command.is_empty() {
        Err(MCP_USAGE.to_string())
    } else {
        let command = add.command[0].clone();
        let args = add.command[1..].to_vec();
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
}

/// Extract `KEY:VALUE` header pairs from the leftover tokens clap didn't
/// consume (the `--header` flag isn't a clap field on `McpAddArgs`; it rides
/// the same `command` trailing-var-arg vec when `--url` is used). Each
/// `--header` must be followed by a `KEY:VALUE` token; a missing value or a
/// value without a `:` separator is a friendly error, matching the pre-Issue-2
/// parser exactly.
fn parse_headers_from_remaining(tokens: &[String]) -> Result<HashMap<String, String>, String> {
    let mut headers = HashMap::new();
    let mut i = 0;
    while i < tokens.len() {
        if tokens[i] == "--header" {
            let kv = tokens
                .get(i + 1)
                .ok_or_else(|| "usage: --header KEY:VALUE".to_string())?;
            let (k, v) = kv
                .split_once(':')
                .ok_or_else(|| format!("invalid --header value: {kv} (expected KEY:VALUE)"))?;
            headers.insert(k.to_string(), v.to_string());
            i += 2;
        } else {
            // An unexpected token after `--url <url>` — only `--header` is valid.
            return Err(format!("unknown /mcp add flag: {}", tokens[i]));
        }
    }
    Ok(headers)
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
        Ok(McpCommand::Auth { name, action }) => send_mcp_auth(app, holly, name, action).await,
        Err(message) => app.record_mcp_error(message),
    }
}

/// Send an `InMsg::McpAuth` (ADR-0153) over the **privileged** `Holly::send` —
/// the variant is wire-refused, and the TUI is in-process/trusted exactly like
/// the existing `/mcp add`/`remove` path.
///
/// A `connect` renders an immediate "opening browser…" line: discovery and
/// dynamic client registration happen before the authorize URL exists, so
/// several seconds can pass before the first `McpAuthChanged` arrives, and
/// silence would read as a hang. Every later update comes from the event.
pub(super) async fn send_mcp_auth(
    app: &mut App,
    holly: &Holly,
    name: String,
    action: McpAuthAction,
) {
    if action == McpAuthAction::Connect {
        app.record_mcp_status(format!("mcp: authorizing `{name}` — opening your browser…"));
    }
    let _ = holly.send(InMsg::McpAuth { name, action }).await;
}

/// Send an `InMsg::McpList` query. Shared by the typed `/mcp list` path above
/// and the command-palette pick
/// (`modal_events::handle_command_palette_event`), which carries no trailing
/// text and so always runs `list`.
pub(super) async fn send_mcp_list(app: &mut App, holly: &Holly) {
    // A correlation id, not a session id (ADR-0164) — minted through the
    // engine's configured generator via `Holly::next_id`.
    let correlation_id = holly.next_id(IdKind::Request);
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
    fn parse_mcp_args_oauth_subcommands() {
        for (text, action) in [
            ("/mcp connect remote", McpAuthAction::Connect),
            ("/mcp check remote", McpAuthAction::Check),
            ("/mcp disconnect remote", McpAuthAction::Disconnect),
        ] {
            assert_eq!(
                parse_mcp_args(text),
                Ok(McpCommand::Auth {
                    name: "remote".to_string(),
                    action,
                }),
                "parsing {text}"
            );
        }
    }

    #[test]
    fn parse_mcp_args_oauth_requires_a_server_name() {
        assert!(parse_mcp_args("/mcp connect").is_err());
        assert!(parse_mcp_args("/mcp check").is_err());
        assert!(parse_mcp_args("/mcp disconnect").is_err());
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
