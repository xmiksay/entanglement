//! `list_sessions` scan of `SessionMetaChanged` records (ADR-0151) — its own
//! module because `tests_sessions.rs` sits at the 400-line file cap.

use super::*;

#[test]
fn list_sessions_takes_the_last_session_meta_name() {
    let temp_dir = tempfile::tempdir().expect("Failed to create temp dir");
    let cwd = temp_dir.path();
    let sid = SessionId::new("named");
    let child = SessionId::new("named-child");
    let meta = |session: &SessionId, name: &str| {
        LogPayload::Out(OutEvent::SessionMetaChanged {
            session: session.clone(),
            name: Some(name.to_string()),
            action: None,
        })
    };
    // Root start, then: root named, a child's interleaved record (must not
    // rename the root's row), then a rename — last write wins (unlike
    // first_prompt's first-write scan).
    let payloads = vec![
        LogPayload::Out(OutEvent::SessionStarted {
            session: sid.clone(),
            parent: None,
            predecessor: None,
            profile: "build".to_string(),
            model: None,
            root: true,
            ts: 1000,
            user: None,
        }),
        meta(&sid, "first name"),
        meta(&child, "child name"),
        meta(&sid, "final name"),
    ];
    for p in payloads {
        append(cwd, &sid, &LogRecord::new(sid.clone(), p)).expect("append should succeed");
    }

    let sessions = list_sessions(cwd).expect("list_sessions should succeed");
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].name.as_deref(), Some("final name"));
}
