//! [`LiveDefinitions`]: the head-held mirrors the watcher swaps, and the
//! [`reload`] that swaps them. Split out of `watch.rs` (#787) to keep it
//! under the file cap.

use std::path::Path;
use std::sync::{Arc, Mutex, RwLock};

use entanglement_core::AgentCatalog;

use crate::config::agent_models::AgentModelStore;
use crate::mcp;
use crate::session_store::SessionStore;
use crate::skills::SkillRegistry;
use crate::system_prompt::PromptContext;

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

/// The head-held mirrors a reload swaps. See the [`super`] module doc for
/// why this is deliberately *not* core's `EngineConfig.agents`.
#[derive(Clone)]
#[non_exhaustive]
pub struct LiveDefinitions {
    pub agents: Arc<RwLock<AgentCatalog>>,
    pub skills: Arc<RwLock<Arc<SkillRegistry>>>,
    pub agent_models: Arc<Mutex<AgentModelStore>>,
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
    /// The session store whose scratch dir a reload's prompt context names;
    /// `None` resolves the default one for the project root. Injected so a
    /// test never creates a scratch dir under the real data dir (#747).
    pub sessions: Option<SessionStore>,
}

impl LiveDefinitions {
    /// The mirrors every head needs. Optional extras join through `with_*`
    /// builders, so a new one never breaks an embedder's construction again
    /// (the struct is `#[non_exhaustive]` for the same reason).
    pub fn new(
        agents: Arc<RwLock<AgentCatalog>>,
        skills: Arc<RwLock<Arc<SkillRegistry>>>,
        agent_models: Arc<Mutex<AgentModelStore>>,
        mcp_capabilities: mcp::McpCapabilityIndex,
    ) -> Self {
        Self {
            agents,
            skills,
            agent_models,
            mcp_capabilities,
            file_backups: None,
            sessions: None,
        }
    }

    /// Re-apply `config.yml`'s `undo:` block onto `backups` on each reload.
    pub fn with_file_backups(mut self, backups: crate::file_backup::FileBackups) -> Self {
        self.file_backups = Some(backups);
        self
    }

    /// Name `sessions`' scratch dir in a reload's prompt context.
    pub fn with_sessions(mut self, sessions: SessionStore) -> Self {
        self.sessions = Some(sessions);
        self
    }
}

/// Re-run the skill + agent-profile loaders, reload the agent-models store
/// from disk, and swap the result into `live` — but only if every step
/// succeeds. A malformed edit mid-save must not crash a long-running watcher
/// or wipe out a previously-good in-memory registry (unlike startup, which
/// fails fast) — log and keep serving the last-known-good state instead.
/// Returns the one-line notice on success. Only called once `spawn_watcher`'s
/// fingerprint check has already confirmed something on disk actually
/// changed, so every call here is expected to produce a real notice.
pub(super) fn reload(cwd: &Path, live: &LiveDefinitions) -> anyhow::Result<String> {
    let new_skills = crate::skills::load_registry_in(cwd, &crate::user_dirs::skills_layer())?;
    let prompt_ctx = match &live.sessions {
        Some(sessions) => PromptContext::load_with(cwd, sessions.scratch_dir().ok()),
        None => {
            PromptContext::load_with(cwd, crate::user_dirs::session_store(cwd).scratch_dir().ok())
        }
    };
    let mut new_profiles = crate::agents::load_registry_in(
        cwd,
        &crate::user_dirs::agents_layer(),
        &prompt_ctx,
        &new_skills,
        &live.mcp_capabilities,
    )?;

    // Managed-file mirrors (#329): re-read whatever another skutter instance
    // may have written since this process's last load, then re-apply pins onto
    // the freshly-loaded profiles (persisted file > frontmatter, ADR-0081).
    {
        let mut agent_models = live.agent_models.lock().unwrap();
        agent_models.reload();
        agent_models.apply(&mut new_profiles);
    }

    let agent_count = new_profiles.iter().count();
    let skill_count = new_skills.disclosures().len();
    *live.skills.write().unwrap() = Arc::new(new_skills);
    *live.agents.write().unwrap() = new_profiles;

    Ok(format!(
        "definitions reloaded: {agent_count} agent(s), {skill_count} skill(s) \
         — new sessions see the update"
    ))
}
