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
//!   [`permission_arg`] extracts it. A `bash`/`call` call also carries its
//!   `workdir` (#425) so a `tool{pattern}` workdir-scoped rule matches too;
//!   [`permission_workdir`] extracts it.
//! - **Tool mask** — [`tool_masked`] (#116, ADR-0038): a tool omitted from a
//!   profile's allowlist (or listed in its denylist) does not *exist* for that
//!   session — a call is refused before permission is even resolved. Like the
//!   ceiling it clamps down the ancestor chain (a child never gains a tool an
//!   ancestor lacked). This is the enforcement half of the physical restriction
//!   whose advertisement half lives in core's `run_turn`.
//! - **Skill mask** — [`skill_masked`] (#400, ADR-0106): layered *after* the
//!   #116 agent mask above — a tool must survive both. Set when a `load_skill`
//!   call activates a skill carrying an `allowed_tools` list, cleared when the
//!   skill's scope ends (the turn's `Done`, or the session ending). Unlike the
//!   agent mask it does not clamp an ancestor chain: a skill's scope is one
//!   conversational turn in the session that loaded it, not an inheritable
//!   profile trait.
//!
//! All four live in the runtime tool executor's single-threaded loop, folded
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
/// ancestor. `arg` is the tool-specific argument (command/path, #173) and
/// `workdir` the `bash`/`call` working directory (#425) so argument-/workdir-
/// scoped rules resolve against the actual call; pass `None` for a name-only
/// decision. A root has no ancestors, so this reduces to its own profile —
/// single-session behavior is unchanged.
pub fn effective_permission(
    active: &HashMap<SessionId, AgentProfile>,
    guard: &SpawnGuard,
    session: &SessionId,
    tool: &str,
    arg: Option<&str>,
    workdir: Option<&str>,
) -> Permission {
    let (perm, source) = resolve_with_source(active, guard, session, tool, arg, workdir);
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
    workdir: Option<&str>,
) -> (Permission, Option<SessionId>) {
    let mut perm = permission_for(active, session, tool, arg, workdir);
    let mut source: Option<SessionId> = None;
    let mut current = session.clone();
    // Guard against a malformed cycle in the parent links (mirrors SpawnGuard).
    let mut visited = HashSet::new();
    while visited.insert(current.clone()) {
        match guard.parent_of(&current) {
            Some(parent) => {
                let clamped =
                    min_permission(perm, permission_for(active, &parent, tool, arg, workdir));
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

/// A skill's tool mask while "active" in a session (#400, ADR-0106): the
/// runtime's tool-execution-record field ADR-0037 deferred pending this
/// enforcement. Set on a resolved `load_skill` call,
/// cleared when the skill's scope ends. `allowed_tools: None` means the loaded
/// skill declared no mask — it inherits whatever the #116 agent mask already
/// allows, same as an absent [`AgentProfile::tools`] allowlist.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActiveSkill {
    pub skill_id: String,
    pub allowed_tools: Option<Vec<String>>,
}

impl ActiveSkill {
    fn allows(&self, tool: &str) -> bool {
        match &self.allowed_tools {
            Some(list) => list.iter().any(|t| t == tool),
            None => true,
        }
    }
}

/// Whether `tool` is masked out by `session`'s active skill (#400, ADR-0106),
/// layered *after* the #116 agent mask ([`tool_masked`]) — a tool must survive
/// both to run. `None` ⇒ not masked (no active skill, or its `allowed_tools` is
/// unrestricted or includes `tool`); `Some(skill_id)` names the skill that
/// denied it, for the refusal message. Scoped to the exact session `load_skill`
/// ran in — unlike [`tool_masked`], it does not clamp down an ancestor chain: a
/// skill's scope is a conversational turn in one session, not an inheritable
/// profile trait a spawned child should pick up.
pub fn skill_masked(
    active_skill: &HashMap<SessionId, ActiveSkill>,
    session: &SessionId,
    tool: &str,
) -> Option<String> {
    let skill = active_skill.get(session)?;
    if skill.allows(tool) {
        None
    } else {
        Some(skill.skill_id.clone())
    }
}

/// The ordered permission profiles the effective grade folds over (#173): the
/// session's own profile followed by each ancestor, walking `guard`'s parent
/// links. The rhai binding policy captures this once per run and resolves each
/// binding call against it with the call's argument, matching
/// [`effective_permission`]'s least-privilege clamp while letting argument-scoped
/// rules see the actual input. An unseen session contributes nothing.
// Only called by `crate::script` (feature-gated) outside this module's own
// unit test below, which is why a lean, rhai-less build sees it as dead.
#[cfg_attr(not(feature = "rhai"), allow(dead_code))]
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
/// tool-specific argument (#173) and `workdir` the `bash`/`call` working
/// directory (#425) so an argument-/workdir-scoped ceiling rule like
/// `bash(rm *): deny` or `bash{/etc/*}: deny` resolves against the actual call.
/// The embedded default is allow-all, so an untouched config is a no-op. This is
/// a pure ceiling (it only tightens); the orthogonal "always allow" grants (#174,
/// [`crate::grants`]) that *raise* an `Ask` are applied by the executor *after*
/// this clamp, so a `Deny` here can never be re-opened by a stale grant.
pub fn clamp_to_base(
    perm: Permission,
    base: &PermissionProfile,
    tool: &str,
    arg: Option<&str>,
    workdir: Option<&str>,
) -> Permission {
    min_permission(perm, base.resolve_scoped(tool, arg, workdir))
}

/// A session's own permission for a `tool` call; an unseen session defaults to
/// `Deny` — **fail-closed** (#156). The executor folds its per-session profile
/// map from the lossy `SessionStarted`/`AgentChanged` broadcast, so under burst a
/// dropped lifecycle event would otherwise leave a restricted session unseen and
/// silently allow-all. The executor self-heals the leaf from `ToolExec.agent`
/// before resolving, so this floor fires only for a genuinely-unknown session (an
/// unresolved agent name, or an ancestor whose spawn was itself dropped). `arg`
/// carries the tool-specific argument (#173) and `workdir` the `bash`/`call`
/// working directory (#425) so argument-/workdir-scoped rules resolve.
pub(crate) fn permission_for(
    active: &HashMap<SessionId, AgentProfile>,
    session: &SessionId,
    tool: &str,
    arg: Option<&str>,
    workdir: Option<&str>,
) -> Permission {
    active
        .get(session)
        .map(|p| p.permission.resolve_scoped(tool, arg, workdir))
        .unwrap_or(Permission::Deny)
}

/// The session + its ancestor chain (nearest first), walking `guard`'s parent
/// links with a cycle guard. The runtime tool executor resolves the effective
/// permission by taking the least-privileged [`PermissionResolver`][crate::policy::PermissionResolver]
/// grade across exactly these sessions — the sub-agent privilege ceiling
/// (ADR-0024) applied *on top of* whatever grade the resolver returns, so a
/// pluggable tenant rule can never widen a child beyond its parent. The set
/// matches the sessions [`effective_permission`] folds over, so the default
/// profile resolver stays byte-identical.
pub(crate) fn ancestor_chain(guard: &SpawnGuard, session: &SessionId) -> Vec<SessionId> {
    let mut chain = vec![session.clone()];
    let mut visited = HashSet::new();
    visited.insert(session.clone());
    let mut current = session.clone();
    loop {
        match guard.parent_of(&current) {
            Some(parent) if visited.insert(parent.clone()) => {
                chain.push(parent.clone());
                current = parent;
            }
            _ => break,
        }
    }
    chain
}

/// The argument string an argument-scoped permission rule (#173) matches
/// against: the shell command for `bash`, the `command`+`args` line for `call`,
/// the target path for `edit`/`write`/`read`/`apply_patch` (#455), the search
/// pattern (itself a path glob) for `glob`, and the optional file filter for
/// `grep` (#417 — a path, distinct from `grep`'s `pattern` which is a regex,
/// not a path). `None` for any other tool, a `grep` call with no `path`
/// filter, or on malformed input — an argument-scoped rule then never
/// matches, so resolution falls through to the tool's name-only rules.
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
        "edit" | "write" | "read" | "apply_patch" => value.get("path")?.as_str().map(String::from),
        "glob" => value.get("pattern")?.as_str().map(String::from),
        "grep" => value.get("path").and_then(|p| p.as_str()).map(String::from),
        _ => None,
    }
}

/// The `workdir` a `bash`/`call` invocation would run in, for a
/// workdir-scoped permission rule (#425, `tool{pattern}`,
/// [`entanglement_core::PermissionProfile::resolve_scoped`]) — distinct from
/// [`permission_arg`] (which yields the *command* line for these two tools).
/// `None` for any other tool, an absent `workdir` (the tool then defaults to
/// root), or on malformed input — a workdir-scoped rule then never matches,
/// falling through to the tool's other rules.
pub fn permission_workdir(tool: &str, input: &str) -> Option<String> {
    match tool {
        "bash" | "call" => {
            let value: serde_json::Value = serde_json::from_str(input).ok()?;
            value.get("workdir")?.as_str().map(String::from)
        }
        _ => None,
    }
}

/// The **filesystem path** a call would touch, for the escape-root gate
/// (ADR-0109) — distinct from [`permission_arg`] (which yields the *command* for
/// `bash`/`call`). It's the `path` for `read`/`edit`/`write`/`apply_patch` and
/// the `workdir` for `bash`/`call` (absent → the tool defaults to root, never
/// an escape), the same value [`permission_workdir`] extracts for permission
/// scoping. `None` for any other tool or on malformed input, so those never
/// trip the gate.
pub fn escape_root_target(tool: &str, input: &str) -> Option<String> {
    match tool {
        "read" | "edit" | "write" | "apply_patch" => {
            let value: serde_json::Value = serde_json::from_str(input).ok()?;
            value.get("path")?.as_str().map(String::from)
        }
        "bash" | "call" => permission_workdir(tool, input),
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
            provider: None,
            permission,
            tools: tools.map(|v| v.into_iter().map(String::from).collect()),
            disallowed_tools: disallowed.into_iter().map(String::from).collect(),
            can_spawn: None,
            spawnable_agents: None,
            sandbox: None,
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
            effective_permission(&active, &guard, &child, "edit", None, None),
            Permission::Ask
        );
        // `read` is Allow on both → stays Allow.
        assert_eq!(
            effective_permission(&active, &guard, &child, "read", None, None),
            Permission::Allow
        );
        // The parent (a root) is never loosened or clamped — its own profile stands.
        assert_eq!(
            effective_permission(&active, &guard, &parent, "edit", None, None),
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
            resolve_with_source(&active, &guard, &child, "edit", None, None),
            (Permission::Ask, Some(gp.clone()))
        );
        // `read`: Allow the whole way → own profile stands, no ancestor source.
        assert_eq!(
            resolve_with_source(&active, &guard, &child, "read", None, None),
            (Permission::Allow, None)
        );
        // A root resolves to its own profile — never an ancestor.
        assert_eq!(
            resolve_with_source(&active, &guard, &gp, "edit", None, None),
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
            effective_permission(&active, &guard, &seen, "edit", None, None),
            Permission::Allow
        );
        // An unseen session (never inserted) fails closed.
        assert_eq!(
            effective_permission(
                &active,
                &guard,
                &SessionId::new("ghost"),
                "edit",
                None,
                None
            ),
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
            effective_permission(&active, &guard, &child, "edit", None, None),
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
            clamp_to_base(Permission::Allow, &open, "bash", None, None),
            Permission::Allow
        );
        assert_eq!(
            clamp_to_base(Permission::Ask, &open, "bash", None, None),
            Permission::Ask
        );
        // A base `bash: ask` tightens an agent's Allow to Ask, but leaves a
        // stricter agent Deny untouched (least-privilege wins either way).
        let base = PermissionProfile::new(Permission::Allow).with("bash", Permission::Ask);
        assert_eq!(
            clamp_to_base(Permission::Allow, &base, "bash", None, None),
            Permission::Ask
        );
        assert_eq!(
            clamp_to_base(Permission::Deny, &base, "bash", None, None),
            Permission::Deny
        );
        // The base never loosens: base Allow over an agent Ask stays Ask.
        assert_eq!(
            clamp_to_base(Permission::Ask, &base, "read", None, None),
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
            effective_permission(&active, &guard, &root, "edit", None, None),
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
        // edit/write/read/apply_patch → the target path.
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
        assert_eq!(
            permission_arg(
                "apply_patch",
                r#"{"path":"src/lib.rs","patch":"@@ -1 +1 @@"}"#
            )
            .as_deref(),
            Some("src/lib.rs")
        );
        // glob → the pattern itself, since a glob pattern is a path.
        assert_eq!(
            permission_arg("glob", r#"{"pattern":"src/*.rs"}"#).as_deref(),
            Some("src/*.rs")
        );
        // grep → the optional file filter, which is a path; the regex `pattern`
        // is never returned, since it isn't one.
        assert_eq!(
            permission_arg("grep", r#"{"pattern":"foo","path":"src/*"}"#).as_deref(),
            Some("src/*")
        );
        // grep without a `path` filter yields None — resolution falls through
        // to grep's name-only rules.
        assert_eq!(permission_arg("grep", r#"{"pattern":"foo"}"#), None);
        // Tools without a meaningful argument, and malformed input, yield None.
        assert_eq!(permission_arg("bash", "not json"), None);
    }

    #[test]
    fn permission_workdir_extracts_bash_and_call_only() {
        assert_eq!(
            permission_workdir("bash", r#"{"command":"ls","workdir":"/tmp"}"#).as_deref(),
            Some("/tmp")
        );
        assert_eq!(
            permission_workdir("call", r#"{"command":"git","workdir":"/tmp/repo"}"#).as_deref(),
            Some("/tmp/repo")
        );
        // No `workdir` field, a tool with no workdir concept, and malformed
        // input all yield None.
        assert_eq!(permission_workdir("bash", r#"{"command":"ls"}"#), None);
        assert_eq!(
            permission_workdir("read", r#"{"path":"x","workdir":"/tmp"}"#),
            None
        );
        assert_eq!(permission_workdir("bash", "not json"), None);
    }

    #[test]
    fn workdir_scoped_rule_resolves_through_the_agent_chain() {
        // A root whose profile pre-approves anything run under `/tmp` but asks
        // for `bash` elsewhere (#425).
        let build = profile(
            "build",
            AgentMode::Primary,
            PermissionProfile::new(Permission::Allow)
                .with("bash", Permission::Ask)
                .with("bash{/tmp/*}", Permission::Allow),
        );
        let root = SessionId::new("root");
        let mut active = HashMap::new();
        active.insert(root.clone(), build);
        let guard = SpawnGuard::new();
        assert_eq!(
            permission_for(&active, &root, "bash", None, Some("/tmp/scratch")),
            Permission::Allow
        );
        assert_eq!(
            permission_for(&active, &root, "bash", None, Some("/home/x")),
            Permission::Ask
        );
        // `effective_permission`/`clamp_to_base` see the same `workdir` slot.
        assert_eq!(
            effective_permission(&active, &guard, &root, "bash", None, Some("/tmp/scratch")),
            Permission::Allow
        );
        let deny_etc =
            PermissionProfile::new(Permission::Allow).with("bash{/etc/*}", Permission::Deny);
        assert_eq!(
            clamp_to_base(
                Permission::Allow,
                &deny_etc,
                "bash",
                None,
                Some("/etc/cron.d")
            ),
            Permission::Deny
        );
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
            effective_permission(&active, &guard, &root, "bash", Some("git status"), None),
            Permission::Allow
        );
        assert_eq!(
            effective_permission(&active, &guard, &root, "bash", Some("rm -rf /"), None),
            Permission::Ask
        );
    }

    #[test]
    fn argument_scoped_rule_resolves_for_search_tools() {
        // #417: grep/glob now yield a path-shaped arg, so a `read`-style
        // arg-scoped rule can restrict them to a subtree.
        let build = profile(
            "build",
            AgentMode::Primary,
            PermissionProfile::new(Permission::Ask)
                .with("grep(src/*)", Permission::Allow)
                .with("glob(src/*)", Permission::Allow),
        );
        let root = SessionId::new("root");
        let mut active = HashMap::new();
        active.insert(root.clone(), build);
        let guard = SpawnGuard::new();
        assert_eq!(
            effective_permission(
                &active,
                &guard,
                &root,
                "grep",
                permission_arg("grep", r#"{"pattern":"foo","path":"src/*"}"#).as_deref(),
                None
            ),
            Permission::Allow
        );
        assert_eq!(
            effective_permission(
                &active,
                &guard,
                &root,
                "glob",
                permission_arg("glob", r#"{"pattern":"src/*"}"#).as_deref(),
                None
            ),
            Permission::Allow
        );
        // Outside the scoped path, or with no file filter at all, falls back
        // to the tool's name-only rule (`Ask` here).
        assert_eq!(
            effective_permission(
                &active,
                &guard,
                &root,
                "grep",
                permission_arg("grep", r#"{"pattern":"foo","path":"docs/*"}"#).as_deref(),
                None
            ),
            Permission::Ask
        );
        assert_eq!(
            effective_permission(
                &active,
                &guard,
                &root,
                "grep",
                permission_arg("grep", r#"{"pattern":"foo"}"#).as_deref(),
                None
            ),
            Permission::Ask
        );
    }

    #[test]
    fn clamp_to_base_honors_argument_scoped_ceiling() {
        // A config ceiling that hard-denies `rm *` but leaves other bash alone.
        let base = PermissionProfile::new(Permission::Allow).with("bash(rm *)", Permission::Deny);
        assert_eq!(
            clamp_to_base(Permission::Allow, &base, "bash", Some("rm -rf /"), None),
            Permission::Deny
        );
        assert_eq!(
            clamp_to_base(Permission::Allow, &base, "bash", Some("git status"), None),
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

    #[test]
    fn skill_masked_refuses_a_tool_outside_the_active_skill() {
        let s = SessionId::new("s");
        let mut active_skill = HashMap::new();
        active_skill.insert(
            s.clone(),
            ActiveSkill {
                skill_id: "commit".into(),
                allowed_tools: Some(vec!["bash".into(), "read".into()]),
            },
        );
        assert_eq!(skill_masked(&active_skill, &s, "bash"), None);
        assert_eq!(
            skill_masked(&active_skill, &s, "edit"),
            Some("commit".to_string())
        );
        // No active skill for a session ⇒ never masked.
        let other = SessionId::new("other");
        assert_eq!(skill_masked(&active_skill, &other, "edit"), None);
    }

    #[test]
    fn skill_masked_is_unrestricted_when_allowed_tools_is_none() {
        let s = SessionId::new("s");
        let mut active_skill = HashMap::new();
        active_skill.insert(
            s.clone(),
            ActiveSkill {
                skill_id: "no-mask".into(),
                allowed_tools: None,
            },
        );
        assert_eq!(skill_masked(&active_skill, &s, "edit"), None);
        assert_eq!(skill_masked(&active_skill, &s, "bash"), None);
    }
}
