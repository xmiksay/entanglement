//! ADR-0207 §9: the advertised tools array no longer varies by agent — the
//! `agent`/`agent_send` spawn family is a constant roster now
//! ([`entanglement_runtime::subagent::agent_specs`], stage 5b), not a
//! per-profile table swapped on `SetAgent` the way `profile_tool_specs` used
//! to. Stage 5a found the exact failure mode this guards against: a spec
//! pushed onto a resolver-replaced list never reaches the model, so this
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
async fn advertised_tools_are_byte_identical_across_set_agent() {
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

    let sid = SessionId::new("s1");
    // Round 1: the session's starting agent (`build`, core's `DEFAULT_MODE`
    // fallback) — no explicit `SetAgent` needed for the first round.
    holly.send(InMsg::prompt(sid.clone(), "go")).await.unwrap();
    let build_names = recorded_at(&seen, 0).await;

    // Round 2: switch to `plan` mid-session and prompt again.
    holly
        .send(InMsg::SetAgent {
            session: sid.clone(),
            agent: "plan".into(),
        })
        .await
        .unwrap();
    holly
        .send(InMsg::prompt(sid.clone(), "go again"))
        .await
        .unwrap();
    let plan_names = recorded_at(&seen, 1).await;

    // Round 3: switch to the read-only leaf `explore` and prompt a third time.
    holly
        .send(InMsg::SetAgent {
            session: sid.clone(),
            agent: "explore".into(),
        })
        .await
        .unwrap();
    holly
        .send(InMsg::prompt(sid.clone(), "go once more"))
        .await
        .unwrap();
    let explore_names = recorded_at(&seen, 2).await;

    assert!(
        build_names.iter().any(|n| n == "agent"),
        "the agent tool must be advertised unconditionally; got {build_names:?}"
    );
    assert!(
        build_names.iter().any(|n| n == "agent_send"),
        "got {build_names:?}"
    );
    assert_eq!(
        build_names, plan_names,
        "SetAgent build->plan must not perturb the advertised array"
    );
    assert_eq!(
        plan_names, explore_names,
        "SetAgent plan->explore must not perturb the advertised array either — \
         explore is a spawn leaf in name only now (ADR-0207 §4/§6), not a \
         narrower advertised surface"
    );
}
