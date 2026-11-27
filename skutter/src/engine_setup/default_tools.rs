//! The default tool registry (split out of `engine_setup.rs` at the file
//! cap): the root-contained sextet plus the always-registered `bash`.

use std::sync::Arc;

use crate::host::BashTool;
use crate::{host, ToolRegistry};

/// What [`register_default_tools`] wires into the host tools.
pub(crate) struct DefaultTools {
    /// The sextet's escape-root store, backups and session workdirs; the
    /// store is shared with `bash`.
    pub(crate) host: host::HostToolOptions,
    pub(crate) secret_env: Vec<String>,
    pub(crate) sandbox_resolver: Arc<dyn host::SandboxResolver>,
    pub(crate) jobs: crate::jobs::JobRegistry,
}

/// Assemble the tool registry: the root-contained sextet plus `bash`,
/// registered unconditionally since ADR-0195 retired the
/// `ENTANGLEMENT_ENABLE_BASH` opt-in (registration is not where its security
/// story lives, the permission modes + config ceiling are). `jobs` (#605) is
/// the one process-lifetime `JobRegistry` `poll` was wired up with — shared,
/// not minted fresh, so a `bash` background job is the same one `poll` can
/// see. `secret_env` (the catalog's provider API-key env vars, #164) is
/// scrubbed from its children. `sandbox_resolver` (#399/ADR-0104, #479)
/// resolves its bubblewrap confinement per session/mode — a resolver that
/// always returns `SandboxPolicy::none()` leaves its spawn behavior unchanged.
pub(crate) fn register_default_tools(root: std::path::PathBuf, opts: DefaultTools) -> ToolRegistry {
    let DefaultTools {
        host,
        secret_env,
        sandbox_resolver,
        jobs,
    } = opts;
    let extra_roots = host.extra_roots.clone();
    let mut tools = host::host_tools_with(root.clone(), host);
    let mut bash = BashTool::new(root.clone())
        .with_secret_env(secret_env)
        .with_jobs(jobs)
        .with_sandbox_resolver(sandbox_resolver);
    if let Some(e) = &extra_roots {
        bash = bash.with_extra_roots(e.clone());
    }
    tools.register(bash);
    // `web_fetch` (#736) is an advertised tool (ADR-0226): on the surface
    // whenever it is compiled in and its client builds.
    match host::WebFetchTool::new() {
        Ok(tool) => tools.register(tool),
        Err(e) => tracing::warn!("web_fetch unavailable: {e:#}"),
    }
    tools
}

#[cfg(test)]
mod tests {
    use super::{register_default_tools, DefaultTools};
    use crate::host::SandboxPolicy;

    /// The advertised roster `register_default_tools` produces with no env var
    /// or opt-in of any kind — the ADR-0195 posture: `bash` is registered
    /// unconditionally at startup, exactly like the sextet.
    fn default_registry() -> crate::ToolRegistry {
        register_default_tools(
            std::env::temp_dir(),
            DefaultTools {
                host: crate::host::HostToolOptions::default(),
                secret_env: Vec::new(),
                sandbox_resolver: std::sync::Arc::new(SandboxPolicy::none()),
                jobs: crate::jobs::JobRegistry::new(),
            },
        )
    }

    fn tool_names() -> Vec<String> {
        default_registry()
            .specs()
            .into_iter()
            .map(|s| s.name.to_string())
            .collect()
    }

    /// #965: the name list `KERNEL` was until 0.8, kept only to prove the
    /// per-tool flag advertises exactly what it did.
    const FORMER_KERNEL: &[&str] = &[
        "read",
        "edit",
        "write",
        "apply_patch",
        "bash",
        "poll",
        "ask_user",
        "update_tasks",
        "load_skill",
        "propose_plan",
        "request_mode",
        "agent",
        "agent_send",
        "rhai",
        "web_fetch",
        "list_agents",
        "list_models",
        "list_skills",
        "list_mcp",
        "mcp_tools",
        "mcp",
    ];

    /// #965: skutter's default registry (what `build_config` registers before
    /// any MCP server connects) advertises the byte-identical first-request
    /// surface the `KERNEL` name filter produced.
    #[test]
    fn the_flag_surface_is_byte_identical_to_the_former_kernel() {
        let root = std::env::temp_dir();
        let mut registry = default_registry();
        let skills = std::sync::Arc::new(std::sync::RwLock::new(std::sync::Arc::new(
            crate::skills::SkillRegistry::default(),
        )));
        registry.register(crate::skills::LoadSkillTool::new(skills));
        registry.register(crate::host::ReadRawTool::new(root.clone()));
        registry.register(crate::host::GlobJsonTool::new(root.clone()));
        registry.register(crate::host::GrepJsonTool::new(root));

        let mut former: Vec<_> = registry
            .specs()
            .into_iter()
            .chain(crate::discover::runtime_owned_specs())
            .chain(crate::subagent::agent_specs())
            .filter(|s| FORMER_KERNEL.contains(&s.name.as_str()))
            .collect();
        former.sort_by(|a, b| a.name.cmp(&b.name));
        former.dedup_by(|a, b| a.name == b.name);

        let surface = crate::tool_advertising::surface::build_surface(
            &registry,
            crate::subagent::agent_specs(),
        );
        assert_eq!(format!("{surface:?}"), format!("{former:?}"));
        let names: Vec<&str> = surface.iter().map(|s| s.name.as_str()).collect();
        let mut expected = FORMER_KERNEL.to_vec();
        expected.sort_unstable();
        assert_eq!(names, expected, "every former kernel name, nothing else");
    }

    #[test]
    fn bash_is_registered_unconditionally_at_startup() {
        // ADR-0195: `bash` is an always-registered built-in — no env var, no
        // toggle needed. What a run may do with it is the permission
        // modes' + the config ceiling's job, not the registry's.
        let names = tool_names();
        assert!(names.contains(&"bash".to_string()), "{names:?}");
        assert!(names.contains(&"web_fetch".to_string()), "{names:?}");
        // The sextet is still there alongside them.
        for expected in ["read", "glob", "grep", "edit", "write", "apply_patch"] {
            assert!(names.contains(&expected.to_string()), "{names:?}");
        }
    }
}
