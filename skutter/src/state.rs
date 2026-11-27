//! The managed state files `skutter` writes on the user's behalf behind the
//! library's in-memory seams (#966): "always allow" grants
//! ([`FileGrantStore`], a `GrantStore`), MCP OAuth credentials
//! ([`McpTokenStore`], a `TokenStore`), the last-seen MCP tool names
//! ([`FileMcpToolCache`], an `McpToolCache`) — all in the managed-state dir
//! (`ENTANGLEMENT_STATE_DIR`) — and the live `/mcp add|remove` write-back to
//! `config.yml`'s `mcp:` section ([`spawn_mcp_config_writer`]).

pub mod grants;
pub mod mcp_config;
pub mod mcp_tokens;
pub mod mcp_tools;
mod stores;

pub use grants::FileGrantStore;
pub use mcp_config::{save_mcp, spawn_mcp_config_writer};
pub use mcp_tokens::McpTokenStore;
pub use mcp_tools::FileMcpToolCache;
pub use stores::Stores;
