//! The runtime's [`ToolSpecResolver`] (ADR-0076 seam): the one place that
//! shapes a session's advertised tools array (ADR-0226). Lives in the library
//! so every head and the integration tests exercise the resolver production
//! runs.
//!
//! This resolver is the **only** thing that shapes the surface — core
//! advertises its output verbatim — so a spec missing here is a spec no model
//! ever sees (the ADR-0190 Bug-1 shape).

use std::sync::Arc;

use entanglement_core::{SessionId, SessionModel, ToolSpec, ToolSpecResolver};

use super::SharedAdvertisingState;
use crate::tool_names::KERNEL;
use crate::{discover, SharedRegistry};

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
    let registry_specs = src
        .tools
        .read()
        .expect("tool registry lock poisoned")
        .specs();
    let specs = build_surface(registry_specs, src.agent_specs.clone());
    surfaces.insert(session.clone(), specs.clone());
    specs
}

/// The kernel picked out of every registered spec plus the runtime-owned
/// pseudo-tools, deduped and name-sorted so the order never depends on
/// registration order (#566).
pub fn build_surface(registry_specs: Vec<ToolSpec>, agent_specs: Vec<ToolSpec>) -> Vec<ToolSpec> {
    let mut specs: Vec<ToolSpec> = registry_specs
        .into_iter()
        .chain(discover::runtime_owned_specs())
        .chain(agent_specs)
        .filter(|s| is_advertised(&s.name))
        .collect();
    specs.sort_by(|a, b| a.name.cmp(&b.name));
    specs.dedup_by(|a, b| a.name == b.name);
    specs
}

/// Kernel tools, plus the definition-driven `endpoint__*`/`skill__*` tools
/// until #819/#820 retire them: with `describe` gone they have no other
/// model-facing path.
fn is_advertised(name: &str) -> bool {
    KERNEL.contains(&name) || name.starts_with("endpoint__") || name.starts_with("skill__")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(name: &str) -> ToolSpec {
        ToolSpec::new(name, "d")
    }

    fn names(specs: &[ToolSpec]) -> Vec<&str> {
        specs.iter().map(|s| s.name.as_str()).collect()
    }

    #[test]
    fn only_kernel_and_definition_tools_are_advertised_sorted_once() {
        let registry = [
            "write",
            "read",
            "glob",
            "grep",
            "call",
            "read_raw",
            "mcp__gh__search",
        ]
        .map(spec)
        .into_iter()
        .chain([
            spec("endpoint__weather"),
            spec("skill__git__log"),
            spec("read"),
        ])
        .collect();
        let specs = build_surface(registry, vec![spec("agent")]);
        let got = names(&specs);
        for hidden in ["glob", "grep", "call", "read_raw", "mcp__gh__search"] {
            assert!(
                !got.contains(&hidden),
                "{hidden} is not model-facing: {got:?}"
            );
        }
        for shown in [
            "read",
            "write",
            "agent",
            "endpoint__weather",
            "skill__git__log",
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
}
