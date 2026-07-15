use super::*;

#[test]
fn list_sessions_skips_one_bad_file() {
    let temp_dir = tempfile::tempdir().expect("Failed to create temp dir");
    let cwd = temp_dir.path();

    let started = |id: &SessionId, ts: u64| {
        LogRecord::new(
            id.clone(),
            LogPayload::Out(OutEvent::SessionStarted {
                session: id.clone(),
                parent: None,
                profile: "build".to_string(),
                model: None,
                root: true,
                ts,
            }),
        )
    };

    let good = SessionId::new("good");
    append(cwd, &good, &started(&good, 1000)).expect("append should succeed");

    // A file with interior corruption: read() errors, so listing must skip it
    // rather than abort the whole enumeration.
    let bad = SessionId::new("bad");
    append(cwd, &bad, &started(&bad, 2000)).expect("append should succeed");
    let bad_path = session_path(cwd, &bad).expect("session_path should succeed");
    let mut f = OpenOptions::new()
        .append(true)
        .open(&bad_path)
        .expect("Failed to open file");
    writeln!(f, "GARBAGE mid-file").expect("Failed to write corrupt line");
    drop(f);
    append(
        cwd,
        &bad,
        &LogRecord::new(
            bad.clone(),
            LogPayload::Out(OutEvent::Done {
                session: bad.clone(),
                seq: 1,
            }),
        ),
    )
    .expect("append should succeed");

    let sessions = list_sessions(cwd).expect("list_sessions should skip the bad file");
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].id, good);
}

#[test]
fn multi_session_interleaving() {
    let temp_dir = tempfile::tempdir().expect("Failed to create temp dir");
    let cwd = temp_dir.path();
    let root_id = SessionId::new("root-session");
    let sub_id = SessionId::new("sub-session");

    let root_record = LogRecord::new(
        root_id.clone(),
        LogPayload::In(InMsg::prompt(root_id.clone(), "root".to_string())),
    );

    let sub_record = LogRecord::new(
        sub_id.clone(),
        LogPayload::In(InMsg::prompt(sub_id.clone(), "sub".to_string())),
    );

    append(cwd, &root_id, &root_record).expect("append should succeed");
    append(cwd, &root_id, &sub_record).expect("append should succeed");

    let records = read(cwd, &root_id).expect("read should succeed");
    assert_eq!(records.len(), 2);

    let root_records: Vec<_> = records.iter().filter(|r| r.session == root_id).collect();
    let sub_records: Vec<_> = records.iter().filter(|r| r.session == sub_id).collect();

    assert_eq!(root_records.len(), 1);
    assert_eq!(sub_records.len(), 1);
}

#[test]
fn children_of_finds_direct_children() {
    let root_id = SessionId::new("root");
    let child1_id = SessionId::new("child1");
    let child2_id = SessionId::new("child2");
    let grandchild_id = SessionId::new("grandchild");

    let sessions = vec![
        SessionMeta {
            id: root_id.clone(),
            agent: "build".to_string(),
            model: None,
            created: 0,
            last_active: 0,
            parent: None,
            root: true,
            first_prompt: None,
        },
        SessionMeta {
            id: child1_id.clone(),
            agent: "build".to_string(),
            model: None,
            created: 0,
            last_active: 0,
            parent: Some(root_id.clone()),
            root: false,
            first_prompt: None,
        },
        SessionMeta {
            id: child2_id.clone(),
            agent: "build".to_string(),
            model: None,
            created: 0,
            last_active: 0,
            parent: Some(root_id.clone()),
            root: false,
            first_prompt: None,
        },
        SessionMeta {
            id: grandchild_id.clone(),
            agent: "build".to_string(),
            model: None,
            created: 0,
            last_active: 0,
            parent: Some(child1_id.clone()),
            root: false,
            first_prompt: None,
        },
    ];

    let children = children_of(&sessions, &root_id);
    assert_eq!(children.len(), 2);
    assert!(children.iter().any(|s| s.id == child1_id));
    assert!(children.iter().any(|s| s.id == child2_id));

    let grandchildren = children_of(&sessions, &child1_id);
    assert_eq!(grandchildren.len(), 1);
    assert_eq!(grandchildren[0].id, grandchild_id);
}

#[test]
fn root_of_walks_up_parent_chain() {
    let root_id = SessionId::new("root");
    let child1_id = SessionId::new("child1");
    let child2_id = SessionId::new("child2");

    let sessions = vec![
        SessionMeta {
            id: root_id.clone(),
            agent: "build".to_string(),
            model: None,
            created: 0,
            last_active: 0,
            parent: None,
            root: true,
            first_prompt: None,
        },
        SessionMeta {
            id: child1_id.clone(),
            agent: "build".to_string(),
            model: None,
            created: 0,
            last_active: 0,
            parent: Some(root_id.clone()),
            root: false,
            first_prompt: None,
        },
        SessionMeta {
            id: child2_id.clone(),
            agent: "build".to_string(),
            model: None,
            created: 0,
            last_active: 0,
            parent: Some(child1_id.clone()),
            root: false,
            first_prompt: None,
        },
    ];

    assert_eq!(root_of(&sessions, &root_id), root_id);
    assert_eq!(root_of(&sessions, &child1_id), root_id);
    assert_eq!(root_of(&sessions, &child2_id), root_id);
}

#[test]
fn root_of_returns_self_for_orphan_session() {
    let orphan_id = SessionId::new("orphan");

    let sessions = vec![SessionMeta {
        id: orphan_id.clone(),
        agent: "build".to_string(),
        model: None,
        created: 0,
        last_active: 0,
        parent: None,
        root: true,
        first_prompt: None,
    }];

    assert_eq!(root_of(&sessions, &orphan_id), orphan_id);
}

#[test]
fn first_prompt_snippet_truncates_on_word_boundary() {
    // Short single line: verbatim, no ellipsis.
    assert_eq!(first_prompt_snippet("fix the bug"), "fix the bug");

    // Multi-line: only the first line, with an ellipsis for the dropped tail.
    assert_eq!(
        first_prompt_snippet("summarize this\nplus a lot more context"),
        "summarize this…"
    );

    // Over the char budget: cut at a word boundary, ellipsis appended, and the
    // result (minus the ellipsis) never exceeds the budget.
    let long = "please refactor the entire authentication subsystem into a much cleaner design";
    let snippet = first_prompt_snippet(long);
    assert!(
        snippet.ends_with('…'),
        "long prompt should be truncated: {snippet}"
    );
    let body = snippet.trim_end_matches('…');
    assert!(
        !body.contains("cleaner"),
        "tail past the budget dropped: {snippet}"
    );
    assert!(
        body.chars().count() <= FIRST_PROMPT_MAX,
        "snippet body over budget: {snippet}"
    );
    assert!(long.starts_with(body), "no mid-word cut: {snippet}");
}

#[test]
fn list_sessions_captures_first_prompt_content_and_legacy_text() {
    use entanglement_core::ContentPart;

    let temp_dir = tempfile::tempdir().expect("Failed to create temp dir");
    let cwd = temp_dir.path();

    let started = |id: &SessionId| {
        LogRecord::new(
            id.clone(),
            LogPayload::Out(OutEvent::SessionStarted {
                session: id.clone(),
                parent: None,
                profile: "build".to_string(),
                model: None,
                root: true,
                ts: 1000,
            }),
        )
    };

    // Session A: a modern content-block prompt (#197/ADR-0064). The `Prompt`
    // record precedes `SessionStarted`, matching the real inbound-logging order.
    let a = SessionId::new("modern");
    let a_prompt = LogRecord::new(
        a.clone(),
        LogPayload::In(InMsg::Prompt {
            session: a.clone(),
            content: vec![ContentPart::text("hello from the block path")],
        }),
    );
    append(cwd, &a, &a_prompt).expect("append should succeed");
    append(cwd, &a, &started(&a)).expect("append should succeed");

    // Session B: a legacy `text:`-shaped prompt written before the migration,
    // deserialized via the serde alias (ADR-0064 back-compat).
    let b = SessionId::new("legacy");
    let legacy_line = r#"{"ts":1,"session":"legacy","payload":{"direction":"in","kind":"prompt","session":"legacy","text":"legacy prompt text"}}"#;
    let b_path = session_path(cwd, &b).expect("session_path should succeed");
    let mut f = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&b_path)
        .expect("open should succeed");
    writeln!(f, "{legacy_line}").expect("write should succeed");
    drop(f);
    append(cwd, &b, &started(&b)).expect("append should succeed");

    let sessions = list_sessions(cwd).expect("list_sessions should succeed");

    let a_meta = sessions.iter().find(|s| s.id == a).expect("A present");
    assert_eq!(
        a_meta.first_prompt.as_deref(),
        Some("hello from the block path")
    );

    let b_meta = sessions.iter().find(|s| s.id == b).expect("B present");
    assert_eq!(b_meta.first_prompt.as_deref(), Some("legacy prompt text"));
}

#[test]
fn forward_compatible_multi_session_log_rebuilds_tree() {
    let temp_dir = tempfile::tempdir().expect("Failed to create temp dir");
    let cwd = temp_dir.path();
    let root_id = SessionId::new("root");
    let child1_id = SessionId::new("child1");
    let child2_id = SessionId::new("child2");

    let root_started = LogRecord::new(
        root_id.clone(),
        LogPayload::Out(OutEvent::SessionStarted {
            session: root_id.clone(),
            parent: None,
            profile: "build".to_string(),
            model: None,
            root: true,
            ts: 1000,
        }),
    );

    let child1_started = LogRecord::new(
        child1_id.clone(),
        LogPayload::Out(OutEvent::SessionStarted {
            session: child1_id.clone(),
            parent: Some(root_id.clone()),
            profile: "build".to_string(),
            model: None,
            root: false,
            ts: 2000,
        }),
    );

    let child2_started = LogRecord::new(
        child2_id.clone(),
        LogPayload::Out(OutEvent::SessionStarted {
            session: child2_id.clone(),
            parent: Some(root_id.clone()),
            profile: "build".to_string(),
            model: None,
            root: false,
            ts: 3000,
        }),
    );

    append(cwd, &root_id, &root_started).expect("append should succeed");
    append(cwd, &root_id, &child1_started).expect("append should succeed");
    append(cwd, &root_id, &child2_started).expect("append should succeed");

    let records = read(cwd, &root_id).expect("read should succeed");
    assert_eq!(records.len(), 3);

    let sessions = list_sessions(cwd).expect("list_sessions should succeed");
    assert_eq!(sessions.len(), 1, "Only root session file exists");

    let root_meta = sessions
        .iter()
        .find(|s| s.id == root_id)
        .expect("root should exist");
    assert_eq!(root_meta.parent, None);
    assert!(root_meta.root);
}
