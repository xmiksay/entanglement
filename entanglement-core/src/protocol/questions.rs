//! `ask_user` question shapes (ADR-0027, #488, #515).

use serde::{Deserialize, Serialize};

use crate::protocol::*;

/// One labelled choice in a model-driven [`OutEvent::UserQuestion`] prompt
/// (ADR-0027). The `label` is what flows back as the answer when picked; the
/// optional `description` is a short hint shown beneath it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuestionOption {
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// One question in a model-driven [`OutEvent::UserQuestion`] call (#488,
/// supersedes parts of ADR-0027): several may ride in a single `ask_user`
/// invocation. `multi_select` lets the user check off any number of
/// `options`; a free-text "Other" answer is always offered by every head
/// regardless of this flag — unlike the v1 shape this supersedes, there is no
/// `allow_free_form` to opt out of it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Question {
    pub question: String,
    pub options: Vec<QuestionOption>,
    #[serde(default)]
    pub multi_select: bool,
}

/// The `questions` payload of an [`OutEvent::UserQuestion`] (#488). A plain
/// `Vec<Question>` field can't absorb a legacy log's sibling top-level keys
/// (`question`/`options`/`allow_free_form`) — `serde`'s field-level
/// `deserialize_with` only ever sees the value already keyed under its own
/// field name, and merging *sibling* keys into one field needs `#[serde(flatten)]`
/// over an `untagged` shape instead (unlike [`de_prompt_content`][super::in_msg::de_prompt_content], which only
/// ever swaps one field's own shape). This newtype is that flattened field:
/// it deserializes either the current `{"questions": [...]}` shape or the
/// pre-#488 flat `{"question", "options", "allow_free_form"}` shape (folded
/// into a one-element vec, `allow_free_form` dropped — free text is
/// unconditional now), and always serializes back out as `{"questions": [...]}`.
#[derive(Debug, Clone, PartialEq)]
pub struct Questions(pub Vec<Question>);

impl Serialize for Questions {
    fn serialize<S>(&self, s: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::SerializeStruct;
        let mut st = s.serialize_struct("Questions", 1)?;
        st.serialize_field("questions", &self.0)?;
        st.end()
    }
}

impl<'de> Deserialize<'de> for Questions {
    fn deserialize<D>(d: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Repr {
            Multi {
                questions: Vec<Question>,
            },
            Legacy {
                question: String,
                #[serde(default)]
                options: Vec<QuestionOption>,
                #[serde(default)]
                #[allow(dead_code)]
                allow_free_form: bool,
            },
        }
        Ok(match Repr::deserialize(d)? {
            Repr::Multi { questions } => Questions(questions),
            Repr::Legacy {
                question, options, ..
            } => Questions(vec![Question {
                question,
                options,
                multi_select: false,
            }]),
        })
    }
}

/// One open `ask_user` question still parked awaiting an answer, as reported
/// in an [`OutEvent::QuestionList`] snapshot (#515, in reply to
/// [`InMsg::ListQuestions`]). Mirrors the payload of the still-open
/// [`OutEvent::UserQuestion`] plus the `session` it belongs to — a
/// `QuestionList` can span every session, so each entry must name its own.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PendingQuestion {
    pub session: SessionId,
    pub request_id: String,
    #[serde(flatten)]
    pub questions: Questions,
}
