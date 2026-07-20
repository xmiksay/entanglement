//! Stop-abort must not leak the #274 in-flight dedupe entry (#448). A call
//! cancelled via `Stop` unwinds with no `ToolResult`/`ToolOutput` — the only
//! place the set was previously pruned — so its `request_id` used to stay
//! recorded as "in flight" forever. Since `request_id`s are unique per call in
//! practice, that's a slow unbounded leak in a long-lived session that `Stop`s
//! often; this test instead proves it deterministically by *reusing* the same
//! id in a later round. If the entry leaked, the reused id would be silently
//! skipped as "still in flight" and the second round would never resolve.

use std::borrow::Cow;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use entanglement_core::{
    stream_from_response, AgentState, EngineConfig, Holly, InMsg, Llm, LlmRequest, LlmResponse,
    LlmStream, OutEvent, Permission, PermissionProfile, SessionId, ToolCall,
};
use entanglement_runtime::tool_runner::spawn_tool_executor;
use entanglement_runtime::{Tool, ToolRegistry};

/// Replays scripted responses in order, then plain text.
struct ScriptedLlm {
    responses: Mutex<Vec<LlmResponse>>,
}
impl ScriptedLlm {
    fn new(mut responses: Vec<LlmResponse>) -> Self {
        responses.reverse();
        Self {
            responses: Mutex::new(responses),
        }
    }
}
#[async_trait]
impl Llm for ScriptedLlm {
    async fn stream(&mut self, _req: LlmRequest<'_>) -> anyhow::Result<LlmStream> {
        let resp = self
            .responses
            .lock()
            .unwrap()
            .pop()
            .unwrap_or_else(|| LlmResponse {
                text: "done".into(),
                tool_calls: vec![],
            });
        Ok(stream_from_response(resp))
    }
}

/// A `bash` tool that counts its runs; only the *first* run sleeps (long
/// enough to still be in flight when `Stop` arrives and get aborted), so the
/// second round's reused id resolves promptly once dispatched.
struct SlowCountingBash {
    runs: Arc<AtomicUsize>,
}
#[async_trait]
impl Tool for SlowCountingBash {
    fn name(&self) -> Cow<'static, str> {
        Cow::Borrowed("bash")
    }
    async fn run(&self, input: &str) -> anyhow::Result<String> {
        let n = self.runs.fetch_add(1, Ordering::SeqCst);
        if n == 0 {
            tokio::time::sleep(Duration::from_secs(5)).await;
        }
        Ok(format!("ran: {input}"))
    }
}

#[tokio::test]
async fn stop_aborted_request_id_is_not_stuck_in_flight_forever() {
    let runs = Arc::new(AtomicUsize::new(0));
    // Round 1: calls bash(t1), which never gets to finish (aborted by Stop).
    // Round 2 (after Stop + a fresh prompt): reuses id "t1", then finishes.
    let scripted = Arc::new(vec![
        LlmResponse {
            text: "".into(),
            tool_calls: vec![ToolCall {
                id: "t1".into(),
                name: "bash".into(),
                input: "sleep".into(),
                provider_meta: None,
            }],
        },
        LlmResponse {
            text: "".into(),
            tool_calls: vec![ToolCall {
                id: "t1".into(),
                name: "bash".into(),
                input: "sleep".into(),
                provider_meta: None,
            }],
        },
        LlmResponse {
            text: "ok".into(),
            tool_calls: vec![],
        },
    ]);
    let cfg = EngineConfig {
        llm_factory: Arc::new(move || {
            Box::new(ScriptedLlm::new((*scripted).clone())) as Box<dyn Llm>
        }),
        ..EngineConfig::default()
    };
    let holly = Holly::spawn(cfg);

    let mut reg = ToolRegistry::new();
    reg.register(SlowCountingBash { runs: runs.clone() });
    let _executor = spawn_tool_executor(
        &holly,
        reg,
        entanglement_runtime::agents::built_in_registry(),
        PermissionProfile::new(Permission::Allow),
    );

    let sid = SessionId::new("s1");
    let mut sub = holly.subscribe();
    holly.send(InMsg::prompt(sid.clone(), "go")).await.unwrap();

    // Wait for the first call to actually dispatch (the tool's own counter
    // ticks past 0) before cancelling it, so the id is genuinely in flight.
    while runs.load(Ordering::SeqCst) == 0 {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    holly
        .send(InMsg::Stop {
            session: sid.clone(),
        })
        .await
        .unwrap();

    // Wait for the Stop-abort to be acknowledged (core's `Idle` status).
    let mut acked = false;
    while let Ok(Ok(ev)) = tokio::time::timeout(Duration::from_secs(3), sub.recv()).await {
        if let OutEvent::Status {
            session,
            state: AgentState::Idle,
        } = &ev
        {
            if session == &sid {
                acked = true;
                break;
            }
        }
    }
    assert!(acked, "the Stop must be acknowledged with an Idle status");

    // A fresh prompt starts a new round that reuses id "t1". If the executor
    // still thinks "t1" is in flight from the aborted first round, this call
    // is silently skipped, the turn never resolves, and `Done` never fires.
    holly
        .send(InMsg::prompt(sid.clone(), "go again"))
        .await
        .unwrap();

    let mut done = false;
    while let Ok(Ok(ev)) = tokio::time::timeout(Duration::from_secs(3), sub.recv()).await {
        if ev.session() == Some(&sid) && matches!(ev, OutEvent::Done { .. }) {
            done = true;
            break;
        }
    }
    assert!(
        done,
        "a reused request_id from a Stop-aborted call must not stay stuck in flight"
    );
    assert_eq!(
        runs.load(Ordering::SeqCst),
        2,
        "the second round's reused id must actually dispatch the tool"
    );
}
