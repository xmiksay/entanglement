//! Pure helpers behind cache-hit visibility (observability epic, #560): one
//! model round-trip's token counts, and the "did this round just go from a
//! cache hit to a full miss" salience check the status bar renders as a
//! warning. Kept side-effect-free and separate from [`super::SessionView`]'s
//! `record_usage` so the detection logic is unit-testable without routing a
//! whole `OutEvent` through the reducer.

/// One round's token counts, folded from `OutEvent::Usage` (deltas, not
/// cumulative — mirrors the wire event itself).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RoundUsage {
    pub input: u64,
    pub output: u64,
    pub cached: u64,
}

/// Below this input size a cache miss is unremarkable (a short follow-up
/// naturally has little to cache) and flagging it would just be noise; above
/// it, a miss right after a hit usually means the cache broke (prompt drift,
/// a busted prefix, a provider-side eviction) and is worth calling out. This
/// is a judgment-call threshold, not a measured one.
const FULL_MISS_MIN_INPUT: u64 = 5_000;

/// True when `cur` is a "full miss right after a hit": the previous round
/// cached something, this round caches nothing, and this round's input is
/// large enough that the miss isn't just noise.
pub fn is_full_miss(prev: Option<&RoundUsage>, cur: &RoundUsage) -> bool {
    let Some(prev) = prev else {
        return false;
    };
    prev.cached > 0 && cur.cached == 0 && cur.input >= FULL_MISS_MIN_INPUT
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round(input: u64, cached: u64) -> RoundUsage {
        RoundUsage {
            input,
            output: 0,
            cached,
        }
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
}
