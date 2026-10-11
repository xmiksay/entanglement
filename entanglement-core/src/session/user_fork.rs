//! The user-initiated, **non-compacting** session fork (#786, ADR-0223):
//! `InMsg::ForkSession` copies a session's history into a fresh root and
//! leaves the source live and untouched.
//!
//! It rides the same session→supervisor channel as compaction's fork
//! (ADR-0205, `fork.rs`), because only the supervisor can mint a session —
//! but sends a `Spawn` with no `predecessor` (the source is not retired, so
//! no tool-result redirect and no `SessionStarted.predecessor`) and an empty
//! prompt (no turn is queued: a fork starts idle). The verbatim history rides
//! in-process on [`Inherited::seed`] and is announced and persisted by the
//! fork itself as `OutEvent::SessionForked`, so the fork's own log replays it
//! exactly; the source's log gains only the inbound `ForkSession` frame,
//! which its replay ignores.

use entanglement_provider::Message;
use tokio::sync::broadcast;

use super::emit::next_seq;
use super::fork::{ForkFrame, Inherited};
use super::Session;
use crate::id_gen::{IdContext, IdKind};
use crate::protocol::{InMsg, OutEvent, SessionId};
use crate::EngineConfig;

/// The history a fork starts from, and where it came from.
pub(crate) struct ForkSeed {
    pub(crate) source: SessionId,
    pub(crate) at_turn: Option<u32>,
    pub(crate) messages: Vec<Message>,
}

impl Inherited {
    /// Everything a session hands a session continuing from it: its live
    /// backend, generation, mode and overlay (#768/#776). `seed` is the
    /// caller's: only a user fork carries history this way.
    pub(crate) fn of(s: &Session) -> Self {
        Self {
            model: s.live_binding(),
            generation: s.generation,
            mode: s.mode.clone(),
            overlay: s.tool_overlay.clone(),
            seed: None,
        }
    }
}

/// Fork `session` (idle — the loop defers the command until it is) into a new
/// root. Returns the fork's id, or emits a recoverable `Error` on the source
/// and returns `None` when this session has no engine handle to ask with.
pub(super) fn fork_session(
    session: &SessionId,
    s: &Session,
    events: &broadcast::Sender<OutEvent>,
    cfg: &EngineConfig,
    at_turn: Option<u32>,
) -> Option<SessionId> {
    let Some(engine) = s.engine.clone() else {
        let _ = events.send(OutEvent::Error {
            session: session.clone(),
            seq: next_seq(&s.seq),
            message: "fork: this session cannot fork outside a running engine".into(),
        });
        return None;
    };
    // Minted in the source's lineage so a `{tenant}:` prefix carries over
    // (ADR-0211); the fork is a root, so nothing else would supply it.
    let ctx = IdContext::successor_of(session).with_user(s.user.as_ref());
    let fork = SessionId::new(cfg.id_gen.next(IdKind::Session, &ctx));
    let mut inherit = Inherited::of(s);
    inherit.seed = Some(ForkSeed {
        source: session.clone(),
        at_turn,
        messages: s.ctx.prefix(at_turn),
    });
    let frame = ForkFrame {
        inherit: Some(inherit),
        msg: InMsg::Spawn {
            session: fork.clone(),
            parent: None,
            predecessor: None,
            agent: s.agent.name.clone(),
            // Empty: no turn is queued, and the persistence tap logs no prompt.
            prompt: String::new(),
            // A fresh root takes the message's own user and mode — set to the
            // source's, so the fork is the same user's, in the same posture.
            user: s.user.clone(),
            model: None,
            mode: Some(s.mode.clone()),
        },
    };
    tracing::info!(%session, %fork, ?at_turn, "forking session");
    // Off-task, as compaction's `dispatch`: the supervisor may be awaiting a
    // route into this very session.
    tokio::spawn(async move {
        if engine.send(frame).await.is_err() {
            tracing::error!("fork: supervisor gone before the fork spawned");
        }
    });
    Some(fork)
}

/// Seed a freshly started fork and announce it. Called by `start_session`
/// after the model/generation/overlay hand-over, so the announcement follows
/// them in the log and nothing the fork does later precedes it.
pub(super) fn seed_fork(
    session: &SessionId,
    s: &mut Session,
    events: &broadcast::Sender<OutEvent>,
    seed: ForkSeed,
) {
    s.ctx.seed(seed.messages.clone());
    let _ = events.send(OutEvent::SessionForked {
        session: session.clone(),
        seq: next_seq(&s.seq),
        forked_from: seed.source,
        at_turn: seed.at_turn,
        context: seed.messages,
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use entanglement_provider::ContentPart;
    use tokio::sync::mpsc;

    fn session(cfg: &EngineConfig) -> Session {
        let profile = cfg.agents.get("general").cloned().expect("general");
        let mut s = Session::new_empty(cfg, profile);
        s.ctx.push_user_content(vec![ContentPart::text("one")]);
        s.ctx.push_assistant("a1", Vec::new());
        s.ctx.push_user_content(vec![ContentPart::text("two")]);
        s.ctx.push_assistant("a2", Vec::new());
        s.mode = "plan".into();
        s
    }

    #[tokio::test]
    async fn a_fork_spawns_an_idle_root_carrying_the_cut_history() {
        let cfg = EngineConfig::default();
        let mut s = session(&cfg);
        let (tx, mut rx) = mpsc::channel(4);
        s.engine = Some(tx);
        let (events, mut sub) = broadcast::channel(4);
        let src = SessionId::new("t1:src");

        let fork = fork_session(&src, &s, &events, &cfg, Some(1)).expect("a fork");
        assert!(fork.0.starts_with("t1:"), "tenant prefix kept: {fork}");
        assert!(sub.try_recv().is_err(), "the source logs nothing");

        let frame = rx.recv().await.expect("a Spawn");
        match frame.msg {
            InMsg::Spawn {
                session,
                parent,
                predecessor,
                prompt,
                mode,
                ..
            } => {
                assert_eq!(session, fork);
                assert_eq!((parent, predecessor), (None, None));
                assert!(prompt.is_empty(), "a fork queues no turn");
                assert_eq!(mode.as_deref(), Some("plan"));
            }
            other => panic!("expected Spawn, got {other:?}"),
        }
        let inherit = frame.inherit.expect("inherited state");
        assert_eq!(inherit.mode, "plan");
        let seed = inherit.seed.expect("a seed");
        assert_eq!(seed.source, src);
        assert_eq!(seed.at_turn, Some(1));
        assert_eq!(seed.messages.len(), 2, "turn 1 only");
        assert_eq!(s.ctx.messages().len(), 4, "the source is untouched");
    }

    #[test]
    fn no_engine_handle_is_a_recoverable_error() {
        let cfg = EngineConfig::default();
        let s = session(&cfg);
        let (events, mut sub) = broadcast::channel(4);
        assert!(fork_session(&SessionId::new("s"), &s, &events, &cfg, None).is_none());
        assert!(matches!(sub.try_recv(), Ok(OutEvent::Error { .. })));
    }
}
