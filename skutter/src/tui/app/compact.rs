//! Following a compaction into its successor session (ADR-0205).
//!
//! The head no longer forks anything. Every compaction — manual `/compact`,
//! auto-summarize on overflow, the prune-only fallback — is the *engine*
//! minting a successor session and retiring the source, so all the TUI does is
//! follow along: remember the summary when `Compacted` arrives, then, when the
//! successor announces itself with `predecessor` pointing at that source, open
//! its view and seed the transcript with what it is continuing from.
//!
//! Following lineage rather than a fork this head issued is what makes it work
//! for a compaction the user never asked for and may not even be watching — a
//! background session that overflowed mid-turn gets its view either way, and
//! only a compaction of the *active* session moves the user.

use entanglement_core::{OutEvent, SessionId};

use super::App;

impl App {
    /// Records a `/compact` parse error (bad `--keep` value) as a transcript
    /// status line (#397) — no engine traffic, mirrors
    /// `App::record_set_error`'s wrapper pattern.
    pub fn record_compact_error(&mut self, message: String) {
        self.sessions
            .active_view_mut()
            .record_status("compact", format!("error: {message}"));
        self.mark_dirty();
    }

    /// Remember a compaction's seed text against the session that compacted,
    /// for [`follow_successor`][Self::follow_successor] to hand to the
    /// successor once it appears. The notice on the source's own view is
    /// rendered separately by the reducer's `Compacted` arm.
    pub(crate) fn note_compaction_seed(&mut self, source: SessionId, seed: String) {
        self.compaction_seeds.insert(source, seed);
        self.mark_dirty();
    }

    /// Open the successor of a compaction and, if the user was watching the
    /// session that compacted, move them into it.
    ///
    /// The successor's first user message is seeded head-side because the
    /// engine never echoes an `InMsg` back as an `OutEvent`: the seed rides
    /// the successor's `Spawn` prompt, so without this the transcript would
    /// open on the model's reply to a prompt the user never saw. Same pattern
    /// as the `propose_plan` handoff and an ordinary user prompt.
    pub(crate) fn follow_successor(&mut self, source: SessionId, successor: SessionId) {
        self.sessions.ensure(&successor);
        if let Some(seed) = self.compaction_seeds.remove(&source) {
            if let Some(view) = self.sessions.view_for_mut(&successor) {
                view.record_user_message(seed);
            }
        }
        // A background session compacting must not yank the view out from
        // under whatever the user is reading.
        if self.sessions.active_id() == &source {
            self.sessions.switch_to(successor);
        }
        self.mark_dirty();
    }

    /// Learn a session's compaction lineage before a card event folds, where
    /// the views' chain lost a `SessionStarted` (broadcast lag). Without it a
    /// moved card's copy, or an ended card's, would stay on a predecessor's
    /// view for good (ADR-0238 §7, ADR-0240 §8): the views' walk stops at
    /// the first missing link or missing view. `predecessors_of` is the
    /// engine's own directory read (`Holly::predecessors_of`, ADR-0242), so
    /// no wire event is needed; it is read on every such event, since only
    /// the engine can tell a lost link from a chain's true head. Each view
    /// in the engine's chain is linked to its nearest ancestor that has a
    /// view, but only where its own link is missing or names a session with
    /// no view: a link to an existing view is kept. A user watching the
    /// ancestor a learned link names follows to the successor, as the lost
    /// event would have moved them (`follow_successor`); one who went back
    /// to an older ancestor on purpose stays.
    pub(crate) fn learn_lineage(
        &mut self,
        event: &OutEvent,
        predecessors_of: impl FnOnce(&SessionId) -> Vec<SessionId>,
    ) {
        let moves = matches!(
            event,
            OutEvent::ToolRequest { detached: true, .. } | OutEvent::ApprovalResolved { .. }
        );
        let Some(session) = event.session().filter(|_| moves).cloned() else {
            return;
        };
        let chain = predecessors_of(&session);
        let mut learned = Vec::new();
        let mut at = session.clone();
        for p in &chain {
            if self.sessions.view_for(p).is_none() {
                continue;
            }
            self.sessions.ensure(&at);
            let link = self.sessions.view_for(&at).and_then(|v| v.predecessor());
            let dangling = link.is_none_or(|q| self.sessions.view_for(q).is_none());
            if dangling {
                if let Some(view) = self.sessions.view_for_mut(&at) {
                    view.relink_predecessor(p.clone());
                    learned.push(p.clone());
                }
            }
            at = p.clone();
        }
        let focused = self.sessions.active_id().clone();
        if learned.contains(&focused) {
            self.follow_successor(focused, session);
        }
    }

    /// Seeds recorded but not yet claimed by a successor. Test-only window
    /// onto the hand-off's one piece of state.
    #[cfg(test)]
    pub(crate) fn compaction_seeds(&self) -> &std::collections::HashMap<SessionId, String> {
        &self.compaction_seeds
    }
}

#[cfg(test)]
mod tests {
    use crate::tui::app::App;
    use crate::tui::session_view::{ApprovalMode, TranscriptEntry};
    use entanglement_core::{AgentState, CompactionMode, OutEvent, SessionId};

    fn compacted(session: &str, seq: u64, summary: &str, auto: bool) -> OutEvent {
        OutEvent::Compacted {
            session: SessionId::new(session),
            seq,
            summary: summary.into(),
            kept: 0,
            auto,
            mode: CompactionMode::Summary,
        }
    }

    fn started(session: &str, predecessor: Option<&str>) -> OutEvent {
        OutEvent::SessionStarted {
            session: SessionId::new(session),
            parent: None,
            predecessor: predecessor.map(SessionId::new),
            agent: "build".into(),
            model: None,
            root: true,
            ts: 0,
            user: None,
        }
    }

    fn first_user_text(app: &App, session: &str) -> Option<String> {
        app.sessions
            .view_for(&SessionId::new(session))?
            .transcript()
            .iter()
            .find_map(|e| match e {
                TranscriptEntry::User { text, .. } => Some(text.clone()),
                _ => None,
            })
    }

    /// The whole hand-off: the source renders its notice, the successor gets a
    /// view seeded with the summary, and the user moves into it.
    #[test]
    fn the_successor_of_the_active_session_is_seeded_and_becomes_active() {
        let mut app = App::new_for_test(SessionId::new("s1"));
        app.handle_out_event(OutEvent::Status {
            session: SessionId::new("s1"),
            state: AgentState::Done,
        });

        app.handle_out_event(compacted("s1", 1, "user asked for X, agent did Y", false));
        // Nothing moves until the engine's successor actually appears.
        assert_eq!(app.active_session_id(), &SessionId::new("s1"));

        app.handle_out_event(started("s2", Some("s1")));

        assert_eq!(
            app.active_session_id(),
            &SessionId::new("s2"),
            "the user follows the compaction forward"
        );
        let seeded = first_user_text(&app, "s2").expect("the successor is seeded");
        assert!(seeded.contains("user asked for X, agent did Y"), "{seeded}");
        assert!(
            app.compaction_seeds().is_empty(),
            "the seed is consumed by the successor"
        );

        // The source's view survives, carrying the compaction notice.
        let source = app
            .sessions
            .view_for(&SessionId::new("s1"))
            .expect("source view survives");
        assert!(source.transcript().iter().any(|e| matches!(
            e,
            TranscriptEntry::ToolOutput { tool: Some(t), .. } if t == "compact"
        )));
    }

    /// The automatic paths take the same route — there is no `auto: true`
    /// special case any more, because the engine forks on every path.
    #[test]
    fn an_auto_compaction_follows_its_successor_too() {
        let mut app = App::new_for_test(SessionId::new("s1"));
        app.handle_out_event(compacted("s1", 1, "overflowed, summarized", true));
        app.handle_out_event(started("s2", Some("s1")));

        assert_eq!(app.active_session_id(), &SessionId::new("s2"));
        let seeded = first_user_text(&app, "s2").expect("the successor is seeded");
        assert!(seeded.contains("overflowed, summarized"));
    }

    /// A background session compacting gets its successor view, but must not
    /// steal the user's place.
    #[test]
    fn a_background_compaction_does_not_switch_the_active_view() {
        let mut app = App::new_for_test(SessionId::new("s1"));
        app.handle_out_event(compacted("bg", 1, "background summary", true));
        app.handle_out_event(started("bg2", Some("bg")));

        assert_eq!(
            app.active_session_id(),
            &SessionId::new("s1"),
            "the active view stays put"
        );
        let seeded = first_user_text(&app, "bg2").expect("the successor still gets its view");
        assert!(seeded.contains("background summary"));
    }

    /// A replayed/lagged duplicate must not overwrite a newer seed.
    #[test]
    fn a_replayed_compacted_does_not_clobber_the_recorded_seed() {
        let mut app = App::new_for_test(SessionId::new("s1"));
        app.handle_out_event(compacted("s1", 5, "the real summary", false));
        app.handle_out_event(compacted("s1", 1, "a stale replay", false));
        app.handle_out_event(started("s2", Some("s1")));

        let seeded = first_user_text(&app, "s2").expect("the successor is seeded");
        assert!(seeded.contains("the real summary"), "{seeded}");
    }

    fn detached_card(session: &str, seq: u64) -> OutEvent {
        OutEvent::ToolRequest {
            session: SessionId::new(session),
            seq,
            request_id: "x-1:rhai:edit".into(),
            tool: "edit (rhai)".into(),
            input: "{}".into(),
            reason: None,
            scopes: Vec::new(),
            detached: true,
        }
    }

    fn card_on(app: &App, session: &str) -> Option<String> {
        let view = app.sessions.view_for(&SessionId::new(session))?;
        view.pending_tool_request().map(|r| r.request_id.clone())
    }

    /// ADR-0238: the runtime moves a background script's card to the
    /// successor and re-announces it there; the predecessor's copy goes, so
    /// exactly one answerable card is left — on the view a `Stop` clears.
    #[tokio::test]
    async fn a_moved_card_moves_from_the_predecessors_view_to_the_successors() {
        let mut app = App::new_for_test(SessionId::new("s1"));
        app.handle_out_event(detached_card("s1", 1));
        app.handle_out_event(compacted("s1", 2, "summary", true));
        app.handle_out_event(started("s2", Some("s1")));
        assert_eq!(card_on(&app, "s1").as_deref(), Some("x-1:rhai:edit"));

        app.handle_out_event(detached_card("s2", 1));
        assert_eq!(
            card_on(&app, "s1"),
            None,
            "the predecessor's copy is dropped"
        );
        assert_eq!(card_on(&app, "s2").as_deref(), Some("x-1:rhai:edit"));
        assert!(matches!(
            app.approval_mode(),
            ApprovalMode::WaitingForApproval { request_id } if request_id == "x-1:rhai:edit"
        ));

        let holly = entanglement_core::Holly::spawn(entanglement_core::EngineConfig::default());
        crate::tui::stop_command::request_stop(&mut app, &holly, SessionId::new("s2")).await;
        assert_eq!(
            card_on(&app, "s2"),
            None,
            "a Stop on the successor clears it"
        );
        assert_eq!(app.approval_mode(), &ApprovalMode::Normal);
    }

    /// A card a script parks fresh on the successor touches no other view,
    /// and an unrelated session's same-id card is never dropped.
    #[test]
    fn only_the_successors_lineage_loses_the_card() {
        let mut app = App::new_for_test(SessionId::new("s1"));
        app.handle_out_event(started("other", None));
        app.handle_out_event(detached_card("other", 1));
        app.handle_out_event(started("s2", Some("s1")));
        app.handle_out_event(detached_card("s2", 1));
        assert_eq!(card_on(&app, "other").as_deref(), Some("x-1:rhai:edit"));
        assert_eq!(card_on(&app, "s2").as_deref(), Some("x-1:rhai:edit"));
    }

    /// An ordinary session start (no lineage) is not a compaction hand-off.
    #[test]
    fn a_plain_session_start_changes_nothing() {
        let mut app = App::new_for_test(SessionId::new("s1"));
        app.handle_out_event(started("other", None));
        assert_eq!(app.active_session_id(), &SessionId::new("s1"));
    }

    /// #888: the TUI missed both successors' `SessionStarted` (p0 → p → s),
    /// so no view knows its lineage. Without the directory read the copy
    /// on `p0` stays when the card is re-announced on `s`; with it the copy
    /// drops, and an end on `s` drops it from every view of the chain.
    #[tokio::test]
    async fn a_card_moved_while_the_lineage_events_were_lost_still_moves() {
        let holly = entanglement_core::Holly::spawn(entanglement_core::EngineConfig::default());
        let (p0, p, s) = (
            SessionId::new("p0"),
            SessionId::new("p"),
            SessionId::new("s"),
        );
        let mut sub = holly.subscribe();
        for (id, predecessor) in [(&p0, None), (&p, Some(&p0)), (&s, Some(&p))] {
            let spawn = entanglement_core::InMsg::Spawn {
                session: id.clone(),
                parent: None,
                predecessor: predecessor.cloned(),
                agent: "general".into(),
                prompt: "hi".into(),
                user: None,
                model: None,
                mode: None,
                workdir: None,
            };
            holly.send(spawn).await.expect("engine accepts Spawn");
        }
        // The directory records the chain before `s` starts (ADR-0234).
        loop {
            match tokio::time::timeout(std::time::Duration::from_secs(5), sub.recv()).await {
                Ok(Ok(OutEvent::SessionStarted { session, .. })) if session == s => break,
                Ok(Ok(_)) => {}
                other => panic!("expected {s} to start: {other:?}"),
            }
        }

        let mut blind = App::new_for_test(p0.clone());
        blind.handle_out_event(detached_card("p0", 1));
        blind.handle_out_event(detached_card("s", 1));
        assert!(
            card_on(&blind, "p0").is_some(),
            "the gap: no lineage, no drop"
        );

        let mut app = App::new_for_test(p0.clone());
        app.handle_out_event(detached_card("p0", 1));
        let moved = detached_card("s", 1);
        app.learn_lineage(&moved, |s| holly.predecessors_of(s));
        app.handle_out_event(moved);
        assert_eq!(
            card_on(&app, "p0"),
            None,
            "the predecessor's copy is dropped"
        );
        assert_eq!(card_on(&app, "s").as_deref(), Some("x-1:rhai:edit"));
        assert_eq!(
            app.sessions
                .view_for(&s)
                .and_then(|v| v.predecessor().cloned()),
            Some(p0.clone()),
            "linked to the nearest ancestor with a view"
        );

        // An end on `s` drops a copy `p0` re-acquired (a lagged replay).
        app.handle_out_event(detached_card("p0", 2));
        let end = OutEvent::ApprovalResolved {
            session: s.clone(),
            seq: 2,
            request_id: "x-1:rhai:edit".into(),
            outcome: entanglement_core::ApprovalOutcome::Approved,
        };
        app.learn_lineage(&end, |s| holly.predecessors_of(s));
        app.handle_out_event(end);
        assert_eq!(card_on(&app, "p0"), None);
        assert_eq!(card_on(&app, "s"), None);
    }

    /// #888: a user watching `p0` whose TUI lost the successors'
    /// `SessionStarted` follows to `s` once a card event teaches the lineage,
    /// as the lost events would have moved them; a background view does not
    /// yank the user.
    #[test]
    fn a_learned_lineage_moves_the_user_watching_an_ancestor() {
        let (p0, p, s) = (
            SessionId::new("p0"),
            SessionId::new("p"),
            SessionId::new("s"),
        );
        let chain = |_: &SessionId| vec![p.clone(), p0.clone()];
        let mut app = App::new_for_test(p0.clone());
        app.handle_out_event(compacted("p0", 1, "the summary", true));
        app.learn_lineage(&detached_card("s", 1), chain);
        assert_eq!(app.sessions.active_id(), &s, "followed to the live end");
        assert_eq!(first_user_text(&app, "s").as_deref(), Some("the summary"));

        let mut watching_other = App::new_for_test(SessionId::new("other"));
        watching_other.sessions.ensure(&p0);
        watching_other.learn_lineage(&detached_card("s", 1), chain);
        assert_eq!(
            watching_other.sessions.active_id(),
            &SessionId::new("other")
        );
    }

    /// Review round 1: the TUI saw `s`'s `SessionStarted{p}` but lost
    /// `p`'s, so the views' walk stops at `p`; or it never had a `p` view,
    /// so `s`'s link dangles. Either way the copy on `p0` must still drop.
    #[test]
    fn a_partly_lost_chain_is_completed() {
        let chain = |_: &SessionId| vec![SessionId::new("p"), SessionId::new("p0")];
        for with_p_view in [true, false] {
            let mut app = App::new_for_test(SessionId::new("other"));
            app.handle_out_event(detached_card("p0", 1));
            if with_p_view {
                app.sessions.ensure(&SessionId::new("p"));
            }
            app.handle_out_event(started("s", Some("p")));
            let moved = detached_card("s", 1);
            app.learn_lineage(&moved, chain);
            app.handle_out_event(moved);
            assert_eq!(card_on(&app, "p0"), None, "p view: {with_p_view}");
            assert_eq!(card_on(&app, "s").as_deref(), Some("x-1:rhai:edit"));
        }
    }

    /// A link to an existing view is never overwritten, and nothing learned
    /// means nobody is moved: a user who went back to `p0` after following
    /// stays there.
    #[test]
    fn a_link_to_an_existing_view_is_kept() {
        let (p0, s) = (SessionId::new("p0"), SessionId::new("s"));
        let mut app = App::new_for_test(p0.clone());
        app.sessions.ensure(&SessionId::new("bogus"));
        app.handle_out_event(started("s", Some("p0")));
        app.sessions.switch_to(p0.clone());
        let card = detached_card("s", 1);
        app.learn_lineage(&card, |_| vec![SessionId::new("bogus")]);
        let link = app
            .sessions
            .view_for(&s)
            .and_then(|v| v.predecessor().cloned());
        assert_eq!(link, Some(p0.clone()), "kept");
        app.learn_lineage(&card, |_| vec![p0.clone()]);
        assert_eq!(
            app.sessions.active_id(),
            &p0,
            "nothing learned, nobody moved"
        );
    }

    /// Review round 2: `SessionStarted{p, p0}` moved the user to `p`, they
    /// went back to `p0` on purpose, then `SessionStarted{s, p}` was lost.
    /// Learning `s → p` must not pull them off `p0`: the lost event would
    /// only have moved a user watching `p`.
    #[test]
    fn a_deliberate_ancestor_view_is_not_pulled_away() {
        let (p0, p) = (SessionId::new("p0"), SessionId::new("p"));
        let mut app = App::new_for_test(p0.clone());
        app.handle_out_event(started("p", Some("p0")));
        assert_eq!(app.sessions.active_id(), &p);
        app.sessions.switch_to(p0.clone());
        app.learn_lineage(&detached_card("s", 1), |_| vec![p.clone(), p0.clone()]);
        let link = app
            .sessions
            .view_for(&SessionId::new("s"))
            .and_then(|v| v.predecessor().cloned());
        assert_eq!(link, Some(p.clone()), "the lost link is learned");
        assert_eq!(app.sessions.active_id(), &p0, "the user stays put");
    }
}
