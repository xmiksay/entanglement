use std::collections::HashMap;

use crate::protocol::*;
use entanglement_provider::ContentPart;

#[test]
fn wire_allowlist_refuses_the_privileged_frames() {
    let s = SessionId::new("s1");
    // The runtime/embedder authors these in process; a wire head must never
    // forward them (#155, #318).
    assert!(!InMsg::tool_result(s.clone(), "r1", "x").wire_allowed());
    assert!(!InMsg::Spawn {
        session: s.clone(),
        parent: Some(s.clone()),
        predecessor: None,
        agent: "build".into(),
        prompt: "go".into(),
        user: None,
        model: None,
        mode: None,
    }
    .wire_allowed());
    assert!(!InMsg::Resume {
        session: s.clone(),
        records: Vec::new(),
    }
    .wire_allowed());
    // Memory eviction is an embedder control, trusted-only (#318): a wire head
    // must not be able to evict another session's in-memory state.
    assert!(!InMsg::HibernateSession { session: s.clone() }.wire_allowed());
    // MCP mutation is trusted-only (#472, ADR-0124): an unapproved `McpAdd`
    // spawns an arbitrary local subprocess, so neither mutating op may
    // arrive over an untrusted wire. The read-only `McpList` stays allowed.
    assert!(!InMsg::McpAdd {
        name: "srv".into(),
        config: McpServerSpec {
            command: Some("evil".into()),
            args: vec![],
            env: std::collections::HashMap::new(),
            url: None,
            headers: std::collections::HashMap::new(),
            disabled: false,
        },
    }
    .wire_allowed());
    assert!(!InMsg::McpRemove { name: "srv".into() }.wire_allowed());
    // The live tool overlay is trusted-only for an *enable* entry (#539,
    // ADR-0149, the McpAdd rationale): it injects tools past the agent
    // mask, optionally graded `allow` with no approval prompt — never
    // wire-grantable. A mixed list (any enable entry present) refuses the
    // whole frame.
    assert!(!InMsg::SetToolOverlay {
        session: s.clone(),
        entries: vec![ToolOverlayEntry::ask("mcp__*")],
    }
    .wire_allowed());
    assert!(!InMsg::SetToolOverlay {
        session: s.clone(),
        entries: vec![
            ToolOverlayEntry::deny("bash"),
            ToolOverlayEntry::ask("mcp__*"),
        ],
    }
    .wire_allowed());
    // A deny-only overlay only withdraws tools the profile already
    // advertises — it can never widen the model's tool surface — so it is
    // wire-allowed (#634, ADR-0177 amending ADR-0149). An empty list
    // (clearing back to the profile default) is vacuously deny-only.
    assert!(InMsg::SetToolOverlay {
        session: s.clone(),
        entries: vec![
            ToolOverlayEntry::deny("bash"),
            ToolOverlayEntry::deny("mcp__*")
        ],
    }
    .wire_allowed());
    assert!(InMsg::SetToolOverlay {
        session: s.clone(),
        entries: vec![],
    }
    .wire_allowed());
    // Mode carries real authority (ADR-0207), unlike the agent (identity
    // fixed at session start, no live switch message at all) — a wire head
    // must not be able to widen its own permission posture by naming a mode
    // directly.
    assert!(!InMsg::SetMode {
        session: s.clone(),
        mode: "build".into(),
    }
    .wire_allowed());
    // Every head-authored frame stays acceptable off the wire.
    for msg in [
        InMsg::prompt(s.clone(), "hi"),
        InMsg::Approve {
            session: s.clone(),
            request_id: "r".into(),
            scope: ApprovalScope::Once,
            mode: None,
        },
        InMsg::Reject {
            session: s.clone(),
            request_id: "r".into(),
            reason: None,
        },
        InMsg::answer_question(s.clone(), "r", vec![vec!["a".into()]]),
        InMsg::Stop { session: s.clone() },
        InMsg::PauseSession { session: s.clone() },
        InMsg::ResumeSession { session: s.clone() },
        InMsg::ListSessions {
            correlation_id: "c1".into(),
        },
        InMsg::ReplayFrom {
            session: s.clone(),
            correlation_id: "c1".into(),
            after_seq: 0,
        },
        InMsg::CloseSession { session: s.clone() },
        InMsg::SetModel {
            session: s.clone(),
            provider: "zai".into(),
            model: "glm-5.2".into(),
        },
        InMsg::SetGeneration {
            session: s.clone(),
            overrides: entanglement_provider::GenerationParams::default(),
        },
        InMsg::SetSessionMeta {
            session: s.clone(),
            name: Some("my session".into()),
            action: None,
            if_unset: false,
        },
        InMsg::Oneshot {
            session: s.clone(),
            op: "compact".into(),
            args: serde_json::Value::Null,
        },
        InMsg::McpList {
            correlation_id: "c1".into(),
        },
    ] {
        assert!(
            msg.wire_allowed(),
            "`{}` should be wire-allowed",
            msg.variant_name()
        );
    }
}

#[test]
fn inbound_roundtrips_as_tagged_json() {
    let msg = InMsg::prompt(SessionId::new("s1"), "hi");
    let json = serde_json::to_string(&msg).unwrap();
    assert_eq!(
        json,
        r#"{"kind":"prompt","session":"s1","content":[{"type":"text","text":"hi"}]}"#
    );
    let back: InMsg = serde_json::from_str(&json).unwrap();
    assert_eq!(msg, back);
}

#[test]
fn prompt_accepts_legacy_text_shape() {
    // Logs persisted before #197 carry a bare `text` string; the shim must
    // still deserialize them into the content-block shape.
    let legacy = r#"{"kind":"prompt","session":"s1","text":"hi"}"#;
    let back: InMsg = serde_json::from_str(legacy).unwrap();
    assert_eq!(back, InMsg::prompt(SessionId::new("s1"), "hi"));
}

#[test]
fn prompt_accepts_image_content_block() {
    let json = r#"{"kind":"prompt","session":"s1","content":[
        {"type":"text","text":"look"},
        {"type":"image","source":{"type":"base64","media_type":"image/png","data":"AAAA"}}
    ]}"#;
    let back: InMsg = serde_json::from_str(json).unwrap();
    match back {
        InMsg::Prompt { content, .. } => {
            assert_eq!(content.len(), 2);
            assert_eq!(content[0], ContentPart::text("look"));
            assert_eq!(content[1], ContentPart::image("image/png", "AAAA"));
        }
        other => panic!("expected Prompt, got {other:?}"),
    }
}

#[test]
fn tool_result_accepts_legacy_output_shape() {
    // Tool results persisted before #221 carry a bare `output` string; the
    // shim aliases it into the multimodal `content` shape.
    let legacy = r#"{"kind":"tool_result","session":"s1","request_id":"r1","output":"done"}"#;
    let back: InMsg = serde_json::from_str(legacy).unwrap();
    assert_eq!(back, InMsg::tool_result(SessionId::new("s1"), "r1", "done"));
}

#[test]
fn tool_result_roundtrips_image_content_block() {
    let msg = InMsg::ToolResult {
        session: SessionId::new("s1"),
        request_id: "r1".into(),
        content: vec![ContentPart::image("image/png", "AAAA")],
        is_error: false,
        duration_ms: None,
        exit_code: None,
    };
    let json = serde_json::to_string(&msg).unwrap();
    let back: InMsg = serde_json::from_str(&json).unwrap();
    assert_eq!(msg, back);
}

#[test]
fn tool_result_is_error_and_duration_round_trip() {
    let msg = InMsg::ToolResult {
        session: SessionId::new("s1"),
        request_id: "r1".into(),
        content: vec![ContentPart::text(
            "tool `bash` denied by permission profile",
        )],
        is_error: true,
        duration_ms: Some(42),
        exit_code: Some(3),
    };
    let json = serde_json::to_string(&msg).unwrap();
    assert!(json.contains("\"is_error\":true"), "{json}");
    assert!(json.contains("\"duration_ms\":42"), "{json}");
    assert!(json.contains("\"exit_code\":3"), "{json}");
    let back: InMsg = serde_json::from_str(&json).unwrap();
    assert_eq!(msg, back);
}

#[test]
fn tool_result_omits_default_error_fields() {
    let json =
        serde_json::to_string(&InMsg::tool_result(SessionId::new("s1"), "r1", "ok")).unwrap();
    assert!(!json.contains("is_error"), "{json}");
    assert!(!json.contains("duration_ms"), "{json}");
    assert!(!json.contains("exit_code"), "{json}");
}

#[test]
fn reject_reason_omits_when_none() {
    let msg = InMsg::Reject {
        session: SessionId::new("s1"),
        request_id: "r1".into(),
        reason: None,
    };
    let json = serde_json::to_string(&msg).unwrap();
    assert!(!json.contains("reason"));
}

#[test]
fn approve_scope_defaults_to_once_and_omits_when_default() {
    // A bare approve serializes without a `scope` (default Once), and an older
    // head's frame (no `scope`) still deserializes to Once — additive on the wire.
    let msg = InMsg::Approve {
        session: SessionId::new("s1"),
        request_id: "r1".into(),
        scope: ApprovalScope::Once,
        mode: None,
    };
    let json = serde_json::to_string(&msg).unwrap();
    assert!(!json.contains("scope"), "default scope must be omitted");
    assert!(!json.contains("mode"), "default mode must be omitted");
    let legacy = r#"{"kind":"approve","session":"s1","request_id":"r1"}"#;
    assert_eq!(serde_json::from_str::<InMsg>(legacy).unwrap(), msg);
}

#[test]
fn approve_scope_roundtrips_when_set() {
    for scope in [
        ApprovalScope::Session,
        ApprovalScope::Always,
        ApprovalScope::SessionDir,
    ] {
        let msg = InMsg::Approve {
            session: SessionId::new("s1"),
            request_id: "r1".into(),
            scope,
            mode: None,
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains("scope"));
        assert_eq!(serde_json::from_str::<InMsg>(&json).unwrap(), msg);
    }
}

/// #560 (ADR-0207 §7 extension): `mode` is the approver's plan-acceptance
/// choice, additive and omitted by default exactly like `scope` — an older
/// head's frame (no `mode`) still deserializes to `None`.
#[test]
fn approve_mode_roundtrips_when_set() {
    let msg = InMsg::Approve {
        session: SessionId::new("s1"),
        request_id: "r1".into(),
        scope: ApprovalScope::Once,
        mode: Some("build".to_string()),
    };
    let json = serde_json::to_string(&msg).unwrap();
    assert!(json.contains("\"mode\":\"build\""), "{json}");
    assert_eq!(serde_json::from_str::<InMsg>(&json).unwrap(), msg);

    let legacy = r#"{"kind":"approve","session":"s1","request_id":"r1"}"#;
    assert_eq!(
        serde_json::from_str::<InMsg>(legacy).unwrap(),
        InMsg::Approve {
            session: SessionId::new("s1"),
            request_id: "r1".into(),
            scope: ApprovalScope::Once,
            mode: None,
        }
    );
}

#[test]
fn answer_question_roundtrips_as_tagged_json() {
    let msg = InMsg::answer_question(SessionId::new("s1"), "q1", vec![vec!["REST".into()]]);
    let json = serde_json::to_string(&msg).unwrap();
    assert_eq!(
        json,
        r#"{"kind":"answer_question","session":"s1","request_id":"q1","answers":[["REST"]]}"#
    );
    let back: InMsg = serde_json::from_str(&json).unwrap();
    assert_eq!(msg, back);
}

#[test]
fn answer_question_deserializes_legacy_shape() {
    let json = r#"{"kind":"answer_question","session":"s1","request_id":"q1","answer":"REST"}"#;
    let msg: InMsg = serde_json::from_str(json).unwrap();
    match msg {
        InMsg::AnswerQuestion {
            answers, answer, ..
        } => {
            assert!(answers.is_empty());
            assert_eq!(answer, "REST");
        }
        _ => panic!("expected AnswerQuestion"),
    }
}

#[test]
fn list_and_close_session_roundtrip_as_tagged_json() {
    let list = InMsg::ListSessions {
        correlation_id: "q1".into(),
    };
    assert_eq!(
        serde_json::to_string(&list).unwrap(),
        r#"{"kind":"list_sessions","correlation_id":"q1"}"#
    );
    assert_eq!(
        serde_json::from_str::<InMsg>(&serde_json::to_string(&list).unwrap()).unwrap(),
        list
    );
    // The supervisor-global list query names no session.
    assert_eq!(list.session(), None);

    let close = InMsg::CloseSession {
        session: SessionId::new("s1"),
    };
    assert_eq!(
        serde_json::to_string(&close).unwrap(),
        r#"{"kind":"close_session","session":"s1"}"#
    );
}

#[test]
fn mcp_ops_are_session_less_and_only_the_query_is_wire_allowed() {
    let list = InMsg::McpList {
        correlation_id: "c1".into(),
    };
    let add = InMsg::McpAdd {
        name: "everything".into(),
        config: McpServerSpec {
            command: Some("npx".into()),
            args: vec![
                "-y".into(),
                "@modelcontextprotocol/server-everything".into(),
            ],
            env: HashMap::new(),
            url: None,
            headers: HashMap::new(),
            disabled: false,
        },
    };
    let remove = InMsg::McpRemove {
        name: "everything".into(),
    };
    for msg in [&list, &add, &remove] {
        assert_eq!(msg.session(), None, "{msg:?} is engine-global");
        let json = serde_json::to_string(msg).unwrap();
        let back: InMsg = serde_json::from_str(&json).unwrap();
        assert_eq!(msg, &back);
    }
    // The read-only query stays wire-allowed; the mutating pair is
    // trusted-only (#472, ADR-0124) — an unapproved `McpAdd` would spawn an
    // arbitrary local subprocess straight off the wire.
    assert!(list.wire_allowed());
    assert!(!add.wire_allowed(), "McpAdd must be wire-refused");
    assert!(!remove.wire_allowed(), "McpRemove must be wire-refused");
    assert_eq!(list.variant_name(), "mcp_list");
    assert_eq!(add.variant_name(), "mcp_add");
    assert_eq!(remove.variant_name(), "mcp_remove");
}

#[test]
fn list_questions_is_session_less_with_an_optional_filter_and_wire_allowed() {
    let global = InMsg::ListQuestions {
        correlation_id: "c1".into(),
        session: None,
    };
    let filtered = InMsg::ListQuestions {
        correlation_id: "c2".into(),
        session: Some(SessionId::new("s1")),
    };
    for msg in [&global, &filtered] {
        // The filter is a data field, not a routing target — both forms
        // are supervisor-global, mirroring `ListSessions`/`McpList`.
        assert_eq!(msg.session(), None, "{msg:?} is engine-global");
        assert!(msg.wire_allowed());
        assert_eq!(msg.variant_name(), "list_questions");
        let json = serde_json::to_string(msg).unwrap();
        assert_eq!(&serde_json::from_str::<InMsg>(&json).unwrap(), msg);
    }
    // The `None` filter omits the field entirely (additive wire shape).
    assert_eq!(
        serde_json::to_string(&global).unwrap(),
        r#"{"kind":"list_questions","correlation_id":"c1"}"#
    );
}

#[test]
fn list_operations_is_session_less_with_an_optional_filter_and_wire_allowed() {
    let global = InMsg::ListOperations {
        correlation_id: "c1".into(),
        session: None,
    };
    let filtered = InMsg::ListOperations {
        correlation_id: "c2".into(),
        session: Some(SessionId::new("s1")),
    };
    for msg in [&global, &filtered] {
        // The filter is a data field, not a routing target — both forms
        // are supervisor-global, mirroring `ListSessions`/`ListQuestions`.
        assert_eq!(msg.session(), None, "{msg:?} is engine-global");
        assert!(msg.wire_allowed());
        assert_eq!(msg.variant_name(), "list_operations");
        let json = serde_json::to_string(msg).unwrap();
        assert_eq!(&serde_json::from_str::<InMsg>(&json).unwrap(), msg);
    }
    // The `None` filter omits the field entirely (additive wire shape).
    assert_eq!(
        serde_json::to_string(&global).unwrap(),
        r#"{"kind":"list_operations","correlation_id":"c1"}"#
    );
}

#[test]
fn list_pending_is_session_less_with_an_optional_root_and_wire_allowed() {
    let global = InMsg::ListPending {
        correlation_id: "c1".into(),
        root: None,
    };
    let rooted = InMsg::ListPending {
        correlation_id: "c2".into(),
        root: Some(SessionId::new("s1")),
    };
    for msg in [&global, &rooted] {
        // `root` filters the reply; it is not a routing target (#726).
        assert_eq!(msg.session(), None, "{msg:?} is engine-global");
        assert!(msg.wire_allowed());
        assert_eq!(msg.variant_name(), "list_pending");
        let json = serde_json::to_string(msg).unwrap();
        assert_eq!(&serde_json::from_str::<InMsg>(&json).unwrap(), msg);
    }
    assert_eq!(
        serde_json::to_string(&global).unwrap(),
        r#"{"kind":"list_pending","correlation_id":"c1"}"#
    );
}

#[test]
fn retract_and_replace_question_roundtrip_and_are_wire_allowed() {
    let retract = InMsg::RetractQuestion {
        session: SessionId::new("s1"),
        request_id: "q1".into(),
    };
    assert_eq!(retract.session(), Some(&SessionId::new("s1")));
    assert!(retract.wire_allowed());
    assert_eq!(retract.variant_name(), "retract_question");
    let json = serde_json::to_string(&retract).unwrap();
    assert_eq!(serde_json::from_str::<InMsg>(&json).unwrap(), retract);

    let replace = InMsg::ReplaceQuestion {
        session: SessionId::new("s1"),
        request_id: "q1".into(),
        questions: Questions(vec![Question {
            question: "Which DB, revised?".into(),
            options: vec![QuestionOption {
                label: "Postgres".into(),
                description: None,
            }],
            multi_select: false,
        }]),
    };
    assert_eq!(replace.session(), Some(&SessionId::new("s1")));
    assert!(replace.wire_allowed());
    assert_eq!(replace.variant_name(), "replace_question");
    let json = serde_json::to_string(&replace).unwrap();
    assert_eq!(serde_json::from_str::<InMsg>(&json).unwrap(), replace);
    assert!(json.contains("Which DB, revised?"), "{json}");
}

#[test]
fn replay_from_roundtrips_and_is_wire_allowed() {
    let msg = InMsg::ReplayFrom {
        session: SessionId::new("s1"),
        correlation_id: "c1".into(),
        after_seq: 12,
    };
    let json = serde_json::to_string(&msg).unwrap();
    assert_eq!(
        json,
        r#"{"kind":"replay_from","session":"s1","correlation_id":"c1","after_seq":12}"#
    );
    assert_eq!(serde_json::from_str::<InMsg>(&json).unwrap(), msg);
    // A head-authored query the wire may forward, unlike the runtime-authored trio.
    assert!(msg.wire_allowed());
    assert_eq!(msg.session(), Some(&SessionId::new("s1")));
}

#[test]
fn oneshot_roundtrips_defaults_args_and_is_wire_allowed() {
    let msg = InMsg::Oneshot {
        session: SessionId::new("s1"),
        op: "compact".into(),
        args: serde_json::json!({"instructions": "keep the file list"}),
    };
    let json = serde_json::to_string(&msg).unwrap();
    assert_eq!(serde_json::from_str::<InMsg>(&json).unwrap(), msg);
    assert!(msg.wire_allowed());
    assert_eq!(msg.session(), Some(&SessionId::new("s1")));
    assert_eq!(msg.variant_name(), "oneshot");

    // `args` defaults to `Value::Null` when omitted on the wire.
    let bare = r#"{"kind":"oneshot","session":"s1","op":"compact"}"#;
    assert_eq!(
        serde_json::from_str::<InMsg>(bare).unwrap(),
        InMsg::Oneshot {
            session: SessionId::new("s1"),
            op: "compact".into(),
            args: serde_json::Value::Null,
        }
    );
}
