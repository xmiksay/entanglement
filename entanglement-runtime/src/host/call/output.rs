//! Where `call`'s explicit `output_file` gets written — the full raw stdout
//! plus its `.stderr` sibling (#381). Split out of `mod.rs` (issue #451) —
//! this is pure path/IO logic independent of process spawning.
//!
//! There is no default/implicit artifact anymore (#608, ADR-0161 §7): a
//! result with no `output_file` that overflows its cap is retained in memory
//! instead (`crate::retained_output::RetainedOutputRegistry`) and paged via
//! `poll`, not written to a scratch file.

use crate::host::resolve_under_root;
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

/// Where the full raw stdout (and its `.stderr` sibling) get written for a
/// model-requested `output_file`.
pub(super) struct OutputTarget {
    pub(super) stdout_abs: PathBuf,
    pub(super) stderr_abs: PathBuf,
    /// Root-relative stdout path, named in the result header.
    pub(super) rel: String,
}

fn stderr_sibling(path: &Path) -> PathBuf {
    let mut s = path.as_os_str().to_os_string();
    s.push(".stderr");
    PathBuf::from(s)
}

/// Resolve `output_file` to an [`OutputTarget`], or `None` when the model gave
/// none — the caller then has no file to write and no path to name.
pub(super) fn resolve_output_target(
    root: &Path,
    output_file: &Option<String>,
) -> Result<Option<OutputTarget>> {
    let Some(rel) = output_file else {
        return Ok(None);
    };
    let stdout_abs = resolve_under_root(root, rel)?;
    let stderr_abs = stderr_sibling(&stdout_abs);
    Ok(Some(OutputTarget {
        stdout_abs,
        stderr_abs,
        rel: rel.clone(),
    }))
}

/// Write the full raw stdout/stderr to `target`, creating missing parent
/// dirs. A write failure is a hard error — an explicit `output_file` was
/// requested.
pub(super) async fn persist_output(
    target: &OutputTarget,
    stdout: &[u8],
    stderr: &[u8],
) -> Result<()> {
    if let Some(parent) = target.stdout_abs.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .context("creating output_file parent dirs")?;
    }
    tokio::fs::write(&target.stdout_abs, stdout)
        .await
        .context("writing output_file")?;
    tokio::fs::write(&target.stderr_abs, stderr)
        .await
        .context("writing output_file stderr sibling")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEST_DIR_COUNTER: AtomicU64 = AtomicU64::new(0);

    /// Isolated per-test root so artifact-writing tests don't collide (and so
    /// their `.entanglement/` litter doesn't accumulate in a shared temp dir).
    struct TempDir {
        path: PathBuf,
    }
    impl TempDir {
        fn new() -> TempDir {
            let id = TEST_DIR_COUNTER.fetch_add(1, Ordering::SeqCst);
            let path = std::env::temp_dir().join(format!(
                "entanglement-call-output-{}-{id}",
                std::process::id()
            ));
            std::fs::create_dir_all(&path).unwrap();
            TempDir { path }
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    #[test]
    fn no_output_file_resolves_to_none() {
        let root = TempDir::new();
        assert!(resolve_output_target(&root.path, &None).unwrap().is_none());
    }

    #[test]
    fn explicit_output_file_stays_contained_to_root() {
        let root = TempDir::new();
        let target = resolve_output_target(&root.path, &Some("out/log.txt".to_string()))
            .unwrap()
            .unwrap();
        assert!(
            target.stdout_abs.starts_with(&root.path),
            "explicit output_file stays under root: {}",
            target.stdout_abs.display()
        );
        assert_eq!(target.rel, "out/log.txt");

        // A path escaping root is still refused.
        assert!(resolve_output_target(&root.path, &Some("../escape.txt".to_string())).is_err());
    }

    #[tokio::test]
    async fn persist_output_writes_stdout_and_stderr_sibling() {
        let root = TempDir::new();
        let target = resolve_output_target(&root.path, &Some("out/log.txt".to_string()))
            .unwrap()
            .unwrap();
        persist_output(&target, b"out-bytes", b"err-bytes")
            .await
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(root.path.join("out/log.txt")).unwrap(),
            "out-bytes"
        );
        assert_eq!(
            std::fs::read_to_string(root.path.join("out/log.txt.stderr")).unwrap(),
            "err-bytes"
        );
    }
}
