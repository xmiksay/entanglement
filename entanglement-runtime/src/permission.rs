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
//! - **Privilege ceiling** — [`effective_permission`]/[`permission_chain`]:
//!   a child sub-agent is never more privileged than its ancestors. Since
//!   ADR-0207 (permission modes), the main dispatch ladder grades every call
//!   from the session's permission **mode** (`crate::mode`/
//!   `crate::policy::ProfileResolver`) instead — these `AgentProfile`-chain
//!   functions now serve only the `rhai` binding policy
//!   ([`crate::script::BindingPolicy`]), which still resolves its bindings
//!   against the profile chain. Resolution takes the call's tool-specific
//!   argument (command/path, #173) so an argument-scoped rule matches the
//!   actual input; [`permission_arg`] extracts it. A `bash`/`call` call also
//!   carries its `workdir` (#425) so a `tool{pattern}` workdir-scoped rule
//!   matches too; [`permission_workdir`] extracts it.
//!
//! The **tool mask** (#116, ADR-0038: `tools`/`disallowed_tools` making a
//! tool not *exist* for a session) is retired (ADR-0207 §8, "the mask
//! machinery is deleted"): `tool_masked`/`tool_mask_source` are gone.
//! `AgentProfile` still carries the `tools`/`disallowed_tools` fields for now
//! (ADR-0207 stage 4b removes them), but nothing reads them any more.
//!
//! Both live in the runtime tool executor's single-threaded loop, folded
//! from the same lifecycle events as permission dispatch — zero core surface.

use std::collections::{HashMap, HashSet};

use entanglement_core::{
    AgentProfile, Permission, PermissionProfile, ProfileRegistry, SessionId, ToolOverlayEntry,
};

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
///
/// A **sponsored child** (ADR-0138) is a permission root despite having a
/// parent link: its authorization is user plan approval, not the ancestor
/// chain, so the walk stops at the child and its own profile stands. This is
/// what lets a `build` child of a read-only `plan` session run write tools.
fn resolve_with_source(
    active: &HashMap<SessionId, AgentProfile>,
    guard: &SpawnGuard,
    session: &SessionId,
    tool: &str,
    arg: Option<&str>,
    workdir: Option<&str>,
) -> (Permission, Option<SessionId>) {
    let mut perm = permission_for(active, session, tool, arg, workdir);
    // A sponsored child (ADR-0138) is a permission root: no ancestor walk, no
    // clamp. Authorization came from user plan approval, not inheritance.
    if guard.is_sponsored(session) {
        return (perm, None);
    }
    let mut source: Option<SessionId> = None;
    let mut current = session.clone();
    // Guard against a malformed cycle in the parent links (mirrors SpawnGuard).
    let mut visited = HashSet::new();
    while visited.insert(current.clone()) {
        match guard.parent_of(&current) {
            Some(parent) => {
                // A sponsored ancestor is itself a permission root — its own
                // ancestors don't clamp this sub-tree either. Stop the walk at
                // it, the same way a plain root's `None` parent does.
                if guard.is_sponsored(&parent) {
                    let clamped =
                        min_permission(perm, permission_for(active, &parent, tool, arg, workdir));
                    if clamped != perm {
                        source = Some(parent.clone());
                    }
                    perm = clamped;
                    break;
                }
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
    // A sponsored child (ADR-0138) is a permission root — its own profile
    // stands, no ancestor walk.
    if guard.is_sponsored(&current) {
        if let Some(profile) = active.get(&current) {
            chain.push(profile.permission.clone());
        }
        return chain;
    }
    // Guard against a malformed cycle in the parent links (mirrors SpawnGuard).
    let mut visited = HashSet::new();
    while visited.insert(current.clone()) {
        if let Some(profile) = active.get(&current) {
            chain.push(profile.permission.clone());
        }
        match guard.parent_of(&current) {
            Some(parent) => {
                // A sponsored ancestor (ADR-0138) is a permission root: include
                // its own profile (it clamps this sub-tree) but stop the walk
                // above it.
                if guard.is_sponsored(&parent) {
                    if let Some(profile) = active.get(&parent) {
                        chain.push(profile.permission.clone());
                    }
                    break;
                }
                current = parent;
            }
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
    min_permission(
        perm,
        crate::permission_bash::resolve_scoped_bash_aware(base, tool, arg, workdir),
    )
}

/// The [`PermissionProfile`] an **enable** [`ToolOverlayEntry`] materializes
/// into for `tool` (ADR-0163, #611 — folds live bash enablement's
/// `BashGrade::Allow { pattern }` into the generic overlay): `allow: false`
/// is a flat `Ask`; `allow: true` with no `arg_pattern` is a flat `Allow`;
/// `allow: true` with `arg_pattern: Some(p)` stays `Ask` by default and adds
/// an argument-scoped `tool(p): allow` rule (the existing `tool(pattern)`
/// syntax, #173/ADR-0114) so only matching commands are pre-approved. `tool`
/// is the *concrete* tool name being dispatched, not the entry's own
/// name-glob `pattern` — an entry like `mcp__chessbase__*` fans this out over
/// every tool it matches. `arg_pattern` is ignored when `allow` is `false`:
/// the entry's own grammar (`--allow [<pattern>]`) never produces that
/// combination, and narrowing an `Ask` down from itself has no meaning. A
/// deny entry is never passed here — [`overlay_grade_entry`]'s
/// `ToolOverlayEntry::find` lookup is enable-only, so a deny entry never
/// reaches this function. This override deliberately replaces the mode
/// resolution outright rather than merely widening it — an explicit
/// `/enable tool X --allow` is the user's own voice and overrides even a
/// mode `deny` for that session (ADR-0207 §8); the model can't reach this
/// path, since an enable entry is trusted-frame-only (ADR-0177).
pub fn overlay_entry_grade(tool: &str, entry: &ToolOverlayEntry) -> PermissionProfile {
    match (entry.allow, entry.arg_pattern.as_deref()) {
        (true, Some(pattern)) => PermissionProfile::new(Permission::Ask)
            .with(format!("{tool}({pattern})"), Permission::Allow),
        (true, None) => PermissionProfile::new(Permission::Allow),
        (false, _) => PermissionProfile::new(Permission::Ask),
    }
}

/// The overlay entry that decides `tool`'s **grade** for a call resolved
/// through `chain` (nearest session first, the same ordering
/// [`ancestor_chain`] produces, #628 closing the ADR-0149 "child sessions"
/// deferral). The first link, walking outward from the call's own session,
/// whose overlay carries an enable entry for `tool` wins: a session's own
/// overlay beats an ancestor's, and a parent's overlay grade reaches its
/// whole spawn sub-tree. `None` ⇒ no link in the chain has an opinion — the
/// ordinary mode resolution stands. A deny entry never reaches this lookup:
/// `ToolOverlayEntry::find` is enable-only, so a session's own `/disable
/// tool` never resolves through this path.
pub fn overlay_grade_entry(
    overlays: &HashMap<SessionId, Vec<ToolOverlayEntry>>,
    chain: &[SessionId],
    tool: &str,
) -> Option<ToolOverlayEntry> {
    chain.iter().find_map(|session| {
        overlays
            .get(session)
            .and_then(|entries| ToolOverlayEntry::find(entries, tool))
            .cloned()
    })
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
        .map(|p| {
            crate::permission_bash::resolve_scoped_bash_aware(&p.permission, tool, arg, workdir)
        })
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
///
/// A **sponsored child** (ADR-0138) is a permission root: the chain stops at
/// it (no ancestors), and stops at any sponsored ancestor mid-walk — the
/// sub-tree rooted at a sponsored session is authorized by user plan approval,
/// not by the chain above it.
pub(crate) fn ancestor_chain(guard: &SpawnGuard, session: &SessionId) -> Vec<SessionId> {
    let mut chain = vec![session.clone()];
    // A sponsored session is a permission root — no ancestors to clamp it.
    if guard.is_sponsored(session) {
        return chain;
    }
    let mut visited = HashSet::new();
    visited.insert(session.clone());
    let mut current = session.clone();
    loop {
        match guard.parent_of(&current) {
            Some(parent) if visited.insert(parent.clone()) => {
                chain.push(parent.clone());
                // A sponsored ancestor is itself a permission root (ADR-0138):
                // its own perms clamp this sub-tree, but the chain above it
                // does not. Stop the walk at it.
                if guard.is_sponsored(&parent) {
                    break;
                }
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
            // Space-joining loses the argv split: `{command:"rm",args:["x"]}`
            // and `{command:"rm x"}` grade (and grant-match) identically.
            // Accepted: `call` execs `command` as the program with no shell,
            // so a colliding decomposition can only fail to exec — it can
            // never run a different program than the one graded.
            if let Some(args) = value.get("args").and_then(|a| a.as_array()) {
                for a in args.iter().filter_map(|a| a.as_str()) {
                    line.push(' ');
                    line.push_str(a);
                }
            }
            Some(line)
        }
        "edit" | "write" | "read" | "apply_patch" => value.get("path")?.as_str().map(String::from),
        "glob" => {
            let pattern = value.get("pattern")?.as_str()?;
            // ADR-0150: the optional `path` base dir joins onto the pattern
            // exactly as the tool resolves it, so an argument-scoped rule or
            // grant like `glob(src/**)` sees the same string the walk uses.
            let base = value.get("path").and_then(|p| p.as_str());
            Some(crate::host::glob::joined_pattern(base, pattern))
        }
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
        let reg = crate::agents::built_in_registry().expect("built-in agents must parse"); // build/plan (Primary), explore (Subagent)
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
        let mut reg = crate::agents::built_in_registry().expect("built-in agents must parse");
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
    fn plan_child_explore_reaches_ask_on_a_real_call_dispatch() {
        // #597 end-to-end: the mask fix alone isn't enough — the ancestor
        // *permission* ceiling (`effective_permission`) also has to clear
        // `call` for an actual dispatch, not just the mask. `plan`'s own
        // coarse (no-arg) `call` grade is `Deny` (ADR-0114's `MULTI_GROUP`
        // floor, tightened by `write: deny`), but a real invocation always
        // carries its command as the argument, and `plan.md`'s `call(*): ask`
        // arg-scoped rule refines exactly that case — reproducing the
        // issue's `gh issue view 594` via `call`.
        let reg = crate::agents::built_in_registry().expect("built-in agents must parse");
        let plan = reg.get("plan").unwrap().clone();
        let explore = reg.get("explore").unwrap().clone();

        let p = SessionId::new("plan");
        let c = SessionId::new("explore-child");
        let mut active = HashMap::new();
        active.insert(p.clone(), plan);
        active.insert(c.clone(), explore);
        let mut guard = SpawnGuard::new();
        guard.record_start(p.clone(), None);
        guard.record_start(c.clone(), Some(p.clone()));

        assert_eq!(
            effective_permission(&active, &guard, &c, "call", Some("gh issue view 594"), None),
            Permission::Ask,
            "a real `call` dispatch under an explore child of plan must reach Ask, not Deny"
        );
    }

    /// #628: `overlay_grade_entry` walks the same ancestor chain the mask
    /// already does — a parent's overlay grade now reaches a child that has
    /// none of its own, and the child's own overlay (nearer in the chain)
    /// still wins when both have an opinion.
    #[test]
    fn overlay_grade_entry_reaches_down_the_ancestor_chain() {
        let parent = SessionId::new("parent");
        let child = SessionId::new("child");
        let mut guard = SpawnGuard::new();
        guard.record_start(parent.clone(), None);
        guard.record_start(child.clone(), Some(parent.clone()));
        let chain_from_child = ancestor_chain(&guard, &child);

        let mut overlays = HashMap::new();
        overlays.insert(
            parent.clone(),
            vec![ToolOverlayEntry {
                pattern: "mcp__docs__*".into(),
                allow: true,
                deny: false,
                arg_pattern: None,
            }],
        );

        // No overlay of its own — the child inherits the parent's entry.
        assert_eq!(
            overlay_grade_entry(&overlays, &chain_from_child, "mcp__docs__search"),
            Some(overlays[&parent][0].clone())
        );
        // No link in the chain has an opinion on an unrelated tool.
        assert!(overlay_grade_entry(&overlays, &chain_from_child, "bash").is_none());

        // The child's own overlay entry, when present, wins over the
        // parent's (nearest link first).
        overlays.insert(child.clone(), vec![ToolOverlayEntry::ask("mcp__docs__*")]);
        let own = overlay_grade_entry(&overlays, &chain_from_child, "mcp__docs__search").unwrap();
        assert!(!own.allow);

        // From the parent's own chain, only the parent's entry is in scope.
        let chain_from_parent = ancestor_chain(&guard, &parent);
        assert_eq!(
            overlay_grade_entry(&overlays, &chain_from_parent, "mcp__docs__search"),
            Some(overlays[&parent][0].clone())
        );
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
    fn sponsored_child_resolves_own_perms_ignoring_readonly_ancestor() {
        // ADR-0138: a sponsored `build` child of a read-only `plan` session
        // runs with its own profile's permissions — the ancestor clamp does
        // not apply, since authorization is user plan approval, not
        // inheritance. `edit` is Allow on the child and Ask on the parent; a
        // plain child would clamp to Ask, a sponsored child stays Allow.
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
        let parent = SessionId::new("plan");
        let child = SessionId::new("build");
        let mut active = HashMap::new();
        active.insert(parent.clone(), plan);
        active.insert(child.clone(), build);

        let mut guard = SpawnGuard::new();
        guard.record_start(parent.clone(), None);
        guard.record_sponsored_start(child.clone(), parent.clone());

        // Sponsored: own profile stands, no ancestor clamp.
        assert_eq!(
            effective_permission(&active, &guard, &child, "edit", None, None),
            Permission::Allow
        );
        assert_eq!(
            resolve_with_source(&active, &guard, &child, "edit", None, None),
            (Permission::Allow, None)
        );
        // `read` is Allow on both → stays Allow.
        assert_eq!(
            effective_permission(&active, &guard, &child, "read", None, None),
            Permission::Allow
        );
        // The plan parent itself is unchanged (a root).
        assert_eq!(
            effective_permission(&active, &guard, &parent, "edit", None, None),
            Permission::Ask
        );
    }

    #[test]
    fn non_sponsored_child_still_clamped_regression() {
        // ADR-0024 regression: a plain (non-sponsored) child under a read-only
        // parent is still clamped. Sponsorship is the *only* exemption.
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
        let parent = SessionId::new("plan");
        let child = SessionId::new("build");
        let mut active = HashMap::new();
        active.insert(parent.clone(), plan);
        active.insert(child.clone(), build);

        let mut guard = SpawnGuard::new();
        guard.record_start(parent.clone(), None);
        guard.record_start(child.clone(), Some(parent.clone()));

        // Plain child: ancestor clamp applies → edit clamps to Ask.
        assert_eq!(
            effective_permission(&active, &guard, &child, "edit", None, None),
            Permission::Ask
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

    /// ADR-0150: glob's optional `path` base dir joins onto the pattern for
    /// grading, matching what the tool actually walks.
    #[test]
    fn glob_path_plus_pattern_grades_joined() {
        assert_eq!(
            permission_arg("glob", r#"{"pattern":"**/*.rs","path":"src"}"#).as_deref(),
            Some("src/**/*.rs")
        );
    }

    /// ADR-0150 regression: grep's graded arg stays the raw `path` even when
    /// it names a directory — the walk's `dir/**/*` auto-expansion happens
    /// after grading and must never leak into it.
    #[test]
    fn grep_graded_arg_is_raw_path_even_for_directory() {
        assert_eq!(
            permission_arg("grep", r#"{"pattern":"foo","path":"src/tui"}"#).as_deref(),
            Some("src/tui")
        );
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

    /// ADR-0197: a compound command built entirely of allowed verbs grades
    /// `Allow` through the ancestor-chain fold, without needing the whole raw
    /// string to match a single rule.
    #[test]
    fn compound_command_allowed_when_every_segment_matches() {
        let build = profile(
            "build",
            AgentMode::Primary,
            PermissionProfile::new(Permission::Ask)
                .with("bash(find *)", Permission::Allow)
                .with("bash(grep *)", Permission::Allow)
                .with("bash(wc *)", Permission::Allow),
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
                "bash",
                Some("find . | grep x | wc -l"),
                None
            ),
            Permission::Allow
        );
    }

    /// ADR-0197 regression: the over-match hole a trailing `*` used to open —
    /// `bash(find *): allow` must never authorize an appended `rm -rf /` via
    /// `&&`.
    #[test]
    fn compound_command_over_match_regression_asks() {
        let build = profile(
            "build",
            AgentMode::Primary,
            PermissionProfile::new(Permission::Ask).with("bash(find *)", Permission::Allow),
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
                "bash",
                Some("find . && rm -rf /tmp/x"),
                None
            ),
            Permission::Ask
        );
    }

    /// ADR-0197: a compound whose leading verb has no rule at all (not just an
    /// unmatched allow) still resolves through the segment fold to `Ask`.
    #[test]
    fn compound_command_with_unmatched_leading_verb_asks() {
        let build = profile(
            "build",
            AgentMode::Primary,
            PermissionProfile::new(Permission::Ask).with("bash(find *)", Permission::Allow),
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
                "bash",
                Some("git status && find ."),
                None
            ),
            Permission::Ask
        );
    }

    /// ADR-0197: a deny rule matching only a *later* segment still denies the
    /// whole compound — the deny doesn't need to be the leading verb.
    #[test]
    fn compound_command_deny_on_trailing_segment_denies_the_whole_command() {
        let build = profile(
            "build",
            AgentMode::Primary,
            PermissionProfile::new(Permission::Ask)
                .with("bash(find *)", Permission::Allow)
                .with("bash(rm *)", Permission::Deny),
        );
        let root = SessionId::new("root");
        let mut active = HashMap::new();
        active.insert(root.clone(), build);
        let guard = SpawnGuard::new();
        assert_eq!(
            effective_permission(&active, &guard, &root, "bash", Some("find . && rm x"), None),
            Permission::Deny
        );
    }

    /// ADR-0197: output redirection is opaque to the splitter, so an
    /// arg-scoped Allow must not fire — `find . > out.txt` still asks despite
    /// `bash(find *): allow`.
    #[test]
    fn compound_command_redirect_still_asks_despite_curated_allow() {
        let build = profile(
            "build",
            AgentMode::Primary,
            PermissionProfile::new(Permission::Ask).with("bash(find *)", Permission::Allow),
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
                "bash",
                Some("find . > out.txt"),
                None
            ),
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

    /// ADR-0197: the ceiling's own per-segment fold still applies *after* the
    /// already-folded ancestor grade — a `bash(rm *): deny` ceiling denies a
    /// compound even when the incoming `perm` (from the agent chain) is
    /// `Allow`.
    #[test]
    fn clamp_to_base_folds_compound_command_per_segment() {
        let base = PermissionProfile::new(Permission::Allow).with("bash(rm *)", Permission::Deny);
        assert_eq!(
            clamp_to_base(
                Permission::Allow,
                &base,
                "bash",
                Some("find . && rm x"),
                None
            ),
            Permission::Deny
        );
        // All segments clear the ceiling — the incoming `Allow` stands.
        let base = PermissionProfile::new(Permission::Allow).with("bash(rm *)", Permission::Deny);
        assert_eq!(
            clamp_to_base(
                Permission::Allow,
                &base,
                "bash",
                Some("find . && git status"),
                None
            ),
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
