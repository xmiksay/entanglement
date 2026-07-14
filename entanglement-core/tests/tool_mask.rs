//! Physical per-agent tool restriction — advertisement half (#116, ADR-0038).
//!
//! A profile's `tools` allowlist / `disallowed_tools` denylist filters the
//! `ToolSpec`s advertised to the model at turn time. Here we assert that a
//! session running under the read-only `explore` profile (allowlist
//! `read`/`glob`/`grep`) never sees the `edit` schema in its `LlmRequest`,
//! whether reached via `SetAgent` or a sub-agent `Spawn`.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use entanglement_core::{
    stream_from_response, AgentMode, AgentProfile, EngineConfig, Holly, InMsg, Llm, LlmRequest,
    LlmResponse, LlmStream, Permission, PermissionProfile, SessionId, ToolSpec,
};

/// The read-only `explore` profile the runtime ships as `explore.md` — core no
/// longer carries it (#201), so these mask tests register it directly. A
/// `Subagent` leaf whose `read`/`glob`/`grep` allowlist masks out `edit`.
fn explore_profile() -> AgentProfile {
    AgentProfile {
        name: "explore".into(),
        description: "Read-only exploration agent.".into(),
        mode: AgentMode::Subagent,
        system_prompt: "You are a read-only exploration agent.".into(),
        model: None,
        permission: PermissionProfile::new(Permission::Deny)
            .with("read", Permission::Allow)
            .with("glob", Permission::Allow)
            .with("grep", Permission::Allow),
        tools: Some(vec!["read".into(), "glob".into(), "grep".into()]),
        disallowed_tools: Vec::new(),
        can_spawn: None,
        spawnable_agents: None,
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
    cfg.profiles.insert(explore_profile());
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
    holly
        .send(InMsg::Prompt {
            session: sid.clone(),
            text: "go".into(),
        })
        .await
        .unwrap();
    let names = first_recorded(&seen).await;
    assert!(names.iter().any(|n| n == "read"), "got {names:?}");
    assert!(names.iter().any(|n| n == "edit"), "got {names:?}");
}

#[tokio::test]
async fn explore_profile_hides_edit_via_set_agent() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let holly = Holly::spawn(recording_config(seen.clone()));
    let sid = SessionId::new("s1");
    holly
        .send(InMsg::SetAgent {
            session: sid.clone(),
            agent: "explore".into(),
        })
        .await
        .unwrap();
    holly
        .send(InMsg::Prompt {
            session: sid.clone(),
            text: "look around".into(),
        })
        .await
        .unwrap();
    let names = first_recorded(&seen).await;
    assert!(
        names.iter().any(|n| n == "read"),
        "explore must still see read; got {names:?}"
    );
    assert!(
        !names.iter().any(|n| n == "edit"),
        "explore's masked `edit` must not be advertised; got {names:?}"
    );
    // `update_plan`/`update_tasks` are runtime state tools now (#231, ADR-0049):
    // core advertises no plan/task built-ins at all, and the runtime withholds
    // them from `explore` via the mask + permission. Neither ever reaches the
    // model here (this config carries no such specs).
    assert!(
        !names
            .iter()
            .any(|n| n == "update_tasks" || n == "update_plan"),
        "core must not advertise plan/task built-ins; got {names:?}"
    );
}

#[tokio::test]
async fn spawned_explore_child_request_carries_no_edit_spec() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let holly = Holly::spawn(recording_config(seen.clone()));
    let parent = SessionId::new("parent");
    let child = SessionId::new("child");

    // Start the parent so it exists as the spawn target.
    holly
        .send(InMsg::Prompt {
            session: parent.clone(),
            text: "start".into(),
        })
        .await
        .unwrap();

    // Spawn a read-only `explore` child; its first turn should advertise the
    // masked set only.
    holly
        .send(InMsg::Spawn {
            session: child.clone(),
            parent: parent.clone(),
            agent: "explore".into(),
            prompt: "explore the tree".into(),
        })
        .await
        .unwrap();

    // Find the child's request among the recorded ones (the parent advertised
    // `edit`; the child must not).
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    let mut child_request: Option<Vec<String>> = None;
    while tokio::time::Instant::now() < deadline {
        // The child's request is the one lacking `edit` (parent has it).
        let all = seen.lock().unwrap().clone();
        if let Some(names) = all.iter().find(|names| !names.iter().any(|n| n == "edit")) {
            child_request = Some(names.clone());
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let names = child_request.expect("child request should have been recorded");
    assert!(
        names.iter().any(|n| n == "read"),
        "child still reads; got {names:?}"
    );
    assert!(
        !names.iter().any(|n| n == "edit"),
        "spawned explore child must not advertise edit; got {names:?}"
    );
}
