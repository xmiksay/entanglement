//! Runtime-authored event emission on the [`Holly`] handle: the sanctioned
//! way for a runtime service to broadcast an [`OutEvent`] without holding the
//! raw outbound sender.

use super::Holly;
#[cfg(doc)]
use crate::protocol::InMsg;
use crate::protocol::{AgentState, OutEvent, SessionId};

impl Holly {
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
