//! Sub-agent permission gating (#77, ADR-0024). Two runtime-only policies layered
//! on top of the per-tool `Allow | Ask | Deny` dispatch (#59):
//!
//! - **Spawn capability** — [`spawn_capability_refusal`]: only `Primary`-mode
//!   profiles may call `agent_spawn`. A read-only sub-agent leaf (`Subagent`
//!   mode, e.g. `explore`) is refused, closing the path where a restricted
//!   profile escalates by spawning a privileged child.
//! - **Privilege ceiling** — [`effective_permission`]: a child sub-agent is never
//!   more privileged than its ancestors. Its effective permission for a tool is
//!   the least-privileged `for_tool` across the session and every ancestor
//!   (`Deny < Ask < Allow`), so a child cannot touch the shared working tree in
//!   ways the parent couldn't.
//! - **Tool mask** — [`tool_masked`] (#116, ADR-0038): a tool omitted from a
//!   profile's allowlist (or listed in its denylist) does not *exist* for that
//!   session — a call is refused before permission is even resolved. Like the
//!   ceiling it clamps down the ancestor chain (a child never gains a tool an
//!   ancestor lacked). This is the enforcement half of the physical restriction
//!   whose advertisement half lives in core's `run_turn`.
//!
//! All three live in the runtime tool executor's single-threaded loop, folded
//! from the same lifecycle events as permission dispatch — zero core surface.

use std::collections::{HashMap, HashSet};

use entanglement_core::{AgentMode, AgentProfile, Permission, SessionId};

use crate::subagent::SpawnGuard;

/// Whether the active `profile` may spawn a sub-agent. Returns `None` when
/// spawning is allowed (`Primary` mode, or an unknown session — nothing to gate
/// on), else the refusal message to relay to the parent's parked tool call.
pub fn spawn_capability_refusal(profile: Option<&AgentProfile>) -> Option<String> {
    match profile.map(|p| p.mode) {
        Some(AgentMode::Subagent) => Some(
            "sub-agent spawn refused: a read-only sub-agent profile cannot spawn \
             further sub-agents. Do the work directly."
                .to_string(),
        ),
        _ => None,
    }
}

/// Effective permission for `tool` in `session`, clamped so a child sub-agent is
/// never more privileged than its ancestors. Walks the parent chain in `guard`,
/// taking the least-privileged `for_tool` across the session and every ancestor.
/// A root has no ancestors, so this reduces to its own profile — single-session
/// behavior is unchanged.
pub fn effective_permission(
    active: &HashMap<SessionId, AgentProfile>,
    guard: &SpawnGuard,
    session: &SessionId,
    tool: &str,
) -> Permission {
    let mut perm = permission_for(active, session, tool);
    let mut current = session.clone();
    // Guard against a malformed cycle in the parent links (mirrors SpawnGuard).
    let mut visited = HashSet::new();
    while visited.insert(current.clone()) {
        match guard.parent_of(&current) {
            Some(parent) => {
                perm = min_permission(perm, permission_for(active, &parent, tool));
                current = parent;
            }
            None => break,
        }
    }
    perm
}

/// Whether `tool` is masked out for `session` — refused because it is not in the
/// effective advertised set (#116, ADR-0038). A tool is available only if the
/// session's own profile *and* every ancestor's profile advertise it: the mask
/// intersects down the chain, so a child never gains a tool an ancestor lacked
/// (mirrors [`effective_permission`]'s privilege ceiling). An unseen session in
/// the chain masks nothing (default-open, matching the permission fallback).
///
/// Orthogonal to permission: this decides a tool's *existence*, the `for_tool`
/// grade decides `Allow`/`Ask`/`Deny` among the tools that survive here.
pub fn tool_masked(
    active: &HashMap<SessionId, AgentProfile>,
    guard: &SpawnGuard,
    session: &SessionId,
    tool: &str,
) -> bool {
    let mut current = session.clone();
    // Guard against a malformed cycle in the parent links (mirrors SpawnGuard).
    let mut visited = HashSet::new();
    while visited.insert(current.clone()) {
        if let Some(profile) = active.get(&current) {
            if !profile.advertises_tool(tool) {
                return true;
            }
        }
        match guard.parent_of(&current) {
            Some(parent) => current = parent,
            None => break,
        }
    }
    false
}

/// A session's own permission for `tool`; an unseen session defaults to `Allow`
/// (nothing to gate on), matching the pre-#77 fallback.
fn permission_for(
    active: &HashMap<SessionId, AgentProfile>,
    session: &SessionId,
    tool: &str,
) -> Permission {
    active
        .get(session)
        .map(|p| p.permission.for_tool(tool))
        .unwrap_or(Permission::Allow)
}

/// The least-privileged of two permissions, ordered `Deny < Ask < Allow`.
fn min_permission(a: Permission, b: Permission) -> Permission {
    if rank(a) <= rank(b) {
        a
    } else {
        b
    }
}

fn rank(p: Permission) -> u8 {
    match p {
        Permission::Deny => 0,
        Permission::Ask => 1,
        Permission::Allow => 2,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use entanglement_core::PermissionProfile;

    fn profile(name: &str, mode: AgentMode, permission: PermissionProfile) -> AgentProfile {
        masked_profile(name, mode, permission, None, Vec::new())
    }

    fn masked_profile(
        name: &str,
        mode: AgentMode,
        permission: PermissionProfile,
        tools: Option<Vec<&str>>,
        disallowed: Vec<&str>,
    ) -> AgentProfile {
        AgentProfile {
            name: name.into(),
            description: String::new(),
            mode,
            system_prompt: String::new(),
            model: None,
            permission,
            tools: tools.map(|v| v.into_iter().map(String::from).collect()),
            disallowed_tools: disallowed.into_iter().map(String::from).collect(),
        }
    }

    #[test]
    fn primary_may_spawn_subagent_may_not() {
        let build = profile(
            "build",
            AgentMode::Primary,
            PermissionProfile::new(Permission::Allow),
        );
        let explore = profile(
            "explore",
            AgentMode::Subagent,
            PermissionProfile::new(Permission::Deny),
        );
        assert!(spawn_capability_refusal(Some(&build)).is_none());
        // Unknown session (never started) is not gated on.
        assert!(spawn_capability_refusal(None).is_none());
        let refusal = spawn_capability_refusal(Some(&explore)).expect("subagent must be refused");
        assert!(refusal.contains("cannot spawn"), "got: {refusal}");
    }

    #[test]
    fn child_permission_is_clamped_to_parent() {
        // Parent `plan`: read allowed, everything else Ask. Child `build`: allow-all.
        let plan = profile(
            "plan",
            AgentMode::Primary,
            PermissionProfile::new(Permission::Ask).with("read", Permission::Allow),
        );
        let build = profile(
            "build",
            AgentMode::Primary,
            PermissionProfile::new(Permission::Allow),
        );

        let parent = SessionId::new("parent");
        let child = SessionId::new("child");
        let mut active = HashMap::new();
        active.insert(parent.clone(), plan);
        active.insert(child.clone(), build);

        let mut guard = SpawnGuard::new();
        guard.record_start(parent.clone(), None);
        guard.record_start(child.clone(), Some(parent.clone()));

        // `edit` is Allow on the child alone, but Ask on the parent → clamped to Ask.
        assert_eq!(
            effective_permission(&active, &guard, &child, "edit"),
            Permission::Ask
        );
        // `read` is Allow on both → stays Allow.
        assert_eq!(
            effective_permission(&active, &guard, &child, "read"),
            Permission::Allow
        );
        // The parent (a root) is never loosened or clamped — its own profile stands.
        assert_eq!(
            effective_permission(&active, &guard, &parent, "edit"),
            Permission::Ask
        );
    }

    #[test]
    fn tool_mask_refuses_tool_absent_from_allowlist() {
        // A read-only leaf: only read/glob/grep advertised.
        let explore = masked_profile(
            "explore",
            AgentMode::Subagent,
            PermissionProfile::new(Permission::Deny),
            Some(vec!["read", "glob", "grep"]),
            Vec::new(),
        );
        let s = SessionId::new("s");
        let mut active = HashMap::new();
        active.insert(s.clone(), explore);
        let guard = SpawnGuard::new();
        assert!(!tool_masked(&active, &guard, &s, "read"));
        assert!(tool_masked(&active, &guard, &s, "edit"));
        assert!(tool_masked(&active, &guard, &s, "agent_spawn"));
        // An unseen session masks nothing (default-open).
        assert!(!tool_masked(
            &active,
            &guard,
            &SessionId::new("other"),
            "edit"
        ));
    }

    #[test]
    fn tool_mask_clamps_down_the_ancestor_chain() {
        // Parent restricted to [read]; child would advertise [read, edit] on its
        // own, but the intersection down the chain drops `edit`.
        let parent = masked_profile(
            "restricted",
            AgentMode::Primary,
            PermissionProfile::new(Permission::Allow),
            Some(vec!["read"]),
            Vec::new(),
        );
        let child = masked_profile(
            "build",
            AgentMode::Primary,
            PermissionProfile::new(Permission::Allow),
            Some(vec!["read", "edit"]),
            Vec::new(),
        );
        let p = SessionId::new("parent");
        let c = SessionId::new("child");
        let mut active = HashMap::new();
        active.insert(p.clone(), parent);
        active.insert(c.clone(), child);
        let mut guard = SpawnGuard::new();
        guard.record_start(p.clone(), None);
        guard.record_start(c.clone(), Some(p.clone()));

        // `read` survives both → available on the child.
        assert!(!tool_masked(&active, &guard, &c, "read"));
        // `edit` is on the child alone but masked by the parent → refused.
        assert!(tool_masked(&active, &guard, &c, "edit"));
        // The parent (a root) keeps its own mask unchanged.
        assert!(tool_masked(&active, &guard, &p, "edit"));
    }

    #[test]
    fn root_with_no_ancestors_uses_own_profile() {
        let build = profile(
            "build",
            AgentMode::Primary,
            PermissionProfile::new(Permission::Allow),
        );
        let root = SessionId::new("root");
        let mut active = HashMap::new();
        active.insert(root.clone(), build);
        let guard = SpawnGuard::new();
        assert_eq!(
            effective_permission(&active, &guard, &root, "edit"),
            Permission::Allow
        );
    }
}
