//! `explore(kind: "pending")` (#560 P12, ADR-0207 §12): everything in flight
//! for the calling session's **whole spawn sub-tree** — running sub-agents,
//! background `bash`/`call` jobs, background rhai scripts, retained outputs,
//! open `ask_user` questions, and parked approvals.
//!
//! WHY this exists: `poll` is the only model-facing path to in-flight work
//! and it **requires a handle already held**. Every compaction forks a
//! successor seeded with a summary plus the kept tail (ADR-0205), and a
//! handle — `x-…` for a job, an `agent_id` for a sub-agent — is just text in
//! the transcript. If it falls outside the kept tail it is unreachable: the
//! work keeps running, finishes, and its result is silently orphaned. The
//! same happens across a hibernate/resume cycle. Listing makes handles
//! **recoverable** instead of something the model must hoard in context.
//!
//! Aggregates the state five registries already track — [`AgentRegistry`],
//! [`JobRegistry`], [`ScriptRegistry`], [`RetainedOutputRegistry`],
//! [`PendingDecisions`] and [`OpenQuestions`] — rather than building any
//! parallel bookkeeping of its own. The only new logic here is the spawn
//! sub-tree walk: every other registry already scopes a snapshot by session,
//! so `None`-scoped snapshots are pulled once and filtered locally against
//! the discovered session set.

use std::collections::HashSet;

use entanglement_core::SessionId;

use crate::agent_registry::{AgentRegistry, AgentStatus};
use crate::host::jobs::{JobRegistry, JobStatus};
use crate::pending::PendingDecisions;
use crate::questions::OpenQuestions;
use crate::retained_output::RetainedOutputRegistry;
use crate::script_ops::ScriptRegistry;

/// Every registry `build_pending_report` reads — bundled so the call site in
/// the interception ladder passes one struct instead of five positional
/// clones.
pub struct PendingSources<'a> {
    pub agents: &'a AgentRegistry,
    pub jobs: &'a JobRegistry,
    pub scripts: &'a ScriptRegistry,
    pub retained: &'a RetainedOutputRegistry,
    pub pending: &'a PendingDecisions,
    pub questions: &'a OpenQuestions,
}

/// Every session in `root`'s spawn sub-tree, `root` included: a breadth-first
/// walk of [`AgentRegistry`]'s parent→child links (each spawn registers a
/// child under its immediate parent, so a grandchild is only reachable by
/// walking one level at a time). Bounded by construction — a mode's
/// `max_depth`/`max_agents` already caps how wide/deep the live tree can be —
/// but capped defensively at a fixed iteration count too, so a bug elsewhere
/// can never turn this into an unbounded loop.
const MAX_WALK_STEPS: usize = 4096;

fn subtree_sessions(agents: &AgentRegistry, root: &SessionId) -> HashSet<SessionId> {
    let every_child = agents.snapshot(None);
    let mut seen: HashSet<SessionId> = HashSet::from([root.clone()]);
    let mut frontier: Vec<SessionId> = vec![root.clone()];
    let mut steps = 0;
    while !frontier.is_empty() && steps < MAX_WALK_STEPS {
        let mut next = Vec::new();
        for parent in &frontier {
            for child in every_child
                .iter()
                .filter(|c| &c.session == parent)
                .map(|c| SessionId::new(c.handle.clone()))
            {
                if seen.insert(child.clone()) {
                    next.push(child);
                }
            }
            steps += 1;
        }
        frontier = next;
    }
    seen
}

/// Build the `explore(kind: "pending")` plain-text report for `caller`'s
/// whole spawn sub-tree. Empty sections are omitted; an entirely empty
/// sub-tree reports so plainly rather than an empty string.
pub fn build_pending_report(src: &PendingSources<'_>, caller: &SessionId) -> String {
    let subtree = subtree_sessions(src.agents, caller);
    let in_subtree = |s: &SessionId| subtree.contains(s);

    let mut sections: Vec<String> = Vec::new();

    let agent_rows: Vec<String> = src
        .agents
        .snapshot(None)
        .into_iter()
        .filter(|a| in_subtree(&a.session))
        .map(|a| {
            let status = match a.status {
                AgentStatus::Running => "running".to_string(),
                AgentStatus::Complete { .. } => "complete (unread)".to_string(),
            };
            format!(
                "  {} — agent '{}', launched by {}, {status}, {}s elapsed",
                a.handle,
                a.agent,
                a.session.0,
                a.elapsed.as_secs()
            )
        })
        .collect();
    push_section(&mut sections, "SUB-AGENTS (poll with agent_id)", agent_rows);

    let job_rows: Vec<String> = src
        .jobs
        .snapshot(None)
        .into_iter()
        .filter(|j| in_subtree(&j.session))
        .map(|j| {
            let status = match j.status {
                JobStatus::Running => "running".to_string(),
                JobStatus::Exited(code) => format!("exited({code:?})"),
            };
            format!(
                "  {} — `{}`, session {}, {status}, {}s elapsed",
                j.handle,
                j.command,
                j.session.0,
                j.elapsed.as_secs()
            )
        })
        .collect();
    push_section(
        &mut sections,
        "BACKGROUND JOBS (poll with handle)",
        job_rows,
    );

    let script_rows: Vec<String> = src
        .scripts
        .snapshot_ops(None)
        .into_iter()
        .filter(|s| in_subtree(&s.session))
        .map(|s| {
            let status = if s.running { "running" } else { "complete" };
            format!(
                "  {} — {}, session {}, {status}, {}s elapsed",
                s.handle,
                s.label,
                s.session.0,
                s.elapsed.as_secs()
            )
        })
        .collect();
    push_section(
        &mut sections,
        "BACKGROUND SCRIPTS (poll with handle)",
        script_rows,
    );

    let retained_rows: Vec<String> = src
        .retained
        .snapshot(None)
        .into_iter()
        .filter(|r| r.owner.as_ref().is_some_and(&in_subtree))
        .map(|r| {
            let kind = if r.is_file { "file" } else { "text" };
            format!(
                "  {} — retained {kind} output, owner {}, {}s old",
                r.handle,
                r.owner.map(|o| o.0).unwrap_or_default(),
                r.age.as_secs()
            )
        })
        .collect();
    push_section(
        &mut sections,
        "RETAINED OUTPUTS (poll with handle, offset/tail)",
        retained_rows,
    );

    let question_rows: Vec<String> = src
        .questions
        .snapshot(None)
        .into_iter()
        .filter(|q| in_subtree(&q.session))
        .map(|q| {
            format!(
                "  {} — session {}, {} question(s) open",
                q.request_id,
                q.session.0,
                q.questions.0.len()
            )
        })
        .collect();
    push_section(&mut sections, "OPEN QUESTIONS (ask_user)", question_rows);

    let approval_rows: Vec<String> = src
        .pending
        .snapshot(None)
        .into_iter()
        // `ask_user` parks are the same wait as an `OPEN QUESTIONS` row above
        // (ADR-0207 §12 lists them as a distinct bullet, but they are one
        // underlying park) — excluded here so a call isn't listed twice.
        .filter(|p| p.kind != "ask_user" && in_subtree(&p.session))
        .map(|p| {
            format!(
                "  {} — {} approval for `{}`, session {}",
                p.request_id, p.kind, p.detail, p.session.0
            )
        })
        .collect();
    push_section(
        &mut sections,
        "PARKED APPROVALS (Approve/Reject to resolve)",
        approval_rows,
    );

    if sections.is_empty() {
        "pending: nothing in flight for this session's spawn sub-tree.".to_string()
    } else {
        sections.join("\n")
    }
}

fn push_section(sections: &mut Vec<String>, header: &str, rows: Vec<String>) {
    if rows.is_empty() {
        return;
    }
    sections.push(format!("{header}:\n{}", rows.join("\n")));
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

    fn sources<'a>(
        agents: &'a AgentRegistry,
        jobs: &'a JobRegistry,
        scripts: &'a ScriptRegistry,
        retained: &'a RetainedOutputRegistry,
        pending: &'a PendingDecisions,
        questions: &'a OpenQuestions,
    ) -> PendingSources<'a> {
        PendingSources {
            agents,
            jobs,
            scripts,
            retained,
            pending,
            questions,
        }
    }

    #[test]
    fn empty_subtree_reports_plainly() {
        let agents = AgentRegistry::default();
        let jobs = JobRegistry::new();
        let scripts = ScriptRegistry::new();
        let retained = RetainedOutputRegistry::new();
        let pending = PendingDecisions::default();
        let questions = OpenQuestions::default();
        let src = sources(&agents, &jobs, &scripts, &retained, &pending, &questions);
        let report = build_pending_report(&src, &SessionId::new("s1"));
        assert!(report.contains("nothing in flight"), "{report}");
    }

    /// The scenario the whole feature exists for: a live sub-agent and a
    /// background job launched by a *grandchild* (two spawn hops away) both
    /// surface — a plain per-session `snapshot` would miss the grandchild.
    #[tokio::test]
    async fn lists_a_live_sub_agent_and_a_background_job_across_the_whole_subtree() {
        let agents = AgentRegistry::default();
        let jobs = JobRegistry::new();
        let scripts = ScriptRegistry::new();
        let retained = RetainedOutputRegistry::new();
        let pending = PendingDecisions::default();
        let questions = OpenQuestions::default();
        let root = SessionId::new("root");
        let child = SessionId::new("child");
        let grandchild = SessionId::new("grandchild");
        agents.register(child.clone(), root.clone(), "general".to_string());
        agents.register(grandchild.clone(), child.clone(), "general".to_string());

        let job_id = jobs
            .spawn(
                "sleep 30".into(),
                sh("sleep 30"),
                Duration::from_secs(60),
                Some(grandchild.clone()),
            )
            .unwrap();

        let src = sources(&agents, &jobs, &scripts, &retained, &pending, &questions);
        let report = build_pending_report(&src, &root);

        assert!(report.contains("SUB-AGENTS"), "{report}");
        assert!(report.contains(&child.to_string()), "{report}");
        assert!(report.contains(&grandchild.to_string()), "{report}");
        assert!(report.contains("BACKGROUND JOBS"), "{report}");
        assert!(report.contains(&job_id), "{report}");

        let _ = jobs.poll(&job_id, &grandchild, true, 0).await;
    }

    #[test]
    fn a_sibling_subtree_is_excluded() {
        let agents = AgentRegistry::default();
        let jobs = JobRegistry::new();
        let scripts = ScriptRegistry::new();
        let retained = RetainedOutputRegistry::new();
        let pending = PendingDecisions::default();
        let questions = OpenQuestions::default();
        let root = SessionId::new("root");
        let other_root = SessionId::new("other-root");
        let other_child = SessionId::new("other-child");
        agents.register(
            other_child.clone(),
            other_root.clone(),
            "general".to_string(),
        );

        let src = sources(&agents, &jobs, &scripts, &retained, &pending, &questions);
        let report = build_pending_report(&src, &root);
        assert!(!report.contains(&other_child.to_string()), "{report}");
    }

    #[test]
    fn open_question_and_parked_approval_each_get_their_own_section_without_duplication() {
        let agents = AgentRegistry::default();
        let jobs = JobRegistry::new();
        let scripts = ScriptRegistry::new();
        let retained = RetainedOutputRegistry::new();
        let pending = PendingDecisions::default();
        let questions = OpenQuestions::default();
        let s = SessionId::new("s1");
        questions.insert(
            &s,
            "q-1",
            vec![entanglement_core::Question {
                question: "which db?".to_string(),
                options: Vec::new(),
                multi_select: false,
            }],
        );
        let _rx_q = pending.register(&s, "q-1", "ask_user", "q-1");
        let _rx_tool = pending.register(&s, "t-1", "tool", "bash");

        let src = sources(&agents, &jobs, &scripts, &retained, &pending, &questions);
        let report = build_pending_report(&src, &s);

        assert!(report.contains("OPEN QUESTIONS"), "{report}");
        assert!(report.contains("q-1"), "{report}");
        assert!(report.contains("PARKED APPROVALS"), "{report}");
        assert!(report.contains("t-1"), "{report}");
        // The ask_user park itself must not also show up as a generic
        // approval row (it's the same wait as the OPEN QUESTIONS row).
        let approvals_section = report.split("PARKED APPROVALS").nth(1).unwrap();
        assert!(!approvals_section.contains("ask_user approval"), "{report}");
    }
}
