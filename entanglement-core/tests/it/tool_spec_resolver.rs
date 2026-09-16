//! Per-session dynamic tool specs (#308, ADR-0076).
//!
//! `EngineConfig.tool_spec_resolver` lets one `Holly` advertise a different
//! base tool surface per session — the seam a multi-tenant embedder needs to
//! vary each user's discovered MCP-server tools without one engine per user.
//! These tests assert the three load-bearing properties:
//!
//! * two concurrent sessions on one engine see **disjoint** tool sets;
//! * changing the resolver's backing data changes the advertised specs on the
//!   **next turn**, with no engine respawn;
//! * the resolver's output is advertised **verbatim** — the profile mask does
//!   not filter it, so the resolver is the single seam that shapes a session's
//!   base surface (a tool it must keep off a tenant's wire has to be omitted,
//!   not masked; masking only declines the call at dispatch).

use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use async_trait::async_trait;
use entanglement_core::{
    stream_from_response, AgentMode, AgentProfile, EngineConfig, Holly, InMsg, Llm, LlmRequest,
    LlmResponse, LlmStream, Permission, PermissionProfile, SessionId, SessionModel, ToolSpec,
};

/// Per-session log of the advertised tool-name lists, one inner `Vec` per
/// recorded request (in arrival order).
type SeenBySession = Arc<Mutex<HashMap<String, Vec<Vec<String>>>>>;

/// An LLM that records, per session, the tool names advertised in each request
/// (in arrival order), then replies with plain text so the turn ends at once.
/// Keyed by session so a two-tenant engine's requests stay distinguishable.
struct RecordingLlm {
    session: String,
    seen: SeenBySession,
}

#[async_trait]
impl Llm for RecordingLlm {
    async fn stream(&mut self, req: LlmRequest<'_>) -> anyhow::Result<LlmStream> {
        let names: Vec<String> = req.tools.iter().map(|t| t.name.clone()).collect();
        self.seen
            .lock()
            .unwrap()
            .entry(self.session.clone())
            .or_default()
            .push(names);
        Ok(stream_from_response(LlmResponse {
            text: "done".into(),
            tool_calls: vec![],
        }))
    }
}

/// The read-only `explore` profile the runtime ships as `explore.md`; core no
/// longer carries it (#201), so the mask test registers it directly. Its
/// `read`/`glob`/`grep` allowlist masks out anything else.
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

/// Poll until session `sid` has recorded at least `n` requests, returning them.
async fn recorded_at_least(seen: &SeenBySession, sid: &str, n: usize) -> Vec<Vec<String>> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    loop {
        if let Some(reqs) = seen.lock().unwrap().get(sid) {
            if reqs.len() >= n {
                return reqs.clone();
            }
        }
        if tokio::time::Instant::now() >= deadline {
            panic!("session `{sid}` recorded fewer than {n} requests");
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// Acceptance #1: two concurrent sessions on one `Holly` advertise disjoint
/// tool sets — the resolver keys the base specs on the session id, so user A's
/// tools never leak into user B's request.
#[tokio::test]
async fn two_sessions_see_disjoint_tool_sets() {
    let seen: Arc<Mutex<HashMap<String, Vec<Vec<String>>>>> = Arc::new(Mutex::new(HashMap::new()));
    let seen_factory = seen.clone();
    // The factory has no session context, so tag each `RecordingLlm` from a
    // per-build counter and rely on the resolver to be the session-varying part.
    let next_session = Arc::new(Mutex::new(vec!["alpha".to_string(), "beta".to_string()]));

    let mut cfg = EngineConfig {
        llm_factory: Arc::new(move || {
            let session = next_session.lock().unwrap().remove(0);
            Box::new(RecordingLlm {
                session,
                seen: seen_factory.clone(),
            }) as Box<dyn Llm>
        }),
        ..EngineConfig::default()
    };
    // Engine-global specs are deliberately empty — the resolver supplies the
    // whole base set, keyed per session.
    cfg.tool_spec_resolver = Some(Arc::new(|sid: &SessionId, _: SessionModel<'_>| {
        match sid.0.as_str() {
            "alpha" => vec![ToolSpec::new("alpha_tool", "only alpha's tool")],
            "beta" => vec![ToolSpec::new("beta_tool", "only beta's tool")],
            _ => vec![],
        }
    }));

    let holly = Holly::spawn(cfg);
    // Sessions must build in the order the factory hands out ids.
    let alpha = SessionId::new("alpha");
    holly.send(InMsg::prompt(alpha, "go")).await.unwrap();
    let alpha_reqs = recorded_at_least(&seen, "alpha", 1).await;

    let beta = SessionId::new("beta");
    holly.send(InMsg::prompt(beta, "go")).await.unwrap();
    let beta_reqs = recorded_at_least(&seen, "beta", 1).await;

    assert_eq!(alpha_reqs[0], vec!["alpha_tool".to_string()]);
    assert_eq!(beta_reqs[0], vec!["beta_tool".to_string()]);
    // Disjoint: neither session ever saw the other's tool.
    assert!(
        !alpha_reqs[0].iter().any(|n| n == "beta_tool"),
        "alpha leaked beta's tool: {alpha_reqs:?}"
    );
    assert!(
        !beta_reqs[0].iter().any(|n| n == "alpha_tool"),
        "beta leaked alpha's tool: {beta_reqs:?}"
    );
}

/// Acceptance #2: mutating the resolver's backing store changes the advertised
/// specs on the next turn — no engine respawn. Mirrors the documented snapshot
/// cache: an `Arc<RwLock<..>>` the embedder rehydrates from its store.
#[tokio::test]
async fn changing_backing_data_changes_specs_next_turn() {
    let seen: Arc<Mutex<HashMap<String, Vec<Vec<String>>>>> = Arc::new(Mutex::new(HashMap::new()));
    let seen_factory = seen.clone();
    let cache: Arc<RwLock<Vec<ToolSpec>>> =
        Arc::new(RwLock::new(vec![ToolSpec::new("before", "initial tool")]));
    let cache_resolver = cache.clone();

    let mut cfg = EngineConfig {
        llm_factory: Arc::new(move || {
            Box::new(RecordingLlm {
                session: "s".into(),
                seen: seen_factory.clone(),
            }) as Box<dyn Llm>
        }),
        ..EngineConfig::default()
    };
    cfg.tool_spec_resolver = Some(Arc::new(move |_sid: &SessionId, _: SessionModel<'_>| {
        cache_resolver.read().unwrap().clone()
    }));

    let holly = Holly::spawn(cfg);
    let sid = SessionId::new("s");

    holly.send(InMsg::prompt(sid.clone(), "one")).await.unwrap();
    let after_first = recorded_at_least(&seen, "s", 1).await;
    assert_eq!(after_first[0], vec!["before".to_string()]);

    // Rehydrate the embedder's snapshot; the very next turn must reflect it.
    *cache.write().unwrap() = vec![ToolSpec::new("after", "swapped tool")];

    holly.send(InMsg::prompt(sid.clone(), "two")).await.unwrap();
    let after_second = recorded_at_least(&seen, "s", 2).await;
    assert_eq!(
        after_second[1],
        vec!["after".to_string()],
        "second turn should see the swapped tool without a respawn"
    );
}

/// Acceptance #3: the resolver's output reaches the model **verbatim** — the
/// profile mask does not filter it. Under the read-only `explore` profile, a
/// resolver that emits `edit` still advertises `edit`; the mask binds at the
/// runtime's dispatch gate instead (see `entanglement-runtime`'s `tool_mask`
/// integration test for the attributed decline). This is the property a
/// multi-tenant embedder must design against: omit, don't mask.
#[tokio::test]
async fn resolver_output_is_advertised_verbatim_past_the_profile_mask() {
    let seen: Arc<Mutex<HashMap<String, Vec<Vec<String>>>>> = Arc::new(Mutex::new(HashMap::new()));
    let seen_factory = seen.clone();

    let mut cfg = EngineConfig {
        llm_factory: Arc::new(move || {
            Box::new(RecordingLlm {
                session: "s".into(),
                seen: seen_factory.clone(),
            }) as Box<dyn Llm>
        }),
        ..EngineConfig::default()
    };
    cfg.tool_spec_resolver = Some(Arc::new(|_sid: &SessionId, _: SessionModel<'_>| {
        vec![
            ToolSpec::new("read", "read a file"),
            ToolSpec::new("edit", "edit a file"),
        ]
    }));
    cfg.profiles.insert(explore_profile());

    let holly = Holly::spawn(cfg);
    let sid = SessionId::new("s");
    holly
        .send(InMsg::SetAgent {
            session: sid.clone(),
            agent: "explore".into(),
        })
        .await
        .unwrap();
    holly.send(InMsg::prompt(sid, "look")).await.unwrap();

    let reqs = recorded_at_least(&seen, "s", 1).await;
    let names = &reqs[0];
    assert!(
        names.iter().any(|n| n == "read"),
        "explore must still see resolver's read: {names:?}"
    );
    assert!(
        names.iter().any(|n| n == "edit"),
        "the mask no longer filters advertisement — `edit` is advertised and \
         declined at dispatch: {names:?}"
    );
}

/// The universal rule, pinned against the real advertisement path — this
/// subsumes ADR-0190's `poll`-only mask exemption, which no longer needs to
/// exist: under a profile whose allowlist admits *only* `read`, every spec the
/// resolver hands over still reaches the model, `poll` and `edit` alike. The
/// ADR-0190 Bug-1 half survives untouched: the production runtime carries
/// `poll_spec()` in its resolver roster (a spec absent there is a spec no model
/// ever sees), which `entanglement-runtime`'s own tests pin.
#[tokio::test]
async fn a_restrictive_profile_still_advertises_every_resolver_spec() {
    let seen: Arc<Mutex<HashMap<String, Vec<Vec<String>>>>> = Arc::new(Mutex::new(HashMap::new()));
    let seen_factory = seen.clone();

    let mut cfg = EngineConfig {
        llm_factory: Arc::new(move || {
            Box::new(RecordingLlm {
                session: "s".into(),
                seen: seen_factory.clone(),
            }) as Box<dyn Llm>
        }),
        ..EngineConfig::default()
    };
    cfg.tool_spec_resolver = Some(Arc::new(|_sid: &SessionId, _: SessionModel<'_>| {
        // Mirrors the runtime's resolver roster: a registry tool + the
        // runtime-owned pseudo-tools, including `poll`.
        vec![
            ToolSpec::new("read", "read a file"),
            ToolSpec::new("edit", "edit a file"),
            ToolSpec::new("poll", "join background jobs and sub-agents"),
        ]
    }));
    // A profile that masks everything but `read`: neither `poll` nor `edit` is
    // in its allowlist. Both are still advertised — the allowlist decides what
    // may *run*, not what the model can see.
    cfg.profiles.insert(AgentProfile {
        name: "locked".into(),
        description: "read-only".into(),
        mode: AgentMode::Primary,
        system_prompt: String::new(),
        model: None,
        provider: None,
        permission: PermissionProfile::new(Permission::Deny).with("read", Permission::Allow),
        tools: Some(vec!["read".into()]),
        disallowed_tools: Vec::new(),
        can_spawn: None,
        spawnable_agents: None,
        sandbox: None,
    });

    let holly = Holly::spawn(cfg);
    let sid = SessionId::new("s");
    holly
        .send(InMsg::SetAgent {
            session: sid.clone(),
            agent: "locked".into(),
        })
        .await
        .unwrap();
    holly.send(InMsg::prompt(sid, "look")).await.unwrap();

    let reqs = recorded_at_least(&seen, "s", 1).await;
    let names = &reqs[0];
    for tool in ["read", "edit", "poll"] {
        assert!(
            names.iter().any(|n| n == tool),
            "`{tool}` must be advertised even under a profile masking it: {names:?}"
        );
    }
}

/// The resolver is handed the session's bound model, and within a round the
/// tool specs resolve before the system prompt. The runtime pins its
/// tool-advertising facts at first resolution from that model and its prompt
/// resolver reads the pin (ADR-0204), so both halves are load-bearing: a
/// broadcast-driven pin raced round 1 and changed the tools array between
/// rounds, a full prompt-cache miss.
#[tokio::test]
async fn resolver_sees_the_bound_model_and_runs_before_the_prompt_resolver() {
    let seen: SeenBySession = Arc::new(Mutex::new(HashMap::new()));
    let order: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let seen_factory = seen.clone();
    let factory: entanglement_core::LlmFactory = Arc::new(move || {
        Box::new(RecordingLlm {
            session: "s".into(),
            seen: seen_factory.clone(),
        }) as Box<dyn Llm>
    });
    let mut cfg = EngineConfig {
        llm_factory: factory.clone(),
        ..EngineConfig::default()
    };
    cfg.model_resolver = Some(Arc::new(move |_user, provider: &str, model: &str| {
        Ok(entanglement_core::ResolvedModel {
            provider: provider.into(),
            model: model.into(),
            llm_factory: factory.clone(),
            generation: None,
            context_window: None,
        })
    }));
    let mut pinned = cfg
        .profiles
        .get("build")
        .cloned()
        .expect("default registry has build");
    pinned.provider = Some("p".into());
    pinned.model = Some("m".into());
    cfg.profiles.insert(pinned);
    let specs_order = order.clone();
    cfg.tool_spec_resolver = Some(Arc::new(move |_sid: &SessionId, m: SessionModel<'_>| {
        specs_order
            .lock()
            .unwrap()
            .push(format!("specs {:?}/{:?}", m.provider, m.model));
        vec![]
    }));
    let prompt_order = order.clone();
    cfg.system_prompt_resolver = Some(Arc::new(move |_sid: &SessionId, _p: &AgentProfile| {
        prompt_order.lock().unwrap().push("prompt".into());
        None
    }));

    let holly = Holly::spawn(cfg);
    holly
        .send(InMsg::prompt(SessionId::new("s"), "go"))
        .await
        .unwrap();
    recorded_at_least(&seen, "s", 1).await;

    let order = order.lock().unwrap().clone();
    assert_eq!(
        order[..2],
        [
            r#"specs Some("p")/Some("m")"#.to_string(),
            "prompt".to_string()
        ],
        "{order:?}"
    );
}
