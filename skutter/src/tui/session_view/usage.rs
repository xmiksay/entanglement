//! Pure helpers behind cache-hit visibility (observability epic, #560): one
//! model round-trip's token counts, and the "did this round just go from a
//! cache hit to a full miss" salience check the status bar renders as a
//! warning. Kept side-effect-free and separate from [`super::SessionView`]'s
//! ledger so the detection logic is unit-testable without routing a whole
//! `OutEvent` through the reducer.

use entanglement_core::GenerationParams;

/// One round's token counts, folded from `OutEvent::Usage` (deltas, not
/// cumulative — mirrors the wire event itself). `input` is the **uncached**
/// prompt portion only; the whole billed prompt is [`Self::prompt_total`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RoundUsage {
    pub input: u64,
    pub output: u64,
    pub cached: u64,
    pub cache_write: u64,
}

impl RoundUsage {
    /// The prompt as the provider billed it: uncached + cache-read +
    /// cache-write. This — not `input` alone — is the context size.
    pub fn prompt_total(&self) -> u64 {
        self.input + self.cached + self.cache_write
    }
}

/// Below this prompt size a cache miss is unremarkable (a short follow-up
/// naturally has little to cache) and flagging it would just be noise; above
/// it, a miss right after a hit usually means the cache broke (prompt drift,
/// a busted prefix, a provider-side eviction) and is worth calling out. This
/// is a judgment-call threshold, not a measured one.
const FULL_MISS_MIN_INPUT: u64 = 5_000;

/// True when `cur` is a "full miss right after a hit": the previous round
/// cached something, this round caches nothing, and this round's prompt is
/// large enough that the miss isn't just noise.
pub fn is_full_miss(prev: Option<&RoundUsage>, cur: &RoundUsage) -> bool {
    let Some(prev) = prev else {
        return false;
    };
    prev.cached > 0 && cur.cached == 0 && cur.prompt_total() >= FULL_MISS_MIN_INPUT
}

/// The one-time transcript notice for a mid-session generation change that
/// re-keys the provider's prompt cache, or `None` when nothing that does
/// changed. Reasoning effort and the thinking budget are part of the cache
/// key (measured on z.ai GLM-5.2: effort low→high dropped a fully cached
/// prompt to 0%; Anthropic documents the same for thinking/effort), while
/// temperature and the output cap are not — flagging those would cry wolf.
pub fn cache_rebuild_notice(prev: &GenerationParams, next: &GenerationParams) -> Option<String> {
    let mut changes = Vec::new();
    if prev.reasoning_effort != next.reasoning_effort {
        changes.push(match next.reasoning_effort {
            Some(effort) => format!("effort changed to {}", effort.as_str()),
            None => "effort cleared".to_string(),
        });
    }
    if prev.thinking_budget_tokens != next.thinking_budget_tokens {
        changes.push(match next.thinking_budget_tokens {
            Some(budget) => format!("thinking budget changed to {budget}"),
            None => "thinking budget cleared".to_string(),
        });
    }
    (!changes.is_empty()).then(|| {
        format!(
            "{} — the next round rebuilds the prompt cache",
            changes.join(", ")
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use entanglement_core::ReasoningEffort;

    fn round(input: u64, cached: u64) -> RoundUsage {
        RoundUsage {
            input,
            output: 0,
            cached,
            cache_write: 0,
        }
    }

    #[test]
    fn prompt_total_sums_every_billed_prompt_portion() {
        let r = RoundUsage {
            input: 1_000,
            output: 5,
            cached: 40_000,
            cache_write: 2_000,
        };
        assert_eq!(r.prompt_total(), 43_000);
    }

    #[test]
    fn no_previous_round_never_flags() {
        assert!(!is_full_miss(None, &round(50_000, 0)));
    }

    #[test]
    fn miss_after_miss_does_not_flag() {
        let prev = round(50_000, 0);
        assert!(!is_full_miss(Some(&prev), &round(50_000, 0)));
    }

    #[test]
    fn hit_after_hit_does_not_flag() {
        let prev = round(50_000, 48_000);
        assert!(!is_full_miss(Some(&prev), &round(50_000, 47_000)));
    }

    #[test]
    fn small_miss_after_hit_stays_below_threshold() {
        let prev = round(50_000, 48_000);
        assert!(!is_full_miss(Some(&prev), &round(1_000, 0)));
    }

    #[test]
    fn large_miss_right_after_a_hit_flags() {
        let prev = round(50_000, 48_000);
        assert!(is_full_miss(Some(&prev), &round(51_200, 0)));
    }

    #[test]
    fn a_large_cache_write_with_no_read_counts_as_a_miss() {
        // The whole prompt was re-written to the cache: nothing was read back,
        // so it is a miss even though `input` alone sits under the threshold.
        let prev = round(50_000, 48_000);
        let cur = RoundUsage {
            input: 100,
            output: 0,
            cached: 0,
            cache_write: 50_000,
        };
        assert!(is_full_miss(Some(&prev), &cur));
    }

    #[test]
    fn only_effort_and_thinking_budget_changes_produce_a_notice() {
        let base = GenerationParams {
            reasoning_effort: Some(ReasoningEffort::Low),
            ..GenerationParams::default()
        };
        let sampling_only = GenerationParams {
            temperature: Some(0.2),
            max_output_tokens: Some(4_096),
            ..base
        };
        assert_eq!(cache_rebuild_notice(&base, &sampling_only), None);

        let both = GenerationParams {
            reasoning_effort: None,
            thinking_budget_tokens: Some(8_000),
            ..base
        };
        assert_eq!(
            cache_rebuild_notice(&base, &both).as_deref(),
            Some(
                "effort cleared, thinking budget changed to 8000 \
                 — the next round rebuilds the prompt cache"
            )
        );
    }
}
