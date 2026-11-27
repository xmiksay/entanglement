//! Per-session cost accounting folded from `OutEvent::Usage` (#192, #560):
//! totals split by [`UsagePurpose`] (a compaction call is billed to the
//! session but is not a conversation round — it must not move the
//! "context size" figure), per-model totals keyed by the model in effect
//! when each round landed, and the last-round state the status bar reads.
//! Held on the [`SessionView`][super::SessionView] so a resumed session
//! restores everything by replaying its persisted records through the same
//! fold.

use entanglement_core::{GenerationParams, OutEvent, UsagePurpose};

use super::usage::{cache_rebuild_notice, is_full_miss, RoundUsage};

/// Token/cost totals for one [`UsagePurpose`].
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct PurposeTotals {
    pub rounds: u64,
    pub uncached: u64,
    pub cached: u64,
    pub cache_write: u64,
    pub output: u64,
    pub cost_usd: f64,
}

impl PurposeTotals {
    /// Whole billed prompt volume: uncached + cache-read + cache-write.
    pub fn prompt_total(&self) -> u64 {
        self.uncached + self.cached + self.cache_write
    }

    fn add(&mut self, round: &RoundUsage, cost_usd: Option<f64>) {
        self.rounds += 1;
        self.uncached += round.input;
        self.cached += round.cached;
        self.cache_write += round.cache_write;
        self.output += round.output;
        self.cost_usd += cost_usd.unwrap_or(0.0);
    }
}

/// Rounds and spend attributed to one `provider/model` key.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct ModelTotals {
    pub rounds: u64,
    pub cost_usd: f64,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct CostLedger {
    turn: PurposeTotals,
    compaction: PurposeTotals,
    /// Whether any round so far carried catalog pricing. `cost_usd` alone
    /// can't tell "genuinely free" apart from "no pricing known" — this lets
    /// a fan-out rollup fall back to token-only display instead of quietly
    /// under-reporting a subtree's real spend (#560).
    cost_known: bool,
    last_round: Option<RoundUsage>,
    last_round_full_miss: bool,
    /// The session's last known generation params, from `GenerationChanged`;
    /// `None` until the first one, which only sets the baseline.
    generation: Option<GenerationParams>,
    /// A cache-keyed generation knob just changed, so the next round's full
    /// miss is the expected rebuild, not a regression — consumed by that
    /// round whatever it turns out to be.
    miss_expected: bool,
    /// The most recent **turn** round's billed prompt — the context size as
    /// the provider saw it. A compaction round summarizes a slice of the
    /// history and would misreport the live context, so it never lands here.
    last_turn_prompt: Option<u64>,
    /// `provider/model` in effect for the next round, from `ModelChanged`.
    model: Option<String>,
    /// Insertion-ordered so the `/cost` breakdown lists models in the order
    /// the session first used them.
    by_model: Vec<(String, ModelTotals)>,
}

/// Key used before any `ModelChanged` has named the session's model.
const UNKNOWN_MODEL: &str = "unknown";

impl CostLedger {
    pub fn set_model(&mut self, provider: &str, model: &str) {
        self.model = Some(format!("{provider}/{model}"));
    }

    /// Folds a `GenerationChanged`: returns the one-time notice when a
    /// cache-keyed knob changed against the known baseline (and excuses the
    /// next round's full miss), `None` for the baseline or a harmless change.
    /// The session's last confirmed generation params, if any arrived.
    pub fn generation(&self) -> Option<GenerationParams> {
        self.generation
    }

    pub fn note_generation(&mut self, next: GenerationParams) -> Option<String> {
        let notice = self
            .generation
            .as_ref()
            .and_then(|prev| cache_rebuild_notice(prev, &next));
        self.generation = Some(next);
        self.miss_expected |= notice.is_some();
        notice
    }

    /// Folds one `OutEvent::Usage`; any other event is ignored so the
    /// reducer can hand the whole event over without destructuring it.
    pub fn fold_event(&mut self, event: &OutEvent) {
        let OutEvent::Usage {
            input_tokens,
            output_tokens,
            cached_input_tokens,
            cache_write_tokens,
            cost_usd,
            purpose,
            ..
        } = event
        else {
            return;
        };
        let round = RoundUsage {
            input: *input_tokens,
            output: *output_tokens,
            cached: *cached_input_tokens,
            cache_write: *cache_write_tokens,
        };
        self.fold(round, *cost_usd, *purpose);
    }

    pub fn fold(&mut self, round: RoundUsage, cost_usd: Option<f64>, purpose: UsagePurpose) {
        let expected = std::mem::take(&mut self.miss_expected);
        self.last_round_full_miss = !expected && is_full_miss(self.last_round.as_ref(), &round);
        self.last_round = Some(round);
        self.cost_known |= cost_usd.is_some();
        match purpose {
            UsagePurpose::Turn => {
                self.last_turn_prompt = Some(round.prompt_total());
                self.turn.add(&round, cost_usd);
            }
            UsagePurpose::Compaction => self.compaction.add(&round, cost_usd),
        }
        let key = self.model.as_deref().unwrap_or(UNKNOWN_MODEL);
        let entry = match self.by_model.iter_mut().find(|(k, _)| k == key) {
            Some((_, totals)) => totals,
            None => {
                self.by_model
                    .push((key.to_string(), ModelTotals::default()));
                &mut self.by_model.last_mut().expect("just pushed").1
            }
        };
        entry.rounds += 1;
        entry.cost_usd += cost_usd.unwrap_or(0.0);
    }

    pub fn compaction(&self) -> &PurposeTotals {
        &self.compaction
    }

    /// Every `Usage` event folded, whatever its purpose.
    pub fn rounds(&self) -> u64 {
        self.turn.rounds + self.compaction.rounds
    }

    /// Whole billed prompt volume across every purpose.
    pub fn prompt_total(&self) -> u64 {
        self.turn.prompt_total() + self.compaction.prompt_total()
    }

    pub fn uncached_tokens(&self) -> u64 {
        self.turn.uncached + self.compaction.uncached
    }

    pub fn cached_tokens(&self) -> u64 {
        self.turn.cached + self.compaction.cached
    }

    pub fn cache_write_tokens(&self) -> u64 {
        self.turn.cache_write + self.compaction.cache_write
    }

    pub fn output_tokens(&self) -> u64 {
        self.turn.output + self.compaction.output
    }

    pub fn cost_usd(&self) -> f64 {
        self.turn.cost_usd + self.compaction.cost_usd
    }

    pub fn cost_known(&self) -> bool {
        self.cost_known
    }

    pub fn last_round(&self) -> Option<&RoundUsage> {
        self.last_round.as_ref()
    }

    pub fn last_round_full_miss(&self) -> bool {
        self.last_round_full_miss
    }

    pub fn last_turn_prompt(&self) -> Option<u64> {
        self.last_turn_prompt
    }

    pub fn by_model(&self) -> &[(String, ModelTotals)] {
        &self.by_model
    }
}

#[cfg(test)]
mod tests {
    use super::super::SessionView;
    use super::*;
    use entanglement_core::SessionId;

    fn sid() -> SessionId {
        SessionId::new("s1")
    }

    fn usage(
        seq: u64,
        input: u64,
        cached: u64,
        cache_write: u64,
        output: u64,
        cost_usd: Option<f64>,
        purpose: UsagePurpose,
    ) -> OutEvent {
        OutEvent::Usage {
            session: sid(),
            seq,
            input_tokens: input,
            output_tokens: output,
            cached_input_tokens: cached,
            cache_write_tokens: cache_write,
            cost_usd,
            purpose,
        }
    }

    fn model_changed(provider: &str, model: &str) -> OutEvent {
        OutEvent::ModelChanged {
            session: sid(),
            provider: provider.to_string(),
            model: model.to_string(),
            context_window: Some(200_000),
        }
    }

    #[test]
    fn totals_accumulate_and_the_pricing_flag_tracks_any_priced_round() {
        let mut v = SessionView::new();
        assert!(!v.cost_known());

        v.apply_event(usage(1, 2_000, 48_000, 0, 1_000, None, UsagePurpose::Turn));
        assert_eq!(v.input_tokens(), 50_000, "in = whole billed prompt");
        assert_eq!(v.output_tokens(), 1_000);
        assert_eq!(v.cached_input_tokens(), 48_000);
        assert_eq!(v.cost_usd(), 0.0);
        // No pricing on this round — the zero above must not read as "known free".
        assert!(!v.cost_known());

        v.apply_event(usage(2, 10_000, 0, 0, 500, Some(0.02), UsagePurpose::Turn));
        assert_eq!(v.input_tokens(), 60_000);
        assert_eq!(v.cached_input_tokens(), 48_000);
        assert!((v.cost_usd() - 0.02).abs() < 1e-9);
        assert!(v.cost_known());
    }

    #[test]
    fn full_miss_after_a_hit_is_flagged_on_the_latest_round_only() {
        let mut v = SessionView::new();
        v.apply_event(usage(1, 2_000, 48_000, 0, 1_000, None, UsagePurpose::Turn));
        assert!(!v.cost().last_round_full_miss());

        // A large-prompt round right after a cache hit, now caching nothing.
        v.apply_event(usage(2, 51_200, 0, 0, 1_100, None, UsagePurpose::Turn));
        assert!(v.cost().last_round_full_miss());
        let round = v.cost().last_round().expect("round recorded");
        assert_eq!(
            (round.input, round.output, round.cached),
            (51_200, 1_100, 0)
        );

        // The flag reflects the hit→miss transition, not a standing "cache is
        // cold" state: round 2 cached nothing, so round 3 isn't a transition.
        v.apply_event(usage(3, 500, 0, 0, 100, None, UsagePurpose::Turn));
        assert!(!v.cost().last_round_full_miss());
    }

    #[test]
    fn mixed_sequence_splits_purposes_models_and_pins_context_to_turn_rounds() {
        // turn → compaction → model switch → turn: the shape a resumed
        // session replays through the same reducer.
        let mut v = SessionView::new();
        v.apply_event(model_changed("zai", "glm-5.2"));
        v.apply_event(usage(
            1,
            1_000,
            80_000,
            4_000,
            2_000,
            Some(0.10),
            UsagePurpose::Turn,
        ));
        v.apply_event(usage(
            2,
            30_000,
            0,
            0,
            1_500,
            Some(0.05),
            UsagePurpose::Compaction,
        ));
        v.apply_event(model_changed("anthropic", "claude-sonnet-5"));
        v.apply_event(usage(
            3,
            500,
            20_000,
            0,
            700,
            Some(0.03),
            UsagePurpose::Turn,
        ));

        let c = v.cost();
        assert_eq!(c.rounds(), 3);
        assert_eq!(c.turn.rounds, 2);
        assert_eq!(c.compaction().rounds, 1);
        assert_eq!(c.turn.prompt_total(), 85_000 + 20_500);
        assert_eq!(c.compaction().prompt_total(), 30_000);
        assert_eq!(c.output_tokens(), 4_200);
        assert!((c.cost_usd() - 0.18).abs() < 1e-9);
        assert!((c.compaction().cost_usd - 0.05).abs() < 1e-9);
        // Context = the last *turn* round's billed prompt; the compaction round
        // in between never moved it.
        assert_eq!(c.last_turn_prompt(), Some(20_500));
        // Costs are float sums (0.10 + 0.05 is not bit-exactly 0.15): compare
        // model keys and round counts exactly, dollars within a tolerance.
        let by_model: Vec<(&str, u64, f64)> = c
            .by_model()
            .iter()
            .map(|(k, t)| (k.as_str(), t.rounds, t.cost_usd))
            .collect();
        let expected = [
            ("zai/glm-5.2", 2, 0.15),
            ("anthropic/claude-sonnet-5", 1, 0.03),
        ];
        assert_eq!(by_model.len(), expected.len());
        for ((key, rounds, cost), (want_key, want_rounds, want_cost)) in
            by_model.iter().zip(expected)
        {
            assert_eq!((*key, *rounds), (want_key, want_rounds));
            assert!(
                (cost - want_cost).abs() < 1e-9,
                "{key}: {cost} != {want_cost}"
            );
        }
    }

    #[test]
    fn compaction_round_does_not_move_the_context_figure_even_when_first() {
        let mut v = SessionView::new();
        v.apply_event(usage(
            1,
            30_000,
            0,
            0,
            1_500,
            None,
            UsagePurpose::Compaction,
        ));
        assert_eq!(v.cost().last_turn_prompt(), None);
        assert_eq!(v.cost().rounds(), 1);
        // The last-round line still shows it — it is the last round billed.
        assert_eq!(
            v.cost().last_round().map(|r| r.prompt_total()),
            Some(30_000)
        );
    }

    #[test]
    fn rounds_before_any_model_announcement_land_under_the_unknown_key() {
        let mut v = SessionView::new();
        v.apply_event(usage(1, 10, 0, 0, 1, None, UsagePurpose::Turn));
        assert_eq!(v.cost().by_model()[0].0, "unknown");
    }
}
