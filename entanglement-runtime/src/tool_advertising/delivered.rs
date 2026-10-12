//! Session-keyed set of tool schemas already delivered to the model
//! (ADR-0196 §6, re-keyed by ADR-0226): an `mcp_tools` read marks every tool
//! it listed, and an arg-validate decline marks the tool it declined, so a
//! repeat schema violation answers "already provided above" instead of
//! resending the same JSON.

use std::collections::{HashMap, HashSet};

use entanglement_core::SessionId;

#[derive(Debug, Default)]
pub struct DeliveredSchemas {
    per_session: HashMap<SessionId, HashSet<String>>,
}

impl DeliveredSchemas {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record `name` as delivered to `session` (idempotent).
    pub fn mark(&mut self, session: &SessionId, name: &str) {
        self.per_session
            .entry(session.clone())
            .or_default()
            .insert(name.to_string());
    }

    pub fn contains(&self, session: &SessionId, name: &str) -> bool {
        self.per_session
            .get(session)
            .is_some_and(|names| names.contains(name))
    }

    /// Release an ended/hibernated session. A resumed session's model may no
    /// longer hold the old reply in a compacted context, so it starts fresh.
    pub fn forget(&mut self, session: &SessionId) {
        self.per_session.remove(session);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn marks_are_per_session_idempotent_and_forgettable() {
        let mut set = DeliveredSchemas::new();
        let (a, b) = (SessionId::new("a"), SessionId::new("b"));
        set.mark(&a, "mcp__gh__search");
        set.mark(&a, "mcp__gh__search");
        assert!(set.contains(&a, "mcp__gh__search"));
        assert!(!set.contains(&b, "mcp__gh__search"));
        set.forget(&a);
        assert!(!set.contains(&a, "mcp__gh__search"));
    }
}
