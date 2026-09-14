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

use entanglement_core::{SessionId, ToolSpec};

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

/// Append the `client_side` discovered-tool tail onto `specs` (the sorted
/// kernel prefix already in hand) — resolving each `discovered` name via
/// `resolve` and skipping one already present in the kernel. Gated by
/// ADR-0200's `advertise_discovered` knob: `false` makes this a hard no-op,
/// so `specs` stays byte-frozen at the kernel + profile specs for the whole
/// session — `describe()` still resolves and marks names discovered (that
/// bookkeeping drives the arg-validate dedup guard, ADR-0196 §6, regardless
/// of this knob), only the *advertised array* stops growing. Pulled out of
/// the `tool_spec_resolver` closure in `main.rs` so it unit-tests without the
/// registry/session machinery that closure needs.
pub fn append_discovered_tail(
    specs: &mut Vec<ToolSpec>,
    discovered: &[String],
    advertise_discovered: bool,
    resolve: impl Fn(&str) -> Option<ToolSpec>,
) {
    if !advertise_discovered {
        return;
    }
    for name in discovered {
        if specs.iter().any(|s| s.name == *name) {
            continue;
        }
        if let Some(spec) = resolve(name) {
            specs.push(spec);
        }
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

    fn spec(name: &str) -> ToolSpec {
        ToolSpec::new(name, "d")
    }

    #[test]
    fn advertise_discovered_true_appends_resolved_names_in_order() {
        let mut specs = vec![spec("bash")];
        let discovered = vec!["glob".to_string(), "grep".to_string()];
        append_discovered_tail(&mut specs, &discovered, true, |n| Some(spec(n)));
        assert_eq!(
            specs.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(),
            vec!["bash", "glob", "grep"]
        );
    }

    #[test]
    fn advertise_discovered_false_is_a_hard_no_op() {
        // ADR-0200: the discovered set can be non-empty (describe() still
        // marks it) but the advertised array must not grow — frozen at the
        // kernel prefix for the whole session.
        let mut specs = vec![spec("bash")];
        let discovered = vec!["glob".to_string()];
        append_discovered_tail(&mut specs, &discovered, false, |n| Some(spec(n)));
        assert_eq!(
            specs.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(),
            vec!["bash"]
        );
    }

    #[test]
    fn resolver_output_is_frozen_across_two_describe_rounds_when_opted_out() {
        // ADR-0200: two `describe()` calls each discover a new name — with
        // `advertise_discovered: false` the second round's specs are
        // byte-identical (same names, same order) to the first, even though
        // the discovered set underneath kept growing.
        let mut discovered = DiscoveredSet::new();
        let session = SessionId::new("s");
        let kernel = vec![spec("bash")];

        discovered.mark(&session, "glob");
        let mut round1 = kernel.clone();
        append_discovered_tail(&mut round1, &discovered.names(&session), false, |n| {
            Some(spec(n))
        });

        discovered.mark(&session, "grep");
        let mut round2 = kernel.clone();
        append_discovered_tail(&mut round2, &discovered.names(&session), false, |n| {
            Some(spec(n))
        });

        let names = |v: &[ToolSpec]| v.iter().map(|s| s.name.clone()).collect::<Vec<_>>();
        assert_eq!(names(&round1), vec!["bash".to_string()]);
        assert_eq!(
            names(&round1),
            names(&round2),
            "frozen at the kernel across rounds"
        );
    }

    #[test]
    fn resolver_output_still_appends_on_a_default_provider() {
        // Regression: `advertise_discovered: true` (unset, the default) keeps
        // growing the tail across rounds exactly as before ADR-0200.
        let mut discovered = DiscoveredSet::new();
        let session = SessionId::new("s");
        let kernel = vec![spec("bash")];

        discovered.mark(&session, "glob");
        let mut round1 = kernel.clone();
        append_discovered_tail(&mut round1, &discovered.names(&session), true, |n| {
            Some(spec(n))
        });
        assert_eq!(
            round1.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(),
            vec!["bash", "glob"]
        );

        discovered.mark(&session, "grep");
        let mut round2 = kernel.clone();
        append_discovered_tail(&mut round2, &discovered.names(&session), true, |n| {
            Some(spec(n))
        });
        assert_eq!(
            round2.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(),
            vec!["bash", "glob", "grep"]
        );
    }

    #[test]
    fn already_in_the_kernel_prefix_is_skipped_and_an_unresolvable_name_is_dropped() {
        let mut specs = vec![spec("bash")];
        let discovered = vec!["bash".to_string(), "gone".to_string()];
        append_discovered_tail(&mut specs, &discovered, true, |n| {
            if n == "gone" {
                None
            } else {
                Some(spec(n))
            }
        });
        assert_eq!(
            specs.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(),
            vec!["bash"]
        );
    }
}
