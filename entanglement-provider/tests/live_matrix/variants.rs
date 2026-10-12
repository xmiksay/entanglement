//! The six call-encoding variants and the two discovery shapes: what the
//! `tools` array holds, what the system prompt says about the convention,
//! what a server's tool listing looks like, and how an earlier call is
//! written into history.

use serde_json::{json, Value};

use super::tasks::{server, Server, ToolDef, SERVERS};

/// How the model finds a server's tools (#818 D1/D2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Discovery {
    /// Today's generic pair: `explore(filter)` + `describe(names)`.
    Explore,
    /// The #818 shape: `list_mcp()` + `mcp_tools(server)`.
    ListTools,
}

impl Discovery {
    pub fn name(self) -> &'static str {
        match self {
            Discovery::Explore => "explore",
            Discovery::ListTools => "list",
        }
    }

    fn tools(self) -> Vec<Value> {
        match self {
            Discovery::Explore => vec![
                function(
                    "explore",
                    "List what is available: \"mcp\" for MCP servers, \"mcp:<server>\" for one server's tools.",
                    json!({ "type": "object", "properties": { "filter": { "type": "string" } }, "required": ["filter"] }),
                ),
                function(
                    "describe",
                    "Load the full schemas of tools by name.",
                    json!({ "type": "object", "properties": { "names": { "type": "array", "items": { "type": "string" } } }, "required": ["names"] }),
                ),
            ],
            Discovery::ListTools => vec![
                function(
                    "list_mcp",
                    "List the available MCP servers with a one-line description each.",
                    json!({ "type": "object", "properties": {} }),
                ),
                function(
                    "mcp_tools",
                    "Show every tool of one MCP server with its schema and how to call it.",
                    json!({ "type": "object", "properties": { "server": { "type": "string" } }, "required": ["server"] }),
                ),
            ],
        }
    }

    fn prompt_note(self) -> &'static str {
        match self {
            Discovery::Explore => {
                "explore(\"mcp\") lists them, explore(\"mcp:<server>\") shows a server's tools, \
                 describe([names]) loads one tool's schema."
            }
            Discovery::ListTools => {
                "list_mcp lists them and mcp_tools(server) shows a server's tools."
            }
        }
    }

    /// The assistant call that lists servers, as seeded history.
    pub fn list_call(self, id: &str) -> Value {
        match self {
            Discovery::Explore => tool_call(id, "explore", json!({ "filter": "mcp" })),
            Discovery::ListTools => tool_call(id, "list_mcp", json!({})),
        }
    }

    /// The assistant call that shows one server's tools, as seeded history.
    pub fn tools_call(self, id: &str, srv: &str) -> Value {
        match self {
            Discovery::Explore => {
                tool_call(id, "explore", json!({ "filter": format!("mcp:{srv}") }))
            }
            Discovery::ListTools => tool_call(id, "mcp_tools", json!({ "server": srv })),
        }
    }

    /// The server-list reply.
    pub fn servers_reply(self) -> String {
        let mut out = String::from("MCP servers:\n");
        for s in SERVERS {
            out.push_str(&format!("- {}: {}\n", s.name, s.description));
        }
        out.push_str(match self {
            Discovery::Explore => "Use explore(\"mcp:<server>\") for a server's tools.",
            Discovery::ListTools => "Use mcp_tools(server) for a server's tools.",
        });
        out
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Variant {
    /// V1 — ADR-0196 `append`: the server's tools are in the array natively.
    Append,
    /// V2 — `native_first` without the fallback: schema delivered, array untouched.
    Undeclared,
    /// V3 — ADR-0204 `invoke {name, args}` envelope.
    Invoke,
    /// V4 — `mcp(server, tool, args)` envelope (#818).
    Mcp,
    /// V5 — rhai script; the listing carries rhai signatures only.
    RhaiSig,
    /// V6 — rhai script; the listing carries JSON schemas + a call example.
    RhaiSchema,
}

impl Variant {
    pub const ALL: [Variant; 6] = [
        Variant::Append,
        Variant::Undeclared,
        Variant::Invoke,
        Variant::Mcp,
        Variant::RhaiSig,
        Variant::RhaiSchema,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Variant::Append => "append",
            Variant::Undeclared => "undeclared",
            Variant::Invoke => "invoke",
            Variant::Mcp => "mcp",
            Variant::RhaiSig => "rhai_sig",
            Variant::RhaiSchema => "rhai_schema",
        }
    }

    pub fn from_name(s: &str) -> Option<Variant> {
        Variant::ALL.into_iter().find(|v| v.name() == s)
    }

    /// The system prompt: the shared agent line, the discovery note, and
    /// this variant's calling convention.
    pub fn system_prompt(self, discovery: Discovery) -> String {
        let convention = match self {
            Variant::Append | Variant::Undeclared => {
                "Call a loaded MCP tool directly by its full name (mcp__<server>__<tool>)."
            }
            Variant::Invoke => {
                "Call a loaded MCP tool through invoke {\"name\": \"mcp__<server>__<tool>\", \"args\": {...}}; \
                 it cannot be called directly."
            }
            Variant::Mcp => {
                "Call an MCP tool through mcp {\"server\": \"<server>\", \"tool\": \"<tool>\", \"args\": {...}}; \
                 it cannot be called directly."
            }
            Variant::RhaiSig | Variant::RhaiSchema => {
                "Call MCP tools from a rhai script: mcp::<server>::<tool>(#{...}) returns the tool's \
                 result as a string; the script's last expression is returned to you. One script may \
                 chain several calls."
            }
        };
        format!(
            "You are a coding agent. Use tools to answer; do not guess tool results. Beyond the \
             tools above, MCP servers are available: {} {convention}",
            discovery.prompt_note()
        )
    }

    /// The advertised `tools` array for a task on `task_server`.
    pub fn tools(self, task_server: &str, discovery: Discovery) -> Value {
        let mut tools = vec![function(
            "read",
            "Read a file from the workspace.",
            json!({ "type": "object", "properties": { "path": { "type": "string" } }, "required": ["path"] }),
        )];
        tools.extend(discovery.tools());
        match self {
            Variant::Append => {
                if let Some(s) = server(task_server) {
                    for t in s.tools {
                        tools.push(function(&native_name(s.name, t.name), t.description, (t.schema)()));
                    }
                }
            }
            Variant::Undeclared => {}
            Variant::Invoke => tools.push(function(
                "invoke",
                "Call a tool you have seen listed but cannot call directly: name is the tool's full name, args its arguments object.",
                json!({ "type": "object", "properties": { "name": { "type": "string" }, "args": { "type": "object" } }, "required": ["name"] }),
            )),
            Variant::Mcp => tools.push(function(
                "mcp",
                "Call a tool on an MCP server: server and tool are names from the server listing, args the tool's arguments object.",
                json!({ "type": "object", "properties": { "server": { "type": "string" }, "tool": { "type": "string" }, "args": { "type": "object" } }, "required": ["server", "tool"] }),
            )),
            Variant::RhaiSig | Variant::RhaiSchema => tools.push(function(
                "rhai",
                "Run a Rhai script (Rust-like: let, fn, if, for; maps are #{key: value}; strings in double quotes). \
                 MCP tools are callable as namespaced functions mcp::<server>::<tool>(#{...}) and return a string. \
                 The value of the script's last expression is returned; print(x) output is included.",
                json!({ "type": "object", "properties": { "script": { "type": "string" } }, "required": ["script"] }),
            )),
        }
        Value::Array(tools)
    }

    /// One server's tool listing, in this variant's convention.
    pub fn server_detail(self, s: &Server) -> String {
        let mut out = format!("MCP server `{}` ({}) — tools:\n", s.name, s.description);
        for t in s.tools {
            out.push_str(&self.tool_entry(s, t));
        }
        out
    }

    fn tool_entry(self, s: &Server, t: &ToolDef) -> String {
        let native = native_name(s.name, t.name);
        match self {
            Variant::Append => format!(
                "- {native}: {} (now loaded; call it directly)\n{}\n",
                t.description,
                (t.schema)()
            ),
            Variant::Undeclared => format!(
                "- {native}: {} — call it directly by this name.\n{}\n",
                t.description,
                (t.schema)()
            ),
            Variant::Invoke => format!(
                "- {native}: {}\n{}\ncall as: invoke {{\"name\": \"{native}\", \"args\": {{...}}}}\n",
                t.description,
                (t.schema)()
            ),
            Variant::Mcp => format!(
                "- {}: {}\n{}\ncall as: mcp {{\"server\": \"{}\", \"tool\": \"{}\", \"args\": {{...}}}}\n",
                t.name,
                t.description,
                (t.schema)(),
                s.name,
                t.name
            ),
            Variant::RhaiSig => format!("- {}: {}\n  {}\n", t.name, t.description, t.signature),
            Variant::RhaiSchema => format!(
                "- {}: {}\n{}\ncall as (rhai): mcp::{}::{}(#{{...}})\n",
                t.name,
                t.description,
                (t.schema)(),
                s.name,
                t.name
            ),
        }
    }

    /// The `describe(names)` reply (Explore shape only).
    pub fn describe_reply(self, names: &[String]) -> String {
        let mut out = String::new();
        for n in names {
            let found = SERVERS.iter().find_map(|s| {
                s.tools
                    .iter()
                    .find(|t| n == t.name || *n == native_name(s.name, t.name))
                    .map(|t| (s, t))
            });
            match found {
                Some((s, t)) => out.push_str(&self.tool_entry(s, t)),
                None => out.push_str(&format!("{n}: unknown tool\n")),
            }
        }
        out
    }

    /// An earlier successful call written into history **as this variant's
    /// model would have emitted it** (ADR-0204: history is never rewritten).
    pub fn prior_call(self, id: &str, server_name: &str, tool: &str, args: &Value) -> Value {
        let (name, arguments) = match self {
            Variant::Append | Variant::Undeclared => (native_name(server_name, tool), args.clone()),
            Variant::Invoke => (
                "invoke".to_string(),
                json!({ "name": native_name(server_name, tool), "args": args }),
            ),
            Variant::Mcp => (
                "mcp".to_string(),
                json!({ "server": server_name, "tool": tool, "args": args }),
            ),
            Variant::RhaiSig | Variant::RhaiSchema => (
                "rhai".to_string(),
                json!({ "script": format!("mcp::{server_name}::{tool}({})", rhai_map(args)) }),
            ),
        };
        tool_call(id, &name, arguments)
    }
}

pub fn native_name(server: &str, tool: &str) -> String {
    format!("mcp__{server}__{tool}")
}

fn function(name: &str, description: &str, parameters: Value) -> Value {
    json!({
        "type": "function",
        "function": { "name": name, "description": description, "parameters": parameters },
    })
}

pub fn tool_call(id: &str, name: &str, args: Value) -> Value {
    json!({
        "role": "assistant",
        "content": "",
        "tool_calls": [{
            "id": id,
            "type": "function",
            "function": { "name": name, "arguments": args.to_string() },
        }],
    })
}

/// A JSON object rendered as a rhai map literal (`#{city: "Prague"}`).
fn rhai_map(v: &Value) -> String {
    match v {
        Value::Object(m) => {
            let fields: Vec<String> = m
                .iter()
                .map(|(k, v)| format!("{k}: {}", rhai_map(v)))
                .collect();
            format!("#{{{}}}", fields.join(", "))
        }
        Value::Array(a) => format!(
            "[{}]",
            a.iter().map(rhai_map).collect::<Vec<_>>().join(", ")
        ),
        other => other.to_string(),
    }
}
