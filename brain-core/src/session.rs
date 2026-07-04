//! Per-session engine: the conversation loop, permission-driven tool dispatch,
//! and the built-in `update_plan` / `update_tasks` tools.

use std::collections::VecDeque;

use futures::StreamExt;
use tokio::sync::{broadcast, mpsc};

use crate::context::Context;
use crate::llm::{Llm, LlmEvent, LlmRequest, ToolCall, ToolSpec};
use crate::protocol::{AgentProfile, AgentState, OutEvent, Permission, SessionId, TaskItem};
use crate::tools::ToolRegistry;
use crate::EngineConfig;

/// Built-in engine tools. They mutate session state only, so they bypass the
/// permission profile and always run (no approval).
pub(crate) const PLAN_TOOL: &str = "update_plan";
pub(crate) const TASKS_TOOL: &str = "update_tasks";

/// Commands routed to a single session by the supervisor (InMsg minus session id).
#[derive(Debug, Clone)]
pub(crate) enum SessionCmd {
    Prompt(String),
    Approve(String),
    Reject(String, Option<String>),
    SetPlan(String),
    SetTasks(Vec<TaskItem>),
    SetAgent(String),
    Stop,
}

/// Mutable per-session state.
pub(crate) struct Session {
    pub ctx: Context,
    pub llm: Box<dyn Llm>,
    pub profile: AgentProfile,
    pub tools: ToolRegistry,
    pub tasks: Vec<TaskItem>,
    pub plan: String,
    pub seq: u64,
}

/// Runs one session until `Stop` / inbox close. Emits an initial `Idle` status
/// and `AgentChanged` so a head knows the starting profile.
pub(crate) async fn session_loop(
    session: SessionId,
    mut rx: mpsc::Receiver<SessionCmd>,
    events: broadcast::Sender<OutEvent>,
    cfg: EngineConfig,
    profile: AgentProfile,
) {
    let mut s = Session {
        ctx: Context::new(),
        llm: (cfg.llm_factory)(),
        profile,
        tools: cfg.tools.clone(),
        tasks: Vec::new(),
        plan: String::new(),
        seq: 0,
    };
    let mut stash: VecDeque<SessionCmd> = VecDeque::new();

    let _ = events.send(OutEvent::Status {
        session: session.clone(),
        state: AgentState::Idle,
    });
    let _ = events.send(OutEvent::AgentChanged {
        session: session.clone(),
        agent: s.profile.name.clone(),
    });

    loop {
        let cmd = if let Some(c) = stash.pop_front() {
            Some(c)
        } else {
            rx.recv().await
        };
        match cmd {
            Some(SessionCmd::Prompt(text)) => {
                s.ctx.push_user(text);
                if let Err(()) = run_turn(&session, &mut rx, &mut s, &events, &mut stash).await {
                    return; // cancelled
                }
            }
            Some(SessionCmd::SetPlan(content)) => {
                s.plan = content;
                emit_plan(&events, &session, &s.plan, &mut s.seq);
            }
            Some(SessionCmd::SetTasks(tasks)) => {
                s.tasks = tasks;
                emit_tasks(&events, &session, &s.tasks, &mut s.seq);
            }
            Some(SessionCmd::SetAgent(name)) => match cfg.profiles.get(&name) {
                Some(p) => {
                    s.profile = p.clone();
                    let _ = events.send(OutEvent::AgentChanged {
                        session: session.clone(),
                        agent: p.name.clone(),
                    });
                }
                None => {
                    let _ = events.send(OutEvent::Error {
                        session: session.clone(),
                        seq: next_seq(&mut s.seq),
                        message: format!("unknown agent: {name}"),
                    });
                }
            },
            // Approve/Reject with no pending tool request: stale, drop silently.
            Some(SessionCmd::Approve(_) | SessionCmd::Reject(..)) => {}
            Some(SessionCmd::Stop) | None => return,
        }
    }
}

/// Runs one reasoning turn to completion. Returns `Err(())` if cancelled mid-turn.
async fn run_turn(
    session: &SessionId,
    rx: &mut mpsc::Receiver<SessionCmd>,
    s: &mut Session,
    events: &broadcast::Sender<OutEvent>,
    stash: &mut VecDeque<SessionCmd>,
) -> Result<(), ()> {
    let _ = events.send(OutEvent::Status {
        session: session.clone(),
        state: AgentState::Thinking,
    });

    // Tool set advertised to the model = host tools + the two built-ins.
    let mut specs: Vec<ToolSpec> = s.tools.specs();
    specs.push(ToolSpec::with_schema(
        PLAN_TOOL,
        "Replace the strategy plan (markdown prose).",
        serde_json::json!({
            "type": "object",
            "properties": {
                "content": {
                    "type": "string",
                    "description": "The full plan document, in markdown."
                }
            },
            "required": ["content"]
        }),
    ));
    specs.push(ToolSpec::with_schema(
        TASKS_TOOL,
        "Replace the task outline.",
        serde_json::json!({
            "type": "object",
            "properties": {
                "tasks": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "id": { "type": "string" },
                            "content": { "type": "string" },
                            "status": {
                                "type": "string",
                                "enum": ["pending", "in_progress", "completed", "cancelled"]
                            }
                        },
                        "required": ["id", "content", "status"]
                    }
                }
            },
            "required": ["tasks"]
        }),
    ));

    loop {
        if !s.ctx.within_limit() {
            let _ = events.send(OutEvent::Error {
                session: session.clone(),
                seq: next_seq(&mut s.seq),
                message: format!("context over limit ({} tokens)", s.ctx.estimated_tokens()),
            });
        }

        let req = LlmRequest {
            system: &s.profile.system_prompt,
            model: s.profile.model.as_deref(),
            messages: s.ctx.messages(),
            tools: &specs,
        };
        let mut stream = match s.llm.stream(req).await {
            Ok(st) => st,
            Err(e) => {
                emit_turn_error(session, &mut s.seq, events, e.to_string());
                return Ok(());
            }
        };

        // Consume the stream: emit incremental TextDelta, assemble tool calls.
        let mut text_buf = String::new();
        let mut tool_calls: Vec<ToolCall> = Vec::new();
        let mut stream_err: Option<String> = None;
        while let Some(ev) = stream.next().await {
            match ev {
                Ok(LlmEvent::Text(delta)) => {
                    if !delta.is_empty() {
                        text_buf.push_str(&delta);
                        let _ = events.send(OutEvent::TextDelta {
                            session: session.clone(),
                            seq: next_seq(&mut s.seq),
                            text: delta,
                        });
                    }
                }
                Ok(LlmEvent::ToolCall(call)) => tool_calls.push(call),
                Ok(LlmEvent::Finish { .. }) => {}
                Err(e) => {
                    stream_err = Some(e.to_string());
                    break;
                }
            }
        }
        drop(stream);

        if let Some(msg) = stream_err {
            // Partial text was already streamed; do not commit the failed turn.
            emit_turn_error(session, &mut s.seq, events, msg);
            return Ok(());
        }

        s.ctx.push_assistant(text_buf, tool_calls.clone());

        if tool_calls.is_empty() {
            let _ = events.send(OutEvent::Done {
                session: session.clone(),
                seq: next_seq(&mut s.seq),
            });
            let _ = events.send(OutEvent::Status {
                session: session.clone(),
                state: AgentState::Done,
            });
            return Ok(());
        }

        for call in tool_calls {
            if handle_tool_call(session, rx, s, events, stash, call).await {
                return Err(()); // cancelled
            }
        }
    }
}

/// Dispatch one tool call. Returns `true` if the turn was cancelled.
async fn handle_tool_call(
    session: &SessionId,
    rx: &mut mpsc::Receiver<SessionCmd>,
    s: &mut Session,
    events: &broadcast::Sender<OutEvent>,
    stash: &mut VecDeque<SessionCmd>,
    call: ToolCall,
) -> bool {
    // Built-ins: always run, mutate session state, emit a snapshot.
    if call.name == PLAN_TOOL {
        let plan = json_field(&call.input, "content").unwrap_or_else(|| call.input.clone());
        s.plan = plan;
        emit_plan(events, session, &s.plan, &mut s.seq);
        let msg = "plan updated".to_string();
        emit_tool_output(events, session, &call.id, msg.clone(), &mut s.seq);
        s.ctx.push_tool(&call.id, msg);
        return false;
    }
    if call.name == TASKS_TOOL {
        let tasks_input = json_field(&call.input, "tasks").unwrap_or_else(|| call.input.clone());
        let msg = match serde_json::from_str::<Vec<TaskItem>>(&tasks_input) {
            Ok(list) => {
                s.tasks = list;
                emit_tasks(events, session, &s.tasks, &mut s.seq);
                format!("tasks updated ({} items)", s.tasks.len())
            }
            Err(e) => format!("invalid task list: {e}"),
        };
        emit_tool_output(events, session, &call.id, msg.clone(), &mut s.seq);
        s.ctx.push_tool(&call.id, msg);
        return false;
    }

    // Host tool: permission profile decides allow / ask / deny.
    match s.profile.permission.for_tool(&call.name) {
        Permission::Allow => {
            let out = s.tools.execute(&call).await;
            emit_tool_output(events, session, &call.id, out.clone(), &mut s.seq);
            s.ctx.push_tool(&call.id, out);
            false
        }
        Permission::Deny => {
            let out = format!("tool `{}` denied by permission profile", call.name);
            emit_tool_output(events, session, &call.id, out.clone(), &mut s.seq);
            s.ctx.push_tool(&call.id, out);
            false
        }
        Permission::Ask => {
            let _ = events.send(OutEvent::ToolRequest {
                session: session.clone(),
                seq: next_seq(&mut s.seq),
                request_id: call.id.clone(),
                tool: call.name.clone(),
                input: call.input.clone(),
            });
            let _ = events.send(OutEvent::Status {
                session: session.clone(),
                state: AgentState::WaitingApproval,
            });
            match wait_approval(rx, stash, &call.id).await {
                Approval::Approved => {
                    set_thinking(events, session);
                    let out = s.tools.execute(&call).await;
                    emit_tool_output(events, session, &call.id, out.clone(), &mut s.seq);
                    s.ctx.push_tool(&call.id, out);
                    false
                }
                Approval::Rejected(reason) => {
                    set_thinking(events, session);
                    let out = format!(
                        "tool `{}` rejected: {}",
                        call.name,
                        reason.as_deref().unwrap_or("user")
                    );
                    emit_tool_output(events, session, &call.id, out.clone(), &mut s.seq);
                    s.ctx.push_tool(&call.id, out);
                    false
                }
                Approval::Cancelled => true,
            }
        }
    }
}

enum Approval {
    Approved,
    Rejected(Option<String>),
    Cancelled,
}

/// Wait for an approve/reject for `pending`, stashing any other commands (e.g. a
/// new prompt or a stale approval) to be processed after the turn.
async fn wait_approval(
    rx: &mut mpsc::Receiver<SessionCmd>,
    stash: &mut VecDeque<SessionCmd>,
    pending: &str,
) -> Approval {
    loop {
        match rx.recv().await {
            Some(SessionCmd::Approve(id)) if id == pending => return Approval::Approved,
            Some(SessionCmd::Reject(id, reason)) if id == pending => {
                return Approval::Rejected(reason)
            }
            Some(SessionCmd::Stop) | None => return Approval::Cancelled,
            Some(other) => stash.push_back(other),
        }
    }
}

/// Extract a field from a JSON-object tool input. Returns `None` when `input`
/// isn't a JSON object or lacks the field, so callers fall back to the raw
/// input — keeping scripted/test backends (raw strings) working alongside
/// structured providers (Anthropic sends a JSON object).
fn json_field(input: &str, field: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(input).ok()?;
    match v.get(field) {
        Some(serde_json::Value::String(s)) => Some(s.clone()),
        Some(other) if !other.is_null() => Some(other.to_string()),
        _ => None,
    }
}

// ── emit helpers ────────────────────────────────────────────────────────────

fn next_seq(s: &mut u64) -> u64 {
    *s += 1;
    *s
}

/// Surface a failed turn: an `Error`, a `Done` (so one-shot heads exit), then
/// the `Error` lifecycle state. The engine stays alive for the next prompt.
fn emit_turn_error(
    session: &SessionId,
    seq: &mut u64,
    events: &broadcast::Sender<OutEvent>,
    message: String,
) {
    let _ = events.send(OutEvent::Error {
        session: session.clone(),
        seq: next_seq(seq),
        message,
    });
    let _ = events.send(OutEvent::Done {
        session: session.clone(),
        seq: next_seq(seq),
    });
    let _ = events.send(OutEvent::Status {
        session: session.clone(),
        state: AgentState::Error,
    });
}

fn set_thinking(events: &broadcast::Sender<OutEvent>, session: &SessionId) {
    let _ = events.send(OutEvent::Status {
        session: session.clone(),
        state: AgentState::Thinking,
    });
}

fn emit_plan(events: &broadcast::Sender<OutEvent>, session: &SessionId, plan: &str, seq: &mut u64) {
    let _ = events.send(OutEvent::Plan {
        session: session.clone(),
        seq: next_seq(seq),
        content: plan.to_string(),
    });
}

fn emit_tasks(
    events: &broadcast::Sender<OutEvent>,
    session: &SessionId,
    tasks: &[TaskItem],
    seq: &mut u64,
) {
    let _ = events.send(OutEvent::TaskList {
        session: session.clone(),
        seq: next_seq(seq),
        tasks: tasks.to_vec(),
    });
}

fn emit_tool_output(
    events: &broadcast::Sender<OutEvent>,
    session: &SessionId,
    request_id: &str,
    output: String,
    seq: &mut u64,
) {
    let _ = events.send(OutEvent::ToolOutput {
        session: session.clone(),
        seq: next_seq(seq),
        request_id: request_id.to_string(),
        output,
    });
}
