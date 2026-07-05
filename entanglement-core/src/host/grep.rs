//! `grep` — search file contents for a regex. Returns matching lines as
//! `path:lineno:line`. An optional `path` glob filters which files to search
//! (default: all files under the working directory).

use super::{list_files, truncate_output};
use crate::tools::Tool;
use anyhow::{Context, Result};
use async_trait::async_trait;
use regex::Regex;
use serde::Deserialize;

pub struct GrepTool {
    root: std::path::PathBuf,
}

impl GrepTool {
    pub fn new(root: std::path::PathBuf) -> Self {
        Self { root }
    }
}

#[derive(Deserialize)]
struct GrepInput {
    pattern: String,
    #[serde(default)]
    path: Option<String>,
}

#[async_trait]
impl Tool for GrepTool {
    fn name(&self) -> &'static str {
        "grep"
    }
    fn description(&self) -> &str {
        "Search file contents for a regular expression. Returns matching lines \
         as `path:lineno:line`. Optional `path` glob filters which files to \
         search (default: all files under the working directory)."
    }
    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "pattern": {
                    "type": "string",
                    "description": "Regular expression (Rust regex syntax)."
                },
                "path": {
                    "type": "string",
                    "description": "Optional glob filter limiting which files to search, e.g. `**/*.rs` (default `**/*`)."
                }
            },
            "required": ["pattern"]
        })
    }
    async fn run(&self, input: &str) -> Result<String> {
        let parsed: GrepInput = serde_json::from_str(input)
            .context("invalid input to grep: expected {\"pattern\": string, ...}")?;
        let re = Regex::new(&parsed.pattern)
            .with_context(|| format!("invalid regex: {}", parsed.pattern))?;
        let filter = parsed.path.as_deref().unwrap_or("**/*");
        let paths = list_files(&self.root, filter)?;
        let mut out = String::new();
        let mut matches = 0usize;
        for p in paths {
            // Bound per-file work: skip files far larger than the output cap.
            let len = match std::fs::metadata(&p) {
                Ok(m) => m.len(),
                Err(_) => continue,
            };
            if len > super::MAX_OUTPUT_BYTES as u64 {
                continue;
            }
            let bytes = tokio::fs::read(&p)
                .await
                .with_context(|| format!("reading {:?}", p))?;
            let text = String::from_utf8_lossy(&bytes);
            for (lineno, line) in text.lines().enumerate() {
                if re.is_match(line) {
                    let rel = p.strip_prefix(&self.root).unwrap_or(&p);
                    out.push_str(&format!(
                        "{}:{}:{}\n",
                        rel.to_string_lossy(),
                        lineno + 1,
                        line
                    ));
                    matches += 1;
                    if matches >= super::MAX_RESULTS {
                        return Ok(truncate_output(out));
                    }
                }
            }
        }
        Ok(truncate_output(out))
    }
}
