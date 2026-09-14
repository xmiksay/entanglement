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
    /// A provider-side web-search result block, minted server-side (#481,
    /// follow-up to #305/ADR-0075's "not persisted" MVP limitation). `data` is
    /// opaque JSON in `provider`'s own wire shape — it round-trips **verbatim**
    /// only when replaying to that same provider (mirrors
    /// [`ToolCall::provider_meta`][crate::ToolCall]'s opaque round-trip
    /// contract); every other converter (a different provider, or a plain
    /// renderer) reads only `summary`, a human-readable one-or-multi-line
    /// rendering of the query/results, and never inspects `data`.
    ProviderSearch {
        provider: String,
        summary: String,
        data: serde_json::Value,
    },
    /// A model's extended-thinking block, captured so it can be replayed to the
    /// provider that minted it. Anthropic requires the unmodified block —
    /// signature included — on the final assistant message whenever tool results
    /// come back, which is exactly a parked turn's shape, so a display-only
    /// reasoning channel cannot satisfy it.
    ///
    /// `data` is opaque JSON in `provider`'s own wire shape and round-trips
    /// **verbatim** only to that same provider (the
    /// [`ProviderSearch`][ContentPart::ProviderSearch] /
    /// [`ToolCall::provider_meta`][crate::ToolCall] contract). Provider-shaped
    /// details — Anthropic's `signature`, whether the block was
    /// `redacted_thinking` — live inside it rather than as fields here, so the
    /// core contract stays wire-agnostic. `text` is the human-readable rendering
    /// and **may be empty**: current models omit thinking text by default while
    /// still returning a live signature, and such a block must still replay.
    ///
    /// Unlike `ProviderSearch`, a foreign converter emits *nothing* rather than
    /// falling back to text — reasoning is not answer content, so leaking it into
    /// history on a model switch would corrupt the conversation.
    Reasoning {
        provider: String,
        text: String,
        data: serde_json::Value,
    },
    /// A `tool_reference` block (ADR-0196 §3, the Anthropic `anthropic_native`
    /// `ToolSearch` encoding): appears in a tool-result message's content
    /// answering a client-executed `describe()` call, naming one tool the API
    /// should auto-expand into context from its (already-sent, `defer_loading:
    /// true`) full definition. Request-side only — the API expands a
    /// referenced tool before Claude sees it, so this variant is never parsed
    /// back out of a response.
    ///
    /// Unlike [`ProviderSearch`][ContentPart::ProviderSearch] and
    /// [`Reasoning`][ContentPart::Reasoning], this carries no opaque
    /// provider-private payload and no `provider` tag: the wire shape is
    /// exactly one field (`tool_name`), nothing to stash verbatim, and the
    /// Anthropic converter renders it the same way regardless of which
    /// provider the request targets (it's the only wire with a native
    /// `tool_reference` mechanism). Every other converter degrades it to
    /// [`tool_reference_fallback_text`] — the same "keep it visible as text"
    /// contract `ProviderSearch::summary` gives a foreign wire, but
    /// synthesized here since there's no separate human-readable field to
    /// fall back to.
    ToolReference { tool_name: String },
    /// A `tool_search_output` input item (ADR-0196 §3, the OpenAI Responses
    /// `responses_native` `ToolSearch` encoding): the runtime's reply to a
    /// client-executed `tool_search_call`, persisted so the next request can
    /// replay the same input item verbatim (`call_id` correlation rides the
    /// enclosing tool-result message's own `tool_call_id`, exactly like
    /// [`ToolReference`][ContentPart::ToolReference] needs none of its own).
    ///
    /// `data` is opaque JSON — the `tools` array (full tool definitions, one
    /// per discovered name) the Responses client echoes back verbatim inside
    /// the `tool_search_output` item — and round-trips only to the
    /// `provider` that minted it, the same opaque-payload contract as
    /// [`ProviderSearch`][ContentPart::ProviderSearch] /
    /// [`Reasoning`][ContentPart::Reasoning]. `summary` is the portable,
    /// human-readable degrade-to-text rendering every foreign converter falls
    /// back to (mirrors `ProviderSearch::summary`) — unlike `ToolReference`,
    /// which has no separate human field and synthesizes its fallback from
    /// `tool_name` alone, this variant already carries one because the
    /// underlying wire event has no single name to fall back to (a search can
    /// discover zero, one, or many tools at once).
    ToolSearchOutput {
        provider: String,
        summary: String,
        data: serde_json::Value,
    },
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

    /// A provider-side web-search result block (#481). See
    /// [`ProviderSearch`][ContentPart::ProviderSearch].
    pub fn provider_search(
        provider: impl Into<String>,
        summary: impl Into<String>,
        data: serde_json::Value,
    ) -> Self {
        ContentPart::ProviderSearch {
            provider: provider.into(),
            summary: summary.into(),
            data,
        }
    }

    /// An extended-thinking block for provider round-trip. See
    /// [`Reasoning`][ContentPart::Reasoning].
    pub fn reasoning(
        provider: impl Into<String>,
        text: impl Into<String>,
        data: serde_json::Value,
    ) -> Self {
        ContentPart::Reasoning {
            provider: provider.into(),
            text: text.into(),
            data,
        }
    }

    /// A `tool_reference` block. See [`ToolReference`][ContentPart::ToolReference].
    pub fn tool_reference(tool_name: impl Into<String>) -> Self {
        ContentPart::ToolReference {
            tool_name: tool_name.into(),
        }
    }

    /// A `tool_search_output` block. See
    /// [`ToolSearchOutput`][ContentPart::ToolSearchOutput].
    pub fn tool_search_output(
        provider: impl Into<String>,
        summary: impl Into<String>,
        data: serde_json::Value,
    ) -> Self {
        ContentPart::ToolSearchOutput {
            provider: provider.into(),
            summary: summary.into(),
            data,
        }
    }

    /// The text of a [`Text`][ContentPart::Text] part, else `None`.
    ///
    /// A [`Reasoning`][ContentPart::Reasoning] part is deliberately **not**
    /// text: it must stay out of [`content_text`], which feeds the token
    /// estimator, compaction, and the text-only converters — reasoning is not
    /// part of the assistant's answer.
    pub fn as_text(&self) -> Option<&str> {
        match self {
            ContentPart::Text { text } => Some(text),
            ContentPart::Image { .. }
            | ContentPart::ProviderSearch { .. }
            | ContentPart::Reasoning { .. }
            | ContentPart::ToolReference { .. }
            | ContentPart::ToolSearchOutput { .. } => None,
        }
    }
}

/// Portable one-line fallback for a [`ToolReference`][ContentPart::ToolReference]
/// part on a wire with no native `tool_reference` mechanism (OpenAI-compat,
/// Gemini) — both converters' tool-result branch append this to the result
/// text so a discovered-tool outcome never silently vanishes from what the
/// model sees, mirroring [`ProviderSearch::summary`][ContentPart::ProviderSearch]'s
/// degrade-to-text contract.
pub fn tool_reference_fallback_text(tool_name: &str) -> String {
    format!("[discovered tool: {tool_name}]")
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
        Self::assistant_content(text_content(text), tool_calls)
    }
    /// An assistant turn with explicit multimodal content — text plus any
    /// provider-native blocks (a search call/result, #481) in arrival order.
    pub fn assistant_content(content: Vec<ContentPart>, tool_calls: Vec<ToolCall>) -> Self {
        Self {
            role: MessageRole::Assistant,
            content,
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

    #[test]
    fn provider_search_block_serializes_and_roundtrips() {
        let part = ContentPart::provider_search(
            "anthropic",
            "[web_search] rust async",
            serde_json::json!({ "type": "server_tool_use", "id": "srvtoolu_1" }),
        );
        let msg = Message::assistant_content(vec![ContentPart::text("here"), part.clone()], vec![]);
        assert_eq!(msg.text(), "here", "as_text skips the search block");
        let json = serde_json::to_string(&msg).unwrap();
        let back: Message = serde_json::from_str(&json).unwrap();
        assert_eq!(back.content, msg.content);
        assert_eq!(back.content[1], part);
    }

    #[test]
    fn tool_reference_block_serializes_and_roundtrips() {
        let part = ContentPart::tool_reference("search_files");
        let msg = Message::tool_content("call_1", vec![ContentPart::text("schema"), part.clone()]);
        assert_eq!(
            msg.text(),
            "schema",
            "as_text skips the tool_reference block"
        );
        let json = serde_json::to_string(&msg).unwrap();
        assert_eq!(
            json,
            r#"{"role":"tool","content":[{"type":"text","text":"schema"},{"type":"tool_reference","tool_name":"search_files"}],"tool_call_id":"call_1"}"#
        );
        let back: Message = serde_json::from_str(&json).unwrap();
        assert_eq!(back.content, msg.content);
    }

    #[test]
    fn tool_search_output_block_serializes_and_roundtrips() {
        let part = ContentPart::tool_search_output(
            "openai_responses",
            "discovered: get_weather",
            serde_json::json!([{ "type": "function", "name": "get_weather" }]),
        );
        let msg = Message::tool_content("call_1", vec![part.clone()]);
        assert_eq!(msg.text(), "", "as_text skips the tool_search_output block");
        let json = serde_json::to_string(&msg).unwrap();
        let back: Message = serde_json::from_str(&json).unwrap();
        assert_eq!(back.content, msg.content);
        assert_eq!(back.content[0], part);
    }

    #[test]
    fn tool_reference_fallback_text_names_the_tool() {
        assert_eq!(
            tool_reference_fallback_text("search_files"),
            "[discovered tool: search_files]"
        );
    }
}
