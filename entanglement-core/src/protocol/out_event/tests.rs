use crate::protocol::*;

#[test]
fn task_list_roundtrips() {
    let ev = OutEvent::TaskList {
        session: SessionId::new("s1"),
        seq: 3,
        content: "- [x] do thing\n- [ ] next thing".into(),
    };
    let json = serde_json::to_string(&ev).unwrap();
    let back: OutEvent = serde_json::from_str(&json).unwrap();
    assert_eq!(ev, back);
}

#[test]
fn skill_active_roundtrips_when_set_and_when_cleared() {
    let active = OutEvent::SkillActive {
        session: SessionId::new("s1"),
        seq: 3,
        skill_id: Some("commit".into()),
        allowed_tools: Some(vec!["bash".into(), "read".into()]),
    };
    let json = serde_json::to_string(&active).unwrap();
    let back: OutEvent = serde_json::from_str(&json).unwrap();
    assert_eq!(active, back);

    let cleared = OutEvent::SkillActive {
        session: SessionId::new("s1"),
        seq: 4,
        skill_id: None,
        allowed_tools: None,
    };
    let json = serde_json::to_string(&cleared).unwrap();
    let back: OutEvent = serde_json::from_str(&json).unwrap();
    assert_eq!(cleared, back);
    assert_eq!(cleared.session(), Some(&SessionId::new("s1")));
    assert_eq!(cleared.seq(), Some(4));
}

#[test]
fn usage_roundtrips_with_and_without_cost() {
    for cost in [Some(0.0123), None] {
        let ev = OutEvent::Usage {
            session: SessionId::new("s1"),
            seq: 5,
            input_tokens: 100,
            output_tokens: 40,
            cached_input_tokens: 30,
            cache_write_tokens: 0,
            cost_usd: cost,
            purpose: UsagePurpose::Compaction,
        };
        let json = serde_json::to_string(&ev).unwrap();
        let back: OutEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(ev, back);
        assert_eq!(back.seq(), Some(5));
    }
}

#[test]
fn usage_without_purpose_on_the_wire_reads_as_a_turn_round() {
    // Logs written before `purpose` existed carry no field at all.
    let json = r#"{"kind":"usage","session":"s1","seq":5,"input_tokens":1,"output_tokens":2,"cached_input_tokens":0,"cache_write_tokens":0,"cost_usd":null}"#;
    let back: OutEvent = serde_json::from_str(json).unwrap();
    assert!(matches!(
        back,
        OutEvent::Usage {
            purpose: UsagePurpose::Turn,
            ..
        }
    ));
}

#[test]
fn tool_call_delta_roundtrips() {
    let ev = OutEvent::ToolCallDelta {
        session: SessionId::new("s1"),
        seq: 7,
        request_id: "call_1".into(),
        tool: "edit".into(),
        delta: r#"{"path":"src/"#.into(),
    };
    let json = serde_json::to_string(&ev).unwrap();
    assert!(json.contains(r#""kind":"tool_call_delta""#), "{json}");
    let back: OutEvent = serde_json::from_str(&json).unwrap();
    assert_eq!(ev, back);
    assert_eq!(back.seq(), Some(7));
    assert_eq!(back.session(), Some(&SessionId::new("s1")));
}

#[test]
fn plan_roundtrips() {
    let ev = OutEvent::Plan {
        session: SessionId::new("s1"),
        seq: 2,
        content: "# Plan\n1. ...".into(),
        path: ".entanglement/plans/s1.md".into(),
    };
    let json = serde_json::to_string(&ev).unwrap();
    let back: OutEvent = serde_json::from_str(&json).unwrap();
    assert_eq!(ev, back);
}

#[test]
fn plan_deserializes_a_pre_513_log_with_no_path_field() {
    let json = "{\"kind\":\"plan\",\"session\":\"s1\",\"seq\":2,\"content\":\"# Plan\"}";
    let ev: OutEvent = serde_json::from_str(json).unwrap();
    assert!(matches!(
        ev,
        OutEvent::Plan { content, path, .. } if content == "# Plan" && path.is_empty()
    ));
}

#[test]
fn user_question_roundtrips_with_multiple_questions() {
    let ev = OutEvent::UserQuestion {
        session: SessionId::new("s1"),
        seq: 4,
        request_id: "q1".into(),
        questions: Questions(vec![
            Question {
                question: "Which approach?".into(),
                options: vec![
                    QuestionOption {
                        label: "REST".into(),
                        description: Some("HTTP + JSON".into()),
                    },
                    QuestionOption {
                        label: "gRPC".into(),
                        description: None,
                    },
                ],
                multi_select: false,
            },
            Question {
                question: "Which regions?".into(),
                options: vec![
                    QuestionOption {
                        label: "us-east".into(),
                        description: None,
                    },
                    QuestionOption {
                        label: "eu-west".into(),
                        description: None,
                    },
                ],
                multi_select: true,
            },
        ]),
    };
    let json = serde_json::to_string(&ev).unwrap();
    assert!(json.contains(r#""questions":[{"#), "{json}");
    let back: OutEvent = serde_json::from_str(&json).unwrap();
    assert_eq!(ev, back);
}

#[test]
fn user_question_deserializes_legacy_flat_shape() {
    let json = r#"{"kind":"user_question","session":"s1","seq":4,"request_id":"q1","question":"Which?","options":[{"label":"A"}],"allow_free_form":true}"#;
    let ev: OutEvent = serde_json::from_str(json).unwrap();
    match ev {
        OutEvent::UserQuestion { questions, .. } => {
            assert_eq!(questions.0.len(), 1);
            assert_eq!(questions.0[0].question, "Which?");
            assert_eq!(questions.0[0].options.len(), 1);
            assert!(!questions.0[0].multi_select);
        }
        _ => panic!("expected UserQuestion"),
    }
}

#[test]
fn session_list_event_roundtrips() {
    let ev = OutEvent::SessionList {
        correlation_id: "q1".into(),
        sessions: vec![
            SessionInfo {
                session: SessionId::new("root"),
                parent: None,
                predecessor: None,
                agent: "build".into(),
                root: true,
                user: None,
            },
            SessionInfo {
                session: SessionId::new("child"),
                parent: Some(SessionId::new("root")),
                predecessor: None,
                agent: "explore".into(),
                root: false,
                user: None,
            },
        ],
    };
    assert_eq!(ev.seq(), None, "SessionList is a lifecycle event, no seq");
    assert_eq!(ev.session(), None, "SessionList names no single session");
    let json = serde_json::to_string(&ev).unwrap();
    let back: OutEvent = serde_json::from_str(&json).unwrap();
    assert_eq!(ev, back);
}

#[test]
fn question_list_event_is_session_less_and_seq_less() {
    let ev = OutEvent::QuestionList {
        correlation_id: "c1".into(),
        questions: vec![PendingQuestion {
            session: SessionId::new("s1"),
            request_id: "q1".into(),
            questions: Questions(vec![Question {
                question: "Which DB?".into(),
                options: vec![],
                multi_select: false,
            }]),
        }],
    };
    assert_eq!(ev.seq(), None);
    assert_eq!(ev.session(), None);
    let json = serde_json::to_string(&ev).unwrap();
    assert_eq!(serde_json::from_str::<OutEvent>(&json).unwrap(), ev);
}

#[test]
fn persistence_transitions_are_session_less_seq_less_and_wire_stable() {
    // `session()` is `None` on purpose (#767): the persistence tap skips
    // session-less events, so its own outage signal is never persisted.
    let degraded = OutEvent::PersistenceDegraded {
        root: SessionId::new("r1"),
        error: "disk full".into(),
    };
    let recovered = OutEvent::PersistenceRecovered {
        root: SessionId::new("r1"),
        lost: 3,
    };
    for ev in [&degraded, &recovered] {
        assert_eq!(ev.seq(), None);
        assert_eq!(ev.session(), None);
        let json = serde_json::to_string(ev).unwrap();
        assert_eq!(&serde_json::from_str::<OutEvent>(&json).unwrap(), ev);
    }
    assert_eq!(
        serde_json::to_value(&degraded).unwrap(),
        serde_json::json!({"kind": "persistence_degraded", "root": "r1", "error": "disk full"})
    );
    assert_eq!(
        serde_json::to_value(&recovered).unwrap(),
        serde_json::json!({"kind": "persistence_recovered", "root": "r1", "lost": 3})
    );
}

#[test]
fn operation_list_event_is_session_less_and_seq_less() {
    let ev = OutEvent::OperationList {
        correlation_id: "c1".into(),
        operations: vec![OperationInfo {
            session: SessionId::new("s1"),
            kind: OperationKind::Job,
            handle: "j-abc".into(),
            launched_by: "echo hi".into(),
            elapsed_secs: 3,
            status: OperationStatus::Running,
        }],
    };
    assert_eq!(ev.seq(), None);
    assert_eq!(ev.session(), None);
    let json = serde_json::to_string(&ev).unwrap();
    assert_eq!(serde_json::from_str::<OutEvent>(&json).unwrap(), ev);
}

#[test]
fn pending_list_event_is_session_less_and_seq_less() {
    let ev = OutEvent::PendingList {
        correlation_id: "c1".into(),
        approvals: vec![PendingApproval {
            session: SessionId::new("s1"),
            request_id: "r1".into(),
            kind: ApprovalKind::Tool,
            tool: "bash".into(),
            input: r#"{"command":"ls"}"#.into(),
            reason: "mode `build` grades `bash` Ask".into(),
            scopes: vec![ApprovalScope::Once, ApprovalScope::Session],
            since_ms: 1,
        }],
    };
    assert_eq!(ev.seq(), None);
    assert_eq!(ev.session(), None);
    let json = serde_json::to_string(&ev).unwrap();
    assert!(json.contains(r#""kind":"tool""#), "{json}");
    assert_eq!(serde_json::from_str::<OutEvent>(&json).unwrap(), ev);
}

#[test]
fn mcp_list_and_changed_events_roundtrip() {
    let list_ev = OutEvent::McpList {
        correlation_id: "c1".into(),
        servers: vec![McpServerStatus {
            name: "everything".into(),
            transport: "stdio".into(),
            connected: true,
            tools: vec!["mcp__everything__echo".into()],
            error: None,
            state: None,
            auth: None,
        }],
    };
    assert_eq!(list_ev.seq(), None);
    assert_eq!(list_ev.session(), None);
    let json = serde_json::to_string(&list_ev).unwrap();
    assert_eq!(serde_json::from_str::<OutEvent>(&json).unwrap(), list_ev);

    let changed_ev = OutEvent::McpChanged {
        name: "everything".into(),
        action: McpAction::Added,
        error: None,
    };
    assert_eq!(changed_ev.seq(), None);
    assert_eq!(changed_ev.session(), None);
    let json = serde_json::to_string(&changed_ev).unwrap();
    assert_eq!(serde_json::from_str::<OutEvent>(&json).unwrap(), changed_ev);
    assert!(json.contains(r#""action":"added""#), "{json}");
    assert!(!json.contains("error"), "{json}");
    // A failed op carries its reason; an older frame without `error` parses.
    let failed = OutEvent::McpChanged {
        name: "everything".into(),
        action: McpAction::Added,
        error: Some("refused".into()),
    };
    let json = serde_json::to_string(&failed).unwrap();
    assert_eq!(serde_json::from_str::<OutEvent>(&json).unwrap(), failed);
}

#[test]
fn compacted_event_roundtrips_and_carries_seq() {
    let ev = OutEvent::Compacted {
        session: SessionId::new("s1"),
        seq: 6,
        summary: "user asked for X, agent did Y".into(),
        kept: 0,
        auto: false,
        mode: CompactionMode::Summary,
    };
    let json = serde_json::to_string(&ev).unwrap();
    let back: OutEvent = serde_json::from_str(&json).unwrap();
    assert_eq!(ev, back);
    assert_eq!(back.seq(), Some(6));
    assert_eq!(back.session(), Some(&SessionId::new("s1")));

    // `kept`/`auto` default to `0`/`false` when omitted on the wire (older
    // writer shape — every pre-#398 record is the manual copy-on-write path).
    let bare = r#"{"kind":"compacted","session":"s1","seq":6,"summary":"x"}"#;
    assert_eq!(
        serde_json::from_str::<OutEvent>(bare).unwrap(),
        OutEvent::Compacted {
            session: SessionId::new("s1"),
            seq: 6,
            summary: "x".into(),
            kept: 0,
            auto: false,
            mode: CompactionMode::Summary,
        }
    );
}

#[test]
fn compacted_event_auto_flag_roundtrips() {
    let ev = OutEvent::Compacted {
        session: SessionId::new("s1"),
        seq: 6,
        summary: "auto-summarized on overflow".into(),
        kept: 2,
        auto: true,
        mode: CompactionMode::Summary,
    };
    let json = serde_json::to_string(&ev).unwrap();
    let back: OutEvent = serde_json::from_str(&json).unwrap();
    assert_eq!(ev, back);
    match back {
        OutEvent::Compacted { auto, .. } => assert!(auto),
        other => panic!("expected Compacted, got {other:?}"),
    }
}

#[test]
fn history_event_roundtrips_and_carries_no_seq() {
    let ev = OutEvent::History {
        correlation_id: "c1".into(),
        session: SessionId::new("s1"),
        events: vec![
            OutEvent::TextDelta {
                session: SessionId::new("s1"),
                seq: 3,
                text: "hi".into(),
            },
            OutEvent::Done {
                session: SessionId::new("s1"),
                seq: 4,
            },
        ],
    };
    assert_eq!(ev.seq(), None, "History is a query reply, no seq");
    assert_eq!(ev.session(), Some(&SessionId::new("s1")));
    let json = serde_json::to_string(&ev).unwrap();
    let back: OutEvent = serde_json::from_str(&json).unwrap();
    assert_eq!(ev, back);
}

#[test]
fn agent_changed_roundtrips() {
    let ev = OutEvent::AgentChanged {
        session: SessionId::new("s"),
        agent: "plan".into(),
    };
    let json = serde_json::to_string(&ev).unwrap();
    assert_eq!(serde_json::from_str::<OutEvent>(&json).unwrap(), ev);
}

#[test]
fn session_started_round_trips_and_stays_backward_compatible() {
    let ev = OutEvent::SessionStarted {
        session: SessionId::new("child"),
        parent: Some(SessionId::new("plan")),
        predecessor: None,
        agent: "build".into(),
        model: None,
        root: false,
        ts: 0,
        user: None,
    };
    let json = serde_json::to_string(&ev).unwrap();
    assert_eq!(serde_json::from_str::<OutEvent>(&json).unwrap(), ev);

    // An older head's persisted frame (no `predecessor`/`user` keys) still
    // deserializes — both default, so a pre-#626/#522 log still replays.
    let legacy = r#"{"kind":"session_started","session":"s","parent":null,"agent":"build","model":null,"root":true,"ts":0}"#;
    assert_eq!(
        serde_json::from_str::<OutEvent>(legacy).unwrap(),
        OutEvent::SessionStarted {
            session: SessionId::new("s"),
            parent: None,
            predecessor: None,
            agent: "build".into(),
            model: None,
            root: true,
            ts: 0,
            user: None,
        }
    );
}

#[test]
fn tool_events_carry_an_optional_envelope() {
    let envelope = Some(ToolEnvelope {
        tool: "invoke".into(),
        input: r#"{"name":"read","args":{"path":"x"}}"#.into(),
    });
    let exec = OutEvent::ToolExec {
        session: SessionId::new("s1"),
        seq: 2,
        request_id: "r1".into(),
        tool: "read".into(),
        input: r#"{"path":"x"}"#.into(),
        agent: "build".into(),
        envelope: envelope.clone(),
        workdir: None,
    };
    let json = serde_json::to_string(&exec).unwrap();
    assert!(
        json.contains(r#""envelope":{"tool":"invoke","input":"#),
        "{json}"
    );
    assert_eq!(serde_json::from_str::<OutEvent>(&json).unwrap(), exec);

    let output = OutEvent::ToolOutput {
        session: SessionId::new("s1"),
        seq: 3,
        request_id: "r1".into(),
        tool: "read".into(),
        output: "contents".into(),
        content: Vec::new(),
        is_error: false,
        duration_ms: None,
        exit_code: None,
        envelope,
    };
    let json = serde_json::to_string(&output).unwrap();
    assert_eq!(serde_json::from_str::<OutEvent>(&json).unwrap(), output);
}

#[test]
fn tool_events_without_envelope_serialize_unchanged() {
    // A pre-ADR-0204 log line: no `envelope` key, and re-serializing an
    // ordinary call must produce the identical bytes.
    let legacy = r#"{"kind":"tool_call","session":"s1","seq":1,"request_id":"r1","tool":"read","input":"{}"}"#;
    let back: OutEvent = serde_json::from_str(legacy).unwrap();
    assert!(matches!(
        &back,
        OutEvent::ToolCall {
            envelope: None,
            provider_meta: None,
            ..
        }
    ));
    assert_eq!(serde_json::to_string(&back).unwrap(), legacy);

    let legacy = r#"{"kind":"tool_output","session":"s1","seq":2,"request_id":"r1","tool":"read","output":"ok"}"#;
    let back: OutEvent = serde_json::from_str(legacy).unwrap();
    assert!(matches!(&back, OutEvent::ToolOutput { envelope: None, .. }));
    assert_eq!(serde_json::to_string(&back).unwrap(), legacy);

    let legacy = r#"{"kind":"tool_exec","session":"s1","seq":1,"request_id":"r1","tool":"read","input":"{}","agent":"build"}"#;
    let back: OutEvent = serde_json::from_str(legacy).unwrap();
    assert!(matches!(&back, OutEvent::ToolExec { envelope: None, .. }));
    assert_eq!(serde_json::to_string(&back).unwrap(), legacy);
}

#[test]
fn tool_call_carries_optional_provider_meta() {
    let call = OutEvent::ToolCall {
        session: SessionId::new("s1"),
        seq: 1,
        request_id: "r1".into(),
        tool: "read".into(),
        input: "{}".into(),
        provider_meta: Some(serde_json::json!({ "thought_signature": "SIG" })),
        envelope: None,
    };
    let json = serde_json::to_string(&call).unwrap();
    assert!(
        json.contains(r#""provider_meta":{"thought_signature":"SIG"}"#),
        "{json}"
    );
    assert_eq!(serde_json::from_str::<OutEvent>(&json).unwrap(), call);
}

/// #731, ADR-0212: a seq-bearing, session-scoped content event; empty lists
/// stay off the wire.
#[test]
fn files_reverted_roundtrips_with_seq_and_compact_empty_lists() {
    let ev = OutEvent::FilesReverted {
        session: SessionId::new("s1"),
        seq: 9,
        to_turn: 1,
        restored: vec!["a.rs".into()],
        skipped: vec![RevertSkip {
            path: "b.rs".into(),
            reason: "modified since the agent's edit".into(),
        }],
    };
    let json = serde_json::to_string(&ev).unwrap();
    assert_eq!(serde_json::from_str::<OutEvent>(&json).unwrap(), ev);
    assert_eq!(ev.seq(), Some(9));
    assert_eq!(ev.session(), Some(&SessionId::new("s1")));

    let empty = OutEvent::FilesReverted {
        session: SessionId::new("s1"),
        seq: 1,
        to_turn: 0,
        restored: vec![],
        skipped: vec![],
    };
    assert_eq!(
        serde_json::to_string(&empty).unwrap(),
        r#"{"kind":"files_reverted","session":"s1","seq":1,"to_turn":0}"#
    );
}

/// #764 (ADR-0224): `WorkdirChanged` is a point-in-time state event, and a
/// `ToolExec` from a session that never set a workdir is byte-identical to
/// the pre-#764 shape.
#[test]
fn workdir_events_roundtrip_and_stay_additive() {
    let ev = OutEvent::WorkdirChanged {
        session: SessionId::new("s1"),
        dir: "/p/sub".into(),
    };
    let json = serde_json::to_string(&ev).unwrap();
    assert_eq!(
        json,
        r#"{"kind":"workdir_changed","session":"s1","dir":"/p/sub"}"#
    );
    assert_eq!(serde_json::from_str::<OutEvent>(&json).unwrap(), ev);
    assert_eq!(
        (ev.seq(), ev.session()),
        (None, Some(&SessionId::new("s1")))
    );

    let exec = |workdir: Option<&str>| OutEvent::ToolExec {
        session: SessionId::new("s1"),
        seq: 1,
        request_id: "r".into(),
        tool: "bash".into(),
        input: "{}".into(),
        agent: String::new(),
        envelope: None,
        workdir: workdir.map(String::from),
    };
    assert!(!serde_json::to_string(&exec(None))
        .unwrap()
        .contains("workdir"));
    let json = serde_json::to_string(&exec(Some("/p/sub"))).unwrap();
    assert!(json.contains(r#""workdir":"/p/sub""#), "{json}");
    assert_eq!(
        serde_json::from_str::<OutEvent>(&json).unwrap(),
        exec(Some("/p/sub"))
    );
}
