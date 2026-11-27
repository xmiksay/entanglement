//! The observe-only hook points: `post_tool_use`, `user_prompt_submit` (#964)
//! and the lifecycle points of #734 — `session_start`, `session_end`, `stop`,
//! `pre_compact`, `subagent_stop` and `notification`.
//!
//! Core and the library stay policy-free (ADR-0066, #964): every point is
//! *derived* from the `OutEvent` stream (or the inbound fan-out) any
//! subscriber sees, so no new seam exists — and therefore none of them can
//! block. A subscriber sees an event
//! only after core acted on it, so there is nothing left to veto; the one
//! point that sounds like a gate, `pre_compact`, would need a core seam to
//! hold the summary back, and since ADR-0205 compaction forks and never
//! rewrites the source log, there is no history for a hook to rescue first.
//!
//! The [`Tracker`] is the pure part: it folds events into [`Firing`]s. The
//! [`spawn`]ed task feeds it and hands each firing to one sequential worker,
//! so hooks run in event order while a slow hook never lags the subscription.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use entanglement_core::{AgentState, CompactionMode, Holly, InMsg, OutEvent, SessionId};
use serde_json::{json, Value};
use tokio::sync::broadcast::{self, error::RecvError, error::TryRecvError};
use tokio::sync::mpsc;
use tokio::task::{JoinHandle, JoinSet};

use super::{parse_input, HookEvent, Hooks};
use resume::ResumeMarks;

mod resume;

/// Every point this module drives, for [`Hooks::has_observers`].
pub(super) const OBSERVED: [HookEvent; 8] = [
    HookEvent::PostToolUse,
    HookEvent::UserPromptSubmit,
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
    /// Exported as `ENTANGLEMENT_TOOL_NAME` (`post_tool_use`, an approval
    /// `notification`).
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
    /// What pending `InMsg::Resume`s are about to re-announce.
    resuming: ResumeMarks,
    /// Sessions that announced `Compacted`: they are retiring into a successor.
    compacted: HashSet<SessionId>,
    /// Sessions whose `Done` arrived; the `Status` right after it says how
    /// the turn ended (`emit_turn_done`/`emit_turn_error` always pair them).
    ended_turn: HashSet<SessionId>,
    last_error: HashMap<SessionId, String>,
    /// `(tool, input)` of each call the runtime was asked to run, by
    /// `(session, request_id)`, until its `ToolOutput` fires `post_tool_use`.
    execs: HashMap<(SessionId, String), (String, String)>,
}

impl Tracker {
    /// Note an inbound frame: a `Prompt` fires `user_prompt_submit`; a
    /// `Resume` is fanned out before the supervisor handles it, so it is
    /// always seen ahead of the `SessionStarted`s it causes (see [`spawn`]).
    pub(crate) fn observe_in(&mut self, msg: &InMsg) -> Option<Firing> {
        if let InMsg::Prompt { session, content } = msg {
            let prompt = entanglement_core::content_text(content);
            return Some(fire(
                HookEvent::UserPromptSubmit,
                session,
                None,
                json!({ "prompt": prompt }),
            ));
        }
        let sessions = &self.sessions;
        self.resuming.observe(msg, |id| sessions.contains_key(id));
        None
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
                let source = if self.resuming.take(session) {
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
            // A sub-agent moved under its parent's compaction successor
            // (ADR-0234): its later hook payloads name the new parent.
            OutEvent::ParentChanged { session, parent } => {
                if let Some(meta) = self.sessions.get_mut(session) {
                    meta.parent = Some(parent.clone());
                }
                None
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
                // An error before the resumed `SessionStarted` is the
                // supervisor refusing the resume (closed/already-live id,
                // failed replay): no start follows, so drop the mark.
                self.resuming.refuse(session);
                // Only for a session we track, so an id that never ends
                // can't grow the map.
                if self.sessions.contains_key(session) {
                    self.last_error.insert(session.clone(), message.clone());
                }
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
            OutEvent::ToolExec {
                session,
                request_id,
                tool,
                input,
                ..
            } => {
                let key = (session.clone(), request_id.clone());
                self.execs.insert(key, (tool.clone(), input.clone()));
                None
            }
            OutEvent::ToolOutput {
                session,
                request_id,
                output,
                is_error,
                exit_code,
                ..
            } => {
                let (tool, input) = self.execs.remove(&(session.clone(), request_id.clone()))?;
                let extra = json!({
                    "tool": tool,
                    "input": parse_input(&input),
                    "output": output,
                    "is_error": is_error,
                    "exit_code": exit_code,
                });
                Some(fire(HookEvent::PostToolUse, session, Some(&tool), extra))
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

    /// After a broadcast lag the `Done`/`Status` pairing can't be trusted,
    /// and a pending resume whose `SessionStarted` was among the dropped
    /// events would otherwise keep its mark forever.
    pub(crate) fn forget_after_lag(&mut self) {
        self.ended_turn.clear();
        self.resuming = ResumeMarks::default();
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
        self.resuming.forget(session);
        self.compacted.remove(session);
        self.ended_turn.remove(session);
        self.last_error.remove(session);
        self.execs.retain(|(s, _), _| s != session);
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

/// Spawn the watcher when any observer point has a command (`None`
/// otherwise — the norm). Both receivers are subscribed here, before this
/// returns, so a first `Prompt` or `SessionStarted` is never missed. Abort the
/// handle to stop it (its two tasks live in a `JoinSet` dropped with it).
pub fn spawn(holly: &Holly, hooks: Arc<Hooks>) -> Option<JoinHandle<()>> {
    if !hooks.has_observers() {
        return None;
    }
    let events = holly.subscribe();
    let inbound = holly.subscribe_inbound();
    Some(tokio::spawn(async move {
        let mut tasks = JoinSet::new();
        watch(&mut tasks, hooks, events, inbound);
        while tasks.join_next().await.is_some() {}
    }))
}

/// The tracker feeding one sequential worker, so hooks run in event order
/// while a slow hook never lags the subscription.
fn watch(
    tasks: &mut JoinSet<()>,
    hooks: Arc<Hooks>,
    mut events: broadcast::Receiver<OutEvent>,
    mut inbound: broadcast::Receiver<InMsg>,
) {
    let (tx, mut rx) = mpsc::unbounded_channel::<Firing>();
    let runner = hooks.clone();
    tasks.spawn(async move {
        while let Some(f) = rx.recv().await {
            let tool = f.tool.as_deref();
            runner
                .run_observer(f.event, &f.session, tool, &f.payload)
                .await;
        }
    });
    tasks.spawn(async move {
        let mut tracker = Tracker::default();
        let send = |f: Firing| hooks.specs(f.event).is_empty() || tx.send(f).is_ok();
        loop {
            let ev = match events.recv().await {
                Ok(ev) => ev,
                // Firings are best-effort under lag: dropped events fire
                // nothing. A pending turn end is forgotten rather than
                // classified off a `Status` whose `Done` may have been lost.
                Err(RecvError::Lagged(n)) => {
                    tracing::warn!(skipped = n, "hook watcher lagged");
                    tracker.forget_after_lag();
                    continue;
                }
                Err(RecvError::Closed) => break,
            };
            // The supervisor fans a frame out before handling it, so every
            // `Prompt`/`Resume` that caused this event is already buffered
            // here (a prompt's hook fires on the first event it causes).
            loop {
                match inbound.try_recv() {
                    Ok(msg) => {
                        if let Some(f) = tracker.observe_in(&msg) {
                            if !send(f) {
                                return;
                            }
                        }
                    }
                    Err(TryRecvError::Lagged(_)) => continue,
                    Err(_) => break,
                }
            }
            if let Some(f) = tracker.observe(&ev) {
                if !send(f) {
                    break;
                }
            }
        }
    });
}

#[cfg(test)]
mod tests;
#[cfg(test)]
mod tool_tests;
