//! Streaming `<think>…</think>` splitter for the OpenAI-compat wire
//! (ADR-0191).
//!
//! A parser-less server serving a qwen3.5-class model inlines its thinking in
//! `delta.content` between `<think>`/`</think>` tags. Left alone that text
//! commits to history as if the model had said it aloud and replays verbatim
//! on every later request — the reported qwen3.5 breakage. When the catalog
//! declares [`ThinkingFormat::InlineTags`](crate::ThinkingFormat) for the
//! request's model, the OpenAI client routes those spans onto the reasoning
//! rail instead: the span's text streams as [`LlmEvent::Reasoning`] and, at
//! finish, the whole span is captured as one
//! [`ContentPart::Reasoning`](crate::ContentPart) block so it round-trips
//! under the same [`replay_thinking`](crate::ModelEntry::replay_thinking)
//! gate as every other captured block.
//!
//! The splitter is a byte-stream state machine because tags can straddle
//! network chunks (`content` deltas are fragments of one growing string):
//! `push` takes each delta, returns `(reasoning_delta, text_delta)`, and
//! `finish` flushes whatever remains open. Unbalanced input degrades
//! gracefully: an opening tag that never closes renders as pure reasoning
//! (the model never said anything aloud), and a stray `</think>` with no
//! opener is passed through as text — some qwen3.5 builds emit it
//! un-negotiated on the first token.

use crate::ContentPart;

/// The opening/closing tags this splitter recognizes.
const OPEN: &str = "<think>";
const CLOSE: &str = "</think>";

#[derive(Debug, Default)]
pub(super) struct ThinkSplitter {
    /// Buffered bytes that may be a partial tag straddling the chunk boundary.
    pending: String,
    /// Inside a `<think>…</think>` span.
    in_think: bool,
    /// Everything routed to the reasoning rail this stream — both for the
    /// flush-at-finish and for deciding whether a block is worth capturing.
    reasoning: String,
}

impl ThinkSplitter {
    pub(super) fn new() -> Self {
        Self::default()
    }

    /// Feed one `content` delta; returns the `(reasoning, text)` fragments
    /// that belong on each rail.
    pub(super) fn push(&mut self, delta: &str) -> (String, String) {
        self.pending.push_str(delta);
        let mut reasoning = String::new();
        let mut text = String::new();
        // The loop terminates: every iteration either consumes `pending`
        // entirely or strips at least one full tag out of it.
        loop {
            let hay = self.pending.as_str();
            if self.in_think {
                match hay.find(CLOSE) {
                    Some(at) => {
                        self.reasoning.push_str(&hay[..at]);
                        reasoning.push_str(&hay[..at]);
                        self.pending.replace_range(..at + CLOSE.len(), "");
                        self.in_think = false;
                    }
                    None => {
                        // No closer yet. Everything except a possible partial
                        // closer suffix is reasoning; hold back the longest
                        // prefix of CLOSE that the buffer ends with.
                        let keep = partial_suffix_len(hay, CLOSE);
                        let flush = hay.len() - keep;
                        self.reasoning.push_str(&hay[..flush]);
                        reasoning.push_str(&hay[..flush]);
                        let tail = hay[flush..].to_string();
                        self.pending = tail;
                        break;
                    }
                }
            } else {
                match hay.find(OPEN) {
                    Some(at) => {
                        text.push_str(&hay[..at]);
                        self.pending.replace_range(..at + OPEN.len(), "");
                        self.in_think = true;
                    }
                    None => {
                        // No opener yet. Everything except a possible partial
                        // opener suffix is text.
                        let keep = partial_suffix_len(hay, OPEN);
                        let flush = hay.len() - keep;
                        text.push_str(&hay[..flush]);
                        let tail = hay[flush..].to_string();
                        self.pending = tail;
                        break;
                    }
                }
            }
        }
        (reasoning, text)
    }

    /// End of stream: flush any buffer left held back for tag detection. A
    /// still-open think span degrades to reasoning (the model never said
    /// anything aloud); a held-back partial tag degrades to whichever rail is
    /// active, matching what a subsequent `</think>`-first-thing build does.
    pub(super) fn finish(&mut self) -> (String, String) {
        let leftover = std::mem::take(&mut self.pending);
        let mut reasoning = String::new();
        let mut text = String::new();
        if self.in_think {
            reasoning.push_str(&leftover);
            self.reasoning.push_str(&leftover);
        } else {
            text.push_str(&leftover);
        }
        (reasoning, text)
    }

    /// The captured block for the round's `content_blocks` — `None` when no
    /// reasoning was seen. `data` is `{"format":"inline_tags"}`: there is no
    /// signature to preserve (this is plain text the model itself emitted),
    /// but the block needs a non-empty payload so downstream replay code can
    /// distinguish it from a malformed one.
    pub(super) fn into_reasoning_block(self, model: &str) -> Option<ContentPart> {
        if self.reasoning.is_empty() {
            return None;
        }
        Some(ContentPart::reasoning(
            "openai",
            self.reasoning,
            serde_json::json!({ "format": "inline_tags", "model": model }),
        ))
    }
}

/// Length of the longest proper suffix of `hay` that is a prefix of `tag`
/// (0 when none) — the bytes that might be the start of `tag` straddling the
/// chunk boundary, held back until more arrive.
fn partial_suffix_len(hay: &str, tag: &str) -> usize {
    let max = hay.len().min(tag.len() - 1);
    (0..=max)
        .rev()
        .find(|&k| hay.ends_with(&tag[..k]))
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn split_all(deltas: &[&str]) -> (String, String, Option<ContentPart>) {
        let mut s = ThinkSplitter::new();
        let mut reasoning = String::new();
        let mut text = String::new();
        for d in deltas {
            let (r, t) = s.push(d);
            reasoning.push_str(&r);
            text.push_str(&t);
        }
        let (r, t) = s.finish();
        reasoning.push_str(&r);
        text.push_str(&t);
        (reasoning, text, s.into_reasoning_block("qwen3.5"))
    }

    #[test]
    fn plain_text_passes_through_untouched() {
        let (r, t, block) = split_all(&["hello ", "world"]);
        assert_eq!(r, "");
        assert_eq!(t, "hello world");
        assert!(block.is_none());
    }

    #[test]
    fn think_span_routes_to_reasoning_rail() {
        let (r, t, block) = split_all(&["<think>plan</think>", "answer"]);
        assert_eq!(r, "plan");
        assert_eq!(t, "answer");
        let ContentPart::Reasoning { text, .. } = block.unwrap() else {
            panic!("expected a reasoning block");
        };
        assert_eq!(text, "plan");
    }

    #[test]
    fn tag_split_across_chunks_is_reassembled() {
        // The pathological case: every boundary position.
        for cut in 0..="<think>x</think>".len() {
            let whole = "<think>x</think>";
            let (r, t, _) = split_all(&[&whole[..cut], &whole[cut..]]);
            assert_eq!(r, "x", "cut at {cut}");
            assert_eq!(t, "", "cut at {cut}");
        }
    }

    #[test]
    fn tag_split_inside_the_word_think() {
        let (r, t, _) = split_all(&["<thi", "nk>refle", "ct</th", "ink>done"]);
        assert_eq!(r, "reflect");
        assert_eq!(t, "done");
    }

    #[test]
    fn leading_think_with_no_text_before() {
        // qwen3.5 opens with the tag as the very first token.
        let (r, t, _) = split_all(&["<think>", "step ", "one</think>", "Hi!"]);
        assert_eq!(r, "step one");
        assert_eq!(t, "Hi!");
    }

    #[test]
    fn multiple_spans_across_the_round() {
        let (r, t, _) = split_all(&["a<think>b</think>c<think>d</think>e"]);
        assert_eq!(r, "bd");
        assert_eq!(t, "ace");
    }

    #[test]
    fn unterminated_span_degrades_to_reasoning() {
        // Stream cut mid-thought: everything after the opener is reasoning,
        // nothing leaks into text.
        let (r, t, _) = split_all(&["<think>half-way "]);
        assert_eq!(r, "half-way ");
        assert_eq!(t, "");
    }

    #[test]
    fn stray_closer_is_kept_as_text() {
        // Some builds emit `</think>` before any opener; dropping it would
        // silently eat model output, so it passes through verbatim.
        let (r, t, _) = split_all(&["</think>hello"]);
        assert_eq!(r, "");
        assert_eq!(t, "</think>hello");
    }

    #[test]
    fn held_back_partial_opener_flushes_as_text() {
        // A trailing "<thi" at stream end is model output, not a tag — it must
        // not be swallowed.
        let (r, t, _) = split_all(&["hello <thi"]);
        assert_eq!(r, "");
        assert_eq!(t, "hello <thi");
    }

    #[test]
    fn held_back_partial_closer_flushes_as_reasoning() {
        let (r, t, _) = split_all(&["<think>musing</th"]);
        assert_eq!(r, "musing</th");
        assert_eq!(t, "");
    }

    #[test]
    fn empty_deltas_are_inert() {
        let (r, t, block) = split_all(&["", ""]);
        assert_eq!((r.as_str(), t.as_str()), ("", ""));
        assert!(block.is_none());
    }

    #[test]
    fn multi_byte_utf8_boundaries_are_preserved() {
        // `…` and emoji must survive arbitrary chunk splits (the #443 lesson).
        let whole = "<think>思考…🤔</think>答え";
        for cut in 1..whole.len() {
            // Only test valid char boundaries.
            if !whole.is_char_boundary(cut) {
                continue;
            }
            let (r, t, _) = split_all(&[&whole[..cut], &whole[cut..]]);
            assert_eq!(r, "思考…🤔", "cut at {cut}");
            assert_eq!(t, "答え", "cut at {cut}");
        }
    }
}
