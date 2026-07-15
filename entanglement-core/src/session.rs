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
use std::sync::atomic::AtomicU64;
use std::sync::Arc;

use tokio::sync::{broadcast, mpsc};

use crate::context::Context;
use crate::holly::SeqRegistry;
use crate::protocol::{AgentProfile, AgentState, OutEvent, SessionId};
use crate::EngineConfig;
use entanglement_provider::{ContentPart, GenerationParams, Llm};
use std::time::{SystemTime, UNIX_EPOCH};

use emit::{emit_tool_exec, emit_tool_output, next_seq};
use turn::drive_turn;

/// Commands routed to a single session by the supervisor (InMsg minus session id).
#[derive(Debug, Clone)]
pub(crate) enum SessionCmd {
    Prompt(Vec<ContentPart>),
    /// Output of a runtime-executed tool (`request_id`, multimodal `content`) —
    /// resolves a pending [`OutEvent::ToolExec`] round-trip (#58). `content` is
    /// text today, an image block when `read` opens an image (#221). Approval
    /// (`Approve`/`Reject`) is no longer a core command: the runtime tool executor
    /// owns it (#59) and never reaches the session loop.
    ToolResult(String, Vec<ContentPart>),
    SetAgent(String),
    /// Switch the live model/provider (`provider`, `model`) — #218. Re-resolves
    /// against [`EngineConfig::model_resolver`][crate::EngineConfig] and rebuilds
    /// `Session::llm` without restarting the engine.
    SetModel(String, String),
    Stop,
    /// Evict this session from memory without tombstoning its id (#318,
    /// ADR-0077). The task emits [`OutEvent::SessionHibernated`], drops its shared
    /// seq counter, and exits — dropping `Session` (the `Context`/history). The
    /// supervisor has already removed the map entry, so no `Prompt` reaches a dead
    /// task; the id stays resumable via [`Holly::resume`][crate::Holly::resume].
    /// Routed by the supervisor on [`InMsg::HibernateSession`]; the sender is
    /// dropped alongside so a turn parked mid-stream unwinds to this teardown
    /// (stop-then-hibernate) rather than stranding.
    Hibernate,
}

/// Mutable per-session loop + turn state (#61). Holds the conversation
/// [`Context`], the provider LLM backend (`llm`, a plain `Box<dyn Llm>` — the
/// resilience state it references is keyed per endpoint in the provider, not per
/// session, so there is no session-scoped handle to wrap it, #195/ADR-0062), the
/// active profile,
/// and the emit sequence — nothing pointing at the filesystem or a fixed tool
/// set. Plan/task snapshots are the runtime's display state, not engine state
/// (#231, ADR-0049), so the session carries neither. The tool schemas advertised
/// to the model are config, not session state: they come from
/// [`EngineConfig::tool_specs`] at turn time (see [`turn`]).
pub struct Session {
    pub ctx: Context,
    pub llm: Box<dyn Llm>,
    pub profile: AgentProfile,
    /// Effective model id when the user switched model/provider mid-session
    /// (#218), overriding the profile's pinned [`AgentProfile::model`] on every
    /// request and in pricing. `None` keeps the profile's model (the startup
    /// default). Set by [`SessionCmd::SetModel`]; reset only by another switch.
    pub model: Option<String>,
    /// Effective generation knobs for the active model (#218). Seeded from
    /// [`EngineConfig::generation`][crate::EngineConfig] at creation and replaced
    /// on a model switch so temperature / max-output / thinking follow the model.
    pub generation: Option<GenerationParams>,
    /// Monotonic per-session emit counter, shared (`Arc<AtomicU64>`, #157) with
    /// the supervisor's seq registry so runtime-authored events minted while this
    /// session is parked (an approval `ToolRequest`, a `Plan`/`TaskList` snapshot,
    /// a `FileChange`) draw a fresh seq from the *same* sequence via
    /// [`Holly::emit_for_session`][crate::Holly] — keeping `(session, seq)` unique
    /// instead of reusing the parked `ToolExec` seq (the pre-#157 defect).
    pub seq: Arc<AtomicU64>,
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
            model: None,
            generation: cfg.generation,
            seq: Arc::new(AtomicU64::new(0)),
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
#[allow(clippy::too_many_arguments)]
pub(crate) async fn session_loop(
    session: SessionId,
    mut rx: mpsc::Receiver<SessionCmd>,
    events: broadcast::Sender<OutEvent>,
    cfg: EngineConfig,
    profile: AgentProfile,
    initial_session: Option<Session>,
    parent: Option<SessionId>,
    seqs: SeqRegistry,
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
    // Publish this session's shared seq counter so the runtime can mint a fresh
    // seq for events it authors while the session is parked (#157). Registered
    // before the first turn (hence before any `ToolExec`), so a runtime emit
    // never races ahead of registration. On a resume it's the replay-seeded
    // counter, so runtime seqs continue past the reconstructed tail.
    seqs.lock()
        .expect("seq registry mutex poisoned")
        .insert(session.clone(), Arc::clone(&s.seq));
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
                emit_tool_exec(&events, &session, c, &s.profile.name, &s.seq);
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
            // Parked on unresolved tool calls (#274, ADR-0071). Bound the wait:
            // after `reoffer_interval` of silence (no `ToolResult` arriving)
            // re-offer the pending batch — re-emit each `ToolExec` with the same
            // `request_id` and a fresh `seq` — so an in-process offer the runtime
            // executor dropped under outbound-broadcast lag (`RecvError::Lagged`)
            // can't strand the turn until a restart/resume. At-least-once by
            // design; the executor dedupes by `request_id`, so a re-offer to a
            // still-in-flight call is a no-op there, not a double-run. `None`
            // disables the timer (park indefinitely, the pre-#274 behavior).
            match cfg.reoffer_interval {
                Some(interval) => match tokio::time::timeout(interval, rx.recv()).await {
                    Ok(cmd) => cmd,
                    Err(_elapsed) => {
                        if let Some(turn) = s.turn.as_ref() {
                            for c in &turn.pending {
                                emit_tool_exec(&events, &session, c, &s.profile.name, &s.seq);
                            }
                        }
                        continue;
                    }
                },
                None => rx.recv().await,
            }
        };
        match cmd {
            Some(SessionCmd::Prompt(content)) => {
                if s.turn.is_some() {
                    // Mid-turn steering (#182, ADR-0058): stash it — the next
                    // round folds stashed prompts into the live context before
                    // the model request.
                    stash.push_back(SessionCmd::Prompt(content));
                } else {
                    s.ctx.push_user_content(content);
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
                            seq: next_seq(&s.seq),
                            message: format!("unknown agent: {name}"),
                        });
                    }
                }
            }
            // Live model/provider switch (#218): re-resolve against the runtime's
            // catalog-backed resolver, rebuild the backend, and retarget the
            // request model + generation + context-window budget — no restart.
            // Deferred during a live turn (stash replay), like `SetAgent`.
            Some(SessionCmd::SetModel(provider, model)) => {
                if s.turn.is_some() {
                    stash.push_back(SessionCmd::SetModel(provider, model));
                    continue;
                }
                let Some(resolver) = cfg.model_resolver.as_ref() else {
                    let _ = events.send(OutEvent::Error {
                        session: session.clone(),
                        seq: next_seq(&s.seq),
                        message: "model switching is not supported by this engine".to_string(),
                    });
                    continue;
                };
                match resolver(&provider, &model) {
                    Ok(resolved) => {
                        s.llm = (resolved.llm_factory)();
                        s.model = Some(resolved.model.clone());
                        s.generation = resolved.generation;
                        s.ctx.set_window(resolved.context_window);
                        let _ = events.send(OutEvent::ModelChanged {
                            session: session.clone(),
                            provider: resolved.provider,
                            model: resolved.model,
                            context_window: resolved.context_window,
                        });
                    }
                    Err(e) => {
                        let _ = events.send(OutEvent::Error {
                            session: session.clone(),
                            seq: next_seq(&s.seq),
                            message: format!("cannot switch model: {e}"),
                        });
                    }
                }
            }
            // A result for the parked batch (#270): fold it into context on
            // arrival — arrival order, matching replay's `ToolOutput`-order
            // fold — and continue the turn once the batch drains. No match:
            // stale (late result after a cancel), duplicate, or unknown id —
            // drop it rather than corrupt context.
            Some(SessionCmd::ToolResult(id, content)) => {
                match s.turn.as_mut().and_then(|t| t.resolve(&id)) {
                    Some(call) => {
                        emit_tool_output(
                            &events,
                            &session,
                            &call.id,
                            &call.name,
                            content.clone(),
                            &s.seq,
                        );
                        s.ctx.push_tool_content(&call.id, content);
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
            // Memory eviction without tombstoning (#318, ADR-0077). Drop `Session`
            // (Context/history) and the shared seq counter, then emit the distinct
            // `SessionHibernated` and exit. A parked-on-approval turn is safe: its
            // pending `ToolExec`s live in the embedder's log and resume re-offers
            // them (ADR-0061/0071). A mid-stream turn reaches here via the
            // supervisor's sender-drop (stream cancels, the stashed `Hibernate`
            // pops when idle) — stop-then-hibernate, discarding the uncommitted
            // round exactly as replay drops a text-only tail.
            Some(SessionCmd::Hibernate) => {
                let ts = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis() as u64;
                seqs.lock()
                    .expect("seq registry mutex poisoned")
                    .remove(&session);
                let _ = events.send(OutEvent::SessionHibernated {
                    session: session.clone(),
                    ts,
                });
                return;
            }
            None => {
                let ts = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis() as u64;
                // Retire the shared seq counter: no more content will be minted
                // for this id (a late runtime emit for a gone session falls back
                // to seq 0, harmless — there is no live content stream to collide).
                seqs.lock()
                    .expect("seq registry mutex poisoned")
                    .remove(&session);
                let _ = events.send(OutEvent::SessionEnded {
                    session: session.clone(),
                    ts,
                });
                return;
            }
        }
    }
}
