//! Persisted per-purpose provider/model pins for auxiliary LLM calls
//! (Issue 5, the tui-ux-batch plan).
//!
//! The main turn loop uses the session's primary model — but a user may want a
//! cheaper/faster model for side transformations like the compaction summary or
//! an auto-generated session title. This module owns the managed file behind
//! that:
//! `<state dir>/aux-models.yml` (relocated by
//! `ENTANGLEMENT_STATE_DIR`, ADR-0213), a sibling of `agent-models.yml` (#323,
//! ADR-0081), the grants file (#174), and the provider-key env file (#220) —
//! **managed, not layered**: skutter rewrites it freely, so it never mixes
//! into the hand-edited `config.yml`.
//!
//! Shape (a [`BTreeMap`] so the file is stable across rewrites):
//!
//! ```yaml
//! aux:
//!   summarize:
//!     provider: zai
//!     model: glm-4.5-flash
//!   session_title:
//!     provider: ollama
//!     model: llama3.1
//! ```
//!
//! The pins themselves are the library's in-memory
//! [`AuxModelStore`]; a pin is consulted at call time by its `AuxLlmRegistry`,
//! which falls back to the session's primary model when a purpose is unset (or
//! when the pin's provider/model is unknown to the catalog). Missing/malformed file
//! → empty + warn (fail-open: a corrupt file must never wedge a side
//! transformation, and a dropped pin only reverts a purpose to the primary
//! model). A write failure is logged, never fatal.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{bail, Result};
pub use entanglement_runtime::aux_llm::{AuxModelStore, Purpose};
use serde::{Deserialize, Serialize};

use super::atomic::atomic_write;
use super::state_dir::{state_file, AUX_MODELS_FILE, STATE_DIR_ENV};

/// One purpose's persisted pin: the provider + model chosen for it via
/// `/aux-model`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct AuxModelPin {
    provider: String,
    model: String,
}

/// On-disk shape of the managed aux-models file. A single `aux:` map keeps
/// room for future keys and lets `deny_unknown_fields` flag typos.
#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct AuxModelsFile {
    #[serde(default)]
    aux: BTreeMap<String, AuxModelPin>,
}

/// The managed `aux-models.yml` behind a shared [`AuxModelStore`].
#[derive(Debug, Clone)]
pub struct AuxModelsPath(Option<PathBuf>);

impl AuxModelsPath {
    /// The file in the managed-state dir (`ENTANGLEMENT_STATE_DIR`,
    /// ADR-0213); `None` inside when no state dir resolves.
    pub fn managed() -> Self {
        Self(state_file(AUX_MODELS_FILE))
    }

    /// An explicit file — or, with `None`, no persistence at all. The
    /// injection seam (#747): a test pins a temp file instead of steering
    /// process-global env.
    pub fn at(path: Option<PathBuf>) -> Self {
        Self(path)
    }

    /// The persisted pins. A missing file is an empty store; a malformed one
    /// is logged and treated as empty (fail-open — a corrupt file only
    /// reverts a purpose to the primary model).
    pub fn load(&self) -> AuxModelStore {
        AuxModelStore::in_memory(match &self.0 {
            Some(p) => read_aux_models(p),
            None => BTreeMap::new(),
        })
    }

    /// Record `purpose`'s pin in the managed file, merged against whatever is
    /// on disk under an exclusive lock (#329) — a concurrent skutter
    /// instance's own pin, recorded since the live store was loaded, must
    /// survive rather than being clobbered by a write from stale in-memory
    /// state — and return the merged pins for the caller to swap into its
    /// live store. Takes no store, so the file I/O never runs under the
    /// store's lock (an aux resolve waits on that). Returns the write error
    /// for the caller to log (never fatal).
    pub fn record(&self, purpose: Purpose, provider: &str, model: &str) -> Result<AuxModelStore> {
        let Some(path) = &self.0 else {
            bail!(
                "no config directory for the managed aux-models file; \
                 set {STATE_DIR_ENV} to a directory first"
            );
        };
        let merged = super::lock::with_locked_file(path, || {
            let mut on_disk = read_aux_models(path);
            on_disk.insert(purpose, (provider.to_string(), model.to_string()));
            persist_map(path, &on_disk)?;
            Ok(on_disk)
        })?;
        Ok(AuxModelStore::in_memory(merged))
    }
}

/// Re-write the managed file at `path` from `aux`. The [`BTreeMap`] keeps the
/// output stable (readable diffs, no churn). The enum keys are serialized as
/// their lowercase snake-case string form so the persisted YAML matches what a
/// user would hand-write.
fn persist_map(path: &Path, aux: &BTreeMap<Purpose, (String, String)>) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let keyed: BTreeMap<String, AuxModelPin> = aux
        .iter()
        .map(|(p, (provider, model))| {
            let pin = AuxModelPin {
                provider: provider.clone(),
                model: model.clone(),
            };
            (p.as_str().to_string(), pin)
        })
        .collect();
    let doc = AuxModelsFile { aux: keyed };
    let body = serde_yaml::to_string(&doc)?;
    let header = "# entanglement — persisted per-purpose provider/model pins for auxiliary LLM\n\
                  # calls (Issue 5). Managed by skutter: a pin is recorded when you run\n\
                  # /aux-model <purpose> <provider>/<model>. A purpose with no pin (or a pin\n\
                  # whose provider/model the catalog no longer knows) falls back to the\n\
                  # session's primary model. Delete an entry to revert to the fallback.\n";
    atomic_write(path, &format!("{header}{body}"))
}

/// Read + parse the aux-models file at `path`. A missing file, or any
/// read/parse error, yields an empty map (logged) — fail-open, since a dropped
/// pin only reverts a purpose to the primary model. An unrecognized purpose key
/// in the file is skipped (logged at debug) rather than failing the whole file,
/// matching how an unknown agent-name pin is treated in `agent-models.yml`.
fn read_aux_models(path: &Path) -> BTreeMap<Purpose, (String, String)> {
    if !path.exists() {
        return BTreeMap::new();
    }
    let parsed = std::fs::read_to_string(path)
        .map_err(|e| format!("{e}"))
        .and_then(|t| serde_yaml::from_str::<AuxModelsFile>(&t).map_err(|e| format!("{e}")));
    match parsed {
        Ok(file) => file
            .aux
            .into_iter()
            .filter_map(|(k, v)| match Purpose::parse(&k) {
                Some(p) => Some((p, (v.provider, v.model))),
                None => {
                    tracing::debug!(
                        key = %k,
                        "aux-models: ignoring unrecognized purpose key; not in known set"
                    );
                    None
                }
            })
            .collect(),
        Err(e) => {
            tracing::warn!("ignoring malformed aux-models file {}: {e}", path.display());
            BTreeMap::new()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_shape_roundtrips_through_serde() {
        // The on-disk shape `aux: { summarize: { provider, model }, ... }`
        // round-trips, with `deny_unknown_fields` catching a typo.
        let yaml = "aux:\n  summarize:\n    provider: zai\n    model: glm-4.5-flash\n  session_title:\n    provider: ollama\n    model: llama3.1\n";
        let file: AuxModelsFile = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(file.aux.len(), 2);
        assert_eq!(file.aux["summarize"].provider, "zai");
        assert_eq!(file.aux["session_title"].model, "llama3.1");
        // A typo'd field is rejected.
        let err = serde_yaml::from_str::<AuxModelsFile>(
            "aux:\n  summarize:\n    provider: zai\n    typo: 1\n",
        )
        .unwrap_err();
        assert!(err.to_string().contains("typo"), "got: {err}");
        // A typo'd top-level key is rejected too.
        let err = serde_yaml::from_str::<AuxModelsFile>("auxx: {}\n").unwrap_err();
        assert!(err.to_string().contains("auxx"), "got: {err}");
    }
}
