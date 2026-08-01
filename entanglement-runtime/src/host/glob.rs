//! `glob` — list files matching a glob pattern (e.g. `**/*.rs`), paths
//! relative to the working directory. A bare directory pattern lists it
//! recursively and a zero-result call always explains itself (ADR-0150).

use super::{list_files_with_extra_roots, truncate_output};
use crate::extra_roots::ExtraRootStore;
use crate::tools::Tool;
use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::Deserialize;
use std::borrow::Cow;
use std::sync::Arc;

pub struct GlobTool {
    root: std::path::PathBuf,
    /// Widens a search into a directory already covered by a durable `read`
    /// grant (ADR-0109/#482). `None` keeps strict containment.
    extra_roots: Option<Arc<ExtraRootStore>>,
}

impl GlobTool {
    pub fn new(root: std::path::PathBuf) -> Self {
        Self {
            root,
            extra_roots: None,
        }
    }

    /// Let a search descend into a directory the user already granted `read`
    /// access to (#482) — see [`super::list_files_with_extra_roots`].
    pub fn with_extra_roots(mut self, extra: Arc<ExtraRootStore>) -> Self {
        self.extra_roots = Some(extra);
        self
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GlobInput {
    pattern: String,
    /// Base directory the pattern is resolved under (Claude-Code `Glob.path`
    /// compat, ADR-0150). Absent → the working root.
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    exclude: Vec<String>,
}

#[async_trait]
impl Tool for GlobTool {
    fn name(&self) -> Cow<'static, str> {
        Cow::Borrowed("glob")
    }
    fn description(&self) -> &str {
        "List files matching a glob pattern (e.g. `**/*.rs`) relative to the \
         working directory. A bare directory path lists it recursively; brace \
         sets like `**/*.{rs,md}` expand. Returns matching paths, one per \
         line. `.git` is always excluded; an optional `exclude` list of glob \
         patterns filters out additional paths."
    }
    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "pattern": {
                    "type": "string",
                    "description": "Glob pattern (`**/*.rs`, `src/**/*.{toml,yml}`) or a bare directory path (listed recursively)."
                },
                "path": {
                    "type": "string",
                    "description": "Optional base directory to resolve `pattern` under (default: the working root)."
                },
                "exclude": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Glob patterns to exclude from results, e.g. `[\"target/**\", \"node_modules/**\"]`. `.git` is always excluded regardless of this list."
                }
            },
            "required": ["pattern"]
        })
    }
    async fn run(&self, input: &str) -> Result<String> {
        let parsed: GlobInput = serde_json::from_str(input)
            .context("invalid input to glob: expected {\"pattern\": string, ...}")?;
        let pattern = joined_pattern(parsed.path.as_deref(), &parsed.pattern);
        tracing::debug!(pattern = %pattern, root = %self.root.display(), "glob tool executing");
        let list = list_files_with_extra_roots(
            &self.root,
            &pattern,
            &parsed.exclude,
            self.extra_roots.as_deref(),
        )?;
        tracing::debug!(
            files = list.files.len(),
            matched_dirs = list.matched_dirs,
            skipped_errors = list.skipped_errors,
            out_of_root = list.out_of_root,
            "glob tool enumerated entries",
        );
        let mut out = String::new();
        for p in &list.files {
            let rel = p.strip_prefix(&self.root).unwrap_or(p);
            out.push_str(&rel.to_string_lossy());
            out.push('\n');
        }
        if out.is_empty() {
            // A zero-result call always explains itself (ADR-0016, extended by
            // ADR-0150): to the model an empty string is indistinguishable
            // from a typo'd pattern, so name the cause instead.
            if list.matched_dirs > 0 {
                let dirs_word = if list.matched_dirs == 1 {
                    "directory"
                } else {
                    "directories"
                };
                let suggested = suggest_files_pattern(&pattern);
                return Ok(format!(
                    "pattern `{}` matched {} {} but no files (files are filtered out). \
                     Try `{}` to list files inside those directories.",
                    pattern, list.matched_dirs, dirs_word, suggested,
                ));
            }
            if list.skipped_errors > 0 {
                return Ok(format!(
                    "pattern `{}` matched no files; {} entries were skipped due to read errors \
                     (see engine logs with `RUST_LOG=entanglement_core::host=warn`).",
                    pattern, list.skipped_errors,
                ));
            }
            let mut msg = format!("pattern `{pattern}` matched no files.");
            if list.out_of_root > 0 {
                msg.push_str(&format!(
                    " {} match(es) outside the project root were excluded.",
                    list.out_of_root,
                ));
            }
            return Ok(msg);
        }
        // Truncate first so the cap notice survives the head-only byte cut.
        let mut result = truncate_output(out);
        if list.capped {
            result.push_str("\n[capped at 1000 results — narrow the pattern]");
        }
        tracing::debug!(output_len = result.len(), "glob tool result");
        Ok(result)
    }
}

/// Resolve the optional `path` base directory onto `pattern` — the same string
/// the permission layer grades (ADR-0150). An empty/blank base is ignored
/// rather than joined: a leading `/` would re-root the walk at the filesystem
/// root via `Path::join`.
pub(crate) fn joined_pattern(base: Option<&str>, pattern: &str) -> String {
    match base.map(|b| b.trim_end_matches('/')) {
        Some(b) if !b.is_empty() => format!("{b}/{pattern}"),
        _ => pattern.to_string(),
    }
}

/// Suggest a pattern that will actually match files when the user-supplied one
/// matched only directories. Appends `/*` unless the pattern already ends in
/// `/*` (a `dir/*` or `**/*` shape — already trying to list files, so the
/// "matched only dirs" outcome is a real finding we just echo back).
pub(crate) fn suggest_files_pattern(pattern: &str) -> String {
    if pattern.ends_with("/*") {
        pattern.to_string()
    } else {
        format!("{pattern}/*")
    }
}

#[cfg(test)]
mod tests {
    use super::{joined_pattern, suggest_files_pattern};

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

    #[test]
    fn joined_pattern_prefixes_base_dir() {
        assert_eq!(joined_pattern(Some("src"), "**/*.rs"), "src/**/*.rs");
        assert_eq!(joined_pattern(Some("src/"), "*.rs"), "src/*.rs");
    }

    #[test]
    fn joined_pattern_ignores_blank_base() {
        assert_eq!(joined_pattern(None, "**/*.rs"), "**/*.rs");
        assert_eq!(joined_pattern(Some(""), "**/*.rs"), "**/*.rs");
        assert_eq!(joined_pattern(Some("/"), "**/*.rs"), "**/*.rs");
    }
}
