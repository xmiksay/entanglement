//! `edit` — exact-string replace within a file. Empty `oldString` creates
//! (refused if exists); non-unique match errors unless `replaceAll` is set.
//! Only writes under the working directory (path-escape rejected).

use super::resolve_under_root_or_grant;
use crate::extra_roots::ExtraRootStore;
use crate::tools::Tool;
use anyhow::{Context, Result};
use async_trait::async_trait;
use entanglement_core::protocol::FileChangeKind;
use entanglement_core::{ContentPart, SessionId};
use serde::Deserialize;
use std::borrow::Cow;
use std::sync::Arc;

type CanEditCallback = Box<dyn Fn(&str) -> Result<()> + Send + Sync>;

pub struct EditTool {
    root: std::path::PathBuf,
    can_edit: Option<CanEditCallback>,
    /// Approval-gated out-of-root access (ADR-0109).
    extra_roots: Option<Arc<ExtraRootStore>>,
}

impl EditTool {
    pub fn new(root: std::path::PathBuf) -> Self {
        Self {
            root,
            can_edit: None,
            extra_roots: None,
        }
    }

    /// Permit approved out-of-root edits (ADR-0109) via the shared grant store.
    pub fn with_extra_roots(mut self, extra: Arc<ExtraRootStore>) -> Self {
        self.extra_roots = Some(extra);
        self
    }

    #[allow(dead_code)]
    pub fn with_can_edit<F>(mut self, f: F) -> Self
    where
        F: Fn(&str) -> Result<()> + Send + Sync + 'static,
    {
        self.can_edit = Some(Box::new(f));
        self
    }
}

#[derive(Deserialize)]
struct EditInput {
    path: String,
    #[serde(rename = "oldString")]
    old_string: String,
    #[serde(rename = "newString")]
    new_string: String,
    #[serde(rename = "replaceAll", default)]
    replace_all: bool,
}

#[async_trait]
impl Tool for EditTool {
    fn name(&self) -> Cow<'static, str> {
        Cow::Borrowed("edit")
    }
    fn description(&self) -> &str {
        "Exact-string replace within a file under the working directory. \
         Empty `oldString` creates a new file (refused if exists). \
         Non-unique match errors unless `replaceAll` is set. \
         Replacing most of a file? Use `write` instead."
    }
    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "File path relative to the working directory."
                },
                "oldString": {
                    "type": "string",
                    "description": "Exact string to replace. Empty string means \"create file\"."
                },
                "newString": {
                    "type": "string",
                    "description": "Replacement string."
                },
                "replaceAll": {
                    "type": "boolean",
                    "description": "If true, replace all occurrences. Default false (error on multiple)."
                }
            },
            "required": ["path", "oldString", "newString"]
        })
    }
    async fn run(&self, input: &str) -> Result<String> {
        self.edit("", input).await
    }

    async fn run_for_session(
        &self,
        _session: &SessionId,
        request_id: &str,
        input: &str,
    ) -> Result<Vec<ContentPart>> {
        Ok(crate::tools::text_parts(
            self.edit(request_id, input).await?,
        ))
    }
}

impl EditTool {
    /// `request_id` (#449) is forwarded to the escape-root grant check so a
    /// `Once` approval is only consumed by the call it was approved for.
    async fn edit(&self, request_id: &str, input: &str) -> Result<String> {
        let parsed: EditInput = serde_json::from_str(input)
            .context("invalid input to edit: expected {\"path\": string, \"oldString\": string, \"newString\": string, ...}")?;
        let target_abs = resolve_under_root_or_grant(
            &self.root,
            self.extra_roots.as_deref(),
            "edit",
            request_id,
            &parsed.path,
        )?;

        if let Some(ref can_edit) = self.can_edit {
            can_edit(&parsed.path)?;
        }

        if parsed.old_string.is_empty() {
            if target_abs.exists() {
                return Err(anyhow::anyhow!(
                    "create patch targets existing file: {} — use `write` to overwrite it",
                    parsed.path
                ));
            }
            if let Some(parent) = target_abs.parent() {
                tokio::fs::create_dir_all(parent)
                    .await
                    .with_context(|| "creating parent dirs".to_string())?;
            }
            tokio::fs::write(&target_abs, &parsed.new_string)
                .await
                .with_context(|| "creating file".to_string())?;

            crate::file_change::record(
                parsed.path.clone(),
                FileChangeKind::Create,
                parsed.new_string.as_bytes(),
            );

            return Ok(format!("created file: {}", parsed.path));
        }

        let content = tokio::fs::read_to_string(&target_abs)
            .await
            .with_context(|| "reading before modify".to_string())?;

        let matches: Vec<_> = content.match_indices(&parsed.old_string).collect();
        if matches.is_empty() {
            return Err(anyhow::anyhow!("oldString not found in file"));
        }
        if matches.len() > 1 && !parsed.replace_all {
            return Err(anyhow::anyhow!(
                "oldString appears {} times in file; use replaceAll to replace all",
                matches.len()
            ));
        }
        let replaced = if parsed.replace_all {
            content.replace(&parsed.old_string, &parsed.new_string)
        } else {
            let (idx, _) = matches[0];
            format!(
                "{}{}{}",
                &content[..idx],
                parsed.new_string,
                &content[idx + parsed.old_string.len()..]
            )
        };
        tokio::fs::write(&target_abs, &replaced)
            .await
            .with_context(|| "writing modified file".to_string())?;

        crate::file_change::record(
            parsed.path.clone(),
            FileChangeKind::Edit,
            replaced.as_bytes(),
        );

        Ok(format!(
            "{} matches replaced",
            if parsed.replace_all { matches.len() } else { 1 }
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
    async fn create_via_empty_old_string_writes_parents() {
        let dir = tmp();
        let tool = EditTool::new(dir.path().to_path_buf());
        let out = tool
            .run(r#"{"path":"nested/new.txt","oldString":"","newString":"body"}"#)
            .await
            .unwrap();
        assert!(out.contains("created file"), "{out}");
        let on_disk = std::fs::read_to_string(dir.path().join("nested/new.txt")).unwrap();
        assert_eq!(on_disk, "body");
    }

    #[tokio::test]
    async fn create_refuses_existing_file() {
        let dir = tmp();
        std::fs::write(dir.path().join("a.txt"), "x").unwrap();
        let tool = EditTool::new(dir.path().to_path_buf());
        let err = tool
            .run(r#"{"path":"a.txt","oldString":"","newString":"y"}"#)
            .await
            .unwrap_err();
        assert!(format!("{err}").contains("existing file"), "{err}");
    }

    #[tokio::test]
    async fn old_string_not_found_errors() {
        let dir = tmp();
        std::fs::write(dir.path().join("a.txt"), "alpha\n").unwrap();
        let tool = EditTool::new(dir.path().to_path_buf());
        let err = tool
            .run(r#"{"path":"a.txt","oldString":"zzz","newString":"y"}"#)
            .await
            .unwrap_err();
        assert!(format!("{err}").contains("oldString not found"), "{err}");
    }

    #[tokio::test]
    async fn non_unique_match_without_replace_all_errors() {
        let dir = tmp();
        std::fs::write(dir.path().join("a.txt"), "x x x\n").unwrap();
        let tool = EditTool::new(dir.path().to_path_buf());
        let err = tool
            .run(r#"{"path":"a.txt","oldString":"x","newString":"y"}"#)
            .await
            .unwrap_err();
        assert!(format!("{err}").contains("appears 3 times"), "{err}");
        // File untouched on the error path.
        assert_eq!(
            std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
            "x x x\n"
        );
    }

    #[tokio::test]
    async fn replace_all_replaces_every_occurrence() {
        let dir = tmp();
        std::fs::write(dir.path().join("a.txt"), "x x x\n").unwrap();
        let tool = EditTool::new(dir.path().to_path_buf());
        let out = tool
            .run(r#"{"path":"a.txt","oldString":"x","newString":"y","replaceAll":true}"#)
            .await
            .unwrap();
        assert!(out.contains("3 matches replaced"), "{out}");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
            "y y y\n"
        );
    }

    #[tokio::test]
    async fn can_edit_callback_can_veto() {
        let dir = tmp();
        std::fs::write(dir.path().join("a.txt"), "alpha\n").unwrap();
        let tool = EditTool::new(dir.path().to_path_buf())
            .with_can_edit(|p| Err(anyhow::anyhow!("nope for {p}")));
        let err = tool
            .run(r#"{"path":"a.txt","oldString":"alpha","newString":"beta"}"#)
            .await
            .unwrap_err();
        assert!(format!("{err}").contains("nope for a.txt"), "{err}");
        // Veto happens before the write.
        assert_eq!(
            std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
            "alpha\n"
        );
    }

    /// Under a `file_change::capture` scope a replace records an `Edit` with the
    /// after-content hash; a create records a `Create`.
    #[tokio::test]
    async fn records_file_change_under_capture() {
        use sha2::{Digest, Sha256};
        let dir = tmp();
        std::fs::write(dir.path().join("a.txt"), "alpha\n").unwrap();
        let tool = EditTool::new(dir.path().to_path_buf());

        let (res, rec) = crate::file_change::capture(
            tool.run(r#"{"path":"a.txt","oldString":"alpha","newString":"beta"}"#),
        )
        .await;
        res.unwrap();
        let rec = rec.expect("edit records a change");
        assert_eq!(rec.path, "a.txt");
        assert_eq!(rec.kind, FileChangeKind::Edit);
        assert_eq!(rec.hash, format!("{:x}", Sha256::digest(b"beta\n")));

        let (res, rec) = crate::file_change::capture(
            tool.run(r#"{"path":"new.txt","oldString":"","newString":"body\n"}"#),
        )
        .await;
        res.unwrap();
        let rec = rec.expect("create records a change");
        assert_eq!(rec.path, "new.txt");
        assert_eq!(rec.kind, FileChangeKind::Create);
        assert_eq!(rec.hash, format!("{:x}", Sha256::digest(b"body\n")));
    }

    #[tokio::test]
    async fn path_escaping_root_is_rejected() {
        let dir = tmp();
        let tool = EditTool::new(dir.path().to_path_buf());
        let err = tool
            .run(r#"{"path":"../x","oldString":"","newString":"y"}"#)
            .await
            .unwrap_err();
        assert!(
            format!("{err}").contains("escapes working directory"),
            "{err}"
        );
    }
}
