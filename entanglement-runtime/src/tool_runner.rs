//! Runtime tool executor. Owns everything about a tool call that is *not* the
//! engine's business: the `Allow | Ask | Deny` permission decision (#59), the
//! approval UX round-trip, and the actual execution against the host-tool
//! [`ToolRegistry`] (#58, ADR-0006/0010).
//!
//! Core emits [`OutEvent::ToolExec`] for **every** host tool and parks on
//! [`InMsg::ToolResult`]; it no longer consults `PermissionProfile`. This task:
//!
//! 1. tracks each session's active [`AgentProfile`] from `SessionStarted` /
//!    `AgentChanged` (ADR-0020), resolved against the [`ProfileRegistry`] it was
//!    handed at startup;
//! 2. on `ToolExec`, resolves the permission for the tool:
//!    - `Deny` → replies `ToolResult("…denied…")` without running it;
//!    - `Allow` → runs it and replies `ToolResult`;
//!    - `Ask` → emits [`OutEvent::ToolRequest`] (the approval prompt) and awaits
//!      the head's `Approve`/`Reject`/`Stop` on the engine's inbound fan-out
//!      ([`Holly::subscribe_inbound`]), then runs-or-refuses accordingly.
//!
//! Each request runs on its own task so a slow tool (or a pending approval) in
//! one session can't stall another; core keeps only one `ToolExec` in flight per
//! session (it awaits the result before continuing), so per-session ordering
//! still holds.

use std::collections::HashMap;

use entanglement_core::{
    AgentProfile, AgentState, Holly, InMsg, OutEvent, Permission, ProfileRegistry, SessionId,
    ToolCall, ToolRegistry,
};
use tokio::sync::broadcast::error::RecvError;

/// Spawn the per-engine tool executor. Subscribes synchronously (so no
/// `ToolExec` emitted before the task is scheduled is missed) and runs until the
/// engine's outbox closes. `profiles` is the runtime's copy of the engine's
/// [`ProfileRegistry`] — the permission *shape* stays a core type; the runtime
/// only reads it (ADR-0003).
pub fn spawn_tool_executor(
    holly: &Holly,
    tools: ToolRegistry,
    profiles: ProfileRegistry,
) -> tokio::task::JoinHandle<()> {
    let mut sub = holly.subscribe();
    let holly = holly.clone();
    tokio::spawn(async move {
        // Active profile per session, folded from lifecycle events in the order
        // the engine emits them (a session's `AgentChanged` always precedes any
        // `ToolExec` it produces under that profile).
        let mut active: HashMap<SessionId, AgentProfile> = HashMap::new();
        loop {
            match sub.recv().await {
                Ok(OutEvent::SessionStarted {
                    session, profile, ..
                }) => {
                    if let Some(p) = profiles.get(&profile) {
                        active.insert(session, p.clone());
                    }
                }
                Ok(OutEvent::AgentChanged { session, agent }) => {
                    if let Some(p) = profiles.get(&agent) {
                        active.insert(session, p.clone());
                    }
                }
                Ok(OutEvent::ToolExec {
                    session,
                    seq,
                    request_id,
                    tool,
                    input,
                    ..
                }) => {
                    // `spawn_agent` only orchestrates sessions (touches no host
                    // resource), so it bypasses the permission profile like core's
                    // `update_plan`/`update_tasks` built-ins (#60). Subscribe
                    // *before* handing off so the child's `Done` can't race ahead
                    // of the watcher.
                    if tool == crate::subagent::SPAWN_TOOL {
                        let child_events = holly.subscribe();
                        let holly = holly.clone();
                        tokio::spawn(async move {
                            crate::subagent::spawn_subagent(
                                holly,
                                child_events,
                                session,
                                request_id,
                                input,
                            )
                            .await;
                        });
                        continue;
                    }
                    // Resolve permission before spawning so the read of `active`
                    // stays ordered with the lifecycle events above. A session we
                    // never saw start defaults to `Allow` (nothing to gate on).
                    let perm = active
                        .get(&session)
                        .map(|p| p.permission.for_tool(&tool))
                        .unwrap_or(Permission::Allow);
                    let tools = tools.clone();
                    let holly = holly.clone();
                    tokio::spawn(async move {
                        dispatch(&holly, &tools, session, seq, request_id, tool, input, perm).await;
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

/// Resolve one `ToolExec` per its permission and reply with a `ToolResult`.
#[allow(clippy::too_many_arguments)]
async fn dispatch(
    holly: &Holly,
    tools: &ToolRegistry,
    session: SessionId,
    seq: u64,
    request_id: String,
    tool: String,
    input: String,
    perm: Permission,
) {
    match perm {
        Permission::Allow => {
            run_and_reply(holly, tools, session, request_id, tool, input).await;
        }
        Permission::Deny => {
            let output = format!("tool `{tool}` denied by permission profile");
            reply(holly, session, request_id, output).await;
        }
        Permission::Ask => {
            // Subscribe *before* prompting so a fast approval can't race ahead of
            // us. The prompt reuses the `ToolExec` seq: core's next content event
            // (the `ToolOutput`) carries a higher seq, so a head's monotonic
            // dedupe still honors the request.
            let mut inbound = holly.subscribe_inbound();
            let _ = holly.events().send(OutEvent::ToolRequest {
                session: session.clone(),
                seq,
                request_id: request_id.clone(),
                tool: tool.clone(),
                input: input.clone(),
            });
            let _ = holly.events().send(OutEvent::Status {
                session: session.clone(),
                state: AgentState::WaitingApproval,
            });
            await_decision(holly, tools, &mut inbound, session, request_id, tool, input).await;
        }
    }
}

/// Park until the head answers the pending approval, then run-or-refuse. A
/// `Stop` (Esc-in-approval) unwinds silently: core's `wait_tool_result` sees the
/// same `Stop` on its inbox and cancels the turn, so no `ToolResult` is owed.
#[allow(clippy::too_many_arguments)]
async fn await_decision(
    holly: &Holly,
    tools: &ToolRegistry,
    inbound: &mut tokio::sync::broadcast::Receiver<InMsg>,
    session: SessionId,
    request_id: String,
    tool: String,
    input: String,
) {
    loop {
        match inbound.recv().await {
            Ok(InMsg::Approve {
                session: s,
                request_id: rid,
            }) if s == session && rid == request_id => {
                set_thinking(holly, &session);
                run_and_reply(holly, tools, session, request_id, tool, input).await;
                return;
            }
            Ok(InMsg::Reject {
                session: s,
                request_id: rid,
                reason,
            }) if s == session && rid == request_id => {
                set_thinking(holly, &session);
                let output = format!(
                    "tool `{tool}` rejected: {}",
                    reason.as_deref().unwrap_or("user")
                );
                reply(holly, session, request_id, output).await;
                return;
            }
            Ok(InMsg::Stop { session: s }) if s == session => return,
            Ok(_) => {}
            Err(RecvError::Lagged(_)) => {}
            Err(RecvError::Closed) => return,
        }
    }
}

async fn run_and_reply(
    holly: &Holly,
    tools: &ToolRegistry,
    session: SessionId,
    request_id: String,
    tool: String,
    input: String,
) {
    let output = tools
        .execute(&ToolCall {
            id: request_id.clone(),
            name: tool,
            input,
        })
        .await;
    reply(holly, session, request_id, output).await;
}

async fn reply(holly: &Holly, session: SessionId, request_id: String, output: String) {
    let _ = holly
        .send(InMsg::ToolResult {
            session,
            request_id,
            output,
        })
        .await;
}

fn set_thinking(holly: &Holly, session: &SessionId) {
    let _ = holly.events().send(OutEvent::Status {
        session: session.clone(),
        state: AgentState::Thinking,
    });
}
