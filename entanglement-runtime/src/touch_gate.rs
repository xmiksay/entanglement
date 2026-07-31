//! File-touch gate (ADR-0142): ensures agents read files before modifying them.
//!
//! The gate prevents blind writes to files that have never been read or have
//! been externally modified. It checks write-eligible tools (`edit`/`write`/
//! `apply_patch`) before they execute.
//!
//! ## Where the state lives
//!
//! Core's `Session` struct owns the `TouchedFiles` map (it serializes with the
//! session), but core's `session_loop` task is the sole holder of that `Session`
//! — the runtime's tool executor only ever sees a [`SessionId`]. So the
//! *runtime* keeps its own per-session `HashMap<SessionId, TouchedFiles>` in
//! [`crate::tool_runner`], exactly mirroring how it already tracks `active`,
//! `in_flight`, `active_skill`, etc. This module holds the pure gate logic that
//! operates on a borrowed `TouchedFiles`, decoupled from any particular owner.

use crate::host::timestamp::get_file_mtime;
use entanglement_core::session::TouchedFiles;
use entanglement_core::{protocol::SessionId, ToolCall};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// Error type for file-touch gate rejections.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TouchGateError {
    /// File was not read in this session.
    NotRead(String),
    /// File has changed since it was last read.
    ExternallyModified(String),
}

impl std::fmt::Display for TouchGateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TouchGateError::NotRead(path) => write!(
                f,
                "File `{path}` was not read in this session. Read the file first to understand \
                 its current state before modifying it."
            ),
            TouchGateError::ExternallyModified(path) => write!(
                f,
                "File `{path}` has changed since it was last read in this session (by user or \
                 another agent). Re-read the file to see its current state before modifying it."
            ),
        }
    }
}

impl std::error::Error for TouchGateError {}

/// Whether a tool name is one the gate must check before it runs.
pub fn is_gated_write(tool: &str) -> bool {
    matches!(tool, "edit" | "write" | "apply_patch")
}

/// Whether a tool name should record a touch after it runs (`read` + the write
/// trio above — a read establishes context, a write re-establishes it).
pub fn is_touch_recording(tool: &str) -> bool {
    matches!(tool, "read" | "edit" | "write" | "apply_patch")
}

/// Runtime-owned, per-session file-touch state (ADR-0142).
///
/// Core's `Session` struct carries its own `TouchedFiles` (it serializes with
/// the session), but the runtime's tool executor never holds a `Session` — only
/// a [`SessionId`]. So the executor keeps this parallel map, exactly mirroring
/// its other per-session bookkeeping (`active`, `active_skill`, …).
///
/// `root` is the canonical project root the gate resolves paths against. When
/// `None` (no escape-root policy wired — every test/default wrapper) the gate is
/// inert: paths can't be canonicalized, so [`Self::check`] and [`Self::mark`]
/// are no-ops. In production (`main.rs`) the root is always present.
#[derive(Clone, Default)]
pub struct TouchState {
    root: Option<PathBuf>,
    files: Arc<Mutex<HashMap<SessionId, TouchedFiles>>>,
}

impl TouchState {
    /// Build a fresh gate rooted at `root` (or inert when `root` is `None`).
    pub fn new(root: Option<&Path>) -> Self {
        Self {
            root: root.map(|p| p.to_path_buf()),
            files: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// The root paths are resolved against, for tests that need it.
    pub fn root(&self) -> Option<&Path> {
        self.root.as_deref()
    }

    /// Check the file-touch gate for `session` + `call` before a write runs.
    /// `Ok(())` allows it; an `Err` carries the actionable rejection message.
    /// A no-op when no root is wired (the gate can't canonicalize without one).
    pub fn check(&self, session: &SessionId, call: &ToolCall) -> Result<(), TouchGateError> {
        let Some(root) = self.root.as_deref() else {
            return Ok(());
        };
        let files = self.files.lock().expect("touch-state mutex poisoned");
        let touched = files.get(session);
        let empty = TouchedFiles::default();
        check_touch_gate(call, touched.unwrap_or(&empty), root)
    }

    /// Record a touch for `session` + `call` after a successful read/write.
    /// A no-op for tools that don't touch files, and when no root is wired.
    pub fn mark(&self, session: &SessionId, call: &ToolCall) {
        let Some(root) = self.root.as_deref() else {
            return;
        };
        let mut files = self.files.lock().expect("touch-state mutex poisoned");
        let touched = files.entry(session.clone()).or_default();
        mark_touched(call, touched, root);
    }

    /// Drop a session's accumulated touches (on session end/hibernate).
    pub fn forget(&self, session: &SessionId) {
        self.files
            .lock()
            .expect("touch-state mutex poisoned")
            .remove(session);
    }
}

/// Extract the canonical path from a tool call argument.
///
/// Returns `None` when the tool has no `path` field, the JSON is malformed, or
/// the path cannot be canonicalized (e.g. it doesn't exist yet — creation is a
/// separate code path). The path is resolved against `root` before
/// canonicalization so a relative spelling and its in-root absolute form key the
/// same entry.
fn extract_path_from_call(call: &ToolCall, root: &Path) -> Option<String> {
    let input: serde_json::Value = serde_json::from_str(&call.input).ok()?;
    let path_field = match call.name.as_str() {
        "edit" | "write" | "read" | "apply_patch" => input.get("path")?,
        // `apply_patch` historically carried the target in `path`; keep parity.
        _ => return None,
    };
    let path_str = path_field.as_str()?;
    let resolved = root.join(path_str);
    let canonical = resolved.canonicalize().ok()?;
    Some(canonical.to_string_lossy().into_owned())
}

/// Check the file-touch gate before allowing a write operation.
///
/// Returns `Ok(())` if the tool should be allowed, `Err(TouchGateError)` if
/// rejected. Non-write tools always pass. Rules:
///
/// 1. File doesn't exist → allowed (creation).
/// 2. File exists but was never touched → reject with [`NotRead`].
/// 3. File exists, was touched, mtime matches → allowed.
/// 4. File exists, was touched, mtime differs → reject with
///    [`ExternallyModified`](TouchGateError::ExternallyModified).
///
/// Failures to extract a path or read metadata degrade gracefully to `Ok(())`:
/// the host tool itself will surface a more specific error, and the gate must
/// never be the thing that obscures it.
pub fn check_touch_gate(
    call: &ToolCall,
    touched: &TouchedFiles,
    root: &Path,
) -> Result<(), TouchGateError> {
    if !is_gated_write(&call.name) {
        return Ok(());
    }

    let Some(path) = extract_path_from_call(call, root) else {
        return Ok(());
    };

    let current_mtime = match get_file_mtime(Path::new(&path)) {
        Ok(mtime) => mtime,
        Err(_) => return Ok(()),
    };

    // File doesn't exist → creation allowed.
    if current_mtime.is_none() {
        return Ok(());
    }

    if !touched.is_touched(&path) {
        return Err(TouchGateError::NotRead(path));
    }

    if !touched.matches_current(&path, current_mtime) {
        return Err(TouchGateError::ExternallyModified(path));
    }

    Ok(())
}

/// Record a touch after a successful `read` or write operation, capturing the
/// file's *current* mtime so a later write can detect an intervening external
/// change. A no-op for tools that don't touch files, and when the path can't be
/// resolved (creation paths canonicalize only once the file exists).
pub fn mark_touched(call: &ToolCall, touched: &mut TouchedFiles, root: &Path) {
    if !is_touch_recording(&call.name) {
        return;
    }

    let Some(path) = extract_path_from_call(call, root) else {
        return;
    };

    let mtime = get_file_mtime(Path::new(&path)).unwrap_or_default();

    touched.mark_touched(path, mtime);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn call(id: &str, tool: &str, input: &str) -> ToolCall {
        ToolCall {
            id: id.to_string(),
            name: tool.to_string(),
            input: input.to_string(),
            provider_meta: None,
        }
    }

    #[test]
    fn gate_allows_creation_of_new_file() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        let c = call("w", "write", r#"{"path":"new.txt","content":"hi"}"#);
        let touched = TouchedFiles::default();
        assert!(check_touch_gate(&c, &touched, root).is_ok());
    }

    #[test]
    fn gate_rejects_write_to_unread_existing_file() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        fs::write(root.join("existing.txt"), "content").unwrap();
        let c = call(
            "e",
            "edit",
            r#"{"path":"existing.txt","oldString":"content","newString":"new"}"#,
        );
        let touched = TouchedFiles::default();
        match check_touch_gate(&c, &touched, root) {
            Err(TouchGateError::NotRead(_)) => {}
            other => panic!("expected NotRead, got {other:?}"),
        }
    }

    #[test]
    fn gate_allows_write_after_read() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        fs::write(root.join("existing.txt"), "content").unwrap();

        let mut touched = TouchedFiles::default();
        let read_call = call("r", "read", r#"{"path":"existing.txt"}"#);
        mark_touched(&read_call, &mut touched, root);

        let edit_call = call(
            "e",
            "edit",
            r#"{"path":"existing.txt","oldString":"content","newString":"new"}"#,
        );
        assert!(check_touch_gate(&edit_call, &touched, root).is_ok());
    }

    #[test]
    fn gate_rejects_after_external_modification() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        let file = root.join("existing.txt");
        fs::write(&file, "content").unwrap();

        let mut touched = TouchedFiles::default();
        let read_call = call("r", "read", r#"{"path":"existing.txt"}"#);
        mark_touched(&read_call, &mut touched, root);

        // Simulate an external change with a measurable mtime bump.
        std::thread::sleep(std::time::Duration::from_millis(20));
        fs::write(&file, "modified externally").unwrap();

        let edit_call = call(
            "e",
            "edit",
            r#"{"path":"existing.txt","oldString":"content","newString":"new"}"#,
        );
        match check_touch_gate(&edit_call, &touched, root) {
            Err(TouchGateError::ExternallyModified(_)) => {}
            other => panic!("expected ExternallyModified, got {other:?}"),
        }
    }

    #[test]
    fn gate_allows_own_prior_edit() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        fs::write(root.join("existing.txt"), "content").unwrap();

        let mut touched = TouchedFiles::default();
        // Read first to establish context.
        let read_call = call("r", "read", r#"{"path":"existing.txt"}"#);
        mark_touched(&read_call, &mut touched, root);
        // An edit re-marks with the post-edit mtime.
        let edit_call = call(
            "e1",
            "edit",
            r#"{"path":"existing.txt","oldString":"content","newString":"new"}"#,
        );
        mark_touched(&edit_call, &mut touched, root);

        // A second edit on the same file should be allowed (the recorded mtime
        // matches the file's current state).
        let edit_call2 = call(
            "e2",
            "edit",
            r#"{"path":"existing.txt","oldString":"new","newString":"newer"}"#,
        );
        assert!(check_touch_gate(&edit_call2, &touched, root).is_ok());
    }

    #[test]
    fn gate_ignores_non_write_tools() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        fs::write(root.join("existing.txt"), "content").unwrap();
        // A `grep`/`bash` call is never gated, even on an unread file.
        let c = call("g", "bash", r#"{"command":"cat existing.txt"}"#);
        let touched = TouchedFiles::default();
        assert!(check_touch_gate(&c, &touched, root).is_ok());
    }
}
