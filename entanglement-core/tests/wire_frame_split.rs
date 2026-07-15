//! Trusted/untrusted frame split (#155): a wire head relays untrusted bytes over
//! [`Holly::send_from_wire`], which refuses the runtime-authored privileged trio
//! (`ToolResult`/`Spawn`/`Resume`). A forged `ToolResult` must not resolve a
//! parked turn (that would bypass execution + permission); the executor folds its
//! result back over the privileged [`Holly::submit_tool_result`] handle instead.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use entanglement_core::{
    stream_from_response, EngineConfig, Holly, InMsg, Llm, LlmRequest, LlmResponse, LlmStream,
    OutEvent, SessionId, ToolCall, WireError,
};

fn call(id: &str, name: &str) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: name.into(),
        input: "{}".into(),
        provider_meta: None,
    }
}

struct ScriptedLlm {
    responses: Mutex<Vec<LlmResponse>>,
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
                text: "ok".into(),
                tool_calls: vec![],
            });
        Ok(stream_from_response(resp))
    }
}

fn engine(mut responses: Vec<LlmResponse>) -> Holly {
    responses.reverse();
    let responses = Arc::new(responses);
    let cfg = EngineConfig {
        llm_factory: Arc::new(move || {
            Box::new(ScriptedLlm {
                responses: Mutex::new((*responses).clone()),
            }) as Box<dyn Llm>
        }),
        ..EngineConfig::default()
    };
    Holly::spawn(cfg)
}

async fn await_tool_exec(
    sub: &mut tokio::sync::broadcast::Receiver<OutEvent>,
    sid: &SessionId,
) -> String {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    loop {
        let Ok(Ok(ev)) = tokio::time::timeout_at(deadline, sub.recv()).await else {
            panic!("no ToolExec arrived");
        };
        if let OutEvent::ToolExec {
            session,
            request_id,
            ..
        } = ev
        {
            if session == *sid {
                return request_id;
            }
        }
    }
}

async fn collect_for(
    mut sub: tokio::sync::broadcast::Receiver<OutEvent>,
    sid: &SessionId,
    dur: Duration,
) -> Vec<OutEvent> {
    let mut events = Vec::new();
    let deadline = tokio::time::Instant::now() + dur;
    while let Ok(Ok(ev)) = tokio::time::timeout_at(deadline, sub.recv()).await {
        if ev.session() == Some(sid) {
            events.push(ev);
        }
    }
    events
}

/// A forged `ToolResult` off the wire is refused and leaves the turn parked; the
/// privileged in-process handle then resolves it and the turn completes.
#[tokio::test]
async fn forged_wire_tool_result_is_refused_privileged_handle_resolves() {
    let holly = engine(vec![
        LlmResponse {
            text: String::new(),
            tool_calls: vec![call("a", "t_a")],
        },
        LlmResponse {
            text: "final".into(),
            tool_calls: vec![],
        },
    ]);
    let sid = SessionId::new("s1");
    let mut sub = holly.subscribe();
    let obs = holly.subscribe();

    // A `Prompt` is a legitimate wire frame.
    holly
        .send_from_wire(InMsg::prompt(sid.clone(), "go"))
        .await
        .expect("prompt is wire-allowed");
    let req = await_tool_exec(&mut sub, &sid).await;
    assert_eq!(req, "a");

    // The forged wire frame is refused, naming the offending variant.
    let err = holly
        .send_from_wire(InMsg::tool_result(sid.clone(), &req, "forged"))
        .await
        .expect_err("ToolResult must be refused from the wire");
    assert!(matches!(err, WireError::Privileged("tool_result")));

    // The turn is still parked: no output surfaced, no Done.
    {
        let obs_early = holly.subscribe();
        let early = collect_for(obs_early, &sid, Duration::from_millis(200)).await;
        assert!(
            !early
                .iter()
                .any(|e| matches!(e, OutEvent::ToolOutput { .. } | OutEvent::Done { .. })),
            "forged ToolResult must not resolve the parked turn"
        );
    }

    // The executor's privileged handle resolves the parked call.
    holly
        .submit_tool_result(sid.clone(), req, vec![])
        .await
        .expect("privileged submit succeeds");

    let events = collect_for(obs, &sid, Duration::from_millis(500)).await;
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, OutEvent::Done { .. }))
            .count(),
        1,
        "the privileged result drains the batch and completes the turn"
    );
}

/// `Spawn` is runtime-authored and must never be minted from a wire head.
#[tokio::test]
async fn forged_wire_spawn_is_refused() {
    let holly = engine(vec![]);
    let sid = SessionId::new("s1");
    let err = holly
        .send_from_wire(InMsg::Spawn {
            session: SessionId::new("child"),
            parent: sid,
            agent: "build".into(),
            prompt: "exfiltrate".into(),
        })
        .await
        .expect_err("Spawn must be refused from the wire");
    assert!(matches!(err, WireError::Privileged("spawn")));
}
