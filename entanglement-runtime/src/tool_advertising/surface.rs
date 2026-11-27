//! The runtime's [`ToolSpecResolver`] (ADR-0076 seam): the one place that
//! shapes a session's advertised tools array (ADR-0226). Lives in the library
//! so every head and the integration tests exercise the resolver production
//! runs.
//!
//! This resolver is the **only** thing that shapes the surface — core
//! advertises its output verbatim — so a spec missing here is a spec no model
//! ever sees (the ADR-0190 Bug-1 shape). The surface is every registered tool
//! that sets [`Tool::advertised`][crate::Tool::advertised] plus the
//! executor's own routes (the runtime-owned specs, `agent`/`agent_send`), which
//! are always advertised; there is no name list (#965).

use std::sync::Arc;

use entanglement_core::{SessionId, SessionModel, ToolSpec, ToolSpecResolver};

use super::SharedAdvertisingState;
use crate::{discover, SharedRegistry, ToolRegistry};

/// Everything the resolver reads.
#[derive(Clone)]
pub struct SurfaceSources {
    pub tools: SharedRegistry,
    pub advertising: SharedAdvertisingState,
    /// The `agent`/`agent_send` specs. They ride here rather than
    /// `EngineConfig::tool_specs` because this resolver *replaces* that field
    /// — a spec pushed there never reaches the model when a resolver is
    /// installed, which is how `propose_plan` and then `agent`/`agent_send`
    /// each went missing once.
    pub agent_specs: Vec<ToolSpec>,
}

/// Wrap [`resolve_surface`] as the engine's resolver.
pub fn tool_spec_resolver(sources: SurfaceSources) -> ToolSpecResolver {
    Arc::new(move |session: &SessionId, _model: SessionModel<'_>| {
        resolve_surface(&sources, session)
    })
}

/// `session`'s advertised array: snapshotted on the first call, the same
/// bytes on every later one.
pub fn resolve_surface(src: &SurfaceSources, session: &SessionId) -> Vec<ToolSpec> {
    let mut surfaces = src
        .advertising
        .surfaces
        .lock()
        .expect("surface mutex poisoned");
    if let Some(specs) = surfaces.get(session) {
        return specs.clone();
    }
    let specs = build_surface(
        &src.tools.read().expect("tool registry lock poisoned"),
        src.agent_specs.clone(),
    );
    surfaces.insert(session.clone(), specs.clone());
    specs
}

/// The advertised registered tools plus the runtime-owned pseudo-tools and
/// `agent_specs`, deduped and name-sorted so the order never depends on
/// registration order (#566).
pub fn build_surface(registry: &ToolRegistry, agent_specs: Vec<ToolSpec>) -> Vec<ToolSpec> {
    let mut specs: Vec<ToolSpec> = registry
        .specs()
        .into_iter()
        .filter(|s| registry.get(&s.name).is_some_and(|t| t.advertised()))
        .chain(discover::runtime_owned_specs())
        .chain(agent_specs)
        .collect();
    specs.sort_by(|a, b| a.name.cmp(&b.name));
    specs.dedup_by(|a, b| a.name == b.name);
    specs
}

#[cfg(test)]
mod tests {
    use std::borrow::Cow;

    use async_trait::async_trait;

    use super::*;
    use crate::Tool;

    /// A registered tool that opts into the surface or not.
    struct Flagged(&'static str, bool);
    #[async_trait]
    impl Tool for Flagged {
        fn name(&self) -> Cow<'static, str> {
            Cow::Borrowed(self.0)
        }
        fn advertised(&self) -> bool {
            self.1
        }
        async fn run(&self, _input: &str) -> anyhow::Result<String> {
            Ok(String::new())
        }
    }

    fn names(specs: &[ToolSpec]) -> Vec<&str> {
        specs.iter().map(|s| s.name.as_str()).collect()
    }

    #[test]
    fn flagged_tools_and_executor_routes_are_advertised_sorted_once() {
        let mut registry = ToolRegistry::new();
        for (name, advertised) in [
            ("write", true),
            ("read", true),
            ("glob", false),
            ("grep", false),
            ("read_raw", false),
            ("mcp__gh__search", false),
            ("skill__git__log", false),
            // A registered tool shadowing a runtime-owned name appears once.
            ("poll", true),
        ] {
            registry.register(Flagged(name, advertised));
        }
        let specs = build_surface(&registry, vec![ToolSpec::new("agent", "d")]);
        let got = names(&specs);
        for hidden in [
            "glob",
            "grep",
            "read_raw",
            "mcp__gh__search",
            "skill__git__log",
        ] {
            assert!(
                !got.contains(&hidden),
                "{hidden} is not model-facing: {got:?}"
            );
        }
        for shown in [
            "read",
            "write",
            "agent",
            "poll",
            "mcp",
            "list_mcp",
            "mcp_tools",
            "list_agents",
            "list_models",
            "list_skills",
        ] {
            assert!(got.contains(&shown), "{shown} advertised: {got:?}");
        }
        let mut sorted = got.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(got, sorted, "name-sorted, no duplicates");
    }

    /// The flag, not the name, decides: an unflagged `read` stays hidden.
    #[test]
    fn a_kernel_name_without_the_flag_is_not_advertised() {
        let mut registry = ToolRegistry::new();
        registry.register(Flagged("read", false));
        let specs = build_surface(&registry, Vec::new());
        assert!(!names(&specs).contains(&"read"), "{:?}", names(&specs));
    }
}
