//! One cell: seed the history, then run a bounded agent loop against the
//! model with stub tool results until it answers in text.

use serde_json::{json, Value};

use super::probe::{cached_share, completion_tokens, Probe};
use super::rhai_exec::run_script;
use super::tasks::{server, stub, Observed, Task};
use super::variants::{Discovery, Variant};

const MAX_ROUNDS: u32 = 5;

#[derive(Debug, Clone, Copy)]
pub struct Cell {
    pub variant: Variant,
    pub task: Task,
    pub discovery: Discovery,
    /// Seeded: the history already holds the server list and the task
    /// server's tool listing. Unseeded: the model starts from the bare task
    /// and has to discover on its own.
    pub seeded: bool,
}

#[derive(Debug, Default, Clone)]
pub struct Outcome {
    pub reached: bool,
    pub args_ok: bool,
    /// Discovery calls the model made on its own (`explore`/`describe` or
    /// `list_mcp`/`mcp_tools`).
    pub discover_calls: u32,
    pub rounds: u32,
    pub completion_tokens: u64,
    /// Cached share of the *first* request's prompt.
    pub cached_first: f64,
    pub error: Option<String>,
    pub final_text: String,
}

fn seed(cell: Cell) -> Vec<Value> {
    let Cell {
        variant,
        task,
        discovery,
        seeded,
    } = cell;
    let srv = server(task.server()).expect("task server exists");
    let mut h = vec![json!({ "role": "system", "content": variant.system_prompt(discovery) })];
    let first_prompt = match task {
        Task::Followup => Task::Scalar.user_prompt(),
        t => t.user_prompt(),
    };
    h.push(json!({ "role": "user", "content": first_prompt }));
    if seeded {
        h.push(discovery.list_call("call_list_1"));
        h.push(tool_result("call_list_1", discovery.servers_reply()));
        h.push(discovery.tools_call("call_tools_1", srv.name));
        h.push(tool_result("call_tools_1", variant.server_detail(srv)));
    }
    if let Some((s, t, args, result, answer)) = task.prior_call() {
        h.push(variant.prior_call("call_prior_1", s, t, &args));
        h.push(tool_result("call_prior_1", result.to_string()));
        h.push(json!({ "role": "assistant", "content": answer }));
        h.push(json!({ "role": "user", "content": task.user_prompt() }));
    }
    h
}

pub async fn run_cell(probe: &Probe, model: &str, thinking: bool, cell: Cell) -> Outcome {
    let mut history = seed(cell);
    let tools = cell.variant.tools(cell.task.server(), cell.discovery);
    let mut out = Outcome::default();
    let mut observed: Vec<Observed> = Vec::new();

    for round in 1..=MAX_ROUNDS {
        out.rounds = round;
        let resp = match probe.chat(model, thinking, &history, &tools, 2048).await {
            Ok(r) => r,
            Err(e) => {
                out.error = Some(e);
                break;
            }
        };
        if round == 1 {
            out.cached_first = cached_share(&resp);
        }
        out.completion_tokens += completion_tokens(&resp);
        let mut message = resp["choices"][0]["message"].clone();
        if message.is_null() {
            out.error = Some(format!("no message in response: {resp}"));
            break;
        }
        if message["content"].is_null() {
            message["content"] = json!("");
        }
        let calls = message["tool_calls"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        history.push(message.clone());
        if calls.is_empty() {
            out.final_text = message["content"].as_str().unwrap_or("").to_string();
            break;
        }
        for call in &calls {
            let id = call["id"].as_str().unwrap_or("call").to_string();
            let name = call
                .pointer("/function/name")
                .and_then(Value::as_str)
                .unwrap_or("");
            let args = as_object(&call["function"]["arguments"]);
            let reply = handle(cell, name, &args, &mut observed, &mut out);
            history.push(tool_result(&id, reply));
        }
    }
    let (reached, ok) = cell.task.check(&observed, &out.final_text);
    out.reached = reached;
    out.args_ok = ok;
    out
}

/// Resolve one emitted call: discovery tools answer from the fixtures, every
/// reachable encoding of an MCP call is unwrapped into `observed` and answered
/// by the stub, anything else is declined the way dispatch would.
fn handle(
    cell: Cell,
    name: &str,
    args: &Value,
    observed: &mut Vec<Observed>,
    out: &mut Outcome,
) -> String {
    let variant = cell.variant;
    let s = |p: &str| args.pointer(p).and_then(Value::as_str).map(str::to_string);
    match name {
        "explore" => {
            out.discover_calls += 1;
            match s("/filter").as_deref() {
                Some(f) if f.starts_with("mcp:") => match server(&f[4..]) {
                    Some(srv) => variant.server_detail(srv),
                    None => format!("unknown MCP server `{}`", &f[4..]),
                },
                _ => cell.discovery.servers_reply(),
            }
        }
        "describe" => {
            out.discover_calls += 1;
            let names: Vec<String> = args
                .pointer("/names")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default();
            variant.describe_reply(&names)
        }
        "list_mcp" => {
            out.discover_calls += 1;
            cell.discovery.servers_reply()
        }
        "mcp_tools" => {
            out.discover_calls += 1;
            match s("/server").and_then(|n| server(&n)) {
                Some(srv) => variant.server_detail(srv),
                None => format!("unknown MCP server {:?}", s("/server")),
            }
        }
        "read" => "error: file not found".to_string(),
        "invoke" => match s("/name") {
            Some(n) => dispatch_native(
                &n,
                &as_object(args.get("args").unwrap_or(&Value::Null)),
                observed,
            ),
            None => "error: invoke needs `name`".to_string(),
        },
        "mcp" => match (s("/server"), s("/tool")) {
            (Some(srv), Some(tool)) => {
                let a = as_object(args.get("args").unwrap_or(&Value::Null));
                record(&srv, &tool, a, observed)
            }
            _ => "error: mcp needs `server` and `tool`".to_string(),
        },
        "rhai" => match s("/script") {
            Some(script) => run_script(&script, observed).unwrap_or_else(|e| e),
            None => "error: rhai needs `script`".to_string(),
        },
        n if n.starts_with("mcp__") => dispatch_native(n, args, observed),
        other => format!("error: unknown tool `{other}`"),
    }
}

fn dispatch_native(full: &str, args: &Value, observed: &mut Vec<Observed>) -> String {
    let mut parts = full.splitn(3, "__");
    match (parts.next(), parts.next(), parts.next()) {
        (Some("mcp"), Some(srv), Some(tool)) => record(srv, tool, args.clone(), observed),
        _ => format!("error: unknown tool `{full}`"),
    }
}

fn record(srv: &str, tool: &str, args: Value, observed: &mut Vec<Observed>) -> String {
    let args = if args.is_object() { args } else { json!({}) };
    let out = stub(srv, tool, &args);
    observed.push(Observed {
        server: srv.into(),
        tool: tool.into(),
        args,
    });
    out
}

/// Arguments arrive as a JSON string on the wire, but a model may also nest
/// an encoded string where an object was asked for — accept both.
fn as_object(v: &Value) -> Value {
    match v {
        Value::String(s) => serde_json::from_str(s).unwrap_or(Value::Null),
        other => other.clone(),
    }
}

fn tool_result(id: &str, content: String) -> Value {
    json!({ "role": "tool", "tool_call_id": id, "content": content })
}
