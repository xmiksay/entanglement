//! The caller-supplied discovery inputs of [`super::ToolExecutor::builder`]
//! (issue #712, split out of `tool_runner.rs`), re-exported at
//! `tool_runner::DiscoverySurface`.

use std::sync::Arc;

use crate::arg_validate;
use crate::mcp::{ActiveServers, AvailableMcp};
use crate::tool_advertising::SharedAdvertisingState;

/// The discovery tools' shared inputs (ADR-0226), bundled into one
/// struct — see [`super::ToolExecutorBuilder::discovery`].
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
    /// The shared endpoint-pool `HttpClient` every lazy MCP connect rides
    /// (`mcp_tools`, or a dispatch, `mcp::available::try_lazy_connect`) —
    /// `None` (the builder default, most tests) degrades a would-be connect
    /// to a clean tool error rather than a panic.
    pub http: Option<entanglement_core::HttpClient>,
}
