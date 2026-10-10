//! Per-request generation knobs ([`GenerationParams`]) and the coarse
//! [`ReasoningEffort`] tier.

/// Per-request generation knobs, resolved by the head from the effective model's
/// catalog capabilities + agent profile (#191). This is the channel that makes
/// the catalog's capability metadata (`supports_temperature`/`default_temperature`/
/// `max_output_tokens`/`supports_thinking`) load-bearing instead of write-only:
/// the head only populates a field when the model actually supports it, and each
/// client maps the present fields to its wire format, omitting the rest.
///
/// Every field is optional — a `None` (or a `None`
/// [`LlmRequest::generation`](super::LlmRequest::generation)) leaves that knob
/// at the backend's own default.
#[derive(Debug, Clone, Copy, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct GenerationParams {
    /// Sampling temperature. `None` ⇒ omit (the model's default), which the head
    /// also does for a model with `supports_temperature: false`.
    pub temperature: Option<f32>,
    /// Hard cap on tokens generated this turn. OpenAI-compat sends it as
    /// `max_tokens`; Anthropic (which *requires* a cap) uses it in place of its
    /// built-in fallback. `None` ⇒ the client's own default.
    pub max_output_tokens: Option<u32>,
    /// Extended-thinking budget in tokens (Anthropic `thinking.budget_tokens`).
    /// `None` — or a model with `supports_thinking: false` — leaves thinking off.
    /// Wires without a thinking channel (OpenAI-compat) omit it.
    pub thinking_budget_tokens: Option<u32>,
    /// Coarse reasoning-effort knob (#374) — OpenAI's native `reasoning_effort`
    /// wire field. Anthropic has no effort concept of its own; its client maps
    /// this onto a thinking budget (see `anthropic.rs`'s `build_body`). Gemini
    /// maps it onto `thinkingConfig.thinkingBudget` the same way. `None` leaves
    /// the knob unset.
    pub reasoning_effort: Option<ReasoningEffort>,
}

impl GenerationParams {
    /// Overlay `overrides` onto `self`, field by field — each `Some` in
    /// `overrides` replaces the corresponding field, a `None` leaves `self`'s
    /// value untouched. The merge primitive behind a partial
    /// `InMsg::SetGeneration { overrides, .. }` (#374): `/set temperature 0.7`
    /// only touches `temperature`, leaving `max_output_tokens`/
    /// `thinking_budget_tokens`/`reasoning_effort` exactly as they were.
    pub fn apply_overrides(&mut self, overrides: GenerationParams) {
        if overrides.temperature.is_some() {
            self.temperature = overrides.temperature;
        }
        if overrides.max_output_tokens.is_some() {
            self.max_output_tokens = overrides.max_output_tokens;
        }
        if overrides.thinking_budget_tokens.is_some() {
            self.thinking_budget_tokens = overrides.thinking_budget_tokens;
        }
        if overrides.reasoning_effort.is_some() {
            self.reasoning_effort = overrides.reasoning_effort;
        }
    }
}

/// Coarse reasoning-effort knob (#374): the wire value OpenAI-compat and
/// Anthropic's adaptive `output_config.effort` both take verbatim
/// (`low|medium|high|xhigh|max`, hence `rename_all = "lowercase"` rather than
/// Rust's usual `PascalCase`). Ordered low → max so a model's supported-tier
/// clamp can pick the nearest neighbour. Wires with no effort concept
/// (Anthropic's fixed-budget shape, Gemini) map each tier onto a thinking
/// budget instead (documented at their `build_body`); the two top tiers share
/// `High`'s budget there — older models have no deeper setting to reach.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "lowercase")]
pub enum ReasoningEffort {
    Low,
    Medium,
    High,
    XHigh,
    Max,
}

impl ReasoningEffort {
    /// Every tier, ascending.
    pub const ALL: [ReasoningEffort; 5] = [
        ReasoningEffort::Low,
        ReasoningEffort::Medium,
        ReasoningEffort::High,
        ReasoningEffort::XHigh,
        ReasoningEffort::Max,
    ];

    /// The wire spelling (`serde`'s lowercase rename), for error text and logs.
    pub fn as_str(self) -> &'static str {
        match self {
            ReasoningEffort::Low => "low",
            ReasoningEffort::Medium => "medium",
            ReasoningEffort::High => "high",
            ReasoningEffort::XHigh => "xhigh",
            ReasoningEffort::Max => "max",
        }
    }

    /// Parse the wire spelling, case-insensitively.
    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|e| e.as_str().eq_ignore_ascii_case(s))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apply_overrides_touches_only_present_fields() {
        let mut base = GenerationParams {
            temperature: Some(0.2),
            max_output_tokens: Some(1024),
            thinking_budget_tokens: None,
            reasoning_effort: None,
        };
        base.apply_overrides(GenerationParams {
            temperature: Some(0.7),
            max_output_tokens: None,
            thinking_budget_tokens: None,
            reasoning_effort: Some(ReasoningEffort::High),
        });
        assert_eq!(base.temperature, Some(0.7));
        assert_eq!(base.max_output_tokens, Some(1024)); // untouched
        assert_eq!(base.thinking_budget_tokens, None);
        assert_eq!(base.reasoning_effort, Some(ReasoningEffort::High));
    }

    #[test]
    fn apply_overrides_of_default_is_a_no_op() {
        let mut base = GenerationParams {
            temperature: Some(0.2),
            max_output_tokens: Some(1024),
            thinking_budget_tokens: Some(4096),
            reasoning_effort: Some(ReasoningEffort::Low),
        };
        let before = base;
        base.apply_overrides(GenerationParams::default());
        assert_eq!(base, before);
    }

    #[test]
    fn reasoning_effort_serializes_lowercase() {
        assert_eq!(
            serde_json::to_string(&ReasoningEffort::High).unwrap(),
            "\"high\""
        );
        assert_eq!(
            serde_json::from_str::<ReasoningEffort>("\"medium\"").unwrap(),
            ReasoningEffort::Medium
        );
    }
}
