//! [`InMsg`] content helpers: the text-only constructors and the back-compat
//! deserializer for content fields that migrated from a bare string.

use entanglement_provider::ContentPart;
use serde::Deserialize;

use crate::protocol::{InMsg, SessionId};

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
