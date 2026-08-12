//! Behavioral coverage for the live action narrator (#635): a `ToolCall`
//! broadcast during a turn must produce a short aux-generated action,
//! delivered via `SetSessionMeta`, and a second `ToolCall` landing while a
//! narration call is already in flight must be skipped rather than queued.
//! Mirrors `session_title.rs`'s harness (a real `Holly` + the generator wired
//! against a gated aux `Llm`) — the seam is the public inbox/outbox, exactly
//! like the other engine integration tests.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use entanglement_core::{
    stream_from_response, EngineConfig, Holly, InMsg, Llm, LlmFactory, LlmRequest, LlmResponse,
    LlmStream, ModelResolver, OutEvent, ResolvedModel, SessionId, ToolCall, UserId,
};
use entanglement_runtime::aux_llm::AuxLlmRegistry;
use entanglement_runtime::config::aux_models::AuxModelStore;
use entanglement_runtime::narrate::spawn_action_narrator;
use tokio::sync::Notify;

async fn recv_until(
    sub: &mut tokio::sync::broadcast::Receiver<OutEvent>,
    pred: impl Fn(&OutEvent) -> bool,
) -> OutEvent {
    loop {
        let recv = tokio::time::timeout(Duration::from_secs(3), sub.recv())
            .await
            .expect("timed out waiting for a matching event");
        match recv {
            Ok(ev) if pred(&ev) => return ev,
            Ok(_) => {}
            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
            Err(_) => panic!("event stream closed before a matching event"),
        }
    }
}

fn is_meta_changed(e: &OutEvent) -> bool {
    matches!(e, OutEvent::SessionMetaChanged { .. })
}

fn meta_action(ev: &OutEvent) -> Option<String> {
    let OutEvent::SessionMetaChanged { action, .. } = ev else {
        unreachable!()
    };
    action.clone()
}

/// A one-shot `Llm` gated on a pair of `Notify`s (mirrors `session_title.rs`'s
/// `GatedLlm`): `entered` fires as soon as `stream()` is called (so a test can
/// wait until the narrator has actually reached its aux call), and `stream()`
/// then blocks until the test signals `proceed`.
struct GatedLlm {
    entered: Arc<Notify>,
    proceed: Arc<Notify>,
    action: String,
}

#[async_trait]
impl Llm for GatedLlm {
    async fn stream(&mut self, _req: LlmRequest<'_>) -> anyhow::Result<LlmStream> {
        self.entered.notify_one();
        self.proceed.notified().await;
        Ok(stream_from_response(LlmResponse {
            text: self.action.clone(),
            tool_calls: vec![],
        }))
    }
}

/// An `AuxLlmRegistry` with no persisted `narrate` pin, so `resolve()` always
/// falls back to `factory` — the resolver is never actually consulted (a
/// no-pin `resolve` short-circuits straight to the primary), so an
/// always-erroring stub is a faithful stand-in.
fn registry_with_primary(label: &str, factory: LlmFactory) -> AuxLlmRegistry {
    let _g = crate::env_lock();
    let path = std::env::temp_dir().join(format!(
        "entanglement-narrate-it-{label}-{}.yml",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&path);
    // SAFETY: single-threaded within the ENV_LOCK critical section.
    unsafe {
        std::env::set_var("ENTANGLEMENT_AUX_MODELS_FILE", &path);
    }
    let store = Arc::new(Mutex::new(AuxModelStore::load()));
    unsafe {
        std::env::remove_var("ENTANGLEMENT_AUX_MODELS_FILE");
    }
    let resolver: ModelResolver = Arc::new(|_u: Option<&UserId>, _p: &str, _m: &str| {
        Err::<ResolvedModel, String>("no catalog in this test".to_string())
    });
    AuxLlmRegistry::new(
        store,
        resolver,
        factory,
        entanglement_provider::Catalog { providers: vec![] },
        None,
    )
}

/// The root turn's `Llm`: always answers with a single tool call. This test
/// never sends `InMsg::ToolResult` back, so the round stays parked — only the
/// pre-execution `ToolCall` broadcast (emitted unconditionally, before any
/// permission check, ADR-0176) matters here.
struct ToolCallingLlm;

#[async_trait]
impl Llm for ToolCallingLlm {
    async fn stream(&mut self, _req: LlmRequest<'_>) -> anyhow::Result<LlmStream> {
        Ok(stream_from_response(LlmResponse {
            text: String::new(),
            tool_calls: vec![ToolCall::new("call-1", "read", r#"{"path":"src/main.rs"}"#)],
        }))
    }
}

#[tokio::test]
async fn a_tool_call_narrates_the_action() {
    let entered = Arc::new(Notify::new());
    let proceed = Arc::new(Notify::new());
    let factory: LlmFactory = {
        let entered = entered.clone();
        let proceed = proceed.clone();
        Arc::new(move || {
            Box::new(GatedLlm {
                entered: entered.clone(),
                proceed: proceed.clone(),
                action: "Reading src/main.rs".to_string(),
            }) as Box<dyn Llm>
        })
    };
    let registry = registry_with_primary("basic", factory);

    let holly = Holly::spawn(EngineConfig {
        llm_factory: Arc::new(|| Box::new(ToolCallingLlm) as Box<dyn Llm>),
        ..EngineConfig::default()
    });
    let handle = spawn_action_narrator(&holly, registry);
    // The narrator's `subscribe()` only runs once its spawned task gets its
    // first poll; yield so it's subscribed before anything is sent (otherwise
    // the broadcast fan-out can miss the `ToolCall` below entirely).
    tokio::task::yield_now().await;
    let mut sub = holly.subscribe();
    let sid = SessionId::new("s1");

    holly
        .send(InMsg::prompt(sid.clone(), "read the file"))
        .await
        .unwrap();

    // The narrator's aux call fires once the `ToolCall` broadcast lands.
    entered.notified().await;
    proceed.notify_one();

    let ev = recv_until(&mut sub, is_meta_changed).await;
    assert_eq!(meta_action(&ev).as_deref(), Some("Reading src/main.rs"));

    handle.abort();
}

/// The root turn's `Llm` for the burst test: answers with *two* tool calls in
/// one round. Core emits both `ToolCall` broadcasts back-to-back with no
/// await between them (`round.rs`'s batch-emit loop), so this reliably lands
/// a second `ToolCall` for the same session while the narrator's first aux
/// call is still gated.
struct TwoToolCallingLlm;

#[async_trait]
impl Llm for TwoToolCallingLlm {
    async fn stream(&mut self, _req: LlmRequest<'_>) -> anyhow::Result<LlmStream> {
        Ok(stream_from_response(LlmResponse {
            text: String::new(),
            tool_calls: vec![
                ToolCall::new("call-1", "read", r#"{"path":"a.rs"}"#),
                ToolCall::new("call-2", "read", r#"{"path":"b.rs"}"#),
            ],
        }))
    }
}

/// A `ToolCall` for a session that already has a narration call in flight is
/// skipped, not queued behind it — the `entered`/`proceed` gate on the first
/// call lets this test hold the narrator mid-call while the round's second
/// `ToolCall` broadcasts, then asserts only one aux call ever fired.
#[tokio::test]
async fn a_second_tool_call_is_skipped_while_one_is_in_flight() {
    let entered = Arc::new(Notify::new());
    let proceed = Arc::new(Notify::new());
    let call_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let factory: LlmFactory = {
        let entered = entered.clone();
        let proceed = proceed.clone();
        let call_count = call_count.clone();
        Arc::new(move || {
            call_count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Box::new(GatedLlm {
                entered: entered.clone(),
                proceed: proceed.clone(),
                action: "Reading a.rs".to_string(),
            }) as Box<dyn Llm>
        })
    };
    let registry = registry_with_primary("burst", factory);

    let holly = Holly::spawn(EngineConfig {
        llm_factory: Arc::new(|| Box::new(TwoToolCallingLlm) as Box<dyn Llm>),
        ..EngineConfig::default()
    });
    let handle = spawn_action_narrator(&holly, registry);
    tokio::task::yield_now().await;
    let mut sub = holly.subscribe();
    let sid = SessionId::new("s1");

    holly
        .send(InMsg::prompt(sid.clone(), "read two files"))
        .await
        .unwrap();

    // Both `ToolCall`s land before the narrator's first (gated) aux call
    // returns; only one aux call must have started.
    entered.notified().await;
    let both_tool_calls_seen = {
        let mut seen = 0;
        while seen < 2 {
            match recv_until(&mut sub, |e| matches!(e, OutEvent::ToolCall { .. })).await {
                OutEvent::ToolCall { .. } => seen += 1,
                _ => unreachable!(),
            }
        }
        seen
    };
    assert_eq!(both_tool_calls_seen, 2);
    assert_eq!(
        call_count.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "the second ToolCall must not start a second aux call while the first is in flight"
    );

    proceed.notify_one();
    let ev = recv_until(&mut sub, is_meta_changed).await;
    assert_eq!(meta_action(&ev).as_deref(), Some("Reading a.rs"));

    handle.abort();
}
