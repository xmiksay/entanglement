//! ADR-0233 (#821): every binding call a script makes is reported live as a
//! `NestedCall` carrying the `rhai` call's id — the first 50 of a run one by
//! one, the rest counted into one closing `NestedCallSummary` — blocking and
//! background alike, and never written to the session log.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use entanglement_core::{Holly, InMsg, OutEvent, Permission, SessionId};
use entanglement_runtime::persistence::RecordSink;
use entanglement_runtime::session_store::{LogPayload, LogRecord};
use entanglement_runtime::tool_names::RHAI_TOOL;

use super::unified_harness::{run, start, Setup};
use super::TempDir;

/// `(index, tool, is_error, duration_ms, parent)` of every `NestedCall`.
fn nested(events: &[OutEvent]) -> Vec<(u64, String, bool, u64, String)> {
    events
        .iter()
        .filter_map(|e| match e {
            OutEvent::NestedCall {
                index,
                tool,
                is_error,
                duration_ms,
                parent_request_id,
                ..
            } => Some((
                *index,
                tool.clone(),
                *is_error,
                *duration_ms,
                parent_request_id.clone(),
            )),
            _ => None,
        })
        .collect()
}

/// `(suppressed, errors)` of every `NestedCallSummary`.
fn summaries(events: &[OutEvent]) -> Vec<(u64, u64)> {
    events
        .iter()
        .filter_map(|e| match e {
            OutEvent::NestedCallSummary {
                suppressed, errors, ..
            } => Some((*suppressed, *errors)),
            _ => None,
        })
        .collect()
}

fn allow() -> Setup {
    Setup::mode(Permission::Allow, &[], 0)
}

/// Approve `ev` if it is a card (the launch card included).
async fn approve_card(holly: &Holly, sid: &SessionId, ev: &OutEvent) {
    if let OutEvent::ToolRequest { request_id, .. } = ev {
        holly
            .send(InMsg::Approve {
                session: sid.clone(),
                request_id: request_id.clone(),
                scope: Default::default(),
                mode: None,
                run_all: false,
            })
            .await
            .unwrap();
    }
}

#[tokio::test]
async fn each_binding_call_is_reported_under_the_rhai_call() {
    let dir = TempDir::new("nested-each");
    let events = run(
        r#"write("a.txt", "x");
           read("a.txt");
           read("missing.txt");
           glob("*.txt");"#,
        &dir.path,
        allow(),
    )
    .await;
    let calls = nested(&events);
    let shape: Vec<(u64, &str, bool)> = calls
        .iter()
        .map(|(i, tool, err, _, _)| (*i, tool.as_str(), *err))
        .collect();
    assert_eq!(
        shape,
        [
            (1, "write", false),
            (2, "read", false),
            (3, "read", true),
            (4, "glob", false)
        ],
        "{events:#?}"
    );
    // Every call names the `rhai` call's id (`t1`) and took a sane time.
    assert!(calls
        .iter()
        .all(|(_, _, _, ms, parent)| parent == "t1" && *ms < 20_000));
    assert!(summaries(&events).is_empty(), "under the cap: no summary");
    // The reports precede the script's own result, which closes the call.
    let last_nested = events
        .iter()
        .rposition(|e| matches!(e, OutEvent::NestedCall { .. }))
        .unwrap();
    let result = events
        .iter()
        .position(|e| matches!(e, OutEvent::ToolOutput { tool, .. } if tool == RHAI_TOOL))
        .unwrap();
    assert!(last_nested < result);
}

#[tokio::test]
async fn a_hundred_calls_are_fifty_reports_and_one_summary() {
    let dir = TempDir::new("nested-cap");
    // i = 0, 10, …, 90 fail: five of them (50..=90) past the cap.
    let events = run(
        r#"for i in 0..100 {
               if i % 10 == 0 { read("missing.txt"); } else { glob("*.none"); }
           }"#,
        &dir.path,
        allow(),
    )
    .await;
    let calls = nested(&events);
    assert_eq!(calls.len(), 50);
    assert_eq!(
        calls.iter().map(|c| c.0).collect::<Vec<_>>(),
        (1..=50).collect::<Vec<_>>()
    );
    assert_eq!(summaries(&events), [(50, 5)]);
}

#[tokio::test]
async fn a_background_script_reports_its_calls_after_it_detached() {
    let dir = TempDir::new("nested-bg");
    let mut setup = allow();
    setup.background = true;
    let (holly, mut sub, sid) = start(
        r#"read("missing.txt"); glob("*"); glob("*");"#,
        &dir.path,
        setup,
    )
    .await;
    let mut events = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while nested(&events).len() < 3 {
        let ev = tokio::time::timeout_at(deadline, sub.recv())
            .await
            .expect("three nested calls before the deadline")
            .expect("the outbox stays open");
        approve_card(&holly, &sid, &ev).await;
        if ev.session() == Some(&sid) {
            events.push(ev);
        }
    }
    let calls = nested(&events);
    assert!(calls.iter().all(|c| c.4 == "t1"), "{calls:?}");
    assert_eq!(
        calls.iter().map(|c| (c.0, c.2)).collect::<Vec<_>>(),
        [(1, true), (2, false), (3, false)]
    );
}

/// Captures every record the persistence tap hands it.
#[derive(Default)]
struct Captured(Mutex<Vec<LogRecord>>);
impl RecordSink for Captured {
    fn append(&self, _root: &SessionId, record: &LogRecord) -> anyhow::Result<()> {
        self.0.lock().unwrap().push(record.clone());
        Ok(())
    }
}

#[tokio::test]
async fn nested_call_events_never_reach_the_session_log() {
    let dir = TempDir::new("nested-log");
    let sink = Arc::new(Captured::default());
    let mut setup = allow();
    setup.sink = Some(sink.clone());
    let events = run(r#"for i in 0..60 { glob("*.none"); }"#, &dir.path, setup).await;
    assert_eq!(nested(&events).len(), 50);
    assert_eq!(summaries(&events), [(10, 0)]);
    // The tap writes asynchronously: wait for the turn's `Done` to land.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let logged_done = || {
        sink.0
            .lock()
            .unwrap()
            .iter()
            .any(|r| matches!(r.payload, LogPayload::Out(OutEvent::Done { .. })))
    };
    while !logged_done() {
        assert!(tokio::time::Instant::now() < deadline, "Done never logged");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let records = sink.0.lock().unwrap();
    assert!(
        !records
            .iter()
            .any(|r| matches!(&r.payload, LogPayload::Out(ev) if ev.is_live_only())),
        "a live-only event was logged"
    );
    // The script itself is logged as ever: its one call and result.
    assert!(records.iter().any(|r| matches!(
        &r.payload,
        LogPayload::Out(OutEvent::ToolOutput { tool, .. }) if tool == RHAI_TOOL
    )));
}

/// A skill's `lib.rhai` function is script code: the bindings it makes are
/// the run's nested calls like any other.
#[tokio::test]
async fn a_skill_library_functions_bindings_are_nested_calls() {
    use super::launch_harness::{fixture, run_calls, table, Launch, Setup};
    let (_tmp, root) = fixture();
    let dir = root.join(".entanglement/skills/peek");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("SKILL.md"),
        "---\nname: peek\ndescription: look\n---\nPeek.\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("lib.rhai"),
        r#"fn peek() { read("scripts/add.rhai"); glob("*.rhai") }"#,
    )
    .unwrap();
    let mut setup = Setup::new(table(Permission::Allow, &[], &[], 0), Launch::RunAll);
    setup.skills =
        entanglement_runtime::skills::load_registry_in(&root, &super::no_user(&root)).unwrap();
    let seen = run_calls(
        &root,
        &[("rhai", r#"{"script":"skill::peek::peek()"}"#)],
        setup,
    )
    .await;
    assert_eq!(seen.nested_calls, ["read", "glob"], "{:?}", seen.outputs);
}

/// A `Stop` aborts a blocking script's bridge task mid-await, so no code
/// after its loop runs — the summary must still go out (it is emitted on
/// drop), counting the call the `Stop` cut off as failed.
#[tokio::test]
async fn a_stopped_blocking_script_still_sends_its_one_summary() {
    let dir = TempDir::new("nested-stop");
    let mut setup = allow();
    setup.slow_grep = true;
    let grep_entered = setup.slow_grep_entered.clone();
    // 55 quick calls, then a `grep` that blocks for 30 s.
    let (holly, mut sub, sid) = start(
        r#"for i in 0..55 { glob("*.none"); } grep("x");"#,
        &dir.path,
        setup,
    )
    .await;
    let mut events = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    // Approve and record until the slow `grep` is running — calls 51..=55
    // have then finished, and the `Stop` cuts off exactly the grep.
    let entered = grep_entered.notified();
    tokio::pin!(entered);
    loop {
        let ev = tokio::select! {
            _ = &mut entered => break,
            ev = tokio::time::timeout_at(deadline, sub.recv()) => ev,
        };
        let ev = ev
            .expect("the slow grep starts before the deadline")
            .expect("the outbox stays open");
        approve_card(&holly, &sid, &ev).await;
        if ev.session() == Some(&sid) {
            events.push(ev);
        }
    }
    holly
        .send(InMsg::Stop {
            session: sid.clone(),
        })
        .await
        .unwrap();
    while let Ok(Ok(ev)) = tokio::time::timeout(Duration::from_secs(2), sub.recv()).await {
        if ev.session() == Some(&sid) {
            events.push(ev);
        }
    }
    assert_eq!(nested(&events).len(), 50);
    // Five held-back globs plus the cut-off grep, which counts as failed.
    assert_eq!(summaries(&events), [(6, 1)]);
}
