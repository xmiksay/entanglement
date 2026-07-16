//! Persisted per-agent-profile generation-parameter overrides (#374, ADR-0094).
//!
//! Mirrors `agent_models` (#323, ADR-0081): picking generation knobs while a
//! profile is active persists them **for that profile** so they survive a
//! restart. This module owns the managed file behind that:
//! `${config_dir}/entanglement/agent-generation.yml` (override
//! `ENTANGLEMENT_AGENT_GENERATION_FILE`), a sibling of `agent-models.yml`, the
//! grants file (#174), and the provider-key env file (#220) — **managed, not
//! layered**: the runtime rewrites it freely, so it never mixes into the
//! hand-edited `config.yml`.
//!
//! Shape (a [`BTreeMap`] so the file is stable across rewrites):
//!
//! ```yaml
//! agents:
//!   build:
//!     temperature: 0.7
//!     max_output_tokens: 4096
//!     thinking_budget_tokens: null
//!     reasoning_effort: high
//! ```
//!
//! Unlike [`AgentModelStore`][super::agent_models::AgentModelStore], this store
//! does **not** overlay onto a loaded [`ProfileRegistry`][entanglement_core::ProfileRegistry]:
//! [`GenerationParams`] carries a non-`Eq` `f32` (`temperature`), so it can't
//! join [`AgentProfile`][entanglement_core::AgentProfile]'s `PartialEq + Eq`
//! derive the way the model pin's `provider`/`model` fields do. Instead
//! [`AgentGenerationStore::resolver`] wraps a shared handle to the store in a
//! [`GenerationResolver`] closure keyed by profile name — the seam
//! `EngineConfig::generation_resolver` exposes, resolved fresh on every lookup
//! (a `set`/`reload` is visible without rebuilding the closure). See
//! ADR-0094 for the full precedence chain (session memory > persisted store >
//! profile/catalog default) and why this deviates from the model pin's
//! registry-overlay shape.
//!
//! Missing/malformed file → empty + warn (fail-open: a corrupt file must never
//! wedge startup, and a dropped override only reverts a profile to its catalog
//! default). A write failure is logged, never fatal.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{bail, Result};
use entanglement_core::{GenerationParams, GenerationResolver};
use serde::{Deserialize, Serialize};

use super::atomic::atomic_write;

/// Env var overriding the managed agent-generation file path (tests + non-XDG
/// setups).
const AGENT_GENERATION_FILE_ENV: &str = "ENTANGLEMENT_AGENT_GENERATION_FILE";

/// On-disk shape of the managed agent-generation file. A single `agents:` map
/// keeps room for future keys and lets `deny_unknown_fields` flag typos.
#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct AgentGenerationFile {
    #[serde(default)]
    agents: BTreeMap<String, GenerationParams>,
}

/// The runtime's per-agent generation overrides: the persisted map plus the
/// file path to re-write when a new override is recorded.
#[derive(Debug, Default)]
pub struct AgentGenerationStore {
    agents: BTreeMap<String, GenerationParams>,
    path: Option<PathBuf>,
}

impl AgentGenerationStore {
    /// Load the persisted overrides from the managed file, resolving its path
    /// from `ENTANGLEMENT_AGENT_GENERATION_FILE` or
    /// `${config_dir}/entanglement/`. A missing file is an empty store; a
    /// malformed one is logged and treated as empty (fail-open — a corrupt file
    /// only reverts profiles to their catalog default).
    pub fn load() -> Self {
        let path = agent_generation_file_path();
        let agents = match &path {
            Some(p) => read_agent_generation(p),
            None => BTreeMap::new(),
        };
        Self { agents, path }
    }

    /// The persisted [`GenerationParams`] for `agent`, if any.
    pub fn get(&self, agent: &str) -> Option<GenerationParams> {
        self.agents.get(agent).copied()
    }

    /// Record `agent`'s override and re-write the managed file, merged against
    /// whatever is on disk under an exclusive lock (#329) — a concurrent skutter
    /// instance's own write, recorded between this store's `load()` and now,
    /// must survive rather than being clobbered by a write from stale in-memory
    /// state. Idempotent: re-setting an identical value still rewrites (a
    /// no-diff write), which keeps the call site simple. Returns the write error
    /// for the caller to log (never fatal).
    pub fn set(&mut self, agent: &str, params: GenerationParams) -> Result<()> {
        let Some(path) = self.path.clone() else {
            bail!(
                "no config directory for the managed agent-generation file; \
                 set {AGENT_GENERATION_FILE_ENV} to a path first"
            );
        };
        let agent_name = agent.to_string();
        let merged = super::lock::with_locked_file(&path, || {
            let mut on_disk = read_agent_generation(&path);
            on_disk.insert(agent_name.clone(), params);
            persist_map(&path, &on_disk)?;
            Ok(on_disk)
        })?;
        self.agents = merged;
        Ok(())
    }

    /// Re-read the persisted overrides from disk (#329) — picks up a write
    /// another skutter instance recorded.
    pub fn reload(&mut self) {
        if let Some(path) = &self.path {
            self.agents = read_agent_generation(path);
        }
    }

    /// Build a [`GenerationResolver`] closure over a shared handle to `store`
    /// (#374) — the seam `EngineConfig::generation_resolver` consumes. Wrapped
    /// in a `Mutex` (core's resolver `Fn` must be `Send + Sync`, but the store
    /// mutates on `set`/`reload`), and resolved fresh on every call so a live
    /// reload is visible without rebuilding the closure.
    pub fn resolver(store: Arc<Mutex<AgentGenerationStore>>) -> GenerationResolver {
        Arc::new(move |agent: &str| {
            store
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .get(agent)
        })
    }
}

/// Re-write the managed file at `path` from `agents`. The [`BTreeMap`] keeps the
/// output stable (readable diffs, no churn).
fn persist_map(path: &Path, agents: &BTreeMap<String, GenerationParams>) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let doc = AgentGenerationFile {
        agents: agents.clone(),
    };
    let body = serde_yaml::to_string(&doc)?;
    let header = "# entanglement — persisted per-agent generation-parameter overrides (#374).\n\
                  # Managed by skutter. Overrides that agent's catalog/profile generation\n\
                  # defaults (temperature/max-output/thinking-budget/reasoning-effort).\n\
                  # Delete an entry to revert to the default.\n";
    atomic_write(path, &format!("{header}{body}"))
}

/// Resolve the managed agent-generation file path:
/// `ENTANGLEMENT_AGENT_GENERATION_FILE` wins, otherwise
/// `${config_dir}/entanglement/agent-generation.yml`. `None` when neither is
/// available (persistence then silently no-ops).
fn agent_generation_file_path() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os(AGENT_GENERATION_FILE_ENV) {
        return Some(PathBuf::from(p));
    }
    dirs::config_dir().map(|d| d.join("entanglement").join("agent-generation.yml"))
}

/// Read + parse the agent-generation file at `path`. A missing file, or any
/// read/parse error, yields an empty map (logged) — fail-open, since a dropped
/// override only reverts a profile to its catalog default.
fn read_agent_generation(path: &Path) -> BTreeMap<String, GenerationParams> {
    if !path.exists() {
        return BTreeMap::new();
    }
    let parsed = std::fs::read_to_string(path)
        .map_err(|e| format!("{e}"))
        .and_then(|t| serde_yaml::from_str::<AgentGenerationFile>(&t).map_err(|e| format!("{e}")));
    match parsed {
        Ok(file) => file.agents,
        Err(e) => {
            tracing::warn!(
                "ignoring malformed agent-generation file {}: {e}",
                path.display()
            );
            BTreeMap::new()
        }
    }
}
