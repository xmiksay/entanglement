//! Writer for the managed provider-key env file (#304, ADR-0073).
//!
//! [`env_file`](super::env_file) is scaffold-and-read only: it drops a commented
//! template on first run and loads `KEY=VALUE` pairs into the process env (env >
//! file). Setting a key meant hand-editing the file. This is the missing
//! **writer** — one shared entry point both surfaces (`skutter config set-key`
//! and the TUI `/key` dialog) drive.
//!
//! - [`upsert`] is the pure text transform: given the file's current text, a key,
//!   and a value, it returns the new text. It replaces the first *live* `KEY=`
//!   line (first-occurrence-wins, matching [`super::env_file`]'s load semantics),
//!   else replaces the first `#KEY=` / `# KEY=` commented placeholder, else
//!   appends. Every other line is preserved byte-for-byte.
//! - [`set_key`] wraps `upsert` with the I/O: it resolves the managed path
//!   (loud error when there is none), creates the file from the template when
//!   absent, and writes atomically (temp file in the same dir + rename, `0o600`
//!   on unix). Empty or newline-containing values are refused — they would either
//!   set nothing or corrupt the single-line `KEY=VALUE` format.

use std::path::PathBuf;

use anyhow::{bail, Context, Result};

use super::atomic::atomic_write;
use super::env_file::{env_file_path, template, ENV_FILE_ENV};

/// Upsert `key=value` into env-file `text`, returning the new text.
///
/// Resolution order (first match wins), mirroring how [`super::env_file`] reads
/// the file back:
///
/// 1. The first **live** `KEY=…` line (uncommented, key matches) is replaced.
///    First-occurrence-wins matches `load()`, which fills a var from the first
///    line it sees and skips later duplicates.
/// 2. Else the first commented **placeholder** (`#KEY=` or `# KEY=`) is replaced
///    in place, so a scaffolded file keeps its ordering.
/// 3. Else `key=value` is appended.
///
/// All untouched lines — including their exact line terminators — are preserved
/// byte-for-byte.
pub fn upsert(text: &str, key: &str, value: &str) -> String {
    let new_line = format!("{key}={value}");
    let segments: Vec<&str> = text.split_inclusive('\n').collect();

    // Pass 1: a live `KEY=` line (uncommented). Pass 2 (fallback): a commented
    // `#KEY=` placeholder. Both compare the key trimmed, before the first `=`.
    let target = segments
        .iter()
        .position(|seg| live_key_matches(seg, key))
        .or_else(|| {
            segments
                .iter()
                .position(|seg| placeholder_key_matches(seg, key))
        });

    match target {
        Some(i) => {
            let mut out = String::with_capacity(text.len() + new_line.len());
            for (j, seg) in segments.iter().enumerate() {
                if j == i {
                    out.push_str(&new_line);
                    // Preserve the original line's terminator (the last line may
                    // have had none).
                    if seg.ends_with('\n') {
                        out.push('\n');
                    }
                } else {
                    out.push_str(seg);
                }
            }
            out
        }
        None => {
            let mut out = text.to_string();
            if !out.is_empty() && !out.ends_with('\n') {
                out.push('\n');
            }
            out.push_str(&new_line);
            out.push('\n');
            out
        }
    }
}

/// Whether `segment` is an uncommented `KEY=…` line whose key equals `key`.
fn live_key_matches(segment: &str, key: &str) -> bool {
    let trimmed = segment.trim();
    if trimmed.starts_with('#') {
        return false;
    }
    trimmed
        .split_once('=')
        .is_some_and(|(k, _)| k.trim() == key)
}

/// Whether `segment` is a commented `#KEY=` / `# KEY=` placeholder for `key`.
fn placeholder_key_matches(segment: &str, key: &str) -> bool {
    let trimmed = segment.trim();
    let Some(rest) = trimmed.strip_prefix('#') else {
        return false;
    };
    rest.trim_start()
        .split_once('=')
        .is_some_and(|(k, _)| k.trim() == key)
}

/// Persist `value` for the env var `key_name` into the managed env file, creating
/// it from the commented template when absent. Returns the file path.
///
/// The write is atomic — content goes to a temp file in the same directory,
/// tightened to `0o600` on unix, then renamed over the target — so a concurrent
/// reader never sees a half-written file. A `None` managed path (no config dir
/// and no `ENTANGLEMENT_ENV_FILE`) is a loud error, as are empty / newline-bearing
/// values (they cannot be represented in the single-line `KEY=VALUE` format).
pub fn set_key(key_name: &str, value: &str) -> Result<PathBuf> {
    if value.is_empty() {
        bail!("refusing to write an empty value for {key_name}");
    }
    if value.contains('\n') {
        bail!("refusing to write a value containing a newline for {key_name}");
    }

    let path = env_file_path().ok_or_else(|| {
        anyhow::anyhow!(
            "no config directory for the managed env file; set {ENV_FILE_ENV} to a path first"
        )
    })?;

    let existing = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("creating config dir {}", parent.display()))?;
            }
            // A fresh file gets just the header comment; `upsert` appends the key.
            template(&[])
        }
        Err(e) => {
            return Err(e).with_context(|| format!("reading env file {}", path.display()));
        }
    };

    let updated = upsert(&existing, key_name, value);
    atomic_write(&path, &updated)?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::super::env_file::parse;
    use super::*;

    /// `set_key` reads the process-global `ENTANGLEMENT_ENV_FILE`, so tests that
    /// touch it serialize (shared with the sibling env_file tests' concern).
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn upsert_replaces_commented_placeholder_in_place() {
        let text = "# header\n#ZAI_API_KEY=\n#OPENAI_API_KEY=\n";
        let got = upsert(text, "ZAI_API_KEY", "zzz");
        assert_eq!(got, "# header\nZAI_API_KEY=zzz\n#OPENAI_API_KEY=\n");
    }

    #[test]
    fn upsert_replaces_spaced_placeholder() {
        let text = "# ZAI_API_KEY=\n";
        assert_eq!(upsert(text, "ZAI_API_KEY", "v"), "ZAI_API_KEY=v\n");
    }

    #[test]
    fn upsert_replaces_first_live_line() {
        let text = "ZAI_API_KEY=old\nOPENAI_API_KEY=keep\n";
        let got = upsert(text, "ZAI_API_KEY", "new");
        assert_eq!(got, "ZAI_API_KEY=new\nOPENAI_API_KEY=keep\n");
    }

    #[test]
    fn upsert_live_line_wins_over_placeholder() {
        // A live line and a placeholder for the same key: the live one is
        // replaced (first-occurrence-wins matches load()), placeholder untouched.
        let text = "#ZAI_API_KEY=\nZAI_API_KEY=old\n";
        let got = upsert(text, "ZAI_API_KEY", "new");
        assert_eq!(got, "#ZAI_API_KEY=\nZAI_API_KEY=new\n");
    }

    #[test]
    fn upsert_only_touches_the_first_of_duplicate_live_lines() {
        let text = "ZAI_API_KEY=a\nZAI_API_KEY=b\n";
        let got = upsert(text, "ZAI_API_KEY", "c");
        assert_eq!(got, "ZAI_API_KEY=c\nZAI_API_KEY=b\n");
    }

    #[test]
    fn upsert_appends_when_absent() {
        let text = "# header only\n";
        assert_eq!(
            upsert(text, "ANTHROPIC_API_KEY", "sk"),
            "# header only\nANTHROPIC_API_KEY=sk\n"
        );
    }

    #[test]
    fn upsert_appends_to_empty_text() {
        assert_eq!(upsert("", "K", "v"), "K=v\n");
    }

    #[test]
    fn upsert_appends_missing_trailing_newline_before_new_line() {
        // Last line without a terminator must not glue onto the appended key.
        assert_eq!(upsert("A=1", "B", "2"), "A=1\nB=2\n");
    }

    #[test]
    fn upsert_is_idempotent() {
        let once = upsert("#K=\n", "K", "v");
        let twice = upsert(&once, "K", "v");
        assert_eq!(once, twice);
        assert_eq!(twice, "K=v\n");
    }

    #[test]
    fn upsert_preserves_unrelated_lines_byte_for_byte() {
        // Blank lines, comments, CRLF terminators, and other keys survive exactly.
        let text = "# a\r\n\nOTHER=leave me\r\n#K=\n";
        let got = upsert(text, "K", "v");
        assert_eq!(got, "# a\r\n\nOTHER=leave me\r\nK=v\n");
    }

    #[test]
    fn set_key_rejects_empty_and_newline_values() {
        assert!(set_key("K", "").is_err());
        assert!(set_key("K", "line1\nline2").is_err());
    }

    #[test]
    fn set_key_creates_file_round_trips_and_is_0600() {
        let _guard = ENV_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join(".env");
        std::env::set_var(ENV_FILE_ENV, &path);

        let written = set_key("ZAI_API_KEY", "secret-value").unwrap();
        assert_eq!(written, path);

        // The value round-trips through the same parser `load()` uses.
        let body = std::fs::read_to_string(&path).unwrap();
        let pairs = parse(&body);
        assert!(pairs
            .iter()
            .any(|(k, v)| k == "ZAI_API_KEY" && v == "secret-value"));

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "managed env file must be user-only");
        }

        std::env::remove_var(ENV_FILE_ENV);
    }

    #[test]
    fn set_key_updates_existing_key_and_preserves_others() {
        let _guard = ENV_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".env");
        std::fs::write(&path, "ZAI_API_KEY=old\nOPENAI_API_KEY=keep\n").unwrap();
        std::env::set_var(ENV_FILE_ENV, &path);

        set_key("ZAI_API_KEY", "new").unwrap();
        let body = std::fs::read_to_string(&path).unwrap();
        assert_eq!(body, "ZAI_API_KEY=new\nOPENAI_API_KEY=keep\n");

        std::env::remove_var(ENV_FILE_ENV);
    }
}
