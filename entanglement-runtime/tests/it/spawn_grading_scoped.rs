//! Scoped `agent(<profile>)`/`agent_send(<agent_id>)` rules (#867,
//! ADR-0235): the spawn target is the pair's permission argument, so a
//! scoped rule grades only the target it names, an "always allow" grant is
//! scoped to that target, and the unattended repeat-denial key tells two
//! different spawns apart.

use std::time::Duration;

use entanglement_core::{ApprovalScope, Holly, InMsg, OutEvent, SessionId};
use entanglement_runtime::mode::Limits;
use entanglement_runtime::tool_names::{AGENT_SEND_TOOL, AGENT_TOOL};
use tokio::sync::broadcast::Receiver;

use crate::spawn_grading::{assert_refused, run_turn, spawn_calls, table_with};

const PLAN: &str = r#"{"agent":"plan","prompt":"child-task","background":true}"#;
const GENERAL: &str = r#"{"agent":"general","prompt":"child-task","background":true}"#;
/// Names no profile: the spawn runs `general`, so it grades as `general`.
const DEFAULTED: &str = r#"{"prompt":"child-task","background":true}"#;
const SEND: &str = r#"{"agent_id":" no-such-child ","prompt":"follow-up"}"#;
const WAIT: Duration = Duration::from_secs(5);

fn engine(calls: &[(&'static str, &'static str)], deny: &[&str], prompt: &[&str]) -> Holly {
    spawn_calls(calls, table_with(deny, prompt, Limits::default()))
}

fn child_started(events: &[OutEvent]) -> bool {
    events.iter().any(|e| {
        matches!(
            e,
            OutEvent::SessionStarted {
                parent: Some(_),
                ..
            }
        )
    })
}

/// The `tool` results `sid` got, in order, through its `Done`.
fn outputs(events: &[OutEvent], sid: &SessionId, tool: &str) -> Vec<String> {
    events
        .iter()
        .filter_map(|e| match e {
            OutEvent::ToolOutput {
                session,
                tool: t,
                output,
                ..
            } if session == sid && t == tool => Some(output.clone()),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn a_scoped_deny_refuses_only_the_named_profile() {
    let sid = SessionId::new("s1");
    let holly = engine(&[(AGENT_TOOL, PLAN)], &["agent(plan)"], &[]);
    assert_refused(&run_turn(&holly, &sid).await, AGENT_TOOL, "denied by mode");

    let holly = engine(&[(AGENT_TOOL, GENERAL)], &["agent(plan)"], &[]);
    assert!(
        child_started(&run_turn(&holly, &sid).await),
        "`agent(plan)` must not touch a `general` spawn"
    );
}

/// The argument is the profile that actually runs, so a call that omits
/// `agent` is caught by a rule naming the default.
#[tokio::test]
async fn a_scoped_rule_on_the_default_profile_covers_an_omitted_agent() {
    let holly = engine(&[(AGENT_TOOL, DEFAULTED)], &["agent(general)"], &[]);
    let events = run_turn(&holly, &SessionId::new("s1")).await;
    assert_refused(&events, AGENT_TOOL, "denied by mode");
}

/// The argument scope globs like any tool's (`bash(cargo *)`).
#[tokio::test]
async fn a_scoped_glob_deny_matches_by_pattern() {
    let holly = engine(&[(AGENT_TOOL, PLAN)], &["agent(pl*)"], &[]);
    let events = run_turn(&holly, &SessionId::new("s1")).await;
    assert_refused(&events, AGENT_TOOL, "denied by mode");
}

async fn next_card(sub: &mut Receiver<OutEvent>) -> (String, String) {
    loop {
        match tokio::time::timeout(WAIT, sub.recv()).await {
            Ok(Ok(OutEvent::ToolRequest {
                request_id,
                tool,
                input,
                ..
            })) if tool == AGENT_TOOL => return (request_id, input),
            Ok(Ok(_)) => {}
            other => panic!("expected an `agent` approval card: {other:?}"),
        }
    }
}

/// `prompt: [agent(plan)]` asks for `plan` only: a `general` spawn, which
/// no rule names, runs unasked under `default: allow`.
#[tokio::test]
async fn a_scoped_prompt_asks_only_for_its_target() {
    let calls = [(AGENT_TOOL, PLAN), (AGENT_TOOL, GENERAL)];
    let holly = engine(&calls, &[], &["agent(plan)"]);
    let sid = SessionId::new("s1");
    let mut sub = holly.subscribe();
    holly.send(InMsg::prompt(sid.clone(), "go")).await.unwrap();
    let (request_id, input) = next_card(&mut sub).await;
    assert!(input.contains("\"plan\""), "{input}");
    holly
        .send(InMsg::Approve {
            session: sid.clone(),
            request_id,
            scope: ApprovalScope::Once,
            mode: None,
            run_all: false,
        })
        .await
        .unwrap();
    let mut events = Vec::new();
    while let Ok(Ok(ev)) = tokio::time::timeout(WAIT, sub.recv()).await {
        let done = matches!(&ev, OutEvent::Done { session, .. } if session == &sid);
        events.push(ev);
        if done {
            break;
        }
    }
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, OutEvent::ToolRequest { .. })),
        "the `general` spawn must not ask: {events:?}"
    );
    let outs = outputs(&events, &sid, AGENT_TOOL);
    assert_eq!(outs.len(), 2, "both spawns answered: {outs:?}");
}

/// A grant earned on `agent(plan)` must not widen another target that still
/// asks.
#[tokio::test]
async fn a_scoped_grant_does_not_cover_another_asking_target() {
    let calls = [(AGENT_TOOL, PLAN), (AGENT_TOOL, GENERAL)];
    let holly = engine(&calls, &[], &[AGENT_TOOL]);
    let sid = SessionId::new("s1");
    let mut sub = holly.subscribe();
    holly.send(InMsg::prompt(sid.clone(), "go")).await.unwrap();
    let (request_id, _) = next_card(&mut sub).await;
    holly
        .send(InMsg::Approve {
            session: sid.clone(),
            request_id,
            scope: ApprovalScope::Session,
            mode: None,
            run_all: false,
        })
        .await
        .unwrap();
    let (_, input) = next_card(&mut sub).await;
    assert!(input.contains("\"general\""), "{input}");
}

/// The gate runs before the handle lookup, so a static id is enough to
/// prove the scoped match; the id is trimmed for grading.
#[tokio::test]
async fn a_scoped_agent_send_rule_matches_only_its_handle() {
    let sid = SessionId::new("s1");
    let holly = engine(&[(AGENT_SEND_TOOL, SEND)], &["agent_send(no-such-*)"], &[]);
    let events = run_turn(&holly, &sid).await;
    assert_refused(&events, AGENT_SEND_TOOL, "denied by mode");

    let holly = engine(
        &[(AGENT_SEND_TOOL, SEND)],
        &["agent_send(other-child)"],
        &[],
    );
    let outs = outputs(&run_turn(&holly, &sid).await, &sid, AGENT_SEND_TOOL);
    assert!(
        outs.len() == 1 && outs[0].contains("unknown agent_id") && !outs[0].contains("denied"),
        "a non-matching rule leaves the call to the handle lookup: {outs:?}"
    );
}

/// Unattended (`question_timeout` > 0), a first `Ask` collapses to a denial
/// and only a *repeat of the same call* parks a card (ADR-0207 §11). Keyed on
/// the target, two different spawns are two first calls.
#[tokio::test]
async fn unattended_repeat_keying_tells_spawn_targets_apart() {
    let limits = Limits {
        question_timeout: 30,
        ..Limits::default()
    };
    let sid = SessionId::new("s1");

    let calls = [(AGENT_TOOL, PLAN), (AGENT_TOOL, GENERAL)];
    let holly = spawn_calls(&calls, table_with(&[], &[AGENT_TOOL], limits));
    let events = run_turn(&holly, &sid).await;
    let outs = outputs(&events, &sid, AGENT_TOOL);
    assert_eq!(outs.len(), 2, "{events:?}");
    assert!(outs.iter().all(|o| o.contains("unattended")), "{outs:?}");
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, OutEvent::ToolRequest { .. })),
        "two different targets are not a repeat: {events:?}"
    );

    let calls = [(AGENT_TOOL, PLAN), (AGENT_TOOL, PLAN)];
    let holly = spawn_calls(&calls, table_with(&[], &[AGENT_TOOL], limits));
    let mut sub = holly.subscribe();
    holly.send(InMsg::prompt(sid.clone(), "go")).await.unwrap();
    let (_, input) = next_card(&mut sub).await;
    assert!(input.contains("\"plan\""), "the same target repeated parks");
}
