//! Per-session skills (ADR-0249): `SkillResolver` + `SessionSkills` +
//! `SkillMeta::new`/`with_lib` through the real executor — `list_skills`,
//! `load_skill` and a script's `skill::<name>::<fn>()` read the session's
//! registry.

use std::sync::{Arc, RwLock};
use std::time::Duration;

use entanglement_core::{
    EngineConfig, Holly, InMsg, LlmResponse, OutEvent, Permission, PermissionProfile, SessionId,
    ToolCall,
};
use entanglement_runtime::host::host_tools;
use entanglement_runtime::skills::{LoadSkillTool, SkillMeta, SkillRegistry, SkillResolver};
use entanglement_runtime::tool_names::RHAI_TOOL;
use entanglement_runtime::tool_runner::ToolExecutor;

use super::{allow_all_table, collect_auto_approving, ScriptedLlm, TempDir};

fn skills(description: &str, who: &str) -> Arc<SkillRegistry> {
    let mut reg = SkillRegistry::default();
    reg.insert(
        SkillMeta::new("inv", description, "Invoice steps.")
            .with_lib(&format!(r#"fn who() {{ "{who}" }}"#))
            .unwrap(),
    );
    Arc::new(reg)
}

fn call(id: &str, name: &str, input: serde_json::Value) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: name.into(),
        input: input.to_string(),
        provider_meta: None,
    }
}

fn scripted() -> Vec<LlmResponse> {
    let calls = [
        call(
            "t0",
            RHAI_TOOL,
            serde_json::json!({"script": r#"skill::inv::who()"#}),
        ),
        call("t1", "list_skills", serde_json::json!({})),
        call("t2", "load_skill", serde_json::json!({"skill_name": "inv"})),
    ];
    let mut out: Vec<LlmResponse> = calls
        .into_iter()
        .map(|c| LlmResponse {
            tool_calls: vec![c],
            ..Default::default()
        })
        .collect();
    out.push(LlmResponse {
        text: "ok".into(),
        ..Default::default()
    });
    out
}

fn engine(dir: &TempDir) -> Holly {
    let tenant_a = skills("tenant a", "a");
    let resolver: SkillResolver =
        Arc::new(move |s: &SessionId| s.to_string().starts_with("a:").then(|| tenant_a.clone()));
    let live = Arc::new(RwLock::new(skills("global", "global")));
    let mut tools = host_tools(dir.path.clone());
    tools.register(LoadSkillTool::with_resolver(live.clone(), resolver.clone()));
    let tools = tools.shared();
    let agents =
        entanglement_runtime::agents::built_in_registry().expect("built-in agents must parse");
    let holly = Holly::spawn(EngineConfig {
        llm_factory: Arc::new(|| {
            Box::new(ScriptedLlm::new(scripted())) as Box<dyn entanglement_core::Llm>
        }),
        agents: agents.clone(),
        ..EngineConfig::default()
    });
    super::answer_launch_cards(&holly);
    let _executor = ToolExecutor::builder(&holly, tools)
        .agents(agents)
        .base(PermissionProfile::new(Permission::Allow))
        .mode_table(allow_all_table())
        .live_skills(live)
        .skill_resolver(resolver)
        .spawn();
    holly
}

async fn run(holly: &Holly, sid: &SessionId) -> std::collections::HashMap<String, String> {
    let sub = holly.subscribe();
    holly
        .send(InMsg::Spawn {
            session: sid.clone(),
            parent: None,
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
    let events = tokio::time::timeout(
        Duration::from_secs(30),
        collect_auto_approving(holly, sub, sid),
    )
    .await
    .expect("the session finishes");
    events
        .into_iter()
        .filter_map(|e| match e {
            OutEvent::ToolOutput {
                request_id, output, ..
            } => Some((request_id, output)),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn each_session_sees_its_own_skills() {
    let dir = TempDir::new("session-skills");
    let holly = engine(&dir);

    for (sid, tenant, skill_desc) in [("a:1", "a", "tenant a"), ("b:1", "global", "global")] {
        let sid = SessionId::new(sid);
        let out = run(&holly, &sid).await;
        let get = |id: &str| {
            out.get(id)
                .cloned()
                .unwrap_or_else(|| panic!("{sid}: no {id}: {out:?}"))
        };
        // The script calls skill::inv::who() — resolved per session.
        let who = get("t0");
        assert!(
            who.contains(tenant),
            "{sid}: skill::inv::who() should return {tenant}, got {who}"
        );
        // list_skills sees the per-session description.
        let listed = get("t1");
        assert!(
            listed.contains(skill_desc),
            "{sid}: list_skills should show '{skill_desc}', got {listed}"
        );
        // load_skill resolves from the session's registry.
        let loaded = get("t2");
        assert!(
            loaded.contains("skill_id: inv"),
            "{sid}: load_skill should resolve, got {loaded}"
        );
    }
}
