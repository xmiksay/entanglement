//! Explicit state of a turn parked on tool results (#270, ADR-0061).
//!
//! When a streamed round ends in tool calls, the engine emits the whole batch
//! as `ToolExec` requests and *returns to the session loop* instead of parking
//! the async stack on the first result. What used to be locals in `run_turn`
//! (the unresolved calls, the round counter) lives here as serde-capable data,
//! so a session can be suspended, persisted (event log + replay), and resumed
//! mid-turn by any embedder — resolution is just `InMsg::ToolResult` messages
//! arriving in any order.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::protocol::ToolEnvelope;
use entanglement_provider::ToolCall;

/// In-flight turn state: `Some` on `Session::turn` exactly while a turn is
/// live (streaming or parked); `None` when the session is idle.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TurnState {
    /// Unresolved tool calls of the current batch, in emit order. Empty while
    /// a round is streaming; filled by [`Self::begin_batch`]; drained by
    /// [`Self::resolve`] as results arrive (any order). Held in dispatch form:
    /// an unwrapped `invoke` call (ADR-0204) by its inner name and args.
    pub pending: Vec<ToolCall>,
    /// The envelope of each pending call core unwrapped from `invoke`
    /// (ADR-0204), keyed by call id, so a re-offered `ToolExec` and the
    /// resolving `ToolOutput` carry the same envelope as the first offer.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub envelopes: HashMap<String, ToolEnvelope>,
    /// LLM round-trips consumed by this turn (`MAX_TURNS` guard, #177). Reset
    /// per prompt by constructing a fresh `TurnState`; a prompt folded into a
    /// live turn (ADR-0058) deliberately does not reset it.
    pub iterations: usize,
    /// Consecutive ambiguous-stop retries in the current stretch (ADR-0118).
    /// Reset to 0 by any round that produces a confident outcome (real tool
    /// calls, or a confident stop) — only a persistently confused model
    /// exhausts the budget. Bounded separately from `iterations`, which
    /// remains the hard backstop (a retry round still counts against it).
    #[serde(default)]
    pub ambiguous_retries: usize,
}

impl TurnState {
    /// Record a freshly emitted batch of tool calls (dispatch form) and the
    /// envelopes of its unwrapped calls as pending.
    pub fn begin_batch(&mut self, calls: Vec<ToolCall>, envelopes: HashMap<String, ToolEnvelope>) {
        self.pending = calls;
        self.envelopes = envelopes;
    }

    /// Resolve one pending call by `request_id`, removing and returning it
    /// with its envelope. `None` for an unknown, duplicate, or stale id — the
    /// caller drops the result rather than corrupting context.
    pub fn resolve(&mut self, request_id: &str) -> Option<(ToolCall, Option<ToolEnvelope>)> {
        let idx = self.pending.iter().position(|c| c.id == request_id)?;
        Some((self.pending.remove(idx), self.envelopes.remove(request_id)))
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
            provider_meta: None,
        }
    }

    #[test]
    fn resolve_drains_out_of_order() {
        let mut t = TurnState::default();
        t.begin_batch(vec![call("a"), call("b"), call("c")], HashMap::new());
        assert!(!t.is_drained());
        assert_eq!(t.resolve("b").map(|(c, _)| c.name), Some("tool_b".into()));
        assert_eq!(t.resolve("c").map(|(c, _)| c.name), Some("tool_c".into()));
        assert_eq!(t.resolve("a").map(|(c, _)| c.name), Some("tool_a".into()));
        assert!(t.is_drained());
    }

    #[test]
    fn resolve_rejects_unknown_and_duplicate_ids() {
        let mut t = TurnState::default();
        t.begin_batch(vec![call("a")], HashMap::new());
        assert!(t.resolve("nope").is_none());
        assert!(t.resolve("a").is_some());
        assert!(t.resolve("a").is_none(), "second resolve is a duplicate");
        assert!(t.is_drained());
    }

    #[test]
    fn serde_round_trips() {
        let mut t = TurnState::default();
        t.begin_batch(vec![call("a"), call("b")], HashMap::new());
        t.iterations = 3;
        let json = serde_json::to_string(&t).expect("serialize");
        let back: TurnState = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back.pending.len(), 2);
        assert_eq!(back.iterations, 3);
        assert_eq!(back.pending[1].id, "b");
    }

    #[test]
    fn resolve_returns_and_drops_the_envelope() {
        let mut t = TurnState::default();
        let envelope = ToolEnvelope {
            tool: "invoke".into(),
            input: r#"{"name":"tool_a"}"#.into(),
        };
        t.begin_batch(
            vec![call("a"), call("b")],
            HashMap::from([("a".to_string(), envelope.clone())]),
        );
        let json = serde_json::to_string(&t).expect("serialize");
        let mut back: TurnState = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back.resolve("b").map(|(_, e)| e), Some(None));
        assert_eq!(back.resolve("a").map(|(_, e)| e), Some(Some(envelope)));
        assert!(back.envelopes.is_empty());
    }

    #[test]
    fn state_without_envelopes_deserializes() {
        let json = r#"{"pending":[{"id":"a","name":"tool_a","input":"{}"}],"iterations":1}"#;
        let t: TurnState = serde_json::from_str(json).expect("deserialize");
        assert!(t.envelopes.is_empty());
        assert!(!serde_json::to_string(&t).unwrap().contains("envelopes"));
    }
}
