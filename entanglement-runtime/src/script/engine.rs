//! One engine run, shared by the blocking and the background path: build
//! the sandboxed engine with its bindings and namespaces on the blocking
//! pool, and service its bridge through the dispatch seam on this task until
//! the engine finishes.

use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::{Duration, Instant};

use rhai::{Dynamic, Engine, EvalAltResult};
use tokio::sync::mpsc;

use crate::tool_names::HTTP_TOOL;
use crate::tool_runner::{Dispatcher, ScriptCall};

use super::{configure_engine, data, namespaces, register_bindings, service, BindingCall, Launch};

pub(super) struct RunSpec {
    pub script: String,
    pub timeout: Duration,
    pub stop: Arc<AtomicBool>,
    /// Set when the deadline fires — the background path reports it.
    pub timed_out: Arc<AtomicBool>,
    /// A background run (#637): no session state transitions.
    pub detached: bool,
}

pub(super) type EvalResult = Result<Result<Dynamic, Box<EvalAltResult>>, tokio::task::JoinError>;

/// Run `spec.script` to completion. Returns the engine's result and whether a
/// `Stop` unwound a nested approval.
pub(super) async fn run_engine(
    dispatcher: &Dispatcher,
    launch: &Launch,
    request_id: &str,
    spec: RunSpec,
    on_print: impl Fn(&str) + Send + Sync + 'static,
) -> (EvalResult, bool) {
    let RunSpec {
        script,
        timeout,
        stop,
        timed_out,
        detached,
    } = spec;
    let (tx, rx) = mpsc::unbounded_channel::<BindingCall>();
    let start = Instant::now();
    // Plain values for the blocking closure: the registry stays here.
    let bash_enabled = dispatcher.tools.contains("bash");
    let http_enabled = dispatcher.tools.contains(HTTP_TOOL);
    let mcp = namespaces::mcp_entries(dispatcher);
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
        engine.eval::<Dynamic>(&script)
    });
    let script_call = ScriptCall::new(detached, start + timeout);
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
