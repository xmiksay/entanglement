//! Unattended-run enforcement for a mode's `question_timeout`/`on_timeout`
//! (ADR-0207 §11, stage 5c: "make `auto` genuinely unattended"; ADR-0247:
//! "unattended" is also a fact about the session). Pure, runtime-side
//! policy — core carries no notion of any of this, only an opaque mode name
//! (ADR-0207 §2); every function here is a property of a [`Limits`] value
//! (plus, since ADR-0247, whether a head watches the session), never a
//! hardcoded `mode == "auto"` check, so a user tuning another mode's
//! `question_timeout` gets the identical behavior.
//!
//! Three pieces:
//! - [`unattended`]/[`expiry`]: whether an `Ask` grade collapses, and when a
//!   parked card or question gives up — never for an attended session.
//! - [`default_answers`]: what a timed-out `ask_user` call answers with — a
//!   question with options defaults to its first, a free-text question (no
//!   default to fall back to) fails the whole call instead of guessing.
//! - [`DenialTracker`]: the repeat-denial escalation — a *second* denial of
//!   the same call in the same turn parks an approval instead of being
//!   refused again silently, since a model insisting twice may genuinely
//!   need it. "The same call" is the same named mode rule, else the same
//!   exact argument ([`Repeat`]).

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;
use std::time::Duration;

use entanglement_core::{Question, SessionId};

use crate::attendance::{Attendance, Expiry};
use crate::mode::Limits;

/// Whether the mode declares a finite `question_timeout` — its own
/// declaration that a parked prompt may go unanswered. `0` (every built-in
/// mode but `auto`) means "wait forever", so nothing collapses there.
pub(crate) fn collapses_ask(limits: &Limits) -> bool {
    limits.question_timeout != 0
}

/// Whether an `Ask` grade collapses to an immediate denial rather than
/// parking a prompt (ADR-0207 §11: "there is no one to ask"): the mode is
/// unattended ([`collapses_ask`]) **and** no head watches the session or any
/// of its ancestors (`chain`, ADR-0247).
pub(crate) fn unattended(limits: &Limits, attendance: &Attendance, chain: &[SessionId]) -> bool {
    collapses_ask(limits) && !attendance.attends_any(chain)
}

/// `question_timeout` as a `Duration`, or `None` for "wait forever".
pub(crate) fn timeout(limits: &Limits) -> Option<Duration> {
    (limits.question_timeout != 0).then(|| Duration::from_secs(limits.question_timeout))
}

/// When the park `request_id` of a session with ancestor `chain` gives up
/// (ADR-0247): never while a head watches the chain or once it was waived,
/// else after the mode's `question_timeout` — waived for good if a watcher
/// arrives first.
pub(crate) fn expiry(
    limits: &Limits,
    attendance: &Attendance,
    chain: &[SessionId],
    request_id: &str,
) -> Expiry {
    Expiry::unless_attended(timeout(limits), attendance, chain, request_id)
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

/// What makes a retry "the same call" for the repeat escalation (ADR-0247,
/// amending ADR-0207 §11's "identical repeat"): the named mode rule that
/// graded it (`bash(rm -r*)` — so `rm -r x` then `rm -r x 2>&1` is a
/// repeat), else — a default grade — the grant-store's own exact
/// `(tool, grading_arg)` key.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) enum Repeat {
    Rule(String),
    Call { tool: String, arg: Option<String> },
}

/// Tracks, per session, which calls a collapsed `Ask` (see [`unattended`])
/// has already denied in the current turn (ADR-0207 §11's repeat-denial
/// escalation). Cleared on `Done` and on session end/hibernate — mirrors
/// `tool_runner`'s own `active_skill` per-turn scope — so a new turn, or a
/// fresh session reusing the same id space, starts clean.
#[derive(Default)]
pub(crate) struct DenialTracker(Mutex<HashMap<SessionId, HashSet<Repeat>>>);

impl DenialTracker {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Record a denial of `repeat` at `session`. Returns `true` when the
    /// same call was already denied earlier this turn — the signal to park
    /// an approval instead of refusing silently again.
    pub(crate) fn record_repeat(&self, session: &SessionId, repeat: Repeat) -> bool {
        !self
            .0
            .lock()
            .expect("denial tracker mutex poisoned")
            .entry(session.clone())
            .or_default()
            .insert(repeat)
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

    fn call(tool: &str, arg: Option<&str>) -> Repeat {
        Repeat::Call {
            tool: tool.to_string(),
            arg: arg.map(str::to_string),
        }
    }

    #[test]
    fn denial_tracker_first_call_is_not_a_repeat() {
        let tracker = DenialTracker::new();
        let s = SessionId::new("s");
        assert!(!tracker.record_repeat(&s, call("bash", Some("rm x"))));
    }

    #[test]
    fn denial_tracker_second_identical_call_is_a_repeat() {
        let tracker = DenialTracker::new();
        let s = SessionId::new("s");
        assert!(!tracker.record_repeat(&s, call("bash", Some("rm x"))));
        assert!(tracker.record_repeat(&s, call("bash", Some("rm x"))));
    }

    /// A default grade keys on the exact argument: two different commands
    /// nothing but the default graded never escalate.
    #[test]
    fn denial_tracker_distinguishes_default_graded_calls_by_arg() {
        let tracker = DenialTracker::new();
        let s = SessionId::new("s");
        assert!(!tracker.record_repeat(&s, call("bash", Some("rm x"))));
        assert!(!tracker.record_repeat(&s, call("bash", Some("rm y"))));
    }

    /// ADR-0247: a retry graded by the same named rule is a repeat however
    /// its argument changed.
    #[test]
    fn denial_tracker_matches_a_retry_by_its_rule() {
        let tracker = DenialTracker::new();
        let s = SessionId::new("s");
        let rule = || Repeat::Rule("bash(rm -r*)".to_string());
        assert!(!tracker.record_repeat(&s, rule()));
        assert!(tracker.record_repeat(&s, rule()));
        assert!(!tracker.record_repeat(&s, Repeat::Rule("bash(rm -R*)".to_string())));
    }

    #[test]
    fn denial_tracker_clear_resets_the_session() {
        let tracker = DenialTracker::new();
        let s = SessionId::new("s");
        assert!(!tracker.record_repeat(&s, call("bash", None)));
        tracker.clear(&s);
        assert!(!tracker.record_repeat(&s, call("bash", None)));
    }

    #[test]
    fn a_watched_ancestor_makes_the_mode_attended() {
        let attendance = Attendance::new();
        let chain = [SessionId::new("child"), SessionId::new("root")];
        assert!(unattended(&limits(60), &attendance, &chain));
        assert!(!unattended(&limits(0), &attendance, &chain));
        let watcher = attendance.watcher();
        watcher.attend(&chain[1]);
        assert!(!unattended(&limits(60), &attendance, &chain));
        assert_eq!(expiry(&limits(60), &attendance, &chain, "a").secs(), 0);
        drop(watcher);
        assert_eq!(expiry(&limits(60), &attendance, &chain, "b").secs(), 60);
    }
}
