//! Explicit state of a turn parked on tool results (#270, ADR-0061).
//!
//! When a streamed round ends in tool calls, the engine emits the whole batch
//! as `ToolExec` requests and *returns to the session loop* instead of parking
//! the async stack on the first result. What used to be locals in `run_turn`
//! (the unresolved calls, the round counter) lives here as serde-capable data,
//! so a session can be suspended, persisted (event log + replay), and resumed
//! mid-turn by any embedder — resolution is just `InMsg::ToolResult` messages
//! arriving in any order.

use serde::{Deserialize, Serialize};

use entanglement_provider::ToolCall;

/// In-flight turn state: `Some` on `Session::turn` exactly while a turn is
/// live (streaming or parked); `None` when the session is idle.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TurnState {
    /// Unresolved tool calls of the current batch, in emit order. Empty while
    /// a round is streaming; filled by [`Self::begin_batch`]; drained by
    /// [`Self::resolve`] as results arrive (any order).
    pub pending: Vec<ToolCall>,
    /// LLM round-trips consumed by this turn (`MAX_TURNS` guard, #177). Reset
    /// per prompt by constructing a fresh `TurnState`; a prompt folded into a
    /// live turn (ADR-0058) deliberately does not reset it.
    pub iterations: usize,
}

impl TurnState {
    /// Record a freshly emitted batch of tool calls as pending.
    pub fn begin_batch(&mut self, calls: Vec<ToolCall>) {
        self.pending = calls;
    }

    /// Resolve one pending call by `request_id`, removing and returning it.
    /// `None` for an unknown, duplicate, or stale id — the caller drops the
    /// result rather than corrupting context.
    pub fn resolve(&mut self, request_id: &str) -> Option<ToolCall> {
        let idx = self.pending.iter().position(|c| c.id == request_id)?;
        Some(self.pending.remove(idx))
    }

    /// True when every call of the batch has been resolved.
    pub fn is_drained(&self) -> bool {
        self.pending.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(id: &str) -> ToolCall {
        ToolCall {
            id: id.to_string(),
            name: format!("tool_{id}"),
            input: "{}".to_string(),
        }
    }

    #[test]
    fn resolve_drains_out_of_order() {
        let mut t = TurnState::default();
        t.begin_batch(vec![call("a"), call("b"), call("c")]);
        assert!(!t.is_drained());
        assert_eq!(t.resolve("b").map(|c| c.name), Some("tool_b".into()));
        assert_eq!(t.resolve("c").map(|c| c.name), Some("tool_c".into()));
        assert_eq!(t.resolve("a").map(|c| c.name), Some("tool_a".into()));
        assert!(t.is_drained());
    }

    #[test]
    fn resolve_rejects_unknown_and_duplicate_ids() {
        let mut t = TurnState::default();
        t.begin_batch(vec![call("a")]);
        assert!(t.resolve("nope").is_none());
        assert!(t.resolve("a").is_some());
        assert!(t.resolve("a").is_none(), "second resolve is a duplicate");
        assert!(t.is_drained());
    }

    #[test]
    fn serde_round_trips() {
        let mut t = TurnState::default();
        t.begin_batch(vec![call("a"), call("b")]);
        t.iterations = 3;
        let json = serde_json::to_string(&t).expect("serialize");
        let back: TurnState = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back.pending.len(), 2);
        assert_eq!(back.iterations, 3);
        assert_eq!(back.pending[1].id, "b");
    }
}
