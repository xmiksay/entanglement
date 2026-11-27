//! `/cost` (ADR-0202 §7): the session's full spend breakdown — cumulative
//! prompt split, the compaction share, per-model rows, and the spawned-child
//! rollup — recorded into the transcript as a status notice. The status bar
//! keeps only what matters for the *next* request (context, spend, last
//! round); everything cumulative lives here. Pure formatting, no `App`.

use crate::tui::format::{format_cache_hit_rate, format_tokens};
use crate::tui::session_view::CostLedger;
use crate::tui::sessions::UsageRollup;

/// Renders the report. `subtree` is the active session's [`UsageRollup`]
/// (own + descendants); its line appears only when the session actually has
/// children.
pub(crate) fn cost_report(ledger: &CostLedger, subtree: &UsageRollup) -> String {
    let mut lines = Vec::new();
    if ledger.rounds() == 0 {
        lines.push("cost: no usage recorded yet".to_string());
    } else {
        let priced = ledger.cost_known();
        let prompt = ledger.prompt_total();
        let total = spend(priced, ledger.cost_usd(), prompt, ledger.output_tokens());
        let rounds = plural(ledger.rounds(), "round");
        lines.push(
            match format_cache_hit_rate(ledger.cached_tokens(), prompt) {
                Some(rate) => format!("cost: {total} over {rounds} ({rate})"),
                None => format!("cost: {total} over {rounds}"),
            },
        );
        lines.push(format!(
            "  prompt: {} · uncached {} · cached {} · cache-write {} · output {}",
            format_tokens(prompt),
            format_tokens(ledger.uncached_tokens()),
            format_tokens(ledger.cached_tokens()),
            format_tokens(ledger.cache_write_tokens()),
            format_tokens(ledger.output_tokens()),
        ));
        let compaction = ledger.compaction();
        if compaction.rounds > 0 {
            lines.push(format!(
                "  compaction: {} · {}",
                plural(compaction.rounds, "call"),
                spend(
                    priced,
                    compaction.cost_usd,
                    compaction.prompt_total(),
                    compaction.output
                ),
            ));
        }
        let models: Vec<String> = ledger
            .by_model()
            .iter()
            .map(|(key, totals)| {
                let rounds = plural(totals.rounds, "round");
                if priced {
                    format!("{key} ${:.2} ({rounds})", totals.cost_usd)
                } else {
                    format!("{key} ({rounds})")
                }
            })
            .collect();
        lines.push(format!("  by model: {}", models.join(" · ")));
    }
    let children = subtree.sessions.saturating_sub(1);
    if children > 0 {
        let total = match subtree.cost_usd {
            Some(cost) => format!("${cost:.4}"),
            None => tokens(subtree.input_tokens, subtree.output_tokens),
        };
        lines.push(format!(
            "  subtree: {total} (own + {})",
            plural(children as u64, "child session")
        ));
    }
    lines.join("\n")
}

/// Dollars when pricing is known, else the token volume — an unpriced
/// endpoint must never read as free.
fn spend(priced: bool, cost_usd: f64, prompt: u64, output: u64) -> String {
    if priced {
        format!("${cost_usd:.4}")
    } else {
        tokens(prompt, output)
    }
}

fn tokens(prompt: u64, output: u64) -> String {
    format!(
        "{} in / {} out",
        format_tokens(prompt),
        format_tokens(output)
    )
}

fn plural(n: u64, word: &str) -> String {
    format!("{n} {word}{}", if n == 1 { "" } else { "s" })
}

#[cfg(test)]
mod tests {
    use entanglement_core::UsagePurpose;

    use super::*;
    use crate::tui::session_view::RoundUsage;

    fn round(input: u64, cached: u64, cache_write: u64, output: u64) -> RoundUsage {
        RoundUsage {
            input,
            output,
            cached,
            cache_write,
        }
    }

    fn own_only() -> UsageRollup {
        UsageRollup {
            sessions: 1,
            ..UsageRollup::default()
        }
    }

    #[test]
    fn no_usage_is_a_single_line() {
        assert_eq!(
            cost_report(&CostLedger::default(), &own_only()),
            "cost: no usage recorded yet"
        );
    }

    #[test]
    fn a_priced_multi_model_session_with_compaction_and_children_renders_every_line() {
        let mut ledger = CostLedger::default();
        ledger.set_model("zai", "glm-5.2");
        ledger.fold(
            round(40_000, 1_700_000, 90_000, 20_000),
            Some(1.00),
            UsagePurpose::Turn,
        );
        ledger.fold(
            round(1_000, 0, 6_000, 1_000),
            Some(0.10),
            UsagePurpose::Compaction,
        );
        ledger.set_model("anthropic", "claude-sonnet-5");
        ledger.fold(
            round(200, 100_000, 0, 3_100),
            Some(0.13),
            UsagePurpose::Turn,
        );
        let subtree = UsageRollup {
            input_tokens: 3_000_000,
            output_tokens: 40_000,
            cached_input_tokens: 0,
            cost_usd: Some(1.9876),
            sessions: 4,
        };
        let expected = [
            "cost: $1.2300 over 3 rounds (93% cached)",
            "  prompt: 1.9M · uncached 41.2k · cached 1.8M · cache-write 96.0k · output 24.1k",
            "  compaction: 1 call · $0.1000",
            "  by model: zai/glm-5.2 $1.10 (2 rounds) · anthropic/claude-sonnet-5 $0.13 (1 round)",
            "  subtree: $1.9876 (own + 3 child sessions)",
        ];
        assert_eq!(cost_report(&ledger, &subtree), expected.join("\n"));
    }

    #[test]
    fn unpriced_usage_shows_tokens_and_a_childless_session_has_no_subtree() {
        let mut ledger = CostLedger::default();
        ledger.fold(round(10_000, 0, 0, 2_000), None, UsagePurpose::Turn);
        let expected = [
            "cost: 10.0k in / 2.0k out over 1 round (0% cached)",
            "  prompt: 10.0k · uncached 10.0k · cached 0 · cache-write 0 · output 2.0k",
            "  by model: unknown (1 round)",
        ];
        assert_eq!(cost_report(&ledger, &own_only()), expected.join("\n"));
    }

    #[test]
    fn an_unpriced_subtree_falls_back_to_tokens() {
        let subtree = UsageRollup {
            input_tokens: 5_000,
            output_tokens: 500,
            cached_input_tokens: 0,
            cost_usd: None,
            sessions: 2,
        };
        assert_eq!(
            cost_report(&CostLedger::default(), &subtree),
            "cost: no usage recorded yet\n  subtree: 5.0k in / 500 out (own + 1 child session)"
        );
    }
}
