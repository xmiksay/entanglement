//! Sub-agent permission gating (#77, ADR-0024; #119, ADR-0040). Runtime-only
//! policies layered on top of the per-tool `Allow | Ask | Deny` dispatch (#59):
//!
//! - **Spawn control** — [`spawn_refusal`] (#119): the per-profile spawn gate,
//!   checked *before* the SpawnGuard budget (ADR-0023) and the ancestor clamp
//!   (ADR-0024). It layers four checks in front of them: the spawner
//!   [`may_spawn`][entanglement_core::AgentProfile::may_spawn] at all (absorbs
//!   the old ADR-0024 capability gate — a `Subagent` leaf or `can_spawn: false`
//!   profile is refused the whole family); the target resolves to a real
//!   profile; the target is spawnable-mode (a `primary` entry agent is never a
//!   valid target, so `build`/`plan` are unreachable via spawn); and the target
//!   is on the spawner's `spawnable_agents` allowlist. Checked against the
//!   spawner's *own* profile, so the allowlist is not transitive.
//! - **Privilege ceiling** — [`effective_permission`]: a child sub-agent is never
//!   more privileged than its ancestors. Its effective permission for a tool call
//!   is the least-privileged `resolve` across the session and every ancestor
//!   (`Deny < Ask < Allow`), so a child cannot touch the shared working tree in
//!   ways the parent couldn't. Resolution takes the call's tool-specific argument
//!   (command/path, #173) so an argument-scoped rule matches the actual input;
//!   [`permission_arg`] extracts it.
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

use entanglement_core::{AgentProfile, Permission, PermissionProfile, ProfileRegistry, SessionId};

use crate::subagent::SpawnGuard;

/// Per-profile spawn gate for `spawner` launching `target` (#119, ADR-0040),
/// checked *before* the SpawnGuard budget (ADR-0023) and the ancestor clamp
/// (ADR-0024). Returns `None` when the spawn is permitted, else the refusal
/// message to relay to the parent's parked tool call. Layered checks, in order:
///
/// 1. spawner may not spawn ([`may_spawn`][AgentProfile::may_spawn]) — a leaf or
///    `can_spawn: false` profile is refused the whole family (this absorbs the
///    old capability gate, same "cannot spawn" phrasing);
/// 2. unknown target — the name resolves to no registered profile;
/// 3. target not spawnable-mode — a `primary` entry agent is never a valid
///    target, so `build`/`plan` are unreachable via spawn;
/// 4. target outside the spawner's `spawnable_agents` allowlist.
///
/// An unknown spawner session (never started) is not gated — nothing to check.
pub fn spawn_refusal(
    spawner: Option<&AgentProfile>,
    target: &str,
    registry: &ProfileRegistry,
) -> Option<String> {
    let spawner = spawner?;
    if !spawner.may_spawn() {
        return Some(
            "sub-agent spawn refused: this agent profile cannot spawn further \
             sub-agents. Do the work directly."
                .to_string(),
        );
    }
    let target_profile = match registry.get(target) {
        Some(p) => p,
        None => {
            return Some(format!(
                "sub-agent spawn refused: unknown agent profile `{target}`."
            ))
        }
    };
    if !target_profile.spawnable_as_subagent() {
        return Some(format!(
            "sub-agent spawn refused: `{target}` is a primary entry agent, not a \
             spawnable sub-agent. Pick a sub-agent profile."
        ));
    }
    if !spawner.spawn_target_allowed(target) {
        return Some(format!(
            "sub-agent spawn refused: this agent profile is not allowed to spawn \
             `{target}`. Pick one of its permitted sub-agents."
        ));
    }
    None
}

/// Effective permission for a `tool` call in `session`, clamped so a child
/// sub-agent is never more privileged than its ancestors. Walks the parent chain
/// in `guard`, taking the least-privileged `resolve` across the session and every
/// ancestor. `arg` is the tool-specific argument (command/path, #173) so
/// argument-scoped rules resolve against the actual call; pass `None` for a
/// name-only decision. A root has no ancestors, so this reduces to its own
/// profile — single-session behavior is unchanged.
pub fn effective_permission(
    active: &HashMap<SessionId, AgentProfile>,
    guard: &SpawnGuard,
    session: &SessionId,
    tool: &str,
    arg: Option<&str>,
) -> Permission {
    let (perm, source) = resolve_with_source(active, guard, session, tool, arg);
    // Per-resolution trace (#189) so sub-agent debugging ("why was this child's
    // edit denied?") reads off logs instead of three `.md` layers by hand.
    tracing::debug!(
        %session,
        tool,
        rule = ?perm,
        source = match &source {
            Some(id) => format!("ancestor {id}"),
            None => "own".to_string(),
        },
        "permission resolved",
    );
    perm
}

/// The clamped permission plus *which* link decided it (#189): `None` ⇒ the
/// session's own profile stands; `Some(id)` ⇒ that ancestor clamped it down.
/// Split from [`effective_permission`] so the deciding source is unit-testable
/// without capturing the trace it feeds.
fn resolve_with_source(
    active: &HashMap<SessionId, AgentProfile>,
    guard: &SpawnGuard,
    session: &SessionId,
    tool: &str,
    arg: Option<&str>,
) -> (Permission, Option<SessionId>) {
    let mut perm = permission_for(active, session, tool, arg);
    let mut source: Option<SessionId> = None;
    let mut current = session.clone();
    // Guard against a malformed cycle in the parent links (mirrors SpawnGuard).
    let mut visited = HashSet::new();
    while visited.insert(current.clone()) {
        match guard.parent_of(&current) {
            Some(parent) => {
                let clamped = min_permission(perm, permission_for(active, &parent, tool, arg));
                // Only a *strictly* lower ancestor changes the outcome (ties keep
                // the nearer link), so record it as the deciding source.
                if clamped != perm {
                    source = Some(parent.clone());
                }
                perm = clamped;
                current = parent;
            }
            None => break,
        }
    }
    (perm, source)
}

/// Whether `tool` is masked out for `session` — refused because it is not in the
/// effective advertised set (#116, ADR-0038). A tool is available only if the
/// session's own profile *and* every ancestor's profile advertise it: the mask
/// intersects down the chain, so a child never gains a tool an ancestor lacked
/// (mirrors [`effective_permission`]'s privilege ceiling). An unseen session in
/// the chain masks **everything** — **fail-closed** (#156): under broadcast
/// overload a dropped `SessionStarted`/`AgentChanged` must not silently un-mask a
/// restricted session. The executor self-heals the leaf from `ToolExec.agent`, so
/// this fires only for a genuinely-unknown session (matching the [`permission_for`]
/// `Deny` fallback).
///
/// Orthogonal to permission: this decides a tool's *existence*, the `resolve`
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
        match active.get(&current) {
            Some(profile) if !profile.advertises_tool(tool) => return true,
            Some(_) => {}
            // Unseen session in the chain ⇒ fail-closed (#156).
            None => return true,
        }
        match guard.parent_of(&current) {
            Some(parent) => current = parent,
            None => break,
        }
    }
    false
}

/// The ordered permission profiles the effective grade folds over (#173): the
/// session's own profile followed by each ancestor, walking `guard`'s parent
/// links. The rhai binding policy captures this once per run and resolves each
/// binding call against it with the call's argument, matching
/// [`effective_permission`]'s least-privilege clamp while letting argument-scoped
/// rules see the actual input. An unseen session contributes nothing.
pub(crate) fn permission_chain(
    active: &HashMap<SessionId, AgentProfile>,
    guard: &SpawnGuard,
    session: &SessionId,
) -> Vec<PermissionProfile> {
    let mut chain = Vec::new();
    let mut current = session.clone();
    // Guard against a malformed cycle in the parent links (mirrors SpawnGuard).
    let mut visited = HashSet::new();
    while visited.insert(current.clone()) {
        if let Some(profile) = active.get(&current) {
            chain.push(profile.permission.clone());
        }
        match guard.parent_of(&current) {
            Some(parent) => current = parent,
            None => break,
        }
    }
    chain
}

/// Clamp an already-resolved permission by the global config base (#172,
/// ADR-0047). The effective grade is the least-privileged of the agent-chain
/// result and the config's rule for the `tool` call — so the user/repo config
/// `permissions` section is a *ceiling*: it can tighten what an agent allows
/// (`bash: ask` forces every agent to ask), never loosen it. `arg` carries the
/// tool-specific argument (#173) so an argument-scoped ceiling rule like
/// `bash(rm *): deny` resolves against the actual command. The embedded default
/// is allow-all, so an untouched config is a no-op. This is a pure ceiling (it
/// only tightens); the orthogonal "always allow" grants (#174, [`crate::grants`])
/// that *raise* an `Ask` are applied by the executor *after* this clamp, so a
/// `Deny` here can never be re-opened by a stale grant.
pub fn clamp_to_base(
    perm: Permission,
    base: &PermissionProfile,
    tool: &str,
    arg: Option<&str>,
) -> Permission {
    min_permission(perm, base.resolve(tool, arg))
}

/// A session's own permission for a `tool` call; an unseen session defaults to
/// `Deny` — **fail-closed** (#156). The executor folds its per-session profile
/// map from the lossy `SessionStarted`/`AgentChanged` broadcast, so under burst a
/// dropped lifecycle event would otherwise leave a restricted session unseen and
/// silently allow-all. The executor self-heals the leaf from `ToolExec.agent`
/// before resolving, so this floor fires only for a genuinely-unknown session (an
/// unresolved agent name, or an ancestor whose spawn was itself dropped). `arg`
/// carries the tool-specific argument (#173) so argument-scoped rules resolve.
fn permission_for(
    active: &HashMap<SessionId, AgentProfile>,
    session: &SessionId,
    tool: &str,
    arg: Option<&str>,
) -> Permission {
    active
        .get(session)
        .map(|p| p.permission.resolve(tool, arg))
        .unwrap_or(Permission::Deny)
}

/// The argument string an argument-scoped permission rule (#173) matches
/// against: the shell command for `bash`, the `command`+`args` line for `call`,
/// and the target path for `edit`/`write`/`read`. `None` for any other tool or
/// on malformed input — an argument-scoped rule then never matches, so
/// resolution falls through to the tool's name-only rules.
pub fn permission_arg(tool: &str, input: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(input).ok()?;
    match tool {
        "bash" => value.get("command")?.as_str().map(String::from),
        "call" => {
            let command = value.get("command")?.as_str()?;
            let mut line = command.to_string();
            if let Some(args) = value.get("args").and_then(|a| a.as_array()) {
                for a in args.iter().filter_map(|a| a.as_str()) {
                    line.push(' ');
                    line.push_str(a);
                }
            }
            Some(line)
        }
        "edit" | "write" | "read" => value.get("path")?.as_str().map(String::from),
        _ => None,
    }
}

/// The least-privileged of two permissions, ordered `Deny < Ask < Allow`.
pub(crate) fn min_permission(a: Permission, b: Permission) -> Permission {
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
    use entanglement_core::{AgentMode, PermissionProfile};

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
            can_spawn: None,
            spawnable_agents: None,
        }
    }

    #[test]
    fn spawn_refusal_layers_the_four_checks() {
        let reg = crate::agents::built_in_registry(); // build/plan (Primary), explore (Subagent)
        let build = reg.get("build").unwrap();
        let explore = reg.get("explore").unwrap();

        // Spawner may not spawn: an explore leaf is refused the capability.
        let refusal = spawn_refusal(Some(explore), "explore", &reg).expect("leaf refused");
        assert!(refusal.contains("cannot spawn"), "got: {refusal}");
        // Unknown spawner session (never started) is not gated.
        assert!(spawn_refusal(None, "explore", &reg).is_none());
        // A primary may spawn a spawnable-mode target.
        assert!(spawn_refusal(Some(build), "explore", &reg).is_none());
        // Unknown target name is refused.
        let r = spawn_refusal(Some(build), "ghost", &reg).expect("unknown refused");
        assert!(r.contains("unknown agent profile"), "got: {r}");
        // A primary target (`plan`) is not a valid spawn target.
        let r = spawn_refusal(Some(build), "plan", &reg).expect("primary target refused");
        assert!(r.contains("primary entry agent"), "got: {r}");
    }

    #[test]
    fn spawn_refusal_honors_the_allowlist() {
        let mut reg = crate::agents::built_in_registry();
        // A worker leaf (Subagent) plus a second spawnable target.
        reg.insert(masked_profile(
            "worker",
            AgentMode::Subagent,
            PermissionProfile::new(Permission::Allow),
            None,
            Vec::new(),
        ));
        // A spawner scoped to only `explore`.
        let mut scoped = profile(
            "scoped",
            AgentMode::Primary,
            PermissionProfile::new(Permission::Allow),
        );
        scoped.spawnable_agents = Some(vec!["explore".into()]);
        assert!(spawn_refusal(Some(&scoped), "explore", &reg).is_none());
        let r = spawn_refusal(Some(&scoped), "worker", &reg).expect("out-of-list refused");
        assert!(r.contains("not allowed to spawn"), "got: {r}");
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
            effective_permission(&active, &guard, &child, "edit", None),
            Permission::Ask
        );
        // `read` is Allow on both → stays Allow.
        assert_eq!(
            effective_permission(&active, &guard, &child, "read", None),
            Permission::Allow
        );
        // The parent (a root) is never loosened or clamped — its own profile stands.
        assert_eq!(
            effective_permission(&active, &guard, &parent, "edit", None),
            Permission::Ask
        );
    }

    #[test]
    fn resolution_source_names_own_vs_the_clamping_ancestor() {
        // grandparent `plan`: edit Ask. parent `build`: edit Allow. child `build`:
        // edit Allow. The chain's least-privileged edit rule comes from the
        // grandparent, two hops up.
        let plan = profile(
            "plan",
            AgentMode::Primary,
            PermissionProfile::new(Permission::Ask).with("read", Permission::Allow),
        );
        let allow_all = |name: &str| {
            profile(
                name,
                AgentMode::Primary,
                PermissionProfile::new(Permission::Allow),
            )
        };
        let gp = SessionId::new("gp");
        let parent = SessionId::new("parent");
        let child = SessionId::new("child");
        let mut active = HashMap::new();
        active.insert(gp.clone(), plan);
        active.insert(parent.clone(), allow_all("build"));
        active.insert(child.clone(), allow_all("build"));

        let mut guard = SpawnGuard::new();
        guard.record_start(gp.clone(), None);
        guard.record_start(parent.clone(), Some(gp.clone()));
        guard.record_start(child.clone(), Some(parent.clone()));

        // `edit`: own+parent Allow, grandparent Ask → clamped to Ask, sourced to gp.
        assert_eq!(
            resolve_with_source(&active, &guard, &child, "edit", None),
            (Permission::Ask, Some(gp.clone()))
        );
        // `read`: Allow the whole way → own profile stands, no ancestor source.
        assert_eq!(
            resolve_with_source(&active, &guard, &child, "read", None),
            (Permission::Allow, None)
        );
        // A root resolves to its own profile — never an ancestor.
        assert_eq!(
            resolve_with_source(&active, &guard, &gp, "edit", None),
            (Permission::Ask, None)
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
        // An unseen session masks everything — fail-closed (#156). Even a tool a
        // seen profile would advertise (`read`) is refused until the session's
        // profile is known, so a dropped `SessionStarted` cannot un-mask a
        // restricted session under overload.
        let other = SessionId::new("other");
        assert!(tool_masked(&active, &guard, &other, "edit"));
        assert!(tool_masked(&active, &guard, &other, "read"));
    }

    #[test]
    fn unseen_session_resolves_to_deny() {
        // #156: a session whose lifecycle events were dropped under broadcast
        // overload is unseen — its effective permission must be `Deny`
        // (fail-closed), not the pre-#156 allow-all default that inverted the
        // security posture. An allow-all *seen* session resolves normally.
        let build = profile(
            "build",
            AgentMode::Primary,
            PermissionProfile::new(Permission::Allow),
        );
        let seen = SessionId::new("seen");
        let mut active = HashMap::new();
        active.insert(seen.clone(), build);
        let guard = SpawnGuard::new();
        assert_eq!(
            effective_permission(&active, &guard, &seen, "edit", None),
            Permission::Allow
        );
        // An unseen session (never inserted) fails closed.
        assert_eq!(
            effective_permission(&active, &guard, &SessionId::new("ghost"), "edit", None),
            Permission::Deny
        );
    }

    #[test]
    fn unseen_ancestor_clamps_child_to_deny() {
        // #156: if a parent's `SessionStarted` was dropped, the parent is unseen.
        // The child's effective permission must clamp to `Deny` down the chain
        // rather than fall through to the child's own (allow-all) grade.
        let build = profile(
            "build",
            AgentMode::Primary,
            PermissionProfile::new(Permission::Allow),
        );
        let parent = SessionId::new("parent");
        let child = SessionId::new("child");
        let mut active = HashMap::new();
        // Only the child is seen; the parent's lifecycle event was lost.
        active.insert(child.clone(), build);
        let mut guard = SpawnGuard::new();
        guard.record_start(parent.clone(), None);
        guard.record_start(child.clone(), Some(parent.clone()));
        assert_eq!(
            effective_permission(&active, &guard, &child, "edit", None),
            Permission::Deny
        );
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
    fn clamp_to_base_is_a_least_privilege_ceiling() {
        // Allow-all base (the embedded default) never changes the agent's grade.
        let open = PermissionProfile::new(Permission::Allow);
        assert_eq!(
            clamp_to_base(Permission::Allow, &open, "bash", None),
            Permission::Allow
        );
        assert_eq!(
            clamp_to_base(Permission::Ask, &open, "bash", None),
            Permission::Ask
        );
        // A base `bash: ask` tightens an agent's Allow to Ask, but leaves a
        // stricter agent Deny untouched (least-privilege wins either way).
        let base = PermissionProfile::new(Permission::Allow).with("bash", Permission::Ask);
        assert_eq!(
            clamp_to_base(Permission::Allow, &base, "bash", None),
            Permission::Ask
        );
        assert_eq!(
            clamp_to_base(Permission::Deny, &base, "bash", None),
            Permission::Deny
        );
        // The base never loosens: base Allow over an agent Ask stays Ask.
        assert_eq!(
            clamp_to_base(Permission::Ask, &base, "read", None),
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
            effective_permission(&active, &guard, &root, "edit", None),
            Permission::Allow
        );
    }

    #[test]
    fn permission_arg_extracts_per_tool_shape() {
        // bash → the raw command.
        assert_eq!(
            permission_arg("bash", r#"{"command":"git status"}"#).as_deref(),
            Some("git status")
        );
        // call → command joined with its verbatim args.
        assert_eq!(
            permission_arg("call", r#"{"command":"git","args":["status","-s"]}"#).as_deref(),
            Some("git status -s")
        );
        // edit/write/read → the target path.
        assert_eq!(
            permission_arg(
                "edit",
                r#"{"path":"src/main.rs","oldString":"a","newString":"b"}"#
            )
            .as_deref(),
            Some("src/main.rs")
        );
        assert_eq!(
            permission_arg("write", r#"{"path":"README.md","content":"x"}"#).as_deref(),
            Some("README.md")
        );
        // Tools without a meaningful argument, and malformed input, yield None.
        assert_eq!(permission_arg("grep", r#"{"pattern":"foo"}"#), None);
        assert_eq!(permission_arg("bash", "not json"), None);
    }

    #[test]
    fn argument_scoped_rule_resolves_through_the_agent_chain() {
        // A root whose profile pre-approves `git *` but asks for every other bash.
        let build = profile(
            "build",
            AgentMode::Primary,
            PermissionProfile::new(Permission::Allow)
                .with("bash", Permission::Ask)
                .with("bash(git *)", Permission::Allow),
        );
        let root = SessionId::new("root");
        let mut active = HashMap::new();
        active.insert(root.clone(), build);
        let guard = SpawnGuard::new();
        assert_eq!(
            effective_permission(&active, &guard, &root, "bash", Some("git status")),
            Permission::Allow
        );
        assert_eq!(
            effective_permission(&active, &guard, &root, "bash", Some("rm -rf /")),
            Permission::Ask
        );
    }

    #[test]
    fn clamp_to_base_honors_argument_scoped_ceiling() {
        // A config ceiling that hard-denies `rm *` but leaves other bash alone.
        let base = PermissionProfile::new(Permission::Allow).with("bash(rm *)", Permission::Deny);
        assert_eq!(
            clamp_to_base(Permission::Allow, &base, "bash", Some("rm -rf /")),
            Permission::Deny
        );
        assert_eq!(
            clamp_to_base(Permission::Allow, &base, "bash", Some("git status")),
            Permission::Allow
        );
    }

    #[test]
    fn permission_chain_folds_own_then_ancestors() {
        let plan = profile(
            "plan",
            AgentMode::Primary,
            PermissionProfile::new(Permission::Ask),
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

        // Chain is [child's own, parent's] — the least-privileged across it is Ask.
        let chain = permission_chain(&active, &guard, &child);
        assert_eq!(chain.len(), 2);
        let perm = chain.iter().fold(Permission::Allow, |acc, p| {
            min_permission(acc, p.resolve("bash", None))
        });
        assert_eq!(perm, Permission::Ask);
    }
}
