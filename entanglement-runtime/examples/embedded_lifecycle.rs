//! Dynamic-resolver + lifecycle embedding seams (issue #364, follow-up to #315).
//!
//! `examples/embedded.rs` exercises the policy seam (#311) end-to-end; the
//! other seams `docs/embedding.md` documents had no compiled example, so a
//! signature change to any of them could break the guide's prose without
//! breaking CI. This sibling closes that gap, all against `EchoLlm` (no
//! provider key, deterministic):
//!
//! - `tool_spec_resolver` (#308) / `system_prompt_resolver` (#310) — the
//!   snapshot-cache pattern (guide §4): an embedder-owned
//!   `Arc<RwLock<HashMap<SessionId, T>>>`, read by a sync `Fn` on the turn's
//!   hot path;
//! - `RecordSink` + `spawn_persistence_subscriber_with_sink` (#313) — a custom,
//!   in-memory persistence target standing in for a DB-backed one;
//! - `Holly::hibernate` → `session_store::pair_records` → `Holly::resume` — the
//!   full eviction/lazy-reload round-trip (#318), reading the replay records
//!   back from the custom sink instead of a file.
//!
//! Run with `cargo run -p entanglement-runtime --example embedded_lifecycle
//! --no-default-features`.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};

use entanglement_core::{AgentProfile, EngineConfig, Holly, InMsg, OutEvent, SessionId, ToolSpec};
use entanglement_runtime::host;
use entanglement_runtime::persistence::{spawn_persistence_subscriber_with_sink, RecordSink};
use entanglement_runtime::session_store::{integrity_gap, pair_records, LogRecord};
use tokio::sync::broadcast::{self, error::RecvError};

/// The in-memory stand-in for a DB-backed `RecordSink` (#313): every record the
/// tap hands it, keyed by root session. A real sink whose store can block must
/// put a bounded channel + writer task behind `append` and return immediately
/// (guide §3); a plain mutex push already satisfies that for this example.
#[derive(Default)]
struct MemorySink {
    logs: Mutex<HashMap<SessionId, Vec<LogRecord>>>,
}

impl RecordSink for MemorySink {
    fn append(&self, root: &SessionId, record: &LogRecord) -> anyhow::Result<()> {
        self.logs
            .lock()
            .unwrap()
            .entry(root.clone())
            .or_default()
            .push(record.clone());
        Ok(())
    }
}

impl MemorySink {
    fn records_for(&self, root: &SessionId) -> Vec<LogRecord> {
        self.logs
            .lock()
            .unwrap()
            .get(root)
            .cloned()
            .unwrap_or_default()
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let tool_specs = host::host_tools(std::env::temp_dir()).specs();
    let session = SessionId::new("lifecycle-demo");

    // Snapshot caches behind the two sync resolvers (guide §4). Seeded once
    // here; a real embedder refreshes them from its own store on a slower
    // cadence than the turn's hot path, never doing I/O inside the closure.
    let tool_cache: Arc<RwLock<HashMap<SessionId, Vec<ToolSpec>>>> = Arc::new(RwLock::new(
        HashMap::from([(session.clone(), tool_specs.clone())]),
    ));
    let prompt_cache: Arc<RwLock<HashMap<SessionId, String>>> =
        Arc::new(RwLock::new(HashMap::from([(
            session.clone(),
            "You are a lifecycle-demo agent — answer tersely.".to_string(),
        )])));

    let tool_cache_read = tool_cache.clone();
    let prompt_cache_read = prompt_cache.clone();
    let holly = Holly::spawn(EngineConfig {
        tool_specs,
        tool_spec_resolver: Some(Arc::new(move |session: &SessionId| {
            tool_cache_read
                .read()
                .unwrap()
                .get(session)
                .cloned()
                .unwrap_or_default()
        })),
        system_prompt_resolver: Some(Arc::new(
            move |session: &SessionId, _profile: &AgentProfile| {
                prompt_cache_read.read().unwrap().get(session).cloned()
            },
        )),
        ..EngineConfig::default() // EchoLlm — no provider key, deterministic
    });

    let sink = Arc::new(MemorySink::default());
    let _persistence = spawn_persistence_subscriber_with_sink(&holly, sink.clone());

    // One subscriber threaded through every step so no event can slip past
    // between a send and the wait for its reaction.
    let mut sub = holly.subscribe();

    run_turn(&holly, &mut sub, &session, "first turn, before hibernation").await?;

    holly.hibernate(session.clone()).await?;
    recv_until(
        &mut sub,
        |ev| matches!(ev, OutEvent::SessionHibernated { session: s, .. } if s == &session),
    )
    .await?;
    println!("[{session}] hibernated — in-memory session state torn down");

    // Lazy resume (guide §3): nothing is loaded until we actually pay for it,
    // here by reading the custom sink's own log instead of a DB round-trip.
    let records = sink.records_for(&session);
    if let Some(dropped) = integrity_gap(&records) {
        anyhow::bail!("refusing to resume: log is missing {dropped} record(s)");
    }
    holly
        .resume(session.clone(), pair_records(&records))
        .await?;
    recv_until(
        &mut sub,
        |ev| matches!(ev, OutEvent::SessionStarted { session: s, .. } if s == &session),
    )
    .await?;

    // Same embedder-side caches, untouched by the hibernate/resume round-trip:
    // the resolvers still see this session's entries on the resumed turn.
    run_turn(&holly, &mut sub, &session, "second turn, after resume").await?;

    Ok(())
}

/// Send one prompt and relay `session`'s own text/completion — the same
/// ownership-filtered relay `embedded.rs` uses, threaded over a shared
/// subscriber so callers can interleave other waits (hibernate/resume) on it.
async fn run_turn(
    holly: &Holly,
    sub: &mut broadcast::Receiver<OutEvent>,
    session: &SessionId,
    prompt: &str,
) -> anyhow::Result<()> {
    holly
        .send(InMsg::prompt(session.clone(), prompt.to_string()))
        .await?;
    loop {
        let ev = recv_next(sub).await?;
        if ev.session() != Some(session) {
            continue;
        }
        if let OutEvent::TextDelta { text, .. } = &ev {
            println!("[{session}] {text}");
        }
        if matches!(ev, OutEvent::Done { .. }) {
            break;
        }
    }
    Ok(())
}

async fn recv_next(sub: &mut broadcast::Receiver<OutEvent>) -> anyhow::Result<OutEvent> {
    loop {
        match sub.recv().await {
            Ok(ev) => return Ok(ev),
            Err(RecvError::Lagged(_)) => continue,
            Err(RecvError::Closed) => anyhow::bail!("holly's outbound channel closed"),
        }
    }
}

async fn recv_until(
    sub: &mut broadcast::Receiver<OutEvent>,
    pred: impl Fn(&OutEvent) -> bool,
) -> anyhow::Result<OutEvent> {
    loop {
        let ev = recv_next(sub).await?;
        if pred(&ev) {
            return Ok(ev);
        }
    }
}
