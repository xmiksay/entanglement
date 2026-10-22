//! Attended sessions wait for their approver (#828, ADR-0247): a session a
//! head watches — or a child of one — parks an `Ask` instead of collapsing
//! it, and its cards and questions never time out. Unattended, a retry is
//! "the same call" when the same named mode rule graded it, and a decision
//! that arrives after its card left is refused with an `Error`.

use std::sync::Arc;
use std::time::Duration;

use entanglement_core::{ApprovalScope, Holly, InMsg, OutEvent, Permission, SessionId};
use entanglement_runtime::attendance::Attendance;
use entanglement_runtime::mode::{Limits, ModeTable};
use entanglement_runtime::ToolRegistry;
use tokio::sync::broadcast::Receiver;

use crate::unattended_mode::{
    ask_user_call, bash_call, collect, collect_for, done_text, one_mode_table, spawn_attended,
    EchoBash,
};

/// A mode that asks for every call and gives up after `question_timeout`s.
pub(crate) fn asking(question_timeout: u64) -> Arc<ModeTable> {
    let limits = Limits {
        question_timeout,
        ..Limits::default()
    };
    one_mode_table(Permission::Ask, limits)
}

pub(crate) fn bash_engine(
    table: Arc<ModeTable>,
    calls: Vec<entanglement_core::LlmResponse>,
    attendance: Attendance,
) -> Holly {
    let mut reg = ToolRegistry::new();
    reg.register(EchoBash);
    spawn_attended(reg, table, calls, attendance)
}

/// `sid`'s next `ToolRequest` id.
pub(crate) async fn card(sub: &mut Receiver<OutEvent>, sid: &SessionId) -> String {
    loop {
        match tokio::time::timeout(Duration::from_secs(5), sub.recv()).await {
            Ok(Ok(OutEvent::ToolRequest {
                session,
                request_id,
                ..
            })) if &session == sid => return request_id,
            Ok(Ok(_)) => {}
            other => panic!("expected a card on {sid}: {other:?}"),
        }
    }
}

pub(crate) fn approve(sid: &SessionId, request_id: &str) -> InMsg {
    InMsg::Approve {
        session: sid.clone(),
        request_id: request_id.into(),
        scope: ApprovalScope::Once,
        mode: None,
        run_all: false,
    }
}

pub(crate) fn ran(events: &[OutEvent], input: &str) -> bool {
    events.iter().any(
        |e| matches!(e, OutEvent::ToolOutput { output, .. } if *output == format!("ran: {input}")),
    )
}

fn denials(events: &[OutEvent]) -> usize {
    events
        .iter()
        .filter(|e| matches!(e, OutEvent::ToolOutput { output, .. } if output.contains("denied")))
        .count()
}

/// No first-call collapse, and the card outlives `question_timeout`.
#[tokio::test]
async fn an_attended_sessions_ask_parks_and_never_expires() {
    let attendance = Attendance::new();
    let _tui = attendance.watch_all();
    let holly = bash_engine(
        asking(1),
        vec![bash_call("t1", "echo hi"), done_text()],
        attendance,
    );
    let sid = SessionId::new("s1");
    let mut sub = holly.subscribe();
    holly.send(InMsg::prompt(sid.clone(), "go")).await.unwrap();
    let rid = card(&mut sub, &sid).await;
    let waited = collect_for(holly.subscribe(), &sid, Duration::from_millis(1600)).await;
    assert!(
        !waited.iter().any(|e| matches!(
            e,
            OutEvent::ToolOutput { .. } | OutEvent::ApprovalResolved { .. }
        )),
        "an attended card waits past question_timeout: {waited:?}"
    );
    let after = holly.subscribe();
    holly.send(approve(&sid, &rid)).await.unwrap();
    let events = collect(after, &sid).await;
    assert!(ran(&events, "echo hi"), "{events:?}");
    assert_eq!(denials(&events), 0, "{events:?}");
}

/// A head watching only the root attends the sub-agent it spawned.
#[tokio::test]
async fn a_watched_parent_attends_its_child() {
    let attendance = Attendance::new();
    let watcher = attendance.watcher();
    let root = SessionId::new("root");
    watcher.attend(&root);
    let holly = bash_engine(
        asking(1),
        vec![bash_call("t1", "echo child"), done_text()],
        attendance,
    );
    let child = SessionId::new("child");
    let mut sub = holly.subscribe();
    // The root must exist: an unseen ancestor grades every call Deny.
    holly
        .send(InMsg::SetMode {
            session: root.clone(),
            mode: "build".into(),
        })
        .await
        .unwrap();
    loop {
        let ev = tokio::time::timeout(Duration::from_secs(5), sub.recv())
            .await
            .expect("the root starts")
            .expect("the engine is up");
        if matches!(&ev, OutEvent::ModeChanged { session, .. } if *session == root) {
            break;
        }
    }
    holly
        .send(InMsg::Spawn {
            session: child.clone(),
            parent: Some(root),
            predecessor: None,
            agent: "general".into(),
            prompt: "go".into(),
            user: None,
            model: None,
            mode: None,
            workdir: None,
        })
        .await
        .unwrap();
    let rid = card(&mut sub, &child).await;
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let after = holly.subscribe();
    holly.send(approve(&child, &rid)).await.unwrap();
    let events = collect(after, &child).await;
    assert!(ran(&events, "echo child"), "{events:?}");
}

/// Unattended, the repeat parks with a running timer; a watcher arriving
/// cancels it, so the card then waits like an attended one.
#[tokio::test]
async fn a_watcher_arriving_cancels_the_running_timeout() {
    let attendance = Attendance::new();
    let holly = bash_engine(
        asking(1),
        vec![
            bash_call("t1", "echo hi"),
            bash_call("t2", "echo hi"),
            done_text(),
        ],
        attendance.clone(),
    );
    let sid = SessionId::new("s1");
    let mut sub = holly.subscribe();
    holly.send(InMsg::prompt(sid.clone(), "go")).await.unwrap();
    let rid = card(&mut sub, &sid).await;
    let watcher = attendance.watcher();
    watcher.attend(&sid);
    let waited = collect_for(holly.subscribe(), &sid, Duration::from_millis(1600)).await;
    assert!(
        !waited
            .iter()
            .any(|e| matches!(e, OutEvent::ApprovalResolved { .. })),
        "the arriving watcher cancelled the timer: {waited:?}"
    );
    let after = holly.subscribe();
    holly.send(approve(&sid, &rid)).await.unwrap();
    assert!(ran(&collect(after, &sid).await, "echo hi"));
}

/// An attended `ask_user` waits for its answer instead of defaulting.
#[tokio::test]
async fn an_attended_question_waits_for_its_answer() {
    let attendance = Attendance::new();
    let _tui = attendance.watch_all();
    let holly = spawn_attended(
        ToolRegistry::new(),
        one_mode_table(
            Permission::Allow,
            Limits {
                question_timeout: 1,
                ..Limits::default()
            },
        ),
        vec![
            ask_user_call(
                "q1",
                r#"{"questions":[{"question":"Which?","options":[{"label":"A"},{"label":"B"}]}]}"#,
            ),
            done_text(),
        ],
        attendance,
    );
    let sid = SessionId::new("s1");
    let sub = holly.subscribe();
    holly.send(InMsg::prompt(sid.clone(), "go")).await.unwrap();
    let waited = collect_for(sub, &sid, Duration::from_millis(1600)).await;
    let rid = waited
        .iter()
        .find_map(|e| match e {
            OutEvent::UserQuestion { request_id, .. } => Some(request_id.clone()),
            _ => None,
        })
        .expect("the question was asked");
    assert!(
        !waited
            .iter()
            .any(|e| matches!(e, OutEvent::ToolOutput { .. })),
        "no default answer while attended: {waited:?}"
    );
    let after = holly.subscribe();
    holly
        .send(InMsg::answer_question(
            sid.clone(),
            rid,
            vec![vec!["B".into()]],
        ))
        .await
        .unwrap();
    let events = collect(after, &sid).await;
    assert!(
        events
            .iter()
            .any(|e| matches!(e, OutEvent::ToolOutput { output, .. } if output == "B")),
        "{events:?}"
    );
}

/// The issue's dropped approval: it arrives after the card expired, and
/// the sender is told instead of being ignored.
#[tokio::test]
async fn an_approve_after_expiry_is_refused_with_an_error() {
    let holly = bash_engine(
        asking(1),
        vec![
            bash_call("t1", "echo hi"),
            bash_call("t2", "echo hi"),
            done_text(),
        ],
        Attendance::new(),
    );
    let sid = SessionId::new("s1");
    let mut sub = holly.subscribe();
    holly.send(InMsg::prompt(sid.clone(), "go")).await.unwrap();
    let rid = card(&mut sub, &sid).await;
    collect(holly.subscribe(), &sid).await;
    let after = holly.subscribe();
    holly.send(approve(&sid, &rid)).await.unwrap();
    let events = collect_for(after, &sid, Duration::from_millis(500)).await;
    assert!(
        events.iter().any(|e| matches!(
            e,
            OutEvent::Error { message, .. }
                if message.contains("`Approve` refused: no pending card") && message.contains(&rid)
        )),
        "{events:?}"
    );
}

/// The issue's changed retry: `rm -r x` then `rm -r x 2>&1` are graded by
/// the same `auto` rule, so the second parks instead of being denied again.
#[tokio::test]
async fn a_retry_graded_by_the_same_rule_parks() {
    let first = r#"{"command":"rm -r x"}"#;
    let retry = r#"{"command":"rm -r x 2>&1"}"#;
    let holly = bash_engine(
        Arc::new(ModeTable::builtin().expect("built-in modes parse")),
        vec![bash_call("t1", first), bash_call("t2", retry), done_text()],
        Attendance::new(),
    );
    let sid = SessionId::new("s1");
    holly
        .send(InMsg::SetMode {
            session: sid.clone(),
            mode: "auto".into(),
        })
        .await
        .unwrap();
    let sub = holly.subscribe();
    let mut watch = holly.subscribe();
    holly.send(InMsg::prompt(sid.clone(), "go")).await.unwrap();
    let rid = card(&mut watch, &sid).await;
    holly.send(approve(&sid, &rid)).await.unwrap();
    let events = collect(sub, &sid).await;
    assert_eq!(denials(&events), 1, "only the first is denied: {events:?}");
    assert!(ran(&events, retry), "{events:?}");
}

/// A default grade keys on the exact argument: two different commands are
/// each denied, and nothing parks.
#[tokio::test]
async fn default_graded_different_commands_never_escalate() {
    let holly = bash_engine(
        asking(30),
        vec![
            bash_call("t1", r#"{"command":"echo a"}"#),
            bash_call("t2", r#"{"command":"echo b"}"#),
            done_text(),
        ],
        Attendance::new(),
    );
    let sid = SessionId::new("s1");
    let sub = holly.subscribe();
    holly.send(InMsg::prompt(sid.clone(), "go")).await.unwrap();
    let events = collect(sub, &sid).await;
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, OutEvent::ToolRequest { .. })),
        "{events:?}"
    );
    assert_eq!(denials(&events), 2, "{events:?}");
}

/// An attended `rhai` launch card waits for its answer: it must not fall
/// through to Ask per command after `question_timeout`.
#[cfg(feature = "rhai")]
#[tokio::test]
async fn an_attended_rhai_launch_card_waits() {
    let attendance = Attendance::new();
    let _tui = attendance.watch_all();
    let launch = entanglement_core::LlmResponse {
        tool_calls: vec![entanglement_core::ToolCall {
            id: "r1".into(),
            name: entanglement_runtime::tool_names::RHAI_TOOL.into(),
            input: r#"{"script":"40 + 2"}"#.into(),
            provider_meta: None,
        }],
        ..Default::default()
    };
    let holly = spawn_attended(
        ToolRegistry::new(),
        asking(1),
        vec![launch, done_text()],
        attendance,
    );
    let sid = SessionId::new("s1");
    let mut sub = holly.subscribe();
    holly.send(InMsg::prompt(sid.clone(), "go")).await.unwrap();
    let rid = card(&mut sub, &sid).await;
    let waited = collect_for(holly.subscribe(), &sid, Duration::from_millis(1600)).await;
    assert!(
        !waited.iter().any(|e| matches!(
            e,
            OutEvent::ToolOutput { .. } | OutEvent::ApprovalResolved { .. }
        )),
        "the launch card waits past question_timeout: {waited:?}"
    );
    let after = holly.subscribe();
    holly.send(approve(&sid, &rid)).await.unwrap();
    let events = collect(after, &sid).await;
    assert!(
        events
            .iter()
            .any(|e| matches!(e, OutEvent::ToolOutput { output, .. } if output.contains("42"))),
        "{events:?}"
    );
}
