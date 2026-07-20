//! Host tools that execute against the local filesystem and shell — `read`,
//! `glob`, `grep`, `edit`, `write`, `apply_patch`, `call`, and the opt-in
//! `bash`. The read-only trio (`read`/`glob`/`grep`) is covered by ADR-0008;
//! `edit`/`bash` by ADR-0009/ADR-0012; whole-file `write` by ADR-0031;
//! multi-hunk `apply_patch` (unified-diff apply, beside `edit`/`write`) by
//! #455; the argv-exec `call` (no shell, auto-tailed output) by ADR-0045;
//! [`host_tools`] assembles the **root-contained sextet**
//! (`read`/`glob`/`grep`/`edit`/`write`/`apply_patch`) — a head registers
//! [`CallTool`] unconditionally alongside it and opts into [`BashTool`]
//! separately (gated by `ENTANGLEMENT_ENABLE_BASH`) — see ADR-0010, amended
//! by ADR-0093 for `call`'s registration.
//!
//! Each tool is constructed with a working-directory `root`; model-supplied
//! paths resolve against it and are **rejected on `..` escape** *and* on
//! **symlink escape** — the resolved target's deepest existing ancestor is
//! canonicalized and must stay under the canonical root (ADR-0054, #163), so a
//! `root/link -> /etc` symlink can't be followed out of tree by `read`/`edit`/
//! `write`/`apply_patch`, and `glob`/`grep` drop any match whose canonical
//! path escapes.
//! Output is byte-capped so a runaway
//! listing or huge file can't silently consume the context window. `bash`/
//! `call` run the command rooted at `root` (or at a validated `workdir`) but
//! otherwise inherit the engine process's full privileges by default —
//! unsandboxed unless opted in (ADR-0009/ADR-0045); registration (opt-in for
//! `bash`, unconditional for `call`) plus permission profiles are the default
//! controls (ADR-0010/ADR-0093). [`sandbox`] adds an optional bubblewrap
//! confinement layer for both (ADR-0104, `ENTANGLEMENT_SANDBOX=bwrap`).

use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result};

use crate::tools::ToolRegistry;

pub mod apply_patch;
pub mod bash;
pub mod bash_output;
pub mod call;
pub mod edit;
pub mod exec;
pub mod glob;
pub mod grep;
pub mod jobs;
pub mod read;
pub mod sandbox;
pub mod unified_diff;
pub mod write;

pub use apply_patch::ApplyPatchTool;
pub use bash::BashTool;
pub use bash_output::BashOutputTool;
pub use call::CallTool;
pub use edit::EditTool;
pub use glob::GlobTool;
pub use grep::GrepTool;
pub use jobs::JobRegistry;
pub use read::{ReadRawTool, ReadTool};
pub use sandbox::{SandboxBackend, SandboxPolicy};
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
    let (resolved, contained) = resolve_and_contained(root, rel)?;
    if !contained {
        return Err(anyhow::anyhow!("path escapes working directory: {rel}"));
    }
    Ok(resolved)
}

/// Like [`resolve_under_root`], but a path that escapes root is permitted when
/// the user has granted `tool` access to it (ADR-0109). `extra == None` (the
/// standalone/test constructors) reduces to the strict containment of
/// [`resolve_under_root`]. A `Once` grant is **consumed** by this call — and
/// only if `request_id` matches the call it was approved for (#449), so a
/// different concurrently-running call to the same `(tool, path)` can't spend
/// someone else's single-use token; a durable (`Session`/`Always`) grant falls
/// back to path-only matching and ignores `request_id`.
pub fn resolve_under_root_or_grant(
    root: &Path,
    extra: Option<&crate::extra_roots::ExtraRootStore>,
    tool: &str,
    request_id: &str,
    rel: &str,
) -> Result<PathBuf> {
    let (resolved, contained) = resolve_and_contained(root, rel)?;
    if contained {
        return Ok(resolved);
    }
    // Escapes root — allow only if the user approved this exact `(tool, path)`.
    // The grant is checked against the *resolved* (symlink-canonicalized) target,
    // so a symlink under root can't smuggle access to an unapproved path.
    if extra.is_some_and(|e| e.take_allowance(tool, &resolved, request_id)) {
        return Ok(resolved);
    }
    Err(anyhow::anyhow!("path escapes working directory: {rel}"))
}

/// The absolute out-of-root target `rel` resolves to under `root`, or `None`
/// when `rel` stays contained (or can't be resolved at all). Used by the
/// executor's escape-root gate (ADR-0109) to decide whether a call needs
/// approval, matching the exact resolution the host tools use so the grant key
/// (the resolved path) is identical on both sides.
pub fn escaping_path(root: &Path, rel: &str) -> Option<PathBuf> {
    match resolve_and_contained(root, rel) {
        Ok((abs, false)) => Some(abs),
        _ => None,
    }
}

/// Resolve `rel` against `root` — lexical `..`/`.` normalization plus symlink-safe
/// canonicalization of the deepest existing ancestor (#163) — returning the
/// absolute target and whether it stays contained under `root`. The containment
/// flag is the AND of the lexical and canonical checks; a strict caller rejects
/// `false`, an escape-root caller (ADR-0109) may still use the path after a grant
/// check. A genuinely malformed path (a `..` popping past the filesystem root)
/// is still a hard `Err`.
fn resolve_and_contained(root: &Path, rel: &str) -> Result<(PathBuf, bool)> {
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
    let lexically_contained = norm.starts_with(&root);
    // Symlink defense (#163): canonicalize the deepest existing ancestor so a
    // symlink under root can't redirect the real target outside it. If the whole
    // path exists (incl. a final-component symlink) it canonicalizes directly;
    // otherwise we peel not-yet-existing tail components off until an ancestor
    // resolves, then re-append the plain tail.
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
    let canonically_contained = canon.starts_with(&root);
    let mut resolved = canon;
    for name in tail.into_iter().rev() {
        resolved.push(name);
    }
    Ok((resolved, lexically_contained && canonically_contained))
}

/// Resolve the per-call working directory for an exec tool (`bash`/`call`):
/// `root` by default, or a model-supplied `workdir` validated to stay under
/// root (same symlink-safe containment as the filesystem tools, ADR-0054/#163)
/// and to be a directory. Shared by [`crate::host::bash::BashTool`] and
/// [`crate::host::call::CallTool`] — the containment + directory check is
/// identical between the two (#386).
pub fn resolve_workdir(root: &Path, workdir: Option<&str>) -> Result<PathBuf> {
    resolve_workdir_or_grant(root, None, "", "", workdir)
}

/// [`resolve_workdir`] with an escape-root grant escape hatch (ADR-0109): a
/// `workdir` outside root is allowed when the user granted `tool` access to it.
/// `extra == None` reduces to the strict [`resolve_workdir`]. `request_id`
/// (#449) is forwarded to [`resolve_under_root_or_grant`] so a `Once` grant is
/// only consumed by the call it was approved for.
pub fn resolve_workdir_or_grant(
    root: &Path,
    extra: Option<&crate::extra_roots::ExtraRootStore>,
    tool: &str,
    request_id: &str,
    workdir: Option<&str>,
) -> Result<PathBuf> {
    match workdir {
        None => Ok(root.to_path_buf()),
        Some(w) => {
            let p = resolve_under_root_or_grant(root, extra, tool, request_id, w)?;
            if !p.is_dir() {
                anyhow::bail!("workdir is not a directory: {w}");
            }
            Ok(p)
        }
    }
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

/// Byte-cap `s` at [`MAX_OUTPUT_BYTES`] keeping a **head + tail** slice, with a
/// notice naming the omitted middle. For build/test output the *tail* (the
/// error, the failing assertion, the summary line) is the load-bearing part, so
/// head-only truncation ([`truncate_output`]) throws away exactly what the model
/// needs — the tail gets three-quarters of the budget, the head one quarter for
/// the invocation context (#170). Cuts land on UTF-8 boundaries.
pub fn truncate_head_tail(s: String) -> String {
    if s.len() <= MAX_OUTPUT_BYTES {
        return s;
    }
    let head_budget = MAX_OUTPUT_BYTES / 4;
    let tail_budget = MAX_OUTPUT_BYTES - head_budget;
    let mut head_end = head_budget;
    while head_end > 0 && !s.is_char_boundary(head_end) {
        head_end -= 1;
    }
    let mut tail_start = s.len() - tail_budget;
    while tail_start < s.len() && !s.is_char_boundary(tail_start) {
        tail_start += 1;
    }
    let omitted = tail_start - head_end;
    let mut out = String::with_capacity(head_end + (s.len() - tail_start) + 64);
    out.push_str(&s[..head_end]);
    out.push_str(&format!(
        "\n... [truncated: {omitted} bytes omitted from the middle; {} bytes total]\n",
        s.len()
    ));
    out.push_str(&s[tail_start..]);
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
/// yielding display paths relative to root. `excludes` is a caller-supplied
/// list of glob patterns (matched against the root-relative path) additional
/// entries are dropped for — on top of that, any path with a `.git` path
/// component is **always** dropped, unconditionally: `.git` internals are
/// large, mostly binary, and never something an agent needs to read or
/// search, so the exclusion isn't tied to (and can't be defeated by) the
/// `excludes` list (ADR-0099). Skips directories (counted in
/// [`FileList::matched_dirs`]) and logs unreadable entries as `warn!` (counted
/// in [`FileList::skipped_errors`]) instead of silently dropping them.
/// Excluded entries are dropped before either count, so an excluded subtree
/// looks to the caller like it was never in the walk at all. Bounds the walk
/// at [`MAX_RESULTS`] files.
pub fn list_files(root: &Path, pattern: &str, excludes: &[String]) -> Result<FileList> {
    let abs = root.join(pattern).to_string_lossy().into_owned();
    let entries = ::glob::glob(&abs).with_context(|| format!("invalid glob: {pattern}"))?;
    let exclude_patterns: Vec<::glob::Pattern> = excludes
        .iter()
        .map(|p| ::glob::Pattern::new(p).with_context(|| format!("invalid exclude pattern: {p}")))
        .collect::<Result<_>>()?;
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
        if p.components().any(|c| c.as_os_str() == ".git") {
            continue;
        }
        if !exclude_patterns.is_empty() {
            let rel = p.strip_prefix(root).unwrap_or(&p).to_string_lossy();
            if exclude_patterns.iter().any(|pat| pat.matches(&rel)) {
                continue;
            }
        }
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

/// Build the **root-contained sextet**
/// (`read`/`glob`/`grep`/`edit`/`write`/`apply_patch`). Bash is opt-in at the
/// head level (ADR-0010): call [`BashTool::new`] directly and register it
/// when `ENTANGLEMENT_ENABLE_BASH=1`.
pub fn host_tools(root: PathBuf) -> ToolRegistry {
    host_tools_with_extra_roots(root, None)
}

/// [`host_tools`] with an optional escape-root grant store (ADR-0109) wired into
/// the path-touching tools (`read`/`edit`/`write`/`apply_patch`), so an approved
/// out-of-root path is reachable. `glob`/`grep` stay strictly root-contained
/// (their pattern-relative search has no single path to approve). `None` is
/// byte-identical to the pre-ADR-0109 strict sextet.
pub fn host_tools_with_extra_roots(
    root: PathBuf,
    extra: Option<std::sync::Arc<crate::extra_roots::ExtraRootStore>>,
) -> ToolRegistry {
    let mut reg = ToolRegistry::new();
    let mut read = ReadTool::new(root.clone());
    let mut edit = EditTool::new(root.clone());
    let mut write = WriteTool::new(root.clone());
    let mut apply_patch = ApplyPatchTool::new(root.clone());
    if let Some(e) = &extra {
        read = read.with_extra_roots(e.clone());
        edit = edit.with_extra_roots(e.clone());
        write = write.with_extra_roots(e.clone());
        apply_patch = apply_patch.with_extra_roots(e.clone());
    }
    reg.register(read);
    reg.register(GlobTool::new(root.clone()));
    reg.register(GrepTool::new(root.clone()));
    reg.register(edit);
    reg.register(write);
    reg.register(apply_patch);
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

    #[test]
    fn truncate_head_tail_keeps_both_ends_under_cap() {
        // A body far over the cap, with unique head and tail markers.
        let mut s = String::from("HEAD_MARKER");
        s.push_str(&"x".repeat(MAX_OUTPUT_BYTES * 2));
        s.push_str("TAIL_MARKER");
        let out = truncate_head_tail(s.clone());
        assert!(
            out.len() < MAX_OUTPUT_BYTES + 128,
            "over cap: {}",
            out.len()
        );
        assert!(out.starts_with("HEAD_MARKER"), "head lost");
        assert!(out.ends_with("TAIL_MARKER"), "tail lost");
        assert!(out.contains("omitted from the middle"), "notice missing");
    }

    #[test]
    fn truncate_head_tail_passes_through_small_input() {
        let s = "small output\n".to_string();
        assert_eq!(truncate_head_tail(s.clone()), s);
    }

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
        assert!(!list.matched_anything());
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

    /// ADR-0109: an out-of-root path is refused with no grant, allowed once a
    /// grant for that exact `(tool, path)` exists, and never satisfies a
    /// different tool's check.
    #[test]
    fn escape_root_grant_permits_only_the_granted_tool_and_path() {
        use crate::extra_roots::ExtraRootStore;
        use std::sync::Arc;
        let dir = TempDir::new();
        let outside = TempDir::new();
        std::fs::write(outside.join("f.txt"), "x\n").unwrap();
        let target = outside.join("f.txt");
        let target_str = target.to_string_lossy().into_owned();

        let store = Arc::new(ExtraRootStore::ephemeral());
        // No grant → still refused, even with a store present.
        let err =
            resolve_under_root_or_grant(&dir.path, Some(&store), "read", "req-1", &target_str)
                .unwrap_err();
        assert!(
            format!("{err}").contains("escapes working directory"),
            "{err}"
        );

        // Grant `read` on this path (session scope) → allowed for `read`,
        // still refused for `write` (per-tool isolation).
        let canon = escaping_path(&dir.path, &target_str).expect("escapes root");
        store.record(
            "read",
            &canon,
            entanglement_core::ApprovalScope::Session,
            "req-1",
        );
        let ok = resolve_under_root_or_grant(&dir.path, Some(&store), "read", "req-1", &target_str)
            .unwrap();
        assert_eq!(ok, canon);
        let err =
            resolve_under_root_or_grant(&dir.path, Some(&store), "write", "req-1", &target_str)
                .unwrap_err();
        assert!(
            format!("{err}").contains("escapes working directory"),
            "{err}"
        );
    }

    /// #449: a `Once` grant approved for one request id can't be consumed by a
    /// different concurrently-running call to the same `(tool, path)` — the
    /// "loser" still sees the strict-containment refusal, and only the call the
    /// approval was actually for can redeem it.
    #[test]
    fn escape_root_once_grant_is_bound_to_its_request_id() {
        use crate::extra_roots::ExtraRootStore;
        use std::sync::Arc;
        let dir = TempDir::new();
        let outside = TempDir::new();
        std::fs::write(outside.join("f.txt"), "x\n").unwrap();
        let target = outside.join("f.txt");
        let target_str = target.to_string_lossy().into_owned();

        let store = Arc::new(ExtraRootStore::ephemeral());
        let canon = escaping_path(&dir.path, &target_str).expect("escapes root");
        // Approved for request "req-A" only.
        store.record(
            "read",
            &canon,
            entanglement_core::ApprovalScope::Once,
            "req-A",
        );

        // A concurrent, unrelated call ("req-B") to the same (tool, path) must
        // not be able to spend req-A's single-use token.
        let err =
            resolve_under_root_or_grant(&dir.path, Some(&store), "read", "req-B", &target_str)
                .unwrap_err();
        assert!(
            format!("{err}").contains("escapes working directory"),
            "{err}"
        );

        // The approved call can still redeem its own token, exactly once.
        let ok = resolve_under_root_or_grant(&dir.path, Some(&store), "read", "req-A", &target_str)
            .unwrap();
        assert_eq!(ok, canon);
        let err =
            resolve_under_root_or_grant(&dir.path, Some(&store), "read", "req-A", &target_str)
                .unwrap_err();
        assert!(
            format!("{err}").contains("escapes working directory"),
            "{err}"
        );
    }

    /// ADR-0109: an in-root path is unaffected by the grant machinery.
    #[test]
    fn escape_root_grant_does_not_change_in_root_resolution() {
        use crate::extra_roots::ExtraRootStore;
        use std::sync::Arc;
        let dir = TempDir::new();
        let store = Arc::new(ExtraRootStore::ephemeral());
        let resolved =
            resolve_under_root_or_grant(&dir.path, Some(&store), "read", "req-1", "a/b.txt")
                .unwrap();
        assert!(resolved.ends_with("a/b.txt"));
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
