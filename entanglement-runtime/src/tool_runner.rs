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

use crate::permission::{effective_permission, spawn_capability_refusal};

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
        // Bounds the spawn tree (#76): tracks parent links from lifecycle events
        // and per-root spawn budgets. Lives in this single-threaded loop, so the
        // spawn decision below is race-free.
        let mut spawn_guard = crate::subagent::SpawnGuard::new();
        // Answer + timing per launched sub-agent, keyed by its handle (#89).
        // Shared with the detached launch watchers and `agent_poll` tasks.
        let registry = crate::agent_poll::AgentRegistry::default();
        loop {
            match sub.recv().await {
                Ok(OutEvent::SessionStarted {
                    session,
                    parent,
                    profile,
                    ..
                }) => {
                    spawn_guard.record_start(session.clone(), parent);
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
                    // resource), so it bypasses per-tool approval like core's
                    // `update_plan`/`update_tasks` built-ins (#60). It is instead
                    // gated as a *capability* (#77): a read-only sub-agent leaf
                    // (Subagent-mode profile, e.g. `explore`) may not spawn, which
                    // closes the path where a restricted profile spawns a
                    // privileged child. Subscribe *before* handing off so the
                    // child's `Done` can't race ahead of the watcher.
                    if tool == crate::subagent::SPAWN_TOOL {
                        if let Some(refusal) = spawn_capability_refusal(active.get(&session)) {
                            let holly = holly.clone();
                            tokio::spawn(async move {
                                reply(&holly, session, request_id, refusal).await;
                            });
                            continue;
                        }
                        match spawn_guard.try_spawn(&session) {
                            Ok(()) => {
                                let child_events = holly.subscribe();
                                let registry = registry.clone();
                                let holly = holly.clone();
                                tokio::spawn(async move {
                                    crate::subagent::launch_subagent(
                                        holly,
                                        child_events,
                                        registry,
                                        session,
                                        request_id,
                                        input,
                                    )
                                    .await;
                                });
                            }
                            // Over a limit: refuse without starting a child, but
                            // still answer the parent's parked tool call so its
                            // turn continues with a clear explanation.
                            Err(refusal) => {
                                let holly = holly.clone();
                                tokio::spawn(async move {
                                    reply(&holly, session, request_id, refusal).await;
                                });
                            }
                        }
                        continue;
                    }
                    // `agent_poll` is the join half of non-blocking spawn (#89,
                    // ADR-0026): it starts no session and touches no host
                    // resource — it only reads accumulated spawn state — so like
                    // `spawn_agent` it bypasses permission and the spawn budget.
                    if tool == crate::agent_poll::AGENT_POLL_TOOL {
                        let registry = registry.clone();
                        let holly = holly.clone();
                        tokio::spawn(async move {
                            crate::agent_poll::run_agent_poll(
                                holly, registry, session, request_id, input,
                            )
                            .await;
                        });
                        continue;
                    }
                    // `ask_user` is a runtime-owned prompt tool (#90, ADR-0027):
                    // like `spawn_agent` it touches no host resource, so it
                    // bypasses permission and instead surfaces a question to the
                    // head. Subscribe *before* handing off so a fast answer can't
                    // race ahead of the parked executor task.
                    if tool == crate::ask_user::ASK_USER_TOOL {
                        let inbound = holly.subscribe_inbound();
                        let holly = holly.clone();
                        tokio::spawn(async move {
                            crate::ask_user::run_ask_user(
                                holly, inbound, session, seq, request_id, input,
                            )
                            .await;
                        });
                        continue;
                    }
                    // Resolve permission before spawning so the read of `active`
                    // stays ordered with the lifecycle events above. A child
                    // sub-agent is clamped to its parent chain (#77): its effective
                    // permission can never exceed any ancestor's, so a child cannot
                    // touch the shared tree in ways the parent couldn't. A root
                    // session (no ancestors) resolves to its own profile unchanged;
                    // a session we never saw start defaults to `Allow`.
                    let perm = effective_permission(&active, &spawn_guard, &session, &tool);
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
