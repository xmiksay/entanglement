//! The status bar's usage segments (ADR-0202 §7): `ctx` (the last turn
//! round's billed prompt against the model window), the session's spend, and
//! the `last:` round with its cache share. Session-cumulative in/out totals
//! live in `/cost` instead — on a cached conversation they grow quadratically
//! and say nothing about the next request.

use ratatui::{
    style::{Color, Style},
    text::Span,
};

use crate::tui::format::{format_context, format_round_usage, format_session_cost};
use crate::tui::session_view::CostLedger;

/// Each segment is preceded by its own `" | "` separator, so an omitted one
/// (no turn round yet → no `ctx`) never leaves a dangling bar.
pub(super) fn usage_spans(ledger: &CostLedger, window: Option<u32>) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    if let Some(prompt) = ledger.last_turn_prompt() {
        push(
            &mut spans,
            format!("ctx {}", format_context(prompt, window)),
            Style::default().fg(Color::Cyan),
        );
    }
    if ledger.rounds() > 0 {
        let spend = format_session_cost(
            ledger.cost_known(),
            ledger.cost_usd(),
            ledger.prompt_total(),
            ledger.output_tokens(),
        );
        push(&mut spans, spend, Style::default().fg(Color::Yellow));
    }
    if let Some(round) = ledger.last_round() {
        // A full miss right after a hit renders in the throttle indicator's
        // red/bold, so a cache regression is as loud as a rate-limit one.
        let style = if ledger.last_round_full_miss() {
            Style::default().fg(Color::Red).bold()
        } else {
            Style::default().fg(Color::Yellow).dim()
        };
        push(
            &mut spans,
            format!("last: {}", format_round_usage(round)),
            style,
        );
    }
    spans
}

fn push(spans: &mut Vec<Span<'static>>, text: String, style: Style) {
    spans.push(Span::raw(" | "));
    spans.push(Span::styled(text, style));
}

#[cfg(test)]
mod tests {
    use entanglement_core::UsagePurpose;
    use ratatui::style::Modifier;

    use super::*;
    use crate::tui::session_view::RoundUsage;

    fn text(spans: &[Span]) -> String {
        spans.iter().map(|s| s.content.as_ref()).collect()
    }

    fn round(input: u64, cached: u64, output: u64) -> RoundUsage {
        RoundUsage {
            input,
            output,
            cached,
            cache_write: 0,
        }
    }

    #[test]
    fn nothing_renders_before_the_first_round() {
        assert!(usage_spans(&CostLedger::default(), Some(200_000)).is_empty());
    }

    #[test]
    fn a_priced_turn_round_renders_ctx_cost_and_last() {
        let mut ledger = CostLedger::default();
        ledger.fold(
            round(3_200, 48_000, 1_100),
            Some(1.2345),
            UsagePurpose::Turn,
        );
        assert_eq!(
            text(&usage_spans(&ledger, Some(200_000))),
            " | ctx 51.2k/200k 26% | $1.2345 | last: 51.2k in (94% cached) · out 1.1k"
        );
    }

    #[test]
    fn a_compaction_only_session_has_no_ctx_and_unpriced_spend_shows_tokens() {
        let mut ledger = CostLedger::default();
        ledger.fold(round(30_000, 0, 1_500), None, UsagePurpose::Compaction);
        assert_eq!(
            text(&usage_spans(&ledger, Some(200_000))),
            " | 30.0k in / 1.5k out | last: 30.0k in (0% cached) · out 1.5k"
        );
    }

    #[test]
    fn a_full_miss_after_a_hit_renders_the_last_segment_red_bold() {
        let mut ledger = CostLedger::default();
        ledger.fold(round(2_000, 48_000, 10), None, UsagePurpose::Turn);
        ledger.fold(round(51_200, 0, 10), None, UsagePurpose::Turn);
        let spans = usage_spans(&ledger, None);
        let last = spans.last().expect("last segment");
        assert_eq!(last.style.fg, Some(Color::Red));
        assert!(last.style.add_modifier.contains(Modifier::BOLD));
    }
}
