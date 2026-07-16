//! `grep` — search file contents for a regex. Returns matching lines as
//! `path:lineno:line`. An optional `path` glob filters which files to search
//! (default: all files under the working directory).

use super::{list_files, truncate_output};
use crate::tools::Tool;
use anyhow::{Context, Result};
use async_trait::async_trait;
use regex::Regex;
use serde::Deserialize;
use std::borrow::Cow;
use std::path::{Path, PathBuf};

/// Cap on how much of a file `grep` is willing to read into memory and scan,
/// **independent of** [`super::MAX_OUTPUT_BYTES`] (the *result-string* cap).
/// Conflating the two meant any file over 32 KiB was silently skipped even
/// though it has nothing to do with how big the matched-lines output is
/// (ADR-0091, superseding the grep clause of ADR-0008 point 4). A file over
/// this cap is reported in a skip notice rather than silently dropped.
const MAX_SCAN_BYTES: u64 = 1024 * 1024;

/// Cap on how many skipped-file entries the skip notice lists per reason
/// before collapsing the rest into an "and N more" tail.
const MAX_SKIP_PREVIEW: usize = 20;

/// Why a file was excluded from the scan.
enum SkipReason {
    TooLarge(u64),
    Binary,
}

pub struct GrepTool {
    root: std::path::PathBuf,
}

impl GrepTool {
    pub fn new(root: std::path::PathBuf) -> Self {
        Self { root }
    }
}

/// Append a labeled, capped-preview notice for skipped files to `out`,
/// grouped by reason. No-op if `skipped` is empty. Runs regardless of match
/// count — a match that exists only in a skipped file must not look
/// identical to "no match" (ADR-0016).
fn append_skip_notice(mut out: String, skipped: &[(PathBuf, SkipReason)], root: &Path) -> String {
    if skipped.is_empty() {
        return out;
    }
    let rel = |p: &Path| {
        p.strip_prefix(root)
            .unwrap_or(p)
            .to_string_lossy()
            .into_owned()
    };
    let too_large: Vec<_> = skipped
        .iter()
        .filter_map(|(p, r)| match r {
            SkipReason::TooLarge(len) => Some((p, *len)),
            SkipReason::Binary => None,
        })
        .collect();
    let binary: Vec<_> = skipped
        .iter()
        .filter(|(_, r)| matches!(r, SkipReason::Binary))
        .map(|(p, _)| p)
        .collect();
    if !too_large.is_empty() {
        out.push_str(&format!(
            "\n[skipped {} file(s) over the {} KiB scan cap:\n",
            too_large.len(),
            MAX_SCAN_BYTES / 1024
        ));
        for (p, len) in too_large.iter().take(MAX_SKIP_PREVIEW) {
            out.push_str(&format!("  {} ({len} bytes)\n", rel(p)));
        }
        if too_large.len() > MAX_SKIP_PREVIEW {
            out.push_str(&format!(
                "  ... and {} more\n",
                too_large.len() - MAX_SKIP_PREVIEW
            ));
        }
        out.push(']');
    }
    if !binary.is_empty() {
        out.push_str(&format!(
            "\n[skipped {} binary file(s) (NUL byte detected):\n",
            binary.len()
        ));
        for p in binary.iter().take(MAX_SKIP_PREVIEW) {
            out.push_str(&format!("  {}\n", rel(p)));
        }
        if binary.len() > MAX_SKIP_PREVIEW {
            out.push_str(&format!(
                "  ... and {} more\n",
                binary.len() - MAX_SKIP_PREVIEW
            ));
        }
        out.push(']');
    }
    out
}

#[derive(Deserialize)]
struct GrepInput {
    pattern: String,
    #[serde(default)]
    path: Option<String>,
}

#[async_trait]
impl Tool for GrepTool {
    fn name(&self) -> Cow<'static, str> {
        Cow::Borrowed("grep")
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
        let mut skipped: Vec<(PathBuf, SkipReason)> = Vec::new();
        for p in list.files {
            // Bound per-file work independent of the output cap (ADR-0091):
            // skip files over MAX_SCAN_BYTES rather than the far smaller
            // result-string cap.
            let len = match std::fs::metadata(&p) {
                Ok(m) => m.len(),
                Err(_) => continue,
            };
            if len > MAX_SCAN_BYTES {
                skipped.push((p, SkipReason::TooLarge(len)));
                continue;
            }
            let bytes = tokio::fs::read(&p)
                .await
                .with_context(|| format!("reading {:?}", p))?;
            if bytes.contains(&0) {
                skipped.push((p, SkipReason::Binary));
                continue;
            }
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
                        return Ok(truncate_output(append_skip_notice(
                            out, &skipped, &self.root,
                        )));
                    }
                }
            }
        }
        Ok(truncate_output(append_skip_notice(
            out, &skipped, &self.root,
        )))
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

    /// Regression: a file over the old 32 KiB output-cap-as-scan-cap used to
    /// be silently skipped even though it's well under the new 1 MiB
    /// MAX_SCAN_BYTES — a match in it must now be found.
    #[tokio::test]
    async fn file_between_old_and_new_cap_is_now_found() {
        let dir = tmp();
        let mut content = "x\n".repeat(20 * 1024); // ~40 KiB, over the old 32 KiB cap
        content.push_str("needle-in-big-file\n");
        std::fs::write(dir.path().join("big.txt"), content).unwrap();
        let tool = GrepTool::new(dir.path().to_path_buf());
        let out = tool
            .run(r#"{"pattern":"needle-in-big-file"}"#)
            .await
            .unwrap();
        assert!(out.contains("big.txt"), "got: {out}");
        assert!(!out.contains("scan cap"), "unexpected skip notice: {out}");
    }

    #[tokio::test]
    async fn file_over_new_scan_cap_is_skipped_with_notice() {
        let dir = tmp();
        let mut content = "x\n".repeat(600 * 1024); // over 1 MiB
        content.push_str("needle\n");
        std::fs::write(dir.path().join("huge.txt"), content).unwrap();
        let tool = GrepTool::new(dir.path().to_path_buf());
        let out = tool.run(r#"{"pattern":"needle"}"#).await.unwrap();
        assert!(
            !out.contains("huge.txt:"),
            "match should not be found: {out}"
        );
        assert!(out.contains("scan cap"), "got: {out}");
        assert!(
            out.contains("huge.txt"),
            "notice should name the path: {out}"
        );
    }

    #[tokio::test]
    async fn binary_file_is_skipped_with_distinct_notice() {
        let dir = tmp();
        let mut content = b"needle\x00binary".to_vec();
        content.extend_from_slice(b"\nneedle\n");
        std::fs::write(dir.path().join("bin.dat"), content).unwrap();
        let tool = GrepTool::new(dir.path().to_path_buf());
        let out = tool.run(r#"{"pattern":"needle"}"#).await.unwrap();
        assert!(
            !out.contains("bin.dat:"),
            "match should not be found: {out}"
        );
        assert!(out.contains("binary"), "got: {out}");
        assert!(
            out.contains("bin.dat"),
            "notice should name the path: {out}"
        );
        assert!(
            !out.contains("scan cap"),
            "should not be reported as too-large: {out}"
        );
    }

    #[tokio::test]
    async fn non_ascii_utf8_without_nul_is_still_searched() {
        let dir = tmp();
        std::fs::write(dir.path().join("intl.txt"), "přehled\nneedle-tady\n").unwrap();
        let tool = GrepTool::new(dir.path().to_path_buf());
        let out = tool.run(r#"{"pattern":"needle-tady"}"#).await.unwrap();
        assert!(out.contains("intl.txt:2:needle-tady"), "got: {out}");
        assert!(!out.contains("binary"), "got: {out}");
    }

    #[tokio::test]
    async fn multiple_skip_reasons_reported_separately() {
        let dir = tmp();
        let mut too_large = "x\n".repeat(600 * 1024);
        too_large.push_str("needle\n");
        std::fs::write(dir.path().join("huge.txt"), too_large).unwrap();
        std::fs::write(dir.path().join("bin.dat"), b"needle\x00\n").unwrap();
        let tool = GrepTool::new(dir.path().to_path_buf());
        let out = tool.run(r#"{"pattern":"needle"}"#).await.unwrap();
        assert!(out.contains("scan cap"), "missing too-large section: {out}");
        assert!(out.contains("binary"), "missing binary section: {out}");
        assert!(out.contains("huge.txt"), "got: {out}");
        assert!(out.contains("bin.dat"), "got: {out}");
    }

    #[tokio::test]
    async fn long_skip_list_preview_truncates_with_and_n_more() {
        let dir = tmp();
        for i in 0..(MAX_SKIP_PREVIEW + 5) {
            let mut content = "x\n".repeat(600 * 1024);
            content.push_str("needle\n");
            std::fs::write(dir.path().join(format!("huge{i}.txt")), content).unwrap();
        }
        let tool = GrepTool::new(dir.path().to_path_buf());
        let out = tool.run(r#"{"pattern":"needle"}"#).await.unwrap();
        assert!(out.contains("and 5 more"), "got: {out}");
    }
}
