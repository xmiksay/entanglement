//! The observe-only lifecycle hook points (#734): `session_start`,
//! `session_end`, `stop`, `pre_compact`, `subagent_stop` and `notification`.
//!
//! Core stays policy-free (ADR-0066): every point is *derived* from the
//! `OutEvent` stream a runtime subscriber already sees, so no new core seam
//! exists — and therefore none of them can block. A subscriber sees an event
//! only after core acted on it, so there is nothing left to veto; the one
//! point that sounds like a gate, `pre_compact`, would need a core seam to
//! hold the summary back, and since ADR-0205 compaction forks and never
//! rewrites the source log, there is no history for a hook to rescue first.
//!
//! The [`Tracker`] is the pure part: it folds events into [`Firing`]s. The
//! [`spawn_watch`] task feeds it and hands each firing to one sequential worker,
//! so hooks run in event order while a slow hook never lags the subscription.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use entanglement_core::{AgentState, CompactionMode, InMsg, OutEvent, SessionId};
use serde_json::{json, Value};
use tokio::sync::broadcast::{self, error::RecvError, error::TryRecvError};
use tokio::sync::mpsc;
use tokio::task::JoinSet;

use super::{parse_input, HookEvent, Hooks};

/// Every point this module drives, for [`Hooks::has_lifecycle`].
pub(crate) const EVENTS: [HookEvent; 6] = [
    HookEvent::SessionStart,
    HookEvent::SessionEnd,
    HookEvent::Stop,
    HookEvent::PreCompact,
    HookEvent::SubagentStop,
    HookEvent::Notification,
];

/// One hook point to run: its payload already carries `event` and `session`.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Firing {
    pub(crate) event: HookEvent,
    pub(crate) session: SessionId,
    /// Exported as `ENTANGLEMENT_TOOL_NAME` (an approval `notification`).
    pub(crate) tool: Option<String>,
    pub(crate) payload: Value,
}

/// What a session's `SessionStarted` said, kept for the later points.
#[derive(Debug, Clone)]
struct Meta {
    parent: Option<SessionId>,
    agent: String,
}

/// Folds the event stream into hook firings. Pure: no I/O, no clock.
#[derive(Debug, Default)]
pub(crate) struct Tracker {
    sessions: HashMap<SessionId, Meta>,
    /// Ids an `InMsg::Resume` is about to re-announce (its records' sub-tree).
    resuming: HashSet<SessionId>,
    /// Sessions that announced `Compacted`: they are retiring into a successor.
    compacted: HashSet<SessionId>,
    /// Sessions whose `Done` arrived; the `Status` right after it says how
    /// the turn ended (`emit_turn_done`/`emit_turn_error` always pair them).
    ended_turn: HashSet<SessionId>,
    last_error: HashMap<SessionId, String>,
}

impl Tracker {
    /// Note an inbound frame. Only `Resume` matters: it is fanned out before
    /// the supervisor handles it, so it is always seen ahead of the
    /// `SessionStarted`s it causes (see [`spawn_watch`]).
    pub(crate) fn observe_in(&mut self, msg: &InMsg) {
        if let InMsg::Resume { records, .. } = msg {
            for (_, ev) in records {
                if let OutEvent::SessionStarted { session, .. } = ev {
                    self.resuming.insert(session.clone());
                }
            }
        }
    }

    /// Fold one outbound event; returns the hook point it completes, if any.
    pub(crate) fn observe(&mut self, ev: &OutEvent) -> Option<Firing> {
        match ev {
            OutEvent::SessionStarted {
                session,
                parent,
                predecessor,
                agent,
                model,
                root,
                ..
            } => {
                let source = if self.resuming.remove(session) {
                    "resume"
                } else if predecessor.is_some() {
                    "compact"
                } else {
                    "fresh"
                };
                self.sessions.insert(
                    session.clone(),
                    Meta {
                        parent: parent.clone(),
                        agent: agent.clone(),
                    },
                );
                let extra = json!({
                    "source": source,
                    "parent": parent.as_ref().map(|p| &p.0),
                    "predecessor": predecessor.as_ref().map(|p| &p.0),
                    "agent": agent,
                    "model": model,
                    "root": root,
                });
                Some(fire(HookEvent::SessionStart, session, None, extra))
            }
            OutEvent::SessionEnded { session, .. } => {
                let reason = match self.compacted.contains(session) {
                    true => "compacted",
                    false => "closed",
                };
                Some(self.end(session, reason))
            }
            OutEvent::SessionHibernated { session, .. } => Some(self.end(session, "hibernated")),
            OutEvent::Error {
                session, message, ..
            } => {
                self.last_error.insert(session.clone(), message.clone());
                None
            }
            OutEvent::Done { session, .. } => {
                // A manual `/compact` closes its op with a `Done` too; that is
                // the retiring source's bookkeeping, not a model turn ending.
                if !self.compacted.contains(session) {
                    self.ended_turn.insert(session.clone());
                }
                None
            }
            OutEvent::Status { session, state } if self.ended_turn.remove(session) => {
                Some(self.stop(session, *state))
            }
            OutEvent::Compacted {
                session,
                auto,
                mode,
                kept,
                summary,
                ..
            } => {
                self.compacted.insert(session.clone());
                let kind = match (auto, mode) {
                    (false, _) => "manual",
                    (true, CompactionMode::Summary) => "auto",
                    (true, CompactionMode::Prune) => "prune",
                };
                let extra = json!({ "kind": kind, "kept": kept, "summary": summary });
                Some(fire(HookEvent::PreCompact, session, None, extra))
            }
            OutEvent::ToolRequest {
                session,
                request_id,
                tool,
                input,
                ..
            } => {
                let extra = json!({
                    "kind": "approval",
                    "request_id": request_id,
                    "tool": tool,
                    "input": parse_input(input),
                    "message": format!("approval needed: {tool}"),
                });
                Some(fire(
                    HookEvent::Notification,
                    session,
                    Some(tool.as_str()),
                    extra,
                ))
            }
            OutEvent::UserQuestion {
                session,
                request_id,
                questions,
                ..
            } => {
                let texts: Vec<&str> = questions.0.iter().map(|q| q.question.as_str()).collect();
                let message = texts.first().copied().unwrap_or("question pending");
                let extra = json!({
                    "kind": "question",
                    "request_id": request_id,
                    "questions": texts,
                    "message": message,
                });
                Some(fire(HookEvent::Notification, session, None, extra))
            }
            _ => None,
        }
    }

    /// A finished turn: `stop` for a root, `subagent_stop` for a spawned child.
    fn stop(&mut self, session: &SessionId, state: AgentState) -> Firing {
        let error = self.last_error.remove(session);
        let reason = match state {
            AgentState::Error => "error",
            _ => "completed",
        };
        let meta = self.sessions.get(session);
        let parent = meta.and_then(|m| m.parent.as_ref());
        let agent = meta.map(|m| m.agent.as_str());
        let event = match parent {
            Some(_) => HookEvent::SubagentStop,
            None => HookEvent::Stop,
        };
        let error = error.filter(|_| reason == "error");
        let extra = json!({
            "reason": reason,
            "error": error,
            "parent": parent.map(|p| &p.0),
            "agent": agent,
        });
        fire(event, session, None, extra)
    }

    /// A session left memory: forget everything kept for it.
    fn end(&mut self, session: &SessionId, reason: &str) -> Firing {
        let meta = self.sessions.remove(session);
        self.compacted.remove(session);
        self.ended_turn.remove(session);
        self.last_error.remove(session);
        let extra = json!({
            "reason": reason,
            "parent": meta.as_ref().and_then(|m| m.parent.as_ref()).map(|p| &p.0),
            "agent": meta.as_ref().map(|m| &m.agent),
        });
        fire(HookEvent::SessionEnd, session, None, extra)
    }
}

/// Build a firing whose payload is `{event, session}` plus `extra`'s fields.
fn fire(event: HookEvent, session: &SessionId, tool: Option<&str>, extra: Value) -> Firing {
    let mut payload = json!({ "event": event.as_str(), "session": session.0 });
    if let (Value::Object(p), Value::Object(e)) = (&mut payload, extra) {
        p.extend(e);
    }
    Firing {
        event,
        session: session.clone(),
        tool: tool.map(str::to_string),
        payload,
    }
}

/// Spawn the watcher into the executor's `JoinSet` (aborted with it, #545).
/// Both receivers are subscribed by the caller *before* the engine can emit,
/// so a first `SessionStarted` is never missed.
pub(crate) fn spawn_watch(
    background: &mut JoinSet<()>,
    hooks: Arc<Hooks>,
    mut events: broadcast::Receiver<OutEvent>,
    mut inbound: broadcast::Receiver<InMsg>,
) {
    let (tx, mut rx) = mpsc::unbounded_channel::<Firing>();
    let runner = hooks.clone();
    background.spawn(async move {
        while let Some(f) = rx.recv().await {
            let tool = f.tool.as_deref();
            runner
                .run_observer(f.event, &f.session, tool, &f.payload)
                .await;
        }
    });
    background.spawn(async move {
        let mut tracker = Tracker::default();
        loop {
            let ev = match events.recv().await {
                Ok(ev) => ev,
                Err(RecvError::Lagged(n)) => {
                    tracing::warn!(skipped = n, "lifecycle hook watcher lagged");
                    continue;
                }
                Err(RecvError::Closed) => break,
            };
            // The supervisor fans a frame out before handling it, so every
            // `Resume` that caused this event is already buffered here.
            loop {
                match inbound.try_recv() {
                    Ok(msg) => tracker.observe_in(&msg),
                    Err(TryRecvError::Lagged(_)) => continue,
                    Err(_) => break,
                }
            }
            if let Some(f) = tracker.observe(&ev) {
                if !hooks.specs(f.event).is_empty() && tx.send(f).is_err() {
                    break;
                }
            }
        }
    });
}

#[cfg(test)]
mod tests;
