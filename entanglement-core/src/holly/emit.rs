//! Runtime-authored event emission on the [`Holly`] handle: the sanctioned
//! way for a runtime service to broadcast an [`OutEvent`] without holding the
//! raw outbound sender.

use std::sync::{Arc, Weak};

use super::outlets::Outlets;
use super::Holly;
#[cfg(doc)]
use crate::protocol::InMsg;
use crate::protocol::{AgentState, OutEvent, SessionId};

/// The persistence tap's emitter for its outage transitions (#767):
/// [`OutEvent::PersistenceDegraded`] / [`OutEvent::PersistenceRecovered`].
///
/// WHY not a [`Holly`] clone: the tap drains until the engine's channels
/// close, and a `Holly` clone keeps the inbox open — an embedder that shuts
/// down by dropping its handles would then never see the tap finish. This
/// holds the outbox weakly, so it keeps nothing alive; once the engine is
/// gone (or shut down) every emit is a silent no-op.
#[derive(Clone, Default)]
pub struct PersistenceSignal(Weak<Outlets>);

impl PersistenceSignal {
    /// A signal wired to no engine: every emit is a no-op (tests, or a tap
    /// driven without a `Holly`).
    pub fn detached() -> Self {
        Self::default()
    }

    /// `root`'s log stopped being written: the first failure of an outage.
    pub fn degraded(&self, root: &SessionId, error: String) {
        self.emit(OutEvent::PersistenceDegraded {
            root: root.clone(),
            error,
        });
    }

    /// `root`'s outage ended with a gap tombstone of `lost` records.
    pub fn recovered(&self, root: &SessionId, lost: u64) {
        self.emit(OutEvent::PersistenceRecovered {
            root: root.clone(),
            lost,
        });
    }

    fn emit(&self, ev: OutEvent) {
        if let Some(outlets) = self.0.upgrade() {
            outlets.emit(ev);
        }
    }
}

impl Holly {
    /// A non-owning [`PersistenceSignal`] onto this engine's outbox (#767).
    pub fn persistence_signal(&self) -> PersistenceSignal {
        PersistenceSignal(Arc::downgrade(&self.outlets))
    }

    /// Mint a fresh monotonic per-session `seq` and broadcast a runtime-authored
    /// content event for `session` (#157). The sanctioned way for a runtime
    /// service to emit a seq-bearing [`OutEvent`] while a session is parked (an
    /// approval `ToolRequest`/`UserQuestion`, a `Plan`/`TaskList` snapshot, a
    /// `FileChange`): the seq is drawn from the session's shared counter — the
    /// *same* sequence the session task uses — so `(session, seq)` stays unique
    /// instead of reusing the parked `ToolExec` seq. `make` receives the minted
    /// seq and builds the event. A session with no live counter (already ended,
    /// or never started) mints seq `0` — harmless, as there is no live content
    /// stream to collide with.
    pub fn emit_for_session(&self, session: &SessionId, make: impl FnOnce(u64) -> OutEvent) {
        let seq = self.next_seq(session);
        self.outlets.emit(make(seq));
    }

    /// Broadcast a point-in-time lifecycle [`OutEvent::Status`] for `session`.
    /// Status carries no `seq` (it's not content, so it's exempt from the seq
    /// contract), so no counter is touched — this is the seq-less sibling of
    /// [`emit_for_session`][Self::emit_for_session] for the runtime's `Thinking`/
    /// `WaitingApproval` transitions around a parked tool call.
    pub fn emit_status(&self, session: &SessionId, state: AgentState) {
        self.outlets.emit(OutEvent::Status {
            session: session.clone(),
            state,
        });
    }

    /// Broadcast a runtime-authored [`OutEvent::History`] reply to an
    /// [`InMsg::ReplayFrom`] query (#160, ADR-0072). Like
    /// [`emit_status`][Self::emit_status] it carries no `seq` — a supervisor-global
    /// query reply, not session content — so no counter is touched. The sanctioned
    /// way for the runtime's log-owning history responder to answer a late
    /// subscriber without the raw outbound sender being exposed.
    pub fn emit_history(&self, correlation_id: String, session: SessionId, events: Vec<OutEvent>) {
        self.outlets.emit(OutEvent::History {
            correlation_id,
            session,
            events,
        });
    }

    /// Broadcast a runtime-authored [`OutEvent::QuestionList`] reply to an
    /// [`InMsg::ListQuestions`] query (#515). Engine-global like
    /// [`emit_mcp_list`][Self::emit_mcp_list]: no `seq`, no counter touched.
    pub fn emit_question_list(
        &self,
        correlation_id: String,
        questions: Vec<crate::protocol::PendingQuestion>,
    ) {
        self.outlets.emit(OutEvent::QuestionList {
            correlation_id,
            questions,
        });
    }

    /// Broadcast a runtime-authored [`OutEvent::OperationList`] reply to an
    /// [`InMsg::ListOperations`] query (#607, ADR-0161 §6). Engine-global like
    /// [`emit_question_list`][Self::emit_question_list]: no `seq`, no counter
    /// touched.
    pub fn emit_operation_list(
        &self,
        correlation_id: String,
        operations: Vec<crate::protocol::OperationInfo>,
    ) {
        self.outlets.emit(OutEvent::OperationList {
            correlation_id,
            operations,
        });
    }

    /// Broadcast a runtime-authored [`OutEvent::PendingList`] reply to an
    /// [`InMsg::ListPending`] query (#726). Engine-global like
    /// [`emit_question_list`][Self::emit_question_list]: no `seq`, no counter
    /// touched.
    pub fn emit_pending_list(
        &self,
        correlation_id: String,
        approvals: Vec<crate::protocol::PendingApproval>,
    ) {
        self.outlets.emit(OutEvent::PendingList {
            correlation_id,
            approvals,
        });
    }

    /// Broadcast a runtime-authored [`OutEvent::McpList`] reply to an
    /// [`InMsg::McpList`] query (#375). Engine-global like
    /// [`emit_history`][Self::emit_history]: no `seq`, no counter touched.
    pub fn emit_mcp_list(
        &self,
        correlation_id: String,
        servers: Vec<crate::protocol::McpServerStatus>,
    ) {
        self.outlets.emit(OutEvent::McpList {
            correlation_id,
            servers,
        });
    }

    /// Broadcast a runtime-authored [`OutEvent::McpChanged`] reply to
    /// [`InMsg::McpAdd`]/[`InMsg::McpRemove`] (#375). No `seq` — a point-in-time
    /// engine-global lifecycle event, not session content.
    pub fn emit_mcp_changed(&self, name: String, action: crate::protocol::McpAction) {
        self.outlets.emit(OutEvent::McpChanged { name, action });
    }

    /// Broadcast a runtime-authored [`OutEvent::McpAuthChanged`] reply to
    /// [`InMsg::McpAuth`] (ADR-0153). No `seq` — a point-in-time engine-global
    /// lifecycle event, not session content. A `Connect` emits twice: an interim
    /// event carrying the authorize URL, then the terminal outcome.
    pub fn emit_mcp_auth_changed(&self, status: crate::protocol::McpAuthStatus) {
        self.outlets.emit(OutEvent::McpAuthChanged { status });
    }

    /// Broadcast a runtime-authored [`OutEvent::Throttle`] transition (#517,
    /// ADR-0141; `waiters`/`shared_leases` added #552). No `seq` — a
    /// point-in-time engine-global lifecycle event, not session content,
    /// exactly like [`emit_mcp_changed`][Self::emit_mcp_changed].
    #[allow(clippy::too_many_arguments)]
    pub fn emit_throttle(
        &self,
        endpoint: String,
        throttled: bool,
        in_flight: usize,
        cap: usize,
        retry_in_ms: Option<u64>,
        pacing_in_ms: Option<u64>,
        waiters: usize,
        shared_leases: Option<usize>,
    ) {
        self.outlets.emit(OutEvent::Throttle {
            endpoint,
            throttled,
            in_flight,
            cap,
            retry_in_ms,
            pacing_in_ms,
            waiters,
            shared_leases,
        });
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use tokio::sync::broadcast::error::RecvError;

    use super::*;
    use crate::EngineConfig;

    #[tokio::test]
    async fn persistence_signal_emits_but_keeps_no_engine_alive() {
        let holly = Holly::spawn(EngineConfig::default());
        let signal = holly.persistence_signal();
        let mut sub = holly.subscribe();
        signal.degraded(&SessionId::new("r"), "down".into());
        assert!(matches!(
            sub.recv().await,
            Ok(OutEvent::PersistenceDegraded { .. })
        ));
        // Dropping the only handle ends the engine although the signal lives
        // on — the persistence tap holding one still sees its channels close.
        drop(holly);
        let closed = tokio::time::timeout(Duration::from_secs(5), sub.recv()).await;
        assert!(matches!(closed, Ok(Err(RecvError::Closed))), "{closed:?}");
        signal.recovered(&SessionId::new("r"), 1); // a no-op, not a panic
        PersistenceSignal::detached().recovered(&SessionId::new("r"), 1);
    }
}
