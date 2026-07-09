//! Integration tests for sub-agent spawn. Drives the real runtime tool
//! executor: the parent model calls `spawn_agent`, which now returns a handle
//! immediately (#89, ADR-0026), then `agent_poll` awaits the child's answer.
//! Spawn limits (#76) and permission gating (#77) still apply per launch.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use entanglement_core::{
    stream_from_response, EngineConfig, Holly, InMsg, Llm, LlmRequest, LlmResponse, LlmSession,
    LlmStream, MessageRole, OutEvent, SessionId, ToolCall, ToolRegistry,
};
use entanglement_runtime::tool_runner::spawn_tool_executor;

/// Pull an `agent_id` out of a `spawn_agent` result string (format:
/// `… agent_id: <uuid>. Call agent_poll …`).
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
        }],
    })
}

/// The most recent tool-result text in the conversation, if any.
fn last_tool<'a>(req: &'a LlmRequest<'_>) -> Option<&'a str> {
    req.messages
        .iter()
        .rev()
        .find(|m| m.role == MessageRole::Tool)
        .map(|m| m.text.as_str())
}

fn last_user<'a>(req: &'a LlmRequest<'_>) -> &'a str {
    req.messages
        .iter()
        .rev()
        .find(|m| m.role == MessageRole::User)
        .map(|m| m.text.as_str())
        .unwrap_or("")
}

/// A content-routing LLM shared by the parent and its spawned child. The parent
/// launches a sub-agent, then polls its handle to collect the answer; the child
/// answers directly. Parameterized by the profile the parent spawns under and
/// the child's answer, so it drives the limit/gating tests too.
struct SpawnPollLlm {
    spawn_agent: &'static str,
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
                    "agent_poll",
                    format!(r#"{{"agent_id":"{id}","timeout_secs":5}}"#),
                )),
                // A refusal (no handle) or a poll result → finish.
                None => Ok(finish("parent done")),
            },
            // First parent turn: launch a sub-agent.
            None => Ok(call(
                "spawn1",
                "spawn_agent",
                format!(
                    r#"{{"agent":"{}","prompt":"child-task"}}"#,
                    self.spawn_agent
                ),
            )),
        }
    }
}

fn config(make: impl Fn() -> SpawnPollLlm + Send + Sync + 'static) -> EngineConfig {
    EngineConfig {
        llm_factory: Arc::new(move || LlmSession::new(Box::new(make()))),
        ..EngineConfig::default()
    }
}

#[tokio::test]
async fn spawn_launches_child_and_poll_collects_its_answer() {
    let cfg = config(|| SpawnPollLlm {
        spawn_agent: "build",
        child_answer: "child-answer",
    });
    let profiles = cfg.profiles.clone();
    let holly = Holly::spawn(cfg);
    // Empty registry: `spawn_agent`/`agent_poll` are orchestration, handled
    // before execution.
    spawn_tool_executor(&holly, ToolRegistry::new(), profiles);

    let parent = SessionId::new("parent");
    let mut sub = holly.subscribe();
    holly
        .send(InMsg::Prompt {
            session: parent.clone(),
            text: "parent-task".into(),
        })
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
                if tool == "spawn_agent" && output.contains("agent_id:") {
                    saw_launch_handle = true;
                }
                if tool == "agent_poll" && output.contains("child-answer") {
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
        "spawn_agent should return an agent_id handle immediately"
    );
    assert!(
        saw_polled_answer,
        "agent_poll should surface the child's answer to the parent"
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
            .map(|m| m.text.as_str())
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
                "agent_poll",
                format!(r#"{{"agent_id":"{id}","timeout_secs":5}}"#),
            ));
        }
        if handles.is_empty() {
            // First parent turn: launch two sub-agents at once.
            return Ok(stream_from_response(LlmResponse {
                text: String::new(),
                tool_calls: vec![
                    ToolCall {
                        id: "s1".into(),
                        name: "spawn_agent".into(),
                        input: r#"{"agent":"build","prompt":"task-a"}"#.into(),
                    },
                    ToolCall {
                        id: "s2".into(),
                        name: "spawn_agent".into(),
                        input: r#"{"agent":"build","prompt":"task-b"}"#.into(),
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
        llm_factory: Arc::new(|| LlmSession::new(Box::new(FanOutLlm))),
        ..EngineConfig::default()
    };
    let profiles = cfg.profiles.clone();
    let holly = Holly::spawn(cfg);
    spawn_tool_executor(&holly, ToolRegistry::new(), profiles);

    let parent = SessionId::new("parent");
    let mut sub = holly.subscribe();
    holly
        .send(InMsg::Prompt {
            session: parent.clone(),
            text: "delegate".into(),
        })
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
            } if session == &parent && tool == "agent_poll" => {
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
    // the root finishes — even though each spawn returns without blocking.
    let cfg = EngineConfig {
        llm_factory: Arc::new(|| LlmSession::new(Box::new(RecursiveLlm))),
        ..EngineConfig::default()
    };
    let profiles = cfg.profiles.clone();
    let holly = Holly::spawn(cfg);
    spawn_tool_executor(&holly, ToolRegistry::new(), profiles);

    let root = SessionId::new("root");
    let mut sub = holly.subscribe();
    holly
        .send(InMsg::Prompt {
            session: root.clone(),
            text: "start".into(),
        })
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

/// Spawns a `build` sub-agent on the first turn, polls its handle, and finishes
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
                    "agent_poll",
                    format!(r#"{{"agent_id":"{id}","timeout_secs":5}}"#),
                )),
                None => Ok(finish("done")),
            },
            None => Ok(call(
                "spawn",
                "spawn_agent",
                r#"{"agent":"build","prompt":"recurse"}"#.into(),
            )),
        }
    }
}

#[tokio::test]
async fn read_only_subagent_cannot_spawn() {
    // The root spawns a read-only `explore` child and polls it; the child (a
    // Subagent-mode leaf) tries to spawn and must be refused the capability (#77).
    let cfg = EngineConfig {
        llm_factory: Arc::new(|| LlmSession::new(Box::new(ExploreThenSpawnLlm))),
        ..EngineConfig::default()
    };
    let profiles = cfg.profiles.clone();
    let holly = Holly::spawn(cfg);
    spawn_tool_executor(&holly, ToolRegistry::new(), profiles);

    let root = SessionId::new("root");
    let mut sub = holly.subscribe();
    holly
        .send(InMsg::Prompt {
            session: root.clone(),
            text: "start".into(),
        })
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
        "the explore child's spawn should be refused as a capability"
    );
    // root(0) + one explore child = 2 sessions; the child's spawn never starts a grandchild.
    assert_eq!(
        sessions_started, 2,
        "a read-only sub-agent must not start a grandchild"
    );
}

/// Spawns an `explore` sub-agent then polls it; the child (same factory) tries
/// to spawn and is refused, so the chain stops at one child.
struct ExploreThenSpawnLlm;

#[async_trait]
impl Llm for ExploreThenSpawnLlm {
    async fn stream(&mut self, req: LlmRequest<'_>) -> anyhow::Result<LlmStream> {
        match last_tool(&req) {
            Some(t) => match extract_agent_id(t) {
                Some(id) => Ok(call(
                    "poll",
                    "agent_poll",
                    format!(r#"{{"agent_id":"{id}","timeout_secs":5}}"#),
                )),
                None => Ok(finish("done")),
            },
            None => Ok(call(
                "spawn",
                "spawn_agent",
                r#"{"agent":"explore","prompt":"look"}"#.into(),
            )),
        }
    }
}
