//! `apply_patch` — apply a unified diff (one or more hunks) to a file. Beside
//! `edit` (single exact-string replace) and `write` (whole-file), this is the
//! multi-hunk producer of the reserved `FileChangeKind::ApplyDiff` (#455).
//! Only writes under the working directory (path-escape rejected, same as
//! `edit`/`write`). Matching is exact and non-fuzzy — see
//! [`crate::host::unified_diff`] for why.

use super::resolve_under_root_or_grant;
use super::unified_diff::{apply_hunks, parse_hunks};
use crate::extra_roots::ExtraRootStore;
use crate::tools::Tool;
use anyhow::{Context, Result};
use async_trait::async_trait;
use entanglement_core::protocol::FileChangeKind;
use entanglement_core::{ContentPart, SessionId};
use serde::Deserialize;
use std::borrow::Cow;
use std::sync::Arc;

pub struct ApplyPatchTool {
    root: std::path::PathBuf,
    /// Approval-gated out-of-root access (ADR-0109).
    extra_roots: Option<Arc<ExtraRootStore>>,
}

impl ApplyPatchTool {
    pub fn new(root: std::path::PathBuf) -> Self {
        Self {
            root,
            extra_roots: None,
        }
    }

    /// Permit approved out-of-root patches (ADR-0109) via the shared grant store.
    pub fn with_extra_roots(mut self, extra: Arc<ExtraRootStore>) -> Self {
        self.extra_roots = Some(extra);
        self
    }
}

#[derive(Deserialize)]
struct ApplyPatchInput {
    path: String,
    patch: String,
}

#[async_trait]
impl Tool for ApplyPatchTool {
    fn name(&self) -> Cow<'static, str> {
        Cow::Borrowed("apply_patch")
    }
    fn description(&self) -> &str {
        "Apply a unified diff (one or more `@@ -l,s +l,s @@` hunks) to a file \
         under the working directory. Context/deleted lines must match the \
         file's current content exactly at the position each hunk declares — \
         a mismatch fails the whole patch and leaves the file untouched (no \
         fuzzy offset search). For one surgical replace use `edit`; for a \
         full rewrite use `write`."
    }
    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "File path relative to the working directory."
                },
                "patch": {
                    "type": "string",
                    "description": "Unified diff text (as produced by `diff -u` or `git diff`), hunks against the current file content."
                }
            },
            "required": ["path", "patch"]
        })
    }
    async fn run(&self, input: &str) -> Result<String> {
        self.apply("", input).await
    }

    async fn run_for_session(
        &self,
        _session: &SessionId,
        request_id: &str,
        input: &str,
    ) -> Result<Vec<ContentPart>> {
        Ok(crate::tools::text_parts(
            self.apply(request_id, input).await?,
        ))
    }
}

impl ApplyPatchTool {
    /// `request_id` (#449) is forwarded to the escape-root grant check so a
    /// `Once` approval is only consumed by the call it was approved for.
    async fn apply(&self, request_id: &str, input: &str) -> Result<String> {
        let parsed: ApplyPatchInput = serde_json::from_str(input).context(
            "invalid input to apply_patch: expected {\"path\": string, \"patch\": string}",
        )?;
        let target_abs = resolve_under_root_or_grant(
            &self.root,
            self.extra_roots.as_deref(),
            "apply_patch",
            request_id,
            &parsed.path,
        )?;

        let content = tokio::fs::read_to_string(&target_abs)
            .await
            .with_context(|| "reading before patch".to_string())?;

        let hunks = parse_hunks(&parsed.patch)?;
        let patched = apply_hunks(&content, &hunks)?;

        tokio::fs::write(&target_abs, &patched)
            .await
            .with_context(|| "writing patched file".to_string())?;

        crate::file_change::record(
            parsed.path.clone(),
            FileChangeKind::ApplyDiff,
            patched.as_bytes(),
        );

        Ok(format!(
            "{} hunk(s) applied to {}",
            hunks.len(),
            parsed.path
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp() -> tempfile::TempDir {
        tempfile::tempdir().expect("temp dir")
    }

    #[tokio::test]
    async fn applies_multi_hunk_patch() {
        let dir = tmp();
        std::fs::write(dir.path().join("a.txt"), "one\ntwo\nthree\nfour\n").unwrap();
        let tool = ApplyPatchTool::new(dir.path().to_path_buf());
        let patch = "@@ -1,2 +1,2 @@\n-one\n+ONE\n two\n@@ -3,2 +3,2 @@\n three\n-four\n+FOUR\n";
        let out = tool
            .run(&serde_json::json!({"path": "a.txt", "patch": patch}).to_string())
            .await
            .unwrap();
        assert!(out.contains("2 hunk(s) applied"), "{out}");
        let on_disk = std::fs::read_to_string(dir.path().join("a.txt")).unwrap();
        assert_eq!(on_disk, "ONE\ntwo\nthree\nFOUR\n");
    }

    #[tokio::test]
    async fn context_mismatch_errors_and_leaves_file_untouched() {
        let dir = tmp();
        std::fs::write(dir.path().join("a.txt"), "alpha\nbeta\n").unwrap();
        let tool = ApplyPatchTool::new(dir.path().to_path_buf());
        let patch = "@@ -1,2 +1,2 @@\n alpha\n-WRONG\n+BETA\n";
        let err = tool
            .run(&serde_json::json!({"path": "a.txt", "patch": patch}).to_string())
            .await
            .unwrap_err();
        assert!(format!("{err}").contains("context does not match"), "{err}");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
            "alpha\nbeta\n"
        );
    }

    #[tokio::test]
    async fn malformed_patch_errors_and_leaves_file_untouched() {
        let dir = tmp();
        std::fs::write(dir.path().join("a.txt"), "alpha\n").unwrap();
        let tool = ApplyPatchTool::new(dir.path().to_path_buf());
        let err = tool
            .run(&serde_json::json!({"path": "a.txt", "patch": "not a diff"}).to_string())
            .await
            .unwrap_err();
        assert!(format!("{err}").contains("malformed patch"), "{err}");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
            "alpha\n"
        );
    }

    #[tokio::test]
    async fn missing_file_errors() {
        let dir = tmp();
        let tool = ApplyPatchTool::new(dir.path().to_path_buf());
        let err = tool
            .run(
                &serde_json::json!({"path": "missing.txt", "patch": "@@ -1,1 +1,1 @@\n-x\n+y\n"})
                    .to_string(),
            )
            .await
            .unwrap_err();
        assert!(format!("{err}").contains("reading before patch"), "{err}");
    }

    #[tokio::test]
    async fn path_escaping_root_is_rejected() {
        let dir = tmp();
        let tool = ApplyPatchTool::new(dir.path().to_path_buf());
        let err = tool
            .run(
                &serde_json::json!({"path": "../x", "patch": "@@ -1,1 +1,1 @@\n-x\n+y\n"})
                    .to_string(),
            )
            .await
            .unwrap_err();
        assert!(
            format!("{err}").contains("escapes working directory"),
            "{err}"
        );
    }

    /// Under a `file_change::capture` scope, a successful apply records an
    /// `ApplyDiff` change with the after-content hash (#455 — the first
    /// producer of the previously-reserved `FileChangeKind::ApplyDiff`).
    #[tokio::test]
    async fn records_file_change_under_capture() {
        use sha2::{Digest, Sha256};
        let dir = tmp();
        std::fs::write(dir.path().join("a.txt"), "alpha\n").unwrap();
        let tool = ApplyPatchTool::new(dir.path().to_path_buf());

        let patch = "@@ -1,1 +1,1 @@\n-alpha\n+beta\n";
        let (res, rec) = crate::file_change::capture(
            tool.run(&serde_json::json!({"path": "a.txt", "patch": patch}).to_string()),
        )
        .await;
        res.unwrap();
        let rec = rec.expect("apply_patch records a change");
        assert_eq!(rec.path, "a.txt");
        assert_eq!(rec.kind, FileChangeKind::ApplyDiff);
        assert_eq!(rec.hash, format!("{:x}", Sha256::digest(b"beta\n")));
    }
}
