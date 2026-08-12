//! `poll` — the single join tool for background work (#605, ADR-0161 §1-4).
//!
//! Replaces `bash_output` and `agent_poll` outright — no aliases (and, with
//! #606, is the only way to join a `background: true` `bash`/`call`/`agent`/
//! `rhai` launch — `agent_spawn` is gone). Dispatches on
//! the handle's kind prefix (ADR-0164) to [`crate::host::jobs::JobRegistry`]
//! (`j-`, a background `bash` job),
//! [`crate::script_ops::ScriptRegistry`] (`x-`, a background `rhai` script —
//! #637, ADR-0185),
//! [`crate::retained_output::RetainedOutputRegistry`] (`o-`, a completed
//! operation's output that overflowed its cap — #608, ADR-0161 §7), or
//! [`crate::agent_registry::AgentRegistry`] (anything else — a sub-agent
//! handle, itself a `s-` session id). Waits up to `timeout_secs` (default 60,
//! cap 600, `0` = wait until terminal) for something to report:
//!
//! - a **job** poll ends on new output *or* exit, whichever comes first, and
//!   returns the incremental delta since the last poll — still destructive,
//!   still `mem::take` (mirrors `bash_output`'s drain);
//! - a **script** poll has exactly the job contract — new `print` output or
//!   finish ends the wait, each read drains the delta, and the terminal poll
//!   carries the script's final `=> value` / error line;
//! - a **retained-output** poll never waits (the operation already finished)
//!   and instead pages the text with `offset`/`tail` — see below;
//! - an **agent** poll ends only on the child's completion and returns its
//!   final answer — idempotent, a later poll of the same handle repeats it
//!   (mirrors `agent_poll`'s `watch`-channel wait).
//!
//! `kill: true` SIGKILLs a job's process group; on a script handle it is
//! **cooperative** — it trips the stop flag the engine's progress callback
//! polls, so the script ends at its next operation (an in-flight `exec`/`bash`
//! binding finishes its own budget-clamped timeout first, the documented
//! ADR-0161 §5 limit). Refused on an agent or
//! retained-output handle — cancelling a child is a distinct authorization
//! gate ADR-0033 deferred, and a retained-output entry has nothing running to
//! kill. An unknown (or not-the-caller's-own) handle is an *error*, adopting
//! `agent_poll`'s convention over `bash_output`'s return-it-as-text. These
//! model-mistake branches — unknown handle, refused `kill` — and a script's
//! own terminal error also set the structured `is_error` flag on the
//! `ToolResult` (#695, ADR-0176's deferred remainder); every outcome of a
//! poll that ran (running/complete/list/paged, a job exiting nonzero, a
//! killed job or script) stays `false`.
//!
//! **`offset`/`tail` page a retained-output handle** (#608, ADR-0161 §7): the
//! operation registry that already holds a completed operation now holds its
//! output too, so the model can read the remainder without a scratch file.
//! `offset` (default 0) is a 0-based line index into the retained text;
//! `tail` (default 30, matching `call`'s own default and for the same reason —
//! value concentrates at the end) is how many lines to return from there,
//! `0` meaning the rest, still byte-capped. A retained entry whose operation
//! had an explicit `output_file` (a real file the model asked for, not a
//! truncation workaround) is named instead of paged — the one place a path
//! still appears. `offset`/`tail` are ignored for job/agent handles, which
//! have their own shape.
//!
//! Called with **no `handle`** (#607, ADR-0161 §6), `poll` instead lists this
//! session's own pending operations — every outstanding job/sub-agent it
//! launched, with kind, handle, launcher, elapsed time, and status — via
//! [`crate::operations::list_operations`], the same listing
//! `InMsg::ListOperations`/`OutEvent::OperationList` expose to a head.
//!
//! Like `agent_poll` before it, `poll` is runtime-owned and intercepted
//! *before* permission resolution (ADR-0161 §3): it starts nothing and touches
//! no host resource — it only reads state a previously-graded launch produced.
//! The descendant check (§4) is the same ownership scoping `JobRegistry`/
//! `AgentRegistry`/`RetainedOutputRegistry` already enforce: a handle is only
//! ever visible to the session that launched it.

mod format;
mod retained;

use std::time::Duration;

use entanglement_core::{Holly, SessionId, ToolSpec};
use tokio::sync::watch;

use crate::agent_registry::{AgentRegistry, AgentStatus};
use crate::host::jobs::{JobRegistry, JobStatus};
use crate::retained_output::RetainedOutputRegistry;
use crate::script_ops::ScriptRegistry;
use crate::seam;
use crate::subagent::format_agent_answer;
use crate::tool_names::POLL_TOOL;
use format::{
    format_job_poll, kill_refused_message, script_poll_outcome, unknown_handle, PollOutcome,
};
use retained::{is_retained_handle, resolve_retained};

/// Default poll timeout when the model omits `timeout_secs`.
const DEFAULT_TIMEOUT_SECS: u64 = 60;
/// Upper bound on a single poll's wait so one call can't park the parent turn
/// indefinitely — the model is expected to poll again rather than block forever.
const MAX_TIMEOUT_SECS: u64 = 600;
/// Default page size for a retained-output poll (#608) — the same default
/// `call`'s own `tail` uses, for the same reason (value concentrates at the
/// end of a normal result, so an unbounded first page would reintroduce the
/// context blow-up the cap exists to prevent).
const DEFAULT_TAIL: u32 = crate::host::DEFAULT_TAIL;

/// The `poll` tool schema advertised to the model.
pub fn poll_spec() -> ToolSpec {
    ToolSpec::with_schema(
        POLL_TOOL,
        "Wait on a handle from a background bash/call job (background=true), a \
         background rhai script (background=true), a \
         sub-agent launched with agent (background=true), or a completed \
         operation's retained output (returned alongside a truncated call \
         result). Blocks up to timeout_secs for something to report — new \
         output or exit for a job or script, the final answer for a sub-agent \
         — then \
         returns a running/complete status plus text. A job or script poll \
         returns only \
         the output produced since the last poll (drained, so each call sees \
         only what's new; a script's terminal poll carries its final => \
         result line); a sub-agent poll returns its final answer once \
         complete and can be called again safely; a retained-output poll \
         never waits (the operation already finished) and instead pages the \
         text with offset/tail — offset (default 0) is a 0-based line index, \
         tail (default 30, matching call's own default) is how many lines to \
         return from there, 0 meaning the rest (still byte-capped). When the \
         operation had an explicit output_file, the poll result names that \
         path instead of paging text. Pass kill: true to SIGKILL a job's \
         process group, or to cooperatively stop a script — it ends at its \
         next operation, after any in-flight exec/bash binding finishes \
         (refused for a sub-agent or retained-output handle — \
         not supported). Pass timeout_secs: 0 to block until the handle \
         reaches a terminal state. Call with no handle at all to instead list \
         this session's own pending operations — every background \
         job/script/sub-agent still outstanding, with kind, handle, launcher, \
         elapsed time, and status.",
        serde_json::json!({
            "type": "object",
            "properties": {
                "handle": {
                    "type": "string",
                    "description": "The handle to await: a job id from bash/call \
                        background=true, a script id from rhai background=true, an \
                        agent_id from agent background=true, or a \
                        retained-output id returned alongside a truncated call result. \
                        Omit to list this session's pending operations instead."
                },
                "timeout_secs": {
                    "type": "integer",
                    "description": "Max seconds to wait this poll before returning a \
                        still-running status. Defaults to 60, capped at 600. 0 means \
                        wait indefinitely for a terminal state. Ignored for a \
                        retained-output handle, which never waits."
                },
                "kill": {
                    "type": "boolean",
                    "description": "Terminate a background job's process group \
                        before reading, or request a background script's \
                        cooperative stop. Refused for a sub-agent or retained-output \
                        handle. Default false."
                },
                "offset": {
                    "type": "integer",
                    "minimum": 0,
                    "description": "0-based line index to start paging a \
                        retained-output handle's text from. Default 0. Ignored for \
                        job/agent handles."
                },
                "tail": {
                    "type": "integer",
                    "minimum": 0,
                    "description": "How many lines to return from offset for a \
                        retained-output handle (default 30). 0 returns the rest, \
                        still byte-capped. Ignored for job/agent handles."
                }
            }
        }),
    )
}

struct PollInput {
    handle: String,
    timeout_secs: u64,
    kill: bool,
    offset: u32,
    tail: u32,
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
    let offset = v
        .get("offset")
        .and_then(|o| o.as_u64())
        .unwrap_or(0)
        .min(u32::MAX as u64) as u32;
    let tail = v
        .get("tail")
        .and_then(|t| t.as_u64())
        .unwrap_or(DEFAULT_TAIL as u64)
        .min(u32::MAX as u64) as u32;
    Some(PollInput {
        handle,
        timeout_secs,
        kill,
        offset,
        tail,
    })
}

/// A job id (ADR-0164's `j-` prefix).
fn is_job_handle(handle: &str) -> bool {
    handle.starts_with("j-")
}

/// A background-script id (ADR-0164's `x-` prefix, #637).
fn is_script_handle(handle: &str) -> bool {
    handle.starts_with("x-")
}

/// Orchestrate one `poll` call: dispatch on the handle's kind prefix and reply
/// with the joined status + text. Thin wrapper over [`resolve`] so the actual
/// join logic is testable without a live `Holly`.
#[allow(clippy::too_many_arguments)]
pub async fn run_poll(
    holly: Holly,
    jobs: JobRegistry,
    agents: AgentRegistry,
    retained: RetainedOutputRegistry,
    scripts: ScriptRegistry,
    session: SessionId,
    request_id: String,
    input: String,
) {
    let outcome = resolve(&jobs, &agents, &retained, &scripts, &session, &input).await;
    // The structured side channel (#636/ADR-0176, #681/ADR-0186, #695):
    // `is_error` marks the branches where the requested call never ran — an
    // unknown handle, a refused `kill`, a script's own terminal error —
    // while a poll that ran and reports state stays `false`. `exit_code`
    // rides alongside, orthogonal: `Some` only when this poll observed a job
    // (`j-`) exit with a real status.
    seam::reply_content(
        &holly,
        session,
        request_id,
        crate::tools::text_parts(outcome.text),
        outcome.is_error,
        None,
        outcome.exit_code,
    )
    .await;
}

/// The `poll` join logic: dispatch on handle kind, wait, and classify the
/// result ([`PollOutcome`]) — the rendered text, the numeric exit code for a
/// job (`j-`) poll that observed the child exit with a real status (#681,
/// ADR-0186), and the `is_error` flag for the model-mistake branches (#695).
/// No `Holly` involved — [`run_poll`] folds this back as the `ToolResult`.
async fn resolve(
    jobs: &JobRegistry,
    agents: &AgentRegistry,
    retained: &RetainedOutputRegistry,
    scripts: &ScriptRegistry,
    session: &SessionId,
    input: &str,
) -> PollOutcome {
    let Some(parsed) = parse_input(input) else {
        // No handle (#607, ADR-0161 §6): list this session's own pending
        // operations instead of joining a single one. Malformed input lands
        // here too, deliberately — the listing is the recovery path.
        let ops = crate::operations::list_operations(jobs, agents, scripts, Some(session));
        return PollOutcome::ok(crate::operations::format_operations(&ops));
    };

    if is_job_handle(&parsed.handle) {
        return match jobs
            .poll(&parsed.handle, session, parsed.kill, parsed.timeout_secs)
            .await
        {
            Some(p) => {
                let exit_code = match p.status {
                    JobStatus::Exited(code) => code,
                    JobStatus::Running => None,
                };
                PollOutcome::exited(format_job_poll(&parsed.handle, p), exit_code)
            }
            None => PollOutcome::err(unknown_handle(&parsed.handle)),
        };
    }

    if is_script_handle(&parsed.handle) {
        match scripts
            .poll(&parsed.handle, session, parsed.kill, parsed.timeout_secs)
            .await
        {
            Some(p) => script_poll_outcome(&parsed.handle, p, parsed.kill),
            None => PollOutcome::err(unknown_handle(&parsed.handle)),
        }
    } else if is_retained_handle(&parsed.handle) {
        resolve_retained(retained, session, &parsed)
    } else {
        resolve_agent(agents, retained, session, parsed).await
    }
}

async fn resolve_agent(
    agents: &AgentRegistry,
    retained: &RetainedOutputRegistry,
    session: &SessionId,
    parsed: PollInput,
) -> PollOutcome {
    if parsed.kill {
        return PollOutcome::err(kill_refused_message(&parsed.handle));
    }

    let child = SessionId::new(parsed.handle.clone());
    let Some((started, mut rx)) = agents.view(session, &child) else {
        return PollOutcome::err(unknown_handle(&parsed.handle));
    };

    if parsed.timeout_secs == 0 {
        // Sentinel: 0 ⇒ wait for the child's completion notification with no
        // caller-side bound (ADR-0123, carried forward by ADR-0161 §2). Mirrors
        // the blocking `agent` tool; `wait_complete` is hang-safe (returns when
        // the watch sender drops).
        match wait_complete(&mut rx).await {
            AgentStatus::Complete { answer, elapsed } => PollOutcome::ok(format_agent_answer(
                &parsed.handle,
                elapsed,
                answer,
                retained,
                Some(session),
            )),
            AgentStatus::Running => unreachable!("wait_complete returns only on completion"),
        }
    } else {
        match tokio::time::timeout(
            Duration::from_secs(parsed.timeout_secs),
            wait_complete(&mut rx),
        )
        .await
        {
            Ok(AgentStatus::Complete { answer, elapsed }) => PollOutcome::ok(format_agent_answer(
                &parsed.handle,
                elapsed,
                answer,
                retained,
                Some(session),
            )),
            Ok(AgentStatus::Running) => unreachable!("wait_complete returns only on completion"),
            Err(_) => PollOutcome::ok(format!(
                "sub-agent `{}` still running ({:.1}s elapsed); polled with a \
                 {}s timeout. Poll again later or do other work meanwhile.",
                parsed.handle,
                started.elapsed().as_secs_f64(),
                parsed.timeout_secs
            )),
        }
    }
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

    /// #608: `offset` defaults to 0, `tail` defaults to 30 (matching `call`).
    #[test]
    fn parse_input_defaults_offset_and_tail() {
        let p = parse_input(r#"{"handle":"o-abc"}"#).unwrap();
        assert_eq!(p.offset, 0);
        assert_eq!(p.tail, DEFAULT_TAIL);
        assert_eq!(DEFAULT_TAIL, 30);

        let explicit = parse_input(r#"{"handle":"o-abc","offset":30,"tail":50}"#).unwrap();
        assert_eq!(explicit.offset, 30);
        assert_eq!(explicit.tail, 50);
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
        assert!(!is_job_handle("o-6a708af0a1002"));
        assert!(!is_job_handle("anything-else"));
    }

    #[test]
    fn is_script_handle_matches_the_x_prefix_only() {
        assert!(is_script_handle("x-6a708af0a1002"));
        assert!(!is_script_handle("j-6a708af0a1002"));
        assert!(!is_script_handle("s-6a708af0a1002"));
    }

    /// #637: a script handle resolves through the `x-` arm — a running poll
    /// drains the streamed delta, the terminal poll reports the final state.
    #[tokio::test]
    async fn script_handle_polls_delta_then_terminal_state() {
        let scripts = ScriptRegistry::new();
        let session = SessionId::new("s-script");
        let (id, op) = scripts.register(
            "rhai: print(1)".to_string(),
            Some(session.clone()),
            Duration::from_secs(120),
            std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        );

        op.append_output("streamed\n");
        let input = serde_json::json!({"handle": id, "timeout_secs": 1}).to_string();
        let running = resolve(
            &JobRegistry::new(),
            &AgentRegistry::default(),
            &RetainedOutputRegistry::new(),
            &scripts,
            &session,
            &input,
        )
        .await;
        assert!(
            running.text.contains(&format!("[script {id}: running]")),
            "{}",
            running.text
        );
        assert!(running.text.contains("streamed"), "{}", running.text);
        assert!(!running.is_error);

        op.finish("=> 42", false);
        let done = resolve(
            &JobRegistry::new(),
            &AgentRegistry::default(),
            &RetainedOutputRegistry::new(),
            &scripts,
            &session,
            &input,
        )
        .await;
        assert!(
            done.text.contains(&format!("[script {id}: done]")),
            "{}",
            done.text
        );
        assert!(done.text.contains("=> 42"), "{}", done.text);
        assert!(!done.is_error);
    }

    /// #695: a script that died in an error (uncaught exception / deadline)
    /// classifies as `is_error` — the same call the blocking `rhai` path
    /// would have reported as an error (#636). Contrast with the cooperative
    /// stop below, which stays a state report.
    #[tokio::test]
    async fn script_terminal_error_sets_is_error() {
        let scripts = ScriptRegistry::new();
        let session = SessionId::new("s-script3");
        let (id, op) = scripts.register(
            "rhai: boom()".to_string(),
            Some(session.clone()),
            Duration::from_secs(120),
            std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        );

        op.finish("error: boom", true);
        let out = resolve(
            &JobRegistry::new(),
            &AgentRegistry::default(),
            &RetainedOutputRegistry::new(),
            &scripts,
            &session,
            &serde_json::json!({"handle": id, "timeout_secs": 1}).to_string(),
        )
        .await;
        assert!(
            out.text.contains(&format!("[script {id}: error]")),
            "{}",
            out.text
        );
        assert!(out.is_error);
        assert_eq!(out.exit_code, None);
    }

    /// #637: `kill: true` on a script handle is accepted (cooperative), not
    /// refused like an agent handle — and says the stop was *requested*.
    #[tokio::test]
    async fn script_kill_is_cooperative_not_refused() {
        let scripts = ScriptRegistry::new();
        let session = SessionId::new("s-script2");
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (id, _op) = scripts.register(
            "rhai: loop {}".to_string(),
            Some(session.clone()),
            Duration::from_secs(120),
            stop.clone(),
        );

        let out = resolve(
            &JobRegistry::new(),
            &AgentRegistry::default(),
            &RetainedOutputRegistry::new(),
            &scripts,
            &session,
            &serde_json::json!({"handle": id, "kill": true}).to_string(),
        )
        .await;
        assert!(!out.text.contains("not supported"), "{}", out.text);
        assert!(
            out.text.contains("cooperative stop requested"),
            "{}",
            out.text
        );
        assert!(stop.load(std::sync::atomic::Ordering::SeqCst));
        // #695: an accepted cooperative stop is a state report, not an error.
        assert!(!out.is_error);
    }

    /// #637: an unknown script handle is the standard poll error — and sets
    /// the structured `is_error` flag (#695).
    #[tokio::test]
    async fn unknown_script_handle_is_an_error() {
        let out = resolve(
            &JobRegistry::new(),
            &AgentRegistry::default(),
            &RetainedOutputRegistry::new(),
            &ScriptRegistry::new(),
            &SessionId::new("s1"),
            &serde_json::json!({"handle": "x-nonexistent"}).to_string(),
        )
        .await;
        assert!(out.text.starts_with("poll: unknown handle"), "{}", out.text);
        assert!(out.is_error);
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
            .run_for_session(&session, "r1", r#"{"command":"echo hi","background":true}"#)
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
        let retained = RetainedOutputRegistry::new();
        let input = serde_json::json!({"handle": id, "timeout_secs": 5}).to_string();
        let mut out = resolve(
            &jobs,
            &agents,
            &retained,
            &ScriptRegistry::new(),
            &session,
            &input,
        )
        .await;
        assert!(out.text.contains("hi"), "{}", out.text);
        for _ in 0..50 {
            if out.text.contains("exited 0") {
                break;
            }
            out = resolve(
                &jobs,
                &agents,
                &retained,
                &ScriptRegistry::new(),
                &session,
                &input,
            )
            .await;
        }
        assert!(out.text.contains("exited 0"), "{}", out.text);
        // #681, ADR-0186: the terminal poll carries the job's numeric status
        // as a real field, not only the `exited 0` text.
        assert_eq!(out.exit_code, Some(0));
        assert!(!out.is_error);
    }

    /// #695 pins ADR-0186's orthogonality rule: a job exiting nonzero still
    /// executed — the poll reports it as content (`is_error: false`) with the
    /// real status in `exit_code`.
    #[tokio::test]
    async fn job_nonzero_exit_is_content_not_error() {
        let dir = tempfile::tempdir().unwrap();
        let jobs = JobRegistry::new();
        let bash = BashTool::new(dir.path().to_path_buf()).with_jobs(jobs.clone());
        let session = SessionId::new("s-exit7");
        let started = bash
            .run_for_session(&session, "r1", r#"{"command":"exit 7","background":true}"#)
            .await
            .unwrap();
        let started_text = entanglement_core::content_text(&started);
        let id = started_text
            .split_whitespace()
            .find(|w| w.starts_with("j-"))
            .unwrap()
            .trim_end_matches(|c: char| !c.is_ascii_alphanumeric() && c != '-')
            .to_string();

        let input = serde_json::json!({"handle": id, "timeout_secs": 5}).to_string();
        let mut out = resolve(
            &jobs,
            &AgentRegistry::default(),
            &RetainedOutputRegistry::new(),
            &ScriptRegistry::new(),
            &session,
            &input,
        )
        .await;
        for _ in 0..50 {
            if out.text.contains("exited 7") {
                break;
            }
            out = resolve(
                &jobs,
                &AgentRegistry::default(),
                &RetainedOutputRegistry::new(),
                &ScriptRegistry::new(),
                &session,
                &input,
            )
            .await;
        }
        assert!(out.text.contains("exited 7"), "{}", out.text);
        assert_eq!(out.exit_code, Some(7));
        assert!(!out.is_error);
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
            .run_for_session(&owner, "r1", r#"{"command":"echo hi","background":true}"#)
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
            &RetainedOutputRegistry::new(),
            &ScriptRegistry::new(),
            &stranger,
            &serde_json::json!({"handle": id}).to_string(),
        )
        .await;
        assert!(out.text.contains("unknown handle"), "{}", out.text);
        assert!(out.is_error);
    }

    /// ADR-0161 §2: `kill: true` is refused on a sub-agent handle — an error
    /// in the structured side channel too (#695).
    #[tokio::test]
    async fn kill_is_refused_on_an_agent_handle() {
        let agents = AgentRegistry::default();
        let parent = SessionId::new("parent");
        let child = SessionId::new("s-child");
        agents.register(child.clone(), parent.clone(), "build".to_string());

        let out = resolve(
            &JobRegistry::new(),
            &agents,
            &RetainedOutputRegistry::new(),
            &ScriptRegistry::new(),
            &parent,
            &serde_json::json!({"handle": child.to_string(), "kill": true}).to_string(),
        )
        .await;
        assert!(
            out.text.contains("not supported") && out.text.contains("sub-agent"),
            "{}",
            out.text
        );
        assert!(out.is_error);
    }

    #[tokio::test]
    async fn unknown_agent_handle_is_an_error_message_not_bare_text() {
        let session = SessionId::new("s2");
        let out = resolve(
            &JobRegistry::new(),
            &AgentRegistry::default(),
            &RetainedOutputRegistry::new(),
            &ScriptRegistry::new(),
            &session,
            &serde_json::json!({"handle": "s-nonexistent"}).to_string(),
        )
        .await;
        assert!(out.text.starts_with("poll: unknown handle"), "{}", out.text);
        assert!(out.is_error);
    }

    /// #607, ADR-0161 §6: called with no handle at all, `poll` lists this
    /// session's pending operations instead of erroring — and the listing is
    /// never an `is_error` result (#695), even for malformed input.
    #[tokio::test]
    async fn no_handle_lists_pending_operations() {
        let session = SessionId::new("s3");
        let empty = resolve(
            &JobRegistry::new(),
            &AgentRegistry::default(),
            &RetainedOutputRegistry::new(),
            &ScriptRegistry::new(),
            &session,
            "{}",
        )
        .await;
        assert_eq!(empty.text, "poll: no pending operations for this session.");
        assert!(!empty.is_error);

        let agents = AgentRegistry::default();
        let child = SessionId::new("s-child9");
        agents.register(child.clone(), session.clone(), "reviewer".to_string());
        let retained = RetainedOutputRegistry::new();
        let out = resolve(
            &JobRegistry::new(),
            &agents,
            &retained,
            &ScriptRegistry::new(),
            &session,
            "{}",
        )
        .await;
        assert!(
            out.text.contains(&child.to_string()) && out.text.contains("reviewer"),
            "{}",
            out.text
        );

        // Malformed (non-JSON) input also falls through to the listing rather
        // than a parse error — `parse_input` gates both cases identically.
        let malformed = resolve(
            &JobRegistry::new(),
            &agents,
            &retained,
            &ScriptRegistry::new(),
            &session,
            "not json",
        )
        .await;
        assert!(
            malformed.text.contains(&child.to_string()),
            "{}",
            malformed.text
        );
        assert!(!malformed.is_error);
    }

    /// A completed sub-agent poll is idempotent — a second poll of the same
    /// handle repeats the final answer instead of erroring (ADR-0161 §2).
    #[tokio::test]
    async fn completed_agent_poll_is_idempotent() {
        let agents = AgentRegistry::default();
        let parent = SessionId::new("parent2");
        let child = SessionId::new("s-child2");
        let (tx, _started) = agents.register(child.clone(), parent.clone(), "build".to_string());
        tx.send(AgentStatus::Complete {
            answer: "done".to_string(),
            elapsed: Duration::from_millis(5),
        })
        .unwrap();

        let retained = RetainedOutputRegistry::new();
        let input = serde_json::json!({"handle": child.to_string()}).to_string();
        let first = resolve(
            &JobRegistry::new(),
            &agents,
            &retained,
            &ScriptRegistry::new(),
            &parent,
            &input,
        )
        .await;
        let second = resolve(
            &JobRegistry::new(),
            &agents,
            &retained,
            &ScriptRegistry::new(),
            &parent,
            &input,
        )
        .await;
        assert_eq!(first.text, second.text);
        assert!(first.text.contains("done"), "{}", first.text);
        assert!(!first.is_error);
    }
}
