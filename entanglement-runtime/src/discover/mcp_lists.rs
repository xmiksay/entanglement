//! `list_mcp` and `mcp_tools` (ADR-0226): the MCP half of discovery.
//!
//! `list_mcp` never connects anything — it lists every registered server with
//! its state and, when known, its tool names: live for a connected server,
//! else the last-seen cache. `mcp_tools(name)` is a read of one server's tool
//! catalog — every tool with its JSON schema and one `mcp {...}` call line,
//! the MCP counterpart of `load_skill` — plus the tool's rhai name
//! (`mcp::<server>::<tool>`, ADR-0227). MCP has no offline catalog, so the
//! first read of a server in the process connects it (an implementation
//! detail; the call through `mcp` is what gets graded).
//!
//! A per-user scoped session (ADR-0188) resolves both through its scope: it
//! sees the scope's servers only, never a global one.

use std::sync::Mutex;

use entanglement_core::{SessionId, ToolSpec};
use serde::Deserialize;
use serde_json::json;

use crate::mcp::available::{self, LazyConnectOutcome};
use crate::mcp::tool::{namespaced_tool_name, owning_server};
use crate::mcp::{ActiveServers, AvailableMcp, McpScopes, ServerRow};
use crate::tool_advertising::DeliveredSchemas;
use crate::tools::SharedRegistry;

/// Everything the MCP discovery pair reads, borrowed from the ladder.
pub struct McpSources<'a> {
    pub avail: &'a AvailableMcp,
    pub active: &'a ActiveServers,
    pub registry: &'a SharedRegistry,
    pub scopes: Option<&'a McpScopes>,
    pub http: Option<&'a entanglement_core::HttpClient>,
    pub delivered: &'a Mutex<DeliveredSchemas>,
}

const CALL_HINT: &str =
    "Call a tool by calling the `mcp` tool with {\"server\": \"<server>\", \"tool\": \"<tool>\", \"args\": {...}}; \
     mcp_tools {\"name\": \"<server>\"} shows a server's tools with their schemas; a rhai script calls \
     mcp::<server>::<tool>(#{...}) instead.";

/// `list_mcp`: one line per registered server.
pub fn list_mcp(src: &McpSources<'_>, session: &SessionId) -> String {
    match src.scopes.and_then(|s| s.rows(session)) {
        // Scoped tool lists never come from the shared cache (one target can
        // expose different tools per credential).
        Some(rows) => render_list(&rows, |_| None),
        None => render_list(&crate::mcp::roster(src.avail, src.active), |row| {
            src.avail.tool_cache().lookup(&row.name, &row.target)
        }),
    }
}

fn render_list(
    rows: &[ServerRow],
    last_seen: impl Fn(&ServerRow) -> Option<Vec<String>>,
) -> String {
    if rows.is_empty() {
        return "No MCP servers are configured.".to_string();
    }
    let mut out = format!("MCP servers — {CALL_HINT}");
    for row in rows {
        let description = row
            .description
            .as_deref()
            .map(|d| format!(" — {d}"))
            .unwrap_or_default();
        let state = if row.connected {
            "connected".to_string()
        } else if row.auth_required {
            format!("auth required — the user can run /mcp connect {}", row.name)
        } else {
            "available".to_string()
        };
        out.push_str(&format!("\n  {}{description} [{state}]", row.name));
        let prefix = namespaced_tool_name(&row.name, "");
        let short = |names: &[String]| -> Vec<String> {
            names
                .iter()
                .map(|n| n.strip_prefix(&prefix).unwrap_or(n).to_string())
                .collect()
        };
        if row.connected {
            out.push_str(&format!("\n    tools: {}", short(&row.tools).join(", ")));
        } else if let Some(seen) = last_seen(row).filter(|t| !t.is_empty()) {
            out.push_str(&format!("\n    tools (last seen): {}", seen.join(", ")));
        }
    }
    out
}

#[derive(Deserialize)]
struct NameInput {
    name: String,
}

/// `mcp_tools(name)`: `(reply, is_error)`. Connects the server first if this
/// process has not yet.
pub async fn mcp_tools(src: &McpSources<'_>, session: &SessionId, input: &str) -> (String, bool) {
    let server = match serde_json::from_str::<NameInput>(input) {
        Ok(NameInput { name }) if !name.is_empty() => name,
        _ => {
            return (
                "mcp_tools expects {\"name\": \"<server>\"} — a server name from list_mcp"
                    .to_string(),
                true,
            )
        }
    };
    let prefix = namespaced_tool_name(&server, "");
    let scoped = src
        .scopes
        .and_then(|s| s.rows(session).map(|rows| (s, rows)));
    let (specs, description, servers) = match scoped {
        Some((scopes, rows)) => {
            let names: Vec<String> = rows.iter().map(|r| r.name.clone()).collect();
            let Some(row) = rows.into_iter().find(|r| r.name == server) else {
                return (unknown_server(&server), true);
            };
            let base = src
                .registry
                .read()
                .expect("tool registry lock poisoned")
                .clone();
            match scopes
                .overlay_registry_for_call(session, base, &prefix)
                .await
            {
                Ok(reg) => (
                    owned_specs(reg.specs(), &names, &server),
                    row.description,
                    names,
                ),
                Err(msg) => return (msg, true),
            }
        }
        None => {
            let outcome =
                available::try_lazy_connect(src.avail, &server, src.registry, src.active, src.http)
                    .await;
            match outcome {
                LazyConnectOutcome::Connected => {}
                LazyConnectOutcome::Unknown => return (unknown_server(&server), true),
                LazyConnectOutcome::Disabled => {
                    return (available::disabled_decline(&server), true)
                }
                LazyConnectOutcome::Failed(msg) => return (msg, true),
            }
            let specs = src
                .registry
                .read()
                .expect("tool registry lock poisoned")
                .specs();
            let description = src.avail.get(&server).and_then(|s| s.config.description);
            let names = src.avail.available_names();
            // The same server set the script namespaces are planned over.
            let servers = crate::mcp::roster(src.avail, src.active)
                .into_iter()
                .map(|r| r.name)
                .collect();
            (owned_specs(specs, &names, &server), description, servers)
        }
    };
    let mut delivered = src
        .delivered
        .lock()
        .expect("delivered-schema mutex poisoned");
    for spec in &specs {
        delivered.mark(session, &spec.name);
    }
    (
        render_tools(&server, description.as_deref(), &specs, &prefix, &servers),
        false,
    )
}

/// The specs whose owning server among `servers` is `server` — by the shared
/// longest-prefix rule, so `gh`'s read never lists (or marks delivered) a
/// `gh__enterprise` server's tools.
fn owned_specs(specs: Vec<ToolSpec>, servers: &[String], server: &str) -> Vec<ToolSpec> {
    let mut owned: Vec<ToolSpec> = specs
        .into_iter()
        .filter(|s| owning_server(servers.iter().map(String::as_str), &s.name) == Some(server))
        .collect();
    owned.sort_by(|a, b| a.name.cmp(&b.name));
    owned
}

fn unknown_server(server: &str) -> String {
    available::unknown_server_decline(server)
}

/// One server's catalog: per tool its description, its schema, and a call
/// line with a minimal valid `args` — the shape #826 measured at 100 % on
/// every GLM model.
fn render_tools(
    server: &str,
    description: Option<&str>,
    specs: &[ToolSpec],
    prefix: &str,
    servers: &[String],
) -> String {
    let about = description.map(|d| format!(" ({d})")).unwrap_or_default();
    if specs.is_empty() {
        return format!("MCP server `{server}`{about} exposes no tools.");
    }
    let mut out = format!("MCP server `{server}`{about} — tools:");
    let tools: Vec<(String, String)> = specs
        .iter()
        .map(|s| {
            let short = s.name.strip_prefix(prefix).unwrap_or(&s.name);
            (server.to_string(), short.to_string())
        })
        .collect();
    let names = RhaiNames::plan(servers, &tools);
    for spec in specs {
        let tool = spec.name.strip_prefix(prefix).unwrap_or(&spec.name);
        let example = crate::arg_validate::minimal_example(&spec.schema);
        // Spelled out rather than `json!` so the key order reads server,
        // tool, args — serde_json would sort them.
        let call = format!(
            "mcp {{\"server\": {}, \"tool\": {}, \"args\": {example}}}",
            json!(server),
            json!(tool),
        );
        // Signature first, schema second (#826: signature-only listings were
        // the more accurate shape), with the mapped rhai name (ADR-0227).
        out.push_str(&format!(
            "\n\n- {tool}: {}{}\n  call as: {call}\n  schema: {}",
            spec.description,
            names.line(server, tool, &example),
            spec.schema
        ));
    }
    out
}

/// The `in rhai:` lines, from the one naming pass script namespaces are
/// registered with — a tool that lost a collision is listed without a rhai
/// name rather than under one that reaches another tool. Absent from a build
/// without the script engine, where nothing could call it.
#[cfg(feature = "rhai")]
struct RhaiNames(std::collections::HashMap<(String, String), crate::rhai_ident::RhaiName>);

#[cfg(feature = "rhai")]
impl RhaiNames {
    fn plan(servers: &[String], tools: &[(String, String)]) -> Self {
        // The listed server may be missing from `servers` (a race with a
        // removal); it still claims its own segment.
        let mut servers = servers.to_vec();
        servers.extend(tools.first().map(|(s, _)| s.clone()));
        Self(crate::rhai_ident::plan_mcp(&servers, tools))
    }

    fn line(&self, server: &str, tool: &str, example: &serde_json::Value) -> String {
        match self.0.get(&(server.to_string(), tool.to_string())) {
            Some(crate::rhai_ident::RhaiName::Path(path)) => format!(
                "\n  in rhai: {path}({})",
                crate::rhai_ident::rhai_literal(example)
            ),
            Some(other) => format!("\n  in rhai: {}", other.listing("mcp(...)")),
            None => String::new(),
        }
    }
}

#[cfg(not(feature = "rhai"))]
struct RhaiNames;

#[cfg(not(feature = "rhai"))]
impl RhaiNames {
    fn plan(_servers: &[String], _tools: &[(String, String)]) -> Self {
        RhaiNames
    }

    fn line(&self, _server: &str, _tool: &str, _example: &serde_json::Value) -> String {
        String::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(name: &str, connected: bool, tools: &[&str]) -> ServerRow {
        ServerRow {
            name: name.into(),
            description: Some(format!("{name} things")),
            transport: "http".into(),
            target: format!("https://{name}/mcp"),
            connected,
            tools: tools.iter().map(|t| t.to_string()).collect(),
            oauth: false,
            auth_required: false,
        }
    }

    #[test]
    fn list_shows_state_and_live_or_last_seen_tool_names() {
        let mut locked = row("remote", false, &[]);
        locked.auth_required = true;
        let rows = [
            row("gh", true, &["mcp__gh__search", "mcp__gh__read"]),
            row("kb", false, &[]),
            locked,
            row("new", false, &[]),
        ];
        let out = render_list(&rows, |r| (r.name == "kb").then(|| vec!["lookup".into()]));
        assert!(
            out.starts_with("MCP servers — Call a tool by calling the `mcp` tool with"),
            "{out}"
        );
        assert!(
            out.contains("gh — gh things [connected]\n    tools: search, read"),
            "{out}"
        );
        assert!(
            out.contains("kb — kb things [available]\n    tools (last seen): lookup"),
            "{out}"
        );
        assert!(
            out.contains(
                "remote — remote things [auth required — the user can run /mcp connect remote]"
            ),
            "{out}"
        );
        assert!(out.contains("new — new things [available]"), "{out}");
        assert!(
            !out.contains("new — new things [available]\n    tools"),
            "{out}"
        );
        assert_eq!(render_list(&[], |_| None), "No MCP servers are configured.");
    }

    #[test]
    fn a_servers_read_never_lists_a_nested_named_servers_tools() {
        let specs = ["mcp__gh__search", "mcp__gh__enterprise__audit", "read"]
            .map(|n| ToolSpec::new(n, "d"))
            .to_vec();
        let servers = vec!["gh".to_string(), "gh__enterprise".to_string()];
        let names = |v: Vec<ToolSpec>| v.into_iter().map(|s| s.name).collect::<Vec<_>>();
        assert_eq!(
            names(owned_specs(specs.clone(), &servers, "gh")),
            ["mcp__gh__search"]
        );
        assert_eq!(
            names(owned_specs(specs, &servers, "gh__enterprise")),
            ["mcp__gh__enterprise__audit"]
        );
    }

    #[test]
    fn tools_render_schema_and_a_valid_mcp_call_line() {
        let spec = ToolSpec::with_schema(
            "mcp__gh__search",
            "search issues",
            json!({"type": "object", "properties": {"q": {"type": "string"}}, "required": ["q"]}),
        );
        let out = render_tools("gh", Some("GitHub"), &[spec], "mcp__gh__", &[]);
        assert!(
            out.starts_with("MCP server `gh` (GitHub) — tools:"),
            "{out}"
        );
        #[cfg(feature = "rhai")]
        assert!(
            out.contains("- search: search issues\n  in rhai: mcp::gh::search(#{q: \"example\"})"),
            "{out}"
        );
        assert!(
            out.contains(
                r#"call as: mcp {"server": "gh", "tool": "search", "args": {"q":"example"}}"#
            ),
            "{out}"
        );
        assert!(out.contains("\n  schema: {"), "schema comes last: {out}");
        // A reserved-word tool shows its mapped rhai name (ADR-0227).
        #[cfg(feature = "rhai")]
        {
            let call = ToolSpec::with_schema("mcp__my-gh__call", "d", json!({"type": "object"}));
            let out = render_tools("my-gh", None, &[call], "mcp__my-gh__", &[]);
            assert!(out.contains("in rhai: mcp::my_gh::call_(#{})"), "{out}");
            // A collision lists the loser without a rhai name, naming the
            // winner — the same plan the script namespaces use.
            let specs = ["delete.item", "delete-item"].map(|t| {
                ToolSpec::with_schema(format!("mcp__s__{t}"), "d", json!({"type": "object"}))
            });
            let out = render_tools("s", None, &specs, "mcp__s__", &["s".to_string()]);
            assert!(out.contains("in rhai: mcp::s::delete_item(#{})"), "{out}");
            assert!(
                out.contains(
                    "in rhai: no rhai name (collides with `mcp__s__delete-item`) — use mcp(...)"
                ),
                "{out}"
            );
        }
        assert_eq!(
            render_tools("gh", None, &[], "mcp__gh__", &[]),
            "MCP server `gh` exposes no tools."
        );
    }
}
