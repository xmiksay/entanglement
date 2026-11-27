//! `skutter`'s MCP layer over the library's (re-exported here): the global
//! server roster answering `McpList`/`McpAdd`/`McpRemove`/`McpAuth`, live
//! add/remove and the OAuth browser/device flows (#375, ADR-0153, #932).
//! The library keeps the transports, the lazy connect, the per-user scopes
//! and the `TokenStore` seam.

pub use entanglement_runtime::mcp::*;

pub mod browser;
pub mod live;
pub mod oauth_ops;
pub mod responder;

pub use live::{mcp_add, mcp_disconnect_only, mcp_reconnect, mcp_remove, ServerConfigs};
pub use responder::spawn_mcp_responder;

/// The handles a head reads MCP state through — the TUI `/tools` view lists
/// the roster and connected tools from these.
#[derive(Clone)]
pub struct McpHandles {
    pub avail: std::sync::Arc<AvailableMcp>,
    pub registry: crate::tools::SharedRegistry,
    pub active: ActiveServers,
}
