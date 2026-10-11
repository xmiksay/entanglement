//! Session working directories (#764,
//! [ADR-0224](../../docs/adr/0224-session-working-directory.md)) — the
//! runtime half.
//!
//! Core carries a session's workdir as an opaque absolute path: it stores,
//! replays and stamps it on every `ToolExec`, but does no file I/O. This
//! module owns everything that touches the disk:
//!
//! - [`resolver`] is the `EngineConfig::workdir_resolver` seam — canonicalize,
//!   require a directory, require it inside the project root or under an
//!   escape-root grant (ADR-0109), recording that grant when the trusted
//!   sender approved it.
//! - [`SessionWorkdirs`] is the executor's per-session view, fed from each
//!   `ToolExec`, which `glob`/`grep` read to print paths relative to it.
//! - [`rewrite`] turns a call's relative path arguments into root-relative
//!   ones *before* anything grades it, so permission rules, grants, the
//!   escape-root gate and every host tool's own containment check see exactly
//!   what they saw before workdirs existed (ADR-0125 is unchanged).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use entanglement_core::{ApprovalScope, SessionId, WorkdirResolver};

use crate::extra_roots::ExtraRootStore;

mod rewrite;
pub(crate) use rewrite::rewrite;

/// The exec tools whose default cwd an out-of-root workdir becomes — the
/// escape-root grant `/cd` records is keyed on these. File tools keep their
/// own per-path escape-root rules there.
const EXEC_TOOLS: [&str; 2] = ["bash", "call"];

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
            for tool in EXEC_TOOLS {
                extra.record(tool, &canon, scope, "");
            }
            Ok(canon)
        }
        None if EXEC_TOOLS
            .iter()
            .all(|tool| extra.is_durably_allowed(tool, &canon)) =>
        {
            Ok(canon)
        }
        None => Err(format!(
            "{} is outside the project root {} — approve it for this run or always \
             (TUI: `/cd --session <dir>` or `/cd --always <dir>`)",
            canon.display(),
            root.display()
        )),
    }
}

/// Each session's working directory as the executor last saw it on a
/// `ToolExec` — the call carries it, so this is always current for the call
/// being run. Shared with the search tools, which print paths relative to it.
#[derive(Clone, Default)]
pub struct SessionWorkdirs(Arc<RwLock<HashMap<SessionId, PathBuf>>>);

impl SessionWorkdirs {
    /// Record the workdir a `ToolExec` carried (`None`: the project root).
    pub(crate) fn observe(&self, session: &SessionId, workdir: Option<&str>) {
        let mut map = self.0.write().expect("session-workdir lock poisoned");
        match workdir {
            Some(dir) => map.insert(session.clone(), PathBuf::from(dir)),
            None => map.remove(session),
        };
    }

    /// The session's workdir, `None` for the project root.
    pub(crate) fn get(&self, session: &SessionId) -> Option<PathBuf> {
        self.0
            .read()
            .expect("session-workdir lock poisoned")
            .get(session)
            .cloned()
    }

    pub(crate) fn forget(&self, session: &SessionId) {
        self.0
            .write()
            .expect("session-workdir lock poisoned")
            .remove(session);
    }

    /// The directory a search tool prints paths relative to: the session's
    /// workdir, else `root`. A path outside it prints absolute, which the
    /// tools accept back unchanged.
    pub(crate) fn display_root(this: Option<&Self>, session: &SessionId, root: &Path) -> PathBuf {
        this.and_then(|w| w.get(session))
            .unwrap_or_else(|| root.to_path_buf())
    }
}

#[cfg(test)]
mod tests;
