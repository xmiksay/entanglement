//! `bash` — run a shell command rooted at the working directory.
//! Runs unsandboxed with the engine's full privileges (ADR-0009).

use super::truncate_output;
use anyhow::{Context, Result};
use async_trait::async_trait;
use entanglement_core::tools::Tool;
use serde::Deserialize;

const MAX_BASH_TIMEOUT_SECONDS: u64 = 600;

pub struct BashTool {
    root: std::path::PathBuf,
}

impl BashTool {
    pub fn new(root: std::path::PathBuf) -> Self {
        Self { root }
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
        let child = tokio::process::Command::new("sh")
            .args(["-c", &parsed.command])
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

fn format_bash_output(code: Option<i32>, stdout: &[u8], stderr: &[u8]) -> String {
    let mut out = String::new();
    out.push_str(&format!("[exit {}]\n", code.unwrap_or(-1)));
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
