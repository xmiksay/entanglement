//! Integration test for skill-scoped `allowed_tools` enforcement (#400,
//! ADR-0106): a `load_skill` call activates the session's skill mask —
//! layered *after* the #116 agent mask — for the rest of that turn, and it
//! clears at `Done` so a later turn is unrestricted again.
//!
//! Drives the real engine + tool executor with a scripted LLM across two
//! turns: turn 1 loads a skill whose `allowed_tools: [read]` lets `read`
//! through but refuses `edit`; turn 2 (after the first turn's `Done` clears
//! the mask) proves `edit` is unmasked again.

use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use async_trait::async_trait;
use entanglement_core::{
    stream_from_response, EngineConfig, Holly, InMsg, Llm, LlmRequest, LlmResponse, LlmStream,
    OutEvent, Permission, PermissionProfile, SessionId, ToolCall,
};
use entanglement_runtime::host::host_tools;
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
/// with this collector, so the skill-mask clear `SkillActive` (#400) can arrive
/// a beat after `Done` itself rather than strictly before it.
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
async fn skill_mask_restricts_tools_for_one_turn_then_clears() {
    let id = std::process::id();
    let root = std::env::temp_dir().join(format!("entanglement-skillmask-e2e-{id}"));
    std::fs::create_dir_all(&root).unwrap();
    let _cleanup = Cleanup(root.clone());

    // A project skill masking everything but `read` for the turn it loads in.
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

    std::env::set_var("ENTANGLEMENT_SKILLS_DIR", root.join("no-such-user-dir"));
    let skill_registry = Arc::new(load_registry(&root).unwrap());
    std::env::remove_var("ENTANGLEMENT_SKILLS_DIR");
    let skills = Arc::new(RwLock::new(skill_registry));

    let mut tools = host_tools(root.clone());
    tools.register(LoadSkillTool::new(skills.clone()));

    let scripted = Arc::new(vec![
        // Turn 1, round 1: activate the skill.
        LlmResponse {
            text: "".into(),
            tool_calls: vec![tool_call(
                "l1",
                "load_skill",
                serde_json::json!({"skill_name": "restricted"}),
            )],
        },
        // Turn 1, round 2: `edit` is outside `allowed_tools` — must be refused
        // without touching the file (no `oldString`/`newString` needed since
        // the mask fires before dispatch).
        LlmResponse {
            text: "".into(),
            tool_calls: vec![tool_call(
                "e1",
                "edit",
                serde_json::json!({"path": target.to_string_lossy(), "oldString": "hello", "newString": "bye"}),
            )],
        },
        // Turn 1, round 3: `read` is inside `allowed_tools` — must succeed.
        LlmResponse {
            text: "".into(),
            tool_calls: vec![tool_call(
                "r1",
                "read",
                serde_json::json!({"path": target.to_string_lossy()}),
            )],
        },
        // Turn 1, round 4: finish — triggers `Done`, clearing the skill mask.
        LlmResponse {
            text: "turn1 done".into(),
            tool_calls: vec![],
        },
        // Turn 2, round 1: `edit` again — must succeed now, unmasked.
        LlmResponse {
            text: "".into(),
            tool_calls: vec![tool_call(
                "e2",
                "edit",
                serde_json::json!({"path": target.to_string_lossy(), "oldString": "hello", "newString": "bye"}),
            )],
        },
        LlmResponse {
            text: "turn2 done".into(),
            tool_calls: vec![],
        },
    ]);

    let profiles = entanglement_runtime::agents::built_in_registry();
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
    let resolver: Arc<dyn entanglement_runtime::policy::PermissionResolver> =
        Arc::new(entanglement_runtime::policy::ProfileResolver::new(
            active.clone(),
            PermissionProfile::new(Permission::Allow),
        ));
    let grants: Arc<dyn entanglement_runtime::policy::GrantStore> =
        Arc::new(entanglement_runtime::policy::DefaultGrantStore::load());
    let _executor = spawn_tool_executor_with_policy(
        &holly,
        tools.shared(),
        Arc::new(RwLock::new(profiles)),
        skills,
        PermissionProfile::new(Permission::Allow),
        active,
        resolver,
        grants,
        Default::default(),
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
    assert!(
        turn1_outputs
            .iter()
            .any(|o| o.contains("not available while skill `restricted` is active")),
        "edit must be refused by the skill mask; got {turn1_outputs:?}"
    );
    assert!(
        turn1_outputs.iter().any(|o| o.contains("hello")),
        "read (in allowed_tools) must succeed; got {turn1_outputs:?}"
    );
    // The file must be untouched — the masked `edit` never dispatched.
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "hello");
    // The activation is surfaced on the wire (#400 item 3).
    assert!(
        turn1.iter().any(|e| matches!(
            e,
            OutEvent::SkillActive { skill_id: Some(id), allowed_tools: Some(tools), .. }
                if id == "restricted" && tools == &vec!["read".to_string()]
        )),
        "expected a SkillActive activation event; got {turn1:?}"
    );
    // `Done` clears it.
    assert!(
        turn1
            .iter()
            .any(|e| matches!(e, OutEvent::SkillActive { skill_id: None, .. })),
        "expected a SkillActive clear event at Done; got {turn1:?}"
    );

    holly
        .send(InMsg::prompt(sid.clone(), "try edit again"))
        .await
        .unwrap();
    let turn2 = collect_through_dones(&mut sub, &sid, 1).await;
    let turn2_outputs = outputs(&turn2);
    assert!(
        !turn2_outputs
            .iter()
            .any(|o| o.contains("not available while skill")),
        "edit must be unmasked in a later turn; got {turn2_outputs:?}"
    );
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "bye");
}
