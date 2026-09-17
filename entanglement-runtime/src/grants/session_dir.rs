//! [`ApprovalScope::SessionDir`][entanglement_core::ApprovalScope::SessionDir]
//! directory derivation/coverage (#486, ADR-0126) — split out of `grants/mod.rs`
//! since it needs neither [`super::GrantKey`] nor its mode-scoping: a
//! `SessionDir` grant widens the read-only triad by *directory*, not by an
//! exact `(tool, arg, mode)` match, and is deliberately unscoped by mode (see
//! the parent module's docs).

use std::path::Path;

/// Derive the directory a `(tool, arg)` call implies, for recording a
/// `SessionDir` grant: `read`/`edit`/`write`/`apply_patch` → the argument's
/// parent directory (a root-level file's parent is the project root itself,
/// `"."`); `grep` → the path filter value verbatim (already directory-shaped
/// — a specific file or a directory); `glob` → the pattern's literal prefix
/// up to its first wildcard, truncated to the last path separator
/// ([`glob_literal_prefix`]). Any other tool (`bash`/`call`, or a call with
/// no argument) has no directory concept and yields `None` — `record`'s
/// caller degrades to an exact `Session` grant in that case. Head-agnostic
/// and reusable beyond the read-only triad `record` currently restricts this
/// to (mirrors #485's `PATH_ARG_TOOLS` table).
pub(super) fn dir_for(tool: &str, arg: Option<&str>) -> Option<String> {
    let arg = arg?;
    match tool {
        "read" | "edit" | "write" | "apply_patch" => {
            let parent = Path::new(arg).parent()?.to_string_lossy().into_owned();
            Some(if parent.is_empty() {
                ".".to_string()
            } else {
                parent
            })
        }
        "grep" => Some(arg.to_string()),
        "glob" => Some(glob_literal_prefix(arg)),
        _ => None,
    }
}

/// The literal (non-wildcard) directory prefix of a glob pattern: everything
/// before the first `*`/`?`, truncated at the last `/` — `"src/*.rs"` → `"src"`,
/// `"src/foo.rs"` (no wildcard) → `"src"`, `"*.rs"` → `"."` (no directory
/// component at all).
fn glob_literal_prefix(pattern: &str) -> String {
    let end = pattern.find(['*', '?']).unwrap_or(pattern.len());
    match pattern[..end].rfind('/') {
        Some(idx) => pattern[..idx].to_string(),
        None => ".".to_string(),
    }
}

/// Whether a granted directory `dir` covers a later call's grading argument
/// `arg` (#486): exact match, path-component-prefix nesting
/// (`arg.starts_with("{dir}/")`), or `dir == "."` covering every relative
/// argument. Operates directly on the already-#485-normalized root-relative
/// argument — a glob pattern's wildcard tail is just a string suffix once its
/// literal root matches, so no separate glob-specific comparison is needed.
///
/// A `"."` grant also string-covers an *absolute* out-of-root argument (an
/// absolute path never gets the #485 root-prefix strip). That is safe only
/// because the escape-root gate (ADR-0109) independently forces its own
/// approval for any out-of-root target *before* grant matching can allow the
/// call — this function is a permission upgrade, not the containment check.
pub(super) fn dir_covers(dir: &str, arg: &str) -> bool {
    dir == "." || arg == dir || arg.starts_with(&format!("{dir}/"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dot_dir_grant_string_covers_absolute_paths_by_design() {
        // Pins the ADR-0109 coupling documented on `dir_covers`: a `.` grant
        // covers absolute out-of-root arguments at the string level. If this
        // ever changes, re-check that the escape-root gate is still the layer
        // refusing out-of-root access — and if `dir_covers` is instead meant
        // to reject absolute args itself, update the doc comment with it.
        assert!(dir_covers(".", "/etc/passwd"));
        assert!(dir_covers(".", "src/main.rs"));
        assert!(!dir_covers("src", "/etc/passwd"));
        assert!(!dir_covers("src", "src2/main.rs"));
    }

    #[test]
    fn dir_for_derivation_table() {
        assert_eq!(dir_for("read", Some("src/a.rs")), Some("src".to_string()));
        assert_eq!(dir_for("read", Some("main.rs")), Some(".".to_string()));
        assert_eq!(dir_for("edit", Some("src/a.rs")), Some("src".to_string()));
        assert_eq!(dir_for("write", Some("src/a.rs")), Some("src".to_string()));
        assert_eq!(
            dir_for("apply_patch", Some("src/a.rs")),
            Some("src".to_string())
        );
        assert_eq!(dir_for("grep", Some("src")), Some("src".to_string()));
        assert_eq!(
            dir_for("grep", Some("src/a.rs")),
            Some("src/a.rs".to_string())
        );
        assert_eq!(dir_for("glob", Some("src/*.rs")), Some("src".to_string()));
        assert_eq!(dir_for("glob", Some("*.rs")), Some(".".to_string()));
        assert_eq!(dir_for("glob", Some("src/a.rs")), Some("src".to_string()));
        assert_eq!(dir_for("bash", Some("git status")), None);
        assert_eq!(dir_for("read", None), None);
    }
}
