//! Session logs persisted by entanglement 0.6.0 must load and resume (#720).
//!
//! `tests/fixtures/v0.6.0-session.jsonl` was recorded by the real `v0.6.0`
//! tag — a `Holly` + tool executor driven through the reference persistence
//! tap (`spawn_persistence_subscriber_with_sink`) — so it carries 0.6.0's own
//! serializations: `SessionStarted.profile` (renamed `agent` by ADR-0207),
//! `AgentChanged.profile_detail` (the retired agent mask), two `set_agent`
//! switches (retired by ADR-0207), the `bash_enable`/`bash_disable`/
//! `bash_changed` toggles (retired by ADR-0163; session-less, so the
//! reference tap never logs them, but a sink storing every frame does), a
//! tool round-trip, a spawned child, a manual compaction, usage, and a final
//! turn parked mid-batch (`call-2` answered, `call-3` never).

use std::collections::VecDeque;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use entanglement_core::{
    stream_from_response, EngineConfig, Holly, InMsg, Llm, LlmRequest, LlmResponse, LlmStream,
    Message, MessageRole, OutEvent, SessionId,
};
use entanglement_runtime::session_store::{
    integrity_gap, pair_records, read_path, LogPayload, LogRecord,
};
use tokio::sync::broadcast::Receiver;

const FIXTURE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/v0.6.0-session.jsonl"
);

fn fixture() -> Vec<LogRecord> {
    read_path(Path::new(FIXTURE)).expect("a 0.6.0 log must load")
}

#[test]
fn v060_log_loads_with_retired_frames_skipped_not_gapped() {
    let records = fixture();
    assert_eq!(records.len(), 57, "every record decodes; none is dropped");
    assert_eq!(
        integrity_gap(&records),
        None,
        "a retired frame is an intentional retirement, not lost data"
    );

    let retired: Vec<&str> = records
        .iter()
        .filter_map(|r| match &r.payload {
            LogPayload::Retired { kind } => Some(kind.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(
        retired,
        [
            "set_agent",
            "set_agent",
            "bash_enable",
            "bash_changed",
            "bash_disable",
            "bash_changed"
        ]
    );

    let started: Vec<(&str, &str)> = records
        .iter()
        .filter_map(|r| match &r.payload {
            LogPayload::Out(OutEvent::SessionStarted { session, agent, .. }) => {
                Some((session.0.as_str(), agent.as_str()))
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        started,
        [("v060-root", "site-editor"), ("v060-child", "explore")],
        "0.6.0's `profile` field lands in `agent`"
    );
}

type Requests = Arc<Mutex<Vec<Vec<Message>>>>;

struct ScriptLlm {
    responses: Arc<Mutex<VecDeque<LlmResponse>>>,
    requests: Requests,
}

#[async_trait]
impl Llm for ScriptLlm {
    async fn stream(&mut self, req: LlmRequest<'_>) -> anyhow::Result<LlmStream> {
        self.requests.lock().unwrap().push(req.messages.to_vec());
        let resp = self.responses.lock().unwrap().pop_front();
        Ok(stream_from_response(resp.unwrap_or_else(|| LlmResponse {
            text: "ok".into(),
            tool_calls: vec![],
        })))
    }
}

fn engine() -> (Holly, Requests) {
    let responses = Arc::new(Mutex::new(VecDeque::from([LlmResponse {
        text: "Published.".into(),
        tool_calls: vec![],
    }])));
    let requests: Requests = Arc::default();
    let r = requests.clone();
    let cfg = EngineConfig {
        llm_factory: Arc::new(move || {
            Box::new(ScriptLlm {
                responses: responses.clone(),
                requests: r.clone(),
            }) as Box<dyn Llm>
        }),
        ..EngineConfig::default()
    };
    (Holly::spawn(cfg), requests)
}

async fn wait_for(sub: &mut Receiver<OutEvent>, sid: &SessionId, pred: impl Fn(&OutEvent) -> bool) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let ev = tokio::time::timeout_at(deadline, sub.recv())
            .await
            .expect("timed out waiting for event")
            .expect("broadcast closed");
        if ev.session() == Some(sid) && pred(&ev) {
            return;
        }
    }
}

#[tokio::test]
async fn v060_log_resumes_its_parked_turn_with_history_intact() {
    let records = fixture();
    let root = SessionId::new("v060-root");
    let (holly, requests) = engine();
    let mut sub = holly.subscribe();

    holly
        .resume(root.clone(), pair_records(&records))
        .await
        .expect("resume a 0.6.0 log");

    // The unanswered half of the parked batch is re-offered (ADR-0061).
    wait_for(
        &mut sub,
        &root,
        |ev| matches!(ev, OutEvent::ToolExec { request_id, .. } if request_id == "call-3"),
    )
    .await;
    holly
        .send(InMsg::tool_result(
            root.clone(),
            "call-3",
            "published draft",
        ))
        .await
        .unwrap();
    wait_for(&mut sub, &root, |ev| matches!(ev, OutEvent::Done { .. })).await;

    let history = requests.lock().unwrap()[0].clone();
    let shape: Vec<(MessageRole, String, Vec<String>, Option<String>)> = history
        .iter()
        .map(|m| {
            let calls = m.tool_calls.iter().map(|c| c.id.clone()).collect();
            (m.role, m.text(), calls, m.tool_call_id.clone())
        })
        .collect();
    let (user, asst, tool) = (MessageRole::User, MessageRole::Assistant, MessageRole::Tool);
    let s = |t: &str| t.to_string();
    // The compaction is a no-op fold (ADR-0205) and the retired frames carry
    // no context, so the resumed turn sees exactly the conversation 0.6.0 had.
    assert_eq!(
        shape,
        [
            (user, s("What does the home page say?"), vec![], None),
            (
                asst,
                s("Let me read the home page."),
                vec![s("call-1")],
                None
            ),
            (
                tool,
                s("page {\"path\":\"home\"}: Welcome to the site."),
                vec![],
                Some(s("call-1"))
            ),
            (
                asst,
                s("The home page says: Welcome to the site."),
                vec![],
                None
            ),
            (user, s("Publish the draft."), vec![], None),
            (
                asst,
                s("Reading the draft, then publishing it."),
                vec![s("call-2"), s("call-3")],
                None
            ),
            (
                tool,
                s("page {\"path\":\"draft\"}: Welcome to the site."),
                vec![],
                Some(s("call-2"))
            ),
            (tool, s("published draft"), vec![], Some(s("call-3"))),
        ]
    );
}
