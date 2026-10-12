//! Small types carried inside [`OutEvent`] variants.

use serde::{Deserialize, Serialize};

#[cfg(doc)]
use crate::protocol::OutEvent;

/// Which request an [`OutEvent::Usage`] round was. A head totals every
/// round's spend, but only a `Turn` round's prompt size is the session's
/// context size — a `Compaction` round prices the summarizer's own request
/// (the head being summarized plus the instruction), not the live context.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UsagePurpose {
    #[default]
    Turn,
    Compaction,
}

/// How a [`Compacted`][OutEvent::Compacted] compaction reclaimed room
/// ([ADR-0205](../../docs/adr/0205-every-compaction-forks-a-successor-session.md)).
/// Orthogonal to that event's `auto` flag, which says *who asked*: `auto`
/// alone cannot tell the overflow guard's two fallbacks apart, and a head
/// renders them differently — one carries a real summary, the other does not.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompactionMode {
    /// An LLM-authored summary of the history's head. The default, so every
    /// pre-ADR-0205 record on the wire keeps its meaning.
    #[default]
    Summary,
    /// The prune-only fallback: no LLM was available (disabled, guard tripped,
    /// or its result still didn't fit), so the oldest tool outputs were
    /// replaced with a short placeholder and the pruned history itself seeds
    /// the successor. `summary` then holds that pruned transcript.
    Prune,
}

/// An `mcp {server, tool, args}` call as the model emitted it (ADR-0226; an
/// older log may carry ADR-0204's `invoke {name, args}`): the outer tool name
/// and the raw argument string, verbatim. Carried on
/// [`ToolCall`][OutEvent::ToolCall]/[`ToolExec`][OutEvent::ToolExec]/
/// [`ToolOutput`][OutEvent::ToolOutput] when core unwrapped the call, so heads,
/// gates and hooks see the inner tool while replay can rebuild the emitted
/// call byte for byte — the model-facing history is never rewritten.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolEnvelope {
    pub tool: String,
    pub input: String,
}
