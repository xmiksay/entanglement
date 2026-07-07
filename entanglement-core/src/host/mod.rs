//! Host tools that execute against the local filesystem and shell — `read`,
//! `glob`, `grep`, `edit`, `apply_diff`, and the opt-in `bash`. The read-only trio
//! (`read`/`glob`/`grep`) is covered by ADR-0008; `edit`/`apply_diff`/`bash` by ADR-0009/ADR-0012;
//! [`host_tools`] assembles the **root-contained set** (`read`/`glob`/
//! `grep`/`edit`/`apply_diff`) and a head explicitly opts into [`BashTool`] (gated by
//! `ENTANGLEMENT_ENABLE_BASH`) — see ADR-0010.
//!
//! Each tool is constructed with a working-directory `root`; model-supplied
//! paths resolve against it and are **rejected on `..` escape** (lexical only
//! for now — no symlink defense yet). Output is byte-capped so a runaway
//! listing or huge file can't silently consume the context window. `bash` runs
//! the command rooted at `root` but otherwise inherits the engine process's
//! full privileges — unsandboxed by design (ADR-0009); the opt-in gate plus
//! permission profiles are the only controls (ADR-0010).

use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result};

use crate::tools::ToolRegistry;

// pub mod apply_diff; Commented, will be fixed later!!!
pub mod bash;
pub mod edit;
pub mod glob;
pub mod grep;
pub mod read;

// pub use apply_diff::ApplyDiffTool; Commented, will be fixed later!!!
pub use bash::BashTool;
pub use edit::EditTool;
pub use glob::GlobTool;
pub use grep::GrepTool;
pub use read::ReadTool;

/// Hard cap on a single tool's textual output, in bytes. Larger output is
/// truncated with a notice. Picked generously below the context budget so a
/// normal source file fits, but a minified bundle or huge directory listing
/// can't blow the window. See ADR-0008.
pub const MAX_OUTPUT_BYTES: usize = 32 * 1024;

/// Cap on how many paths `glob` returns and how many matches `grep` reports —
/// bounds the work + output for pathologically large trees.
const MAX_RESULTS: usize = 1000;

// ┗━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// ┃ Shared helpers
// ┗━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

/*
fn count_patch_changes(patch_text: &str) -> (usize, usize) {
    let mut insertions = 0;
    let mut deletions = 0;
    for line in patch_text.lines() {
        let trimmed = line.trim_start_matches(' ');
        if trimmed.starts_with('+') && !trimmed.starts_with("+++") && !trimmed.starts_with("@@") {
            insertions += 1;
        } else if trimmed.starts_with('-')
            && !trimmed.starts_with("---")
            && !trimmed.starts_with("@@")
        {
            deletions += 1;
        }
    }
    (insertions, deletions)
}
*/
/// Resolve `rel` against `root`, rejecting paths that escape the root via `..`
/// (and absolute paths that don't live under it). Lexical only — symlinks can
/// still point outside, which is accepted for now (ADR-0008).
pub fn resolve_under_root(root: &Path, rel: &str) -> Result<PathBuf> {
    let joined = if Path::new(rel).is_absolute() {
        PathBuf::from(rel)
    } else {
        root.join(rel)
    };
    let mut norm = PathBuf::new();
    for comp in joined.components() {
        match comp {
            Component::CurDir => {}
            Component::ParentDir => {
                if !norm.pop() {
                    return Err(anyhow::anyhow!("path escapes working directory: {rel}"));
                }
            }
            other => norm.push(other.as_os_str()),
        }
    }
    if !norm.starts_with(root) {
        return Err(anyhow::anyhow!("path escapes working directory: {rel}"));
    }
    Ok(norm)
}

/// Cap `s` at [`MAX_OUTPUT_BYTES`] on a UTF-8 boundary, appending a notice of
/// the original size so the model knows data was dropped.
pub fn truncate_output(s: String) -> String {
    if s.len() <= MAX_OUTPUT_BYTES {
        return s;
    }
    let mut cut = MAX_OUTPUT_BYTES;
    while !s.is_char_boundary(cut) {
        cut -= 1;
    }
    let mut out = String::from(&s[..cut]);
    out.push_str(&format!("\n... [truncated: {} bytes total]", s.len()));
    out
}

/// Result of [`list_files`]: the matched files plus enough metadata for the
/// caller to distinguish "no match at all" from "matched only directories" or
/// "every entry errored." Without that distinction a bare-`**` pattern (which
/// matches directories only) looks identical to a typo, and the model has no
/// way to self-correct — see ADR-0016.
#[derive(Debug, Default)]
pub struct FileList {
    /// Files (in arbitrary glob-walk order), already capped at [`MAX_RESULTS`].
    pub files: Vec<PathBuf>,
    /// Entries the pattern matched but that were directories (filtered out).
    pub matched_dirs: usize,
    /// Entries the glob iterator yielded as `Err` (permissions, IO, etc.).
    pub skipped_errors: usize,
}

impl FileList {
    /// True iff the pattern matched at least one entry of any kind.
    pub fn matched_anything(&self) -> bool {
        !self.files.is_empty() || self.matched_dirs > 0
    }
}

/// Enumerate files under `root` matching `pattern` (a glob relative to root),
/// yielding display paths relative to root. Skips directories (counted in
/// [`FileList::matched_dirs`]) and logs unreadable entries as `warn!` (counted
/// in [`FileList::skipped_errors`]) instead of silently dropping them. Bounds
/// the walk at [`MAX_RESULTS`] files.
pub fn list_files(root: &Path, pattern: &str) -> Result<FileList> {
    let abs = root.join(pattern).to_string_lossy().into_owned();
    let entries = ::glob::glob(&abs).with_context(|| format!("invalid glob: {pattern}"))?;
    let mut list = FileList::default();
    for entry in entries {
        let p = match entry {
            Ok(p) => p,
            Err(err) => {
                tracing::warn!(?err, pattern, "glob entry skipped");
                list.skipped_errors += 1;
                continue;
            }
        };
        match std::fs::metadata(&p) {
            Ok(m) if m.is_file() => {
                list.files.push(p);
                if list.files.len() >= MAX_RESULTS {
                    break;
                }
            }
            Ok(m) if m.is_dir() => list.matched_dirs += 1,
            _ => {}
        }
    }
    Ok(list)
}

// ┗━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// ┃ host_tools registry
// ┗━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

/// Build the **root-contained set** (`read`/`glob`/`grep`/`edit`/`apply_diff`).
/// Bash is opt-in at the head level (ADR-0010): call [`BashTool::new`] directly and
/// register it when `ENTANGLEMENT_ENABLE_BASH=1`.
pub fn host_tools(root: PathBuf) -> ToolRegistry {
    let mut reg = ToolRegistry::new();
    reg.register(ReadTool::new(root.clone()));
    reg.register(GlobTool::new(root.clone()));
    reg.register(GrepTool::new(root.clone()));
    reg.register(EditTool::new(root.clone()));
    //reg.register(ApplyDiffTool::new(root)); Commented, will be fixed later!!!
    reg
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::Tool;
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

    /// `list_files` distinguishes no-files-but-matched-dirs from no-match-at-all
    /// so callers can produce the right diagnostic.
    #[test]
    fn list_files_counts_matched_dirs_when_no_files() {
        let dir = TempDir::new();
        fs::write(dir.join("nested/a.rs"), "x\n").unwrap();
        let list = list_files(&dir.path, "**").unwrap();
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
        let list = list_files(&dir.path, "does-not-exist-*").unwrap();
        assert!(list.files.is_empty());
        assert_eq!(list.matched_dirs, 0);
        assert_eq!(list.skipped_errors, 0);
        assert!(!list.matched_anything());
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
        //assert!(names.contains(&"apply_diff"), "{names:?}");
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
