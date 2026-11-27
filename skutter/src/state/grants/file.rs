//! The managed grants file (`<state dir>/grants.yml`, ADR-0213): on-disk
//! shape and the read/write helpers.

use std::collections::HashSet;
use std::path::Path;

use serde::{Deserialize, Serialize};

use super::GrantKey;

/// On-disk shape of the managed grants file. A top-level `grants:` list keeps
/// room for future keys and lets `deny_unknown_fields` flag typos.
#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct GrantsFile {
    #[serde(default)]
    grants: Vec<GrantEntry>,
}

/// One line of the grants file: a **scoped** entry (mode + rule, written by
/// every grant recorded since ADR-0207) or a bare rule **string** — a
/// pre-ADR-0207 file, kept parseable so an existing user's file doesn't error
/// out, but loaded with `mode: None` so it never matches a live call
/// (ADR-0207 §8).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
enum GrantEntry {
    Scoped { mode: String, rule: String },
    Legacy(String),
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
    entanglement_runtime::config::atomic::atomic_write(path, &format!("{header}{body}"))
}
