//! The sandboxed Rhai engine: resource caps, the progress interrupt, and the
//! registered bindings — `http()` and each registered tool's
//! [`ScriptBinding`] (#933) — that cross the bridge to [`super::service`].

use std::any::TypeId;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use rhai::packages::{Package, StandardPackage};
use rhai::{Dynamic, Engine, EvalAltResult, ImmutableString, Position};
use tokio::sync::mpsc::UnboundedSender;

use crate::host_ns::{ScriptArg, ScriptBinding};
use crate::tool_names::HTTP_TOOL;

use super::{
    BindingCall, MAX_ARRAY_SIZE, MAX_CALL_LEVELS, MAX_MAP_SIZE, MAX_OPERATIONS, MAX_STRING_SIZE,
};

/// Apply the sandbox: standard (IO-free) package, resource caps, disabled
/// `eval`, the wall-clock progress interrupt, and print capture. `new_raw()`
/// starts with no module resolver, so `import` cannot reach the filesystem.
/// `on_print` receives each `print` line (no trailing newline) — the blocking
/// path buffers them for the final result, the background path (#637) streams
/// them into its registry entry. `timed_out` is set when the deadline branch
/// fires, so the background path can distinguish a timeout from an ordinary
/// script error without parsing the eval error text.
pub(super) fn configure_engine(
    engine: &mut Engine,
    timeout: Duration,
    start: Instant,
    on_print: impl Fn(&str) + Send + Sync + 'static,
    stop: Arc<AtomicBool>,
    timed_out: Arc<AtomicBool>,
) {
    engine.register_global_module(StandardPackage::new().as_shared_module());
    engine.set_max_operations(MAX_OPERATIONS);
    engine.set_max_call_levels(MAX_CALL_LEVELS);
    engine.set_max_string_size(MAX_STRING_SIZE);
    engine.set_max_array_size(MAX_ARRAY_SIZE);
    engine.set_max_map_size(MAX_MAP_SIZE);
    // No re-entry into the parser from inside a script.
    engine.disable_symbol("eval");
    engine.on_progress(move |_ops| {
        // A `Stop` for the session trips this flag (#167): terminating from the
        // progress callback yields `ErrorTerminated`, which — unlike a thrown
        // binding error — the script cannot `try`/`catch` and keep running.
        if stop.load(Ordering::Relaxed) {
            Some(Dynamic::from("script stopped".to_string()))
        } else if start.elapsed() >= timeout {
            timed_out.store(true, Ordering::Relaxed);
            Some(Dynamic::from(format!(
                "script exceeded the {}s time limit",
                timeout.as_secs()
            )))
        } else {
            None
        }
    });
    engine.on_print(move |text| on_print(text));
}

/// Bind every registered tool that declares a [`ScriptBinding`] (#933) as a
/// plain script function under its own name, one overload per parameter
/// list, plus `http()` (ADR-0227) when the `http` tool is registered. Each
/// closure marshals its positional args into the tool's JSON input and
/// blocks on the bridge for the resolved output. A tool not registered binds
/// nothing, so calling it is an unknown (catchable) function rather than a
/// graded-then-failing binding.
pub(super) fn register_bindings(
    engine: &mut Engine,
    tx: UnboundedSender<BindingCall>,
    tools: &[(String, ScriptBinding)],
    http_enabled: bool,
    start: Instant,
    timeout: Duration,
) {
    if http_enabled {
        register_http(engine, &tx);
    }
    for (tool, binding) in tools {
        for params in binding.overloads {
            let types: Vec<TypeId> = params.iter().map(|(_, arg)| type_of(*arg)).collect();
            let (t, name, binding) = (tx.clone(), tool.clone(), *binding);
            engine.register_raw_fn(
                tool.as_str(),
                types,
                move |_ctx, args: &mut [&mut Dynamic]| -> Result<Dynamic, Box<EvalAltResult>> {
                    let mut input = serde_json::Map::new();
                    for ((key, arg), value) in params.iter().zip(args.iter()) {
                        input.insert(key.to_string(), to_json(*arg, value));
                    }
                    if let Some(key) = binding.budget_key {
                        input.insert(key.into(), remaining_timeout_secs(start, timeout).into());
                    }
                    let input = serde_json::Value::Object(input);
                    if binding.json {
                        call_binding_dynamic(&t, &name, input)
                    } else {
                        call_binding(&t, &name, input).map(Dynamic::from)
                    }
                },
            );
        }
    }
}

/// The Rhai type a parameter of `arg`'s kind is registered with.
fn type_of(arg: ScriptArg) -> TypeId {
    match arg {
        ScriptArg::Str => TypeId::of::<ImmutableString>(),
        ScriptArg::Int => TypeId::of::<rhai::INT>(),
        ScriptArg::Bool => TypeId::of::<bool>(),
        ScriptArg::StrList => TypeId::of::<rhai::Array>(),
    }
}

/// One argument as JSON. Rhai matched the registered [`type_of`] before the
/// call, so each cast holds.
fn to_json(arg: ScriptArg, value: &Dynamic) -> serde_json::Value {
    match arg {
        ScriptArg::Str => value.to_string().into(),
        ScriptArg::Int => value.as_int().unwrap_or_default().into(),
        ScriptArg::Bool => value.as_bool().unwrap_or_default().into(),
        ScriptArg::StrList => value
            .read_lock::<rhai::Array>()
            .map(|items| items.iter().map(|d| d.to_string()).collect::<Vec<_>>())
            .unwrap_or_default()
            .into(),
    }
}

/// `http(method, url)` / `http(method, url, #{headers, body})` (ADR-0227):
/// the body text of the response; a non-2xx status or a refusal throws.
/// A map/array `body` is sent as JSON; any other option key is refused so a
/// typo never silently drops a header.
fn register_http(engine: &mut Engine, tx: &UnboundedSender<BindingCall>) {
    let t = tx.clone();
    engine.register_fn("http", move |method: &str, url: &str| {
        call_binding(
            &t,
            HTTP_TOOL,
            serde_json::json!({ "method": method, "url": url }),
        )
    });
    let t = tx.clone();
    engine.register_fn("http", move |method: &str, url: &str, opts: rhai::Map| {
        let mut input = serde_json::json!({ "method": method, "url": url });
        let mut json_body = false;
        for (key, value) in opts {
            let value = match key.as_str() {
                "headers" => rhai::serde::from_dynamic::<serde_json::Value>(&value)
                    .map_err(|e| runtime_err(&format!("http: headers: {e}")))?,
                "body" if value.is_string() => serde_json::Value::String(value.to_string()),
                "body" => {
                    json_body = true;
                    serde_json::Value::String(
                        rhai::serde::from_dynamic::<serde_json::Value>(&value)
                            .map_err(|e| runtime_err(&format!("http: body: {e}")))?
                            .to_string(),
                    )
                }
                other => {
                    return Err(runtime_err(&format!(
                        "http: unknown option `{other}` (expected headers, body)"
                    )))
                }
            };
            input[key.as_str()] = value;
        }
        // A map/array body was serialized as JSON here, so say so — unless
        // the script chose its own content type.
        if json_body {
            let has_type = input["headers"]
                .as_object()
                .is_some_and(|h| h.keys().any(|k| k.eq_ignore_ascii_case("content-type")));
            if !has_type {
                if !input["headers"].is_object() {
                    input["headers"] = serde_json::json!({});
                }
                input["headers"]["Content-Type"] = "application/json".into();
            }
        }
        call_binding(&t, HTTP_TOOL, input)
    });
}

/// A binding's [`ScriptBinding::budget_key`] value (whole seconds, minimum
/// 1): the script's own remaining wall-clock budget, so an in-flight call
/// cannot outlive the run — rhai's `on_progress` interrupt cannot reach a
/// binding call parked in `blocking_recv`, so the bound has to ride along in
/// the marshalled input instead (#419 fix B).
pub(super) fn remaining_timeout_secs(start: Instant, timeout: Duration) -> u64 {
    timeout.saturating_sub(start.elapsed()).as_secs().max(1)
}

/// Send one binding call across the bridge and block for the reply. A denied or
/// rejected call comes back as `Err`, surfaced to the script as a thrown
/// exception it may `try`/`catch`.
pub(super) fn call_binding(
    tx: &UnboundedSender<BindingCall>,
    tool: &str,
    input: serde_json::Value,
) -> Result<String, Box<EvalAltResult>> {
    super::send_binding(tx, tool, None, input)
}

pub(super) fn runtime_err(msg: &str) -> Box<EvalAltResult> {
    Box::new(EvalAltResult::ErrorRuntime(msg.into(), Position::NONE))
}

/// [`call_binding`] for a [`ScriptBinding::json`] binding (ADR-0206): the
/// tool returns a JSON document, so the reply is parsed into a Rhai value
/// here instead of passing the raw string through. Shares the bridge and
/// permission path exactly; only the return shape differs. A reply that
/// fails to parse is a binding failure (catchable with `try`/`catch`), not a
/// string the script would have to defensively parse itself.
pub(super) fn call_binding_dynamic(
    tx: &UnboundedSender<BindingCall>,
    tool: &str,
    input: serde_json::Value,
) -> Result<Dynamic, Box<EvalAltResult>> {
    let text = call_binding(tx, tool, input)?;
    rhai::serde::to_dynamic(
        &serde_json::from_str::<serde_json::Value>(&text)
            .map_err(|e| runtime_err(&format!("binding `{tool}` returned malformed JSON: {e}")))?,
    )
    .map_err(|e| {
        runtime_err(&format!(
            "binding `{tool}` result not Rhai-representable: {e}"
        ))
    })
}
