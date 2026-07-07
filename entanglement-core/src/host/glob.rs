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
        let paths = list_files(&self.root, &parsed.pattern)?;
        tracing::debug!(count = paths.len(), "glob tool found files");
        let mut out = String::new();
        for p in paths {
            let rel = p.strip_prefix(&self.root).unwrap_or(&p);
            out.push_str(&rel.to_string_lossy());
            out.push('\n');
        }
        let result = truncate_output(out);
        tracing::debug!(output_len = result.len(), "glob tool result");
        Ok(result)
    }
}
