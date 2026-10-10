//! Live MCP server management wire shapes (#375, ADR-0153).

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

#[cfg(doc)]
use crate::protocol::{InMsg, OutEvent};

/// Wire shape of one MCP server's spawn/connect config, carried by
/// [`InMsg::McpAdd`]. Mirrors `entanglement_runtime::mcp::McpServerConfig`
/// field-for-field, but lives here as a passive DTO: core holds no MCP logic
/// (ADR-0067) — the `command` XOR `url` transport choice is validated
/// runtime-side, on connect.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct McpServerSpec {
    #[serde(default)]
    pub command: Option<String>,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: HashMap<String, String>,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub headers: HashMap<String, String>,
    #[serde(default)]
    pub disabled: bool,
}

/// One server's live status, as reported in an [`OutEvent::McpList`] snapshot
/// (#375). `connected`/`tools` reflect the runtime's `ActiveServers` map;
/// `error` is set for a server that failed to connect (empty `tools`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct McpServerStatus {
    pub name: String,
    /// `"stdio"` or `"http"`.
    pub transport: String,
    pub connected: bool,
    pub tools: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Three-state activation (#542): `"enabled"` (startup-connected) or
    /// `"allowed"` (available for per-session enablement — possibly already
    /// lazily connected, see `connected`). `None` from a pre-#542 peer; the
    /// runtime MCP responder always fills it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<String>,
    /// OAuth posture (ADR-0153): `"authenticated"` (a credential is stored) or
    /// `"needs auth"` (the server declares `oauth:` but has none yet, so it was
    /// skipped at startup — run `/mcp connect <name>`). `None` for a server that
    /// does not use OAuth at all, and from a pre-ADR-0153 peer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<String>,
}

/// What changed in reply to [`InMsg::McpAdd`]/[`InMsg::McpRemove`] (#375),
/// carried by [`OutEvent::McpChanged`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum McpAction {
    Added,
    Removed,
}

/// Which OAuth operation [`InMsg::McpAuth`] requests (ADR-0153).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum McpAuthAction {
    /// Run the full authorization-code flow — discovery, dynamic client
    /// registration if needed, browser consent, token exchange — then persist
    /// the credential and (re)connect the server.
    Connect,
    /// The RFC 8628 device-authorization grant (#631) — the alternative entry
    /// point for a host with no browser *and* no reachable loopback port:
    /// discovery, dynamic client registration, and persistence are identical to
    /// [`Connect`][McpAuthAction::Connect]; only "how does the human authorize"
    /// differs. Reports a verification URL + short code instead of parking on a
    /// browser redirect.
    ConnectDeviceCode,
    /// Report whether the stored credential would work right now, refreshing it
    /// if it is merely expired. Never opens a browser.
    Check,
    /// Revoke the credential upstream when the server supports it, then delete
    /// it locally and disconnect.
    Disconnect,
}

/// The result of an [`InMsg::McpAuth`] operation, carried by
/// [`OutEvent::McpAuthChanged`] (ADR-0153).
///
/// `state` is the provider's own outcome label (`authorized`, `already valid`,
/// `refreshed`, `disconnected`, `not authorized`); `error` is set instead when
/// the operation failed, so a head can render one line either way without
/// knowing which action ran.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct McpAuthStatus {
    pub name: String,
    pub action: McpAuthAction,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// The URL the user must visit, emitted as an interim event while a
    /// [`Connect`][McpAuthAction::Connect] waits on the browser. A head prints
    /// it so a headless/SSH session can copy it even when the browser launch
    /// could not happen.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub authorize_url: Option<String>,
    /// The short code to enter at `authorize_url`, present only on
    /// [`ConnectDeviceCode`][McpAuthAction::ConnectDeviceCode]'s interim event
    /// (RFC 8628) — the browser-redirect flow needs no code, so this is always
    /// `None` for [`Connect`][McpAuthAction::Connect].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_code: Option<String>,
}
