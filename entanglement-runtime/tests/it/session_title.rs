//! Behavioral coverage for the auto session-title generator (#553): a session
//! that already has a name must never be re-titled, whether the name arrived
//! via a racing `/name` or was restored by `Resume`. Drives the real `Holly`
//! engine + `spawn_session_title_generator` end-to-end — the seam is the
//! public inbox/outbox, exactly like the other engine integration tests.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use entanglement_core::{
    stream_from_response, EngineConfig, Holly, InMsg, Llm, LlmFactory, LlmRequest, LlmResponse,
    LlmStream, ModelResolver, OutEvent, ResolvedModel, SessionId, UserId,
};
use entanglement_runtime::aux_llm::AuxLlmRegistry;
use entanglement_runtime::config::aux_models::AuxModelStore;
use entanglement_runtime::session_title::spawn_session_title_generator;
use tokio::sync::Notify;

// `ENTANGLEMENT_AUX_MODELS_FILE` is process-global; this file's tests serialize.
// Serialized via the harness-wide `crate::env_lock()` (ADR-0180).

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

fn meta_name(ev: &OutEvent) -> Option<String> {
    let OutEvent::SessionMetaChanged { name, .. } = ev else {
        unreachable!()
    };
    name.clone()
}

/// A one-shot `Llm` gated on a pair of `Notify`s: `entered` fires as soon as
/// `stream()` is called (so a test can wait until the generator has actually
/// reached its aux call), and `stream()` then blocks until the test signals
/// `proceed` — the hook that lets a test win a deliberate race against the
/// generator's write.
struct GatedLlm {
    entered: Arc<Notify>,
    proceed: Arc<Notify>,
    title: String,
}

#[async_trait]
impl Llm for GatedLlm {
    async fn stream(&mut self, _req: LlmRequest<'_>) -> anyhow::Result<LlmStream> {
        self.entered.notify_one();
        self.proceed.notified().await;
        Ok(stream_from_response(LlmResponse {
            text: self.title.clone(),
            tool_calls: vec![],
        }))
    }
}

/// An `AuxLlmRegistry` with no persisted pin (so `resolve()` always falls
/// back to the primary factory) wired to `factory`. `primary_concurrency` is
/// the cap `concurrency_cap` reports for the no-pin fallback (#589) — `None`
/// in the racing/resume tests below (irrelevant there), `Some(1)` in the
/// deferral test to simulate a contended model.
fn registry_with_primary(
    label: &str,
    factory: LlmFactory,
    primary_concurrency: Option<usize>,
) -> AuxLlmRegistry {
    let _g = crate::env_lock();
    let path = std::env::temp_dir().join(format!(
        "entanglement-session-title-it-{label}-{}.yml",
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
        primary_concurrency,
    )
}

/// Issue #553, scenario 1: `/name` racing the generator. The generator's aux
/// call is deliberately parked mid-flight so the test can land a user `/name`
/// (an ordinary `if_unset: false` `SetSessionMeta`) *while the generator is
/// still waiting on its LLM* — then release it. The late generated title must
/// not clobber the user's name.
#[tokio::test]
async fn a_racing_name_survives_the_generators_late_write() {
    let entered = Arc::new(Notify::new());
    let proceed = Arc::new(Notify::new());
    let factory: LlmFactory = {
        let entered = entered.clone();
        let proceed = proceed.clone();
        Arc::new(move || {
            Box::new(GatedLlm {
                entered: entered.clone(),
                proceed: proceed.clone(),
                title: "Generated Title".to_string(),
            }) as Box<dyn Llm>
        })
    };
    let registry = registry_with_primary("race", factory, None);

    let holly = Holly::spawn(EngineConfig::default());
    let handle = spawn_session_title_generator(&holly, registry);
    // The generator's `subscribe_inbound()` only runs once its spawned task
    // gets its first poll; yield so it's subscribed before anything is sent
    // (otherwise the broadcast fan-out can miss the `Prompt` below entirely).
    tokio::task::yield_now().await;
    let mut sub = holly.subscribe();
    let sid = SessionId::new("s1");

    holly
        .send(InMsg::prompt(sid.clone(), "fix the login bug"))
        .await
        .unwrap();
    // Wait until the generator has actually reached its (parked) aux call.
    entered.notified().await;

    // The user names the session while the generator is still waiting.
    holly
        .send(InMsg::SetSessionMeta {
            session: sid.clone(),
            name: Some("My Session".to_string()),
            action: None,
            if_unset: false,
        })
        .await
        .unwrap();
    let ev = recv_until(&mut sub, is_meta_changed).await;
    assert_eq!(meta_name(&ev).as_deref(), Some("My Session"));

    // Now let the generator's aux call complete and send its title.
    proceed.notify_one();
    let ev = recv_until(&mut sub, is_meta_changed).await;
    assert_eq!(
        meta_name(&ev).as_deref(),
        Some("My Session"),
        "the generator's late write must not clobber the user-set name"
    );

    handle.abort();
}

/// Issue #553, scenario 2: a resumed session that already has a name must not
/// be re-titled — and the generator must not even call the aux LLM for it, so
/// the `GatedLlm` staying un-entered (`entered.notified()` timing out) is
/// itself the assertion.
#[tokio::test]
async fn a_resumed_named_session_is_never_retitled() {
    let entered = Arc::new(Notify::new());
    let proceed = Arc::new(Notify::new());
    let factory: LlmFactory = {
        let entered = entered.clone();
        let proceed = proceed.clone();
        Arc::new(move || {
            Box::new(GatedLlm {
                entered: entered.clone(),
                proceed: proceed.clone(),
                title: "Generated Title".to_string(),
            }) as Box<dyn Llm>
        })
    };
    let registry = registry_with_primary("resume", factory, None);

    let holly = Holly::spawn(EngineConfig::default());
    let handle = spawn_session_title_generator(&holly, registry);
    // See the comment in the racing test: yield so the generator is
    // subscribed to the inbound fan-out before `Resume` is sent.
    tokio::task::yield_now().await;
    let mut sub = holly.subscribe();
    let sid = SessionId::new("s1");

    // A minimal resumable log: the session already has a name.
    let log = vec![(
        None,
        OutEvent::SessionMetaChanged {
            session: sid.clone(),
            name: Some("Resumed Session Name".to_string()),
            action: None,
        },
    )];
    holly.resume(sid.clone(), log).await.unwrap();
    recv_until(
        &mut sub,
        |e| matches!(e, OutEvent::Status { session, .. } if *session == sid),
    )
    .await;

    holly
        .send(InMsg::prompt(sid.clone(), "continue the work"))
        .await
        .unwrap();

    // The generator must skip the aux call entirely for an already-named
    // resumed session — `entered` never fires.
    let entered_in_time = tokio::time::timeout(Duration::from_millis(300), entered.notified())
        .await
        .is_ok();
    assert!(
        !entered_in_time,
        "the generator must not call the aux LLM for an already-named resumed session"
    );

    // Unblock the (unreached) gate defensively, then confirm the name held.
    proceed.notify_one();
    handle.abort();
}

/// Issue #589: when the primary model's effective concurrency cap is 1 (a
/// contended model, e.g. z.ai's glm-4.7-flash), the generator must not fire
/// its aux call while the main turn's own request is still in flight — that
/// would just have it queue behind the main turn's request for the model's
/// one permit. It must instead wait for the main turn to settle (`Done`) and
/// only then call the aux LLM.
#[tokio::test]
async fn a_contended_primary_model_defers_the_aux_call_until_the_turn_settles() {
    let aux_entered = Arc::new(Notify::new());
    let aux_proceed = Arc::new(Notify::new());
    let aux_factory: LlmFactory = {
        let aux_entered = aux_entered.clone();
        let aux_proceed = aux_proceed.clone();
        Arc::new(move || {
            Box::new(GatedLlm {
                entered: aux_entered.clone(),
                proceed: aux_proceed.clone(),
                title: "Generated Title".to_string(),
            }) as Box<dyn Llm>
        })
    };
    // `Some(1)`: simulates a primary model with a single in-flight permit, so
    // the generator's contention guard treats firing alongside the main turn
    // as guaranteed contention.
    let registry = registry_with_primary("contended", aux_factory, Some(1));

    let primary_entered = Arc::new(Notify::new());
    let primary_proceed = Arc::new(Notify::new());
    let primary_factory: LlmFactory = {
        let primary_entered = primary_entered.clone();
        let primary_proceed = primary_proceed.clone();
        Arc::new(move || {
            Box::new(GatedLlm {
                entered: primary_entered.clone(),
                proceed: primary_proceed.clone(),
                title: "ok".to_string(),
            }) as Box<dyn Llm>
        })
    };
    let holly = Holly::spawn(EngineConfig {
        llm_factory: primary_factory,
        ..EngineConfig::default()
    });
    let handle = spawn_session_title_generator(&holly, registry);
    tokio::task::yield_now().await;
    let mut sub = holly.subscribe();
    let sid = SessionId::new("s1");

    holly
        .send(InMsg::prompt(sid.clone(), "fix the login bug"))
        .await
        .unwrap();

    // The main turn's own request reaches the primary model first...
    primary_entered.notified().await;
    // ...and the aux call must NOT have fired yet.
    let aux_entered_early =
        tokio::time::timeout(Duration::from_millis(300), aux_entered.notified())
            .await
            .is_ok();
    assert!(
        !aux_entered_early,
        "the aux call must be deferred while the main turn is still in flight"
    );

    // Let the main turn finish — it emits `Done`, releasing the deferral.
    primary_proceed.notify_one();
    recv_until(
        &mut sub,
        |e| matches!(e, OutEvent::Done { session, .. } if *session == sid),
    )
    .await;

    // Now the aux call fires.
    tokio::time::timeout(Duration::from_secs(1), aux_entered.notified())
        .await
        .expect("the aux call must fire once the main turn has settled");
    aux_proceed.notify_one();
    let ev = recv_until(&mut sub, is_meta_changed).await;
    assert_eq!(meta_name(&ev).as_deref(), Some("Generated Title"));

    handle.abort();
}
