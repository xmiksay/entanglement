//! Compaction's one fork mechanism (ADR-0205): **every** compaction — manual
//! `/compact`, auto-summarize on overflow, and the prune-only fallback —
//! produces a successor session seeded from the compacted history and retires
//! the source unchanged.
//!
//! The two frames this sends are exactly the pair the TUI used to send
//! head-side for `/compact` (ADR-0101/0110): `Spawn { parent: None,
//! predecessor: Some(source) }` mints the successor as a *root* that records
//! its lineage without joining the source's spawn sub-tree, and
//! `CloseSession { source }` retires the source right after. Moving them here
//! is what makes the automatic paths work for every head — `pipe`, `serve` and
//! an embedder have no compaction-fork code of their own, and a mid-turn
//! overflow has no head to ask.
//!
//! A session task cannot mint a session itself; only the supervisor can. It
//! asks through [`Session::engine`], a dedicated session→supervisor channel
//! whose frames the supervisor handles exactly like inbox ones — fan-out
//! included, so the persistence tap still synthesizes the successor's seed
//! prompt from its `Spawn` (ADR-0113). Delivery runs on a detached task so a
//! session task never blocks while the supervisor may be waiting to route into
//! that very session.

use entanglement_provider::{GenerationParams, ResolvedModel};
use tokio::sync::{broadcast, mpsc};

use super::emit::next_seq;
use super::Session;
use crate::id_gen::{IdContext, IdKind};
use crate::protocol::{CompactionMode, InMsg, OutEvent, SessionId, ToolOverlayEntry};
use crate::EngineConfig;

/// One frame on the session→supervisor fork channel: an ordinary `InMsg`
/// (handled, and fanned out, exactly like an inbox one) plus, on the
/// successor's `Spawn` only, the live state it takes over from the source.
pub(crate) struct ForkFrame {
    pub(crate) msg: InMsg,
    pub(crate) inherit: Option<Inherited>,
}

/// What a compaction successor continues with from its source (#768): the
/// same session goes on, so a live `SetModel`/`SetGeneration` must survive the
/// fork. Carried in-process rather than as `Spawn.model`, because that field
/// is *re-resolved* and refuses the spawn when resolution fails (#725) — by
/// then the source is being retired, so a refusal would lose the session. An
/// already-bound backend needs no resolution and so cannot fail.
pub(crate) struct Inherited {
    /// The source's bound backend, `None` on the startup default.
    pub(crate) model: Option<ResolvedModel>,
    /// The source's effective generation params.
    pub(crate) generation: Option<GenerationParams>,
    /// The source's mode at the fork, which scopes `overlay` (ADR-0207).
    pub(crate) mode: String,
    /// The source's live tool overlay (#776).
    pub(crate) overlay: Vec<ToolOverlayEntry>,
}

/// Whether a compaction forked the session away. `Yes` means this session is
/// retired: its log ends here and it must emit nothing further.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Forked {
    Yes,
    No,
}

/// What one completed compaction produced, whichever path ran.
pub(crate) struct Compaction<'a> {
    /// Both the `Compacted` event's payload and the successor's seed prompt:
    /// one string, so a head renders exactly the history the successor starts
    /// from. For [`CompactionMode::Summary`] it is the LLM summary plus the
    /// ADR-0102 verbatim kept tail (`summarize::compose_report`); for
    /// [`CompactionMode::Prune`] it is the pruned transcript.
    pub seed: &'a str,
    pub kept: usize,
    pub auto: bool,
    pub mode: CompactionMode,
}

/// A fork announced but not yet dispatched. Held so the caller can finish
/// writing the source's own log tail (`Usage`/`Done`) *before* the successor
/// starts emitting, keeping the two sessions' records unambiguously ordered.
#[must_use = "a fork that is never dispatched leaves the successor unspawned"]
pub(crate) struct PendingFork {
    engine: mpsc::Sender<ForkFrame>,
    spawn: ForkFrame,
    close: InMsg,
}

impl PendingFork {
    /// Send `Spawn` then `CloseSession`, off-task. Ordering between the two is
    /// preserved (one task, two sequential sends); ordering against the
    /// supervisor's other work is not needed — the successor is a fresh id
    /// nothing else can name yet, and the source is already done emitting.
    pub(crate) fn dispatch(self) {
        tokio::spawn(async move {
            if self.engine.send(self.spawn).await.is_err() {
                tracing::error!("compaction fork: supervisor gone before the successor spawned");
                return;
            }
            let close = ForkFrame {
                msg: self.close,
                inherit: None,
            };
            if self.engine.send(close).await.is_err() {
                tracing::error!("compaction fork: supervisor gone before the source retired");
            }
        });
    }
}

/// Announce `compaction` on `session` and prepare its successor fork.
///
/// Emits `OutEvent::Compacted` — the source's last content event — and returns
/// the fork for the caller to [`dispatch`][PendingFork::dispatch] once it has
/// finished with the source. `None` when this session has no engine handle (a
/// `Session` built outside a running engine): nothing is emitted, because
/// announcing a fork that cannot happen would be a lie, and the caller treats
/// it as a failed compaction.
pub(crate) fn fork_successor(
    session: &SessionId,
    s: &mut Session,
    events: &broadcast::Sender<OutEvent>,
    cfg: &EngineConfig,
    compaction: Compaction<'_>,
) -> Option<PendingFork> {
    let engine = s.engine.clone()?;
    // Minted in the source's lineage so the default generator keeps its
    // tenant prefix — the successor is a root, so nothing else would (ADR-0211).
    let ctx = IdContext::successor_of(session).with_user(s.user.as_ref());
    let successor = SessionId::new(cfg.id_gen.next(IdKind::Session, &ctx));

    let _ = events.send(OutEvent::Compacted {
        session: session.clone(),
        seq: next_seq(&s.seq),
        summary: compaction.seed.to_string(),
        kept: compaction.kept as u64,
        auto: compaction.auto,
        mode: compaction.mode,
    });

    tracing::info!(
        %session, %successor, mode = ?compaction.mode, auto = compaction.auto,
        "compaction forked a successor session"
    );

    let inherit = Inherited {
        model: s.live_binding(),
        generation: s.generation,
        mode: s.mode.clone(),
        overlay: s.tool_overlay.clone(),
    };
    Some(PendingFork {
        engine,
        spawn: ForkFrame {
            inherit: Some(inherit),
            msg: InMsg::Spawn {
                session: successor,
                // A root, not a child: the `CloseSession` below cascades over the
                // source's spawn sub-tree, which the successor must not be in
                // (ADR-0110).
                parent: None,
                predecessor: Some(session.clone()),
                // The successor runs under the source's profile; its live model
                // and generation ride on `inherit` above, its mode and user are
                // inherited by the supervisor (ADR-0207 §6, #522).
                agent: s.agent.name.clone(),
                prompt: seed_prompt(compaction.mode, compaction.seed),
                user: None,
                // Never set here: a `Spawn` model is re-resolved and refuses on
                // failure (#725); see `Inherited`.
                model: None,
                mode: None,
            },
        },
        close: InMsg::CloseSession {
            session: session.clone(),
        },
    })
}

/// Frame the seed so the successor's first message says what it is continuing
/// from. The successor receives this as an ordinary `Spawn` prompt, which the
/// persistence tap records as a synthesized `InMsg::Prompt` (ADR-0113) — that
/// is what makes the successor's own log replay to the history it started
/// live with.
fn seed_prompt(mode: CompactionMode, seed: &str) -> String {
    match mode {
        CompactionMode::Summary => format!(
            "[Conversation summary — this session continues from a compaction of an \
             earlier session]\n\n{seed}"
        ),
        CompactionMode::Prune => format!(
            "[Conversation transcript — this session continues from an earlier session \
             whose oldest tool output was pruned to fit the context window]\n\n{seed}"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_summary_seed_is_framed_as_a_continuation() {
        let framed = seed_prompt(CompactionMode::Summary, "the gist");
        assert!(framed.starts_with("[Conversation summary"));
        assert!(framed.ends_with("the gist"));
    }

    #[test]
    fn a_prune_seed_says_what_was_lost_rather_than_claiming_a_summary() {
        let framed = seed_prompt(CompactionMode::Prune, "[user]\nhi\n\n");
        assert!(framed.contains("pruned to fit the context window"));
        assert!(
            !framed.contains("Conversation summary"),
            "a prune fork must not present itself as a summary: {framed}"
        );
    }

    /// A `Session` with no engine handle cannot fork, and must not announce
    /// one either — the caller needs a clean "compaction unavailable" signal,
    /// not a `Compacted` event with no successor behind it.
    #[test]
    fn no_engine_handle_means_no_event_and_no_fork() {
        let cfg = EngineConfig::default();
        let profile = cfg
            .agents
            .get("general")
            .cloned()
            .expect("the general profile");
        let mut s = Session::new_empty(&cfg, profile);
        let (events, mut sub) = broadcast::channel(8);
        let session = SessionId::new("s1");

        let fork = fork_successor(
            &session,
            &mut s,
            &events,
            &cfg,
            Compaction {
                seed: "summary",
                kept: 0,
                auto: true,
                mode: CompactionMode::Summary,
            },
        );

        assert!(fork.is_none());
        assert!(
            sub.try_recv().is_err(),
            "no Compacted may be announced when the fork cannot happen"
        );
    }

    #[tokio::test]
    async fn a_fork_announces_the_source_then_spawns_a_root_successor_and_closes_the_source() {
        let cfg = EngineConfig::default();
        let profile = cfg
            .agents
            .get("general")
            .cloned()
            .expect("the general profile");
        let mut s = Session::new_empty(&cfg, profile);
        let (engine_tx, mut engine_rx) = mpsc::channel(8);
        s.engine = Some(engine_tx);
        let (events, mut sub) = broadcast::channel(8);
        let session = SessionId::new("s1");

        let fork = fork_successor(
            &session,
            &mut s,
            &events,
            &cfg,
            Compaction {
                seed: "the gist",
                kept: 2,
                auto: true,
                mode: CompactionMode::Prune,
            },
        )
        .expect("a session with an engine handle can fork");

        // The announcement lands before anything is dispatched.
        match sub.try_recv().expect("a Compacted event") {
            OutEvent::Compacted {
                summary,
                kept,
                auto,
                mode,
                ..
            } => {
                assert_eq!(summary, "the gist");
                assert_eq!(kept, 2);
                assert!(auto);
                assert_eq!(mode, CompactionMode::Prune);
            }
            other => panic!("expected Compacted, got {other:?}"),
        }
        assert!(
            engine_rx.try_recv().is_err(),
            "nothing is sent until the caller dispatches"
        );

        fork.dispatch();

        let frame = engine_rx.recv().await.expect("a Spawn");
        let inherit = frame
            .inherit
            .expect("the successor's Spawn carries the source state");
        assert!(
            inherit.model.is_none(),
            "an unbound source carries no backend"
        );
        let successor = match frame.msg {
            InMsg::Spawn {
                session: successor,
                parent,
                predecessor,
                agent,
                prompt,
                ..
            } => {
                assert_eq!(parent, None, "the successor is a root, not a child");
                assert_eq!(predecessor, Some(session.clone()));
                assert_eq!(agent, "general", "the source's profile carries over");
                assert!(prompt.contains("the gist"));
                successor
            }
            other => panic!("expected Spawn, got {other:?}"),
        };
        assert_ne!(successor, session);

        let frame = engine_rx.recv().await.expect("a CloseSession");
        assert!(frame.inherit.is_none());
        match frame.msg {
            InMsg::CloseSession { session: closed } => assert_eq!(closed, session),
            other => panic!("expected CloseSession, got {other:?}"),
        }
    }

    /// A bound source hands the successor its backend with the *live*
    /// generation (a `SetGeneration` moves it off the resolution's own), so the
    /// successor resolves nothing (#768).
    #[tokio::test]
    async fn a_bound_source_hands_over_its_backend_and_live_generation() {
        let cfg = EngineConfig::default();
        let profile = cfg
            .agents
            .get("general")
            .cloned()
            .expect("the general profile");
        let mut s = Session::new_empty(&cfg, profile);
        let (engine_tx, mut engine_rx) = mpsc::channel(8);
        s.engine = Some(engine_tx);
        s.bind(ResolvedModel {
            provider: "zai".into(),
            model: "glm-b".into(),
            llm_factory: cfg.llm_factory.clone(),
            generation: None,
            context_window: Some(8_000),
        });
        let live = GenerationParams {
            temperature: Some(0.3),
            ..Default::default()
        };
        s.generation = Some(live);
        let (events, _sub) = broadcast::channel(8);

        fork_successor(
            &SessionId::new("s1"),
            &mut s,
            &events,
            &cfg,
            Compaction {
                seed: "gist",
                kept: 0,
                auto: false,
                mode: CompactionMode::Summary,
            },
        )
        .expect("a fork")
        .dispatch();

        let frame = engine_rx.recv().await.expect("a Spawn");
        assert!(
            matches!(frame.msg, InMsg::Spawn { model: None, .. }),
            "never a re-resolved Spawn model"
        );
        let inherit = frame.inherit.expect("inherited state");
        let model = inherit.model.expect("the bound backend");
        assert_eq!(model.provider, "zai");
        assert_eq!(model.model, "glm-b");
        assert_eq!(model.context_window, Some(8_000));
        assert_eq!(model.generation, Some(live));
        assert_eq!(inherit.generation, Some(live));
    }
}
