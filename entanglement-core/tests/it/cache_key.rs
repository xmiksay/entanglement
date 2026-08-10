//! `LlmRequest::cache_key` (#673): every main-turn request carries the
//! session's own id as a stable implicit-cache routing hint, unchanged across
//! turns of the same session.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use entanglement_core::{
    stream_from_response, EngineConfig, Holly, InMsg, Llm, LlmRequest, LlmResponse, LlmStream,
    OutEvent, SessionId,
};

/// Every request's `cache_key`, in order.
type Seen = Arc<Mutex<Vec<Option<String>>>>;

struct RecordingLlm {
    seen: Seen,
}

#[async_trait]
impl Llm for RecordingLlm {
    async fn stream(&mut self, req: LlmRequest<'_>) -> anyhow::Result<LlmStream> {
        self.seen
            .lock()
            .unwrap()
            .push(req.cache_key.map(str::to_string));
        Ok(stream_from_response(LlmResponse {
            text: "done".into(),
            tool_calls: vec![],
        }))
    }
}

async fn recv_done(sub: &mut tokio::sync::broadcast::Receiver<OutEvent>, session: &SessionId) {
    loop {
        let ev = tokio::time::timeout(Duration::from_secs(3), sub.recv())
            .await
            .expect("timed out waiting for Done")
            .expect("event channel closed");
        if matches!(&ev, OutEvent::Done { session: s, .. } if s == session) {
            return;
        }
    }
}

#[tokio::test]
async fn cache_key_is_the_session_id_and_stable_across_turns() {
    let seen: Seen = Arc::new(Mutex::new(Vec::new()));
    let factory = {
        let seen = seen.clone();
        Arc::new(move || Box::new(RecordingLlm { seen: seen.clone() }) as Box<dyn Llm>)
    };
    let holly = Holly::spawn(EngineConfig {
        llm_factory: factory,
        ..EngineConfig::default()
    });
    let mut sub = holly.subscribe();
    let session = SessionId::new("cache-key-session");

    holly
        .send(InMsg::prompt(session.clone(), "first"))
        .await
        .unwrap();
    recv_done(&mut sub, &session).await;
    holly
        .send(InMsg::prompt(session.clone(), "second"))
        .await
        .unwrap();
    recv_done(&mut sub, &session).await;

    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 2, "one request per turn");
    // The key is the session's own id, byte-identical on every turn — the
    // property a provider-side implicit cache routes on.
    assert_eq!(seen[0].as_deref(), Some("cache-key-session"));
    assert_eq!(seen[1].as_deref(), Some("cache-key-session"));
}
