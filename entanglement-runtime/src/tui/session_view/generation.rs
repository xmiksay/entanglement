//! Folds `OutEvent::GenerationChanged` into the session view: a mid-session
//! change to a knob the provider's prompt cache is keyed on is allowed, but it
//! rebuilds that cache, so the user gets one transcript notice and the
//! ledger excuses the rebuild's full miss instead of flagging it red.

use entanglement_core::GenerationParams;

use super::SessionView;

impl SessionView {
    pub(super) fn fold_generation(&mut self, generation: GenerationParams) -> bool {
        match self.cost.note_generation(generation) {
            Some(notice) => {
                self.record_status("cache", notice);
                true
            }
            None => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use entanglement_core::{OutEvent, ReasoningEffort, SessionId, UsagePurpose};

    use super::super::TranscriptEntry;
    use super::*;

    fn changed(effort: ReasoningEffort, temperature: Option<f32>) -> OutEvent {
        OutEvent::GenerationChanged {
            session: SessionId::new("s1"),
            generation: GenerationParams {
                reasoning_effort: Some(effort),
                temperature,
                ..GenerationParams::default()
            },
        }
    }

    /// A 50k-token turn round; `cached` = 0 is a full miss after a hit.
    fn round(v: &mut SessionView, seq: u64, cached: u64) {
        v.apply_event(OutEvent::Usage {
            session: SessionId::new("s1"),
            seq,
            input_tokens: 50_000 - cached,
            output_tokens: 100,
            cached_input_tokens: cached,
            cache_write_tokens: 0,
            cost_usd: None,
            purpose: UsagePurpose::Turn,
        });
    }

    fn notices(v: &SessionView) -> Vec<&str> {
        v.transcript()
            .iter()
            .filter_map(|e| match e {
                TranscriptEntry::ToolOutput { output, .. } => Some(output.as_str()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn the_first_generation_is_only_the_baseline() {
        let mut v = SessionView::new();
        assert!(!v.apply_event(changed(ReasoningEffort::Low, None)));
        assert!(v.transcript().is_empty());
    }

    #[test]
    fn an_effort_change_warns_once() {
        let mut v = SessionView::new();
        v.apply_event(changed(ReasoningEffort::Low, None));
        assert!(v.apply_event(changed(ReasoningEffort::High, None)));
        // A repeat of the now-current params (e.g. a `/show`) is no change.
        assert!(!v.apply_event(changed(ReasoningEffort::High, None)));
        assert_eq!(
            notices(&v),
            ["effort changed to high — the next round rebuilds the prompt cache"]
        );
    }

    #[test]
    fn a_temperature_only_change_neither_warns_nor_excuses_a_miss() {
        let mut v = SessionView::new();
        v.apply_event(changed(ReasoningEffort::Low, None));
        round(&mut v, 1, 49_000);
        assert!(!v.apply_event(changed(ReasoningEffort::Low, Some(0.2))));
        assert!(notices(&v).is_empty());
        round(&mut v, 2, 0);
        assert!(v.cost().last_round_full_miss());
    }

    #[test]
    fn only_the_round_right_after_the_change_has_its_full_miss_excused() {
        let mut v = SessionView::new();
        v.apply_event(changed(ReasoningEffort::Low, None));
        round(&mut v, 1, 49_000);
        v.apply_event(changed(ReasoningEffort::High, None));
        round(&mut v, 2, 0);
        assert!(!v.cost().last_round_full_miss(), "expected rebuild");
        round(&mut v, 3, 49_000);
        round(&mut v, 4, 0);
        assert!(v.cost().last_round_full_miss(), "later misses flag again");
    }

    #[test]
    fn the_excuse_is_spent_even_when_the_next_round_still_hits() {
        let mut v = SessionView::new();
        v.apply_event(changed(ReasoningEffort::Low, None));
        round(&mut v, 1, 49_000);
        v.apply_event(changed(ReasoningEffort::High, None));
        round(&mut v, 2, 49_000);
        round(&mut v, 3, 0);
        assert!(v.cost().last_round_full_miss());
    }
}
