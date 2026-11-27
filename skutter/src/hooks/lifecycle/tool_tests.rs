//! The tracker's `post_tool_use` and `user_prompt_submit` firings (#964):
//! derived from `ToolExec`/`ToolOutput` and the inbound `Prompt`.

use entanglement_core::ContentPart;

use super::*;

fn sid(s: &str) -> SessionId {
    SessionId::new(s)
}

fn exec(s: &str, rid: &str, tool: &str, input: &str) -> OutEvent {
    OutEvent::ToolExec {
        session: sid(s),
        seq: 1,
        request_id: rid.into(),
        tool: tool.into(),
        input: input.into(),
        agent: String::new(),
        envelope: None,
        workdir: None,
    }
}

fn output(s: &str, rid: &str, text: &str, is_error: bool, exit_code: Option<i32>) -> OutEvent {
    OutEvent::ToolOutput {
        session: sid(s),
        seq: 2,
        request_id: rid.into(),
        tool: "ignored".into(),
        output: text.into(),
        content: Vec::new(),
        is_error,
        duration_ms: None,
        exit_code,
        envelope: None,
    }
}

#[test]
fn a_run_call_fires_post_tool_use_with_its_input_and_result() {
    let mut t = Tracker::default();
    assert!(t
        .observe(&exec("s", "r1", "bash", r#"{"command":"ls"}"#))
        .is_none());
    let f = t
        .observe(&output("s", "r1", "[exit 3]\nboom", false, Some(3)))
        .expect("post_tool_use fires");
    assert_eq!(f.event, HookEvent::PostToolUse);
    assert_eq!(f.tool.as_deref(), Some("bash"));
    assert_eq!(f.payload["event"], "post_tool_use");
    assert_eq!(f.payload["session"], "s");
    assert_eq!(f.payload["tool"], "bash");
    assert_eq!(f.payload["input"]["command"], "ls");
    assert_eq!(f.payload["output"], "[exit 3]\nboom");
    assert_eq!(f.payload["is_error"], false);
    assert_eq!(f.payload["exit_code"], 3);
}

#[test]
fn a_failed_call_carries_is_error_and_a_null_exit_code() {
    let mut t = Tracker::default();
    t.observe(&exec("s", "r1", "write", "{}"));
    let f = t.observe(&output("s", "r1", "denied", true, None)).unwrap();
    assert_eq!(f.payload["is_error"], true);
    assert_eq!(f.payload["exit_code"], Value::Null);
}

/// A result core produced without asking the runtime (no `ToolExec`) fires
/// nothing, and each `ToolExec` fires once.
#[test]
fn only_a_call_the_runtime_ran_fires_and_only_once() {
    let mut t = Tracker::default();
    assert!(t.observe(&output("s", "r1", "x", true, None)).is_none());
    t.observe(&exec("s", "r2", "read", "{}"));
    assert!(t
        .observe(&output("other", "r2", "x", false, None))
        .is_none());
    assert!(t.observe(&output("s", "r2", "x", false, None)).is_some());
    assert!(t.observe(&output("s", "r2", "x", false, None)).is_none());
}

#[test]
fn a_session_end_forgets_its_open_calls() {
    let mut t = Tracker::default();
    t.observe(&exec("s", "r1", "read", "{}"));
    t.observe(&OutEvent::SessionEnded {
        session: sid("s"),
        ts: 1,
    });
    assert!(t.execs.is_empty());
}

#[test]
fn a_prompt_fires_user_prompt_submit_with_its_text() {
    let mut t = Tracker::default();
    let f = t
        .observe_in(&InMsg::Prompt {
            session: sid("s"),
            content: vec![ContentPart::text("hello there")],
        })
        .expect("user_prompt_submit fires");
    assert_eq!(f.event, HookEvent::UserPromptSubmit);
    assert_eq!(f.tool, None);
    assert_eq!(f.payload["event"], "user_prompt_submit");
    assert_eq!(f.payload["session"], "s");
    assert_eq!(f.payload["prompt"], "hello there");
}
