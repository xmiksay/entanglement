//! Conversation message types shared across the LLM seam.
//!
//! `Message`/`MessageRole` are the wire representation of one conversation turn.
//! They live in `entanglement-provider` because they are part of the `Llm`
//! request contract ([`crate::LlmRequest`]) — a raw-LLM consumer needs them
//! without pulling in the engine. `entanglement-core` re-exports them and owns
//! the rolling history (`Context`) built on top.

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

/// A single conversation message.
///
/// Assistant messages may carry [`ToolCall`]s in addition to (or instead of)
/// text; tool results are stored as plain text on a `Tool`-role message, linked
/// back to the originating tool call via `tool_call_id`. That id is load-bearing
/// for providers like Anthropic, whose `tool_result` block requires `tool_use_id`.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Message {
    pub role: MessageRole,
    pub text: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCall>,
    /// `Some` only on `Tool`-role messages: the id of the tool call this result
    /// answers. Echoed as Anthropic's `tool_use_id` / OpenAI's `tool_call_id`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

impl Message {
    pub fn user(text: impl Into<String>) -> Self {
        Self {
            role: MessageRole::User,
            text: text.into(),
            tool_calls: Vec::new(),
            tool_call_id: None,
        }
    }
    pub fn assistant(text: impl Into<String>, tool_calls: Vec<ToolCall>) -> Self {
        Self {
            role: MessageRole::Assistant,
            text: text.into(),
            tool_calls,
            tool_call_id: None,
        }
    }
    pub fn tool(tool_call_id: impl Into<String>, text: impl Into<String>) -> Self {
        Self {
            role: MessageRole::Tool,
            text: text.into(),
            tool_calls: Vec::new(),
            tool_call_id: Some(tool_call_id.into()),
        }
    }
}
