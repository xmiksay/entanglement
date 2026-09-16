//! The thinking-related catalog knobs, split out of `catalog.rs` for the
//! 400-line file cap: the Anthropic request shape ([`ThinkingStyle`]), the
//! OpenAI-compat emission format ([`ThinkingFormat`], ADR-0191), the
//! provider-level control object ([`ThinkingControl`]), and the per-request
//! resolution of all of them — plus the model's effort tiers and whether it
//! can run without thinking — into [`ThinkingSpec`] (OpenAI-compat and
//! Responses wires) or [`AnthropicModelSpec`] (Anthropic, construction-time).
//!
//! These are per-model *facts about the wire*, so they live in the catalog
//! next to the other capability flags — a user adding an endpoint picks the
//! right shape with no code change (#118's "catalog data, not hardcode").

use std::sync::Arc;

use serde::Deserialize;

use crate::catalog::effort::{resolve_effort, EffortTiers, ResolvedEffort};
use crate::catalog::{Catalog, ModelEntry};
use crate::ReasoningEffort;

/// Which extended-thinking request shape a model accepts on the Anthropic wire.
///
/// Anthropic replaced the fixed-budget form with an adaptive one, and the two are
/// mutually exclusive: the newer models reject `budget_tokens` outright. Which
/// shape is legal is a per-model fact, so it lives in the catalog next to the
/// other capability flags rather than being hardcoded in the client — a user can
/// add a model to their `providers.yml` and pick the right shape with no code
/// change, the same "catalog data, not hardcode" property `wire:` has (#118).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThinkingStyle {
    /// `thinking: {type: "enabled", budget_tokens: N}`. The default, so an
    /// existing user `providers.yml` keeps working untouched.
    #[default]
    Budget,
    /// `thinking: {type: "adaptive"}` plus `output_config.effort`. The model
    /// decides how much to think; `budget_tokens` is rejected.
    Adaptive,
}

/// How a model emits its thinking on the **OpenAI-compat** wire (ADR-0191).
///
/// Endpoints differ in whether their reasoning models stream thinking as a
/// structured delta field or inline it in `delta.content`:
///
/// - Structured (`reasoning` / `reasoning_content` delta fields) is what
///   z.ai, vLLM with a reasoning parser, and Ollama's parsed models do — the
///   default, and display-only: those fields never round-trip unless
///   [`replay_thinking`][ModelEntry::replay_thinking] opts in.
/// - Inline `<think>…</think>` spans in `content` is what a parser-less server
///   serving a qwen3.5-class model does. Left alone, that text commits to
///   history as if the model had said it aloud and replays verbatim on every
///   later request — the reported qwen3.5 breakage. `inline_tags` routes those
///   spans onto the reasoning rail instead: captured as a labeled
///   `ContentPart::Reasoning` block, stripped from the assistant text, and
///   replayed only when the same `replay_thinking` knob opts in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThinkingFormat {
    /// Structured `reasoning` / `reasoning_content` delta fields (the default,
    /// so every pre-existing catalog entry keeps today's byte-identical
    /// behavior).
    #[default]
    Fields,
    /// `<think>…</think>` spans inline in `delta.content`.
    InlineTags,
}

/// How an endpoint is told to think on the **OpenAI-compat** wire — a
/// provider-level fact (`ProviderEntry::thinking_control`), since it is the
/// endpoint's request dialect, not a per-model capability.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThinkingControl {
    /// z.ai's `thinking: {type: "enabled" | "disabled"}` request object,
    /// sent on every request: `enabled` (plus `reasoning_effort` when the
    /// model takes one) whenever an effort resolves or the model is
    /// `thinking_required`, `disabled` otherwise.
    Zai,
}

/// Per-request OpenAI-wire thinking handling for one model — what the
/// OpenAI-compat and Responses clients resolve per request against the
/// *actual* request model (mirroring the per-model concurrency resolver,
/// #550: a `model:`-only profile pin can send a request under a different
/// model than the client was built for).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ThinkingSpec {
    /// How thinking arrives (and, on replay, how it is rendered back).
    pub format: ThinkingFormat,
    /// Whether captured thinking is sent back to the provider. Default off —
    /// the OpenAI-compat wire has no requirement for it, and feeding a model
    /// its own thinking is exactly the qwen3.5 failure mode.
    pub replay: bool,
    /// The endpoint's thinking request dialect; `None` sends no `thinking`
    /// object (the only safe default — OpenAI proper 400s on unknown fields).
    pub control: Option<ThinkingControl>,
    /// `ModelEntry::thinking_required`: the model cannot run with thinking
    /// off, so an effort-less request still enables it at the lowest tier.
    pub required: bool,
    /// `ModelEntry::effort_tiers` — see [`EffortTiers`] for the three states.
    pub effort_tiers: Option<EffortTiers>,
}

impl ThinkingSpec {
    /// Resolve the effort a request should carry: the request's own
    /// `reasoning_effort` clamped to the model's tiers, falling back to the
    /// lowest tier for a `thinking_required` model with nothing asked.
    pub fn resolve_effort(&self, requested: Option<ReasoningEffort>) -> ResolvedEffort {
        resolve_effort(requested, self.effort_tiers, self.required)
    }
}

/// The per-model facts the **Anthropic** client binds at construction (the
/// `thinking_style` pattern): resolved once for the client's default model
/// by [`Catalog::anthropic_model_spec`] and threaded through
/// `anthropic_factory`. Caveat, as for `thinking_style` before it: a
/// `model:`-only profile pin that sends a request under a *different* model
/// id uses the default model's shape/tiers/temperature facts — the #550
/// per-request-resolver shape is not (yet) applied on this wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AnthropicModelSpec {
    /// Which extended-thinking request shape the model accepts.
    pub thinking_style: ThinkingStyle,
    /// Whether captured thinking blocks are sent back (gates replay only —
    /// capture and persistence are unconditional).
    pub replay_thinking: bool,
    /// `ModelEntry::effort_tiers`, applied to the adaptive shape's
    /// `output_config.effort`; the budget shape has no effort field.
    pub effort_tiers: Option<EffortTiers>,
    /// `ModelEntry::supports_temperature`: `false` drops `temperature` from
    /// the body even when a live `SetGeneration` set one — the current
    /// generation 400s on any sampling parameter.
    pub supports_temperature: bool,
}

impl Default for AnthropicModelSpec {
    /// The unknown-model answer: the fixed-budget shape (a pre-existing user
    /// catalog keeps emitting what it did), replay **on** (the API requires
    /// the block back on a tool round-trip, and it is inert with thinking
    /// off), no tier clamp, temperature allowed.
    fn default() -> Self {
        Self {
            thinking_style: ThinkingStyle::default(),
            replay_thinking: true,
            effort_tiers: None,
            supports_temperature: true,
        }
    }
}

impl ModelEntry {
    /// Resolved [`ThinkingStyle`], defaulting to [`ThinkingStyle::Budget`]. Not
    /// clamped by `supports_thinking`: with thinking off no `thinking` field is
    /// emitted at all, so the style is simply never consulted.
    pub fn resolved_thinking_style(&self) -> ThinkingStyle {
        self.thinking_style.unwrap_or_default()
    }

    /// Whether thinking blocks replay to this model. `wire_default` is the
    /// answer for a catalog that leaves `replay_thinking` unset — the calling
    /// client knows its own wire, a [`ModelEntry`] does not. Clamped by
    /// `supports_thinking` the way `thinking_budget_tokens` is in
    /// [`generation_params`][ModelEntry::generation_params]: a model that cannot think
    /// has nothing to replay.
    pub fn replays_thinking(&self, wire_default: bool) -> bool {
        self.supports_thinking && self.replay_thinking.unwrap_or(wire_default)
    }

    /// The OpenAI-compat thinking handling for this model (ADR-0191): its
    /// emission format plus whether captured thinking replays. The wire
    /// default for replay is **off** — unlike Anthropic, nothing on this wire
    /// requires the block back, and replaying it is what breaks
    /// parser-less qwen3.5-class endpoints.
    pub fn openai_thinking(&self) -> ThinkingSpec {
        ThinkingSpec {
            format: self.thinking_format.unwrap_or_default(),
            replay: self.replays_thinking(false),
            // Provider-level; `Catalog::thinking_spec_resolver` folds it in.
            control: None,
            required: self.thinking_required,
            effort_tiers: self.effort_tiers,
        }
    }

    /// The Anthropic construction-time facts for this model (see
    /// [`AnthropicModelSpec`]).
    pub fn anthropic_spec(&self) -> AnthropicModelSpec {
        AnthropicModelSpec {
            thinking_style: self.resolved_thinking_style(),
            replay_thinking: self.replays_thinking(true),
            effort_tiers: self.effort_tiers,
            supports_temperature: self.supports_temperature,
        }
    }
}

impl Catalog {
    /// Build a [`crate::ThinkingSpecResolver`] that looks `provider`'s
    /// `thinking_format` + `replay_thinking` up **at request time**, against
    /// whatever model id the request names — the #550 shape (a `model:`-only
    /// pin must not silently fall back to the client's construction model). A
    /// model absent from the catalog resolves to [`ThinkingSpec::default()`]
    /// (structured fields, no replay): exactly the pre-ADR-0191 behavior.
    pub fn thinking_spec_resolver(&self, provider: &str) -> crate::llm::ThinkingSpecResolver {
        let catalog = self.clone();
        let control = self.provider(provider).and_then(|p| p.thinking_control);
        let provider = provider.to_string();
        Arc::new(move |model: &str| {
            let mut spec = catalog
                .model(&provider, model)
                .map(|m| m.openai_thinking())
                .unwrap_or_default();
            // The control object is the endpoint's dialect, so it applies
            // even to a model the catalog doesn't list.
            spec.control = control;
            spec
        })
    }

    /// The [`AnthropicModelSpec`] for `(provider, model)`, resolved once at
    /// client construction; an unlisted model gets [`AnthropicModelSpec::default`].
    pub fn anthropic_model_spec(&self, provider: &str, model: &str) -> AnthropicModelSpec {
        self.model(provider, model)
            .map(ModelEntry::anthropic_spec)
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(yaml: &str) -> ModelEntry {
        // `id` is the one mandatory field; everything else under test.
        let doc = format!("id: m\n{yaml}");
        serde_yaml::from_str(&doc).expect("catalog model entry parses")
    }

    #[test]
    fn thinking_format_defaults_to_fields() {
        assert_eq!(entry("").thinking_format, None);
        assert_eq!(entry("").openai_thinking(), ThinkingSpec::default());
    }

    #[test]
    fn inline_tags_parses_and_drives_the_spec() {
        let spec =
            entry("supports_thinking: true\nthinking_format: inline_tags\n").openai_thinking();
        assert_eq!(spec.format, ThinkingFormat::InlineTags);
        assert!(!spec.replay, "replay defaults off on this wire");
        // The one knob turns both on: replay needs supports_thinking (the
        // same clamp replays_thinking applies) — set it, not guess it.
        let on =
            entry("supports_thinking: true\nthinking_format: inline_tags\nreplay_thinking: true\n")
                .openai_thinking();
        assert!(on.replay);
    }

    #[test]
    fn fields_replay_is_opt_in_via_replay_thinking() {
        let spec = entry("supports_thinking: true\nreplay_thinking: true\n").openai_thinking();
        assert_eq!(spec.format, ThinkingFormat::Fields);
        assert!(spec.replay);
        // The clamp: without supports_thinking, replay is dropped.
        assert!(!entry("replay_thinking: true\n").openai_thinking().replay);
    }

    #[test]
    fn unknown_format_value_is_rejected() {
        let err = serde_yaml::from_str::<ModelEntry>("id: m\nthinking_format: xml\n")
            .expect_err("unknown enum value must be loud, not silent");
        assert!(err.to_string().contains("xml"), "got: {err}");
    }

    #[test]
    fn resolver_folds_the_provider_control_and_model_facts() {
        use crate::ReasoningEffort::*;
        let catalog = Catalog::builtin();
        let resolve = catalog.thinking_spec_resolver("zai");
        let glm53 = resolve("glm-5.3");
        assert_eq!(glm53.control, Some(ThinkingControl::Zai));
        assert!(glm53.required);
        assert_eq!(
            glm53.effort_tiers.map(Vec::from),
            Some(vec![Low, High, Max])
        );
        assert_eq!(glm53.resolve_effort(Some(XHigh)).effort, Some(High));
        // The control is the endpoint's dialect: an unlisted model on the
        // same endpoint still gets the object, with no model facts.
        let unknown = resolve("glm-99");
        assert_eq!(unknown.control, Some(ThinkingControl::Zai));
        assert!(!unknown.required);
        assert_eq!(unknown.effort_tiers, None);
        // OpenAI proper: no control object, ever.
        assert_eq!(
            catalog.thinking_spec_resolver("openai")("gpt-4o").control,
            None
        );
    }

    #[test]
    fn anthropic_model_spec_resolves_the_catalog_facts_and_defaults_for_unknowns() {
        let catalog = Catalog::builtin();
        let fable = catalog.anthropic_model_spec("anthropic", "claude-fable-5-1");
        assert_eq!(fable.thinking_style, ThinkingStyle::Adaptive);
        assert!(fable.replay_thinking);
        assert!(!fable.supports_temperature);
        assert!(fable.effort_tiers.is_some());
        let sonnet45 = catalog.anthropic_model_spec("anthropic", "claude-sonnet-4-5");
        assert_eq!(sonnet45.thinking_style, ThinkingStyle::Budget);
        assert!(sonnet45.supports_temperature);
        assert_eq!(sonnet45.effort_tiers, None);
        assert_eq!(
            catalog.anthropic_model_spec("anthropic", "not-listed"),
            AnthropicModelSpec::default()
        );
        assert!(AnthropicModelSpec::default().replay_thinking);
    }

    #[test]
    fn resolver_misses_default_to_off() {
        let catalog = Catalog::builtin();
        let resolve = catalog.thinking_spec_resolver("ollama");
        assert_eq!(
            resolve("never-listed-model"),
            ThinkingSpec::default(),
            "an unknown model keeps the pre-ADR-0191 behavior"
        );
        assert_eq!(
            resolve("llama3.1"),
            ThinkingSpec::default(),
            "a cataloged model without the flags is identical"
        );
    }
}
