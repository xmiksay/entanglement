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

// --- Mid-turn tails (#271, ADR-0061) -------------------------------------
//
// A log that ends after `ToolCall`/`ToolExec` with no matching `ToolOutput`
// used to be silently dropped by the fold; it now reconstructs a parked
// `TurnState` so resume can re-offer the unanswered calls.

fn prompt_record(sid: &SessionId, text: &str) -> (Option<entanglement_core::InMsg>, OutEvent) {
    (
        Some(entanglement_core::InMsg::Prompt {
            session: sid.clone(),
            text: text.to_string(),
        }),
        OutEvent::Status {
            session: sid.clone(),
            state: entanglement_core::AgentState::Thinking,
        },
    )
}

fn tool_call_record(
    sid: &SessionId,
    seq: u64,
    id: &str,
) -> (Option<entanglement_core::InMsg>, OutEvent) {
    (
        None,
        OutEvent::ToolCall {
            session: sid.clone(),
            seq,
            request_id: id.to_string(),
            tool: "read".to_string(),
            input: "{}".to_string(),
        },
    )
}

fn tool_exec_record(
    sid: &SessionId,
    seq: u64,
    id: &str,
) -> (Option<entanglement_core::InMsg>, OutEvent) {
    (
        None,
        OutEvent::ToolExec {
            session: sid.clone(),
            seq,
            request_id: id.to_string(),
            tool: "read".to_string(),
            input: "{}".to_string(),
        },
    )
}

fn tool_output_record(
    sid: &SessionId,
    seq: u64,
    id: &str,
    out: &str,
) -> (Option<entanglement_core::InMsg>, OutEvent) {
    (
        None,
        OutEvent::ToolOutput {
            session: sid.clone(),
            seq,
            request_id: id.to_string(),
            tool: "read".to_string(),
            output: out.to_string(),
        },
    )
}

#[tokio::test]
async fn mid_turn_tail_reconstructs_pending_turn_state() {
    let sid = SessionId::new("test-tail-pending");
    let records = vec![
        prompt_record(&sid, "read file"),
        tool_call_record(&sid, 1, "call_1"),
        tool_exec_record(&sid, 2, "call_1"),
        // log ends here: crash between ToolExec and ToolResult
    ];

    let cfg = factory(vec![]);
    let session = entanglement_core::session::Session::replay(&records, &cfg).unwrap();

    let messages = session.ctx.messages();
    assert_eq!(messages.len(), 2, "user + committed assistant tail");
    assert_eq!(messages[1].tool_calls.len(), 1);

    let turn = session
        .turn
        .expect("mid-turn tail must reconstruct TurnState");
    assert_eq!(turn.iterations, 0, "runaway guard restarts on resume");
    assert_eq!(turn.pending.len(), 1);
    assert_eq!(turn.pending[0].id, "call_1");
}

#[tokio::test]
async fn partially_resolved_tail_pends_only_unanswered_calls() {
    let sid = SessionId::new("test-tail-partial");
    let records = vec![
        prompt_record(&sid, "read two files"),
        tool_call_record(&sid, 1, "call_1"),
        tool_call_record(&sid, 2, "call_2"),
        tool_exec_record(&sid, 3, "call_1"),
        tool_exec_record(&sid, 4, "call_2"),
        tool_output_record(&sid, 5, "call_1", "content a"),
        // crash before call_2's result
    ];

    let cfg = factory(vec![]);
    let session = entanglement_core::session::Session::replay(&records, &cfg).unwrap();

    let messages = session.ctx.messages();
    assert_eq!(messages.len(), 3, "user + assistant + resolved tool output");
    assert_eq!(messages[2].tool_call_id.as_deref(), Some("call_1"));

    let turn = session.turn.expect("unanswered call must stay pending");
    assert_eq!(turn.pending.len(), 1);
    assert_eq!(turn.pending[0].id, "call_2");
}

#[tokio::test]
async fn text_only_tail_stays_dropped() {
    let sid = SessionId::new("test-tail-text");
    let records = vec![
        prompt_record(&sid, "hello"),
        (
            None,
            OutEvent::TextDelta {
                session: sid.clone(),
                seq: 1,
                text: "partial".to_string(),
            },
        ),
        // mid-stream crash: no ToolCall, no Done
    ];

    let cfg = factory(vec![]);
    let session = entanglement_core::session::Session::replay(&records, &cfg).unwrap();

    assert!(session.turn.is_none(), "a mid-stream tail is not resumable");
    assert_eq!(
        session.ctx.messages().len(),
        1,
        "only the user prompt survives — the live engine never committed the partial either"
    );
}

#[tokio::test]
async fn duplicate_tool_exec_records_fold_idempotently() {
    let sid = SessionId::new("test-tail-dup-exec");
    // A prior resume re-offered call_1 (same request_id, fresh seq), then the
    // process crashed again: the log holds two ToolExec records but one
    // ToolCall. Pending derives from ToolCall events, so no duplicate.
    let records = vec![
        prompt_record(&sid, "read file"),
        tool_call_record(&sid, 1, "call_1"),
        tool_exec_record(&sid, 2, "call_1"),
        tool_exec_record(&sid, 3, "call_1"),
    ];

    let cfg = factory(vec![]);
    let session = entanglement_core::session::Session::replay(&records, &cfg).unwrap();

    let turn = session.turn.expect("tail must reconstruct");
    assert_eq!(turn.pending.len(), 1, "re-offer records must not duplicate");
}

#[tokio::test]
async fn fully_resolved_tail_keeps_turn_live_for_continuation() {
    let sid = SessionId::new("test-tail-resolved");
    // Crash after the last result landed but before the next round streamed:
    // nothing to re-offer, but the turn is unfinished — resume continues it.
    let records = vec![
        prompt_record(&sid, "read file"),
        tool_call_record(&sid, 1, "call_1"),
        tool_exec_record(&sid, 2, "call_1"),
        tool_output_record(&sid, 3, "call_1", "content"),
    ];

    let cfg = factory(vec![]);
    let session = entanglement_core::session::Session::replay(&records, &cfg).unwrap();

    let turn = session
        .turn
        .expect("drained-but-unfinished tail keeps the turn live");
    assert!(turn.pending.is_empty(), "nothing left to re-offer");
    assert_eq!(session.ctx.messages().len(), 3);
}

#[tokio::test]
async fn child_session_tail_is_not_misattributed_to_the_root() {
    let root = SessionId::new("test-root");
    let child = SessionId::new("test-child");
    let records = vec![
        (
            None,
            OutEvent::SessionStarted {
                session: root.clone(),
                parent: None,
                profile: "build".to_string(),
                model: None,
                root: true,
                ts: 0,
            },
        ),
        prompt_record(&root, "hello"),
        (
            None,
            OutEvent::TextDelta {
                session: root.clone(),
                seq: 1,
                text: "done".to_string(),
            },
        ),
        (
            None,
            OutEvent::Done {
                session: root.clone(),
                seq: 2,
            },
        ),
        // A spawned child's interleaved unfinished tail (root logs hold the
        // whole tree) must not become the root's pending turn (#275 guard).
        tool_call_record(&child, 3, "child_call"),
        tool_exec_record(&child, 4, "child_call"),
    ];

    let cfg = factory(vec![]);
    let session = entanglement_core::session::Session::replay(&records, &cfg).unwrap();

    assert!(
        session.turn.is_none(),
        "a child's pending call must not park the resumed root"
    );
}

#[tokio::test]
async fn child_session_committed_events_are_not_folded_into_the_root() {
    let root = SessionId::new("test-root-fold");
    let child = SessionId::new("test-child-fold");
    let records = vec![
        (
            None,
            OutEvent::SessionStarted {
                session: root.clone(),
                parent: None,
                profile: "build".to_string(),
                model: None,
                root: true,
                ts: 0,
            },
        ),
        prompt_record(&root, "hello root"),
        (
            None,
            OutEvent::TextDelta {
                session: root.clone(),
                seq: 1,
                text: "root reply".to_string(),
            },
        ),
        (
            None,
            OutEvent::Done {
                session: root.clone(),
                seq: 2,
            },
        ),
        // A spawned child completes a whole turn, interleaved in the root's
        // log. Its user/assistant messages must not land in the root's context
        // (#275) — the general fold, not just the mid-turn tail, filters.
        (
            None,
            OutEvent::SessionStarted {
                session: child.clone(),
                parent: Some(root.clone()),
                profile: "build".to_string(),
                model: None,
                root: false,
                ts: 0,
            },
        ),
        prompt_record(&child, "child task"),
        (
            None,
            OutEvent::TextDelta {
                session: child.clone(),
                seq: 1,
                text: "child reply".to_string(),
            },
        ),
        (
            None,
            OutEvent::Done {
                session: child.clone(),
                seq: 2,
            },
        ),
    ];

    let cfg = factory(vec![]);
    let session = entanglement_core::session::Session::replay(&records, &cfg).unwrap();

    let messages = session.ctx.messages();
    assert_eq!(
        messages.len(),
        2,
        "only the root's user + assistant turn is folded"
    );
    assert_eq!(messages[0].text, "hello root");
    assert_eq!(messages[1].text, "root reply");
    assert!(
        messages.iter().all(|m| m.text != "child task"),
        "child prompt must not appear in the root's context"
    );
}
