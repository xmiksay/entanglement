//! Human/model-facing text for the four built-in modes (#560 P12, ADR-0207
//! §9/§12): a short "what does this mode permit" summary per mode, used by
//! three surfaces that must agree on the same wording rather than each
//! inventing its own — `EngineConfig::modes_preamble` (folded once into the
//! cached system prompt), `explore(kind: "modes")` (so a blocked model can
//! explain itself), and the TUI `/mode` picker + `/set` dialog.
//!
//! **Must stay static** (ADR-0207 §9): the preamble sits in the provider's
//! cached prefix, so this is a hand-written constant describing the shipped
//! rule tables in `mode/builtin/*.yml`, never derived from [`super::Mode`] at
//! runtime — a `config.yml` `modes:` tuning only *adds* rules (§5), so the
//! shipped defaults this text describes still hold for every mode.

/// `(name, one-line summary)` for the four built-ins, in the same fixed
/// order [`super::ModeTable::builtin`] parses them (research → plan → build
/// → auto) — the order every rendering below preserves.
pub const MODE_SUMMARIES: [(&str, &str); 4] = [
    (
        "research",
        "read-only investigation: read/curated exec (find, grep, rg, ls, cat, \
         head, tail, wc) allowed; write and plan authorship denied; \
         everything else prompts.",
    ),
    (
        "plan",
        "analyze and author a plan, no code changes: read and propose_plan \
         allowed, writes confined to .entanglement/plans/*.md; everything \
         else prompts.",
    ),
    (
        "build",
        "implement changes: read/write/exec allowed; plan authorship and a \
         short destructive-command list (rm -rf /, force-push, hard reset, \
         ...) denied; everything else prompts.",
    ),
    (
        "auto",
        "unattended run: a curated allowlist (read, write, cargo, make, \
         git status/diff/log) runs outright and a short destructive list \
         (rm -rf, push, hard reset, publish, gh/glab writes) is denied \
         outright; anything else is refused on first call and parks a \
         bounded approval if you call it again — which expires as a denial \
         after 60s when nobody is watching.",
    ),
];

/// The static system-prompt preamble (`EngineConfig::modes_preamble`,
/// ADR-0207 §9): what modes exist and what each permits, so the model can
/// read its own boundaries once, up front, rather than only discovering them
/// call-by-call via denials. The *current* mode is a separate, per-round
/// notice core appends itself (`mode_notice`) — this text never names it.
pub fn modes_preamble() -> String {
    let mut out = String::from(
        "This session runs under a permission mode that grades every tool \
         call. Switching mode is the user's own action (/mode), or — for a \
         narrow, bounded widening — the request_mode tool. The four modes:",
    );
    for (name, summary) in MODE_SUMMARIES {
        out.push_str(&format!("\n- {name}: {summary}"));
    }
    out
}

/// `explore(kind: "modes")`'s plain-text index — the same four summaries,
/// framed as a listing rather than a preamble sentence (used by a model that
/// wants to check what a mode permits mid-session, e.g. after a denial).
pub fn modes_index() -> String {
    let mut out = String::from("MODES:");
    for (name, summary) in MODE_SUMMARIES {
        out.push_str(&format!("\n  {name} — {summary}"));
    }
    out
}

/// One mode's summary, for `describe(["mode:<name>"])`. `None` for an
/// unknown name.
pub fn mode_summary(name: &str) -> Option<&'static str> {
    MODE_SUMMARIES
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(_, s)| *s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preamble_names_every_mode_and_stays_identical_across_calls() {
        let a = modes_preamble();
        let b = modes_preamble();
        assert_eq!(a, b, "the preamble must be static (ADR-0207 §9)");
        for (name, _) in MODE_SUMMARIES {
            assert!(a.contains(name), "{a}");
        }
    }

    #[test]
    fn index_lists_every_mode() {
        let idx = modes_index();
        for (name, _) in MODE_SUMMARIES {
            assert!(idx.contains(name), "{idx}");
        }
    }

    #[test]
    fn mode_summary_resolves_known_and_rejects_unknown() {
        assert!(mode_summary("build").is_some());
        assert!(mode_summary("nonexistent").is_none());
    }
}
