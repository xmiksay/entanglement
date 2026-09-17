//! Replay/fold: reconstruct a [`Session`]'s in-memory state from a persisted
//! log of `(Option<InMsg>, OutEvent)` records. Separable from the live turn
//! loop — this is pure state reconstruction, no LLM or tool round-trip.

use anyhow::Result;

use super::replay_pending::TurnFold;
use super::Session;
use crate::protocol::{AgentState, InMsg, OutEvent, SessionId, UsagePurpose};
use crate::EngineConfig;
use entanglement_provider::{ContentPart, ToolCall};

impl Session {
    /// Resume a session from replayed log records.
    ///
    /// This reconstructs the session state from the provided records and returns
    /// the `Session` that can be passed to `session_loop_with_initial`.
    ///
    /// # Parameters
    ///
    /// - `records`: A slice of `(Option<InMsg>, OutEvent)` tuples representing the
    ///   log — a whole root file, which may interleave a spawned child's events
    ///   with the root's own (#275)
    /// - `cfg`: Engine configuration for constructing the per-session LLM
    /// - `target`: which session in the log to reconstruct — the root itself, or
    ///   one of its (grand)children when a cascaded resume rebuilds the whole
    ///   spawn sub-tree (#415)
    ///
    /// # Returns
    ///
    /// A reconstructed `Session` with all state folded from the log.
    pub fn replay(
        records: &[(Option<InMsg>, OutEvent)],
        cfg: &EngineConfig,
        target: &SessionId,
    ) -> Result<Self> {
        let default_profile = cfg
            .profiles
            .get("build")
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("default 'build' profile not found"))?;

        // Fold only `target`'s own records — otherwise a sibling/child session's
        // text/tool events are misattributed to `target`'s `Context` (#275). A log
        // that never mentions `target` at all (a standalone session captured on
        // its own, predating `SessionStarted`) falls back to folding everything.
        let target_started = records.iter().any(
            |(_, ev)| matches!(ev, OutEvent::SessionStarted { session, .. } if session == target),
        );
        let is_target = |sid: &SessionId| !target_started || sid == target;

        // Reconstruct `target`'s live `children` by inverting the parent edges
        // recorded across the shared root log (#child-lineage): a child's
        // `SessionStarted { parent: target }` adds it, its `SessionEnded` /
        // `SessionHibernated` removes it. The supervisor's `parent_links` stays
        // the authoritative tree; this only re-seeds the per-session mirror so a
        // resumed session still knows its live children. Only direct children of
        // `target` (grandchildren belong to their own parent).
        let mut children: Vec<SessionId> = Vec::new();
        for (_, ev) in records {
            match ev {
                OutEvent::SessionStarted {
                    session: child,
                    parent: Some(p),
                    ..
                } if p == target => {
                    if !children.contains(child) {
                        children.push(child.clone());
                    }
                }
                OutEvent::SessionEnded { session: gone, .. }
                | OutEvent::SessionHibernated { session: gone, .. } => {
                    children.retain(|c| c != gone);
                }
                _ => {}
            }
        }

        let mut session = Self::new_empty(cfg, default_profile);
        session.children = children;
        let mut fold = TurnFold::default();
        let mut max_seq: u64 = 0;

        for (in_msg, out_event) in records {
            // A prompt is scoped by its own session, not the event it was
            // paired with: interleaved child events must neither drop a root
            // prompt nor claim a child's.
            match in_msg {
                Some(InMsg::Prompt {
                    session: to,
                    content,
                }) if is_target(to) => fold.prompt(&mut session.ctx, content.clone()),
                Some(InMsg::Stop { session: to }) if is_target(to) => fold.stop(),
                _ => {}
            }
            // Skip any record belonging to a sibling/child session (#275): the
            // whole fold below stays scoped to `target`. A session-less query
            // reply (SessionList/History, #160) never appears in a log.
            if out_event.session().is_some_and(|s| !is_target(s)) {
                continue;
            }
            max_seq = max_seq.max(out_event.seq().unwrap_or(0));
            let ctx = &mut session.ctx;

            match out_event {
                OutEvent::SessionStarted {
                    parent,
                    predecessor,
                    user,
                    profile,
                    sponsored,
                    ..
                } => {
                    session.parent = parent.clone();
                    session.predecessor = predecessor.clone();
                    session.user = user.clone();
                    session.sponsored = *sponsored;
                    // Seed from the session's own authoritative statement of what
                    // it was spawned as (#638), rather than depending solely on a
                    // later `AgentChanged` record surviving in the log — a hole in
                    // the retained prefix that drops just that record must not
                    // silently degrade a restricted leaf back to the base `build`
                    // seed. An unknown profile name falls back to the base seed
                    // (same behavior `AgentChanged` already has below); a later
                    // in-session `/agent` switch still overrides via that fold.
                    if let Some(p) = cfg.profiles.get(profile) {
                        session.profile = p.clone();
                    }
                }
                OutEvent::TextDelta { text, .. } => fold.push_text(ctx, text),
                // Display-only rails, never folded into context — but they
                // prove the round began streaming. The replayable reasoning
                // arrives as `ReasoningBlock`; the assembled call as `ToolCall`.
                OutEvent::ReasoningDelta { .. } | OutEvent::ToolCallDelta { .. } => {
                    fold.round_event(ctx)
                }
                // Persisted reasoning (ADR-0160) and provider-search (#481)
                // blocks join the round's assistant message after its text.
                OutEvent::ReasoningBlock { part, .. } | OutEvent::SearchResult { part, .. } => {
                    fold.push_block(ctx, part.clone())
                }
                // Context gets the call as emitted: the fold rebuilds an
                // unwrapped `invoke` call from its envelope (ADR-0204).
                OutEvent::ToolCall {
                    request_id,
                    tool,
                    input,
                    provider_meta,
                    envelope,
                    ..
                } => {
                    let call = ToolCall {
                        id: request_id.clone(),
                        name: tool.clone(),
                        input: input.clone(),
                        provider_meta: provider_meta.clone(),
                    };
                    fold.push_call(ctx, call, envelope.as_ref());
                }
                OutEvent::ToolOutput {
                    request_id,
                    output,
                    content,
                    ..
                } => {
                    // `content` rides whenever `output` can't rebuild the
                    // result exactly (an image, #221); an empty text yields no
                    // parts, matching the live fold.
                    let parts = if !content.is_empty() {
                        content.clone()
                    } else if output.is_empty() {
                        Vec::new()
                    } else {
                        vec![ContentPart::text(output.clone())]
                    };
                    fold.tool_output(ctx, request_id, parts);
                }
                OutEvent::Usage {
                    purpose: UsagePurpose::Turn,
                    ..
                } => fold.round_event(ctx),
                OutEvent::Status {
                    state: state @ (AgentState::Done | AgentState::Paused),
                    ..
                } => fold.cancelled(ctx, *state == AgentState::Paused),
                OutEvent::SessionHibernated { .. } => fold.hibernated(ctx),
                OutEvent::AgentChanged { agent, .. } => {
                    if let Some(profile) = cfg.profiles.get(agent) {
                        session.profile = profile.clone();
                    }
                }
                // Reconstruct the mode axis (ADR-0207): overwrite — last write
                // wins, same as every other lifecycle fold in this match.
                // State only, no `ctx` push: the model-visible notice is
                // rebuilt fresh from `session.mode` every round (`stream.rs`),
                // never persisted — see `mode::mode_notice`'s doc for why a
                // persisted push would desync from the live session's history
                // (the pairing-order hazard around a session's very first
                // `Prompt`).
                OutEvent::ModeChanged { mode, .. } => {
                    session.mode = mode.clone();
                }
                // Re-bind a resumed session to the model it was switched to
                // (#218) so the continued turn runs under the same provider/model
                // + generation + context budget the user picked. Best-effort: an
                // embedder replaying without a resolver (or a provider whose key
                // is now unset) keeps the startup default rather than failing.
                OutEvent::ModelChanged {
                    provider, model, ..
                } => {
                    // Reconstruct the per-profile session memory (#323, ADR-0081):
                    // the logged `(provider, model)` is the resolved canonical pair,
                    // keyed by the active profile the preceding `AgentChanged` folds
                    // set. So a resumed session re-applies a `/model` choice per
                    // profile exactly like the live one, wins over a static pin on a
                    // later `SetAgent` switch-back.
                    session.profile_models.insert(
                        session.profile.name.clone(),
                        (provider.clone(), model.clone()),
                    );
                    if let Some(resolver) = cfg.model_resolver.as_ref() {
                        match resolver(session.user.as_ref(), provider, model) {
                            Ok(resolved) => {
                                session.provider = Some(resolved.provider);
                                session.llm = (resolved.llm_factory)();
                                session.model = Some(resolved.model);
                                session.generation = resolved.generation;
                                session.ctx.set_window(resolved.context_window);
                            }
                            Err(e) => tracing::warn!(
                                provider, model, error = %e,
                                "replay: could not re-resolve switched model; keeping default"
                            ),
                        }
                    }
                }
                // Re-bind a resumed session's generation knobs to whatever they
                // were last set to (#374, ADR-0094), mirroring the `ModelChanged`
                // fold above: the logged value is already the full effective
                // params, so replay just overwrites `generation` and reconstructs
                // the per-profile session memory keyed by the active profile the
                // preceding `AgentChanged` fold set. A later `GenerationChanged`/
                // `ModelChanged` record in the log still wins (last-write, same as
                // the live engine).
                OutEvent::GenerationChanged { generation, .. } => {
                    session.generation = Some(*generation);
                    session
                        .profile_generation
                        .insert(session.profile.name.clone(), *generation);
                }
                // Restore the live tool overlay (#539, ADR-0149): the logged
                // value is the full effective list, so replay overwrites it —
                // last write wins, same as the live engine's full-replacement
                // semantics.
                OutEvent::ToolOverlayChanged { entries, .. } => {
                    session.tool_overlay = entries.clone();
                }
                // Display metadata: the logged values are the full merged
                // state, so replay overwrites — last write wins.
                OutEvent::SessionMetaChanged { name, action, .. } => {
                    session.name = name.clone();
                    session.action = action.clone();
                }
                // `Plan`/`TaskList` are the runtime's display state now (#231,
                // ADR-0049): they carry nothing the engine's `Context` needs, so
                // replay ignores them. A resuming head folds them from the log
                // itself to restore its plan/task panels.
                OutEvent::Done { .. } => fold.done(ctx),
                // Session compaction (#324, ADR-0082 → ADR-0101/0103/0205):
                // **always a no-op**, on every path. Since ADR-0205 no
                // compaction mutates the session it is emitted on — each one
                // forks a successor seeded through that successor's own
                // `Spawn` prompt, and retires the source unchanged. So this
                // session's history is exactly what the rest of the log
                // reconstructs, with or without this record.
                //
                // That covers the legacy shapes too. A pre-ADR-0101 in-place
                // record and a pre-ADR-0205 `auto: true` one both land here
                // and are likewise ignored: replaying their mutation would
                // clobber the full pre-compaction history the log still holds
                // — and for a source that was retired at the fork, that
                // history is precisely what a reader of this log wants back.
                OutEvent::Compacted { .. } => {}
                // An ambiguous-stop retry (#ADR-0118): the live engine committed
                // the round's partial text as an assistant message, then injected
                // `nudge` as a user-role steering message and re-queried in place.
                // Reconstruct that exact boundary so a resumed session's history
                // matches what the live model saw, instead of merging both
                // rounds' text.
                OutEvent::AmbiguousRetry { nudge, .. } => fold.ambiguous_retry(ctx, nudge),
                _ => {}
            }
        }

        // A log ending mid-turn parks as `TurnState` so resume can re-offer
        // the unanswered calls (#271, ADR-0061). The fold above already
        // dropped every child record, so this tail is the resumed root's own.
        session.turn = fold.into_parked_turn(&mut session.ctx);

        // Seed the shared counter past the reconstructed tail so a resumed
        // session — and any runtime event minted for it — continues the sequence
        // rather than colliding with a replayed seq (#157).
        session
            .seq
            .store(max_seq, std::sync::atomic::Ordering::Relaxed);
        Ok(session)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn started(session: &str, parent: Option<&str>, predecessor: Option<&str>) -> OutEvent {
        started_as(session, parent, predecessor, "build")
    }

    fn started_as(
        session: &str,
        parent: Option<&str>,
        predecessor: Option<&str>,
        profile: &str,
    ) -> OutEvent {
        OutEvent::SessionStarted {
            session: SessionId::new(session),
            parent: parent.map(SessionId::new),
            predecessor: predecessor.map(SessionId::new),
            profile: profile.into(),
            model: None,
            root: parent.is_none(),
            ts: 0,
            user: None,
            sponsored: false,
        }
    }

    /// A registry carrying a restricted `Subagent` leaf alongside the built-in
    /// `build`, for the #638 profile-fold tests below.
    fn cfg_with_leaf_profile(name: &str) -> EngineConfig {
        use crate::protocol::{AgentMode, AgentProfile, Permission, PermissionProfile};

        let mut cfg = EngineConfig::default();
        cfg.profiles.insert(AgentProfile {
            name: name.into(),
            description: "restricted leaf".into(),
            mode: AgentMode::Subagent,
            system_prompt: "leaf".into(),
            model: None,
            provider: None,
            permission: PermissionProfile::new(Permission::Deny),
            tools: None,
            disallowed_tools: Vec::new(),
            can_spawn: None,
            spawnable_agents: None,
            sandbox: None,
        });
        cfg
    }

    /// The resumed root's live `children` are reconstructed by inverting the
    /// parent edges in the shared root log; an ended child is dropped.
    #[test]
    fn replay_reconstructs_children_from_parent_edges() {
        let cfg = EngineConfig::default();
        let records: Vec<(Option<InMsg>, OutEvent)> = vec![
            (None, started("root", None, None)),
            (None, started("child-a", Some("root"), None)),
            (None, started("child-b", Some("root"), None)),
            // A grandchild belongs to child-a, not the root.
            (None, started("grand", Some("child-a"), None)),
            // child-b ends → pruned from the root's live children.
            (
                None,
                OutEvent::SessionEnded {
                    session: SessionId::new("child-b"),
                    ts: 1,
                },
            ),
        ];
        let s = Session::replay(&records, &cfg, &SessionId::new("root")).unwrap();
        assert_eq!(s.parent, None);
        assert_eq!(s.children, vec![SessionId::new("child-a")]);
    }

    /// A successor's `predecessor` is reconstructed from its own `SessionStarted`.
    #[test]
    fn replay_reconstructs_predecessor() {
        let cfg = EngineConfig::default();
        let records: Vec<(Option<InMsg>, OutEvent)> =
            vec![(None, started("successor", None, Some("source")))];
        let s = Session::replay(&records, &cfg, &SessionId::new("successor")).unwrap();
        assert_eq!(s.predecessor, Some(SessionId::new("source")));
        assert_eq!(s.parent, None);
    }

    /// An ambiguous-stop retry (ADR-0118) folds back into three distinct
    /// messages — the truncated partial, the injected nudge, and the recovered
    /// reply — not one merged assistant message. Without folding
    /// `AmbiguousRetry` the two rounds' `TextDelta`s coalesce and the nudge
    /// vanishes, resuming from a history the live model never saw.
    #[test]
    fn replay_reconstructs_ambiguous_retry_boundary_and_nudge() {
        let cfg = EngineConfig::default();
        let sid = SessionId::new("root");
        let text = |t: &str, seq: u64| OutEvent::TextDelta {
            session: sid.clone(),
            seq,
            text: t.into(),
        };
        let records: Vec<(Option<InMsg>, OutEvent)> = vec![
            (None, started("root", None, None)),
            (
                Some(InMsg::prompt(sid.clone(), "do it")),
                text("partial", 1),
            ),
            (
                None,
                OutEvent::AmbiguousRetry {
                    session: sid.clone(),
                    seq: 2,
                    nudge: "[nudge]".into(),
                },
            ),
            (None, text("final", 3)),
            (
                None,
                OutEvent::Done {
                    session: sid.clone(),
                    seq: 4,
                },
            ),
        ];
        let s = Session::replay(&records, &cfg, &sid).unwrap();
        let msgs = s.ctx.messages();
        let rendered: Vec<(_, String)> = msgs.iter().map(|m| (m.role, m.text())).collect();
        use entanglement_provider::MessageRole::*;
        assert_eq!(
            rendered,
            vec![
                (User, "do it".to_string()),
                (Assistant, "partial".to_string()),
                (User, "[nudge]".to_string()),
                (Assistant, "final".to_string()),
            ],
            "the retry boundary and nudge must survive replay distinctly"
        );
    }

    /// A `SearchResult` (#481) folds into the same assistant message as the
    /// surrounding `TextDelta`s, not a separate one — mirroring the live
    /// commit in `session/round.rs`, which appends search blocks after the
    /// round's text in one `Message::assistant_content` push.
    #[test]
    fn replay_folds_search_result_into_the_assistant_message_content() {
        let cfg = EngineConfig::default();
        let sid = SessionId::new("root");
        let part = entanglement_provider::ContentPart::provider_search(
            "anthropic",
            "[web_search] rust async",
            serde_json::json!({ "type": "server_tool_use", "id": "srvtoolu_1" }),
        );
        let records: Vec<(Option<InMsg>, OutEvent)> = vec![
            (None, started("root", None, None)),
            (
                Some(InMsg::prompt(sid.clone(), "search for rust async")),
                OutEvent::TextDelta {
                    session: sid.clone(),
                    seq: 1,
                    text: "looking it up".into(),
                },
            ),
            (
                None,
                OutEvent::SearchResult {
                    session: sid.clone(),
                    seq: 2,
                    part: part.clone(),
                },
            ),
            (
                None,
                OutEvent::Done {
                    session: sid.clone(),
                    seq: 3,
                },
            ),
        ];
        let s = Session::replay(&records, &cfg, &sid).unwrap();
        let msgs = s.ctx.messages();
        assert_eq!(msgs.len(), 2, "user prompt + one assistant message");
        let assistant = &msgs[1];
        assert_eq!(
            assistant.role,
            entanglement_provider::MessageRole::Assistant
        );
        assert_eq!(
            assistant.content,
            vec![
                entanglement_provider::ContentPart::text("looking it up"),
                part,
            ]
        );
    }

    /// Replaying a *child*'s own id (not the log's flagged root) reconstructs its
    /// own context/lineage, scoped to its own records — the seam a cascaded
    /// resume (#415) relies on to rebuild a whole spawn sub-tree from one root
    /// log.
    #[test]
    fn replay_reconstructs_a_non_root_target() {
        let cfg = EngineConfig::default();
        let records: Vec<(Option<InMsg>, OutEvent)> = vec![
            (None, started("root", None, None)),
            (None, started("child", Some("root"), None)),
            (None, started("grand", Some("child"), None)),
        ];
        let s = Session::replay(&records, &cfg, &SessionId::new("child")).unwrap();
        assert_eq!(s.parent, Some(SessionId::new("root")));
        assert_eq!(s.children, vec![SessionId::new("grand")]);
    }

    /// #638: a resumed sub-agent's profile must come back from its own
    /// `SessionStarted.profile`, not depend on a later `AgentChanged` record
    /// surviving in the log — a hole in the retained prefix that drops just
    /// that record must not silently degrade a restricted leaf back to the
    /// base `build` seed (the privilege-escalating direction).
    #[test]
    fn replay_seeds_profile_from_session_started_without_agent_changed() {
        let cfg = cfg_with_leaf_profile("page-writer");
        let records: Vec<(Option<InMsg>, OutEvent)> =
            vec![(None, started_as("child", Some("root"), None, "page-writer"))];
        let s = Session::replay(&records, &cfg, &SessionId::new("child")).unwrap();
        assert_eq!(s.profile.name, "page-writer");
    }

    /// A later in-session `/agent` switch (a genuine `AgentChanged` record)
    /// still overrides the `SessionStarted` seed — the fold order documented
    /// at the fix site.
    #[test]
    fn replay_agent_changed_overrides_session_started_profile() {
        let cfg = cfg_with_leaf_profile("page-writer");
        let sid = SessionId::new("child");
        let records: Vec<(Option<InMsg>, OutEvent)> = vec![
            (None, started_as("child", Some("root"), None, "build")),
            (
                None,
                OutEvent::AgentChanged {
                    session: sid.clone(),
                    agent: "page-writer".into(),
                    profile_detail: None,
                },
            ),
        ];
        let s = Session::replay(&records, &cfg, &sid).unwrap();
        assert_eq!(s.profile.name, "page-writer");
    }

    /// An unknown `SessionStarted.profile` (a name the replaying registry
    /// doesn't carry) falls back to the base seed rather than erroring —
    /// mirroring the existing `AgentChanged` fallback below it.
    #[test]
    fn replay_unknown_session_started_profile_falls_back_to_base_seed() {
        let cfg = EngineConfig::default();
        let records: Vec<(Option<InMsg>, OutEvent)> = vec![(
            None,
            started_as("child", Some("root"), None, "no-such-profile"),
        )];
        let s = Session::replay(&records, &cfg, &SessionId::new("child")).unwrap();
        assert_eq!(s.profile.name, "build");
    }
}
