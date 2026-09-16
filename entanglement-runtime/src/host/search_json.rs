//! Script-facing structured variants of the search tools (`glob_json`/
//! `grep_json`) — the [`read_raw`](super::read::ReadRawTool) pattern applied
//! to search (ADR-0206, extending ADR-0098's principle): registered for the
//! rhai bridge to execute, never advertised to the model, graded and masked
//! as aliases of their model-facing counterparts. The model-facing `glob`/
//! `grep` return prose-shaped text — one path per line, zero-match
//! explanations, bracketed skip notices — because that is what the *model*
//! needs. A script needs structure: the old binding handed that prose to
//! rhai as one string, where indexing silently yields single characters and
//! `.len()` a char count — every consumer became a hand-rolled parser, and
//! every parser mistake a wrong-but-not-error result.
//!
//! Both tools reuse their counterpart's walk and match logic and differ only
//! in output encoding: a JSON document the binding layer parses into Rhai
//! values. Notices (cap hits, skipped files, the zero-match explanation)
//! ride in a `notices` array instead of being dropped — a script that
//! ignores them loses diagnostics, not data.

use super::grep::{compile_regex, scan_matches, SkipReason, MAX_SCAN_BYTES};
use super::walk::MAX_RESULTS;
use super::{glob, list_files_with_extra_roots, FileList};
use crate::extra_roots::ExtraRootStore;
use crate::tools::Tool;
use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::Deserialize;
use std::borrow::Cow;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Keep the serialized document safely under the host output cap
/// ([`super::MAX_OUTPUT_BYTES`]): prose survives a byte-cap cut mid-line, a
/// JSON document does not — an over-budget doc would arrive at the binding
/// layer as unparseable text. Over-budget result arrays are trimmed from the
/// tail with a notice naming the original count.
const JSON_BUDGET: usize = super::MAX_OUTPUT_BYTES - 2 * 1024;

fn rel_path(p: &Path, root: &Path) -> String {
    p.strip_prefix(root)
        .unwrap_or(p)
        .to_string_lossy()
        .into_owned()
}

/// Walk-metadata notices shared by both tools — the structured counterpart
/// of the model-facing prose (ADR-0150: no silent zero-result), one string
/// per fact. `empty` gates the zero-result explanations (the prose tools only
/// surface "matched only directories"/"every entry errored"/"matched no
/// files" when they have nothing else to show).
fn walk_notices(list: &FileList, pattern: &str, narrow: &str, empty: bool) -> Vec<String> {
    let mut notices = Vec::new();
    if empty && list.matched_dirs > 0 {
        notices.push(format!(
            "pattern `{pattern}` matched {} director{} but no files — try `{}`",
            list.matched_dirs,
            if list.matched_dirs == 1 { "y" } else { "ies" },
            glob::suggest_files_pattern(pattern),
        ));
    } else if empty && list.skipped_errors > 0 {
        notices.push(format!(
            "{} entries skipped due to read errors (see engine logs with `RUST_LOG=entanglement_core::host=warn`)",
            list.skipped_errors
        ));
    } else if empty && list.files.is_empty() {
        notices.push(format!("pattern `{pattern}` matched no files."));
    }
    if list.out_of_root > 0 {
        notices.push(format!(
            "{} match(es) outside the project root were excluded",
            list.out_of_root
        ));
    }
    if list.capped {
        notices.push(format!(
            "[capped at {MAX_RESULTS} results — narrow {narrow}]"
        ));
    }
    if list.scan_capped {
        notices.push("[walk stopped after scanning 100000 entries — narrow the pattern]".into());
    }
    notices
}

/// Skip reasons as notice strings — the structured counterpart of
/// [`super::grep`]'s bracketed skip blocks.
fn skip_notices(skipped: &[(PathBuf, SkipReason)], root: &Path) -> Vec<String> {
    skipped
        .iter()
        .map(|(p, reason)| match reason {
            SkipReason::TooLarge(len) => format!(
                "skipped {} ({len} bytes, over the {} KiB scan cap)",
                rel_path(p, root),
                MAX_SCAN_BYTES / 1024
            ),
            SkipReason::Binary => format!("skipped {} (binary file)", rel_path(p, root)),
        })
        .collect()
}

/// Serialize `{key: entries, "notices": notices}` under [`JSON_BUDGET`],
/// trimming `entries` from the tail if needed and recording the original
/// count in a notice. Prose tools can truncate mid-line and stay readable;
/// this side must hand the binding layer *valid JSON*, so it budgets
/// instead of cutting.
fn fit_document(
    key: &str,
    entries: Vec<serde_json::Value>,
    notices: Vec<String>,
) -> Result<String> {
    let total = entries.len();
    let mut notices = notices;
    let doc = |entries: &[serde_json::Value], notices: &[String]| {
        serde_json::to_string(&serde_json::json!({
            key: entries,
            "notices": notices,
        }))
        .map_err(|e| anyhow::anyhow!("serializing search results: {e}"))
    };
    let mut owned = entries;
    let mut out = doc(&owned, &notices)?;
    if out.len() <= JSON_BUDGET {
        return Ok(out);
    }
    // Proportional first cut, then a linear trim to land under the budget —
    // at most MAX_RESULTS entries, so this is bounded work.
    let keep = (owned.len() * JSON_BUDGET / out.len().max(1)).max(1);
    owned.truncate(keep);
    while out.len() > JSON_BUDGET && owned.len() > 1 {
        owned.pop();
        out = doc(&owned, &notices)?;
    }
    notices.insert(
        0,
        format!(
            "[output truncated: first {} of {total} results — narrow the pattern]",
            owned.len()
        ),
    );
    doc(&owned, &notices)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GlobJsonInput {
    pattern: String,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    exclude: Vec<String>,
}

/// `glob` for scripts: the same walk as [`super::GlobTool`], encoded as
/// `{"files": […], "notices": […]}`. `files` empty plus a notice is the
/// zero-match shape — for a script an empty array is already actionable, the
/// notice carries the why.
pub struct GlobJsonTool {
    root: PathBuf,
    extra_roots: Option<Arc<ExtraRootStore>>,
}

impl GlobJsonTool {
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            extra_roots: None,
        }
    }

    /// Ride the same durable `read` grants a direct `glob` search does
    /// (#482/ADR-0132).
    pub fn with_extra_roots(mut self, extra: Arc<ExtraRootStore>) -> Self {
        self.extra_roots = Some(extra);
        self
    }
}

#[async_trait]
impl Tool for GlobJsonTool {
    fn name(&self) -> Cow<'static, str> {
        Cow::Borrowed("glob_json")
    }
    fn description(&self) -> &str {
        "Script-facing structured variant of `glob` (rhai binding layer only, \
         never model-advertised — ADR-0206): matching paths as a JSON document \
         `{\"files\": […], \"notices\": […]}` instead of prose text."
    }
    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "pattern": {
                    "type": "string",
                    "description": "Glob pattern (`**/*.rs`) or a bare directory path (listed recursively)."
                },
                "path": {
                    "type": "string",
                    "description": "Optional base dir the pattern resolves under (input parity with `glob`)."
                },
                "exclude": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Glob patterns to exclude, e.g. `[\"target/**\"]`."
                }
            },
            "required": ["pattern"]
        })
    }
    async fn run(&self, input: &str) -> Result<String> {
        let parsed: GlobJsonInput = serde_json::from_str(input)
            .context("invalid input to glob_json: expected {\"pattern\": string, ...}")?;
        let pattern = glob::joined_pattern(parsed.path.as_deref(), &parsed.pattern);
        let list = list_files_with_extra_roots(
            &self.root,
            &pattern,
            &parsed.exclude,
            self.extra_roots.as_deref(),
        )?;
        let files: Vec<serde_json::Value> = list
            .files
            .iter()
            .map(|p| serde_json::Value::String(rel_path(p, &self.root)))
            .collect();
        let notices = walk_notices(&list, &pattern, "the pattern", files.is_empty());
        fit_document("files", files, notices)
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GrepJsonInput {
    pattern: String,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    exclude: Vec<String>,
    #[serde(default, alias = "-i")]
    case_insensitive: bool,
}

/// `grep` for scripts: the same scan as [`super::GrepTool`] (via the shared
/// [`scan_matches`]), encoded as `{"matches": [{"path": …, "lineno": …,
/// "line": …}, …], "notices": […]}`.
pub struct GrepJsonTool {
    root: PathBuf,
    extra_roots: Option<Arc<ExtraRootStore>>,
}

impl GrepJsonTool {
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            extra_roots: None,
        }
    }

    /// Ride the same durable `read` grants a direct `grep` search does.
    pub fn with_extra_roots(mut self, extra: Arc<ExtraRootStore>) -> Self {
        self.extra_roots = Some(extra);
        self
    }
}

#[async_trait]
impl Tool for GrepJsonTool {
    fn name(&self) -> Cow<'static, str> {
        Cow::Borrowed("grep_json")
    }
    fn description(&self) -> &str {
        "Script-facing structured variant of `grep` (rhai binding layer only, \
         never model-advertised — ADR-0206): matches as a JSON document \
         `{\"matches\": [{\"path\": …, \"lineno\": …, \"line\": …}, …], \"notices\": […]}`."
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
                    "description": "Optional: a directory to search recursively or a glob filter. Default: `**/*`."
                },
                "exclude": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Glob patterns to exclude from the search."
                },
                "case_insensitive": {
                    "type": "boolean",
                    "description": "Match case-insensitively (default false)."
                }
            },
            "required": ["pattern"]
        })
    }
    async fn run(&self, input: &str) -> Result<String> {
        let parsed: GrepJsonInput = serde_json::from_str(input)
            .context("invalid input to grep_json: expected {\"pattern\": string, ...}")?;
        let re = compile_regex(&parsed.pattern, parsed.case_insensitive)?;
        let filter = parsed.path.as_deref().unwrap_or("**/*");
        let mut list = list_files_with_extra_roots(
            &self.root,
            filter,
            &parsed.exclude,
            self.extra_roots.as_deref(),
        )?;
        let scanned = list.files.len();
        let scan = scan_matches(&self.root, &mut list, &re).await?;
        let mut notices = skip_notices(&scan.skipped, &self.root);
        if scan.hit_match_cap {
            notices.push(format!("[match cap: first {MAX_RESULTS} matches shown]"));
        }
        notices.extend(walk_notices(
            &list,
            filter,
            "`path`",
            scan.matches.is_empty(),
        ));
        if scan.matches.is_empty() {
            notices.extend(grep_zero_notices(&parsed.pattern, filter, scanned, &list));
        }
        let matches: Vec<serde_json::Value> = scan
            .matches
            .iter()
            .map(|m| {
                serde_json::json!({
                    "path": m.path,
                    "lineno": m.lineno,
                    "line": m.line,
                })
            })
            .collect();
        fit_document("matches", matches, notices)
    }
}

/// The zero-match explanation, structured counterpart of
/// [`super::grep`]'s `zero_match_message` (ADR-0150): "nothing was searched"
/// (call-shape) vs "N files scanned, none matched" (real no-match).
fn grep_zero_notices(pattern: &str, filter: &str, scanned: usize, list: &FileList) -> Vec<String> {
    let mut notices = Vec::new();
    if scanned == 0 {
        if list.matched_dirs > 0 {
            notices.push(format!(
                "path filter `{filter}` matched {} director{} but no files — try `{}`",
                list.matched_dirs,
                if list.matched_dirs == 1 { "y" } else { "ies" },
                glob::suggest_files_pattern(filter),
            ));
        } else {
            notices.push(format!(
                "path filter `{filter}` matched no files — nothing was searched"
            ));
        }
    } else {
        notices.push(format!(
            "no matches for `{pattern}` in {scanned} file(s) scanned"
        ));
    }
    if list.out_of_root > 0 {
        notices.push(format!(
            "{} match(es) outside the project root were excluded",
            list.out_of_root
        ));
    }
    if list.capped {
        notices.push(format!(
            "[file walk capped at {MAX_RESULTS} files — narrow `path`]"
        ));
    }
    if list.scan_capped {
        notices.push("[file walk stopped after scanning 100000 entries — narrow `path`]".into());
    }
    notices
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp() -> tempfile::TempDir {
        tempfile::tempdir().expect("temp dir")
    }

    #[tokio::test]
    async fn glob_json_returns_a_files_array() {
        let dir = tmp();
        std::fs::write(dir.path().join("a.rs"), "fn a() {}\n").unwrap();
        std::fs::write(dir.path().join("b.rs"), "fn b() {}\n").unwrap();
        std::fs::write(dir.path().join("c.md"), "doc\n").unwrap();
        let tool = GlobJsonTool::new(dir.path().to_path_buf());
        let out = tool.run(r#"{"pattern":"*.rs"}"#).await.unwrap();
        let doc: serde_json::Value = serde_json::from_str(&out).expect("valid JSON: {out}");
        let files = doc["files"].as_array().expect("files array: {out}");
        assert_eq!(files.len(), 2, "{out}");
        assert!(files.contains(&serde_json::json!("a.rs")), "{out}");
        assert!(doc["notices"].as_array().unwrap().is_empty(), "{out}");
    }

    #[tokio::test]
    async fn glob_json_zero_match_is_an_empty_array_plus_notice() {
        let dir = tmp();
        std::fs::write(dir.path().join("a.rs"), "x\n").unwrap();
        let tool = GlobJsonTool::new(dir.path().to_path_buf());
        let out = tool.run(r#"{"pattern":"*.zzz"}"#).await.unwrap();
        let doc: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert!(doc["files"].as_array().unwrap().is_empty(), "{out}");
        let notices = doc["notices"].as_array().unwrap();
        assert!(
            notices
                .iter()
                .any(|n| n.as_str().unwrap().contains("matched no files")),
            "{out}"
        );
    }

    #[tokio::test]
    async fn glob_json_dir_pattern_suggests_files() {
        let dir = tmp();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/a.rs"), "x\n").unwrap();
        let tool = GlobJsonTool::new(dir.path().to_path_buf());
        // `sr?` matches the directory entry itself (no metachar-free
        // auto-expansion, ADR-0150) — the "matched only directories" shape.
        let out = tool.run(r#"{"pattern":"sr?"}"#).await.unwrap();
        let doc: serde_json::Value = serde_json::from_str(&out).unwrap();
        let notices = doc["notices"].as_array().unwrap();
        assert!(
            notices
                .iter()
                .any(|n| n.as_str().unwrap().contains("try `sr?/*`")),
            "{out}"
        );
    }

    #[tokio::test]
    async fn glob_json_caps_with_a_notice() {
        let dir = tmp();
        for i in 0..(MAX_RESULTS + 5) {
            std::fs::write(dir.path().join(format!("f{i:04}.txt")), "x\n").unwrap();
        }
        let tool = GlobJsonTool::new(dir.path().to_path_buf());
        let out = tool.run(r#"{"pattern":"*.txt"}"#).await.unwrap();
        let doc: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(doc["files"].as_array().unwrap().len(), MAX_RESULTS, "{out}");
        assert!(
            doc["notices"]
                .as_array()
                .unwrap()
                .iter()
                .any(|n| n.as_str().unwrap().contains("narrow")),
            "{out}"
        );
    }

    #[tokio::test]
    async fn grep_json_returns_match_records() {
        let dir = tmp();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/m.rs"), "fn one() {}\nfn two() {}\n").unwrap();
        let tool = GrepJsonTool::new(dir.path().to_path_buf());
        let out = tool.run(r#"{"pattern":"fn two"}"#).await.unwrap();
        let doc: serde_json::Value = serde_json::from_str(&out).unwrap();
        let matches = doc["matches"].as_array().expect("matches array: {out}");
        assert_eq!(matches.len(), 1, "{out}");
        assert_eq!(matches[0]["path"], "src/m.rs", "{out}");
        assert_eq!(matches[0]["lineno"], 2, "{out}");
        assert_eq!(matches[0]["line"], "fn two() {}", "{out}");
    }

    #[tokio::test]
    async fn grep_json_zero_match_names_the_cause() {
        let dir = tmp();
        std::fs::write(dir.path().join("f.txt"), "hello\n").unwrap();
        let tool = GrepJsonTool::new(dir.path().to_path_buf());
        let out = tool.run(r#"{"pattern":"zzz"}"#).await.unwrap();
        let doc: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert!(doc["matches"].as_array().unwrap().is_empty(), "{out}");
        let notices = doc["notices"].as_array().unwrap();
        assert!(
            notices
                .iter()
                .any(|n| n.as_str().unwrap().contains("1 file(s) scanned")),
            "{out}"
        );
    }

    #[tokio::test]
    async fn grep_json_skips_binary_with_a_notice() {
        let dir = tmp();
        std::fs::write(dir.path().join("bin.dat"), b"needle\x00data\n").unwrap();
        let tool = GrepJsonTool::new(dir.path().to_path_buf());
        let out = tool.run(r#"{"pattern":"needle"}"#).await.unwrap();
        let doc: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert!(doc["matches"].as_array().unwrap().is_empty(), "{out}");
        let notices = doc["notices"].as_array().unwrap();
        assert!(
            notices
                .iter()
                .any(|n| n.as_str().unwrap().contains("binary")),
            "{out}"
        );
    }

    #[tokio::test]
    async fn grep_json_path_filter_matching_nothing_says_so() {
        let dir = tmp();
        std::fs::write(dir.path().join("a.rs"), "needle\n").unwrap();
        let tool = GrepJsonTool::new(dir.path().to_path_buf());
        let out = tool
            .run(r#"{"pattern":"needle","path":"**/*.py"}"#)
            .await
            .unwrap();
        let doc: serde_json::Value = serde_json::from_str(&out).unwrap();
        let notices = doc["notices"].as_array().unwrap();
        assert!(
            notices
                .iter()
                .any(|n| n.as_str().unwrap().contains("nothing was searched")),
            "{out}"
        );
    }

    /// A result set too big for the output cap must still be *valid JSON* —
    /// trimmed from the tail with a notice, never cut mid-document (a
    /// byte-cut doc would arrive at the binding layer unparseable).
    #[tokio::test]
    async fn an_oversized_document_is_trimmed_not_byte_cut() {
        let dir = tmp();
        let line = format!("{}\n", "needle ".repeat(30));
        std::fs::write(dir.path().join("big.txt"), line.repeat(400)).unwrap();
        let tool = GrepJsonTool::new(dir.path().to_path_buf());
        let out = tool.run(r#"{"pattern":"needle"}"#).await.unwrap();
        assert!(out.len() <= super::JSON_BUDGET + 256, "len: {}", out.len());
        let doc: serde_json::Value =
            serde_json::from_str(&out).expect("must stay parseable JSON: truncated?");
        let shown = doc["matches"].as_array().unwrap().len();
        assert!(shown < 400, "should have trimmed: {shown}");
        assert!(
            doc["notices"]
                .as_array()
                .unwrap()
                .iter()
                .any(|n| n.as_str().unwrap().contains("of 400 results")),
            "{out}"
        );
    }

    #[tokio::test]
    async fn invalid_input_errors_actionably() {
        let dir = tmp();
        let tool = GlobJsonTool::new(dir.path().to_path_buf());
        let err = tool.run("{}").await.unwrap_err();
        assert!(
            format!("{err:#}").contains("invalid input to glob_json"),
            "{err}"
        );
    }
}
