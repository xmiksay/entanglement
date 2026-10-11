//! A compaction successor continues its source's live binding (#768): a
//! `SetModel`, `SetGeneration` and `SetMode` made on the source survive every
//! fork path (manual `/compact`, auto-summarize, the prune-only fallback,
//! ADR-0205) instead of the successor falling back to the agent's pin. The
//! backend is carried, not re-resolved, so a model that has since become
//! unresolvable cannot refuse the successor and lose the session.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use entanglement_core::session::Session;
use entanglement_core::{
    stream_from_response, Agent, AgentCatalog, EngineConfig, GenerationParams, Holly, InMsg, Llm,
    LlmRequest, LlmResponse, LlmStream, ModelResolver, OutEvent, ResolvedModel, SessionId,
    ToolCall, ToolSpec,
};
use tokio::sync::broadcast::Receiver;

/// Every request's `(model, temperature)`, in order, across every backend.
type Seen = Arc<Mutex<Vec<(Option<String>, Option<f32>)>>>;
/// Every request's advertised tool names, in the same order as `Seen`.
type Tools = Arc<Mutex<Vec<Vec<String>>>>;

/// Records each request, calls `probe` when the last message is exactly
/// `"tool"`, and otherwise answers `"done"` (a compaction summary included).
struct Recording {
    seen: Seen,
    tools: Tools,
}

#[async_trait]
impl Llm for Recording {
    async fn stream(&mut self, req: LlmRequest<'_>) -> anyhow::Result<LlmStream> {
        self.seen.lock().unwrap().push((
            req.model.map(str::to_string),
            req.generation.and_then(|g| g.temperature),
        ));
        self.tools
            .lock()
            .unwrap()
            .push(req.tools.iter().map(|t| t.name.clone()).collect());
        let wants_tool = req.messages.last().is_some_and(|m| m.text() == "tool");
        Ok(stream_from_response(if wants_tool {
            LlmResponse {
                tool_calls: vec![ToolCall::new("c1", "probe", "{}")],
                ..Default::default()
            }
        } else {
            LlmResponse {
                text: "done".into(),
                ..Default::default()
            }
        }))
    }
}

/// The resolver plus the knobs a test turns: `fail` makes every later
/// resolution error (a revoked key, a catalog edit), `calls` counts them.
pub(crate) struct Harness {
    seen: Seen,
    pub(crate) tools: Tools,
    fail: Arc<AtomicBool>,
    calls: Arc<AtomicUsize>,
}

impl Harness {
    pub(crate) fn new() -> Self {
        Self {
            seen: Arc::new(Mutex::new(Vec::new())),
            tools: Arc::new(Mutex::new(Vec::new())),
            fail: Arc::new(AtomicBool::new(false)),
            calls: Arc::new(AtomicUsize::new(0)),
        }
    }

    fn factory(&self) -> entanglement_core::LlmFactory {
        let (seen, tools) = (self.seen.clone(), self.tools.clone());
        Arc::new(move || {
            Box::new(Recording {
                seen: seen.clone(),
                tools: tools.clone(),
            }) as Box<dyn Llm>
        })
    }

    fn resolver(&self) -> ModelResolver {
        let (factory, fail, calls) = (self.factory(), self.fail.clone(), self.calls.clone());
        Arc::new(move |_user, provider: &str, model: &str| {
            calls.fetch_add(1, Ordering::SeqCst);
            let known = matches!(
                (provider, model),
                ("anthropic", "claude-x") | ("zai", "glm-b")
            );
            if fail.load(Ordering::SeqCst) || !known {
                return Err(format!("cannot resolve `{provider}/{model}`"));
            }
            Ok(ResolvedModel {
                provider: provider.into(),
                model: model.into(),
                llm_factory: factory.clone(),
                generation: None,
                // Small enough for the overflow paths to trip.
                context_window: Some(4_000),
            })
        })
    }

    /// `general` pinned to `anthropic/claude-x` — the binding a successor
    /// must *not* fall back to.
    pub(crate) fn config(&self, auto_compact: bool) -> EngineConfig {
        let mut agents = AgentCatalog::default();
        agents.insert(Agent {
            name: "general".into(),
            description: String::new(),
            system_prompt: String::new(),
            model: Some("claude-x".into()),
            provider: Some("anthropic".into()),
            default_mode: None,
        });
        EngineConfig {
            llm_factory: self.factory(),
            agents,
            model_resolver: Some(self.resolver()),
            tool_specs: vec![ToolSpec::new("probe", "a test tool")],
            context_window: Some(4_000),
            auto_compact,
            ..EngineConfig::default()
        }
    }

    pub(crate) fn last_request(&self) -> (Option<String>, Option<f32>) {
        self.seen
            .lock()
            .unwrap()
            .last()
            .cloned()
            .expect("a request")
    }
}

pub(crate) async fn next(sub: &mut Receiver<OutEvent>) -> OutEvent {
    loop {
        match tokio::time::timeout(Duration::from_secs(5), sub.recv())
            .await
            .expect("timed out waiting for an event")
        {
            Ok(ev) => return ev,
            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
            Err(e) => panic!("event stream closed: {e}"),
        }
    }
}

pub(crate) async fn until_done(sub: &mut Receiver<OutEvent>, sid: &SessionId) {
    while !matches!(next(sub).await, OutEvent::Done { session, .. } if session == *sid) {}
}

/// One turn under the pin, then the live switches every test carries over.
pub(crate) async fn start_and_switch(holly: &Holly, sub: &mut Receiver<OutEvent>, sid: &SessionId) {
    holly.send(InMsg::prompt(sid.clone(), "one")).await.unwrap();
    until_done(sub, sid).await;
    for msg in [
        InMsg::SetModel {
            session: sid.clone(),
            provider: "zai".into(),
            model: "glm-b".into(),
        },
        InMsg::SetGeneration {
            session: sid.clone(),
            overrides: GenerationParams {
                temperature: Some(0.3),
                ..Default::default()
            },
        },
        InMsg::SetMode {
            session: sid.clone(),
            mode: "plan".into(),
        },
    ] {
        holly.send(msg).await.unwrap();
    }
    while !matches!(next(sub).await, OutEvent::ModeChanged { session, .. } if session == *sid) {}
}

/// Drain until `source`'s successor finishes its first turn; return its id and
/// every event it emitted.
pub(crate) async fn successor_turn(
    sub: &mut Receiver<OutEvent>,
    source: &SessionId,
) -> (SessionId, Vec<OutEvent>) {
    let successor = loop {
        if let OutEvent::SessionStarted {
            session,
            predecessor: Some(p),
            ..
        } = next(sub).await
        {
            if p == *source {
                break session;
            }
        }
    };
    let mut events = Vec::new();
    loop {
        let ev = next(sub).await;
        if ev.session() == Some(&successor) {
            let done = matches!(ev, OutEvent::Done { .. });
            events.push(ev);
            if done {
                return (successor, events);
            }
        }
    }
}

/// The three ADR-0205 fork paths.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Path {
    Manual,
    Auto,
    Prune,
}

impl Path {
    pub(crate) const ALL: [Path; 3] = [Path::Manual, Path::Auto, Path::Prune];

    /// Prune-only is the fallback when auto-summarize is off.
    pub(crate) fn auto_compact(self) -> bool {
        !matches!(self, Path::Prune)
    }
}

/// Compact `sid` along `path` and return its successor's first turn.
pub(crate) async fn compact(
    holly: &Holly,
    sub: &mut Receiver<OutEvent>,
    sid: &SessionId,
    path: Path,
) -> (SessionId, Vec<OutEvent>) {
    match path {
        Path::Manual => {
            holly
                .send(InMsg::Oneshot {
                    session: sid.clone(),
                    op: "compact".into(),
                    args: serde_json::Value::Null,
                })
                .await
                .unwrap();
        }
        // The `auto_compact` suite's overflow shape: a few short turns give
        // the summary a head, then one large prompt overflows the 4k window.
        Path::Auto => {
            for i in 0..3 {
                let text = format!("turn-{i}: {}", "y".repeat(490));
                holly.send(InMsg::prompt(sid.clone(), text)).await.unwrap();
                until_done(sub, sid).await;
            }
            holly
                .send(InMsg::prompt(sid.clone(), "x".repeat(11_000)))
                .await
                .unwrap();
        }
        // One bulky tool result overflows the window; with auto-summarize
        // off the prune fallback forks.
        Path::Prune => {
            holly
                .send(InMsg::prompt(sid.clone(), "tool"))
                .await
                .unwrap();
            let request_id = loop {
                if let OutEvent::ToolExec {
                    session,
                    request_id,
                    ..
                } = next(sub).await
                {
                    if session == *sid {
                        break request_id;
                    }
                }
            };
            holly
                .send(InMsg::tool_result(
                    sid.clone(),
                    request_id,
                    "x".repeat(14_000),
                ))
                .await
                .unwrap();
        }
    }
    successor_turn(sub, sid).await
}

/// The successor's first round ran on the switched model and generation, it
/// never rebound to the pin, and its own log replays to the same state.
fn assert_continues(h: &Harness, cfg: &EngineConfig, successor: &SessionId, events: &[OutEvent]) {
    assert_eq!(h.last_request(), (Some("glm-b".into()), Some(0.3)));
    let models: Vec<&str> = events
        .iter()
        .filter_map(|e| match e {
            OutEvent::ModelChanged { model, .. } => Some(model.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(models, ["glm-b"], "the pin is never reapplied: {events:?}");
    assert!(events
        .iter()
        .any(|e| matches!(e, OutEvent::ModeChanged { mode, .. } if mode == "plan")));

    let records: Vec<(Option<InMsg>, OutEvent)> =
        events.iter().cloned().map(|e| (None, e)).collect();
    let replayed = Session::replay(&records, cfg, successor).expect("replay");
    assert_eq!(replayed.model.as_deref(), Some("glm-b"));
    assert_eq!(replayed.generation.and_then(|g| g.temperature), Some(0.3));
    assert_eq!(replayed.mode, "plan");
}

#[tokio::test]
async fn every_compaction_successor_keeps_the_live_binding() {
    for path in Path::ALL {
        let h = Harness::new();
        let cfg = h.config(path.auto_compact());
        let holly = Holly::spawn(cfg.clone());
        let sid = SessionId::new("s1");
        let mut sub = holly.subscribe();
        start_and_switch(&holly, &mut sub, &sid).await;

        let (successor, events) = compact(&holly, &mut sub, &sid, path).await;
        assert_continues(&h, &cfg, &successor, &events);
    }
}

/// The switched model stops resolving before the fork: the successor still
/// starts — on the backend the source was already bound to — because the fork
/// resolves nothing at all.
#[tokio::test]
async fn an_unresolvable_model_does_not_lose_the_session() {
    let h = Harness::new();
    let cfg = h.config(true);
    let holly = Holly::spawn(cfg.clone());
    let sid = SessionId::new("s1");
    let mut sub = holly.subscribe();
    start_and_switch(&holly, &mut sub, &sid).await;
    h.fail.store(true, Ordering::SeqCst);
    let resolutions = h.calls.load(Ordering::SeqCst);

    holly
        .send(InMsg::Oneshot {
            session: sid.clone(),
            op: "compact".into(),
            args: serde_json::Value::Null,
        })
        .await
        .unwrap();
    let (successor, events) = successor_turn(&mut sub, &sid).await;

    assert!(holly.is_live(&successor));
    assert_eq!(h.last_request(), (Some("glm-b".into()), Some(0.3)));
    assert_eq!(
        h.calls.load(Ordering::SeqCst),
        resolutions,
        "nothing re-resolved"
    );
    assert!(
        !events.iter().any(|e| matches!(e, OutEvent::Error { .. })),
        "{events:?}"
    );
}

/// Resuming the successor keeps the inherited generation — and keeps it on
/// every *later* resume too: the resume's corrective `ModelChanged`
/// re-announce lands in the log after the `GenerationChanged`, so it must
/// re-log the live params or the next replay folds back to the catalog's.
#[tokio::test]
async fn a_twice_resumed_successor_keeps_the_inherited_generation() {
    let h = Harness::new();
    let cfg = h.config(true);
    let holly = Holly::spawn(cfg.clone());
    let sid = SessionId::new("s1");
    let mut sub = holly.subscribe();
    start_and_switch(&holly, &mut sub, &sid).await;
    holly
        .send(InMsg::Oneshot {
            session: sid.clone(),
            op: "compact".into(),
            args: serde_json::Value::Null,
        })
        .await
        .unwrap();
    let (successor, mut log) = successor_turn(&mut sub, &sid).await;

    for _ in 0..2 {
        let fresh = Holly::spawn(cfg.clone());
        let mut fresh_sub = fresh.subscribe();
        let records = log.iter().cloned().map(|e| (None, e)).collect();
        fresh.resume(successor.clone(), records).await.unwrap();
        fresh
            .send(InMsg::prompt(successor.clone(), "again"))
            .await
            .unwrap();
        // This engine's events extend the persisted log the next resume reads.
        loop {
            let ev = next(&mut fresh_sub).await;
            if ev.session() == Some(&successor) {
                let done = matches!(ev, OutEvent::Done { .. });
                log.push(ev);
                if done {
                    break;
                }
            }
        }
        assert_eq!(h.last_request(), (Some("glm-b".into()), Some(0.3)));
    }
}
