//! `call` — direct process execution (argv, **no shell**) with auto-tailed
//! output. Complements `bash` (ADR-0009): what the model sends as `command` +
//! `args` execs verbatim — no `sh -c`, so no pipes, globbing, `$VAR` expansion,
//! or metacharacter injection. A fixed argv is auditable, which is why a profile
//! may reasonably `Allow` `call` while keeping `bash` at `Ask`/`Deny`. Runs
//! unsandboxed with the engine's full privileges, same opt-in gate as `bash`
//! (ADR-0010).

use super::exec::{own_process_group, wait_or_kill_group, ExecOutcome};
use super::truncate_output;
use crate::tools::Tool;
use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::Deserialize;

const MAX_CALL_TIMEOUT_SECONDS: u64 = 600;
const DEFAULT_TAIL: u32 = 30;

pub struct CallTool {
    root: std::path::PathBuf,
    /// Env vars scrubbed from the child before spawn — the provider API keys
    /// (`ZAI_API_KEY`, …) so a model-authored binary can't read the engine's
    /// credentials (#164). The no-shell design doesn't help here: a plain
    /// `env`/`printenv` still inherits them. Empty by default; wired from the
    /// catalog.
    secret_env: Vec<String>,
}

impl CallTool {
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
struct CallInput {
    command: String,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default = "default_tail")]
    tail: u32,
    #[serde(default)]
    timeout: Option<u64>,
}

fn default_tail() -> u32 {
    DEFAULT_TAIL
}

#[async_trait]
impl Tool for CallTool {
    fn name(&self) -> &'static str {
        "call"
    }
    fn description(&self) -> &str {
        "Execute a binary directly (argv, NO shell) rooted at the working \
         directory: `command` + `args` are passed verbatim to exec — no `sh -c`, \
         so pipes, globbing, `$VAR` expansion, and metacharacters are NOT \
         interpreted. Prefer this over `bash` for a fixed command. Output is \
         tailed to the last `tail` lines per stream (default 30 — command value \
         concentrates at the end); pass `tail=0` deliberately for full output \
         (still byte-capped). Returns `[exit N]`, tailed stdout, and a tailed \
         `[stderr]` block."
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
        let mut cmd = tokio::process::Command::new(&parsed.command);
        cmd.args(&parsed.args)
            .current_dir(&self.root)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        // Own process group so a timeout kills the whole tree, not just the
        // direct child (a launched server/pipeline would otherwise orphan — #168).
        own_process_group(&mut cmd);
        for var in &self.secret_env {
            cmd.env_remove(var);
        }
        let child = cmd
            .spawn()
            // A missing binary (or non-exec target) surfaces here — return it as
            // tool output, never panic (ADR-0016 clean-error contract).
            .with_context(|| format!("spawning `{}`", parsed.command))?;

        match wait_or_kill_group(child, dur).await {
            Ok(ExecOutcome::Completed(output)) => Ok(format_call_output(
                output.status.code(),
                &output.stdout,
                &output.stderr,
                parsed.tail,
            )),
            Ok(ExecOutcome::TimedOut) => Ok(format!("[killed: timed out after {secs}s]")),
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
fn format_call_output(code: Option<i32>, stdout: &[u8], stderr: &[u8], tail: u32) -> String {
    let mut out = String::new();
    out.push_str(&format!("[exit {}]\n", code.unwrap_or(-1)));
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
        let out = format_call_output(Some(2), b"hello\n", b"boom\n", 30);
        assert!(out.starts_with("[exit 2]\n"), "got: {out}");
        assert!(out.contains("hello\n"), "got: {out}");
        assert!(out.contains("[stderr]\nboom\n"), "got: {out}");
    }

    #[test]
    fn format_tails_both_streams_independently() {
        let big: String = (1..=50).map(|i| format!("o{i}\n")).collect();
        let err: String = (1..=50).map(|i| format!("e{i}\n")).collect();
        let out = format_call_output(Some(0), big.as_bytes(), err.as_bytes(), 5);
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
}
