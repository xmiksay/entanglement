//! `ask_user` — model-driven user-decision prompt (#90, ADR-0027).
//!
//! Like `agent_spawn` (ADR-0022), `ask_user` is a runtime-owned tool intercepted
//! on [`OutEvent::ToolExec`] *before* permission resolution: it touches no host
//! resource, so it bypasses the permission profile. When the model calls it,
//! [`run_ask_user`] surfaces the question to the head via a dedicated
//! [`OutEvent::UserQuestion`] (a plain `ToolRequest` can't carry labelled
//! choices), parks for the head's [`InMsg::AnswerQuestion`], and folds the answer
//! back as the tool's [`InMsg::ToolResult`] — reusing the #58 round-trip so the
//! parent turn sees an ordinary tool result and core needs no new semantics.
//!
//! The answer fed to the model is the picked option's label or the free-form
//! text verbatim. A `Stop` for the session while parked unwinds silently: core's
//! turn cancels on the same `Stop`, so no `ToolResult` is owed (mirrors approval).

use entanglement_core::{AgentState, Holly, InMsg, OutEvent, QuestionOption, SessionId, ToolSpec};
use tokio::sync::broadcast::{error::RecvError, Receiver};

/// Tool name the model calls to ask the user a decision question.
pub const ASK_USER_TOOL: &str = "ask_user";

/// The `ask_user` tool schema advertised to the model. Appended to the engine's
/// `tool_specs` alongside the host quartet and `agent_spawn`.
pub fn ask_user_spec() -> ToolSpec {
    ToolSpec::with_schema(
        ASK_USER_TOOL,
        "Ask the user a decision question when the choice is genuinely theirs to \
         make (ambiguous requirements, a trade-off, a preference). Offer 2-4 \
         labelled options; set allow_free_form when a typed 'Other' answer makes \
         sense. The user's selected label (or typed text) becomes this tool's \
         output. Prefer doing the work directly over asking when you can decide.",
        serde_json::json!({
            "type": "object",
            "properties": {
                "question": {
                    "type": "string",
                    "description": "The question to put to the user."
                },
                "options": {
                    "type": "array",
                    "description": "Labelled choices the user picks from.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "label": {
                                "type": "string",
                                "description": "Short choice label — this exact text is returned when picked."
                            },
                            "description": {
                                "type": "string",
                                "description": "Optional one-line hint shown beneath the label."
                            }
                        },
                        "required": ["label"]
                    }
                },
                "allow_free_form": {
                    "type": "boolean",
                    "description": "Whether to offer an 'Other' entry for a typed free-text answer. Defaults to false."
                }
            },
            "required": ["question", "options"]
        }),
    )
}

/// Parsed `ask_user` arguments.
struct Question {
    question: String,
    options: Vec<QuestionOption>,
    allow_free_form: bool,
}

/// Parse the `ask_user` tool input. Providers send a JSON object; a malformed or
/// bare-string input degrades to a free-form question so the turn still gets an
/// answer path instead of a schema error.
fn parse_input(input: &str) -> Question {
    match serde_json::from_str::<serde_json::Value>(input) {
        Ok(v) => {
            let question = v
                .get("question")
                .and_then(|q| q.as_str())
                .unwrap_or(input)
                .to_string();
            let options = v
                .get("options")
                .and_then(|o| o.as_array())
                .map(|arr| arr.iter().filter_map(parse_option).collect::<Vec<_>>())
                .unwrap_or_default();
            let allow_free_form = v
                .get("allow_free_form")
                .and_then(|b| b.as_bool())
                // With no options offered, a free-form answer is the only way out.
                .unwrap_or(false)
                || options.is_empty();
            Question {
                question,
                options,
                allow_free_form,
            }
        }
        Err(_) => Question {
            question: input.to_string(),
            options: Vec::new(),
            allow_free_form: true,
        },
    }
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

/// Orchestrate one `ask_user` call: surface the question, await the head's
/// answer, and reply to the model with it.
///
/// The caller subscribes to the inbound fan-out *before* handing off (so a fast
/// answer can't race ahead) and passes the receiver in — mirroring the approval
/// path in [`crate::tool_runner`].
pub async fn run_ask_user(
    holly: Holly,
    mut inbound: Receiver<InMsg>,
    session: SessionId,
    seq: u64,
    request_id: String,
    input: String,
) {
    let q = parse_input(&input);

    let _ = holly.events().send(OutEvent::UserQuestion {
        session: session.clone(),
        seq,
        request_id: request_id.clone(),
        question: q.question,
        options: q.options,
        allow_free_form: q.allow_free_form,
    });
    let _ = holly.events().send(OutEvent::Status {
        session: session.clone(),
        state: AgentState::WaitingApproval,
    });

    loop {
        match inbound.recv().await {
            Ok(InMsg::AnswerQuestion {
                session: s,
                request_id: rid,
                answer,
            }) if s == session && rid == request_id => {
                let _ = holly.events().send(OutEvent::Status {
                    session: session.clone(),
                    state: AgentState::Thinking,
                });
                reply(&holly, session, request_id, answer).await;
                return;
            }
            Ok(InMsg::Stop { session: s }) if s == session => return,
            Ok(_) => {}
            Err(RecvError::Lagged(_)) => {}
            Err(RecvError::Closed) => return,
        }
    }
}

async fn reply(holly: &Holly, session: SessionId, request_id: String, output: String) {
    let _ = holly
        .send(InMsg::ToolResult {
            session,
            request_id,
            output,
        })
        .await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_input_reads_options_and_free_form() {
        let q = parse_input(
            r#"{"question":"Which?","options":[{"label":"A","description":"first"},{"label":"B"}],"allow_free_form":true}"#,
        );
        assert_eq!(q.question, "Which?");
        assert_eq!(q.options.len(), 2);
        assert_eq!(q.options[0].label, "A");
        assert_eq!(q.options[0].description.as_deref(), Some("first"));
        assert_eq!(q.options[1].description, None);
        assert!(q.allow_free_form);
    }

    #[test]
    fn parse_input_skips_empty_labels() {
        let q = parse_input(r#"{"question":"Q","options":[{"label":""},{"label":"ok"}]}"#);
        assert_eq!(q.options.len(), 1);
        assert_eq!(q.options[0].label, "ok");
    }

    #[test]
    fn no_options_forces_free_form() {
        let q = parse_input(r#"{"question":"Open?","options":[]}"#);
        assert!(q.options.is_empty());
        assert!(q.allow_free_form, "no options must allow a typed answer");
    }

    #[test]
    fn bare_string_degrades_to_free_form_question() {
        let q = parse_input("just asking");
        assert_eq!(q.question, "just asking");
        assert!(q.options.is_empty());
        assert!(q.allow_free_form);
    }
}
