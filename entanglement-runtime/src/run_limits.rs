//! Unattended-run enforcement for a mode's `question_timeout`/`on_timeout`
//! (ADR-0207 §11, stage 5c: "make `auto` genuinely unattended"). Pure,
//! runtime-side policy — core carries no notion of any of this, only an
//! opaque mode name (ADR-0207 §2); every function here is a property of a
//! [`Limits`] value, never a hardcoded `mode == "auto"` check, so a user
//! tuning another mode's `question_timeout` gets the identical behavior.
//!
//! Three pieces:
//! - [`collapses_ask`]/[`timeout`]: whether/how long an `Ask` grade waits
//!   before the mode's `on_timeout` policy applies.
//! - [`default_answers`]: what a timed-out `ask_user` call answers with — a
//!   question with options defaults to its first, a free-text question (no
//!   default to fall back to) fails the whole call instead of guessing.
//! - [`DenialTracker`]: the repeat-denial escalation — a *second* identical
//!   `(tool, arg)` denial in the same turn parks an approval instead of
//!   being refused again silently, since a model insisting twice may
//!   genuinely need it.

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;
use std::time::Duration;

use entanglement_core::{Question, SessionId};

use crate::mode::Limits;

/// Whether an `Ask` grade must collapse to an immediate denial rather than
/// parking a prompt (ADR-0207 §11: "a `prompt` grade likewise collapses to a
/// denial here, since there is no one to ask") — true whenever the mode
/// declares a finite `question_timeout`, the mode's own declaration that a
/// parked prompt may go unanswered. `0` (every built-in mode but `auto`)
/// means "wait forever", so nothing collapses there.
pub(crate) fn collapses_ask(limits: &Limits) -> bool {
    limits.question_timeout != 0
}

/// `question_timeout` as a `Duration` for [`crate::pending::await_decision_timed`],
/// or `None` for "wait forever".
pub(crate) fn timeout(limits: &Limits) -> Option<Duration> {
    (limits.question_timeout != 0).then(|| Duration::from_secs(limits.question_timeout))
}

/// The default answer set for an `ask_user` call whose `question_timeout`
/// elapsed (ADR-0207 §11): each question with at least one option defaults
/// to its first (`on_timeout`'s only built-in posture, `Deny`, governs a
/// parked *approval* — a question has no such field, its default-option
/// behavior is unconditional). A question with **no** options is free text
/// with nothing to default to, so it fails the *whole* call instead of
/// inventing an answer — `Err` names the offending question.
pub(crate) fn default_answers(questions: &[Question]) -> Result<Vec<Vec<String>>, String> {
    questions
        .iter()
        .map(|q| {
            q.options
                .first()
                .map(|opt| vec![opt.label.clone()])
                .ok_or_else(|| {
                    format!(
                        "question \"{}\" timed out waiting for an answer and has no options to \
                     default to",
                        q.question
                    )
                })
        })
        .collect()
}

/// The grant-store's own `(tool, grading_arg)` key (`apply_grant`/`GrantStore`),
/// reused here so "the same call" means exactly what it would mean to a
/// grant.
type DenialKey = (String, Option<String>);

/// Tracks, per session, which `(tool, arg)` calls a collapsed `Ask` (see
/// [`collapses_ask`]) has already denied in the current turn (ADR-0207 §11's
/// repeat-denial escalation). Cleared on `Done` and on session end/hibernate
/// — mirrors `tool_runner`'s own `active_skill` per-turn scope — so a new
/// turn, or a fresh session reusing the same id space, starts clean.
#[derive(Default)]
pub(crate) struct DenialTracker(Mutex<HashMap<SessionId, HashSet<DenialKey>>>);

impl DenialTracker {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Record a denial of `(tool, arg)` at `session`. Returns `true` when
    /// this exact call was already denied earlier this turn — the signal to
    /// park an approval instead of refusing silently again.
    pub(crate) fn record_repeat(&self, session: &SessionId, tool: &str, arg: Option<&str>) -> bool {
        let key: DenialKey = (tool.to_string(), arg.map(str::to_string));
        !self
            .0
            .lock()
            .expect("denial tracker mutex poisoned")
            .entry(session.clone())
            .or_default()
            .insert(key)
    }

    /// Drop `session`'s tracked denials: a new turn or a closed session
    /// starts clean.
    pub(crate) fn clear(&self, session: &SessionId) {
        self.0
            .lock()
            .expect("denial tracker mutex poisoned")
            .remove(session);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use entanglement_core::QuestionOption;

    fn limits(question_timeout: u64) -> Limits {
        Limits {
            question_timeout,
            ..Limits::default()
        }
    }

    #[test]
    fn zero_question_timeout_never_collapses() {
        assert!(!collapses_ask(&limits(0)));
        assert_eq!(timeout(&limits(0)), None);
    }

    #[test]
    fn nonzero_question_timeout_collapses_and_converts() {
        assert!(collapses_ask(&limits(60)));
        assert_eq!(timeout(&limits(60)), Some(Duration::from_secs(60)));
    }

    fn q(question: &str, options: &[&str]) -> Question {
        Question {
            question: question.to_string(),
            options: options
                .iter()
                .map(|l| QuestionOption {
                    label: l.to_string(),
                    description: None,
                })
                .collect(),
            multi_select: false,
        }
    }

    #[test]
    fn default_answers_picks_the_first_option() {
        let questions = vec![q("Which?", &["A", "B"])];
        let answers = default_answers(&questions).expect("has options");
        assert_eq!(answers, vec![vec!["A".to_string()]]);
    }

    #[test]
    fn default_answers_fails_the_whole_call_on_any_free_text_question() {
        let questions = vec![q("Which?", &["A", "B"]), q("Describe?", &[])];
        let err = default_answers(&questions).expect_err("no default for free text");
        assert!(err.contains("Describe?"));
    }

    #[test]
    fn denial_tracker_first_call_is_not_a_repeat() {
        let tracker = DenialTracker::new();
        let s = SessionId::new("s");
        assert!(!tracker.record_repeat(&s, "bash", Some("rm x")));
    }

    #[test]
    fn denial_tracker_second_identical_call_is_a_repeat() {
        let tracker = DenialTracker::new();
        let s = SessionId::new("s");
        assert!(!tracker.record_repeat(&s, "bash", Some("rm x")));
        assert!(tracker.record_repeat(&s, "bash", Some("rm x")));
    }

    #[test]
    fn denial_tracker_distinguishes_by_arg() {
        let tracker = DenialTracker::new();
        let s = SessionId::new("s");
        assert!(!tracker.record_repeat(&s, "bash", Some("rm x")));
        assert!(!tracker.record_repeat(&s, "bash", Some("rm y")));
    }

    #[test]
    fn denial_tracker_clear_resets_the_session() {
        let tracker = DenialTracker::new();
        let s = SessionId::new("s");
        assert!(!tracker.record_repeat(&s, "bash", None));
        tracker.clear(&s);
        assert!(!tracker.record_repeat(&s, "bash", None));
    }
}
