//! Assembling the **root-contained sextet**
//! (`read`/`glob`/`grep`/`edit`/`write`/`apply_patch`). `bash` is
//! registered by the head alongside it, unconditionally (ADR-0195).

use std::path::PathBuf;

use super::{ApplyPatchTool, EditTool, GlobTool, GrepTool, ReadTool, WriteTool};
use crate::file_backup::FileBackups;
use crate::path_grading::SessionWorkdirs;
use crate::tools::ToolRegistry;

/// The optional wiring for [`host_tools_with`]; `Default` is the
/// backup-free, root-relative sextet.
#[derive(Clone, Default)]
pub struct HostToolOptions {
    /// Per-write backups (ADR-0212) for `edit`/`write`/`apply_patch` — the
    /// undo/rewind source.
    pub backups: Option<FileBackups>,
    /// The executor's per-session workdirs (#764, ADR-0224): `glob`/`grep`
    /// list matches relative to the calling session's workdir.
    pub workdirs: Option<SessionWorkdirs>,
}

/// The sextet with no backups.
pub fn host_tools(root: PathBuf) -> ToolRegistry {
    host_tools_with(root, HostToolOptions::default())
}

/// The sextet with every optional piece of [`HostToolOptions`] wired in.
pub fn host_tools_with(root: PathBuf, opts: HostToolOptions) -> ToolRegistry {
    let HostToolOptions { backups, workdirs } = opts;
    let read = ReadTool::new(root.clone());
    let mut glob = GlobTool::new(root.clone());
    let mut grep = GrepTool::new(root.clone());
    let mut edit = EditTool::new(root.clone());
    let mut write = WriteTool::new(root.clone());
    let mut apply_patch = ApplyPatchTool::new(root);
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

/// Every host tool `skutter` registers (the sextet, `bash`, the
/// script-only read variants and `web_fetch`), for tests that ask the
/// registered tools rather than a name list (#962).
#[cfg(test)]
pub(crate) fn every_tool(root: PathBuf) -> ToolRegistry {
    let mut reg = host_tools(root.clone());
    reg.register(super::ReadRawTool::new(root.clone()));
    reg.register(super::GlobJsonTool::new(root.clone()));
    reg.register(super::GrepJsonTool::new(root.clone()));
    reg.register(super::BashTool::new(root));
    reg.register(super::WebFetchTool::new().expect("web_fetch client builds"));
    reg
}
