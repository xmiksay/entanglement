//! Shared helpers for core integration tests.

use std::time::Duration;

use entanglement_core::{Holly, InMsg, OutEvent, SessionId};
use tokio::sync::broadcast::error::RecvError;

/// Collect every event for `sid` until `Done` (or a 3s timeout), so a test
/// can inspect the full sequence of events a turn produced.
#[allow(dead_code)]
pub async fn collect_until_done(
    mut sub: tokio::sync::broadcast::Receiver<OutEvent>,
    sid: &SessionId,
) -> Vec<OutEvent> {
    let mut out = Vec::new();
    loop {
        let Ok(recv) = tokio::time::timeout(Duration::from_secs(3), sub.recv()).await else {
            break;
        };
        match recv {
            Ok(ev) if ev.session() == Some(sid) => {
                let done = matches!(ev, OutEvent::Done { .. });
                out.push(ev);
                if done {
                    break;
                }
            }
            Ok(_) => {}
            Err(_) => break,
        }
    }
    out
}

/// Minimal stand-in for the runtime tool-executor (#58). Core no longer runs
/// tools inline — a cleared tool call becomes an [`OutEvent::ToolExec`] the
/// runtime answers. `exec` maps `(tool, input)` to the output string; the
/// executor sends it straight back as [`InMsg::ToolResult`], exactly as the
/// real runtime executor does after resolving permission.
///
/// Core no longer owns a `ToolRegistry` (that vocabulary moved to the runtime,
/// #206), so tests describe the tool surface with a plain closure instead.
/// [`unknown_tool`] is the default: it mirrors the old empty-registry behavior.
///
/// Subscribes synchronously so no `ToolExec` emitted before the task is
/// scheduled is missed. (Not every test binary that includes this shared
/// module references it.)
#[allow(dead_code)]
pub fn spawn_tool_executor<F>(holly: &Holly, exec: F)
where
    F: Fn(&str, &str) -> String + Send + Sync + 'static,
{
    let mut sub = holly.subscribe();
    let holly = holly.clone();
    tokio::spawn(async move {
        loop {
            match sub.recv().await {
                Ok(OutEvent::ToolExec {
                    session,
                    request_id,
                    tool,
                    input,
                    ..
                }) => {
                    let output = exec(&tool, &input);
                    let _ = holly
                        .send(InMsg::tool_result(session, request_id, output))
                        .await;
                }
                Ok(_) | Err(RecvError::Lagged(_)) => {}
                Err(RecvError::Closed) => break,
            }
        }
    });
}

/// Default executor reply for a tool the test doesn't model — mirrors the old
/// `ToolRegistry::execute` unknown-tool string. (Not every test binary that
/// includes this shared module references it.)
#[allow(dead_code)]
pub fn unknown_tool(name: &str, _input: &str) -> String {
    format!("unknown tool: `{name}`")
}
