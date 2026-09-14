//! Small shared session-display helpers used by the sidebar, the sessions
//! modal, the status bar, and the attention panel.

use entanglement_core::SessionId;
use ratatui::style::Color;

use crate::tui::session_view::{RoundUsage, SessionView};

/// Human-scale session label: the first 8 chars of the id. A v4 UUID's first
/// group is unique enough to tell sessions apart in a list; short hand-picked
/// ids (tests, embedders) pass through unchanged. Char-indexed, not
/// byte-sliced, so an arbitrary id can never split a codepoint.
pub(crate) fn short_id(id: &SessionId) -> String {
    id.to_string().chars().take(8).collect()
}

/// Compact token-count display with SI-style multipliers (k/M/G) so large
/// per-session totals stay readable in the bottom bar.
pub(crate) fn format_tokens(n: u64) -> String {
    if n < 1_000 {
        n.to_string()
    } else if n < 1_000_000 {
        format!("{:.1}k", n as f64 / 1_000.0)
    } else if n < 1_000_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else {
        format!("{:.1}G", n as f64 / 1_000_000_000.0)
    }
}

/// Session-cumulative cache-hit rate as a short label (`"62% cached"`), or
/// `None` while no input has been billed yet (#560).
pub(crate) fn format_cache_hit_rate(cached_input_tokens: u64, input_tokens: u64) -> Option<String> {
    (input_tokens > 0).then(|| {
        let pct = cached_input_tokens as f64 / input_tokens as f64 * 100.0;
        format!("{pct:.0}% cached")
    })
}

/// One round's usage as a compact line, e.g. `in 51.2k (cached 48.0k) · out
/// 1.1k` (#560) — the per-round cache-share the status bar shows next to the
/// session-cumulative totals.
pub(crate) fn format_round_usage(round: &RoundUsage) -> String {
    format!(
        "in {} (cached {}) · out {}",
        format_tokens(round.input),
        format_tokens(round.cached),
        format_tokens(round.output)
    )
}

/// A fan-out usage rollup (own + descendant sessions) as a compact line
/// (#560): a dollar figure when every contributing session carried catalog
/// pricing, else token counts only — never a total that silently dropped an
/// unpriced session's cost.
pub(crate) fn format_rollup(
    input_tokens: u64,
    output_tokens: u64,
    cost_usd: Option<f64>,
) -> String {
    let tokens = format!(
        "{} in / {} out",
        format_tokens(input_tokens),
        format_tokens(output_tokens)
    );
    match cost_usd {
        Some(cost) => format!("{tokens} (${cost:.4})"),
        None => format!("{tokens} (pricing n/a)"),
    }
}

/// The attention word (and its accent color) for a session that is parked on
/// user input: an approval prompt or an `ask_user` question. Derived from the
/// pending queues, not `AgentState` — `Status` briefly flaps to `Thinking`
/// between two parked requests (#273), so the queues are the reliable signal.
pub(crate) fn attention_word(view: &SessionView) -> Option<(&'static str, Color)> {
    if view.is_waiting_approval() {
        Some(("needs approval", Color::Yellow))
    } else if view.is_asking() {
        Some(("question", Color::Cyan))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use entanglement_core::{OutEvent, Question, Questions};

    #[test]
    fn short_id_takes_first_eight_chars_of_a_uuid() {
        let id = SessionId::new("a1b2c3d4-e5f6-7890-abcd-ef0123456789");
        assert_eq!(short_id(&id), "a1b2c3d4");
    }

    #[test]
    fn short_id_leaves_short_ids_unchanged() {
        assert_eq!(short_id(&SessionId::new("s1")), "s1");
    }

    #[test]
    fn short_id_is_multibyte_safe() {
        // Char-based take: a multibyte id must not panic or split a codepoint.
        assert_eq!(
            short_id(&SessionId::new("日本語のセッション識別子")),
            "日本語のセッショ"
        );
    }

    #[test]
    fn attention_word_distinguishes_approval_from_question() {
        let sid = SessionId::new("s1");
        let mut view = SessionView::new();
        assert_eq!(attention_word(&view), None);

        view.apply_event(OutEvent::ToolRequest {
            session: sid.clone(),
            seq: 1,
            request_id: "r1".to_string(),
            tool: "bash".to_string(),
            input: "{}".to_string(),
        });
        assert_eq!(
            attention_word(&view),
            Some(("needs approval", Color::Yellow))
        );

        let mut asking = SessionView::new();
        asking.apply_event(OutEvent::UserQuestion {
            session: sid,
            seq: 1,
            request_id: "q1".to_string(),
            questions: Questions(vec![Question {
                question: "pick one".to_string(),
                options: Vec::new(),
                multi_select: false,
            }]),
        });
        assert_eq!(attention_word(&asking), Some(("question", Color::Cyan)));
    }

    #[test]
    fn format_tokens_uses_si_multipliers() {
        assert_eq!(format_tokens(0), "0");
        assert_eq!(format_tokens(999), "999");
        assert_eq!(format_tokens(1_000), "1.0k");
        assert_eq!(format_tokens(51_200), "51.2k");
        assert_eq!(format_tokens(1_000_000), "1.0M");
    }

    #[test]
    fn cache_hit_rate_is_none_with_no_input() {
        assert_eq!(format_cache_hit_rate(0, 0), None);
    }

    #[test]
    fn cache_hit_rate_covers_zero_partial_and_full() {
        assert_eq!(
            format_cache_hit_rate(0, 1_000),
            Some("0% cached".to_string())
        );
        assert_eq!(
            format_cache_hit_rate(480, 1_000),
            Some("48% cached".to_string())
        );
        assert_eq!(
            format_cache_hit_rate(1_000, 1_000),
            Some("100% cached".to_string())
        );
    }

    #[test]
    fn round_usage_formats_the_documented_example() {
        let round = RoundUsage {
            input: 51_200,
            output: 1_100,
            cached: 48_000,
        };
        assert_eq!(
            format_round_usage(&round),
            "in 51.2k (cached 48.0k) · out 1.1k"
        );
    }

    #[test]
    fn rollup_shows_a_dollar_figure_when_pricing_is_known() {
        assert_eq!(
            format_rollup(10_000, 2_000, Some(0.0456)),
            "10.0k in / 2.0k out ($0.0456)"
        );
    }

    #[test]
    fn rollup_falls_back_to_tokens_when_pricing_is_missing() {
        assert_eq!(
            format_rollup(10_000, 2_000, None),
            "10.0k in / 2.0k out (pricing n/a)"
        );
    }
}
