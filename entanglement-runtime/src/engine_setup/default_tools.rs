//! The default tool registry (split out of `engine_setup.rs` at the file
//! cap): the root-contained sextet plus the always-registered `bash`.

use std::sync::Arc;

use crate::host::BashTool;
use crate::{host, policy, ToolRegistry};

/// What [`register_default_tools`] wires into the host tools.
pub(crate) struct DefaultTools {
    /// The sextet's escape-root store, backups and session workdirs; the
    /// store is shared with `bash`.
    pub(crate) host: host::HostToolOptions,
    pub(crate) secret_env: Vec<String>,
    pub(crate) sandbox_resolver: Arc<dyn policy::SandboxResolver>,
    pub(crate) jobs: host::JobRegistry,
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
    // `web_fetch` (#736) is a kernel tool (ADR-0226): advertised natively
    // whenever it is compiled in and its client builds.
    #[cfg(feature = "web-fetch")]
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
    fn tool_names() -> Vec<String> {
        let root = std::env::temp_dir();
        register_default_tools(
            root,
            DefaultTools {
                host: crate::host::HostToolOptions::default(),
                secret_env: Vec::new(),
                sandbox_resolver: std::sync::Arc::new(SandboxPolicy::none()),
                jobs: crate::host::JobRegistry::new(),
            },
        )
        .specs()
        .into_iter()
        .map(|s| s.name.to_string())
        .collect()
    }

    #[test]
    fn bash_is_registered_unconditionally_at_startup() {
        // ADR-0195: `bash` is an always-registered built-in — no env var, no
        // toggle needed. What a run may do with it is the permission
        // modes' + the config ceiling's job, not the registry's.
        let names = tool_names();
        assert!(names.contains(&"bash".to_string()), "{names:?}");
        #[cfg(feature = "web-fetch")]
        assert!(names.contains(&"web_fetch".to_string()), "{names:?}");
        // The sextet is still there alongside them.
        for expected in ["read", "glob", "grep", "edit", "write", "apply_patch"] {
            assert!(names.contains(&expected.to_string()), "{names:?}");
        }
    }
}
