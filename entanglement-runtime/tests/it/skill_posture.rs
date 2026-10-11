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
use entanglement_runtime::skills::{load_registry_in, LoadSkillTool};
use entanglement_runtime::tool_runner::ToolExecutor;

use crate::no_user;

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
                ..Default::default()
            })
        };
        Ok(stream_from_response(resp))
    }
}

/// Collect events for `sid` up to and including the *n*th `Done` *and* the
/// skill-posture clear `SkillActive` (#400). The tool executor's own broadcast
/// subscription processes `Done` concurrently with this collector, so the
/// clear can arrive a beat after `Done`; waiting for that event itself (not
/// a fixed linger, #815) keeps a loaded runner from losing the race. Each
/// recv is bounded, so a missing event fails the caller's assertion.
async fn collect_through_dones(
    sub: &mut tokio::sync::broadcast::Receiver<OutEvent>,
    sid: &SessionId,
    dones: usize,
) -> Vec<OutEvent> {
    let mut out = Vec::new();
    let mut seen_dones = 0;
    let mut cleared = false;
    while seen_dones < dones || !cleared {
        let Ok(Ok(ev)) = tokio::time::timeout(Duration::from_secs(10), sub.recv()).await else {
            break;
        };
        if ev.session() != Some(sid) {
            continue;
        }
        match ev {
            OutEvent::Done { .. } => seen_dones += 1,
            OutEvent::SkillActive { skill_id: None, .. } => cleared = true,
            _ => {}
        }
        out.push(ev);
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

    let skill_registry = Arc::new(load_registry_in(&root, &no_user(&root)).unwrap());
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
            ..Default::default()
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
            ..Default::default()
        },
        // Round 3: finish — triggers `Done`, clearing the skill-active posture.
        LlmResponse {
            text: "turn1 done".into(),
            tool_calls: vec![],
            ..Default::default()
        },
    ]);

    let profiles =
        entanglement_runtime::agents::built_in_registry().expect("built-in agents must parse");
    let cfg = EngineConfig {
        llm_factory: Arc::new(move || {
            Box::new(ScriptedLlm::new((*scripted).clone())) as Box<dyn Llm>
        }),
        tool_specs: tools.specs(),
        agents: profiles.clone(),
        ..EngineConfig::default()
    };
    let holly = Holly::spawn(cfg);
    let active = Arc::new(Mutex::new(std::collections::HashMap::new()));
    let perm_modes = crate::mode_support::perm_modes();
    let shared_tools = tools.shared();
    let resolver: Arc<dyn entanglement_runtime::policy::PermissionResolver> =
        Arc::new(entanglement_runtime::policy::ModeResolver::new(
            perm_modes.clone(),
            crate::mode_support::allow_all_table(),
            shared_tools.clone(),
            PermissionProfile::new(Permission::Allow),
            None,
        ));
    let grants: Arc<dyn entanglement_runtime::policy::GrantStore> = Arc::new(
        entanglement_runtime::policy::DefaultGrantStore::load_from(None),
    );
    let _executor = ToolExecutor::builder(&holly, shared_tools)
        .agents(profiles)
        .live_skills(skills)
        .base(PermissionProfile::new(Permission::Allow))
        .active(active)
        .perm_modes(perm_modes)
        .resolver(resolver)
        .grants(grants)
        .mode_table(Arc::new(
            entanglement_runtime::mode::ModeTable::builtin()
                .expect("built-in permission modes must parse"),
        ))
        .spawn();

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
