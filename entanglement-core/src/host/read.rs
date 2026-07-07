//! `read` — read a UTF-8 text file relative to the working directory, returned
//! as `{lineno}: {line}` so the model can address ranges precisely.

use super::{resolve_under_root, truncate_output};
use crate::tools::Tool;
use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::Deserialize;

type OnReadCallback = Box<dyn Fn(String, Vec<u8>) + Send + Sync>;

pub struct ReadTool {
    root: std::path::PathBuf,
    on_read: Option<OnReadCallback>,
}

impl ReadTool {
    pub fn new(root: std::path::PathBuf) -> Self {
        Self {
            root,
            on_read: None,
        }
    }

    pub fn with_on_read<F>(mut self, f: F) -> Self
    where
        F: Fn(String, Vec<u8>) + Send + Sync + 'static,
    {
        self.on_read = Some(Box::new(f));
        self
    }
}

#[derive(Deserialize)]
struct ReadInput {
    path: String,
    #[serde(default)]
    offset: Option<usize>,
    #[serde(default)]
    limit: Option<usize>,
}

#[async_trait]
impl Tool for ReadTool {
    fn name(&self) -> &'static str {
        "read"
    }
    fn description(&self) -> &str {
        "Read a UTF-8 text file under the working directory, returning its \
         contents with 1-based line numbers. Optional `offset` (line to start \
         at) and `limit` (max lines)."
    }
    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "File path relative to the working directory, or an absolute path inside it."
                },
                "offset": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "1-based line number to start at (default 1)."
                },
                "limit": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "Maximum number of lines to return (default 2000)."
                }
            },
            "required": ["path"]
        })
    }
    async fn run(&self, input: &str) -> Result<String> {
        let parsed: ReadInput = serde_json::from_str(input)
            .context("invalid input to read: expected {\"path\": string, ...}")?;
        let full = resolve_under_root(&self.root, &parsed.path)?;
        let bytes = tokio::fs::read(&full)
            .await
            .with_context(|| format!("reading {}", parsed.path))?;
        let text = String::from_utf8(bytes.clone())
            .with_context(|| format!("{} is not valid UTF-8", parsed.path))?;

        if let Some(ref on_read) = self.on_read {
            on_read(parsed.path.clone(), bytes);
        }

        let offset = parsed.offset.unwrap_or(1).max(1);
        let limit = parsed.limit.unwrap_or(2000);
        let mut out = String::new();
        for (i, line) in text.lines().enumerate() {
            let lineno = i + 1;
            if lineno < offset {
                continue;
            }
            if lineno >= offset + limit {
                break;
            }
            out.push_str(&format!("{lineno}: {line}\n"));
        }
        Ok(truncate_output(out))
    }
}
