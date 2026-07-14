//! `read` — read a file relative to the working directory. Text files come back
//! as `{lineno}: {line}` so the model can address ranges precisely; an image file
//! (detected by extension) comes back as a base64 image **content block** the
//! provider renders to its native image format (#221).

use super::{resolve_under_root, truncate_output};
use crate::tools::Tool;
use anyhow::{Context, Result};
use async_trait::async_trait;
use base64::Engine as _;
use entanglement_core::ContentPart;
use serde::Deserialize;

/// Map an image file extension to its IANA media type, or `None` for a
/// non-image. Only the formats the model providers accept inline (Anthropic's
/// image block / OpenAI's `image_url`) are recognized; anything else falls
/// through to the text path.
fn image_media_type(path: &str) -> Option<&'static str> {
    let ext = std::path::Path::new(path)
        .extension()
        .and_then(|e| e.to_str())?
        .to_ascii_lowercase();
    Some(match ext.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        _ => return None,
    })
}

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
        "Read a file under the working directory. A UTF-8 text file returns its \
         contents with 1-based line numbers (optional `offset` (line to start \
         at) and `limit` (max lines)); an image file (png/jpeg/gif/webp) returns \
         the image itself for you to view."
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

    /// An image file (by extension) is base64-encoded into an image content block
    /// the provider renders natively (#221); `offset`/`limit` don't apply. Every
    /// other file takes the text path via [`run`][Self::run].
    async fn run_content(&self, input: &str) -> Result<Vec<ContentPart>> {
        let parsed: ReadInput = serde_json::from_str(input)
            .context("invalid input to read: expected {\"path\": string, ...}")?;
        let Some(media_type) = image_media_type(&parsed.path) else {
            return Ok(crate::tools::text_parts(self.run(input).await?));
        };
        let full = resolve_under_root(&self.root, &parsed.path)?;
        let bytes = tokio::fs::read(&full)
            .await
            .with_context(|| format!("reading {}", parsed.path))?;
        if let Some(ref on_read) = self.on_read {
            on_read(parsed.path.clone(), bytes.clone());
        }
        let data = base64::engine::general_purpose::STANDARD.encode(&bytes);
        Ok(vec![ContentPart::image(media_type, data)])
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

    #[test]
    fn image_extensions_map_to_media_types() {
        assert_eq!(image_media_type("a.png"), Some("image/png"));
        assert_eq!(image_media_type("photo.JPG"), Some("image/jpeg"));
        assert_eq!(image_media_type("x.jpeg"), Some("image/jpeg"));
        assert_eq!(image_media_type("anim.gif"), Some("image/gif"));
        assert_eq!(image_media_type("logo.webp"), Some("image/webp"));
        assert_eq!(image_media_type("readme.md"), None);
        assert_eq!(image_media_type("noext"), None);
    }

    #[tokio::test]
    async fn image_file_returns_a_base64_image_block() {
        // A non-UTF-8 image body that would fail the text path is base64-encoded
        // into an image content block instead (#221).
        let dir = tmp();
        let bytes = [0x89u8, 0x50, 0x4e, 0x47, 0xff, 0x00];
        std::fs::write(dir.path().join("pic.png"), bytes).unwrap();
        let tool = ReadTool::new(dir.path().to_path_buf());
        let content = tool.run_content(r#"{"path":"pic.png"}"#).await.unwrap();
        assert_eq!(
            content,
            vec![ContentPart::image(
                "image/png",
                base64::engine::general_purpose::STANDARD.encode(bytes)
            )]
        );
    }

    #[tokio::test]
    async fn text_file_via_run_content_stays_text() {
        let dir = tmp();
        std::fs::write(dir.path().join("a.txt"), "alpha\nbeta\n").unwrap();
        let tool = ReadTool::new(dir.path().to_path_buf());
        let content = tool.run_content(r#"{"path":"a.txt"}"#).await.unwrap();
        assert_eq!(content, vec![ContentPart::text("1: alpha\n2: beta\n")]);
    }

    #[tokio::test]
    async fn image_read_fires_on_read_with_raw_bytes() {
        let dir = tmp();
        let bytes = [0x47u8, 0x49, 0x46, 0x38];
        std::fs::write(dir.path().join("x.gif"), bytes).unwrap();
        type ReadRecord = (String, Vec<u8>);
        let seen: Arc<Mutex<Vec<ReadRecord>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = seen.clone();
        let tool = ReadTool::new(dir.path().to_path_buf())
            .with_on_read(move |p, b| sink.lock().unwrap().push((p, b)));
        tool.run_content(r#"{"path":"x.gif"}"#).await.unwrap();
        let recorded = seen.lock().unwrap();
        assert_eq!(recorded.len(), 1);
        assert_eq!(recorded[0].0, "x.gif");
        assert_eq!(recorded[0].1, bytes);
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
