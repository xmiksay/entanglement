//! Pending-operations listing (#607, ADR-0161 §6): "what do I still have
//! running" — the no-handle counterpart to `poll`'s single-handle "how is
//! this going." Attributes every outstanding background job and launched
//! sub-agent to the session that launched it, using the same ownership
//! bookkeeping [`JobRegistry`]/[`AgentRegistry`] already track for the
//! single-handle join (#605/#618) — one mechanism serving both. Shared by
//! `poll`'s no-handle model surface (always scoped to the calling session)
//! and the head-facing `InMsg::ListOperations` (optionally scoped, mirroring
//! `ListQuestions`'s filter convention).
//!
//! **Lifetimes differ by kind, deliberately not unified**: an agent handle is
//! itself a session, so it persists however that session's own bookkeeping
//! does; a background job is an OS process owned by this engine process and
//! cannot outlive it. The listing surfaces `kind` precisely so a caller can
//! render that distinction rather than treat every entry alike.

use entanglement_core::{OperationInfo, OperationKind, OperationStatus, SessionId};

use crate::agent_registry::{AgentRegistry, AgentStatus};
use crate::host::jobs::{JobRegistry, JobStatus};

/// Snapshot every pending job/agent operation, optionally scoped to one
/// session. `None` spans every session (the wire-level `InMsg::ListOperations`
/// engine-wide scope); `poll`'s own no-handle use always passes `Some`, since
/// a model call is inherently scoped to its own turn. Sorted by
/// `(session, handle)` for a deterministic reply.
pub fn list_operations(
    jobs: &JobRegistry,
    agents: &AgentRegistry,
    session: Option<&SessionId>,
) -> Vec<OperationInfo> {
    let job_ops = jobs.snapshot(session).into_iter().map(|j| OperationInfo {
        session: j.session,
        kind: OperationKind::Job,
        handle: j.handle,
        launched_by: j.command,
        elapsed_secs: j.elapsed.as_secs(),
        status: match j.status {
            JobStatus::Running => OperationStatus::Running,
            JobStatus::Exited(_) => OperationStatus::Complete,
        },
    });
    let agent_ops = agents.snapshot(session).into_iter().map(|a| OperationInfo {
        session: a.session,
        kind: OperationKind::Agent,
        handle: a.handle,
        launched_by: a.agent,
        elapsed_secs: a.elapsed.as_secs(),
        status: match a.status {
            AgentStatus::Running => OperationStatus::Running,
            AgentStatus::Complete { .. } => OperationStatus::Complete,
        },
    });
    let mut list: Vec<OperationInfo> = job_ops.chain(agent_ops).collect();
    list.sort_by(|a, b| {
        (a.session.0.as_str(), a.handle.as_str()).cmp(&(b.session.0.as_str(), b.handle.as_str()))
    });
    list
}

/// Render a listing for the model (`poll`'s no-handle reply): one line per
/// operation naming its kind, handle, launcher, elapsed time, and status.
pub fn format_operations(ops: &[OperationInfo]) -> String {
    if ops.is_empty() {
        return "poll: no pending operations for this session.".to_string();
    }
    let mut out = format!("poll: {} pending operation(s):\n", ops.len());
    for op in ops {
        let kind = match op.kind {
            OperationKind::Job => "job",
            OperationKind::Agent => "agent",
        };
        let status = match op.status {
            OperationStatus::Running => "running",
            OperationStatus::Complete => "complete",
        };
        out.push_str(&format!(
            "- [{kind} {status}] {} — launched: {} — {}s elapsed\n",
            op.handle, op.launched_by, op.elapsed_secs
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::process::Command;

    fn sh(script: &str) -> Command {
        let mut cmd = Command::new("sh");
        cmd.args(["-c", script])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        crate::host::exec::own_process_group(&mut cmd);
        cmd
    }

    #[tokio::test]
    async fn empty_registries_yield_no_operations() {
        let jobs = JobRegistry::new();
        let agents = AgentRegistry::default();
        let session = SessionId::new("s1");
        assert!(list_operations(&jobs, &agents, Some(&session)).is_empty());
        assert_eq!(
            format_operations(&list_operations(&jobs, &agents, Some(&session))),
            "poll: no pending operations for this session."
        );
    }

    #[tokio::test]
    async fn lists_a_job_and_an_agent_for_the_same_session() {
        let jobs = JobRegistry::new();
        let agents = AgentRegistry::default();
        let session = SessionId::new("s1");

        let job_id = jobs
            .spawn(
                "sleep 30".into(),
                sh("sleep 30"),
                Duration::from_secs(60),
                Some(session.clone()),
            )
            .unwrap();
        let child = SessionId::new("s-child");
        agents.register(child.clone(), session.clone(), "reviewer".to_string());

        let ops = list_operations(&jobs, &agents, Some(&session));
        assert_eq!(ops.len(), 2);
        assert!(ops.iter().any(|o| o.kind == OperationKind::Job
            && o.handle == job_id
            && o.launched_by == "sleep 30"
            && o.status == OperationStatus::Running));
        assert!(ops.iter().any(|o| o.kind == OperationKind::Agent
            && o.handle == child.to_string()
            && o.launched_by == "reviewer"
            && o.status == OperationStatus::Running));

        let text = format_operations(&ops);
        assert!(text.contains("[job running]"), "{text}");
        assert!(text.contains("[agent running]"), "{text}");

        let _ = jobs.poll(&job_id, &session, true, 0).await;
    }

    #[tokio::test]
    async fn scopes_to_the_requested_session_only() {
        let jobs = JobRegistry::new();
        let agents = AgentRegistry::default();
        let mine = SessionId::new("mine");
        let theirs = SessionId::new("theirs");
        let child = SessionId::new("s-child2");
        agents.register(child, theirs.clone(), "build".to_string());

        assert!(list_operations(&jobs, &agents, Some(&mine)).is_empty());
        assert_eq!(list_operations(&jobs, &agents, Some(&theirs)).len(), 1);
        assert_eq!(list_operations(&jobs, &agents, None).len(), 1);
    }
}
