//! A freshly spawned session's starting permission mode (#763, ADR-0218).

use crate::holly::DEFAULT_MODE;

/// Where a fresh (never resumed) session's mode comes from.
pub(super) enum Lineage<'a> {
    /// A genuine fresh root: no parent, no predecessor.
    Root,
    /// A spawned child, carrying its parent's live mode (ADR-0207 §6).
    Child(&'a str),
    /// A compaction successor (ADR-0205): the same session continuing, so it
    /// keeps the source's mode and its agent's hint plays no part.
    Successor(&'a str),
}

/// The mode a fresh session starts in. A root takes its agent's
/// `default_mode` (else [`DEFAULT_MODE`]); the head's `--mode`/config
/// `mode:` then lands as an ordinary `SetMode`, so it out-ranks the hint. A
/// child takes the hint only when `ladder` proves it no wider than the
/// parent's mode — an incomparable pair inherits the parent, so a hint can
/// only ever narrow a sub-tree, never widen it.
pub(super) fn starting_mode(
    lineage: Lineage<'_>,
    default_mode: Option<&str>,
    ladder: &[String],
) -> String {
    match lineage {
        Lineage::Root => default_mode.unwrap_or(DEFAULT_MODE).to_string(),
        Lineage::Successor(source) => source.to_string(),
        Lineage::Child(parent) => {
            let rank = |m: &str| ladder.iter().position(|l| l == m);
            match default_mode {
                Some(hint) if matches!((rank(hint), rank(parent)), (Some(h), Some(p)) if h <= p) => {
                    hint.to_string()
                }
                _ => parent.to_string(),
            }
        }
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

    #[test]
    fn root_takes_the_hint_else_the_default() {
        assert_eq!(
            starting_mode(Lineage::Root, Some("plan"), &ladder()),
            "plan"
        );
        assert_eq!(starting_mode(Lineage::Root, None, &ladder()), DEFAULT_MODE);
    }

    #[test]
    fn child_takes_a_narrower_or_equal_hint() {
        let l = ladder();
        assert_eq!(
            starting_mode(Lineage::Child("build"), Some("plan"), &l),
            "plan"
        );
        assert_eq!(
            starting_mode(Lineage::Child("plan"), Some("plan"), &l),
            "plan"
        );
        assert_eq!(starting_mode(Lineage::Child("build"), None, &l), "build");
    }

    #[test]
    fn child_is_clamped_to_its_parent_when_the_hint_is_wider() {
        let l = ladder();
        assert_eq!(
            starting_mode(Lineage::Child("research"), Some("plan"), &l),
            "research"
        );
        assert_eq!(
            starting_mode(Lineage::Child("build"), Some("auto"), &l),
            "build"
        );
    }

    #[test]
    fn an_incomparable_pair_inherits_the_parent() {
        assert_eq!(
            starting_mode(Lineage::Child("build"), Some("plan"), &[]),
            "build"
        );
        assert_eq!(
            starting_mode(Lineage::Child("custom"), Some("research"), &ladder()),
            "custom"
        );
        assert_eq!(
            starting_mode(Lineage::Child("build"), Some("custom"), &ladder()),
            "build"
        );
    }

    #[test]
    fn a_successor_keeps_its_source_mode() {
        assert_eq!(
            starting_mode(Lineage::Successor("research"), Some("plan"), &ladder()),
            "research"
        );
    }
}
