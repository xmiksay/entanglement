//! Integration tests for the Holly engine actor: session multiplexing, the
//! tool-execution round-trip to the runtime, and the built-in plan/tasks tools.
//!
//! Permission dispatch (allow/ask/deny) relocated to the runtime (#59) — those
//! tests now live in `entanglement-runtime/tests/permission_dispatch.rs`. Core
//! only proves it hands *every* host tool to the runtime via `ToolExec` and
//! surfaces the returned `ToolResult` as `ToolOutput`.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use entanglement_core::{
    stream_from_response, AgentMode, AgentProfile, EngineConfig, Holly, InMsg, Llm, LlmRequest,
    LlmResponse, LlmSession, LlmStream, OutEvent, Permission, PermissionProfile, SessionId,
    ToolCall,
};

mod common;
use common::spawn_tool_executor;

/// Collect events for `sid` until `Done`, with a safety timeout.
async fn collect(
    mut sub: tokio::sync::broadcast::Receiver<OutEvent>,
    sid: &SessionId,
) -> Vec<OutEvent> {
    let mut out = Vec::new();
    loop {
        let Ok(recv) = tokio::time::timeout(Duration::from_secs(3), sub.recv()).await else {
            break;
        };
        match recv {
            Ok(ev) if ev.session() == sid => {
                let done = matches!(ev, OutEvent::Done { .. });
                out.push(ev);
                if done {
                    break;
                }
            }
            Ok(_) => {}
            Err(_) => break,
        }
    }
    out
}

/// Wait for the first event matching `pred`, tolerating broadcast lag and
/// events for other sessions. Used by the lifecycle tests, which watch for
/// point-in-time events (`SessionList`, `SessionEnded`) that carry no `Done`.
async fn recv_until(
    sub: &mut tokio::sync::broadcast::Receiver<OutEvent>,
    pred: impl Fn(&OutEvent) -> bool,
) -> OutEvent {
    loop {
        let recv = tokio::time::timeout(Duration::from_secs(3), sub.recv())
            .await
            .expect("timed out waiting for a matching event");
        match recv {
            Ok(ev) if pred(&ev) => return ev,
            Ok(_) => {}
            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
            Err(_) => panic!("event stream closed before a matching event"),
        }
    }
}

#[tokio::test]
async fn list_sessions_enumerates_live_sessions() {
    // Two live sessions plus a supervisor-global `ListSessions` query returns a
    // snapshot naming both, with lineage (ADR-0028). The correlation id echoes.
    let holly = Holly::spawn(factory(vec![]));
    let s1 = SessionId::new("s1");
    let s2 = SessionId::new("s2");
    let mut sub = holly.subscribe();
    for s in [&s1, &s2] {
        holly
            .send(InMsg::Prompt {
                session: s.clone(),
                text: "hi".into(),
            })
            .await
            .unwrap();
    }
    let corr = SessionId::new("query");
    holly
        .send(InMsg::ListSessions {
            session: corr.clone(),
        })
        .await
        .unwrap();

    let ev = recv_until(
        &mut sub,
        |e| matches!(e, OutEvent::SessionList { session, .. } if *session == corr),
    )
    .await;
    let OutEvent::SessionList { sessions, .. } = ev else {
        unreachable!()
    };
    let ids: Vec<_> = sessions.iter().map(|i| i.session.clone()).collect();
    assert!(ids.contains(&s1) && ids.contains(&s2), "got {ids:?}");
    let info = sessions.iter().find(|i| i.session == s1).unwrap();
    assert_eq!(info.profile, "build");
    assert!(info.root && info.parent.is_none());
}

#[tokio::test]
async fn close_session_terminates_and_drops_from_list() {
    // `CloseSession` destroys a live session: it emits `SessionEnded` and no
    // longer appears in a subsequent `ListSessions` snapshot (ADR-0028).
    let holly = Holly::spawn(factory(vec![]));
    let s1 = SessionId::new("s1");
    let mut sub = holly.subscribe();
    holly
        .send(InMsg::Prompt {
            session: s1.clone(),
            text: "hi".into(),
        })
        .await
        .unwrap();
    // Let the turn finish before closing, so `SessionEnded` follows `Done`.
    recv_until(
        &mut sub,
        |e| matches!(e, OutEvent::Done { session, .. } if *session == s1),
    )
    .await;
    holly
        .send(InMsg::CloseSession {
            session: s1.clone(),
        })
        .await
        .unwrap();
    recv_until(
        &mut sub,
        |e| matches!(e, OutEvent::SessionEnded { session, .. } if *session == s1),
    )
    .await;

    let corr = SessionId::new("query");
    holly
        .send(InMsg::ListSessions {
            session: corr.clone(),
        })
        .await
        .unwrap();
    let ev = recv_until(
        &mut sub,
        |e| matches!(e, OutEvent::SessionList { session, .. } if *session == corr),
    )
    .await;
    let OutEvent::SessionList { sessions, .. } = ev else {
        unreachable!()
    };
    assert!(
        !sessions.iter().any(|i| i.session == s1),
        "closed session must be gone from the list; got {sessions:?}"
    );
}

#[tokio::test]
async fn close_unknown_session_is_a_noop() {
    // Closing an id that was never live must not panic or emit `SessionEnded`.
    let holly = Holly::spawn(factory(vec![]));
    let ghost = SessionId::new("never-existed");
    let corr = SessionId::new("query");
    let mut sub = holly.subscribe();
    holly
        .send(InMsg::CloseSession {
            session: ghost.clone(),
        })
        .await
        .unwrap();
    holly
        .send(InMsg::ListSessions {
            session: corr.clone(),
        })
        .await
        .unwrap();
    // The `SessionList` reply proves the supervisor survived the no-op close.
    let ev = recv_until(
        &mut sub,
        |e| matches!(e, OutEvent::SessionList { session, .. } if *session == corr),
    )
    .await;
    let OutEvent::SessionList { sessions, .. } = ev else {
        unreachable!()
    };
    assert!(
        sessions.is_empty(),
        "no sessions should be live; got {sessions:?}"
    );
}

#[tokio::test]
async fn close_session_cascades_to_descendants() {
    // Closing a session retires its whole spawn sub-tree (#180): without a
    // cascade, spawned descendants keep running with no consumer for their
    // answers, burning provider tokens. Build parent → child → grandchild, close
    // the root, and assert every level ends and drops from the list.
    let holly = Holly::spawn(factory(vec![]));
    let parent = SessionId::new("parent");
    let child = SessionId::new("child");
    let grandchild = SessionId::new("grandchild");
    let mut sub = holly.subscribe();

    // Materialize the parent as a live root session.
    holly
        .send(InMsg::Prompt {
            session: parent.clone(),
            text: "hi".into(),
        })
        .await
        .unwrap();
    recv_until(
        &mut sub,
        |e| matches!(e, OutEvent::Done { session, .. } if *session == parent),
    )
    .await;

    // Spawn a child under the parent, then a grandchild under the child.
    holly
        .send(InMsg::Spawn {
            session: child.clone(),
            parent: parent.clone(),
            agent: "build".into(),
            prompt: "subtask".into(),
        })
        .await
        .unwrap();
    recv_until(
        &mut sub,
        |e| matches!(e, OutEvent::Done { session, .. } if *session == child),
    )
    .await;
    holly
        .send(InMsg::Spawn {
            session: grandchild.clone(),
            parent: child.clone(),
            agent: "build".into(),
            prompt: "sub-subtask".into(),
        })
        .await
        .unwrap();
    recv_until(
        &mut sub,
        |e| matches!(e, OutEvent::Done { session, .. } if *session == grandchild),
    )
    .await;

    // Closing the root must cascade to the entire sub-tree.
    holly
        .send(InMsg::CloseSession {
            session: parent.clone(),
        })
        .await
        .unwrap();

    let mut ended = std::collections::HashSet::new();
    while ended.len() < 3 {
        let ev = recv_until(&mut sub, |e| matches!(e, OutEvent::SessionEnded { .. })).await;
        if let OutEvent::SessionEnded { session, .. } = ev {
            ended.insert(session);
        }
    }
    assert!(
        ended.contains(&parent) && ended.contains(&child) && ended.contains(&grandchild),
        "cascade must end parent, child and grandchild; got {ended:?}"
    );

    // None of the sub-tree may survive in a subsequent list snapshot.
    let corr = SessionId::new("query");
    holly
        .send(InMsg::ListSessions {
            session: corr.clone(),
        })
        .await
        .unwrap();
    let ev = recv_until(
        &mut sub,
        |e| matches!(e, OutEvent::SessionList { session, .. } if *session == corr),
    )
    .await;
    let OutEvent::SessionList { sessions, .. } = ev else {
        unreachable!()
    };
    assert!(
        sessions.is_empty(),
        "cascade must drop every descendant from the list; got {sessions:?}"
    );
}

/// An LLM that replays a scripted list of responses, in order.
struct ScriptedLlm {
    responses: Mutex<Vec<LlmResponse>>,
}

impl ScriptedLlm {
    fn new(responses: Vec<LlmResponse>) -> Self {
        Self {
            responses: Mutex::new(responses),
        }
    }
}

#[async_trait]
impl Llm for ScriptedLlm {
    async fn stream(&mut self, _req: LlmRequest<'_>) -> anyhow::Result<LlmStream> {
        let resp = {
            let mut responses = self.responses.lock().unwrap();
            responses.pop().unwrap_or_else(|| LlmResponse {
                text: "ok".into(),
                tool_calls: vec![],
            })
        };
        Ok(stream_from_response(resp))
    }
}

fn factory(responses: Vec<LlmResponse>) -> EngineConfig {
    // Reverse because ScriptedLlm::complete pops from the back.
    let mut r = responses;
    r.reverse();
    let llm = Arc::new(ScriptedLlm::new(r));
    EngineConfig {
        llm_factory: Arc::new(move || {
            LlmSession::new(Box::new(ScriptedLlm::new(
                llm.responses.lock().unwrap().clone(),
            )))
        }),
        ..EngineConfig::default()
    }
}

#[tokio::test]
async fn dummy_turn_streams_text_and_done() {
    let holly = Holly::spawn(factory(vec![LlmResponse {
        text: "hello".into(),
        tool_calls: vec![],
    }]));
    let sid = SessionId::new("s1");
    let sub = holly.subscribe();
    holly
        .send(InMsg::Prompt {
            session: sid.clone(),
            text: "hi".into(),
        })
        .await
        .unwrap();
    let events = collect(sub, &sid).await;

    assert!(events
        .iter()
        .any(|e| matches!(e, OutEvent::TextDelta { text, .. } if text == "hello")));
    assert!(events.iter().any(|e| matches!(e, OutEvent::Done { .. })));
}

#[tokio::test]
async fn every_host_tool_round_trips_through_toolexec() {
    // Core relocated execution (#58) and permission dispatch (#59): every host
    // tool is emitted as a ToolExec (core no longer decides Allow/Ask/Deny, so
    // never a ToolRequest), and the runtime executor's output must surface as
    // ToolOutput. A modeled `echo` tool proves the value round-trips.
    let holly = Holly::spawn(factory(vec![LlmResponse {
        text: "".into(),
        tool_calls: vec![ToolCall {
            id: "t1".into(),
            name: "echo".into(),
            input: "ping".into(),
        }],
    }]));
    spawn_tool_executor(&holly, |name, input| match name {
        "echo" => format!("echoed: {input}"),
        other => format!("unknown tool: `{other}`"),
    });
    let sid = SessionId::new("s1");
    let sub = holly.subscribe();
    holly
        .send(InMsg::Prompt {
            session: sid.clone(),
            text: "go".into(),
        })
        .await
        .unwrap();
    let events = collect(sub, &sid).await;

    assert!(
        events.iter().any(|e| matches!(
            e,
            OutEvent::ToolExec { tool, request_id, .. } if tool == "echo" && request_id == "t1"
        )),
        "Allow tool should be handed to the runtime via ToolExec; got {events:?}"
    );
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, OutEvent::ToolRequest { .. })),
        "Allow must not ask for approval"
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, OutEvent::ToolOutput { output, .. } if output == "echoed: ping")),
        "executor output should surface as ToolOutput; got {events:?}"
    );
}

#[tokio::test]
async fn update_plan_and_update_tasks_round_trip_as_tool_exec() {
    // `update_plan`/`update_tasks` are runtime state tools now (#231, ADR-0049):
    // core no longer has built-ins for them. A call takes the ordinary #58
    // round-trip — core emits `ToolExec` and parks — so with no runtime executor
    // wired in, the engine emits the request and produces no `Plan`/`TaskList`
    // of its own (those are the runtime's to emit).
    let holly = Holly::spawn(factory(vec![LlmResponse {
        text: "".into(),
        tool_calls: vec![ToolCall {
            id: "t1".into(),
            name: "update_tasks".into(),
            input: r#"{"content":"- [ ] x"}"#.into(),
        }],
    }]));
    let sid = SessionId::new("s1");
    let mut sub = holly.subscribe();
    holly
        .send(InMsg::Prompt {
            session: sid.clone(),
            text: "track".into(),
        })
        .await
        .unwrap();

    // Await the ToolExec round-trip rather than a Done: the turn parks on the
    // tool result, which never arrives without a runtime executor.
    let mut saw_tool_exec = false;
    while let Ok(Ok(ev)) =
        tokio::time::timeout(std::time::Duration::from_millis(500), sub.recv()).await
    {
        if ev.session() != &sid {
            continue;
        }
        match ev {
            OutEvent::ToolExec { tool, .. } if tool == "update_tasks" => {
                saw_tool_exec = true;
                break;
            }
            // Core must not emit plan/task snapshots itself anymore.
            OutEvent::Plan { .. } | OutEvent::TaskList { .. } => {
                panic!("core must not emit Plan/TaskList; got {ev:?}")
            }
            _ => {}
        }
    }
    assert!(
        saw_tool_exec,
        "update_tasks must round-trip to the runtime via ToolExec"
    );
}

#[tokio::test]
async fn set_agent_emits_agent_changed() {
    // Core carries only the `build` built-in (#201); the runtime owns the
    // plan/explore trio. Register a second profile here to exercise the switch.
    let mut cfg = EngineConfig::default();
    cfg.profiles.insert(AgentProfile {
        name: "reviewer".into(),
        description: String::new(),
        mode: AgentMode::Primary,
        system_prompt: "Review the changes.".into(),
        model: None,
        permission: PermissionProfile::new(Permission::Ask),
        tools: None,
        disallowed_tools: Vec::new(),
        can_spawn: None,
        spawnable_agents: None,
    });
    let holly = Holly::spawn(cfg);
    let sid = SessionId::new("s1");
    let mut sub = holly.subscribe();
    holly
        .send(InMsg::SetAgent {
            session: sid.clone(),
            agent: "reviewer".into(),
        })
        .await
        .unwrap();

    let mut saw_build = false;
    let mut saw_reviewer = false;
    while let Ok(ev) = tokio::time::timeout(Duration::from_secs(2), sub.recv()).await {
        if let Ok(OutEvent::AgentChanged { agent, .. }) = &ev {
            if agent == "build" {
                saw_build = true;
            }
            if agent == "reviewer" {
                saw_reviewer = true;
            }
        }
        if saw_reviewer {
            break;
        }
    }
    assert!(saw_build, "session should start under build");
    assert!(saw_reviewer, "should switch to reviewer");
}

#[tokio::test]
async fn two_sessions_are_independent() {
    let holly = Holly::spawn(factory(vec![
        LlmResponse {
            text: "from-s1".into(),
            tool_calls: vec![],
        },
        LlmResponse {
            text: "from-s2".into(),
            tool_calls: vec![],
        },
    ]));
    let s1 = SessionId::new("s1");
    let s2 = SessionId::new("s2");
    let sub1 = holly.subscribe();
    holly
        .send(InMsg::Prompt {
            session: s1.clone(),
            text: "hi".into(),
        })
        .await
        .unwrap();
    holly
        .send(InMsg::Prompt {
            session: s2.clone(),
            text: "hi".into(),
        })
        .await
        .unwrap();

    let e1 = collect(sub1, &s1).await;
    assert!(e1
        .iter()
        .any(|e| matches!(e, OutEvent::TextDelta { text, .. } if text == "from-s1")));
    // s1's events should not contain s2's text.
    assert!(!e1
        .iter()
        .any(|e| matches!(e, OutEvent::TextDelta { text, .. } if text == "from-s2")));
}

#[tokio::test]
async fn spawn_starts_child_with_parent_link() {
    // A `Spawn` starts a child session that runs its prompt and whose
    // `SessionStarted` carries the parent (populating the session tree #60).
    let holly = Holly::spawn(factory(vec![LlmResponse {
        text: "child answer".into(),
        tool_calls: vec![],
    }]));
    let parent = SessionId::new("parent");
    let child = SessionId::new("child");
    let sub = holly.subscribe();
    holly
        .send(InMsg::Spawn {
            session: child.clone(),
            parent: parent.clone(),
            agent: "build".into(),
            prompt: "do the subtask".into(),
        })
        .await
        .unwrap();
    let events = collect(sub, &child).await;

    assert!(
        events.iter().any(|e| matches!(
            e,
            OutEvent::SessionStarted { parent: Some(p), root: false, .. } if *p == parent
        )),
        "child SessionStarted must carry the parent link; got {events:?}"
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, OutEvent::TextDelta { text, .. } if text == "child answer")),
        "child should run its prompt; got {events:?}"
    );
    assert!(events.iter().any(|e| matches!(e, OutEvent::Done { .. })));
}

#[tokio::test]
async fn spawn_of_unknown_agent_errors_instead_of_falling_back_to_build() {
    // An unknown spawn target must not silently escalate to `build` (#119): the
    // supervisor emits an Error and starts no child.
    let holly = Holly::spawn(factory(vec![LlmResponse {
        text: "unused".into(),
        tool_calls: vec![],
    }]));
    let child = SessionId::new("ghost-child");
    let mut sub = holly.subscribe();
    holly
        .send(InMsg::Spawn {
            session: child.clone(),
            parent: SessionId::new("parent"),
            agent: "does-not-exist".into(),
            prompt: "go".into(),
        })
        .await
        .unwrap();

    let mut saw_error = false;
    let mut saw_start = false;
    while let Ok(Ok(ev)) = tokio::time::timeout(Duration::from_secs(2), sub.recv()).await {
        match &ev {
            OutEvent::Error {
                session, message, ..
            } if session == &child && message.contains("unknown agent profile") => {
                saw_error = true;
                break;
            }
            OutEvent::SessionStarted { session, .. } if session == &child => saw_start = true,
            _ => {}
        }
    }
    assert!(saw_error, "an unknown spawn target should emit an Error");
    assert!(
        !saw_start,
        "no child session should start for an unknown target"
    );
}

#[tokio::test]
async fn duplicate_spawn_is_ignored() {
    // A second `Spawn` for a live child id is a no-op (one child, one start).
    let holly = Holly::spawn(factory(vec![
        LlmResponse {
            text: "first".into(),
            tool_calls: vec![],
        },
        LlmResponse {
            text: "second".into(),
            tool_calls: vec![],
        },
    ]));
    let child = SessionId::new("child");
    let mut sub = holly.subscribe();
    for _ in 0..2 {
        holly
            .send(InMsg::Spawn {
                session: child.clone(),
                parent: SessionId::new("parent"),
                agent: "build".into(),
                prompt: "go".into(),
            })
            .await
            .unwrap();
    }

    let mut starts = 0;
    while let Ok(Ok(ev)) = tokio::time::timeout(Duration::from_secs(2), sub.recv()).await {
        if let OutEvent::SessionStarted { session, .. } = &ev {
            if session == &child {
                starts += 1;
            }
        }
        if matches!(&ev, OutEvent::Done { session, .. } if session == &child) {
            break;
        }
    }
    assert_eq!(starts, 1, "duplicate spawn must not start a second child");
}

#[tokio::test]
async fn custom_profile_is_selectable() {
    let mut cfg = EngineConfig::default();
    cfg.profiles.insert(AgentProfile {
        name: "paranoid".into(),
        description: String::new(),
        mode: AgentMode::Primary,
        system_prompt: "Ask before anything.".into(),
        model: None,
        permission: PermissionProfile::new(Permission::Ask),
        tools: None,
        disallowed_tools: Vec::new(),
        can_spawn: None,
        spawnable_agents: None,
    });
    let holly = Holly::spawn(cfg);
    let sid = SessionId::new("s1");
    let mut sub = holly.subscribe();
    holly
        .send(InMsg::SetAgent {
            session: sid.clone(),
            agent: "paranoid".into(),
        })
        .await
        .unwrap();

    let mut ok = false;
    while let Ok(ev) = tokio::time::timeout(Duration::from_secs(2), sub.recv()).await {
        if let Ok(OutEvent::AgentChanged { agent, .. }) = ev {
            if agent == "paranoid" {
                ok = true;
                break;
            }
        }
    }
    assert!(ok);
}
