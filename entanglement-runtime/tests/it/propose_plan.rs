//! Integration tests for the runtime-owned `propose_plan` tool (#141, ADR-0042;
//! #513, ADR-0145; #560, ADR-0207 §7, which retires the sponsored-build
//! handoff below in favor of a plain mode switch).
//!
//! The model calls `propose_plan(content XOR path)`; the executor intercepts it
//! on `ToolExec` (before permission resolution, like `ask_user`) and, once past
//! the mode grade for `Capability::Plan`, **force-parks it on the `Ask` path
//! unconditionally** — a `ToolRequest` is emitted even under an all-`Allow`
//! mode, *unless* the call is malformed (both/neither of `content`/`path`, a
//! missing/non-`.md` file, or a stale `path`), which replies immediately with
//! no prompt at all. Per ADR-0207 §7:
//!
//! - **Approve** switches the session's mode to `build` (`InMsg::SetMode`) and
//!   replies at once, naming the plan file — no child spawned, no blocking
//!   wait; the same turn continues with the plan already in context.
//! - **Reject** folds the typed reason back, unchanged, still naming the file.
//! - **Stop** while parked on the Ask wait unwinds silently: no `ToolResult` is
//!   ever owed for that call.

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
use entanglement_runtime::mode::ModeTable;
use entanglement_runtime::plan_files::PlanFileRegistry;
use entanglement_runtime::policy::{DefaultGrantStore, ProfileResolver};
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

/// Like [`ScriptedLlm`] but sleeps `delay` before every response, so a test
/// has a reliable window to act (e.g. edit the bound plan file out of band)
/// between two of the session's own turns instead of racing the next tool
/// call's instant dispatch.
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
/// per-session `llm_factory`, graded against the always-`Allow` single-mode
/// fixture (`mode_support::allow_all_table`). Mirrors `tests/rhai.rs`'s
/// `spawn_with_rhai_escape` harness: a real `EscapeRoot` (so `propose_plan`
/// can resolve a project root) and the root-contained host registry (so a
/// `write`/`edit` a test scripts against the bound plan file actually lands
/// and fires `FileChange`).
fn spawn_with_root(root: &Path, llm_factory: Arc<dyn Fn() -> Box<dyn Llm> + Send + Sync>) -> Holly {
    spawn_with_root_and_table(root, llm_factory, crate::mode_support::allow_all_table())
}

/// Like [`spawn_with_root`], but graded against an arbitrary `table` — used
/// by the `Capability::Plan` mode-grading tests below, which need the real
/// built-in `research`/`plan`/`build`/`auto` postures instead of the
/// always-`Allow` fixture.
fn spawn_with_root_and_table(
    root: &Path,
    llm_factory: Arc<dyn Fn() -> Box<dyn Llm> + Send + Sync>,
    table: Arc<ModeTable>,
) -> Holly {
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
    let perm_modes = crate::mode_support::perm_modes();
    let shared_tools = tools.shared();
    let resolver = Arc::new(ProfileResolver::new(
        perm_modes.clone(),
        table,
        shared_tools.clone(),
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
        shared_tools,
        entanglement_runtime::host::jobs::JobRegistry::new(),
        entanglement_runtime::retained_output::RetainedOutputRegistry::new(),
        entanglement_runtime::script_ops::ScriptRegistry::new(),
        Arc::new(RwLock::new(profiles)),
        Arc::new(RwLock::new(Arc::new(SkillRegistry::default()))),
        base,
        active,
        perm_modes,
        resolver,
        grants,
        Hooks::default(),
        Some(escape_root),
        Arc::new(
            entanglement_runtime::mode::ModeTable::builtin()
                .expect("built-in permission modes must parse"),
        ),
        Arc::new(PlanFileRegistry::new()),
        // No per-user MCP scopes (#684) — single-user.
        None,
        // No tool-advertising inputs (ADR-0196) — resolves tool_search.
        None,
        None,
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
async fn approve_switches_the_session_to_build_mode_and_continues_the_turn() {
    let dir = tempdir();
    let root = dir.path();
    let scripted = Arc::new(vec![
        propose_plan_call("p1", serde_json::json!({"content": "# Ship it"})),
        text_response("continuing to implement the plan"),
    ]);
    let holly = spawn_with_root(
        root,
        Arc::new(move || Box::new(ScriptedLlm::new((*scripted).clone())) as Box<dyn Llm>),
    );
    let sid = SessionId::new("s1");
    let request_id = await_request(&holly, &sid).await;

    // Subscribed *after* the approval, not before: `allow_all_table`'s one
    // fixture mode happens to be named `"build"` too, so a subscriber that
    // also captured session start would see that coincidental early
    // `ModeChanged("build")` and could satisfy the assertion below without
    // ever observing the real, approval-triggered switch.
    let mut sub = holly.subscribe();
    holly
        .send(InMsg::Approve {
            session: sid.clone(),
            request_id,
            scope: Default::default(),
        })
        .await
        .unwrap();

    // `InMsg::SetMode` is deferred while a turn is live, exactly like
    // `SetAgent` (session.rs's stash) — and the turn very much still is:
    // `run_propose_plan` sends `SetMode` *before* the `ToolResult` that
    // continues it. So the approval's `ModeChanged` lands only once this
    // whole turn concludes (after `Done`), not synchronously with the
    // `ToolOutput` — collect a bit past `Done` instead of breaking on it.
    let mut saw_mode_changed = false;
    let mut got_output = false;
    let mut saw_other_session = false;
    let mut saw_done = false;
    loop {
        let per_event_timeout = if saw_done {
            Duration::from_millis(300)
        } else {
            Duration::from_secs(5)
        };
        let Ok(Ok(ev)) = tokio::time::timeout(per_event_timeout, sub.recv()).await else {
            break;
        };
        match &ev {
            OutEvent::ModeChanged { session, mode } if session == &sid => {
                assert_eq!(
                    mode, "build",
                    "approval must switch the session to build mode"
                );
                saw_mode_changed = true;
            }
            OutEvent::SessionStarted { session, .. } if session != &sid => {
                saw_other_session = true;
            }
            OutEvent::ToolOutput {
                session,
                tool,
                output,
                is_error,
                ..
            } if session == &sid && tool == PROPOSE_PLAN_TOOL => {
                assert!(!is_error, "an approved plan is not a tool error: {output}");
                assert!(
                    output.contains(".entanglement/plans/s1.md"),
                    "the tool result must name the plan file's location (#513): {output}"
                );
                assert!(
                    output.contains("build"),
                    "the tool result must name the mode it switched to: {output}"
                );
                got_output = true;
            }
            OutEvent::Done { session, .. } if session == &sid => saw_done = true,
            _ => {}
        }
    }
    assert!(saw_mode_changed, "approval must emit ModeChanged(build)");
    assert!(
        got_output,
        "approve must reply at once — no child, no blocking wait"
    );
    assert!(
        !saw_other_session,
        "ADR-0207 §7: approval spawns no sponsored child any more"
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
    // Multi-phase plan -> build -> review loop, now all in the *same* session
    // (ADR-0207 §7 retires the sponsored build child): each propose_plan call
    // still force-parks on its own approval, even though the mode is
    // Allow-all — proving grading doesn't skip the park on a second call.
    let dir = tempdir();
    let root = dir.path().to_path_buf();
    let scripted = Arc::new(vec![
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
    let holly = spawn_with_root(
        &root,
        Arc::new(move || Box::new(ScriptedLlm::new((*scripted).clone())) as Box<dyn Llm>),
    );
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
async fn stop_while_parked_on_the_ask_wait_owes_no_reply() {
    // ADR-0207 §7 retired the post-approval blocking build wait along with
    // the sponsored child, so the only wait left to interrupt is the Ask
    // park itself — mirroring how every other force-parked runtime tool
    // (`ask_user`, the generic `Ask` dispatch path) unwinds on `Stop`: core
    // cancels the turn on the same `Stop`, so no `ToolResult` is ever owed.
    let dir = tempdir();
    let root = dir.path();
    let scripted = Arc::new(vec![propose_plan_call(
        "p1",
        serde_json::json!({"content": "# Ship it"}),
    )]);
    let holly = spawn_with_root(
        root,
        Arc::new(move || Box::new(ScriptedLlm::new((*scripted).clone())) as Box<dyn Llm>),
    );
    let sid = SessionId::new("s1");
    let mut sub = holly.subscribe();
    await_request(&holly, &sid).await;

    holly
        .send(InMsg::Stop {
            session: sid.clone(),
        })
        .await
        .unwrap();

    let events = collect_until_done(&mut sub, &sid, Duration::from_millis(300)).await;
    assert!(
        !events.iter().any(|e| matches!(
            e,
            OutEvent::ToolOutput { tool, .. } if tool == PROPOSE_PLAN_TOOL
        )),
        "a Stop while parked on the Ask wait must owe no propose_plan reply: {events:?}"
    );
}

/// ADR-0207 §7: `propose_plan` is advertised unconditionally now, but grading
/// closes authorship outside `plan` mode — these tests exercise the real
/// built-in `research`/`build`/`auto`/`plan` postures (`ModeTable::builtin()`),
/// not the always-`Allow` fixture every other test in this file uses.
async fn set_mode_and_wait(holly: &Holly, sid: &SessionId, mode: &str) {
    let mut sub = holly.subscribe();
    holly
        .send(InMsg::SetMode {
            session: sid.clone(),
            mode: mode.to_string(),
        })
        .await
        .unwrap();
    while let Ok(Ok(ev)) = tokio::time::timeout(Duration::from_secs(2), sub.recv()).await {
        if let OutEvent::ModeChanged {
            session, mode: got, ..
        } = &ev
        {
            if session == sid && got == mode {
                return;
            }
        }
    }
    panic!("timed out waiting for ModeChanged({mode})");
}

async fn assert_denied_by_mode(mode: &str) {
    let dir = tempdir();
    let root = dir.path();
    let scripted = Arc::new(vec![
        propose_plan_call("p1", serde_json::json!({"content": "# Ship it"})),
        text_response("ok"),
    ]);
    let holly = spawn_with_root_and_table(
        root,
        Arc::new(move || Box::new(ScriptedLlm::new((*scripted).clone())) as Box<dyn Llm>),
        Arc::new(ModeTable::builtin().expect("built-in modes must parse")),
    );
    let sid = SessionId::new("s1");
    set_mode_and_wait(&holly, &sid, mode).await;
    let mut sub = holly.subscribe();
    holly.send(InMsg::prompt(sid.clone(), "go")).await.unwrap();
    let events = collect_until_done(&mut sub, &sid, Duration::from_secs(3)).await;

    assert!(
        !events
            .iter()
            .any(|e| matches!(e, OutEvent::ToolRequest { .. })),
        "`{mode}` denies Plan — no approval prompt must ever be parked: {events:?}"
    );
    assert!(
        events.iter().any(|e| matches!(
            e,
            OutEvent::ToolOutput { tool, output, is_error, .. }
                if tool == PROPOSE_PLAN_TOOL && *is_error
                    && output.contains("denied by mode")
                    && output.contains(mode)
        )),
        "`{mode}` must decline propose_plan flat, naming the mode: {events:?}"
    );
    // No plan file materialized — the call was declined before it ever ran.
    assert!(!root.join(".entanglement/plans/s1.md").exists());
}

#[tokio::test]
async fn research_mode_denies_propose_plan() {
    assert_denied_by_mode("research").await;
}

#[tokio::test]
async fn build_mode_denies_propose_plan() {
    assert_denied_by_mode("build").await;
}

#[tokio::test]
async fn auto_mode_denies_propose_plan() {
    assert_denied_by_mode("auto").await;
}

#[tokio::test]
async fn plan_mode_still_force_parks_propose_plan() {
    let dir = tempdir();
    let root = dir.path();
    let scripted = Arc::new(vec![
        propose_plan_call("p1", serde_json::json!({"content": "# Ship it"})),
        text_response("ok"),
    ]);
    let holly = spawn_with_root_and_table(
        root,
        Arc::new(move || Box::new(ScriptedLlm::new((*scripted).clone())) as Box<dyn Llm>),
        Arc::new(ModeTable::builtin().expect("built-in modes must parse")),
    );
    let sid = SessionId::new("s1");
    set_mode_and_wait(&holly, &sid, "plan").await;
    let request_id = await_request(&holly, &sid).await;

    // Reject — this test only cares that `plan` mode reaches the park at
    // all, the approve path is covered by the always-Allow-fixture tests
    // above.
    let mut sub = holly.subscribe();
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
        events
            .iter()
            .any(|e| matches!(e, OutEvent::ToolOutput { tool, .. } if tool == PROPOSE_PLAN_TOOL)),
        "plan mode must let the call reach the ordinary reject fold-back: {events:?}"
    );
}
