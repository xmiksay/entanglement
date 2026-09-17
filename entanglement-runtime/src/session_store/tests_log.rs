use super::*;

#[test]
fn base_dir_returns_data_dir_entanglement_sessions() {
    let dir = base_dir().expect("base_dir should succeed");
    assert!(dir.ends_with("entanglement/sessions"));
}

#[test]
fn base_dir_creates_directory_if_missing() {
    let dir = base_dir().expect("base_dir should succeed");
    assert!(dir.exists(), "Base directory should exist");
    assert!(dir.is_dir(), "Base should be a directory");
}

#[test]
fn safe_cwd_name_replaces_slashes() {
    assert_eq!(
        safe_cwd_name(Path::new("/mnt/nvme/agent")),
        "mnt-nvme-agent"
    );
    assert_eq!(safe_cwd_name(Path::new("/a/b/c")), "a-b-c");
}

#[test]
fn safe_cwd_name_trims_leading_dash() {
    assert_eq!(safe_cwd_name(Path::new("/a-b")), "a-b");
    assert_eq!(safe_cwd_name(Path::new("///a")), "a");
}

#[test]
fn safe_cwd_name_handles_windows_paths() {
    assert_eq!(safe_cwd_name(Path::new("C:\\Users\\test")), "C:-Users-test");
}

#[test]
fn safe_cwd_name_preserves_spaces_and_unicode() {
    assert_eq!(safe_cwd_name(Path::new("/my path")), "my path");
    assert_eq!(safe_cwd_name(Path::new("/héllo/wørld")), "héllo-wørld");
}

#[test]
fn append_and_read_roundtrip() {
    let temp_dir = tempfile::tempdir().expect("Failed to create temp dir");
    let cwd = temp_dir.path();
    let session_id = SessionId::new("test-session");

    let record1 = LogRecord::new(
        session_id.clone(),
        LogPayload::In(InMsg::prompt(session_id.clone(), "hello".to_string())),
    );

    let record2 = LogRecord::new(
        session_id.clone(),
        LogPayload::Out(OutEvent::Done {
            session: session_id.clone(),
            seq: 1,
        }),
    );

    append(cwd, &session_id, &record1).expect("append should succeed");
    append(cwd, &session_id, &record2).expect("append should succeed");

    let records = read(cwd, &session_id).expect("read should succeed");
    assert_eq!(records.len(), 2);
    assert_eq!(records[0].session, session_id);
    assert_eq!(records[1].session, session_id);

    match &records[0].payload {
        LogPayload::In(InMsg::Prompt { content, .. }) => {
            assert_eq!(entanglement_core::content_text(content), "hello")
        }
        _ => panic!("Expected Prompt"),
    }

    match &records[1].payload {
        LogPayload::Out(OutEvent::Done { .. }) => {}
        _ => panic!("Expected Done"),
    }
}

#[test]
fn pair_records_associates_each_prompt_with_following_events() {
    let sid = SessionId::new("s");
    let prompt = |t: &str| {
        LogRecord::new(
            sid.clone(),
            LogPayload::In(InMsg::prompt(sid.clone(), t.to_string())),
        )
    };
    let text = |seq: u64, t: &str| {
        LogRecord::new(
            sid.clone(),
            LogPayload::Out(OutEvent::TextDelta {
                session: sid.clone(),
                seq,
                text: t.to_string(),
            }),
        )
    };
    let done = |seq: u64| {
        LogRecord::new(
            sid.clone(),
            LogPayload::Out(OutEvent::Done {
                session: sid.clone(),
                seq,
            }),
        )
    };

    let records = vec![
        prompt("hi"),
        text(1, "hello"),
        done(2),
        prompt("again"),
        text(3, "yo"),
        done(4),
    ];

    let paired = pair_records(&records);
    assert_eq!(paired.len(), 4);

    // First prompt pairs with the first out event; it's consumed so the
    // trailing events of that turn pair with `None`.
    match &paired[0] {
        (Some(InMsg::Prompt { content, .. }), OutEvent::TextDelta { .. }) => {
            assert_eq!(entanglement_core::content_text(content), "hi")
        }
        other => panic!("unexpected pairing: {other:?}"),
    }
    assert!(matches!(paired[1], (None, OutEvent::Done { .. })));
    match &paired[2] {
        (Some(InMsg::Prompt { content, .. }), OutEvent::TextDelta { .. }) => {
            assert_eq!(entanglement_core::content_text(content), "again")
        }
        other => panic!("unexpected pairing: {other:?}"),
    }
    assert!(matches!(paired[3], (None, OutEvent::Done { .. })));
}

#[test]
fn pair_records_drops_trailing_inbound_without_output() {
    let sid = SessionId::new("s");
    let records = vec![LogRecord::new(
        sid.clone(),
        LogPayload::In(InMsg::prompt(sid.clone(), "no reply yet".to_string())),
    )];
    assert!(pair_records(&records).is_empty());
}

#[test]
fn read_skips_corrupt_lines() {
    let temp_dir = tempfile::tempdir().expect("Failed to create temp dir");
    let cwd = temp_dir.path();
    let session_id = SessionId::new("test-corrupt");

    let valid_record = LogRecord::new(
        session_id.clone(),
        LogPayload::In(InMsg::prompt(session_id.clone(), "valid".to_string())),
    );

    append(cwd, &session_id, &valid_record).expect("append should succeed");

    let path = session_path(cwd, &session_id).expect("session_path should succeed");
    let mut file = OpenOptions::new()
        .append(true)
        .open(&path)
        .expect("Failed to open file");
    writeln!(file, "{{invalid json}}").expect("Failed to write corrupt line");
    writeln!(file).expect("Failed to write empty line");

    let records = read(cwd, &session_id).expect("read should succeed");
    assert_eq!(records.len(), 1);
}

#[test]
fn read_tolerates_truncated_tail_line() {
    let temp_dir = tempfile::tempdir().expect("Failed to create temp dir");
    let cwd = temp_dir.path();
    let session_id = SessionId::new("test-tail");

    append(
        cwd,
        &session_id,
        &LogRecord::new(
            session_id.clone(),
            LogPayload::In(InMsg::prompt(session_id.clone(), "kept".to_string())),
        ),
    )
    .expect("append should succeed");

    // Simulate a crash mid-append: a partial final line, no trailing newline.
    let path = session_path(cwd, &session_id).expect("session_path should succeed");
    let mut file = OpenOptions::new()
        .append(true)
        .open(&path)
        .expect("Failed to open file");
    write!(file, "{{\"ts\":123,\"sess").expect("Failed to write truncated line");
    drop(file);

    let records = read(cwd, &session_id).expect("truncated tail is tolerated");
    assert_eq!(records.len(), 1);
}

#[test]
fn read_rejects_interior_corruption() {
    let temp_dir = tempfile::tempdir().expect("Failed to create temp dir");
    let cwd = temp_dir.path();
    let session_id = SessionId::new("test-interior");

    append(
        cwd,
        &session_id,
        &LogRecord::new(
            session_id.clone(),
            LogPayload::In(InMsg::prompt(session_id.clone(), "one".to_string())),
        ),
    )
    .expect("append should succeed");

    // Garbage in the middle...
    let path = session_path(cwd, &session_id).expect("session_path should succeed");
    let mut file = OpenOptions::new()
        .append(true)
        .open(&path)
        .expect("Failed to open file");
    writeln!(file, "{{ not json").expect("Failed to write corrupt line");
    drop(file);

    // ...followed by a valid record → a hole, not a truncated tail.
    append(
        cwd,
        &session_id,
        &LogRecord::new(
            session_id.clone(),
            LogPayload::Out(OutEvent::Done {
                session: session_id.clone(),
                seq: 1,
            }),
        ),
    )
    .expect("append should succeed");

    let err = read(cwd, &session_id).expect_err("interior corruption must error");
    assert!(
        err.to_string().contains("Interior corruption"),
        "unexpected error: {err}"
    );
}

#[test]
fn integrity_gap_detects_and_sums_tombstones() {
    let sid = SessionId::new("s");
    let records = vec![
        LogRecord::new(sid.clone(), LogPayload::Gap { dropped: 4 }),
        LogRecord::new(
            sid.clone(),
            LogPayload::Out(OutEvent::Done {
                session: sid.clone(),
                seq: 1,
            }),
        ),
        LogRecord::new(sid.clone(), LogPayload::Gap { dropped: 6 }),
    ];
    assert_eq!(integrity_gap(&records), Some(10));
}

#[test]
fn integrity_gap_none_for_clean_log() {
    let sid = SessionId::new("s");
    let records = vec![LogRecord::new(
        sid.clone(),
        LogPayload::Out(OutEvent::Done {
            session: sid.clone(),
            seq: 1,
        }),
    )];
    assert_eq!(integrity_gap(&records), None);
}

/// A `SessionStarted.profile` naming a retired agent (ADR-0207 stage 6a) is
/// caught, named, and pointed at its replacement.
#[test]
fn retired_agent_finds_a_retired_session_started_profile() {
    let sid = SessionId::new("s");
    let records = vec![LogRecord::new(
        sid.clone(),
        LogPayload::Out(OutEvent::SessionStarted {
            session: sid.clone(),
            parent: None,
            predecessor: None,
            profile: "build".to_string(),
            model: None,
            root: true,
            ts: 0,
            user: None,
            sponsored: false,
        }),
    )];
    let (retired, replacement) = retired_agent(&records).expect("build is retired");
    assert_eq!(retired, "build");
    assert!(replacement.contains("general"), "{replacement}");
}

/// A retired name can also arrive via a legacy log's `AgentChanged` record
/// (a genuine pre-ADR-0207 `SetAgent` switch) — not just the session's own
/// starting `SessionStarted.profile`.
#[test]
fn retired_agent_finds_a_retired_agent_changed_record() {
    let sid = SessionId::new("s");
    let records = vec![
        LogRecord::new(
            sid.clone(),
            LogPayload::Out(OutEvent::SessionStarted {
                session: sid.clone(),
                parent: None,
                predecessor: None,
                profile: "general".to_string(),
                model: None,
                root: true,
                ts: 0,
                user: None,
                sponsored: false,
            }),
        ),
        LogRecord::new(
            sid.clone(),
            LogPayload::Out(OutEvent::AgentChanged {
                session: sid.clone(),
                agent: "explore".to_string(),
            }),
        ),
    ];
    let (retired, replacement) = retired_agent(&records).expect("explore is retired");
    assert_eq!(retired, "explore");
    assert!(replacement.contains("research"), "{replacement}");
}

#[test]
fn retired_agent_none_for_a_current_roster_name() {
    let sid = SessionId::new("s");
    let records = vec![LogRecord::new(
        sid.clone(),
        LogPayload::Out(OutEvent::SessionStarted {
            session: sid.clone(),
            parent: None,
            predecessor: None,
            profile: "general".to_string(),
            model: None,
            root: true,
            ts: 0,
            user: None,
            sponsored: false,
        }),
    )];
    assert_eq!(retired_agent(&records), None);
}

/// A prompt sent while a batch is parked lands just before the executor's
/// `ToolResult`, both ahead of the next event: the result must not displace
/// the prompt, and a second queued prompt pairs with the event after.
#[test]
fn pair_records_keeps_prompts_queued_behind_other_inbound() {
    let sid = SessionId::new("s");
    let inbound = |msg: InMsg| LogRecord::new(sid.clone(), LogPayload::In(msg));
    let done = |seq: u64| {
        LogRecord::new(
            sid.clone(),
            LogPayload::Out(OutEvent::Done {
                session: sid.clone(),
                seq,
            }),
        )
    };
    let records = vec![
        inbound(InMsg::prompt(sid.clone(), "steer")),
        inbound(InMsg::tool_result(sid.clone(), "c1", "out")),
        inbound(InMsg::Stop {
            session: sid.clone(),
        }),
        done(1),
        done(2),
        done(3),
    ];
    let paired = pair_records(&records);
    assert!(matches!(&paired[0].0, Some(InMsg::Prompt { content, .. })
        if entanglement_core::content_text(content) == "steer"));
    assert!(matches!(paired[1].0, Some(InMsg::Stop { .. })));
    assert!(paired[2].0.is_none());
}
