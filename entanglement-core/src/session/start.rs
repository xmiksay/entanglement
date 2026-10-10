//! Session start: build or adopt the [`Session`], announce it, and apply the
//! session-start model pin and persisted generation overlay.

use std::sync::Arc;

use entanglement_provider::UserId;
use tokio::sync::{broadcast, mpsc};

use super::commands::now_ms;
use super::Session;
use crate::holly::SeqRegistry;
use crate::protocol::{Agent, AgentState, InMsg, OutEvent, SessionId};
use crate::EngineConfig;

/// What a session task starts from: its agent, the replayed state when
/// resuming, and the spawn-time lineage/mode facts a fresh spawn applies.
pub(super) struct SessionStart {
    pub(super) agent: Agent,
    pub(super) initial_session: Option<Session>,
    pub(super) parent: Option<SessionId>,
    pub(super) predecessor: Option<SessionId>,
    pub(super) user: Option<UserId>,
    pub(super) initial_mode: String,
}

/// Build (or adopt the resumed) [`Session`], emit its start-up lifecycle
/// events, register its shared seq counter, and apply the session-start model
/// pin and generation overlay. The returned session is ready for the loop.
pub(super) fn start_session(
    session: &SessionId,
    events: &broadcast::Sender<OutEvent>,
    cfg: &EngineConfig,
    seqs: &SeqRegistry,
    forks: mpsc::Sender<InMsg>,
    start: SessionStart,
) -> Session {
    let SessionStart {
        agent,
        initial_session,
        parent,
        predecessor,
        user,
        initial_mode,
    } = start;
    let ts = now_ms();

    let root = parent.is_none();
    let agent_name = agent.name.clone();
    let agent_model = agent.model.clone();
    // Captured before `initial_session` is consumed below — `Session` isn't
    // `Copy`, so this is the only place left to tell "fresh spawn" from
    // "resumed" once `s` exists.
    let is_resumed = initial_session.is_some();

    let mut s = initial_session.unwrap_or_else(|| Session::new_empty(cfg, agent));
    // Lets this session fork itself into a compaction successor (ADR-0205);
    // see `Session::engine`.
    s.engine = Some(forks);
    // ADR-0207 §6: a spawned child inherits its parent's mode. A resumed
    // session's `s.mode` is already correct (replay's last-write-wins fold
    // over its own `ModeChanged` log), so only a genuinely fresh spawn takes
    // `initial_mode` — mirroring the resumed-takes-precedence rule the
    // `Option`-shaped fields below use, spelled with the captured `bool`
    // instead since `mode` has no "unset" value of its own to fall back on.
    if !is_resumed {
        s.mode = initial_mode;
    }
    // A fresh (non-resumed) successor records the session it succeeds; a resumed
    // one already reconstructed it from its `SessionStarted` log (replay) — that
    // takes precedence over the raw `predecessor` param, which `Holly`'s `Resume`
    // handling intentionally passes as `None` so it can't clobber the replayed
    // value. The *announced* event must reflect the same resolved value, or a
    // resumed successor's re-emitted `SessionStarted` (and its persisted copy)
    // would wrongly blank out the lineage on the next replay.
    let effective_predecessor = s.predecessor.clone().or_else(|| predecessor.clone());
    s.predecessor = effective_predecessor.clone();
    // Same resumed-takes-precedence rule as `predecessor` above: a replayed
    // session already reconstructed `s.user` from its own `SessionStarted` log
    // record, which must win over the `user` param (`Holly::resume` passes
    // `None`) so a resumed session's re-announced event can't blank out its
    // multi-user identity.
    let effective_user = s.user.clone().or_else(|| user.clone());
    s.user = effective_user.clone();
    // Same resumed-takes-precedence rule as `predecessor`/`user`
    // above: a resumed session's replay already rebound `s.model` from its
    // `ModelChanged` log (ADR-0081 — session memory wins over the static
    // agent pin `agent_model`), so the *announced* value must reflect
    // that resolved binding, not the pin a fresh session still falls back to.
    // A fresh session has `s.model == None` here (the pin re-bind below hasn't
    // run yet), so this is unchanged there.
    let effective_model = s.model.clone().or_else(|| agent_model.clone());

    let _ = events.send(OutEvent::SessionStarted {
        session: session.clone(),
        parent,
        predecessor: effective_predecessor,
        agent: agent_name,
        model: effective_model,
        root,
        ts,
        user: effective_user,
    });
    // Publish this session's shared seq counter so the runtime can mint a fresh
    // seq for events it authors while the session is parked (#157). Registered
    // before the first turn (hence before any `ToolExec`), so a runtime emit
    // never races ahead of registration. On a resume it's the replay-seeded
    // counter, so runtime seqs continue past the reconstructed tail.
    seqs.lock()
        .expect("seq registry mutex poisoned")
        .insert(session.clone(), Arc::clone(&s.seq));

    let _ = events.send(OutEvent::Status {
        session: session.clone(),
        state: AgentState::Idle,
    });
    let _ = events.send(OutEvent::AgentChanged {
        session: session.clone(),
        agent: s.agent.name.clone(),
    });
    // Announce the starting mode unconditionally, mirroring `AgentChanged`
    // above — a head that (re)connects learns the live posture without
    // re-reading history. State only: the mode *notice* the model sees is
    // built fresh every round from `s.mode` (see `stream.rs`), never pushed
    // into `ctx` here — see `mode::mode_notice`'s doc for why a persisted
    // push would desync live vs. replayed history.
    let _ = events.send(OutEvent::ModeChanged {
        session: session.clone(),
        mode: s.mode.clone(),
    });

    // Session-start model pin (#323, ADR-0081): bind the starting agent's pin
    // when no model is bound yet. A fresh `build`/spawned sub-agent (e.g. a
    // cheap-model `explore`) lands straight on its pinned endpoint; a resumed
    // session already re-bound from its `ModelChanged` log (so `s.model` is
    // `Some`) is skipped. Best-effort: a resolver failure warns and keeps the
    // startup default, matching replay's stance.
    if s.model.is_none() {
        if let Some((provider, model)) = s
            .agent
            .model_pin()
            .map(|(p, m)| (p.to_string(), m.to_string()))
        {
            if let Some(resolver) = cfg.model_resolver.as_ref() {
                match resolver(s.user.as_ref(), &provider, &model) {
                    Ok(resolved) => s.rebind(session, resolved, events),
                    Err(e) => tracing::warn!(
                        provider, model, error = %e,
                        "session start: could not apply agent model pin; keeping default"
                    ),
                }
            }
        }
    } else if let (Some(provider), Some(model)) = (s.provider.clone(), s.model.clone()) {
        // Resumed session, corrective announce: replay already rebound
        // `s.provider`/`s.model` from the log's last `ModelChanged` (ADR-0081),
        // but a freshly resumed process's heads have no prior state — they only
        // see this session's re-emitted `SessionStarted` (now carrying the
        // resolved model above) and, critically, the TUI status bar updates
        // *only* on `ModelChanged`, never on `SessionStarted.model`. Re-resolve
        // the already-bound pair once more solely to recover `context_window`
        // (not carried on `Session` state between replay and here) and emit one
        // corrective `ModelChanged` via the same `rebind` the live `SetModel`
        // path uses — idempotent, it re-folds to the same binding replay
        // already applied. Best-effort, matching the pin arm's stance.
        if let Some(resolver) = cfg.model_resolver.as_ref() {
            match resolver(s.user.as_ref(), &provider, &model) {
                Ok(resolved) => s.rebind(session, resolved, events),
                Err(e) => tracing::warn!(
                    provider, model, error = %e,
                    "session start: could not re-resolve resumed model for corrective announce"
                ),
            }
        }
    }

    // Session-start persisted generation overlay (#374, ADR-0094 — mirrors the
    // model pin above): apply the starting agent's persisted generation
    // override via `cfg.generation_resolver` when no per-agent memory is
    // already recorded for it. A resumed session's memory reconstructed by
    // replay (see `Session::replay`'s `GenerationChanged` fold) skips this, same
    // as the pin's `s.model.is_none()` guard.
    if !s.generation_by_agent.contains_key(&s.agent.name) {
        if let Some(generation) = cfg
            .generation_resolver
            .as_ref()
            .and_then(|r| r(&s.agent.name))
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
    s
}
