use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rhai::{Dynamic, Engine, EvalAltResult};
use tokio::sync::mpsc;

use super::output::{format_output, serialize_return};
use super::{configure_engine, data, register_bindings, BindingCall};

/// Build a sandboxed engine with no bindings (the bridge senders are dropped
/// immediately, so any binding call errors) — enough to exercise the sandbox
/// and resource limits directly.
fn sandbox_engine(timeout: Duration) -> Engine {
    let mut engine = Engine::new_raw();
    configure_engine(
        &mut engine,
        timeout,
        Instant::now(),
        |_text| {},
        Arc::new(AtomicBool::new(false)),
        Arc::new(AtomicBool::new(false)),
    );
    engine
}

/// A sandboxed engine with the pure JSON/YAML functions registered (no host
/// bindings — those need the async bridge, irrelevant to these tests).
fn data_engine(timeout: Duration) -> Engine {
    let mut engine = sandbox_engine(timeout);
    data::register_data_functions(&mut engine);
    engine
}

#[test]
fn arithmetic_and_return_serialize_to_json() {
    let engine = sandbox_engine(Duration::from_secs(5));
    let v = engine.eval::<Dynamic>("let x = 2 + 3; x * 4").unwrap();
    assert_eq!(serialize_return(&v), "20");
}

#[test]
fn array_return_serializes_as_json() {
    let engine = sandbox_engine(Duration::from_secs(5));
    let v = engine.eval::<Dynamic>("[1, 2, 3]").unwrap();
    assert_eq!(serialize_return(&v), "[1,2,3]");
}

#[test]
fn unit_return_renders_explicitly() {
    let engine = sandbox_engine(Duration::from_secs(5));
    let v = engine.eval::<Dynamic>("let _x = 1;").unwrap();
    assert_eq!(serialize_return(&v), "()");
}

#[test]
fn import_is_refused_no_module_resolver() {
    let engine = sandbox_engine(Duration::from_secs(5));
    let err = engine
        .eval::<Dynamic>(r#"import "std" as s; 1"#)
        .unwrap_err();
    // No filesystem module resolver is installed, so `import` cannot escape.
    let msg = err.to_string().to_lowercase();
    assert!(
        msg.contains("module") || msg.contains("import") || msg.contains("resolver"),
        "unexpected error: {err}"
    );
}

#[test]
fn no_ambient_file_functions() {
    let engine = sandbox_engine(Duration::from_secs(5));
    // Nothing like `open_file`/`read_file` exists — a bare call is unknown.
    let err = engine
        .eval::<Dynamic>(r#"open_file("/etc/passwd")"#)
        .unwrap_err();
    assert!(
        err.to_string().to_lowercase().contains("function"),
        "expected unknown-function error, got: {err}"
    );
}

#[test]
fn operation_limit_terminates_infinite_loop() {
    let engine = sandbox_engine(Duration::from_secs(30));
    // Never returns on its own; the operation cap must kill it.
    let err = engine
        .eval::<Dynamic>("let i = 0; loop { i += 1; }")
        .unwrap_err();
    assert!(
        matches!(*err, EvalAltResult::ErrorTooManyOperations(_)),
        "expected too-many-operations, got: {err}"
    );
}

#[test]
fn timeout_terminates_a_slow_loop() {
    // A sub-second budget: the progress callback interrupts the loop.
    let engine = sandbox_engine(Duration::from_millis(50));
    let err = engine
        .eval::<Dynamic>("let i = 0; loop { i += 1; }")
        .unwrap_err();
    assert!(
        matches!(*err, EvalAltResult::ErrorTerminated(_, _)),
        "expected terminated-by-progress, got: {err}"
    );
}

#[test]
fn stop_flag_terminates_and_cannot_be_caught() {
    // #167: a `Stop` trips the flag; the engine terminates via the progress
    // callback, and a wrapping `try`/`catch` cannot swallow it and continue.
    let stop = Arc::new(AtomicBool::new(true));
    let mut engine = Engine::new_raw();
    configure_engine(
        &mut engine,
        Duration::from_secs(30),
        Instant::now(),
        |_text| {},
        stop,
        Arc::new(AtomicBool::new(false)),
    );
    let err = engine
        .eval::<Dynamic>(r#"try { let i = 0; loop { i += 1; } } catch(e) { 0 }"#)
        .unwrap_err();
    assert!(
        matches!(*err, EvalAltResult::ErrorTerminated(_, _)),
        "expected terminated-by-stop, got: {err}"
    );
}

#[test]
fn string_size_cap_is_enforced() {
    let engine = sandbox_engine(Duration::from_secs(30));
    // Doubling a string blows past MAX_STRING_SIZE well before it OOMs.
    let err = engine
        .eval::<Dynamic>(r#"let s = "x"; loop { s += s; }"#)
        .unwrap_err();
    assert!(
        matches!(*err, EvalAltResult::ErrorDataTooLarge(_, _))
            || matches!(*err, EvalAltResult::ErrorTooManyOperations(_))
            || matches!(*err, EvalAltResult::ErrorTerminated(_, _)),
        "expected a size/operation/timeout bound, got: {err}"
    );
}

#[test]
fn print_output_is_captured() {
    let prints = Arc::new(Mutex::new(String::new()));
    let sink = prints.clone();
    let mut engine = Engine::new_raw();
    configure_engine(
        &mut engine,
        Duration::from_secs(5),
        Instant::now(),
        move |text| {
            if let Ok(mut buf) = sink.lock() {
                buf.push_str(text);
                buf.push('\n');
            }
        },
        Arc::new(AtomicBool::new(false)),
        Arc::new(AtomicBool::new(false)),
    );
    let _ = engine
        .eval::<Dynamic>(r#"print("hello"); print("world");"#)
        .unwrap();
    let captured = prints.lock().unwrap().clone();
    assert_eq!(captured, "hello\nworld\n");
}

/// #622: oversized output keeps a head + tail slice (not head-only), so a
/// return value/error at the end survives truncation.
#[test]
fn format_output_oversized_keeps_head_and_tail() {
    use crate::host::MAX_OUTPUT_BYTES;
    let prints = format!("HEAD_MARKER{}", "x".repeat(MAX_OUTPUT_BYTES * 2));
    let engine = sandbox_engine(Duration::from_secs(5));
    let v = engine.eval::<Dynamic>(r#""TAIL_MARKER""#).unwrap();
    let (out, is_error) = format_output(prints, Ok(Ok(v)));
    assert!(out.starts_with("HEAD_MARKER"), "head lost: {out}");
    assert!(out.contains("TAIL_MARKER"), "tail lost: {out}");
    assert!(out.contains("omitted from the middle"), "got: {out}");
    assert!(!is_error);
}

#[test]
fn format_output_combines_prints_and_return() {
    let engine = sandbox_engine(Duration::from_secs(5));
    let v = engine.eval::<Dynamic>("40 + 2").unwrap();
    let (out, is_error) = format_output("printed line\n".to_string(), Ok(Ok(v)));
    assert_eq!(out, "printed line\n=> 42");
    assert!(!is_error);
}

#[test]
fn format_output_flags_a_script_error() {
    let engine = sandbox_engine(Duration::from_secs(5));
    let err = engine.eval::<Dynamic>("throw \"boom\"").unwrap_err();
    let (out, is_error) = format_output(String::new(), Ok(Err(err)));
    assert!(out.contains("rhai error"), "{out}");
    assert!(is_error);
}

/// #480: `exec`/`bash`'s new three/two-arg overloads marshal `workdir`
/// into the dispatched tool's own JSON input; the workdir-less overloads
/// leave the field out entirely (not `null`), matching a direct tool
/// call's shape.
#[test]
fn exec_and_bash_bindings_marshal_workdir_when_given() {
    let (tx, mut rx) = mpsc::unbounded_channel::<BindingCall>();
    let mut engine = Engine::new_raw();
    register_bindings(
        &mut engine,
        tx,
        true,
        true,
        Instant::now(),
        Duration::from_secs(5),
    );

    let captured: Arc<Mutex<Vec<(String, String)>>> = Arc::new(Mutex::new(Vec::new()));
    let captured2 = captured.clone();
    let responder = std::thread::spawn(move || {
        while let Some(call) = rx.blocking_recv() {
            captured2
                .lock()
                .unwrap()
                .push((call.tool, call.input.clone()));
            let _ = call.reply.send(Ok(String::new()));
        }
    });

    let _ = engine
        .eval::<Dynamic>(
            r#"
            exec("echo", ["hi"]);
            exec("echo", ["hi"], "/tmp/x");
            bash("echo hi");
            bash("echo hi", "/tmp/y");
            "#,
        )
        .unwrap();
    // Dropping the engine drops every closure's `tx` clone, closing the
    // channel so the responder thread's loop ends.
    drop(engine);
    responder.join().unwrap();

    let calls = captured.lock().unwrap();
    assert_eq!(calls.len(), 4);
    assert!(calls
        .iter()
        .all(|(tool, _)| tool == "call" || tool == "bash"));

    let parse = |i: usize| -> serde_json::Value { serde_json::from_str(&calls[i].1).unwrap() };
    assert!(
        parse(0).get("workdir").is_none(),
        "exec(cmd, args) carries no workdir field"
    );
    assert_eq!(parse(1)["workdir"], "/tmp/x");
    assert!(
        parse(2).get("workdir").is_none(),
        "bash(cmd) carries no workdir field"
    );
    assert_eq!(parse(3)["workdir"], "/tmp/y");
}

#[test]
fn parse_json_round_trips_object_and_array() {
    let engine = data_engine(Duration::from_secs(5));
    let v = engine
        .eval::<Dynamic>(r#"let v = parse_json("{\"a\":1,\"b\":[1,2,3]}"); v["b"][1]"#)
        .unwrap();
    assert_eq!(v.as_int().unwrap(), 2);
}

#[test]
fn parse_json_null_becomes_unit() {
    let engine = data_engine(Duration::from_secs(5));
    let v = engine.eval::<Dynamic>(r#"parse_json("null")"#).unwrap();
    assert!(v.is_unit());
}

#[test]
fn parse_json_throws_on_invalid_input_and_is_catchable() {
    let engine = data_engine(Duration::from_secs(5));
    // A bare `try`/`catch` always evaluates to `()` in Rhai regardless of
    // which branch ran — assign into an outer variable to observe the
    // catch branch actually executed.
    let v = engine
        .eval::<Dynamic>(
            r#"let result = ""; try { parse_json("{not json"); } catch(e) { result = "caught"; } result"#,
        )
        .unwrap();
    assert_eq!(v.into_string().unwrap(), "caught");
}

#[test]
fn parse_json_out_of_i64_range_number_widens_to_float() {
    let engine = data_engine(Duration::from_secs(5));
    // u64::MAX exceeds Rhai's i64 INT range. Rhai's own serde bridge
    // (ser.rs `serialize_u64`) falls back i64 -> decimal (off by default in
    // this build) -> float rather than erroring — same as JS's
    // `JSON.parse`. Not a throw: verified empirically, documented on
    // `register_data_functions` rather than assumed.
    let v = engine
        .eval::<Dynamic>(r#"parse_json("18446744073709551615")"#)
        .unwrap();
    assert!(
        v.is_float(),
        "expected the oversized integer to widen to FLOAT, got: {v:?}"
    );
}

#[test]
fn parse_json_callable_as_method_via_ufcs() {
    let engine = data_engine(Duration::from_secs(5));
    let v = engine.eval::<Dynamic>(r#""[1,2]".parse_json()"#).unwrap();
    assert_eq!(serialize_return(&v), "[1,2]");
}

#[test]
fn to_json_stringifies_a_rhai_value() {
    let engine = data_engine(Duration::from_secs(5));
    let v = engine
        .eval::<Dynamic>(r#"#{a: 1, b: "x"}.to_json()"#)
        .unwrap();
    // Map key order isn't guaranteed — assert on parsed structure, not text.
    let parsed: serde_json::Value = serde_json::from_str(&v.into_string().unwrap()).unwrap();
    assert_eq!(parsed["a"], 1);
    assert_eq!(parsed["b"], "x");
}

#[test]
fn json_round_trip_is_stable() {
    let engine = data_engine(Duration::from_secs(5));
    let v = engine
        .eval::<Dynamic>(r#"parse_json(to_json(parse_json("[1,2,3]")))"#)
        .unwrap();
    assert_eq!(serialize_return(&v), "[1,2,3]");
}

#[test]
fn parse_yaml_round_trips_and_method_call_works() {
    let engine = data_engine(Duration::from_secs(5));
    let v = engine
        .eval::<Dynamic>(
            r#"let m = "a: 1\nb: two\n".parse_yaml(); m["a"].to_string() + "," + m["b"]"#,
        )
        .unwrap();
    assert_eq!(v.into_string().unwrap(), "1,two");
}

#[test]
fn parse_yaml_throws_on_invalid_input() {
    let engine = data_engine(Duration::from_secs(5));
    // Unbalanced flow-mapping brace — invalid YAML.
    let result = engine.eval::<Dynamic>(r#"parse_yaml("a: [1, 2")"#);
    assert!(result.is_err(), "expected invalid YAML to throw");
}

/// ADR-0227: `http(method, url, #{headers, body})` marshals into the `http`
/// tool's input; an unknown option is a catchable error, not a silent drop.
#[test]
fn http_binding_marshals_method_url_and_options() {
    let (tx, mut rx) = mpsc::unbounded_channel::<BindingCall>();
    let mut engine = Engine::new_raw();
    register_bindings(
        &mut engine,
        tx,
        false,
        true,
        Instant::now(),
        Duration::from_secs(5),
    );
    let responder = std::thread::spawn(move || {
        let mut seen = Vec::new();
        while let Some(call) = rx.blocking_recv() {
            seen.push((call.tool.clone(), call.input.clone()));
            let _ = call.reply.send(Ok("ok".to_string()));
        }
        seen
    });
    let out = engine
        .eval::<Dynamic>(
            r#"
            http("GET", "https://a.io/x");
            http("post", "https://a.io/y", #{headers: #{"X-A": "1"}, body: #{q: 1}});
            let r = "not caught";
            try { http("GET", "https://a.io", #{bogus: 1}); } catch (e) { r = "caught"; }
            r
            "#,
        )
        .unwrap();
    assert_eq!(out.into_string().unwrap(), "caught");
    drop(engine);
    let seen = responder.join().unwrap();
    assert_eq!(seen.len(), 2, "the bad option never reached the bridge");
    assert!(seen.iter().all(|(tool, _)| tool == "http"));
    let second: serde_json::Value = serde_json::from_str(&seen[1].1).unwrap();
    assert_eq!(second["method"], "post");
    assert_eq!(second["headers"]["X-A"], "1");
    assert_eq!(second["body"], r#"{"q":1}"#, "a map body is sent as JSON");
    assert_eq!(second["headers"]["Content-Type"], "application/json");
}

/// A script's own content type wins over the JSON default; a string body
/// gets none added.
#[test]
fn http_binding_keeps_a_script_content_type() {
    let (tx, mut rx) = mpsc::unbounded_channel::<BindingCall>();
    let mut engine = Engine::new_raw();
    register_bindings(
        &mut engine,
        tx,
        false,
        true,
        Instant::now(),
        Duration::from_secs(5),
    );
    let responder = std::thread::spawn(move || {
        let mut seen = Vec::new();
        while let Some(call) = rx.blocking_recv() {
            seen.push(call.input.clone());
            let _ = call.reply.send(Ok(String::new()));
        }
        seen
    });
    let _ = engine
        .eval::<Dynamic>(
            r#"http("POST", "https://a.io", #{body: [1], headers: #{"content-type": "text/x"}});
               http("POST", "https://a.io", #{body: "raw"});"#,
        )
        .unwrap();
    drop(engine);
    let seen = responder.join().unwrap();
    let first: serde_json::Value = serde_json::from_str(&seen[0]).unwrap();
    assert_eq!(
        first["headers"],
        serde_json::json!({"content-type": "text/x"})
    );
    let second: serde_json::Value = serde_json::from_str(&seen[1]).unwrap();
    assert!(second.get("headers").is_none(), "{second}");
}
