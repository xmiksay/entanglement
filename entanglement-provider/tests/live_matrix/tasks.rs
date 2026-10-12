//! The simulated MCP surface (three servers, five tools, deterministic stub
//! results) and the four tasks every cell has to solve.

use serde_json::{json, Value};

/// One tool call the model reached, in dispatch form — whatever shape it was
/// emitted in (native name, `invoke`, `mcp`, or a line in a rhai script).
#[derive(Debug, Clone)]
pub struct Observed {
    pub server: String,
    pub tool: String,
    pub args: Value,
}

pub struct ToolDef {
    pub name: &'static str,
    pub description: &'static str,
    pub schema: fn() -> Value,
    /// The rhai-signature rendering (variants V5/V6).
    pub signature: &'static str,
}

pub struct Server {
    pub name: &'static str,
    pub description: &'static str,
    pub tools: &'static [ToolDef],
}

fn weather_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "city": { "type": "string", "description": "City name" },
            "units": { "type": "string", "enum": ["metric", "imperial"] }
        },
        "required": ["city", "units"]
    })
}

fn event_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "event": {
                "type": "object",
                "properties": {
                    "title": { "type": "string" },
                    "when": {
                        "type": "object",
                        "properties": {
                            "start": { "type": "string", "description": "ISO 8601 local time, no zone" },
                            "end": { "type": "string", "description": "ISO 8601 local time, no zone" }
                        },
                        "required": ["start", "end"]
                    }
                },
                "required": ["title", "when"]
            }
        },
        "required": ["event"]
    })
}

fn search_schema() -> Value {
    json!({
        "type": "object",
        "properties": { "query": { "type": "string" } },
        "required": ["query"]
    })
}

fn read_doc_schema() -> Value {
    json!({
        "type": "object",
        "properties": { "id": { "type": "string", "description": "A doc id from search_docs" } },
        "required": ["id"]
    })
}

pub const SERVERS: &[Server] = &[
    Server {
        name: "weather",
        description: "Current weather data",
        tools: &[ToolDef {
            name: "get_weather",
            description: "Current weather for a city.",
            schema: weather_schema,
            signature: "mcp::weather::get_weather(#{city: string, units: \"metric\" | \"imperial\"}) -> string",
        }],
    },
    Server {
        name: "calendar",
        description: "The user's calendar",
        tools: &[ToolDef {
            name: "create_event",
            description: "Create a calendar event.",
            schema: event_schema,
            signature: "mcp::calendar::create_event(#{event: #{title: string, when: #{start: string, end: string}}}) -> string  // ISO 8601 local times, no zone",
        }],
    },
    Server {
        name: "docs",
        description: "Internal documentation search",
        tools: &[
            ToolDef {
                name: "search_docs",
                description: "Full-text search; returns matching doc ids as JSON.",
                schema: search_schema,
                signature: "mcp::docs::search_docs(#{query: string}) -> string  // JSON: {\"ids\": [..]}",
            },
            ToolDef {
                name: "read_doc",
                description: "The full text of one doc by id.",
                schema: read_doc_schema,
                signature: "mcp::docs::read_doc(#{id: string}) -> string",
            },
        ],
    },
];

pub fn server(name: &str) -> Option<&'static Server> {
    SERVERS.iter().find(|s| s.name == name)
}

/// Deterministic stub result for a call, or an error text for an unknown
/// tool / missing argument (what a real dispatch would decline with).
pub fn stub(server: &str, tool: &str, args: &Value) -> String {
    let str_at = |ptr: &str| {
        args.pointer(ptr)
            .and_then(Value::as_str)
            .map(str::to_string)
    };
    match (server, tool) {
        ("weather", "get_weather") => match str_at("/city") {
            Some(city) if str_at("/units").as_deref() == Some("imperial") => {
                format!("{city}: 64°F, partly cloudy")
            }
            Some(city) => format!("{city}: 18°C, partly cloudy"),
            None => "error: missing required argument `city`".into(),
        },
        ("calendar", "create_event") => match str_at("/event/title") {
            Some(title) => format!("created event ev-7 \"{title}\""),
            None => "error: missing required argument `event.title`".into(),
        },
        ("docs", "search_docs") => match str_at("/query") {
            Some(q) if q.to_lowercase().contains("rate") => r#"{"ids": ["doc-42"]}"#.into(),
            Some(_) => r#"{"ids": []}"#.into(),
            None => "error: missing required argument `query`".into(),
        },
        ("docs", "read_doc") => match str_at("/id").as_deref() {
            Some("doc-42") => "Rate limits: 60 requests per minute per key.\nBurst: 10.".into(),
            Some(id) => format!("error: no doc `{id}`"),
            None => "error: missing required argument `id`".into(),
        },
        _ => format!("error: unknown tool `{server}/{tool}`"),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Task {
    /// One call, two scalar args.
    Scalar,
    /// One call, a nested-object arg.
    Nested,
    /// Two calls where the second needs the first's output.
    Compose,
    /// A follow-up turn after an earlier successful call in history.
    Followup,
}

impl Task {
    pub const ALL: [Task; 4] = [Task::Scalar, Task::Nested, Task::Compose, Task::Followup];

    pub fn name(self) -> &'static str {
        match self {
            Task::Scalar => "scalar",
            Task::Nested => "nested",
            Task::Compose => "compose",
            Task::Followup => "followup",
        }
    }

    pub fn from_name(s: &str) -> Option<Task> {
        Task::ALL.into_iter().find(|t| t.name() == s)
    }

    /// The server whose tools the task needs.
    pub fn server(self) -> &'static str {
        match self {
            Task::Scalar | Task::Followup => "weather",
            Task::Nested => "calendar",
            Task::Compose => "docs",
        }
    }

    pub fn user_prompt(self) -> &'static str {
        match self {
            Task::Scalar => {
                "What's the weather in Prague right now? Report it in Celsius (metric units)."
            }
            Task::Nested => {
                "Create a calendar event titled \"Standup\" on 2026-10-13 from 09:00 to 09:15."
            }
            Task::Compose => "Find the doc about rate limits and tell me its first line.",
            Task::Followup => "And in Brno?",
        }
    }

    /// The call that precedes the follow-up question in history:
    /// `(server, tool, args, tool result, assistant answer)`.
    pub fn prior_call(
        self,
    ) -> Option<(
        &'static str,
        &'static str,
        Value,
        &'static str,
        &'static str,
    )> {
        match self {
            Task::Followup => Some((
                "weather",
                "get_weather",
                json!({ "city": "Prague", "units": "metric" }),
                "Prague: 18°C, partly cloudy",
                "It's 18°C and partly cloudy in Prague.",
            )),
            _ => None,
        }
    }

    /// `(reached, args_ok)`: whether the expected tool(s) were called at all,
    /// and whether they were called with the right arguments (and, for the
    /// composition, whether the final answer carries the read value).
    pub fn check(self, observed: &[Observed], final_text: &str) -> (bool, bool) {
        let find = |server: &str, tool: &str| {
            observed
                .iter()
                .find(|o| o.server == server && o.tool == tool)
        };
        let has = |v: &Value, ptr: &str, needle: &str| {
            v.pointer(ptr)
                .and_then(Value::as_str)
                .is_some_and(|s| s.to_lowercase().contains(needle))
        };
        match self {
            Task::Scalar => {
                let Some(o) = find("weather", "get_weather") else {
                    return (false, false);
                };
                let ok = (has(&o.args, "/city", "prague") || has(&o.args, "/city", "praha"))
                    && has(&o.args, "/units", "metric");
                (true, ok)
            }
            Task::Followup => {
                let Some(o) = find("weather", "get_weather") else {
                    return (false, false);
                };
                (true, has(&o.args, "/city", "brno"))
            }
            Task::Nested => {
                let Some(o) = find("calendar", "create_event") else {
                    return (false, false);
                };
                let ok = has(&o.args, "/event/title", "standup")
                    && has(&o.args, "/event/when/start", "2026-10-13t09:00")
                    && has(&o.args, "/event/when/end", "09:15");
                (true, ok)
            }
            Task::Compose => {
                let search = find("docs", "search_docs");
                let read = find("docs", "read_doc");
                let reached = search.is_some() && read.is_some();
                let ok = reached
                    && search.is_some_and(|o| has(&o.args, "/query", "rate"))
                    && read.is_some_and(|o| has(&o.args, "/id", "doc-42"))
                    && final_text.contains("60");
                (reached, ok)
            }
        }
    }
}
