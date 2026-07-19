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
//! Split along the natural seam (#109): [`replay`] reconstructs a session from
//! a persisted log; [`turn`] is the live reasoning turn's per-round setup and
//! retry driver; [`round`] is one streamed attempt plus the ADR-0118
//! ambiguous-stop retry (#436); [`stream`] is the streamed round-trip;
//! [`turn_state`] is the parked-turn state; [`emit`] is the outbound-event
//! helpers; [`ops`] is single-shot ops (#324, ADR-0082); [`summarize`] is the
//! LLM-summarization core `ops` (copy-on-write) and `turn` (in-place
//! auto-compact, #398/ADR-0103) both call.

mod emit;
mod ops;
mod replay;
mod round;
mod state;
mod stream;
mod summarize;
mod turn;
mod turn_state;

pub use state::{Session, SessionUsage};
pub use turn_state::TurnState;

use std::collections::VecDeque;
use std::sync::Arc;

use tokio::sync::{broadcast, mpsc};

use crate::holly::{ActivityRegistry, SeqRegistry};
use crate::protocol::{AgentProfile, AgentState, OutEvent, SessionId};
use crate::EngineConfig;
use entanglement_provider::ContentPart;
use std::time::{SystemTime, UNIX_EPOCH};

use emit::{emit_tool_exec, emit_tool_output, next_seq};
use ops::run_oneshot;
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
    /// Live-adjust generation knobs (#374, ADR-0094): partial overrides merged
    /// onto `Session::generation` via `GenerationParams::apply_overrides`.
    SetGeneration(entanglement_provider::GenerationParams),
    /// Single out-of-band LLM op (`op`, `args`, #324) — `"compact"` today.
    Oneshot(String, serde_json::Value),
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
    /// A sub-agent this session spawned came to life — append it to
    /// [`Session::children`][crate::session::Session::children]. Sent by the
    /// supervisor on [`InMsg::Spawn`] to the *parent* task, mirroring the
    /// `parent_links` edge it records. A pure state update: applied immediately,
    /// never stashed, so a mid-turn spawn reflects at once.
    ChildSpawned(SessionId),
    /// A child of this session was retired (its sub-tree closed) — remove it from
    /// [`Session::children`][crate::session::Session::children]. Sent by the
    /// supervisor on the [`InMsg::CloseSession`] cascade to the (still-live)
    /// parent of the closed sub-tree root.
    ChildClosed(SessionId),
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
    predecessor: Option<SessionId>,
    seqs: SeqRegistry,
    activity: ActivityRegistry,
) {
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;

    let root = parent.is_none();
    let profile_name = profile.name.clone();
    let profile_model = profile.model.clone();

    let mut s = initial_session.unwrap_or_else(|| Session::new_empty(&cfg, profile));
    // A fresh (non-resumed) successor records the session it succeeds; a resumed
    // one already reconstructed it from its `SessionStarted` log (replay) — that
    // takes precedence over the raw `predecessor` param, which `Holly`'s `Resume`
    // handling intentionally passes as `None` so it can't clobber the replayed
    // value. The *announced* event must reflect the same resolved value, or a
    // resumed successor's re-emitted `SessionStarted` (and its persisted copy)
    // would wrongly blank out the lineage on the next replay.
    let effective_predecessor = s.predecessor.clone().or_else(|| predecessor.clone());
    s.predecessor = effective_predecessor.clone();

    let _ = events.send(OutEvent::SessionStarted {
        session: session.clone(),
        parent,
        predecessor: effective_predecessor,
        profile: profile_name,
        model: profile_model,
        root,
        ts,
    });
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

    // Session-start model pin (#323, ADR-0081): bind the starting profile's pin
    // when no model is bound yet. A fresh `build`/spawned sub-agent (e.g. a
    // cheap-model `explore`) lands straight on its pinned endpoint; a resumed
    // session already re-bound from its `ModelChanged` log (so `s.model` is
    // `Some`) is skipped. Best-effort: a resolver failure warns and keeps the
    // startup default, matching replay's stance.
    if s.model.is_none() {
        if let Some((provider, model)) = s
            .profile
            .model_pin()
            .map(|(p, m)| (p.to_string(), m.to_string()))
        {
            if let Some(resolver) = cfg.model_resolver.as_ref() {
                match resolver(&provider, &model) {
                    Ok(resolved) => s.rebind(&session, resolved, &events),
                    Err(e) => tracing::warn!(
                        provider, model, error = %e,
                        "session start: could not apply profile model pin; keeping default"
                    ),
                }
            }
        }
    }

    // Session-start persisted generation overlay (#374, ADR-0094 — mirrors the
    // model pin above): apply the starting profile's persisted generation
    // override via `cfg.generation_resolver` when no per-profile memory is
    // already recorded for it. A resumed session's memory reconstructed by
    // replay (see `Session::replay`'s `GenerationChanged` fold) skips this, same
    // as the pin's `s.model.is_none()` guard.
    if !s.profile_generation.contains_key(&s.profile.name) {
        if let Some(generation) = cfg
            .generation_resolver
            .as_ref()
            .and_then(|r| r(&s.profile.name))
        {
            if s.generation != Some(generation) {
                s.generation = Some(generation);
                let _ = events.send(OutEvent::GenerationChanged {
                    session: session.clone(),
                    generation,
                });
            }
        }
    }

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
        // Publish settledness for the idle-TTL sweep (#363): `Some(now)` the
        // instant this session is genuinely at rest (about to pop a stash entry
        // or block on `rx.recv()`), `None` while parked on unresolved tool calls
        // (mid-turn or waiting on an approval/question result). Using tokio's
        // clock (not `std::time::Instant`) keeps the sweep test-friendly under a
        // paused/advanced runtime clock.
        activity
            .lock()
            .expect("activity registry mutex poisoned")
            .insert(
                session.clone(),
                s.turn.is_none().then(tokio::time::Instant::now),
            );

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
                    // Flip to busy *before* the round runs, not just at the next
                    // loop top (#363): the top-of-loop publish above ran while
                    // this session was still idle, and `drive_turn` may stream
                    // for a long time — an idle-TTL sweep must never see a stale
                    // "settled" timestamp for a session that just started a turn.
                    activity
                        .lock()
                        .expect("activity registry mutex poisoned")
                        .insert(session.clone(), None);
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
                        let p = p.clone();
                        s.profile = p.clone();
                        let _ = events.send(OutEvent::AgentChanged {
                            session: session.clone(),
                            agent: p.name.clone(),
                            profile_detail: Some(p.detail()),
                        });
                        // Per-profile model pin (#323, ADR-0081): re-bind the
                        // backend to this profile's model. Precedence: session
                        // memory (a `/model` choice made under this profile) >
                        // the profile's static `model_pin()`. A pin-less profile
                        // with no memory keeps the current binding — no rebuild,
                        // no `ModelChanged`. The `AgentChanged` above already
                        // succeeded, so a resolver error here surfaces the same
                        // `Error` as `SetModel` and keeps the old binding.
                        let pin = s.profile_models.get(&p.name).cloned().or_else(|| {
                            p.model_pin().map(|(pr, m)| (pr.to_string(), m.to_string()))
                        });
                        if let Some((provider, model)) = pin {
                            let unchanged = s.provider.as_deref() == Some(provider.as_str())
                                && s.model.as_deref() == Some(model.as_str());
                            if !unchanged {
                                if let Some(resolver) = cfg.model_resolver.as_ref() {
                                    match resolver(&provider, &model) {
                                        Ok(resolved) => s.rebind(&session, resolved, &events),
                                        Err(e) => {
                                            let _ = events.send(OutEvent::Error {
                                                session: session.clone(),
                                                seq: next_seq(&s.seq),
                                                message: format!("cannot switch model: {e}"),
                                            });
                                        }
                                    }
                                }
                            }
                        }
                        // Per-profile generation overlay (#374, ADR-0094 —
                        // mirrors the model pin's precedence exactly, #323): session
                        // memory (a live `SetGeneration` recorded under this
                        // profile) wins, then this profile's persisted override via
                        // `cfg.generation_resolver`, then the current binding
                        // unchanged (no-op — no spurious `GenerationChanged`, same
                        // guard as the pin-less-profile case above).
                        let overlay =
                            s.profile_generation.get(&p.name).copied().or_else(|| {
                                cfg.generation_resolver.as_ref().and_then(|r| r(&p.name))
                            });
                        if let Some(generation) = overlay {
                            if s.generation != Some(generation) {
                                s.generation = Some(generation);
                                let _ = events.send(OutEvent::GenerationChanged {
                                    session: session.clone(),
                                    generation,
                                });
                            }
                        }
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
                        s.rebind(&session, resolved, &events);
                        // Record the choice as this profile's session memory (#323):
                        // a later `SetAgent` back to it re-applies this binding,
                        // winning over the profile's static pin. Uses the resolved
                        // canonical `(provider, model)` so switch-back re-resolves
                        // the same endpoint.
                        if let (Some(p), Some(m)) = (s.provider.clone(), s.model.clone()) {
                            s.profile_models.insert(s.profile.name.clone(), (p, m));
                        }
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
            // Live generation-parameter adjustment (#374, ADR-0094): unlike
            // `SetModel`, there is no resolver to fail against, so this always
            // succeeds. Deferred during a live turn (stash replay), like
            // `SetAgent`/`SetModel`.
            Some(SessionCmd::SetGeneration(overrides)) => {
                if s.turn.is_some() {
                    stash.push_back(SessionCmd::SetGeneration(overrides));
                    continue;
                }
                let mut merged = s.generation.unwrap_or_default();
                merged.apply_overrides(overrides);
                s.generation = Some(merged);
                // Session memory (#323-style, mirrors `profile_models`): a later
                // `SetAgent` switch back to this profile re-applies it, winning
                // over the profile's persisted/catalog default.
                s.profile_generation.insert(s.profile.name.clone(), merged);
                let _ = events.send(OutEvent::GenerationChanged {
                    session: session.clone(),
                    generation: merged,
                });
            }
            Some(SessionCmd::Oneshot(op, args)) => {
                if s.turn.is_some() {
                    stash.push_back(SessionCmd::Oneshot(op, args));
                    continue;
                }
                run_oneshot(&session, &mut s, &events, &cfg, op, args).await;
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
            // Lineage mirror (children): a spawn/close edge the supervisor
            // records in `parent_links` is reflected onto this session's live
            // children list. Pure state — applied immediately even mid-turn, and
            // idempotent (a duplicate spawn or an unknown close is a no-op).
            Some(SessionCmd::ChildSpawned(child)) => {
                if !s.children.contains(&child) {
                    s.children.push(child);
                }
            }
            Some(SessionCmd::ChildClosed(child)) => {
                s.children.retain(|c| c != &child);
            }
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
                activity
                    .lock()
                    .expect("activity registry mutex poisoned")
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
                activity
                    .lock()
                    .expect("activity registry mutex poisoned")
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
