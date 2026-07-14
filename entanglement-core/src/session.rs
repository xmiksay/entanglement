//! Per-session engine: the conversation loop and the tool-request round-trip to
//! the runtime.
//!
//! Permission dispatch (`Allow`/`Ask`/`Deny`) and the approval wait no longer
//! live here (#59): core batch-emits `OutEvent::ToolExec` for every tool call
//! of a round and parks the turn as explicit [`TurnState`] data (#270,
//! ADR-0061); the runtime tool executor — or any external resolver — answers
//! each call with `InMsg::ToolResult`, in any order. The runtime owns the
//! policy decision and the approval UX (ADR-0003/0010).
//! `update_plan`/`update_tasks` are ordinary runtime state tools too now
//! (#231, ADR-0049) — the engine holds no plan/task state and makes no
//! plan-authority call.
//!
//! Split along the natural seam (#109): the replay/fold that reconstructs a
//! session from a persisted log lives in [`replay`]; the live reasoning turn in
//! [`turn`]; the streamed round-trip in [`stream`]; the parked-turn state in
//! [`turn_state`]; the outbound-event emit helpers in [`emit`].

mod emit;
mod replay;
mod stream;
mod turn;
mod turn_state;

pub use turn_state::TurnState;

use std::collections::VecDeque;

use tokio::sync::{broadcast, mpsc};

use crate::context::Context;
use crate::protocol::{AgentProfile, AgentState, OutEvent, SessionId};
use crate::EngineConfig;
use entanglement_provider::LlmSession;
use std::time::{SystemTime, UNIX_EPOCH};

use emit::{emit_tool_exec, emit_tool_output, next_seq};
use turn::drive_turn;

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
/// and the emit sequence — nothing pointing at the filesystem or a fixed tool
/// set. Plan/task snapshots are the runtime's display state, not engine state
/// (#231, ADR-0049), so the session carries neither. The tool schemas advertised
/// to the model are config, not session state: they come from
/// [`EngineConfig::tool_specs`] at turn time (see [`turn`]).
pub struct Session {
    pub ctx: Context,
    pub llm: LlmSession,
    pub profile: AgentProfile,
    pub seq: u64,
    pub parent: Option<SessionId>,
    /// Cumulative token usage + cost across every model round-trip this session
    /// has run (#192). Each `LlmEvent::Finish` folds its normalized `Usage` in
    /// here and emits the per-round-trip delta as [`OutEvent::Usage`].
    pub usage: SessionUsage,
    /// The in-flight turn (#270, ADR-0061): `Some` while a turn is live —
    /// streaming or parked on unresolved tool calls — `None` when idle.
    /// Serde-capable so an embedder can persist a suspended-mid-turn session
    /// (via the event log + replay) and resolve the pending calls against its
    /// own state.
    pub turn: Option<TurnState>,
}

/// Running per-session usage tally (#192): the sum of every round-trip's
/// normalized token counts plus the accrued dollar cost. Kept in the engine so a
/// session total survives across turns; heads reconstruct the same total by
/// accumulating the per-round-trip [`OutEvent::Usage`] deltas.
#[derive(Debug, Clone, Copy, Default)]
pub struct SessionUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cached_input_tokens: u64,
    pub cache_write_tokens: u64,
    pub cost_usd: f64,
}

impl Session {
    /// Creates a new empty session with the given configuration and profile.
    pub fn new_empty(cfg: &EngineConfig, profile: AgentProfile) -> Self {
        Self {
            // Budget the history against the active model's real context window
            // (#178), not a fixed Anthropic-shaped ceiling.
            ctx: Context::with_window(cfg.context_window),
            llm: (cfg.llm_factory)(),
            profile,
            seq: 0,
            parent: None,
            usage: SessionUsage::default(),
            turn: None,
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

    // A session resumed mid-turn (#271/#272, ADR-0061): re-offer every pending
    // call — same `request_id`, fresh `seq` — so the tool executor (or an
    // external resolver) answers it exactly like a first offer, then fall into
    // the loop parked. At-least-once by design: a tool that ran before the
    // crash but whose result was never logged runs again. Display `ToolCall`
    // events are not re-emitted — heads rebuild those from the log. A drained
    // tail (every result logged, next round never streamed) has nothing to
    // re-offer; continue the turn directly.
    if let Some(turn) = s.turn.as_ref() {
        if turn.pending.is_empty() {
            drive_turn(&session, &mut rx, &mut s, &events, &mut stash, &cfg).await;
        } else {
            let _ = events.send(OutEvent::Status {
                session: session.clone(),
                state: AgentState::Thinking,
            });
            let pending = turn.pending.clone();
            for c in &pending {
                emit_tool_exec(&events, &session, c, &mut s.seq);
            }
        }
    }

    loop {
        // Pop the stash only when idle: a command stashed during a live turn
        // replays after the turn ends (ADR-0018). While parked, popping a
        // stashed `Prompt` here would only re-stash it below — a busy loop.
        let cmd = if s.turn.is_none() {
            if let Some(c) = stash.pop_front() {
                Some(c)
            } else {
                rx.recv().await
            }
        } else {
            rx.recv().await
        };
        match cmd {
            Some(SessionCmd::Prompt(text)) => {
                if s.turn.is_some() {
                    // Mid-turn steering (#182, ADR-0058): stash it — the next
                    // round folds stashed prompts into the live context before
                    // the model request.
                    stash.push_back(SessionCmd::Prompt(text));
                } else {
                    s.ctx.push_user(text);
                    s.turn = Some(TurnState::default());
                    drive_turn(&session, &mut rx, &mut s, &events, &mut stash, &cfg).await;
                }
            }
            Some(SessionCmd::SetAgent(name)) => {
                if s.turn.is_some() {
                    // Applied once the turn ends (stash replay), same as when
                    // it arrived mid-stream before #270.
                    stash.push_back(SessionCmd::SetAgent(name));
                    continue;
                }
                match cfg.profiles.get(&name) {
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
                }
            }
            // A result for the parked batch (#270): fold it into context on
            // arrival — arrival order, matching replay's `ToolOutput`-order
            // fold — and continue the turn once the batch drains. No match:
            // stale (late result after a cancel), duplicate, or unknown id —
            // drop it rather than corrupt context.
            Some(SessionCmd::ToolResult(id, output)) => {
                match s.turn.as_mut().and_then(|t| t.resolve(&id)) {
                    Some(call) => {
                        emit_tool_output(
                            &events,
                            &session,
                            &call.id,
                            &call.name,
                            output.clone(),
                            &mut s.seq,
                        );
                        s.ctx.push_tool(&call.id, output);
                        if s.turn.as_ref().is_some_and(TurnState::is_drained) {
                            drive_turn(&session, &mut rx, &mut s, &events, &mut stash, &cfg).await;
                        }
                    }
                    None => {
                        tracing::debug!(request_id = %id, "dropping stale/unknown ToolResult");
                    }
                }
            }
            // Cancel semantics (ADR-0017): a parked turn is cancelled by
            // clearing its state — the committed assistant message and any
            // already-arrived outputs stay in Context. Idle Stop is a no-op
            // (a mid-stream Stop is caught inside the streamed round).
            Some(SessionCmd::Stop) => {
                if s.turn.take().is_some() {
                    let _ = events.send(OutEvent::Status {
                        session: session.clone(),
                        state: AgentState::Idle,
                    });
                }
            }
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
