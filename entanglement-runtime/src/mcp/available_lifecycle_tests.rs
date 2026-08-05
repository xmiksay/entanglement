//! Tests for [`super`] (`mcp::available_lifecycle`, #630). Sibling file
//! (`#[path]` child module, so private fields stay reachable) to keep both
//! sides of the 400-line file cap.

use super::*;

/// #630: a spawned child inherits its parent's lazy-server enablement, and
/// transitively a grandchild's — the ancestor walk composes across
/// generations, not just one hop.
#[test]
fn child_inherits_parent_enablement() {
    let avail = AvailableMcp::default();
    let parent = SessionId::new("parent");
    let child = SessionId::new("child");
    let grandchild = SessionId::new("grandchild");
    let unrelated = SessionId::new("unrelated");
    avail.mark_enabled("zread", &parent);
    // Not yet linked: no inheritance.
    assert!(!avail.spec_visible("mcp__zread__search_doc", &child));
    record_parent(&avail, child.clone(), Some(parent.clone()));
    record_parent(&avail, grandchild.clone(), Some(child.clone()));
    assert!(avail.spec_visible("mcp__zread__search_doc", &child));
    assert!(avail.spec_visible("mcp__zread__search_doc", &grandchild));
    assert!(!avail.spec_visible("mcp__zread__search_doc", &unrelated));
    // An ancestor enabling *after* the child already exists is picked up
    // live — the walk is resolved at `spec_visible` time, not snapshotted.
    let late_parent = SessionId::new("late-parent");
    let late_child = SessionId::new("late-child");
    record_parent(&avail, late_child.clone(), Some(late_parent.clone()));
    assert!(!avail.spec_visible("mcp__zread__search_doc", &late_child));
    avail.mark_enabled("zread", &late_parent);
    assert!(avail.spec_visible("mcp__zread__search_doc", &late_child));
}

/// #630: the ancestor walk never loops forever on a malformed cycle in the
/// parent links (mirrors `permission::ancestor_chain`'s own guard).
#[test]
fn ancestor_walk_is_cycle_guarded() {
    let avail = AvailableMcp::default();
    let a = SessionId::new("a");
    let b = SessionId::new("b");
    // Someone unrelated has it enabled, so `spec_visible` doesn't
    // short-circuit `true` on an unenabled server before ever reaching the
    // ancestor walk below.
    avail.mark_enabled("zread", &SessionId::new("someone-else"));
    record_parent(&avail, a.clone(), Some(b.clone()));
    record_parent(&avail, b.clone(), Some(a.clone()));
    assert!(!avail.spec_visible("mcp__zread__search_doc", &a));
}

/// #630: `SessionEnded` drops a session's enablement marks on every server it
/// touched, and its parent link — otherwise both maps grow for the process
/// lifetime, one entry per session that ever spawned or enabled a lazy
/// server.
#[test]
fn forget_session_clears_enablement_and_parent_link() {
    let avail = AvailableMcp::default();
    let parent = SessionId::new("parent");
    let child = SessionId::new("child");
    let other = SessionId::new("other");
    avail.mark_enabled("zread", &parent);
    avail.mark_enabled("zread", &other);
    record_parent(&avail, child.clone(), Some(parent.clone()));
    forget_session(&avail, &parent);
    // The ended session's own direct mark is gone...
    assert!(!avail.spec_visible("mcp__zread__search_doc", &parent));
    // ...and so is its inheritance link, so its child no longer sees it either.
    assert!(!avail.spec_visible("mcp__zread__search_doc", &child));
    // A different session that also enabled it is untouched.
    assert!(avail.spec_visible("mcp__zread__search_doc", &other));
    assert!(
        avail.is_lazy("zread"),
        "another session still has it enabled"
    );
}

/// #630: `forget_session` for the *last* enabling session drops the
/// bookkeeping entry entirely (mirrors #561's `mark_disabled` guarantee) —
/// an empty `HashSet` left behind would hide the server from every session,
/// not just the one that ended.
#[test]
fn forget_session_by_the_last_session_drops_the_entry() {
    let avail = AvailableMcp::default();
    let a = SessionId::new("a");
    let c = SessionId::new("c");
    avail.mark_enabled("zread", &a);
    forget_session(&avail, &a);
    assert!(!avail.is_lazy("zread"));
    assert!(avail.spec_visible("mcp__zread__search_doc", &c));
}
