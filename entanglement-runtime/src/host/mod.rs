//! Host tools that execute against the local filesystem and shell — `read`,
//! `glob`, `grep`, `edit`, `write`, and the opt-in exec pair `bash`/`call`. The
//! read-only trio (`read`/`glob`/`grep`) is covered by ADR-0008; `edit`/`bash`
//! by ADR-0009/ADR-0012; whole-file `write` by ADR-0031; the argv-exec `call`
//! (no shell, auto-tailed output) by ADR-0045; [`host_tools`] assembles the
//! **root-contained quintet** (`read`/`glob`/`grep`/`edit`/`write`) and a head
//! explicitly opts into the exec pair [`BashTool`]/[`CallTool`] (gated by
//! `ENTANGLEMENT_ENABLE_BASH`) — see ADR-0010.
//!
//! Each tool is constructed with a working-directory `root`; model-supplied
//! paths resolve against it and are **rejected on `..` escape** *and* on
//! **symlink escape** — the resolved target's deepest existing ancestor is
//! canonicalized and must stay under the canonical root (ADR-0054, #163), so a
//! `root/link -> /etc` symlink can't be followed out of tree by `read`/`edit`/
//! `write`, and `glob`/`grep` drop any match whose canonical path escapes.
//! Output is byte-capped so a runaway
//! listing or huge file can't silently consume the context window. `bash` runs
//! the command rooted at `root` but otherwise inherits the engine process's
//! full privileges — unsandboxed by design (ADR-0009); the opt-in gate plus
//! permission profiles are the only controls (ADR-0010).

use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result};

use crate::tools::ToolRegistry;
use entanglement_core::protocol::FileChangeKind;

pub mod bash;
pub mod call;
pub mod edit;
pub mod glob;
pub mod grep;
pub mod read;
pub mod write;

pub use bash::BashTool;
pub use call::CallTool;
pub use edit::EditTool;
pub use glob::GlobTool;
pub use grep::GrepTool;
pub use read::ReadTool;
pub use write::WriteTool;

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

/// Resolve `rel` against `root`, rejecting paths that escape the root via `..`
/// (and absolute paths that don't live under it) **or via a symlink**. The root
/// is canonicalized, then the resolved path is normalized lexically and its
/// deepest existing ancestor is canonicalized (following symlinks) and required
/// to stay under the canonical root; the not-yet-existing tail (for `edit`/
/// `write` creating a file) is `..`-free plain names, so re-appending it can't
/// escape. This upgrades ADR-0008's lexical-only containment to a real
/// write/read boundary without breaking the create path (ADR-0054, #163).
pub fn resolve_under_root(root: &Path, rel: &str) -> Result<PathBuf> {
    // Canonicalize the root defensively so containment holds even when the head
    // (or a test) passes a non-canonical root; startup also canonicalizes cwd.
    let root = root
        .canonicalize()
        .with_context(|| format!("canonicalizing working directory {}", root.display()))?;
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
    if !norm.starts_with(&root) {
        return Err(anyhow::anyhow!("path escapes working directory: {rel}"));
    }
    // Symlink defense (#163): canonicalize the deepest existing ancestor so a
    // symlink under root can't redirect the real target outside it. If the whole
    // path exists (incl. a final-component symlink) it canonicalizes directly;
    // otherwise we peel not-yet-existing tail components off until an ancestor
    // resolves, check *that* against root, then re-append the plain tail.
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    let mut ancestor = norm.clone();
    let canon = loop {
        match ancestor.canonicalize() {
            Ok(c) => break c,
            Err(_) => {
                let name = ancestor
                    .file_name()
                    .ok_or_else(|| anyhow::anyhow!("path escapes working directory: {rel}"))?
                    .to_os_string();
                if !ancestor.pop() {
                    return Err(anyhow::anyhow!("path escapes working directory: {rel}"));
                }
                tail.push(name);
            }
        }
    };
    if !canon.starts_with(&root) {
        return Err(anyhow::anyhow!("path escapes working directory: {rel}"));
    }
    let mut resolved = canon;
    for name in tail.into_iter().rev() {
        resolved.push(name);
    }
    Ok(resolved)
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
    #[allow(dead_code)]
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
    // Containment (#163): a `..` or absolute `pattern` makes the glob walk
    // outside root, and a symlink under root resolves elsewhere — route glob/
    // grep through the same boundary as read/write by dropping any entry whose
    // canonical path escapes the canonical root (ADR-0054).
    let canon_root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
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
        let contained = p
            .canonicalize()
            .map(|c| c.starts_with(&canon_root))
            .unwrap_or(false);
        if !contained {
            continue;
        }
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

/// Build the **root-contained quintet** (`read`/`glob`/`grep`/`edit`/`write`).
/// Bash is opt-in at the head level (ADR-0010): call [`BashTool::new`] directly and
/// register it when `ENTANGLEMENT_ENABLE_BASH=1`.
pub fn host_tools(root: PathBuf) -> ToolRegistry {
    let mut reg = ToolRegistry::new();
    reg.register(ReadTool::new(root.clone()));
    reg.register(GlobTool::new(root.clone()));
    reg.register(GrepTool::new(root.clone()));
    reg.register(EditTool::new(root.clone()));
    reg.register(WriteTool::new(root.clone()));
    reg
}

#[allow(dead_code)]
pub fn host_tools_with_callbacks<F, G>(root: PathBuf, on_read: F, on_edit: G) -> ToolRegistry
where
    F: Fn(String, Vec<u8>) + Send + Sync + 'static,
    G: Fn(String, Option<Vec<u8>>, Option<Vec<u8>>, FileChangeKind) + Send + Sync + 'static,
{
    let mut reg = ToolRegistry::new();
    reg.register(ReadTool::new(root.clone()).with_on_read(on_read));
    reg.register(GlobTool::new(root.clone()));
    reg.register(GrepTool::new(root.clone()));
    reg.register(EditTool::new(root.clone()).with_on_edit(on_edit));
    reg.register(WriteTool::new(root.clone()));
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

    /// #163: a symlink whose *final component* points outside root must not be
    /// followed by the resolver — canonicalizing the whole path lands outside.
    #[cfg(unix)]
    #[tokio::test]
    async fn resolve_rejects_write_through_final_symlink() {
        let dir = TempDir::new();
        let outside = TempDir::new();
        std::fs::write(outside.join("target.txt"), "secret\n").unwrap();
        std::os::unix::fs::symlink(outside.join("target.txt"), dir.join("link")).unwrap();
        let err = resolve_under_root(&dir.path, "link").unwrap_err();
        assert!(
            format!("{err}").contains("escapes working directory"),
            "{err}"
        );
        // The write actually stays contained: edit through the link is refused.
        let tool = EditTool::new(dir.path.clone());
        let err = tool
            .run(r#"{"path":"link","oldString":"secret","newString":"pwned"}"#)
            .await
            .unwrap_err();
        assert!(
            format!("{err}").contains("escapes working directory"),
            "{err}"
        );
        assert_eq!(
            std::fs::read_to_string(outside.join("target.txt")).unwrap(),
            "secret\n",
            "edit must not have followed the symlink out of tree"
        );
    }

    /// #163: a symlinked *directory* under root can't be used as a parent to
    /// create a new file outside the tree.
    #[cfg(unix)]
    #[test]
    fn resolve_rejects_create_under_symlinked_dir() {
        let dir = TempDir::new();
        let outside = TempDir::new();
        std::os::unix::fs::symlink(&outside.path, dir.join("escape")).unwrap();
        let err = resolve_under_root(&dir.path, "escape/new.txt").unwrap_err();
        assert!(
            format!("{err}").contains("escapes working directory"),
            "{err}"
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
        let resolved = resolve_under_root(&dir.path, "alias").unwrap();
        assert!(resolved.starts_with(dir.path.canonicalize().unwrap()));
        assert!(resolved.ends_with("real.txt"));
    }

    /// #163: creating a genuinely-new file (no symlink) still works — the tail
    /// past the deepest existing ancestor is re-appended verbatim.
    #[test]
    fn resolve_allows_new_nested_file() {
        let dir = TempDir::new();
        let resolved = resolve_under_root(&dir.path, "a/b/c.txt").unwrap();
        assert!(resolved.ends_with("a/b/c.txt"));
        assert!(resolved.starts_with(dir.path.canonicalize().unwrap()));
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
        let list = list_files(&dir.path, "**/*").unwrap();
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
