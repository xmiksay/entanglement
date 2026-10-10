//! Integration test: the observe-only lifecycle hook points (#734) —
//! `session_start`, `session_end`, `stop`, `pre_compact`, `subagent_stop` and
//! `notification` — fired off the real engine's event stream by the executor's
//! lifecycle watcher. Every hook appends `<event> <session> <tool> <payload>`
//! to one log; the watcher runs hooks sequentially, so lines never interleave.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use entanglement_core::{
    stream_from_response, EngineConfig, Holly, InMsg, Llm, LlmRequest, LlmResponse, LlmStream,
    OutEvent, Permission, PermissionProfile, SessionId, ToolCall,
};
use entanglement_runtime::agents::built_in_registry;
use entanglement_runtime::hooks::{HookSpec, Hooks};
use entanglement_runtime::host::host_tools;
use entanglement_runtime::tool_runner::ToolExecutor;
use serde_json::Value;
use tokio::sync::broadcast::Receiver;

/// Pops scripted replies, then answers `"ok"` forever.
struct ScriptedLlm(Arc<Mutex<Vec<LlmResponse>>>);
#[async_trait]
impl Llm for ScriptedLlm {
    async fn stream(&mut self, _req: LlmRequest<'_>) -> anyhow::Result<LlmStream> {
        let resp = self.0.lock().unwrap().pop().unwrap_or_else(|| text("ok"));
        Ok(stream_from_response(resp))
    }
}

fn text(t: &str) -> LlmResponse {
    LlmResponse {
        text: t.into(),
        ..Default::default()
    }
}

fn spec(command: &str, timeout_secs: u64) -> HookSpec {
    HookSpec {
        command: command.to_string(),
        tools: Vec::new(),
        timeout_secs,
    }
}

/// A hook appending one `<event> <session> <tool|-> <payload>` line to `log`.
fn logger(log: &Path) -> HookSpec {
    spec(
        &format!(
            "{{ printf '%s %s %s ' \"$ENTANGLEMENT_HOOK_EVENT\" \"$ENTANGLEMENT_SESSION_ID\" \
             \"${{ENTANGLEMENT_TOOL_NAME:--}}\"; cat; echo; }} >> {}",
            log.display()
        ),
        30,
    )
}

/// Every observe-only point logging to `log`.
fn all_points(log: &Path) -> Hooks {
    let l = vec![logger(log)];
    Hooks {
        session_start: l.clone(),
        session_end: l.clone(),
        stop: l.clone(),
        pre_compact: l.clone(),
        subagent_stop: l.clone(),
        notification: l,
        ..Default::default()
    }
}

/// One parsed hook-log line.
#[derive(Debug)]
struct Line {
    event: String,
    session: String,
    tool: String,
    payload: Value,
}

fn lines(log: &Path) -> Vec<Line> {
    let text = std::fs::read_to_string(log).unwrap_or_default();
    text.lines()
        .filter_map(|l| {
            let mut parts = l.splitn(4, ' ');
            Some(Line {
                event: parts.next()?.to_string(),
                session: parts.next()?.to_string(),
                tool: parts.next()?.to_string(),
                payload: serde_json::from_str(parts.next()?).ok()?,
            })
        })
        .collect()
}

/// Poll `log` until `pred` holds over its lines (5s budget).
async fn wait_lines(log: &Path, pred: impl Fn(&[Line]) -> bool) -> Vec<Line> {
    for _ in 0..100 {
        let got = lines(log);
        if pred(&got) {
            return got;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("hook log never matched; got {:?}", lines(log));
}

fn count(lines: &[Line], event: &str, session: &str) -> usize {
    lines
        .iter()
        .filter(|l| l.event == event && l.session == session)
        .count()
}

fn find<'a>(lines: &'a [Line], event: &str, session: &str) -> &'a Line {
    lines
        .iter()
        .find(|l| l.event == event && l.session == session)
        .unwrap_or_else(|| panic!("no {event} for {session} in {lines:?}"))
}

struct Rig {
    holly: Holly,
    log: PathBuf,
    _dir: tempfile::TempDir,
}

/// An engine + executor with `hooks(log)` wired, under a temp root.
fn rig(replies: Vec<LlmResponse>, base: Permission, hooks: impl Fn(&Path) -> Hooks) -> Rig {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("hello.txt"), "greetings\n").unwrap();
    let log = dir.path().join("hooks.log");
    let tools = host_tools(dir.path().to_path_buf());
    let mut replies = replies;
    replies.reverse();
    let replies = Arc::new(Mutex::new(replies));
    let holly = Holly::spawn(EngineConfig {
        llm_factory: Arc::new(move || Box::new(ScriptedLlm(replies.clone())) as Box<dyn Llm>),
        tool_specs: tools.specs(),
        agents: built_in_registry().expect("built-in agents must parse"),
        ..EngineConfig::default()
    });
    let _exec = ToolExecutor::builder(&holly, tools.shared())
        .agents(built_in_registry().expect("built-in agents must parse"))
        .base(PermissionProfile::new(base))
        .hooks(hooks(&log))
        .spawn();
    Rig {
        holly,
        log,
        _dir: dir,
    }
}

/// Every event until `sid`'s next `Done` (5s budget).
async fn until_done(sub: &mut Receiver<OutEvent>, sid: &SessionId) -> Vec<OutEvent> {
    let mut out = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while let Ok(Ok(ev)) = tokio::time::timeout_at(deadline, sub.recv()).await {
        let done = matches!(&ev, OutEvent::Done { session, .. } if session == sid);
        out.push(ev);
        if done {
            return out;
        }
    }
    panic!("{sid} never finished a turn");
}

#[tokio::test]
async fn a_fresh_turn_fires_session_start_then_stop_once() {
    let r = rig(vec![text("hi")], Permission::Allow, all_points);
    let sid = SessionId::new("s1");
    let mut sub = r.holly.subscribe();
    r.holly
        .send(InMsg::prompt(sid.clone(), "hello"))
        .await
        .unwrap();
    until_done(&mut sub, &sid).await;

    wait_lines(&r.log, |l| count(l, "stop", "s1") == 1).await;
    // Let any duplicate land before counting exactly-once.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let got = lines(&r.log);
    assert_eq!(count(&got, "session_start", "s1"), 1, "{got:?}");
    assert_eq!(count(&got, "stop", "s1"), 1, "{got:?}");
    assert_eq!(count(&got, "subagent_stop", "s1"), 0);
    assert_eq!(got[0].event, "session_start", "start precedes stop");
    let start = find(&got, "session_start", "s1");
    assert_eq!(start.payload["source"], "fresh");
    assert_eq!(start.payload["event"], "session_start");
    assert_eq!(start.payload["root"], true);
    let stop = find(&got, "stop", "s1");
    assert_eq!(stop.payload["reason"], "completed");
    assert_eq!(stop.tool, "-", "no tool env outside an approval");
}

#[tokio::test]
async fn an_approval_park_fires_notification_with_the_tool_env() {
    let read = LlmResponse {
        tool_calls: vec![ToolCall {
            id: "r1".into(),
            name: "read".into(),
            input: r#"{"path":"hello.txt"}"#.into(),
            provider_meta: None,
        }],
        ..Default::default()
    };
    // An `Ask` ceiling clamps the `read` into a parked approval.
    let r = rig(vec![read], Permission::Ask, all_points);
    r.holly
        .send(InMsg::prompt(SessionId::new("s1"), "read it"))
        .await
        .unwrap();

    let got = wait_lines(&r.log, |l| count(l, "notification", "s1") == 1).await;
    let n = find(&got, "notification", "s1");
    assert_eq!(n.tool, "read");
    assert_eq!(n.payload["kind"], "approval");
    assert_eq!(n.payload["tool"], "read");
    assert_eq!(n.payload["input"]["path"], "hello.txt");
    assert_eq!(count(&got, "stop", "s1"), 0, "the turn is still parked");
}

#[tokio::test]
async fn a_child_turn_fires_subagent_stop_not_stop() {
    let r = rig(vec![], Permission::Allow, all_points);
    let (parent, child) = (SessionId::new("p"), SessionId::new("c"));
    let mut sub = r.holly.subscribe();
    r.holly
        .send(InMsg::prompt(parent.clone(), "hi"))
        .await
        .unwrap();
    until_done(&mut sub, &parent).await;
    r.holly
        .send(InMsg::Spawn {
            session: child.clone(),
            parent: Some(parent.clone()),
            predecessor: None,
            agent: "general".into(),
            prompt: "child task".into(),
            user: None,
            model: None,
            mode: None,
        })
        .await
        .unwrap();

    let got = wait_lines(&r.log, |l| count(l, "subagent_stop", "c") == 1).await;
    let stop = find(&got, "subagent_stop", "c");
    assert_eq!(stop.payload["parent"], "p");
    assert_eq!(stop.payload["agent"], "general");
    assert_eq!(count(&got, "stop", "c"), 0);
    assert_eq!(find(&got, "session_start", "c").payload["parent"], "p");
}

#[tokio::test]
async fn hibernate_then_resume_ends_and_restarts_as_resume() {
    let r = rig(vec![], Permission::Allow, all_points);
    let sid = SessionId::new("s1");
    let mut sub = r.holly.subscribe();
    r.holly
        .send(InMsg::prompt(sid.clone(), "hello"))
        .await
        .unwrap();
    let log = until_done(&mut sub, &sid).await;

    r.holly.hibernate(sid.clone()).await.unwrap();
    let got = wait_lines(&r.log, |l| count(l, "session_end", "s1") == 1).await;
    assert_eq!(
        find(&got, "session_end", "s1").payload["reason"],
        "hibernated"
    );

    let records = log.into_iter().map(|e| (None, e)).collect();
    r.holly.resume(sid.clone(), records).await.unwrap();
    let got = wait_lines(&r.log, |l| count(l, "session_start", "s1") == 2).await;
    let sources: Vec<_> = got
        .iter()
        .filter(|l| l.event == "session_start")
        .map(|l| l.payload["source"].clone())
        .collect();
    assert_eq!(sources, ["fresh", "resume"]);
}

#[tokio::test]
async fn manual_compaction_fires_pre_compact_and_hands_over_to_the_successor() {
    let r = rig(
        vec![text("turn"), text("the summary")],
        Permission::Allow,
        all_points,
    );
    let sid = SessionId::new("src");
    let mut sub = r.holly.subscribe();
    r.holly
        .send(InMsg::prompt(sid.clone(), "hello"))
        .await
        .unwrap();
    until_done(&mut sub, &sid).await;
    r.holly
        .send(InMsg::Oneshot {
            session: sid.clone(),
            op: "compact".into(),
            args: Value::Null,
        })
        .await
        .unwrap();

    let got = wait_lines(&r.log, |l| {
        count(l, "session_end", "src") == 1
            && l.iter()
                .any(|x| x.event == "stop" && x.payload["session"] != "src")
    })
    .await;
    let pre = find(&got, "pre_compact", "src");
    assert_eq!(pre.payload["kind"], "manual");
    assert!(pre.payload["summary"]
        .as_str()
        .unwrap()
        .contains("the summary"));
    assert_eq!(
        find(&got, "session_end", "src").payload["reason"],
        "compacted"
    );
    // The compaction op's own `Done` is not a second turn end.
    assert_eq!(count(&got, "stop", "src"), 1);
    let successor = got
        .iter()
        .find(|l| l.event == "session_start" && l.payload["predecessor"] == "src")
        .expect("successor announced");
    assert_eq!(successor.payload["source"], "compact");
}

/// No observe-only point can block: a failing and a hung hook are logged, the
/// turn finishes, and the hooks after them still run.
#[tokio::test]
async fn failing_and_timed_out_hooks_block_nothing() {
    let r = rig(vec![text("hi")], Permission::Allow, |log| Hooks {
        session_start: vec![spec("sleep 30", 1), logger(log)],
        stop: vec![spec("exit 7", 30), logger(log)],
        ..Default::default()
    });
    let sid = SessionId::new("s1");
    let mut sub = r.holly.subscribe();
    r.holly
        .send(InMsg::prompt(sid.clone(), "hello"))
        .await
        .unwrap();
    let events = until_done(&mut sub, &sid).await;
    assert!(!events.iter().any(|e| matches!(e, OutEvent::Error { .. })));

    let got = wait_lines(&r.log, |l| count(l, "stop", "s1") == 1).await;
    assert_eq!(count(&got, "session_start", "s1"), 1);
}
