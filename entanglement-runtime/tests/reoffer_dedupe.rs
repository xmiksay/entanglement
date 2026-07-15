//! Executor-side re-offer idempotence (#274, ADR-0071). Core arms a re-offer
//! timer while a turn is parked and re-emits the pending `ToolExec` batch after a
//! stretch of silence — its recovery for an in-process offer dropped under
//! outbound-broadcast lag. The runtime executor must dedupe by `request_id` so a
//! re-offer to a *still-in-flight* call is a no-op, not a double-run. This drives
//! a deliberately slow tool with a short re-offer interval so several re-offers
//! land while the first (and only) run is in flight, then asserts the tool ran
//! exactly once and the turn still completes.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use entanglement_core::{
    stream_from_response, EngineConfig, Holly, InMsg, Llm, LlmRequest, LlmResponse, LlmStream,
    OutEvent, Permission, PermissionProfile, SessionId, ToolCall,
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

/// A `bash` tool that counts its runs and sleeps long enough that several
/// re-offers arrive while it is still executing.
struct SlowCountingBash {
    runs: Arc<AtomicUsize>,
}
#[async_trait]
impl Tool for SlowCountingBash {
    fn name(&self) -> &'static str {
        "bash"
    }
    async fn run(&self, input: &str) -> anyhow::Result<String> {
        self.runs.fetch_add(1, Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(300)).await;
        Ok(format!("ran: {input}"))
    }
}

#[tokio::test]
async fn reoffered_tool_exec_runs_only_once() {
    let runs = Arc::new(AtomicUsize::new(0));
    let scripted = Arc::new(vec![
        LlmResponse {
            text: "".into(),
            tool_calls: vec![ToolCall {
                id: "t1".into(),
                name: "bash".into(),
                input: "echo hi".into(),
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
        // Re-offer well before the 300ms tool finishes, so re-offers land while
        // the single run is in flight.
        reoffer_interval: Some(Duration::from_millis(80)),
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

    // Drain to Done (the second round's "ok").
    let mut done = false;
    while let Ok(Ok(ev)) = tokio::time::timeout(Duration::from_secs(3), sub.recv()).await {
        if ev.session() == Some(&sid) && matches!(ev, OutEvent::Done { .. }) {
            done = true;
            break;
        }
    }
    assert!(done, "the turn must complete despite the re-offers");
    assert_eq!(
        runs.load(Ordering::SeqCst),
        1,
        "a re-offer to a still-in-flight call must not run the tool a second time"
    );
}

/// A fast tool that counts its runs.
struct FastCountingBash {
    runs: Arc<AtomicUsize>,
}
#[async_trait]
impl Tool for FastCountingBash {
    fn name(&self) -> &'static str {
        "bash"
    }
    async fn run(&self, input: &str) -> anyhow::Result<String> {
        self.runs.fetch_add(1, Ordering::SeqCst);
        Ok(format!("ran: {input}"))
    }
}

/// The dedupe is scoped to *in-flight* calls, not the whole session: once a call
/// resolves (core emits its `ToolOutput`), a later round that reuses the same
/// `request_id` still dispatches. Core matches a `ToolResult` by id only within a
/// round's pending set, so id reuse across rounds is legitimate and must not be
/// swallowed. Two rounds both call `bash` with id `"t1"`; both must run.
#[tokio::test]
async fn resolved_id_can_be_reused_in_a_later_round() {
    let runs = Arc::new(AtomicUsize::new(0));
    let call = || LlmResponse {
        text: "".into(),
        tool_calls: vec![ToolCall {
            id: "t1".into(),
            name: "bash".into(),
            input: "echo hi".into(),
        }],
    };
    // Round 1 calls bash(t1); its result comes back; round 2 reuses id t1; then
    // the turn ends.
    let scripted = Arc::new(vec![
        call(),
        call(),
        LlmResponse {
            text: "ok".into(),
            tool_calls: vec![],
        },
    ]);
    let cfg = EngineConfig {
        llm_factory: Arc::new(move || {
            Box::new(ScriptedLlm::new((*scripted).clone())) as Box<dyn Llm>
        }),
        reoffer_interval: Some(Duration::from_millis(80)),
        ..EngineConfig::default()
    };
    let holly = Holly::spawn(cfg);

    let mut reg = ToolRegistry::new();
    reg.register(FastCountingBash { runs: runs.clone() });
    let _executor = spawn_tool_executor(
        &holly,
        reg,
        entanglement_runtime::agents::built_in_registry(),
        PermissionProfile::new(Permission::Allow),
    );

    let sid = SessionId::new("s1");
    let mut sub = holly.subscribe();
    holly.send(InMsg::prompt(sid.clone(), "go")).await.unwrap();

    let mut done = false;
    while let Ok(Ok(ev)) = tokio::time::timeout(Duration::from_secs(3), sub.recv()).await {
        if ev.session() == Some(&sid) && matches!(ev, OutEvent::Done { .. }) {
            done = true;
            break;
        }
    }
    assert!(done, "the turn completes");
    assert_eq!(
        runs.load(Ordering::SeqCst),
        2,
        "reusing a resolved request_id in a later round must dispatch again"
    );
}
