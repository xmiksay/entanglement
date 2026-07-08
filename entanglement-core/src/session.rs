//! Per-session engine: the conversation loop, permission-driven tool dispatch,
//! and the built-in `update_plan` / `update_tasks` tools.

use std::collections::VecDeque;

use futures::StreamExt;
use tokio::sync::{broadcast, mpsc};

use crate::context::Context;
use crate::llm::{Llm, LlmEvent, LlmRequest, ToolCall, ToolSpec};
use crate::protocol::{AgentProfile, AgentState, InMsg, OutEvent, Permission, SessionId, TaskItem};
use crate::tools::ToolRegistry;
use crate::EngineConfig;
use anyhow::Result;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

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
pub struct Session {
    pub ctx: Context,
    pub llm: Box<dyn Llm>,
    pub profile: AgentProfile,
    pub tools: ToolRegistry,
    pub tasks: Vec<TaskItem>,
    pub plan: String,
    pub seq: u64,
    pub turn_count: usize,
    pub parent: Option<SessionId>,
}

impl Session {
    /// Creates a new empty session with the given configuration and profile.
    pub fn new_empty(cfg: &EngineConfig, profile: AgentProfile) -> Self {
        Self {
            ctx: Context::new(),
            llm: (cfg.llm_factory)(),
            profile,
            tools: cfg.tools.clone(),
            tasks: Vec::new(),
            plan: String::new(),
            seq: 0,
            turn_count: 0,
            parent: None,
        }
    }

    /// Resume a session from replayed log records.
    ///
    /// This reconstructs the session state from the provided records and returns
    /// the `Session` that can be passed to `session_loop_with_initial`.
    ///
    /// # Parameters
    ///
    /// - `records`: A slice of `(Option<InMsg>, OutEvent)` tuples representing the log
    /// - `cfg`: Engine configuration for constructing tools and LLM
    /// - `root`: Root directory for tool operations (unused in core but required for consistency)
    ///
    /// # Returns
    ///
    /// A reconstructed `Session` with all state folded from the log.
    pub fn replay(
        records: &[(Option<InMsg>, OutEvent)],
        cfg: &EngineConfig,
        _root: &Path,
    ) -> Result<Self> {
        let default_profile = cfg
            .profiles
            .get("build")
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("default 'build' profile not found"))?;

        let mut session = Self::new_empty(cfg, default_profile);
        let mut pending_text: String = String::new();
        let mut pending_tools: Vec<ToolCall> = Vec::new();
        let mut pending_tool_outputs: Vec<(String, String)> = Vec::new();
        let mut max_seq: u64 = 0;

        for (in_msg, out_event) in records {
            max_seq = max_seq.max(out_event.seq());

            if let Some(InMsg::Prompt { text, .. }) = in_msg {
                if !pending_text.is_empty() || !pending_tools.is_empty() {
                    session
                        .ctx
                        .push_assistant(pending_text.clone(), pending_tools.clone());
                    pending_text.clear();
                    pending_tools.clear();
                }
                for (request_id, output) in &pending_tool_outputs {
                    session.ctx.push_tool(request_id.clone(), output.clone());
                }
                pending_tool_outputs.clear();

                session.ctx.push_user(text.clone());
            }

            match out_event {
                OutEvent::SessionStarted { parent, .. } => {
                    session.parent = parent.clone();
                }
                OutEvent::TextDelta { text, .. } => {
                    pending_text.push_str(text);
                }
                OutEvent::ReasoningDelta { .. } => {
                    // Reasoning is not stored in context; it's display-only.
                }
                OutEvent::ToolCall {
                    request_id,
                    tool,
                    input,
                    ..
                } => {
                    pending_tools.push(ToolCall {
                        id: request_id.clone(),
                        name: tool.clone(),
                        input: input.clone(),
                    });
                }
                OutEvent::ToolOutput {
                    request_id, output, ..
                } => {
                    pending_tool_outputs.push((request_id.clone(), output.clone()));
                }
                OutEvent::AgentChanged { agent, .. } => {
                    if let Some(profile) = cfg.profiles.get(agent) {
                        session.profile = profile.clone();
                    }
                }
                OutEvent::TaskList { tasks, .. } => {
                    session.tasks = tasks.clone();
                }
                OutEvent::Plan { content, .. } => {
                    session.plan = content.clone();
                }
                OutEvent::Done { .. } => {
                    if !pending_text.is_empty() || !pending_tools.is_empty() {
                        session
                            .ctx
                            .push_assistant(pending_text.clone(), pending_tools.clone());
                        pending_text.clear();
                        pending_tools.clear();
                    }
                    for (request_id, output) in &pending_tool_outputs {
                        session.ctx.push_tool(request_id.clone(), output.clone());
                    }
                    pending_tool_outputs.clear();
                }
                _ => {}
            }
        }

        session.seq = max_seq;
        Ok(session)
    }
}

/// Runs one session until `Stop` / inbox close. Emits `SessionStarted`, `Idle` status
/// and `AgentChanged` so a head knows the starting profile.
///
/// If `initial_session` is provided, it's used as the starting state (for resume);
/// otherwise, a fresh session is created.
pub(crate) async fn session_loop(
    session: SessionId,
    mut rx: mpsc::Receiver<SessionCmd>,
    events: broadcast::Sender<OutEvent>,
    cfg: EngineConfig,
    profile: AgentProfile,
    initial_session: Option<Session>,
    parent: Option<SessionId>,
) {
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;

    let root = parent.is_none();
    let _ = events.send(OutEvent::SessionStarted {
        session: session.clone(),
        parent,
        profile: profile.name.clone(),
        model: profile.model.clone(),
        root,
        ts,
    });

    let mut s = initial_session.unwrap_or_else(|| Session::new_empty(&cfg, profile));
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
                // run_turn returns Err(()) only on a cancel-via-Stop during
                // tool approval (ADR-0017). The turn is aborted but the
                // session task stays alive — drop the cancel and keep
                // listening for the next command, preserving Context.
                let _ = run_turn(&session, &mut rx, &mut s, &events, &mut stash).await;
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
            // Stop arrived while idle (a turn-in-flight Stop is caught by the
            // try_recv inside run_turn). Cancel semantics (ADR-0017): no-op,
            // the session is already idle; just keep listening.
            Some(SessionCmd::Stop) => {}
            None => {
                let ts = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis() as u64;
                let _ = events.send(OutEvent::SessionEnded {
                    session: session.clone(),
                    ts,
                });
                return;
            }
        }
    }
}

/// Runs one reasoning turn to completion. Returns `Err(())` only when a
/// `SessionCmd::Stop` arrives during tool-request approval (cancel-via-Esc);
/// the caller keeps the session task alive and just awaits the next command
/// (ADR-0017). Context is preserved in either case.
async fn run_turn(
    session: &SessionId,
    rx: &mut mpsc::Receiver<SessionCmd>,
    s: &mut Session,
    events: &broadcast::Sender<OutEvent>,
    stash: &mut VecDeque<SessionCmd>,
) -> Result<(), ()> {
    s.turn_count += 1;
    const MAX_TURNS: usize = 50;
    if s.turn_count > MAX_TURNS {
        let _ = events.send(OutEvent::Error {
            session: session.clone(),
            seq: next_seq(&mut s.seq),
            message: format!("exceeded maximum turn limit ({MAX_TURNS}) - possible infinite loop"),
        });
        return Ok(());
    }

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
        tracing::debug!(
            messages_count = req.messages.len(),
            estimated_tokens = s.ctx.estimated_tokens(),
            "sending request to LLM"
        );
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
            // Drain any commands queued mid-stream: Stop interrupts the turn,
            // everything else is stashed for replay after this turn ends
            // (ADR-0018 — previously non-Stop commands were silently dropped).
            while let Ok(cmd) = rx.try_recv() {
                match cmd {
                    SessionCmd::Stop => {
                        tracing::debug!("turn interrupted during streaming");
                        drop(stream);
                        let _ = events.send(OutEvent::Status {
                            session: session.clone(),
                            state: AgentState::Idle,
                        });
                        return Ok(());
                    }
                    other => {
                        tracing::debug!(
                            cmd = ?other,
                            "command arrived mid-stream; stashed for replay after turn"
                        );
                        stash.push_back(other);
                    }
                }
            }
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
                Ok(LlmEvent::Reasoning(delta)) => {
                    if !delta.is_empty() {
                        let _ = events.send(OutEvent::ReasoningDelta {
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

        s.ctx.push_assistant(text_buf.clone(), tool_calls.clone());
        tracing::debug!(
            text_len = text_buf.len(),
            tool_calls_count = tool_calls.len(),
            "assistant message pushed"
        );
        tracing::debug!(
            context_messages = s.ctx.messages().len(),
            "context after assistant message"
        );

        // End turn if no tool calls (conversation complete)
        if tool_calls.is_empty() {
            tracing::debug!("no tool calls - emitting Done");
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

        // Execute tool calls
        for call in tool_calls {
            // Drain any commands queued between tools: Stop interrupts, the
            // rest are stashed for replay (ADR-0018).
            while let Ok(cmd) = rx.try_recv() {
                match cmd {
                    SessionCmd::Stop => {
                        tracing::debug!("turn interrupted between tool calls");
                        let _ = events.send(OutEvent::Status {
                            session: session.clone(),
                            state: AgentState::Idle,
                        });
                        return Ok(());
                    }
                    other => {
                        tracing::debug!(
                            cmd = ?other,
                            "command arrived between tool calls; stashed for replay after turn"
                        );
                        stash.push_back(other);
                    }
                }
            }
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
    emit_tool_call(
        events,
        session,
        &call.id,
        &call.name,
        &call.input,
        &mut s.seq,
    );

    // Built-ins: always run, mutate session state, emit a snapshot.
    if call.name == PLAN_TOOL {
        let plan = json_field(&call.input, "content").unwrap_or_else(|| call.input.clone());
        s.plan = plan;
        emit_plan(events, session, &s.plan, &mut s.seq);
        let msg = "plan updated".to_string();
        emit_tool_output(
            events,
            session,
            &call.id,
            PLAN_TOOL,
            msg.clone(),
            &mut s.seq,
        );
        s.ctx.push_tool(&call.id, msg.clone());
        tracing::debug!(tool_id = %call.id, result = %msg, "tool result pushed to context");
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
        emit_tool_output(
            events,
            session,
            &call.id,
            TASKS_TOOL,
            msg.clone(),
            &mut s.seq,
        );
        s.ctx.push_tool(&call.id, msg);
        return false;
    }

    // Host tool: permission profile decides allow / ask / deny.
    match s.profile.permission.for_tool(&call.name) {
        Permission::Allow => {
            let out = s.tools.execute(&call).await;
            emit_tool_output(
                events,
                session,
                &call.id,
                &call.name,
                out.clone(),
                &mut s.seq,
            );
            s.ctx.push_tool(&call.id, out);
            false
        }
        Permission::Deny => {
            let out = format!("tool `{}` denied by permission profile", call.name);
            emit_tool_output(
                events,
                session,
                &call.id,
                &call.name,
                out.clone(),
                &mut s.seq,
            );
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
                    emit_tool_output(
                        events,
                        session,
                        &call.id,
                        &call.name,
                        out.clone(),
                        &mut s.seq,
                    );
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
                    emit_tool_output(
                        events,
                        session,
                        &call.id,
                        &call.name,
                        out.clone(),
                        &mut s.seq,
                    );
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

fn emit_tool_call(
    events: &broadcast::Sender<OutEvent>,
    session: &SessionId,
    request_id: &str,
    tool: &str,
    input: &str,
    seq: &mut u64,
) {
    let _ = events.send(OutEvent::ToolCall {
        session: session.clone(),
        seq: next_seq(seq),
        request_id: request_id.to_string(),
        tool: tool.to_string(),
        input: input.to_string(),
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
    tool: &str,
    output: String,
    seq: &mut u64,
) {
    let _ = events.send(OutEvent::ToolOutput {
        session: session.clone(),
        seq: next_seq(seq),
        request_id: request_id.to_string(),
        tool: tool.to_string(),
        output,
    });
}
