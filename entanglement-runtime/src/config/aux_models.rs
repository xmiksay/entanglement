//! Persisted per-purpose provider/model pins for auxiliary LLM calls
//! (Issue 5, the tui-ux-batch plan).
//!
//! The main turn loop uses the session's primary model — but a user may want a
//! cheaper/faster model for side transformations like the compaction summary or
//! an auto-generated session title. This module owns the managed file behind
//! that:
//! `${config_dir}/entanglement/aux-models.yml` (override
//! `ENTANGLEMENT_AUX_MODELS_FILE`), a sibling of `agent-models.yml` (#323,
//! ADR-0081), the grants file (#174), and the provider-key env file (#220) —
//! **managed, not layered**: the runtime rewrites it freely, so it never mixes
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
//! A pin is consulted at call time by the runtime's [`AuxLlmRegistry`], which
//! falls back to the session's primary model when a purpose is unset (or when
//! the pin's provider/model is unknown to the catalog). Missing/malformed file
//! → empty + warn (fail-open: a corrupt file must never wedge a side
//! transformation, and a dropped pin only reverts a purpose to the primary
//! model). A write failure is logged, never fatal.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};

use super::atomic::atomic_write;

/// Env var overriding the managed aux-models file path (tests + non-XDG
/// setups).
const AUX_MODELS_FILE_ENV: &str = "ENTANGLEMENT_AUX_MODELS_FILE";

/// A recognized auxiliary LLM purpose (Issue 5). Each one has its own persisted
/// `(provider, model)` pin and a runtime-side call site.
///
/// - [`Summarize`][Purpose::Summarize] — the compaction summary (the same
///   oneshot op `session/summarize.rs::summarize` core's `compact` uses, today
///   driven by the session's primary model). *Wiring this purpose into the
///   compact path is the documented next step (Issue 5 batch A); the pin is
///   storable today.*
/// - [`SessionTitle`][Purpose::SessionTitle] — the auto session title/description
///   generator (purely runtime-side, on the first prompt).
/// - [`Narrate`][Purpose::Narrate] — the live action narrator (#635): a short
///   "what the agent is doing now" phrase, set as `Session.action` on every
///   tool call (purely runtime-side, `narrate.rs`).
///
/// Serialized as the lowercase snake-case key (`summarize`, `session_title`,
/// `narrate`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Purpose {
    Summarize,
    SessionTitle,
    Narrate,
}

impl std::fmt::Display for Purpose {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Purpose {
    /// The stable string key used in the managed file. The serde derive already
    /// round-trips this, but this is the canonical spelling callers can reach
    /// for without constructing the enum first (e.g. the `/aux-model` parser).
    pub fn as_str(self) -> &'static str {
        match self {
            Purpose::Summarize => "summarize",
            Purpose::SessionTitle => "session_title",
            Purpose::Narrate => "narrate",
        }
    }

    /// Parse a purpose key, case-insensitively, into [`Purpose`]. `None` for an
    /// unrecognized string (the `/aux-model` command surfaces this as a parse
    /// error rather than silently storing an inert pin).
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "summarize" => Some(Purpose::Summarize),
            "session_title" | "title" => Some(Purpose::SessionTitle),
            "narrate" => Some(Purpose::Narrate),
            _ => None,
        }
    }
}

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

/// The runtime's per-purpose model pins: the persisted map (keyed by the
/// `Purpose` enum for type-safe lookups) plus the file path to re-write when a
/// new pin is recorded.
#[derive(Debug, Default)]
pub struct AuxModelStore {
    aux: BTreeMap<Purpose, AuxModelPin>,
    path: Option<PathBuf>,
}

impl AuxModelStore {
    /// Load the persisted pins from the managed file, resolving its path from
    /// `ENTANGLEMENT_AUX_MODELS_FILE` or `${config_dir}/entanglement/`. A
    /// missing file is an empty store; a malformed one is logged and treated as
    /// empty (fail-open — a corrupt file only reverts a purpose to the primary
    /// model).
    pub fn load() -> Self {
        let path = aux_models_file_path();
        let aux = match &path {
            Some(p) => read_aux_models(p),
            None => BTreeMap::new(),
        };
        Self { aux, path }
    }

    /// The persisted `(provider, model)` pin for `purpose`, if any.
    pub fn get(&self, purpose: Purpose) -> Option<(&str, &str)> {
        self.aux
            .get(&purpose)
            .map(|p| (p.provider.as_str(), p.model.as_str()))
    }

    /// Record `purpose`'s pin and re-write the managed file, merged against
    /// whatever is on disk under an exclusive lock (#329) — a concurrent
    /// skutter instance's own pin, recorded between this store's `load()` and
    /// now, must survive rather than being clobbered by a write from stale
    /// in-memory state. Idempotent: re-setting an identical pin still rewrites
    /// (a no-diff write), which keeps the call site simple. Returns the write
    /// error for the caller to log (never fatal).
    pub fn set(&mut self, purpose: Purpose, provider: &str, model: &str) -> Result<()> {
        let pin = AuxModelPin {
            provider: provider.to_string(),
            model: model.to_string(),
        };
        let Some(path) = self.path.clone() else {
            bail!(
                "no config directory for the managed aux-models file; \
                 set {AUX_MODELS_FILE_ENV} to a path first"
            );
        };
        let merged = super::lock::with_locked_file(&path, || {
            let mut on_disk = read_aux_models(&path);
            on_disk.insert(purpose, pin.clone());
            persist_map(&path, &on_disk)?;
            Ok(on_disk)
        })?;
        self.aux = merged;
        Ok(())
    }

    /// Re-read the persisted pins from disk (#329) — picks up a pin another
    /// skutter instance recorded via `/aux-model`.
    pub fn reload(&mut self) {
        if let Some(path) = &self.path {
            self.aux = read_aux_models(path);
        }
    }

    /// Build an in-memory store from `pins` directly — no file, no env var, no
    /// re-write on [`set`](Self::set) (there is no `path` to write to). The
    /// production counterpart of [`for_test`](Self::for_test): a multi-user
    /// embedder's [`UserAuxModelStore`][crate::multi_user::aux::UserAuxModelStore]
    /// lookup returns a user's pins as plain data, and this wraps them into the
    /// same store shape [`AuxLlmRegistry`][crate::aux_llm::AuxLlmRegistry]
    /// already knows how to consult — no separate per-user resolution path
    /// needed.
    pub fn in_memory(pins: BTreeMap<Purpose, (String, String)>) -> Self {
        let aux = pins
            .into_iter()
            .map(|(p, (provider, model))| (p, AuxModelPin { provider, model }))
            .collect();
        Self { aux, path: None }
    }

    /// Build an in-memory store from `pins` directly — no file, no env var.
    /// Lets a caller (e.g. `inspect::aux_models`'s render tests) exercise
    /// [`get`](Self::get) without mutating the process-global
    /// `ENTANGLEMENT_AUX_MODELS_FILE` env var, which would race against
    /// [`crate::aux_llm`]'s own tests under `cargo test`'s parallel threads.
    #[cfg(test)]
    pub(crate) fn for_test(pins: &[(Purpose, &str, &str)]) -> Self {
        let aux = pins
            .iter()
            .map(|(p, provider, model)| {
                (
                    *p,
                    AuxModelPin {
                        provider: provider.to_string(),
                        model: model.to_string(),
                    },
                )
            })
            .collect();
        Self { aux, path: None }
    }
}

/// Re-write the managed file at `path` from `aux`. The [`BTreeMap`] keeps the
/// output stable (readable diffs, no churn). The enum keys are serialized as
/// their lowercase snake-case string form so the persisted YAML matches what a
/// user would hand-write.
fn persist_map(path: &Path, aux: &BTreeMap<Purpose, AuxModelPin>) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let keyed: BTreeMap<String, AuxModelPin> = aux
        .iter()
        .map(|(p, v)| (p.as_str().to_string(), v.clone()))
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

/// Resolve the managed aux-models file path: `ENTANGLEMENT_AUX_MODELS_FILE`
/// wins, otherwise `${config_dir}/entanglement/aux-models.yml`. `None` when
/// neither is available (persistence then silently no-ops).
fn aux_models_file_path() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os(AUX_MODELS_FILE_ENV) {
        return Some(PathBuf::from(p));
    }
    dirs::config_dir().map(|d| d.join("entanglement").join("aux-models.yml"))
}

/// Read + parse the aux-models file at `path`. A missing file, or any
/// read/parse error, yields an empty map (logged) — fail-open, since a dropped
/// pin only reverts a purpose to the primary model. An unrecognized purpose key
/// in the file is skipped (logged at debug) rather than failing the whole file,
/// matching how an unknown agent-name pin is treated in `agent-models.yml`.
fn read_aux_models(path: &Path) -> BTreeMap<Purpose, AuxModelPin> {
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
                Some(p) => Some((p, v)),
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
    fn purpose_roundtrips_through_serde_keys() {
        for p in [Purpose::Summarize, Purpose::SessionTitle, Purpose::Narrate] {
            let s = serde_yaml::to_string(&p).unwrap();
            let back: Purpose = serde_yaml::from_str(s.trim()).unwrap();
            assert_eq!(p, back);
        }
        // `title` is accepted as an alias of `session_title` (the friendly
        // spelling a user is likeliest to type at `/aux-model`).
        assert_eq!(Purpose::parse("session_title"), Some(Purpose::SessionTitle));
        assert_eq!(Purpose::parse("title"), Some(Purpose::SessionTitle));
        assert_eq!(Purpose::parse("summarize"), Some(Purpose::Summarize));
        assert_eq!(Purpose::parse("narrate"), Some(Purpose::Narrate));
        assert_eq!(Purpose::parse("bogus_purpose"), None);
        // Case-insensitive parse.
        assert_eq!(Purpose::parse("Summarize"), Some(Purpose::Summarize));
        // Whitespace is tolerated.
        assert_eq!(Purpose::parse("  summarize "), Some(Purpose::Summarize));
    }

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
