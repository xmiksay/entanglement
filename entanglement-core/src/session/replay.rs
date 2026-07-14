//! Replay/fold: reconstruct a [`Session`]'s in-memory state from a persisted
//! log of `(Option<InMsg>, OutEvent)` records. Separable from the live turn
//! loop — this is pure state reconstruction, no LLM or tool round-trip.

use std::collections::HashSet;

use anyhow::Result;

use super::{Session, TurnState};
use crate::protocol::{InMsg, OutEvent, SessionId};
use crate::EngineConfig;
use entanglement_provider::ToolCall;

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

        let mut session = Self::new_empty(cfg, default_profile);
        let mut pending_text: String = String::new();
        // Tool calls/outputs carry their originating session so the mid-turn
        // tail below can be guarded against a root log's interleaved child
        // events. The general fold still ignores the session id — that
        // pre-existing misattribution flaw is tracked separately (#275).
        let mut pending_tools: Vec<(SessionId, ToolCall)> = Vec::new();
        let mut pending_tool_outputs: Vec<(SessionId, String, String)> = Vec::new();
        let mut root: Option<SessionId> = None;
        let mut max_seq: u64 = 0;

        for (in_msg, out_event) in records {
            max_seq = max_seq.max(out_event.seq());

            if let Some(InMsg::Prompt { text, .. }) = in_msg {
                if !pending_text.is_empty() || !pending_tools.is_empty() {
                    session.ctx.push_assistant(
                        pending_text.clone(),
                        pending_tools.iter().map(|(_, c)| c.clone()).collect(),
                    );
                    pending_text.clear();
                    pending_tools.clear();
                }
                for (_, request_id, output) in &pending_tool_outputs {
                    session.ctx.push_tool(request_id.clone(), output.clone());
                }
                pending_tool_outputs.clear();

                session.ctx.push_user(text.clone());
            }

            match out_event {
                OutEvent::SessionStarted {
                    session: sid,
                    parent,
                    root: is_root,
                    ..
                } => {
                    if *is_root && root.is_none() {
                        root = Some(sid.clone());
                    }
                    session.parent = parent.clone();
                }
                OutEvent::TextDelta { text, .. } => {
                    pending_text.push_str(text);
                }
                OutEvent::ReasoningDelta { .. } => {
                    // Reasoning is not stored in context; it's display-only.
                }
                OutEvent::ToolCall {
                    session: sid,
                    request_id,
                    tool,
                    input,
                    ..
                } => {
                    pending_tools.push((
                        sid.clone(),
                        ToolCall {
                            id: request_id.clone(),
                            name: tool.clone(),
                            input: input.clone(),
                        },
                    ));
                }
                OutEvent::ToolOutput {
                    session: sid,
                    request_id,
                    output,
                    ..
                } => {
                    pending_tool_outputs.push((sid.clone(), request_id.clone(), output.clone()));
                }
                OutEvent::AgentChanged { agent, .. } => {
                    if let Some(profile) = cfg.profiles.get(agent) {
                        session.profile = profile.clone();
                    }
                }
                // `Plan`/`TaskList` are the runtime's display state now (#231,
                // ADR-0049): they carry nothing the engine's `Context` needs, so
                // replay ignores them. A resuming head folds them from the log
                // itself to restore its plan/task panels.
                OutEvent::Done { .. } => {
                    if !pending_text.is_empty() || !pending_tools.is_empty() {
                        session.ctx.push_assistant(
                            pending_text.clone(),
                            pending_tools.iter().map(|(_, c)| c.clone()).collect(),
                        );
                        pending_text.clear();
                        pending_tools.clear();
                    }
                    for (_, request_id, output) in &pending_tool_outputs {
                        session.ctx.push_tool(request_id.clone(), output.clone());
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
        // not a quota. Guarded to the resumed root's own events so a child's
        // interleaved tail is not misattributed (#275).
        let is_root = |sid: &SessionId| root.as_ref().is_none_or(|r| r == sid);
        let tail: Vec<ToolCall> = pending_tools
            .iter()
            .filter(|(sid, _)| is_root(sid))
            .map(|(_, c)| c.clone())
            .collect();
        if !tail.is_empty() {
            session
                .ctx
                .push_assistant(pending_text.clone(), tail.clone());
            let resolved: HashSet<&str> = pending_tool_outputs
                .iter()
                .filter(|(sid, ..)| is_root(sid))
                .map(|(_, id, _)| id.as_str())
                .collect();
            for (sid, request_id, output) in &pending_tool_outputs {
                if is_root(sid) {
                    session.ctx.push_tool(request_id.clone(), output.clone());
                }
            }
            // Pending = calls without a logged output. Kept `Some` even when
            // fully resolved (the crash hit before the next round streamed):
            // resume then continues the turn instead of re-offering.
            let pending: Vec<ToolCall> = tail
                .into_iter()
                .filter(|c| !resolved.contains(c.id.as_str()))
                .collect();
            session.turn = Some(TurnState {
                pending,
                iterations: 0,
            });
        }

        session.seq = max_seq;
        Ok(session)
    }
}
