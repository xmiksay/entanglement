//! `grep` — search file contents for a regex. Returns matching lines as
//! `path:lineno:line`. An optional `path` glob filters which files to search
//! (default: all files under the working directory).

use super::{list_files, truncate_output};
use anyhow::{Context, Result};
use async_trait::async_trait;
use entanglement_core::tools::Tool;
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
        let list = list_files(&self.root, filter)?;
        let mut out = String::new();
        let mut matches = 0usize;
        for p in list.files {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp() -> tempfile::TempDir {
        tempfile::tempdir().expect("temp dir")
    }

    #[tokio::test]
    async fn matches_report_path_lineno_line() {
        let dir = tmp();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/m.rs"), "fn one() {}\nfn two() {}\n").unwrap();
        let tool = GrepTool::new(dir.path().to_path_buf());
        let out = tool.run(r#"{"pattern":"fn two"}"#).await.unwrap();
        assert!(out.contains("src/m.rs:2:fn two() {}"), "got: {out}");
        assert!(!out.contains("one"), "got: {out}");
    }

    #[tokio::test]
    async fn path_glob_filters_which_files_are_searched() {
        let dir = tmp();
        std::fs::write(dir.path().join("keep.rs"), "needle\n").unwrap();
        std::fs::write(dir.path().join("skip.md"), "needle\n").unwrap();
        let tool = GrepTool::new(dir.path().to_path_buf());
        let out = tool
            .run(r#"{"pattern":"needle","path":"**/*.rs"}"#)
            .await
            .unwrap();
        assert!(out.contains("keep.rs"), "got: {out}");
        assert!(!out.contains("skip.md"), "got: {out}");
    }

    #[tokio::test]
    async fn regex_syntax_is_honored() {
        let dir = tmp();
        std::fs::write(dir.path().join("f.txt"), "foo123\nbarbaz\n").unwrap();
        let tool = GrepTool::new(dir.path().to_path_buf());
        let out = tool.run(r#"{"pattern":"foo\\d+"}"#).await.unwrap();
        assert!(out.contains("foo123"), "got: {out}");
        assert!(!out.contains("barbaz"), "got: {out}");
    }

    #[tokio::test]
    async fn invalid_regex_errors() {
        let dir = tmp();
        let tool = GrepTool::new(dir.path().to_path_buf());
        let err = tool.run(r#"{"pattern":"("}"#).await.unwrap_err();
        assert!(format!("{err}").contains("invalid regex"), "{err}");
    }

    #[tokio::test]
    async fn no_match_yields_empty_output() {
        let dir = tmp();
        std::fs::write(dir.path().join("f.txt"), "hello\n").unwrap();
        let tool = GrepTool::new(dir.path().to_path_buf());
        let out = tool.run(r#"{"pattern":"zzz"}"#).await.unwrap();
        assert!(out.is_empty(), "got: {out:?}");
    }

    #[tokio::test]
    async fn invalid_json_input_errors() {
        let dir = tmp();
        let tool = GrepTool::new(dir.path().to_path_buf());
        let err = tool.run("{}").await.unwrap_err();
        assert!(format!("{err}").contains("invalid input to grep"), "{err}");
    }
}
