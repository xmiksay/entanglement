//! The convenience entry points over [`super::spawn_tool_executor_with_policy`]
//! (issue #712, split out of `tool_runner.rs`): the historical 4-arg
//! [`spawn_tool_executor`] and its hook-taking sibling, which plug in the
//! default single-user policy seams. Re-exported at their pre-split paths
//! (`tool_runner::spawn_tool_executor`/`spawn_tool_executor_with_hooks`).

use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};

use entanglement_core::{AgentCatalog, Holly, PermissionProfile};

use crate::hooks::Hooks;
use crate::mode::ModeTable;
use crate::plan_files::PlanFileRegistry;
use crate::policy::{DefaultGrantStore, GrantStore, ModeResolver, PermissionResolver};
use crate::skills::SkillRegistry;
use crate::tools::ToolRegistry;

use super::spawn_tool_executor_with_policy;

/// Spawn the per-engine tool executor. Subscribes synchronously (so no
/// `ToolExec` emitted before the task is scheduled is missed) and runs until the
/// engine's outbox closes. `agents` is the runtime's copy of the engine's
/// [`AgentCatalog`] — the permission *shape* stays a core type; the runtime
/// only reads it (ADR-0003). `base` is the user config's global permission
/// ceiling (#172): every resolved grade is clamped least-privilege against it.
pub fn spawn_tool_executor(
    holly: &Holly,
    tools: ToolRegistry,
    agents: AgentCatalog,
    base: PermissionProfile,
) -> tokio::task::JoinHandle<()> {
    spawn_tool_executor_with_hooks(holly, tools, agents, base, Hooks::default())
}

/// Wrap a caller's [`SkillRegistry`] for [`spawn_tool_executor_with_policy`]'s
/// `skills` parameter, mirroring [`wrap_agents`]. The convenience wrappers
/// below plug in an empty registry — no `load_skill` mask ever activates for
/// their (~30, test-only) callers, matching their historical no-skill-mask
/// behavior byte-for-byte.
fn wrap_skills(skills: SkillRegistry) -> Arc<RwLock<Arc<SkillRegistry>>> {
    Arc::new(RwLock::new(Arc::new(skills)))
}

/// Wrap a caller's owned [`AgentCatalog`] for [`spawn_tool_executor_with_policy`],
/// which reads it through an `Arc<RwLock<..>>` so a live definitions watcher
/// (#329) can swap it for a fresher one without restarting the executor. The
/// convenience wrappers here keep their historical owned-registry signature for
/// existing callers (and tests) that need no live reload.
fn wrap_agents(agents: AgentCatalog) -> Arc<RwLock<AgentCatalog>> {
    Arc::new(RwLock::new(agents))
}

/// Like [`spawn_tool_executor`] but with user-configured lifecycle hooks (#199,
/// ADR-0066): `pre_tool_use` can veto a generic tool dispatch, `post_tool_use`
/// runs as a side-effect after it, and `user_prompt_submit` fires on every
/// inbound `Prompt`. The no-hook wrapper keeps the historical 4-arg signature for
/// callers (and tests) that need no hooks.
pub fn spawn_tool_executor_with_hooks(
    holly: &Holly,
    tools: ToolRegistry,
    agents: AgentCatalog,
    base: PermissionProfile,
    hooks: Hooks,
) -> tokio::task::JoinHandle<()> {
    // The default single-user policy (#311, ADR-0207 stage 4): the executor
    // folds lifecycle events into `active` (still needed for spawn gating
    // and the `rhai` binding policy) and `modes` (the session→mode map
    // `ModeResolver` grades from — sandboxing reads the identical map via
    // `crate::policy::ModeSandboxResolver`, ADR-0207 §6, wired independently
    // by a caller that wants it; these test-only wrappers don't). "Always
    // allow" grants persist to the managed file.
    let active = Arc::new(Mutex::new(HashMap::new()));
    let modes = Arc::new(Mutex::new(HashMap::new()));
    let mode_table = Arc::new(ModeTable::builtin().expect("built-in permission modes must parse"));
    let shared_tools = tools.shared();
    // No escape-root policy wired here (root: None) — the strict-containment
    // 4-arg wrapper keeps the pre-#485 verbatim arg match (ADR-0125).
    let resolver: Arc<dyn PermissionResolver> = Arc::new(ModeResolver::new(
        modes.clone(),
        mode_table.clone(),
        shared_tools.clone(),
        base.clone(),
        None,
    ));
    let grants: Arc<dyn GrantStore> = Arc::new(DefaultGrantStore::load());
    spawn_tool_executor_with_policy(
        holly,
        shared_tools,
        // No job registry shared with an external `BashTool` here — the
        // convenience wrappers' (~30, test-only) callers never wire one up
        // either, so a `poll` of a job id from this executor's own private
        // registry is simply unreachable, matching pre-#605 behavior where
        // these wrappers never registered `bash_output` against a shared one.
        crate::host::jobs::JobRegistry::new(),
        // Same story for retained output (#608): no external `CallTool` shares
        // this private registry either, so a truncated call from one of these
        // wrappers' callers never mints a handle in the first place.
        crate::retained_output::RetainedOutputRegistry::new(),
        // And for background scripts (#637): a private registry is still
        // reachable here — the executor's own `rhai` arm writes it and its
        // `poll` arm reads it back, both inside this one executor.
        crate::script_ops::ScriptRegistry::new(),
        wrap_agents(agents),
        wrap_skills(SkillRegistry::default()),
        base,
        active,
        modes,
        resolver,
        grants,
        hooks,
        // The default 4-arg wrapper keeps strict root containment — escape-root
        // approval is opt-in, wired only by the full head (`main.rs`).
        None,
        mode_table,
        // These convenience wrappers' (~30, test-only) callers never wire up a
        // plans-folder watch either, so a private, unshared registry is fine —
        // matches their historical no-external-sharing behavior for `jobs`/
        // `retained` above.
        Arc::new(PlanFileRegistry::new()),
        // No per-user MCP scopes (#684) — single-user, like skutter itself.
        None,
        // No advertising-resolution inputs (ADR-0196 Phase P1): these
        // wrappers' (~30, test-only) callers hand no catalog/config pair, so
        // every session resolves `tool_search` — the map still folds,
        // nothing dispatches on it yet.
        None,
        // No discovery surface (ADR-0196 §4): these wrappers' (~30, test-only)
        // callers build no `tool_spec_resolver` of their own either, so a
        // private, unshared `AdvertisingState` and an empty MCP roster (
        // `explore` simply has nothing MCP to report) are exactly right.
        None,
    )
}
