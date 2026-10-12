//! Runs a model-authored rhai script against stub `mcp::<server>::<tool>`
//! modules, recording every call it makes — the most faithful check that a
//! script would actually reach the tool (syntax, namespace, map literal).

use std::sync::{Arc, Mutex};

use rhai::{Dynamic, Engine, EvalAltResult, Map, Module};
use serde_json::Value;

use super::tasks::{stub, Observed, SERVERS};

type Shared = Arc<Mutex<Vec<Observed>>>;

/// `Ok(result text)` as the tool result the model would read, or the script
/// error text. `observed` receives every stub call in order either way.
pub fn run_script(script: &str, observed: &mut Vec<Observed>) -> Result<String, String> {
    let calls: Shared = Arc::new(Mutex::new(Vec::new()));
    let prints = Arc::new(Mutex::new(String::new()));

    let mut engine = Engine::new();
    engine.set_max_operations(500_000);
    let sink = prints.clone();
    engine.on_print(move |s| {
        if let Ok(mut p) = sink.lock() {
            p.push_str(s);
            p.push('\n');
        }
    });

    for server in SERVERS {
        let mut module = Module::new();
        for tool in server.tools {
            let (srv, name) = (server.name, tool.name);
            let c = calls.clone();
            module.set_native_fn(name, move |args: Map| {
                call(&c, srv, name, map_to_json(args))
            });
            let c = calls.clone();
            // A script may pass the arguments as a JSON string instead of a map.
            module.set_native_fn(name, move |args: &str| {
                let v = serde_json::from_str(args).unwrap_or(Value::String(args.to_string()));
                call(&c, srv, name, v)
            });
            let c = calls.clone();
            module.set_native_fn(name, move || {
                call(&c, srv, name, Value::Object(Default::default()))
            });
        }
        engine.register_static_module(format!("mcp::{}", server.name), module.into());
    }

    let result = engine.eval::<Dynamic>(script);
    observed.extend(calls.lock().map(|c| c.clone()).unwrap_or_default());
    let printed = prints.lock().map(|p| p.clone()).unwrap_or_default();
    match result {
        Ok(v) => {
            let value = if v.is_unit() {
                String::new()
            } else {
                format!("=> {v}")
            };
            Ok(format!("{printed}{value}").trim().to_string())
        }
        Err(e) => Err(format!("{printed}script error: {e}").trim().to_string()),
    }
}

fn call(
    calls: &Shared,
    server: &'static str,
    tool: &'static str,
    args: Value,
) -> Result<String, Box<EvalAltResult>> {
    let out = stub(server, tool, &args);
    if let Ok(mut c) = calls.lock() {
        c.push(Observed {
            server: server.into(),
            tool: tool.into(),
            args,
        });
    }
    Ok(out)
}

fn map_to_json(map: Map) -> Value {
    rhai::serde::from_dynamic::<Value>(&Dynamic::from_map(map)).unwrap_or(Value::Null)
}
