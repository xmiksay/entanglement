//! Runtime tool executor (#58). Core relocated tool *execution* out of the
//! engine: when a tool is cleared to run it emits [`OutEvent::ToolExec`] and
//! parks the turn. This task subscribes to the engine, runs each `ToolExec`
//! against the runtime's own [`ToolRegistry`] (the host-tool impls, ADR-0010),
//! and feeds the output back with [`InMsg::ToolResult`].
//!
//! Approval (`Ask`) and denial (`Deny`) stay in core (#59 relocates permission
//! dispatch): a denied tool never produces a `ToolExec`, so it never reaches
//! this executor. Each request is executed on its own task so a slow tool in
//! one session can't stall another — core only has one `ToolExec` in flight per
//! session (it awaits the result before continuing), so per-session ordering
//! still holds.

use entanglement_core::{Holly, InMsg, OutEvent, ToolCall, ToolRegistry};
use tokio::sync::broadcast::error::RecvError;

/// Spawn the per-engine tool executor. Subscribes synchronously (so no
/// `ToolExec` emitted before the task is scheduled is missed) and runs until
/// the engine's outbox closes.
pub fn spawn_tool_executor(holly: &Holly, tools: ToolRegistry) -> tokio::task::JoinHandle<()> {
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
                    let tools = tools.clone();
                    let holly = holly.clone();
                    tokio::spawn(async move {
                        let output = tools
                            .execute(&ToolCall {
                                id: request_id.clone(),
                                name: tool,
                                input,
                            })
                            .await;
                        let _ = holly
                            .send(InMsg::ToolResult {
                                session,
                                request_id,
                                output,
                            })
                            .await;
                    });
                }
                Ok(_) => {}
                // A lagging executor drops broadcast events; the affected turn
                // stays parked, but that's preferable to executing stale calls.
                Err(RecvError::Lagged(n)) => {
                    tracing::warn!(skipped = n, "tool executor lagged; some ToolExec dropped");
                }
                Err(RecvError::Closed) => break,
            }
        }
    })
}
