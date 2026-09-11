//! The thinking-related catalog knobs, split out of `catalog.rs` for the
//! 400-line file cap: the Anthropic request shape ([`ThinkingStyle`]), the
//! OpenAI-compat emission format ([`ThinkingFormat`], ADR-0191), and the
//! per-request resolution of both into [`ThinkingSpec`].
//!
//! These are per-model *facts about the wire*, so they live in the catalog
//! next to the other capability flags — a user adding an endpoint picks the
//! right shape with no code change (#118's "catalog data, not hardcode").

use std::sync::Arc;

use serde::Deserialize;

use crate::catalog::{Catalog, ModelEntry};

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

/// Per-request OpenAI-wire thinking handling for one model — the pair the
/// OpenAI-compat client resolves per request against the *actual* request
/// model (mirroring the per-model concurrency resolver, #550: a `model:`-only
/// profile pin can send a request under a different model than the client was
/// built for).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ThinkingSpec {
    /// How thinking arrives (and, on replay, how it is rendered back).
    pub format: ThinkingFormat,
    /// Whether captured thinking is sent back to the provider. Default off —
    /// the OpenAI-compat wire has no requirement for it, and feeding a model
    /// its own thinking is exactly the qwen3.5 failure mode.
    pub replay: bool,
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
        let provider = provider.to_string();
        Arc::new(move |model: &str| {
            catalog
                .model(&provider, model)
                .map(|m| m.openai_thinking())
                .unwrap_or_default()
        })
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
        assert_eq!(
            entry("").openai_thinking(),
            ThinkingSpec {
                format: ThinkingFormat::Fields,
                replay: false,
            }
        );
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
