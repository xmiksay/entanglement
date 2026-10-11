//! [`LiveDefinitions`]: the runtime-held mirrors the watcher swaps. Split
//! out of `watch.rs` (#787) to keep it under the file cap.

use std::sync::{Arc, Mutex, RwLock};

use entanglement_core::AgentCatalog;

use crate::config::agent_models::AgentModelStore;
use crate::mcp;
use crate::policy::DefaultGrantStore;
use crate::skills::SkillRegistry;

/// What the watcher tells a head after a content change (#785). A head that
/// renders config of its own (the TUI's `tui:` block) re-applies it on
/// *either* variant: a malformed agent/skill file failing the definitions
/// reload must not also block an unrelated `config.yml` edit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReloadNotice {
    /// The definitions were swapped in; the one-line summary.
    Reloaded(String),
    /// The definitions reload failed and the previous state is kept; why.
    Failed(String),
}

/// The runtime-held mirrors a reload swaps. See the [`super`] module doc for
/// why this is deliberately *not* core's `EngineConfig.agents`.
#[derive(Clone)]
#[non_exhaustive]
pub struct LiveDefinitions {
    pub agents: Arc<RwLock<AgentCatalog>>,
    pub skills: Arc<RwLock<Arc<SkillRegistry>>>,
    /// User slash commands (#733) — read by the heads on every use, so a
    /// reload is live in the very next popup or `/name` invocation.
    pub commands: crate::slash_commands::LiveCommands,
    pub agent_models: Arc<Mutex<AgentModelStore>>,
    pub grants: Arc<DefaultGrantStore>,
    /// The MCP capability index (#426), captured once at startup like the
    /// config ceiling permissions — `config.yml`'s `mcp:` section is watched
    /// for the same *file content* fingerprint as everything else here, but
    /// (like the ceiling) is never itself re-parsed on a reload; only the
    /// agent/skill definition re-parse is live. A config edit to
    /// `capabilities:` needs a restart to take effect, same as a ceiling edit.
    pub mcp_capabilities: mcp::McpCapabilityIndex,
    /// Per-write file backups (#787): unlike the ceiling, `config.yml`'s
    /// `undo:` block *is* re-applied on every reload.
    pub file_backups: Option<crate::file_backup::FileBackups>,
}

impl LiveDefinitions {
    /// The mirrors every head needs. Optional extras join through `with_*`
    /// builders, so a new one never breaks an embedder's construction again
    /// (the struct is `#[non_exhaustive]` for the same reason).
    pub fn new(
        agents: Arc<RwLock<AgentCatalog>>,
        skills: Arc<RwLock<Arc<SkillRegistry>>>,
        commands: crate::slash_commands::LiveCommands,
        agent_models: Arc<Mutex<AgentModelStore>>,
        grants: Arc<DefaultGrantStore>,
        mcp_capabilities: mcp::McpCapabilityIndex,
    ) -> Self {
        Self {
            agents,
            skills,
            commands,
            agent_models,
            grants,
            mcp_capabilities,
            file_backups: None,
        }
    }

    /// Re-apply `config.yml`'s `undo:` block onto `backups` on each reload.
    pub fn with_file_backups(mut self, backups: crate::file_backup::FileBackups) -> Self {
        self.file_backups = Some(backups);
        self
    }
}
