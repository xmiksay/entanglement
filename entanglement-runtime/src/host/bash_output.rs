//! `bash_output` — poll a background `bash` job for the output it produced since
//! the last poll, plus its status (#170). The companion to `bash`'s
//! `run_in_background`: a long build/test or a launched dev server is no longer a
//! black hole — the model spawns it, gets a job id back, and reads incremental
//! output by polling. `kill: true` SIGKILLs the job's whole process group.

use super::jobs::{JobRegistry, JobStatus};
use super::truncate_head_tail;
use crate::tools::Tool;
use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::Deserialize;
use std::borrow::Cow;

pub struct BashOutputTool {
    jobs: JobRegistry,
}

impl BashOutputTool {
    pub fn new(jobs: JobRegistry) -> Self {
        Self { jobs }
    }
}

#[derive(Deserialize)]
struct BashOutputInput {
    job_id: String,
    #[serde(default)]
    kill: bool,
}

#[async_trait]
impl Tool for BashOutputTool {
    fn name(&self) -> Cow<'static, str> {
        Cow::Borrowed("bash_output")
    }
    fn description(&self) -> &str {
        "Poll a background `bash` job (started with run_in_background=true) for \
         the output it produced since the last poll, plus its status \
         (`running` / `exited N`). Output is drained per poll — each call returns \
         only what is new. Pass `kill=true` to terminate the job's whole process \
         group."
    }
    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "job_id": {
                    "type": "string",
                    "description": "Id returned by a `bash` call with run_in_background=true."
                },
                "kill": {
                    "type": "boolean",
                    "description": "Terminate the job's process group before reading. Default false."
                }
            },
            "required": ["job_id"]
        })
    }
    async fn run(&self, input: &str) -> Result<String> {
        let parsed: BashOutputInput = serde_json::from_str(input)
            .context("invalid input to bash_output: expected {\"job_id\": string, ...}")?;
        match self.jobs.poll(&parsed.job_id, parsed.kill) {
            Some(poll) => Ok(format_poll(&parsed.job_id, poll)),
            None => Ok(format!(
                "[unknown job `{}`] — no background job with that id (started via bash run_in_background=true?)",
                parsed.job_id
            )),
        }
    }
}

fn format_poll(id: &str, poll: super::jobs::Poll) -> String {
    let status = match poll.status {
        JobStatus::Running => "running".to_string(),
        JobStatus::Exited(Some(code)) => format!("exited {code}"),
        JobStatus::Exited(None) => "exited (killed)".to_string(),
    };
    let mut out = format!("[job {id}: {status}]\n");
    if poll.stdout_dropped > 0 {
        out.push_str(&format!(
            "[{} bytes of older stdout dropped]\n",
            poll.stdout_dropped
        ));
    }
    let stdout = String::from_utf8_lossy(&poll.stdout);
    if !stdout.is_empty() {
        out.push_str(&stdout);
    }
    if poll.stderr_dropped > 0 {
        out.push_str(&format!(
            "[{} bytes of older stderr dropped]\n",
            poll.stderr_dropped
        ));
    }
    let stderr = String::from_utf8_lossy(&poll.stderr);
    if !stderr.is_empty() {
        out.push_str("[stderr]\n");
        out.push_str(&stderr);
    }
    if stdout.is_empty() && stderr.is_empty() {
        out.push_str("(no new output)\n");
    }
    truncate_head_tail(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::bash::BashTool;

    #[tokio::test]
    async fn poll_unknown_job_is_clean_message() {
        let reg = JobRegistry::new();
        let tool = BashOutputTool::new(reg);
        let out = tool.run(r#"{"job_id":"bg-404"}"#).await.unwrap();
        assert!(out.contains("unknown job"), "{out}");
    }

    #[tokio::test]
    async fn invalid_json_errors() {
        let tool = BashOutputTool::new(JobRegistry::new());
        let err = tool.run("{}").await.unwrap_err();
        assert!(
            format!("{err}").contains("invalid input to bash_output"),
            "{err}"
        );
    }

    /// End-to-end: `bash` starts a background job, `bash_output` polls it to
    /// completion and sees the exit status + output.
    #[tokio::test]
    async fn bash_background_job_is_pollable() {
        let dir = tempfile::tempdir().unwrap();
        let jobs = JobRegistry::new();
        let bash = BashTool::new(dir.path().to_path_buf()).with_jobs(jobs.clone());
        let started = bash
            .run(r#"{"command":"echo hello","run_in_background":true}"#)
            .await
            .unwrap();
        assert!(started.contains("background job"), "{started}");
        // Extract the id ("bg-0") from the start notice.
        let id = started
            .split_whitespace()
            .find(|w| w.starts_with("bg-"))
            .unwrap()
            .trim_end_matches(|c: char| !c.is_ascii_alphanumeric() && c != '-');
        let poller = BashOutputTool::new(jobs);
        for _ in 0..50 {
            let out = poller
                .run(&serde_json::json!({ "job_id": id }).to_string())
                .await
                .unwrap();
            if out.contains("exited 0") {
                assert!(out.contains("hello"), "{out}");
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        panic!("background job never reached exited 0");
    }
}
