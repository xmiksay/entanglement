//! Advertisement is decoupled from enforcement (#116, ADR-0038 revisited).
//!
//! Core's turn loop advertises **every** spec the config provides — the profile
//! mask and the session tool overlay no longer filter it. WHY: a surface that
//! changes mid-session (an overlay toggle, `SetAgent` to a differently-masked
//! profile, a live tool enable) invalidates the provider's prompt cache from
//! the tools block onward, i.e. the whole prompt. The mask still binds, but at
//! the runtime's dispatch gate: the enforcement half is
//! `entanglement-runtime/tests/it/tool_mask.rs`, which pins the attributed
//! decline every masked call now gets.
//!
//! These tests pin the advertisement half of that contract: a restrictive
//! profile, an overlay deny, and a spawned read-only child all still see the
//! full schema set.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use entanglement_core::{
    stream_from_response, AgentMode, AgentProfile, EngineConfig, Holly, InMsg, Llm, LlmRequest,
    LlmResponse, LlmStream, Permission, PermissionProfile, SessionId, ToolOverlayEntry, ToolSpec,
};

/// The read-only `explore` profile the runtime ships as `explore.md` — core no
/// longer carries it (#201), so these tests register it directly. A `Subagent`
/// leaf whose `read`/`glob`/`grep` allowlist masks out `edit` **at dispatch**.
fn explore_profile() -> AgentProfile {
    AgentProfile {
        name: "explore".into(),
        description: "Read-only exploration agent.".into(),
        mode: AgentMode::Subagent,
        system_prompt: "You are a read-only exploration agent.".into(),
        model: None,
        provider: None,
        permission: PermissionProfile::new(Permission::Deny)
            .with("read", Permission::Allow)
            .with("glob", Permission::Allow)
            .with("grep", Permission::Allow),
        tools: Some(vec!["read".into(), "glob".into(), "grep".into()]),
        disallowed_tools: Vec::new(),
        can_spawn: None,
        spawnable_agents: None,
        sandbox: None,
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
    holly.send(InMsg::prompt(sid.clone(), "go")).await.unwrap();
    let names = first_recorded(&seen).await;
    assert!(names.iter().any(|n| n == "read"), "got {names:?}");
    assert!(names.iter().any(|n| n == "edit"), "got {names:?}");
}

#[tokio::test]
async fn restrictive_profile_still_advertises_the_full_set() {
    // The rewrite of the old `explore_profile_hides_edit_via_set_agent`: under
    // a `read`/`glob`/`grep` allowlist, `edit`'s schema still reaches the model
    // — the mask binds at dispatch, not here, so switching agents mid-session
    // leaves the advertised array (and the provider's prompt cache) untouched.
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

#[tokio::test]
async fn setting_a_tool_overlay_does_not_perturb_the_advertised_set() {
    // The rewrite of `tool_overlay_injects_past_the_profile_mask` +
    // `tool_overlay_deny_withdraws_a_profile_advertised_tool` (#539, ADR-0149).
    // The overlay is still the per-session escape hatch, but it moves the
    // *grade*/existence decision at dispatch only: setting, then clearing, an
    // overlay leaves the advertised array byte-identical across all three
    // turns — precisely the prompt-cache stability this change buys.
    let seen = Arc::new(Mutex::new(Vec::new()));
    let mut cfg = recording_config(seen.clone());
    cfg.tool_specs
        .push(ToolSpec::new("mcp__docs__search", "search the docs server"));
    let holly = Holly::spawn(cfg);
    let mut events = holly.subscribe();
    let sid = SessionId::new("s1");
    holly
        .send(InMsg::SetAgent {
            session: sid.clone(),
            agent: "explore".into(),
        })
        .await
        .unwrap();
    holly
        .send(InMsg::prompt(sid.clone(), "before the overlay"))
        .await
        .unwrap();
    let baseline = first_recorded(&seen).await;
    assert!(
        ["read", "edit", "mcp__docs__search"]
            .iter()
            .all(|t| baseline.iter().any(|n| n == t)),
        "everything the config provides is advertised; got {baseline:?}"
    );

    // A deny entry for a tool the profile advertises, plus an enable entry for
    // one it masks: neither moves the needle on advertisement.
    holly
        .send(InMsg::SetToolOverlay {
            session: sid.clone(),
            entries: vec![
                ToolOverlayEntry::deny("read"),
                ToolOverlayEntry::ask("mcp__docs__*"),
            ],
        })
        .await
        .unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    loop {
        tokio::select! {
            ev = events.recv() => {
                if let Ok(entanglement_core::OutEvent::ToolOverlayChanged { entries, .. }) = ev {
                    assert_eq!(entries.len(), 2);
                    break;
                }
            }
            _ = tokio::time::sleep_until(deadline) => panic!("no ToolOverlayChanged seen"),
        }
    }
    holly
        .send(InMsg::prompt(sid.clone(), "with the overlay"))
        .await
        .unwrap();
    let with_overlay = recorded_at_least(&seen, 2).await;
    assert_eq!(
        with_overlay[1], baseline,
        "an overlay must not rewrite the advertised tools array"
    );

    // Clearing it is likewise inert on the wire.
    holly
        .send(InMsg::SetToolOverlay {
            session: sid.clone(),
            entries: vec![],
        })
        .await
        .unwrap();
    holly
        .send(InMsg::prompt(sid.clone(), "after clearing"))
        .await
        .unwrap();
    let cleared = recorded_at_least(&seen, 3).await;
    assert_eq!(
        cleared[2], baseline,
        "clearing an overlay must not rewrite it either"
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
            sponsored: false,
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
