//! The managed grants file (`${config_dir}/entanglement/grants.yml`):
//! path resolution, on-disk shape, and the locked read/write helpers.

use std::collections::HashSet;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::GrantKey;

/// Env var overriding the managed grants file path (non-XDG setups).
const GRANTS_FILE_ENV: &str = "ENTANGLEMENT_GRANTS_FILE";

/// On-disk shape of the managed grants file. A top-level `grants:` list keeps
/// room for future keys and lets `deny_unknown_fields` flag typos.
#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct GrantsFile {
    #[serde(default)]
    grants: Vec<GrantEntry>,
}

/// One line of the grants file: a **scoped** entry (mode + rule, written by
/// every grant this stage records) or a bare rule **string** — a pre-ADR-0207
/// file, kept parseable so an existing user's file doesn't error out, but
/// loaded with `mode: None` so it never matches a live call (ADR-0207 §8:
/// "existing grants.yml entries have no mode; treat them as non-matching").
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
enum GrantEntry {
    Scoped { mode: String, rule: String },
    Legacy(String),
}

/// Resolve the managed grants file path: `ENTANGLEMENT_GRANTS_FILE` wins,
/// otherwise `${config_dir}/entanglement/grants.yml`. `None` when neither is
/// available (persistence then silently no-ops).
pub(super) fn grants_file_path() -> Option<PathBuf> {
    resolve_path(|name| std::env::var_os(name), default_grants_file)
}

/// [`grants_file_path`] over an injected env lookup and config-dir fallback,
/// so the precedence is unit-testable without `set_var` (#747).
fn resolve_path(
    env: impl Fn(&str) -> Option<OsString>,
    default_file: impl FnOnce() -> Option<PathBuf>,
) -> Option<PathBuf> {
    env(GRANTS_FILE_ENV)
        .map(PathBuf::from)
        .or_else(default_file)
}

#[cfg(not(test))]
fn default_grants_file() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("entanglement").join("grants.yml"))
}

/// Guard (#747): a unit test reaching the real config dir would read the
/// developer's `grants.yml` (an `Always` grant there turns an expected Ask into
/// Allow) and could write to it — fail loudly so the test uses
/// `FileGrantStore::load_from` instead.
#[cfg(test)]
fn default_grants_file() -> Option<PathBuf> {
    panic!(
        "test resolved the real grants file; use FileGrantStore::load_from / \
         DefaultGrantStore::load_from with a temp path (or None)"
    )
}

/// Read + parse the grants file at `path` into a key set. A missing file, or any
/// read/parse error, yields an empty set (logged) — grants only widen access, so
/// a corrupt file failing closed is the safe outcome.
pub(super) fn read_grants(path: &Path) -> HashSet<GrantKey> {
    if !path.exists() {
        return HashSet::new();
    }
    let parsed = std::fs::read_to_string(path)
        .map_err(|e| format!("{e}"))
        .and_then(|t| serde_yaml::from_str::<GrantsFile>(&t).map_err(|e| format!("{e}")));
    match parsed {
        Ok(file) => file
            .grants
            .iter()
            .map(|entry| match entry {
                GrantEntry::Scoped { mode, rule } => GrantKey::from_rule(rule, Some(mode.clone())),
                // Pre-ADR-0207: no mode was ever recorded — `mode: None`
                // never matches a live call (module docs).
                GrantEntry::Legacy(rule) => GrantKey::from_rule(rule, None),
            })
            .collect(),
        Err(e) => {
            tracing::warn!("ignoring malformed grants file {}: {e}", path.display());
            HashSet::new()
        }
    }
}

/// Write `grants` to `path` as the managed YAML file, creating the config dir if
/// needed. Keys are sorted so the file is stable across writes (readable diffs,
/// no churn). A key with `mode: None` (a pre-ADR-0207 entry this process never
/// re-earned) round-trips as its original bare-string spelling rather than
/// being force-upgraded to a fabricated mode.
pub(super) fn write_grants(path: &Path, grants: &HashSet<GrantKey>) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut keys: Vec<(String, GrantEntry)> = grants
        .iter()
        .map(|key| {
            let rule = key.to_rule();
            let entry = match &key.mode {
                Some(mode) => GrantEntry::Scoped {
                    mode: mode.clone(),
                    rule: rule.clone(),
                },
                None => GrantEntry::Legacy(rule.clone()),
            };
            (rule, entry)
        })
        .collect();
    keys.sort_by(|a, b| a.0.cmp(&b.0));
    let doc = GrantsFile {
        grants: keys.into_iter().map(|(_, e)| e).collect(),
    };
    let body = serde_yaml::to_string(&doc)?;
    let header = "# entanglement — persisted \"always allow\" tool grants (#174).\n\
                  # Managed by skutter: a line is appended when you approve a tool with the\n\
                  # \"always\" scope. Each entry upgrades a matching Ask to Allow (exact match on\n\
                  # tool + argument); it never overrides a Deny. Delete a line to revoke.\n";
    crate::config::atomic::atomic_write(path, &format!("{header}{body}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lookup(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<OsString> {
        let pairs: Vec<(String, String)> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |name| {
            pairs
                .iter()
                .find(|(k, _)| k == name)
                .map(|(_, v)| OsString::from(v))
        }
    }

    #[test]
    fn env_override_wins_over_the_config_dir() {
        let path = resolve_path(lookup(&[(GRANTS_FILE_ENV, "/tmp/g.yml")]), || {
            Some(PathBuf::from("/cfg/entanglement/grants.yml"))
        });
        assert_eq!(path, Some(PathBuf::from("/tmp/g.yml")));
    }

    #[test]
    fn config_dir_is_the_fallback_and_none_stays_none() {
        let path = resolve_path(lookup(&[]), || {
            Some(PathBuf::from("/cfg/entanglement/grants.yml"))
        });
        assert_eq!(path, Some(PathBuf::from("/cfg/entanglement/grants.yml")));
        assert_eq!(resolve_path(lookup(&[]), || None), None);
    }

    #[test]
    #[should_panic(expected = "real grants file")]
    fn the_real_grants_file_is_unreachable_under_test() {
        let _ = grants_file_path_without_env();
    }

    /// The production resolution with no env override — reaches the guard.
    fn grants_file_path_without_env() -> Option<PathBuf> {
        resolve_path(lookup(&[]), default_grants_file)
    }
}
