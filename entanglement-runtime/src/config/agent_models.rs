//! Persisted per-agent-profile provider/model pins (#323, ADR-0081).
//!
//! Picking a model via the TUI `/model` picker while a profile is active persists
//! that `(provider, model)` **for that profile** so it survives a restart. This
//! module owns the managed file behind that:
//! `${config_dir}/entanglement/agent-models.yml` (override
//! `ENTANGLEMENT_AGENT_MODELS_FILE`), a sibling of the grants file (#174) and the
//! provider-key env file (#220) — **managed, not layered**: the runtime rewrites
//! it freely, so it never mixes into the hand-edited `config.yml`.
//!
//! Shape (a [`BTreeMap`] so the file is stable across rewrites):
//!
//! ```yaml
//! agents:
//!   build:
//!     provider: zai
//!     model: glm-5.2
//! ```
//!
//! [`AgentModelStore::apply`] overlays these pins onto the loaded
//! [`ProfileRegistry`] at startup, so a persisted pin wins over a profile's
//! frontmatter `provider`/`model` — precedence *persisted file > frontmatter*.
//! Missing/malformed file → empty + warn (fail-open: a corrupt file must never
//! wedge startup, and a dropped pin only reverts a profile to its frontmatter
//! default). A write failure is logged, never fatal.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{bail, Result};
use entanglement_core::ProfileRegistry;
use serde::{Deserialize, Serialize};

use super::atomic::atomic_write;

/// Env var overriding the managed agent-models file path (tests + non-XDG setups).
const AGENT_MODELS_FILE_ENV: &str = "ENTANGLEMENT_AGENT_MODELS_FILE";

/// One profile's persisted pin: the provider + model chosen for it via `/model`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct AgentModelPin {
    provider: String,
    model: String,
}

/// On-disk shape of the managed agent-models file. A single `agents:` map keeps
/// room for future keys and lets `deny_unknown_fields` flag typos.
#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct AgentModelsFile {
    #[serde(default)]
    agents: BTreeMap<String, AgentModelPin>,
}

/// The runtime's per-agent model pins: the persisted map plus the file path to
/// re-write when a new pin is recorded.
#[derive(Debug, Default)]
pub struct AgentModelStore {
    agents: BTreeMap<String, AgentModelPin>,
    path: Option<PathBuf>,
}

impl AgentModelStore {
    /// Load the persisted pins from the managed file, resolving its path from
    /// `ENTANGLEMENT_AGENT_MODELS_FILE` or `${config_dir}/entanglement/`. A missing
    /// file is an empty store; a malformed one is logged and treated as empty
    /// (fail-open — a corrupt file only reverts profiles to their frontmatter).
    pub fn load() -> Self {
        let path = agent_models_file_path();
        let agents = match &path {
            Some(p) => read_agent_models(p),
            None => BTreeMap::new(),
        };
        Self { agents, path }
    }

    /// The persisted `(provider, model)` pin for `agent`, if any.
    pub fn get(&self, agent: &str) -> Option<(&str, &str)> {
        self.agents
            .get(agent)
            .map(|p| (p.provider.as_str(), p.model.as_str()))
    }

    /// Record `agent`'s pin and re-write the managed file, merged against
    /// whatever is on disk under an exclusive lock (#329) — a concurrent skutter
    /// instance's own pin, recorded between this store's `load()` and now, must
    /// survive rather than being clobbered by a write from stale in-memory
    /// state. Idempotent: re-setting an identical pin still rewrites (a no-diff
    /// write), which keeps the call site simple. Returns the write error for the
    /// caller to log (never fatal).
    pub fn set(&mut self, agent: &str, provider: &str, model: &str) -> Result<()> {
        let pin = AgentModelPin {
            provider: provider.to_string(),
            model: model.to_string(),
        };
        let Some(path) = self.path.clone() else {
            bail!(
                "no config directory for the managed agent-models file; \
                 set {AGENT_MODELS_FILE_ENV} to a path first"
            );
        };
        let agent_name = agent.to_string();
        let merged = super::lock::with_locked_file(&path, || {
            let mut on_disk = read_agent_models(&path);
            on_disk.insert(agent_name.clone(), pin.clone());
            persist_map(&path, &on_disk)?;
            Ok(on_disk)
        })?;
        self.agents = merged;
        Ok(())
    }

    /// Re-read the persisted pins from disk (#329) — picks up a pin another
    /// skutter instance recorded via `/model`.
    pub fn reload(&mut self) {
        if let Some(path) = &self.path {
            self.agents = read_agent_models(path);
        }
    }

    /// Overlay the persisted pins onto `registry` (#323): for each stored agent
    /// with a matching profile, set its `provider` + `model` so it forms a
    /// [`model_pin`][entanglement_core::AgentProfile::model_pin] — persisted file
    /// wins over frontmatter. A pin for an unknown profile is ignored (logged).
    pub fn apply(&self, registry: &mut ProfileRegistry) {
        for (name, pin) in &self.agents {
            match registry.get(name) {
                Some(profile) => {
                    let mut profile = profile.clone();
                    profile.provider = Some(pin.provider.clone());
                    profile.model = Some(pin.model.clone());
                    registry.insert(profile);
                }
                None => tracing::debug!(
                    agent = %name,
                    "agent-models: no matching profile for persisted pin; ignoring"
                ),
            }
        }
    }
}

/// Re-write the managed file at `path` from `agents`. The [`BTreeMap`] keeps the
/// output stable (readable diffs, no churn). Factored out of the old `persist`
/// method (#329) so both [`AgentModelStore::set`]'s locked read-modify-write
/// closure and any future caller can write the file without needing `&self`.
fn persist_map(path: &Path, agents: &BTreeMap<String, AgentModelPin>) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let doc = AgentModelsFile {
        agents: agents.clone(),
    };
    let body = serde_yaml::to_string(&doc)?;
    let header = "# entanglement — persisted per-agent provider/model pins (#323).\n\
                  # Managed by skutter: a pin is recorded when you pick a model via the TUI\n\
                  # /model picker while an agent is active. It overrides that agent's\n\
                  # frontmatter provider/model. Delete an entry to revert to the default.\n";
    atomic_write(path, &format!("{header}{body}"))
}

/// Resolve the managed agent-models file path: `ENTANGLEMENT_AGENT_MODELS_FILE`
/// wins, otherwise `${config_dir}/entanglement/agent-models.yml`. `None` when
/// neither is available (persistence then silently no-ops).
fn agent_models_file_path() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os(AGENT_MODELS_FILE_ENV) {
        return Some(PathBuf::from(p));
    }
    dirs::config_dir().map(|d| d.join("entanglement").join("agent-models.yml"))
}

/// Read + parse the agent-models file at `path`. A missing file, or any
/// read/parse error, yields an empty map (logged) — fail-open, since a dropped
/// pin only reverts a profile to its frontmatter default.
fn read_agent_models(path: &Path) -> BTreeMap<String, AgentModelPin> {
    if !path.exists() {
        return BTreeMap::new();
    }
    let parsed = std::fs::read_to_string(path)
        .map_err(|e| format!("{e}"))
        .and_then(|t| serde_yaml::from_str::<AgentModelsFile>(&t).map_err(|e| format!("{e}")));
    match parsed {
        Ok(file) => file.agents,
        Err(e) => {
            tracing::warn!(
                "ignoring malformed agent-models file {}: {e}",
                path.display()
            );
            BTreeMap::new()
        }
    }
}
