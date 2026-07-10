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
//!
//! Both live in the runtime tool executor's single-threaded loop, folded from the
//! same lifecycle events as permission dispatch — zero core surface.

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
        AgentProfile {
            name: name.into(),
            mode,
            system_prompt: String::new(),
            model: None,
            permission,
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
