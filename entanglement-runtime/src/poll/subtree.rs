//! The subtree pending report a handle-less `poll` appends after the
//! session's own operations (#829): retained outputs, open `ask_user`
//! questions and parked approvals across the caller's spawn sub-tree.
//!
//! WHY: a handle is just text in the transcript, and it falls out of a
//! compacted or resumed context (ADR-0205). The session's own operations
//! listing recovers its jobs, scripts and sub-agents, but not a retained
//! output's `o-` handle, nor the fact that a descendant is blocked on the
//! user. This restores that half of the retired `explore(kind: "pending")`
//! report (ADR-0226 deferred it here) without a new argument. The section
//! only appears when something is pending, so an idle sub-tree leaves the
//! reply byte-identical to the plain listing. Across a compaction, the
//! predecessor's jobs, scripts and retained outputs follow the successor
//! (ADR-0231), so they show up here and in the own listing as the
//! successor's. Its sub-agents are still closed by the compaction's close
//! cascade (#862).
//!
//! Visibility is the downward spawn walk ([`crate::discover::subtree_sessions`]):
//! only sessions in the caller's own spawn sub-tree, never a sibling or an
//! ancestor.

use entanglement_core::SessionId;

use crate::agent_registry::AgentRegistry;
use crate::discover::subtree_sessions;
use crate::host::jobs::JobRegistry;
use crate::pending::PendingDecisions;
use crate::questions::OpenQuestions;
use crate::retained_output::RetainedOutputRegistry;
use crate::script_ops::ScriptRegistry;

use super::format::PollOutcome;

/// Every registry a `poll` reads — the four the single-handle join uses plus
/// the two parked-decision registries the handle-less subtree report needs.
/// Cheap to clone (each field is an `Arc`).
#[derive(Clone)]
pub struct PollRegistries {
    pub jobs: JobRegistry,
    pub agents: AgentRegistry,
    pub retained: RetainedOutputRegistry,
    pub scripts: ScriptRegistry,
    pub pending: PendingDecisions,
    pub questions: OpenQuestions,
}

impl PollRegistries {
    /// Re-run the ADR-0231 successor hand-over from `predecessor` to
    /// `session`. Idempotent, so it is safe on every poll.
    pub(super) fn adopt_predecessor_work(
        &self,
        predecessor: Option<SessionId>,
        session: &SessionId,
    ) {
        if let Some(p) = predecessor {
            self.jobs.reassign_owner(&p, session);
            self.scripts.reassign_owner(&p, session);
            self.retained.reassign_owner(&p, session);
        }
    }
}

/// One `poll` call end to end: the handle dispatch of [`super::resolve`],
/// plus — only for a handle-less call — the subtree section when non-empty.
pub(super) async fn resolve_with_subtree(
    regs: &PollRegistries,
    session: &SessionId,
    input: &str,
) -> PollOutcome {
    let mut outcome = super::resolve(
        &regs.jobs,
        &regs.agents,
        &regs.retained,
        &regs.scripts,
        session,
        input,
    )
    .await;
    if super::parse_input(input).is_none() {
        if let Some(section) = subtree_report(regs, session) {
            outcome.text = format!("{}\n\n{section}", outcome.text.trim_end());
        }
    }
    outcome
}

/// Render `caller`'s sub-tree pending items, `None` when there are none.
pub(super) fn subtree_report(regs: &PollRegistries, caller: &SessionId) -> Option<String> {
    let subtree = subtree_sessions(&regs.agents, caller);
    let mut sections: Vec<String> = Vec::new();

    let retained_rows = regs
        .retained
        .snapshot(None)
        .into_iter()
        // An ownerless entry is engine-wide, not this sub-tree's work.
        .filter_map(|r| {
            let owner = r.owner.filter(|o| subtree.contains(o))?;
            let kind = if r.is_file { "file" } else { "text" };
            // The registry pages an entry to its owner only, so a
            // descendant's handle is named for awareness, not as pollable.
            let reach = if &owner == caller {
                "yours — poll with handle, offset/tail".to_string()
            } else {
                format!("owned by {owner} — only that session can poll it")
            };
            Some(format!(
                "  {} — retained {kind} output, {reach}, {}s old",
                r.handle,
                r.age.as_secs()
            ))
        })
        .collect();
    push_section(&mut sections, "retained outputs", retained_rows);

    let question_rows = regs
        .questions
        .snapshot(None)
        .into_iter()
        .filter(|q| subtree.contains(&q.session))
        .map(|q| {
            format!(
                "  {} — session {}, {} question(s) awaiting the user",
                q.request_id,
                q.session,
                q.questions.0.len()
            )
        })
        .collect();
    push_section(&mut sections, "open ask_user questions", question_rows);

    let approval_rows = regs
        .pending
        .snapshot(None)
        .into_iter()
        // An `ask_user` park is the same wait as its open-question row.
        .filter(|p| p.kind != "ask_user" && subtree.contains(&p.session))
        .map(|p| {
            format!(
                "  {} — {} approval for `{}`, session {}, awaiting the user",
                p.request_id, p.kind, p.detail, p.session
            )
        })
        .collect();
    push_section(&mut sections, "parked approvals", approval_rows);

    if sections.is_empty() {
        return None;
    }
    Some(format!(
        "subtree (this session and its sub-agents):\n{}\n",
        sections.join("\n")
    ))
}

fn push_section(sections: &mut Vec<String>, header: &str, rows: Vec<String>) {
    if !rows.is_empty() {
        sections.push(format!("{header}:\n{}", rows.join("\n")));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use entanglement_core::Question;

    fn regs() -> PollRegistries {
        PollRegistries {
            jobs: JobRegistry::new(),
            agents: AgentRegistry::default(),
            retained: RetainedOutputRegistry::new(),
            scripts: ScriptRegistry::new(),
            pending: PendingDecisions::default(),
            questions: OpenQuestions::default(),
        }
    }

    fn question(text: &str) -> Vec<Question> {
        vec![Question {
            question: text.to_string(),
            options: Vec::new(),
            multi_select: false,
        }]
    }

    /// The self-heal for a missed lifecycle fold: an entry still owned by the
    /// predecessor is adopted by the successor's next poll, and a session
    /// with no predecessor adopts nothing.
    #[tokio::test]
    async fn a_poll_adopts_work_the_lifecycle_fold_missed() {
        let r = regs();
        let (pred, succ) = (SessionId::new("pred"), SessionId::new("succ"));
        let out = r.retained.register_text(Some(pred.clone()), "late".into());

        r.adopt_predecessor_work(None, &succ);
        assert!(r.retained.page(&out, &succ, 0, 30).is_none());

        r.adopt_predecessor_work(Some(pred.clone()), &succ);
        let listed = resolve_with_subtree(&r, &succ, "{}").await;
        assert!(listed
            .text
            .contains(&format!("{out} — retained text output, yours")));
        let paged = resolve_with_subtree(&r, &succ, &format!(r#"{{"handle":"{out}"}}"#)).await;
        assert!(
            !paged.is_error && paged.text.contains("late"),
            "{}",
            paged.text
        );
    }

    #[test]
    fn an_empty_subtree_renders_no_section() {
        assert!(subtree_report(&regs(), &SessionId::new("root")).is_none());
    }

    /// The handle-less reply is byte-identical to the plain listing when the
    /// sub-tree has nothing pending — the "no change when idle" contract.
    #[tokio::test]
    async fn handle_less_poll_is_unchanged_when_the_subtree_is_idle() {
        let r = regs();
        let root = SessionId::new("root");
        let out = resolve_with_subtree(&r, &root, "{}").await;
        assert_eq!(out.text, "poll: no pending operations for this session.");
        assert!(!out.is_error);
    }

    #[tokio::test]
    async fn the_section_follows_the_own_listing_and_never_rides_a_handle_poll() {
        let r = regs();
        let root = SessionId::new("root");
        r.retained.register_text(Some(root.clone()), "x".into());
        let out = resolve_with_subtree(&r, &root, "{}").await;
        let (own, sub) = out.text.split_once("\nsubtree").expect("section appended");
        assert_eq!(
            own.trim_end(),
            "poll: no pending operations for this session."
        );
        assert!(sub.contains("retained outputs:"), "{}", out.text);
        assert!(sub.contains("yours — poll with handle"), "{}", out.text);

        let handled = resolve_with_subtree(&r, &root, r#"{"handle":"j-nope"}"#).await;
        assert!(!handled.text.contains("subtree"), "{}", handled.text);
    }

    #[test]
    fn a_grandchilds_items_appear_and_a_siblings_and_parents_do_not() {
        let r = regs();
        let (parent, root, child, grandchild, sibling) = (
            SessionId::new("parent"),
            SessionId::new("root"),
            SessionId::new("child"),
            SessionId::new("grandchild"),
            SessionId::new("sibling"),
        );
        r.agents
            .register(root.clone(), parent.clone(), "general".into());
        r.agents
            .register(child.clone(), root.clone(), "general".into());
        r.agents
            .register(grandchild.clone(), child.clone(), "general".into());
        r.agents
            .register(sibling.clone(), parent.clone(), "general".into());
        r.questions
            .insert(&grandchild, "q-gc", question("which db?"));
        let _q = r.pending.register(&grandchild, "q-gc", "ask_user", "q-gc");
        let _t = r.pending.register(&child, "t-child", "tool", "bash");
        r.retained
            .register_text(Some(child.clone()), "child text".into());
        r.questions.insert(&sibling, "q-sib", question("leak?"));
        let _ts = r.pending.register(&sibling, "t-sib", "tool", "bash");
        let _tp = r.pending.register(&parent, "t-parent", "tool", "write");

        let report = subtree_report(&r, &root).expect("non-empty");
        assert!(
            report.contains("q-gc — session grandchild, 1 question(s)"),
            "{report}"
        );
        assert!(
            report.contains("t-child — tool approval for `bash`"),
            "{report}"
        );
        assert!(
            report.contains("owned by child — only that session"),
            "{report}"
        );
        for leaked in ["q-sib", "t-sib", "t-parent", "sibling", "parent"] {
            assert!(!report.contains(leaked), "{leaked} leaked: {report}");
        }
        // The ask_user park is reported once, as a question, not as an approval.
        assert!(!report.contains("ask_user approval"), "{report}");
    }

    #[test]
    fn an_ownerless_retained_entry_is_not_attributed_to_the_subtree() {
        let r = regs();
        r.retained.register_text(None, "engine-wide".into());
        assert!(subtree_report(&r, &SessionId::new("root")).is_none());
    }
}
