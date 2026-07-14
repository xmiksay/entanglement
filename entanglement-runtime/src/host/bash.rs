//! `bash` — run a shell command rooted at the working directory.
//! Runs unsandboxed with the engine's full privileges (ADR-0009).

use super::exec::{own_process_group, wait_or_kill_group, ExecOutcome};
use super::truncate_output;
use crate::tools::Tool;
use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::Deserialize;

const MAX_BASH_TIMEOUT_SECONDS: u64 = 600;

pub struct BashTool {
    root: std::path::PathBuf,
    /// Env vars scrubbed from the child before spawn — the provider API keys
    /// (`ZAI_API_KEY`, …) so a model-authored `env`/`printenv` can't read the
    /// engine's credentials (#164). Empty by default; wired from the catalog.
    secret_env: Vec<String>,
}

impl BashTool {
    pub fn new(root: std::path::PathBuf) -> Self {
        Self {
            root,
            secret_env: Vec::new(),
        }
    }

    /// Scrub `vars` from the spawned command's environment (provider API keys).
    pub fn with_secret_env(mut self, vars: Vec<String>) -> Self {
        self.secret_env = vars;
        self
    }
}

#[derive(Deserialize)]
struct BashInput {
    command: String,
    #[serde(default)]
    timeout: Option<u64>,
}

#[async_trait]
impl Tool for BashTool {
    fn name(&self) -> &'static str {
        "bash"
    }
    fn description(&self) -> &str {
        "Run a shell command rooted at the working directory. The command \
         runs with the engine's full privileges (unsandboxed). Returns \
         `[exit N]`, stdout, and `[stderr]`."
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
                    "description": "Timeout in seconds (default 120, capped at 600)."
                }
            },
            "required": ["command"]
        })
    }
    async fn run(&self, input: &str) -> Result<String> {
        let parsed: BashInput = serde_json::from_str(input)
            .context("invalid input to bash: expected {\"command\": string, ...}")?;
        let secs = parsed.timeout.unwrap_or(120);
        let dur = std::time::Duration::from_secs(secs.min(MAX_BASH_TIMEOUT_SECONDS));
        let mut cmd = tokio::process::Command::new("sh");
        cmd.args(["-c", &parsed.command])
            .current_dir(&self.root)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        // Own process group so a timeout kills the whole tree, not just `sh`
        // (a launched server/pipeline would otherwise orphan — #168).
        own_process_group(&mut cmd);
        for var in &self.secret_env {
            cmd.env_remove(var);
        }
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
    truncate_output(out)
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
}
