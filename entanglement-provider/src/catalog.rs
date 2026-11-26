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

use std::path::Path;

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use serde_yaml::Value;

// User-file resolution + the pre-deserialization deep merge, split out for the
// 400-line file cap. The merge semantics themselves stay documented above,
// where a reader of `Catalog` needs them.
mod effort;
mod generation;
mod merge;
mod pricing;
mod thinking;
mod usable;

pub use effort::{clamp_within, EffortTiers, ResolvedEffort};
use merge::{merge_value, providers_file_path};
pub use pricing::ModelPricing;
pub use thinking::{
    AnthropicModelSpec, ThinkingControl, ThinkingFormat, ThinkingSpec, ThinkingStyle,
};

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
    /// This provider's endpoint limits (`limits: {rpm, tpm, concurrency}`);
    /// an unset `rpm`/`concurrency` falls back to the client's built-in
    /// default (50 rpm, 3 in flight).
    #[serde(default)]
    pub limits: crate::RateLimits,
    pub default_model: String,
    #[serde(default)]
    pub models: Vec<ModelEntry>,
    /// Whether this endpoint accepts OpenAI's `prompt_cache_key` implicit-cache
    /// routing hint (#673). Off by default; the embedded defaults enable it
    /// only for providers verified to accept the field — an endpoint that
    /// rejects unknown body fields would otherwise 400 on every request. A
    /// user `providers.yml` can flip it for any OpenAI-compat endpoint (the
    /// #118 "catalog data, not hardcode" property). Openai-wire only; the
    /// other wires have no equivalent field and ignore it.
    #[serde(default)]
    pub prompt_cache_key: bool,
    /// How this endpoint is told to think, OpenAI-compat wire only (see
    /// [`ThinkingControl`]). `None` — the default, and every non-z.ai entry —
    /// sends no `thinking` object at all: OpenAI proper 400s on the unknown
    /// field.
    #[serde(default)]
    pub thinking_control: Option<ThinkingControl>,
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
    /// Google Gemini `:streamGenerateContent` wire (#309). Native, not the
    /// OpenAI-compat surface — it round-trips `thoughtSignature` the compat
    /// endpoint drops.
    Gemini,
    /// OpenAI's native Responses API `/responses` wire (ADR-0196 §3, P7):
    /// flat typed input/output items instead of Chat Completions'
    /// role+content messages. Opt-in per catalog entry — `openai` stays on
    /// Chat Completions; a user (or a future embedded default) picks this
    /// wire explicitly for a model that supports it (`gpt-5.4`+).
    #[serde(rename = "openai_responses")]
    OpenaiResponses,
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
    /// Accepts image input (#736). Gates the TUI's clipboard image paste;
    /// off by default so an unknown model is never sent an image it would 400 on.
    #[serde(default)]
    pub supports_vision: bool,
    /// Extended-thinking budget in tokens to request when thinking is enabled for
    /// this model (#191). Only meaningful with `supports_thinking`; left unset in
    /// the embedded defaults so thinking stays off until a user opts in — the
    /// channel is now reachable (a user `providers.yml` sets a budget), not forced
    /// on. Surfaces as Anthropic's `thinking.budget_tokens`.
    #[serde(default)]
    pub thinking_budget_tokens: Option<u32>,
    /// Which extended-thinking request shape this model accepts. See
    /// [`ThinkingStyle`]; only meaningful with `supports_thinking`. Unset means
    /// [`ThinkingStyle::Budget`], preserving pre-existing behaviour.
    #[serde(default)]
    pub thinking_style: Option<ThinkingStyle>,
    /// How this model emits thinking on the **OpenAI-compat** wire:
    /// structured `reasoning`/`reasoning_content` delta fields (the default)
    /// or inline `<think>…</think>` spans in `content` (ADR-0191 —
    /// `thinking_format: inline_tags`, the qwen3.5-class parser-less-server
    /// case; see [`ThinkingFormat`]). Only meaningful with `supports_thinking`;
    /// ignored by the Anthropic/Gemini wires.
    #[serde(default)]
    pub thinking_format: Option<ThinkingFormat>,
    /// Whether captured thinking blocks are **sent back** to the provider on the
    /// next request. Only meaningful with `supports_thinking`.
    ///
    /// `None` derives the answer from the wire: on Anthropic, replay is on
    /// whenever thinking is enabled, because the API *requires* the unmodified
    /// block on a tool round-trip and a silent `false` would surface as a 400 the
    /// user cannot diagnose; every other wire defaults off. An explicit value
    /// always wins, so a user can force replay off (to cut input tokens, or work
    /// around a provider quirk) or on (an OpenAI-compatible endpoint that does
    /// accept reasoning back) without a code change.
    ///
    /// This gates **replay only, never capture**: blocks are always assembled and
    /// always persisted, so flipping the flag never rewrites history — it only
    /// changes what the next request carries.
    #[serde(default)]
    pub replay_thinking: Option<bool>,
    #[serde(default = "default_true")]
    pub supports_temperature: bool,
    #[serde(default)]
    pub default_temperature: Option<f32>,
    /// Default [`ReasoningEffort`](crate::ReasoningEffort) tier for this model
    /// (#374). Unset in the embedded defaults — the knob stays off until a user
    /// `providers.yml` or a live `SetGeneration` opts in, mirroring how
    /// `thinking_budget_tokens` starts unset.
    #[serde(default)]
    pub default_reasoning_effort: Option<crate::ReasoningEffort>,
    /// The effort tiers this model accepts (see [`EffortTiers`]): unset ⇒
    /// any tier passes through; a list ⇒ clamp to the nearest listed tier;
    /// the empty list ⇒ never send an effort field. Applied by every wire
    /// that names a tier (`reasoning_effort`, Responses `reasoning.effort`,
    /// Anthropic adaptive `output_config.effort`).
    #[serde(default)]
    pub effort_tiers: Option<EffortTiers>,
    /// The model cannot run with thinking off (GLM-5.3, Claude Fable): with
    /// no effort resolved the wire still enables thinking at the lowest
    /// listed tier instead of sending a `disabled` shape the API rejects.
    #[serde(default)]
    pub thinking_required: bool,
    /// Limits on **this model** on its provider's endpoint, enforced on top
    /// of the provider's own `limits` — same shape. Some providers (z.ai) cap
    /// concurrency per model: GLM-4.7-Flash allows 1 in flight, GLM-5.2 5, on
    /// one base URL and key. Unset axes leave the model bound by the endpoint
    /// alone.
    #[serde(default)]
    pub limits: crate::RateLimits,
    #[serde(default)]
    pub pricing: Option<ModelPricing>,
}

fn default_true() -> bool {
    true
}

impl Catalog {
    /// The embedded defaults. Parsing + invariants are guarded by a unit test
    /// (`builtin_parses`), so the `.expect` here is provably unreachable.
    pub fn builtin() -> Catalog {
        serde_yaml::from_str(DEFAULTS_YML)
            .expect("embedded defaults.yml is valid — guarded by test")
    }

    /// Resolve the user override file and, if it exists, deep-merge it over the
    /// builtin. A malformed user file is a loud error (never a silent fallback);
    /// so, now, is an explicit `ENTANGLEMENT_PROVIDERS_FILE` pointing at a path
    /// that does not exist — the default `${config_dir}` path being absent stays
    /// the normal "no user override" case (#204).
    pub fn load() -> Result<Catalog> {
        Catalog::load_located(providers_file_path())
    }

    /// [`load`](Self::load) for an already-resolved user-file location.
    fn load_located(location: Option<merge::CatalogFile>) -> Result<Catalog> {
        match location {
            Some(loc) if loc.path.exists() => Catalog::load_from(&loc.path),
            Some(loc) if loc.explicit => bail!(
                "{PROVIDERS_FILE_ENV} points at a missing provider catalog: {}",
                loc.path.display()
            ),
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

    /// Build a [`crate::LimitsResolver`] for `provider`: per request, its
    /// endpoint `limits` plus the requested model's own (#550 — resolved
    /// against the request's model, not the client's construction-time one).
    pub fn limits_resolver(&self, provider: &str) -> crate::LimitsResolver {
        let catalog = self.clone();
        let provider = provider.to_string();
        std::sync::Arc::new(move |model: &str| crate::RequestLimits {
            endpoint: catalog
                .provider(&provider)
                .map(|p| p.limits)
                .unwrap_or_default(),
            model: catalog
                .model(&provider, model)
                .map(|m| m.limits)
                .unwrap_or_default(),
        })
    }

    /// The tightest configured in-flight cap for `(provider, model)`: the
    /// model's own, else its provider's, else `None` (the client default).
    pub fn effective_concurrency(&self, provider: &str, model: &str) -> Option<usize> {
        self.model(provider, model)
            .and_then(|m| m.limits.concurrency)
            .or_else(|| self.provider(provider).and_then(|p| p.limits.concurrency))
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

#[cfg(test)]
mod tests {
    use super::*;

    /// LLM-endpoint OAuth was removed (#917): an `oauth:` block on a provider
    /// entry is an unknown field, refused loudly rather than silently ignored.
    #[test]
    fn provider_oauth_block_is_rejected() {
        let err = serde_yaml::from_str::<ProviderEntry>(
            "name: myproxy\nbase_url: https://llm.example/v1\ndefault_model: m\noauth: {}\n",
        )
        .expect_err("oauth is no longer a provider field");
        assert!(err.to_string().contains("oauth"), "{err}");
    }

    #[test]
    fn builtin_parses() {
        let c = Catalog::builtin();
        assert!(c.provider("zai").is_some());
        assert!(c.provider("openai").is_some());
        assert!(c.provider("ollama").is_some());
        assert!(c.provider("anthropic").is_some());
        // z.ai is first — the auto-detect priority the head relies on.
        assert_eq!(c.providers[0].name, "zai");
        // The full z.ai text palette is present, glm-5.3 leading (#303).
        const ZAI_MODELS: [&str; 15] = [
            "glm-5.3",
            "glm-5.3-flash",
            "glm-5.2",
            "glm-5.1",
            "glm-5",
            "glm-4.7",
            "glm-4.7-flashx",
            "glm-4.7-flash",
            "glm-4.6",
            "glm-4.5",
            "glm-4.5-x",
            "glm-4.5-air",
            "glm-4.5-airx",
            "glm-4.5-flash",
            "glm-4-32b-0414-128k",
        ];
        let zai = c.provider("zai").unwrap();
        assert_eq!(zai.models.len(), ZAI_MODELS.len());
        assert_eq!(zai.models.first().unwrap().id, "glm-5.3");
        for id in ZAI_MODELS {
            assert!(
                c.model("zai", id).is_some(),
                "zai model {id} missing from the embedded catalog"
            );
        }
        // The retired ids are gone (glm-5-turbo left the pricing page;
        // claude-3-5-sonnet-20241022 was retired 2025-10-28).
        assert!(c.model("zai", "glm-5-turbo").is_none());
        assert!(c.model("anthropic", "claude-3-5-sonnet-20241022").is_none());
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
    fn openai_responses_wire_parses_and_is_opt_in_not_a_default_flip() {
        // ADR-0196 §3, P7: a distinct catalog entry on the new wire — `openai`
        // itself stays on Chat Completions, and (since auto-detect walks
        // `providers` in order) sits *before* `openai_responses` in the list,
        // so an OPENAI_API_KEY-only environment still auto-detects to plain
        // `openai`, never the Responses surface.
        let c = Catalog::builtin();
        let openai_idx = c.providers.iter().position(|p| p.name == "openai").unwrap();
        let responses_idx = c
            .providers
            .iter()
            .position(|p| p.name == "openai_responses")
            .unwrap();
        assert!(openai_idx < responses_idx);
        let entry = c.provider("openai_responses").unwrap();
        assert_eq!(entry.wire, Wire::OpenaiResponses);
        assert_eq!(entry.key_env.as_deref(), Some("OPENAI_API_KEY"));
        assert_eq!(entry.default_model, "gpt-5.4");
        assert!(c.model("openai_responses", "gpt-5.4").is_some());
        // The wire's YAML/serde spelling is the underscore form, not
        // `#[serde(rename_all = "lowercase")]`'s default `openairesponses`.
        let parsed: Wire = serde_yaml::from_str("openai_responses").unwrap();
        assert_eq!(parsed, Wire::OpenaiResponses);
    }

    #[test]
    fn defaults_and_wires_and_helpers() {
        let c = Catalog::builtin();
        // Ollama is keyless; anthropic speaks its own wire; others default openai.
        assert_eq!(c.provider("ollama").unwrap().key_env, None);
        assert_eq!(c.provider("anthropic").unwrap().wire, Wire::Anthropic);
        assert_eq!(c.provider("zai").unwrap().wire, Wire::Openai);
        // Gemini speaks its own native wire (#309), keyed by GEMINI_API_KEY.
        assert_eq!(c.provider("gemini").unwrap().wire, Wire::Gemini);
        assert_eq!(
            c.provider("gemini").unwrap().key_env.as_deref(),
            Some("GEMINI_API_KEY")
        );
        assert!(
            c.model("gemini", "gemini-2.5-flash")
                .unwrap()
                .supports_thinking
        );
        // Anthropic base_url falls through to the wire client's own default.
        assert_eq!(c.provider("anthropic").unwrap().base_url, None);
        // supports_temperature defaults true, supports_thinking false.
        let glm = c.model("zai", "glm-5.2").unwrap();
        assert!(glm.supports_temperature);
        assert!(glm.supports_thinking);
        assert_eq!(glm.display_name(), "GLM-5.2");
        assert_eq!(glm.pricing.unwrap().input, Some(1.4));
        // model_by_id searches across providers.
        assert_eq!(
            c.model_by_id("gpt-4o").unwrap().context_window,
            Some(128000)
        );
        assert!(c.model_by_id("does-not-exist").is_none());
    }

    #[test]
    fn zai_palette_pricing_and_thinking() {
        let c = Catalog::builtin();
        // The free tiers price every usage dimension at zero (#303).
        let usage = crate::Usage {
            input_tokens: Some(10_000),
            output_tokens: Some(5_000),
            cached_input_tokens: Some(1_000),
            cache_write_tokens: None,
        };
        for free in ["glm-4.7-flash", "glm-4.5-flash"] {
            let pricing = c.model("zai", free).unwrap().pricing.unwrap();
            assert_eq!(pricing.cost_usd(&usage), 0.0, "{free} should be free");
        }
        // A paid model bills something for the same usage.
        assert!(
            c.model("zai", "glm-5.2")
                .unwrap()
                .pricing
                .unwrap()
                .cost_usd(&usage)
                > 0.0
        );
        // glm-4-32b-0414-128k is the one non-reasoning model in the palette.
        let legacy = c.model("zai", "glm-4-32b-0414-128k").unwrap();
        assert!(!legacy.supports_thinking);
        assert_eq!(legacy.pricing.unwrap().cached_input, None);
        // Every other zai model is thinking-capable.
        for m in &c.provider("zai").unwrap().models {
            if m.id != "glm-4-32b-0414-128k" {
                assert!(m.supports_thinking, "{} should support thinking", m.id);
            }
        }
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
        assert_eq!(glm.pricing.unwrap().output, Some(4.4));
        // ...as do sibling model fields and other models in the provider.
        assert!(glm.supports_thinking);
        assert_eq!(glm.context_window, Some(1000000));
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
        assert_eq!(zai.models.first().unwrap().id, "glm-5.3");
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
    fn explicit_env_override_at_missing_path_is_a_loud_error() {
        // A user who sets ENTANGLEMENT_PROVIDERS_FILE and mistypes it should not
        // silently get the embedded defaults — that override must be honored or
        // loudly rejected (#204).
        let location = merge::resolve_providers_file(
            |n| (n == PROVIDERS_FILE_ENV).then(|| "/no/such/providers.yml".into()),
            || panic!("the config dir must not be consulted under an override"),
        );
        let err = Catalog::load_located(location).unwrap_err();
        assert!(err.to_string().contains(PROVIDERS_FILE_ENV), "got: {err:#}");
    }

    #[test]
    fn a_missing_default_providers_file_is_the_builtin() {
        let dir = tempfile::tempdir().unwrap();
        // `ENTANGLEMENT_STATE_DIR` (ADR-0213) never moves this hand-edited input.
        let location = merge::resolve_providers_file(
            |n| (n == "ENTANGLEMENT_STATE_DIR").then(|| "/state".into()),
            || Some(dir.path().into()),
        );
        let loc = location
            .as_ref()
            .expect("a config dir yields a default path");
        assert!(!loc.explicit);
        assert!(loc.path.ends_with("entanglement/providers.yml"));
        let catalog = Catalog::load_located(location).unwrap();
        assert_eq!(catalog.providers.len(), Catalog::builtin().providers.len());
    }

    #[test]
    fn scalar_override_replaces() {
        let c = merge_str("providers:\n  - name: zai\n    default_model: glm-4.7\n");
        assert_eq!(c.provider("zai").unwrap().default_model, "glm-4.7");
    }

    #[test]
    fn generation_params_gate_on_capability_flags() {
        let c = Catalog::builtin();
        // glm-5.2 supports temperature with a catalog default; no thinking budget
        // is configured, so thinking stays off even though it *supports* it.
        let glm = c.model("zai", "glm-5.2").unwrap().generation_params();
        assert_eq!(glm.temperature, Some(0.7));
        assert_eq!(glm.max_output_tokens, Some(131072));
        assert_eq!(glm.thinking_budget_tokens, None);

        // A user file that opts a supporting model into thinking and turns another
        // model's temperature off: both flags must gate the resolved knobs.
        let merged = merge_str(
            "providers:\n\
             \x20 - name: zai\n\
             \x20   models:\n\
             \x20     - id: glm-5.2\n\
             \x20       thinking_budget_tokens: 8000\n\
             \x20     - id: glm-4.7\n\
             \x20       supports_temperature: false\n\
             \x20       default_temperature: 0.9\n",
        );
        let glm52 = merged.model("zai", "glm-5.2").unwrap().generation_params();
        assert_eq!(glm52.thinking_budget_tokens, Some(8000));
        // supports_thinking off drops the budget; supports_temperature off drops
        // the temperature even though a default_temperature is present.
        let glm47 = merged.model("zai", "glm-4.7").unwrap();
        let g47 = glm47.generation_params();
        assert!(!glm47.supports_temperature);
        assert_eq!(g47.temperature, None);
    }

    #[test]
    fn prompt_cache_key_defaults_off_and_is_user_overridable() {
        // Off by default (#673) — only endpoints verified to tolerate the
        // extra body field opt in; the embedded defaults enable OpenAI proper.
        assert!(!Catalog::builtin().provider("zai").unwrap().prompt_cache_key);
        assert!(
            Catalog::builtin()
                .provider("openai")
                .unwrap()
                .prompt_cache_key
        );
        // A user file can flip it for any OpenAI-compat endpoint without
        // touching sibling fields (the #118 catalog-data property).
        let c = merge_str(
            "providers:\n\
             \x20 - name: zai\n\
             \x20   prompt_cache_key: true\n",
        );
        assert!(c.provider("zai").unwrap().prompt_cache_key);
        assert_eq!(c.provider("zai").unwrap().default_model, "glm-5.3");
    }

    #[test]
    fn thinking_style_defaults_to_budget_and_is_user_overridable() {
        // Unset everywhere in the embedded defaults, so every pre-existing user
        // catalog keeps emitting the fixed-budget shape it emitted before.
        let entry = Catalog::builtin()
            .model("anthropic", "claude-sonnet-4-5")
            .unwrap()
            .clone();
        assert_eq!(entry.thinking_style, None);
        assert_eq!(entry.resolved_thinking_style(), ThinkingStyle::Budget);

        // A user adding a current model picks the adaptive shape with no code
        // change — the point of keeping this in the catalog (#118).
        let c = merge_str(
            "providers:\n\
             \x20 - name: anthropic\n\
             \x20   models:\n\
             \x20     - id: claude-sonnet-4-5\n\
             \x20       thinking_style: adaptive\n",
        );
        let merged = c.model("anthropic", "claude-sonnet-4-5").unwrap();
        assert_eq!(merged.resolved_thinking_style(), ThinkingStyle::Adaptive);
        assert_eq!(
            merged.context_window,
            Some(200000),
            "sibling fields survive the merge untouched"
        );
    }

    #[test]
    fn replay_thinking_falls_back_to_the_wire_default() {
        let c = merge_str(
            "providers:\n\
             \x20 - name: anthropic\n\
             \x20   models:\n\
             \x20     - id: claude-unlisted\n\
             \x20       supports_thinking: true\n",
        );
        // A user-added model: every embedded Anthropic entry now sets
        // `replay_thinking: true` explicitly (ADR-0202), so "unset" needs one
        // the defaults don't list.
        let entry = c.model("anthropic", "claude-unlisted").unwrap();
        // Unset → the wire decides.
        assert!(entry.replays_thinking(true));
        assert!(!entry.replays_thinking(false));

        // An explicit value overrides the wire default in both directions, so a
        // user can force replay off to cut input tokens, or on for an endpoint
        // the embedded defaults don't know about.
        let off = merge_str(
            "providers:\n\
             \x20 - name: anthropic\n\
             \x20   models:\n\
             \x20     - id: claude-sonnet-4-5\n\
             \x20       supports_thinking: true\n\
             \x20       replay_thinking: false\n",
        );
        assert!(!off
            .model("anthropic", "claude-sonnet-4-5")
            .unwrap()
            .replays_thinking(true));

        let on = merge_str(
            "providers:\n\
             \x20 - name: zai\n\
             \x20   models:\n\
             \x20     - id: glm-5.2\n\
             \x20       replay_thinking: true\n",
        );
        assert!(on.model("zai", "glm-5.2").unwrap().replays_thinking(false));
    }

    #[test]
    fn replay_thinking_is_clamped_by_supports_thinking() {
        // A model that cannot think has nothing to replay, so an over-eager
        // `replay_thinking: true` is dropped rather than sent — mirrors how
        // `generation_params` clamps `thinking_budget_tokens`.
        let c = merge_str(
            "providers:\n\
             \x20 - name: anthropic\n\
             \x20   models:\n\
             \x20     - id: claude-sonnet-4-5\n\
             \x20       supports_thinking: false\n\
             \x20       replay_thinking: true\n",
        );
        assert!(!c
            .model("anthropic", "claude-sonnet-4-5")
            .unwrap()
            .replays_thinking(true));
    }

    #[test]
    fn provider_limits_are_optional_and_user_overridable() {
        // zai ships its documented endpoint ceiling — at least glm-5.2's own
        // cap (5), or that model's cap could never bind.
        let zai = Catalog::builtin().provider("zai").unwrap().limits;
        assert_eq!((zai.rpm, zai.tpm, zai.concurrency), (None, None, Some(5)));
        // A user file sets one axis without touching the others or siblings.
        let c = merge_str("providers:\n  - name: zai\n    limits:\n      rpm: 120\n");
        let zai = c.provider("zai").unwrap();
        assert_eq!(
            (zai.limits.rpm, zai.limits.concurrency),
            (Some(120), Some(5))
        );
        assert_eq!(zai.default_model, "glm-5.3");
    }

    #[test]
    fn the_retired_flat_rpm_and_concurrency_keys_are_rejected() {
        for yaml in [
            "providers:\n  - name: zai\n    rpm: 120\n",
            "providers:\n  - name: zai\n    concurrency: 8\n",
            "providers:\n  - name: zai\n    models:\n      - id: glm-4.6\n        concurrency: 2\n",
        ] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("providers.yml");
            std::fs::write(&path, yaml).unwrap();
            assert!(Catalog::load_from(&path).is_err(), "{yaml}");
        }
    }

    #[test]
    fn model_limits_ship_the_known_zai_tiers_and_are_overridable() {
        let c = Catalog::builtin();
        assert_eq!(
            c.model("zai", "glm-4.7-flash").unwrap().limits.concurrency,
            Some(1)
        );
        assert_eq!(
            c.model("zai", "glm-5.2").unwrap().limits.concurrency,
            Some(5)
        );
        // No documented tier → unset, bound by the endpoint alone.
        assert_eq!(
            c.model("zai", "glm-4.7").unwrap().limits,
            crate::RateLimits::default()
        );

        let c = merge_str(
            "providers:\n\
             \x20 - name: zai\n\
             \x20   models:\n\
             \x20     - id: glm-4.6\n\
             \x20       limits: { concurrency: 2, tpm: 100000 }\n",
        );
        let glm46 = c.model("zai", "glm-4.6").unwrap();
        assert_eq!(
            (glm46.limits.concurrency, glm46.limits.tpm),
            (Some(2), Some(100_000))
        );
        assert_eq!(glm46.context_window, Some(204800));
        assert_eq!(
            c.model("zai", "glm-4.7-flash").unwrap().limits.concurrency,
            Some(1)
        );
    }

    #[test]
    fn limits_resolver_pairs_the_endpoint_with_the_requested_model() {
        let resolve = Catalog::builtin().limits_resolver("zai");
        let flash = resolve("glm-4.7-flash");
        assert_eq!(flash.endpoint.concurrency, Some(5));
        assert_eq!(flash.model.concurrency, Some(1));
        let unknown = resolve("no-such-model");
        assert_eq!(unknown.endpoint.concurrency, Some(5));
        assert_eq!(unknown.model, crate::RateLimits::default());
        let none = Catalog::builtin().limits_resolver("no-such-provider")("x");
        assert_eq!(none, crate::RequestLimits::default());
    }

    #[test]
    fn effective_concurrency_prefers_model_then_provider_then_unset() {
        let c = Catalog::builtin();
        assert_eq!(c.effective_concurrency("zai", "glm-4.7-flash"), Some(1));
        assert_eq!(c.effective_concurrency("zai", "glm-4.7"), Some(5));
        assert_eq!(c.effective_concurrency("zai", "no-such-model"), Some(5));
        assert_eq!(c.effective_concurrency("no-such-provider", "x"), None);
    }

    #[test]
    fn zai_paas_shares_key_and_models_with_the_coding_plan_entry() {
        // Two endpoints, one key env: the Coding Plan entry stays first (the
        // auto-detect winner); a pay-as-you-go key selects `zai_paas`
        // explicitly. The model list is a YAML
        // anchor/alias — the alias-expanded doc must survive the
        // pre-deserialization merge with an identical model order.
        let c = Catalog::builtin();
        let zai = c.provider("zai").unwrap();
        let paas = c.provider("zai_paas").unwrap();
        assert_eq!(
            zai.base_url.as_deref(),
            Some("https://api.z.ai/api/coding/paas/v4")
        );
        assert_eq!(
            paas.base_url.as_deref(),
            Some("https://api.z.ai/api/paas/v4")
        );
        assert_eq!(paas.key_env, zai.key_env);
        assert_eq!(paas.default_model, "glm-5.3");
        assert_eq!(paas.thinking_control, Some(ThinkingControl::Zai));
        assert_eq!(zai.thinking_control, Some(ThinkingControl::Zai));
        let ids = |p: &ProviderEntry| p.models.iter().map(|m| m.id.clone()).collect::<Vec<_>>();
        assert_eq!(ids(zai), ids(paas));
        let zai_idx = c.providers.iter().position(|p| p.name == "zai").unwrap();
        let paas_idx = c
            .providers
            .iter()
            .position(|p| p.name == "zai_paas")
            .unwrap();
        assert!(zai_idx < paas_idx);
        // A user override on one entry must not leak into the alias twin.
        let merged = merge_str(
            "providers:\n  - name: zai_paas\n    models:\n      - id: glm-5.3\n        pricing: { input: 9.0 }\n",
        );
        assert_eq!(
            merged
                .model("zai_paas", "glm-5.3")
                .unwrap()
                .pricing
                .unwrap()
                .input,
            Some(9.0)
        );
        assert_eq!(
            merged
                .model("zai", "glm-5.3")
                .unwrap()
                .pricing
                .unwrap()
                .input,
            Some(1.4)
        );
        assert_eq!(merged.provider("zai").unwrap().models.len(), 15);
    }

    #[test]
    fn zai_effort_tiers_and_required_thinking_ship_in_the_defaults() {
        use crate::ReasoningEffort::*;
        let c = Catalog::builtin();
        let glm53 = c.model("zai", "glm-5.3").unwrap();
        assert!(glm53.thinking_required);
        assert_eq!(glm53.default_reasoning_effort, Some(High));
        assert_eq!(
            glm53.effort_tiers.map(Vec::from),
            Some(vec![Low, High, Max])
        );
        // 5.2 takes every tier; the 4.x family enables thinking with no
        // effort field at all (the empty list).
        assert_eq!(
            c.model("zai", "glm-5.2")
                .unwrap()
                .effort_tiers
                .map(Vec::from),
            Some(vec![Low, Medium, High, XHigh, Max])
        );
        let glm47 = c.model("zai", "glm-4.7").unwrap();
        assert_eq!(glm47.effort_tiers, Some(EffortTiers::from(Vec::new())));
        assert_eq!(glm47.default_reasoning_effort, Some(High));
        assert!(!glm47.thinking_required);
        // The non-reasoning legacy model carries none of it.
        let legacy = c.model("zai", "glm-4-32b-0414-128k").unwrap();
        assert_eq!(legacy.effort_tiers, None);
        assert_eq!(legacy.default_reasoning_effort, None);
    }

    #[test]
    fn anthropic_defaults_carry_the_current_generation() {
        use crate::ReasoningEffort::*;
        let c = Catalog::builtin();
        let a = c.provider("anthropic").unwrap();
        assert_eq!(a.default_model, "claude-sonnet-5");
        assert_eq!(a.thinking_control, None);
        let fable = c.model("anthropic", "claude-fable-5-1").unwrap();
        assert!(fable.thinking_required);
        assert!(!fable.supports_temperature);
        assert_eq!(fable.replay_thinking, Some(true));
        assert_eq!(fable.thinking_style, Some(ThinkingStyle::Adaptive));
        assert_eq!(
            fable.effort_tiers.map(Vec::from),
            Some(vec![Low, Medium, High, XHigh, Max])
        );
        // 4.6 has no xhigh and still takes sampling params.
        let opus46 = c.model("anthropic", "claude-opus-4-6").unwrap();
        assert!(opus46.supports_temperature);
        assert_eq!(
            opus46.effort_tiers.map(Vec::from),
            Some(vec![Low, Medium, High, Max])
        );
        // Opus 5 thinks by default but can be switched off — not required.
        assert!(
            !c.model("anthropic", "claude-opus-5")
                .unwrap()
                .thinking_required
        );
        // The budget-style holdovers keep the fixed-budget thinking shape.
        for id in ["claude-sonnet-4-5", "claude-haiku-4-5"] {
            let m = c.model("anthropic", id).unwrap();
            assert_eq!(m.thinking_style, None, "{id}");
            assert_eq!(m.context_window, Some(200_000), "{id}");
        }
        assert_eq!(
            c.model("anthropic", "claude-sonnet-5")
                .unwrap()
                .pricing
                .unwrap()
                .input,
            Some(2.0)
        );
    }

    #[test]
    fn unknown_thinking_control_is_rejected() {
        let err = serde_yaml::from_str::<ProviderEntry>(
            "name: p\ndefault_model: m\nthinking_control: openai\n",
        )
        .expect_err("unknown enum value must be loud");
        assert!(err.to_string().contains("openai"), "got: {err}");
    }
}
