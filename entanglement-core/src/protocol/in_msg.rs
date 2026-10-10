//! Inbound frames ([`InMsg`]). The enum is too large for one file, so its
//! variants are split by family: this file holds the turn/interaction
//! variants inside the `in_msg_enum!` definition, and `control.rs` invokes it
//! with the session-control/query variants — the expanded type is one plain
//! `enum`. Constructors live here; routing/trust classification in
//! `routing.rs`.

use entanglement_provider::ContentPart;
use serde::Deserialize;

use crate::protocol::*;

pub use control::InMsg;

/// Back-compat deserializer for a content field that migrated from a bare string
/// to `[ContentPart]`: [`InMsg::Prompt`]'s `content` (#197, legacy `text`) and
/// [`InMsg::ToolResult`]'s `content` (#221, legacy `output`) both use it. Accepts
/// either the array or the legacy string (aliased on the field); an empty legacy
/// string yields no parts.
pub(super) fn de_prompt_content<'de, D>(d: D) -> Result<Vec<ContentPart>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Repr {
        Text(String),
        Parts(Vec<ContentPart>),
    }
    Ok(match Repr::deserialize(d)? {
        Repr::Text(t) if t.is_empty() => Vec::new(),
        Repr::Text(t) => vec![ContentPart::text(t)],
        Repr::Parts(p) => p,
    })
}

/// Defines [`InMsg`] from its turn/interaction variants below plus the
/// remaining variants passed in by `control.rs` (variant order preserved).
macro_rules! in_msg_enum {
    ($($rest:tt)*) => {
        /// Inbound: harness → engine. One connection multiplexes sessions via
        /// [`SessionId`].
        ///
        /// Not `Eq`: [`Resume`][InMsg::Resume] carries [`OutEvent`] records, which are
        /// `PartialEq`-only because of the floating-point cost in
        /// [`OutEvent::Usage`] (#192).
        #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
        #[serde(tag = "kind", rename_all = "snake_case")]
        pub enum InMsg {
            /// Feed a new user prompt into the conversation. `content` carries the
            /// message body as multimodal [`ContentPart`]s (#197, ADR-0064) — text
            /// and/or image blocks. A serde back-compat shim accepts the legacy
            /// text-only `text: "…"` shape so logs persisted before the migration still
            /// deserialize; new writes emit `content`. Build the text-only case with
            /// [`InMsg::prompt`].
            Prompt {
                session: SessionId,
                #[serde(alias = "text", default, deserialize_with = "de_prompt_content")]
                content: Vec<ContentPart>,
            },
            /// Approve a pending tool request (`request_id` from [`OutEvent::ToolRequest`]).
            /// `scope` (#174) controls how long the approval lasts — [`ApprovalScope::Once`]
            /// by default, so a head that omits it keeps the historical one-shot behavior
            /// (and the default scope is omitted on the wire, additive for older heads).
            ///
            /// `mode` (#560, ADR-0207 §7 extension) is meaningful only when this approves
            /// a `propose_plan` request: it carries the mode the *approver* chose to
            /// switch the session into (`"build"` or `"auto"`) — acceptance is a mode
            /// switch (ADR-0207 §7), and picking which one is the human's decision, made
            /// at the approval prompt itself. `None` (the wire default, additive for
            /// older heads) means a bare accept with no mode named, which the
            /// `propose_plan` orchestrator resolves to `"auto"` — the documented default
            /// for "go implement this", not a second-guess of it. Every other approval
            /// ignores this field. Core carries it as opaque `Option<String>`, exactly
            /// like [`SetMode`][InMsg::SetMode]'s own `mode` — validating it against the
            /// closed `build`/`auto` set is the runtime's job (the runtime owns the mode
            /// table), not core's.
            Approve {
                session: SessionId,
                request_id: String,
                #[serde(default, skip_serializing_if = "ApprovalScope::is_once")]
                scope: ApprovalScope,
                #[serde(default, skip_serializing_if = "Option::is_none")]
                mode: Option<String>,
            },
            /// Reject a pending tool request.
            Reject {
                session: SessionId,
                request_id: String,
                #[serde(default, skip_serializing_if = "Option::is_none")]
                reason: Option<String>,
            },
            /// Result of a runtime-executed tool (`request_id` from
            /// [`OutEvent::ToolExec`]). The runtime owns tool execution (ADR-0006/0010):
            /// core emits a `ToolExec` request and parks the turn until this arrives.
            /// Distinct from [`Approve`][InMsg::Approve]/[`Reject`][InMsg::Reject], which
            /// stay for approval semantics. `content` carries the result as multimodal
            /// [`ContentPart`]s (#221) — text today, an image block when `read` opens an
            /// image file. A serde back-compat shim accepts the legacy text-only
            /// `output: "…"` shape so pre-migration logs still deserialize.
            ///
            /// `is_error`/`duration_ms` (#636, ADR-0176) are a **structured side channel**
            /// alongside `content` — the runtime already knows whether a call was denied,
            /// masked, refused, or actually executed, and how long execution took; these
            /// fields carry that as real data instead of only as a textual marker inside
            /// `content` (`` tool `X` denied by permission profile ``, `` tool `X` failed:
            /// … ``). `content`'s text is unchanged — a human/model reading it still needs
            /// the marker — this is additive for anything that wants to branch on the
            /// outcome without parsing text. Both default (`false`/`None`) so a pre-#636
            /// log or a hand-built `InMsg::tool_result` still deserializes/round-trips.
            ///
            /// `exit_code` (#681, ADR-0186) extends the same side channel with the
            /// numeric exit status of a process-running tool (`bash`/`call`, and a
            /// `poll` that observed a background job exit). `None` everywhere else —
            /// including a signal-killed process, where the `[exit -1]` text keeps its
            /// lossy sentinel but the field stays honest. Orthogonal to `is_error`:
            /// a command exiting nonzero still executed (`is_error: false`).
            ToolResult {
                session: SessionId,
                request_id: String,
                #[serde(alias = "output", default, deserialize_with = "de_prompt_content")]
                content: Vec<ContentPart>,
                #[serde(default, skip_serializing_if = "std::ops::Not::not")]
                is_error: bool,
                #[serde(default, skip_serializing_if = "Option::is_none")]
                duration_ms: Option<u64>,
                #[serde(default, skip_serializing_if = "Option::is_none")]
                exit_code: Option<i32>,
            },
            /// Answer a pending model-driven `ask_user` call (`request_id` from
            /// [`OutEvent::UserQuestion`]). Like [`Approve`][InMsg::Approve]/
            /// [`Reject`][InMsg::Reject], it is consumed by the runtime off the inbound
            /// fan-out (the `ask_user` executor parks on it), not routed to a session —
            /// core stays unaware of the interaction (ADR-0027). `answers` (#488) is
            /// one inner vec of chosen option labels (multi-select) or a single
            /// free-form string per question, in the call's `questions` order — the
            /// always-available "Other" answer is just a string among them, no longer
            /// gated by a dropped `allow_free_form`. Build one with
            /// [`InMsg::answer_question`]. Legacy `answer: String` (pre-#488, a single
            /// question's picked label/free text) still deserializes; a current head
            /// never writes it, leaving it at its empty-string default. The runtime
            /// folds the answers back as the `ask_user` tool's
            /// [`ToolResult`][InMsg::ToolResult].
            AnswerQuestion {
                session: SessionId,
                request_id: String,
                #[serde(default, skip_serializing_if = "Vec::is_empty")]
                answers: Vec<Vec<String>>,
                #[serde(default, skip_serializing_if = "String::is_empty")]
                answer: String,
            },
            /// Withdraw a pending `ask_user` question (`request_id` from
            /// [`OutEvent::UserQuestion`]) without answering it and without cancelling
            /// the rest of the turn (#515) — the targeted counterpart to a
            /// session-wide [`Stop`][InMsg::Stop], which would abandon every other
            /// parked call too. Resolved by the same runtime `ask_user` orchestrator
            /// [`AnswerQuestion`][InMsg::AnswerQuestion] parks on: it folds a
            /// withdrawal note back as the tool's `ToolResult` so the model's call
            /// still completes (unlike `Stop`'s silent unwind, which leaves it
            /// unresolved on purpose because the whole turn is being torn down
            /// anyway). Unknown/already-resolved `(session, request_id)` is a no-op.
            RetractQuestion {
                session: SessionId,
                request_id: String,
            },
            /// Swap a pending `ask_user` question's content in place (#515):
            /// re-parks the same `request_id` with revised `questions`, re-emitting
            /// [`OutEvent::UserQuestion`] rather than resolving the call. Lets a head
            /// correct or narrow a question it already surfaced without cancelling
            /// the turn or losing the model's original tool call. Unknown/
            /// already-resolved `(session, request_id)` is a no-op.
            ReplaceQuestion {
                session: SessionId,
                request_id: String,
                #[serde(flatten)]
                questions: Questions,
            },
            /// Cancel the current turn and park the session at idle.
            Stop { session: SessionId },
            /// Hold a live session at [`AgentState::Paused`] (#516, ADR-0208) without
            /// cancelling anything or evicting memory — the middle ground `Stop`
            /// (destroys the in-flight round) and `HibernateSession` (evicts memory)
            /// don't cover. An idle session defers its next `Prompt` (and
            /// `SetModel`/`SetGeneration`/`Oneshot`) until
            /// [`ResumeSession`][InMsg::ResumeSession]; a session parked on a tool-call
            /// batch keeps folding arriving `ToolResult`s into `Context` as normal, but
            /// the turn does not continue past a drained batch until resumed — so the
            /// same round picks up again with no re-prompt needed. A session actively
            /// streaming when this arrives is unaffected until the round reaches its
            /// next safe point (turn end or park) — mirroring how a mid-stream
            /// `SetModel` is deferred, *not* raced via `tokio::select!` like
            /// `Stop` — so `PauseSession` never interrupts an in-flight model
            /// round-trip; use `Stop` for that. `Stop`/`HibernateSession` always take
            /// priority: both apply regardless of `paused`, and neither clears it.
            /// Idempotent; wire-allowed like `Stop` (no elevated capability).
            PauseSession { session: SessionId },
            /// Lift a hold placed by [`PauseSession`][InMsg::PauseSession] (#516,
            /// ADR-0208). A deferred idle `Prompt`/`SetModel`/
            /// `SetGeneration`/`Oneshot` now applies; a parked turn whose batch already
            /// drained while paused continues immediately with no new model request
            /// needed to re-enter it. A no-op on a session that isn't paused.
            /// Wire-allowed like `PauseSession`.
            ResumeSession { session: SessionId },
            $($rest)*
        }
    };
}

mod control;
mod routing;
#[cfg(test)]
mod tests;

impl InMsg {
    /// Build a text-only [`Prompt`][InMsg::Prompt] — the common case. Empty text
    /// yields an empty `content` (no stray text part). Multimodal prompts build
    /// the `content` vec directly.
    pub fn prompt(session: SessionId, text: impl Into<String>) -> Self {
        let text = text.into();
        let content = if text.is_empty() {
            Vec::new()
        } else {
            vec![ContentPart::text(text)]
        };
        InMsg::Prompt { session, content }
    }

    /// Build a text [`ToolResult`][InMsg::ToolResult] — the common case. Empty
    /// text yields an empty `content` (matching [`Message::tool`]'s fold);
    /// multimodal results (an image `read`, #221) build the `content` vec
    /// directly.
    pub fn tool_result(
        session: SessionId,
        request_id: impl Into<String>,
        output: impl Into<String>,
    ) -> Self {
        let output = output.into();
        let content = if output.is_empty() {
            Vec::new()
        } else {
            vec![ContentPart::text(output)]
        };
        InMsg::ToolResult {
            session,
            request_id: request_id.into(),
            content,
            is_error: false,
            duration_ms: None,
            exit_code: None,
        }
    }

    /// Build an [`AnswerQuestion`][InMsg::AnswerQuestion] in the current v2
    /// shape (#488) — one inner vec of chosen labels / free text per question,
    /// in the answered call's `questions` order. Leaves the legacy `answer`
    /// field at its empty default.
    pub fn answer_question(
        session: SessionId,
        request_id: impl Into<String>,
        answers: Vec<Vec<String>>,
    ) -> Self {
        InMsg::AnswerQuestion {
            session,
            request_id: request_id.into(),
            answers,
            answer: String::new(),
        }
    }
}
