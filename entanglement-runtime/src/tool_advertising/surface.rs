//! The runtime's [`ToolSpecResolver`] (ADR-0076 seam): the one place that
//! shapes a session's advertised tools array per its pinned advertising mode,
//! encoding and discovery strategy (ADR-0196 §2-3, ADR-0204). Lifted out of
//! `main.rs` so every head and the integration tests exercise the exact
//! resolver production runs.
//!
//! This resolver is the **only** thing that shapes the surface — core
//! advertises its output verbatim, masks enforce at dispatch — so a spec
//! missing here is a spec no model ever sees (the ADR-0190 Bug-1 shape).

use std::sync::Arc;

use entanglement_core::{SessionId, SessionModel, ToolAdvertising, ToolSpec, ToolSpecResolver};

use super::{client_side_surface, AdvertisingInputs, Encoding, SharedAdvertisingState};
use crate::mcp::AvailableMcp;
use crate::tool_names::TOOL_SEARCH_KERNEL;
use crate::{discover, SharedRegistry};

/// Everything the resolver reads, all shared with the tool executor.
#[derive(Clone)]
pub struct SurfaceSources {
    pub tools: SharedRegistry,
    pub avail: Arc<AvailableMcp>,
    pub advertising: SharedAdvertisingState,
    /// What a session is pinned from at its first resolution.
    pub inputs: Arc<AdvertisingInputs>,
    /// The `agent`/`agent_send` specs, snapshotted once at startup.
    ///
    /// Deliberately a snapshot and not a live registry read: the advertised
    /// array must not vary between rounds or between agents (ADR-0207 §9), and
    /// a roster read per round would let a newly discovered agent file change
    /// it mid-session and invalidate the prompt-cache prefix. Snapshotting
    /// makes that impossible rather than merely discouraged.
    ///
    /// These ride here rather than `EngineConfig::tool_specs` because this
    /// resolver *replaces* that field — a spec pushed there never reaches the
    /// model when a resolver is installed, which is exactly how `propose_plan`
    /// and then `agent`/`agent_send` each went missing once.
    pub agent_specs: Vec<ToolSpec>,
}

/// Wrap [`resolve_surface`] as the engine's resolver.
pub fn tool_spec_resolver(sources: SurfaceSources) -> ToolSpecResolver {
    Arc::new(move |session: &SessionId, model: SessionModel<'_>| {
        resolve_surface(&sources, session, model)
    })
}

/// One round's advertised array for `session`.
pub fn resolve_surface(
    src: &SurfaceSources,
    session: &SessionId,
    model: SessionModel<'_>,
) -> Vec<ToolSpec> {
    // Pin before reading anything: round 1 must advertise what every later
    // round does (ADR-0204).
    src.advertising.ensure_pinned(&src.inputs, session, model);
    let registry = src.tools.read().expect("tool registry lock poisoned");
    // `read_raw` is graded/masked as an alias of `read` (ADR-0098), which only
    // holds if a profile author never sees it. A lazily-connected `allowed`
    // MCP server's tools (#542) stay scoped to the sessions that enabled them.
    let visible_specs: Vec<_> = registry
        .specs()
        .into_iter()
        .filter(|s| s.name != "read_raw" && src.avail.spec_visible(&s.name, session))
        .collect();
    let advertising = &src.advertising;
    let discovery = advertising.discovery(session);
    let mut runtime_specs = discover::runtime_owned_specs();
    runtime_specs.extend(src.agent_specs.iter().cloned());
    runtime_specs.push(discover::explore_spec(discovery));
    runtime_specs.push(discover::describe_spec(discovery));

    let discovered = advertising
        .discovered
        .lock()
        .expect("discovered-tool mutex poisoned")
        .names(session);
    match advertising.mode(session) {
        // ADR-0204 §5: the start snapshot, plus each later-delivered schema
        // appended once at the end. A tool added or removed afterwards never
        // moves or shrinks the array; a call to a removed one declines at
        // dispatch.
        ToolAdvertising::Full => {
            let mut snapshots = advertising
                .full_surfaces
                .lock()
                .expect("full-surface mutex poisoned");
            let snapshot = snapshots
                .entry(session.clone())
                .or_insert_with(|| full_surface(visible_specs, runtime_specs.clone()));
            for name in &discovered {
                if snapshot.iter().any(|s| s.name == *name) {
                    continue;
                }
                if let Some(spec) = registry
                    .spec_for(name)
                    .or_else(|| runtime_specs.iter().find(|s| s.name == *name).cloned())
                {
                    snapshot.push(spec);
                }
            }
            snapshot.clone()
        }
        ToolAdvertising::ToolSearch => {
            match advertising.encoding(session) {
                Encoding::ClientSide => {
                    let mut kernel_pool = visible_specs;
                    kernel_pool.extend(runtime_specs.iter().cloned());
                    // A `describe`d MCP tool's tail entry resolves through the
                    // unfiltered registry: append-only outranks the per-session
                    // MCP visibility gate once the model discovered it.
                    client_side_surface(kernel_pool, &discovered, discovery, |name| {
                        registry
                            .spec_for(name)
                            .or_else(|| runtime_specs.iter().find(|s| s.name == name).cloned())
                    })
                }
                // ADR-0196 §3: the full surface with every non-kernel tool
                // `defer_loading` for the whole session; discovery delivers
                // through the transcript, never by mutating this array.
                Encoding::AnthropicNative | Encoding::ResponsesNative => {
                    let mut specs = full_surface(visible_specs, runtime_specs);
                    mark_defer_loading(&mut specs);
                    specs
                }
            }
        }
    }
}

/// The full deduped, name-sorted tool surface (#566): every visible registry
/// spec plus the runtime-owned pseudo-tools, in one stable order independent
/// of registration order.
pub(crate) fn full_surface(
    visible_specs: Vec<ToolSpec>,
    runtime_specs: Vec<ToolSpec>,
) -> Vec<ToolSpec> {
    let mut specs = visible_specs;
    specs.extend(runtime_specs);
    // A runtime-owned pseudo-tool also present in the registry would appear
    // twice; sorting first makes duplicates adjacent.
    specs.sort_by(|a, b| a.name.cmp(&b.name));
    specs.dedup_by(|a, b| a.name == b.name);
    specs
}

/// Flag every spec `defer_loading` except the lean kernel. A *discovered*
/// tool stays deferred too: its definition already reached the model through
/// the `tool_reference` / `tool_search_output` block in the transcript, and
/// un-deferring it would insert bytes into the cached `tools` prefix once per
/// discovery. The kernel is always present, so at least one entry stays
/// non-deferred — Anthropic's hard requirement.
pub(crate) fn mark_defer_loading(specs: &mut [ToolSpec]) {
    for spec in specs.iter_mut() {
        spec.defer_loading = !TOOL_SEARCH_KERNEL.contains(&spec.name.as_str());
    }
}

#[cfg(test)]
#[path = "surface_tests.rs"]
mod tests;
