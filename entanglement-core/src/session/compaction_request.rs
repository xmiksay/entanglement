//! The request one compaction summarization sends (ADR-0202 §4), in one of
//! two shapes.
//!
//! **Structured** — the session's own backend, when the head fits the real
//! window: the round's resolved system prompt, the advertised tool specs, the
//! head messages verbatim, one trailing user instruction, and the session
//! `cache_key` — byte-identical to a turn's request up to that instruction.
//! The head is a strict prefix of the live history, so tools → system →
//! history replay the provider's cached prefix instead of re-reading the whole
//! conversation at full price, and the summarizer sees the real tool calls,
//! results and thinking blocks rather than a capped rendering. Nothing but the
//! instruction text forbids a tool call: a non-`auto` tool choice invalidates
//! Anthropic's messages cache and z.ai accepts only `auto`. A reply that calls
//! a tool anyway falls back once to the rendered shape
//! (`session/summary_attempt.rs`).
//!
//! **Rendered** — a pinned `summarize` aux model, or a head too large for the
//! window: the capped plain-text transcript under the summarizer's own system
//! string, no tools, no cache key. A pinned model is a different cache
//! namespace anyway, and a foreign wire may reject the history's signed
//! thinking blocks or tool-call id format.

use entanglement_provider::{GenerationParams, LlmRequest, Message, RetryConfig, ToolSpec};

use super::summarize::SummarizeError;
use super::transcript::render_transcript;
use crate::context::{estimate_message_tokens, estimate_text_tokens};

const SUMMARIZER_SYSTEM: &str = "You are a summarization assistant compacting a coding \
                                 agent's conversation history into a dense, information-\
                                 preserving summary.";

/// What the summary must carry — shared so the two shapes differ only in
/// where the conversation sits relative to the instruction.
const SUMMARY_GOAL: &str = "so it can fully replace the conversation history while a \
                            coding agent continues the work. Preserve: the user's goals, \
                            decisions made, files/paths touched, commands run, and \
                            outstanding next steps. Be concise but complete.";

/// Appended to the structured instruction: the request advertises the
/// session's tools (it must, to match the cached prefix), so only the prompt
/// can keep the reply text-only.
const NO_TOOLS: &str = "Do not call any tools; reply with the summary text only.";

/// Slack kept below the real window, as a divisor (1/50 = 2%): the
/// chars/token estimate is a heuristic, and unlike the rendered shape the
/// structured one has no per-message cap to absorb an underestimate.
const WINDOW_MARGIN_DIVISOR: usize = 50;

/// What a session sends ahead of its history. A structured compaction request
/// must replay exactly these to hit the cached prefix.
#[derive(Clone, Copy)]
pub(crate) struct SessionPrefix<'a> {
    pub system_prompt: &'a str,
    pub specs: &'a [ToolSpec],
    pub cache_key: &'a str,
}

/// An owned compaction request body; [`Self::llm_request`] borrows it.
pub(crate) struct CompactionRequest<'a> {
    system: &'a str,
    messages: Vec<Message>,
    tools: &'a [ToolSpec],
    cache_key: Option<&'a str>,
}

impl<'a> CompactionRequest<'a> {
    /// The structured shape, or `None` when head + prefix + the reserved reply
    /// would not fit `window` — the caller then falls back to
    /// [`Self::rendered`]. Judged against the real window, not
    /// `Context::limit`: auto-compaction fires precisely because the context
    /// is over the limit, and mid-turn the kept tail collapses to nothing, so
    /// the head *is* that over-limit context.
    pub(crate) fn structured(
        prefix: SessionPrefix<'a>,
        head: &[Message],
        instructions: Option<&str>,
        window: usize,
        generation: Option<GenerationParams>,
    ) -> Option<Self> {
        let instruction = with_extra(
            format!("Summarize the conversation above {SUMMARY_GOAL} {NO_TOOLS}"),
            instructions,
        );
        let tokens = estimate_message_tokens(head)
            + estimate_text_tokens(&instruction)
            + estimate_text_tokens(prefix.system_prompt)
            + spec_tokens(prefix.specs);
        let budget = structured_budget(window, generation);
        if tokens > budget {
            tracing::debug!(
                tokens,
                budget,
                "compaction: head exceeds the real window, rendering a transcript instead"
            );
            return None;
        }
        let mut messages = head.to_vec();
        messages.push(Message::user(instruction));
        Some(Self {
            system: prefix.system_prompt,
            messages,
            tools: prefix.specs,
            cache_key: Some(prefix.cache_key),
        })
    }

    /// The rendered shape, refused when the transcript alone overflows `limit`
    /// (#178, ADR-0101) — shipping it would burn a paid round-trip and 4xx.
    pub(crate) fn rendered(
        head: &[Message],
        instructions: Option<&str>,
        limit: usize,
    ) -> Result<Self, SummarizeError> {
        let transcript = render_transcript(head);
        let tokens = estimate_text_tokens(&transcript);
        if tokens > limit {
            return Err(SummarizeError::TranscriptTooLarge { tokens, limit });
        }
        let prompt = with_extra(
            format!("Summarize the conversation transcript below {SUMMARY_GOAL}\n\n{transcript}"),
            instructions,
        );
        Ok(Self {
            system: SUMMARIZER_SYSTEM,
            messages: vec![Message::user(prompt)],
            tools: &[],
            cache_key: None,
        })
    }

    /// The session id on the structured shape (its cache key), `None` on the
    /// rendered one.
    pub(crate) fn cache_key(&self) -> Option<&'a str> {
        self.cache_key
    }

    pub(crate) fn is_structured(&self) -> bool {
        self.cache_key.is_some()
    }

    pub(crate) fn llm_request<'r>(
        &'r self,
        model: Option<&'r str>,
        generation: Option<GenerationParams>,
        retry: Option<RetryConfig>,
    ) -> LlmRequest<'r> {
        LlmRequest {
            system: self.system,
            model,
            messages: &self.messages,
            tools: self.tools,
            generation,
            cache_key: self.cache_key,
            retry,
            // Compaction's own trailing instruction (`NO_TOOLS`, above) is
            // already folded into `messages` so the structured shape stays
            // byte-identical to a turn's request up to that instruction
            // (ADR-0202 §4) — the mode notice has no place here.
            trailing_notice: None,
        }
    }
}

fn with_extra(mut prompt: String, instructions: Option<&str>) -> String {
    if let Some(extra) = instructions {
        prompt.push_str(&format!("\n\nAdditional instructions: {extra}"));
    }
    prompt
}

fn spec_tokens(specs: &[ToolSpec]) -> usize {
    specs
        .iter()
        .map(|t| {
            estimate_text_tokens(&t.name)
                + estimate_text_tokens(&t.description)
                + estimate_text_tokens(&t.schema.to_string())
        })
        .sum()
}

/// Input room in `window` once the reply's `max_output_tokens` (0 when
/// unset) and the estimator margin are reserved.
fn structured_budget(window: usize, generation: Option<GenerationParams>) -> usize {
    let reply = generation.and_then(|g| g.max_output_tokens).unwrap_or(0) as usize;
    window
        .saturating_sub(reply)
        .saturating_sub(window / WINDOW_MARGIN_DIVISOR)
}

#[cfg(test)]
mod tests {
    use super::*;
    use entanglement_provider::MessageRole;

    fn prefix(specs: &[ToolSpec]) -> SessionPrefix<'_> {
        SessionPrefix {
            system_prompt: "SYS",
            specs,
            cache_key: "s-1",
        }
    }

    fn max_output(tokens: u32) -> Option<GenerationParams> {
        Some(GenerationParams {
            max_output_tokens: Some(tokens),
            ..Default::default()
        })
    }

    #[test]
    fn budget_reserves_the_reply_and_a_two_percent_margin() {
        assert_eq!(structured_budget(100_000, None), 98_000);
        assert_eq!(structured_budget(100_000, max_output(8_000)), 90_000);
        assert_eq!(structured_budget(1_000, max_output(5_000)), 0);
    }

    #[test]
    fn structured_replays_the_prefix_and_appends_one_instruction() {
        let specs = [ToolSpec::new("read", "reads a file")];
        let head = [Message::user("hi"), Message::tool("t1", "out")];
        let req =
            CompactionRequest::structured(prefix(&specs), &head, Some("focus on X"), 100_000, None)
                .expect("a tiny head fits");
        assert!(req.is_structured());
        assert_eq!(req.system, "SYS");
        assert_eq!(req.tools.len(), 1);
        assert_eq!(req.cache_key, Some("s-1"));
        assert_eq!(req.messages.len(), 3);
        assert_eq!(&req.messages[..2], &head[..]);
        let last = &req.messages[2];
        assert_eq!(last.role, MessageRole::User);
        assert!(last.text().contains("the conversation above"));
        assert!(
            last.text().contains(NO_TOOLS),
            "the instruction forbids tools"
        );
        assert!(last.text().ends_with("Additional instructions: focus on X"));
    }

    #[test]
    fn structured_declines_a_head_over_the_real_window() {
        let head = [Message::user("x".repeat(3_500))]; // 1000 tokens
        assert!(CompactionRequest::structured(prefix(&[]), &head, None, 1_000, None).is_none());
        assert!(CompactionRequest::structured(prefix(&[]), &head, None, 2_000, None).is_some());
        assert!(
            CompactionRequest::structured(prefix(&[]), &head, None, 2_000, max_output(1_000))
                .is_none(),
            "the reserved reply counts against the window"
        );
    }

    #[test]
    fn rendered_is_tool_less_uncached_and_guarded_by_the_limit() {
        let head = [Message::user("hi")];
        let Ok(req) = CompactionRequest::rendered(&head, None, 1_000) else {
            panic!("a tiny transcript fits");
        };
        assert!(!req.is_structured());
        assert!(req.system.contains("summarization assistant"));
        assert!(req.tools.is_empty());
        assert_eq!(req.cache_key, None);
        assert_eq!(req.messages.len(), 1);
        assert!(req.messages[0].text().contains("transcript below"));
        assert!(req.messages[0].text().contains("[user]\nhi"));

        let big = [Message::user("x".repeat(400))];
        assert!(matches!(
            CompactionRequest::rendered(&big, None, 10),
            Err(SummarizeError::TranscriptTooLarge { limit: 10, .. })
        ));
    }
}
