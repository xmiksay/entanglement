//! Session working directories (#764,
//! [ADR-0224](../../../docs/adr/archive/0224-session-working-directory.md)) —
//! `skutter`'s `/cd` (moved from the library by #968).
//!
//! Core carries a session's workdir as an opaque absolute path: it stores,
//! replays and stamps it on every `ToolExec`, but does no file I/O. This
//! module is the `/cd` side that touches the disk: [`resolver`] is the
//! `EngineConfig::workdir_resolver` seam — canonicalize, require a
//! directory, require it inside the project root or under an escape-root
//! grant (ADR-0109), recording that grant when the trusted sender approved
//! it. The executor's per-session view and the rewrite of relative path
//! arguments live in the library's `path_grading`.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use entanglement_core::{ApprovalScope, WorkdirResolver};

use crate::state::ExtraRootStore;

/// The exec tool whose default cwd an out-of-root workdir becomes — the
/// escape-root grant `/cd` records is keyed on it. File tools keep their
/// own per-path escape-root rules there.
const EXEC_TOOL: &str = "bash";

/// Build the `EngineConfig::workdir_resolver` seam over the canonical
/// project `root` and the shared escape-root store (`None`: strict
/// containment, nothing outside the root is ever accepted).
pub fn resolver(root: PathBuf, extra: Option<Arc<ExtraRootStore>>) -> WorkdirResolver {
    Arc::new(move |current, dir, approve| {
        resolve(&root, extra.as_deref(), current, dir, approve)
            .map(|p| p.to_string_lossy().into_owned())
    })
}

/// Validate `dir` (absolute, or relative to `current` — the root when
/// `None`) as a session working directory. Inside the root it is accepted
/// as is. Outside it needs an escape-root grant for the exec tools: an
/// existing durable one, or the `Session`/`Always` approval the caller sent,
/// which is recorded here. A one-shot approval cannot back a workdir that
/// every later command runs in.
pub fn resolve(
    root: &Path,
    extra: Option<&ExtraRootStore>,
    current: Option<&str>,
    dir: &str,
    approve: Option<ApprovalScope>,
) -> Result<PathBuf, String> {
    let dir = dir.trim();
    if dir.is_empty() {
        return Err("empty directory".to_string());
    }
    let base = current.map_or_else(|| root.to_path_buf(), PathBuf::from);
    let path = base.join(dir);
    let canon = path
        .canonicalize()
        .map_err(|e| format!("{}: {e}", path.display()))?;
    if !canon.is_dir() {
        return Err(format!("not a directory: {}", canon.display()));
    }
    if canon.starts_with(root) {
        return Ok(canon);
    }
    let Some(extra) = extra else {
        return Err(format!(
            "{} is outside the project root {}",
            canon.display(),
            root.display()
        ));
    };
    match approve {
        Some(ApprovalScope::Once) => Err(
            "a working directory outside the project root needs a session or always approval"
                .to_string(),
        ),
        Some(scope) => {
            extra.record(EXEC_TOOL, &canon, scope);
            Ok(canon)
        }
        None if extra.is_durably_allowed(EXEC_TOOL, &canon) => Ok(canon),
        None => Err(format!(
            "{} is outside the project root {} — approve it for this run or always \
             (TUI: `/cd --session <dir>` or `/cd --always <dir>`)",
            canon.display(),
            root.display()
        )),
    }
}

#[cfg(test)]
mod tests;
