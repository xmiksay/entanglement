//! Per-model reasoning-effort tiers — catalog data (`ModelEntry::effort_tiers`),
//! and the one clamp every effort-emitting wire runs before naming a tier on
//! the wire. Split out of `catalog.rs` for the 400-line file cap.
//!
//! Three catalog states matter, see [`clamp_within`]: *unset* (any tier the
//! user asks for passes through unchanged — an endpoint we know nothing
//! about), a *non-empty* list (clamp to the nearest listed tier, ties
//! rounding down), and the *empty* list (never send an effort field at all —
//! z.ai's 4.x family takes `thinking: {type: enabled}` but rejects
//! `reasoning_effort`).

use serde::{Deserialize, Serialize};

use crate::ReasoningEffort;

/// The set of [`ReasoningEffort`] tiers a model accepts, as a bitmask over
/// [`ReasoningEffort::ALL`] — `Copy`, so it rides the per-request
/// [`ThinkingSpec`](super::ThinkingSpec) without an allocation. Serializes as
/// the list of tier names (`[low, high, max]`), which is the catalog spelling.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(from = "Vec<ReasoningEffort>", into = "Vec<ReasoningEffort>")]
pub struct EffortTiers(u8);

impl EffortTiers {
    fn bit(effort: ReasoningEffort) -> u8 {
        // `ALL` is ascending and complete, so the index is the tier's rank.
        let idx = ReasoningEffort::ALL
            .iter()
            .position(|e| *e == effort)
            .expect("ReasoningEffort::ALL lists every variant");
        1 << idx
    }

    pub fn contains(self, effort: ReasoningEffort) -> bool {
        self.0 & Self::bit(effort) != 0
    }

    pub fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// Ascending iteration over the listed tiers.
    pub fn iter(self) -> impl Iterator<Item = ReasoningEffort> {
        ReasoningEffort::ALL
            .into_iter()
            .filter(move |e| self.contains(*e))
    }

    /// The cheapest listed tier; `None` for the empty set.
    pub fn lowest(self) -> Option<ReasoningEffort> {
        self.iter().next()
    }

    /// Nearest listed tier to `requested`: itself when listed, else the
    /// closest by rank with ties rounding **down** (a model that lacks the
    /// asked-for depth should not silently cost more). `None` for the empty
    /// set — that model takes no effort field.
    pub fn clamp(self, requested: ReasoningEffort) -> Option<ReasoningEffort> {
        if self.contains(requested) {
            return Some(requested);
        }
        let rank = |e: ReasoningEffort| {
            ReasoningEffort::ALL
                .iter()
                .position(|x| *x == e)
                .expect("ReasoningEffort::ALL lists every variant")
        };
        let want = rank(requested);
        // Ascending iteration + strict `<` keeps the lower tier on a tie.
        self.iter().fold(None, |best, e| match best {
            Some(b) if rank(b).abs_diff(want) <= rank(e).abs_diff(want) => Some(b),
            _ => Some(e),
        })
    }
}

impl From<Vec<ReasoningEffort>> for EffortTiers {
    fn from(list: Vec<ReasoningEffort>) -> Self {
        EffortTiers(list.into_iter().fold(0, |acc, e| acc | Self::bit(e)))
    }
}

impl From<EffortTiers> for Vec<ReasoningEffort> {
    fn from(tiers: EffortTiers) -> Self {
        tiers.iter().collect()
    }
}

/// Apply a model's catalog tiers to an effort about to be sent: `None` tiers
/// (catalog unset) pass `requested` through, `Some` clamps via
/// [`EffortTiers::clamp`] (the empty set yielding `None` — send no field).
pub fn clamp_within(
    tiers: Option<EffortTiers>,
    requested: ReasoningEffort,
) -> Option<ReasoningEffort> {
    match tiers {
        None => Some(requested),
        Some(t) => t.clamp(requested),
    }
}

/// What an effort-aware wire should emit for one request, resolved from the
/// request's own `reasoning_effort` against the model's catalog facts
/// (`effort_tiers` + `thinking_required`) — see
/// [`ThinkingSpec::resolve_effort`](super::ThinkingSpec::resolve_effort).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResolvedEffort {
    /// Whether thinking is on at all: an effort was asked for, or the model
    /// cannot run without it (`thinking_required`).
    pub enabled: bool,
    /// The tier to name on the wire, already clamped; `None` means send no
    /// effort field (thinking off, or a model that takes none).
    pub effort: Option<ReasoningEffort>,
}

pub(super) fn resolve_effort(
    requested: Option<ReasoningEffort>,
    tiers: Option<EffortTiers>,
    required: bool,
) -> ResolvedEffort {
    let enabled = requested.is_some() || required;
    // A required model with nothing asked for runs at its cheapest tier so
    // the request is valid; `Low` when the catalog lists no tiers at all.
    let wanted = requested.or_else(|| {
        if !required {
            return None;
        }
        match tiers {
            None => Some(ReasoningEffort::Low),
            Some(t) => t.lowest(),
        }
    });
    ResolvedEffort {
        enabled,
        effort: wanted.and_then(|e| clamp_within(tiers, e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ReasoningEffort::*;

    fn tiers(list: &[ReasoningEffort]) -> EffortTiers {
        EffortTiers::from(list.to_vec())
    }

    #[test]
    fn list_round_trips_through_serde_in_rank_order() {
        let t: EffortTiers = serde_yaml::from_str("[max, low, high]").unwrap();
        assert_eq!(Vec::<ReasoningEffort>::from(t), vec![Low, High, Max]);
        assert_eq!(
            serde_json::to_string(&t).unwrap(),
            r#"["low","high","max"]"#
        );
        let empty: EffortTiers = serde_yaml::from_str("[]").unwrap();
        assert!(empty.is_empty());
        assert_eq!(empty.lowest(), None);
    }

    #[test]
    fn listed_tier_passes_through() {
        let t = tiers(&[Low, High, Max]);
        assert_eq!(t.clamp(High), Some(High));
        assert_eq!(t.lowest(), Some(Low));
    }

    #[test]
    fn unlisted_tier_snaps_to_nearest_with_ties_rounding_down() {
        // [low, high, max]: medium is one step from both low and high — the
        // tie goes to the cheaper tier; xhigh is one step from both high and
        // max — same rule.
        let t = tiers(&[Low, High, Max]);
        assert_eq!(t.clamp(Medium), Some(Low));
        assert_eq!(t.clamp(XHigh), Some(High));
        // [low, medium, high, max] (no xhigh): xhigh rounds down to high.
        assert_eq!(tiers(&[Low, Medium, High, Max]).clamp(XHigh), Some(High));
        // A one-tier model clamps everything to it.
        assert_eq!(tiers(&[Medium]).clamp(Max), Some(Medium));
        assert_eq!(tiers(&[Medium]).clamp(Low), Some(Medium));
    }

    #[test]
    fn empty_set_sends_no_effort_and_unset_passes_anything() {
        assert_eq!(tiers(&[]).clamp(High), None);
        assert_eq!(clamp_within(Some(tiers(&[])), Max), None);
        assert_eq!(clamp_within(None, XHigh), Some(XHigh));
    }

    #[test]
    fn resolve_effort_off_when_nothing_asked_and_not_required() {
        let r = resolve_effort(None, Some(tiers(&[Low, High, Max])), false);
        assert_eq!(
            r,
            ResolvedEffort {
                enabled: false,
                effort: None
            }
        );
    }

    #[test]
    fn resolve_effort_required_falls_back_to_the_lowest_tier() {
        // Listed tiers: the cheapest listed one.
        let r = resolve_effort(None, Some(tiers(&[Medium, Max])), true);
        assert_eq!(
            r,
            ResolvedEffort {
                enabled: true,
                effort: Some(Medium)
            }
        );
        // No tiers known: `low`.
        assert_eq!(resolve_effort(None, None, true).effort, Some(Low));
        // Empty set: on, but no effort field.
        assert_eq!(
            resolve_effort(None, Some(tiers(&[])), true),
            ResolvedEffort {
                enabled: true,
                effort: None
            }
        );
    }

    #[test]
    fn resolve_effort_clamps_a_request_and_empty_tiers_keep_thinking_on() {
        let r = resolve_effort(Some(XHigh), Some(tiers(&[Low, High, Max])), true);
        assert_eq!(r.effort, Some(High));
        assert!(r.enabled);
        // The z.ai 4.x shape: an effort was asked (catalog default `high`)
        // so thinking is on, but the model takes no effort field.
        let r = resolve_effort(Some(High), Some(tiers(&[])), false);
        assert_eq!(
            r,
            ResolvedEffort {
                enabled: true,
                effort: None
            }
        );
    }
}
