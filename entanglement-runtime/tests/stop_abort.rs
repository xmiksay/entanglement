//! Integration test for `Stop` aborting an in-flight tool task (#167).
//!
//! Core parks a turn as data and, on `Stop`, only clears that state — it never
//! owns the running tool. The runtime tool executor registers each in-flight
//! task and, on a `Stop` for the session off the inbound fan-out, aborts it. A
//! slow tool that flips a `completed` flag only *after* its work must therefore
//! never flip it once the turn is stopped mid-execution.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use entanglement_core::{
    stream_from_response, EngineConfig, Holly, InMsg, Llm, LlmRequest, LlmResponse, LlmStream,
    Permission, PermissionProfile, ProfileRegistry, SessionId, ToolCall,
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

/// A `bash`-named tool that signals when it starts, sleeps, then records that it
/// *completed*. A `Stop` mid-sleep must abort it before the completion flag flips.
struct BlockingTool {
    started: Arc<AtomicBool>,
    completed: Arc<AtomicBool>,
}
#[async_trait]
impl Tool for BlockingTool {
    fn name(&self) -> &'static str {
        "bash"
    }
    async fn run(&self, _input: &str) -> anyhow::Result<String> {
        self.started.store(true, Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(800)).await;
        self.completed.store(true, Ordering::SeqCst);
        Ok("finished".into())
    }
}

fn spawn_blocking_bash(started: Arc<AtomicBool>, completed: Arc<AtomicBool>) -> Holly {
    // One tool call (id `t1`), then plain text so the turn would otherwise end.
    let scripted = Arc::new(vec![
        LlmResponse {
            text: "".into(),
            tool_calls: vec![ToolCall {
                id: "t1".into(),
                name: "bash".into(),
                input: "{}".into(),
            }],
        },
        LlmResponse {
            text: "ok".into(),
            tool_calls: vec![],
        },
    ]);
    let profiles: ProfileRegistry = entanglement_runtime::agents::built_in_registry();
    let cfg = EngineConfig {
        llm_factory: Arc::new(move || {
            Box::new(ScriptedLlm::new((*scripted).clone())) as Box<dyn Llm>
        }),
        profiles: profiles.clone(),
        ..EngineConfig::default()
    };
    let holly = Holly::spawn(cfg);
    let mut reg = ToolRegistry::new();
    reg.register(BlockingTool { started, completed });
    let _executor = spawn_tool_executor(
        &holly,
        reg,
        profiles,
        PermissionProfile::new(Permission::Allow),
    );
    holly
}

#[tokio::test]
async fn stop_aborts_a_running_tool_task() {
    let started = Arc::new(AtomicBool::new(false));
    let completed = Arc::new(AtomicBool::new(false));
    let holly = spawn_blocking_bash(started.clone(), completed.clone());
    let sid = SessionId::new("s1");

    holly.send(InMsg::prompt(sid.clone(), "go")).await.unwrap();

    // Wait for the tool to actually be running, then stop the session.
    let mut waited = 0;
    while !started.load(Ordering::SeqCst) {
        tokio::time::sleep(Duration::from_millis(10)).await;
        waited += 1;
        assert!(waited < 200, "the tool never started");
    }
    holly
        .send(InMsg::Stop {
            session: sid.clone(),
        })
        .await
        .unwrap();

    // Past the tool's own sleep: with the abort it was cancelled mid-sleep and
    // never flipped `completed`; without it the detached task would run on.
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert!(
        !completed.load(Ordering::SeqCst),
        "Stop must abort the in-flight tool task before it completes"
    );
}
