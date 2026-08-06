//! Session-scoped **advertisement** of lazily-registered built-ins (#673,
//! ADR-0178): a `LAZY_BUILTINS` tool registered into the process-global
//! [`crate::tools::SharedRegistry`] by one session's overlay enable
//! (ADR-0163 §2) must not appear in every *other* session's advertised tool
//! list — that both hands those sessions a tool nobody opted into and
//! rewrites their `tools` array mid-session, busting the provider prompt
//! cache for every live session at once. Registration stays global (the
//! executor is shared, ADR-0163 §3); only *visibility* is scoped here, the
//! exact split `AvailableMcp::spec_visible` already applies to lazy MCP
//! servers (#542/#630) — and this store reuses that walk's parent map via
//! [`AvailableMcp::enabled_by_or_ancestor`] rather than keeping a third copy
//! of the session tree.
//!
//! Consulted by the runtime `tool_spec_resolver` closure in `main.rs`
//! alongside `spec_visible`; fed by the same `ToolOverlayChanged` events that
//! trigger registration (`crate::bash_live::spawn_lazy_builtin_responder`).

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use entanglement_core::{SessionId, ToolOverlayEntry};

use crate::bash_live::LAZY_BUILTINS;
use crate::mcp::available::AvailableMcp;

/// Which sessions may see each lazily-registrable built-in advertised.
pub struct BuiltinVisibility {
    /// Names registered at startup (`ENTANGLEMENT_ENABLE_BASH`) — visible to
    /// every session with no enablement, exactly as before ADR-0178.
    startup: HashSet<String>,
    /// Lazy name → sessions whose own overlay enabled it. Same shape (and
    /// same #561 empty-set-drop rule) as `AvailableMcp::enabled`.
    enabled: Mutex<HashMap<String, HashSet<SessionId>>>,
}

impl BuiltinVisibility {
    pub fn new(startup: impl IntoIterator<Item = String>) -> Arc<Self> {
        Arc::new(Self {
            startup: startup.into_iter().collect(),
            enabled: Mutex::new(HashMap::new()),
        })
    }

    /// Fold one full `ToolOverlayChanged` entry list for `session`. The
    /// overlay is full-list semantics (ADR-0149): an **enable** disposition
    /// inserts the session's mark, anything else — a deny, or the entry
    /// simply gone from the new list — withdraws it, so a later
    /// `/disable tool bash` re-hides the advertisement even though the
    /// registration itself is permanent. Empty session sets are dropped
    /// (the #561 shape) purely to keep the map from growing; unlike
    /// `spec_visible`, an absent entry here means *invisible*, not visible.
    pub fn fold_overlay(&self, session: &SessionId, entries: &[ToolOverlayEntry]) {
        let mut enabled = self
            .enabled
            .lock()
            .expect("builtin-visibility mutex poisoned");
        for name in LAZY_BUILTINS {
            if ToolOverlayEntry::disposition(entries, name) == Some(true) {
                enabled
                    .entry((*name).to_string())
                    .or_default()
                    .insert(session.clone());
            } else if let Some(sessions) = enabled.get_mut(*name) {
                sessions.remove(session);
                if sessions.is_empty() {
                    enabled.remove(*name);
                }
            }
        }
    }

    /// `SessionEnded` cleanup — drop every mark `session` holds (the
    /// `forget_session` pattern from `mcp::available_lifecycle`). Deliberately
    /// not called on `SessionHibernated`: core replays `ToolOverlayChanged`
    /// on resume, so a resumed session re-folds its marks — but only if a
    /// hibernated one wasn't erased in the meantime by its *own* lifecycle
    /// event arriving after the marks were rebuilt.
    pub fn forget_session(&self, session: &SessionId) {
        self.enabled
            .lock()
            .expect("builtin-visibility mutex poisoned")
            .retain(|_, sessions| {
                sessions.remove(session);
                !sessions.is_empty()
            });
    }

    /// Whether `name` may be advertised to `session`. Non-lazy names always
    /// pass — host tools and MCP names are someone else's concern
    /// (`spec_visible`). A lazy name passes iff it was startup-registered or
    /// `session`'s own overlay chain — itself, or an ancestor, live-resolved
    /// (#630 semantics via [`AvailableMcp::enabled_by_or_ancestor`]) —
    /// enabled it.
    pub fn visible(&self, name: &str, session: &SessionId, avail: &AvailableMcp) -> bool {
        if !LAZY_BUILTINS.contains(&name) {
            return true;
        }
        if self.startup.contains(name) {
            return true;
        }
        let enabled = self
            .enabled
            .lock()
            .expect("builtin-visibility mutex poisoned");
        match enabled.get(name) {
            Some(sessions) => avail.enabled_by_or_ancestor(sessions, session),
            None => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::available::record_parent;

    fn avail() -> AvailableMcp {
        AvailableMcp::default()
    }

    fn s(id: &str) -> SessionId {
        SessionId::new(id)
    }

    #[test]
    fn non_lazy_names_always_pass() {
        let v = BuiltinVisibility::new(None);
        let a = avail();
        assert!(v.visible("read", &s("s1"), &a));
        assert!(v.visible("mcp__srv__tool", &s("s1"), &a));
    }

    #[test]
    fn lazy_name_invisible_by_default_visible_after_enable() {
        let v = BuiltinVisibility::new(None);
        let a = avail();
        assert!(!v.visible("bash", &s("s1"), &a));
        v.fold_overlay(&s("s1"), &[ToolOverlayEntry::ask("bash")]);
        assert!(v.visible("bash", &s("s1"), &a));
        // The regression this module exists for: an unrelated sibling session
        // must NOT see session 1's lazily-enabled built-in.
        assert!(!v.visible("bash", &s("s2"), &a));
    }

    #[test]
    fn deny_or_absent_entry_withdraws_visibility() {
        let v = BuiltinVisibility::new(None);
        let a = avail();
        v.fold_overlay(&s("s1"), &[ToolOverlayEntry::ask("bash")]);
        assert!(v.visible("bash", &s("s1"), &a));
        v.fold_overlay(&s("s1"), &[ToolOverlayEntry::deny("bash")]);
        assert!(!v.visible("bash", &s("s1"), &a));
        v.fold_overlay(&s("s1"), &[ToolOverlayEntry::ask("bash")]);
        v.fold_overlay(&s("s1"), &[]);
        assert!(!v.visible("bash", &s("s1"), &a));
        // Empty sets are dropped, not kept as tombstones.
        assert!(v.enabled.lock().unwrap().is_empty());
    }

    #[test]
    fn startup_registration_is_globally_visible() {
        let v = BuiltinVisibility::new(Some("bash".to_string()));
        let a = avail();
        assert!(v.visible("bash", &s("s1"), &a));
        assert!(v.visible("bash", &s("s2"), &a));
    }

    #[test]
    fn child_inherits_ancestor_enable_sibling_does_not() {
        let v = BuiltinVisibility::new(None);
        let a = avail();
        record_parent(&a, s("child"), Some(s("parent")));
        record_parent(&a, s("sibling"), None);
        v.fold_overlay(&s("parent"), &[ToolOverlayEntry::ask("bash")]);
        assert!(v.visible("bash", &s("parent"), &a));
        assert!(v.visible("bash", &s("child"), &a));
        assert!(!v.visible("bash", &s("sibling"), &a));
    }

    #[test]
    fn forget_session_drops_marks() {
        let v = BuiltinVisibility::new(None);
        let a = avail();
        v.fold_overlay(&s("s1"), &[ToolOverlayEntry::ask("bash")]);
        v.forget_session(&s("s1"));
        assert!(!v.visible("bash", &s("s1"), &a));
        assert!(v.enabled.lock().unwrap().is_empty());
    }
}
