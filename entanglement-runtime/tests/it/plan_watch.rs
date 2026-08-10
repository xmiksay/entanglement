//! Integration tests for the dedicated plans-folder watch (#627, ADR-0145
//! "Consequences"): `propose_plan`'s staleness guard already refuses a stale
//! `path` resubmit at the *next* call, but this watch surfaces the same
//! out-of-band-edit detection live, as an `OutEvent::PlanChanged` on the
//! bound session — without waiting for another tool call.

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
use entanglement_runtime::plan_watch::spawn_plans_watcher;
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

fn tempdir() -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix("entanglement-plan-watch-it-")
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

/// Spawn a `Holly` + real tool executor + the dedicated plans watcher, all
/// sharing one `PlanFileRegistry` — mirrors `tests/propose_plan.rs`'s
/// `spawn_with_root`, plus the watcher wiring `main.rs` does. Pre-creates the
/// plans folder (the watch's known v1 limitation, matching the definitions
/// watcher's own: a directory that doesn't exist at watch-start needs a
/// restart to be picked up) so the very first `propose_plan(content=...)`
/// call — which is what actually creates it in the real CLI — isn't required
/// before the watch can see anything.
fn spawn_with_root(root: &Path, llm_factory: Arc<dyn Fn() -> Box<dyn Llm> + Send + Sync>) -> Holly {
    std::fs::create_dir_all(root.join(".entanglement/plans")).unwrap();
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
    let plan_files = Arc::new(PlanFileRegistry::new());
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
        plan_files.clone(),
    );
    let _watcher = spawn_plans_watcher(&holly, root.to_path_buf(), plan_files)
        .expect("the pre-created plans folder must be watchable");
    holly
}

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
    panic!("expected a ToolRequest for propose_plan");
}

#[tokio::test]
async fn an_out_of_band_edit_surfaces_a_live_plan_changed_notice() {
    let dir = tempdir();
    let root = dir.path();
    let scripted = Arc::new(vec![
        propose_plan_call("p1", serde_json::json!({"content": "# v1"})),
        text_response("revised"),
    ]);
    let holly = spawn_with_root(
        root,
        Arc::new(move || Box::new(ScriptedLlm::new((*scripted).clone())) as Box<dyn Llm>),
    );
    let sid = SessionId::new("s1");
    let mut sub = holly.subscribe();
    let request_id = await_request(&holly, &sid).await;

    // Reject — the file is materialized and bound either way (content mode
    // always writes); we only care about the binding, not the approval outcome.
    holly
        .send(InMsg::Reject {
            session: sid.clone(),
            request_id,
            reason: Some("not yet".into()),
        })
        .await
        .unwrap();
    while let Ok(Ok(ev)) = tokio::time::timeout(Duration::from_secs(3), sub.recv()).await {
        if let OutEvent::ToolOutput { tool, .. } = &ev {
            if tool == PROPOSE_PLAN_TOOL {
                break;
            }
        }
    }

    // The "user" edits the plan file directly, bypassing every tool the
    // runtime would see execute.
    std::fs::write(
        root.join(".entanglement/plans/s1.md"),
        "# v2, edited by the user",
    )
    .unwrap();

    let mut saw_notice = false;
    while let Ok(Ok(ev)) = tokio::time::timeout(Duration::from_secs(3), sub.recv()).await {
        if let OutEvent::PlanChanged { session, path, .. } = &ev {
            assert_eq!(session, &sid);
            assert_eq!(path, ".entanglement/plans/s1.md");
            saw_notice = true;
            break;
        }
    }
    assert!(
        saw_notice,
        "an out-of-band plan-file edit must surface a live PlanChanged notice"
    );
}

#[tokio::test]
async fn the_agents_own_edit_does_not_also_produce_a_plan_changed_notice() {
    // The intended review loop: the plan agent edits the bound file directly
    // (via `write`) between phases. The tool executor's own `FileChange`
    // listener already refreshes `plan_files` for this, so the watcher's
    // later debounced firing must see no mismatch and stay silent — a
    // `PlanChanged` notice here would misleadingly suggest a *user* edit.
    let dir = tempdir();
    let root = dir.path();
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
        text_response("done"),
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
            reason: Some("not yet".into()),
        })
        .await
        .unwrap();

    // Collect for a window comfortably past the watch's debounce, and assert
    // no `PlanChanged` ever arrives for this session's own `write`.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    let mut saw_notice = false;
    while tokio::time::Instant::now() < deadline {
        if let Ok(Ok(ev)) =
            tokio::time::timeout(deadline - tokio::time::Instant::now(), sub.recv()).await
        {
            if let OutEvent::PlanChanged { session, .. } = &ev {
                if session == &sid {
                    saw_notice = true;
                    break;
                }
            }
        } else {
            break;
        }
    }
    assert!(
        !saw_notice,
        "the plan agent's own tracked `write` must not also trigger the watcher's notice"
    );
}
