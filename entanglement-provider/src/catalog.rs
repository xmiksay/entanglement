//! Provider/model catalog — YAML, not code.
//!
//! An embedded default catalog ([`include_str!`] of `defaults.yml`) is
//! deep-merged with an optional user override file. This is what lets a user
//! add a provider (any OpenAI-compatible endpoint: a proxy, a local vLLM, a new
//! vendor) or tweak model metadata/pricing without a code change.
//!
//! # Merge semantics
//! The merge happens at the [`serde_yaml::Value`] level *before* deserializing,
//! so field-level override falls out for free (no parallel "patch" structs, no
//! `bool` fields getting reset to defaults by a whole-struct replace):
//!
//! 1. Both documents are parsed to `Value`.
//! 2. `providers` sequences merge **by `name`**; `models` sequences merge **by
//!    `id`**: a matching entry merges recursively, a user-only entry appends
//!    (defaults first, then new user entries — order is the auto-detect
//!    priority).
//! 3. Mappings merge key-wise recursively; scalars and other sequences are
//!    replaced by the user value.
//! 4. The merged `Value` is deserialized into [`Catalog`], where
//!    `deny_unknown_fields` validates the user file (typos are loud, not silent).
//!
//! Precedence overall is **env > user YAML > embedded defaults** — the env layer
//! is applied by the head when it reads a resolved [`ProviderEntry`].

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Deserialize;
use serde_yaml::Value;

const DEFAULTS_YML: &str = include_str!("defaults.yml");

/// Env var overriding the user override file path (also what tests and non-XDG
/// setups use).
const PROVIDERS_FILE_ENV: &str = "ENTANGLEMENT_PROVIDERS_FILE";

/// The full provider/model catalog.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Catalog {
    pub providers: Vec<ProviderEntry>,
}

/// One provider: how to reach it and which models it serves.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderEntry {
    /// Unique key; the merge identity across defaults + user file.
    pub name: String,
    /// Which client speaks to this provider.
    #[serde(default)]
    pub wire: Wire,
    /// `None` → the wire client's own default (Anthropic).
    #[serde(default)]
    pub base_url: Option<String>,
    /// Env var holding the API key; `None` = keyless (e.g. local Ollama).
    #[serde(default)]
    pub key_env: Option<String>,
    /// Requests-per-minute budget for this provider's endpoint bucket; `None`
    /// falls back to the client's default (`RetryConfig::rpm`). Plumbed into the
    /// per-endpoint rate limiter so each provider gets its real budget (#241).
    #[serde(default)]
    pub rpm: Option<u32>,
    pub default_model: String,
    #[serde(default)]
    pub models: Vec<ModelEntry>,
}

/// Which concrete client talks to a provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Wire {
    /// OpenAI Chat Completions wire — z.ai, OpenAI, Ollama, any compatible proxy.
    #[default]
    Openai,
    /// Anthropic `/v1/messages` wire.
    Anthropic,
}

/// One model plus its capability + pricing metadata.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelEntry {
    /// Unique within its provider; the merge identity.
    pub id: String,
    #[serde(default)]
    pub display_name: Option<String>,
    #[serde(default)]
    pub context_window: Option<u32>,
    #[serde(default)]
    pub max_output_tokens: Option<u32>,
    #[serde(default)]
    pub supports_thinking: bool,
    #[serde(default = "default_true")]
    pub supports_temperature: bool,
    #[serde(default)]
    pub default_temperature: Option<f32>,
    #[serde(default)]
    pub pricing: Option<ModelPricing>,
}

/// USD per million tokens. Every field is optional; providers without a given
/// billing dimension (e.g. no separate cache-write charge) omit it.
#[derive(Debug, Clone, Copy, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelPricing {
    #[serde(default)]
    pub input: Option<f64>,
    #[serde(default)]
    pub output: Option<f64>,
    #[serde(default)]
    pub cached_input: Option<f64>,
    #[serde(default)]
    pub cache_write: Option<f64>,
}

impl ModelPricing {
    /// USD cost for a normalized [`Usage`] tally (#192). Each token dimension is
    /// multiplied by its per-million rate; a rate the provider doesn't bill (an
    /// unset field) contributes nothing. Because [`Usage::input_tokens`] is the
    /// *uncached* input, the cached/cache-write dimensions never double-count.
    ///
    /// [`Usage`]: crate::Usage
    /// [`Usage::input_tokens`]: crate::Usage::input_tokens
    pub fn cost_usd(&self, usage: &crate::Usage) -> f64 {
        let bill = |tokens: Option<u64>, rate: Option<f64>| {
            rate.map_or(0.0, |r| tokens.unwrap_or(0) as f64 * r / 1_000_000.0)
        };
        bill(usage.input_tokens, self.input)
            + bill(usage.output_tokens, self.output)
            + bill(usage.cached_input_tokens, self.cached_input)
            + bill(usage.cache_write_tokens, self.cache_write)
    }
}

fn default_true() -> bool {
    true
}

impl ModelEntry {
    /// Human-facing label, falling back to the id when `display_name` is unset.
    pub fn display_name(&self) -> &str {
        self.display_name.as_deref().unwrap_or(&self.id)
    }
}

impl Catalog {
    /// The embedded defaults. Parsing + invariants are guarded by a unit test
    /// (`builtin_parses`), so the `.expect` here is provably unreachable.
    pub fn builtin() -> Catalog {
        serde_yaml::from_str(DEFAULTS_YML)
            .expect("embedded defaults.yml is valid — guarded by test")
    }

    /// Resolve the user override file and, if it exists, deep-merge it over the
    /// builtin. A malformed user file is a loud error (never a silent fallback).
    pub fn load() -> Result<Catalog> {
        match providers_file_path() {
            Some(path) if path.exists() => Catalog::load_from(&path),
            _ => Ok(Catalog::builtin()),
        }
    }

    /// Deep-merge the user file at `path` over the embedded defaults. The
    /// testable core `load()` delegates to.
    pub fn load_from(path: &Path) -> Result<Catalog> {
        let user_str = std::fs::read_to_string(path)
            .with_context(|| format!("reading provider catalog {}", path.display()))?;
        let user_doc: Value = serde_yaml::from_str(&user_str)
            .with_context(|| format!("parsing provider catalog {}", path.display()))?;
        let base_doc: Value = serde_yaml::from_str(DEFAULTS_YML)
            .expect("embedded defaults.yml is valid — guarded by test");
        let merged = merge_value(base_doc, user_doc);
        serde_yaml::from_value(merged)
            .with_context(|| format!("validating merged provider catalog with {}", path.display()))
    }

    pub fn provider(&self, name: &str) -> Option<&ProviderEntry> {
        self.providers.iter().find(|p| p.name == name)
    }

    pub fn model(&self, provider: &str, id: &str) -> Option<&ModelEntry> {
        self.provider(provider)?.models.iter().find(|m| m.id == id)
    }

    /// Find a model by id across *all* providers (the model picker only carries
    /// the id, not which provider it came from).
    pub fn model_by_id(&self, id: &str) -> Option<&ModelEntry> {
        self.providers
            .iter()
            .flat_map(|p| &p.models)
            .find(|m| m.id == id)
    }

    /// The set of env vars every provider reads its API key from (deduped,
    /// keyless providers skipped). A head scrubs these from unsandboxed exec
    /// tools so a model-authored command can't read the credentials (#164).
    pub fn key_envs(&self) -> Vec<String> {
        let mut seen = Vec::new();
        for entry in &self.providers {
            if let Some(key) = &entry.key_env {
                if !seen.contains(key) {
                    seen.push(key.clone());
                }
            }
        }
        seen
    }
}

/// The user override file path: `${config_dir}/entanglement/providers.yml`,
/// overridable via `ENTANGLEMENT_PROVIDERS_FILE`.
fn providers_file_path() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os(PROVIDERS_FILE_ENV) {
        return Some(PathBuf::from(p));
    }
    dirs::config_dir().map(|d| d.join("entanglement").join("providers.yml"))
}

// ── merge ────────────────────────────────────────────────────────────────────

/// Deep-merge `over` onto `base`. Mappings merge key-wise; the two keyed
/// sequences (`providers` by `name`, `models` by `id`) merge by identity;
/// everything else is replaced by `over`.
fn merge_value(base: Value, over: Value) -> Value {
    match (base, over) {
        (Value::Mapping(mut base_map), Value::Mapping(over_map)) => {
            for (key, over_val) in over_map {
                let merged = match base_map.remove(&key) {
                    Some(base_val) => match key.as_str() {
                        Some("providers") => merge_seq_by(base_val, over_val, "name"),
                        Some("models") => merge_seq_by(base_val, over_val, "id"),
                        _ => merge_value(base_val, over_val),
                    },
                    None => over_val,
                };
                base_map.insert(key, merged);
            }
            Value::Mapping(base_map)
        }
        // Scalars and non-keyed sequences: the user value wins outright.
        (_, over) => over,
    }
}

/// Merge two sequences by a shared identity key: matching entries merge
/// recursively (in the base's position), user-only entries append. On a type
/// mismatch (either side isn't a sequence) the user value wins outright.
fn merge_seq_by(base: Value, over: Value, id_key: &str) -> Value {
    let mut base_seq = match base {
        Value::Sequence(s) => s,
        _ => return over,
    };
    let over_seq = match over {
        Value::Sequence(s) => s,
        other => return other,
    };
    for over_item in over_seq {
        let over_id = over_item.get(id_key).cloned();
        let pos = over_id
            .as_ref()
            .and_then(|oid| base_seq.iter().position(|b| b.get(id_key) == Some(oid)));
        match pos {
            Some(i) => {
                let base_item = base_seq.remove(i);
                base_seq.insert(i, merge_value(base_item, over_item));
            }
            None => base_seq.push(over_item),
        }
    }
    Value::Sequence(base_seq)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_parses() {
        let c = Catalog::builtin();
        assert!(c.provider("zai").is_some());
        assert!(c.provider("openai").is_some());
        assert!(c.provider("ollama").is_some());
        assert!(c.provider("anthropic").is_some());
        // z.ai is first — the auto-detect priority the head relies on.
        assert_eq!(c.providers[0].name, "zai");
        // Every provider has a default model that actually exists in its list.
        for p in &c.providers {
            assert!(
                p.models.iter().any(|m| m.id == p.default_model),
                "provider {} default_model {} missing from its models",
                p.name,
                p.default_model
            );
        }
    }

    #[test]
    fn key_envs_collects_keyed_providers_deduped() {
        let c = Catalog::builtin();
        let keys = c.key_envs();
        assert!(keys.contains(&"ZAI_API_KEY".to_string()), "{keys:?}");
        assert!(keys.contains(&"OPENAI_API_KEY".to_string()), "{keys:?}");
        assert!(keys.contains(&"ANTHROPIC_API_KEY".to_string()), "{keys:?}");
        // Keyless Ollama contributes nothing, and there are no duplicates.
        let mut deduped = keys.clone();
        deduped.dedup();
        assert_eq!(deduped.len(), keys.len(), "no duplicate key_env: {keys:?}");
    }

    #[test]
    fn defaults_and_wires_and_helpers() {
        let c = Catalog::builtin();
        // Ollama is keyless; anthropic speaks its own wire; others default openai.
        assert_eq!(c.provider("ollama").unwrap().key_env, None);
        assert_eq!(c.provider("anthropic").unwrap().wire, Wire::Anthropic);
        assert_eq!(c.provider("zai").unwrap().wire, Wire::Openai);
        // Anthropic base_url falls through to the wire client's own default.
        assert_eq!(c.provider("anthropic").unwrap().base_url, None);
        // supports_temperature defaults true, supports_thinking false.
        let glm = c.model("zai", "glm-5.2").unwrap();
        assert!(glm.supports_temperature);
        assert!(glm.supports_thinking);
        assert_eq!(glm.display_name(), "GLM-5.2");
        assert_eq!(glm.pricing.unwrap().input, Some(0.6));
        // model_by_id searches across providers.
        assert_eq!(
            c.model_by_id("gpt-4o").unwrap().context_window,
            Some(128000)
        );
        assert!(c.model_by_id("does-not-exist").is_none());
    }

    fn merge_str(user: &str) -> Catalog {
        let base: Value = serde_yaml::from_str(DEFAULTS_YML).unwrap();
        let over: Value = serde_yaml::from_str(user).unwrap();
        serde_yaml::from_value(merge_value(base, over)).unwrap()
    }

    #[test]
    fn field_level_override_keeps_siblings() {
        // Overriding one price must not reset other fields to their defaults.
        let c = merge_str(
            "providers:\n  - name: zai\n    models:\n      - id: glm-5.2\n        pricing: { input: 0.5 }\n",
        );
        let glm = c.model("zai", "glm-5.2").unwrap();
        assert_eq!(glm.pricing.unwrap().input, Some(0.5));
        // untouched sibling within the same pricing block survives...
        assert_eq!(glm.pricing.unwrap().output, Some(2.2));
        // ...as do sibling model fields and other models in the provider.
        assert!(glm.supports_thinking);
        assert_eq!(glm.context_window, Some(128000));
        assert!(c.model("zai", "glm-4.7").is_some());
    }

    #[test]
    fn user_can_append_model_and_provider() {
        let c = merge_str(
            "providers:\n\
             \x20 - name: zai\n\
             \x20   models:\n\
             \x20     - id: glm-5-flash\n\
             \x20       context_window: 128000\n\
             \x20 - name: myproxy\n\
             \x20   base_url: http://localhost:8000/v1\n\
             \x20   key_env: MYPROXY_KEY\n\
             \x20   default_model: custom-1\n\
             \x20   models:\n\
             \x20     - id: custom-1\n",
        );
        // Appended model on an existing provider (defaults preserved before it).
        let zai = c.provider("zai").unwrap();
        assert_eq!(zai.models.first().unwrap().id, "glm-5.2");
        assert!(zai.models.iter().any(|m| m.id == "glm-5-flash"));
        // New user provider appended after the defaults.
        assert_eq!(c.providers.last().unwrap().name, "myproxy");
        assert_eq!(c.provider("myproxy").unwrap().wire, Wire::Openai);
    }

    #[test]
    fn unknown_field_is_rejected() {
        let base: Value = serde_yaml::from_str(DEFAULTS_YML).unwrap();
        let over: Value =
            serde_yaml::from_str("providers:\n  - name: zai\n    typo_field: 1\n").unwrap();
        let err = serde_yaml::from_value::<Catalog>(merge_value(base, over)).unwrap_err();
        assert!(err.to_string().contains("typo_field"), "got: {err}");
    }

    #[test]
    fn load_from_missing_file_errors() {
        let err = Catalog::load_from(Path::new("/no/such/providers.yml")).unwrap_err();
        assert!(err.to_string().contains("reading provider catalog"));
    }

    #[test]
    fn scalar_override_replaces() {
        let c = merge_str("providers:\n  - name: zai\n    default_model: glm-4.7\n");
        assert_eq!(c.provider("zai").unwrap().default_model, "glm-4.7");
    }

    #[test]
    fn rpm_is_optional_and_user_overridable() {
        // Unset in the embedded defaults → None (falls back to the client default).
        assert_eq!(Catalog::builtin().provider("zai").unwrap().rpm, None);
        // A user file can set a per-provider rpm without touching sibling fields.
        let c = merge_str("providers:\n  - name: zai\n    rpm: 120\n");
        assert_eq!(c.provider("zai").unwrap().rpm, Some(120));
        assert_eq!(c.provider("zai").unwrap().default_model, "glm-5.2");
    }
}
