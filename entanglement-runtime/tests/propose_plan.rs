//! Integration tests for the runtime-owned `propose_plan` tool (#141, ADR-0042;
//! #513, ADR-0145, amending ADR-0138).
//!
//! The model calls `propose_plan(content XOR path)`; the executor intercepts it
//! on `ToolExec` (before permission resolution, like `ask_user`) and
//! **force-parks it on the `Ask` path unconditionally** — a `ToolRequest` is
//! emitted even under an all-`Allow` profile, *unless* the call is malformed
//! (both/neither of `content`/`path`, a missing/non-`.md` file, or a stale
//! `path`), which replies immediately with no prompt at all. Per ADR-0145:
//!
//! - **Approve** spawns a sponsored `build` child of the plan session. The plan
//!   session parks on `WaitingAgent`; the build child runs under the `build`
//!   profile and its answer folds back as the `propose_plan` tool result (named
//!   with the plan file's location), so the plan agent can cycle. The build
//!   child also receives an `OutEvent::Plan` snapshot of the accepted plan.
//! - **Reject** folds the typed reason back, unchanged, still naming the file.
//! - **Stop** while parked on the blocking build wait detaches by default: the
//!   plan session's wait is cancelled with no reply owed, and the build child
//!   keeps running untouched. A head wanting the child stopped too sends it an
//!   explicit second `Stop` ("cascade").

use std::path::Path;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use async_trait::async_trait;
use entanglement_core::{
    stream_from_response, EngineConfig, Holly, InMsg, Llm, LlmRequest, LlmResponse, LlmStream,
    OutEvent, SessionId, ToolCall,
};
use entanglement_runtime::extra_roots::ExtraRootStore;
use entanglement_runtime::hooks::Hooks;
use entanglement_runtime::host::host_tools_with_extra_roots;
use entanglement_runtime::plan_files::PlanFileRegistry;
use entanglement_runtime::policy::{DefaultGrantStore, ProfileResolver, SandboxConfig};
use entanglement_runtime::skills::SkillRegistry;
use entanglement_runtime::tool_names::PROPOSE_PLAN_TOOL;
use entanglement_runtime::tool_runner::{spawn_tool_executor_with_policy, EscapeRoot};

/// Replays scripted responses in order, then plain text so the turn terminates.
struct ScriptedLlm {
    responses: Mutex<Vec<LlmResponse>>,
}
impl ScriptedLlm {
    fn new(mut responses: Vec<LlmResponse>) -> Self {
        responses.reverse();
        Self {
            responses: Mutex::new(responses),
        }
    }
}
#[async_trait]
impl Llm for ScriptedLlm {
    async fn stream(&mut self, _req: LlmRequest<'_>) -> anyhow::Result<LlmStream> {
        let resp = self
            .responses
            .lock()
            .unwrap()
            .pop()
            .unwrap_or_else(|| LlmResponse {
                text: "done".into(),
                tool_calls: vec![],
            });
        Ok(stream_from_response(resp))
    }
}

/// Like [`ScriptedLlm`] but sleeps `delay` before its first response, so a test
/// has a reliable window to act (e.g. send `Stop`) while a sponsored build
/// child is still in flight instead of racing its instant completion.
struct SlowScriptedLlm {
    responses: Mutex<Vec<LlmResponse>>,
    delay: Duration,
}
#[async_trait]
impl Llm for SlowScriptedLlm {
    async fn stream(&mut self, _req: LlmRequest<'_>) -> anyhow::Result<LlmStream> {
        tokio::time::sleep(self.delay).await;
        let resp = self
            .responses
            .lock()
            .unwrap()
            .pop()
            .unwrap_or_else(|| LlmResponse {
                text: "done".into(),
                tool_calls: vec![],
            });
        Ok(stream_from_response(resp))
    }
}

fn tempdir() -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix("entanglement-propose-plan-it-")
        .tempdir()
        .unwrap()
}

fn propose_plan_call(id: &str, input: serde_json::Value) -> LlmResponse {
    LlmResponse {
        text: "".into(),
        tool_calls: vec![ToolCall {
            id: id.into(),
            name: PROPOSE_PLAN_TOOL.into(),
            input: input.to_string(),
            provider_meta: None,
        }],
    }
}

fn text_response(text: &str) -> LlmResponse {
    LlmResponse {
        text: text.into(),
        tool_calls: vec![],
    }
}

/// Spawn a `Holly` + real tool executor rooted at `root`, with the given
/// per-session `llm_factory`. Mirrors `tests/rhai.rs`'s `spawn_with_rhai_escape`
/// harness: a real `EscapeRoot` (so `propose_plan` can resolve a project root)
/// and the root-contained host registry (so a `write`/`edit` a test scripts
/// against the bound plan file actually lands and fires `FileChange`).
fn spawn_with_root(root: &Path, llm_factory: Arc<dyn Fn() -> Box<dyn Llm> + Send + Sync>) -> Holly {
    let profiles =
        entanglement_runtime::agents::built_in_registry().expect("built-in agents must parse");
    let cfg = EngineConfig {
        llm_factory,
        profiles: profiles.clone(),
        ..EngineConfig::default()
    };
    let holly = Holly::spawn(cfg);
    let store = Arc::new(ExtraRootStore::ephemeral());
    let tools = host_tools_with_extra_roots(root.to_path_buf(), Some(store.clone()));
    let base = entanglement_core::PermissionProfile::new(entanglement_core::Permission::Allow);
    let active = Arc::new(Mutex::new(std::collections::HashMap::new()));
    let resolver = Arc::new(ProfileResolver::new(
        active.clone(),
        base.clone(),
        Some(root.to_path_buf()),
    ));
    let grants = Arc::new(DefaultGrantStore::load());
    let escape_root = EscapeRoot {
        root: root.to_path_buf(),
        store,
    };
    let _executor = spawn_tool_executor_with_policy(
        &holly,
        tools.shared(),
        entanglement_runtime::host::jobs::JobRegistry::new(),
        entanglement_runtime::retained_output::RetainedOutputRegistry::new(),
        Arc::new(RwLock::new(profiles)),
        Arc::new(RwLock::new(Arc::new(SkillRegistry::default()))),
        base,
        active,
        resolver,
        grants,
        Hooks::default(),
        Some(escape_root),
        SandboxConfig::none(),
        Arc::new(PlanFileRegistry::new()),
    );
    holly
}

/// The request must surface as a `ToolRequest` even though `build` is an
/// all-`Allow` profile — proving `propose_plan` force-parks regardless.
async fn await_request(holly: &Holly, sid: &SessionId) -> String {
    let mut watch = holly.subscribe();
    holly.send(InMsg::prompt(sid.clone(), "go")).await.unwrap();
    while let Ok(Ok(ev)) = tokio::time::timeout(Duration::from_secs(2), watch.recv()).await {
        if let OutEvent::ToolRequest {
            request_id, tool, ..
        } = &ev
        {
            assert_eq!(tool, PROPOSE_PLAN_TOOL);
            return request_id.clone();
        }
    }
    panic!("expected a ToolRequest for propose_plan under an Allow profile");
}

/// Collect events for `sid` until its turn ends (`Done`), with a bounded
/// per-event timeout.
async fn collect_until_done(
    sub: &mut tokio::sync::broadcast::Receiver<OutEvent>,
    sid: &SessionId,
    timeout: Duration,
) -> Vec<OutEvent> {
    let mut events = Vec::new();
    while let Ok(Ok(ev)) = tokio::time::timeout(timeout, sub.recv()).await {
        let done = matches!(&ev, OutEvent::Done { session, .. } if session == sid);
        if ev.session() == Some(sid) {
            events.push(ev);
        }
        if done {
            break;
        }
    }
    events
}

#[tokio::test]
async fn approve_spawns_sponsored_build_and_folds_answer_back() {
    let dir = tempdir();
    let root = dir.path();
    let plan_responses = vec![
        propose_plan_call("p1", serde_json::json!({"content": "# Ship it"})),
        text_response("plan agent acknowledged build result"),
    ];
    let build_responses = vec![text_response("build child finished the work")];
    let first = Arc::new(Mutex::new(true));
    let plan_responses = Arc::new(plan_responses);
    let build_responses = Arc::new(build_responses);
    let holly = spawn_with_root(
        root,
        Arc::new(move || {
            let is_first = {
                let mut g = first.lock().unwrap();
                let was = *g;
                *g = false;
                was
            };
            if is_first {
                Box::new(ScriptedLlm::new((*plan_responses).clone())) as Box<dyn Llm>
            } else {
                Box::new(ScriptedLlm::new((*build_responses).clone())) as Box<dyn Llm>
            }
        }),
    );
    let sid = SessionId::new("s1");
    let mut sub = holly.subscribe();
    let request_id = await_request(&holly, &sid).await;

    holly
        .send(InMsg::Approve {
            session: sid.clone(),
            request_id,
            scope: Default::default(),
        })
        .await
        .unwrap();

    let mut saw_waiting_agent = false;
    let mut saw_build_plan = false;
    let mut saw_build_text = false;
    let mut got_output = false;
    let mut build_session: Option<SessionId> = None;
    let mut pending_plan_for_build: Option<String> = None;
    while let Ok(Ok(ev)) = tokio::time::timeout(Duration::from_secs(5), sub.recv()).await {
        match &ev {
            OutEvent::Status {
                session,
                state: entanglement_core::AgentState::WaitingAgent,
                ..
            } if session == &sid => {
                saw_waiting_agent = true;
            }
            OutEvent::SessionStarted {
                session,
                parent: Some(p),
                profile,
                ..
            } if p == &sid && profile == "build" => {
                build_session = Some(session.clone());
            }
            OutEvent::Plan {
                session, content, ..
            } => {
                if build_session.as_ref() == Some(session) {
                    assert!(content.contains("Ship it"), "plan content: {content}");
                    saw_build_plan = true;
                } else if content.contains("Ship it") {
                    pending_plan_for_build = Some(content.clone());
                }
            }
            OutEvent::TextDelta { session, text, .. } => {
                if build_session.as_ref() == Some(session) && text.contains("build child finished")
                {
                    saw_build_text = true;
                    if let Some(c) = &pending_plan_for_build {
                        assert!(c.contains("Ship it"));
                        saw_build_plan = true;
                    }
                }
            }
            OutEvent::ToolOutput {
                session,
                tool,
                output,
                ..
            } if session == &sid && tool == PROPOSE_PLAN_TOOL => {
                assert!(
                    output.contains("completed in"),
                    "approve must fold the build result back: {output}"
                );
                assert!(
                    output.contains("build child finished the work"),
                    "the build child's answer must reach the plan agent: {output}"
                );
                assert!(
                    output.contains(".entanglement/plans/s1.md"),
                    "the tool result must name the plan file's location (#513): {output}"
                );
                if let Some(child) = &build_session {
                    assert!(
                        output.contains(&child.to_string()),
                        "the tool result must name the build child's agent_id \
                         (#609, ADR-0162 §5), so the plan agent can agent_send \
                         it another round: {output}"
                    );
                }
                got_output = true;
            }
            OutEvent::Done { session, .. } if session == &sid => break,
            _ => {}
        }
    }
    assert!(saw_waiting_agent, "plan session must park on WaitingAgent");
    assert!(
        build_session.is_some(),
        "a sponsored build child session must be spawned"
    );
    assert!(saw_build_plan, "build child must receive a Plan snapshot");
    assert!(saw_build_text, "build child's text must stream");
    assert!(
        got_output,
        "approve must fold the build's answer back as the propose_plan tool result"
    );
    assert_eq!(
        std::fs::read_to_string(root.join(".entanglement/plans/s1.md")).unwrap(),
        "# Ship it"
    );
}

#[tokio::test]
async fn reject_folds_reason_and_records_no_plan() {
    let dir = tempdir();
    let root = dir.path();
    let scripted = Arc::new(vec![
        propose_plan_call("p1", serde_json::json!({"content": "# Draft"})),
        text_response("revised"),
    ]);
    let holly = spawn_with_root(
        root,
        Arc::new(move || Box::new(ScriptedLlm::new((*scripted).clone())) as Box<dyn Llm>),
    );
    let sid = SessionId::new("s1");
    let mut sub = holly.subscribe();
    let request_id = await_request(&holly, &sid).await;

    holly
        .send(InMsg::Reject {
            session: sid.clone(),
            request_id,
            reason: Some("needs more detail on migrations".into()),
        })
        .await
        .unwrap();

    let mut got_output = false;
    while let Ok(Ok(ev)) = tokio::time::timeout(Duration::from_secs(3), sub.recv()).await {
        if ev.session() != Some(&sid) {
            continue;
        }
        match &ev {
            OutEvent::ToolOutput { tool, output, .. } if tool == PROPOSE_PLAN_TOOL => {
                assert!(
                    output.contains("needs more detail on migrations"),
                    "reject must fold the typed reason back: {output}"
                );
                assert!(
                    output.contains(".entanglement/plans/s1.md"),
                    "reject must still name the plan file: {output}"
                );
                got_output = true;
            }
            OutEvent::Done { .. } => break,
            _ => {}
        }
    }
    assert!(got_output, "reject must fold a ToolOutput back");
    // The plan file was still materialized (content-mode always writes) —
    // rejection is about the *proposal*, not the file (last writer wins).
    assert_eq!(
        std::fs::read_to_string(root.join(".entanglement/plans/s1.md")).unwrap(),
        "# Draft"
    );
}

#[tokio::test]
async fn both_content_and_path_is_refused_with_no_approval_prompt() {
    let dir = tempdir();
    let root = dir.path();
    let scripted = Arc::new(vec![
        propose_plan_call("p1", serde_json::json!({"content": "# X", "path": "a.md"})),
        text_response("ok"),
    ]);
    let holly = spawn_with_root(
        root,
        Arc::new(move || Box::new(ScriptedLlm::new((*scripted).clone())) as Box<dyn Llm>),
    );
    let sid = SessionId::new("s1");
    let mut sub = holly.subscribe();
    holly.send(InMsg::prompt(sid.clone(), "go")).await.unwrap();
    let events = collect_until_done(&mut sub, &sid, Duration::from_secs(3)).await;

    assert!(
        !events
            .iter()
            .any(|e| matches!(e, OutEvent::ToolRequest { .. })),
        "a malformed call must never force an approval prompt: {events:?}"
    );
    assert!(
        events.iter().any(|e| matches!(
            e,
            OutEvent::ToolOutput { tool, output, .. }
                if tool == PROPOSE_PLAN_TOOL && output.contains("exactly one")
        )),
        "both content and path must be refused: {events:?}"
    );
}

#[tokio::test]
async fn neither_content_nor_path_is_refused_with_no_approval_prompt() {
    let dir = tempdir();
    let root = dir.path();
    let scripted = Arc::new(vec![
        propose_plan_call("p1", serde_json::json!({})),
        text_response("ok"),
    ]);
    let holly = spawn_with_root(
        root,
        Arc::new(move || Box::new(ScriptedLlm::new((*scripted).clone())) as Box<dyn Llm>),
    );
    let sid = SessionId::new("s1");
    let mut sub = holly.subscribe();
    holly.send(InMsg::prompt(sid.clone(), "go")).await.unwrap();
    let events = collect_until_done(&mut sub, &sid, Duration::from_secs(3)).await;

    assert!(
        !events
            .iter()
            .any(|e| matches!(e, OutEvent::ToolRequest { .. })),
        "a malformed call must never force an approval prompt: {events:?}"
    );
    assert!(
        events.iter().any(|e| matches!(
            e,
            OutEvent::ToolOutput { tool, output, .. }
                if tool == PROPOSE_PLAN_TOOL && output.contains("exactly one")
        )),
        "neither content nor path must be refused: {events:?}"
    );
}

#[tokio::test]
async fn path_to_a_missing_file_is_refused_with_no_approval_prompt() {
    let dir = tempdir();
    let root = dir.path();
    let scripted = Arc::new(vec![
        propose_plan_call(
            "p1",
            serde_json::json!({"path": ".entanglement/plans/missing.md"}),
        ),
        text_response("ok"),
    ]);
    let holly = spawn_with_root(
        root,
        Arc::new(move || Box::new(ScriptedLlm::new((*scripted).clone())) as Box<dyn Llm>),
    );
    let sid = SessionId::new("s1");
    let mut sub = holly.subscribe();
    holly.send(InMsg::prompt(sid.clone(), "go")).await.unwrap();
    let events = collect_until_done(&mut sub, &sid, Duration::from_secs(3)).await;

    assert!(
        !events
            .iter()
            .any(|e| matches!(e, OutEvent::ToolRequest { .. })),
        "a missing file must never force an approval prompt: {events:?}"
    );
    assert!(
        events.iter().any(|e| matches!(
            e,
            OutEvent::ToolOutput { tool, output, .. }
                if tool == PROPOSE_PLAN_TOOL && output.contains("not found")
        )),
        "a missing plan file must be refused: {events:?}"
    );
}

#[tokio::test]
async fn path_binds_an_existing_file_seeded_by_the_user() {
    // #514's seeding story: a plan file the user (or a prior session) already
    // wrote, submitted via `path` with no prior binding — never stale.
    let dir = tempdir();
    let root = dir.path();
    std::fs::create_dir_all(root.join(".entanglement/plans")).unwrap();
    std::fs::write(
        root.join(".entanglement/plans/seed.md"),
        "# Seeded by the user",
    )
    .unwrap();
    let scripted = Arc::new(vec![
        propose_plan_call(
            "p1",
            serde_json::json!({"path": ".entanglement/plans/seed.md"}),
        ),
        text_response("revised"),
    ]);
    let holly = spawn_with_root(
        root,
        Arc::new(move || Box::new(ScriptedLlm::new((*scripted).clone())) as Box<dyn Llm>),
    );
    let sid = SessionId::new("s1");
    let mut sub = holly.subscribe();
    let request_id = await_request(&holly, &sid).await;
    // Reject — we only care that the seeded file was accepted (no staleness
    // error), not the sponsored build path (covered elsewhere).
    holly
        .send(InMsg::Reject {
            session: sid.clone(),
            request_id,
            reason: Some("not yet".into()),
        })
        .await
        .unwrap();
    let events = collect_until_done(&mut sub, &sid, Duration::from_secs(3)).await;
    assert!(
        events.iter().any(|e| matches!(
            e,
            OutEvent::Plan { content, .. } if content == "# Seeded by the user"
        )),
        "the seeded file's content must resolve: {events:?}"
    );
}

#[tokio::test]
async fn a_path_resubmit_changed_out_of_band_is_refused_as_stale() {
    let dir = tempdir();
    let root = dir.path().to_path_buf();
    // A per-call delay (not just the first response) so there's a reliable
    // window between the reject's `ToolOutput` and the model's *next* turn
    // (p2's dispatch) for the "user" to edit the file — the turn loop
    // otherwise continues to the next tool call as fast as the executor can
    // schedule it, racing ahead of a synchronous `std::fs::write` in the test.
    // Only one session (the plan session itself) is created in this test, so
    // the factory runs once.
    let holly = spawn_with_root(
        &root,
        Arc::new(move || {
            Box::new(SlowScriptedLlm {
                responses: Mutex::new(vec![
                    text_response("done"),
                    propose_plan_call(
                        "p2",
                        serde_json::json!({"path": ".entanglement/plans/s1.md"}),
                    ),
                    propose_plan_call("p1", serde_json::json!({"content": "# v1"})),
                ]),
                delay: Duration::from_millis(300),
            }) as Box<dyn Llm>
        }),
    );
    let sid = SessionId::new("s1");
    let mut sub = holly.subscribe();
    let request_id = await_request(&holly, &sid).await;
    // Reject the first proposal — the file is now materialized and bound.
    holly
        .send(InMsg::Reject {
            session: sid.clone(),
            request_id,
            reason: Some("not yet".into()),
        })
        .await
        .unwrap();
    // Wait for the reject's ToolOutput before the "user" edits the file, so
    // the edit is provably ordered after the session's own materialize.
    while let Ok(Ok(ev)) = tokio::time::timeout(Duration::from_secs(3), sub.recv()).await {
        if let OutEvent::ToolOutput { tool, .. } = &ev {
            if tool == PROPOSE_PLAN_TOOL {
                break;
            }
        }
    }
    // The "user" edits the plan file directly — bypassing every tool the
    // runtime would see execute, so `plan_files` never learns of it. The
    // model's next turn (p2) is still sleeping in `SlowScriptedLlm`, so this
    // is provably ordered before p2's dispatch reads the file.
    std::fs::write(
        root.join(".entanglement/plans/s1.md"),
        "# v2, edited by the user",
    )
    .unwrap();

    let events = collect_until_done(&mut sub, &sid, Duration::from_secs(3)).await;
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, OutEvent::ToolRequest { .. })),
        "a stale resubmit must never force an approval prompt: {events:?}"
    );
    assert!(
        events.iter().any(|e| matches!(
            e,
            OutEvent::ToolOutput { tool, output, .. }
                if tool == PROPOSE_PLAN_TOOL && output.contains("changed since")
        )),
        "the second propose_plan(path) must be refused as stale: {events:?}"
    );
}

#[tokio::test]
async fn a_path_resubmit_after_the_agents_own_edit_succeeds() {
    // The intended review loop: the plan agent edits the bound file directly
    // (via `write`) between phases — that must never trip the staleness guard,
    // since the `FileChange` audit keeps `plan_files` in sync with it.
    let dir = tempdir();
    let root = dir.path().to_path_buf();
    let scripted = Arc::new(vec![
        propose_plan_call("p1", serde_json::json!({"content": "# v1\n- [ ] a"})),
        LlmResponse {
            text: "".into(),
            tool_calls: vec![ToolCall {
                id: "w1".into(),
                name: "write".into(),
                input: serde_json::json!({
                    "path": ".entanglement/plans/s1.md",
                    "content": "# v1\n- [x] a",
                })
                .to_string(),
                provider_meta: None,
            }],
        },
        propose_plan_call(
            "p2",
            serde_json::json!({"path": ".entanglement/plans/s1.md"}),
        ),
        text_response("done"),
    ]);
    let holly = spawn_with_root(
        &root,
        Arc::new(move || Box::new(ScriptedLlm::new((*scripted).clone())) as Box<dyn Llm>),
    );
    let sid = SessionId::new("s1");
    let mut sub = holly.subscribe();
    let request_id = await_request(&holly, &sid).await;
    holly
        .send(InMsg::Reject {
            session: sid.clone(),
            request_id,
            reason: Some("not yet".into()),
        })
        .await
        .unwrap();

    // The second propose_plan (after the agent's own `write`) must still force
    // an approval prompt — proving it was accepted, not refused as stale.
    let mut second_request = None;
    while let Ok(Ok(ev)) = tokio::time::timeout(Duration::from_secs(3), sub.recv()).await {
        if let OutEvent::ToolRequest {
            request_id, input, ..
        } = &ev
        {
            if input.contains("[x] a") {
                second_request = Some(request_id.clone());
                break;
            }
        }
    }
    assert!(
        second_request.is_some(),
        "the agent's own edit must not trip the staleness guard"
    );
    holly
        .send(InMsg::Reject {
            session: sid.clone(),
            request_id: second_request.unwrap(),
            reason: Some("not yet either".into()),
        })
        .await
        .unwrap();
    let events = collect_until_done(&mut sub, &sid, Duration::from_secs(3)).await;
    assert!(
        !events.iter().any(|e| matches!(
            e,
            OutEvent::ToolOutput { tool, output, .. }
                if tool == PROPOSE_PLAN_TOOL && output.contains("changed since")
        )),
        "must not be refused as stale: {events:?}"
    );
}

#[tokio::test]
async fn every_phase_re_parks_on_ask_independently() {
    // Multi-phase plan -> build -> review loop: each propose_plan call force-
    // parks on its own approval, even though the profile is Allow-all.
    let dir = tempdir();
    let root = dir.path().to_path_buf();
    let plan_responses = Arc::new(vec![
        propose_plan_call(
            "p1",
            serde_json::json!({"content": "# Plan\n- [ ] 1\n- [ ] 2"}),
        ),
        propose_plan_call(
            "p2",
            serde_json::json!({"path": ".entanglement/plans/s1.md"}),
        ),
        text_response("both phases done"),
    ]);
    let build_responses = Arc::new(vec![text_response("phase built")]);
    let first = Arc::new(Mutex::new(true));
    let holly = spawn_with_root(&root, {
        let plan_responses = plan_responses.clone();
        let build_responses = build_responses.clone();
        Arc::new(move || {
            let is_first = {
                let mut g = first.lock().unwrap();
                let was = *g;
                *g = false;
                was
            };
            if is_first {
                Box::new(ScriptedLlm::new((*plan_responses).clone())) as Box<dyn Llm>
            } else {
                Box::new(ScriptedLlm::new((*build_responses).clone())) as Box<dyn Llm>
            }
        })
    });
    let sid = SessionId::new("s1");
    let mut sub = holly.subscribe();
    let request_id_1 = await_request(&holly, &sid).await;
    holly
        .send(InMsg::Approve {
            session: sid.clone(),
            request_id: request_id_1.clone(),
            scope: Default::default(),
        })
        .await
        .unwrap();

    // Collect every event through to `Done`, approving the *second*
    // `ToolRequest` inline as soon as it arrives — proof the tool re-parked on
    // Ask for phase 2 instead of running unattended under the Allow-all
    // profile. `sub` was subscribed before `await_request` (which uses its own
    // separate subscription internally), so it still carries a queued copy of
    // the *first* `ToolRequest` too — skip it explicitly by id rather than
    // matching by variant alone. Unlike `collect_until_done`, this collects
    // from the start so both phases' `ToolOutput`s are captured, not just the
    // second (a fresh collect after approving would miss the first).
    let mut events = Vec::new();
    let mut approved_second = false;
    while let Ok(Ok(ev)) = tokio::time::timeout(Duration::from_secs(5), sub.recv()).await {
        if let OutEvent::ToolRequest { request_id, .. } = &ev {
            if *request_id != request_id_1 && !approved_second {
                approved_second = true;
                holly
                    .send(InMsg::Approve {
                        session: sid.clone(),
                        request_id: request_id.clone(),
                        scope: Default::default(),
                    })
                    .await
                    .unwrap();
            }
        }
        let done = matches!(&ev, OutEvent::Done { session, .. } if session == &sid);
        if ev.session() == Some(&sid) {
            events.push(ev);
        }
        if done {
            break;
        }
    }
    let outputs: Vec<_> = events
        .iter()
        .filter(|e| matches!(e, OutEvent::ToolOutput { tool, .. } if tool == PROPOSE_PLAN_TOOL))
        .collect();
    assert_eq!(
        outputs.len(),
        2,
        "each phase must fold its own tool result back: {events:?}"
    );
}

#[tokio::test]
async fn stop_on_the_plan_session_detaches_the_build_child_which_keeps_running() {
    let dir = tempdir();
    let root = dir.path().to_path_buf();
    let plan_responses = Arc::new(vec![propose_plan_call(
        "p1",
        serde_json::json!({"content": "# Ship it"}),
    )]);
    let first = Arc::new(Mutex::new(true));
    let holly = spawn_with_root(&root, {
        let plan_responses = plan_responses.clone();
        Arc::new(move || {
            let is_first = {
                let mut g = first.lock().unwrap();
                let was = *g;
                *g = false;
                was
            };
            if is_first {
                Box::new(ScriptedLlm::new((*plan_responses).clone())) as Box<dyn Llm>
            } else {
                Box::new(SlowScriptedLlm {
                    responses: Mutex::new(vec![text_response("build child finished the work")]),
                    delay: Duration::from_millis(600),
                }) as Box<dyn Llm>
            }
        })
    });
    let sid = SessionId::new("s1");
    let mut sub = holly.subscribe();
    let request_id = await_request(&holly, &sid).await;
    holly
        .send(InMsg::Approve {
            session: sid.clone(),
            request_id,
            scope: Default::default(),
        })
        .await
        .unwrap();

    // Wait for the sponsored build child's `SessionStarted` — it and the plan
    // session's `WaitingAgent` status race (both are plain `holly.emit_*`
    // calls in `launch_sponsored_build`, with no ordering guarantee between
    // the child's async `Spawn` processing and the status emit), so key only
    // on the event this test actually needs, not their relative order. The
    // child is still mid-flight (its `SlowScriptedLlm` is still sleeping).
    let mut build_session = None;
    while let Ok(Ok(ev)) = tokio::time::timeout(Duration::from_secs(3), sub.recv()).await {
        if let OutEvent::SessionStarted {
            session,
            parent: Some(p),
            sponsored,
            ..
        } = &ev
        {
            if p == &sid {
                // #626: the TUI's cascade-vs-detach confirm modal disambiguates
                // `WaitingAgent`'s two callers by this flag — a sponsored build
                // child must announce it, unlike a plain `agent`-spawned one.
                assert!(sponsored, "a propose_plan build child must be sponsored");
                build_session = Some(session.clone());
                break;
            }
        }
    }
    let build_session = build_session.expect("a sponsored build child must have been spawned");

    holly
        .send(InMsg::Stop {
            session: sid.clone(),
        })
        .await
        .unwrap();

    // The plan session's propose_plan call must never get a reply — the wait
    // was aborted, not resolved.
    let plan_events = collect_until_done(&mut sub, &sid, Duration::from_millis(300)).await;
    assert!(
        !plan_events.iter().any(|e| matches!(
            e,
            OutEvent::ToolOutput { tool, .. } if tool == PROPOSE_PLAN_TOOL
        )),
        "a detached Stop must owe no propose_plan reply: {plan_events:?}"
    );

    // The build child, left untouched, must still complete on its own —
    // "detach" is Stop on the plan session simply never resuming this task.
    let mut sub2 = holly.subscribe();
    let mut child_finished = false;
    while let Ok(Ok(ev)) = tokio::time::timeout(Duration::from_secs(3), sub2.recv()).await {
        if let OutEvent::TextDelta { session, text, .. } = &ev {
            if session == &build_session && text.contains("build child finished") {
                child_finished = true;
                break;
            }
        }
    }
    assert!(
        child_finished,
        "the sponsored build child must keep running after the plan session's Stop (detach)"
    );
}

#[tokio::test]
async fn stop_on_both_sessions_cascades_and_stops_the_build_child_too() {
    let dir = tempdir();
    let root = dir.path().to_path_buf();
    let plan_responses = Arc::new(vec![propose_plan_call(
        "p1",
        serde_json::json!({"content": "# Ship it"}),
    )]);
    let first = Arc::new(Mutex::new(true));
    let holly = spawn_with_root(&root, {
        let plan_responses = plan_responses.clone();
        Arc::new(move || {
            let is_first = {
                let mut g = first.lock().unwrap();
                let was = *g;
                *g = false;
                was
            };
            if is_first {
                Box::new(ScriptedLlm::new((*plan_responses).clone())) as Box<dyn Llm>
            } else {
                Box::new(SlowScriptedLlm {
                    responses: Mutex::new(vec![text_response("build child finished the work")]),
                    delay: Duration::from_millis(600),
                }) as Box<dyn Llm>
            }
        })
    });
    let sid = SessionId::new("s1");
    let mut sub = holly.subscribe();
    let request_id = await_request(&holly, &sid).await;
    holly
        .send(InMsg::Approve {
            session: sid.clone(),
            request_id,
            scope: Default::default(),
        })
        .await
        .unwrap();

    let mut build_session = None;
    while let Ok(Ok(ev)) = tokio::time::timeout(Duration::from_secs(3), sub.recv()).await {
        if let OutEvent::SessionStarted {
            session,
            parent: Some(p),
            ..
        } = &ev
        {
            if p == &sid {
                build_session = Some(session.clone());
                break;
            }
        }
    }
    let build_session = build_session.expect("a sponsored build child must have been spawned");

    // Cascade: stop both sessions explicitly — the plan session's own Stop
    // detaches the wait (as above); the caller also stops the child so it
    // never gets to finish either.
    holly
        .send(InMsg::Stop {
            session: sid.clone(),
        })
        .await
        .unwrap();
    holly
        .send(InMsg::Stop {
            session: build_session.clone(),
        })
        .await
        .unwrap();

    // Neither session should ever surface the child's completion text —
    // proving the cascade reached the child too, not just the plan session.
    let mut sub2 = holly.subscribe();
    let saw_child_finish = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            match sub2.recv().await {
                Ok(OutEvent::TextDelta { session, text, .. })
                    if session == build_session && text.contains("build child finished") =>
                {
                    return true;
                }
                Ok(_) => continue,
                Err(_) => return false,
            }
        }
    })
    .await
    .unwrap_or(false);
    assert!(
        !saw_child_finish,
        "a cascaded Stop must also interrupt the build child"
    );
}
