//! The tracker's `post_tool_use` and `user_prompt_submit` firings (#964):
//! derived from `ToolExec`/`ToolOutput` and the inbound `Prompt`. Its
//! `post_tool_use` is for tools that are not `skutter`'s own (#968): an MCP
//! server's here.

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

/// The result of a call that ran: the runtime measured its duration.
fn output(s: &str, rid: &str, text: &str, is_error: bool, exit_code: Option<i32>) -> OutEvent {
    OutEvent::ToolOutput {
        session: sid(s),
        seq: 2,
        request_id: rid.into(),
        tool: "ignored".into(),
        output: text.into(),
        content: Vec::new(),
        is_error,
        duration_ms: Some(1),
        exit_code,
        envelope: None,
    }
}

/// The result of a call the runtime refused (or declined at the door):
/// no duration, since nothing ran.
fn refused(s: &str, rid: &str) -> OutEvent {
    let mut ev = output(s, rid, "tool `write` denied by mode `plan`", true, None);
    if let OutEvent::ToolOutput { duration_ms, .. } = &mut ev {
        *duration_ms = None;
    }
    ev
}

#[test]
fn a_run_call_fires_post_tool_use_with_its_input_and_result() {
    let mut t = Tracker::default();
    assert!(t
        .observe(&exec("s", "r1", "mcp__gh__run", r#"{"command":"ls"}"#))
        .is_none());
    let f = t
        .observe(&output("s", "r1", "[exit 3]\nboom", false, Some(3)))
        .expect("post_tool_use fires");
    assert_eq!(f.event, HookEvent::PostToolUse);
    assert_eq!(f.tool.as_deref(), Some("mcp__gh__run"));
    assert_eq!(f.payload["event"], "post_tool_use");
    assert_eq!(f.payload["session"], "s");
    assert_eq!(f.payload["tool"], "mcp__gh__run");
    assert_eq!(f.payload["input"]["command"], "ls");
    assert_eq!(f.payload["output"], "[exit 3]\nboom");
    assert_eq!(f.payload["is_error"], false);
    assert_eq!(f.payload["exit_code"], 3);
}

#[test]
fn a_failed_call_carries_is_error_and_a_null_exit_code() {
    let mut t = Tracker::default();
    t.observe(&exec("s", "r1", "mcp__fs__put", "{}"));
    let f = t
        .observe(&output("s", "r1", "disk full", true, None))
        .unwrap();
    assert_eq!(f.payload["is_error"], true);
    assert_eq!(f.payload["exit_code"], Value::Null);
}

/// Observe-only (#964): a refused call fires nothing and leaves no open
/// call behind; `update_tasks`'s unmeasured ack still fires.
#[test]
fn a_refused_call_fires_no_post_tool_use() {
    let mut t = Tracker::default();
    t.observe(&exec("s", "r1", "mcp__fs__put", "{}"));
    assert!(t.observe(&refused("s", "r1")).is_none());
    assert!(t.execs.is_empty());
    t.observe(&exec("s", "r2", "update_tasks", "{}"));
    let mut ack = refused("s", "r2");
    if let OutEvent::ToolOutput { is_error, .. } = &mut ack {
        *is_error = false;
    }
    assert!(t.observe(&ack).is_some());
}

/// The executor's own routes fire nothing, as in 0.7 (`post_tool_use` ran
/// only from the host-tool execute step); `load_skill`/`update_tasks` took
/// that step through the `Control` bypass, so they still fire.
#[test]
fn route_tools_fire_no_post_tool_use() {
    let mut t = Tracker::default();
    for (i, tool) in ROUTE_TOOLS.iter().enumerate() {
        let rid = format!("r{i}");
        t.observe(&exec("s", &rid, tool, "{}"));
        assert!(
            t.observe(&output("s", &rid, "x", false, None)).is_none(),
            "{tool}"
        );
    }
    assert!(t.execs.is_empty());
    for tool in ["load_skill", "update_tasks", "mcp__gh__issue"] {
        t.observe(&exec("s", tool, tool, "{}"));
        assert!(
            t.observe(&output("s", tool, "x", false, None)).is_some(),
            "{tool}"
        );
    }
}

/// A result core produced without asking the runtime (no `ToolExec`) fires
/// nothing, and each `ToolExec` fires once.
#[test]
fn only_a_call_the_runtime_ran_fires_and_only_once() {
    let mut t = Tracker::default();
    assert!(t.observe(&output("s", "r1", "x", true, None)).is_none());
    t.observe(&exec("s", "r2", "mcp__fs__get", "{}"));
    assert!(t
        .observe(&output("other", "r2", "x", false, None))
        .is_none());
    assert!(t.observe(&output("s", "r2", "x", false, None)).is_some());
    assert!(t.observe(&output("s", "r2", "x", false, None)).is_none());
}

#[test]
fn a_session_end_forgets_its_open_calls() {
    let mut t = Tracker::default();
    t.observe(&exec("s", "r1", "mcp__fs__get", "{}"));
    t.observe(&OutEvent::SessionEnded {
        session: sid("s"),
        ts: 1,
    });
    assert!(t.execs.is_empty());
}

/// Without a `post_tool_use` command nothing is kept per call, and a lag
/// drops the calls whose `ToolOutput` it may have swallowed.
#[test]
fn open_calls_are_kept_only_when_needed_and_dropped_on_lag() {
    let mut idle = Tracker {
        skip_execs: true,
        ..Tracker::default()
    };
    idle.observe(&exec("s", "r1", "mcp__fs__get", "{}"));
    assert!(idle.execs.is_empty());
    assert!(idle.observe(&output("s", "r1", "x", false, None)).is_none());

    let mut t = Tracker::default();
    t.observe(&exec("s", "r1", "mcp__fs__get", "{}"));
    t.forget_after_lag();
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

/// `skutter`'s own tools run `post_tool_use` themselves (#968): the
/// tracker keeps nothing for them, so the hook never fires twice.
#[test]
fn host_tools_fire_no_post_tool_use_here() {
    let mut t = Tracker::default();
    for tool in crate::tool_vocab::HOST_TOOL_NAMES {
        t.observe(&exec("s", tool, tool, "{}"));
        assert!(
            t.observe(&output("s", tool, "x", false, None)).is_none(),
            "{tool}"
        );
    }
    assert!(t.execs.is_empty());
}
