//! Provider-bundled MCP server catalog data (#542, ADR-0152).
//!
//! Split from [`crate::catalog`] along the 400-line file cap. These are pure
//! *data* types the catalog carries — a provider entry may bundle first-party
//! MCP servers (e.g. z.ai's `web_search_prime`/`web_reader`/`zread` in the
//! embedded `defaults.yml`), unlocked by the provider's `key_env`. The
//! runtime (`entanglement-runtime::mcp::available`) owns all interpretation:
//! key gating, the user-config field-merge, and the three-state activation.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

/// Activation state of an MCP server (#542): `enabled` connects at startup and
/// advertises to every session; `allowed` is *available* — not connected until
/// a user (`/enable mcp <name>`) or the agent (the `mcp_enable` tool) enables
/// it for a session; `disabled` is invisible. Declared here (not the runtime)
/// because the catalog's bundled entries carry it as data; the runtime owns
/// all interpretation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum McpServerState {
    Enabled,
    Allowed,
    Disabled,
}

/// One provider-bundled MCP server (#542). Mirrors the runtime's
/// `McpServerConfig` transport surface (`command` XOR `url` — validated by the
/// runtime, not here) plus the capability hint (#426) and the default
/// activation [`McpServerState`] (`None` ⇒ `allowed`: available but not
/// connected until enabled).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderMcpServer {
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub command: Option<String>,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: HashMap<String, String>,
    /// Static request headers; values may reference `${VAR}` — typically the
    /// provider's own `key_env` (`Authorization: "Bearer ${ZAI_API_KEY}"`).
    #[serde(default)]
    pub headers: HashMap<String, String>,
    /// Capability hint (#426): raw tool name → `read`/`write`/`call`.
    #[serde(default)]
    pub capabilities: HashMap<String, String>,
    #[serde(default)]
    pub state: Option<McpServerState>,
}
