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

mod format;
mod output;

use super::exec::{own_process_group, wait_or_kill_group, ExecOutcome};
use super::resolve_under_root;
use super::sandbox::{self, SandboxPolicy};
use crate::tools::Tool;
use anyhow::{Context, Result};
use async_trait::async_trait;
use entanglement_core::{ContentPart, SessionId};
use format::{format_call_output, format_call_streams};
use output::{persist_output, resolve_output_target};
use serde::Deserialize;
use std::borrow::Cow;
use std::path::PathBuf;
use tokio::io::AsyncWriteExt;

const MAX_CALL_TIMEOUT_SECONDS: u64 = 600;
const DEFAULT_TAIL: u32 = 30;

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
    /// Approval-gated out-of-root `workdir` (ADR-0109).
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

    /// Permit an approved out-of-root `workdir` (ADR-0109).
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
        self.run_impl("", input).await
    }

    async fn run_for_session(
        &self,
        _session: &SessionId,
        request_id: &str,
        input: &str,
    ) -> Result<Vec<ContentPart>> {
        Ok(crate::tools::text_parts(
            self.run_impl(request_id, input).await?,
        ))
    }
}

impl CallTool {
    /// `request_id` (#449) is forwarded to the escape-root grant check so a
    /// `Once` approval for `workdir` is only consumed by the call it was
    /// approved for.
    async fn run_impl(&self, request_id: &str, input: &str) -> Result<String> {
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
            request_id,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::MAX_OUTPUT_BYTES;
    use std::sync::atomic::{AtomicU64, Ordering};

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
