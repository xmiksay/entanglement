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

use std::path::Path;

use super::{ConfigLayer, RawLayer};

/// The scope key for `kind` over `layers` loaded for `root`. Keyed on the
/// root itself, not on which project file happened to exist, so deleting and
/// restoring a project `config.yml` with the same finding resets and then
/// warns again (#817 review); the non-project layer files still distinguish
/// e.g. a different user file. With no root (embedded defaults only) every
/// layer file keys it.
pub(super) fn scope(kind: &str, root: Option<&Path>, layers: &[RawLayer]) -> String {
    let mut key = kind.to_string();
    if let Some(root) = root {
        key.push('\0');
        key.push_str(&root.display().to_string());
    }
    for l in layers
        .iter()
        .filter(|l| root.is_none() || l.layer != ConfigLayer::Project)
    {
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

    fn layer(layer: ConfigLayer, source: &str) -> RawLayer {
        RawLayer {
            layer,
            source: source.into(),
            doc: serde_yaml::Value::Null,
        }
    }

    /// Deleting the project file (only the user layer left) keeps the same
    /// scope, so the clean load resets it and a restored finding warns again.
    #[test]
    fn the_scope_is_the_root_whether_or_not_the_project_file_exists() {
        let root = Path::new("/warn_once_root");
        let user = layer(ConfigLayer::User, "/u/config.yml");
        let project = layer(
            ConfigLayer::Project,
            "/warn_once_root/.entanglement/config.yml",
        );
        let with = scope("k", Some(root), &[user.clone(), project]);
        let without = scope("k", Some(root), std::slice::from_ref(&user));
        assert_eq!(with, without);
        assert_ne!(with, scope("k", Some(Path::new("/other")), &[user]));
        let a = vec!["a".to_string()];
        assert_eq!(fresh(with.clone(), &a), [true]);
        assert!(fresh(without, &[]).is_empty());
        assert_eq!(fresh(with, &a), [true], "restored file warns again");
    }
}
