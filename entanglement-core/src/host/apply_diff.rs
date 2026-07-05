//! `apply_diff` — apply a unified diff patch to files under the working
//! directory. Supports modify, create, and delete operations. Rejects patches
//! that escape the working directory or use binary format. Atomic across all
//! hunks — any conflict aborts the entire operation without writing any files.
//!
//! This is the implementation of ADR-0012.

use super::{count_patch_changes, resolve_under_root, truncate_output};
use crate::tools::Tool;
use anyhow::{Context, Result};
use async_trait::async_trait;
use diffy::{patch_set, patch_set::ParseOptions};
use serde::Deserialize;

const MAX_DIFF_INPUT_BYTES: usize = 256 * 1024; // 256 KiB

pub struct ApplyDiffTool {
    root: std::path::PathBuf,
}

impl ApplyDiffTool {
    pub fn new(root: std::path::PathBuf) -> Self {
        Self { root }
    }
}

#[derive(Deserialize)]
struct ApplyDiffInput {
    diff: String,
}

#[async_trait]
impl Tool for ApplyDiffTool {
    fn name(&self) -> &'static str {
        "apply_diff"
    }
    fn description(&self) -> &str {
        "Apply a unified diff patch to files under the working directory. \
         Supports modify, create, and delete operations. \
         Rejects patches that escape the working directory or use binary format. \
         Atomic across all hunks — any conflict aborts the entire operation."
    }
    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "diff": {
                    "type": "string",
                    "description": "Unified diff patch to apply (standard git diff format)."
                }
            },
            "required": ["diff"]
        })
    }
    async fn run(&self, input: &str) -> Result<String> {
        let parsed: ApplyDiffInput = serde_json::from_str(input)
            .context("invalid input to apply_diff: expected {\"diff\": string}")?;

        if parsed.diff.len() > MAX_DIFF_INPUT_BYTES {
            return Err(anyhow::anyhow!(
                "diff too large ({} bytes, max {})",
                parsed.diff.len(),
                MAX_DIFF_INPUT_BYTES
            ));
        }

        if parsed.diff.trim().is_empty() {
            return Err(anyhow::anyhow!("diff is empty"));
        }

        let patch_set = patch_set::PatchSet::parse(&parsed.diff, ParseOptions::gitdiff());

        let mut file_ops: Vec<(std::path::PathBuf, String, String)> = Vec::new();
        let mut total_insertions = 0;
        let mut total_deletions = 0;

        for file_patch in patch_set {
            let file_patch: Result<patch_set::FilePatch<str>, _> = file_patch;
            let file_patch = file_patch.map_err(|e| anyhow::anyhow!("parse error: {e}"))?;
            let op = file_patch.operation();
            let target_rel = match op {
                patch_set::FileOperation::Modify { original, modified } => {
                    if original != modified {
                        return Err(anyhow::anyhow!(
                            "unsupported: rename patches are not allowed",
                        ));
                    }
                    original.to_string()
                }
                patch_set::FileOperation::Create(path) => path.to_string(),
                patch_set::FileOperation::Delete(path) => path.to_string(),
                patch_set::FileOperation::Rename { from, to } => {
                    return Err(anyhow::anyhow!(
                        "unsupported: rename patches are not allowed ({from} -> {to})",
                    ));
                }
                _ => {
                    return Err(anyhow::anyhow!("unrecognized file operation in patch"));
                }
            };
            let kind = match op {
                patch_set::FileOperation::Modify { .. } => String::from("modify"),
                patch_set::FileOperation::Create(_) => String::from("create"),
                patch_set::FileOperation::Delete(_) => String::from("delete"),
                _ => unreachable!(),
            };

            let target_abs = resolve_under_root(&self.root, &target_rel)
                .with_context(|| format!("invalid path in patch: {}", target_rel))?;

            let text_patch = file_patch.patch().as_text().ok_or_else(|| {
                anyhow::anyhow!("binary patches are not supported: {}", target_rel)
            })?;

            let patch_text = text_patch.to_string();

            match &kind[..] {
                "modify" => {
                    if !target_abs.exists() {
                        return Err(anyhow::anyhow!(
                            "modify patch targets missing file: {}",
                            target_rel,
                        ));
                    }
                    let content = tokio::fs::read_to_string(&target_abs)
                        .await
                        .with_context(|| format!("reading {}", target_rel))?;
                    diffy::apply(&content, text_patch)
                        .map_err(|e| anyhow::anyhow!("patch conflict or io error: {e}"))?;
                    let (ins, del) = count_patch_changes(&patch_text);
                    total_insertions += ins;
                    total_deletions += del;
                }
                "create" => {
                    if target_abs.exists() {
                        return Err(anyhow::anyhow!(
                            "create patch targets existing file: {}",
                            target_rel,
                        ));
                    }
                    let (ins, del) = count_patch_changes(&patch_text);
                    total_insertions += ins;
                    total_deletions += del;
                }
                "delete" => {
                    if !target_abs.exists() {
                        return Err(anyhow::anyhow!(
                            "delete patch targets missing file: {}",
                            target_rel,
                        ));
                    }
                    let (ins, del) = count_patch_changes(&patch_text);
                    total_insertions += ins;
                    total_deletions += del;
                }
                _ => unreachable!(),
            }
            file_ops.push((target_abs, kind.clone(), patch_text));
        }

        for (target_abs, kind, patch_text) in file_ops.iter() {
            let text_patch = diffy::Patch::from_str(patch_text).context("invalid patch text")?;
            match kind.as_str() {
                "modify" => {
                    let content = tokio::fs::read_to_string(&target_abs)
                        .await
                        .with_context(|| "reading before modify")?;
                    let applied = diffy::apply(&content, &text_patch)
                        .context("unexpected apply failure after validation")?;
                    tokio::fs::write(&target_abs, &applied)
                        .await
                        .with_context(|| "writing modified file")?;
                }
                "create" => {
                    let new_content = patch_text
                        .lines()
                        .filter(|l| {
                            let trimmed = l.trim_start_matches(' ');
                            !trimmed.starts_with("---")
                                && !trimmed.starts_with("+++")
                                && !trimmed.starts_with("@@")
                                && trimmed.starts_with('+')
                        })
                        .map(|l| l.trim_start_matches('+'))
                        .collect::<Vec<_>>()
                        .join("\n");
                    if let Some(parent) = target_abs.parent() {
                        tokio::fs::create_dir_all(parent)
                            .await
                            .with_context(|| "creating parent dirs")?;
                    }
                    tokio::fs::write(&target_abs, new_content)
                        .await
                        .with_context(|| "writing new file")?;
                }
                "delete" => {
                    tokio::fs::remove_file(&target_abs)
                        .await
                        .with_context(|| "deleting file")?;
                }
                _ => unreachable!(),
            }
        }

        let summary = format!(
            "applied diff: {} files ({} modify, {} create, {} delete)",
            file_ops.len(),
            file_ops.iter().filter(|(_, k, _)| k == "modify").count(),
            file_ops.iter().filter(|(_, k, _)| k == "create").count(),
            file_ops.iter().filter(|(_, k, _)| k == "delete").count(),
        );

        let changes = format!(
            ", {} insertions, {} deletions",
            total_insertions, total_deletions
        );

        let paths = format!(
            ", paths: {}",
            file_ops
                .iter()
                .map(|(_, _, p)| {
                    p.lines()
                        .find(|l| l.starts_with("+++"))
                        .and_then(|l| l.strip_prefix("+++ "))
                        .and_then(|l| l.strip_prefix("b/"))
                        .unwrap_or("unknown")
                })
                .collect::<Vec<_>>()
                .join(", ")
        );

        Ok(truncate_output(format!("{}{}{}", summary, changes, paths)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn apply_diff_simple_modify() {
        let dir = tempfile::TempDir::new().unwrap();
        let root = dir.path().to_path_buf();
        tokio::fs::write(root.join("a.txt"), "alpha\nbeta\n")
            .await
            .unwrap();
        let tool = ApplyDiffTool::new(root.clone());
        let diff = "diff --git a/a.txt b/a.txt\nindex fbbee86..37b33db 100644\n--- a/a.txt\n+++ b/a.txt\n@@ -1,2 +1,2 @@\n alpha\n-beta\n+BETA\n";
        let out = tool
            .run(&serde_json::json!({ "diff": diff }).to_string())
            .await
            .unwrap();
        assert!(out.contains("1 modify, 0 create, 0 delete"), "got: {out}");
        assert!(out.contains("1 insertion, 1 deletion"), "got: {out}");
        assert!(out.contains("paths: a.txt"), "got: {out}");
        let on_disk = tokio::fs::read_to_string(root.join("a.txt")).await.unwrap();
        assert_eq!(on_disk, "alpha\nBETA\n");
    }

    #[tokio::test]
    async fn apply_diff_creates_file() {
        let dir = tempfile::TempDir::new().unwrap();
        let root = dir.path().to_path_buf();
        let tool = ApplyDiffTool::new(root.clone());
        let diff = "diff --git a/new.txt b/new.txt\nnew file mode 100644\nindex 0000000..94954ab\n--- /dev/null\n+++ b/new.txt\n@@ -0,0 +1,2 @@\n+hello\n+world\n";
        let out = tool
            .run(&serde_json::json!({ "diff": diff }).to_string())
            .await
            .unwrap();
        assert!(out.contains("0 modify, 1 create, 0 delete"), "got: {out}");
        assert!(out.contains("2 insertions"), "got: {out}");
        assert!(out.contains("paths: new.txt"), "got: {out}");
        let on_disk = tokio::fs::read_to_string(root.join("new.txt"))
            .await
            .unwrap();
        assert_eq!(on_disk, "hello\nworld\n");
    }

    #[tokio::test]
    async fn apply_diff_deletes_file() {
        let dir = tempfile::TempDir::new().unwrap();
        let root = dir.path().to_path_buf();
        tokio::fs::write(root.join("gone.txt"), "bye\n")
            .await
            .unwrap();
        let tool = ApplyDiffTool::new(root.clone());
        let diff = "diff --git a/gone.txt b/gone.txt\ndeleted file mode 100644\nindex 980a0d..0000000\n--- a/gone.txt\n+++ /dev/null\n@@ -1 +0,0 @@\n-bye\n";
        let out = tool
            .run(&serde_json::json!({ "diff": diff }).to_string())
            .await
            .unwrap();
        assert!(out.contains("0 modify, 0 create, 1 delete"), "got: {out}");
        assert!(out.contains("1 deletion"), "got: {out}");
        assert!(out.contains("paths: gone.txt"), "got: {out}");
        assert!(!root.join("gone.txt").exists());
    }

    #[tokio::test]
    async fn apply_diff_multi_file() {
        let dir = tempfile::TempDir::new().unwrap();
        let root = dir.path().to_path_buf();
        tokio::fs::write(root.join("a.txt"), "alpha\n")
            .await
            .unwrap();
        tokio::fs::write(root.join("b.txt"), "beta\n")
            .await
            .unwrap();
        let tool = ApplyDiffTool::new(root.clone());
        let diff = "diff --git a/a.txt b/a.txt\nindex 7898192..85c050f 100644\n--- a/a.txt\n+++ b/a.txt\n@@ -1,1 +1,1 @@\n-alpha\n+ALPHA\n\ndiff --git a/b.txt b/b.txt\nindex 980a0d..85c050f 100644\n--- a/b.txt\n+++ b/b.txt\n@@ -1,1 +1,1 @@\n-beta\n+BETA\n";
        let out = tool
            .run(&serde_json::json!({ "diff": diff }).to_string())
            .await
            .unwrap();
        assert!(out.contains("2 files"), "got: {out}");
        assert!(out.contains("2 insertions, 2 deletions"), "got: {out}");
        assert!(out.contains("paths: a.txt, b.txt"), "got: {out}");
        let a = tokio::fs::read_to_string(root.join("a.txt")).await.unwrap();
        let b = tokio::fs::read_to_string(root.join("b.txt")).await.unwrap();
        assert_eq!(a, "ALPHA\n");
        assert_eq!(b, "BETA\n");
    }

    #[tokio::test]
    async fn apply_diff_rejects_conflict() {
        let dir = tempfile::TempDir::new().unwrap();
        let root = dir.path().to_path_buf();
        tokio::fs::write(root.join("a.txt"), "alpha\nbeta\ngamma\n")
            .await
            .unwrap();
        let tool = ApplyDiffTool::new(root.clone());
        let diff = "diff --git a/a.txt b/a.txt\nindex fbbee86..37b33db 100644\n--- a/a.txt\n+++ b/a.txt\n@@ -1,2 +1,2 @@\n alpha\n-beta\n+Beta\n";
        let res = tool
            .run(&serde_json::json!({ "diff": diff }).to_string())
            .await;
        let err = res.unwrap_err().to_string();
        assert!(
            err.contains("conflict") || err.contains("io error"),
            "got: {err}"
        );
        let on_disk = tokio::fs::read_to_string(root.join("a.txt")).await.unwrap();
        assert_eq!(on_disk, "alpha\nbeta\ngamma\n");
    }

    #[tokio::test]
    async fn apply_diff_rejects_path_escape() {
        let dir = tempfile::TempDir::new().unwrap();
        let root = dir.path().to_path_buf();
        let tool = ApplyDiffTool::new(root.clone());
        let diff = "diff --git a/../outside.txt b/../outside.txt\nnew file mode 100644\nindex 0000000..d00491f\n--- /dev/null\n+++ b/../outside.txt\n@@ -0,0 +1 @@\n+escaped\n";
        let res = tool
            .run(&serde_json::json!({ "diff": diff }).to_string())
            .await;
        let err = res.unwrap_err().to_string();
        assert!(
            err.contains("escapes working directory") || err.contains("outside"),
            "got: {err}"
        );
    }

    #[tokio::test]
    async fn apply_diff_rejects_binary_patch() {
        let dir = tempfile::TempDir::new().unwrap();
        let root = dir.path().to_path_buf();
        let tool = ApplyDiffTool::new(root.clone());
        let diff = "diff --git a/bin.dat b/bin.dat\nGIT binary patch\nliteral 5\nMc$N*OH-<00?\n";
        let res = tool
            .run(&serde_json::json!({ "diff": diff }).to_string())
            .await;
        let err = res.unwrap_err().to_string();
        assert!(
            err.contains("binary") || err.contains("patch format"),
            "got: {err}"
        );
    }

    #[tokio::test]
    async fn apply_diff_atomic_one_bad_file_aborts_all() {
        let dir = tempfile::TempDir::new().unwrap();
        let root = dir.path().to_path_buf();
        tokio::fs::write(root.join("good.txt"), "good\n")
            .await
            .unwrap();
        tokio::fs::write(root.join("bad.txt"), "old\n")
            .await
            .unwrap();
        let tool = ApplyDiffTool::new(root.clone());
        let diff = "diff --git a/good.txt b/good.txt\nindex 980a0d..8c7a5d6 100644\n--- a/good.txt\n+++ b/good.txt\n@@ -1,1 +1,1 @@\n-good\n+GOOD\n\ndiff --git a/bad.txt b/bad.txt\nindex 6c0d73..85c050f 100644\n--- a/bad.txt\n+++ b/bad.txt\n@@ -1,1 +1,1 @@\n-wrong\n+RIGHT\n";
        let res = tool
            .run(&serde_json::json!({ "diff": diff }).to_string())
            .await;
        let err = res.unwrap_err().to_string();
        assert!(
            err.contains("conflict") || err.contains("io error"),
            "got: {err}"
        );
        let good = tokio::fs::read_to_string(root.join("good.txt"))
            .await
            .unwrap();
        let bad = tokio::fs::read_to_string(root.join("bad.txt"))
            .await
            .unwrap();
        assert_eq!(good, "good\n");
        assert_eq!(bad, "old\n");
    }

    #[tokio::test]
    async fn apply_diff_rejects_oversize_input() {
        let dir = tempfile::TempDir::new().unwrap();
        let root = dir.path().to_path_buf();
        let tool = ApplyDiffTool::new(root.clone());
        let big_line = "x".repeat(1000);
        let mut diff = String::from("diff --git a/a.txt b/a.txt\n");
        for _ in 0..300 {
            diff.push_str(&format!("@@ -1,1 +1,1 @@\n-{}\n+{}\n", big_line, big_line));
        }
        assert!(
            diff.len() > MAX_DIFF_INPUT_BYTES,
            "big diff should exceed limit"
        );
        let res = tool
            .run(&serde_json::json!({ "diff": diff }).to_string())
            .await;
        let err = res.unwrap_err().to_string();
        assert!(
            err.contains("too large") || err.contains("256"),
            "got: {err}"
        );
    }

    #[tokio::test]
    async fn apply_diff_empty_diff_is_rejected() {
        let dir = tempfile::TempDir::new().unwrap();
        let root = dir.path().to_path_buf();
        let tool = ApplyDiffTool::new(root.clone());
        let res = tool
            .run(&serde_json::json!({ "diff": "" }).to_string())
            .await;
        let err = res.unwrap_err().to_string();
        assert!(
            err.contains("empty") || err.contains("no hunks"),
            "got: {err}"
        );
    }
}
