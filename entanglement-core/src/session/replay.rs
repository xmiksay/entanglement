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
    /// - `records`: A slice of `(Option<InMsg>, OutEvent)` tuples representing the log
    /// - `cfg`: Engine configuration for constructing the per-session LLM
    ///
    /// # Returns
    ///
    /// A reconstructed `Session` with all state folded from the log.
    pub fn replay(records: &[(Option<InMsg>, OutEvent)], cfg: &EngineConfig) -> Result<Self> {
        let default_profile = cfg
            .profiles
            .get("build")
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("default 'build' profile not found"))?;

        // A root log persists the whole spawn sub-tree, so the record stream
        // interleaves a spawned child's events with the resumed root's. Fold
        // only the root's own records — otherwise a child's text/tool events
        // are misattributed to the root's `Context` (#275). Root = the first
        // session flagged `root` in the log; a log with none (a standalone
        // session captured on its own) folds every record.
        let root: Option<SessionId> = records.iter().find_map(|(_, ev)| match ev {
            OutEvent::SessionStarted {
                session,
                root: true,
                ..
            } => Some(session.clone()),
            _ => None,
        });
        let is_root = |sid: &SessionId| root.as_ref().is_none_or(|r| r == sid);

        let mut session = Self::new_empty(cfg, default_profile);
        let mut pending_text: String = String::new();
        let mut pending_tools: Vec<ToolCall> = Vec::new();
        // Reconstructed tool results keyed by request id — multimodal so an image
        // read (#221) rebuilds as its image block, not the display placeholder.
        let mut pending_tool_outputs: Vec<(String, Vec<ContentPart>)> = Vec::new();
        let mut max_seq: u64 = 0;

        for (in_msg, out_event) in records {
            // Skip any record belonging to a spawned child session (#275): the
            // whole fold below stays scoped to the resumed root. A session-less
            // query reply (SessionList/History, #160) never appears in a log.
            if out_event.session().is_some_and(|s| !is_root(s)) {
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
                OutEvent::SessionStarted { parent, .. } => {
                    session.parent = parent.clone();
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
