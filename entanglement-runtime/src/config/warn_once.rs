//! Dedupe for the warnings every config load re-runs (#785, #813).
//!
//! The TUI reloads config on any watched write (a `grants.yml` write after an
//! "Always" approval included), so a load-time warning would otherwise repeat
//! in the log on every reload. Findings are remembered **per scope** (the
//! warning kind plus the layer files it read, i.e. per root), and the set is
//! replaced on every load: a finding that was fixed and later reintroduced
//! warns again, and a second root in the same process gets its own warnings.

use std::collections::{HashMap, HashSet};
use std::sync::{Mutex, OnceLock, PoisonError};

use super::RawLayer;

/// The scope key for `kind` over `layers`: one per root's set of layer files.
pub(super) fn scope(kind: &str, layers: &[RawLayer]) -> String {
    let mut key = kind.to_string();
    for l in layers {
        key.push('\0');
        key.push_str(&l.source);
    }
    key
}

/// Which of `findings` are new for `scope` since its previous call (in
/// order), remembering exactly `findings` as the scope's current set.
pub(super) fn fresh(scope: String, findings: &[String]) -> Vec<bool> {
    static SEEN: OnceLock<Mutex<HashMap<String, HashSet<String>>>> = OnceLock::new();
    let mut seen = SEEN
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    let previous = seen.remove(&scope).unwrap_or_default();
    let new = findings.iter().map(|f| !previous.contains(f)).collect();
    seen.insert(scope, findings.iter().cloned().collect());
    new
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_finding_warns_again_after_a_clean_load_and_per_scope() {
        let a = vec!["a".to_string()];
        assert_eq!(fresh("warn_once_t1".into(), &a), [true]);
        assert_eq!(fresh("warn_once_t1".into(), &a), [false]);
        assert!(fresh("warn_once_t1".into(), &[]).is_empty());
        assert_eq!(fresh("warn_once_t1".into(), &a), [true], "reintroduced");
        assert_eq!(fresh("warn_once_t2".into(), &a), [true], "another root");
    }
}
