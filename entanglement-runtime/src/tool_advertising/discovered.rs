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

use entanglement_core::{Discovery, SessionId, ToolSpec};

use crate::tool_names::TOOL_SEARCH_KERNEL;

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

/// The `client_side` advertised array (ADR-0196 §3, ADR-0204): the sorted
/// kernel picked out of `pool`, then per `discovery` either the discovered
/// tail in discovery order (`append`, each name resolved via `resolve`, one
/// already in the kernel skipped) or the `invoke` envelope. Under
/// `native_first`/`invoke` `discovered` is ignored, so the array is
/// byte-stable for the whole session whatever `describe`
/// or an arg-validate decline records. Pulled out of the `tool_spec_resolver`
/// closure in `main.rs` so it unit-tests without the registry.
pub fn client_side_surface(
    pool: Vec<ToolSpec>,
    discovered: &[String],
    discovery: Discovery,
    resolve: impl Fn(&str) -> Option<ToolSpec>,
) -> Vec<ToolSpec> {
    let mut specs: Vec<_> = pool
        .into_iter()
        .filter(|s| TOOL_SEARCH_KERNEL.contains(&s.name.as_str()))
        .collect();
    specs.sort_by(|a, b| a.name.cmp(&b.name));
    specs.dedup_by(|a, b| a.name == b.name);
    if discovery.advertises_invoke() {
        specs.push(crate::discover::invoke_spec());
        return specs;
    }
    for name in discovered {
        if specs.iter().any(|s| s.name == *name) {
            continue;
        }
        if let Some(spec) = resolve(name) {
            specs.push(spec);
        }
    }
    specs
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

    fn spec(name: &str) -> ToolSpec {
        ToolSpec::new(name, "d")
    }

    fn names(specs: &[ToolSpec]) -> Vec<&str> {
        specs.iter().map(|s| s.name.as_str()).collect()
    }

    /// `glob`/`grep` are registered but not kernel; `read`/`bash` are.
    fn pool() -> Vec<ToolSpec> {
        ["read", "glob", "bash", "grep", "read"].map(spec).to_vec()
    }

    fn surface(discovered: &DiscoveredSet, s: &SessionId, d: Discovery) -> Vec<ToolSpec> {
        client_side_surface(pool(), &discovered.names(s), d, |n| Some(spec(n)))
    }

    #[test]
    fn append_grows_the_tail_in_discovery_order_after_the_sorted_kernel() {
        let mut discovered = DiscoveredSet::new();
        let s = SessionId::new("s");
        assert_eq!(
            names(&surface(&discovered, &s, Discovery::Append)),
            ["bash", "read"]
        );
        discovered.mark(&s, "grep");
        discovered.mark(&s, "glob");
        assert_eq!(
            names(&surface(&discovered, &s, Discovery::Append)),
            ["bash", "read", "grep", "glob"]
        );
    }

    #[test]
    fn native_first_and_invoke_are_byte_stable_across_discoveries() {
        for d in [Discovery::NativeFirst, Discovery::Invoke] {
            let mut discovered = DiscoveredSet::new();
            let s = SessionId::new("s");
            let before = surface(&discovered, &s, d);
            discovered.mark(&s, "grep");
            discovered.mark(&s, "glob");
            let after = surface(&discovered, &s, d);
            assert_eq!(names(&before), ["bash", "read", "invoke"], "{d:?}");
            assert_eq!(format!("{before:?}"), format!("{after:?}"), "{d:?}");
        }
    }

    #[test]
    fn already_in_the_kernel_prefix_is_skipped_and_an_unresolvable_name_is_dropped() {
        let discovered = vec!["bash".to_string(), "gone".to_string()];
        let specs = client_side_surface(pool(), &discovered, Discovery::Append, |n| {
            (n != "gone").then(|| spec(n))
        });
        assert_eq!(names(&specs), ["bash", "read"]);
    }
}
