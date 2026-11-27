//! Sub-agent permission gating (#77, ADR-0024; #119, ADR-0040). Runtime-only
//! policies layered on top of the per-tool `Allow | Ask | Deny` dispatch (#59):
//!
//! - **Spawn control** — [`spawn_refusal`]: any agent may be a session root
//!   or a spawn target (ADR-0207 §6), so the old per-profile
//!   `can_spawn`/`spawnable_agents`/target-mode gates (ADR-0040) are gone —
//!   the only target check left is that the name resolves to a real,
//!   registered profile. The call itself is graded like any tool (ADR-0232)
//!   and bounded by the session's mode `max_depth`/`max_agents`
//!   ([`SpawnGuard::try_spawn`][crate::subagent::SpawnGuard::try_spawn]).
//! - **Privilege ceiling** — [`ancestor_chain`]: a child sub-agent is never
//!   more privileged than its ancestors. The main dispatch ladder
//!   (`tool_runner::resolve_effective`) takes the least-privileged
//!   [`crate::policy::PermissionResolver`] grade across exactly this chain,
//!   and every `rhai` binding runs that same ladder (ADR-0227) — the old
//!   `Agent`-chain grading path (`effective_permission`/
//!   `permission_chain`/`permission_for`) that used to serve `rhai` alone is
//!   retired along with it, since nothing resolves a session's mode from
//!   `Agent.permission` any more. Resolution takes the call's
//!   tool-specific argument (command/path, #173) so an argument-scoped rule
//!   matches the actual input; [`permission_arg`] extracts it. A `bash`
//!   call also carries its `workdir` (#425) so a `tool{pattern}`
//!   workdir-scoped rule matches too; [`permission_workdir`] extracts it.
//!
//! The **tool mask** (#116, ADR-0038: `tools`/`disallowed_tools` making a
//! tool not *exist* for a session) is retired (ADR-0207 §8, "the mask
//! machinery is deleted"): `tool_masked`/`tool_mask_source` are gone.
//! `Agent` carries none of `tools`/`disallowed_tools`/`permission`/
//! `can_spawn`/`spawnable_agents`/`sandbox`/`mode` any more (stage 4c/5b) —
//! identity only.
//!
//! Both live in the runtime tool executor's single-threaded loop, folded
//! from the same lifecycle events as permission dispatch — zero core surface.

use std::collections::HashSet;

use entanglement_core::{AgentCatalog, Permission, PermissionProfile, SessionId};

pub use crate::permission_arg::permission_arg;
pub(crate) use crate::permission_arg::takes_permission_arg;
use crate::subagent::SpawnGuard;

/// Target-existence gate for a spawn to `target` (ADR-0207 §6): the only
/// check left once spawning is unconditional — any registered agent is a
/// valid target. The `agent` spec names no agent (ADR-0226: session-invariant,
/// `list_agents` carries the roster), so this is the real validation, and its
/// refusal lists the valid names so the model can retry without a lookup.
/// Returns `None` when the spawn is permitted, else the refusal message to
/// relay to the parent's parked tool call.
pub fn spawn_refusal(target: &str, registry: &AgentCatalog) -> Option<String> {
    if registry.get(target).is_some() {
        return None;
    }
    let valid: Vec<&str> = registry.iter().map(|a| a.name.as_str()).collect();
    Some(format!(
        "sub-agent spawn refused: unknown agent profile `{target}` — valid agents: {} \
         (list_agents describes them).",
        valid.join(", ")
    ))
}

/// Resolve an `agent` call's optional `model` parameter against the active
/// catalog (#560 P12, ADR-0207 §12): a spawning model may pick the child's
/// model, in either a bare `id` (the first *usable* catalog entry whose model
/// id matches wins) or a qualified `provider/id` form that targets exactly
/// one provider — the only way to pick between two providers that
/// legitimately share model ids against different endpoints (`zai` /
/// `zai_paas`, ADR-0203). Only *usable* providers are considered
/// ([`entanglement_provider::ProviderEntry::is_usable`] — a keyless entry,
/// or a keyed one whose env var is actually set): a model
/// nothing can reach is worse to offer than not listing it at all.
///
/// `Ok(None)` means "omitted, inherit exactly as before"; `Ok(Some((provider,
/// model)))` is what the caller sends as `InMsg::SetModel` right after
/// `InMsg::Spawn`. `Err` refuses the whole spawn (mirroring
/// [`spawn_refusal`]'s "no silent substitution" posture) rather than
/// silently falling back to inherit, naming every valid *usable* id in
/// `provider/id` form — unambiguous and directly reusable as the qualified
/// spelling — so the model can retry correctly. No catalog and "a catalog
/// but nothing in it is usable" are distinguished: they are different
/// problems with different fixes (configure a catalog vs. set a key).
pub fn resolve_model(
    requested: Option<&str>,
    catalog: Option<&entanglement_core::Catalog>,
) -> Result<Option<(String, String)>, String> {
    let Some(model) = requested else {
        return Ok(None);
    };
    let Some(catalog) = catalog else {
        return Err(format!(
            "sub-agent spawn refused: model `{model}` requested but no catalog is configured"
        ));
    };
    let usable: Vec<_> = catalog.providers.iter().filter(|p| p.is_usable()).collect();
    if usable.is_empty() {
        return Err(format!(
            "sub-agent spawn refused: model `{model}` requested but no configured provider is \
             currently usable — set an API key for at least one provider in the catalog"
        ));
    }

    if let Some((provider_name, id)) = model.split_once('/') {
        if let Some(provider) = usable
            .iter()
            .find(|p| p.name == provider_name && p.models.iter().any(|m| m.id == id))
        {
            return Ok(Some((provider.name.clone(), id.to_string())));
        }
    } else if let Some(provider) = usable
        .iter()
        .find(|p| p.models.iter().any(|m| m.id == model))
    {
        return Ok(Some((provider.name.clone(), model.to_string())));
    }

    let mut ids: Vec<String> = usable
        .iter()
        .flat_map(|p| p.models.iter().map(|m| format!("{}/{}", p.name, m.id)))
        .collect();
    ids.sort_unstable();
    ids.dedup();
    Err(format!(
        "sub-agent spawn refused: unknown model `{model}` — valid ids: {}",
        ids.join(", ")
    ))
}

/// Clamp an already-resolved permission by the global config base (#172,
/// ADR-0047). The effective grade is the least-privileged of the agent-chain
/// result and the config's rule for the `tool` call — so the user/repo config
/// `permissions` section is a *ceiling*: it can tighten what an agent allows
/// (`bash: ask` forces every agent to ask), never loosen it. `arg` carries the
/// tool-specific argument (#173) and `workdir` the `bash` working
/// directory (#425) so an argument-/workdir-scoped ceiling rule like
/// `bash(rm *): deny` or `bash{/etc/*}: deny` resolves against the actual call.
/// The embedded default is allow-all, so an untouched config is a no-op. This is
/// a pure ceiling (it only tightens); the orthogonal "always allow" grants (#174,
/// [`crate::grants`]) that *raise* an `Ask` are applied by the executor *after*
/// this clamp, so a `Deny` here can never be re-opened by a stale grant.
///
/// `base` keeps its historical [`PermissionProfile`] type — the type every
/// call site in the executor and ~30 test fixtures already
/// threads by value — but is graded through [`crate::mode::Mode::resolve`]
/// via [`crate::mode::Mode::from_permission_profile`] (ADR-0207 stage 6c),
/// not `PermissionProfile`'s own matcher: `config.yml`'s `permissions:`
/// ceiling adopts the same `default`/`allow`/`deny`/`prompt` grammar a mode
/// body uses (§5), so `capabilities` — the call's resolved
/// [`crate::capability::Capability`] set — lets a bare `deny: [write]`
/// ceiling rule catch every write-capable tool, not just one named
/// literally `write`. An allow-all `base` with no rules converts to an
/// empty-rules `Mode` and resolves identically to before this existed.
pub fn clamp_to_base(
    perm: Permission,
    base: &PermissionProfile,
    tool: &str,
    capabilities: &[crate::capability::Capability],
    arg: Option<&str>,
    workdir: Option<&str>,
) -> Permission {
    let ceiling = crate::mode::Mode::from_permission_profile(base);
    min_permission(perm, ceiling.resolve(tool, capabilities, arg, workdir))
}

/// The session + its ancestor chain (nearest first), walking `guard`'s parent
/// links with a cycle guard. The runtime tool executor resolves the effective
/// permission by taking the least-privileged [`PermissionResolver`][crate::policy::PermissionResolver]
/// grade across exactly these sessions — the sub-agent privilege ceiling
/// (ADR-0024) applied *on top of* whatever grade the resolver returns, so a
/// pluggable tenant rule can never widen a child beyond its parent. Shared by
/// `tool_runner`'s dispatch seam, which every `rhai` binding runs too
/// (ADR-0227) — a binding resolves across the identical chain a direct tool
/// call does.
///
/// ADR-0207 §7 retires the sponsored-child exemption (ADR-0138): approving a
/// `propose_plan` now switches the session's mode instead of spawning a
/// permission-root child, so this walk always reaches all the way to the
/// true root — no stop-early case left, no exemption from the clamp above.
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

/// The `workdir` a `bash` invocation would run in, for a workdir-scoped
/// permission rule (#425, `tool{pattern}`,
/// [`entanglement_core::PermissionProfile::resolve_scoped`]) — distinct from
/// [`permission_arg`] (which yields the *command* line for it).
/// `None` for any other tool, an absent `workdir` (the tool then defaults to
/// root), or on malformed input — a workdir-scoped rule then never matches,
/// falling through to the tool's other rules.
pub fn permission_workdir(tool: &str, input: &str) -> Option<String> {
    if !takes_workdir_scope(tool) {
        return None;
    }
    let value: serde_json::Value = serde_json::from_str(input).ok()?;
    value.get("workdir")?.as_str().map(String::from)
}

/// Whether `tool` ever has a [`permission_workdir`] — when not, a
/// workdir-scoped `tool{pattern}` rule can never match a call
/// (`config::mode_warn` warns on one, #881). The gate `permission_workdir`
/// itself checks, so the warning can't drift from what grading sees.
pub(crate) fn takes_workdir_scope(tool: &str) -> bool {
    tool == "bash"
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
    use entanglement_core::Agent;

    fn profile(name: &str) -> Agent {
        Agent {
            name: name.into(),
            description: String::new(),
            system_prompt: String::new(),
            model: None,
            provider: None,
            default_mode: None,
        }
    }

    #[test]
    fn spawn_refusal_only_checks_target_existence() {
        let reg = crate::agents::built_in_registry().expect("built-in agents must parse");
        // Every registered agent is a valid spawn target now (ADR-0207 §6) —
        // `general`, `plan`, and `debug` alike.
        assert!(spawn_refusal("general", &reg).is_none());
        assert!(spawn_refusal("plan", &reg).is_none());
        assert!(spawn_refusal("debug", &reg).is_none());
        // Unknown target name is refused.
        let r = spawn_refusal("ghost", &reg).expect("unknown refused");
        assert!(r.contains("unknown agent profile"), "got: {r}");
        // ADR-0226: the spec carries no roster, so the refusal does.
        for name in ["general", "plan", "debug"] {
            assert!(r.contains(name), "{name} listed: {r}");
        }
        assert!(r.contains("list_agents"), "got: {r}");
    }

    #[test]
    fn spawn_refusal_permits_a_freshly_registered_agent() {
        let mut reg = crate::agents::built_in_registry().expect("built-in agents must parse");
        reg.insert(profile("worker"));
        assert!(spawn_refusal("worker", &reg).is_none());
    }

    fn test_catalog() -> entanglement_core::Catalog {
        serde_yaml::from_str(
            "providers:\n\
             \x20\x20- name: zai\n\
             \x20\x20\x20\x20default_model: glm-5.3\n\
             \x20\x20\x20\x20models:\n\
             \x20\x20\x20\x20\x20\x20- id: glm-5.3\n\
             \x20\x20- name: openai\n\
             \x20\x20\x20\x20default_model: gpt-4o\n\
             \x20\x20\x20\x20models:\n\
             \x20\x20\x20\x20\x20\x20- id: gpt-4o\n",
        )
        .expect("test catalog must parse")
    }

    #[test]
    fn resolve_model_omitted_means_inherit() {
        assert_eq!(resolve_model(None, Some(&test_catalog())), Ok(None));
    }

    #[test]
    fn resolve_model_finds_the_owning_provider() {
        assert_eq!(
            resolve_model(Some("gpt-4o"), Some(&test_catalog())),
            Ok(Some(("openai".to_string(), "gpt-4o".to_string())))
        );
    }

    #[test]
    fn resolve_model_unknown_id_names_every_valid_id_instead_of_falling_back() {
        let err = resolve_model(Some("bogus"), Some(&test_catalog()))
            .expect_err("unknown id must refuse, not silently inherit");
        assert!(err.contains("unknown model `bogus`"), "{err}");
        assert!(err.contains("glm-5.3"), "{err}");
        assert!(err.contains("gpt-4o"), "{err}");
    }

    #[test]
    fn resolve_model_with_no_catalog_refuses_rather_than_guessing() {
        assert!(resolve_model(Some("glm-5.3"), None).is_err());
    }

    /// Two providers genuinely sharing a model id against different endpoints
    /// (`zai`/`zai_paas`, ADR-0203) — `zai_key_env`/`paas_key_env` let each
    /// test point at its own env var name so tests never race each other's
    /// process-global state.
    fn collision_catalog(
        zai_key_env: &str,
        paas_key_env: &str,
        openai_key_env: &str,
    ) -> entanglement_core::Catalog {
        serde_yaml::from_str(&format!(
            "providers:\n\
             \x20\x20- name: zai\n\
             \x20\x20\x20\x20key_env: {zai_key_env}\n\
             \x20\x20\x20\x20default_model: glm-5.1\n\
             \x20\x20\x20\x20models:\n\
             \x20\x20\x20\x20\x20\x20- id: glm-5.1\n\
             \x20\x20- name: zai_paas\n\
             \x20\x20\x20\x20key_env: {paas_key_env}\n\
             \x20\x20\x20\x20default_model: glm-5.1\n\
             \x20\x20\x20\x20models:\n\
             \x20\x20\x20\x20\x20\x20- id: glm-5.1\n\
             \x20\x20- name: openai\n\
             \x20\x20\x20\x20key_env: {openai_key_env}\n\
             \x20\x20\x20\x20default_model: gpt-4o\n\
             \x20\x20\x20\x20models:\n\
             \x20\x20\x20\x20\x20\x20- id: gpt-4o\n"
        ))
        .expect("collision test catalog must parse")
    }

    /// `zai/glm-5.1` (Bug 1): the qualified form is accepted and targets
    /// exactly the named provider, never the sibling that shares the id.
    #[test]
    fn resolve_model_qualified_form_targets_exactly_one_provider() {
        std::env::set_var("PERM_TEST_ZAI_A_560", "k");
        std::env::set_var("PERM_TEST_PAAS_A_560", "k");
        let catalog = collision_catalog(
            "PERM_TEST_ZAI_A_560",
            "PERM_TEST_PAAS_A_560",
            "PERM_TEST_OPENAI_A_560_UNSET",
        );
        assert_eq!(
            resolve_model(Some("zai_paas/glm-5.1"), Some(&catalog)),
            Ok(Some(("zai_paas".to_string(), "glm-5.1".to_string())))
        );
        assert_eq!(
            resolve_model(Some("zai/glm-5.1"), Some(&catalog)),
            Ok(Some(("zai".to_string(), "glm-5.1".to_string())))
        );
        std::env::remove_var("PERM_TEST_ZAI_A_560");
        std::env::remove_var("PERM_TEST_PAAS_A_560");
    }

    /// A colliding bare id resolves deterministically — the first usable
    /// catalog entry carrying it (catalog order), not an error or a guess.
    #[test]
    fn resolve_model_bare_colliding_id_picks_first_usable_provider() {
        std::env::set_var("PERM_TEST_ZAI_B_560", "k");
        std::env::set_var("PERM_TEST_PAAS_B_560", "k");
        let catalog = collision_catalog(
            "PERM_TEST_ZAI_B_560",
            "PERM_TEST_PAAS_B_560",
            "PERM_TEST_OPENAI_B_560_UNSET",
        );
        assert_eq!(
            resolve_model(Some("glm-5.1"), Some(&catalog)),
            Ok(Some(("zai".to_string(), "glm-5.1".to_string())))
        );
        std::env::remove_var("PERM_TEST_ZAI_B_560");
        std::env::remove_var("PERM_TEST_PAAS_B_560");
    }

    /// Bug 2: a provider with no usable key contributes nothing to the
    /// refusal's valid-id list.
    #[test]
    fn resolve_model_refusal_lists_only_usable_providers() {
        std::env::set_var("PERM_TEST_ZAI_C_560", "k");
        std::env::set_var("PERM_TEST_PAAS_C_560", "k");
        std::env::remove_var("PERM_TEST_OPENAI_C_560_UNSET");
        let catalog = collision_catalog(
            "PERM_TEST_ZAI_C_560",
            "PERM_TEST_PAAS_C_560",
            "PERM_TEST_OPENAI_C_560_UNSET",
        );
        let err = resolve_model(Some("bogus"), Some(&catalog)).expect_err("unknown id refuses");
        assert!(err.contains("zai/glm-5.1"), "{err}");
        assert!(err.contains("zai_paas/glm-5.1"), "{err}");
        assert!(!err.contains("gpt-4o"), "{err}");
        std::env::remove_var("PERM_TEST_ZAI_C_560");
        std::env::remove_var("PERM_TEST_PAAS_C_560");
    }

    /// Bug 2, the plain-message half: a catalog is configured but nothing in
    /// it is usable — distinct from "no catalog configured" (different fix).
    #[test]
    fn resolve_model_no_usable_provider_says_so_plainly() {
        std::env::remove_var("PERM_TEST_ZAI_D_560_UNSET");
        std::env::remove_var("PERM_TEST_PAAS_D_560_UNSET");
        std::env::remove_var("PERM_TEST_OPENAI_D_560_UNSET");
        let catalog = collision_catalog(
            "PERM_TEST_ZAI_D_560_UNSET",
            "PERM_TEST_PAAS_D_560_UNSET",
            "PERM_TEST_OPENAI_D_560_UNSET",
        );
        let err =
            resolve_model(Some("glm-5.1"), Some(&catalog)).expect_err("nothing usable refuses");
        assert!(
            err.contains("no configured provider is currently usable"),
            "{err}"
        );
        assert!(!err.contains("valid ids"), "{err}");
    }

    /// ADR-0207 §7: the sponsored-child exemption (ADR-0138) is retired along
    /// with the sponsored `propose_plan` build handoff — `SpawnGuard` no
    /// longer has any API to mark a session a permission root, so every
    /// child's chain walks all the way to its true root. A three-deep chain
    /// (mirroring what a `plan` → `build` handoff used to short-circuit)
    /// proves the clamp has no exemption left to consult.
    #[test]
    fn ancestor_clamp_has_no_sponsorship_exemption_left() {
        let root = SessionId::new("root");
        let mid = SessionId::new("mid");
        let leaf = SessionId::new("leaf");
        let mut guard = SpawnGuard::new();
        guard.record_start(root.clone(), None);
        guard.record_start(mid.clone(), Some(root.clone()));
        guard.record_start(leaf.clone(), Some(mid.clone()));

        let chain = ancestor_chain(&guard, &leaf);
        assert_eq!(
            chain,
            vec![leaf.clone(), mid.clone(), root.clone()],
            "the walk must reach every ancestor — nothing can mark itself a \
             permission root any more"
        );
    }

    #[test]
    fn the_ceiling_ranks_a_tool_named_rule_above_a_class_rule() {
        // ADR-0228: the ceiling grades through the same engine as a mode, so
        // `deny: [write]` + `allow: [edit]` lets `edit` through — the rule
        // naming the tool outranks the longer class key.
        let caps = [crate::capability::Capability::Write];
        let base = PermissionProfile::new(Permission::Allow)
            .with("write", Permission::Deny)
            .with("edit", Permission::Allow);
        assert_eq!(
            clamp_to_base(Permission::Allow, &base, "edit", &caps, None, None),
            Permission::Allow
        );
        assert_eq!(
            clamp_to_base(Permission::Allow, &base, "apply_patch", &caps, None, None),
            Permission::Deny
        );
        // A `*` rule meets a named one by length: the long credentials deny
        // beats `bash(cat*)`.
        let creds = PermissionProfile::new(Permission::Allow)
            .with("*(*credentials*)", Permission::Deny)
            .with("bash(cat*)", Permission::Allow);
        let exec = [crate::capability::Capability::Exec];
        assert_eq!(
            clamp_to_base(
                Permission::Allow,
                &creds,
                "bash",
                &exec,
                Some("cat .credentials"),
                None
            ),
            Permission::Deny
        );
    }

    #[test]
    fn clamp_to_base_is_a_least_privilege_ceiling() {
        // Allow-all base (the embedded default) never changes the agent's grade.
        let open = PermissionProfile::new(Permission::Allow);
        assert_eq!(
            clamp_to_base(Permission::Allow, &open, "bash", &[], None, None),
            Permission::Allow
        );
        assert_eq!(
            clamp_to_base(Permission::Ask, &open, "bash", &[], None, None),
            Permission::Ask
        );
        // A base `bash: ask` tightens an agent's Allow to Ask, but leaves a
        // stricter agent Deny untouched (least-privilege wins either way).
        let base = PermissionProfile::new(Permission::Allow).with("bash", Permission::Ask);
        assert_eq!(
            clamp_to_base(Permission::Allow, &base, "bash", &[], None, None),
            Permission::Ask
        );
        assert_eq!(
            clamp_to_base(Permission::Deny, &base, "bash", &[], None, None),
            Permission::Deny
        );
        // The base never loosens: base Allow over an agent Ask stays Ask.
        assert_eq!(
            clamp_to_base(Permission::Ask, &base, "read", &[], None, None),
            Permission::Ask
        );
    }

    /// ADR-0207 stage 6c: `config.yml`'s `permissions: {deny: [write]}`
    /// ceiling — a bare capability-class key spelled exactly like a mode's
    /// own `deny: [write]` — must deny every write-capable tool (`edit`
    /// included), not just a literal tool named `write`. This is the
    /// ceiling's own version of the tuning guard's worked example.
    #[test]
    fn clamp_to_base_honors_capability_class_ceiling_rules() {
        let base = PermissionProfile::new(Permission::Allow).with("write", Permission::Deny);
        assert_eq!(
            clamp_to_base(
                Permission::Allow,
                &base,
                "edit",
                &[crate::capability::Capability::Write],
                None,
                None
            ),
            Permission::Deny,
            "edit is Write-capable even though the ceiling rule literally says `write`"
        );
        assert_eq!(
            clamp_to_base(
                Permission::Allow,
                &base,
                "read",
                &[crate::capability::Capability::Read],
                None,
                None
            ),
            Permission::Allow
        );
    }

    /// ADR-0228: a `path` run is graded and escape-checked on its path; an
    /// inline script has no argument, so only a bare `rhai` rule matches it.
    #[test]
    fn rhai_path_is_its_permission_argument_and_escape_target() {
        use crate::path_grading::escape_target;
        let none = crate::tools::ToolRegistry::new();
        let path = r#"{"path":"scripts/x.rhai","args":{"n":1}}"#;
        assert_eq!(
            permission_arg("rhai", path).as_deref(),
            Some("scripts/x.rhai")
        );
        assert_eq!(
            escape_target("rhai", &none, path).as_deref(),
            Some("scripts/x.rhai")
        );
        let inline = r#"{"script":"read(\"/etc/x\")"}"#;
        assert_eq!(escape_target("rhai", &none, inline), None);
        assert_eq!(permission_arg("rhai", inline), None);
    }

    #[test]
    fn permission_arg_extracts_per_tool_shape() {
        // bash → the raw command.
        assert_eq!(
            permission_arg("bash", r#"{"command":"git status"}"#).as_deref(),
            Some("git status")
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
        // web_fetch → the URL (#736).
        assert_eq!(
            permission_arg("web_fetch", r#"{"url":"https://docs.rs/x"}"#).as_deref(),
            Some("https://docs.rs/x")
        );
        // …normalized exactly as the fetcher will request it.
        assert_eq!(
            permission_arg("web_fetch", r#"{"url":" HTTPS://PASTEBIN.COM.:443/x"}"#).as_deref(),
            Some("https://pastebin.com/x")
        );
        assert_eq!(permission_arg("web_fetch", r#"{"url":"nope"}"#), None);
        // http → `{METHOD} {url}`, both normalized (ADR-0227).
        assert_eq!(
            permission_arg(
                "http",
                r#"{"method":"get","url":"HTTPS://API.github.com./r"}"#
            )
            .as_deref(),
            Some("GET https://api.github.com/r")
        );
        assert_eq!(
            permission_arg("http", r#"{"method":"TRACE","url":"https://a/"}"#),
            None
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

    /// ADR-0235: `agent` grades on the profile the spawn runs — the default
    /// when the input names none or isn't JSON — and `agent_send` on its
    /// trimmed handle.
    #[test]
    fn spawning_pair_permission_args_are_their_targets() {
        let agent = |input| permission_arg(crate::tool_names::AGENT_TOOL, input);
        assert_eq!(
            agent(r#"{"agent":"plan","prompt":"x"}"#).as_deref(),
            Some("plan")
        );
        for defaulted in [
            r#"{"prompt":"x"}"#,
            r#"{"agent":"","prompt":"x"}"#,
            "raw prompt",
        ] {
            assert_eq!(agent(defaulted).as_deref(), Some("general"), "{defaulted}");
        }
        let send = |input| permission_arg(crate::tool_names::AGENT_SEND_TOOL, input);
        assert_eq!(
            send(r#"{"agent_id":" s-1 ","prompt":"x"}"#).as_deref(),
            Some("s-1")
        );
        assert_eq!(send(r#"{"agent_id":"  ","prompt":"x"}"#), None);
        assert_eq!(send(r#"{"prompt":"x"}"#), None);
        assert_eq!(send("not json"), None);
    }

    /// The scoped rule reaches the mode matcher: `agent(plan)` grades a
    /// `plan` spawn and leaves every other target to the rest of the table.
    #[test]
    fn a_scoped_agent_rule_grades_only_its_target() {
        use crate::mode::Rules;
        let rules = Rules::from_lists(
            &["agent(plan)".to_string()],
            &[],
            &["agent_send(acme:*)".to_string()],
        );
        let control = [crate::capability::Capability::Control];
        let grade = |tool: &str, input: &str| {
            let arg = permission_arg(tool, input);
            crate::mode::rules::resolve(
                &rules,
                Permission::Allow,
                tool,
                &control,
                arg.as_deref(),
                None,
            )
        };
        assert_eq!(grade("agent", r#"{"agent":"plan"}"#), Permission::Deny);
        assert_eq!(grade("agent", r#"{"agent":"general"}"#), Permission::Allow);
        assert_eq!(
            grade("agent_send", r#"{"agent_id":"acme:s-1"}"#),
            Permission::Ask
        );
        assert_eq!(
            grade("agent_send", r#"{"agent_id":"s-1"}"#),
            Permission::Allow
        );
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
    fn permission_workdir_extracts_bash_only() {
        assert_eq!(
            permission_workdir("bash", r#"{"command":"ls","workdir":"/tmp"}"#).as_deref(),
            Some("/tmp")
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

    /// #881: `takes_workdir_scope` agrees with what `permission_workdir`
    /// actually extracts, for every known tool and for an MCP tool.
    #[test]
    fn takes_workdir_scope_matches_what_permission_workdir_extracts() {
        let input = r#"{"command":"x","path":"a","workdir":"/tmp"}"#;
        for tool in crate::tool_names::known_tool_names()
            .iter()
            .copied()
            .chain(["mcp__gh__create"])
        {
            assert_eq!(
                takes_workdir_scope(tool),
                permission_workdir(tool, input).is_some(),
                "{tool}"
            );
        }
        assert!(takes_workdir_scope("bash"));
    }

    /// The config ceiling's own workdir-scoped rule (#425) — the one
    /// coverage-preserving fragment kept from the retired
    /// `workdir_scoped_rule_resolves_through_the_agent_chain` (deleted with
    /// `effective_permission`/`permission_for`, ADR-0207 stage 4b): `clamp_to_base`
    /// itself is still very much alive, on the mode resolver's ceiling path
    /// (`policy::ModeResolver`).
    #[test]
    fn clamp_to_base_honors_workdir_scoped_ceiling() {
        let deny_etc =
            PermissionProfile::new(Permission::Allow).with("bash{/etc/*}", Permission::Deny);
        assert_eq!(
            clamp_to_base(
                Permission::Allow,
                &deny_etc,
                "bash",
                &[],
                None,
                Some("/etc/cron.d")
            ),
            Permission::Deny
        );
    }

    #[test]
    fn clamp_to_base_honors_argument_scoped_ceiling() {
        // A config ceiling that hard-denies `rm *` but leaves other bash alone.
        let base = PermissionProfile::new(Permission::Allow).with("bash(rm *)", Permission::Deny);
        assert_eq!(
            clamp_to_base(
                Permission::Allow,
                &base,
                "bash",
                &[],
                Some("rm -rf /"),
                None
            ),
            Permission::Deny
        );
        assert_eq!(
            clamp_to_base(
                Permission::Allow,
                &base,
                "bash",
                &[],
                Some("git status"),
                None
            ),
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
                &[],
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
                &[],
                Some("find . && git status"),
                None
            ),
            Permission::Allow
        );
    }
}
