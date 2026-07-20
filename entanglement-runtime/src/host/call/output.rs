//! Where `call`'s full untailed output is durably written: either the
//! model-requested `output_file` or an auto-named default artifact under the
//! runtime scratch dir, plus its `.stderr` sibling (#381). Split out of
//! `mod.rs` (issue #451) — this is pure path/IO logic independent of process
//! spawning.

use crate::host::resolve_under_root;
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// Per-process counter disambiguating default artifact filenames across
/// concurrent `call` invocations sharing one pid.
static CALL_SEQ: AtomicU64 = AtomicU64::new(0);

/// Where the full raw stdout (and its `.stderr` sibling) get written — either
/// the model-requested `output_file` or an auto-named default artifact.
pub(super) struct OutputTarget {
    pub(super) stdout_abs: PathBuf,
    pub(super) stderr_abs: PathBuf,
    /// Root-relative stdout path, named in the result header.
    pub(super) rel: String,
    /// Explicit (`output_file` given) → a write failure is a hard error.
    /// Default (auto-named) → a write failure is best-effort (log + notice).
    pub(super) explicit: bool,
}

fn stderr_sibling(path: &Path) -> PathBuf {
    let mut s = path.as_os_str().to_os_string();
    s.push(".stderr");
    PathBuf::from(s)
}

pub(super) fn resolve_output_target(
    root: &Path,
    scratch_base: Option<&Path>,
    output_file: &Option<String>,
) -> Result<OutputTarget> {
    match output_file {
        Some(rel) => {
            let stdout_abs = resolve_under_root(root, rel)?;
            let stderr_abs = stderr_sibling(&stdout_abs);
            Ok(OutputTarget {
                stdout_abs,
                stderr_abs,
                rel: rel.clone(),
                explicit: true,
            })
        }
        None => {
            let seq = CALL_SEQ.fetch_add(1, Ordering::Relaxed);
            let name = format!("call-output/call-{}-{seq}.stdout", std::process::id());
            // Default artifacts go to the runtime-owned per-project scratch dir
            // (outside the repo). The header names the absolute path since it is
            // no longer root-relative. Standalone/test constructors with no
            // scratch base fall back to the legacy in-repo location.
            let stdout_abs = match scratch_base {
                Some(base) => base.join(&name),
                None => root.join(".entanglement/tmp").join(&name),
            };
            let stderr_abs = stderr_sibling(&stdout_abs);
            let rel = stdout_abs.display().to_string();
            Ok(OutputTarget {
                stdout_abs,
                stderr_abs,
                rel,
                explicit: false,
            })
        }
    }
}

/// Write the full raw stdout/stderr to `target`, creating missing parent dirs.
/// An explicit (`output_file`) failure propagates as a hard error — it was
/// requested. A default-artifact failure is logged and returned as a degraded
/// notice instead, so an unrelated disk issue can't fail a command that would
/// otherwise have succeeded.
pub(super) async fn persist_output(
    target: &OutputTarget,
    stdout: &[u8],
    stderr: &[u8],
) -> Result<Option<String>> {
    let result: Result<()> = async {
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
    .await;

    match result {
        Ok(()) => Ok(None),
        Err(e) if target.explicit => Err(e),
        Err(e) => {
            tracing::warn!("call: failed to write default output artifact: {e:#}");
            Ok(Some(format!("[output artifact write failed: {e:#}]\n")))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn default_artifact_goes_to_scratch_base_not_the_repo() {
        let root = TempDir::new();
        let scratch = TempDir::new();
        let target = resolve_output_target(&root.path, Some(&scratch.path), &None).unwrap();
        assert!(!target.explicit);
        assert!(
            target.stdout_abs.starts_with(&scratch.path),
            "default artifact under scratch: {}",
            target.stdout_abs.display()
        );
        assert!(
            !target.stdout_abs.starts_with(&root.path),
            "default artifact must NOT be under the project root: {}",
            target.stdout_abs.display()
        );
        // The header names the absolute scratch path.
        assert_eq!(target.rel, target.stdout_abs.display().to_string());
    }

    #[test]
    fn default_artifact_falls_back_to_repo_without_scratch_base() {
        let root = TempDir::new();
        let target = resolve_output_target(&root.path, None, &None).unwrap();
        assert!(target
            .stdout_abs
            .starts_with(root.path.join(".entanglement/tmp")));
    }

    #[test]
    fn explicit_output_file_stays_contained_to_root() {
        let root = TempDir::new();
        let scratch = TempDir::new();
        let target = resolve_output_target(
            &root.path,
            Some(&scratch.path),
            &Some("out/log.txt".to_string()),
        )
        .unwrap();
        assert!(target.explicit);
        assert!(
            target.stdout_abs.starts_with(&root.path),
            "explicit output_file stays under root: {}",
            target.stdout_abs.display()
        );
        // A path escaping root is still refused.
        assert!(resolve_output_target(
            &root.path,
            Some(&scratch.path),
            &Some("../escape.txt".to_string()),
        )
        .is_err());
    }
}
