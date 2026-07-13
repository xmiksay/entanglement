//! Replay/fold: reconstruct a [`Session`]'s in-memory state from a persisted
//! log of `(Option<InMsg>, OutEvent)` records. Separable from the live turn
//! loop — this is pure state reconstruction, no LLM or tool round-trip.

use std::path::Path;

use anyhow::Result;

use super::Session;
use crate::protocol::{InMsg, OutEvent};
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
    /// - `cfg`: Engine configuration for constructing tools and LLM
    /// - `root`: Root directory for tool operations (unused in core but required for consistency)
    ///
    /// # Returns
    ///
    /// A reconstructed `Session` with all state folded from the log.
    pub fn replay(
        records: &[(Option<InMsg>, OutEvent)],
        cfg: &EngineConfig,
        _root: &Path,
    ) -> Result<Self> {
        let default_profile = cfg
            .profiles
            .get("build")
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("default 'build' profile not found"))?;

        let mut session = Self::new_empty(cfg, default_profile);
        let mut pending_text: String = String::new();
        let mut pending_tools: Vec<ToolCall> = Vec::new();
        let mut pending_tool_outputs: Vec<(String, String)> = Vec::new();
        let mut max_seq: u64 = 0;

        for (in_msg, out_event) in records {
            max_seq = max_seq.max(out_event.seq());

            if let Some(InMsg::Prompt { text, .. }) = in_msg {
                if !pending_text.is_empty() || !pending_tools.is_empty() {
                    session
                        .ctx
                        .push_assistant(pending_text.clone(), pending_tools.clone());
                    pending_text.clear();
                    pending_tools.clear();
                }
                for (request_id, output) in &pending_tool_outputs {
                    session.ctx.push_tool(request_id.clone(), output.clone());
                }
                pending_tool_outputs.clear();

                session.ctx.push_user(text.clone());
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
                    });
                }
                OutEvent::ToolOutput {
                    request_id, output, ..
                } => {
                    pending_tool_outputs.push((request_id.clone(), output.clone()));
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
                        session
                            .ctx
                            .push_assistant(pending_text.clone(), pending_tools.clone());
                        pending_text.clear();
                        pending_tools.clear();
                    }
                    for (request_id, output) in &pending_tool_outputs {
                        session.ctx.push_tool(request_id.clone(), output.clone());
                    }
                    pending_tool_outputs.clear();
                }
                _ => {}
            }
        }

        session.seq = max_seq;
        Ok(session)
    }
}
