//! The two mutually exclusive Anthropic extended-thinking request shapes
//! (`ModelEntry::thinking_style`) — split out of `anthropic/request.rs` for
//! the 400-line file cap. Both mutate the body in place and report whether
//! thinking ended up enabled, which decides whether `temperature` may ride.

use crate::catalog::{clamp_within, EffortTiers};
use crate::{GenerationParams, ReasoningEffort};
use serde_json::{json, Value};

/// Thinking-budget tokens for [`ReasoningEffort::High`] when the request sets no
/// explicit [`GenerationParams::thinking_budget_tokens`] (#374) — Anthropic has
/// no effort concept of its own, so `reasoning_effort` maps onto a thinking
/// tier here instead.
pub(super) const HIGH_EFFORT_THINKING_BUDGET: u32 = 32_000;
/// Thinking-budget tokens for [`ReasoningEffort::Medium`] (#374).
pub(super) const MEDIUM_EFFORT_THINKING_BUDGET: u32 = 8_000;
/// Bump amount for `max_tokens` when a thinking budget would otherwise swallow
/// the whole cap (mirrors the client's own `DEFAULT_MAX_TOKENS`
/// fallback so this module stays self-contained).
pub(super) const MAX_TOKENS_BUDGET_HEADROOM: u32 = 16_384;

/// The fixed-budget shape: `thinking: {type: "enabled", budget_tokens: N}`.
/// An explicit [`GenerationParams::thinking_budget_tokens`] always wins; absent
/// one, `reasoning_effort` (#374 — Anthropic has no effort concept of its own on
/// this shape) derives a tier default, with `Low`/unset leaving thinking off.
/// Anthropic requires `budget_tokens < max_tokens`, so the cap is bumped when the
/// budget would swallow it. Returns whether thinking was enabled.
pub(super) fn apply_budget_thinking(
    body: &mut Value,
    g: &GenerationParams,
    max_tokens: &mut u32,
) -> bool {
    let budget = g.thinking_budget_tokens.or(match g.reasoning_effort {
        // The fixed-budget shape predates the two top tiers; they get the
        // deepest budget it knows.
        Some(ReasoningEffort::High | ReasoningEffort::XHigh | ReasoningEffort::Max) => {
            Some(HIGH_EFFORT_THINKING_BUDGET)
        }
        Some(ReasoningEffort::Medium) => Some(MEDIUM_EFFORT_THINKING_BUDGET),
        Some(ReasoningEffort::Low) | None => None,
    });
    let Some(budget) = budget else {
        return false;
    };
    if budget >= *max_tokens {
        *max_tokens = budget.saturating_add(MAX_TOKENS_BUDGET_HEADROOM);
        body["max_tokens"] = json!(*max_tokens);
    }
    body["thinking"] = json!({ "type": "enabled", "budget_tokens": budget });
    true
}

/// The adaptive shape: `thinking: {type: "adaptive"}` plus `output_config.effort`,
/// where the model decides how much to think. `budget_tokens` is rejected on this
/// shape, so an explicit [`GenerationParams::thinking_budget_tokens`] is *not* an
/// enable signal here — only `reasoning_effort` is, which is the knob that
/// actually survives onto the wire. The effort is clamped to the model's
/// `effort_tiers` (ADR-0203) — an unaccepted tier is a 400; an empty tier set
/// keeps `thinking: adaptive` and omits `output_config` (the API default,
/// `high`). With no effort nothing is sent: a disable shape is never emitted,
/// since Fable 400s on one and the other adaptive models already run their
/// own default when `thinking` is omitted. There is no `max_tokens` headroom
/// bump: with no budget to swallow the cap, the existing value stands.
/// Returns whether thinking was enabled.
pub(super) fn apply_adaptive_thinking(
    body: &mut Value,
    g: &GenerationParams,
    tiers: Option<EffortTiers>,
) -> bool {
    let Some(effort) = g.reasoning_effort else {
        return false;
    };
    body["thinking"] = json!({ "type": "adaptive" });
    if let Some(effort) = clamp_within(tiers, effort) {
        body["output_config"] = json!({ "effort": effort.as_str() });
    }
    true
}
