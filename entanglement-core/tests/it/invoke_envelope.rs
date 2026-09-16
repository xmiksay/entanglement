//! ADR-0204 core half: when a round advertises the `invoke {name, args}` spec,
//! an `invoke` call is unwrapped before its events are emitted —
//! `ToolCall`/`ToolExec`/`ToolOutput` name the inner tool and carry the emitted
//! call as `envelope` — while `Context` (and so every later request, live or
//! resumed) keeps the call exactly as the model emitted it.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use entanglement_core::{
    stream_from_response, EngineConfig, Holly, InMsg, Llm, LlmRequest, LlmResponse, LlmStream,
    Message, OutEvent, SessionId, ToolCall, ToolEnvelope, ToolSpec, INVOKE_TOOL,
};

type Seen = Arc<Mutex<Vec<Vec<Message>>>>;
type Log = Vec<(Option<InMsg>, OutEvent)>;

/// Scripted LLM recording every request's messages.
struct RecordingLlm {
    responses: Vec<LlmResponse>,
    seen: Seen,
}

#[async_trait]
impl Llm for RecordingLlm {
    async fn stream(&mut self, req: LlmRequest<'_>) -> anyhow::Result<LlmStream> {
        self.seen.lock().unwrap().push(req.messages.to_vec());
        let resp = if self.responses.is_empty() {
            text("ok")
        } else {
            self.responses.remove(0)
        };
        Ok(stream_from_response(resp))
    }
}

fn engine(responses: Vec<LlmResponse>, tool_specs: Vec<ToolSpec>) -> (Holly, Seen) {
    let seen: Seen = Arc::new(Mutex::new(Vec::new()));
    let seen_factory = seen.clone();
    let cfg = EngineConfig {
        llm_factory: Arc::new(move || {
            Box::new(RecordingLlm {
                responses: responses.clone(),
                seen: seen_factory.clone(),
            }) as Box<dyn Llm>
        }),
        tool_specs,
        ..EngineConfig::default()
    };
    (Holly::spawn(cfg), seen)
}

fn with_invoke() -> Vec<ToolSpec> {
    vec![
        ToolSpec::new("read", "read a file"),
        ToolSpec::new(INVOKE_TOOL, "call a discovered tool"),
    ]
}

fn call(id: &str, name: &str, input: &str) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: name.into(),
        input: input.into(),
        provider_meta: None,
    }
}

fn tools(calls: Vec<ToolCall>) -> LlmResponse {
    LlmResponse {
        text: String::new(),
        tool_calls: calls,
    }
}

fn text(t: &str) -> LlmResponse {
    LlmResponse {
        text: t.into(),
        tool_calls: vec![],
    }
}

fn envelope(raw: &str) -> Option<ToolEnvelope> {
    Some(ToolEnvelope {
        tool: INVOKE_TOOL.into(),
        input: raw.into(),
    })
}

/// Record `sid`'s events (the first one carrying `prompt`, like the
/// persistence tap) until `stop` matches, inclusive.
async fn record_until(
    sub: &mut tokio::sync::broadcast::Receiver<OutEvent>,
    sid: &SessionId,
    mut prompt: Option<InMsg>,
    stop: impl Fn(&OutEvent, usize) -> bool,
) -> Log {
    let mut log = Vec::new();
    let mut execs = 0;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    loop {
        let Ok(Ok(ev)) = tokio::time::timeout_at(deadline, sub.recv()).await else {
            panic!("timed out; recorded so far: {log:?}");
        };
        if ev.session() != Some(sid) {
            continue;
        }
        execs += usize::from(matches!(ev, OutEvent::ToolExec { .. }));
        let done = stop(&ev, execs);
        log.push((prompt.take(), ev));
        if done {
            return log;
        }
    }
}

async fn until_execs(
    sub: &mut tokio::sync::broadcast::Receiver<OutEvent>,
    sid: &SessionId,
    prompt: Option<InMsg>,
    n: usize,
) -> Log {
    record_until(sub, sid, prompt, |_, execs| execs == n).await
}

async fn until_done(sub: &mut tokio::sync::broadcast::Receiver<OutEvent>, sid: &SessionId) -> Log {
    record_until(sub, sid, None, |ev, _| matches!(ev, OutEvent::Done { .. })).await
}

/// `(request_id, tool, input-or-output, envelope)` of every event of `kind`.
fn tool_events(log: &Log, kind: &str) -> Vec<(String, String, String, Option<ToolEnvelope>)> {
    log.iter()
        .filter_map(|(_, ev)| match (kind, ev) {
            (
                "call",
                OutEvent::ToolCall {
                    request_id,
                    tool,
                    input,
                    envelope,
                    ..
                },
            )
            | (
                "exec",
                OutEvent::ToolExec {
                    request_id,
                    tool,
                    input,
                    envelope,
                    ..
                },
            )
            | (
                "output",
                OutEvent::ToolOutput {
                    request_id,
                    tool,
                    output: input,
                    envelope,
                    ..
                },
            ) => Some((
                request_id.clone(),
                tool.clone(),
                input.clone(),
                envelope.clone(),
            )),
            _ => None,
        })
        .collect()
}

/// The tool calls of the last assistant message in `messages`.
fn last_assistant_calls(messages: &[Message]) -> Vec<ToolCall> {
    messages
        .iter()
        .rev()
        .find(|m| !m.tool_calls.is_empty())
        .map(|m| m.tool_calls.clone())
        .unwrap_or_default()
}

/// Case (a): specs advertise `invoke` → events show the inner `read` call with
/// the envelope; the next request's history still holds the emitted `invoke`.
#[tokio::test]
async fn advertised_invoke_unwraps_events_but_context_keeps_the_emitted_call() {
    let raw = r#"{"name":"read","args":{"path":"x"}}"#;
    let (holly, seen) = engine(
        vec![tools(vec![call("c1", INVOKE_TOOL, raw)]), text("done")],
        with_invoke(),
    );
    let sid = SessionId::new("s");
    let mut sub = holly.subscribe();
    holly.send(InMsg::prompt(sid.clone(), "go")).await.unwrap();
    let mut log = until_execs(&mut sub, &sid, None, 1).await;

    let inner = (
        "c1".to_string(),
        "read".to_string(),
        r#"{"path":"x"}"#.to_string(),
        envelope(raw),
    );
    assert_eq!(tool_events(&log, "call"), vec![inner.clone()]);
    assert_eq!(tool_events(&log, "exec"), vec![inner]);

    holly
        .send(InMsg::tool_result(sid.clone(), "c1", "contents"))
        .await
        .unwrap();
    log.extend(until_done(&mut sub, &sid).await);
    assert_eq!(
        tool_events(&log, "output"),
        vec![("c1".into(), "read".into(), "contents".into(), envelope(raw))]
    );

    let requests = seen.lock().unwrap().clone();
    assert_eq!(requests.len(), 2);
    assert_eq!(
        last_assistant_calls(&requests[1]),
        vec![call("c1", INVOKE_TOOL, raw)],
        "the model-facing history keeps the emitted invoke call byte for byte"
    );
    assert!(requests[1]
        .iter()
        .any(|m| m.tool_call_id.as_deref() == Some("c1")));
}

/// Case (b): the same call when `invoke` is not advertised stays an ordinary
/// (unknown) `invoke` call with no envelope.
#[tokio::test]
async fn unadvertised_invoke_is_not_unwrapped() {
    let raw = r#"{"name":"read","args":{"path":"x"}}"#;
    let (holly, _) = engine(
        vec![tools(vec![call("c1", INVOKE_TOOL, raw)])],
        vec![ToolSpec::new("read", "read a file")],
    );
    let sid = SessionId::new("s");
    let mut sub = holly.subscribe();
    holly.send(InMsg::prompt(sid.clone(), "go")).await.unwrap();
    let log = until_execs(&mut sub, &sid, None, 1).await;
    let plain = (
        "c1".to_string(),
        INVOKE_TOOL.to_string(),
        raw.to_string(),
        None,
    );
    assert_eq!(tool_events(&log, "call"), vec![plain.clone()]);
    assert_eq!(tool_events(&log, "exec"), vec![plain]);
}

/// Cases (c) + (d): stringified `args` are parsed; a reserved inner name is
/// left as an `invoke` call without an envelope.
#[tokio::test]
async fn string_args_unwrap_and_reserved_names_do_not() {
    let stringified = r#"{"name":"read","args":"{\"path\":\"x\"}"}"#;
    let reserved = r#"{"name":"invoke","args":{}}"#;
    let (holly, _) = engine(
        vec![tools(vec![
            call("c1", INVOKE_TOOL, stringified),
            call("c2", INVOKE_TOOL, reserved),
        ])],
        with_invoke(),
    );
    let sid = SessionId::new("s");
    let mut sub = holly.subscribe();
    holly.send(InMsg::prompt(sid.clone(), "go")).await.unwrap();
    let log = until_execs(&mut sub, &sid, None, 2).await;
    assert_eq!(
        tool_events(&log, "exec"),
        vec![
            (
                "c1".into(),
                "read".into(),
                r#"{"path":"x"}"#.into(),
                envelope(stringified)
            ),
            ("c2".into(), INVOKE_TOOL.into(), reserved.into(), None),
        ]
    );
}

/// Case (e): parallel calls in one round unwrap independently, and each
/// out-of-order result carries its own call's envelope.
#[tokio::test]
async fn parallel_invoke_calls_unwrap_independently() {
    let a = r#"{"name":"read","args":{"path":"a"}}"#;
    let b = r#"{"name":"grep","args":{"pattern":"p"}}"#;
    let batch = vec![
        call("c1", INVOKE_TOOL, a),
        call("c2", INVOKE_TOOL, b),
        call("c3", "read", r#"{"path":"n"}"#),
    ];
    let (holly, seen) = engine(vec![tools(batch.clone()), text("done")], with_invoke());
    let sid = SessionId::new("s");
    let mut sub = holly.subscribe();
    holly.send(InMsg::prompt(sid.clone(), "go")).await.unwrap();
    let mut log = until_execs(&mut sub, &sid, None, 3).await;
    let names: Vec<(String, Option<ToolEnvelope>)> = tool_events(&log, "exec")
        .into_iter()
        .map(|(_, tool, _, env)| (tool, env))
        .collect();
    assert_eq!(
        names,
        vec![
            ("read".into(), envelope(a)),
            ("grep".into(), envelope(b)),
            ("read".into(), None)
        ]
    );

    for id in ["c2", "c3", "c1"] {
        holly
            .send(InMsg::tool_result(sid.clone(), id, format!("out-{id}")))
            .await
            .unwrap();
    }
    log.extend(until_done(&mut sub, &sid).await);
    let outputs: Vec<(String, String, Option<ToolEnvelope>)> = tool_events(&log, "output")
        .into_iter()
        .map(|(id, tool, _, env)| (id, tool, env))
        .collect();
    assert_eq!(
        outputs,
        vec![
            ("c2".into(), "grep".into(), envelope(b)),
            ("c3".into(), "read".into(), None),
            ("c1".into(), "read".into(), envelope(a)),
        ]
    );
    assert_eq!(last_assistant_calls(&seen.lock().unwrap()[1]), batch);
}

/// Replay of a completed turn: a fresh engine resumed from the log sends the
/// control's exact history — the emitted `invoke` call rebuilt from its
/// envelope, paired with its result, the text round after it kept separate.
#[tokio::test]
async fn resumed_session_sends_the_emitted_invoke_call() {
    let raw = r#"{"name":"read","args":{"path":"x"}}"#;
    let (control, control_seen) = engine(
        vec![
            tools(vec![call("c1", INVOKE_TOOL, raw)]),
            text("done"),
            text("two-reply"),
        ],
        with_invoke(),
    );
    let sid = SessionId::new("s");
    let mut sub = control.subscribe();
    let prompt = InMsg::prompt(sid.clone(), "go");
    control.send(prompt.clone()).await.unwrap();
    let mut log = until_execs(&mut sub, &sid, Some(prompt), 1).await;
    control
        .send(InMsg::tool_result(sid.clone(), "c1", "contents"))
        .await
        .unwrap();
    log.extend(until_done(&mut sub, &sid).await);
    control
        .send(InMsg::prompt(sid.clone(), "two"))
        .await
        .unwrap();
    until_done(&mut sub, &sid).await;
    let control_two = control_seen.lock().unwrap().last().cloned().unwrap();
    assert_eq!(
        last_assistant_calls(&control_two),
        vec![call("c1", INVOKE_TOOL, raw)]
    );

    let (resumed, resumed_seen) = engine(vec![text("two-reply")], with_invoke());
    let mut sub = resumed.subscribe();
    resumed.resume(sid.clone(), log).await.unwrap();
    resumed
        .send(InMsg::prompt(sid.clone(), "two"))
        .await
        .unwrap();
    until_done(&mut sub, &sid).await;
    let resumed_two = resumed_seen.lock().unwrap().last().cloned().unwrap();

    assert_eq!(
        resumed_two, control_two,
        "resume rebuilds the control's history, the emitted invoke call included"
    );
}

/// Replay of a parked turn: the re-offered `ToolExec` and the resolving
/// `ToolOutput` carry the envelope, and the continuation request keeps the
/// emitted `invoke` call.
#[tokio::test]
async fn parked_resume_reoffers_the_unwrapped_call_with_its_envelope() {
    let raw = r#"{"name":"read","args":{"path":"x"}}"#;
    let (live, _) = engine(
        vec![tools(vec![call("c1", INVOKE_TOOL, raw)])],
        with_invoke(),
    );
    let sid = SessionId::new("s");
    let mut sub = live.subscribe();
    let prompt = InMsg::prompt(sid.clone(), "go");
    live.send(prompt.clone()).await.unwrap();
    let log = until_execs(&mut sub, &sid, Some(prompt), 1).await;

    let (resumed, seen) = engine(vec![text("done")], with_invoke());
    let mut sub = resumed.subscribe();
    resumed.resume(sid.clone(), log).await.unwrap();
    let reoffer = until_execs(&mut sub, &sid, None, 1).await;
    let inner = (
        "c1".to_string(),
        "read".to_string(),
        r#"{"path":"x"}"#.to_string(),
        envelope(raw),
    );
    assert_eq!(tool_events(&reoffer, "exec"), vec![inner]);

    resumed
        .send(InMsg::tool_result(sid.clone(), "c1", "contents"))
        .await
        .unwrap();
    let rest = until_done(&mut sub, &sid).await;
    assert_eq!(
        tool_events(&rest, "output"),
        vec![("c1".into(), "read".into(), "contents".into(), envelope(raw))]
    );
    let requests = seen.lock().unwrap().clone();
    assert_eq!(
        last_assistant_calls(requests.last().unwrap()),
        vec![call("c1", INVOKE_TOOL, raw)]
    );
}
