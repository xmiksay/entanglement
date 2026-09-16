//! ADR-0201 tier classification for [`super::AvailableMcp`]: whether an
//! `mcp__<server>__*` name's `server` is eligible for a zero-approval lazy
//! (re-)connect, explicitly `disabled`, or genuinely unknown — read live
//! from the roster with **no connect attempt** — plus the dispatch-time
//! re-enable failure cooldown `available_enable.rs`'s `try_lazy_reenable`
//! consults before it *does* attempt one. Split out of `available.rs` along
//! the 400-line file cap (mirrors `available_lifecycle.rs`'s #630 split) —
//! sibling `#[path]` child module, so private fields stay reachable via the
//! normal descendant-module visibility rule; the cooldown methods are
//! `pub(super)` so `available_enable.rs` (a sibling module, not a
//! descendant of this one) can call them too.

use std::time::{Duration, Instant};

use super::AvailableMcp;

/// Which ADR-0152 tier a `mcp__<server>__*` name's `server` resolves to,
/// read live from the roster with **no connect attempt** (ADR-0201) —
/// shared by the runtime's out-of-mask hard-limit check (decide whether to
/// hard-refuse or park an approval) and dispatch's own unknown-tool
/// self-heal (decide whether a real connect is even worth attempting).
/// Never confuse this with a *connect* outcome — `LazyReenableOutcome`
/// (`available_enable.rs`) is that.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpTier {
    /// `allowed`, or already lazily-connected (by this or another session) —
    /// eligible for the same zero-approval connect `mcp_enable`/
    /// `/enable mcp` perform. Also covers the edge case of a startup
    /// `enabled` server whose registration is momentarily missing (that
    /// server is in neither `servers` nor `enabled`, but the immediate
    /// unknown-tool report on it would be actively wrong).
    Eligible,
    /// Explicitly configured `disabled` — a **known** name, but consent was
    /// withheld by configuration, not merely absent.
    Disabled,
    /// No registered tool, no configured/bundled server under this name in
    /// any tier — genuinely unknown.
    Unknown,
}

/// Parse `mcp__<server>__<tool>` into `server` — `None` for a name that
/// isn't namespaced this way (a host tool, or malformed). Shared by
/// [`AvailableMcp::spec_visible`](super::AvailableMcp::spec_visible) and the
/// ADR-0201 tier checks.
pub fn server_name_of(tool_name: &str) -> Option<&str> {
    tool_name.strip_prefix("mcp__")?.split("__").next()
}

/// The truthful, attributed decline for a [`McpTier::Disabled`] server
/// (ADR-0201) — shared by every call site that reports it, so the wording is
/// identical regardless of which one refuses the call.
pub fn disabled_decline(server: &str) -> String {
    format!(
        "mcp server `{server}` is disabled by configuration — its tools cannot be enabled this session"
    )
}

impl AvailableMcp {
    /// Classify `server` per [`McpTier`] (ADR-0201) — a pure roster read, no
    /// connect attempt. `get`/`is_lazy` cover `allowed`-or-already-connected;
    /// `disabled_names` (populated only by `partition`) covers an explicit
    /// `disabled`; everything else is genuinely unknown.
    pub fn tier_of(&self, server: &str) -> McpTier {
        if self.get(server).is_some() || self.is_lazy(server) {
            McpTier::Eligible
        } else if self.disabled_names.contains(server) {
            McpTier::Disabled
        } else {
            McpTier::Unknown
        }
    }

    /// Whether `server` failed a dispatch-time lazy re-enable within
    /// `cooldown` (ADR-0201) — the stampede guard a caller consults before
    /// retrying the connect.
    pub(super) fn recently_failed_enable(&self, server: &str, cooldown: Duration) -> bool {
        self.recent_enable_failures
            .lock()
            .expect("available-server enable-failure mutex poisoned")
            .get(server)
            .is_some_and(|at| at.elapsed() < cooldown)
    }

    /// Record a dispatch-time lazy re-enable failure for `server` (ADR-0201),
    /// starting/refreshing its cooldown window.
    pub(super) fn record_enable_failure(&self, server: &str) {
        self.recent_enable_failures
            .lock()
            .expect("available-server enable-failure mutex poisoned")
            .insert(server.to_string(), Instant::now());
    }

    /// Clear a server's recorded failure (ADR-0201) — called after a
    /// successful re-enable so a later transient failure doesn't inherit a
    /// stale cooldown start.
    pub(super) fn clear_enable_failure(&self, server: &str) {
        self.recent_enable_failures
            .lock()
            .expect("available-server enable-failure mutex poisoned")
            .remove(server);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_name_of_parses_the_namespaced_form_only() {
        assert_eq!(
            server_name_of("mcp__testserver__search"),
            Some("testserver")
        );
        assert_eq!(server_name_of("mcp__testserver__"), Some("testserver"));
        assert_eq!(server_name_of("bash"), None);
        assert_eq!(server_name_of(""), None);
    }

    #[test]
    fn tier_of_classifies_eligible_disabled_and_unknown() {
        // Direct field access to `disabled_names` (private, but this test
        // module descends from `available`, same as `tier_of` itself) —
        // mirrors `available_tests.rs`'s existing struct-literal construction.
        let mut avail = AvailableMcp::default();
        avail.disabled_names.insert("off".to_string());
        assert_eq!(avail.tier_of("off"), McpTier::Disabled);
        assert_eq!(avail.tier_of("nope"), McpTier::Unknown);
        avail.mark_enabled("lazy", &entanglement_core::SessionId::new("s"));
        assert_eq!(avail.tier_of("lazy"), McpTier::Eligible);
    }

    #[test]
    fn cooldown_guard_expires() {
        let avail = AvailableMcp::default();
        assert!(!avail.recently_failed_enable("srv", Duration::from_secs(30)));
        avail.record_enable_failure("srv");
        assert!(avail.recently_failed_enable("srv", Duration::from_secs(30)));
        assert!(!avail.recently_failed_enable("srv", Duration::from_secs(0)));
        avail.clear_enable_failure("srv");
        assert!(!avail.recently_failed_enable("srv", Duration::from_secs(30)));
    }
}
