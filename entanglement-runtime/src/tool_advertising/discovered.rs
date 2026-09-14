//! Session-keyed discovered-tool set (#560, ADR-0196 §3 — `client_side`
//! encoding): grows via a successful `describe()` call under a
//! `ToolSearch`-mode session, or (ADR-0196 §6) via
//! [`crate::arg_validate`]'s schema-violation decline delivering a tool's
//! full schema regardless of mode — both share this same tracking so the
//! decline guard never re-sends a schema `describe()` (or a prior decline)
//! already put in context. **Never shrinks mid-session** — the resolver
//! appends these names' specs after the lean kernel, so the advertised
//! array's prefix (kernel + profile-defining specs) never changes, only its
//! tail grows. Discovery order is preserved, not re-sorted: sorting the whole
//! array would let a later-discovered, alphabetically-earlier tool insert
//! into the middle of an already-cached prefix instead of at the end —
//! exactly the cache invalidation the append-only design exists to avoid.

use std::collections::HashMap;

use entanglement_core::SessionId;

/// One session's discovered names, in first-discovery order, plus the whole
/// engine's session→names map. Lives behind [`super::AdvertisingState`]'s
/// mutex, shared by the executor's `describe` dispatch (writer) and the
/// `tool_spec_resolver` closure (reader).
#[derive(Debug, Default)]
pub struct DiscoveredSet {
    per_session: HashMap<SessionId, Vec<String>>,
}

impl DiscoveredSet {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record `name` as discovered for `session` — idempotent: a name already
    /// present keeps its original discovery-order position, so a repeat
    /// `describe` of the same tool never moves it in the advertised tail.
    pub fn mark(&mut self, session: &SessionId, name: &str) {
        let names = self.per_session.entry(session.clone()).or_default();
        if !names.iter().any(|n| n == name) {
            names.push(name.to_string());
        }
    }

    /// This session's discovered names, in discovery order. Empty for a
    /// session that has discovered nothing (or doesn't exist) — never a
    /// distinguished error, since the resolver's fallback is simply "no
    /// extra tail yet".
    pub fn names(&self, session: &SessionId) -> Vec<String> {
        self.per_session.get(session).cloned().unwrap_or_default()
    }

    /// Whether `name`'s schema was already delivered to `session` this
    /// session — via a `describe()` result or (ADR-0196 §6)
    /// [`crate::arg_validate`]'s schema-violation decline, which also calls
    /// [`mark`][Self::mark]. The delivered-schema dedup guard reads this
    /// before re-sending a full schema on a repeat violation.
    pub fn contains(&self, session: &SessionId, name: &str) -> bool {
        self.per_session
            .get(session)
            .is_some_and(|names| names.iter().any(|n| n == name))
    }

    /// Release an ended/hibernated session's entry. A resume starts
    /// rediscovery fresh — the client_side encoding re-sends `describe` calls
    /// naturally, since the model doesn't remember what a *previous* process
    /// life discovered either.
    pub fn forget(&mut self, session: &SessionId) {
        self.per_session.remove(session);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mark_is_append_only_and_idempotent() {
        let mut set = DiscoveredSet::new();
        let s = SessionId::new("s");
        assert_eq!(set.names(&s), Vec::<String>::new());
        set.mark(&s, "glob");
        set.mark(&s, "grep");
        set.mark(&s, "glob"); // repeat — must not move or duplicate
        assert_eq!(set.names(&s), vec!["glob".to_string(), "grep".to_string()]);
    }

    #[test]
    fn sessions_are_independent() {
        let mut set = DiscoveredSet::new();
        let a = SessionId::new("a");
        let b = SessionId::new("b");
        set.mark(&a, "glob");
        assert_eq!(set.names(&a), vec!["glob".to_string()]);
        assert_eq!(set.names(&b), Vec::<String>::new());
    }

    #[test]
    fn forget_drops_the_session() {
        let mut set = DiscoveredSet::new();
        let s = SessionId::new("s");
        set.mark(&s, "glob");
        set.forget(&s);
        assert_eq!(set.names(&s), Vec::<String>::new());
    }
}
