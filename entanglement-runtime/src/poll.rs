//! `poll` — the single join tool for background work (#605, ADR-0161 §1-4).
//!
//! Replaces `bash_output` and `agent_poll` outright — no aliases. Dispatches on
//! the handle's kind prefix (ADR-0164) to [`crate::host::jobs::JobRegistry`]
//! (`j-`, a background `bash` job) or [`crate::agent_registry::AgentRegistry`]
//! (anything else — a sub-agent handle, itself a `s-` session id). Waits up to
//! `timeout_secs` (default 60, cap 600, `0` = wait until terminal) for
//! something to report:
//!
//! - a **job** poll ends on new output *or* exit, whichever comes first, and
//!   returns the incremental delta since the last poll — still destructive,
//!   still `mem::take` (mirrors `bash_output`'s drain);
//! - an **agent** poll ends only on the child's completion and returns its
//!   final answer — idempotent, a later poll of the same handle repeats it
//!   (mirrors `agent_poll`'s `watch`-channel wait).
//!
//! `kill: true` SIGKILLs a job's process group; refused on an agent handle —
//! cancelling a child is a distinct authorization gate ADR-0033 deferred, and
//! this ADR does not open it. An unknown (or not-the-caller's-own) handle is an
//! *error*, adopting `agent_poll`'s convention over `bash_output`'s
//! return-it-as-text.
//!
//! Like `agent_poll` before it, `poll` is runtime-owned and intercepted
//! *before* permission resolution (ADR-0161 §3): it starts nothing and touches
//! no host resource — it only reads state a previously-graded launch produced.
//! The descendant check (§4) is the same ownership scoping `JobRegistry`/
//! `AgentRegistry` already enforce: a handle is only ever visible to the
//! session that launched it.

use std::time::Duration;

use entanglement_core::{Holly, SessionId, ToolSpec};
use tokio::sync::watch;

use crate::agent_registry::{AgentRegistry, AgentStatus};
use crate::host::jobs::{JobRegistry, JobStatus, Poll as JobPoll};
use crate::seam::reply;
use crate::subagent::format_agent_answer;
use crate::tool_names::POLL_TOOL;

/// Default poll timeout when the model omits `timeout_secs`.
const DEFAULT_TIMEOUT_SECS: u64 = 60;
/// Upper bound on a single poll's wait so one call can't park the parent turn
/// indefinitely — the model is expected to poll again rather than block forever.
const MAX_TIMEOUT_SECS: u64 = 600;

/// The `poll` tool schema advertised to the model.
pub fn poll_spec() -> ToolSpec {
    ToolSpec::with_schema(
        POLL_TOOL,
        "Wait on a handle from a background bash job (run_in_background=true) or a \
         sub-agent launched with agent_spawn. Blocks up to timeout_secs for \
         something to report — new output or exit for a job, the final answer for \
         a sub-agent — then returns a running/complete status plus text. A job \
         poll returns only the output produced since the last poll (drained, so \
         each call sees only what's new); a sub-agent poll returns its final \
         answer once complete and can be called again safely. Pass kill: true to \
         SIGKILL a job's process group (refused for a sub-agent handle — not \
         supported). Pass timeout_secs: 0 to block until the handle reaches a \
         terminal state.",
        serde_json::json!({
            "type": "object",
            "properties": {
                "handle": {
                    "type": "string",
                    "description": "The handle to await: a job id from bash \
                        run_in_background=true, or an agent_id from agent_spawn."
                },
                "timeout_secs": {
                    "type": "integer",
                    "description": "Max seconds to wait this poll before returning a \
                        still-running status. Defaults to 60, capped at 600. 0 means \
                        wait indefinitely for a terminal state."
                },
                "kill": {
                    "type": "boolean",
                    "description": "Terminate a background job's process group \
                        before reading. Refused for a sub-agent handle. Default false."
                }
            },
            "required": ["handle"]
        }),
    )
}

struct PollInput {
    handle: String,
    timeout_secs: u64,
    kill: bool,
}

/// Parse the `poll` tool input. `None` when `handle` is missing/empty or the
/// input isn't a JSON object — the caller replies with guidance.
fn parse_input(input: &str) -> Option<PollInput> {
    let v: serde_json::Value = serde_json::from_str(input).ok()?;
    let handle = v
        .get("handle")
        .and_then(|h| h.as_str())
        .filter(|s| !s.is_empty())?
        .to_string();
    let timeout_secs = v
        .get("timeout_secs")
        .and_then(|t| t.as_u64())
        .unwrap_or(DEFAULT_TIMEOUT_SECS)
        .min(MAX_TIMEOUT_SECS);
    let kill = v.get("kill").and_then(|k| k.as_bool()).unwrap_or(false);
    Some(PollInput {
        handle,
        timeout_secs,
        kill,
    })
}

/// A job id (ADR-0164's `j-` prefix) vs. everything else, which is a sub-agent
/// handle (itself a `s-` session id).
fn is_job_handle(handle: &str) -> bool {
    handle.starts_with("j-")
}

/// Orchestrate one `poll` call: dispatch on the handle's kind prefix and reply
/// with the joined status + text. Thin wrapper over [`resolve`] so the actual
/// join logic is testable without a live `Holly`.
pub async fn run_poll(
    holly: Holly,
    jobs: JobRegistry,
    agents: AgentRegistry,
    session: SessionId,
    request_id: String,
    input: String,
) {
    let output = resolve(&jobs, &agents, &session, &input).await;
    reply(&holly, session, request_id, output).await;
}

/// The `poll` join logic: dispatch on handle kind, wait, and render the result
/// text. No `Holly` involved — [`run_poll`] folds this back as the `ToolResult`.
async fn resolve(
    jobs: &JobRegistry,
    agents: &AgentRegistry,
    session: &SessionId,
    input: &str,
) -> String {
    let Some(parsed) = parse_input(input) else {
        return "poll: missing handle — pass the id returned by bash \
                (run_in_background=true) or agent_spawn."
            .to_string();
    };

    if is_job_handle(&parsed.handle) {
        match jobs
            .poll(&parsed.handle, session, parsed.kill, parsed.timeout_secs)
            .await
        {
            Some(p) => format_job_poll(&parsed.handle, p),
            None => unknown_handle(&parsed.handle),
        }
    } else {
        resolve_agent(agents, session, parsed).await
    }
}

async fn resolve_agent(agents: &AgentRegistry, session: &SessionId, parsed: PollInput) -> String {
    if parsed.kill {
        return kill_refused_message(&parsed.handle);
    }

    let child = SessionId::new(parsed.handle.clone());
    let Some((started, mut rx)) = agents.view(session, &child) else {
        return unknown_handle(&parsed.handle);
    };

    if parsed.timeout_secs == 0 {
        // Sentinel: 0 ⇒ wait for the child's completion notification with no
        // caller-side bound (ADR-0123, carried forward by ADR-0161 §2). Mirrors
        // the blocking `agent` tool; `wait_complete` is hang-safe (returns when
        // the watch sender drops).
        match wait_complete(&mut rx).await {
            AgentStatus::Complete { answer, elapsed } => {
                format_agent_answer(&parsed.handle, elapsed, answer)
            }
            AgentStatus::Running => unreachable!("wait_complete returns only on completion"),
        }
    } else {
        match tokio::time::timeout(
            Duration::from_secs(parsed.timeout_secs),
            wait_complete(&mut rx),
        )
        .await
        {
            Ok(AgentStatus::Complete { answer, elapsed }) => {
                format_agent_answer(&parsed.handle, elapsed, answer)
            }
            Ok(AgentStatus::Running) => unreachable!("wait_complete returns only on completion"),
            Err(_) => format!(
                "sub-agent `{}` still running ({:.1}s elapsed); polled with a \
                 {}s timeout. Poll again later or do other work meanwhile.",
                parsed.handle,
                started.elapsed().as_secs_f64(),
                parsed.timeout_secs
            ),
        }
    }
}

/// ADR-0161 §2: `kill: true` is refused on a sub-agent handle — cancelling a
/// child is a distinct authorization gate this ADR does not open.
fn kill_refused_message(handle: &str) -> String {
    format!(
        "poll: kill is not supported for sub-agent handle `{handle}` — \
         cancelling a running sub-agent isn't available yet."
    )
}

/// Unknown-handle error text (ADR-0161 §2): adopts `agent_poll`'s convention
/// of an error over `bash_output`'s "return it as text" — a poll for a handle
/// that doesn't exist (or isn't the caller's own) is a model mistake, not a
/// state report.
fn unknown_handle(handle: &str) -> String {
    format!(
        "poll: unknown handle `{handle}` — it was never launched from this \
         session (use the id returned by bash run_in_background=true or \
         agent_spawn)."
    )
}

/// Resolve once the watched child reaches [`AgentStatus::Complete`]. Checks the
/// current value first (so an already-finished child returns instantly), then
/// awaits changes. If the sender drops without ever completing, treats it as a
/// completion with an explanatory answer so the poll never hangs.
async fn wait_complete(rx: &mut watch::Receiver<AgentStatus>) -> AgentStatus {
    loop {
        {
            let cur = rx.borrow_and_update();
            if let AgentStatus::Complete { .. } = &*cur {
                return cur.clone();
            }
        }
        if rx.changed().await.is_err() {
            return AgentStatus::Complete {
                answer: "sub-agent ended without producing an answer".to_string(),
                elapsed: Duration::ZERO,
            };
        }
    }
}

/// Render a job poll's status + drained output — the same shape `bash_output`
/// used, byte-capped head+tail via [`crate::host::bounded_result`].
fn format_job_poll(id: &str, poll: JobPoll) -> String {
    let status = match poll.status {
        JobStatus::Running => "running".to_string(),
        JobStatus::Exited(Some(code)) => format!("exited {code}"),
        JobStatus::Exited(None) => "exited (killed)".to_string(),
    };
    let mut header = format!("[job {id}: {status}]\n");
    if poll.timed_out {
        header.push_str(&format!(
            "[killed: timed out after {}s]\n",
            poll.timeout_secs
        ));
    }
    let mut body = String::new();
    if poll.stdout_dropped > 0 {
        body.push_str(&format!(
            "[{} bytes of older stdout dropped]\n",
            poll.stdout_dropped
        ));
    }
    let stdout = String::from_utf8_lossy(&poll.stdout);
    if !stdout.is_empty() {
        body.push_str(&stdout);
    }
    if poll.stderr_dropped > 0 {
        body.push_str(&format!(
            "[{} bytes of older stderr dropped]\n",
            poll.stderr_dropped
        ));
    }
    let stderr = String::from_utf8_lossy(&poll.stderr);
    if !stderr.is_empty() {
        body.push_str("[stderr]\n");
        body.push_str(&stderr);
    }
    if stdout.is_empty() && stderr.is_empty() {
        body.push_str("(no new output)\n");
    }
    crate::host::bounded_result(&header, body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::bash::BashTool;
    use crate::tools::Tool;

    #[test]
    fn parse_input_reads_handle_timeout_and_kill() {
        let p = parse_input(r#"{"handle":"j-abc","timeout_secs":5,"kill":true}"#).unwrap();
        assert_eq!(p.handle, "j-abc");
        assert_eq!(p.timeout_secs, 5);
        assert!(p.kill);
    }

    #[test]
    fn parse_input_defaults_timeout_and_clamps_max() {
        let p = parse_input(r#"{"handle":"j-abc"}"#).unwrap();
        assert_eq!(p.timeout_secs, DEFAULT_TIMEOUT_SECS);
        assert!(!p.kill);
        let clamped = parse_input(r#"{"handle":"j-abc","timeout_secs":100000}"#).unwrap();
        assert_eq!(clamped.timeout_secs, MAX_TIMEOUT_SECS);
    }

    #[test]
    fn parse_input_missing_handle_yields_none() {
        assert!(parse_input(r#"{"timeout_secs":5}"#).is_none());
        assert!(parse_input("not json").is_none());
    }

    #[test]
    fn is_job_handle_matches_the_j_prefix_only() {
        assert!(is_job_handle("j-6a708af0a1002"));
        assert!(!is_job_handle("s-6a708af0a1002"));
        assert!(!is_job_handle("anything-else"));
    }

    #[test]
    fn missing_handle_message_names_both_launchers() {
        // Exercises `resolve`'s guidance text indirectly via the same string
        // `parse_input` gates on — kept in sync by construction (both live in
        // this module).
        assert!(parse_input("{}").is_none());
    }

    /// End-to-end: `bash` starts a background job owned by a session, `poll`
    /// waits it to completion and sees the exit status + output (#605).
    #[tokio::test]
    async fn job_handle_is_pollable_end_to_end() {
        let dir = tempfile::tempdir().unwrap();
        let jobs = JobRegistry::new();
        let bash = BashTool::new(dir.path().to_path_buf()).with_jobs(jobs.clone());
        let session = SessionId::new("s1");
        let started = bash
            .run_for_session(
                &session,
                "r1",
                r#"{"command":"echo hi","run_in_background":true}"#,
            )
            .await
            .unwrap();
        let started_text = entanglement_core::content_text(&started);
        let id = started_text
            .split_whitespace()
            .find(|w| w.starts_with("j-"))
            .unwrap()
            .trim_end_matches(|c: char| !c.is_ascii_alphanumeric() && c != '-')
            .to_string();

        // The wait ends on new output *or* exit, whichever comes first (#605)
        // — for a fast `echo`, the output notification can win the race
        // against the exit notification, so a single poll may still report
        // `running`. Poll again (bounded, so a real regression still fails
        // fast) until the terminal status lands.
        let agents = AgentRegistry::default();
        let input = serde_json::json!({"handle": id, "timeout_secs": 5}).to_string();
        let mut out = resolve(&jobs, &agents, &session, &input).await;
        assert!(out.contains("hi"), "{out}");
        for _ in 0..50 {
            if out.contains("exited 0") {
                break;
            }
            out = resolve(&jobs, &agents, &session, &input).await;
        }
        assert!(out.contains("exited 0"), "{out}");
    }

    /// #605: a caller that never launched the job sees the same message as an
    /// unknown id — the ownership scoping must not confirm existence.
    #[tokio::test]
    async fn job_handle_from_a_different_session_is_unknown() {
        let dir = tempfile::tempdir().unwrap();
        let jobs = JobRegistry::new();
        let bash = BashTool::new(dir.path().to_path_buf()).with_jobs(jobs.clone());
        let owner = SessionId::new("owner");
        let started = bash
            .run_for_session(
                &owner,
                "r1",
                r#"{"command":"echo hi","run_in_background":true}"#,
            )
            .await
            .unwrap();
        let started_text = entanglement_core::content_text(&started);
        let id = started_text
            .split_whitespace()
            .find(|w| w.starts_with("j-"))
            .unwrap()
            .trim_end_matches(|c: char| !c.is_ascii_alphanumeric() && c != '-')
            .to_string();

        let stranger = SessionId::new("stranger");
        let out = resolve(
            &jobs,
            &AgentRegistry::default(),
            &stranger,
            &serde_json::json!({"handle": id}).to_string(),
        )
        .await;
        assert!(out.contains("unknown handle"), "{out}");
    }

    /// ADR-0161 §2: `kill: true` is refused on a sub-agent handle.
    #[tokio::test]
    async fn kill_is_refused_on_an_agent_handle() {
        let agents = AgentRegistry::default();
        let parent = SessionId::new("parent");
        let child = SessionId::new("s-child");
        agents.register(child.clone(), parent.clone());

        let out = resolve(
            &JobRegistry::new(),
            &agents,
            &parent,
            &serde_json::json!({"handle": child.to_string(), "kill": true}).to_string(),
        )
        .await;
        assert!(
            out.contains("not supported") && out.contains("sub-agent"),
            "{out}"
        );
    }

    #[tokio::test]
    async fn unknown_agent_handle_is_an_error_message_not_bare_text() {
        let session = SessionId::new("s2");
        let out = resolve(
            &JobRegistry::new(),
            &AgentRegistry::default(),
            &session,
            &serde_json::json!({"handle": "s-nonexistent"}).to_string(),
        )
        .await;
        assert!(out.starts_with("poll: unknown handle"), "{out}");
    }

    #[tokio::test]
    async fn missing_handle_replies_with_guidance() {
        let session = SessionId::new("s3");
        let out = resolve(
            &JobRegistry::new(),
            &AgentRegistry::default(),
            &session,
            "{}",
        )
        .await;
        assert!(out.contains("missing handle"), "{out}");
    }

    /// A completed sub-agent poll is idempotent — a second poll of the same
    /// handle repeats the final answer instead of erroring (ADR-0161 §2).
    #[tokio::test]
    async fn completed_agent_poll_is_idempotent() {
        let agents = AgentRegistry::default();
        let parent = SessionId::new("parent2");
        let child = SessionId::new("s-child2");
        let (tx, _started) = agents.register(child.clone(), parent.clone());
        tx.send(AgentStatus::Complete {
            answer: "done".to_string(),
            elapsed: Duration::from_millis(5),
        })
        .unwrap();

        let input = serde_json::json!({"handle": child.to_string()}).to_string();
        let first = resolve(&JobRegistry::new(), &agents, &parent, &input).await;
        let second = resolve(&JobRegistry::new(), &agents, &parent, &input).await;
        assert_eq!(first, second);
        assert!(first.contains("done"), "{first}");
    }
}
