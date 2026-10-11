use entanglement_core::{Question, Questions};

use super::*;

fn sid(s: &str) -> SessionId {
    SessionId::new(s)
}

fn started(s: &str, parent: Option<&str>, predecessor: Option<&str>) -> OutEvent {
    OutEvent::SessionStarted {
        session: sid(s),
        parent: parent.map(sid),
        predecessor: predecessor.map(sid),
        agent: "build".into(),
        model: Some("m".into()),
        root: parent.is_none(),
        ts: 1,
        user: None,
    }
}

fn done(s: &str) -> OutEvent {
    OutEvent::Done {
        session: sid(s),
        seq: 1,
    }
}

fn status(s: &str, state: AgentState) -> OutEvent {
    OutEvent::Status {
        session: sid(s),
        state,
    }
}

fn source_of(t: &mut Tracker, ev: &OutEvent) -> Value {
    let f = t.observe(ev).expect("session_start fires");
    assert_eq!(f.event, HookEvent::SessionStart);
    f.payload["source"].clone()
}

#[test]
fn session_start_names_fresh_compact_and_resume() {
    let mut t = Tracker::default();
    assert_eq!(source_of(&mut t, &started("a", None, None)), "fresh");
    assert_eq!(source_of(&mut t, &started("b", None, Some("a"))), "compact");
    t.observe_in(&InMsg::Resume {
        session: sid("a"),
        records: vec![(None, started("a", None, None))],
    });
    let f = t.observe(&started("a", None, None)).unwrap();
    assert_eq!(f.payload["source"], "resume");
    assert_eq!(f.payload["event"], "session_start");
    assert_eq!(f.payload["session"], "a");
    assert_eq!(f.payload["agent"], "build");
    // The resume mark is spent: a later fresh start of the id is fresh again.
    assert_eq!(source_of(&mut t, &started("a", None, None)), "fresh");
}

#[test]
fn a_resumed_compaction_successor_reports_resume() {
    let mut t = Tracker::default();
    t.observe_in(&InMsg::Resume {
        session: sid("b"),
        records: vec![(None, started("b", None, Some("a")))],
    });
    assert_eq!(source_of(&mut t, &started("b", None, Some("a"))), "resume");
}

#[test]
fn stop_fires_once_on_the_status_after_done() {
    let mut t = Tracker::default();
    t.observe(&started("a", None, None));
    assert!(t.observe(&done("a")).is_none());
    let f = t.observe(&status("a", AgentState::Done)).expect("stop");
    assert_eq!(f.event, HookEvent::Stop);
    assert_eq!(f.payload["reason"], "completed");
    assert_eq!(f.payload["error"], Value::Null);
    // A later status with no `Done` before it is not a turn end.
    assert!(t.observe(&status("a", AgentState::Done)).is_none());
}

#[test]
fn a_failed_turn_stops_with_the_error_message() {
    let mut t = Tracker::default();
    t.observe(&started("a", None, None));
    t.observe(&OutEvent::Error {
        session: sid("a"),
        seq: 1,
        message: "boom".into(),
    });
    t.observe(&done("a"));
    let f = t.observe(&status("a", AgentState::Error)).unwrap();
    assert_eq!(f.payload["reason"], "error");
    assert_eq!(f.payload["error"], "boom");
}

#[test]
fn a_child_turn_fires_subagent_stop_with_parent_and_agent() {
    let mut t = Tracker::default();
    t.observe(&started("p", None, None));
    t.observe(&started("c", Some("p"), None));
    t.observe(&done("c"));
    let f = t.observe(&status("c", AgentState::Done)).unwrap();
    assert_eq!(f.event, HookEvent::SubagentStop);
    assert_eq!(f.payload["parent"], "p");
    assert_eq!(f.payload["agent"], "build");
}

#[test]
fn compaction_fires_pre_compact_and_ends_the_source_as_compacted() {
    let mut t = Tracker::default();
    t.observe(&started("a", None, None));
    let f = t
        .observe(&OutEvent::Compacted {
            session: sid("a"),
            seq: 2,
            summary: "sum".into(),
            kept: 0,
            auto: false,
            mode: CompactionMode::Summary,
        })
        .unwrap();
    assert_eq!(f.event, HookEvent::PreCompact);
    assert_eq!(f.payload["kind"], "manual");
    assert_eq!(f.payload["summary"], "sum");
    // The manual op's closing `Done` is not a turn end.
    t.observe(&done("a"));
    assert!(t.observe(&status("a", AgentState::Done)).is_none());
    let end = t
        .observe(&OutEvent::SessionEnded {
            session: sid("a"),
            ts: 3,
        })
        .unwrap();
    assert_eq!(end.event, HookEvent::SessionEnd);
    assert_eq!(end.payload["reason"], "compacted");
}

#[test]
fn compaction_kind_tells_auto_from_prune() {
    let mut t = Tracker::default();
    let mut kind = |mode| {
        let ev = OutEvent::Compacted {
            session: sid("a"),
            seq: 1,
            summary: String::new(),
            kept: 0,
            auto: true,
            mode,
        };
        t.observe(&ev).unwrap().payload["kind"].clone()
    };
    assert_eq!(kind(CompactionMode::Summary), "auto");
    assert_eq!(kind(CompactionMode::Prune), "prune");
}

#[test]
fn session_end_reasons_closed_and_hibernated() {
    let mut t = Tracker::default();
    t.observe(&started("a", None, None));
    let f = t
        .observe(&OutEvent::SessionHibernated {
            session: sid("a"),
            ts: 1,
        })
        .unwrap();
    assert_eq!(f.payload["reason"], "hibernated");
    let f = t
        .observe(&OutEvent::SessionEnded {
            session: sid("b"),
            ts: 1,
        })
        .unwrap();
    assert_eq!(f.payload["reason"], "closed");
}

#[test]
fn approval_and_question_park_fire_notification() {
    let mut t = Tracker::default();
    let f = t
        .observe(&OutEvent::ToolRequest {
            session: sid("a"),
            seq: 1,
            request_id: "r1".into(),
            tool: "bash".into(),
            input: r#"{"command":"ls"}"#.into(),
        })
        .unwrap();
    assert_eq!(f.event, HookEvent::Notification);
    assert_eq!(f.tool.as_deref(), Some("bash"));
    assert_eq!(f.payload["kind"], "approval");
    assert_eq!(f.payload["input"]["command"], "ls");

    let f = t
        .observe(&OutEvent::UserQuestion {
            session: sid("a"),
            seq: 2,
            request_id: "q1".into(),
            questions: Questions(vec![Question {
                question: "Which db?".into(),
                options: vec![],
                multi_select: false,
            }]),
        })
        .unwrap();
    assert_eq!(f.payload["kind"], "question");
    assert_eq!(f.payload["message"], "Which db?");
    assert_eq!(f.payload["questions"][0], "Which db?");
    assert!(f.tool.is_none());
}

#[test]
fn has_lifecycle_tracks_only_the_observer_points() {
    let none = Hooks {
        pre_tool_use: vec![super::super::HookSpec {
            command: "true".into(),
            tools: vec![],
            timeout_secs: 30,
        }],
        ..Default::default()
    };
    assert!(!none.has_lifecycle());
    let some = Hooks {
        notification: none.pre_tool_use.clone(),
        ..Default::default()
    };
    assert!(some.has_lifecycle());
    assert!(!some.is_empty());
}

/// A hung observer is killed at its timeout and does not stop later hooks:
/// the failure is logged, never acted on.
#[tokio::test]
async fn a_timed_out_observer_is_killed_and_the_next_still_runs() {
    let dir = tempfile::tempdir().unwrap();
    let marker = dir.path().join("ran");
    let spec = |command: String, timeout_secs| super::super::HookSpec {
        command,
        tools: vec![],
        timeout_secs,
    };
    let hooks = Hooks {
        stop: vec![
            spec("sleep 30".into(), 1),
            spec(format!("touch {}", marker.display()), 30),
        ],
        ..Default::default()
    };
    let started = std::time::Instant::now();
    hooks
        .run_observer(HookEvent::Stop, &sid("a"), None, &json!({}))
        .await;
    assert!(started.elapsed() < std::time::Duration::from_secs(10));
    assert!(marker.exists());
}

fn ended(s: &str) -> OutEvent {
    OutEvent::SessionEnded {
        session: sid(s),
        ts: 2,
    }
}

fn refusal(s: &str) -> OutEvent {
    OutEvent::Error {
        session: sid(s),
        seq: 0,
        message: "cannot resume a closed session id".into(),
    }
}

/// Only what core re-announces is marked: an ended child in the log is not,
/// and a refused resume leaves nothing behind.
#[test]
fn resume_marks_only_reannounced_ids_and_a_refusal_clears_its_mark() {
    let mut t = Tracker::default();
    t.observe_in(&InMsg::Resume {
        session: sid("root"),
        records: vec![
            (None, started("root", None, None)),
            (None, started("gone", Some("root"), None)),
            (None, started("live", Some("root"), None)),
            (None, ended("gone")),
        ],
    });
    assert_eq!(t.resuming.marked(), HashSet::from(["root", "live"]));

    // A refused resume: the supervisor's `Error` arrives, no start follows.
    t.observe_in(&InMsg::Resume {
        session: sid("closed"),
        records: vec![],
    });
    assert!(t.observe(&refusal("closed")).is_none());
    assert!(!t.resuming.marked().contains("closed"));

    // Ending a marked session drops its mark too.
    t.observe(&ended("live"));
    assert!(!t.resuming.marked().contains("live"));
}

fn already_live(s: &str) -> OutEvent {
    OutEvent::Error {
        session: sid(s),
        seq: 0,
        message: "cannot resume an already-live session id".into(),
    }
}

/// #791: a duplicate `Resume` refused as already-live before the original's
/// start lands must not spend the original's mark — in either order.
#[test]
fn a_refused_duplicate_resume_keeps_the_originals_mark() {
    let resume = || InMsg::Resume {
        session: sid("a"),
        records: vec![(None, started("a", None, None))],
    };
    let mut t = Tracker::default();
    t.observe_in(&resume());
    t.observe_in(&resume());
    assert!(t.observe(&already_live("a")).is_none());
    assert_eq!(source_of(&mut t, &started("a", None, None)), "resume");
    assert!(t.resuming.marked().is_empty());

    let mut t = Tracker::default();
    t.observe_in(&resume());
    t.observe_in(&resume());
    assert_eq!(source_of(&mut t, &started("a", None, None)), "resume");
    t.observe(&already_live("a"));
    assert!(
        t.resuming.marked().is_empty(),
        "the refusal spent the duplicate"
    );
}

/// #791: resuming a non-root target with the root's whole log marks only the
/// target's own still-live sub-tree, never its siblings' branches.
#[test]
fn a_non_root_resume_marks_only_the_targets_subtree() {
    let mut t = Tracker::default();
    t.observe_in(&InMsg::Resume {
        session: sid("child"),
        records: vec![
            (None, started("root", None, None)),
            (None, started("child", Some("root"), None)),
            (None, started("grand", Some("child"), None)),
            (None, started("sibling", Some("root"), None)),
            (None, started("nephew", Some("sibling"), None)),
        ],
    });
    assert_eq!(t.resuming.marked(), HashSet::from(["child", "grand"]));
}

#[test]
fn errors_for_untracked_sessions_are_not_kept() {
    let mut t = Tracker::default();
    t.observe(&refusal("never-started"));
    assert!(t.last_error.is_empty());
}

#[test]
fn a_lag_forgets_pending_turn_ends() {
    let mut t = Tracker::default();
    t.observe(&started("a", None, None));
    t.observe(&done("a"));
    t.forget_after_lag();
    assert!(t.observe(&status("a", AgentState::Done)).is_none());
}

/// A lag may have dropped a resumed `SessionStarted`: its mark goes too.
#[test]
fn a_lag_forgets_pending_resume_marks() {
    let mut t = Tracker::default();
    t.observe_in(&InMsg::Resume {
        session: sid("a"),
        records: vec![(None, started("a", None, None))],
    });
    t.forget_after_lag();
    assert!(t.resuming.marked().is_empty());
}

/// Core's cascade skips an already-live child without any event, so such a
/// child is never marked (its mark could never clear).
#[test]
fn an_already_live_cascade_child_is_not_marked() {
    let mut t = Tracker::default();
    t.observe(&started("kid", Some("root"), None));
    t.observe_in(&InMsg::Resume {
        session: sid("root"),
        records: vec![
            (None, started("root", None, None)),
            (None, started("kid", Some("root"), None)),
        ],
    });
    assert_eq!(t.resuming.marked(), HashSet::from(["root"]));
}

/// Backstop: pending resumes are capped, the oldest dropped first.
#[test]
fn pending_resumes_are_capped() {
    let mut t = Tracker::default();
    for i in 0..100 {
        t.observe_in(&InMsg::Resume {
            session: sid(&format!("s{i}")),
            records: vec![],
        });
    }
    let marked = t.resuming.marked();
    assert_eq!(marked.len(), 64);
    assert!(marked.contains("s99") && !marked.contains("s0"));
}
