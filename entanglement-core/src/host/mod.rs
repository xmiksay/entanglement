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

pub mod bash;
pub mod edit;
pub mod glob;
pub mod grep;
pub mod read;

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

/// Enumerate files under `root` matching `pattern` (a glob relative to root),
/// yielding display paths relative to root. Skips directories and unreadable
/// entries. Bounds the walk at [`MAX_RESULTS`] paths.
pub fn list_files(root: &Path, pattern: &str) -> Result<Vec<PathBuf>> {
    let abs = root.join(pattern).to_string_lossy().into_owned();
    let entries = ::glob::glob(&abs).with_context(|| format!("invalid glob: {pattern}"))?;
    let mut out = Vec::new();
    for entry in entries {
        let p = match entry {
            Ok(p) => p,
            Err(_) => continue,
        };
        if std::fs::metadata(&p).map(|m| m.is_file()).unwrap_or(false) {
            out.push(p);
            if out.len() >= MAX_RESULTS {
                break;
            }
        }
    }
    Ok(out)
}

// ┗━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// ┃ host_tools registry
// ┗━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

/// Build the **root-contained set** (`read`/`glob`/`grep`/`edit`).
/// Bash is opt-in at the head level (ADR-0010): call [`BashTool::new`] directly and
/// register it when `ENTANGLEMENT_ENABLE_BASH=1`.
pub fn host_tools(root: PathBuf) -> ToolRegistry {
    let mut reg = ToolRegistry::new();
    reg.register(ReadTool::new(root.clone()));
    reg.register(GlobTool::new(root.clone()));
    reg.register(GrepTool::new(root.clone()));
    reg.register(EditTool::new(root));
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

    #[tokio::test]
    async fn read_returns_lines_with_numbers() {
        let dir = TempDir::new();
        fs::write(dir.join("a.txt"), "alpha\nbeta\n").unwrap();
        let tool = ReadTool::new(dir.path.clone());
        let out = tool.run(r#"{"path":"a.txt"}"#).await.unwrap();
        assert!(out.contains("1: alpha"), "got: {out}");
        assert!(out.contains("2: beta"), "got: {out}");
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
        assert!(!names.contains(&"apply_diff"), "{names:?}");
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
