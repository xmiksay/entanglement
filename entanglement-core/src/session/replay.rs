//! Replay/fold: reconstruct a [`Session`]'s in-memory state from a persisted
//! log of `(Option<InMsg>, OutEvent)` records. Separable from the live turn
//! loop — this is pure state reconstruction, no LLM or tool round-trip.

use std::collections::HashSet;

use anyhow::Result;

use super::{Session, TurnState};
use crate::protocol::{InMsg, OutEvent, SessionId};
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
        let mut pending_text: String = String::new();
        let mut pending_tools: Vec<ToolCall> = Vec::new();
        // Reconstructed tool results keyed by request id — multimodal so an image
        // read (#221) rebuilds as its image block, not the display placeholder.
        let mut pending_tool_outputs: Vec<(String, Vec<ContentPart>)> = Vec::new();
        let mut max_seq: u64 = 0;

        for (in_msg, out_event) in records {
            // Skip any record belonging to a sibling/child session (#275): the
            // whole fold below stays scoped to `target`. A session-less query
            // reply (SessionList/History, #160) never appears in a log.
            if out_event.session().is_some_and(|s| !is_target(s)) {
                continue;
            }
            max_seq = max_seq.max(out_event.seq().unwrap_or(0));

            if let Some(InMsg::Prompt { content, .. }) = in_msg {
                if !pending_text.is_empty() || !pending_tools.is_empty() {
                    session
                        .ctx
                        .push_assistant(pending_text.clone(), pending_tools.clone());
                    pending_text.clear();
                    pending_tools.clear();
                }
                for (request_id, output) in &pending_tool_outputs {
                    session
                        .ctx
                        .push_tool_content(request_id.clone(), output.clone());
                }
                pending_tool_outputs.clear();

                session.ctx.push_user_content(content.clone());
            }

            match out_event {
                OutEvent::SessionStarted {
                    parent,
                    predecessor,
                    ..
                } => {
                    session.parent = parent.clone();
                    session.predecessor = predecessor.clone();
                }
                OutEvent::TextDelta { text, .. } => {
                    pending_text.push_str(text);
                }
                OutEvent::ReasoningDelta { .. } => {
                    // Reasoning is not stored in context; it's display-only.
                }
                OutEvent::ToolCallDelta { .. } => {
                    // Streaming arg fragments (#194) are display-only; the
                    // assembled `ToolCall` below reconstructs the call for context.
                }
                OutEvent::ToolCall {
                    request_id,
                    tool,
                    input,
                    ..
                } => {
                    pending_tools.push(ToolCall {
                        id: request_id.clone(),
                        name: tool.clone(),
                        input: input.clone(),
                        provider_meta: None,
                    });
                }
                OutEvent::ToolOutput {
                    request_id,
                    output,
                    content,
                    ..
                } => {
                    // Prefer the multimodal `content` (an image read, #221); fall
                    // back to the text `output` for the common case (and for logs
                    // written before the field existed). An empty text yields no
                    // parts, matching the live fold.
                    let parts = if !content.is_empty() {
                        content.clone()
                    } else if output.is_empty() {
                        Vec::new()
                    } else {
                        vec![ContentPart::text(output.clone())]
                    };
                    pending_tool_outputs.push((request_id.clone(), parts));
                }
                OutEvent::AgentChanged { agent, .. } => {
                    if let Some(profile) = cfg.profiles.get(agent) {
                        session.profile = profile.clone();
                    }
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
                        match resolver(provider, model) {
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
                // `Plan`/`TaskList` are the runtime's display state now (#231,
                // ADR-0049): they carry nothing the engine's `Context` needs, so
                // replay ignores them. A resuming head folds them from the log
                // itself to restore its plan/task panels.
                OutEvent::Done { .. } => {
                    if !pending_text.is_empty() || !pending_tools.is_empty() {
                        session
                            .ctx
                            .push_assistant(pending_text.clone(), pending_tools.clone());
                        pending_text.clear();
                        pending_tools.clear();
                    }
                    for (request_id, output) in &pending_tool_outputs {
                        session
                            .ctx
                            .push_tool_content(request_id.clone(), output.clone());
                    }
                    pending_tool_outputs.clear();
                }
                // Session compaction (#324, ADR-0082 → ADR-0101/0103): two
                // mutation semantics share this event, told apart by `auto`.
                // `auto: false` — manual `/compact`, **copy-on-write** — the
                // source `Context` is never mutated, so there is nothing to
                // fold here; the summary rides only in the event (a head forks
                // it into a new session). A record written under the old
                // in-place design (pre-ADR-0101) also lands here (its `auto`
                // defaults to `false` on the wire) and is likewise ignored:
                // replaying it would clobber the full pre-compaction history
                // the log still holds, which is exactly the history the
                // source session should recover with.
                //
                // `auto: true` — automatic in-place compaction on context
                // overflow (#398): the live engine mutated `Context` via
                // `apply_compaction` before continuing the turn, so replay
                // must reconstruct that same mutation. Flush whatever
                // pending assistant/tool state has accumulated so far (same
                // flush the `Done` arm above does) so `apply_compaction`
                // operates on the messages actually pushed, not a stale tail.
                OutEvent::Compacted {
                    auto: true,
                    summary,
                    kept,
                    ..
                } => {
                    if !pending_text.is_empty() || !pending_tools.is_empty() {
                        session
                            .ctx
                            .push_assistant(pending_text.clone(), pending_tools.clone());
                        pending_text.clear();
                        pending_tools.clear();
                    }
                    for (request_id, output) in &pending_tool_outputs {
                        session
                            .ctx
                            .push_tool_content(request_id.clone(), output.clone());
                    }
                    pending_tool_outputs.clear();

                    session.ctx.apply_compaction(summary, *kept as usize);
                }
                OutEvent::Compacted { auto: false, .. } => {}
                _ => {}
            }
        }

        // A log ending mid-turn (#271, ADR-0061): `ToolCall` events are only
        // emitted after a completed stream, so a non-empty pending set means
        // the last round finished streaming and parked — reconstruct it as
        // `TurnState` so resume can re-offer the unanswered calls. A text-only
        // tail (deltas with no `ToolCall`) is a genuine mid-stream crash and
        // stays dropped, matching the live engine, which never committed it
        // either. `iterations` restarts at 0: `MAX_TURNS` is a runaway guard,
        // not a quota. The fold above already dropped every child record, so
        // this tail is the resumed root's own.
        if !pending_tools.is_empty() {
            session
                .ctx
                .push_assistant(pending_text.clone(), pending_tools.clone());
            let resolved: HashSet<&str> = pending_tool_outputs
                .iter()
                .map(|(id, _)| id.as_str())
                .collect();
            for (request_id, output) in &pending_tool_outputs {
                session
                    .ctx
                    .push_tool_content(request_id.clone(), output.clone());
            }
            // Pending = calls without a logged output. Kept `Some` even when
            // fully resolved (the crash hit before the next round streamed):
            // resume then continues the turn instead of re-offering.
            let pending: Vec<ToolCall> = pending_tools
                .into_iter()
                .filter(|c| !resolved.contains(c.id.as_str()))
                .collect();
            session.turn = Some(TurnState {
                pending,
                iterations: 0,
                ambiguous_retries: 0,
            });
        }

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
        OutEvent::SessionStarted {
            session: SessionId::new(session),
            parent: parent.map(SessionId::new),
            predecessor: predecessor.map(SessionId::new),
            profile: "build".into(),
            model: None,
            root: parent.is_none(),
            ts: 0,
        }
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
}
