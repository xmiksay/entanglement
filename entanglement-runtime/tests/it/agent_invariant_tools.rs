//! ADR-0207 §9: the advertised tools array no longer varies by agent — the
//! `agent`/`agent_send` spawn family is a constant roster now
//! ([`entanglement_runtime::subagent::agent_specs`], stage 5b), not a
//! per-profile table the old `profile_tool_specs` swapped on a live agent
//! switch (`SetAgent`, itself retired in stage 6a — an agent is chosen once,
//! at spawn). Stage 5a found the exact failure mode this guards against: a
//! spec pushed onto a resolver-replaced list never reaches the model, so this
//! drives a *real* engine + executor and asserts against what the LLM
//! actually receives — not `subagent::agent_specs`'s return value in
//! isolation.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use entanglement_core::{
    stream_from_response, EngineConfig, Holly, InMsg, Llm, LlmRequest, LlmResponse, LlmStream,
    SessionId,
};
use entanglement_runtime::tool_runner::spawn_tool_executor;
use entanglement_runtime::ToolRegistry;

/// Every request's advertised tool-name set, in arrival order.
struct RecordingLlm {
    seen: Arc<Mutex<Vec<Vec<String>>>>,
}

#[async_trait]
impl Llm for RecordingLlm {
    async fn stream(&mut self, req: LlmRequest<'_>) -> anyhow::Result<LlmStream> {
        let mut names: Vec<String> = req.tools.iter().map(|t| t.name.clone()).collect();
        names.sort();
        self.seen.lock().unwrap().push(names);
        Ok(stream_from_response(LlmResponse {
            text: "done".into(),
            tool_calls: vec![],
        }))
    }
}

async fn recorded_at(seen: &Arc<Mutex<Vec<Vec<String>>>>, index: usize) -> Vec<String> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    loop {
        if let Some(names) = seen.lock().unwrap().get(index).cloned() {
            return names;
        }
        if tokio::time::Instant::now() >= deadline {
            panic!("round {index} was never recorded");
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test]
async fn advertised_tools_are_byte_identical_across_agents() {
    let profiles =
        entanglement_runtime::agents::built_in_registry().expect("built-in agents must parse");
    // Mirrors `main.rs`'s wiring: the constant `agent`/`agent_send` roster
    // joins the plain shared `tool_specs`, exactly like `ask_user`/
    // `update_tasks` — not a per-profile table keyed by the active agent.
    let mut tool_specs = vec![
        entanglement_core::ToolSpec::new("read", "read a file"),
        entanglement_core::ToolSpec::new("edit", "edit a file"),
    ];
    tool_specs.extend(entanglement_runtime::subagent::agent_specs(&profiles));

    let seen = Arc::new(Mutex::new(Vec::new()));
    let seen_factory = seen.clone();
    let cfg = EngineConfig {
        llm_factory: Arc::new(move || {
            Box::new(RecordingLlm {
                seen: seen_factory.clone(),
            }) as Box<dyn Llm>
        }),
        tool_specs,
        profiles: profiles.clone(),
        ..EngineConfig::default()
    };
    let holly = Holly::spawn(cfg);
    spawn_tool_executor(
        &holly,
        ToolRegistry::new(),
        profiles,
        entanglement_core::PermissionProfile::new(entanglement_core::Permission::Allow),
    );

    // ADR-0207 §9: an agent is chosen once, at spawn, and fixed for the
    // session's whole life — so the "does the array vary by agent" question
    // is now asked across three separately-spawned sessions, not one session
    // switched three times.
    let general_sid = SessionId::new("s-general");
    holly
        .send(InMsg::prompt(general_sid.clone(), "go"))
        .await
        .unwrap();
    let general_names = recorded_at(&seen, 0).await;

    let plan_sid = SessionId::new("s-plan");
    holly
        .send(InMsg::Spawn {
            session: plan_sid.clone(),
            parent: None,
            predecessor: None,
            agent: "plan".into(),
            prompt: "go again".into(),
            user: None,
            sponsored: false,
        })
        .await
        .unwrap();
    let plan_names = recorded_at(&seen, 1).await;

    let debug_sid = SessionId::new("s-debug");
    holly
        .send(InMsg::Spawn {
            session: debug_sid.clone(),
            parent: None,
            predecessor: None,
            agent: "debug".into(),
            prompt: "go once more".into(),
            user: None,
            sponsored: false,
        })
        .await
        .unwrap();
    let debug_names = recorded_at(&seen, 2).await;

    assert!(
        general_names.iter().any(|n| n == "agent"),
        "the agent tool must be advertised unconditionally; got {general_names:?}"
    );
    assert!(
        general_names.iter().any(|n| n == "agent_send"),
        "got {general_names:?}"
    );
    assert_eq!(
        general_names, plan_names,
        "the advertised array must not vary between a `general` and a `plan` session"
    );
    assert_eq!(
        plan_names, debug_names,
        "the advertised array must not vary between a `plan` and a `debug` session either"
    );
}
