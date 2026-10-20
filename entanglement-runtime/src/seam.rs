//! Shared seam plumbing for the runtime-owned tool orchestrators (#205).
//!
//! Every runtime-owned tool — the generic `tool_runner` dispatch plus
//! `ask_user`, `propose_plan`, `rhai`, `agent`, `poll` —
//! speaks the same #58 round-trip: it emits a request event, parks for the
//! head's decision, and folds the outcome back as an [`InMsg::ToolResult`]. Two
//! steps of that round-trip were copied across every module:
//!
//! - **[`reply`]** — the `ToolResult` fold-back (was duplicated 6×);
//! - the **park for a decision** — previously each orchestrator held its own
//!   `broadcast` subscription of the inbound fan-out and filtered it to
//!   `(session, request_id)`. That per-task subscriber could *lag* under burst
//!   and silently drop the very `Approve`/`Reject`/`AnswerQuestion` it waited for,
//!   parking the request forever (#156). Decision delivery now runs off the
//!   lag-proof [`crate::pending::PendingDecisions`] registry: a single light
//!   router task ([`crate::tool_runner`]) consumes the fan-out and fans each
//!   decision — mapped here by [`Decision::from_inmsg`] — to its parked waiter's
//!   oneshot. This module keeps the [`Decision`] type and the fold-back.

use entanglement_core::{ApprovalScope, ContentPart, Holly, InMsg, Question, SessionId};
// `ToolResult` folds back over the privileged in-process handle (#155), not the
// untrusted wire path — a forged `ToolResult` off a wire head must never resolve
// a parked turn.

/// Fold a **text** tool result back to core as the #58 `ToolResult`, completing
/// the parked `ToolExec` round-trip. The text-producing tools (denials, refusals,
/// `rhai`, `ask_user`, …) all end here; an empty string folds to no content
/// parts, matching a text-only tool message. `is_error` (#636, ADR-0176) is the
/// caller's own classification of whether the call ran to completion — denials,
/// refusals, and unknown/masked-tool messages pass `true`; acks and answers pass
/// `false` — carried as a real field on [`InMsg::ToolResult`]/
/// [`OutEvent::ToolOutput`] alongside the still-unchanged text.
pub async fn reply(
    holly: &Holly,
    session: SessionId,
    request_id: String,
    output: String,
    is_error: bool,
) {
    let content = if output.is_empty() {
        Vec::new()
    } else {
        vec![ContentPart::text(output)]
    };
    reply_content(holly, session, request_id, content, is_error, None, None).await;
}

/// Fold a **multimodal** tool result back to core (#221) — `read` uses this to
/// return an image content block. Other callers use the text [`reply`].
/// `duration_ms` (#636, ADR-0176) is `Some` only from the generic host-tool
/// dispatch (`run_and_reply`), which is the one caller that actually measures
/// execution wall-clock; every other route (denials, refusals, orchestrated
/// tools) passes `None`. `exit_code` (#681, ADR-0186) is `Some` only when the
/// call ran a process to a real exit status — the host-tool dispatch for a
/// foreground `bash`/`call`, and `poll` when it observes a background job
/// exit; everything else passes `None`.
pub async fn reply_content(
    holly: &Holly,
    session: SessionId,
    request_id: String,
    content: Vec<ContentPart>,
    is_error: bool,
    duration_ms: Option<u64>,
    exit_code: Option<i32>,
) {
    let _ = holly
        .submit_tool_result(
            session,
            request_id,
            content,
            is_error,
            duration_ms,
            exit_code,
        )
        .await;
}

/// The head's answer to a parked tool round-trip, already filtered to the
/// waiting `(session, request_id)`.
#[derive(Debug)]
pub enum Decision {
    /// `Approve` — carries the approval scope (#174) and, for a `propose_plan`
    /// approval only (#560, ADR-0207 §7 extension), the mode the approver chose
    /// to switch into (`mode`, mirroring [`InMsg::Approve::mode`]). Every other
    /// orchestrator ignores `mode`.
    Approve {
        scope: ApprovalScope,
        mode: Option<String>,
        /// A `rhai` launch card's **Run all** (ADR-0228,
        /// [`InMsg::Approve::run_all`]); every other orchestrator ignores it.
        run_all: bool,
    },
    /// `Reject` — carries the optional reason.
    Reject { reason: Option<String> },
    /// `AnswerQuestion` — one inner vec of chosen labels / free text per
    /// question, in the answered call's `questions` order (`ask_user`, #90,
    /// #488).
    Answer { answers: Vec<Vec<String>> },
    /// `RetractQuestion` (#515): withdraw an open `ask_user` question. Unlike
    /// `Stop`, the waiting orchestrator must still reply — its `ToolResult` is
    /// owed regardless, since only this one call is retracted, not the whole
    /// turn.
    Retract,
    /// `ReplaceQuestion` (#515): swap an open `ask_user` question's content in
    /// place — the orchestrator re-emits `UserQuestion` with `questions` under
    /// the same `request_id` and re-parks, rather than resolving the call.
    Replace { questions: Vec<Question> },
    /// `Stop` for this session, or the inbound stream closed: unwind silently —
    /// core cancels the turn on the same `Stop`, so no `ToolResult` is owed
    /// (ADR-0017).
    Stop,
}

/// What an approval park (a permission `Ask`, `propose_plan`,
/// `request_mode`, a `rhai` card) receives: [`Decision`] narrowed by the
/// registry, which delivers it an `Approve`/`Reject` or a `Stop` and
/// refuses a question decision aimed at its id (ADR-0245).
#[derive(Debug)]
pub enum ApprovalDecision {
    Approve {
        scope: ApprovalScope,
        mode: Option<String>,
        run_all: bool,
    },
    Reject {
        reason: Option<String>,
    },
    Stop,
}

/// What an `ask_user` park receives: [`Decision`] narrowed by the registry,
/// which refuses an `Approve`/`Reject` aimed at its id (ADR-0245).
#[derive(Debug)]
pub enum QuestionDecision {
    Answer {
        answers: Vec<Vec<String>>,
    },
    Retract,
    Replace {
        questions: Vec<Question>,
    },
    /// The park itself was refused: a live card already holds the id on
    /// this session (`PendingDecisions::register`). The call still owes the
    /// model a `ToolResult`, so it replies with `reason` as an error.
    Refused {
        reason: String,
    },
    Stop,
}

impl Decision {
    /// `Answer`/`Retract`/`Replace`: only an `ask_user` park takes it.
    pub fn is_question(&self) -> bool {
        matches!(
            self,
            Decision::Answer { .. } | Decision::Retract | Decision::Replace { .. }
        )
    }

    /// The `InMsg` variant a decision came from, for a refusal's message.
    pub fn frame(&self) -> &'static str {
        match self {
            Decision::Approve { .. } => "Approve",
            Decision::Reject { .. } => "Reject",
            Decision::Answer { .. } => "AnswerQuestion",
            Decision::Retract => "RetractQuestion",
            Decision::Replace { .. } => "ReplaceQuestion",
            Decision::Stop => "Stop",
        }
    }

    /// The approval park's view. Nothing sends an approval waiter a
    /// question decision: `PendingDecisions::resolve` refuses one
    /// (ADR-0245), and the registry's own sends are `Stop` (unwind,
    /// supersede) or `Reject` (a refused park). Only a hand-fed channel could
    /// carry one; it unwinds like `Stop` rather than panicking.
    pub fn into_approval(self) -> ApprovalDecision {
        match self {
            Decision::Approve {
                scope,
                mode,
                run_all,
            } => ApprovalDecision::Approve {
                scope,
                mode,
                run_all,
            },
            Decision::Reject { reason } => ApprovalDecision::Reject { reason },
            Decision::Answer { .. }
            | Decision::Retract
            | Decision::Replace { .. }
            | Decision::Stop => ApprovalDecision::Stop,
        }
    }

    /// The `ask_user` park's view. A `Reject` reaching a question waiter is
    /// the registry refusing the park itself (a live card holds the id), so
    /// it is [`QuestionDecision::Refused`]. An `Approve` never reaches one
    /// (`resolve` refuses it, ADR-0245); a hand-fed one unwinds like `Stop`.
    pub fn into_question(self) -> QuestionDecision {
        match self {
            Decision::Answer { answers } => QuestionDecision::Answer { answers },
            Decision::Retract => QuestionDecision::Retract,
            Decision::Replace { questions } => QuestionDecision::Replace { questions },
            Decision::Reject { reason } => QuestionDecision::Refused {
                reason: reason.unwrap_or_else(|| "the question could not be parked".into()),
            },
            Decision::Approve { .. } | Decision::Stop => QuestionDecision::Stop,
        }
    }

    /// The `(session, request_id)` a decision frame answers — the key
    /// [`from_inmsg`](Self::from_inmsg) would resolve, without consuming it.
    pub fn target(msg: &InMsg) -> Option<(&SessionId, &str)> {
        match msg {
            InMsg::Approve {
                session,
                request_id,
                ..
            }
            | InMsg::Reject {
                session,
                request_id,
                ..
            }
            | InMsg::AnswerQuestion {
                session,
                request_id,
                ..
            }
            | InMsg::RetractQuestion {
                session,
                request_id,
            }
            | InMsg::ReplaceQuestion {
                session,
                request_id,
                ..
            } => Some((session, request_id)),
            _ => None,
        }
    }

    /// Map an inbound frame to its routing tuple `(session, request_id,
    /// Decision)`, or `None` for a frame that is not a head decision (#156). The
    /// executor's single inbound router uses this to fan each `Approve`/`Reject`/
    /// `AnswerQuestion`/`RetractQuestion`/`ReplaceQuestion` (#515) to its parked
    /// waiter in [`crate::pending::PendingDecisions`]. `Stop` is *not* mapped
    /// here — it is session-scoped, not request-scoped, so the router unwinds
    /// every waiter of the session via
    /// [`crate::pending::PendingDecisions::stop_session`].
    ///
    /// `AnswerQuestion`'s legacy `answer: String` shape (pre-#488) folds to
    /// `[[answer]]` here when `answers` arrived empty — the one place that
    /// back-compat mapping happens.
    pub fn from_inmsg(msg: InMsg) -> Option<(SessionId, String, Decision)> {
        match msg {
            InMsg::Approve {
                session,
                request_id,
                scope,
                mode,
                run_all,
            } => Some((
                session,
                request_id,
                Decision::Approve {
                    scope,
                    mode,
                    run_all,
                },
            )),
            InMsg::Reject {
                session,
                request_id,
                reason,
            } => Some((session, request_id, Decision::Reject { reason })),
            InMsg::AnswerQuestion {
                session,
                request_id,
                answers,
                answer,
            } => {
                let answers = if answers.is_empty() {
                    vec![vec![answer]]
                } else {
                    answers
                };
                Some((session, request_id, Decision::Answer { answers }))
            }
            InMsg::RetractQuestion {
                session,
                request_id,
            } => Some((session, request_id, Decision::Retract)),
            InMsg::ReplaceQuestion {
                session,
                request_id,
                questions,
            } => Some((
                session,
                request_id,
                Decision::Replace {
                    questions: questions.0,
                },
            )),
            _ => None,
        }
    }
}
