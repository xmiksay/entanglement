//! `write` — whole-file create or overwrite. Unlike `edit` (surgical
//! exact-string replace), `write` replaces the file's entire content in one
//! call, creating it (and any missing parent dirs) if absent or truncating it
//! if present. Only writes under the working directory (path-escape rejected).
//! See ADR-0031 (supersedes-by-addition of ADR-0008/0009).

use super::resolve_under_root_or_grant;
use crate::extra_roots::ExtraRootStore;
use crate::tools::Tool;
use anyhow::{Context, Result};
use async_trait::async_trait;
use entanglement_core::protocol::FileChangeKind;
use serde::Deserialize;
use std::borrow::Cow;
use std::sync::Arc;

type CanWriteCallback = Box<dyn Fn(&str) -> Result<()> + Send + Sync>;

pub struct WriteTool {
    root: std::path::PathBuf,
    can_write: Option<CanWriteCallback>,
    /// Approval-gated out-of-root access (ADR-0109).
    extra_roots: Option<Arc<ExtraRootStore>>,
}

impl WriteTool {
    pub fn new(root: std::path::PathBuf) -> Self {
        Self {
            root,
            can_write: None,
            extra_roots: None,
        }
    }

    /// Permit approved out-of-root writes (ADR-0109) via the shared grant store.
    pub fn with_extra_roots(mut self, extra: Arc<ExtraRootStore>) -> Self {
        self.extra_roots = Some(extra);
        self
    }

    #[allow(dead_code)]
    pub fn with_can_write<F>(mut self, f: F) -> Self
    where
        F: Fn(&str) -> Result<()> + Send + Sync + 'static,
    {
        self.can_write = Some(Box::new(f));
        self
    }
}

#[derive(Deserialize)]
struct WriteInput {
    path: String,
    content: String,
}

/// Line count reported in the confirmation. `lines()` treats a trailing newline
/// as a terminator (not a new empty line), so `"a\nb\n"` and `"a\nb"` both count
/// 2; empty content counts 0.
fn count_lines(s: &str) -> usize {
    s.lines().count()
}

#[async_trait]
impl Tool for WriteTool {
    fn name(&self) -> Cow<'static, str> {
        Cow::Borrowed("write")
    }
    fn description(&self) -> &str {
        "Create or fully overwrite a file under the working directory with the \
         given content; missing parent directories are created. Use this to \
         generate a new file or regenerate most of an existing one — for a \
         surgical change to part of a file, use `edit` instead."
    }
    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "File path relative to the working directory. Created (with parent dirs) if absent, overwritten if present."
                },
                "content": {
                    "type": "string",
                    "description": "Full UTF-8 text content to write. Replaces the entire file."
                }
            },
            "required": ["path", "content"]
        })
    }
    async fn run(&self, input: &str) -> Result<String> {
        let parsed: WriteInput = serde_json::from_str(input)
            .context("invalid input to write: expected {\"path\": string, \"content\": string}")?;
        let target_abs = resolve_under_root_or_grant(
            &self.root,
            self.extra_roots.as_deref(),
            "write",
            &parsed.path,
        )?;

        if let Some(ref can_write) = self.can_write {
            can_write(&parsed.path)?;
        }

        // Read the prior line count (if any) for the overwrite report — and, via
        // its presence, whether this write creates or overwrites — before we
        // truncate the file.
        let before = tokio::fs::read(&target_abs).await.ok();

        if let Some(parent) = target_abs.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .with_context(|| "creating parent dirs".to_string())?;
        }
        tokio::fs::write(&target_abs, &parsed.content)
            .await
            .with_context(|| "writing file".to_string())?;

        let new_lines = count_lines(&parsed.content);
        let (change_kind, summary) = match &before {
            None => (
                FileChangeKind::Create,
                format!("created {} ({} lines)", parsed.path, new_lines),
            ),
            Some(prev) => {
                let old_lines = count_lines(&String::from_utf8_lossy(prev));
                (
                    FileChangeKind::Edit,
                    format!(
                        "overwrote {} ({} lines, was {})",
                        parsed.path, new_lines, old_lines
                    ),
                )
            }
        };

        crate::file_change::record(parsed.path.clone(), change_kind, parsed.content.as_bytes());

        Ok(summary)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
            let path = std::env::temp_dir()
                .join(format!("entanglement-write-{}-{id}", std::process::id()));
            fs::create_dir_all(&path).unwrap();
            TempDir { path }
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    #[test]
    fn count_lines_treats_trailing_newline_as_terminator() {
        assert_eq!(count_lines(""), 0);
        assert_eq!(count_lines("a\n"), 1);
        assert_eq!(count_lines("a\nb\n"), 2);
        assert_eq!(count_lines("a\nb"), 2);
    }

    #[tokio::test]
    async fn create_makes_missing_parents_and_reports_line_count() {
        let dir = TempDir::new();
        let tool = WriteTool::new(dir.path.clone());
        let out = tool
            .run(r#"{"path":"nested/deep/new.txt","content":"one\ntwo\n"}"#)
            .await
            .unwrap();
        assert!(out.contains("created"), "got: {out}");
        assert!(out.contains("2 lines"), "got: {out}");
        let on_disk = fs::read_to_string(dir.path.join("nested/deep/new.txt")).unwrap();
        assert_eq!(on_disk, "one\ntwo\n");
    }

    #[tokio::test]
    async fn overwrite_reports_old_and_new_line_counts() {
        let dir = TempDir::new();
        fs::write(dir.path.join("a.txt"), "x\ny\nz\n").unwrap();
        let tool = WriteTool::new(dir.path.clone());
        let out = tool
            .run(r#"{"path":"a.txt","content":"only\n"}"#)
            .await
            .unwrap();
        assert!(out.contains("overwrote"), "got: {out}");
        assert!(out.contains("1 lines, was 3"), "got: {out}");
        let on_disk = fs::read_to_string(dir.path.join("a.txt")).unwrap();
        assert_eq!(on_disk, "only\n");
    }

    #[tokio::test]
    async fn path_escape_is_refused() {
        let dir = TempDir::new();
        let tool = WriteTool::new(dir.path.clone());
        let err = tool
            .run(r#"{"path":"../escape.txt","content":"nope\n"}"#)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("escapes"), "got: {err}");
        assert!(!dir.path.join("../escape.txt").exists());
    }

    #[tokio::test]
    async fn empty_content_truncates_to_empty_file() {
        let dir = TempDir::new();
        fs::write(dir.path.join("f.txt"), "some\nlines\n").unwrap();
        let tool = WriteTool::new(dir.path.clone());
        let out = tool.run(r#"{"path":"f.txt","content":""}"#).await.unwrap();
        assert!(out.contains("overwrote"), "got: {out}");
        assert!(out.contains("0 lines, was 2"), "got: {out}");
        let on_disk = fs::read_to_string(dir.path.join("f.txt")).unwrap();
        assert_eq!(on_disk, "");
    }

    #[tokio::test]
    async fn result_never_echoes_content() {
        let dir = TempDir::new();
        let tool = WriteTool::new(dir.path.clone());
        let secret = "SECRET-PAYLOAD-DO-NOT-ECHO";
        let out = tool
            .run(&format!(r#"{{"path":"s.txt","content":"{secret}\n"}}"#))
            .await
            .unwrap();
        assert!(!out.contains(secret), "confirmation leaked content: {out}");
    }

    /// The `FileChange` audit records `Create` on the first write and `Edit` on
    /// a subsequent overwrite, each carrying the after-content hash (#202).
    #[tokio::test]
    async fn records_create_then_edit_under_capture() {
        use sha2::{Digest, Sha256};
        let dir = TempDir::new();
        let tool = WriteTool::new(dir.path.clone());

        let (res, rec) =
            crate::file_change::capture(tool.run(r#"{"path":"c.txt","content":"first\n"}"#)).await;
        res.unwrap();
        let rec = rec.expect("create records a change");
        assert_eq!(rec.path, "c.txt");
        assert_eq!(rec.kind, FileChangeKind::Create);
        assert_eq!(rec.hash, format!("{:x}", Sha256::digest(b"first\n")));

        let (res, rec) =
            crate::file_change::capture(tool.run(r#"{"path":"c.txt","content":"second\n"}"#)).await;
        res.unwrap();
        let rec = rec.expect("overwrite records a change");
        assert_eq!(rec.kind, FileChangeKind::Edit);
        assert_eq!(rec.hash, format!("{:x}", Sha256::digest(b"second\n")));
    }
}
