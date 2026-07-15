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

use std::path::PathBuf;

use anyhow::{Context, Result};

/// Env var overriding the managed env-file path (tests + non-XDG setups).
pub(super) const ENV_FILE_ENV: &str = "ENTANGLEMENT_ENV_FILE";

/// The managed env-file path: `${config_dir}/entanglement/.env`, overridable via
/// `ENTANGLEMENT_ENV_FILE` (which tests point at a temp file). `None` when the
/// platform has no config dir and no override is set.
pub(super) fn env_file_path() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os(ENV_FILE_ENV) {
        return Some(PathBuf::from(p));
    }
    dirs::config_dir().map(|d| d.join("entanglement").join(".env"))
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
    let Some(path) = env_file_path() else {
        return Ok(None);
    };
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
    if !path.exists() {
        return Ok(None);
    }
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("reading env file {}", path.display()))?;
    let mut set = 0usize;
    for (key, value) in parse(&text) {
        // Process env wins: only fill a var the environment left unset. Presence
        // (even empty) counts as set, matching standard dotenv no-override.
        if std::env::var_os(&key).is_none() {
            std::env::set_var(&key, value);
            set += 1;
        }
    }
    Ok(Some((path, set)))
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

/// `load`/`scaffold`/`set_key` all mutate the process-global environment
/// (`ENTANGLEMENT_ENV_FILE` and the keys themselves), so every test touching it —
/// here and in the sibling `env_key` module — serializes under this one lock.
/// Two separate `static`s would each only serialize their own module's tests
/// against each other, not against the other module's, defeating the point.
#[cfg(test)]
pub(crate) static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

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
    fn load_sets_unset_but_never_overrides_process_env() {
        let _guard = ENV_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".env");
        std::fs::write(
            &path,
            "ENTG_TEST_UNSET=from_file\nENTG_TEST_PRESET=from_file\n",
        )
        .unwrap();

        // The real env wins for a var already set; the other is filled from file.
        std::env::set_var("ENTG_TEST_PRESET", "from_env");
        std::env::remove_var("ENTG_TEST_UNSET");
        std::env::set_var(ENV_FILE_ENV, &path);

        let (loaded, set) = load().unwrap().unwrap();
        assert_eq!(loaded, path);
        assert_eq!(set, 1, "only the unset var is filled");
        assert_eq!(std::env::var("ENTG_TEST_UNSET").unwrap(), "from_file");
        assert_eq!(std::env::var("ENTG_TEST_PRESET").unwrap(), "from_env");

        for k in ["ENTG_TEST_UNSET", "ENTG_TEST_PRESET", ENV_FILE_ENV] {
            std::env::remove_var(k);
        }
    }

    #[test]
    fn load_is_noop_when_file_absent() {
        let _guard = ENV_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var(ENV_FILE_ENV, dir.path().join("missing.env"));
        assert!(load().unwrap().is_none());
        std::env::remove_var(ENV_FILE_ENV);
    }

    #[test]
    fn scaffold_writes_template_once_then_leaves_it_alone() {
        let _guard = ENV_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join(".env");
        std::env::set_var(ENV_FILE_ENV, &path);

        let keys = vec!["ZAI_API_KEY".to_string(), "OPENAI_API_KEY".to_string()];
        let written = scaffold_if_missing(&keys).unwrap();
        assert_eq!(written.as_deref(), Some(path.as_path()));
        let body = std::fs::read_to_string(&path).unwrap();
        assert!(body.contains("#ZAI_API_KEY=\n"));

        // A pre-existing file is never overwritten.
        std::fs::write(&path, "ZAI_API_KEY=mine\n").unwrap();
        assert!(scaffold_if_missing(&keys).unwrap().is_none());
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "ZAI_API_KEY=mine\n"
        );

        std::env::remove_var(ENV_FILE_ENV);
    }
}
