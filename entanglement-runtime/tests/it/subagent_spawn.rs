//! Integration tests for sub-agent spawn. Drives the real runtime tool
//! executor: the parent model calls `agent { background: true }`, which
//! returns a handle immediately (#89, ADR-0026; #606, ADR-0161), then `poll`
//! awaits the child's answer. The default (blocking) `agent` call (#120,
//! ADR-0033) spawns and waits in one call. Spawn limits (#76) and permission
//! gating (#77) still apply per launch.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use entanglement_core::{
    stream_from_response, AgentMode, AgentProfile, EngineConfig, Holly, InMsg, Llm, LlmRequest,
    LlmResponse, LlmStream, MessageRole, OutEvent, Permission, PermissionProfile, SessionId,
    ToolCall,
};
use entanglement_runtime::tool_runner::spawn_tool_executor;
use entanglement_runtime::ToolRegistry;
use tokio::sync::Notify;

/// Pull an `agent_id` out of a `background: true` `agent` result string
/// (format: `… agent_id: <uuid>. Call poll …`).
fn extract_agent_id(s: &str) -> Option<String> {
    let start = s.find("agent_id: ")? + "agent_id: ".len();
    let rest = &s[start..];
    let end = rest
        .find(|c: char| c == '.' || c.is_whitespace())
        .unwrap_or(rest.len());
    Some(rest[..end].to_string())
}

fn finish(text: &str) -> LlmStream {
    stream_from_response(LlmResponse {
        text: text.into(),
        tool_calls: vec![],
    })
}

fn call(id: &str, name: &str, input: String) -> LlmStream {
    stream_from_response(LlmResponse {
        text: String::new(),
        tool_calls: vec![ToolCall {
            id: id.into(),
            name: name.into(),
            input,
            provider_meta: None,
        }],
    })
}

/// The most recent tool-result text in the conversation, if any.
fn last_tool<'a>(req: &'a LlmRequest<'_>) -> Option<&'a str> {
    req.messages
        .iter()
        .rev()
        .find(|m| m.role == MessageRole::Tool)
        .and_then(|m| m.content.iter().find_map(|p| p.as_text()))
}

fn last_user<'a>(req: &'a LlmRequest<'_>) -> &'a str {
    req.messages
        .iter()
        .rev()
        .find(|m| m.role == MessageRole::User)
        .and_then(|m| m.content.iter().find_map(|p| p.as_text()))
        .unwrap_or("")
}

/// A content-routing LLM shared by the parent and its spawned child. The parent
/// launches a sub-agent with `background: true`, then polls its handle to
/// collect the answer; the child answers directly. Parameterized by the
/// profile the parent spawns under and the child's answer, so it drives the
/// limit/gating tests too.
struct SpawnPollLlm {
    target: &'static str,
    child_answer: &'static str,
}

#[async_trait]
impl Llm for SpawnPollLlm {
    async fn stream(&mut self, req: LlmRequest<'_>) -> anyhow::Result<LlmStream> {
        // A child session (its prompt is the spawn task) answers directly.
        if last_user(&req) == "child-task" && last_tool(&req).is_none() {
            return Ok(finish(self.child_answer));
        }
        match last_tool(&req) {
            // A successful launch → poll the returned handle.
            Some(t) => match extract_agent_id(t) {
                Some(id) => Ok(call(
                    "poll1",
                    "poll",
                    format!(r#"{{"handle":"{id}","timeout_secs":5}}"#),
                )),
                // A refusal (no handle) or a poll result → finish.
                None => Ok(finish("parent done")),
            },
            // First parent turn: launch a sub-agent, non-blocking. An empty
            // `target` omits the `agent` key to exercise the default-target
            // fill-in (`DEFAULT_SUBAGENT`).
            None => Ok(call(
                "spawn1",
                "agent",
                if self.target.is_empty() {
                    r#"{"prompt":"child-task","background":true}"#.to_string()
                } else {
                    format!(
                        r#"{{"agent":"{}","prompt":"child-task","background":true}}"#,
                        self.target
                    )
                },
            )),
        }
    }
}

fn config(make: impl Fn() -> SpawnPollLlm + Send + Sync + 'static) -> EngineConfig {
    EngineConfig {
        llm_factory: Arc::new(move || Box::new(make()) as Box<dyn Llm>),
        // Core carries only `build` now (#201); spawn tests target `explore`/`plan`,
        // so the engine needs the full runtime trio.
        profiles: entanglement_runtime::agents::built_in_registry()
            .expect("built-in agents must parse"),
        ..EngineConfig::default()
    }
}

#[tokio::test]
async fn spawn_launches_child_and_poll_collects_its_answer() {
    // Spawns `explore` (a valid Subagent-mode target). A `primary` like `build`
    // is no longer a spawnable target (#119): the target-mode gate refuses it.
    let cfg = config(|| SpawnPollLlm {
        target: "explore",
        child_answer: "child-answer",
    });
    let profiles = cfg.profiles.clone();
    let holly = Holly::spawn(cfg);
    // Empty registry: `agent`/`poll` are orchestration, handled
    // before execution.
    spawn_tool_executor(
        &holly,
        ToolRegistry::new(),
        profiles,
        entanglement_core::PermissionProfile::new(entanglement_core::Permission::Allow),
    );

    let parent = SessionId::new("parent");
    let mut sub = holly.subscribe();
    holly
        .send(InMsg::prompt(parent.clone(), "parent-task"))
        .await
        .unwrap();

    let mut child_started_under_parent = false;
    let mut saw_launch_handle = false;
    let mut saw_polled_answer = false;
    let mut parent_finished = false;
    while let Ok(Ok(ev)) = tokio::time::timeout(Duration::from_secs(5), sub.recv()).await {
        match &ev {
            OutEvent::SessionStarted {
                parent: Some(p),
                root: false,
                ..
            } if p == &parent => child_started_under_parent = true,
            OutEvent::ToolOutput {
                session,
                tool,
                output,
                ..
            } if session == &parent => {
                if tool == "agent" && output.contains("agent_id:") {
                    saw_launch_handle = true;
                }
                if tool == "poll" && output.contains("child-answer") {
                    saw_polled_answer = true;
                }
            }
            OutEvent::Done { session, .. } if session == &parent && saw_polled_answer => {
                parent_finished = true;
                break;
            }
            _ => {}
        }
    }

    assert!(
        child_started_under_parent,
        "a child session should start under the parent"
    );
    assert!(
        saw_launch_handle,
        "agent background=true should return an agent_id handle immediately"
    );
    assert!(
        saw_polled_answer,
        "poll should surface the child's answer to the parent"
    );
    assert!(
        parent_finished,
        "the parent should finish its turn after polling the sub-agent"
    );
}

/// A model that spawns two sub-agents in one turn, then polls both. Proves the
/// fan-out that non-blocking spawn enables: two live handles at once, both
/// answers collected. The children answer based on their prompt (`task-a` /
/// `task-b`).
struct FanOutLlm;

#[async_trait]
impl Llm for FanOutLlm {
    async fn stream(&mut self, req: LlmRequest<'_>) -> anyhow::Result<LlmStream> {
        // Children answer directly, keyed by their task prompt.
        if last_tool(&req).is_none() {
            match last_user(&req) {
                "task-a" => return Ok(finish("child-a")),
                "task-b" => return Ok(finish("child-b")),
                _ => {}
            }
        }
        let tool_msgs: Vec<&str> = req
            .messages
            .iter()
            .filter(|m| m.role == MessageRole::Tool)
            .filter_map(|m| m.content.iter().find_map(|p| p.as_text()))
            .collect();
        // Both polls have returned once both child answers are in the transcript.
        if tool_msgs.iter().any(|t| t.contains("child-a"))
            && tool_msgs.iter().any(|t| t.contains("child-b"))
        {
            return Ok(finish("parent done"));
        }
        // Launch handles are present but not yet polled → poll the next handle.
        let handles: Vec<String> = tool_msgs
            .iter()
            .filter_map(|t| extract_agent_id(t))
            .collect();
        let polled = tool_msgs
            .iter()
            .filter(|t| t.contains("completed in") || t.contains("still running"))
            .count();
        if let Some(id) = handles.get(polled) {
            return Ok(call(
                "poll",
                "poll",
                format!(r#"{{"handle":"{id}","timeout_secs":5}}"#),
            ));
        }
        if handles.is_empty() {
            // First parent turn: launch two sub-agents at once.
            return Ok(stream_from_response(LlmResponse {
                text: String::new(),
                tool_calls: vec![
                    ToolCall {
                        id: "s1".into(),
                        name: "agent".into(),
                        input: r#"{"agent":"explore","prompt":"task-a","background":true}"#.into(),
                        provider_meta: None,
                    },
                    ToolCall {
                        id: "s2".into(),
                        name: "agent".into(),
                        input: r#"{"agent":"explore","prompt":"task-b","background":true}"#.into(),
                        provider_meta: None,
                    },
                ],
            }));
        }
        Ok(finish("parent done"))
    }
}

#[tokio::test]
async fn two_sub_agents_fan_out_and_both_answers_are_polled() {
    let cfg = EngineConfig {
        llm_factory: Arc::new(|| Box::new(FanOutLlm) as Box<dyn Llm>),
        // Core carries only `build` now (#201); the spawn targets need the trio.
        profiles: entanglement_runtime::agents::built_in_registry()
            .expect("built-in agents must parse"),
        ..EngineConfig::default()
    };
    let profiles = cfg.profiles.clone();
    let holly = Holly::spawn(cfg);
    spawn_tool_executor(
        &holly,
        ToolRegistry::new(),
        profiles,
        entanglement_core::PermissionProfile::new(entanglement_core::Permission::Allow),
    );

    let parent = SessionId::new("parent");
    let mut sub = holly.subscribe();
    holly
        .send(InMsg::prompt(parent.clone(), "delegate"))
        .await
        .unwrap();

    let mut children = 0usize;
    let mut got_a = false;
    let mut got_b = false;
    let mut parent_finished = false;
    while let Ok(Ok(ev)) = tokio::time::timeout(Duration::from_secs(5), sub.recv()).await {
        match &ev {
            OutEvent::SessionStarted {
                parent: Some(p),
                root: false,
                ..
            } if p == &parent => children += 1,
            OutEvent::ToolOutput {
                session,
                tool,
                output,
                ..
            } if session == &parent && tool == "poll" => {
                if output.contains("child-a") {
                    got_a = true;
                }
                if output.contains("child-b") {
                    got_b = true;
                }
            }
            OutEvent::Done { session, .. } if session == &parent && got_a && got_b => {
                parent_finished = true;
                break;
            }
            _ => {}
        }
    }

    assert_eq!(children, 2, "the parent should launch two sub-agents");
    assert!(got_a && got_b, "both sub-agent answers should be collected");
    assert!(parent_finished, "the parent finishes after polling both");
}

#[tokio::test]
async fn spawn_depth_is_bounded_and_refusal_is_relayed() {
    // Every level spawns then polls, so the whole chain forms and unwinds before
    // the root finishes — even though each spawn returns without blocking. Uses an
    // `all`-mode `worker` (both a valid spawn *target* and able to spawn further,
    // #119), so the chain can recurse until the depth cap — not the mode gate —
    // refuses it.
    let mut profiles =
        entanglement_runtime::agents::built_in_registry().expect("built-in agents must parse");
    profiles.insert(all_mode_worker());
    let cfg = EngineConfig {
        llm_factory: Arc::new(|| Box::new(RecursiveLlm) as Box<dyn Llm>),
        profiles,
        ..EngineConfig::default()
    };
    let profiles = cfg.profiles.clone();
    let holly = Holly::spawn(cfg);
    spawn_tool_executor(
        &holly,
        ToolRegistry::new(),
        profiles,
        entanglement_core::PermissionProfile::new(entanglement_core::Permission::Allow),
    );

    let root = SessionId::new("root");
    let mut sub = holly.subscribe();
    holly
        .send(InMsg::prompt(root.clone(), "start"))
        .await
        .unwrap();

    let mut sessions_started = 0usize;
    let mut saw_depth_refusal = false;
    while let Ok(Ok(ev)) = tokio::time::timeout(Duration::from_secs(5), sub.recv()).await {
        match &ev {
            OutEvent::SessionStarted { .. } => sessions_started += 1,
            OutEvent::ToolOutput { output, .. } if output.contains("max spawn depth") => {
                saw_depth_refusal = true;
            }
            OutEvent::Done { session, .. } if session == &root => break,
            _ => {}
        }
    }

    assert!(
        saw_depth_refusal,
        "the deepest sub-agent's spawn should be refused with a max-depth message"
    );
    // root(0) + children at depth 1, 2, 3 = 4 sessions; the depth-3 spawn is refused.
    assert_eq!(
        sessions_started, 4,
        "the spawn tree should be capped at MAX_SPAWN_DEPTH below the root"
    );
}

/// An `all`-mode `worker`: a valid spawn *target* (subagent/all modes) that may
/// itself spawn (mode ≠ subagent), so a chain of workers can recurse until the
/// depth cap refuses it (#119).
fn all_mode_worker() -> AgentProfile {
    AgentProfile {
        name: "worker".into(),
        description: "recursive worker".into(),
        mode: AgentMode::All,
        system_prompt: String::new(),
        model: None,
        provider: None,
        permission: PermissionProfile::new(Permission::Allow),
        tools: None,
        disallowed_tools: Vec::new(),
        can_spawn: None,
        spawnable_agents: None,
        sandbox: None,
    }
}

/// Spawns a `worker` sub-agent on the first turn, polls its handle, and finishes
/// once a poll/refusal folds back in. Recurses because the child (same factory)
/// tries to spawn again — the depth guard must cap the chain.
struct RecursiveLlm;

#[async_trait]
impl Llm for RecursiveLlm {
    async fn stream(&mut self, req: LlmRequest<'_>) -> anyhow::Result<LlmStream> {
        match last_tool(&req) {
            Some(t) => match extract_agent_id(t) {
                Some(id) => Ok(call(
                    "poll",
                    "poll",
                    format!(r#"{{"handle":"{id}","timeout_secs":5}}"#),
                )),
                None => Ok(finish("done")),
            },
            None => Ok(call(
                "spawn",
                "agent",
                r#"{"agent":"worker","prompt":"recurse","background":true}"#.into(),
            )),
        }
    }
}

#[tokio::test]
async fn read_only_subagent_cannot_spawn() {
    // A Subagent-mode `explore` leaf is refused the spawn *capability* (#77),
    // whether the call is non-blocking (`background: true`) …
    assert_leaf_spawn_refused(true).await;
}

#[tokio::test]
async fn read_only_subagent_cannot_use_blocking_agent() {
    // … or the default blocking call (#120) — one guard path, so both flag
    // values are refused identically.
    assert_leaf_spawn_refused(false).await;
}

/// Drive a root that spawns a read-only `explore` child; the child (a
/// Subagent-mode leaf) tries to spawn again — with `background` either flag
/// value — and must be refused the capability. Asserts exactly one child
/// starts and the refusal is relayed.
async fn assert_leaf_spawn_refused(background: bool) {
    // Isolate the ADR-0024 capability gate from the #116 tool mask: give this
    // test's `explore` an allowlist that *advertises* `agent`, so the
    // mask does not preempt — the refusal must then come from the Subagent-mode
    // capability gate ("cannot spawn"), not the mask ("not available"). (The
    // stock `explore` masks `agent` too; that path is covered by the
    // `tool_mask` tests.)
    let mut profiles =
        entanglement_runtime::agents::built_in_registry().expect("built-in agents must parse");
    profiles.insert(AgentProfile {
        name: "explore".into(),
        description: "read-only leaf".into(),
        mode: AgentMode::Subagent,
        system_prompt: String::new(),
        model: None,
        provider: None,
        permission: PermissionProfile::new(Permission::Deny).with("read", Permission::Allow),
        tools: Some(vec![
            "read".into(),
            "glob".into(),
            "grep".into(),
            "agent".into(),
        ]),
        disallowed_tools: Vec::new(),
        can_spawn: None,
        spawnable_agents: None,
        sandbox: None,
    });
    let cfg = EngineConfig {
        llm_factory: Arc::new(move || Box::new(ExploreThenSpawnLlm { background }) as Box<dyn Llm>),
        profiles: profiles.clone(),
        ..EngineConfig::default()
    };
    let profiles = cfg.profiles.clone();
    let holly = Holly::spawn(cfg);
    spawn_tool_executor(
        &holly,
        ToolRegistry::new(),
        profiles,
        entanglement_core::PermissionProfile::new(entanglement_core::Permission::Allow),
    );

    let root = SessionId::new("root");
    let mut sub = holly.subscribe();
    holly
        .send(InMsg::prompt(root.clone(), "start"))
        .await
        .unwrap();

    let mut sessions_started = 0usize;
    let mut saw_capability_refusal = false;
    while let Ok(Ok(ev)) = tokio::time::timeout(Duration::from_secs(5), sub.recv()).await {
        match &ev {
            OutEvent::SessionStarted { .. } => sessions_started += 1,
            OutEvent::ToolOutput { output, .. } if output.contains("cannot spawn") => {
                saw_capability_refusal = true;
            }
            OutEvent::Done { session, .. } if session == &root => break,
            _ => {}
        }
    }

    assert!(
        saw_capability_refusal,
        "the explore child's `agent` call (background={background}) should be \
         refused as a capability"
    );
    // root(0) + one explore child = 2 sessions; the child's spawn never starts a grandchild.
    assert_eq!(
        sessions_started, 2,
        "a read-only sub-agent must not start a grandchild via `agent` (background={background})"
    );
}

/// The root spawns an `explore` sub-agent (non-blocking `background: true`)
/// and polls it; the child (same factory) tries to spawn again — with
/// `background` either flag value — and is refused, so the chain stops at one
/// child. Parametrized so both the non-blocking and blocking paths hit the
/// same guard.
struct ExploreThenSpawnLlm {
    background: bool,
}

#[async_trait]
impl Llm for ExploreThenSpawnLlm {
    async fn stream(&mut self, req: LlmRequest<'_>) -> anyhow::Result<LlmStream> {
        match last_tool(&req) {
            Some(t) => match extract_agent_id(t) {
                Some(id) => Ok(call(
                    "poll",
                    "poll",
                    format!(r#"{{"handle":"{id}","timeout_secs":5}}"#),
                )),
                None => Ok(finish("done")),
            },
            None => Ok(call(
                "spawn",
                "agent",
                format!(
                    r#"{{"agent":"explore","prompt":"look","background":{}}}"#,
                    self.background
                ),
            )),
        }
    }
}

/// Parent delegates once with the blocking `agent` tool; the child answers
/// directly. One round-trip: the parent's tool result already carries the answer,
/// so there is no separate poll (#120).
struct BlockingAgentLlm;

#[async_trait]
impl Llm for BlockingAgentLlm {
    async fn stream(&mut self, req: LlmRequest<'_>) -> anyhow::Result<LlmStream> {
        if last_user(&req) == "child-task" && last_tool(&req).is_none() {
            return Ok(finish("child-answer"));
        }
        match last_tool(&req) {
            // The blocking `agent` result already holds the child's answer → done.
            Some(_) => Ok(finish("parent done")),
            None => Ok(call(
                "agent1",
                "agent",
                r#"{"agent":"explore","prompt":"child-task"}"#.into(),
            )),
        }
    }
}

#[tokio::test]
async fn agent_blocks_and_returns_child_answer_in_one_call() {
    let cfg = EngineConfig {
        llm_factory: Arc::new(|| Box::new(BlockingAgentLlm) as Box<dyn Llm>),
        // Core carries only `build` now (#201); the spawn targets need the trio.
        profiles: entanglement_runtime::agents::built_in_registry()
            .expect("built-in agents must parse"),
        ..EngineConfig::default()
    };
    let profiles = cfg.profiles.clone();
    let holly = Holly::spawn(cfg);
    spawn_tool_executor(
        &holly,
        ToolRegistry::new(),
        profiles,
        entanglement_core::PermissionProfile::new(entanglement_core::Permission::Allow),
    );

    let parent = SessionId::new("parent");
    let mut sub = holly.subscribe();
    holly
        .send(InMsg::prompt(parent.clone(), "delegate"))
        .await
        .unwrap();

    let mut child_started = false;
    let mut agent_output_has_answer = false;
    let mut parent_finished = false;
    while let Ok(Ok(ev)) = tokio::time::timeout(Duration::from_secs(5), sub.recv()).await {
        match &ev {
            OutEvent::SessionStarted {
                parent: Some(p),
                root: false,
                ..
            } if p == &parent => child_started = true,
            OutEvent::ToolOutput {
                session,
                tool,
                output,
                ..
            } if session == &parent && tool == "agent" => {
                if output.contains("child-answer") {
                    agent_output_has_answer = true;
                }
            }
            OutEvent::Done { session, .. } if session == &parent && agent_output_has_answer => {
                parent_finished = true;
                break;
            }
            _ => {}
        }
    }

    assert!(
        child_started,
        "the blocking `agent` should start a child session"
    );
    assert!(
        agent_output_has_answer,
        "the `agent` tool output should carry the child's answer directly, in one call"
    );
    assert!(
        parent_finished,
        "the parent finishes after a single blocking `agent` call — no poll needed"
    );
}

/// Parent delegates with the blocking `agent` tool, but the child is gated on a
/// release signal so the parent is provably parked. After a `Stop`, the parent
/// re-asks with `poll` for the (now captured) child handle — proving the
/// answer stays collectable even though the blocking call was cancelled (#120).
struct StopThenPollLlm {
    release: Arc<Notify>,
    poll_id: Arc<Mutex<Option<String>>>,
}

#[async_trait]
impl Llm for StopThenPollLlm {
    async fn stream(&mut self, req: LlmRequest<'_>) -> anyhow::Result<LlmStream> {
        // Child: block until the test releases it, then answer.
        if last_user(&req) == "child-task" && last_tool(&req).is_none() {
            self.release.notified().await;
            return Ok(finish("late-child-answer"));
        }
        // Parent's second prompt: poll the captured handle for the parked child.
        if last_user(&req) == "poll-now" && last_tool(&req).is_none() {
            let id = self.poll_id.lock().unwrap().clone().unwrap_or_default();
            return Ok(call(
                "poll1",
                "poll",
                format!(r#"{{"handle":"{id}","timeout_secs":5}}"#),
            ));
        }
        // Any tool result folds back → finish.
        if last_tool(&req).is_some() {
            return Ok(finish("parent done"));
        }
        // Parent's first prompt: delegate with the blocking `agent` tool.
        Ok(call(
            "agent1",
            "agent",
            r#"{"agent":"explore","prompt":"child-task"}"#.into(),
        ))
    }
}

#[tokio::test]
async fn agent_stop_while_parked_cancels_and_child_stays_pollable() {
    let release = Arc::new(Notify::new());
    let poll_id: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let (r, p) = (release.clone(), poll_id.clone());
    let cfg = EngineConfig {
        llm_factory: Arc::new(move || {
            Box::new(StopThenPollLlm {
                release: r.clone(),
                poll_id: p.clone(),
            }) as Box<dyn Llm>
        }),
        // Core carries only `build` now (#201); the spawn target needs the trio.
        profiles: entanglement_runtime::agents::built_in_registry()
            .expect("built-in agents must parse"),
        ..EngineConfig::default()
    };
    let profiles = cfg.profiles.clone();
    let holly = Holly::spawn(cfg);
    spawn_tool_executor(
        &holly,
        ToolRegistry::new(),
        profiles,
        entanglement_core::PermissionProfile::new(entanglement_core::Permission::Allow),
    );

    let parent = SessionId::new("parent");
    let mut sub = holly.subscribe();
    holly
        .send(InMsg::prompt(parent.clone(), "delegate"))
        .await
        .unwrap();

    // Wait for the child to start (the `agent` call is now parked on it), and
    // capture the child's id — that handle is what a later `poll` needs.
    let child_id = loop {
        match tokio::time::timeout(Duration::from_secs(5), sub.recv())
            .await
            .expect("child should start")
            .unwrap()
        {
            OutEvent::SessionStarted {
                session,
                parent: Some(p),
                root: false,
                ..
            } if p == parent => break session.to_string(),
            _ => {}
        }
    };
    *poll_id.lock().unwrap() = Some(child_id);

    // Cancel the parent's turn while the blocking `agent` is parked (ADR-0017).
    holly
        .send(InMsg::Stop {
            session: parent.clone(),
        })
        .await
        .unwrap();
    // Now let the child finish; its answer is recorded into the registry even
    // though the parent's blocking call was cancelled.
    release.notify_one();

    // Re-ask: poll the captured handle. The answer must still be collectable.
    holly
        .send(InMsg::prompt(parent.clone(), "poll-now"))
        .await
        .unwrap();

    let mut polled_answer = false;
    let mut parent_finished = false;
    while let Ok(Ok(ev)) = tokio::time::timeout(Duration::from_secs(5), sub.recv()).await {
        match &ev {
            OutEvent::ToolOutput {
                session,
                tool,
                output,
                ..
            } if session == &parent && tool == "poll" && output.contains("late-child-answer") => {
                polled_answer = true;
            }
            OutEvent::Done { session, .. } if session == &parent && polled_answer => {
                parent_finished = true;
                break;
            }
            _ => {}
        }
    }

    assert!(
        polled_answer,
        "the cancelled `agent` child's answer must remain collectable via poll"
    );
    assert!(
        parent_finished,
        "the parent finishes its second turn after polling the parked child"
    );
}

/// Parent spawns an `explore` child (non-blocking), then polls it with
/// `timeout_secs: 0` — the indefinite-wait sentinel (ADR-0123). The child is
/// gated on a release signal so the poll is provably parked; the test releases
/// the child, then asserts the poll returned the answer (not a still-running
/// status) and that the parent turn completed. This is the behavioral proof that
/// `0` means "wait for notification," not the old "return immediately."
struct ZeroTimeoutPollLlm {
    release: Arc<Notify>,
}

#[async_trait]
impl Llm for ZeroTimeoutPollLlm {
    async fn stream(&mut self, req: LlmRequest<'_>) -> anyhow::Result<LlmStream> {
        // Child: block until the test releases it, then answer.
        if last_user(&req) == "child-task" && last_tool(&req).is_none() {
            self.release.notified().await;
            return Ok(finish("zero-timeout-child-answer"));
        }
        match last_tool(&req) {
            // A launch handle → poll with `timeout_secs: 0` (the sentinel).
            Some(t) => match extract_agent_id(t) {
                Some(id) => Ok(call(
                    "poll1",
                    "poll",
                    format!(r#"{{"handle":"{id}","timeout_secs":0}}"#),
                )),
                // A poll result → finish.
                None => Ok(finish("parent done")),
            },
            // First parent turn: launch a sub-agent, non-blocking.
            None => Ok(call(
                "spawn1",
                "agent",
                r#"{"agent":"explore","prompt":"child-task","background":true}"#.into(),
            )),
        }
    }
}

#[tokio::test]
async fn poll_zero_timeout_blocks_until_completion() {
    let release = Arc::new(Notify::new());
    let r = release.clone();
    let cfg = EngineConfig {
        llm_factory: Arc::new(move || {
            Box::new(ZeroTimeoutPollLlm { release: r.clone() }) as Box<dyn Llm>
        }),
        // Core carries only `build` now (#201); the spawn target needs the trio.
        profiles: entanglement_runtime::agents::built_in_registry()
            .expect("built-in agents must parse"),
        ..EngineConfig::default()
    };
    let profiles = cfg.profiles.clone();
    let holly = Holly::spawn(cfg);
    spawn_tool_executor(
        &holly,
        ToolRegistry::new(),
        profiles,
        entanglement_core::PermissionProfile::new(entanglement_core::Permission::Allow),
    );

    let parent = SessionId::new("parent");
    let mut sub = holly.subscribe();
    holly
        .send(InMsg::prompt(parent.clone(), "delegate"))
        .await
        .unwrap();

    // Drain until we've seen the spawn return a handle and a child start, but
    // NOT the poll result yet — the poll is parked on the child (the signal has
    // not been released). We confirm the park by observing the child is running
    // without a poll answer landing.
    let mut child_started = false;
    let mut saw_launch_handle = false;
    let mut saw_poll_answer = false;
    let deadline = std::time::Instant::now() + Duration::from_millis(300);
    while std::time::Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_millis(50), sub.recv()).await {
            Ok(Ok(OutEvent::SessionStarted {
                parent: Some(p),
                root: false,
                ..
            })) if p == parent => child_started = true,
            Ok(Ok(OutEvent::ToolOutput {
                session,
                tool,
                output,
                ..
            })) if session == parent => {
                if tool == "agent" && output.contains("agent_id:") {
                    saw_launch_handle = true;
                }
                if tool == "poll" {
                    saw_poll_answer = true;
                }
            }
            _ => {}
        }
    }
    assert!(child_started, "the child should start under the parent");
    assert!(
        saw_launch_handle,
        "agent background=true should return a handle"
    );
    assert!(
        !saw_poll_answer,
        "the timeout_secs:0 poll must NOT return while the child is still running"
    );

    // Release the child; the parked poll should now wake and surface the answer.
    release.notify_one();

    let mut parent_finished = false;
    while let Ok(Ok(ev)) = tokio::time::timeout(Duration::from_secs(5), sub.recv()).await {
        match &ev {
            OutEvent::ToolOutput {
                session,
                tool,
                output,
                ..
            } if session == &parent && tool == "poll" => {
                assert!(
                    output.contains("zero-timeout-child-answer"),
                    "the timeout_secs:0 poll should return the child's answer, got: {output}"
                );
                assert!(
                    !output.contains("still running"),
                    "timeout_secs:0 must block, not return a still-running status"
                );
                saw_poll_answer = true;
            }
            OutEvent::Done { session, .. } if session == &parent && saw_poll_answer => {
                parent_finished = true;
                break;
            }
            _ => {}
        }
    }

    assert!(saw_poll_answer, "the timeout_secs:0 poll should complete");
    assert!(
        parent_finished,
        "the parent finishes its turn after the indefinite-wait poll returns"
    );
}

/// A second Subagent-mode target, used to exercise the `spawnable_agents`
/// allowlist (a valid target that is nonetheless off a scoped spawner's list).
fn subagent_helper() -> AgentProfile {
    AgentProfile {
        name: "helper".into(),
        description: "a second subagent".into(),
        mode: AgentMode::Subagent,
        system_prompt: String::new(),
        model: None,
        provider: None,
        permission: PermissionProfile::new(Permission::Allow),
        tools: None,
        disallowed_tools: Vec::new(),
        can_spawn: None,
        spawnable_agents: None,
        sandbox: None,
    }
}

/// Drive a root (default `build` profile) whose model spawns once, and assert the
/// spawn is refused with `expected` in the `ToolOutput` and that **no** child
/// session starts (the refusal lands before a child is minted, #119).
async fn assert_root_spawn_refused(holly: &Holly, expected: &str) {
    let root = SessionId::new("root");
    let mut sub = holly.subscribe();
    holly
        .send(InMsg::prompt(root.clone(), "start"))
        .await
        .unwrap();

    let mut children = 0usize;
    let mut saw_refusal = false;
    while let Ok(Ok(ev)) = tokio::time::timeout(Duration::from_secs(5), sub.recv()).await {
        match &ev {
            OutEvent::SessionStarted {
                parent: Some(_), ..
            } => children += 1,
            OutEvent::ToolOutput { output, .. } if output.contains(expected) => {
                saw_refusal = true;
            }
            OutEvent::Done { session, .. } if session == &root => break,
            _ => {}
        }
    }

    assert!(
        saw_refusal,
        "expected a spawn refusal containing `{expected}`"
    );
    assert_eq!(
        children, 0,
        "no child session should start when the spawn is refused"
    );
}

#[tokio::test]
async fn spawn_of_a_primary_target_is_refused() {
    // `build` tries to spawn `plan`, a primary entry agent — the target-mode gate
    // refuses it before a child is minted (#119).
    let cfg = config(|| SpawnPollLlm {
        target: "plan",
        child_answer: "unused",
    });
    let profiles = cfg.profiles.clone();
    let holly = Holly::spawn(cfg);
    spawn_tool_executor(
        &holly,
        ToolRegistry::new(),
        profiles,
        entanglement_core::PermissionProfile::new(entanglement_core::Permission::Allow),
    );
    assert_root_spawn_refused(&holly, "primary entry agent").await;
}

#[tokio::test]
async fn spawn_outside_the_allowlist_is_refused() {
    // A `build` scoped to spawn only `explore` tries to spawn `helper` (a valid
    // Subagent target, but off-list) → refused with the reason in the output.
    let mut profiles =
        entanglement_runtime::agents::built_in_registry().expect("built-in agents must parse");
    let mut build = profiles.get("build").unwrap().clone();
    build.spawnable_agents = Some(vec!["explore".into()]);
    profiles.insert(build);
    profiles.insert(subagent_helper());
    let cfg = EngineConfig {
        llm_factory: Arc::new(|| {
            Box::new(SpawnPollLlm {
                target: "helper",
                child_answer: "unused",
            }) as Box<dyn Llm>
        }),
        profiles: profiles.clone(),
        ..EngineConfig::default()
    };
    let profiles = cfg.profiles.clone();
    let holly = Holly::spawn(cfg);
    spawn_tool_executor(
        &holly,
        ToolRegistry::new(),
        profiles,
        entanglement_core::PermissionProfile::new(entanglement_core::Permission::Allow),
    );
    assert_root_spawn_refused(&holly, "not allowed to spawn").await;
}

#[tokio::test]
async fn primary_with_can_spawn_false_cannot_spawn() {
    // `can_spawn: false` on a primary withholds the whole family and refuses a
    // stale call — even for an otherwise-valid target like `explore` (#119).
    let mut profiles =
        entanglement_runtime::agents::built_in_registry().expect("built-in agents must parse");
    let mut build = profiles.get("build").unwrap().clone();
    build.can_spawn = Some(false);
    profiles.insert(build);
    let cfg = EngineConfig {
        llm_factory: Arc::new(|| {
            Box::new(SpawnPollLlm {
                target: "explore",
                child_answer: "unused",
            }) as Box<dyn Llm>
        }),
        profiles: profiles.clone(),
        ..EngineConfig::default()
    };
    let profiles = cfg.profiles.clone();
    let holly = Holly::spawn(cfg);
    spawn_tool_executor(
        &holly,
        ToolRegistry::new(),
        profiles,
        entanglement_core::PermissionProfile::new(entanglement_core::Permission::Allow),
    );
    assert_root_spawn_refused(&holly, "cannot spawn").await;
}

/// Switch `session` to `agent` (auto-creating the session actor) and wait for
/// the `AgentChanged` ack, so the next prompt runs under that profile.
async fn set_agent(holly: &Holly, session: &SessionId, agent: &str) {
    let mut sub = holly.subscribe();
    holly
        .send(InMsg::SetAgent {
            session: session.clone(),
            agent: agent.into(),
        })
        .await
        .unwrap();
    while let Ok(Ok(ev)) = tokio::time::timeout(Duration::from_secs(5), sub.recv()).await {
        if matches!(&ev, OutEvent::AgentChanged { agent: a, .. } if a == agent) {
            return;
        }
    }
    panic!("no AgentChanged ack for `{agent}`");
}

#[tokio::test]
async fn research_spawns_research() {
    // ADR-0167: `research` (`mode: all`) is both an entry agent and its own
    // spawn target — a research root delegating to a research child works end
    // to end, and the child runs under the `research` profile.
    let cfg = config(|| SpawnPollLlm {
        target: "research",
        child_answer: "child-answer",
    });
    let profiles = cfg.profiles.clone();
    let holly = Holly::spawn(cfg);
    spawn_tool_executor(
        &holly,
        ToolRegistry::new(),
        profiles,
        entanglement_core::PermissionProfile::new(entanglement_core::Permission::Allow),
    );

    let root = SessionId::new("root");
    set_agent(&holly, &root, "research").await;
    let mut sub = holly.subscribe();
    holly
        .send(InMsg::prompt(root.clone(), "parent-task"))
        .await
        .unwrap();

    let mut child_profile: Option<String> = None;
    let mut saw_polled_answer = false;
    while let Ok(Ok(ev)) = tokio::time::timeout(Duration::from_secs(5), sub.recv()).await {
        match &ev {
            OutEvent::SessionStarted {
                parent: Some(p),
                profile,
                root: false,
                ..
            } if p == &root => child_profile = Some(profile.clone()),
            OutEvent::ToolOutput {
                session,
                tool,
                output,
                ..
            } if session == &root && tool == "poll" && output.contains("child-answer") => {
                saw_polled_answer = true;
            }
            OutEvent::Done { session, .. } if session == &root && saw_polled_answer => break,
            _ => {}
        }
    }

    assert_eq!(
        child_profile.as_deref(),
        Some("research"),
        "the child should run under the `research` profile"
    );
    assert!(saw_polled_answer, "poll should surface the child's answer");
}

#[tokio::test]
async fn research_spawn_of_non_research_is_refused() {
    // ADR-0167: `explore` is a perfectly valid Subagent-mode target, but it is
    // off research's self-only allowlist — refused before a child is minted,
    // so the research subtree can never widen into another profile.
    let cfg = config(|| SpawnPollLlm {
        target: "explore",
        child_answer: "unused",
    });
    let profiles = cfg.profiles.clone();
    let holly = Holly::spawn(cfg);
    spawn_tool_executor(
        &holly,
        ToolRegistry::new(),
        profiles,
        entanglement_core::PermissionProfile::new(entanglement_core::Permission::Allow),
    );

    let root = SessionId::new("root");
    set_agent(&holly, &root, "research").await;
    assert_root_spawn_refused(&holly, "not allowed to spawn").await;
}

#[tokio::test]
async fn research_spawn_without_agent_falls_to_default_and_is_refused() {
    // A spawn omitting `agent` falls to `DEFAULT_SUBAGENT` (`explore`), which
    // research's self-only allowlist refuses — the prompt body tells the model
    // to always name `agent: research` explicitly.
    let cfg = config(|| SpawnPollLlm {
        target: "",
        child_answer: "unused",
    });
    let profiles = cfg.profiles.clone();
    let holly = Holly::spawn(cfg);
    spawn_tool_executor(
        &holly,
        ToolRegistry::new(),
        profiles,
        entanglement_core::PermissionProfile::new(entanglement_core::Permission::Allow),
    );

    let root = SessionId::new("root");
    set_agent(&holly, &root, "research").await;
    assert_root_spawn_refused(&holly, "not allowed to spawn").await;
}

#[test]
fn specs_advertise_the_agent_tool_with_a_background_flag() {
    // #606, ADR-0161 §1: `agent_spawn` is retired — one `agent` tool carries a
    // `background` flag instead. The family is per-profile (#119):
    // `spawn_specs_for` scopes the roster + enum to who the spawning profile
    // may target.
    let reg =
        entanglement_runtime::agents::built_in_registry().expect("built-in agents must parse");
    let build = reg.get("build").unwrap();
    let specs = entanglement_runtime::subagent::spawn_specs_for(build, &reg);
    let names: Vec<&str> = specs.iter().map(|s| s.name.as_str()).collect();
    // `poll` (#605) is no longer part of the per-profile spawn family — it
    // rides the shared specs like `ask_user`, since it also joins non-spawn
    // job handles. `agent_send` (#609, ADR-0162) rides alongside `agent`
    // here instead, since it's equally only useful to a spawn-capable
    // profile.
    assert_eq!(names, vec!["agent", "agent_send"]);
    let agent = &specs[0];
    // The scoped roster is disclosed in both the description and the enum: only
    // spawnable targets (explore), never the primaries (build/plan).
    assert!(
        agent.description.contains("explore:"),
        "roster in description"
    );
    let enum_names = agent.schema["properties"]["agent"]["enum"]
        .as_array()
        .unwrap();
    assert!(enum_names.iter().any(|n| n == "explore"));
    assert!(!enum_names.iter().any(|n| n == "build"));
    assert_eq!(
        agent.schema["properties"]["background"]["type"],
        serde_json::json!("boolean"),
        "the input schema carries a background flag"
    );
}
