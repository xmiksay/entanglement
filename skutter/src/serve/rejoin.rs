//! Listing wakes a hibernated session (ADR-0247): a reconnecting client's
//! `ListPending{root}`/`ListQuestions{session}` for a session this process
//! hibernated resumes it from the log first — with the sub-agents its own
//! hibernate put down ([`revived_with`]) — then waits until every
//! call core re-offers on that resume (each replayed turn's unresolved
//! batch) is back as a card, a question or a result, so that one listing
//! shows them. Only roots are woken; a sub-agent's listing wakes its root.
//! One wake per root: a second listing waits for the first. The
//! connection's frames for any session in the woken tree queue behind the
//! listing ([`Relays`]), in order, while its other frames are served at
//! once. A live or unknown session is listed as before.

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, RwLock};
use std::time::Duration;

use entanglement_core::session::Session;
use entanglement_core::{AgentCatalog, EngineConfig, Holly, InMsg, OutEvent, SessionId, WireError};
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::mpsc;

use super::sleepers::{Begin, Sleepers};

use crate::session_store::{resume_with_mode, revived_with, SessionStore};
use crate::tool_names::{AGENT_SEND_TOOL, AGENT_TOOL, POLL_TOOL};

/// How long a listing waits for the re-offers before answering anyway.
const REOFFER_WAIT: Duration = Duration::from_secs(5);

/// The most frames one connection queues behind a waking listing before
/// its read loop waits (backpressure, not loss).
const RELAY_QUEUE: usize = 64;

type Wait = Pin<Box<dyn Future<Output = ()> + Send>>;

/// What `serve` needs to resume a session: the log store and the live agent
/// catalog (a retired agent resumes under a fallback, ADR-0215).
#[derive(Clone)]
pub struct Rejoin {
    store: SessionStore,
    agents: Arc<RwLock<AgentCatalog>>,
    sleepers: Arc<Mutex<Sleepers>>,
}

impl Rejoin {
    pub fn new(store: SessionStore, agents: Arc<RwLock<AgentCatalog>>) -> Self {
        Self {
            store,
            agents,
            sleepers: Arc::default(),
        }
    }

    /// Fold the hibernated roots and the parent links off `holly`, for as
    /// long as the engine runs. A hibernated id closes with no
    /// `SessionEnded`, so `CloseSession` is read off the inbound side.
    pub(super) fn track(&self, holly: &Holly) {
        let (mut events, mut inbound) = (holly.subscribe(), holly.subscribe_inbound());
        let sleepers = self.sleepers.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    ev = events.recv() => match ev {
                        Ok(OutEvent::SessionHibernated { session, .. }) => {
                            lock(&sleepers).hibernated(&session);
                        }
                        Ok(OutEvent::SessionStarted { session, parent, .. }) => {
                            lock(&sleepers).started(&session, parent.as_ref());
                        }
                        Ok(OutEvent::ParentChanged { session, parent }) => {
                            lock(&sleepers).reparented(&session, &parent);
                        }
                        Ok(OutEvent::SessionEnded { session, .. }) => {
                            lock(&sleepers).ended(&session);
                        }
                        Ok(_) => {}
                        Err(RecvError::Lagged(n)) => tracing::warn!(skipped = n, "serve: rejoin tracker lagged"),
                        Err(RecvError::Closed) => break,
                    },
                    msg = inbound.recv() => match msg {
                        Ok(InMsg::CloseSession { session }) => lock(&sleepers).ended(&session),
                        Ok(_) => {}
                        Err(RecvError::Lagged(n)) => tracing::warn!(skipped = n, "serve: rejoin tracker lagged"),
                        Err(RecvError::Closed) => break,
                    },
                }
            }
        });
    }

    /// The wait before a listing of `session` may go out: `None` when its
    /// root is not asleep (list it now). A sub-agent's listing wakes its
    /// root, which brings it back. The first caller starts the wake; a
    /// caller while it runs waits for the same one.
    fn begin_wake(&self, holly: &Holly, session: &SessionId) -> Option<Wait> {
        let mut sleepers = lock(&self.sleepers);
        let root = sleepers.root_of(session);
        match sleepers.begin(&root, holly.is_live(&root)) {
            Begin::Now => None,
            Begin::Follow(mut done) => Some(Box::pin(async move {
                let _ = done.wait_for(|finished| *finished).await;
            })),
            Begin::Lead(finished) => {
                let (rejoin, holly) = (self.clone(), holly.clone());
                Some(Box::pin(async move {
                    rejoin.wake(&holly, &root).await;
                    lock(&rejoin.sleepers).finish(&root, holly.is_live(&root));
                    finished.send_replace(true);
                }))
            }
        }
    }

    /// Resume `session` and the descendants its hibernate put down, and
    /// return once core's re-offers are back (bounded by [`REOFFER_WAIT`]).
    /// A failure is logged; the listing then answers as it would have.
    async fn wake(&self, holly: &Holly, session: &SessionId) {
        let catalog = self
            .agents
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        let (store, known, id) = (self.store.clone(), catalog.clone(), session.clone());
        let loaded = tokio::task::spawn_blocking(move || {
            store.load_for_resume(&id, |name| known.iter().any(|a| a.name == name))
        })
        .await;
        let (records, check) = match loaded {
            Ok(Ok(loaded)) => loaded,
            Ok(Err(e)) => return tracing::warn!(%session, "serve: cannot resume: {e:#}"),
            Err(e) => return tracing::warn!(%session, "serve: resume load failed: {e}"),
        };
        let revived = revived_with(&records, session);
        let replay = check.paired_for_resume(&records, false, None);
        let mut parked: HashSet<(SessionId, String)> = std::iter::once(session)
            .chain(&revived)
            .flat_map(|s| reoffered_ids(&replay, s, catalog.clone()))
            .collect();
        let mut events = holly.subscribe();
        if let Err(e) = resume_with_mode(holly, session, &records, &check, false, None).await {
            return tracing::warn!(%session, "serve: resume for a listing failed: {e:#}");
        }
        let back = async {
            while !parked.is_empty() {
                match events.recv().await {
                    Ok(
                        OutEvent::ToolRequest {
                            session,
                            request_id,
                            ..
                        }
                        | OutEvent::UserQuestion {
                            session,
                            request_id,
                            ..
                        }
                        | OutEvent::ToolOutput {
                            session,
                            request_id,
                            ..
                        },
                    ) => {
                        parked.remove(&(session, request_id));
                    }
                    Ok(_) => {}
                    Err(RecvError::Lagged(n)) => {
                        tracing::warn!(skipped = n, "serve: a wake's re-offer watch lagged")
                    }
                    Err(RecvError::Closed) => break,
                }
            }
        };
        if tokio::time::timeout(REOFFER_WAIT, back).await.is_err() {
            tracing::warn!(%session, "serve: listing answered before every re-offer arrived");
        }
    }
}

/// The calls core re-offers when it resumes `session` from `replay` that
/// come back as a card, a question or a result: the replayed turn's
/// unresolved batch, from core's own replay fold, so a stopped, finished or
/// retried turn re-offers nothing. A spawn, `agent_send` or `poll` call is
/// left out: re-offered, it waits on its sub-agent rather than answering.
fn reoffered_ids(
    replay: &[(Option<InMsg>, OutEvent)],
    session: &SessionId,
    agents: AgentCatalog,
) -> Vec<(SessionId, String)> {
    let cfg = EngineConfig {
        agents,
        ..EngineConfig::default()
    };
    match Session::replay(replay, &cfg, session) {
        Ok(s) => s
            .turn
            .into_iter()
            .flat_map(|t| t.pending)
            .filter(|c| ![AGENT_TOOL, AGENT_SEND_TOOL, POLL_TOOL].contains(&c.name.as_str()))
            .map(|c| (session.clone(), c.id))
            .collect(),
        Err(e) => {
            tracing::warn!(%session, "serve: cannot replay for a listing: {e:#}");
            Vec::new()
        }
    }
}

/// One connection's queues for sessions it woke: each gets a task that
/// forwards, in order, the frames queued for it — waiting out each wake
/// queued between them — for the rest of the connection, so a frame never
/// overtakes one queued earlier.
#[derive(Default)]
pub(super) struct Relays(HashMap<SessionId, mpsc::Sender<Relayed>>);

/// One item of a relay's queue.
enum Relayed {
    /// A wake every later frame waits for.
    Wake(Wait),
    Frame(Box<InMsg>),
}

impl Relays {
    /// Route `frame` for `session`: queued when a relay for it runs, or when
    /// it is the owner's listing (`wakes`) of a sleeping session, whose wake
    /// it queues first (a relay starts on the first). Handed back to forward
    /// directly otherwise.
    pub(super) async fn route(
        &mut self,
        rejoin: Option<&Rejoin>,
        holly: &Holly,
        session: &SessionId,
        wakes: bool,
        frame: InMsg,
    ) -> Option<InMsg> {
        // Keyed by the woken root: a frame for any session in its tree waits.
        let key = rejoin.map_or_else(|| session.clone(), |r| lock(&r.sleepers).root_of(session));
        let wake = rejoin
            .filter(|_| wakes)
            .and_then(|r| r.begin_wake(holly, session));
        if !self.0.contains_key(&key) {
            if wake.is_none() {
                return Some(frame);
            }
            self.0.insert(key.clone(), relay(holly.clone()));
        }
        let queue = self.0.get(&key)?;
        if let Some(wake) = wake {
            if queue.send(Relayed::Wake(wake)).await.is_err() {
                return Some(frame);
            }
        }
        match queue.send(Relayed::Frame(Box::new(frame))).await {
            Err(mpsc::error::SendError(Relayed::Frame(frame))) => Some(*frame),
            Ok(()) | Err(_) => None,
        }
    }
}

/// Start a relay task over `holly`, returning its bounded queue.
fn relay(holly: Holly) -> mpsc::Sender<Relayed> {
    let (tx, mut rx) = mpsc::channel::<Relayed>(RELAY_QUEUE);
    tokio::spawn(async move {
        while let Some(item) = rx.recv().await {
            let frame = match item {
                Relayed::Wake(wake) => {
                    wake.await;
                    continue;
                }
                Relayed::Frame(frame) => *frame,
            };
            match holly.send_from_wire(frame).await {
                Ok(()) => {}
                // The engine is gone: the outbound pump closes the socket,
                // which ends the connection.
                Err(WireError::Closed) => break,
                Err(e) => tracing::warn!("serve: queued frame refused: {e}"),
            }
        }
    });
    tx
}

fn lock(sleepers: &Mutex<Sleepers>) -> MutexGuard<'_, Sleepers> {
    sleepers.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
#[path = "rejoin_tests.rs"]
mod tests;
