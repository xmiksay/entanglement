//! A freshly spawned session's starting permission mode (#763, ADR-0218).

use crate::holly::DEFAULT_MODE;

/// Where a fresh (never resumed) session's mode comes from.
pub(super) enum Lineage<'a> {
    /// A genuine fresh root: no parent, no predecessor.
    Root,
    /// A spawned child, carrying its parent's live mode (ADR-0207 §6).
    Child(&'a str),
    /// A compaction successor (ADR-0205): the same session continuing, so it
    /// keeps the source's mode and neither a requested mode nor the agent's
    /// hint plays any part.
    Successor(&'a str),
}

/// What a fresh session asked to start in: the `Spawn`'s own `mode` (a
/// trusted head's resolved `--mode`/config `mode:`) and its agent's
/// `default_mode` hint.
pub(super) struct Requested<'a> {
    pub(super) explicit: Option<&'a str>,
    pub(super) hint: Option<&'a str>,
}

/// `candidate` is provably no wider than `ceiling` on `ladder`. A pair not
/// both on it is incomparable, which counts as "not provably" — the safe
/// answer, since the caller then falls back to the ceiling.
fn no_wider(candidate: &str, ceiling: &str, ladder: &[String]) -> bool {
    let rank = |m: &str| ladder.iter().position(|l| l == m);
    matches!((rank(candidate), rank(ceiling)), (Some(c), Some(x)) if c <= x)
}

/// The mode a fresh session starts in.
///
/// - A root takes its explicit mode (a human's `--mode`/config choice, `auto`
///   included); else its agent's hint, but only when that hint is no wider
///   than [`DEFAULT_MODE`] — an agent file alone can never start a session
///   wider than an ordinary one, least of all unattended (`auto`); else
///   [`DEFAULT_MODE`].
/// - A child takes its explicit mode, else its hint, only when no wider than
///   its parent's mode; otherwise it inherits the parent's mode, so neither
///   can widen a sub-tree.
pub(super) fn starting_mode(
    lineage: Lineage<'_>,
    requested: Requested<'_>,
    ladder: &[String],
) -> String {
    match lineage {
        Lineage::Successor(source) => source.to_string(),
        Lineage::Root => match (requested.explicit, requested.hint) {
            (Some(explicit), _) => explicit.to_string(),
            (None, Some(hint)) if no_wider(hint, DEFAULT_MODE, ladder) => hint.to_string(),
            _ => DEFAULT_MODE.to_string(),
        },
        Lineage::Child(parent) => match requested.explicit.or(requested.hint) {
            Some(candidate) if no_wider(candidate, parent, ladder) => candidate.to_string(),
            _ => parent.to_string(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ladder() -> Vec<String> {
        ["research", "plan", "build", "auto"]
            .map(String::from)
            .to_vec()
    }

    fn hint(h: &str) -> Requested<'_> {
        Requested {
            explicit: None,
            hint: Some(h),
        }
    }

    fn explicit<'a>(e: &'a str, h: Option<&'a str>) -> Requested<'a> {
        Requested {
            explicit: Some(e),
            hint: h,
        }
    }

    const NONE: Requested<'static> = Requested {
        explicit: None,
        hint: None,
    };

    #[test]
    fn root_takes_the_hint_else_the_default() {
        assert_eq!(
            starting_mode(Lineage::Root, hint("plan"), &ladder()),
            "plan"
        );
        assert_eq!(starting_mode(Lineage::Root, NONE, &ladder()), DEFAULT_MODE);
    }

    #[test]
    fn a_root_hint_never_starts_wider_than_the_default() {
        assert_eq!(
            starting_mode(Lineage::Root, hint("auto"), &ladder()),
            DEFAULT_MODE
        );
        // Off-ladder (or no ladder at all): not provably narrower.
        assert_eq!(
            starting_mode(Lineage::Root, hint("plan"), &[]),
            DEFAULT_MODE
        );
        assert_eq!(
            starting_mode(Lineage::Root, hint("custom"), &ladder()),
            DEFAULT_MODE
        );
    }

    #[test]
    fn a_roots_explicit_mode_wins_over_the_hint() {
        let l = ladder();
        assert_eq!(
            starting_mode(Lineage::Root, explicit("build", Some("plan")), &l),
            "build"
        );
        // A human may choose `auto` explicitly; only a hint is clamped.
        assert_eq!(
            starting_mode(Lineage::Root, explicit("auto", None), &l),
            "auto"
        );
    }

    #[test]
    fn child_takes_a_narrower_or_equal_hint() {
        let l = ladder();
        assert_eq!(
            starting_mode(Lineage::Child("build"), hint("plan"), &l),
            "plan"
        );
        assert_eq!(
            starting_mode(Lineage::Child("plan"), hint("plan"), &l),
            "plan"
        );
        assert_eq!(starting_mode(Lineage::Child("build"), NONE, &l), "build");
    }

    #[test]
    fn child_is_clamped_to_its_parent_when_wider() {
        let l = ladder();
        assert_eq!(
            starting_mode(Lineage::Child("research"), hint("plan"), &l),
            "research"
        );
        assert_eq!(
            starting_mode(Lineage::Child("build"), hint("auto"), &l),
            "build"
        );
        assert_eq!(
            starting_mode(Lineage::Child("plan"), explicit("auto", None), &l),
            "plan"
        );
    }

    #[test]
    fn an_incomparable_pair_inherits_the_parent() {
        assert_eq!(
            starting_mode(Lineage::Child("build"), hint("plan"), &[]),
            "build"
        );
        assert_eq!(
            starting_mode(Lineage::Child("custom"), hint("research"), &ladder()),
            "custom"
        );
        assert_eq!(
            starting_mode(Lineage::Child("build"), hint("custom"), &ladder()),
            "build"
        );
    }

    #[test]
    fn a_successor_keeps_its_source_mode() {
        let req = explicit("auto", Some("plan"));
        assert_eq!(
            starting_mode(Lineage::Successor("research"), req, &ladder()),
            "research"
        );
    }
}
