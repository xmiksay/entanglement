//! Conversation context: the message history the engine sends to the LLM,
//! plus a conservative token estimate for future trimming.

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

/// Approximate tokens-per-char for the heuristic estimator.
///
/// Anthropic's exact tokenizer isn't readily available in Rust, so we use a
/// safe heuristic and keep the limit below the real ceiling.
const CHARS_PER_TOKEN: f32 = 3.5;

/// Conservative soft cap (in tokens) below the model's hard 200k ceiling.
pub const CONTEXT_LIMIT_TOKENS: usize = 180_000;

/// Owns the rolling conversation history and a token estimate.
///
/// Trimming / summarization lands in a later phase; for now this just appends
/// and reports usage so the engine can refuse an over-long turn.
#[derive(Debug, Default)]
pub struct Context {
    messages: Vec<Message>,
}

impl Context {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn messages(&self) -> &[Message] {
        &self.messages
    }

    pub fn push(&mut self, message: Message) {
        tracing::debug!(
            role = ?message.role,
            text_len = message.text.len(),
            tool_calls = message.tool_calls.len(),
            tool_call_id = message.tool_call_id.as_deref(),
            "pushing message to context"
        );
        self.messages.push(message);
    }

    pub fn push_user(&mut self, text: impl Into<String>) {
        self.push(Message::user(text));
    }
    pub fn push_assistant(&mut self, text: impl Into<String>, tool_calls: Vec<ToolCall>) {
        self.push(Message::assistant(text, tool_calls));
    }
    pub fn push_tool(&mut self, tool_call_id: impl Into<String>, text: impl Into<String>) {
        self.push(Message::tool(tool_call_id, text));
    }

    pub fn clear(&mut self) {
        self.messages.clear();
    }

    /// Rough token estimate for the whole history.
    pub fn estimated_tokens(&self) -> usize {
        let chars: usize = self
            .messages
            .iter()
            .map(|m| {
                m.text.chars().count()
                    + m.tool_calls
                        .iter()
                        .map(|c| c.input.chars().count())
                        .sum::<usize>()
            })
            .sum();
        (chars as f32 / CHARS_PER_TOKEN).ceil() as usize
    }

    /// True when we are within budget.
    pub fn within_limit(&self) -> bool {
        self.estimated_tokens() <= CONTEXT_LIMIT_TOKENS
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn estimates_tokens_from_chars() {
        let mut ctx = Context::new();
        ctx.push_user("hello world"); // 11 chars / 3.5 ~= 4 tokens
        assert!(ctx.estimated_tokens() >= 3 && ctx.estimated_tokens() <= 5);
        assert!(ctx.within_limit());
    }

    #[test]
    fn history_roundtrips_through_push() {
        let mut ctx = Context::new();
        ctx.push_user("hi");
        ctx.push_assistant("hello", Vec::new());
        assert_eq!(ctx.messages().len(), 2);
    }
}
