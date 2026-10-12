//! Provider-bundled MCP server catalog data (#542, ADR-0152).
//!
//! Split from [`crate::catalog`] along the 400-line file cap. These are pure
//! *data* types the catalog carries — a provider entry may bundle first-party
//! MCP servers (e.g. z.ai's `web_search_prime`/`web_reader`/`zread` in the
//! embedded `defaults.yml`), unlocked by the provider's `key_env`. The
//! runtime (`entanglement-runtime::mcp::available`) owns all interpretation:
//! key gating and the user-config field-merge. A bundled server is available
//! while its provider key resolves; `disabled: true` is the one opt-out
//! (ADR-0226 retired ADR-0152's three-state `state:`).

use std::collections::HashMap;

use serde::Deserialize;

/// One provider-bundled MCP server (#542). Mirrors the runtime's
/// `McpServerConfig` transport surface (`command` XOR `url` — validated by the
/// runtime, not here) plus the capability hint (#426), the `list_mcp`
/// description, and the `disabled` opt-out.
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
    /// One line `list_mcp` shows for the server (ADR-0226).
    #[serde(default)]
    pub description: Option<String>,
    /// The single explicit opt-out (ADR-0226): a disabled server is never
    /// listed or connected.
    #[serde(default)]
    pub disabled: bool,
}
