//! Shared helpers for core integration tests.

use entanglement_core::{Holly, InMsg, OutEvent, ToolCall, ToolRegistry};
use tokio::sync::broadcast::error::RecvError;

/// Minimal stand-in for the runtime tool-executor (#58). Core no longer runs
/// tools inline — a cleared tool call becomes an [`OutEvent::ToolExec`] the
/// runtime answers. Integration tests that drive a tool call to completion
/// spawn one of these against a registry (empty is fine: unknown tools report
/// back, exactly as the old inline `ToolRegistry::execute` did).
///
/// Subscribes synchronously so no `ToolExec` emitted before the task is
/// scheduled is missed.
pub fn spawn_tool_executor(holly: &Holly, tools: ToolRegistry) {
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
                }
                Ok(_) | Err(RecvError::Lagged(_)) => {}
                Err(RecvError::Closed) => break,
            }
        }
    });
}
