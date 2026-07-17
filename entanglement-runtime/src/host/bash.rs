//! `bash` — run a shell command rooted at the working directory.
//! Runs unsandboxed with the engine's full privileges by default (ADR-0009);
//! an opt-in bubblewrap confinement layer is available (ADR-0104,
//! [`SandboxPolicy`]). Stdin is
//! always closed, not inherited from the engine (ADR-0092/ADR-0093's
//! default-closed-stdin fix, carried over from `call` to `bash` — #389); use
//! shell-native `< file` redirection if a command needs input.

use super::exec::{own_process_group, wait_or_kill_group, ExecOutcome};
use super::jobs::JobRegistry;
use super::sandbox::{self, SandboxPolicy};
use super::{resolve_workdir, truncate_head_tail};
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
    /// Optional bubblewrap confinement (ADR-0104). Defaults to
    /// [`SandboxPolicy::none()`] — unsandboxed, unchanged from before this
    /// existed.
    sandbox: SandboxPolicy,
}

impl BashTool {
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            secret_env: Vec::new(),
            jobs: JobRegistry::new(),
            sandbox: SandboxPolicy::none(),
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

    /// Confine every spawned command under `policy` (ADR-0104).
    pub fn with_sandbox(mut self, policy: SandboxPolicy) -> Self {
        self.sandbox = policy;
        self
    }

    /// Build the `sh -c` command with cwd, piped stdio, own process group, and
    /// scrubbed secrets — shared by the foreground and background paths.
    fn build_command(&self, command: &str, cwd: &Path) -> tokio::process::Command {
        let args = vec!["-c".to_string(), command.to_string()];
        let mut cmd = sandbox::command(&self.sandbox, &self.root, "sh", &args);
        cmd.current_dir(cwd)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            // Close stdin explicitly rather than inherit the engine's real
            // stdin — the same leak ADR-0092 closed for `call` (#389). Applies
            // to both the foreground and `run_in_background` paths, since both
            // share this helper.
            .stdin(std::process::Stdio::null())
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
         runs with the engine's full privileges (unsandboxed). Stdin is \
         closed, not inherited — use shell-native `< file` redirection if the \
         command needs input. Returns \
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
        let cwd = resolve_workdir(&self.root, parsed.workdir.as_deref())?;
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
    async fn run_closes_stdin_not_inherited() {
        // Regression for the unintentional inherit (#389, same class as #381's
        // `call` fix): without stdin wired up, `cat` must see immediate EOF, not
        // block on the engine's real stdin. If it inherited, this would time out
        // instead of exiting clean.
        let dir = tempfile::tempdir().unwrap();
        let tool = BashTool::new(dir.path().to_path_buf());
        let out = tool.run(r#"{"command":"cat","timeout":3}"#).await.unwrap();
        assert!(!out.contains("timed out"), "stdin must be closed: {out}");
        assert!(out.contains("[exit 0]"), "{out}");
    }

    #[tokio::test]
    async fn run_in_background_closes_stdin_not_inherited() {
        // The more dangerous case (#389): a detached job holding the fd open
        // would race the engine's own stdin reader indefinitely. `cat` run in
        // the background must still see immediate EOF and exit promptly.
        let dir = tempfile::tempdir().unwrap();
        let jobs = JobRegistry::new();
        let tool = BashTool::new(dir.path().to_path_buf()).with_jobs(jobs.clone());
        let out = tool
            .run(r#"{"command":"cat","run_in_background":true}"#)
            .await
            .unwrap();
        let id = out
            .lines()
            .find_map(|l| {
                l.strip_prefix("[background job ")
                    .and_then(|rest| rest.strip_suffix(" started]"))
            })
            .expect("job id in response")
            .to_string();

        for _ in 0..50 {
            let p = jobs.poll(&id, false).expect("job registered");
            if p.status == crate::host::jobs::JobStatus::Exited(Some(0)) {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        panic!("background cat never exited — stdin was likely inherited, not closed");
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

    fn bwrap_policy(network: bool) -> SandboxPolicy {
        SandboxPolicy {
            backend: sandbox::SandboxBackend::Bubblewrap,
            network,
        }
    }

    /// ADR-0104: a sandboxed command can still write inside the bind-mounted
    /// project root, but the rest of the filesystem is read-only — writing
    /// outside root (even to a directory the test process itself owns and can
    /// normally write to) must fail. `outside` is deliberately placed under
    /// `/var/tmp`, not `/tmp` — the sandbox recipe gives the latter a fresh
    /// empty tmpfs, which would make this pass for the wrong reason (path
    /// doesn't exist) rather than the read-only-bind reason being tested.
    #[tokio::test]
    async fn sandbox_confines_writes_to_root() {
        if !sandbox::bwrap_available() {
            eprintln!("skipping: bwrap not installed");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::Builder::new()
            .prefix("entanglement-sandbox-test-")
            .tempdir_in("/var/tmp")
            .unwrap();
        let tool = BashTool::new(dir.path().to_path_buf()).with_sandbox(bwrap_policy(false));

        let inside = tool
            .run(r#"{"command":"echo ok > inside.txt && cat inside.txt"}"#)
            .await
            .unwrap();
        assert!(
            inside.contains("[exit 0]") && inside.contains("ok"),
            "{inside}"
        );
        assert!(dir.path().join("inside.txt").exists());

        let leak_path = outside.path().join("leak.txt");
        let out = tool
            .run(&format!(
                r#"{{"command":"echo pwned > {}"}}"#,
                leak_path.display()
            ))
            .await
            .unwrap();
        assert!(
            !out.contains("[exit 0]"),
            "write outside root should fail: {out}"
        );
        assert!(
            !leak_path.exists(),
            "sandbox must not allow writes outside the project root"
        );
    }

    /// ADR-0104: sandboxed network is cut by default (no `network: true`) —
    /// use bash's own `/dev/tcp` so the assertion needs no external binary
    /// (`curl`/`nc`) and can't pass just because the host has no internet.
    #[tokio::test]
    async fn sandbox_cuts_network_by_default() {
        if !sandbox::bwrap_available() {
            eprintln!("skipping: bwrap not installed");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let tool = BashTool::new(dir.path().to_path_buf()).with_sandbox(bwrap_policy(false));
        let out = tool
            .run(r#"{"command":"exec 3<>/dev/tcp/1.1.1.1/80","timeout":5}"#)
            .await
            .unwrap();
        assert!(
            !out.contains("[exit 0]"),
            "network must be unreachable when sandboxed without network:true: {out}"
        );
    }

    /// ADR-0104 §6: the process-group timeout/kill path (#167/#168/#169) must
    /// still tear down a sandboxed command's whole tree, not just the outer
    /// `bwrap` process.
    #[tokio::test]
    async fn sandbox_timeout_still_kills_the_whole_tree() {
        if !sandbox::bwrap_available() {
            eprintln!("skipping: bwrap not installed");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let tool = BashTool::new(dir.path().to_path_buf()).with_sandbox(bwrap_policy(false));
        let out = tool
            .run(r#"{"command":"sleep 30","timeout":1}"#)
            .await
            .unwrap();
        assert!(out.contains("killed") && out.contains("timed out"), "{out}");
    }
}
