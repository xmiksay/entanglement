//! `bash` — run a shell command rooted at the working directory.
//! Runs unsandboxed with the engine's full privileges (ADR-0009).

use super::exec::{own_process_group, wait_or_kill_group, ExecOutcome};
use super::jobs::JobRegistry;
use super::{resolve_under_root, truncate_head_tail};
use crate::tools::Tool;
use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::Deserialize;
use std::borrow::Cow;
use std::path::{Path, PathBuf};

const MAX_BASH_TIMEOUT_SECONDS: u64 = 600;

pub struct BashTool {
    root: PathBuf,
    /// Env vars scrubbed from the child before spawn — the provider API keys
    /// (`ZAI_API_KEY`, …) so a model-authored `env`/`printenv` can't read the
    /// engine's credentials (#164). Empty by default; wired from the catalog.
    secret_env: Vec<String>,
    /// Background-job registry shared with `bash_output` (#170). A private
    /// per-tool default keeps standalone/TUI construction working; the head wires
    /// the shared instance via [`BashTool::with_jobs`] so polls reach the jobs
    /// this tool spawned.
    jobs: JobRegistry,
}

impl BashTool {
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            secret_env: Vec::new(),
            jobs: JobRegistry::new(),
        }
    }

    /// Scrub `vars` from the spawned command's environment (provider API keys).
    pub fn with_secret_env(mut self, vars: Vec<String>) -> Self {
        self.secret_env = vars;
        self
    }

    /// Share `jobs` with the paired `bash_output` tool so background jobs this
    /// tool spawns are pollable (#170).
    pub fn with_jobs(mut self, jobs: JobRegistry) -> Self {
        self.jobs = jobs;
        self
    }

    /// Resolve the per-call working directory: the tool `root` by default, or a
    /// model-supplied `workdir` validated to stay under root (same symlink-safe
    /// containment as the filesystem tools, ADR-0054/#163) and to be a directory.
    fn resolve_workdir(&self, workdir: Option<&str>) -> Result<PathBuf> {
        match workdir {
            None => Ok(self.root.clone()),
            Some(w) => {
                let p = resolve_under_root(&self.root, w)?;
                if !p.is_dir() {
                    anyhow::bail!("workdir is not a directory: {w}");
                }
                Ok(p)
            }
        }
    }

    /// Build the `sh -c` command with cwd, piped stdio, own process group, and
    /// scrubbed secrets — shared by the foreground and background paths.
    fn build_command(&self, command: &str, cwd: &Path) -> tokio::process::Command {
        let mut cmd = tokio::process::Command::new("sh");
        cmd.args(["-c", command])
            .current_dir(cwd)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        // Own process group so a timeout/cancel kills the whole tree, not just
        // `sh` (a launched server/pipeline would otherwise orphan — #168).
        own_process_group(&mut cmd);
        for var in &self.secret_env {
            cmd.env_remove(var);
        }
        cmd
    }
}

#[derive(Deserialize)]
struct BashInput {
    command: String,
    #[serde(default)]
    timeout: Option<u64>,
    /// Optional per-call working directory, resolved under the tool root.
    #[serde(default)]
    workdir: Option<String>,
    /// Spawn detached and return a job id to poll via `bash_output` (#170).
    #[serde(default)]
    run_in_background: bool,
}

#[async_trait]
impl Tool for BashTool {
    fn name(&self) -> Cow<'static, str> {
        Cow::Borrowed("bash")
    }
    fn description(&self) -> &str {
        "Run a shell command rooted at the working directory. The command \
         runs with the engine's full privileges (unsandboxed). Returns \
         `[exit N]`, stdout, and `[stderr]`; oversized output keeps a head + \
         tail slice so the trailing error survives truncation. Pass `workdir` to \
         run in a subdirectory (validated under root). Pass \
         `run_in_background=true` to start a long job (build, dev server) \
         detached and get a job id — poll it with `bash_output`."
    }
    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "command": {
                    "type": "string",
                    "description": "Shell command to run via `sh -c`."
                },
                "timeout": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "Timeout in seconds (default 120, capped at 600). \
                        Ignored when run_in_background=true."
                },
                "workdir": {
                    "type": "string",
                    "description": "Working directory for this call, relative to \
                        the root (must stay under it). Defaults to the root."
                },
                "run_in_background": {
                    "type": "boolean",
                    "description": "Start the command detached and return a job id \
                        to poll with `bash_output` instead of blocking. Default false."
                }
            },
            "required": ["command"]
        })
    }
    async fn run(&self, input: &str) -> Result<String> {
        let parsed: BashInput = serde_json::from_str(input)
            .context("invalid input to bash: expected {\"command\": string, ...}")?;
        let cwd = self.resolve_workdir(parsed.workdir.as_deref())?;
        let mut cmd = self.build_command(&parsed.command, &cwd);

        if parsed.run_in_background {
            let id = self
                .jobs
                .spawn(parsed.command.clone(), cmd)
                .with_context(|| "spawning background bash command")?;
            return Ok(format!(
                "[background job {id} started]\n\
                 Poll with `bash_output` (job_id=\"{id}\") for incremental output; \
                 pass kill=true to stop it."
            ));
        }

        let secs = parsed.timeout.unwrap_or(120);
        let dur = std::time::Duration::from_secs(secs.min(MAX_BASH_TIMEOUT_SECONDS));
        let child = cmd.spawn().with_context(|| "spawning bash command")?;

        match wait_or_kill_group(child, dur).await {
            Ok(ExecOutcome::Completed(output)) => Ok(format_bash_output(
                output.status.code(),
                &output.stdout,
                &output.stderr,
            )),
            // Return the output buffered before the kill alongside the notice —
            // the prefix is often the diagnostic the model needs (#169).
            Ok(ExecOutcome::TimedOut { stdout, stderr }) => Ok(format_bash_streams(
                &format!("[killed: timed out after {secs}s]\n"),
                &stdout,
                &stderr,
            )),
            Err(e) => Err(anyhow::anyhow!("bash io error: {e}")),
        }
    }
}

fn format_bash_output(code: Option<i32>, stdout: &[u8], stderr: &[u8]) -> String {
    format_bash_streams(&format!("[exit {}]\n", code.unwrap_or(-1)), stdout, stderr)
}

/// Assemble `header` + stdout + a `[stderr]` block, then apply the byte cap.
/// Shared by the exit path (`[exit N]`) and the timeout path (`[killed: …]`).
fn format_bash_streams(header: &str, stdout: &[u8], stderr: &[u8]) -> String {
    let mut out = String::from(header);
    let stdout_str = String::from_utf8_lossy(stdout);
    if !stdout_str.is_empty() {
        out.push_str(&stdout_str);
    }
    let stderr_str = String::from_utf8_lossy(stderr);
    if !stderr_str.is_empty() {
        out.push_str("[stderr]\n");
        out.push_str(&stderr_str);
    }
    // Head+tail cap (#170): build/test output puts the load-bearing error at the
    // end, so head-only truncation would drop exactly what the model needs.
    truncate_head_tail(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_includes_exit_and_stdout() {
        let out = format_bash_output(Some(0), b"hello\n", b"");
        assert_eq!(out, "[exit 0]\nhello\n");
    }

    #[test]
    fn format_appends_stderr_section() {
        let out = format_bash_output(Some(2), b"out\n", b"boom\n");
        assert_eq!(out, "[exit 2]\nout\n[stderr]\nboom\n");
    }

    #[test]
    fn format_missing_code_reports_minus_one() {
        let out = format_bash_output(None, b"", b"");
        assert_eq!(out, "[exit -1]\n");
    }

    #[tokio::test]
    async fn run_echoes_stdout_with_zero_exit() {
        let dir = tempfile::tempdir().unwrap();
        let tool = BashTool::new(dir.path().to_path_buf());
        let out = tool.run(r#"{"command":"echo hi"}"#).await.unwrap();
        assert!(out.starts_with("[exit 0]\n"), "{out}");
        assert!(out.contains("hi"), "{out}");
    }

    #[tokio::test]
    async fn run_reports_nonzero_exit_and_stderr() {
        let dir = tempfile::tempdir().unwrap();
        let tool = BashTool::new(dir.path().to_path_buf());
        let out = tool
            .run(r#"{"command":"echo oops 1>&2; exit 3"}"#)
            .await
            .unwrap();
        assert!(out.contains("[exit 3]"), "{out}");
        assert!(out.contains("[stderr]") && out.contains("oops"), "{out}");
    }

    #[tokio::test]
    async fn run_is_rooted_at_working_directory() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("marker.txt"), "x").unwrap();
        let tool = BashTool::new(dir.path().to_path_buf());
        let out = tool.run(r#"{"command":"ls"}"#).await.unwrap();
        assert!(out.contains("marker.txt"), "{out}");
    }

    #[tokio::test]
    async fn run_times_out_and_reports_killed() {
        let dir = tempfile::tempdir().unwrap();
        let tool = BashTool::new(dir.path().to_path_buf());
        let out = tool
            .run(r#"{"command":"sleep 5","timeout":1}"#)
            .await
            .unwrap();
        assert!(out.contains("killed") && out.contains("timed out"), "{out}");
    }

    #[tokio::test]
    async fn run_timeout_returns_buffered_partial_output() {
        // #169: a line printed before the deadline must accompany the notice.
        let dir = tempfile::tempdir().unwrap();
        let tool = BashTool::new(dir.path().to_path_buf());
        let out = tool
            .run(r#"{"command":"echo early; echo late 1>&2; sleep 5","timeout":1}"#)
            .await
            .unwrap();
        assert!(out.contains("timed out after 1s"), "{out}");
        assert!(out.contains("early"), "buffered stdout lost: {out}");
        assert!(
            out.contains("[stderr]") && out.contains("late"),
            "buffered stderr lost: {out}"
        );
    }

    #[tokio::test]
    async fn secret_env_is_scrubbed_from_command() {
        // A scrubbed var must be invisible to the model-authored command (#164),
        // while an unrelated var still passes through.
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("ENTANGLEMENT_TEST_SECRET_BASH", "leak-me");
        std::env::set_var("ENTANGLEMENT_TEST_PUBLIC_BASH", "public");
        let tool = BashTool::new(dir.path().to_path_buf())
            .with_secret_env(vec!["ENTANGLEMENT_TEST_SECRET_BASH".to_string()]);
        let out = tool
            .run(r#"{"command":"echo secret=[$ENTANGLEMENT_TEST_SECRET_BASH] public=[$ENTANGLEMENT_TEST_PUBLIC_BASH]"}"#)
            .await
            .unwrap();
        std::env::remove_var("ENTANGLEMENT_TEST_SECRET_BASH");
        std::env::remove_var("ENTANGLEMENT_TEST_PUBLIC_BASH");
        assert!(out.contains("secret=[]"), "secret must be scrubbed: {out}");
        assert!(out.contains("public=[public]"), "unrelated env kept: {out}");
    }

    #[tokio::test]
    async fn invalid_json_input_errors() {
        let dir = tempfile::tempdir().unwrap();
        let tool = BashTool::new(dir.path().to_path_buf());
        let err = tool.run("{}").await.unwrap_err();
        assert!(format!("{err}").contains("invalid input to bash"), "{err}");
    }

    #[tokio::test]
    async fn workdir_runs_in_subdirectory() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("sub/inner.txt"), "x").unwrap();
        let tool = BashTool::new(dir.path().to_path_buf());
        let out = tool
            .run(r#"{"command":"ls","workdir":"sub"}"#)
            .await
            .unwrap();
        assert!(out.contains("inner.txt"), "{out}");
    }

    #[tokio::test]
    async fn workdir_escaping_root_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let tool = BashTool::new(dir.path().to_path_buf());
        let err = tool
            .run(r#"{"command":"ls","workdir":".."}"#)
            .await
            .unwrap_err();
        assert!(
            format!("{err}").contains("escapes working directory"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn workdir_nonexistent_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let tool = BashTool::new(dir.path().to_path_buf());
        let err = tool
            .run(r#"{"command":"ls","workdir":"nope"}"#)
            .await
            .unwrap_err();
        assert!(format!("{err}").contains("not a directory"), "{err}");
    }

    /// #170: oversized output keeps a head **and** tail slice so the trailing
    /// error survives — head-only truncation would drop it.
    #[tokio::test]
    async fn oversized_output_keeps_head_and_tail() {
        use crate::host::MAX_OUTPUT_BYTES;
        let dir = tempfile::tempdir().unwrap();
        let tool = BashTool::new(dir.path().to_path_buf());
        // Print a large body, then a distinctive final line (the "error").
        let n = MAX_OUTPUT_BYTES * 2;
        let cmd = format!(
            r#"{{"command":"head -c {n} /dev/zero | tr '\\0' a; echo; echo FINAL_ERROR_LINE"}}"#
        );
        let out = tool.run(&cmd).await.unwrap();
        assert!(out.contains("[exit 0]"), "{out}");
        assert!(out.contains("FINAL_ERROR_LINE"), "tail lost: {out}");
        assert!(
            out.contains("omitted from the middle"),
            "expected head+tail notice: {out}"
        );
        assert!(
            out.len() < MAX_OUTPUT_BYTES + 200,
            "byte cap held: {}",
            out.len()
        );
    }
}
