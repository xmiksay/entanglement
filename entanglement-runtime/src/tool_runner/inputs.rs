//! The caller-supplied policy inputs of
//! [`super::spawn_tool_executor_with_policy`] (issue #712, split out of
//! `tool_runner.rs`): the escape-root policy and the discovery surface.
//! Re-exported at their pre-split paths (`tool_runner::EscapeRoot`/
//! `DiscoverySurface`).

use std::sync::Arc;

use crate::arg_validate;
use crate::mcp::{ActiveServers, AvailableMcp};
use crate::tool_advertising::SharedAdvertisingState;

/// Escape-root policy for the executor (ADR-0109): the canonical project `root`
/// against which an out-of-root `read`/`edit`/`write` path or `bash`/`call`
/// `workdir` is detected, plus the shared [`ExtraRootStore`] approvals are
/// recorded into and the host tools read. `None` (the convenience wrappers, all tests)
/// keeps strict containment — an out-of-root path is a hard error, never a
/// prompt.
///
#[derive(Clone)]
pub struct EscapeRoot {
    pub root: std::path::PathBuf,
    pub store: Arc<crate::extra_roots::ExtraRootStore>,
}

impl EscapeRoot {
    /// The absolute out-of-root path a call to `tool` with `input` would touch,
    /// or `None` when it stays contained (or the tool has no path argument).
    /// `pub(crate)`: also called from [`crate::script`], whose bindings route
    /// through this same gate (#446).
    pub(crate) fn escaping(&self, tool: &str, input: &str) -> Option<std::path::PathBuf> {
        let rel = crate::permission::escape_root_target(tool, input)?;
        crate::host::escaping_path(&self.root, &rel)
    }
}

/// `explore`/`describe`'s shared inputs (#560, ADR-0196 §4), bundled into one
/// struct — see [`spawn_tool_executor_with_policy`]'s `discovery` param.
/// `Default` gives every field's own empty/private state: an advertising
/// state no external resolver shares, an empty MCP roster, and a private
/// loop-breaker tracker.
#[derive(Default)]
pub struct DiscoverySurface {
    pub advertising: SharedAdvertisingState,
    pub mcp_avail: Arc<AvailableMcp>,
    pub mcp_active: ActiveServers,
    /// The pre-dispatch argument-validation loop-breaker guard (#560,
    /// ADR-0196 §6) — bundled here rather than as a fourth top-level param
    /// since it's session-keyed state alongside `advertising`, read/written
    /// by the same `dispatch`/`run_and_reply` call sites.
    pub validation: Arc<arg_validate::LoopBreaker>,
    /// The shared endpoint-pool `HttpClient` a dispatch-time lazy MCP
    /// re-enable rides (ADR-0201, `mcp::available::try_lazy_reenable`) —
    /// `None` (the convenience wrappers, every test-only caller) degrades a
    /// would-be re-enable to a clean tool error rather than a panic; those
    /// callers' `mcp_avail` is also the empty default, so the path is never
    /// actually exercised.
    pub http: Option<entanglement_core::HttpClient>,
}
