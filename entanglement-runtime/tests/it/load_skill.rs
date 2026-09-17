//! Integration test for the `load_skill` tier-2 tool (#115, ADR-0037).
//!
//! Drives the real engine with a scripted LLM: turn 1 calls `load_skill` for a
//! project skill; the handler substitutes the body's relative `references/…`
//! ref to an absolute path. Turn 2 feeds that exact absolute path back into the
//! `read` tool, proving the model can open a substituted ref without guessing a
//! base directory (the bug class ADR-0037 closes). A third case pins the
//! opposite of ADR-0037's old "no special exemption" contract: `load_skill`
//! declares `Capability::Control` (ADR-0207 §3), so a mode rule naming it is
//! inert — it is never graded, unlike ADR-0037's original per-profile
//! `permission` gate (superseded, gap 1 of ADR-0207 stage 4b).

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use entanglement_core::{
    stream_from_response, EngineConfig, Holly, InMsg, Llm, LlmRequest, LlmResponse, LlmStream,
    OutEvent, Permission, PermissionProfile, SessionId, ToolCall,
};
use entanglement_runtime::host::host_tools;
use entanglement_runtime::skills::{load_registry, LoadSkillTool};
use entanglement_runtime::tool_runner::spawn_tool_executor;

/// Replays scripted responses in order, then a plain text reply so the turn loop
/// terminates. The `read` call in turn 2 references a path captured *after* the
/// scripts are built, so it is injected via a shared slot the LLM reads live.
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
            Ok(ev) if ev.session() == Some(sid) => {
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

/// Write a project skill under `<root>/.entanglement/skills/<name>/` with a
/// `references/guide.md` payload, and return the root the tools/registry use.
fn write_project_skill(root: &std::path::Path) {
    let skill_dir = root.join(".entanglement/skills/demo");
    std::fs::create_dir_all(skill_dir.join("references")).unwrap();
    std::fs::write(skill_dir.join("references/guide.md"), "the guide body\n").unwrap();
    std::fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname: demo\ndescription: a demo skill\n---\n\
         Follow references/guide.md to proceed.\n",
    )
    .unwrap();
}

#[test]
fn cross_vendor_project_skill_resolves_root_dir() {
    // A skill under the cross-vendor `.agents/skills/` project dir (ADR-0074)
    // loads through the same registry with its `root_dir` resolved, so the
    // tier-2 `load_skill` path-substitution pipeline works unchanged.
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let skill_dir = root.join(".agents/skills/vendor");
    std::fs::create_dir_all(&skill_dir).unwrap();
    std::fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname: vendor\ndescription: cross-vendor skill\n---\nbody\n",
    )
    .unwrap();

    let _guard = crate::env_lock();
    std::env::set_var("ENTANGLEMENT_SKILLS_DIR", root.join("no-such-user-dir"));
    let registry = load_registry(root).unwrap();
    std::env::remove_var("ENTANGLEMENT_SKILLS_DIR");

    let meta = registry.get("vendor").expect("cross-vendor skill loaded");
    assert_eq!(meta.root_dir.as_deref(), Some(skill_dir.as_path()));
}

#[tokio::test]
async fn load_skill_then_read_a_substituted_ref() {
    let id = std::process::id();
    let root = std::env::temp_dir().join(format!("entanglement-loadskill-e2e-{id}"));
    std::fs::create_dir_all(&root).unwrap();
    struct Drop_(std::path::PathBuf);
    impl Drop for Drop_ {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    let _cleanup = Drop_(root.clone());
    write_project_skill(&root);

    // The absolute path the handler substitutes for `references/guide.md`.
    let abs_ref = root.join(".entanglement/skills/demo/references/guide.md");

    let load_call = LlmResponse {
        text: "".into(),
        tool_calls: vec![ToolCall {
            id: "l1".into(),
            name: "load_skill".into(),
            input: r#"{"skill_name":"demo"}"#.into(),
            provider_meta: None,
        }],
    };
    // Turn 2 reads the substituted absolute path directly — no base-guessing.
    let read_call = LlmResponse {
        text: "".into(),
        tool_calls: vec![ToolCall {
            id: "r1".into(),
            name: "read".into(),
            input: serde_json::json!({ "path": abs_ref.to_string_lossy() }).to_string(),
            provider_meta: None,
        }],
    };
    let finish = LlmResponse {
        text: "done".into(),
        tool_calls: vec![],
    };

    let scripted = Arc::new(vec![load_call, read_call, finish]);
    // Point the user layer at a non-existent dir so a real ~/.config skill can't
    // leak in; the project layer under `root` supplies `demo`.
    let registry = {
        let _guard = crate::env_lock();
        std::env::set_var("ENTANGLEMENT_SKILLS_DIR", root.join("no-such-user-dir"));
        let registry = Arc::new(load_registry(&root).unwrap());
        std::env::remove_var("ENTANGLEMENT_SKILLS_DIR");
        registry
    };

    let mut tools = host_tools(root.clone());
    tools.register(LoadSkillTool::new(Arc::new(std::sync::RwLock::new(
        registry,
    ))));
    let cfg = EngineConfig {
        llm_factory: Arc::new(move || {
            Box::new(ScriptedLlm::new((*scripted).clone())) as Box<dyn Llm>
        }),
        tool_specs: tools.specs(),
        ..EngineConfig::default()
    };
    let holly = Holly::spawn(cfg);
    let _executor = spawn_tool_executor(
        &holly,
        tools,
        entanglement_runtime::agents::built_in_registry().expect("built-in agents must parse"),
        entanglement_core::PermissionProfile::new(entanglement_core::Permission::Allow),
    );
    let sid = SessionId::new("s1");
    let sub = holly.subscribe();
    let mut watch = holly.subscribe();
    holly
        .send(InMsg::prompt(sid.clone(), "use the demo skill"))
        .await
        .unwrap();

    // `load_skill` carries `Capability::Control` (ADR-0207 §3) but has no
    // dedicated `Intercept` route, so it grades through `build` mode's
    // `default: prompt` — no built-in mode writes an explicit `control`
    // rule. Approve it so the turn continues into the `read` of the
    // substituted ref (which build mode's `allow: [read, ...]` clears
    // unprompted).
    while let Ok(Ok(ev)) = tokio::time::timeout(Duration::from_secs(2), watch.recv()).await {
        if matches!(&ev, OutEvent::ToolRequest { tool, .. } if tool == "load_skill") {
            break;
        }
    }
    holly
        .send(InMsg::Approve {
            session: sid.clone(),
            request_id: "l1".into(),
            scope: entanglement_core::ApprovalScope::Once,
        })
        .await
        .unwrap();

    let events = collect(sub, &sid).await;
    let outputs: Vec<String> = events
        .iter()
        .filter_map(|e| match e {
            OutEvent::ToolOutput { output, .. } => Some(output.clone()),
            _ => None,
        })
        .collect();

    // The load_skill result carries the skill_id and the absolute-substituted ref.
    assert!(
        outputs.iter().any(|o| o.contains("skill_id: demo")),
        "expected a load_skill result; got {outputs:?}"
    );
    assert!(
        outputs
            .iter()
            .any(|o| o.contains(&abs_ref.display().to_string())),
        "relative ref must be substituted to absolute; got {outputs:?}"
    );
    // The subsequent read of that absolute path returned the guide body.
    assert!(
        outputs.iter().any(|o| o.contains("the guide body")),
        "read of the substituted ref must return its contents; got {outputs:?}"
    );
    assert!(events.iter().any(|e| matches!(e, OutEvent::Done { .. })));
}

#[tokio::test]
async fn load_skill_ignores_a_mode_rule_naming_it() {
    let id = std::process::id();
    let root = std::env::temp_dir().join(format!("entanglement-loadskill-deny-{id}"));
    std::fs::create_dir_all(&root).unwrap();
    struct Drop_(std::path::PathBuf);
    impl Drop for Drop_ {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    let _cleanup = Drop_(root.clone());
    write_project_skill(&root);

    let load_call = LlmResponse {
        text: "".into(),
        tool_calls: vec![ToolCall {
            id: "l1".into(),
            name: "load_skill".into(),
            input: r#"{"skill_name":"demo"}"#.into(),
            provider_meta: None,
        }],
    };
    let finish = LlmResponse {
        text: "done".into(),
        tool_calls: vec![],
    };
    let scripted = Arc::new(vec![load_call, finish]);
    let registry = {
        let _guard = crate::env_lock();
        std::env::set_var("ENTANGLEMENT_SKILLS_DIR", root.join("no-such-user-dir"));
        let registry = Arc::new(load_registry(&root).unwrap());
        std::env::remove_var("ENTANGLEMENT_SKILLS_DIR");
        registry
    };

    let mut tools = host_tools(root.clone());
    tools.register(LoadSkillTool::new(Arc::new(std::sync::RwLock::new(
        registry,
    ))));
    // A mode that denies `load_skill` by its literal name — inert, since
    // `load_skill` declares `Capability::Control` (ADR-0207 §3, gap 1 of
    // stage 4b): a Control tool is never graded, so this rule never even
    // runs. Named `"build"` so a fresh session picks it up as
    // `DEFAULT_MODE` with no `SetMode` call.
    let profiles =
        entanglement_runtime::agents::built_in_registry().expect("built-in agents must parse");
    let mode = entanglement_runtime::mode::Mode {
        name: "build".to_string(),
        default: Permission::Allow,
        rules: entanglement_runtime::mode::Rules::from_lists(&["load_skill".to_string()], &[], &[]),
        limits: entanglement_runtime::mode::Limits::default(),
        sandbox: None,
    };
    let mode_table = Arc::new(
        entanglement_runtime::mode::ModeTable::new(vec![mode]).expect("single-mode table is valid"),
    );
    let cfg = EngineConfig {
        llm_factory: Arc::new(move || {
            Box::new(ScriptedLlm::new((*scripted).clone())) as Box<dyn Llm>
        }),
        tool_specs: tools.specs(),
        profiles: profiles.clone(),
        ..EngineConfig::default()
    };
    let holly = Holly::spawn(cfg);
    let shared_tools = tools.shared();
    let active = Arc::new(Mutex::new(std::collections::HashMap::new()));
    let perm_modes = Arc::new(Mutex::new(std::collections::HashMap::new()));
    let resolver: Arc<dyn entanglement_runtime::policy::PermissionResolver> =
        Arc::new(entanglement_runtime::policy::ProfileResolver::new(
            perm_modes.clone(),
            mode_table,
            shared_tools.clone(),
            PermissionProfile::new(Permission::Allow),
            None,
        ));
    let grants: Arc<dyn entanglement_runtime::policy::GrantStore> =
        Arc::new(entanglement_runtime::policy::DefaultGrantStore::load());
    let _executor = entanglement_runtime::tool_runner::spawn_tool_executor_with_policy(
        &holly,
        shared_tools,
        entanglement_runtime::host::jobs::JobRegistry::new(),
        entanglement_runtime::retained_output::RetainedOutputRegistry::new(),
        entanglement_runtime::script_ops::ScriptRegistry::new(),
        Arc::new(std::sync::RwLock::new(profiles)),
        Arc::new(std::sync::RwLock::new(Arc::new(
            entanglement_runtime::skills::SkillRegistry::default(),
        ))),
        PermissionProfile::new(Permission::Allow),
        active,
        perm_modes,
        resolver,
        grants,
        Default::default(),
        None,
        entanglement_runtime::policy::SandboxConfig::none(),
        Arc::new(entanglement_runtime::plan_files::PlanFileRegistry::new()),
        None,
        None,
        None,
    );
    let sid = SessionId::new("s1");
    let sub = holly.subscribe();
    holly
        .send(InMsg::prompt(sid.clone(), "try the demo skill"))
        .await
        .unwrap();

    let events = collect(sub, &sid).await;
    assert!(
        events.iter().any(|e| matches!(
            e,
            OutEvent::ToolOutput { output, .. } if output.contains("skill_id")
        )),
        "load_skill is Capability::Control — a mode rule naming it must not \
         deny it; got {events:?}"
    );
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, OutEvent::ToolRequest { .. })),
        "a Control tool must never park an approval either; got {events:?}"
    );
}
