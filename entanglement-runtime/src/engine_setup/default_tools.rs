//! The default tool registry (split out of `engine_setup.rs` at the file
//! cap): the root-contained sextet plus the always-registered exec pair.

use std::sync::Arc;

use crate::host::{BashTool, CallTool};
use crate::{host, policy, retained_output, ToolRegistry};

/// What [`register_default_tools`] wires into the host tools.
pub(crate) struct DefaultTools {
    /// The sextet's escape-root store, backups and session workdirs; the
    /// store is shared with `call`/`bash`.
    pub(crate) host: host::HostToolOptions,
    pub(crate) secret_env: Vec<String>,
    pub(crate) sandbox_resolver: Arc<dyn policy::SandboxResolver>,
    pub(crate) jobs: host::JobRegistry,
    pub(crate) retained: retained_output::RetainedOutputRegistry,
}

/// Assemble the tool registry: the root-contained sextet plus the exec pair —
/// `call` (registered unconditionally since ADR-0094) and `bash` (registered
/// unconditionally since ADR-0195, which retired the `ENTANGLEMENT_ENABLE_BASH`
/// opt-in: registration is not where either tool's security story lives, the
/// permission profiles + config ceiling are). `jobs` (#605; #606) is the one
/// process-lifetime `JobRegistry` `poll` was wired up with — shared with both
/// exec tools (not minted fresh) so a `call background=true`/`bash`
/// background job is the same one `poll` can see. `retained` (#608) is
/// likewise the one process-lifetime `RetainedOutputRegistry` `poll` was
/// wired up with, shared with `call` so a truncated blocking result's handle
/// is pollable. `secret_env` (the catalog's provider API-key env vars, #164)
/// is scrubbed from both exec tools' children. `sandbox_resolver`
/// (#399/ADR-0104, #479) resolves both `bash` and `call`'s bubblewrap
/// confinement per session/profile — a resolver that always returns
/// `SandboxPolicy::none()` leaves their spawn behavior unchanged.
pub(crate) fn register_default_tools(root: std::path::PathBuf, opts: DefaultTools) -> ToolRegistry {
    let DefaultTools {
        host,
        secret_env,
        sandbox_resolver,
        jobs,
        retained,
    } = opts;
    let extra_roots = host.extra_roots.clone();
    let mut tools = host::host_tools_with(root.clone(), host);
    let mut call = CallTool::new(root.clone())
        .with_secret_env(secret_env.clone())
        .with_sandbox_resolver(sandbox_resolver.clone())
        .with_jobs(jobs.clone())
        .with_retained_output(retained);
    if let Some(e) = &extra_roots {
        call = call.with_extra_roots(e.clone());
    }
    tools.register(call);
    let mut bash = BashTool::new(root.clone())
        .with_secret_env(secret_env.clone())
        .with_jobs(jobs)
        .with_sandbox_resolver(sandbox_resolver);
    if let Some(e) = &extra_roots {
        bash = bash.with_extra_roots(e.clone());
    }
    tools.register(bash);
    // `web_fetch` (#736) is not in the tool-search kernel: under
    // `tool_search` it surfaces through `explore`/`describe` like any other
    // registered tool, so registering it never changes the advertised array.
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
    use crate::retained_output;

    /// The advertised roster `register_default_tools` produces with no env var
    /// or opt-in of any kind — the ADR-0195 posture: both exec tools are
    /// registered unconditionally at startup, exactly like the sextet.
    fn tool_names() -> Vec<String> {
        let root = std::env::temp_dir();
        register_default_tools(
            root,
            DefaultTools {
                host: crate::host::HostToolOptions::default(),
                secret_env: Vec::new(),
                sandbox_resolver: std::sync::Arc::new(SandboxPolicy::none()),
                jobs: crate::host::JobRegistry::new(),
                retained: retained_output::RetainedOutputRegistry::new(),
            },
        )
        .specs()
        .into_iter()
        .map(|s| s.name.to_string())
        .collect()
    }

    #[test]
    fn call_and_bash_are_registered_unconditionally_at_startup() {
        // ADR-0195: `bash` joins `call` (ADR-0093) as an always-registered
        // built-in — no env var, no `/enable` needed. What a run may do with
        // it is the permission profiles' + the config ceiling's job, not the
        // registry's.
        let names = tool_names();
        assert!(names.contains(&"call".to_string()), "{names:?}");
        assert!(names.contains(&"bash".to_string()), "{names:?}");
        #[cfg(feature = "web-fetch")]
        assert!(names.contains(&"web_fetch".to_string()), "{names:?}");
        // The sextet is still there alongside them.
        for expected in ["read", "glob", "grep", "edit", "write", "apply_patch"] {
            assert!(names.contains(&expected.to_string()), "{names:?}");
        }
    }
}
