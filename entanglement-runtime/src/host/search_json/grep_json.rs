//! `grep_json` (ADR-0206): the script-facing structured `grep`, split out of
//! `search_json.rs` along the glob/grep seam.

use super::{fit_document, skip_notices, walk_notices};
use crate::capability::Capability;
use crate::extra_roots::ExtraRootStore;
use crate::host::grep::{compile_regex, scan_matches};
use crate::host::walk::MAX_RESULTS;
use crate::host::{glob, list_files_with_extra_roots, FileList};
use crate::tools::Tool;
use crate::workdir::SessionWorkdirs;
use anyhow::{Context, Result};
use async_trait::async_trait;
use entanglement_core::{ContentPart, SessionId};
use serde::Deserialize;
use std::borrow::Cow;
use std::path::{Path, PathBuf};
use std::sync::Arc;

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
    workdirs: Option<SessionWorkdirs>,
}

impl GrepJsonTool {
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            extra_roots: None,
            workdirs: None,
        }
    }

    /// Print match paths relative to the calling session's workdir (#764).
    pub fn with_workdirs(mut self, workdirs: SessionWorkdirs) -> Self {
        self.workdirs = Some(workdirs);
        self
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
    fn capabilities(&self) -> &'static [Capability] {
        &[Capability::Read]
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
        self.search(input, &self.root).await
    }
    async fn run_for_session(
        &self,
        session: &SessionId,
        _request_id: &str,
        input: &str,
    ) -> Result<Vec<ContentPart>> {
        let shown = SessionWorkdirs::display_root(self.workdirs.as_ref(), session, &self.root);
        Ok(crate::tools::text_parts(self.search(input, &shown).await?))
    }
}

impl GrepJsonTool {
    /// The scan, with match paths relative to `shown`.
    async fn search(&self, input: &str, shown: &Path) -> Result<String> {
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
        let scan = scan_matches(shown, &mut list, &re).await?;
        let mut notices = skip_notices(&scan.skipped, shown);
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
