//! Plain-text rendering of history: the compaction fallback shape's body
//! (ADR-0202 §4 — a pinned aux model, or a head too large for the real
//! window) and the kept tail a copy-on-write `/compact` report carries
//! verbatim (ADR-0102). Split out of `session/summarize.rs` (400-line cap).

use entanglement_provider::{Message, MessageRole};

/// Per-tool-message transcript cap (head+tail chars), so one oversized tool
/// output doesn't blow the summarizer's own context window.
const TRANSCRIPT_TOOL_MESSAGE_CAP: usize = 2_000;

/// Render `messages` as a `[role]`-tagged transcript. Each `Tool`-role message
/// beyond [`TRANSCRIPT_TOOL_MESSAGE_CAP`] chars is truncated head+tail.
pub(crate) fn render_transcript(messages: &[Message]) -> String {
    let mut out = String::new();
    for msg in messages {
        let role = match msg.role {
            MessageRole::User => "user",
            MessageRole::Assistant => "assistant",
            MessageRole::Tool => "tool",
        };
        let text = msg.text();
        let body = if msg.role == MessageRole::Tool {
            truncate_head_tail(&text, TRANSCRIPT_TOOL_MESSAGE_CAP)
        } else {
            text
        };
        out.push_str(&format!("[{role}]\n{body}\n\n"));
    }
    out
}

/// Truncate `text` to at most `cap` chars, keeping the first and last `cap/2`
/// chars with a marker in between. A no-op under the cap.
fn truncate_head_tail(text: &str, cap: usize) -> String {
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= cap {
        return text.to_string();
    }
    let half = cap / 2;
    let head: String = chars[..half].iter().collect();
    let tail: String = chars[chars.len() - half..].iter().collect();
    let dropped = chars.len() - cap;
    format!("{head}\n... [{dropped} chars truncated] ...\n{tail}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_head_tail_is_a_noop_under_the_cap() {
        assert_eq!(truncate_head_tail("short", 100), "short");
    }

    #[test]
    fn truncate_head_tail_keeps_head_and_tail() {
        let text = "a".repeat(50) + &"b".repeat(50);
        let truncated = truncate_head_tail(&text, 40);
        assert!(truncated.starts_with(&"a".repeat(20)));
        assert!(truncated.ends_with(&"b".repeat(20)));
        assert!(truncated.contains("truncated"));
    }

    #[test]
    fn render_transcript_truncates_only_oversized_tool_messages() {
        let messages = vec![
            Message::user("short user text"),
            Message::tool("t1", "x".repeat(5_000)),
        ];
        let out = render_transcript(&messages);
        assert!(out.contains("[user]\nshort user text"));
        assert!(out.contains("truncated"));
        assert!(!out.starts_with("[tool]"));
    }
}
