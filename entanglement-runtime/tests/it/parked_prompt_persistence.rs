//! A prompt sent while a tool batch is parked (ADR-0058) must survive the real
//! persistence round-trip — tap → file → `read` → `pair_records` →
//! `Holly::resume` — and the resumed session must send the live session's
//! history byte for byte (ADR-0202).

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use entanglement_core::{
    stream_from_response, AgentState, EngineConfig, Holly, InMsg, Llm, LlmRequest, LlmResponse,
    LlmStream, Message, OutEvent, SessionId, ToolCall,
};
use entanglement_runtime::persistence::spawn_persistence_subscriber;
use entanglement_runtime::session_store::{pair_records, read, LogPayload, LogRecord};
use tokio::sync::broadcast::Receiver;

type Requests = Arc<Mutex<Vec<String>>>;

struct ScriptLlm {
    responses: Arc<Mutex<VecDeque<LlmResponse>>>,
    requests: Requests,
}

#[async_trait]
impl Llm for ScriptLlm {
    async fn stream(&mut self, req: LlmRequest<'_>) -> anyhow::Result<LlmStream> {
        let history = serde_json::to_string(req.messages)?;
        self.requests.lock().unwrap().push(history);
        let resp = self.responses.lock().unwrap().pop_front();
        Ok(stream_from_response(resp.unwrap_or_else(|| text("ok"))))
    }
}

fn text(t: &str) -> LlmResponse {
    LlmResponse {
        text: t.into(),
        tool_calls: vec![],
    }
}

fn engine(responses: Vec<LlmResponse>) -> (Holly, Requests) {
    let responses = Arc::new(Mutex::new(VecDeque::from(responses)));
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

fn is_done(ev: &OutEvent) -> bool {
    matches!(ev, OutEvent::Done { .. })
}

/// The history `holly` sends for a `probe` prompt.
async fn probe(holly: &Holly, requests: &Requests, sid: &SessionId) -> String {
    let mut sub = holly.subscribe();
    let before = requests.lock().unwrap().len();
    holly
        .send(InMsg::prompt(sid.clone(), "probe"))
        .await
        .unwrap();
    wait_for(&mut sub, sid, is_done).await;
    let history = requests.lock().unwrap()[before].clone();
    history
}

/// The tap writes off its own subscription: wait until `sid`'s log holds `n` `Done`s.
async fn flushed(cwd: &std::path::Path, sid: &SessionId, n: usize) -> Vec<LogRecord> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let records = read(cwd, sid).expect("read log");
        let done = records
            .iter()
            .filter(|r| matches!(&r.payload, LogPayload::Out(ev) if is_done(ev)))
            .count();
        if done >= n {
            return records;
        }
        assert!(tokio::time::Instant::now() < deadline, "tap never flushed");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn prompt_sent_while_parked_survives_the_log_round_trip() {
    let tmp = tempfile::tempdir().expect("temp dir");
    let cwd = tmp.path().to_path_buf();
    let sid = SessionId::new("parked-prompt");
    let call = ToolCall::new("c1", "read", r#"{"path":"x"}"#);
    let (live, live_requests) = engine(vec![
        LlmResponse {
            text: String::new(),
            tool_calls: vec![call],
        },
        text("done"),
    ]);
    let _tap = spawn_persistence_subscriber(&live, cwd.clone());
    let mut sub = live.subscribe();

    live.send(InMsg::prompt(sid.clone(), "go")).await.unwrap();
    // `Working` is the last event of the parked round, so nothing is left to
    // pair the steering prompt with before the executor's result lands.
    wait_for(&mut sub, &sid, |ev| {
        matches!(
            ev,
            OutEvent::Status {
                state: AgentState::Working,
                ..
            }
        )
    })
    .await;
    live.send(InMsg::prompt(sid.clone(), "also check y"))
        .await
        .unwrap();
    live.send(InMsg::tool_result(sid.clone(), "c1", "contents"))
        .await
        .unwrap();
    wait_for(&mut sub, &sid, is_done).await;

    let records = flushed(&cwd, &sid, 1).await;
    let live_history = probe(&live, &live_requests, &sid).await;

    let (resumed, resumed_requests) = engine(Vec::new());
    resumed
        .resume(sid.clone(), pair_records(&records))
        .await
        .unwrap();
    let resumed_history = probe(&resumed, &resumed_requests, &sid).await;

    assert_eq!(
        resumed_history, live_history,
        "resume must send the live history"
    );
    let messages: Vec<Message> = serde_json::from_str(&live_history).unwrap();
    let texts: Vec<String> = messages.iter().map(Message::text).collect();
    // Trailing entry is the mode notice (ADR-0207 §9) — appended fresh to
    // every request from `Session::mode`, never persisted.
    assert_eq!(
        texts,
        [
            "go",
            "",
            "contents",
            "also check y",
            "done",
            "probe",
            "[mode: build]"
        ]
    );
}
