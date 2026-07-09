//! `write` — whole-file create or overwrite. Unlike `edit` (surgical
//! exact-string replace), `write` replaces the file's entire content in one
//! call, creating it (and any missing parent dirs) if absent or truncating it
//! if present. Only writes under the working directory (path-escape rejected).
//! See ADR-0031 (supersedes-by-addition of ADR-0008/0009).

use super::resolve_under_root;
use anyhow::{Context, Result};
use async_trait::async_trait;
use entanglement_core::protocol::FileChangeKind;
use entanglement_core::tools::Tool;
use serde::Deserialize;

type CanWriteCallback = Box<dyn Fn(&str) -> Result<()> + Send + Sync>;
type OnWriteCallback =
    Box<dyn Fn(String, Option<Vec<u8>>, Option<Vec<u8>>, FileChangeKind) + Send + Sync>;

pub struct WriteTool {
    root: std::path::PathBuf,
    can_write: Option<CanWriteCallback>,
    on_write: Option<OnWriteCallback>,
}

impl WriteTool {
    pub fn new(root: std::path::PathBuf) -> Self {
        Self {
            root,
            can_write: None,
            on_write: None,
        }
    }

    #[allow(dead_code)]
    pub fn with_can_write<F>(mut self, f: F) -> Self
    where
        F: Fn(&str) -> Result<()> + Send + Sync + 'static,
    {
        self.can_write = Some(Box::new(f));
        self
    }

    #[allow(dead_code)]
    pub fn with_on_write<F>(mut self, f: F) -> Self
    where
        F: Fn(String, Option<Vec<u8>>, Option<Vec<u8>>, FileChangeKind) + Send + Sync + 'static,
    {
        self.on_write = Some(Box::new(f));
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
    fn name(&self) -> &'static str {
        "write"
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
        let target_abs = resolve_under_root(&self.root, &parsed.path)?;

        if let Some(ref can_write) = self.can_write {
            can_write(&parsed.path)?;
        }

        // Capture the prior content (if any) for the audit's `before` bytes and
        // the overwrite line-count report, before we truncate it.
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
        let after_bytes = parsed.content.into_bytes();
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

        if let Some(ref on_write) = self.on_write {
            on_write(parsed.path.clone(), before, Some(after_bytes), change_kind);
        }

        Ok(summary)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Arc, Mutex};

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

    /// The `FileChange` audit records `Create` with no `before` on first write,
    /// and `Edit` with the prior bytes as `before` on overwrite (#41 machinery).
    #[tokio::test]
    async fn on_write_emits_correct_kind_and_before_after() {
        let dir = TempDir::new();
        type Rec = (String, Option<Vec<u8>>, Option<Vec<u8>>, FileChangeKind);
        let seen: Arc<Mutex<Vec<Rec>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = seen.clone();
        let tool =
            WriteTool::new(dir.path.clone()).with_on_write(move |path, before, after, kind| {
                sink.lock().unwrap().push((path, before, after, kind));
            });

        tool.run(r#"{"path":"c.txt","content":"first\n"}"#)
            .await
            .unwrap();
        tool.run(r#"{"path":"c.txt","content":"second\n"}"#)
            .await
            .unwrap();

        let recs = seen.lock().unwrap();
        assert_eq!(recs.len(), 2);
        let (p0, before0, after0, kind0) = &recs[0];
        assert_eq!(p0, "c.txt");
        assert_eq!(*kind0, FileChangeKind::Create);
        assert!(before0.is_none(), "create should have no before");
        assert_eq!(after0.as_deref(), Some(b"first\n".as_ref()));
        let (_, before1, after1, kind1) = &recs[1];
        assert_eq!(*kind1, FileChangeKind::Edit);
        assert_eq!(before1.as_deref(), Some(b"first\n".as_ref()));
        assert_eq!(after1.as_deref(), Some(b"second\n".as_ref()));
    }
}
