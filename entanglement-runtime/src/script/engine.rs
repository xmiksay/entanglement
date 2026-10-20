//! One engine run, shared by the blocking and the background path: build
//! the sandboxed engine with its bindings and namespaces on the blocking
//! pool, and service its bridge through the dispatch seam on this task until
//! the engine finishes.

use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::{Duration, Instant};

use rhai::serde::to_dynamic;
use rhai::{Dynamic, Engine, EvalAltResult, Scope};
use tokio::sync::mpsc;

use crate::tool_names::HTTP_TOOL;
use crate::tool_runner::{Dispatcher, Posture, ScriptCall};

use super::{configure_engine, data, namespaces, register_bindings, service, BindingCall, Launch};

pub(super) struct RunSpec {
    pub script: String,
    /// Bound as the constant map `args` (ADR-0228).
    pub args: serde_json::Map<String, serde_json::Value>,
    pub timeout: Duration,
    pub stop: Arc<AtomicBool>,
    /// Set when the deadline fires — the background path reports it.
    pub timed_out: Arc<AtomicBool>,
    /// A background run (#637): no session state transitions.
    pub detached: bool,
    /// How nested calls are cleared (ADR-0228).
    pub posture: Posture,
}

pub(super) type EvalResult = Result<Result<Dynamic, Box<EvalAltResult>>, tokio::task::JoinError>;

/// Run `spec.script` to completion. Returns the engine's result and whether a
/// `Stop` unwound a nested approval.
pub(super) async fn run_engine(
    dispatcher: &mut Dispatcher,
    launch: &Launch,
    request_id: &str,
    spec: RunSpec,
    on_print: impl Fn(&str) + Send + Sync + 'static,
) -> (EvalResult, bool) {
    let RunSpec {
        script,
        args,
        timeout,
        stop,
        timed_out,
        detached,
        posture,
    } = spec;
    let (tx, rx) = mpsc::unbounded_channel::<BindingCall>();
    let start = Instant::now();
    // Plain values for the blocking closure: the registry stays here.
    let bash_enabled = dispatcher.tools.contains("bash");
    let http_enabled = dispatcher.tools.contains(HTTP_TOOL);
    let mcp = namespaces::mcp_entries(&*dispatcher);
    let skills = launch.skills.clone();
    let handle = tokio::task::spawn_blocking(move || {
        let mut engine = Engine::new_raw();
        configure_engine(&mut engine, timeout, start, on_print, stop, timed_out);
        register_bindings(
            &mut engine,
            tx.clone(),
            bash_enabled,
            http_enabled,
            start,
            timeout,
        );
        namespaces::register(&mut engine, tx, &mcp, &skills);
        data::register_data_functions(&mut engine);
        // A constant, so `args = …`/`args.n = …` fails: the caller's input
        // is not the script's scratch space. Compiled *without* the scope:
        // given it, the optimizer inlines the constant and Rhai panics on
        // `args.n = …` ("cannot assign to expression") instead of refusing.
        let mut scope = Scope::new();
        scope.push_constant("args", to_dynamic(serde_json::Value::Object(args))?);
        let ast = engine.compile(&script)?;
        engine.eval_ast_with_scope::<Dynamic>(&mut scope, &ast)
    });
    let script_call = ScriptCall::new(detached, posture, start + timeout);
    let stopped = service::service_bindings(
        rx,
        dispatcher,
        launch.workdir.as_ref(),
        &script_call,
        request_id,
    )
    .await;
    (handle.await, stopped)
}
