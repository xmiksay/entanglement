//! `edit` — exact-string replace within a file. Empty `oldString` creates
//! (refused if exists); non-unique match errors unless `replaceAll` is set.
//! Only writes under the working directory (path-escape rejected).

use super::resolve_under_root;
use anyhow::{Context, Result};
use async_trait::async_trait;
use entanglement_core::protocol::FileChangeKind;
use entanglement_core::tools::Tool;
use serde::Deserialize;

type CanEditCallback = Box<dyn Fn(&str) -> Result<()> + Send + Sync>;
type OnEditCallback =
    Box<dyn Fn(String, Option<Vec<u8>>, Option<Vec<u8>>, FileChangeKind) + Send + Sync>;

pub struct EditTool {
    root: std::path::PathBuf,
    can_edit: Option<CanEditCallback>,
    on_edit: Option<OnEditCallback>,
}

impl EditTool {
    pub fn new(root: std::path::PathBuf) -> Self {
        Self {
            root,
            can_edit: None,
            on_edit: None,
        }
    }

    #[allow(dead_code)]
    pub fn with_can_edit<F>(mut self, f: F) -> Self
    where
        F: Fn(&str) -> Result<()> + Send + Sync + 'static,
    {
        self.can_edit = Some(Box::new(f));
        self
    }

    #[allow(dead_code)]
    pub fn with_on_edit<F>(mut self, f: F) -> Self
    where
        F: Fn(String, Option<Vec<u8>>, Option<Vec<u8>>, FileChangeKind) + Send + Sync + 'static,
    {
        self.on_edit = Some(Box::new(f));
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
    fn name(&self) -> &'static str {
        "edit"
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
        let parsed: EditInput = serde_json::from_str(input)
            .context("invalid input to edit: expected {\"path\": string, \"oldString\": string, \"newString\": string, ...}")?;
        let target_abs = resolve_under_root(&self.root, &parsed.path)?;

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

            if let Some(ref on_edit) = self.on_edit {
                on_edit(
                    parsed.path.clone(),
                    None,
                    Some(parsed.new_string.into_bytes()),
                    FileChangeKind::Create,
                );
            }

            return Ok(format!("created file: {}", parsed.path));
        }

        let content = tokio::fs::read_to_string(&target_abs)
            .await
            .with_context(|| "reading before modify".to_string())?;
        let before_bytes = content.clone().into_bytes();

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
                &parsed.new_string,
                &content[idx + parsed.old_string.len()..]
            )
        };
        let after_bytes = replaced.clone().into_bytes();

        tokio::fs::write(&target_abs, &replaced)
            .await
            .with_context(|| "writing modified file".to_string())?;

        if let Some(ref on_edit) = self.on_edit {
            on_edit(
                parsed.path.clone(),
                Some(before_bytes),
                Some(after_bytes),
                FileChangeKind::Edit,
            );
        }

        Ok(format!(
            "{} matches replaced",
            if parsed.replace_all { matches.len() } else { 1 }
        ))
    }
}
