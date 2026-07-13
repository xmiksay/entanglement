//! Conversation context: the message history the engine sends to the LLM,
//! plus a conservative token estimate for future trimming.
//!
//! `Message`/`MessageRole` (the wire message shape) live in
//! `entanglement-provider` alongside the `Llm` request contract (ADR-0053);
//! this module owns the rolling history built on top of them.

use entanglement_provider::{Message, ToolCall};

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
