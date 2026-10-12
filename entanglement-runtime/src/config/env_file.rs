//! Managed provider-key env file (#220).
//!
//! Provider API keys used to come only from the process environment. This adds a
//! managed `${config_dir}/entanglement/.env` (override the path via
//! `ENTANGLEMENT_ENV_FILE`) holding the provider key vars — a home for
//! `ZAI_API_KEY`, `OPENAI_API_KEY`, `ANTHROPIC_API_KEY`, … outside any repo.
//!
//! Two operations, both best-effort at startup (a read-only home must never fail
//! the process):
//!
//! - [`scaffold_if_missing`] drops a commented starter template listing the
//!   catalog's known key names on first run, so the file is a discoverable
//!   starting point rather than something the user has to invent. Every line is
//!   commented out, so an untouched scaffold sets nothing — mirroring the config
//!   scaffold (#219).
//! - [`load`] reads `KEY=VALUE` lines and exports each into the process
//!   environment **only if that var is not already set** — the real process env
//!   always wins (env > file). Malformed lines are skipped, not fatal.

use std::ffi::OsString;
use std::path::PathBuf;

use anyhow::{Context, Result};

/// Env var overriding the managed env-file path (tests + non-XDG setups).
pub(super) const ENV_FILE_ENV: &str = "ENTANGLEMENT_ENV_FILE";

/// The managed env-file path: `${config_dir}/entanglement/.env`, overridable via
/// `ENTANGLEMENT_ENV_FILE` (which tests point at a temp file). `None` when the
/// platform has no config dir and no override is set.
pub fn env_file_path() -> Option<PathBuf> {
    resolve_path(|name| std::env::var_os(name), crate::user_dirs::config_dir)
}

/// [`env_file_path`] over an injected env lookup and config dir, so the
/// precedence is unit-testable without mutating process env (#773).
fn resolve_path(
    env: impl Fn(&str) -> Option<OsString>,
    config_dir: impl FnOnce() -> Option<PathBuf>,
) -> Option<PathBuf> {
    env(ENV_FILE_ENV)
        .map(PathBuf::from)
        .or_else(|| config_dir().map(|d| d.join("entanglement").join(".env")))
}

/// First-run scaffold (#220): if the managed env file does not exist yet, write a
/// commented template listing `key_names` (the catalog's known key vars) so the
/// file is a discoverable place to drop credentials. Every line is commented, so
/// the scaffold changes nothing until a user fills a value in.
///
/// Returns the written path on success, `None` if a file was already present (or
/// the config dir is unknown). A write error is returned for the caller to log —
/// startup must not fail because the home directory is read-only.
pub fn scaffold_if_missing(key_names: &[String]) -> Result<Option<PathBuf>> {
    env_file_path().map_or(Ok(None), |path| scaffold_at(path, key_names))
}

/// [`scaffold_if_missing`] at an explicit `path`.
fn scaffold_at(path: PathBuf, key_names: &[String]) -> Result<Option<PathBuf>> {
    if path.exists() {
        return Ok(None);
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating config dir {}", parent.display()))?;
    }
    std::fs::write(&path, template(key_names))
        .with_context(|| format!("writing env file {}", path.display()))?;
    Ok(Some(path))
}

/// The commented starter template: a header plus one `#KEY=` line per known key.
pub(super) fn template(key_names: &[String]) -> String {
    let mut out = String::from(
        "# entanglement — provider API keys.\n\
         #\n\
         # This file was scaffolded on first run (#220). Uncomment a line and give\n\
         # it a value to set that provider's key. Keep this file out of any repo.\n\
         # A matching variable already set in the real environment always wins over\n\
         # this file (env > file).\n\
         #\n\
         # Secrets that config.yml `http_auth:` references as ${VAR} (the rhai\n\
         # http() binding's hidden credentials, ADR-0227) belong here too, e.g.\n\
         # GITHUB_TOKEN=...\n\
         #\n",
    );
    for key in key_names {
        out.push('#');
        out.push_str(key);
        out.push_str("=\n");
    }
    out
}

/// Load `KEY=VALUE` pairs from the managed env file into the process environment,
/// **without overriding** variables the real environment already set (#220). A
/// missing file is fine (returns `None`); an unreadable file is a loud error.
///
/// Returns the loaded path and how many variables it actually set (skipped keys —
/// already present in the environment — are not counted), for the caller to log.
pub fn load() -> Result<Option<(PathBuf, usize)>> {
    let Some(path) = env_file_path() else {
        return Ok(None);
    };
    load_at(
        path,
        |key| std::env::var_os(key).is_some(),
        |key, value| std::env::set_var(key, value),
    )
}

/// [`load`] from an explicit `path` into an injected environment (`is_set`
/// probes it, `set` exports into it), so the env-wins rule is unit-testable
/// without mutating the process env (#773).
fn load_at(
    path: PathBuf,
    is_set: impl Fn(&str) -> bool,
    mut set: impl FnMut(&str, &str),
) -> Result<Option<(PathBuf, usize)>> {
    if !path.exists() {
        return Ok(None);
    }
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("reading env file {}", path.display()))?;
    let mut count = 0usize;
    for (key, value) in parse(&text) {
        // Process env wins: only fill a var the environment left unset. Presence
        // (even empty) counts as set, matching standard dotenv no-override.
        if !is_set(&key) {
            set(&key, &value);
            count += 1;
        }
    }
    Ok(Some((path, count)))
}

/// Parse `KEY=VALUE` lines: blank lines and `#` comments are skipped, the key is
/// split on the first `=`, both sides trimmed, and a single layer of matching
/// surrounding quotes stripped from the value. Lines without `=` or with an empty
/// key are skipped (malformed, not fatal — a managed env file is not user code).
pub(super) fn parse(text: &str) -> Vec<(String, String)> {
    text.lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                return None;
            }
            let (key, value) = line.split_once('=')?;
            let key = key.trim();
            if key.is_empty() {
                return None;
            }
            Some((key.to_string(), unquote(value.trim()).to_string()))
        })
        .collect()
}

/// Strip one layer of matching single or double quotes wrapping `value`.
fn unquote(value: &str) -> &str {
    let bytes = value.as_bytes();
    if bytes.len() >= 2 {
        let (first, last) = (bytes[0], bytes[bytes.len() - 1]);
        if first == last && (first == b'"' || first == b'\'') {
            return &value[1..value.len() - 1];
        }
    }
    value
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_skips_comments_blanks_and_malformed() {
        let text = "\
# a comment
   # indented comment

ZAI_API_KEY=zzz
  OPENAI_API_KEY = spaced
no_equals_here
=missing_key
ANTHROPIC_API_KEY=\"quoted value\"
SINGLE='single quoted'
";
        let got = parse(text);
        assert_eq!(
            got,
            vec![
                ("ZAI_API_KEY".to_string(), "zzz".to_string()),
                ("OPENAI_API_KEY".to_string(), "spaced".to_string()),
                ("ANTHROPIC_API_KEY".to_string(), "quoted value".to_string()),
                ("SINGLE".to_string(), "single quoted".to_string()),
            ]
        );
    }

    #[test]
    fn value_may_contain_equals() {
        assert_eq!(
            parse("KEY=a=b=c"),
            vec![("KEY".to_string(), "a=b=c".to_string())]
        );
    }

    #[test]
    fn template_lists_every_key_commented() {
        let keys = vec!["ZAI_API_KEY".to_string(), "OPENAI_API_KEY".to_string()];
        let tpl = template(&keys);
        assert!(tpl.contains("#ZAI_API_KEY=\n"));
        assert!(tpl.contains("#OPENAI_API_KEY=\n"));
        // Fully commented ⇒ parsing it back sets nothing.
        assert!(parse(&tpl).is_empty());
    }

    #[test]
    fn unquote_only_strips_matching_pairs() {
        assert_eq!(unquote("\"x\""), "x");
        assert_eq!(unquote("'x'"), "x");
        assert_eq!(unquote("\"x'"), "\"x'");
        assert_eq!(unquote("x"), "x");
        assert_eq!(unquote("\"\""), "");
    }

    #[test]
    fn the_env_override_wins_over_the_config_dir() {
        let path = resolve_path(
            |n| (n == ENV_FILE_ENV).then(|| "/srv/keys.env".into()),
            || panic!("the default must not be consulted"),
        );
        assert_eq!(path, Some(PathBuf::from("/srv/keys.env")));
        let path = resolve_path(|_| None, || Some(PathBuf::from("/cfg")));
        assert_eq!(path, Some(PathBuf::from("/cfg/entanglement/.env")));
        // ADR-0213: the managed-state dir never moves this hand-edited input.
        let path = resolve_path(
            |n| (n == super::super::state_dir::STATE_DIR_ENV).then(|| "/state".into()),
            || Some(PathBuf::from("/cfg")),
        );
        assert_eq!(path, Some(PathBuf::from("/cfg/entanglement/.env")));
    }

    #[test]
    fn load_sets_unset_but_never_overrides_the_env() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".env");
        std::fs::write(&path, "UNSET=from_file\nPRESET=from_file\n").unwrap();

        // The real env wins for a var already set; the other is filled from file.
        let env = std::cell::RefCell::new(std::collections::HashMap::from([(
            "PRESET".to_string(),
            "from_env".to_string(),
        )]));
        let (loaded, set) = load_at(
            path.clone(),
            |k| env.borrow().contains_key(k),
            |k, v| {
                env.borrow_mut().insert(k.to_string(), v.to_string());
            },
        )
        .unwrap()
        .unwrap();
        assert_eq!(loaded, path);
        assert_eq!(set, 1, "only the unset var is filled");
        assert_eq!(env.borrow()["UNSET"], "from_file");
        assert_eq!(env.borrow()["PRESET"], "from_env");
    }

    #[test]
    fn load_is_noop_when_file_absent() {
        let dir = tempfile::tempdir().unwrap();
        let loaded = load_at(
            dir.path().join("missing.env"),
            |_| false,
            |_, _| panic!("nothing to set"),
        );
        assert!(loaded.unwrap().is_none());
    }

    #[test]
    fn scaffold_writes_template_once_then_leaves_it_alone() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join(".env");

        let keys = vec!["ZAI_API_KEY".to_string(), "OPENAI_API_KEY".to_string()];
        let written = scaffold_at(path.clone(), &keys).unwrap();
        assert_eq!(written.as_deref(), Some(path.as_path()));
        let body = std::fs::read_to_string(&path).unwrap();
        assert!(body.contains("#ZAI_API_KEY=\n"));

        // A pre-existing file is never overwritten.
        std::fs::write(&path, "ZAI_API_KEY=mine\n").unwrap();
        assert!(scaffold_at(path.clone(), &keys).unwrap().is_none());
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "ZAI_API_KEY=mine\n"
        );
    }
}
