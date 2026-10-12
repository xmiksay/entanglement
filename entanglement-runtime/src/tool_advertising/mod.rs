//! The one advertised surface (ADR-0226) and the per-session state around it.
//!
//! There is no advertising *mode* any more: every session on every wire gets
//! the same [`KERNEL`][crate::tool_names::KERNEL] — native host tools, the
//! six discovery tools, `rhai`, and the `mcp` envelope (`{server, tool,
//! args}`) — plus, until #819/#820 remove them, the registered
//! `endpoint__*`/`skill__*` tools — snapshotted at its first resolution and
//! never changed afterwards, so the
//! provider's cached `tools` prefix holds for the whole session. MCP tool
//! schemas are never advertised; `mcp_tools` delivers them as a tool result.
//!
//! [`AdvertisingState`] is the one `Arc` the tool-spec resolver, the
//! system-prompt resolver and the tool executor share: the surface snapshots,
//! the delivered-schema dedup set, and the env-date pin (ADR-0202 §5).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use entanglement_core::{SessionId, ToolSpec};

mod delivered;
pub use delivered::DeliveredSchemas;

mod envelope;
pub use envelope::{mcp_example, unknown_tool_reply};

pub mod surface;

/// Per-session advertising state, shared by the resolvers and the executor.
#[derive(Debug, Default)]
pub struct AdvertisingState {
    /// Schemas already put in front of the model this session — by an
    /// `mcp_tools` read or an arg-validate decline — so a repeat decline
    /// does not resend them (ADR-0196 §6).
    pub delivered: Mutex<DeliveredSchemas>,
    pub env_dates: Mutex<crate::env_date::EnvDatePins>,
    /// Each session's advertised array, snapshotted at its first resolution:
    /// a tool registered or removed later never moves the cached prefix.
    pub surfaces: Mutex<HashMap<SessionId, Vec<ToolSpec>>>,
}

impl AdvertisingState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Release every per-session record on end/hibernate. A resumed session
    /// re-snapshots at its first round — the same bytes, since the kernel is
    /// compiled in.
    pub fn forget(&self, session: &SessionId) {
        self.delivered
            .lock()
            .expect("delivered-schema mutex poisoned")
            .forget(session);
        self.env_dates
            .lock()
            .expect("env-date pin mutex poisoned")
            .forget(session);
        self.surfaces
            .lock()
            .expect("surface mutex poisoned")
            .remove(session);
    }

    /// The names `session` was advertised, `None` before its first round —
    /// what the TUI `/tools` view marks as advertised.
    pub fn advertised_names(&self, session: &SessionId) -> Option<Vec<String>> {
        self.surfaces
            .lock()
            .expect("surface mutex poisoned")
            .get(session)
            .map(|specs| specs.iter().map(|s| s.name.clone()).collect())
    }
}

/// Shared handle every consumer of [`AdvertisingState`] holds.
pub type SharedAdvertisingState = Arc<AdvertisingState>;
