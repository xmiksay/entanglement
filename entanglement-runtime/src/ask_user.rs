//! `ask_user` — model-driven user-decision prompt (#90, ADR-0027; #488 v2:
//! several questions per call, an always-available free-text answer, and
//! per-question multi-select — supersedes parts of ADR-0027; #515: list/
//! retract/replace an open question).
//!
//! Like `agent` (ADR-0022), `ask_user` is a runtime-owned tool intercepted
//! on [`OutEvent::ToolExec`] *before* permission resolution: it touches no host
//! resource, so it bypasses the permission profile. When the model calls it,
//! [`run_ask_user`] surfaces the questions to the head via a dedicated
//! [`OutEvent::UserQuestion`] (a plain `ToolRequest` can't carry labelled
//! choices), parks for the head's decision, and folds the outcome back as the
//! tool's [`InMsg::ToolResult`] — reusing the #58 round-trip so the parent turn
//! sees an ordinary tool result and core needs no new semantics. Three
//! decisions resolve the park:
//!
//! - [`InMsg::AnswerQuestion`] — the answer fed to the model is, per question,
//!   the picked option label(s) (joined for a multi-select) or the free-form
//!   text verbatim. Terminal: the call is done.
//! - [`InMsg::RetractQuestion`] (#515) — withdraws the question *without*
//!   cancelling the turn: unlike a session-wide `Stop`, the call must still
//!   reply, so the model sees a withdrawal note instead of hanging forever.
//!   Terminal.
//! - [`InMsg::ReplaceQuestion`] (#515) — swaps the parked question's content
//!   in place: re-emits `UserQuestion` under the same `request_id` and loops
//!   back to parking, rather than replying. Not terminal.
//!
//! A `Stop` for the session while parked unwinds silently: core's turn cancels
//! on the same `Stop`, so no `ToolResult` is owed (mirrors approval). Every
//! call registers into [`OpenQuestions`] before its first emit and is removed
//! on every terminal outcome, so [`InMsg::ListQuestions`] always sees exactly
//! the calls still genuinely parked.

use entanglement_core::{
    AgentState, Holly, OutEvent, Question, QuestionOption, Questions, SessionId, ToolSpec,
};

use crate::pending::{self, PendingDecisions};
use crate::questions::OpenQuestions;
use crate::seam;
use crate::tool_names::ASK_USER_TOOL;

/// The `ask_user` tool schema advertised to the model. Appended to the engine's
/// `tool_specs` alongside the host quintet and `agent`.
pub fn ask_user_spec() -> ToolSpec {
    ToolSpec::with_schema(
        ASK_USER_TOOL,
        "Ask the user one or more decision questions when the choice is genuinely \
         theirs (ambiguous requirements, a trade-off, a preference). Batch \
         related questions into one call. Offer 2-4 labelled options per \
         question; set multi_select to allow picking several. A typed free-text \
         answer is always offered alongside the options. Prefer deciding \
         yourself when you can.",
        serde_json::json!({
            "type": "object",
            "properties": {
                "questions": {
                    "type": "array",
                    "description": "Questions for the user, asked in order.",
                    "minItems": 1,
                    "items": {
                        "type": "object",
                        "properties": {
                            "question": {
                                "type": "string",
                                "description": "The question text."
                            },
                            "options": {
                                "type": "array",
                                "description": "Labelled choices.",
                                "items": {
                                    "type": "object",
                                    "properties": {
                                        "label": {
                                            "type": "string",
                                            "description": "Choice label — returned verbatim when picked."
                                        },
                                        "description": {
                                            "type": "string",
                                            "description": "Optional one-line hint shown beneath the label."
                                        }
                                    },
                                    "required": ["label"]
                                }
                            },
                            "multi_select": {
                                "type": "boolean",
                                "description": "Allow picking more than one option (default false)."
                            }
                        },
                        "required": ["question", "options"]
                    }
                }
            },
            "required": ["questions"]
        }),
    )
}

/// Parse the `ask_user` tool input. Accepts the current `{"questions": [...]}`
/// array shape, the legacy single-question `{"question", "options",
/// "allow_free_form"}` shape (folded into a one-element vec, `allow_free_form`
/// dropped — free text is unconditional now), or a malformed/bare-string input,
/// which degrades to a single free-form question with no options so the turn
/// still gets an answer path instead of a schema error.
fn parse_input(input: &str) -> Vec<Question> {
    match serde_json::from_str::<serde_json::Value>(input) {
        Ok(v) => {
            if let Some(arr) = v.get("questions").and_then(|q| q.as_array()) {
                let questions = arr.iter().filter_map(parse_question).collect::<Vec<_>>();
                if !questions.is_empty() {
                    return questions;
                }
            }
            // Legacy single-question shape, or a `questions` array that parsed
            // to nothing usable — fall back to reading the flat fields.
            vec![parse_question(&v).unwrap_or_else(|| Question {
                question: input.to_string(),
                options: Vec::new(),
                multi_select: false,
            })]
        }
        Err(_) => vec![Question {
            question: input.to_string(),
            options: Vec::new(),
            multi_select: false,
        }],
    }
}

/// Parse one question object (new array element or the legacy flat shape).
fn parse_question(v: &serde_json::Value) -> Option<Question> {
    let question = v.get("question").and_then(|q| q.as_str())?.to_string();
    let options = v
        .get("options")
        .and_then(|o| o.as_array())
        .map(|arr| arr.iter().filter_map(parse_option).collect::<Vec<_>>())
        .unwrap_or_default();
    let multi_select = v
        .get("multi_select")
        .and_then(|b| b.as_bool())
        .unwrap_or(false);
    Some(Question {
        question,
        options,
        multi_select,
    })
}

fn parse_option(v: &serde_json::Value) -> Option<QuestionOption> {
    let label = v.get("label").and_then(|l| l.as_str())?.to_string();
    if label.is_empty() {
        return None;
    }
    let description = v
        .get("description")
        .and_then(|d| d.as_str())
        .filter(|d| !d.is_empty())
        .map(str::to_string);
    Some(QuestionOption { label, description })
}

/// Render one question's chosen answers as readable text for the model: the
/// joined labels (or free text) for a multi-select, the single answer
/// otherwise. Empty (no answer supplied for this question) renders as-is.
fn render_answer(labels: &[String]) -> String {
    labels.join(", ")
}

/// Fold a call's `questions` + the head's per-question `answers` into the
/// tool's text output: `question -> answer` per line, so the model sees which
/// answer belongs to which question when several were asked.
fn fold_answers(questions: &[Question], answers: &[Vec<String>]) -> String {
    if questions.len() == 1 {
        return answers
            .first()
            .map(|a| render_answer(a))
            .unwrap_or_default();
    }
    questions
        .iter()
        .enumerate()
        .map(|(i, q)| {
            let answer = answers.get(i).map(|a| render_answer(a)).unwrap_or_default();
            format!("{}: {}", q.question, answer)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Orchestrate one `ask_user` call: surface the questions, await the head's
/// decision, and reply to the model once it reaches a terminal outcome
/// (answered or retracted) — looping back to park again on a replace (#515).
///
/// Registers the waiter with the lag-proof [`PendingDecisions`] registry (#156)
/// *before* emitting the questions, so a fast answer routes to this park rather
/// than racing a per-task broadcast subscription that could lag and drop it.
/// Mirrors that discipline into [`OpenQuestions`] so a concurrent
/// `InMsg::ListQuestions` never observes a call between "about to emit" and
/// "registered".
pub async fn run_ask_user(
    holly: Holly,
    pending: PendingDecisions,
    open: OpenQuestions,
    session: SessionId,
    request_id: String,
    input: String,
) {
    let mut questions = parse_input(&input);
    loop {
        open.insert(&session, &request_id, questions.clone());

        // Register before emitting so the inbound router can never resolve the
        // decision ahead of this waiter (#156).
        let rx = pending.register(&session, &request_id);

        // Mint a fresh per-session seq (#157) so the questions take an ordered
        // place in the content stream rather than reusing the parked `ToolExec`
        // seq. Moved into the closure since `emit_for_session` builds the event
        // with the seq.
        let questions_for_emit = questions.clone();
        holly.emit_for_session(&session, |seq| OutEvent::UserQuestion {
            session: session.clone(),
            seq,
            request_id: request_id.clone(),
            questions: Questions(questions_for_emit),
        });
        // A question is not a permission decision (#160): surface `WaitingAnswer`,
        // not `WaitingApproval`, so heads render "waiting for answer" distinctly.
        holly.emit_status(&session, AgentState::WaitingAnswer);

        // `Stop` (and a dropped registry) unwind silently — core cancels the
        // turn on the same `Stop`, so no `ToolResult` is owed. Approve/Reject
        // never target an `ask_user` request id.
        match pending::await_decision(rx).await {
            seam::Decision::Answer { answers } => {
                open.remove(&session, &request_id);
                holly.emit_status(&session, AgentState::Thinking);
                let output = fold_answers(&questions, &answers);
                seam::reply(&holly, session, request_id, output, false).await;
                return;
            }
            // #515: withdraw without cancelling the rest of the turn — unlike
            // `Stop`, this call's `ToolResult` is still owed, so the model
            // sees a withdrawal note rather than a call that never resolves.
            seam::Decision::Retract => {
                open.remove(&session, &request_id);
                holly.emit_status(&session, AgentState::Thinking);
                seam::reply(
                    &holly,
                    session,
                    request_id,
                    "The user withdrew this question without answering.".to_string(),
                    false,
                )
                .await;
                return;
            }
            // #515: swap the content and re-park under the same request id —
            // not terminal, so the loop re-emits `UserQuestion` instead of
            // replying.
            seam::Decision::Replace { questions: revised } => {
                questions = revised;
            }
            seam::Decision::Stop => {
                open.remove(&session, &request_id);
                return;
            }
            seam::Decision::Approve { .. } | seam::Decision::Reject { .. } => {
                // Never targets an `ask_user` request id; treat like `Stop`.
                open.remove(&session, &request_id);
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_input_reads_multiple_questions() {
        let qs = parse_input(
            r#"{"questions":[
                {"question":"Which?","options":[{"label":"A","description":"first"},{"label":"B"}]},
                {"question":"Regions?","options":[{"label":"us"},{"label":"eu"}],"multi_select":true}
            ]}"#,
        );
        assert_eq!(qs.len(), 2);
        assert_eq!(qs[0].question, "Which?");
        assert_eq!(qs[0].options.len(), 2);
        assert_eq!(qs[0].options[0].description.as_deref(), Some("first"));
        assert_eq!(qs[0].options[1].description, None);
        assert!(!qs[0].multi_select);
        assert_eq!(qs[1].question, "Regions?");
        assert!(qs[1].multi_select);
    }

    #[test]
    fn parse_input_skips_empty_labels() {
        let qs = parse_input(
            r#"{"questions":[{"question":"Q","options":[{"label":""},{"label":"ok"}]}]}"#,
        );
        assert_eq!(qs.len(), 1);
        assert_eq!(qs[0].options.len(), 1);
        assert_eq!(qs[0].options[0].label, "ok");
    }

    #[test]
    fn no_options_is_fine_free_text_is_always_available() {
        let qs = parse_input(r#"{"questions":[{"question":"Open?","options":[]}]}"#);
        assert_eq!(qs.len(), 1);
        assert!(qs[0].options.is_empty());
    }

    #[test]
    fn bare_string_degrades_to_free_form_question() {
        let qs = parse_input("just asking");
        assert_eq!(qs.len(), 1);
        assert_eq!(qs[0].question, "just asking");
        assert!(qs[0].options.is_empty());
    }

    #[test]
    fn parse_input_accepts_legacy_single_question_shape() {
        let qs = parse_input(
            r#"{"question":"Which?","options":[{"label":"A"},{"label":"B"}],"allow_free_form":true}"#,
        );
        assert_eq!(qs.len(), 1);
        assert_eq!(qs[0].question, "Which?");
        assert_eq!(qs[0].options.len(), 2);
        assert!(!qs[0].multi_select);
    }

    #[test]
    fn fold_answers_single_question_is_bare_answer() {
        let qs = vec![Question {
            question: "Which?".into(),
            options: vec![],
            multi_select: false,
        }];
        assert_eq!(fold_answers(&qs, &[vec!["REST".into()]]), "REST");
    }

    #[test]
    fn fold_answers_multi_select_joins_labels() {
        let qs = vec![Question {
            question: "Regions?".into(),
            options: vec![],
            multi_select: true,
        }];
        assert_eq!(
            fold_answers(&qs, &[vec!["us-east".into(), "eu-west".into()]]),
            "us-east, eu-west"
        );
    }

    #[test]
    fn fold_answers_multiple_questions_labels_each_line() {
        let qs = vec![
            Question {
                question: "Which?".into(),
                options: vec![],
                multi_select: false,
            },
            Question {
                question: "Regions?".into(),
                options: vec![],
                multi_select: true,
            },
        ];
        let answers = vec![vec!["REST".into()], vec!["us".into(), "eu".into()]];
        assert_eq!(
            fold_answers(&qs, &answers),
            "Which?: REST\nRegions?: us, eu"
        );
    }
}
