use super::*;
use entanglement_core::SessionId;

fn render(ev: &OutEvent) -> String {
    let mut buf = Vec::new();
    render_text(&mut buf, ev).expect("render into a Vec never fails");
    String::from_utf8(buf).expect("rendered text is UTF-8")
}

#[test]
fn tool_call_prints_the_real_name_and_a_readable_summary() {
    let text = render(&OutEvent::ToolCall {
        session: SessionId::new("s"),
        seq: 1,
        request_id: "c1".to_string(),
        tool: "mcp__chess__move".to_string(),
        input: r#"{"move":"e2e4"}"#.to_string(),
        envelope: None,
        provider_meta: None,
    });
    assert_eq!(text, "→ chess › move  e2e4\n");
}

#[test]
fn json_tool_output_prints_key_value_lines() {
    let text = render(&OutEvent::ToolOutput {
        session: SessionId::new("s"),
        seq: 2,
        request_id: "c1".to_string(),
        tool: "mcp__chess__move".to_string(),
        output: r#"{"moves":["e4"],"ok":true}"#.to_string(),
        content: vec![],
        is_error: false,
        duration_ms: None,
        exit_code: None,
        envelope: None,
    });
    assert_eq!(text, "= moves:\n    - e4\n  ok: true\n");
}

#[test]
fn persistence_transitions_get_one_stderr_line_each() {
    let degraded = persistence_notice(&OutEvent::PersistenceDegraded {
        root: SessionId::new("r1"),
        error: "disk full".into(),
    })
    .expect("degraded notice");
    assert!(degraded.starts_with("⚠ session log r1"), "{degraded}");
    assert!(degraded.contains("disk full"), "{degraded}");
    assert!(!degraded.contains('\n'), "one line: {degraded}");
    let recovered = persistence_notice(&OutEvent::PersistenceRecovered {
        root: SessionId::new("r1"),
        lost: 3,
    })
    .expect("recovered notice");
    assert!(
        recovered.starts_with("✓ session log r1 recovered"),
        "{recovered}"
    );
    assert!(recovered.contains("3 record(s) lost"), "{recovered}");
    // The stdout renderer leaves them to the stderr line.
    let ev = OutEvent::PersistenceRecovered {
        root: SessionId::new("r1"),
        lost: 3,
    };
    assert_eq!(render(&ev), "");
}

#[test]
fn other_events_get_no_persistence_notice() {
    let ev = OutEvent::Status {
        session: SessionId::new("s"),
        state: AgentState::Idle,
    };
    assert_eq!(persistence_notice(&ev), None);
}

#[test]
fn nested_calls_print_as_indented_compact_lines() {
    let call = render(&OutEvent::NestedCall {
        session: SessionId::new("s"),
        parent_request_id: "t1".into(),
        index: 2,
        tool: "read".into(),
        args: "a.txt".into(),
        duration_ms: 3,
        is_error: true,
    });
    assert_eq!(call, "  ✗ #2 read  a.txt  3ms\n");
    let summary = render(&OutEvent::NestedCallSummary {
        session: SessionId::new("s"),
        parent_request_id: "t1".into(),
        suppressed: 50,
        errors: 0,
    });
    assert_eq!(summary, "  ↳ +50 more calls, 0 errors\n");
}
