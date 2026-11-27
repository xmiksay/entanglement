//! Advertisement is decoupled from enforcement (#116, ADR-0038 revisited).
//!
//! Core's turn loop advertises **every** spec the config provides — the profile
//! mask no longer filters it. WHY: a surface that changes mid-session (a live
//! tool enable) invalidates the
//! provider's prompt cache from the tools block onward, i.e. the whole prompt
//! — moot for the agent axis specifically now anyway, since an agent is fixed
//! for a session's whole life (ADR-0207 §9). The mask still binds, but at
//! the runtime's dispatch gate: the enforcement half is
//! `entanglement-runtime/tests/it/tool_mask.rs`, which pins the attributed
//! decline every masked call now gets.
//!
//! These tests pin the advertisement half of that contract: a restrictive
//! profile and a spawned read-only child both still see the
//! full schema set.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use entanglement_core::{
    stream_from_response, Agent, EngineConfig, Holly, InMsg, Llm, LlmRequest, LlmResponse,
    LlmStream, SessionId, ToolSpec,
};

/// The read-only `explore` profile the runtime ships as `explore.md` — core no
/// longer carries it (#201), so these tests register it directly. Its
/// read-only posture is a runtime permission-mode fact now (ADR-0207); the
/// profile itself carries no mask or permission rules any more, so it's
/// identical in shape to any other `Subagent` leaf.
fn explore_profile() -> Agent {
    Agent {
        name: "explore".into(),
        description: "Read-only exploration agent.".into(),
        system_prompt: "You are a read-only exploration agent.".into(),
        model: None,
        provider: None,
        default_mode: None,
    }
}

/// An LLM that records the tool names advertised in each request, then replies
/// with plain text so the turn ends immediately.
struct RecordingLlm {
    seen: Arc<Mutex<Vec<Vec<String>>>>,
}

#[async_trait]
impl Llm for RecordingLlm {
    async fn stream(&mut self, req: LlmRequest<'_>) -> anyhow::Result<LlmStream> {
        let names: Vec<String> = req.tools.iter().map(|t| t.name.clone()).collect();
        self.seen.lock().unwrap().push(names);
        Ok(stream_from_response(LlmResponse {
            text: "done".into(),
            tool_calls: vec![],
            ..Default::default()
        }))
    }
}

/// EngineConfig whose host tool_specs are `read` + `edit`, and whose LLM records
/// the advertised tool set of every request into `seen`.
fn recording_config(seen: Arc<Mutex<Vec<Vec<String>>>>) -> EngineConfig {
    let mut cfg = EngineConfig {
        llm_factory: Arc::new(move || {
            Box::new(RecordingLlm { seen: seen.clone() }) as Box<dyn Llm>
        }),
        ..EngineConfig::default()
    };
    cfg.tool_specs = vec![
        ToolSpec::new("read", "read a file"),
        ToolSpec::new("edit", "edit a file"),
    ];
    cfg.agents.insert(explore_profile());
    cfg
}

/// Wait until at least one request has been recorded, then return its tool set.
async fn first_recorded(seen: &Arc<Mutex<Vec<Vec<String>>>>) -> Vec<String> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    loop {
        if let Some(names) = seen.lock().unwrap().first().cloned() {
            return names;
        }
        if tokio::time::Instant::now() >= deadline {
            panic!("no LLM request was recorded");
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test]
async fn build_profile_advertises_edit() {
    // Sanity: the unmasked default `build` profile sees the full host set.
    let seen = Arc::new(Mutex::new(Vec::new()));
    let holly = Holly::spawn(recording_config(seen.clone()));
    let sid = SessionId::new("s1");
    holly.send(InMsg::prompt(sid.clone(), "go")).await.unwrap();
    let names = first_recorded(&seen).await;
    assert!(names.iter().any(|n| n == "read"), "got {names:?}");
    assert!(names.iter().any(|n| n == "edit"), "got {names:?}");
}

#[tokio::test]
async fn restrictive_profile_still_advertises_the_full_set() {
    // The rewrite of the old `explore_profile_hides_edit_via_set_agent`: a
    // read-only-named profile (`explore`, whose restriction is a runtime
    // permission-mode fact, not anything `Agent` carries, ADR-0207)
    // still sees `edit`'s schema — spawning under a differently-postured
    // agent leaves the advertised array (and the provider's prompt cache)
    // untouched.
    let seen = Arc::new(Mutex::new(Vec::new()));
    let holly = Holly::spawn(recording_config(seen.clone()));
    let sid = SessionId::new("s1");
    holly
        .send(InMsg::Spawn {
            session: sid.clone(),
            parent: None,
            predecessor: None,
            agent: "explore".into(),
            prompt: String::new(),
            user: None,
            model: None,
            mode: None,
            workdir: None,
        })
        .await
        .unwrap();
    holly
        .send(InMsg::prompt(sid.clone(), "look around"))
        .await
        .unwrap();
    let names = first_recorded(&seen).await;
    assert!(
        names.iter().any(|n| n == "read"),
        "explore must still see read; got {names:?}"
    );
    assert!(
        names.iter().any(|n| n == "edit"),
        "a masked tool is advertised and declined at dispatch, not withheld; got {names:?}"
    );
    // `update_tasks`/`propose_plan` are runtime state tools (#231, ADR-0049):
    // core advertises no plan/task built-ins of its own, and this config
    // carries no such specs, so neither can appear.
    assert!(
        !names
            .iter()
            .any(|n| n == "update_tasks" || n == "propose_plan"),
        "core must not advertise plan/task built-ins; got {names:?}"
    );
}

/// Poll until at least `n` requests have been recorded, then return them all.
async fn recorded_at_least(seen: &Arc<Mutex<Vec<Vec<String>>>>, n: usize) -> Vec<Vec<String>> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    loop {
        let all = seen.lock().unwrap().clone();
        if all.len() >= n {
            return all;
        }
        if tokio::time::Instant::now() >= deadline {
            panic!("fewer than {n} requests recorded");
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test]
async fn spawned_explore_child_advertises_the_same_set_as_its_parent() {
    // The rewrite of `spawned_explore_child_request_carries_no_edit_spec`: a
    // read-only child's *capability* clamp is the runtime's ancestor-chain mask
    // walk, which declines `edit` at dispatch. Its advertised surface is the
    // same as everyone else's.
    let seen = Arc::new(Mutex::new(Vec::new()));
    let holly = Holly::spawn(recording_config(seen.clone()));
    let parent = SessionId::new("parent");
    let child = SessionId::new("child");

    holly
        .send(InMsg::prompt(parent.clone(), "start"))
        .await
        .unwrap();
    holly
        .send(InMsg::Spawn {
            session: child.clone(),
            parent: Some(parent.clone()),
            predecessor: None,
            agent: "explore".into(),
            prompt: "explore the tree".into(),
            user: None,
            model: None,
            mode: None,
            workdir: None,
        })
        .await
        .unwrap();

    let requests = recorded_at_least(&seen, 2).await;
    for names in &requests {
        assert!(
            names.iter().any(|n| n == "read") && names.iter().any(|n| n == "edit"),
            "parent and child advertise the same full set; got {names:?}"
        );
    }
}
