//! Session start: build or adopt the [`Session`], announce it, and apply the
//! session-start model pin and persisted generation overlay.

use entanglement_provider::{ResolvedModel, UserId};
use tokio::sync::{broadcast, mpsc};

use super::commands::now_ms;
use super::fork::{ForkFrame, Inherited};
use super::Session;
use crate::holly::SeqRegistry;
use crate::protocol::{Agent, AgentState, OutEvent, SessionId};
use crate::EngineConfig;

/// What a session task starts from: its agent, the replayed state when
/// resuming, and the spawn-time lineage/mode/model facts a fresh spawn applies.
pub(crate) struct SessionStart {
    pub(crate) agent: Agent,
    pub(crate) initial_session: Option<Session>,
    pub(crate) parent: Option<SessionId>,
    pub(crate) predecessor: Option<SessionId>,
    pub(crate) user: Option<UserId>,
    /// The mode a fresh spawn starts under — the parent's live mode at spawn
    /// time (ADR-0207 §6: mode applies to the whole spawn sub-tree), or
    /// `DEFAULT_MODE` for a root. Ignored on the resume path (a replayed
    /// session already carries the correct value in `s.mode`) — the caller
    /// passes `DEFAULT_MODE` there too, mirroring the `None` it passes for
    /// `predecessor`/`user`.
    pub(crate) initial_mode: String,
    /// An explicit `Spawn` model (#725), already resolved by the supervisor —
    /// or a compaction successor's inherited backend (#768).
    pub(crate) spawn_model: Option<ResolvedModel>,
    /// What a compaction successor continues from its source (#768/#776);
    /// its `model` has already moved into `spawn_model`.
    pub(crate) inherited: Option<Inherited>,
    /// The validated working directory a fresh spawn starts in (#764) — its
    /// own `Spawn.workdir`, else its source's or parent's. Ignored on resume.
    pub(crate) initial_workdir: Option<String>,
}

/// Build (or adopt the resumed) [`Session`], emit its start-up lifecycle
/// events, register its shared seq counter, and apply the session-start model
/// pin and generation overlay. The returned session is ready for the loop.
pub(super) fn start_session(
    session: &SessionId,
    events: &broadcast::Sender<OutEvent>,
    cfg: &EngineConfig,
    seqs: &SeqRegistry,
    forks: mpsc::Sender<ForkFrame>,
    start: SessionStart,
) -> Session {
    let SessionStart {
        agent,
        initial_session,
        parent,
        predecessor,
        user,
        initial_mode,
        spawn_model,
        inherited,
        initial_workdir,
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
        s.workdir = initial_workdir;
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
    // An explicit `Spawn` model (#725) out-ranks the agent's pin, exactly as
    // a `SetModel` would (ADR-0081: session memory > static pin); only a fresh
    // spawn ever carries one, so it never meets a replayed `s.model`.
    let effective_model = spawn_model
        .as_ref()
        .map(|r| r.model.clone())
        .or_else(|| s.model.clone())
        .or_else(|| agent_model.clone());

    // Publish this session's shared seq counter so the runtime can mint a fresh
    // seq for events it authors while the session is parked (#157). Registered
    // before the first turn (hence before any `ToolExec`), so a runtime emit
    // never races ahead of registration. On a resume it's the replay-seeded
    // counter, so runtime seqs continue past the reconstructed tail. Also
    // registered before `SessionStarted` is announced: this registry backs
    // `Holly::is_live`, which must already read `true` once a head sees it.
    // `register` also lifts the counter above any seq minted for this id
    // while it was retired (ADR-0239), so a resume never reuses one.
    seqs.register(session, &s.seq);

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
    // The workdir, fresh or replayed: the runtime reads it off each
    // `ToolExec`, but a head only learns it here (#764).
    super::workdir::announce(&s, session, events);
    // Re-announce a resumed session's replayed overlay, after the mode, for
    // the same reason. The runtime grades dispatch off its own copy, built
    // only from `ToolOverlayChanged` and forgotten on hibernate/end. If it is
    // not told again, a user's deny entry stops applying after a resume
    // (fail-open) and an enable entry is lost (#776).
    if is_resumed && !s.tool_overlay.is_empty() {
        let _ = events.send(OutEvent::ToolOverlayChanged {
            session: session.clone(),
            entries: s.tool_overlay.clone(),
        });
    }

    // Session-start model pin (#323, ADR-0081): bind the starting agent's pin
    // when no model is bound yet. A fresh `build`/spawned sub-agent (e.g. a
    // cheap-model `explore`) lands straight on its pinned endpoint; a resumed
    // session already re-bound from its `ModelChanged` log (so `s.model` is
    // `Some`) is skipped. Best-effort: a resolver failure warns and keeps the
    // startup default, matching replay's stance. An explicit `Spawn` model
    // (#725) takes this slot instead: its `ModelChanged` here, right after
    // `SessionStarted`, is what persistence logs and replay re-folds.
    if let Some(resolved) = spawn_model {
        s.rebind(session, resolved, events);
    } else if s.model.is_none() {
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
                Ok(resolved) => {
                    // The re-announce must not undo replay's generation fold
                    // (a logged `SetGeneration`, or a successor's inherited
                    // params) — live *or* in the log: this `ModelChanged` is
                    // persisted after that `GenerationChanged`, and replay's
                    // fold resets to the resolution's params, so without
                    // re-logging the live params the next resume loses them.
                    let live = s.generation;
                    let catalog = resolved.generation;
                    s.rebind(session, resolved, events);
                    if let Some(generation) = live.filter(|_| live != catalog) {
                        s.generation = live;
                        let _ = events.send(OutEvent::GenerationChanged {
                            session: session.clone(),
                            generation,
                        });
                    }
                }
                Err(e) => tracing::warn!(
                    provider, model, error = %e,
                    "session start: could not re-resolve resumed model for corrective announce"
                ),
            }
        }
    }

    // A compaction successor continues the source's effective generation
    // (#768). Logged as `GenerationChanged` because the `ModelChanged` above
    // replays through a fresh resolution, which yields the catalog's params,
    // not a live `SetGeneration`'s; recording it as this profile's session
    // memory also keeps the persisted default below from overriding it.
    let mut inherited = inherited.filter(|_| !is_resumed);
    let fork_seed = inherited.as_mut().and_then(|i| i.seed.take());
    if let Some(generation) = inherited.as_ref().and_then(|i| i.generation) {
        s.generation = Some(generation);
        s.generation_by_agent
            .insert(s.agent.name.clone(), generation);
        let _ = events.send(OutEvent::GenerationChanged {
            session: session.clone(),
            generation,
        });
    }
    // ...and its tool overlay (#776), logged after the `ModeChanged` above
    // so neither replay nor the runtime's mirror drops it as a mode switch.
    // Mode-scoped (ADR-0207): carried only when the successor starts in the
    // mode the entries were set under — a `SetMode` the source never got to
    // apply before the fork already moved the supervisor's mode on.
    if let Some(i) = inherited.filter(|i| !i.overlay.is_empty() && i.mode == s.mode) {
        s.tool_overlay = i.overlay;
        let _ = events.send(OutEvent::ToolOverlayChanged {
            session: session.clone(),
            entries: s.tool_overlay.clone(),
        });
    }

    // A user fork's verbatim history (#786), announced after the hand-over
    // above so the fork's log carries it ahead of anything it does next.
    if let Some(seed) = fork_seed {
        super::user_fork::seed_fork(session, &mut s, events, seed);
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::ToolOverlayEntry;

    /// Start a fresh successor whose source set its overlay under
    /// `source_mode` while the supervisor hands it `initial_mode`.
    fn successor_overlay(source_mode: &str, initial_mode: &str) -> (Session, Vec<OutEvent>) {
        let cfg = EngineConfig::default();
        let agent = cfg.agents.get("general").cloned().expect("general");
        let (events, mut sub) = broadcast::channel(32);
        let (forks, _rx) = mpsc::channel(1);
        let s = start_session(
            &SessionId::new("succ"),
            &events,
            &cfg,
            &SeqRegistry::default(),
            forks,
            SessionStart {
                agent,
                initial_session: None,
                parent: None,
                predecessor: Some(SessionId::new("src")),
                user: None,
                initial_mode: initial_mode.into(),
                spawn_model: None,
                inherited: Some(Inherited {
                    model: None,
                    generation: None,
                    mode: source_mode.into(),
                    overlay: vec![ToolOverlayEntry::allow("bash")],
                    workdir: None,
                    seed: None,
                }),
                initial_workdir: None,
            },
        );
        let mut out = Vec::new();
        while let Ok(ev) = sub.try_recv() {
            out.push(ev);
        }
        (s, out)
    }

    #[test]
    fn the_overlay_carries_under_the_same_mode() {
        let (s, events) = successor_overlay("plan", "plan");
        assert_eq!(s.tool_overlay, [ToolOverlayEntry::allow("bash")]);
        assert!(events
            .iter()
            .any(|e| matches!(e, OutEvent::ToolOverlayChanged { .. })));
    }

    /// A `SetMode` the supervisor already applied but the source never
    /// processed before forking: the overlay belongs to the old mode
    /// (ADR-0207), so it is not carried.
    #[test]
    fn the_overlay_is_dropped_when_the_inherited_mode_differs() {
        let (s, events) = successor_overlay("plan", "build");
        assert!(s.tool_overlay.is_empty());
        assert!(!events
            .iter()
            .any(|e| matches!(e, OutEvent::ToolOverlayChanged { .. })));
    }
}
