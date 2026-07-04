//! Host tools that execute against the local filesystem and shell — `read`,
//! `glob`, `grep`, `edit`, and the opt-in `bash`. The read-only trio
//! (`read`/`glob`/`grep`) is covered by ADR-0008; `edit`/`bash` by ADR-0009;
//! [`host_tools`] assembles the **root-contained quartet** (`read`/`glob`/
//! `grep`/`edit`) and a head explicitly opts into [`BashTool`] (gated by
//! `ENTANGLEMENT_ENABLE_BASH`) — see ADR-0010.
//!
//! Each tool is constructed with a working-directory `root`; model-supplied
//! paths resolve against it and are **rejected on `..` escape** (lexical only
//! for now — no symlink defense yet). Output is byte-capped so a runaway
//! listing or huge file can't silently consume the context window. `bash` runs
//! the command rooted at `root` but otherwise inherits the engine process's
//! full privileges — unsandboxed by design (ADR-0009); the opt-in gate plus
//! permission profiles are the only controls (ADR-0010).

use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::Deserialize;

use crate::tools::{Tool, ToolRegistry};

/// Hard cap on a single tool's textual output, in bytes. Larger output is
/// truncated with a notice. Picked generously below the context budget so a
/// normal source file fits, but a minified bundle or huge directory listing
/// can't blow the window. See ADR-0008.
pub const MAX_OUTPUT_BYTES: usize = 32 * 1024;

/// Default line ceiling for [`ReadTool`] when the model doesn't pass `limit`.
const READ_DEFAULT_LIMIT: usize = 2000;

/// Cap on how many paths `glob` returns and how many matches `grep` reports —
/// bounds the work + output for pathologically large trees.
const MAX_RESULTS: usize = 1000;

// ┗━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// ┃ Shared helpers
// ┗━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

/// Resolve `rel` against `root`, rejecting paths that escape the root via `..`
/// (and absolute paths that don't live under it). Lexical only — symlinks can
/// still point outside, which is accepted for now (ADR-0008).
fn resolve_under_root(root: &Path, rel: &str) -> Result<PathBuf> {
    let joined = if Path::new(rel).is_absolute() {
        PathBuf::from(rel)
    } else {
        root.join(rel)
    };
    let mut norm = PathBuf::new();
    for comp in joined.components() {
        match comp {
            Component::CurDir => {}
            Component::ParentDir => {
                if !norm.pop() {
                    return Err(anyhow::anyhow!("path escapes working directory: {rel}"));
                }
            }
            other => norm.push(other.as_os_str()),
        }
    }
    if !norm.starts_with(root) {
        return Err(anyhow::anyhow!("path escapes working directory: {rel}"));
    }
    Ok(norm)
}

/// Cap `s` at [`MAX_OUTPUT_BYTES`] on a UTF-8 boundary, appending a notice of
/// the original size so the model knows data was dropped.
fn truncate_output(s: String) -> String {
    if s.len() <= MAX_OUTPUT_BYTES {
        return s;
    }
    let mut cut = MAX_OUTPUT_BYTES;
    while !s.is_char_boundary(cut) {
        cut -= 1;
    }
    let mut out = String::from(&s[..cut]);
    out.push_str(&format!("\n... [truncated: {} bytes total]", s.len()));
    out
}

/// Enumerate files under `root` matching `pattern` (a glob relative to root),
/// yielding display paths relative to root. Skips directories and unreadable
/// entries. Bounds the walk at [`MAX_RESULTS`] paths.
fn list_files(root: &Path, pattern: &str) -> Result<Vec<PathBuf>> {
    // The glob crate is synchronous; root is absolute so joining yields an
    // absolute glob. Brief blocking IO on a local repo is accepted (ADR-0008).
    let abs = root.join(pattern).to_string_lossy().into_owned();
    let entries = glob::glob(&abs).with_context(|| format!("invalid glob: {pattern}"))?;
    let mut out = Vec::new();
    for entry in entries {
        let p = match entry {
            Ok(p) => p,
            Err(_) => continue,
        };
        // Files only — mirrors what a coding agent wants to enumerate/read.
        if std::fs::metadata(&p).map(|m| m.is_file()).unwrap_or(false) {
            out.push(p);
            if out.len() >= MAX_RESULTS {
                break;
            }
        }
    }
    Ok(out)
}

// ┗━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// ┃ read
// ┗━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

/// `read` — read a UTF-8 text file relative to the working directory, returned
/// as `{lineno}: {line}` so the model can address ranges precisely.
pub struct ReadTool {
    root: PathBuf,
}

impl ReadTool {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
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
        let text = String::from_utf8(bytes)
            .with_context(|| format!("{} is not valid UTF-8", parsed.path))?;
        let offset = parsed.offset.unwrap_or(1).max(1);
        let limit = parsed.limit.unwrap_or(READ_DEFAULT_LIMIT);
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

// ┗━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// ┃ glob
// ┗━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

/// `glob` — list files matching a glob pattern (e.g. `**/*.rs`), paths
/// relative to the working directory.
pub struct GlobTool {
    root: PathBuf,
}

impl GlobTool {
    pub fn new(root: PathBuf) -> Self {
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
        let paths = list_files(&self.root, &parsed.pattern)?;
        let mut out = String::new();
        for p in paths {
            let rel = p.strip_prefix(&self.root).unwrap_or(&p);
            out.push_str(&rel.to_string_lossy());
            out.push('\n');
        }
        Ok(truncate_output(out))
    }
}

// ┗━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// ┃ grep
// ┗━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

/// `grep` — search file contents for a regex. Returns matching lines as
/// `path:lineno:line`. An optional `path` glob filters which files to search
/// (default: all files under the working directory).
pub struct GrepTool {
    root: PathBuf,
}

impl GrepTool {
    pub fn new(root: PathBuf) -> Self {
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
        let re = regex::Regex::new(&parsed.pattern)
            .with_context(|| format!("invalid regex: {}", parsed.pattern))?;
        let filter = parsed.path.as_deref().unwrap_or("**/*");
        let paths = list_files(&self.root, filter)?;
        let mut out = String::new();
        let mut matches = 0usize;
        for p in paths {
            // Bound per-file work: skip files far larger than the output cap.
            let len = match std::fs::metadata(&p) {
                Ok(m) => m.len() as usize,
                Err(_) => continue,
            };
            if len > MAX_OUTPUT_BYTES * 4 {
                continue;
            }
            let bytes = match std::fs::read(&p) {
                Ok(b) => b,
                Err(_) => continue,
            };
            // Skip non-UTF-8 (binary) files silently.
            let Ok(text) = std::str::from_utf8(&bytes) else {
                continue;
            };
            let rel = p.strip_prefix(&self.root).unwrap_or(&p);
            for (i, line) in text.lines().enumerate() {
                if re.is_match(line) {
                    out.push_str(&format!("{}:{}:{}\n", rel.display(), i + 1, line));
                    matches += 1;
                    if matches >= MAX_RESULTS {
                        break;
                    }
                }
            }
            if matches >= MAX_RESULTS {
                break;
            }
        }
        Ok(truncate_output(out))
    }
}

// ┗━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// ┃ edit
// ┗━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

/// `edit` — exact-string search/replace inside a file under the working
/// directory (mirrors opencode's `edit`). `oldString == ""` creates a new file
/// with `newString` as content (refused if the path already exists); otherwise
/// `oldString` must appear exactly once unless `replaceAll` is set. Paths
/// escape the root via `..` are rejected by [`resolve_under_root`].
pub struct EditTool {
    root: PathBuf,
}

impl EditTool {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct EditInput {
    path: String,
    old_string: String,
    new_string: String,
    #[serde(default)]
    replace_all: Option<bool>,
}

#[async_trait]
impl Tool for EditTool {
    fn name(&self) -> &'static str {
        "edit"
    }
    fn description(&self) -> &str {
        "Edit a file under the working directory by exact-string search/replace, \
         or create it. `oldString` must match exactly once (pass `replaceAll` to \
         substitute every occurrence). An empty `oldString` creates the file with \
         `newString` as content (refused if it already exists)."
    }
    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "File path relative to the working directory, or an absolute path inside it."
                },
                "oldString": {
                    "type": "string",
                    "description": "Exact text to find. Empty string creates a new file (refused if the file exists)."
                },
                "newString": {
                    "type": "string",
                    "description": "Text to replace `oldString` with (or the new file's content when creating)."
                },
                "replaceAll": {
                    "type": "boolean",
                    "description": "Replace every occurrence of `oldString` (default false). Required when `oldString` is not unique."
                }
            },
            "required": ["path", "oldString", "newString"]
        })
    }
    async fn run(&self, input: &str) -> Result<String> {
        let parsed: EditInput = serde_json::from_str(input).context(
            "invalid input to edit: expected {\"path\",\"oldString\",\"newString\",...}",
        )?;
        let full = resolve_under_root(&self.root, &parsed.path)?;

        // Create-file path: empty oldString. Refused if the file already exists
        // so the model can't accidentally clobber a file it meant to modify.
        if parsed.old_string.is_empty() {
            if full.exists() {
                return Err(anyhow::anyhow!(
                    "edit refused: {} already exists (use a non-empty oldString to modify it)",
                    parsed.path
                ));
            }
            if let Some(parent) = full.parent() {
                tokio::fs::create_dir_all(parent)
                    .await
                    .with_context(|| format!("creating parent dirs for {}", parsed.path))?;
            }
            let bytes = parsed.new_string.len();
            tokio::fs::write(&full, &parsed.new_string)
                .await
                .with_context(|| format!("creating {}", parsed.path))?;
            return Ok(format!("created {} ({} bytes)", parsed.path, bytes));
        }

        let bytes = tokio::fs::read(&full)
            .await
            .with_context(|| format!("reading {}", parsed.path))?;
        let text = String::from_utf8(bytes)
            .with_context(|| format!("{} is not valid UTF-8", parsed.path))?;

        let count = text.matches(&parsed.old_string).count();
        if count == 0 {
            return Err(anyhow::anyhow!(
                "edit failed: oldString not found in {}",
                parsed.path
            ));
        }
        let replace_all = parsed.replace_all.unwrap_or(false);
        if count > 1 && !replace_all {
            return Err(anyhow::anyhow!(
                "edit failed: oldString appears {count} times in {} — pass replaceAll=true or make oldString more specific",
                parsed.path
            ));
        }

        let new_text = if replace_all {
            text.replace(&parsed.old_string, &parsed.new_string)
        } else {
            text.replacen(&parsed.old_string, &parsed.new_string, 1)
        };
        tokio::fs::write(&full, &new_text)
            .await
            .with_context(|| format!("writing {}", parsed.path))?;
        let plural = if count == 1 { "" } else { "es" };
        Ok(format!(
            "edited {} ({} match{} replaced)",
            parsed.path, count, plural
        ))
    }
}

// ┗━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// ┃ bash
// ┗━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

/// Default per-command timeout when the model omits `timeout`. Matches
/// opencode's Bash default and is short enough to keep a hung command from
/// stalling a turn. See ADR-0009.
const BASH_DEFAULT_TIMEOUT_SECS: u64 = 120;
/// Hard ceiling on the model-supplied `timeout`. Prevents a runaway model from
/// pinning a session for tens of minutes. See ADR-0009.
const BASH_MAX_TIMEOUT_SECS: u64 = 600;

/// `bash` — run a command line under `sh -c` rooted at the working directory.
/// Captures stdout + stderr and the exit code. A per-call `timeout` (seconds,
/// default 120, capped at 600) kills the process on expiry. Output is run
/// through [`truncate_output`].
///
/// Runs with the engine process's full privileges — `root` only sets the cwd,
/// it is **not** a sandbox. Permission profiles gate whether `bash` runs at
/// all; true sandboxing is deferred (ADR-0009).
pub struct BashTool {
    root: PathBuf,
}

impl BashTool {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct BashInput {
    command: String,
    #[serde(default)]
    timeout: Option<u64>,
}

/// Format captured stdout/stderr + exit code into the model-facing string.
/// Separates streams so the model can tell command output apart from
/// diagnostics, and prefixes the exit code so non-zero failures are obvious.
fn format_bash_output(code: Option<i32>, stdout: &[u8], stderr: &[u8]) -> String {
    let code = code.unwrap_or(-1);
    let mut s = String::new();
    s.push_str(&format!("[exit {code}]\n"));
    if !stdout.is_empty() {
        s.push_str(&String::from_utf8_lossy(stdout));
        if !s.ends_with('\n') {
            s.push('\n');
        }
    }
    if !stderr.is_empty() {
        s.push_str("[stderr]\n");
        s.push_str(&String::from_utf8_lossy(stderr));
        if !s.ends_with('\n') {
            s.push('\n');
        }
    }
    truncate_output(s)
}

#[async_trait]
impl Tool for BashTool {
    fn name(&self) -> &'static str {
        "bash"
    }
    fn description(&self) -> &str {
        "Run a shell command line under `sh -c` rooted at the working directory. \
         Captures stdout, stderr, and the exit code. Optional `timeout` in \
         seconds (default 120, capped at 600) kills the process on expiry."
    }
    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "command": {
                    "type": "string",
                    "description": "Shell command to execute (passed to `sh -c`)."
                },
                "timeout": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "Maximum seconds to let the command run before it is killed (default 120, capped at 600)."
                }
            },
            "required": ["command"]
        })
    }
    async fn run(&self, input: &str) -> Result<String> {
        let parsed: BashInput = serde_json::from_str(input)
            .context("invalid input to bash: expected {\"command\": string, ...}")?;
        let secs = parsed
            .timeout
            .unwrap_or(BASH_DEFAULT_TIMEOUT_SECS)
            .clamp(1, BASH_MAX_TIMEOUT_SECS);
        let dur = std::time::Duration::from_secs(secs);

        // `kill_on_drop(true)` is the cleanup guarantee: if `wait_with_output`
        // is dropped by a timeout (or a panic, or task cancellation), the child
        // is reaped instead of orphaned. The child is moved into the timed
        // future, so on timeout it is dropped here and killed.
        let child = tokio::process::Command::new("sh")
            .arg("-c")
            .arg(&parsed.command)
            .current_dir(&self.root)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .with_context(|| "spawning bash command")?;

        match tokio::time::timeout(dur, child.wait_with_output()).await {
            Ok(Ok(output)) => Ok(format_bash_output(
                output.status.code(),
                &output.stdout,
                &output.stderr,
            )),
            Ok(Err(e)) => Err(anyhow::anyhow!("bash io error: {e}")),
            Err(_) => Ok(format!("[killed: timed out after {secs}s]")),
        }
    }
}

// ┗━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// ┃ Registry builder
// ┗━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

/// Build a [`ToolRegistry`] with the root-contained host quartet (`read`,
/// `glob`, `grep`, `edit`) rooted at `root`. `bash` is intentionally **not**
/// registered here — it runs arbitrary code with the engine's full privileges
/// (ADR-0009), so a head must opt into it explicitly by registering
/// [`BashTool`] (e.g. when `ENTANGLEMENT_ENABLE_BASH=1`). See ADR-0010.
///
/// A head (e.g. the `skutter` binary) passes its working directory; the engine
/// then advertises these tools to the model and the session dispatches model
/// calls to them under the active permission profile.
pub fn host_tools(root: PathBuf) -> ToolRegistry {
    let mut reg = ToolRegistry::new();
    reg.register(ReadTool::new(root.clone()));
    reg.register(GlobTool::new(root.clone()));
    reg.register(GrepTool::new(root.clone()));
    reg.register(EditTool::new(root));
    reg
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    /// Scratch temp dir per test, removed on drop. Avoids pulling `tempfile`
    /// as a dev-dep.
    struct TempDir {
        path: PathBuf,
    }
    impl TempDir {
        fn new() -> TempDir {
            let id = COUNTER.fetch_add(1, Ordering::SeqCst);
            let path =
                std::env::temp_dir().join(format!("entanglement-host-{}-{id}", std::process::id()));
            fs::create_dir_all(&path).unwrap();
            TempDir { path }
        }
        fn join(&self, rel: &str) -> PathBuf {
            let p = self.path.join(rel);
            if let Some(parent) = p.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            p
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    #[tokio::test]
    async fn read_returns_lines_with_numbers() {
        let dir = TempDir::new();
        fs::write(dir.join("a.txt"), "alpha\nbeta\n").unwrap();
        let tool = ReadTool::new(dir.path.clone());
        let out = tool.run(r#"{"path":"a.txt"}"#).await.unwrap();
        assert!(out.contains("1: alpha"), "got: {out}");
        assert!(out.contains("2: beta"), "got: {out}");
    }

    #[tokio::test]
    async fn read_respects_offset_and_limit() {
        let dir = TempDir::new();
        fs::write(dir.join("n.txt"), "one\ntwo\nthree\nfour\nfive\nsix\n").unwrap();
        let tool = ReadTool::new(dir.path.clone());
        let out = tool
            .run(r#"{"path":"n.txt","offset":2,"limit":2}"#)
            .await
            .unwrap();
        assert!(out.contains("2: two"), "got: {out}");
        assert!(out.contains("3: three"), "got: {out}");
        assert!(!out.contains("one"));
        assert!(!out.contains("four"));
    }

    #[tokio::test]
    async fn read_rejects_path_escape() {
        let dir = TempDir::new();
        fs::write(dir.join("inside.txt"), "ok\n").unwrap();
        let tool = ReadTool::new(dir.path.clone());
        let res = tool.run(r#"{"path":"../escape.txt"}"#).await;
        assert!(res.is_err(), "expected escape to be rejected");
        let err = res.unwrap_err().to_string();
        assert!(err.contains("escapes"), "got: {err}");
    }

    #[tokio::test]
    async fn read_missing_file_errors() {
        let dir = TempDir::new();
        let tool = ReadTool::new(dir.path.clone());
        let res = tool.run(r#"{"path":"nope.txt"}"#).await;
        assert!(res.is_err());
    }

    #[tokio::test]
    async fn glob_lists_matching_files_relative() {
        let dir = TempDir::new();
        fs::write(dir.join("src/a.rs"), "x\n").unwrap();
        fs::write(dir.join("src/b.rs"), "x\n").unwrap();
        fs::write(dir.join("src/c.txt"), "x\n").unwrap();
        let tool = GlobTool::new(dir.path.clone());
        let out = tool.run(r#"{"pattern":"**/*.rs"}"#).await.unwrap();
        assert!(out.contains("src/a.rs"), "got: {out}");
        assert!(out.contains("src/b.rs"), "got: {out}");
        assert!(!out.contains("c.txt"), "got: {out}");
    }

    #[tokio::test]
    async fn grep_returns_matches_with_line_numbers() {
        let dir = TempDir::new();
        fs::write(dir.join("src/m.rs"), "fn alpha() {}\nfn beta() {}\n").unwrap();
        fs::write(dir.join("src/other.md"), "# alpha\n").unwrap();
        let tool = GrepTool::new(dir.path.clone());
        let out = tool.run(r#"{"pattern":"alpha"}"#).await.unwrap();
        assert!(out.contains("src/m.rs:1:"), "got: {out}");
        assert!(out.contains("src/other.md:1:"), "got: {out}");
        // beta line must not appear (no match).
        assert!(!out.contains("beta"), "got: {out}");
    }

    #[tokio::test]
    async fn grep_path_filter_restricts_files() {
        let dir = TempDir::new();
        fs::write(dir.join("src/m.rs"), "rare_token\n").unwrap();
        fs::write(dir.join("docs/m.md"), "rare_token\n").unwrap();
        let tool = GrepTool::new(dir.path.clone());
        let out = tool
            .run(r#"{"pattern":"rare_token","path":"**/*.rs"}"#)
            .await
            .unwrap();
        assert!(out.contains("src/m.rs:1:"), "got: {out}");
        assert!(!out.contains("docs/m.md"), "got: {out}");
    }

    #[tokio::test]
    async fn edit_creates_file_when_old_string_empty() {
        let dir = TempDir::new();
        let tool = EditTool::new(dir.path.clone());
        let out = tool
            .run(r#"{"path":"new.txt","oldString":"","newString":"hello\n"}"#)
            .await
            .unwrap();
        assert!(out.contains("created"), "got: {out}");
        let on_disk = std::fs::read_to_string(dir.join("new.txt")).unwrap();
        assert_eq!(on_disk, "hello\n");
    }

    #[tokio::test]
    async fn edit_create_refused_when_file_exists() {
        let dir = TempDir::new();
        fs::write(dir.join("exists.txt"), "x\n").unwrap();
        let tool = EditTool::new(dir.path.clone());
        let res = tool
            .run(r#"{"path":"exists.txt","oldString":"","newString":"y"}"#)
            .await;
        assert!(res.is_err(), "expected create refusal");
    }

    #[tokio::test]
    async fn edit_replaces_unique_match() {
        let dir = TempDir::new();
        fs::write(dir.join("a.txt"), "alpha\nbeta\n").unwrap();
        let tool = EditTool::new(dir.path.clone());
        let out = tool
            .run(r#"{"path":"a.txt","oldString":"beta","newString":"BETA"}"#)
            .await
            .unwrap();
        assert!(out.contains("1 match replaced"), "got: {out}");
        let on_disk = std::fs::read_to_string(dir.join("a.txt")).unwrap();
        assert_eq!(on_disk, "alpha\nBETA\n");
    }

    #[tokio::test]
    async fn edit_rejects_ambiguous_match_without_replace_all() {
        let dir = TempDir::new();
        fs::write(dir.join("a.txt"), "dup\ndup\n").unwrap();
        let tool = EditTool::new(dir.path.clone());
        let res = tool
            .run(r#"{"path":"a.txt","oldString":"dup","newString":"x"}"#)
            .await;
        let err = res.unwrap_err().to_string();
        assert!(err.contains("2 times"), "got: {err}");
    }

    #[tokio::test]
    async fn edit_replace_all_substitutes_every_occurrence() {
        let dir = TempDir::new();
        fs::write(dir.join("a.txt"), "dup\ndup\ndup\n").unwrap();
        let tool = EditTool::new(dir.path.clone());
        let out = tool
            .run(r#"{"path":"a.txt","oldString":"dup","newString":"x","replaceAll":true}"#)
            .await
            .unwrap();
        assert!(out.contains("3 matches replaced"), "got: {out}");
        let on_disk = std::fs::read_to_string(dir.join("a.txt")).unwrap();
        assert_eq!(on_disk, "x\nx\nx\n");
    }

    #[tokio::test]
    async fn edit_errors_when_old_string_not_found() {
        let dir = TempDir::new();
        fs::write(dir.join("a.txt"), "hello\n").unwrap();
        let tool = EditTool::new(dir.path.clone());
        let res = tool
            .run(r#"{"path":"a.txt","oldString":"nope","newString":"x"}"#)
            .await;
        let err = res.unwrap_err().to_string();
        assert!(err.contains("not found"), "got: {err}");
    }

    #[tokio::test]
    async fn edit_rejects_path_escape() {
        let dir = TempDir::new();
        let tool = EditTool::new(dir.path.clone());
        let res = tool
            .run(r#"{"path":"../out.txt","oldString":"","newString":"x"}"#)
            .await;
        assert!(res.is_err(), "expected escape rejection");
    }

    #[tokio::test]
    async fn bash_captures_stdout_and_exit_code() {
        let dir = TempDir::new();
        let tool = BashTool::new(dir.path.clone());
        let out = tool.run(r#"{"command":"printf 'hi\\n'"}"#).await.unwrap();
        assert!(out.contains("[exit 0]"), "got: {out}");
        assert!(out.contains("hi"), "got: {out}");
    }

    #[tokio::test]
    async fn bash_reports_nonzero_exit_and_stderr() {
        let dir = TempDir::new();
        let tool = BashTool::new(dir.path.clone());
        let out = tool
            .run(r#"{"command":"echo oops 1>&2; exit 7"}"#)
            .await
            .unwrap();
        assert!(out.contains("[exit 7]"), "got: {out}");
        assert!(out.contains("[stderr]"), "got: {out}");
        assert!(out.contains("oops"), "got: {out}");
    }

    #[tokio::test]
    async fn bash_runs_rooted_at_working_directory() {
        let dir = TempDir::new();
        fs::write(dir.join("marker.txt"), "here\n").unwrap();
        let tool = BashTool::new(dir.path.clone());
        let out = tool.run(r#"{"command":"ls marker.txt"}"#).await.unwrap();
        assert!(out.contains("marker.txt"), "got: {out}");
    }

    #[tokio::test]
    async fn bash_kills_on_timeout() {
        let dir = TempDir::new();
        let tool = BashTool::new(dir.path.clone());
        // `sleep 5` under a 1s timeout must be killed, not awaited.
        let out = tool
            .run(r#"{"command":"sleep 5","timeout":1}"#)
            .await
            .unwrap();
        assert!(out.contains("[killed: timed out after 1s]"), "got: {out}");
    }

    #[tokio::test]
    async fn bash_clamps_oversize_timeout_to_max() {
        // A timeout over the cap is clamped down; the command still runs and
        // exits normally (well under the clamped deadline). This guards the
        // `.clamp(1, MAX)` path without needing to wait the full cap.
        let dir = TempDir::new();
        let tool = BashTool::new(dir.path.clone());
        let out = tool
            .run(r#"{"command":"true","timeout":99999}"#)
            .await
            .unwrap();
        assert!(out.contains("[exit 0]"), "got: {out}");
    }

    #[test]
    fn truncate_caps_large_output_with_notice() {
        let big = "x".repeat(MAX_OUTPUT_BYTES + 5000);
        let out = truncate_output(big);
        assert!(out.len() < MAX_OUTPUT_BYTES + 200, "got len {}", out.len());
        assert!(
            out.contains("[truncated:"),
            "got: ...{}",
            &out[out.len().saturating_sub(80)..]
        );
    }

    #[test]
    fn host_tools_registers_root_contained_quartet_without_bash() {
        let dir = TempDir::new();
        let reg = host_tools(dir.path.clone());
        let specs = reg.specs();
        let names: Vec<&str> = specs.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"read"), "{names:?}");
        assert!(names.contains(&"glob"), "{names:?}");
        assert!(names.contains(&"grep"), "{names:?}");
        assert!(names.contains(&"edit"), "{names:?}");
        // bash is opt-in at the head level (ADR-0010) — never auto-registered.
        assert!(!names.contains(&"bash"), "{names:?}");
        // Schemas are non-empty objects with a `properties` field.
        for s in &specs {
            assert!(
                s.schema.get("properties").is_some(),
                "{} missing properties",
                s.name
            );
        }
    }
}
