//! `glob` — list files matching a glob pattern (e.g. `**/*.rs`), paths
//! relative to the working directory.

use super::{list_files, truncate_output};
use crate::tools::Tool;
use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::Deserialize;

pub struct GlobTool {
    root: std::path::PathBuf,
}

impl GlobTool {
    pub fn new(root: std::path::PathBuf) -> Self {
        Self { root }
    }
}

#[derive(Deserialize)]
struct GlobInput {
    pattern: String,
}

#[async_trait]
impl Tool for GlobTool {
    fn name(&self) -> &'static str {
        "glob"
    }
    fn description(&self) -> &str {
        "List files matching a glob pattern (e.g. `**/*.rs`) relative to the \
         working directory. Returns matching paths, one per line."
    }
    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "pattern": {
                    "type": "string",
                    "description": "Glob pattern, e.g. `**/*.rs` or `src/**/*.toml`."
                }
            },
            "required": ["pattern"]
        })
    }
    async fn run(&self, input: &str) -> Result<String> {
        let parsed: GlobInput = serde_json::from_str(input)
            .context("invalid input to glob: expected {\"pattern\": string}")?;
        tracing::debug!(pattern = %parsed.pattern, root = %self.root.display(), "glob tool executing");
        let list = list_files(&self.root, &parsed.pattern)?;
        tracing::debug!(
            files = list.files.len(),
            matched_dirs = list.matched_dirs,
            skipped_errors = list.skipped_errors,
            "glob tool enumerated entries",
        );
        let mut out = String::new();
        for p in &list.files {
            let rel = p.strip_prefix(&self.root).unwrap_or(p);
            out.push_str(&rel.to_string_lossy());
            out.push('\n');
        }
        if out.is_empty() {
            // The glob crate's bare `**` yields directory paths only, which
            // list_files filters out — to the model that looks identical to a
            // typo'd pattern. Surface an actionable hint so it can retry with
            // `**/*` instead of guessing. See ADR-0016.
            if list.matched_dirs > 0 {
                let dirs_word = if list.matched_dirs == 1 {
                    "directory"
                } else {
                    "directories"
                };
                let suggested = suggest_files_pattern(&parsed.pattern);
                return Ok(format!(
                    "pattern `{}` matched {} {} but no files (files are filtered out). \
                     Try `{}` to list files inside those directories.",
                    parsed.pattern, list.matched_dirs, dirs_word, suggested,
                ));
            }
            if list.skipped_errors > 0 {
                return Ok(format!(
                    "pattern `{}` matched no files; {} entries were skipped due to read errors \
                     (see engine logs with `RUST_LOG=entanglement_core::host=warn`).",
                    parsed.pattern, list.skipped_errors,
                ));
            }
            // Clean no-match: fall through and return the empty string.
        }
        let result = truncate_output(out);
        tracing::debug!(output_len = result.len(), "glob tool result");
        Ok(result)
    }
}

/// Suggest a pattern that will actually match files when the user-supplied one
/// matched only directories. Appends `/*` unless the pattern already ends in
/// `/*` (a `dir/*` or `**/*` shape — already trying to list files, so the
/// "matched only dirs" outcome is a real finding we just echo back).
fn suggest_files_pattern(pattern: &str) -> String {
    if pattern.ends_with("/*") {
        pattern.to_string()
    } else {
        format!("{pattern}/*")
    }
}

#[cfg(test)]
mod tests {
    use super::suggest_files_pattern;

    #[test]
    fn suggest_appends_slash_star_for_bare_doublestar() {
        assert_eq!(suggest_files_pattern("**"), "**/*");
    }

    #[test]
    fn suggest_appends_for_dir_prefix() {
        assert_eq!(suggest_files_pattern("src/**"), "src/**/*");
    }

    #[test]
    fn suggest_leaves_existing_glob_alone() {
        assert_eq!(suggest_files_pattern("**/*"), "**/*");
        assert_eq!(suggest_files_pattern("src/**/*"), "src/**/*");
    }
}
