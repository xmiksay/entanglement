//! The sandboxed Rhai engine: resource caps, the progress interrupt, and the
//! registered file/exec/data bindings that cross the bridge to [`super::service`].

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use rhai::packages::{Package, StandardPackage};
use rhai::{Dynamic, Engine, EvalAltResult, Position};
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::oneshot;

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

/// Bind the host quintet plus permission-gated process-exec as script
/// functions (ADR-0115, amending ADR-0046). Each closure marshals its args to
/// the tool's JSON shape and blocks on the bridge for the resolved output.
/// The argv-exec host tool is bound under the script-callable name `exec`,
/// **not** `call`: `call` is a hard-reserved Rhai keyword
/// (`KEYWORD_FN_PTR_CALL`) the interpreter special-cases in
/// `make_function_call` to always mean "invoke this `FnPtr`" — registering a
/// same-named function is silently shadowed (its first argument gets coerced
/// as a function pointer instead of dispatched to ours). The tool name
/// dispatched to the bridge, its permission grade, and its `BINDING_TOOLS`/
/// capability membership all stay the literal `call` (matching the
/// model-facing `call` tool) — only the script-facing identifier differs.
/// `exec`/`bash` additionally stamp a `timeout` derived from this run's own
/// remaining wall-clock budget (`start`/`timeout`, #419 fix B): rhai's
/// `on_progress` interrupt can't reach into a binding call blocked on
/// `blocking_recv`, so the exec tool's own (much longer, up to 600s) timeout
/// would otherwise stand alone as the only bound on an in-flight child.
/// `bash` is registered only when the host `bash` tool itself is in the
/// registry the script delegates through — off, `bash(...)` is an
/// unknown (catchable) script function rather than a graded-then-failing
/// binding. Each also gains a `workdir` overload (#480, ADR-0129:
/// `exec(command, args, workdir)`/`bash(command, workdir)`) that marshals the
/// value into the delegated tool's own `workdir` field — the same field a
/// `tool{pattern}` workdir-scoped rule (#425) and the escape-root gate (#446)
/// both extract, so a script gets identical scoping to a direct tool call.
pub(super) fn register_bindings(
    engine: &mut Engine,
    tx: UnboundedSender<BindingCall>,
    bash_enabled: bool,
    start: Instant,
    timeout: Duration,
) {
    let t = tx.clone();
    engine.register_fn("read", move |path: &str| {
        call_binding(&t, "read", serde_json::json!({ "path": path }))
    });
    let t = tx.clone();
    engine.register_fn("read", move |path: &str, offset: i64, limit: i64| {
        call_binding(
            &t,
            "read",
            serde_json::json!({ "path": path, "offset": offset, "limit": limit }),
        )
    });
    let t = tx.clone();
    // Raw counterpart of `read` with no line-number prefix — source for
    // parse_json/parse_yaml, since `read`'s "{lineno}: {line}" format isn't
    // valid JSON/YAML. Graded and masked as an alias of `read`
    // (`BindingPolicy::decide`), not a distinct permission surface.
    engine.register_fn("read_raw", move |path: &str| {
        call_binding(&t, "read_raw", serde_json::json!({ "path": path }))
    });
    let t = tx.clone();
    engine.register_fn("glob", move |pattern: &str| {
        call_binding(&t, "glob", serde_json::json!({ "pattern": pattern }))
    });
    let t = tx.clone();
    engine.register_fn("grep", move |pattern: &str| {
        call_binding(&t, "grep", serde_json::json!({ "pattern": pattern }))
    });
    let t = tx.clone();
    engine.register_fn("grep", move |pattern: &str, path: &str| {
        call_binding(
            &t,
            "grep",
            serde_json::json!({ "pattern": pattern, "path": path }),
        )
    });
    // Structured search bindings (ADR-0206): same permission grading and
    // same underlying walk/scan as `glob`/`grep` (dispatched to the
    // script-facing variant tools), but the result arrives as a parsed Rhai
    // value — `#{files: […]}` / `#{matches: […], notices: […]}` — not a
    // newline-joined string. The prose tools' text is model-facing; a script
    // consuming it had to hand-parse, and Rhai's string indexing (chars, not
    // lines) turned every mistake into a wrong-but-not-error result.
    let t = tx.clone();
    engine.register_fn("glob_json", move |pattern: &str| {
        call_binding_dynamic(&t, "glob_json", serde_json::json!({ "pattern": pattern }))
    });
    let t = tx.clone();
    engine.register_fn("glob_json", move |pattern: &str, exclude: rhai::Array| {
        call_binding_dynamic(
            &t,
            "glob_json",
            serde_json::json!({
                "pattern": pattern,
                "exclude": exclude.iter().map(|d| d.to_string()).collect::<Vec<_>>(),
            }),
        )
    });
    let t = tx.clone();
    engine.register_fn("grep_json", move |pattern: &str| {
        call_binding_dynamic(&t, "grep_json", serde_json::json!({ "pattern": pattern }))
    });
    let t = tx.clone();
    engine.register_fn("grep_json", move |pattern: &str, path: &str| {
        call_binding_dynamic(
            &t,
            "grep_json",
            serde_json::json!({ "pattern": pattern, "path": path }),
        )
    });
    let t = tx.clone();
    engine.register_fn("edit", move |path: &str, old: &str, new: &str| {
        call_binding(
            &t,
            "edit",
            serde_json::json!({ "path": path, "oldString": old, "newString": new }),
        )
    });
    let t = tx.clone();
    engine.register_fn(
        "edit",
        move |path: &str, old: &str, new: &str, replace_all: bool| {
            call_binding(
                &t,
                "edit",
                serde_json::json!({
                    "path": path, "oldString": old, "newString": new, "replaceAll": replace_all
                }),
            )
        },
    );
    let t = tx.clone();
    engine.register_fn("write", move |path: &str, content: &str| {
        call_binding(
            &t,
            "write",
            serde_json::json!({ "path": path, "content": content }),
        )
    });
    let t = tx.clone();
    engine.register_fn("exec", move |command: &str| {
        call_binding(
            &t,
            "call",
            serde_json::json!({
                "command": command,
                "args": Vec::<String>::new(),
                "timeout": remaining_timeout_secs(start, timeout),
            }),
        )
    });
    let t = tx.clone();
    engine.register_fn("exec", move |command: &str, args: rhai::Array| {
        let args = call_args_to_strings(args)?;
        call_binding(
            &t,
            "call",
            serde_json::json!({ "command": command, "args": args, "timeout": remaining_timeout_secs(start, timeout) }),
        )
    });
    let t = tx.clone();
    // #480: an explicit `workdir` so a workdir-scoped permission rule
    // (`call{pattern}`, #425) and the escape-root gate (#446) both see it —
    // neither fires for a binding call that never marshals the field.
    engine.register_fn(
        "exec",
        move |command: &str, args: rhai::Array, workdir: &str| {
            let args = call_args_to_strings(args)?;
            call_binding(
                &t,
                "call",
                serde_json::json!({
                    "command": command,
                    "args": args,
                    "workdir": workdir,
                    "timeout": remaining_timeout_secs(start, timeout),
                }),
            )
        },
    );
    if bash_enabled {
        let t = tx.clone();
        engine.register_fn("bash", move |command: &str| {
            call_binding(
                &t,
                "bash",
                serde_json::json!({
                    "command": command,
                    "timeout": remaining_timeout_secs(start, timeout),
                }),
            )
        });
        let t = tx;
        engine.register_fn("bash", move |command: &str, workdir: &str| {
            call_binding(
                &t,
                "bash",
                serde_json::json!({
                    "command": command,
                    "workdir": workdir,
                    "timeout": remaining_timeout_secs(start, timeout),
                }),
            )
        });
    }
}

/// Convert a Rhai `args` array (`exec(command, args)`) to argv strings — a
/// non-string element throws a catchable error naming the offending type
/// rather than silently stringifying it (Rhai's `to_string()` on, say, a map
/// would produce nonsense argv).
pub(super) fn call_args_to_strings(args: rhai::Array) -> Result<Vec<String>, Box<EvalAltResult>> {
    args.into_iter()
        .map(|v| {
            v.into_string()
                .map_err(|ty| runtime_err(&format!("call: args must be strings, got {ty}")))
        })
        .collect()
}

/// Derive a `call`/`bash` binding's `timeout` (whole seconds, minimum 1) from
/// the script's own remaining wall-clock budget, so an in-flight child cannot
/// outlive the run — rhai's `on_progress` interrupt cannot reach a binding
/// call parked in `blocking_recv`, so the bound has to ride along in the
/// marshalled input instead (#419 fix B).
pub(super) fn remaining_timeout_secs(start: Instant, timeout: Duration) -> u64 {
    timeout.saturating_sub(start.elapsed()).as_secs().max(1)
}

/// Send one binding call across the bridge and block for the reply. A denied or
/// rejected call comes back as `Err`, surfaced to the script as a thrown
/// exception it may `try`/`catch`.
pub(super) fn call_binding(
    tx: &UnboundedSender<BindingCall>,
    tool: &'static str,
    input: serde_json::Value,
) -> Result<String, Box<EvalAltResult>> {
    let (reply, wait) = oneshot::channel();
    tx.send(BindingCall {
        tool,
        input: input.to_string(),
        reply,
    })
    .map_err(|_| runtime_err("rhai host bridge closed"))?;
    match wait.blocking_recv() {
        Ok(Ok(out)) => Ok(out),
        Ok(Err(msg)) => Err(runtime_err(&msg)),
        Err(_) => Err(runtime_err("rhai host bridge dropped")),
    }
}

pub(super) fn runtime_err(msg: &str) -> Box<EvalAltResult> {
    Box::new(EvalAltResult::ErrorRuntime(msg.into(), Position::NONE))
}

/// [`call_binding`] for the structured search variants (ADR-0206): the tool
/// returns a JSON document, so the reply is parsed into a Rhai value here —
/// `glob_json` yields `#{files: […], notices: […]}`, `grep_json` a
/// `#{matches: […], notices: […]}` map — instead of passing the raw string
/// through. Shares the bridge and permission path exactly (`graded_name`
/// handles the policy identity); only the return shape differs. A reply that
/// fails to parse is a binding failure (catchable with `try`/`catch`), not a
/// string the script would have to defensively parse itself.
pub(super) fn call_binding_dynamic(
    tx: &UnboundedSender<BindingCall>,
    tool: &'static str,
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
