//! What the `serve` wake brings back (ADR-0247): a woken root's sub-agents
//! its hibernate put down — not a closed one, nor a closed root — and a
//! sub-agent's own listing wakes its root.

use std::time::{Duration, Instant};

use entanglement_core::{InMsg, OutEvent, SessionId};

use crate::attended_mode::approve;
use crate::attended_serve::{connect, first, send};
use entanglement_runtime::session_store::LogPayload;

use crate::attended_wake::{
    asleep_with_card, drain, errors, hibernate, list, served, served_with, Served,
};

/// A closed id is never woken: its listing triggers no resume (which core
/// would refuse with an `Error`).
#[tokio::test]
async fn a_closed_session_is_not_woken() {
    let s = served(false).await;
    let sid = SessionId::new("ws-closed");
    asleep_with_card(&s, &sid).await;

    let mut ws = connect(s.port).await;
    send(
        &mut ws,
        &InMsg::CloseSession {
            session: sid.clone(),
        },
    )
    .await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    let listing = InMsg::ListPending {
        correlation_id: "c".into(),
        root: Some(sid.clone()),
    };
    send(&mut ws, &listing).await;
    let seen = drain(&mut ws, Duration::from_secs(1)).await;
    assert!(
        errors(&seen).is_empty(),
        "a closed id was woken: {:?}",
        errors(&seen)
    );
    assert!(!s.holly.is_live(&sid));
}

/// `root` started (claimed by a client that then leaves) with a sub-agent
/// `child` whose model call parks, the tree then hibernated by the root:
/// returns the child's parked request id (its card's, or its question's).
async fn asleep_with_child(s: &Served, root: &SessionId, child: &SessionId) -> String {
    let mut ws = connect(s.port).await;
    let mut sub = s.holly.subscribe();
    s.holly
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
        if matches!(&ev, OutEvent::ModeChanged { session, .. } if session == root) {
            break;
        }
    }
    list(&mut ws, root).await;
    s.holly
        .send(InMsg::Spawn {
            session: child.clone(),
            parent: Some(root.clone()),
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
    let rid = first(&mut ws, |e| match e {
        OutEvent::ToolRequest {
            session,
            request_id,
            ..
        }
        | OutEvent::UserQuestion {
            session,
            request_id,
            ..
        } if session == *child => Some(request_id),
        _ => None,
    })
    .await;
    drop(ws);
    hibernate(s, root).await;
    rid
}

/// The reviewer's repro: a sub-agent's card under a root that hibernated
/// with it. The root's listing wakes both, lists the child's card at once,
/// and its approve runs the child's call.
#[tokio::test]
async fn a_woken_root_brings_back_its_sub_agents_card() {
    let s = served(false).await;
    let (root, child) = (SessionId::new("ws-root"), SessionId::new("ws-child"));
    let rid = asleep_with_child(&s, &root, &child).await;

    let mut ws = connect(s.port).await;
    let started = Instant::now();
    let listed = list(&mut ws, &root).await;
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "{:?}",
        started.elapsed()
    );
    assert!(
        listed
            .iter()
            .any(|a| a.session == child && a.request_id == rid),
        "{listed:?}"
    );
    send(&mut ws, &approve(&child, &rid)).await;
    let output = first(&mut ws, |e| match e {
        OutEvent::ToolOutput {
            session, output, ..
        } if session == child => Some(output),
        _ => None,
    })
    .await;
    assert_eq!(output, "ran: echo hi");
}

/// Review F2: a listing naming the hibernated sub-agent itself wakes its
/// root (a child has no log of its own), which brings the child back.
#[tokio::test]
async fn a_sub_agents_own_listing_wakes_its_root() {
    let s = served(false).await;
    let (root, child) = (SessionId::new("ws-r2"), SessionId::new("ws-c2"));
    let rid = asleep_with_child(&s, &root, &child).await;

    let mut ws = connect(s.port).await;
    let listed = list(&mut ws, &child).await;
    assert!(listed.iter().any(|a| a.request_id == rid), "{listed:?}");
}

/// The same for the child's `ask_user` question and `ListQuestions`.
#[tokio::test]
async fn a_sub_agents_question_listing_wakes_its_root() {
    let ask = crate::unattended_mode::ask_user_call(
        "q1",
        r#"{"questions":[{"question":"Which?","options":[{"label":"A"}]}]}"#,
    );
    let s = served_with(false, vec![ask, crate::unattended_mode::done_text()]).await;
    let (root, child) = (SessionId::new("ws-r3"), SessionId::new("ws-c3"));
    let rid = asleep_with_child(&s, &root, &child).await;

    let mut ws = connect(s.port).await;
    let list = InMsg::ListQuestions {
        correlation_id: "q".into(),
        session: Some(child.clone()),
    };
    send(&mut ws, &list).await;
    let questions = first(&mut ws, |e| match e {
        OutEvent::QuestionList { questions, .. } => Some(questions),
        _ => None,
    })
    .await;
    assert!(
        questions.iter().any(|q| q.request_id == rid),
        "{questions:?}"
    );
}

/// Review F1: a sub-agent closed while hibernated stays closed — the
/// root's listing does not revive it, nor wait on its card.
#[tokio::test]
async fn a_child_closed_while_hibernated_stays_closed() {
    let s = served(false).await;
    let (root, child) = (SessionId::new("ws-r4"), SessionId::new("ws-c4"));
    asleep_with_child(&s, &root, &child).await;
    s.holly
        .send(InMsg::CloseSession {
            session: child.clone(),
        })
        .await
        .unwrap();
    wait_logged(
        &s,
        &root,
        |p| matches!(p, LogPayload::In(InMsg::CloseSession { session }) if *session == child),
    )
    .await;

    let mut ws = connect(s.port).await;
    let started = Instant::now();
    let listed = list(&mut ws, &root).await;
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "{:?}",
        started.elapsed()
    );
    assert!(listed.is_empty(), "{listed:?}");
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(s.holly.is_live(&root));
    assert!(!s.holly.is_live(&child), "the closed child came back");
}

/// Review F4: a wake that fails (the log unreadable) leaves the root asleep,
/// so a later listing retries it and lists the card.
#[tokio::test]
async fn a_failed_wake_is_retried_by_the_next_listing() {
    use std::os::unix::fs::PermissionsExt;
    let s = served(false).await;
    let sid = SessionId::new("ws-retry");
    let rid = asleep_with_card(&s, &sid).await;
    let dir = s.store.dir().expect("store dir");
    let locked = std::fs::Permissions::from_mode(0o000);
    std::fs::set_permissions(&dir, locked).expect("chmod 000");

    let mut ws = connect(s.port).await;
    let listed = list(&mut ws, &sid).await;
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).expect("chmod back");
    assert!(listed.is_empty(), "nothing could be woken: {listed:?}");
    assert!(!s.holly.is_live(&sid));

    let listed = list(&mut ws, &sid).await;
    assert!(listed.iter().any(|a| a.request_id == rid), "{listed:?}");
}

/// Wait until `root`'s log holds a record `hit` accepts.
async fn wait_logged(s: &Served, root: &SessionId, hit: impl Fn(&LogPayload) -> bool) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while !s
        .store
        .read(root)
        .unwrap_or_default()
        .iter()
        .any(|r| hit(&r.payload))
    {
        assert!(tokio::time::Instant::now() < deadline, "never logged");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Review F1: a frame for another session of the woken tree — the child's
/// `Approve` behind the root's listing — waits for the wake too.
#[tokio::test]
async fn a_childs_approve_behind_the_roots_listing_waits() {
    let s = served(false).await;
    let (root, child) = (SessionId::new("ws-r5"), SessionId::new("ws-c5"));
    let rid = asleep_with_child(&s, &root, &child).await;

    let mut ws = connect(s.port).await;
    let listing = InMsg::ListPending {
        correlation_id: "c".into(),
        root: Some(root.clone()),
    };
    send(&mut ws, &listing).await;
    send(&mut ws, &approve(&child, &rid)).await;
    let seen = drain(&mut ws, Duration::from_secs(3)).await;
    assert!(errors(&seen).is_empty(), "{:?}", errors(&seen));
    assert!(
        seen.iter()
            .any(|e| matches!(e, OutEvent::ToolOutput { session, output, .. }
            if *session == child && output == "ran: echo hi")),
        "{seen:?}"
    );
}

/// Review F1: a prompt to the root behind a child's listing lands on the
/// resumed root, not a blank one that the wake would then collide with.
#[tokio::test]
async fn a_roots_prompt_behind_a_childs_listing_waits() {
    let s = served(false).await;
    let (root, child) = (SessionId::new("ws-r6"), SessionId::new("ws-c6"));
    asleep_with_child(&s, &root, &child).await;

    let mut ws = connect(s.port).await;
    let listing = InMsg::ListPending {
        correlation_id: "c".into(),
        root: Some(child.clone()),
    };
    send(&mut ws, &listing).await;
    send(&mut ws, &InMsg::prompt(root.clone(), "next")).await;
    let seen = drain(&mut ws, Duration::from_secs(3)).await;
    assert!(errors(&seen).is_empty(), "{:?}", errors(&seen));
    let starts: Vec<_> = seen
        .iter()
        .filter(|e| matches!(e, OutEvent::SessionStarted { session, .. } if *session == root))
        .collect();
    assert_eq!(starts.len(), 1, "one start, the wake's: {starts:?}");
    assert!(
        seen.iter()
            .any(|e| matches!(e, OutEvent::ToolCall { session, .. } if *session == root)),
        "the prompt ran a turn on the resumed root: {seen:?}"
    );
}

/// A root resumed by another route (a trusted `resume_with_mode`) is awake:
/// a listing then wakes nothing — no second resume, no `Error`.
#[tokio::test]
async fn a_root_resumed_elsewhere_is_not_woken_again() {
    use entanglement_runtime::session_store::resume_with_mode;
    let s = served(false).await;
    let sid = SessionId::new("ws-elsewhere");
    let rid = asleep_with_card(&s, &sid).await;
    let mut sub = s.holly.subscribe();
    let (records, check) = s.store.load_for_resume(&sid, |_| true).expect("loads");
    resume_with_mode(&s.holly, &sid, &records, &check, false, None)
        .await
        .expect("resumes");
    loop {
        let ev = tokio::time::timeout(Duration::from_secs(5), sub.recv())
            .await
            .expect("re-offered")
            .expect("up");
        if matches!(&ev, OutEvent::ToolRequest { session, .. } if *session == sid) {
            break;
        }
    }

    let mut ws = connect(s.port).await;
    let listed = list(&mut ws, &sid).await;
    assert!(listed.iter().any(|a| a.request_id == rid), "{listed:?}");
    let seen = drain(&mut ws, Duration::from_millis(500)).await;
    assert!(errors(&seen).is_empty(), "{:?}", errors(&seen));
    assert!(
        !seen
            .iter()
            .any(|e| matches!(e, OutEvent::SessionStarted { session, .. } if *session == sid)),
        "resumed twice: {seen:?}"
    );
}
