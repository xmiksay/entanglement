//! Integration test for skills' additive-only posture (#400, ADR-0106,
//! retired by ADR-0194): a `load_skill` call whose skill carries
//! `allowed_tools` no longer narrows the session's tool set for the rest of
//! the turn — a tool outside the list still dispatches normally. The wire
//! posture event (`OutEvent::SkillActive`) is unchanged: it still fires on
//! activation (with the frontmatter's `allowed_tools`, vestigial) and clears
//! at the turn's `Done`.
//!
//! Drives the real engine + tool executor with a scripted LLM: turn 1 loads a
//! skill whose `allowed_tools: [read]` used to refuse `edit` — it must now
//! succeed instead.

use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use async_trait::async_trait;
use entanglement_core::{
    stream_from_response, EngineConfig, Holly, InMsg, Llm, LlmRequest, LlmResponse, LlmStream,
    OutEvent, Permission, PermissionProfile, SessionId, ToolCall,
};
use entanglement_runtime::host::host_tools;
use entanglement_runtime::plan_files::PlanFileRegistry;
use entanglement_runtime::skills::{load_registry, LoadSkillTool};
use entanglement_runtime::tool_runner::spawn_tool_executor_with_policy;

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

/// Collect events for `sid` up to and including the *n*th `Done`, then linger
/// briefly to also catch anything the tool executor emits asynchronously right
/// after `Done` — its own broadcast subscription processes `Done` concurrently
/// with this collector, so the skill-posture clear `SkillActive` (#400) can
/// arrive a beat after `Done` itself rather than strictly before it.
async fn collect_through_dones(
    sub: &mut tokio::sync::broadcast::Receiver<OutEvent>,
    sid: &SessionId,
    dones: usize,
) -> Vec<OutEvent> {
    let mut out = Vec::new();
    let mut seen_dones = 0;
    while seen_dones < dones {
        let Ok(Ok(ev)) = tokio::time::timeout(Duration::from_secs(3), sub.recv()).await else {
            break;
        };
        if ev.session() != Some(sid) {
            continue;
        }
        if matches!(ev, OutEvent::Done { .. }) {
            seen_dones += 1;
        }
        out.push(ev);
    }
    while let Ok(Ok(ev)) = tokio::time::timeout(Duration::from_millis(200), sub.recv()).await {
        if ev.session() == Some(sid) {
            out.push(ev);
        }
    }
    out
}

fn tool_call(id: &str, name: &str, input: serde_json::Value) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: name.into(),
        input: input.to_string(),
        provider_meta: None,
    }
}

struct Cleanup(std::path::PathBuf);
impl Drop for Cleanup {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[tokio::test]
async fn skill_allowed_tools_no_longer_narrows_the_turn_posture_event_unchanged() {
    let id = std::process::id();
    let root = std::env::temp_dir().join(format!("entanglement-skillmask-e2e-{id}"));
    std::fs::create_dir_all(&root).unwrap();
    let _cleanup = Cleanup(root.clone());

    // A project skill that *used to* mask everything but `read` for the turn
    // it loads in (ADR-0106) — now purely additive (ADR-0194): the frontmatter
    // still parses (and still feeds the vestigial wire field), but it no
    // longer refuses anything.
    let skill_dir = root.join(".entanglement/skills/restricted");
    std::fs::create_dir_all(&skill_dir).unwrap();
    std::fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname: restricted\ndescription: a read-only skill\nallowed_tools: [read]\n---\n\
         Only read.\n",
    )
    .unwrap();
    let target = root.join("target.txt");
    std::fs::write(&target, "hello").unwrap();

    let skill_registry = {
        let _guard = crate::env_lock();
        std::env::set_var("ENTANGLEMENT_SKILLS_DIR", root.join("no-such-user-dir"));
        let skill_registry = Arc::new(load_registry(&root).unwrap());
        std::env::remove_var("ENTANGLEMENT_SKILLS_DIR");
        skill_registry
    };
    let skills = Arc::new(RwLock::new(skill_registry));

    let mut tools = host_tools(root.clone());
    tools.register(LoadSkillTool::new(skills.clone()));

    let scripted = Arc::new(vec![
        // Round 1: activate the skill.
        LlmResponse {
            text: "".into(),
            tool_calls: vec![tool_call(
                "l1",
                "load_skill",
                serde_json::json!({"skill_name": "restricted"}),
            )],
        },
        // Round 2: `edit` is outside `allowed_tools` — under ADR-0106 this was
        // refused before dispatch; ADR-0194 makes it succeed like any other
        // agent-mask-permitted tool.
        LlmResponse {
            text: "".into(),
            tool_calls: vec![tool_call(
                "e1",
                "edit",
                serde_json::json!({"path": target.to_string_lossy(), "oldString": "hello", "newString": "bye"}),
            )],
        },
        // Round 3: finish — triggers `Done`, clearing the skill-active posture.
        LlmResponse {
            text: "turn1 done".into(),
            tool_calls: vec![],
        },
    ]);

    let profiles =
        entanglement_runtime::agents::built_in_registry().expect("built-in agents must parse");
    let cfg = EngineConfig {
        llm_factory: Arc::new(move || {
            Box::new(ScriptedLlm::new((*scripted).clone())) as Box<dyn Llm>
        }),
        tool_specs: tools.specs(),
        profiles: profiles.clone(),
        ..EngineConfig::default()
    };
    let holly = Holly::spawn(cfg);
    let active = Arc::new(Mutex::new(std::collections::HashMap::new()));
    let perm_modes = crate::mode_support::perm_modes();
    let shared_tools = tools.shared();
    let resolver: Arc<dyn entanglement_runtime::policy::PermissionResolver> =
        Arc::new(entanglement_runtime::policy::ProfileResolver::new(
            perm_modes.clone(),
            crate::mode_support::allow_all_table(),
            shared_tools.clone(),
            PermissionProfile::new(Permission::Allow),
            None,
        ));
    let grants: Arc<dyn entanglement_runtime::policy::GrantStore> =
        Arc::new(entanglement_runtime::policy::DefaultGrantStore::load());
    let _executor = spawn_tool_executor_with_policy(
        &holly,
        shared_tools,
        entanglement_runtime::host::jobs::JobRegistry::new(),
        entanglement_runtime::retained_output::RetainedOutputRegistry::new(),
        entanglement_runtime::script_ops::ScriptRegistry::new(),
        Arc::new(RwLock::new(profiles)),
        skills,
        PermissionProfile::new(Permission::Allow),
        active,
        perm_modes,
        resolver,
        grants,
        Default::default(),
        None,
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

    let sid = SessionId::new("s1");
    let mut sub = holly.subscribe();
    holly
        .send(InMsg::prompt(sid.clone(), "use the restricted skill"))
        .await
        .unwrap();
    let turn1 = collect_through_dones(&mut sub, &sid, 1).await;

    let outputs = |events: &[OutEvent]| -> Vec<String> {
        events
            .iter()
            .filter_map(|e| match e {
                OutEvent::ToolOutput { output, .. } => Some(output.clone()),
                _ => None,
            })
            .collect()
    };
    let turn1_outputs = outputs(&turn1);
    assert!(
        turn1_outputs
            .iter()
            .any(|o| o.contains("skill_id: restricted")),
        "expected the load_skill result; got {turn1_outputs:?}"
    );
    // The additive posture: `edit`, outside the loaded skill's
    // `allowed_tools`, dispatches like any other agent-mask-permitted tool —
    // no skill-mask decline.
    assert!(
        !turn1_outputs
            .iter()
            .any(|o| o.contains("Declined by skill")),
        "a skill's allowed_tools must no longer refuse a tool (ADR-0194); got {turn1_outputs:?}"
    );
    assert_eq!(
        std::fs::read_to_string(&target).unwrap(),
        "bye",
        "the edit outside allowed_tools must have actually run"
    );
    // The activation is still surfaced on the wire, allowed_tools populated
    // from the frontmatter exactly as before (vestigial, #400 item 3).
    assert!(
        turn1.iter().any(|e| matches!(
            e,
            OutEvent::SkillActive { skill_id: Some(id), allowed_tools: Some(tools), .. }
                if id == "restricted" && tools == &vec!["read".to_string()]
        )),
        "expected a SkillActive activation event; got {turn1:?}"
    );
    // `Done` still clears the posture.
    assert!(
        turn1
            .iter()
            .any(|e| matches!(e, OutEvent::SkillActive { skill_id: None, .. })),
        "expected a SkillActive clear event at Done; got {turn1:?}"
    );
}
