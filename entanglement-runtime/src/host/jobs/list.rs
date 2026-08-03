//! Pending-operations listing for background jobs (#607, ADR-0161 §6) — the
//! no-handle `poll`/`InMsg::ListOperations` answer with. A descendant module
//! of `jobs`, so it reads `Job`'s private fields directly.

use std::time::Duration;

use entanglement_core::SessionId;

use super::{JobRegistry, JobStatus};

/// One outstanding job, as reported to a #607 pending-operations listing.
/// `session` is its owner — always present: an ownerless job (the
/// session-less `Tool::run` path) never appears in a listing, since there is
/// no session to attribute it to.
pub struct JobOpInfo {
    pub session: SessionId,
    pub handle: String,
    pub command: String,
    pub status: JobStatus,
    pub elapsed: Duration,
}

impl JobRegistry {
    /// Snapshot outstanding jobs (#607, ADR-0161 §6) — the no-handle listing
    /// `poll`/`InMsg::ListOperations` answer with. `session`, when set,
    /// scopes to that owner only (the `poll` tool's own use, always scoped to
    /// the calling session); `None` spans every owned job, mirroring
    /// [`crate::questions::OpenQuestions::snapshot`]'s filter convention. An
    /// ownerless job (the session-less `Tool::run` path) never appears
    /// either way. Sorted by handle for a deterministic reply.
    pub fn snapshot(&self, session: Option<&SessionId>) -> Vec<JobOpInfo> {
        self.evict_expired();
        let jobs = self.inner.jobs.lock().expect("job registry poisoned");
        let mut list: Vec<JobOpInfo> = jobs
            .iter()
            .filter_map(|(id, job)| {
                let owner = job.owner.clone()?;
                if session.is_some_and(|s| s != &owner) {
                    return None;
                }
                let status = {
                    let st = job.state.lock().expect("job state poisoned");
                    match st.finished {
                        Some(code) => JobStatus::Exited(code),
                        None => JobStatus::Running,
                    }
                };
                Some(JobOpInfo {
                    session: owner,
                    handle: id.clone(),
                    command: job.command.clone(),
                    status,
                    elapsed: job.started.elapsed(),
                })
            })
            .collect();
        list.sort_by(|a, b| a.handle.cmp(&b.handle));
        list
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use tokio::process::Command;

    use super::super::JobRegistry;
    use entanglement_core::SessionId;

    fn sh(script: &str) -> Command {
        let mut cmd = Command::new("sh");
        cmd.args(["-c", script])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        super::super::super::exec::own_process_group(&mut cmd);
        cmd
    }

    #[tokio::test]
    async fn snapshot_is_empty_with_no_owned_jobs() {
        let reg = JobRegistry::new();
        assert!(reg.snapshot(Some(&SessionId::new("s1"))).is_empty());
        assert!(reg.snapshot(None).is_empty());
    }

    #[tokio::test]
    async fn snapshot_scopes_to_the_owning_session_when_filtered() {
        let reg = JobRegistry::new();
        let owner = SessionId::new("owner");
        let stranger = SessionId::new("stranger");
        let id = reg
            .spawn(
                "sleep 30".into(),
                sh("sleep 30"),
                Duration::from_secs(60),
                Some(owner.clone()),
            )
            .unwrap();

        let mine = reg.snapshot(Some(&owner));
        assert_eq!(mine.len(), 1);
        assert_eq!(mine[0].session, owner);
        assert_eq!(mine[0].handle, id);
        assert_eq!(mine[0].command, "sleep 30");

        assert!(reg.snapshot(Some(&stranger)).is_empty());
        let _ = reg.poll(&id, &owner, true, 0).await;
    }

    #[tokio::test]
    async fn snapshot_spans_every_owner_when_unfiltered() {
        let reg = JobRegistry::new();
        let a = SessionId::new("a");
        let b = SessionId::new("b");
        let ida = reg
            .spawn(
                "sleep 30".into(),
                sh("sleep 30"),
                Duration::from_secs(60),
                Some(a.clone()),
            )
            .unwrap();
        let idb = reg
            .spawn(
                "sleep 30".into(),
                sh("sleep 30"),
                Duration::from_secs(60),
                Some(b.clone()),
            )
            .unwrap();

        let all = reg.snapshot(None);
        assert_eq!(all.len(), 2);

        let _ = reg.poll(&ida, &a, true, 0).await;
        let _ = reg.poll(&idb, &b, true, 0).await;
    }

    #[tokio::test]
    async fn snapshot_excludes_ownerless_jobs() {
        let reg = JobRegistry::new();
        reg.spawn("true".into(), sh("true"), Duration::from_secs(60), None)
            .unwrap();
        assert!(reg.snapshot(Some(&SessionId::new("anyone"))).is_empty());
        assert!(reg.snapshot(None).is_empty());
    }
}
