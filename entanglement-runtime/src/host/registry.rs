//! Assembling the **root-contained sextet**
//! (`read`/`glob`/`grep`/`edit`/`write`/`apply_patch`). The exec pair
//! (`call`/`bash`) is registered by the head alongside it, unconditionally
//! (ADR-0093/ADR-0195).

use std::path::PathBuf;
use std::sync::Arc;

use super::{ApplyPatchTool, EditTool, GlobTool, GrepTool, ReadTool, WriteTool};
use crate::extra_roots::ExtraRootStore;
use crate::file_backup::FileBackups;
use crate::tools::ToolRegistry;
use crate::workdir::SessionWorkdirs;

/// The optional wiring for [`host_tools_with`]; `Default` is the strict,
/// backup-free, root-relative sextet.
#[derive(Clone, Default)]
pub struct HostToolOptions {
    /// Escape-root grants (ADR-0109): the path-touching quartet reaches an
    /// approved out-of-root path, and `glob`/`grep` (#482) widen a search into
    /// a directory already covered by a durable `read` grant.
    pub extra_roots: Option<Arc<ExtraRootStore>>,
    /// Per-write backups (ADR-0212) for `edit`/`write`/`apply_patch` — the
    /// undo/rewind source.
    pub backups: Option<FileBackups>,
    /// The executor's per-session workdirs (#764, ADR-0224): `glob`/`grep`
    /// list matches relative to the calling session's workdir.
    pub workdirs: Option<SessionWorkdirs>,
}

/// The strict sextet.
pub fn host_tools(root: PathBuf) -> ToolRegistry {
    host_tools_with(root, HostToolOptions::default())
}

/// [`host_tools`] with an optional escape-root grant store (ADR-0109).
pub fn host_tools_with_extra_roots(
    root: PathBuf,
    extra: Option<Arc<ExtraRootStore>>,
) -> ToolRegistry {
    host_tools_with(
        root,
        HostToolOptions {
            extra_roots: extra,
            ..HostToolOptions::default()
        },
    )
}

/// The sextet with every optional piece of [`HostToolOptions`] wired in.
pub fn host_tools_with(root: PathBuf, opts: HostToolOptions) -> ToolRegistry {
    let HostToolOptions {
        extra_roots,
        backups,
        workdirs,
    } = opts;
    let mut read = ReadTool::new(root.clone());
    let mut glob = GlobTool::new(root.clone());
    let mut grep = GrepTool::new(root.clone());
    let mut edit = EditTool::new(root.clone());
    let mut write = WriteTool::new(root.clone());
    let mut apply_patch = ApplyPatchTool::new(root);
    if let Some(e) = &extra_roots {
        read = read.with_extra_roots(e.clone());
        glob = glob.with_extra_roots(e.clone());
        grep = grep.with_extra_roots(e.clone());
        edit = edit.with_extra_roots(e.clone());
        write = write.with_extra_roots(e.clone());
        apply_patch = apply_patch.with_extra_roots(e.clone());
    }
    if let Some(w) = workdirs {
        glob = glob.with_workdirs(w.clone());
        grep = grep.with_workdirs(w);
    }
    if let Some(b) = backups {
        edit = edit.with_backups(b.clone());
        write = write.with_backups(b.clone());
        apply_patch = apply_patch.with_backups(b);
    }
    let mut reg = ToolRegistry::new();
    reg.register(read);
    reg.register(glob);
    reg.register(grep);
    reg.register(edit);
    reg.register(write);
    reg.register(apply_patch);
    reg
}
