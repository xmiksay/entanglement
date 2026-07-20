//! Conversation context: the message history the engine sends to the LLM,
//! plus a conservative token estimate and per-model budget so the engine can
//! compact — and, failing that, refuse — an over-window turn instead of
//! shipping a request the provider will reject (#178).
//!
//! `Message`/`MessageRole` (the wire message shape) live in
//! `entanglement-provider` alongside the `Llm` request contract (ADR-0053);
//! this module owns the rolling history built on top of them.

use entanglement_provider::{ContentPart, Message, MessageRole, ToolCall};

/// Approximate tokens-per-char for the heuristic estimator.
///
/// Anthropic's exact tokenizer isn't readily available in Rust, so we use a
/// safe heuristic and keep the limit below the real ceiling.
const CHARS_PER_TOKEN: f32 = 3.5;

/// Conservative soft cap (in tokens) used when the active model's context
/// window is unknown (the `EchoLlm` stub, or an env-override model absent from
/// the catalog). Sits below a typical 200k hard ceiling; a known window scales
/// the budget instead (see [`Context::budget_for`]).
pub const CONTEXT_LIMIT_TOKENS: usize = 180_000;

/// Fraction of a model's context window the *input* history may occupy. The
/// remainder is headroom for the model's reply plus slack in the char/token
/// estimate, so we compact or refuse before the real request would overflow the
/// window at the provider.
const INPUT_BUDGET_FRACTION: f32 = 0.85;

/// Placeholder text a pruned tool output is collapsed to during compaction.
/// Kept short and stable so a re-compaction skips an already-pruned message.
const PRUNED_PLACEHOLDER: &str = "[tool output pruned to fit the context window]";

/// Owns the rolling conversation history, a token estimate, and the per-model
/// token budget the engine compacts/refuses against.
///
/// Compaction (`compact`) prunes the oldest tool outputs — the bulkiest, least
/// load-bearing history — to reclaim room; LLM summarization is a later phase.
#[derive(Debug)]
pub struct Context {
    messages: Vec<Message>,
    /// Token budget the history is kept under (`within_limit`). Derived from the
    /// active model's context window, or [`CONTEXT_LIMIT_TOKENS`] when unknown.
    limit: usize,
}

impl Default for Context {
    fn default() -> Self {
        Self {
            messages: Vec::new(),
            limit: CONTEXT_LIMIT_TOKENS,
        }
    }
}

impl Context {
    pub fn new() -> Self {
        Self::default()
    }

    /// New context whose token budget is derived from the active model's
    /// `context_window` (#178). `None` (unknown model) falls back to
    /// [`CONTEXT_LIMIT_TOKENS`].
    pub fn with_window(context_window: Option<usize>) -> Self {
        Self {
            messages: Vec::new(),
            limit: Self::budget_for(context_window),
        }
    }

    /// The input-history token budget for a model whose context window is
    /// `context_window`: [`INPUT_BUDGET_FRACTION`] of the window, reserving the
    /// remainder for the reply and estimator slack. Unknown → the flat fallback.
    fn budget_for(context_window: Option<usize>) -> usize {
        match context_window {
            Some(w) => ((w as f32) * INPUT_BUDGET_FRACTION) as usize,
            None => CONTEXT_LIMIT_TOKENS,
        }
    }

    /// The active token budget (`estimated_tokens` is kept at or below this).
    pub fn limit(&self) -> usize {
        self.limit
    }

    /// Re-budget the history against a new model's context window after a live
    /// model switch (#218): the compaction/refuse threshold must follow the model
    /// the session now runs under. `None` (unknown model) resets to the flat
    /// [`CONTEXT_LIMIT_TOKENS`] fallback. History is left intact — the next turn
    /// compacts against the new limit if it now overflows.
    pub fn set_window(&mut self, context_window: Option<usize>) {
        self.limit = Self::budget_for(context_window);
    }

    pub fn messages(&self) -> &[Message] {
        &self.messages
    }

    pub fn push(&mut self, message: Message) {
        tracing::debug!(
            role = ?message.role,
            text_len = message.text().len(),
            content_parts = message.content.len(),
            tool_calls = message.tool_calls.len(),
            tool_call_id = message.tool_call_id.as_deref(),
            "pushing message to context"
        );
        self.messages.push(message);
    }

    pub fn push_user(&mut self, text: impl Into<String>) {
        self.push(Message::user(text));
    }
    /// Push a user turn with explicit multimodal content (e.g. a screenshot
    /// prompt, #197).
    pub fn push_user_content(&mut self, content: Vec<ContentPart>) {
        self.push(Message::user_content(content));
    }
    pub fn push_assistant(&mut self, text: impl Into<String>, tool_calls: Vec<ToolCall>) {
        self.push(Message::assistant(text, tool_calls));
    }
    pub fn push_tool(&mut self, tool_call_id: impl Into<String>, text: impl Into<String>) {
        self.push(Message::tool(tool_call_id, text));
    }
    /// Push a tool result with explicit multimodal content — an image block when
    /// `read` opens an image file (#221). The construction paths never emit an
    /// empty text part, so an empty result yields empty content, matching
    /// [`push_tool`][Self::push_tool].
    pub fn push_tool_content(
        &mut self,
        tool_call_id: impl Into<String>,
        content: Vec<ContentPart>,
    ) {
        self.push(Message::tool_content(tool_call_id, content));
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
                m.text().chars().count()
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
        self.estimated_tokens() <= self.limit
    }

    /// Compact the history in place to fit the budget, returning whether it now
    /// fits. Strategy (#178): prune the oldest tool outputs — the bulkiest,
    /// least load-bearing history — to a short placeholder, newest preserved, and
    /// stop as soon as the estimate drops under the limit. Already-pruned outputs
    /// are skipped, so a repeat call is idempotent. LLM summarization of what
    /// survives is a later phase; when even fully-pruned history overflows (a
    /// single oversized message), this returns `false` and the caller refuses the
    /// turn rather than shipping an over-window request.
    ///
    /// Unlike [`Self::apply_compaction`], this mutation is **never recorded on
    /// the wire** — no caller emits an `OutEvent` for it (#450, ADR-0121).
    /// `Session::replay` therefore never replays a prune, so a resumed session
    /// can briefly see more history than the live session did after this ran;
    /// deliberate, since this function is a deterministic, idempotent
    /// re-derivation of "this transcript, this budget" that the resumed
    /// session's own next overflow check reproduces on its own — nothing an
    /// event would let replay reconstruct that the guard doesn't already.
    pub fn compact(&mut self) -> bool {
        // Prune oldest-first so recent tool results (the ones the model is
        // actively reasoning over) survive as long as possible.
        for i in 0..self.messages.len() {
            if self.within_limit() {
                break;
            }
            let msg = &mut self.messages[i];
            if msg.role == MessageRole::Tool && msg.text() != PRUNED_PLACEHOLDER {
                msg.content = vec![ContentPart::text(PRUNED_PLACEHOLDER)];
            }
        }
        self.within_limit()
    }

    /// Apply an LLM-generated compaction (#324, ADR-0082): replace the whole
    /// history with a single summary message, preserving the last `kept`
    /// messages verbatim after it. User role deliberately — `system` has no
    /// in-history wire mapping, and an assistant-authored summary is trusted
    /// less reliably by some providers than a user-authored one. `kept` is
    /// clamped to a safe turn boundary first (see [`Self::safe_kept`]), so a
    /// caller can never split a `Tool`/tool-call pair across the
    /// summary/tail boundary.
    pub fn apply_compaction(&mut self, summary: &str, kept: usize) {
        let kept = self.safe_kept(kept);
        let tail_start = self.messages.len().saturating_sub(kept);
        let tail = self.messages.split_off(tail_start);
        self.clear();
        self.push_user(format!(
            "[Conversation summary — earlier history was compacted]\n\n{summary}"
        ));
        self.messages.extend(tail);
    }

    /// Clamp a requested keep-tail count to the nearest safe turn boundary
    /// (#397): the tail must start at a `User`-role message — turns begin on a
    /// user prompt, so starting there guarantees every `Assistant`/`Tool`
    /// tool-call round-trip in the preceding turn stays fully inside the
    /// summarized head. Starting mid-turn (on a `Tool` reply, or an
    /// `Assistant` message that issued the tool call) would replay a `Tool`
    /// message without its paired half, breaking providers' `tool_use`/
    /// `tool_result` block-pairing requirement (ADR-0082's deferred-to-v1
    /// blocker). Walks forward from the naive split point to the next `User`
    /// message — preferring fewer kept messages over an unsafe boundary; no
    /// later `User` message means the tail collapses to empty (`0`).
    /// `requested_kept` at or beyond the whole history is returned unchanged
    /// (nothing is split, so there's no boundary to violate).
    pub fn safe_kept(&self, requested_kept: usize) -> usize {
        let len = self.messages.len();
        if requested_kept == 0 || requested_kept >= len {
            return requested_kept.min(len);
        }
        let requested_start = len - requested_kept;
        if self.messages[requested_start].role == MessageRole::User {
            return requested_kept;
        }
        let safe_start = (requested_start..len)
            .find(|&i| self.messages[i].role == MessageRole::User)
            .unwrap_or(len);
        len - safe_start
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

    #[test]
    fn window_scales_the_budget() {
        // 128k window → 85% input budget; unknown → flat fallback.
        let ctx = Context::with_window(Some(128_000));
        assert_eq!(ctx.limit(), (128_000f32 * INPUT_BUDGET_FRACTION) as usize);
        assert_eq!(Context::with_window(None).limit(), CONTEXT_LIMIT_TOKENS);
        assert_eq!(Context::new().limit(), CONTEXT_LIMIT_TOKENS);
    }

    #[test]
    fn compact_prunes_oldest_tool_outputs_until_it_fits() {
        // Tiny budget so a couple of tool outputs blow it.
        let mut ctx = Context::with_window(Some(100)); // limit = 85 tokens
        ctx.push_user("start");
        let big = "x".repeat(1000); // ~286 tokens each
        ctx.push_tool("a", big.clone());
        ctx.push_tool("b", big.clone());
        ctx.push_tool("c", "recent"); // small, most-recent
        assert!(!ctx.within_limit());

        assert!(
            ctx.compact(),
            "pruning the bulky outputs must bring it under budget"
        );
        assert!(ctx.within_limit());
        // Oldest outputs pruned, the small most-recent one preserved.
        assert_eq!(ctx.messages()[1].text(), PRUNED_PLACEHOLDER);
        assert_eq!(ctx.messages()[2].text(), PRUNED_PLACEHOLDER);
        assert_eq!(ctx.messages()[3].text(), "recent");
        // User text is never pruned.
        assert_eq!(ctx.messages()[0].text(), "start");
    }

    #[test]
    fn compact_refuses_when_a_single_message_overflows() {
        let mut ctx = Context::with_window(Some(100)); // limit = 85 tokens
        ctx.push_user("x".repeat(1000)); // one un-prunable oversized user turn
        assert!(
            !ctx.compact(),
            "no tool output to prune → still over budget"
        );
        assert!(!ctx.within_limit());
    }

    #[test]
    fn apply_compaction_replaces_history_with_a_user_role_summary() {
        let mut ctx = Context::new();
        ctx.push_user("do the thing");
        ctx.push_assistant("working on it", Vec::new());
        ctx.push_tool("t1", "tool output");

        ctx.apply_compaction("did the thing, files X and Y touched", 0);

        assert_eq!(ctx.messages().len(), 1);
        assert_eq!(ctx.messages()[0].role, MessageRole::User);
        assert!(ctx.messages()[0].text().contains("did the thing"));
        assert!(ctx.messages()[0]
            .text()
            .starts_with("[Conversation summary"));
    }

    #[test]
    fn apply_compaction_preserves_the_trailing_kept_messages() {
        let mut ctx = Context::new();
        ctx.push_user("first");
        ctx.push_assistant("second", Vec::new());
        ctx.push_user("third");

        ctx.apply_compaction("summary of the earlier turns", 1);

        assert_eq!(ctx.messages().len(), 2, "summary + the 1 kept tail message");
        assert_eq!(ctx.messages()[0].role, MessageRole::User);
        assert!(ctx.messages()[0]
            .text()
            .contains("summary of the earlier turns"));
        assert_eq!(ctx.messages()[1].text(), "third");
    }

    #[test]
    fn apply_compaction_kept_larger_than_history_keeps_everything_after_the_summary() {
        let mut ctx = Context::new();
        ctx.push_user("only message");

        ctx.apply_compaction("summary", 10);

        assert_eq!(ctx.messages().len(), 2);
        assert_eq!(ctx.messages()[1].text(), "only message");
    }

    #[test]
    fn safe_kept_clamps_a_mid_tool_round_split_to_the_next_user_message() {
        let mut ctx = Context::new();
        ctx.push_user("u1"); // 0
        ctx.push_assistant("a1", Vec::new()); // 1 — issues no tool call here
        ctx.push_tool("t1", "tool result"); // 2 — dangling half if split lands here
        ctx.push_assistant("a2", Vec::new()); // 3
        ctx.push_user("u2"); // 4

        // Naive split for kept=3 would start at index 2 (the `Tool` message) —
        // unsafe. Clamp forward to the next `User` message, index 4 (kept=1).
        assert_eq!(ctx.safe_kept(3), 1);

        ctx.apply_compaction("summary of the earlier turns", 3);
        assert_eq!(ctx.messages().len(), 2, "summary + the 1 safe tail message");
        assert_eq!(ctx.messages()[1].text(), "u2");
    }

    #[test]
    fn safe_kept_leaves_an_already_safe_boundary_untouched() {
        let mut ctx = Context::new();
        ctx.push_user("first");
        ctx.push_assistant("second", Vec::new());
        ctx.push_user("third");

        assert_eq!(ctx.safe_kept(1), 1, "index 2 is already a User message");
    }

    #[test]
    fn safe_kept_collapses_to_zero_when_no_later_user_message_exists() {
        let mut ctx = Context::new();
        ctx.push_user("u1");
        ctx.push_assistant("a1", Vec::new());
        ctx.push_tool("t1", "tool result");

        // Every candidate split from index 1 onward is non-User and there's no
        // later User message — the tail collapses to empty rather than risk a
        // dangling half.
        assert_eq!(ctx.safe_kept(2), 0);
    }

    #[test]
    fn safe_kept_kept_larger_than_history_is_unchanged() {
        let ctx = {
            let mut c = Context::new();
            c.push_user("only message");
            c
        };
        assert_eq!(ctx.safe_kept(10), 1);
    }

    #[test]
    fn compact_is_idempotent_and_a_noop_within_budget() {
        let mut ctx = Context::with_window(Some(128_000));
        ctx.push_tool("a", "small output");
        assert!(ctx.compact());
        // Nothing pruned when already within budget.
        assert_eq!(ctx.messages()[0].text(), "small output");
    }
}
