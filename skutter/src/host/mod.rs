//! Host tools that execute against the local filesystem and shell — `read`,
//! `glob`, `grep`, `edit`, `write`, `apply_patch`, `bash` and `web_fetch`
//! (#968: moved from the library). The read-only trio (`read`/`glob`/
//! `grep`) is covered by ADR-0008; `edit`/`bash` by ADR-0009/ADR-0012;
//! whole-file `write` by ADR-0031; multi-hunk `apply_patch` by #455.
//! [`host_tools`] assembles the **root-contained sextet**
//! (`read`/`glob`/`grep`/`edit`/`write`/`apply_patch`) — `skutter` registers
//! [`BashTool`] unconditionally alongside it (ADR-0195: registration is not
//! where its security story lives, permission is).
//!
//! Each tool is constructed with a working-directory `root`; model-supplied
//! paths resolve against it symlink-safely — the resolved target's deepest
//! existing ancestor is canonicalized (ADR-0054, #163) — and each tool names
//! the path it touches ([`crate::tools::Tool::escape_target`]). A path
//! outside the root is the escape-root check's question ([`escape`]): it
//! asks before the call runs, so a tool runs whatever path the gate cleared
//! (#968, no per-call token). `glob`/`grep` drop a match whose canonical
//! path escapes their walk ([`list_files`]). Output is byte-capped so a
//! runaway listing or huge file can't silently consume the context window.
//! `bash` runs the command rooted at `root` (or at `workdir`) but otherwise
//! inherits the engine process's full privileges by default — unsandboxed
//! unless opted in (ADR-0009); the permission modes and the config ceiling
//! are the default controls (ADR-0010/ADR-0195). [`sandbox`] adds an
//! optional bubblewrap confinement layer (ADR-0104,
//! `ENTANGLEMENT_SANDBOX=bwrap`).

use std::path::{Path, PathBuf};

use anyhow::Result;

pub mod apply_patch;
pub mod bash;
mod brace;
pub mod containment;
pub mod edit;
pub mod escape;
pub mod glob;
pub mod grep;
pub mod read;
mod registry;
pub mod sandbox;
pub mod search_json;
pub mod unified_diff;
mod walk;
pub mod web_fetch;
pub mod workdir;
pub mod write;

pub use apply_patch::ApplyPatchTool;
pub use bash::BashTool;
pub use edit::EditTool;
pub use escape::EscapeResolver;
pub use glob::GlobTool;
pub use grep::GrepTool;
pub use read::{ReadRawTool, ReadTool};
#[cfg(test)]
pub(crate) use registry::every_tool;
pub use registry::{host_tools, host_tools_with, HostToolOptions};
pub use sandbox::{SandboxBackend, SandboxConfig, SandboxPolicy, SandboxResolver};
pub use search_json::{GlobJsonTool, GrepJsonTool};
pub(crate) use walk::MAX_RESULTS;
pub use walk::{list_files, FileList};
pub use web_fetch::WebFetchTool;
pub use write::WriteTool;

/// The absolute file `rel` names under `root`, resolved symlink-safely
/// ([`containment::resolve_and_contained`]) — inside the root or not: a path
/// outside it reaches a tool only once the escape-root check cleared it.
pub fn resolve_path(root: &Path, rel: &str) -> Result<PathBuf> {
    containment::resolve_and_contained(root, rel).map(|(path, _)| path)
}

/// The per-call working directory for `bash`: `root` by default, or the
/// model-supplied `workdir`, which must be a directory (#386).
pub fn resolve_workdir(root: &Path, workdir: Option<&str>) -> Result<PathBuf> {
    match workdir {
        None => Ok(root.to_path_buf()),
        Some(w) => {
            let p = resolve_path(root, w)?;
            if !p.is_dir() {
                anyhow::bail!("workdir is not a directory: {w}");
            }
            Ok(p)
        }
    }
}

#[cfg(test)]
mod backup_tests;
#[cfg(test)]
mod permission_arg_tests;
#[cfg(test)]
mod workdir_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::Tool;
    use containment::escaping_path;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    struct TempDir {
        path: PathBuf,
    }
    impl TempDir {
        fn new() -> TempDir {
            let id = COUNTER.fetch_add(1, Ordering::SeqCst);
            let path =
                std::env::temp_dir().join(format!("entanglement-host-{}-{id}", std::process::id()));
            fs::create_dir_all(&path).unwrap();
            TempDir { path }
        }
        fn join(&self, rel: &str) -> PathBuf {
            let p = self.path.join(rel);
            if let Some(parent) = p.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            p
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    /*
    #[tokio::test]
    async fn read_returns_lines_with_numbers() {
        let dir = TempDir::new();
        fs::write(dir.join("a.txt"), "alpha\nbeta\n").unwrap();
        let tool = ReadTool::new(dir.path.clone());
        let out = tool.run(r#"{"path":"a.txt"}"#).await.unwrap();
        assert!(out.contains("1: alpha"), "got: {out}");
        assert!(out.contains("2: beta"), "got: {out}");
    }
    */

    #[tokio::test]
    async fn glob_lists_matching_files_relative() {
        let dir = TempDir::new();
        fs::write(dir.join("src/a.rs"), "x\n").unwrap();
        fs::write(dir.join("src/b.rs"), "x\n").unwrap();
        fs::write(dir.join("src/c.txt"), "x\n").unwrap();
        let tool = GlobTool::new(dir.path.clone());
        let out = tool.run(r#"{"pattern":"**/*.rs"}"#).await.unwrap();
        assert!(out.contains("src/a.rs"), "got: {out}");
        assert!(out.contains("src/b.rs"), "got: {out}");
        assert!(!out.contains("c.txt"), "got: {out}");
    }

    /// Regression for the bare-`**` trap (ADR-0016): the glob crate yields
    /// directory paths only for `**`, which `list_files` filters out. Without
    /// the hint, the model sees an indistinguishable-from-typo empty result.
    #[tokio::test]
    async fn glob_bare_doublestar_returns_directory_hint() {
        let dir = TempDir::new();
        fs::write(dir.join("src/a.rs"), "x\n").unwrap();
        fs::write(dir.join("root.txt"), "x\n").unwrap();
        let tool = GlobTool::new(dir.path.clone());
        let out = tool.run(r#"{"pattern":"**"}"#).await.unwrap();
        assert!(
            out.contains("matched") && out.contains("director"),
            "expected directory-match hint, got: {out}"
        );
        assert!(
            out.contains("**/*"),
            "hint should suggest `**/*`, got: {out}"
        );
    }

    /// `**/*` (the suggested pattern) must still work — list files in nested
    /// directories.
    #[tokio::test]
    async fn glob_doublestar_slash_star_lists_files() {
        let dir = TempDir::new();
        fs::write(dir.join("src/a.rs"), "x\n").unwrap();
        fs::write(dir.join("root.txt"), "x\n").unwrap();
        let tool = GlobTool::new(dir.path.clone());
        let out = tool.run(r#"{"pattern":"**/*"}"#).await.unwrap();
        assert!(out.contains("src/a.rs"), "got: {out}");
        assert!(out.contains("root.txt"), "got: {out}");
        assert!(
            !out.contains("matched") || !out.contains("director"),
            "should not emit dir-match hint when files exist, got: {out}"
        );
    }

    /// ADR-0150: a bare directory pattern lists it recursively (tool level).
    #[tokio::test]
    async fn glob_dir_pattern_lists_recursively() {
        let dir = TempDir::new();
        fs::write(dir.join("src/nested/a.rs"), "x\n").unwrap();
        fs::write(dir.join("other.txt"), "x\n").unwrap();
        let tool = GlobTool::new(dir.path.clone());
        let out = tool.run(r#"{"pattern":"src"}"#).await.unwrap();
        assert!(out.contains("src/nested/a.rs"), "got: {out}");
        assert!(!out.contains("other.txt"), "got: {out}");
    }

    /// ADR-0150: the Claude-Code-style `path` base dir scopes the pattern.
    #[tokio::test]
    async fn glob_path_param_scopes_pattern() {
        let dir = TempDir::new();
        fs::write(dir.join("src/a.rs"), "x\n").unwrap();
        fs::write(dir.join("docs/b.rs"), "x\n").unwrap();
        let tool = GlobTool::new(dir.path.clone());
        let out = tool
            .run(r#"{"pattern":"**/*.rs","path":"src"}"#)
            .await
            .unwrap();
        assert!(out.contains("src/a.rs"), "got: {out}");
        assert!(!out.contains("docs/b.rs"), "got: {out}");
    }

    /// ADR-0150 (supersedes the ADR-0016 empty-string allowance): a clean
    /// no-match returns a message, never the empty string.
    #[tokio::test]
    async fn glob_clean_no_match_returns_message() {
        let dir = TempDir::new();
        fs::write(dir.join("a.rs"), "x\n").unwrap();
        let tool = GlobTool::new(dir.path.clone());
        let out = tool.run(r#"{"pattern":"*.py"}"#).await.unwrap();
        assert!(
            out.starts_with("pattern `") && out.contains("matched no files"),
            "got: {out:?}"
        );
    }

    /// ADR-0150: brace sets expand (tool level).
    #[tokio::test]
    async fn glob_brace_pattern_matches_both_extensions() {
        let dir = TempDir::new();
        fs::write(dir.join("a.rs"), "x\n").unwrap();
        fs::write(dir.join("b.md"), "x\n").unwrap();
        fs::write(dir.join("c.txt"), "x\n").unwrap();
        let tool = GlobTool::new(dir.path.clone());
        let out = tool.run(r#"{"pattern":"*.{rs,md}"}"#).await.unwrap();
        assert!(out.contains("a.rs"), "got: {out}");
        assert!(out.contains("b.md"), "got: {out}");
        assert!(!out.contains("c.txt"), "got: {out}");
    }

    /// ADR-0150: hitting the walk cap is announced (tool level).
    #[tokio::test]
    async fn glob_capped_appends_notice() {
        let dir = TempDir::new();
        for i in 0..(MAX_RESULTS + 1) {
            fs::write(dir.join(&format!("f{i:04}.txt")), "x").unwrap();
        }
        let tool = GlobTool::new(dir.path.clone());
        let out = tool.run(r#"{"pattern":"**/*"}"#).await.unwrap();
        assert!(out.contains("capped at 1000 results"), "cap notice missing");
    }

    /// ADR-0150: an unknown field errors loudly, naming the field.
    #[tokio::test]
    async fn glob_unknown_field_errors_actionably() {
        let dir = TempDir::new();
        let tool = GlobTool::new(dir.path.clone());
        let err = tool
            .run(r#"{"pattern":"*.rs","limit":5}"#)
            .await
            .unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("limit"), "should name the field: {msg}");
    }

    /// `list_files` distinguishes no-files-but-matched-dirs from no-match-at-all
    /// so callers can produce the right diagnostic.
    #[test]
    fn list_files_counts_matched_dirs_when_no_files() {
        let dir = TempDir::new();
        fs::write(dir.join("nested/a.rs"), "x\n").unwrap();
        let list = list_files(&dir.path, "**", &[]).unwrap();
        assert!(list.files.is_empty(), "bare ** yields no files");
        assert!(
            list.matched_dirs > 0,
            "expected directories to be counted, got {:?}",
            list
        );
        assert!(list.matched_anything(), "matched_anything should be true");
    }

    #[test]
    fn list_files_clean_empty_when_pattern_matches_nothing() {
        let dir = TempDir::new();
        fs::write(dir.join("a.rs"), "x\n").unwrap();
        let list = list_files(&dir.path, "does-not-exist-*", &[]).unwrap();
        assert!(list.files.is_empty());
        assert_eq!(list.matched_dirs, 0);
        assert_eq!(list.skipped_errors, 0);
        assert!(!list.capped);
        assert_eq!(list.out_of_root, 0);
        assert!(!list.matched_anything());
    }

    /// ADR-0150: a metachar-free pattern naming an existing directory is the
    /// `grep -r DIR` shape — it expands to `DIR/**/*` instead of matching only
    /// the (filtered-out) directory entry.
    #[test]
    fn list_files_expands_directory_pattern_recursively() {
        let dir = TempDir::new();
        fs::write(dir.join("src/nested/a.rs"), "x\n").unwrap();
        fs::write(dir.join("src/b.rs"), "x\n").unwrap();
        fs::write(dir.join("other.rs"), "x\n").unwrap();
        let list = list_files(&dir.path, "src", &[]).unwrap();
        let names: Vec<String> = list
            .files
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect();
        assert!(
            names.iter().any(|n| n.ends_with("src/nested/a.rs")),
            "{names:?}"
        );
        assert!(names.iter().any(|n| n.ends_with("src/b.rs")), "{names:?}");
        assert!(!names.iter().any(|n| n.ends_with("other.rs")), "{names:?}");
    }

    #[test]
    fn list_files_dir_expansion_accepts_trailing_slash() {
        let dir = TempDir::new();
        fs::write(dir.join("src/a.rs"), "x\n").unwrap();
        let list = list_files(&dir.path, "src/", &[]).unwrap();
        assert_eq!(list.files.len(), 1, "{:?}", list.files);
    }

    /// A pattern with glob metachars is never dir-expanded — bare `**` keeps
    /// its matched-dirs semantics (and the ADR-0016 hint built on it).
    #[test]
    fn list_files_does_not_expand_glob_patterns() {
        let dir = TempDir::new();
        fs::write(dir.join("nested/a.rs"), "x\n").unwrap();
        let list = list_files(&dir.path, "**", &[]).unwrap();
        assert!(list.files.is_empty(), "{:?}", list.files);
        assert!(list.matched_dirs > 0);
    }

    /// ADR-0150 × #968: an external directory — one the escape-root check
    /// already asked about — expands like an in-root one, so
    /// `{"path": "/approved/dir"}` searches it recursively.
    #[test]
    fn list_files_expands_an_external_directory() {
        let dir = TempDir::new();
        let outside = TempDir::new();
        std::fs::write(outside.join("nested/lib.rs"), "x\n").unwrap();

        let pattern = outside.path.display().to_string();
        let list = list_files(&dir.path, &pattern, &[]).unwrap();
        let names: Vec<String> = list
            .files
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect();
        assert!(
            names.iter().any(|n| n.ends_with("nested/lib.rs")),
            "{names:?}"
        );
    }

    /// ADR-0150: `{a,b}` brace sets expand into alternatives.
    #[test]
    fn list_files_brace_set_expands_alternatives() {
        let dir = TempDir::new();
        fs::write(dir.join("src/a.rs"), "x\n").unwrap();
        fs::write(dir.join("docs/b.md"), "x\n").unwrap();
        fs::write(dir.join("src/c.txt"), "x\n").unwrap();
        let list = list_files(&dir.path, "**/*.{rs,md}", &[]).unwrap();
        let names: Vec<String> = list
            .files
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect();
        assert!(names.iter().any(|n| n.ends_with("a.rs")), "{names:?}");
        assert!(names.iter().any(|n| n.ends_with("b.md")), "{names:?}");
        assert!(!names.iter().any(|n| n.ends_with("c.txt")), "{names:?}");
    }

    /// Overlapping alternatives must not duplicate a file in the result.
    #[test]
    fn list_files_brace_overlap_deduped() {
        let dir = TempDir::new();
        fs::write(dir.join("a.rs"), "x\n").unwrap();
        let list = list_files(&dir.path, "{*.rs,a.*}", &[]).unwrap();
        assert_eq!(list.files.len(), 1, "{:?}", list.files);
    }

    #[test]
    fn list_files_excludes_accept_braces() {
        let dir = TempDir::new();
        fs::write(dir.join("src/a.rs"), "x\n").unwrap();
        fs::write(dir.join("target/b.rs"), "x\n").unwrap();
        fs::write(dir.join("node_modules/c.rs"), "x\n").unwrap();
        let list = list_files(
            &dir.path,
            "**/*.rs",
            &["{target,node_modules}/**".to_string()],
        )
        .unwrap();
        let names: Vec<String> = list
            .files
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect();
        assert!(names.iter().any(|n| n.ends_with("a.rs")), "{names:?}");
        assert_eq!(names.len(), 1, "{names:?}");
    }

    /// ADR-0150: hitting MAX_RESULTS is recorded instead of silently breaking.
    #[test]
    fn list_files_sets_capped_flag() {
        let dir = TempDir::new();
        for i in 0..(MAX_RESULTS + 1) {
            fs::write(dir.join(&format!("f{i:04}.txt")), "x").unwrap();
        }
        let list = list_files(&dir.path, "**/*", &[]).unwrap();
        assert_eq!(list.files.len(), MAX_RESULTS);
        assert!(list.capped);
    }

    /// ADR-0150: containment drops are counted so they do not read as a
    /// clean no-match.
    #[cfg(unix)]
    #[test]
    fn list_files_counts_out_of_root() {
        let dir = TempDir::new();
        let outside = TempDir::new();
        std::fs::write(outside.join("leak.txt"), "x\n").unwrap();
        std::os::unix::fs::symlink(&outside.path, dir.join("escape")).unwrap();
        let list = list_files(&dir.path, "**/*.txt", &[]).unwrap();
        assert!(list.files.is_empty(), "{:?}", list.files);
        assert!(list.out_of_root > 0, "{list:?}");
    }

    /// `.git` is excluded unconditionally — no `excludes` entry required, and
    /// it can't be searched even if the pattern targets it directly (ADR-0099).
    #[test]
    fn list_files_excludes_dot_git_by_default() {
        let dir = TempDir::new();
        fs::write(dir.join(".git/config"), "x\n").unwrap();
        fs::write(dir.join(".git/objects/abc"), "x\n").unwrap();
        fs::write(dir.join("src/main.rs"), "x\n").unwrap();
        let list = list_files(&dir.path, "**/*", &[]).unwrap();
        let names: Vec<String> = list
            .files
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect();
        assert!(
            names.iter().any(|n| n.ends_with("src/main.rs")),
            "in-tree file should be listed: {names:?}"
        );
        assert!(
            !names.iter().any(|n| n.contains(".git")),
            ".git contents leaked: {names:?}"
        );

        // Even a pattern that targets `.git` directly finds nothing.
        let git_list = list_files(&dir.path, ".git/**/*", &[]).unwrap();
        assert!(
            git_list.files.is_empty(),
            "explicit .git/** pattern should still be excluded: {:?}",
            git_list.files
        );
    }

    /// A caller-supplied `excludes` glob filters out matching paths, including
    /// whole subtrees via `**`.
    #[test]
    fn list_files_honors_caller_excludes() {
        let dir = TempDir::new();
        fs::write(dir.join("src/a.rs"), "x\n").unwrap();
        fs::write(dir.join("target/debug/build.log"), "x\n").unwrap();
        let list = list_files(&dir.path, "**/*", &["target/**".to_string()]).unwrap();
        let names: Vec<String> = list
            .files
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect();
        assert!(names.iter().any(|n| n.ends_with("src/a.rs")), "{names:?}");
        assert!(
            !names.iter().any(|n| n.contains("target")),
            "excluded subtree leaked: {names:?}"
        );
    }

    /// #629/ADR-0170: a root `.gitignore` entry is honored with no `exclude`
    /// needed — the concrete `target/`/`node_modules/` problem the issue named.
    #[test]
    fn list_files_honors_root_gitignore() {
        let dir = TempDir::new();
        fs::write(dir.join(".gitignore"), "target/\nnode_modules/\n").unwrap();
        fs::write(dir.join("src/a.rs"), "x\n").unwrap();
        fs::write(dir.join("target/debug/build.log"), "x\n").unwrap();
        fs::write(dir.join("node_modules/pkg/index.js"), "x\n").unwrap();
        let list = list_files(&dir.path, "**/*", &[]).unwrap();
        let names: Vec<String> = list
            .files
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect();
        assert!(names.iter().any(|n| n.ends_with("src/a.rs")), "{names:?}");
        assert!(
            !names
                .iter()
                .any(|n| n.contains("target") || n.contains("node_modules")),
            ".gitignore'd trees leaked: {names:?}"
        );
    }

    /// #629/ADR-0170: a nested `.gitignore` (not just the root one) is
    /// respected, matching real git/ripgrep behavior.
    #[test]
    fn list_files_honors_nested_gitignore() {
        let dir = TempDir::new();
        fs::write(dir.join("crate/.gitignore"), "build/\n").unwrap();
        fs::write(dir.join("crate/src/a.rs"), "x\n").unwrap();
        fs::write(dir.join("crate/build/out.o"), "x\n").unwrap();
        let list = list_files(&dir.path, "**/*", &[]).unwrap();
        let names: Vec<String> = list
            .files
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect();
        assert!(
            names.iter().any(|n| n.ends_with("crate/src/a.rs")),
            "{names:?}"
        );
        assert!(
            !names.iter().any(|n| n.contains("build/out.o")),
            "nested .gitignore'd tree leaked: {names:?}"
        );
    }

    /// #629/ADR-0170: `.gitignore` is honored even when `root` isn't (yet) a
    /// real git repository — `require_git(false)`, so a freshly scaffolded
    /// project doesn't need `git init` first for this to kick in.
    #[test]
    fn list_files_honors_gitignore_without_a_git_repo() {
        let dir = TempDir::new();
        assert!(!dir.join(".git").exists());
        fs::write(dir.join(".gitignore"), "ignored.txt\n").unwrap();
        fs::write(dir.join("kept.txt"), "x\n").unwrap();
        fs::write(dir.join("ignored.txt"), "x\n").unwrap();
        let list = list_files(&dir.path, "**/*", &[]).unwrap();
        let names: Vec<String> = list
            .files
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect();
        assert!(names.iter().any(|n| n.ends_with("kept.txt")), "{names:?}");
        assert!(
            !names.iter().any(|n| n.ends_with("ignored.txt")),
            "{names:?}"
        );
    }

    /// #629/ADR-0170 × #968: a walk of an approved external directory is a
    /// deliberate, narrow reach — its own `.gitignore` must not silently
    /// hide files the user approved searching.
    #[test]
    fn list_files_external_walk_bypasses_the_dirs_own_gitignore() {
        let dir = TempDir::new();
        let outside = TempDir::new();
        fs::write(outside.join(".gitignore"), "lib.rs\n").unwrap();
        fs::write(outside.join("lib.rs"), "x\n").unwrap();

        let pattern = format!("{}/**/*", outside.path.display());
        let list = list_files(&dir.path, &pattern, &[]).unwrap();
        let names: Vec<String> = list
            .files
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect();
        assert!(
            names.iter().any(|n| n.ends_with("lib.rs")),
            "granted external file must not be hidden by its own .gitignore: {names:?}"
        );
    }

    /// #678: a `.gitignore`d directory is pruned — never opened — not merely
    /// filtered from results. An unreadable ignored dir is the proof: the old
    /// two-pass walk descended into it and logged read errors; pruning means
    /// the walk never touches it.
    #[cfg(unix)]
    #[test]
    fn list_files_prunes_ignored_dirs_without_descending() {
        use std::os::unix::fs::PermissionsExt;
        let dir = TempDir::new();
        fs::write(dir.join(".gitignore"), "locked/\n").unwrap();
        fs::write(dir.join("kept.txt"), "x\n").unwrap();
        fs::write(dir.join("locked/secret.txt"), "x\n").unwrap();
        let locked = dir.join("locked");
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
        let list = list_files(&dir.path, "**/*", &[]).unwrap();
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();
        let names: Vec<String> = list
            .files
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect();
        assert!(names.iter().any(|n| n.ends_with("kept.txt")), "{names:?}");
        assert_eq!(
            list.skipped_errors, 0,
            "ignored dir was opened instead of pruned: {list:?}"
        );
    }

    /// #678: the total-scan budget terminates a walk over a huge tree even
    /// when almost nothing matches — the hard backstop behind the TUI
    /// black-screen fix.
    #[test]
    fn list_files_scan_budget_terminates() {
        let dir = TempDir::new();
        for i in 0..50 {
            fs::write(dir.join(&format!("f{i:02}.txt")), "x").unwrap();
        }
        let list = walk::list_files_bounded(&dir.path, "**/*", &[], 10).unwrap();
        assert!(list.scan_capped, "{list:?}");
        assert!(list.files.len() <= 10, "{:?}", list.files);
    }

    /// #678: a symlink loop terminates (the `ignore` crate detects it; the
    /// `glob` crate's `**` walked it forever).
    #[cfg(unix)]
    #[test]
    fn list_files_survives_symlink_loop() {
        let dir = TempDir::new();
        fs::write(dir.join("sub/real.txt"), "x\n").unwrap();
        std::os::unix::fs::symlink(&dir.path, dir.join("sub/loop")).unwrap();
        let list = list_files(&dir.path, "**/*", &[]).unwrap();
        let names: Vec<String> = list
            .files
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect();
        assert!(
            names.iter().any(|n| n.ends_with("sub/real.txt")),
            "{names:?}"
        );
    }

    /// Glob parity pins for the single-pass matcher (#678): dotfiles stay
    /// enumerable (`require_literal_leading_dot` default), and `*` never
    /// crosses a `/` (the iterator matched per-component;
    /// `require_literal_separator` keeps that).
    #[test]
    fn list_files_matches_dotfiles_and_star_stays_within_a_component() {
        let dir = TempDir::new();
        fs::write(dir.join(".env"), "x\n").unwrap();
        fs::write(dir.join("src/nested.rs"), "x\n").unwrap();
        let all = list_files(&dir.path, "**/*", &[]).unwrap();
        let names: Vec<String> = all
            .files
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect();
        assert!(names.iter().any(|n| n.ends_with(".env")), "{names:?}");
        let top = list_files(&dir.path, "*.rs", &[]).unwrap();
        assert!(
            top.files.is_empty(),
            "`*` crossed a directory boundary: {:?}",
            top.files
        );
    }

    /// An invalid exclude pattern errors rather than being silently ignored.
    #[test]
    fn list_files_rejects_invalid_exclude_pattern() {
        let dir = TempDir::new();
        let err = list_files(&dir.path, "**/*", &["[".to_string()]).unwrap_err();
        assert!(
            format!("{err}").contains("invalid exclude pattern"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn grep_returns_matches_with_line_numbers() {
        let dir = TempDir::new();
        fs::write(dir.join("src/m.rs"), "fn alpha() {}\nfn beta() {}\n").unwrap();
        fs::write(dir.join("src/other.md"), "# alpha\n").unwrap();
        let tool = GrepTool::new(dir.path.clone());
        let out = tool.run(r#"{"pattern":"alpha"}"#).await.unwrap();
        assert!(out.contains("src/m.rs:1:"), "got: {out}");
        assert!(out.contains("src/other.md:1:"), "got: {out}");
        assert!(!out.contains("beta"), "got: {out}");
    }

    #[tokio::test]
    async fn glob_excludes_dot_git_by_default() {
        let dir = TempDir::new();
        fs::write(dir.join(".git/config"), "x\n").unwrap();
        fs::write(dir.join("src/a.rs"), "x\n").unwrap();
        let tool = GlobTool::new(dir.path.clone());
        let out = tool.run(r#"{"pattern":"**/*"}"#).await.unwrap();
        assert!(out.contains("src/a.rs"), "got: {out}");
        assert!(!out.contains(".git"), "got: {out}");
    }

    #[tokio::test]
    async fn glob_exclude_param_filters_matching_subtree() {
        let dir = TempDir::new();
        fs::write(dir.join("src/a.rs"), "x\n").unwrap();
        fs::write(dir.join("target/debug/build.log"), "x\n").unwrap();
        let tool = GlobTool::new(dir.path.clone());
        let out = tool
            .run(r#"{"pattern":"**/*","exclude":["target/**"]}"#)
            .await
            .unwrap();
        assert!(out.contains("src/a.rs"), "got: {out}");
        assert!(!out.contains("target"), "got: {out}");
    }

    #[tokio::test]
    async fn grep_excludes_dot_git_by_default() {
        let dir = TempDir::new();
        fs::write(dir.join(".git/config"), "needle\n").unwrap();
        fs::write(dir.join("src/a.rs"), "needle\n").unwrap();
        let tool = GrepTool::new(dir.path.clone());
        let out = tool.run(r#"{"pattern":"needle"}"#).await.unwrap();
        assert!(out.contains("src/a.rs"), "got: {out}");
        assert!(!out.contains(".git"), "got: {out}");
    }

    #[tokio::test]
    async fn grep_exclude_param_filters_matching_subtree() {
        let dir = TempDir::new();
        fs::write(dir.join("src/a.rs"), "needle\n").unwrap();
        fs::write(dir.join("target/debug/build.log"), "needle\n").unwrap();
        let tool = GrepTool::new(dir.path.clone());
        let out = tool
            .run(r#"{"pattern":"needle","exclude":["target/**"]}"#)
            .await
            .unwrap();
        assert!(out.contains("src/a.rs"), "got: {out}");
        assert!(!out.contains("target"), "got: {out}");
    }

    #[tokio::test]
    async fn edit_creates_file_when_old_string_empty() {
        let dir = TempDir::new();
        let tool = EditTool::new(dir.path.clone());
        let out = tool
            .run(r#"{"path":"new.txt","oldString":"","newString":"hello\n"}"#)
            .await
            .unwrap();
        assert!(out.contains("created"), "got: {out}");
        let on_disk = std::fs::read_to_string(dir.join("new.txt")).unwrap();
        assert_eq!(on_disk, "hello\n");
    }

    #[tokio::test]
    async fn edit_replaces_unique_match() {
        let dir = TempDir::new();
        fs::write(dir.join("a.txt"), "alpha\nbeta\n").unwrap();
        let tool = EditTool::new(dir.path.clone());
        let out = tool
            .run(r#"{"path":"a.txt","oldString":"beta","newString":"BETA"}"#)
            .await
            .unwrap();
        assert!(out.contains("1 matches replaced"), "got: {out}");
        let on_disk = std::fs::read_to_string(dir.join("a.txt")).unwrap();
        assert_eq!(on_disk, "alpha\nBETA\n");
    }

    /// #163: a symlink whose *final component* points outside root is judged
    /// by its target — the path the escape-root check asks about — and a
    /// symlinked *directory* can't hide a new file's real location either.
    #[cfg(unix)]
    #[test]
    fn a_symlink_out_of_the_root_resolves_to_its_target() {
        let dir = TempDir::new();
        let outside = TempDir::new();
        std::fs::write(outside.join("target.txt"), "secret\n").unwrap();
        std::os::unix::fs::symlink(outside.join("target.txt"), dir.join("link")).unwrap();
        std::os::unix::fs::symlink(&outside.path, dir.join("escape")).unwrap();
        let canon = outside.path.canonicalize().unwrap();
        assert_eq!(
            escaping_path(&dir.path, "link"),
            Some(canon.join("target.txt"))
        );
        assert_eq!(
            escaping_path(&dir.path, "escape/new.txt"),
            Some(canon.join("new.txt"))
        );
        assert_eq!(
            resolve_path(&dir.path, "link").unwrap(),
            canon.join("target.txt")
        );
    }

    /// #163: a symlink pointing *inside* root stays contained and resolves to
    /// its canonical in-tree target.
    #[cfg(unix)]
    #[test]
    fn resolve_allows_symlink_inside_root() {
        let dir = TempDir::new();
        std::fs::write(dir.join("real.txt"), "x\n").unwrap();
        std::os::unix::fs::symlink(dir.join("real.txt"), dir.join("alias")).unwrap();
        let resolved = resolve_path(&dir.path, "alias").unwrap();
        assert!(resolved.starts_with(dir.path.canonicalize().unwrap()));
        assert!(resolved.ends_with("real.txt"));
    }

    /// #163: creating a genuinely-new file (no symlink) still works — the tail
    /// past the deepest existing ancestor is re-appended verbatim.
    #[test]
    fn resolve_allows_new_nested_file() {
        let dir = TempDir::new();
        let resolved = resolve_path(&dir.path, "a/b/c.txt").unwrap();
        assert!(resolved.ends_with("a/b/c.txt"));
        assert!(resolved.starts_with(dir.path.canonicalize().unwrap()));
    }

    /// `escaping_path` reports the resolved target only when it leaves root.
    #[test]
    fn escaping_path_detects_out_of_root_only() {
        let dir = TempDir::new();
        assert!(escaping_path(&dir.path, "inside.txt").is_none());
        let out = TempDir::new();
        let abs = out.join("x.txt").to_string_lossy().into_owned();
        assert!(escaping_path(&dir.path, &abs).is_some());
    }

    /// #163: `glob` must not surface files reached through a symlink that leaves
    /// the tree, nor via an absolute/`..` pattern.
    #[cfg(unix)]
    #[test]
    fn list_files_drops_symlinked_escape() {
        let dir = TempDir::new();
        let outside = TempDir::new();
        std::fs::write(outside.join("leak.txt"), "x\n").unwrap();
        std::os::unix::fs::symlink(&outside.path, dir.join("escape")).unwrap();
        std::fs::write(dir.join("inside.txt"), "x\n").unwrap();
        let list = list_files(&dir.path, "**/*", &[]).unwrap();
        let names: Vec<String> = list
            .files
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect();
        assert!(
            names.iter().any(|n| n.ends_with("inside.txt")),
            "in-tree file should be listed: {names:?}"
        );
        assert!(
            !names.iter().any(|n| n.contains("leak.txt")),
            "symlinked out-of-tree file leaked: {names:?}"
        );
    }

    /// #968: a pattern outside the root — one the escape-root check asked
    /// about — walks there, surfacing the matches under it.
    #[test]
    fn list_files_walks_an_external_pattern() {
        let dir = TempDir::new();
        let outside = TempDir::new();
        std::fs::write(outside.join("lib.rs"), "x\n").unwrap();

        let pattern = format!("{}/**/*", outside.path.display());
        let list = list_files(&dir.path, &pattern, &[]).unwrap();
        let names: Vec<String> = list
            .files
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect();
        assert!(
            names.iter().any(|n| n.ends_with("lib.rs")),
            "the external file should be listed: {names:?}"
        );
    }

    /// A symlink inside an external walk pointing *outside* it must not leak
    /// matches — the walk is confined to its own start (and its real
    /// descendants), not wherever a symlink redirects.
    #[cfg(unix)]
    #[test]
    fn an_external_walk_does_not_leak_through_a_symlink_out_of_it() {
        let dir = TempDir::new();
        let granted = TempDir::new();
        let secret = TempDir::new();
        std::fs::write(secret.join("leak.txt"), "x\n").unwrap();
        std::os::unix::fs::symlink(&secret.path, granted.join("escape")).unwrap();
        std::fs::write(granted.join("inside.txt"), "x\n").unwrap();

        let pattern = format!("{}/**/*", granted.path.display());
        let list = list_files(&dir.path, &pattern, &[]).unwrap();
        let names: Vec<String> = list
            .files
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect();
        assert!(
            names.iter().any(|n| n.ends_with("inside.txt")),
            "the walked directory's own file should be listed: {names:?}"
        );
        assert!(
            !names.iter().any(|n| n.contains("leak.txt")),
            "a symlink out of the walked directory must not leak: {names:?}"
        );
    }

    /// #968: `GlobTool`/`GrepTool` end-to-end — an external directory the
    /// escape-root check cleared is searchable by both tools.
    #[tokio::test]
    async fn glob_and_grep_search_an_external_directory() {
        let dir = TempDir::new();
        let outside = TempDir::new();
        std::fs::write(outside.join("lib.rs"), "fn needle() {}\n").unwrap();

        let glob_tool = GlobTool::new(dir.path.clone());
        let pattern = format!(r#"{{"pattern":"{}/**/*.rs"}}"#, outside.path.display());
        let out = glob_tool.run(&pattern).await.unwrap();
        assert!(out.contains("lib.rs"), "got: {out}");

        let grep_tool = GrepTool::new(dir.path.clone());
        let grep_input = format!(
            r#"{{"pattern":"needle","path":"{}/**/*"}}"#,
            outside.path.display()
        );
        let out = grep_tool.run(&grep_input).await.unwrap();
        assert!(out.contains("needle"), "got: {out}");
    }

    #[test]
    fn host_tools_registers_root_contained_set_without_bash() {
        let dir = TempDir::new();
        let reg = host_tools(dir.path.clone());
        let specs = reg.specs();
        let names: Vec<&str> = specs.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"read"), "{names:?}");
        assert!(names.contains(&"glob"), "{names:?}");
        assert!(names.contains(&"grep"), "{names:?}");
        assert!(names.contains(&"edit"), "{names:?}");
        assert!(names.contains(&"write"), "{names:?}");
        assert!(names.contains(&"apply_patch"), "{names:?}");
        assert!(!names.contains(&"bash"), "{names:?}");
        for s in &specs {
            assert!(
                s.schema.get("properties").is_some(),
                "{} missing properties",
                s.name
            );
        }
    }
}
