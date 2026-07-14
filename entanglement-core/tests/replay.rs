//! Tests for session replay fidelity.

use std::sync::Arc;

use async_trait::async_trait;
use entanglement_core::{
    stream_from_response, AgentMode, AgentProfile, EngineConfig, Llm, LlmRequest, LlmResponse,
    LlmSession, LlmStream, OutEvent, Permission, PermissionProfile, SessionId,
};

/// An LLM that replays a scripted list of responses, in order.
struct ScriptedLlm {
    responses: Vec<LlmResponse>,
}

impl ScriptedLlm {
    fn new(responses: Vec<LlmResponse>) -> Self {
        Self { responses }
    }
}

#[async_trait]
impl Llm for ScriptedLlm {
    async fn stream(&mut self, _req: LlmRequest<'_>) -> anyhow::Result<LlmStream> {
        let resp = self.responses.pop().unwrap_or_else(|| LlmResponse {
            text: "ok".into(),
            tool_calls: vec![],
        });
        Ok(stream_from_response(resp))
    }
}

fn factory(_responses: Vec<LlmResponse>) -> EngineConfig {
    EngineConfig {
        llm_factory: Arc::new(move || LlmSession::new(Box::new(ScriptedLlm::new(vec![])))),
        ..EngineConfig::default()
    }
}

#[tokio::test]
async fn text_only_turn_replay_fidelity() {
    let sid = SessionId::new("test-text-only");
    let records = vec![
        (
            Some(entanglement_core::InMsg::Prompt {
                session: sid.clone(),
                text: "hello".to_string(),
            }),
            OutEvent::Status {
                session: sid.clone(),
                state: entanglement_core::AgentState::Thinking,
            },
        ),
        (
            None,
            OutEvent::TextDelta {
                session: sid.clone(),
                seq: 1,
                text: "Hi".to_string(),
            },
        ),
        (
            None,
            OutEvent::TextDelta {
                session: sid.clone(),
                seq: 2,
                text: " there".to_string(),
            },
        ),
        (
            None,
            OutEvent::Done {
                session: sid.clone(),
                seq: 3,
            },
        ),
    ];

    let cfg = factory(vec![]);
    let result = entanglement_core::session::Session::replay(&records, &cfg);

    assert!(result.is_ok());
    let session = result.unwrap();
    let messages = session.ctx.messages();

    assert_eq!(
        messages.len(),
        2,
        "Should have 2 messages (user + assistant)"
    );
    assert_eq!(messages[0].role, entanglement_core::MessageRole::User);
    assert_eq!(messages[0].text, "hello");
    assert_eq!(messages[1].role, entanglement_core::MessageRole::Assistant);
    assert_eq!(messages[1].text, "Hi there");
}

#[tokio::test]
async fn single_tool_turn_replay_fidelity() {
    let sid = SessionId::new("test-single-tool");
    let records = vec![
        (
            Some(entanglement_core::InMsg::Prompt {
                session: sid.clone(),
                text: "read file".to_string(),
            }),
            OutEvent::Status {
                session: sid.clone(),
                state: entanglement_core::AgentState::Thinking,
            },
        ),
        (
            None,
            OutEvent::ToolCall {
                session: sid.clone(),
                seq: 1,
                request_id: "call_1".to_string(),
                tool: "read".to_string(),
                input: r#"{"path": "test.txt"}"#.to_string(),
            },
        ),
        (
            None,
            OutEvent::ToolOutput {
                session: sid.clone(),
                seq: 2,
                request_id: "call_1".to_string(),
                tool: "read".to_string(),
                output: "file content".to_string(),
            },
        ),
        (
            None,
            OutEvent::Done {
                session: sid.clone(),
                seq: 3,
            },
        ),
    ];

    let cfg = factory(vec![]);
    let result = entanglement_core::session::Session::replay(&records, &cfg);

    assert!(result.is_ok());
    let session = result.unwrap();
    let messages = session.ctx.messages();

    eprintln!("Messages: {:#?}", messages);

    assert_eq!(
        messages.len(),
        3,
        "Should have 3 messages (user, assistant, tool)"
    );
    assert_eq!(messages[0].role, entanglement_core::MessageRole::User);
    assert_eq!(messages[0].text, "read file");
    assert_eq!(messages[1].role, entanglement_core::MessageRole::Assistant);
    assert_eq!(messages[1].text, "");
    assert_eq!(messages[1].tool_calls.len(), 1);
    assert_eq!(messages[1].tool_calls[0].id, "call_1");
    assert_eq!(messages[1].tool_calls[0].name, "read");
    assert_eq!(messages[2].role, entanglement_core::MessageRole::Tool);
    assert_eq!(
        messages[2].tool_call_id.as_ref().unwrap(),
        &"call_1".to_string()
    );
    assert_eq!(messages[2].text, "file content");
}

#[tokio::test]
async fn multi_tool_turn_replay_fidelity() {
    let sid = SessionId::new("test-multi-tool");
    let records = vec![
        (
            Some(entanglement_core::InMsg::Prompt {
                session: sid.clone(),
                text: "read two files".to_string(),
            }),
            OutEvent::Status {
                session: sid.clone(),
                state: entanglement_core::AgentState::Thinking,
            },
        ),
        (
            None,
            OutEvent::ToolCall {
                session: sid.clone(),
                seq: 1,
                request_id: "call_1".to_string(),
                tool: "read".to_string(),
                input: r#"{"path": "a.txt"}"#.to_string(),
            },
        ),
        (
            None,
            OutEvent::ToolCall {
                session: sid.clone(),
                seq: 2,
                request_id: "call_2".to_string(),
                tool: "read".to_string(),
                input: r#"{"path": "b.txt"}"#.to_string(),
            },
        ),
        (
            None,
            OutEvent::ToolOutput {
                session: sid.clone(),
                seq: 3,
                request_id: "call_1".to_string(),
                tool: "read".to_string(),
                output: "content a".to_string(),
            },
        ),
        (
            None,
            OutEvent::ToolOutput {
                session: sid.clone(),
                seq: 4,
                request_id: "call_2".to_string(),
                tool: "read".to_string(),
                output: "content b".to_string(),
            },
        ),
        (
            None,
            OutEvent::Done {
                session: sid.clone(),
                seq: 5,
            },
        ),
    ];

    let cfg = factory(vec![]);
    let result = entanglement_core::session::Session::replay(&records, &cfg);

    assert!(result.is_ok());
    let session = result.unwrap();
    let messages = session.ctx.messages();

    assert_eq!(
        messages.len(),
        4,
        "Should have 4 messages (user, assistant, 2 tools)"
    );
    assert_eq!(messages[1].role, entanglement_core::MessageRole::Assistant);
    assert_eq!(messages[1].tool_calls.len(), 2);
    assert_eq!(messages[1].tool_calls[0].id, "call_1");
    assert_eq!(messages[1].tool_calls[1].id, "call_2");
    assert_eq!(messages[2].text, "content a");
    assert_eq!(messages[3].text, "content b");
}

#[tokio::test]
async fn multi_turn_conversation_replay_fidelity() {
    let sid = SessionId::new("test-multi-turn");
    let records = vec![
        (
            Some(entanglement_core::InMsg::Prompt {
                session: sid.clone(),
                text: "hello".to_string(),
            }),
            OutEvent::Status {
                session: sid.clone(),
                state: entanglement_core::AgentState::Thinking,
            },
        ),
        (
            None,
            OutEvent::TextDelta {
                session: sid.clone(),
                seq: 1,
                text: "Hi".to_string(),
            },
        ),
        (
            None,
            OutEvent::Done {
                session: sid.clone(),
                seq: 2,
            },
        ),
        (
            Some(entanglement_core::InMsg::Prompt {
                session: sid.clone(),
                text: "how are you?".to_string(),
            }),
            OutEvent::Status {
                session: sid.clone(),
                state: entanglement_core::AgentState::Thinking,
            },
        ),
        (
            None,
            OutEvent::TextDelta {
                session: sid.clone(),
                seq: 3,
                text: "Good".to_string(),
            },
        ),
        (
            None,
            OutEvent::Done {
                session: sid.clone(),
                seq: 4,
            },
        ),
    ];

    let cfg = factory(vec![]);
    let result = entanglement_core::session::Session::replay(&records, &cfg);

    assert!(result.is_ok());
    let session = result.unwrap();
    let messages = session.ctx.messages();

    assert_eq!(messages.len(), 4, "Should have 4 messages (2 turns × 2)");
    assert_eq!(messages[0].text, "hello");
    assert_eq!(messages[1].text, "Hi");
    assert_eq!(messages[2].text, "how are you?");
    assert_eq!(messages[3].text, "Good");
}

#[tokio::test]
async fn profile_changes_during_replay() {
    let sid = SessionId::new("test-profile-change");
    let records = vec![
        (
            Some(entanglement_core::InMsg::Prompt {
                session: sid.clone(),
                text: "hello".to_string(),
            }),
            OutEvent::Status {
                session: sid.clone(),
                state: entanglement_core::AgentState::Thinking,
            },
        ),
        (
            None,
            OutEvent::AgentChanged {
                session: sid.clone(),
                agent: "reviewer".to_string(),
                profile_detail: None,
            },
        ),
        (
            None,
            OutEvent::TextDelta {
                session: sid.clone(),
                seq: 1,
                text: "hi".to_string(),
            },
        ),
        (
            None,
            OutEvent::Done {
                session: sid.clone(),
                seq: 2,
            },
        ),
    ];

    // Core carries only the `build` built-in (#201); replay resolves the
    // `AgentChanged` name against the registry, so register the target here.
    let mut cfg = factory(vec![]);
    cfg.profiles.insert(AgentProfile {
        name: "reviewer".into(),
        description: String::new(),
        mode: AgentMode::Primary,
        system_prompt: "Review the changes.".into(),
        model: None,
        permission: PermissionProfile::new(Permission::Ask),
        tools: None,
        disallowed_tools: Vec::new(),
        can_spawn: None,
        spawnable_agents: None,
    });
    let result = entanglement_core::session::Session::replay(&records, &cfg);

    assert!(result.is_ok());
    let session = result.unwrap();
    assert_eq!(session.profile.name, "reviewer");
}

#[tokio::test]
async fn seq_tracking_during_replay() {
    let sid = SessionId::new("test-seq-tracking");
    let records = vec![
        (
            Some(entanglement_core::InMsg::Prompt {
                session: sid.clone(),
                text: "hello".to_string(),
            }),
            OutEvent::Status {
                session: sid.clone(),
                state: entanglement_core::AgentState::Thinking,
            },
        ),
        (
            None,
            OutEvent::TextDelta {
                session: sid.clone(),
                seq: 10,
                text: "hi".to_string(),
            },
        ),
        (
            None,
            OutEvent::TextDelta {
                session: sid.clone(),
                seq: 20,
                text: " there".to_string(),
            },
        ),
        (
            None,
            OutEvent::Done {
                session: sid.clone(),
                seq: 30,
            },
        ),
    ];

    let cfg = factory(vec![]);
    let result = entanglement_core::session::Session::replay(&records, &cfg);

    assert!(result.is_ok());
    let session = result.unwrap();
    assert_eq!(session.seq, 30, "Should track the max seq number");
}
