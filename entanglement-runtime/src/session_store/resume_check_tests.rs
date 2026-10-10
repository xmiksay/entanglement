use super::*;

fn out(ev: OutEvent) -> LogRecord {
    LogRecord::new(SessionId::new("root"), LogPayload::Out(ev))
}

fn started(sid: &str, parent: Option<&str>, agent: &str) -> LogRecord {
    out(OutEvent::SessionStarted {
        session: SessionId::new(sid),
        parent: parent.map(SessionId::new),
        predecessor: None,
        agent: agent.to_string(),
        model: None,
        root: parent.is_none(),
        ts: 0,
        user: None,
    })
}

fn changed(sid: &str, agent: &str) -> LogRecord {
    out(OutEvent::AgentChanged {
        session: SessionId::new(sid),
        agent: agent.to_string(),
    })
}

/// The current built-in roster plus one embedder agent.
fn known(name: &str) -> bool {
    matches!(name, "general" | "plan" | "debug" | "site-editor")
}

fn check(records: &[LogRecord]) -> ResumeCheck {
    check_for_resume(records, known).expect("no gap")
}

#[test]
fn a_retired_build_root_resumes_as_general_in_the_default_mode() {
    let c = check(&[started("root", None, "build")]);
    let [f] = c.fallbacks.as_slice() else {
        panic!("{c:?}")
    };
    assert_eq!(f.forced_mode(false), None, "build was never read-only");
    assert_eq!(
        f.note(false),
        "note: session used retired agent `build`; resuming as `general` (ADR-0215)"
    );
}

#[test]
fn a_read_only_retired_root_is_forced_into_research_unless_mode_is_explicit() {
    let c = check(&[started("root", None, "explore")]);
    let f = &c.fallbacks[0];
    assert_eq!(f.forced_mode(false), Some("research"));
    assert_eq!(
        f.note(false),
        "note: session used retired agent `explore`; resuming as `general` in `research` mode \
         (ADR-0215)"
    );
    assert_eq!(f.forced_mode(true), None, "an explicit --mode wins");
    assert!(!f.note(true).contains("mode"), "{}", f.note(true));
}

/// The note follows the *last* agent the session named: a `build` root that
/// switched to `plan` resumes as plan — no fallback, no note.
#[test]
fn a_switch_away_from_a_retired_agent_is_no_fallback() {
    let c = check(&[started("root", None, "build"), changed("root", "plan")]);
    assert_eq!(c, ResumeCheck::default());
}

/// A switch *to* a retired agent keeps the last known one (replay skips an
/// unknown `AgentChanged`), and that is what the note names.
#[test]
fn a_switch_to_a_retired_agent_resumes_as_the_last_known_one() {
    let c = check(&[
        started("root", None, "site-editor"),
        changed("root", "explore"),
    ]);
    assert_eq!(c.fallbacks[0].resumed_as, "site-editor");
    assert_eq!(c.fallbacks[0].retired, "explore");
}

/// A child is named in its note, and a child resume won't rebuild (closed or
/// hibernated) gets none.
#[test]
fn children_are_named_and_only_live_ones_count() {
    let c = check(&[
        started("root", None, "site-editor"),
        started("kid", Some("root"), "explore"),
        started("gone", Some("root"), "explore"),
        out(OutEvent::SessionEnded {
            session: SessionId::new("gone"),
            ts: 0,
        }),
    ]);
    let notes: Vec<String> = c.fallbacks.iter().map(|f| f.note(false)).collect();
    assert_eq!(
        notes,
        ["note: session `kid` used retired agent `explore`; resuming as `general` in `research` \
          mode (ADR-0215)"]
    );
}

/// A user agent that still carries a retired name is a real agent: no fallback.
#[test]
fn a_user_agent_named_like_a_retired_one_is_no_fallback() {
    let c = check_for_resume(&[started("root", None, "build")], |n| n == "build").unwrap();
    assert_eq!(c, ResumeCheck::default());
}

#[test]
fn a_gap_still_refuses() {
    let gap = LogRecord::new(SessionId::new("root"), LogPayload::Gap { dropped: 2 });
    let err = check_for_resume(&[started("root", None, "general"), gap], known).unwrap_err();
    assert!(err.to_string().contains("missing 2 record"), "{err}");
}
