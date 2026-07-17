//! `call` — direct process execution (argv, **no shell**) with auto-tailed
//! output. Complements `bash` (ADR-0009): what the model sends as `command` +
//! `args` execs verbatim — no `sh -c`, so no pipes, globbing, `$VAR` expansion,
//! or metacharacter injection. A fixed argv is auditable, which is why a profile
//! may reasonably `Allow` `call` while keeping `bash` at `Ask`/`Deny`. Runs
//! unsandboxed with the engine's full privileges, but — unlike `bash` — is
//! registered unconditionally, independent of `ENTANGLEMENT_ENABLE_BASH`
//! (ADR-0093, supersedes ADR-0010 §3/ADR-0045 §3 for `call`); per-profile
//! permission (`Allow`/`Ask`/`Deny`) is the actual dispatch gate.
//!
//! `input_file`/`output_file` (ADR-0092, #381) give a call a durable trace:
//! `input_file` is read before spawn and piped to the child's stdin (no
//! `input_file` → stdin is explicitly closed, not inherited from the engine);
//! `output_file` gets the full untruncated stdout, with a `<output_file>.stderr`
//! sibling always written alongside. With no `output_file` an artifact is still
//! written, auto-named under a runtime-owned per-project **scratch dir** outside
//! the repo (`session_store::scratch_dir`, wired via [`CallTool::with_scratch_base`])
//! so it neither pollutes the workdir nor re-triggers the definitions watcher;
//! its absolute path is named in the result header. Standalone/test constructors
//! with no scratch base fall back to `<root>/.entanglement/tmp/call-output/`.
//!
//! Like `bash`, an opt-in bubblewrap confinement layer is available
//! (ADR-0104, [`SandboxPolicy`]).

use super::exec::{own_process_group, wait_or_kill_group, ExecOutcome};
use super::sandbox::{self, SandboxPolicy};
use super::{resolve_under_root, truncate_output};
use crate::tools::Tool;
use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::Deserialize;
use std::borrow::Cow;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::io::AsyncWriteExt;

const MAX_CALL_TIMEOUT_SECONDS: u64 = 600;
const DEFAULT_TAIL: u32 = 30;

/// Per-process counter disambiguating default artifact filenames across
/// concurrent `call` invocations sharing one pid.
static CALL_SEQ: AtomicU64 = AtomicU64::new(0);

pub struct CallTool {
    root: std::path::PathBuf,
    /// Where a default (no `output_file`) artifact is written: a runtime-owned
    /// per-project scratch dir *outside* the repo (`session_store::scratch_dir`)
    /// so a routine `call` neither pollutes the workdir nor re-triggers the
    /// definitions watcher. `None` falls back to `<root>/.entanglement/tmp` for
    /// the standalone/test constructors that have no session context.
    scratch_base: Option<PathBuf>,
    /// Env vars scrubbed from the child before spawn — the provider API keys
    /// (`ZAI_API_KEY`, …) so a model-authored binary can't read the engine's
    /// credentials (#164). The no-shell design doesn't help here: a plain
    /// `env`/`printenv` still inherits them. Empty by default; wired from the
    /// catalog.
    secret_env: Vec<String>,
    /// Optional bubblewrap confinement (ADR-0104). Defaults to
    /// [`SandboxPolicy::none()`] — unsandboxed, unchanged from before this
    /// existed.
    sandbox: SandboxPolicy,
    /// Approval-gated out-of-root `workdir` (ADR-0107).
    extra_roots: Option<std::sync::Arc<crate::extra_roots::ExtraRootStore>>,
}

impl CallTool {
    pub fn new(root: std::path::PathBuf) -> Self {
        Self {
            root,
            scratch_base: None,
            secret_env: Vec::new(),
            sandbox: SandboxPolicy::none(),
            extra_roots: None,
        }
    }

    /// Permit an approved out-of-root `workdir` (ADR-0107).
    pub fn with_extra_roots(
        mut self,
        extra: std::sync::Arc<crate::extra_roots::ExtraRootStore>,
    ) -> Self {
        self.extra_roots = Some(extra);
        self
    }

    /// Write default (no `output_file`) artifacts under `scratch_base` — the
    /// per-project scratch dir from `session_store::scratch_dir`, outside the
    /// repo and every watched tree.
    pub fn with_scratch_base(mut self, scratch_base: PathBuf) -> Self {
        self.scratch_base = Some(scratch_base);
        self
    }

    /// Scrub `vars` from the spawned command's environment (provider API keys).
    pub fn with_secret_env(mut self, vars: Vec<String>) -> Self {
        self.secret_env = vars;
        self
    }

    /// Confine every spawned command under `policy` (ADR-0104).
    pub fn with_sandbox(mut self, policy: SandboxPolicy) -> Self {
        self.sandbox = policy;
        self
    }
}

#[derive(Deserialize)]
struct CallInput {
    command: String,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default = "default_tail")]
    tail: u32,
    #[serde(default)]
    timeout: Option<u64>,
    #[serde(default)]
    input_file: Option<String>,
    #[serde(default)]
    output_file: Option<String>,
    /// Optional per-call working directory, resolved under the tool root.
    #[serde(default)]
    workdir: Option<String>,
}

fn default_tail() -> u32 {
    DEFAULT_TAIL
}

/// Where the full raw stdout (and its `.stderr` sibling) get written — either
/// the model-requested `output_file` or an auto-named default artifact.
struct OutputTarget {
    stdout_abs: PathBuf,
    stderr_abs: PathBuf,
    /// Root-relative stdout path, named in the result header.
    rel: String,
    /// Explicit (`output_file` given) → a write failure is a hard error.
    /// Default (auto-named) → a write failure is best-effort (log + notice).
    explicit: bool,
}

fn stderr_sibling(path: &Path) -> PathBuf {
    let mut s = path.as_os_str().to_os_string();
    s.push(".stderr");
    PathBuf::from(s)
}

fn resolve_output_target(
    root: &Path,
    scratch_base: Option<&Path>,
    output_file: &Option<String>,
) -> Result<OutputTarget> {
    match output_file {
        Some(rel) => {
            let stdout_abs = resolve_under_root(root, rel)?;
            let stderr_abs = stderr_sibling(&stdout_abs);
            Ok(OutputTarget {
                stdout_abs,
                stderr_abs,
                rel: rel.clone(),
                explicit: true,
            })
        }
        None => {
            let seq = CALL_SEQ.fetch_add(1, Ordering::Relaxed);
            let name = format!("call-output/call-{}-{seq}.stdout", std::process::id());
            // Default artifacts go to the runtime-owned per-project scratch dir
            // (outside the repo). The header names the absolute path since it is
            // no longer root-relative. Standalone/test constructors with no
            // scratch base fall back to the legacy in-repo location.
            let stdout_abs = match scratch_base {
                Some(base) => base.join(&name),
                None => root.join(".entanglement/tmp").join(&name),
            };
            let stderr_abs = stderr_sibling(&stdout_abs);
            let rel = stdout_abs.display().to_string();
            Ok(OutputTarget {
                stdout_abs,
                stderr_abs,
                rel,
                explicit: false,
            })
        }
    }
}

/// Write the full raw stdout/stderr to `target`, creating missing parent dirs.
/// An explicit (`output_file`) failure propagates as a hard error — it was
/// requested. A default-artifact failure is logged and returned as a degraded
/// notice instead, so an unrelated disk issue can't fail a command that would
/// otherwise have succeeded.
async fn persist_output(
    target: &OutputTarget,
    stdout: &[u8],
    stderr: &[u8],
) -> Result<Option<String>> {
    let result: Result<()> = async {
        if let Some(parent) = target.stdout_abs.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .context("creating output_file parent dirs")?;
        }
        tokio::fs::write(&target.stdout_abs, stdout)
            .await
            .context("writing output_file")?;
        tokio::fs::write(&target.stderr_abs, stderr)
            .await
            .context("writing output_file stderr sibling")?;
        Ok(())
    }
    .await;

    match result {
        Ok(()) => Ok(None),
        Err(e) if target.explicit => Err(e),
        Err(e) => {
            tracing::warn!("call: failed to write default output artifact: {e:#}");
            Ok(Some(format!("[output artifact write failed: {e:#}]\n")))
        }
    }
}

#[async_trait]
impl Tool for CallTool {
    fn name(&self) -> Cow<'static, str> {
        Cow::Borrowed("call")
    }
    fn description(&self) -> &str {
        "Execute a binary directly (argv, NO shell) rooted at the working \
         directory (or `workdir`, if given): `command` + `args` are passed \
         verbatim to exec — no `sh -c`, \
         so pipes, globbing, `$VAR` expansion, and metacharacters are NOT \
         interpreted. Prefer this over `bash` for a fixed command. Output is \
         tailed to the last `tail` lines per stream (default 30 — command value \
         concentrates at the end); pass `tail=0` deliberately for full output \
         (still byte-capped). Returns `[exit N]`, tailed stdout, and a tailed \
         `[stderr]` block. The full untruncated output is always persisted to a \
         file — `output_file` if given, else an auto-named default artifact — \
         named in the result header; `input_file` pipes a file to the child's \
         stdin (omitted → stdin is closed, not inherited). Pass `workdir` to \
         run in a subdirectory (validated under root) instead of reaching for \
         `bash` just to `cd` first."
    }
    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "command": {
                    "type": "string",
                    "description": "Binary to execute (looked up on PATH). Run \
                        directly, not through a shell."
                },
                "args": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Arguments passed verbatim as argv — no shell \
                        interpretation. Default []."
                },
                "tail": {
                    "type": "integer",
                    "minimum": 0,
                    "description": "Keep only the last N lines of each stream \
                        (default 30). Use 0 for full output (still byte-capped)."
                },
                "timeout": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "Timeout in seconds (default 120, capped at 600)."
                },
                "input_file": {
                    "type": "string",
                    "description": "Path (relative to the root, not `workdir`) of \
                        a file whose content is piped to the child's stdin. \
                        Omitted → stdin is closed, not inherited."
                },
                "output_file": {
                    "type": "string",
                    "description": "Path (relative to the root, not `workdir`) to \
                        write the full, untruncated raw stdout to (missing \
                        parent dirs are created); a `<output_file>.stderr` \
                        sibling is always written alongside. Omitted → an \
                        artifact is still written to a runtime-owned scratch dir \
                        outside the project, its absolute path named in the result."
                },
                "workdir": {
                    "type": "string",
                    "description": "Working directory for this call, relative to \
                        the root (must stay under it). Defaults to the root."
                }
            },
            "required": ["command"]
        })
    }
    async fn run(&self, input: &str) -> Result<String> {
        let parsed: CallInput = serde_json::from_str(input)
            .context("invalid input to call: expected {\"command\": string, ...}")?;
        let secs = parsed.timeout.unwrap_or(120);
        let dur = std::time::Duration::from_secs(secs.min(MAX_CALL_TIMEOUT_SECONDS));

        // Validate + read input_file, resolve the output target, and resolve
        // workdir *before* spawning — a bad path (escape, missing input_file,
        // non-directory workdir) must never launch the child (#381, #386).
        let stdin_data = match &parsed.input_file {
            Some(rel) => {
                let abs = resolve_under_root(&self.root, rel)?;
                Some(
                    tokio::fs::read(&abs)
                        .await
                        .with_context(|| format!("reading input_file `{rel}`"))?,
                )
            }
            None => None,
        };
        let output_target = resolve_output_target(
            &self.root,
            self.scratch_base.as_deref(),
            &parsed.output_file,
        )?;
        let cwd = super::resolve_workdir_or_grant(
            &self.root,
            self.extra_roots.as_deref(),
            "call",
            parsed.workdir.as_deref(),
        )?;

        let mut cmd = sandbox::command(&self.sandbox, &self.root, &parsed.command, &parsed.args);
        cmd.current_dir(&cwd)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            // No `input_file` → close stdin explicitly rather than inherit the
            // engine's real stdin, an unintentional leak until now (#381).
            .stdin(if stdin_data.is_some() {
                std::process::Stdio::piped()
            } else {
                std::process::Stdio::null()
            })
            .kill_on_drop(true);
        // Own process group so a timeout kills the whole tree, not just the
        // direct child (a launched server/pipeline would otherwise orphan — #168).
        own_process_group(&mut cmd);
        for var in &self.secret_env {
            cmd.env_remove(var);
        }
        let mut child = cmd
            .spawn()
            // A missing binary (or non-exec target) surfaces here — return it as
            // tool output, never panic (ADR-0016 clean-error contract).
            .with_context(|| format!("spawning `{}`", parsed.command))?;

        // Feed stdin concurrently with draining stdout/stderr (below) so a
        // chatty child can't deadlock against a full pipe buffer either way.
        let stdin_task = match (child.stdin.take(), stdin_data) {
            (Some(mut stdin), Some(data)) => Some(tokio::spawn(async move {
                let _ = stdin.write_all(&data).await;
                // `stdin` drops here, closing the pipe (EOF) once fully written.
            })),
            _ => None,
        };

        let outcome = wait_or_kill_group(child, dur).await;
        if let Some(t) = stdin_task {
            let _ = t.await;
        }

        match outcome {
            Ok(ExecOutcome::Completed(output)) => {
                let notice = persist_output(&output_target, &output.stdout, &output.stderr).await?;
                Ok(format_call_output(
                    output.status.code(),
                    &output.stdout,
                    &output.stderr,
                    parsed.tail,
                    &output_target.rel,
                    notice,
                ))
            }
            // Return the output buffered before the kill (tailed like a normal
            // result) alongside the notice — the prefix is often the diagnostic
            // the model needs (#169). The artifacts get the same partial bytes.
            Ok(ExecOutcome::TimedOut { stdout, stderr }) => {
                let notice = persist_output(&output_target, &stdout, &stderr).await?;
                Ok(format_call_streams(
                    &format!("[killed: timed out after {secs}s]\n"),
                    &stdout,
                    &stderr,
                    parsed.tail,
                    &output_target.rel,
                    notice,
                ))
            }
            Err(e) => Err(anyhow::anyhow!("call io error: {e}")),
        }
    }
}

/// Keep only the last `tail` lines of `s`. `tail == 0` disables line cutting
/// (the byte cap still applies downstream). When lines are dropped, prepend a
/// self-correction notice (ADR-0016) naming the count and `tail=0` escape hatch.
fn tail_lines(s: &str, tail: u32) -> String {
    if tail == 0 || s.is_empty() {
        return s.to_string();
    }
    let lines: Vec<&str> = s.lines().collect();
    let tail = tail as usize;
    if lines.len() <= tail {
        return s.to_string();
    }
    let omitted = lines.len() - tail;
    let mut out = format!(
        "(… {omitted} earlier lines omitted, tail={tail} — rerun with tail=0 for full output)\n"
    );
    out.push_str(&lines[lines.len() - tail..].join("\n"));
    out.push('\n');
    out
}

/// Assemble `[exit N]` + tailed stdout + a tailed `[stderr]` block, then apply
/// the 32 KiB byte cap (ADR-0008) as the outer bound. The line tail and byte cap
/// are independent limits — either may fire, and the byte-cap notice names the
/// byte limit explicitly.
#[allow(clippy::too_many_arguments)]
fn format_call_output(
    code: Option<i32>,
    stdout: &[u8],
    stderr: &[u8],
    tail: u32,
    output_rel: &str,
    artifact_notice: Option<String>,
) -> String {
    format_call_streams(
        &format!("[exit {}]\n", code.unwrap_or(-1)),
        stdout,
        stderr,
        tail,
        output_rel,
        artifact_notice,
    )
}

/// `header` + tailed stdout + a tailed `[stderr]` block, byte-capped. Shared by
/// the exit path (`[exit N]`) and the timeout path (`[killed: …]`, #169). Also
/// names the durable artifact holding the *full* (untailed) output (#381).
#[allow(clippy::too_many_arguments)]
fn format_call_streams(
    header: &str,
    stdout: &[u8],
    stderr: &[u8],
    tail: u32,
    output_rel: &str,
    artifact_notice: Option<String>,
) -> String {
    let mut out = String::from(header);
    out.push_str(&format!(
        "[output: {output_rel}] [stderr: {output_rel}.stderr]\n"
    ));
    if let Some(notice) = artifact_notice {
        out.push_str(&notice);
    }
    let stdout_str = String::from_utf8_lossy(stdout);
    let stdout_tailed = tail_lines(&stdout_str, tail);
    if !stdout_tailed.is_empty() {
        out.push_str(&stdout_tailed);
    }
    let stderr_str = String::from_utf8_lossy(stderr);
    let stderr_tailed = tail_lines(&stderr_str, tail);
    if !stderr_tailed.is_empty() {
        out.push_str("[stderr]\n");
        out.push_str(&stderr_tailed);
    }
    // Belt-and-suspenders: with tail=0 (or very long lines) the assembled output
    // can still exceed the context budget, so the 32 KiB byte cap
    // ([`MAX_OUTPUT_BYTES`]) remains the outer bound. Its notice
    // (`... [truncated: N bytes total]`) names the byte limit.
    truncate_output(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::MAX_OUTPUT_BYTES;

    static TEST_DIR_COUNTER: AtomicU64 = AtomicU64::new(0);

    /// Isolated per-test root so artifact-writing tests don't collide (and so
    /// their `.entanglement/` litter doesn't accumulate in a shared temp dir).
    struct TempDir {
        path: PathBuf,
    }
    impl TempDir {
        fn new() -> TempDir {
            let id = TEST_DIR_COUNTER.fetch_add(1, Ordering::SeqCst);
            let path =
                std::env::temp_dir().join(format!("entanglement-call-{}-{id}", std::process::id()));
            std::fs::create_dir_all(&path).unwrap();
            TempDir { path }
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    #[test]
    fn default_artifact_goes_to_scratch_base_not_the_repo() {
        let root = TempDir::new();
        let scratch = TempDir::new();
        let target = resolve_output_target(&root.path, Some(&scratch.path), &None).unwrap();
        assert!(!target.explicit);
        assert!(
            target.stdout_abs.starts_with(&scratch.path),
            "default artifact under scratch: {}",
            target.stdout_abs.display()
        );
        assert!(
            !target.stdout_abs.starts_with(&root.path),
            "default artifact must NOT be under the project root: {}",
            target.stdout_abs.display()
        );
        // The header names the absolute scratch path.
        assert_eq!(target.rel, target.stdout_abs.display().to_string());
    }

    #[test]
    fn default_artifact_falls_back_to_repo_without_scratch_base() {
        let root = TempDir::new();
        let target = resolve_output_target(&root.path, None, &None).unwrap();
        assert!(target
            .stdout_abs
            .starts_with(root.path.join(".entanglement/tmp")));
    }

    #[test]
    fn explicit_output_file_stays_contained_to_root() {
        let root = TempDir::new();
        let scratch = TempDir::new();
        let target = resolve_output_target(
            &root.path,
            Some(&scratch.path),
            &Some("out/log.txt".to_string()),
        )
        .unwrap();
        assert!(target.explicit);
        assert!(
            target.stdout_abs.starts_with(&root.path),
            "explicit output_file stays under root: {}",
            target.stdout_abs.display()
        );
        // A path escaping root is still refused.
        assert!(resolve_output_target(
            &root.path,
            Some(&scratch.path),
            &Some("../escape.txt".to_string()),
        )
        .is_err());
    }

    #[test]
    fn tail_keeps_last_n_and_notes_omitted() {
        let body: String = (1..=100).map(|i| format!("line{i}\n")).collect();
        let out = tail_lines(&body, 30);
        assert!(
            out.starts_with(
                "(… 70 earlier lines omitted, tail=30 — rerun with tail=0 for full output)\n"
            ),
            "got: {out}"
        );
        assert!(out.contains("line100"), "keeps the last line: {out}");
        assert!(out.contains("line71"), "keeps 30th-from-end: {out}");
        assert!(!out.contains("line70\n"), "drops the 31st-from-end: {out}");
    }

    #[test]
    fn tail_zero_is_full_output() {
        let body: String = (1..=100).map(|i| format!("line{i}\n")).collect();
        let out = tail_lines(&body, 0);
        assert_eq!(out, body);
        assert!(!out.contains("omitted"), "no notice with tail=0");
    }

    #[test]
    fn tail_under_threshold_is_untouched() {
        let body = "a\nb\nc\n";
        assert_eq!(tail_lines(body, 30), body);
    }

    #[test]
    fn format_renders_exit_and_separate_stderr() {
        let out = format_call_output(Some(2), b"hello\n", b"boom\n", 30, "out.stdout", None);
        assert!(out.starts_with("[exit 2]\n"), "got: {out}");
        assert!(out.contains("hello\n"), "got: {out}");
        assert!(out.contains("[stderr]\nboom\n"), "got: {out}");
        assert!(
            out.contains("[output: out.stdout] [stderr: out.stdout.stderr]"),
            "got: {out}"
        );
    }

    #[test]
    fn format_tails_both_streams_independently() {
        let big: String = (1..=50).map(|i| format!("o{i}\n")).collect();
        let err: String = (1..=50).map(|i| format!("e{i}\n")).collect();
        let out = format_call_output(
            Some(0),
            big.as_bytes(),
            err.as_bytes(),
            5,
            "out.stdout",
            None,
        );
        assert!(
            out.contains("o50") && !out.contains("o40\n"),
            "stdout tailed: {out}"
        );
        assert!(
            out.contains("e50") && !out.contains("e40\n"),
            "stderr tailed: {out}"
        );
        // Two omission notices — one per stream.
        assert_eq!(
            out.matches("earlier lines omitted").count(),
            2,
            "got: {out}"
        );
    }

    #[tokio::test]
    async fn args_are_passed_verbatim_no_shell_interpretation() {
        // `$HOME`, `;`, `&&`, `|` and a glob must reach the binary as literal
        // argv — a shell would expand/split them.
        let root = std::env::temp_dir();
        let tool = CallTool::new(root);
        let payload = "$HOME; rm -rf / && echo x | cat *.rs";
        let input = serde_json::json!({
            "command": "printf",
            "args": ["%s", payload],
        })
        .to_string();
        let out = tool.run(&input).await.unwrap();
        assert!(out.contains("[exit 0]"), "got: {out}");
        assert!(out.contains(payload), "argv must be verbatim, got: {out}");
    }

    #[tokio::test]
    async fn missing_binary_is_clean_error_not_panic() {
        let tool = CallTool::new(std::env::temp_dir());
        let input =
            serde_json::json!({ "command": "definitely-not-a-real-binary-xyz" }).to_string();
        let err = tool.run(&input).await.unwrap_err();
        assert!(
            err.to_string().contains("spawning"),
            "expected a clean spawn error, got: {err}"
        );
    }

    #[tokio::test]
    async fn nonzero_exit_is_rendered() {
        let tool = CallTool::new(std::env::temp_dir());
        // `false` exits 1 with no output.
        let input = serde_json::json!({ "command": "false" }).to_string();
        let out = tool.run(&input).await.unwrap();
        assert!(out.contains("[exit 1]"), "got: {out}");
    }

    #[tokio::test]
    async fn timeout_kills_long_process() {
        let tool = CallTool::new(std::env::temp_dir());
        let input =
            serde_json::json!({ "command": "sleep", "args": ["30"], "timeout": 1 }).to_string();
        let out = tool.run(&input).await.unwrap();
        assert!(out.contains("timed out after 1s"), "got: {out}");
    }

    #[tokio::test]
    async fn timeout_returns_buffered_partial_output() {
        // #169: output emitted before the deadline must accompany the notice.
        // `call` runs no shell, so exec `sh` directly to print then sleep.
        let tool = CallTool::new(std::env::temp_dir());
        let input = serde_json::json!({
            "command": "sh",
            "args": ["-c", "echo early; echo late 1>&2; sleep 30"],
            "timeout": 1,
        })
        .to_string();
        let out = tool.run(&input).await.unwrap();
        assert!(out.contains("timed out after 1s"), "got: {out}");
        assert!(out.contains("early"), "buffered stdout lost: {out}");
        assert!(
            out.contains("[stderr]") && out.contains("late"),
            "buffered stderr lost: {out}"
        );
    }

    #[tokio::test]
    async fn secret_env_is_scrubbed_from_child() {
        // The no-shell design doesn't protect the env: a plain `env` inherits it.
        // A scrubbed var must be gone while an unrelated var survives (#164).
        std::env::set_var("ENTANGLEMENT_TEST_SECRET_CALL", "leak-me");
        std::env::set_var("ENTANGLEMENT_TEST_PUBLIC_CALL", "public");
        let tool = CallTool::new(std::env::temp_dir())
            .with_secret_env(vec!["ENTANGLEMENT_TEST_SECRET_CALL".to_string()]);
        let input = serde_json::json!({ "command": "env", "tail": 0 }).to_string();
        let out = tool.run(&input).await.unwrap();
        std::env::remove_var("ENTANGLEMENT_TEST_SECRET_CALL");
        std::env::remove_var("ENTANGLEMENT_TEST_PUBLIC_CALL");
        assert!(
            !out.contains("ENTANGLEMENT_TEST_SECRET_CALL"),
            "secret must be scrubbed: {out}"
        );
        assert!(
            out.contains("ENTANGLEMENT_TEST_PUBLIC_CALL=public"),
            "unrelated env kept: {out}"
        );
    }

    #[tokio::test]
    async fn tail_zero_still_byte_capped() {
        // A single stream far larger than the 32 KiB cap must still be bounded.
        let tool = CallTool::new(std::env::temp_dir());
        let big = "x".repeat(MAX_OUTPUT_BYTES * 2);
        let input =
            serde_json::json!({ "command": "printf", "args": ["%s", big], "tail": 0 }).to_string();
        let out = tool.run(&input).await.unwrap();
        assert!(
            out.len() < MAX_OUTPUT_BYTES + 200,
            "byte cap must fire: {}",
            out.len()
        );
        assert!(out.contains("truncated"), "byte-cap notice expected: {out}");
    }

    #[tokio::test]
    async fn workdir_runs_in_subdirectory() {
        let dir = TempDir::new();
        std::fs::create_dir(dir.path.join("sub")).unwrap();
        std::fs::write(dir.path.join("sub/inner.txt"), "x").unwrap();
        let tool = CallTool::new(dir.path.clone());
        let input = serde_json::json!({ "command": "ls", "workdir": "sub" }).to_string();
        let out = tool.run(&input).await.unwrap();
        assert!(out.contains("inner.txt"), "got: {out}");
    }

    #[tokio::test]
    async fn workdir_escaping_root_is_rejected() {
        let dir = TempDir::new();
        let tool = CallTool::new(dir.path.clone());
        let input = serde_json::json!({ "command": "ls", "workdir": ".." }).to_string();
        let err = tool.run(&input).await.unwrap_err();
        assert!(
            format!("{err}").contains("escapes working directory"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn workdir_nonexistent_is_rejected() {
        let dir = TempDir::new();
        let tool = CallTool::new(dir.path.clone());
        let input = serde_json::json!({ "command": "ls", "workdir": "nope" }).to_string();
        let err = tool.run(&input).await.unwrap_err();
        assert!(format!("{err}").contains("not a directory"), "{err}");
    }

    #[tokio::test]
    async fn no_workdir_defaults_to_root() {
        let dir = TempDir::new();
        std::fs::write(dir.path.join("marker.txt"), "x").unwrap();
        let tool = CallTool::new(dir.path.clone());
        let input = serde_json::json!({ "command": "ls" }).to_string();
        let out = tool.run(&input).await.unwrap();
        assert!(out.contains("marker.txt"), "got: {out}");
    }

    #[tokio::test]
    async fn input_file_feeds_child_stdin() {
        let dir = TempDir::new();
        std::fs::write(dir.path.join("in.txt"), "hello-from-file\n").unwrap();
        let tool = CallTool::new(dir.path.clone());
        let input = serde_json::json!({ "command": "cat", "input_file": "in.txt" }).to_string();
        let out = tool.run(&input).await.unwrap();
        assert!(out.contains("hello-from-file"), "got: {out}");
    }

    #[tokio::test]
    async fn no_input_file_closes_stdin_not_inherited() {
        // Regression for the unintentional inherit: without `input_file`, `cat`
        // must see immediate EOF (closed stdin), not block on the engine's real
        // stdin. If it inherited, this would time out instead of exiting clean.
        let dir = TempDir::new();
        let tool = CallTool::new(dir.path.clone());
        let input = serde_json::json!({ "command": "cat", "timeout": 3 }).to_string();
        let out = tool.run(&input).await.unwrap();
        assert!(!out.contains("timed out"), "stdin must be closed: {out}");
        assert!(out.contains("[exit 0]"), "got: {out}");
    }

    #[tokio::test]
    async fn missing_input_file_is_clean_error_child_never_spawned() {
        let dir = TempDir::new();
        let tool = CallTool::new(dir.path.clone());
        let input = serde_json::json!({
            "command": "touch",
            "args": ["spawned-marker"],
            "input_file": "does-not-exist.txt",
        })
        .to_string();
        let err = tool.run(&input).await.unwrap_err();
        assert!(err.to_string().contains("input_file"), "got: {err}");
        assert!(
            !dir.path.join("spawned-marker").exists(),
            "child must not spawn on a bad input_file"
        );
    }

    #[tokio::test]
    async fn escaping_root_paths_error_before_spawn() {
        let dir = TempDir::new();
        let tool = CallTool::new(dir.path.clone());

        let in_err = tool
            .run(
                &serde_json::json!({
                    "command": "touch", "args": ["m1"], "input_file": "../escape-in.txt",
                })
                .to_string(),
            )
            .await
            .unwrap_err();
        assert!(in_err.to_string().contains("escapes"), "got: {in_err}");
        assert!(!dir.path.join("m1").exists());

        let out_err = tool
            .run(
                &serde_json::json!({
                    "command": "touch", "args": ["m2"], "output_file": "../escape-out.txt",
                })
                .to_string(),
            )
            .await
            .unwrap_err();
        assert!(out_err.to_string().contains("escapes"), "got: {out_err}");
        assert!(!dir.path.join("m2").exists());
    }

    #[tokio::test]
    async fn output_file_and_stderr_sibling_hold_full_raw_content_under_tail() {
        let dir = TempDir::new();
        let tool = CallTool::new(dir.path.clone());
        let full: String = (1..=50).map(|i| format!("line{i}\n")).collect();
        let input = serde_json::json!({
            "command": "printf",
            "args": ["%s", full],
            "tail": 5,
            "output_file": "out/result.txt",
        })
        .to_string();
        let out = tool.run(&input).await.unwrap();
        assert!(out.contains("earlier lines omitted"), "got: {out}");
        assert!(!out.contains("line1\n"), "response must be tailed: {out}");

        let on_disk = std::fs::read_to_string(dir.path.join("out/result.txt")).unwrap();
        assert_eq!(
            on_disk, full,
            "artifact must hold the full untruncated output"
        );
        assert!(dir.path.join("out/result.txt.stderr").exists());
    }

    #[tokio::test]
    async fn output_file_missing_parent_dirs_are_created() {
        let dir = TempDir::new();
        let tool = CallTool::new(dir.path.clone());
        let input = serde_json::json!({
            "command": "printf",
            "args": ["%s", "hi\n"],
            "output_file": "nested/deep/out.txt",
        })
        .to_string();
        tool.run(&input).await.unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.path.join("nested/deep/out.txt")).unwrap(),
            "hi\n"
        );
        assert!(dir.path.join("nested/deep/out.txt.stderr").exists());
    }

    #[tokio::test]
    async fn default_artifact_created_and_path_named_when_no_output_file() {
        let dir = TempDir::new();
        let tool = CallTool::new(dir.path.clone());
        let input = serde_json::json!({ "command": "printf", "args": ["%s", "auto-artifact\n"] })
            .to_string();
        let out = tool.run(&input).await.unwrap();
        let header = out
            .lines()
            .find(|l| l.starts_with("[output: "))
            .expect("header names the artifact path");
        assert!(
            header.contains(".entanglement/tmp/call-output/call-"),
            "got: {header}"
        );
        let start = "[output: ".len();
        let end = header.find("] [stderr:").expect("stderr sibling named");
        let rel = &header[start..end];
        assert_eq!(
            std::fs::read_to_string(dir.path.join(rel)).unwrap(),
            "auto-artifact\n"
        );
        assert!(dir.path.join(format!("{rel}.stderr")).exists());
    }

    #[tokio::test]
    async fn concurrent_calls_do_not_collide_on_default_filenames() {
        let dir = TempDir::new();
        let tool = std::sync::Arc::new(CallTool::new(dir.path.clone()));
        let mut handles = Vec::new();
        for i in 0..8 {
            let tool = tool.clone();
            handles.push(tokio::spawn(async move {
                let input = serde_json::json!({
                    "command": "printf",
                    "args": ["%s", format!("call-{i}\n")],
                })
                .to_string();
                tool.run(&input).await.unwrap()
            }));
        }
        let mut headers = std::collections::HashSet::new();
        for h in handles {
            let out = h.await.unwrap();
            let header = out
                .lines()
                .find(|l| l.starts_with("[output: "))
                .unwrap()
                .to_string();
            assert!(
                headers.insert(header),
                "default artifact filenames collided"
            );
        }
    }

    #[tokio::test]
    async fn timeout_writes_partial_output_to_artifacts() {
        let dir = TempDir::new();
        let tool = CallTool::new(dir.path.clone());
        let input = serde_json::json!({
            "command": "sh",
            "args": ["-c", "echo early; echo late 1>&2; sleep 30"],
            "timeout": 1,
            "output_file": "partial.txt",
        })
        .to_string();
        let out = tool.run(&input).await.unwrap();
        assert!(out.contains("timed out after 1s"), "got: {out}");

        let stdout_on_disk = std::fs::read_to_string(dir.path.join("partial.txt")).unwrap();
        assert!(
            stdout_on_disk.contains("early"),
            "artifact must hold buffered stdout: {stdout_on_disk}"
        );
        let stderr_on_disk = std::fs::read_to_string(dir.path.join("partial.txt.stderr")).unwrap();
        assert!(
            stderr_on_disk.contains("late"),
            "artifact must hold buffered stderr: {stderr_on_disk}"
        );
    }

    fn bwrap_policy(network: bool) -> SandboxPolicy {
        SandboxPolicy {
            backend: sandbox::SandboxBackend::Bubblewrap,
            network,
        }
    }

    /// ADR-0104: a sandboxed `call` can still write inside the bind-mounted
    /// project root, but the rest of the filesystem is read-only. `outside` is
    /// deliberately under `/var/tmp`, not `/tmp` — the recipe gives the latter
    /// a fresh empty tmpfs, which would fail for the wrong reason (path
    /// doesn't exist) rather than the read-only-bind reason under test.
    #[tokio::test]
    async fn sandbox_confines_writes_to_root() {
        if !sandbox::bwrap_available() {
            eprintln!("skipping: bwrap not installed");
            return;
        }
        let dir = TempDir::new();
        let outside = tempfile::Builder::new()
            .prefix("entanglement-sandbox-call-test-")
            .tempdir_in("/var/tmp")
            .unwrap();
        let tool = CallTool::new(dir.path.clone()).with_sandbox(bwrap_policy(false));

        let input = serde_json::json!({ "command": "touch", "args": ["inside.txt"] }).to_string();
        let out = tool.run(&input).await.unwrap();
        assert!(out.contains("[exit 0]"), "{out}");
        assert!(dir.path.join("inside.txt").exists());

        let leak_path = outside.path().join("leak.txt");
        let input = serde_json::json!({
            "command": "touch",
            "args": [leak_path.to_string_lossy()],
        })
        .to_string();
        let out = tool.run(&input).await.unwrap();
        assert!(
            !out.contains("[exit 0]"),
            "write outside root should fail: {out}"
        );
        assert!(
            !leak_path.exists(),
            "sandbox must not allow writes outside the project root"
        );
    }

    /// ADR-0104: sandboxed network is cut by default. `call` has no shell, so
    /// exec `sh` directly to reuse bash's `/dev/tcp` trick — needs no external
    /// network binary (`curl`/`nc`).
    #[tokio::test]
    async fn sandbox_cuts_network_by_default() {
        if !sandbox::bwrap_available() {
            eprintln!("skipping: bwrap not installed");
            return;
        }
        let dir = TempDir::new();
        let tool = CallTool::new(dir.path.clone()).with_sandbox(bwrap_policy(false));
        let input = serde_json::json!({
            "command": "sh",
            "args": ["-c", "exec 3<>/dev/tcp/1.1.1.1/80"],
            "timeout": 5,
        })
        .to_string();
        let out = tool.run(&input).await.unwrap();
        assert!(
            !out.contains("[exit 0]"),
            "network must be unreachable when sandboxed without network:true: {out}"
        );
    }

    /// ADR-0104 §6: the process-group timeout/kill path must still tear down a
    /// sandboxed command's whole tree, not just the outer `bwrap` process.
    #[tokio::test]
    async fn sandbox_timeout_still_kills_the_whole_tree() {
        if !sandbox::bwrap_available() {
            eprintln!("skipping: bwrap not installed");
            return;
        }
        let tool = CallTool::new(std::env::temp_dir()).with_sandbox(bwrap_policy(false));
        let input =
            serde_json::json!({ "command": "sleep", "args": ["30"], "timeout": 1 }).to_string();
        let out = tool.run(&input).await.unwrap();
        assert!(out.contains("timed out after 1s"), "got: {out}");
    }

    /// ADR-0104: `call`'s no-shell argv-exec guarantee (a shell metacharacter
    /// reaches the binary literally, never interpreted) must hold identically
    /// when sandboxed — bwrap wraps the exec, it must not reintroduce a shell.
    #[tokio::test]
    async fn sandbox_preserves_no_shell_argv_semantics() {
        if !sandbox::bwrap_available() {
            eprintln!("skipping: bwrap not installed");
            return;
        }
        let tool = CallTool::new(std::env::temp_dir()).with_sandbox(bwrap_policy(false));
        let payload = "$HOME; rm -rf / && echo x | cat *.rs";
        let input = serde_json::json!({
            "command": "printf",
            "args": ["%s", payload],
        })
        .to_string();
        let out = tool.run(&input).await.unwrap();
        assert!(out.contains("[exit 0]"), "got: {out}");
        assert!(out.contains(payload), "argv must be verbatim, got: {out}");
    }
}
