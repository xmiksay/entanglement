//! LLM-summarization core shared by the manual `"compact"` oneshot op
//! (`session/ops.rs`, copy-on-write, ADR-0101) and automatic in-place
//! auto-summarize on context overflow (`session/turn.rs`, #398, ADR-0103).
//! Both callers render the same head/tail transcript, guard it against the
//! session's own context budget, and ask the model for a dense summary of the
//! head with the tail (clamped to a safe turn boundary, #397/ADR-0102) riding
//! verbatim after it — they differ only in what happens to the result
//! (a report event vs. an in-place `Context` mutation).

use crate::context::Context;
use entanglement_provider::{
    GenerationParams, Llm, LlmEvent, LlmRequest, Message, MessageRole, StopReason, Usage,
};
use futures::StreamExt;

/// Per-tool-message transcript cap (head+tail chars) fed into the compaction
/// prompt, so one oversized tool output doesn't blow the summarizer's own
/// context window.
const TRANSCRIPT_TOOL_MESSAGE_CAP: usize = 2_000;

/// Why [`summarize`] couldn't produce (or accept) a summary. Every variant's
/// `Display` text matches what `compact_op` has always surfaced via
/// `OutEvent::Error`, so lifting the logic here changes no user-visible text.
pub(crate) enum SummarizeError {
    NoHistory,
    EntireHistoryKept {
        kept: usize,
    },
    TranscriptTooLarge {
        tokens: usize,
        limit: usize,
    },
    TailTooLarge {
        kept: usize,
        tokens: usize,
        limit: usize,
    },
    Truncated,
    Llm(anyhow::Error),
}

impl std::fmt::Display for SummarizeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SummarizeError::NoHistory => {
                write!(f, "cannot compact: no conversation history")
            }
            SummarizeError::EntireHistoryKept { kept } => write!(
                f,
                "cannot compact: kept ({kept}) covers the entire conversation — \
                 nothing left to summarize; use a smaller --keep value"
            ),
            SummarizeError::TranscriptTooLarge { tokens, limit } => write!(
                f,
                "cannot compact: transcript (~{tokens} tokens) exceeds the \
                 {limit}-token context budget — start a new session or shorten \
                 the conversation"
            ),
            SummarizeError::TailTooLarge {
                kept,
                tokens,
                limit,
            } => write!(
                f,
                "cannot compact: the {kept} kept trailing messages (~{tokens} \
                 tokens) alone exceed the {limit}-token context budget — use a \
                 smaller --keep value"
            ),
            SummarizeError::Truncated => write!(
                f,
                "compaction failed: the summary was truncated (stop reason: \
                 max_tokens) — refusing to fork a cut-off summary; the \
                 original session is unchanged"
            ),
            SummarizeError::Llm(e) => write!(f, "{e}"),
        }
    }
}

/// A completed summarization: `summary` already has the verbatim `kept` tail
/// (#397/ADR-0102) rendered separately in `tail_rendered` — deliberately
/// *not* baked into `summary`, since the two callers preserve the tail two
/// different ways: `ops.rs` (copy-on-write) has only a single flat string to
/// hand a forked session, so it composes `summary` + `tail_rendered` into one
/// report; `turn.rs` (in-place, #398) hands `summary` alone to
/// `Context::apply_compaction`, which re-derives the same tail *structurally*
/// from `kept` — baking the rendered tail text into `summary` there would
/// duplicate it (once as text, once as the real messages).
pub(crate) struct SummarizeOutcome {
    pub summary: String,
    pub kept: usize,
    pub tail_rendered: Option<String>,
    pub finish: Option<(Option<StopReason>, Usage)>,
}

/// Compose `summary` and the rendered `tail` (if any) into one flat report —
/// what a copy-on-write fork's single seed prompt carries (#397/ADR-0102).
pub(crate) fn compose_report(summary: &str, kept: usize, tail_rendered: Option<&str>) -> String {
    match tail_rendered {
        Some(tail) => format!(
            "{summary}\n\n---\nThe following {kept} most recent messages are \
             preserved verbatim (not summarized):\n\n{tail}"
        ),
        None => summary.to_string(),
    }
}

/// Summarize `ctx`'s head with `llm`, preserving the tail (clamped to
/// `ctx.safe_kept(requested_kept)`) verbatim. Shared by both callers — see the
/// module doc for what differs after this returns.
pub(crate) async fn summarize(
    ctx: &Context,
    llm: &mut dyn Llm,
    model: Option<&str>,
    generation: Option<GenerationParams>,
    requested_kept: usize,
    instructions: Option<&str>,
) -> Result<SummarizeOutcome, SummarizeError> {
    if ctx.messages().is_empty() {
        return Err(SummarizeError::NoHistory);
    }

    let kept = ctx.safe_kept(requested_kept);
    let split = ctx.messages().len() - kept;
    let (head, tail) = ctx.messages().split_at(split);

    if head.is_empty() {
        return Err(SummarizeError::EntireHistoryKept { kept });
    }

    let transcript = render_transcript(head);
    let tail_transcript = (!tail.is_empty()).then(|| render_transcript(tail));

    // Guard an oversized transcript (#178, ADR-0101): if the rendered input
    // alone already blows the context budget, shipping it would just burn a
    // paid round-trip and 4xx at the provider.
    let transcript_tokens = estimate_tokens(&transcript);
    if transcript_tokens > ctx.limit() {
        return Err(SummarizeError::TranscriptTooLarge {
            tokens: transcript_tokens,
            limit: ctx.limit(),
        });
    }

    // The kept tail rides verbatim (unsummarized), so it must fit the budget
    // on its own too.
    if let Some(tail_transcript) = &tail_transcript {
        let tail_tokens = estimate_tokens(tail_transcript);
        if tail_tokens > ctx.limit() {
            return Err(SummarizeError::TailTooLarge {
                kept,
                tokens: tail_tokens,
                limit: ctx.limit(),
            });
        }
    }

    let mut prompt = format!(
        "Summarize the conversation transcript below so it can fully replace \
         the conversation history while a coding agent continues the work. \
         Preserve: the user's goals, decisions made, files/paths touched, \
         commands run, and outstanding next steps. Be concise but complete.\n\n\
         {}",
        transcript
    );
    if let Some(extra) = instructions {
        prompt.push_str(&format!("\n\nAdditional instructions: {extra}"));
    }

    const SYSTEM: &str = "You are a summarization assistant compacting a coding \
                          agent's conversation history into a dense, information-\
                          preserving summary.";
    let messages = [Message::user(prompt)];

    let (summary, finish) = oneshot_text(llm, SYSTEM, &messages, model, generation)
        .await
        .map_err(SummarizeError::Llm)?;

    // Refuse a truncated summary: a `max_tokens`-cut-off fragment must not
    // replace (or report as replacing) real history.
    if let Some((Some(StopReason::MaxTokens), _)) = &finish {
        return Err(SummarizeError::Truncated);
    }

    Ok(SummarizeOutcome {
        summary,
        kept,
        tail_rendered: tail_transcript,
        finish,
    })
}

/// Run one tool-less, non-streamed-to-the-UI completion: build the request,
/// drain the stream concatenating `Text` chunks, and return the assembled text
/// plus the `Finish` payload (for usage/cost).
async fn oneshot_text(
    llm: &mut dyn Llm,
    system: &str,
    messages: &[Message],
    model: Option<&str>,
    generation: Option<GenerationParams>,
) -> anyhow::Result<(String, Option<(Option<StopReason>, Usage)>)> {
    let req = LlmRequest {
        system,
        model,
        messages,
        tools: &[],
        generation,
    };
    let mut stream = llm.stream(req).await?;
    let mut text = String::new();
    let mut finish = None;
    while let Some(ev) = stream.next().await {
        match ev? {
            LlmEvent::Text(delta) => text.push_str(&delta),
            LlmEvent::Finish { stop_reason, usage } => finish = Some((stop_reason, usage)),
            _ => {}
        }
    }
    Ok((text, finish))
}

/// Rough token estimate for an arbitrary string, mirroring
/// `Context::estimated_tokens`'s `CHARS_PER_TOKEN` heuristic (3.5 chars/token).
fn estimate_tokens(text: &str) -> usize {
    let chars = text.chars().count();
    ((chars as f32) / 3.5).ceil() as usize
}

/// Render the history as a plain-text transcript for the summarization prompt.
/// Each `Tool`-role message beyond [`TRANSCRIPT_TOOL_MESSAGE_CAP`] chars is
/// truncated head+tail so one oversized tool output can't blow the
/// summarizer's own context window.
fn render_transcript(messages: &[Message]) -> String {
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

    #[test]
    fn compose_report_is_the_bare_summary_with_no_tail() {
        assert_eq!(
            compose_report("a dense summary", 0, None),
            "a dense summary"
        );
    }

    #[test]
    fn compose_report_appends_the_rendered_tail() {
        let report = compose_report("a dense summary", 2, Some("[user]\nsecond\n\n"));
        assert!(report.starts_with("a dense summary"));
        assert!(report.contains("2 most recent messages"));
        assert!(report.contains("[user]\nsecond"));
    }
}
