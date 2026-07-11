//! Per-session engine: the conversation loop, the tool-request round-trip to
//! the runtime, and the built-in `update_plan` / `update_tasks` tools.
//!
//! Permission dispatch (`Allow`/`Ask`/`Deny`) and the approval wait no longer
//! live here (#59): core emits `OutEvent::ToolExec` for every host tool and
//! parks on `InMsg::ToolResult`; the runtime tool executor owns the policy
//! decision and the approval UX (ADR-0003/0010).

use std::collections::{HashMap, VecDeque};

use futures::StreamExt;
use tokio::sync::{broadcast, mpsc};

use crate::context::Context;
use crate::llm::{Llm, LlmEvent, LlmRequest, LlmSession, ToolCall, ToolSpec};
use crate::protocol::{AgentProfile, AgentState, InMsg, OutEvent, SessionId};
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
    /// Output of a runtime-executed tool (`request_id`, `output`) — resolves a
    /// pending [`OutEvent::ToolExec`] round-trip (#58). Approval (`Approve`/
    /// `Reject`) is no longer a core command: the runtime tool executor owns it
    /// (#59) and never reaches the session loop.
    ToolResult(String, String),
    SetPlan(String),
    SetTasks(String),
    SetAgent(String),
    Stop,
}

/// Mutable per-session loop + turn state (#61). Holds the conversation
/// [`Context`], the provider session handle (`llm`, #55), the active profile,
/// the plan/tasks snapshots, and the loop counters — nothing pointing at the
/// filesystem or a fixed tool set. The tool schemas advertised to the model are
/// config, not session state: they come from [`EngineConfig::tool_specs`] at
/// turn time (see [`run_turn`]).
pub struct Session {
    pub ctx: Context,
    pub llm: LlmSession,
    pub profile: AgentProfile,
    pub tasks: String,
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
            tasks: String::new(),
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
                OutEvent::TaskList { content, .. } => {
                    session.tasks = content.clone();
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
                let _ = run_turn(
                    &session,
                    &mut rx,
                    &mut s,
                    &events,
                    &mut stash,
                    &cfg.tool_specs,
                    &cfg.profile_tool_specs,
                )
                .await;
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
            // A ToolResult with no pending tool request: stale (e.g. a late
            // result after the turn was cancelled), drop silently.
            Some(SessionCmd::ToolResult(..)) => {}
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
    tool_specs: &[ToolSpec],
    profile_tool_specs: &HashMap<String, Vec<ToolSpec>>,
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

    // Tool set advertised to the model = host tools (from config, #61) filtered
    // by the active profile's allowlist/denylist mask (#116, ADR-0038), plus the
    // two built-ins. Core caches no fixed tool set on the session; the schemas
    // come from `EngineConfig.tool_specs` at turn time. The mask is a *physical*
    // restriction — a masked tool's schema never reaches the model — layered
    // under the runtime's `Allow`/`Ask`/`Deny` dispatch, which grades only the
    // tools that survive here. The `update_plan`/`update_tasks` built-ins below
    // are session-state tools, not host tools, so they bypass the mask.
    let mut specs: Vec<ToolSpec> = tool_specs
        .iter()
        .filter(|spec| s.profile.advertises_tool(&spec.name))
        .cloned()
        .collect();
    // Per-profile specs (#119, ADR-0040): the active profile's spawnable roster
    // (the `agent_*` family with a target enum scoped to who *this* profile may
    // spawn) lives outside the shared `tool_specs` so a masked schema never
    // reaches the model. The runtime leaves the entry empty for a profile that
    // may not spawn. Still filtered through the #116 mask, so a `disallowed_tools`
    // list can subtract even a per-profile tool.
    if let Some(profile_specs) = profile_tool_specs.get(&s.profile.name) {
        specs.extend(
            profile_specs
                .iter()
                .filter(|spec| s.profile.advertises_tool(&spec.name))
                .cloned(),
        );
    }
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
        "Replace the task list (markdown). Shown to the user as progress info — \
         it is not fed back to you, so keep it a short checklist.",
        serde_json::json!({
            "type": "object",
            "properties": {
                "content": {
                    "type": "string",
                    "description": "The full task list, in markdown — e.g. `- [ ]` / `- [x]` checkbox lines."
                }
            },
            "required": ["content"]
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
        let tasks = json_field(&call.input, "content").unwrap_or_else(|| call.input.clone());
        s.tasks = tasks;
        emit_tasks(events, session, &s.tasks, &mut s.seq);
        let msg = "tasks updated".to_string();
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

    // Host tool: hand it to the runtime. Core no longer decides permission or
    // waits for approval (#59) — that policy moved to the runtime tool executor
    // (ADR-0003/0010), which resolves Allow/Ask/Deny, drives the approval UX,
    // and answers every call with `InMsg::ToolResult`. Core just emits the
    // request and parks on the result (the same #58 round-trip).
    run_tool_via_runtime(session, rx, s, events, stash, &call).await
}

/// Hand a permission-cleared tool call to the runtime and await its result
/// (#58). Emits [`OutEvent::ToolExec`], parks the turn on [`wait_tool_result`],
/// then surfaces the output as a [`OutEvent::ToolOutput`] and folds it into
/// context. Returns `true` if the turn was cancelled while waiting.
async fn run_tool_via_runtime(
    session: &SessionId,
    rx: &mut mpsc::Receiver<SessionCmd>,
    s: &mut Session,
    events: &broadcast::Sender<OutEvent>,
    stash: &mut VecDeque<SessionCmd>,
    call: &ToolCall,
) -> bool {
    let _ = events.send(OutEvent::ToolExec {
        session: session.clone(),
        seq: next_seq(&mut s.seq),
        request_id: call.id.clone(),
        tool: call.name.clone(),
        input: call.input.clone(),
    });
    match wait_tool_result(rx, stash, &call.id).await {
        ToolResultOutcome::Ready(out) => {
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
        ToolResultOutcome::Cancelled => true,
    }
}

enum ToolResultOutcome {
    Ready(String),
    Cancelled,
}

/// Wait for the runtime's [`SessionCmd::ToolResult`] matching `pending`,
/// stashing any other commands for replay after the turn. `Stop`/inbox-close
/// cancels the turn (ADR-0017); a late result for a cancelled call arrives at
/// the idle loop and is dropped as stale.
async fn wait_tool_result(
    rx: &mut mpsc::Receiver<SessionCmd>,
    stash: &mut VecDeque<SessionCmd>,
    pending: &str,
) -> ToolResultOutcome {
    loop {
        match rx.recv().await {
            Some(SessionCmd::ToolResult(id, output)) if id == pending => {
                return ToolResultOutcome::Ready(output)
            }
            Some(SessionCmd::Stop) | None => return ToolResultOutcome::Cancelled,
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
    tasks: &str,
    seq: &mut u64,
) {
    let _ = events.send(OutEvent::TaskList {
        session: session.clone(),
        seq: next_seq(seq),
        content: tasks.to_string(),
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
