//! Conversation message types shared across the LLM seam.
//!
//! `Message`/`MessageRole` are the wire representation of one conversation turn.
//! They live in `entanglement-provider` because they are part of the `Llm`
//! request contract ([`crate::LlmRequest`]) — a raw-LLM consumer needs them
//! without pulling in the engine. `entanglement-core` re-exports them and owns
//! the rolling history (`Context`) built on top.
//!
//! A message's body is a `Vec<ContentPart>` (multimodal), not a bare `String`
//! (#197, ADR-0064): text today, image blocks as of #221 (`read` emits images).
//! A serde back-compat shim keeps the old text-only shape (`text: "…"`)
//! deserializable so persisted logs written before the migration still replay.

use crate::llm::ToolCall;

/// Author of a [`Message`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageRole {
    User,
    Assistant,
    /// Result of a tool invocation, reported back to the model.
    Tool,
}

/// One part of a message's multimodal content. Tagged by `type` on the wire so
/// the enum can grow (audio, documents) without breaking older readers of the
/// existing variants.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentPart {
    /// A run of plain text.
    Text { text: String },
    /// An image block. First emitted by #221 (`read` on an image file); the
    /// converters render it to each provider's native image wire format.
    Image { source: ImageSource },
}

impl ContentPart {
    /// A text part.
    pub fn text(text: impl Into<String>) -> Self {
        ContentPart::Text { text: text.into() }
    }

    /// A base64-inline image part.
    pub fn image(media_type: impl Into<String>, data: impl Into<String>) -> Self {
        ContentPart::Image {
            source: ImageSource::Base64 {
                media_type: media_type.into(),
                data: data.into(),
            },
        }
    }

    /// The text of a [`Text`][ContentPart::Text] part, else `None`.
    pub fn as_text(&self) -> Option<&str> {
        match self {
            ContentPart::Text { text } => Some(text),
            ContentPart::Image { .. } => None,
        }
    }
}

/// Source of an [image content block][ContentPart::Image]. Base64-inline today
/// (maps to Anthropic's `image`/base64 source and OpenAI's `data:` URL); a
/// `Url` variant can be added later without touching the existing wire.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ImageSource {
    Base64 { media_type: String, data: String },
}

/// Concatenated text of every [`Text`][ContentPart::Text] part; image parts are
/// skipped. The token estimator, compaction, and text-only converters read this.
pub fn content_text(content: &[ContentPart]) -> String {
    content.iter().filter_map(ContentPart::as_text).collect()
}

/// Whether `content` carries at least one [image][ContentPart::Image] part. Lets
/// the converters and the tool-result fold pick the multimodal path (block array
/// / trailing user message) only when an image is actually present (#221).
pub fn content_has_image(content: &[ContentPart]) -> bool {
    content
        .iter()
        .any(|p| matches!(p, ContentPart::Image { .. }))
}

/// Build the content vec for a text-only message: empty text → no parts (so an
/// assistant turn that is tool-calls-only carries no stray empty text block),
/// non-empty → a single [`Text`][ContentPart::Text] part.
fn text_content(text: impl Into<String>) -> Vec<ContentPart> {
    let text = text.into();
    if text.is_empty() {
        Vec::new()
    } else {
        vec![ContentPart::text(text)]
    }
}

/// A single conversation message.
///
/// Assistant messages may carry [`ToolCall`]s in addition to (or instead of)
/// text; tool results are stored as content on a `Tool`-role message, linked
/// back to the originating tool call via `tool_call_id`. That id is load-bearing
/// for providers like Anthropic, whose `tool_result` block requires `tool_use_id`.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(from = "MessageRepr")]
pub struct Message {
    pub role: MessageRole,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub content: Vec<ContentPart>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCall>,
    /// `Some` only on `Tool`-role messages: the id of the tool call this result
    /// answers. Echoed as Anthropic's `tool_use_id` / OpenAI's `tool_call_id`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

/// Deserialization shim (#197, ADR-0064): accepts both the current
/// `content: [ContentPart]` shape and the legacy text-only `text: "…"` shape so
/// logs persisted before the migration still replay. New writes always emit
/// `content`.
#[derive(serde::Deserialize)]
struct MessageRepr {
    role: MessageRole,
    #[serde(default)]
    content: Option<Vec<ContentPart>>,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    tool_calls: Vec<ToolCall>,
    #[serde(default)]
    tool_call_id: Option<String>,
}

impl From<MessageRepr> for Message {
    fn from(r: MessageRepr) -> Self {
        let content = match (r.content, r.text) {
            (Some(content), _) => content,
            (None, Some(text)) => text_content(text),
            (None, None) => Vec::new(),
        };
        Message {
            role: r.role,
            content,
            tool_calls: r.tool_calls,
            tool_call_id: r.tool_call_id,
        }
    }
}

impl Message {
    pub fn user(text: impl Into<String>) -> Self {
        Self::user_content(text_content(text))
    }
    /// A user message with explicit multimodal content (e.g. a screenshot prompt).
    pub fn user_content(content: Vec<ContentPart>) -> Self {
        Self {
            role: MessageRole::User,
            content,
            tool_calls: Vec::new(),
            tool_call_id: None,
        }
    }
    pub fn assistant(text: impl Into<String>, tool_calls: Vec<ToolCall>) -> Self {
        Self {
            role: MessageRole::Assistant,
            content: text_content(text),
            tool_calls,
            tool_call_id: None,
        }
    }
    pub fn tool(tool_call_id: impl Into<String>, text: impl Into<String>) -> Self {
        Self::tool_content(tool_call_id, text_content(text))
    }
    /// A tool-result message with explicit multimodal content (e.g. `read` on an
    /// image, #221).
    pub fn tool_content(tool_call_id: impl Into<String>, content: Vec<ContentPart>) -> Self {
        Self {
            role: MessageRole::Tool,
            content,
            tool_calls: Vec::new(),
            tool_call_id: Some(tool_call_id.into()),
        }
    }

    /// Concatenated text of the message's [`Text`][ContentPart::Text] parts;
    /// image parts are skipped. See [`content_text`].
    pub fn text(&self) -> String {
        content_text(&self.content)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deserializes_legacy_text_shape() {
        // A message persisted before #197 carries a bare `text` string.
        let legacy = r#"{"role":"user","text":"hello"}"#;
        let msg: Message = serde_json::from_str(legacy).unwrap();
        assert_eq!(msg.content, vec![ContentPart::text("hello")]);
        assert_eq!(msg.text(), "hello");
    }

    #[test]
    fn legacy_empty_text_yields_no_parts() {
        let legacy = r#"{"role":"assistant","text":"","tool_calls":[]}"#;
        let msg: Message = serde_json::from_str(legacy).unwrap();
        assert!(msg.content.is_empty());
    }

    #[test]
    fn serializes_content_blocks_and_roundtrips() {
        let msg = Message::user_content(vec![
            ContentPart::text("look"),
            ContentPart::image("image/png", "AAAA"),
        ]);
        let json = serde_json::to_string(&msg).unwrap();
        assert_eq!(
            json,
            r#"{"role":"user","content":[{"type":"text","text":"look"},{"type":"image","source":{"type":"base64","media_type":"image/png","data":"AAAA"}}]}"#
        );
        let back: Message = serde_json::from_str(&json).unwrap();
        assert_eq!(back.content, msg.content);
        // Only the text part contributes to `text()`.
        assert_eq!(back.text(), "look");
    }

    #[test]
    fn text_constructors_skip_empty_bodies() {
        assert!(Message::user("").content.is_empty());
        assert_eq!(Message::user("hi").content, vec![ContentPart::text("hi")]);
    }
}
