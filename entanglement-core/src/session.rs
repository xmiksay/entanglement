//! Per-session engine: the conversation loop and the tool-request round-trip to
//! the runtime.
//!
//! Permission dispatch (`Allow`/`Ask`/`Deny`) and the approval wait no longer
//! live here (#59): core emits `OutEvent::ToolExec` for every tool and parks on
//! `InMsg::ToolResult`; the runtime tool executor owns the policy decision and
//! the approval UX (ADR-0003/0010). `update_plan`/`update_tasks` are ordinary
//! runtime state tools too now (#231, ADR-0049) — the engine holds no plan/task
//! state and makes no plan-authority call.
//!
//! Split along the natural seam (#109): the replay/fold that reconstructs a
//! session from a persisted log lives in [`replay`]; the live reasoning turn in
//! [`turn`]; per-turn tool dispatch in [`tools`]; the outbound-event emit
//! helpers in [`emit`].

mod emit;
mod replay;
mod tools;
mod turn;

use std::collections::VecDeque;

use tokio::sync::{broadcast, mpsc};

use crate::context::Context;
use crate::llm::LlmSession;
use crate::protocol::{AgentProfile, AgentState, OutEvent, SessionId};
use crate::EngineConfig;
use std::time::{SystemTime, UNIX_EPOCH};

use emit::next_seq;
use turn::run_turn;

/// Commands routed to a single session by the supervisor (InMsg minus session id).
#[derive(Debug, Clone)]
pub(crate) enum SessionCmd {
    Prompt(String),
    /// Output of a runtime-executed tool (`request_id`, `output`) — resolves a
    /// pending [`OutEvent::ToolExec`] round-trip (#58). Approval (`Approve`/
    /// `Reject`) is no longer a core command: the runtime tool executor owns it
    /// (#59) and never reaches the session loop.
    ToolResult(String, String),
    SetAgent(String),
    Stop,
}

/// Mutable per-session loop + turn state (#61). Holds the conversation
/// [`Context`], the provider session handle (`llm`, #55), the active profile,
/// and the loop counters — nothing pointing at the filesystem or a fixed tool
/// set. Plan/task snapshots are the runtime's display state, not engine state
/// (#231, ADR-0049), so the session carries neither. The tool schemas advertised
/// to the model are config, not session state: they come from
/// [`EngineConfig::tool_specs`] at turn time (see [`turn::run_turn`]).
pub struct Session {
    pub ctx: Context,
    pub llm: LlmSession,
    pub profile: AgentProfile,
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
            seq: 0,
            turn_count: 0,
            parent: None,
        }
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
        profile_detail: Some(s.profile.detail()),
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
            Some(SessionCmd::SetAgent(name)) => match cfg.profiles.get(&name) {
                Some(p) => {
                    s.profile = p.clone();
                    let _ = events.send(OutEvent::AgentChanged {
                        session: session.clone(),
                        agent: p.name.clone(),
                        profile_detail: Some(p.detail()),
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
