//! `read` — read a UTF-8 text file relative to the working directory, returned
//! as `{lineno}: {line}` so the model can address ranges precisely.

use super::{resolve_under_root, truncate_output};
use anyhow::{Context, Result};
use async_trait::async_trait;
use entanglement_core::tools::Tool;
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

    #[allow(dead_code)]
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    fn tmp() -> tempfile::TempDir {
        tempfile::tempdir().expect("temp dir")
    }

    #[tokio::test]
    async fn returns_lines_with_1_based_numbers() {
        let dir = tmp();
        std::fs::write(dir.path().join("a.txt"), "alpha\nbeta\ngamma\n").unwrap();
        let tool = ReadTool::new(dir.path().to_path_buf());
        let out = tool.run(r#"{"path":"a.txt"}"#).await.unwrap();
        assert_eq!(out, "1: alpha\n2: beta\n3: gamma\n");
    }

    #[tokio::test]
    async fn offset_and_limit_bound_the_window() {
        let dir = tmp();
        std::fs::write(dir.path().join("a.txt"), "l1\nl2\nl3\nl4\nl5\n").unwrap();
        let tool = ReadTool::new(dir.path().to_path_buf());
        let out = tool
            .run(r#"{"path":"a.txt","offset":2,"limit":2}"#)
            .await
            .unwrap();
        assert_eq!(out, "2: l2\n3: l3\n");
    }

    #[tokio::test]
    async fn offset_zero_is_clamped_to_one() {
        let dir = tmp();
        std::fs::write(dir.path().join("a.txt"), "only\n").unwrap();
        let tool = ReadTool::new(dir.path().to_path_buf());
        let out = tool.run(r#"{"path":"a.txt","offset":0}"#).await.unwrap();
        assert_eq!(out, "1: only\n");
    }

    #[tokio::test]
    async fn missing_file_errors() {
        let dir = tmp();
        let tool = ReadTool::new(dir.path().to_path_buf());
        let err = tool.run(r#"{"path":"nope.txt"}"#).await.unwrap_err();
        assert!(format!("{err}").contains("reading nope.txt"), "{err}");
    }

    #[tokio::test]
    async fn non_utf8_file_errors() {
        let dir = tmp();
        std::fs::write(dir.path().join("bin"), [0xff, 0xfe, 0x00]).unwrap();
        let tool = ReadTool::new(dir.path().to_path_buf());
        let err = tool.run(r#"{"path":"bin"}"#).await.unwrap_err();
        assert!(format!("{err}").contains("not valid UTF-8"), "{err}");
    }

    #[tokio::test]
    async fn path_escaping_root_is_rejected() {
        let dir = tmp();
        let tool = ReadTool::new(dir.path().to_path_buf());
        let err = tool.run(r#"{"path":"../secret"}"#).await.unwrap_err();
        assert!(
            format!("{err}").contains("escapes working directory"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn on_read_callback_fires_with_raw_bytes() {
        let dir = tmp();
        std::fs::write(dir.path().join("a.txt"), "hi\n").unwrap();
        type ReadRecord = (String, Vec<u8>);
        let seen: Arc<Mutex<Vec<ReadRecord>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = seen.clone();
        let tool = ReadTool::new(dir.path().to_path_buf())
            .with_on_read(move |p, b| sink.lock().unwrap().push((p, b)));
        tool.run(r#"{"path":"a.txt"}"#).await.unwrap();
        let recorded = seen.lock().unwrap();
        assert_eq!(recorded.len(), 1);
        assert_eq!(recorded[0].0, "a.txt");
        assert_eq!(recorded[0].1, b"hi\n");
    }

    #[tokio::test]
    async fn invalid_json_input_errors() {
        let dir = tmp();
        let tool = ReadTool::new(dir.path().to_path_buf());
        let err = tool.run("not json").await.unwrap_err();
        assert!(format!("{err}").contains("invalid input to read"), "{err}");
    }
}
