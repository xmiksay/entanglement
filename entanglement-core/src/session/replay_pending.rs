//! The replay fold's turn state machine: rebuilds `Context` the way the live
//! engine commits it — one assistant message per model round
//! (`session/round.rs`), each tool result on arrival (`session.rs`), a
//! mid-turn prompt at the start of the next round (ADR-0058) — so a resumed
//! session sends the exact history the live one would have (ADR-0202). Split
//! out of `replay.rs` (400-line cap); every commit point the fold reconstructs
//! funnels through here so none can drift from the live commit.

use std::collections::HashMap;
use std::mem::take;

use super::invoke_envelope::emitted_call;
use super::TurnState;
use crate::context::Context;
use crate::protocol::ToolEnvelope;
use entanglement_provider::{ContentPart, Message, ToolCall};

#[derive(Default)]
pub(super) struct TurnFold {
    /// The current round's streamed, uncommitted text — committed ahead of
    /// its persisted search/reasoning `blocks`, as live `content_blocks` are.
    text: String,
    blocks: Vec<ContentPart>,
    /// The current round's calls in dispatch form, plus the envelopes of the
    /// unwrapped `invoke` calls among them (ADR-0204).
    calls: Vec<ToolCall>,
    envelopes: HashMap<String, ToolEnvelope>,
    /// A round event arrived since the last commit point, so the live engine
    /// had already folded its stashed prompts (the fold precedes the stream).
    round_started: bool,
    /// A `Stop` for this session was logged mid-turn: the next resting
    /// `Status` is its cancel, even before any round event streamed.
    stop_requested: bool,
    /// The turn's last committed tool batch, `pending` drained as outputs
    /// land: what resume re-offers, or continues once drained.
    batch: Option<TurnState>,
    /// Mirrors live `Session::turn.is_some()`: a prompt arriving meanwhile is
    /// stashed, not pushed.
    in_turn: bool,
    stashed: Vec<Vec<ContentPart>>,
}

impl TurnFold {
    pub(super) fn prompt(&mut self, ctx: &mut Context, content: Vec<ContentPart>) {
        if self.in_turn {
            self.stashed.push(content);
        } else {
            ctx.push_user_content(content);
            self.in_turn = true;
        }
    }

    /// Any event a model round streams: its stash fold already happened.
    pub(super) fn round_event(&mut self, ctx: &mut Context) {
        if !self.round_started {
            self.round_started = true;
            self.in_turn = true;
            self.fold_stash(ctx);
        }
    }

    pub(super) fn push_text(&mut self, ctx: &mut Context, text: &str) {
        self.round_event(ctx);
        self.text.push_str(text);
    }

    pub(super) fn push_block(&mut self, ctx: &mut Context, part: ContentPart) {
        self.round_event(ctx);
        self.blocks.push(part);
    }

    pub(super) fn push_call(
        &mut self,
        ctx: &mut Context,
        call: ToolCall,
        envelope: Option<&ToolEnvelope>,
    ) {
        self.round_event(ctx);
        if let Some(env) = envelope {
            self.envelopes.insert(call.id.clone(), env.clone());
        }
        self.calls.push(call);
    }

    /// A logged `Stop` can precede the events the session emits before
    /// seeing it, so it only arms the cancel; an idle `Stop` is a no-op live.
    pub(super) fn stop(&mut self) {
        if self.in_turn {
            self.stop_requested = true;
        }
    }

    /// Live pushes each result the moment it resolves — after the batch's
    /// assistant message, before the next round.
    pub(super) fn tool_output(&mut self, ctx: &mut Context, id: &str, parts: Vec<ContentPart>) {
        self.commit(ctx);
        if let Some(batch) = self.batch.as_mut() {
            batch.resolve(id);
        }
        ctx.push_tool_content(id, parts);
    }

    /// ADR-0118: the partial round commits (skipped when empty), then the
    /// nudge. An event-less round still folded the stash before streaming.
    pub(super) fn ambiguous_retry(&mut self, ctx: &mut Context, nudge: &str) {
        self.round_event(ctx);
        self.commit(ctx);
        self.batch = None;
        ctx.push_user(nudge);
    }

    pub(super) fn done(&mut self, ctx: &mut Context) {
        self.commit(ctx);
        self.end_turn(ctx);
    }

    /// A resting `Status` (`Done`, or `Paused` for a paused session) with no
    /// `Done` before it: a `Stop` cancelled the turn. Without a logged `Stop`
    /// (an embedder log), an open round or batch still proves it — never a
    /// trailing `Status` after `Done`, which an early-logged prompt can
    /// precede. The live engine never committed the interrupted stream, but
    /// an emitted batch was committed before its calls went out.
    pub(super) fn cancelled(&mut self, ctx: &mut Context, paused: bool) {
        let open = self.round_started || self.batch.is_some();
        if !(self.stop_requested || (!paused && open)) {
            return;
        }
        self.drop_uncommitted_stream();
        self.commit(ctx);
        self.end_turn(ctx);
    }

    /// `SessionHibernated`: the live session dropped its in-flight stream and
    /// stash, and a resume continues from the parked batch, if any.
    pub(super) fn hibernated(&mut self, ctx: &mut Context) {
        self.drop_uncommitted_stream();
        self.commit(ctx);
        self.stashed.clear();
        self.stop_requested = false;
        self.in_turn = self.batch.is_some();
    }

    /// End of the log (#271, ADR-0061): the turn resume re-offers from —
    /// unresolved calls in dispatch form with their envelopes, or a drained
    /// batch whose next round never streamed. A text-only tail is a
    /// mid-stream crash and stays dropped, like the live engine drops it.
    /// `iterations` restarts at 0: `max_turns` is a runaway guard, not a quota.
    pub(super) fn into_parked_turn(mut self, ctx: &mut Context) -> Option<TurnState> {
        self.hibernated(ctx);
        self.batch
    }

    fn is_empty(&self) -> bool {
        self.text.is_empty() && self.blocks.is_empty() && self.calls.is_empty()
    }

    /// `ToolCall`s are emitted only after the live commit, so a round with
    /// calls is kept; one without was still streaming.
    fn drop_uncommitted_stream(&mut self) {
        if self.calls.is_empty() {
            self.text.clear();
            self.blocks.clear();
        }
    }

    fn commit(&mut self, ctx: &mut Context) {
        self.round_started = false;
        if self.is_empty() {
            return;
        }
        let mut content = Vec::new();
        if !self.text.is_empty() {
            content.push(ContentPart::text(take(&mut self.text)));
        }
        content.append(&mut self.blocks);
        let emitted = self
            .calls
            .iter()
            .map(|c| emitted_call(c, self.envelopes.get(&c.id)))
            .collect();
        ctx.push(Message::assistant_content(content, emitted));
        if !self.calls.is_empty() {
            self.batch = Some(TurnState {
                pending: take(&mut self.calls),
                envelopes: take(&mut self.envelopes),
                ..TurnState::default()
            });
        }
    }

    /// The live loop pops its stash once idle: the first prompt starts the
    /// next turn and the rest fold into that turn's first round.
    fn end_turn(&mut self, ctx: &mut Context) {
        self.batch = None;
        self.stop_requested = false;
        self.in_turn = !self.stashed.is_empty();
        self.fold_stash(ctx);
    }

    fn fold_stash(&mut self, ctx: &mut Context) {
        for content in self.stashed.drain(..) {
            ctx.push_user_content(content);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use entanglement_provider::MessageRole;

    fn call(id: &str) -> ToolCall {
        ToolCall::new(id, "read", "{}")
    }

    fn shape(ctx: &Context) -> Vec<String> {
        ctx.messages()
            .iter()
            .map(|m| match m.role {
                MessageRole::Tool => format!("tool:{}", m.tool_call_id.clone().unwrap_or_default()),
                role => format!("{role:?}:{}:{}", m.text(), m.tool_calls.len()),
            })
            .collect()
    }

    fn prompt(fold: &mut TurnFold, ctx: &mut Context, text: &str) {
        fold.prompt(ctx, vec![ContentPart::text(text)]);
    }

    #[test]
    fn a_tool_round_and_the_text_round_after_it_stay_separate_messages() {
        let (mut fold, mut ctx) = (TurnFold::default(), Context::new());
        prompt(&mut fold, &mut ctx, "go");
        fold.push_text(&mut ctx, "reading");
        fold.push_call(&mut ctx, call("c1"), None);
        fold.tool_output(&mut ctx, "c1", vec![ContentPart::text("out")]);
        fold.push_text(&mut ctx, "done");
        fold.done(&mut ctx);
        assert_eq!(
            shape(&ctx),
            [
                "User:go:0",
                "Assistant:reading:1",
                "tool:c1",
                "Assistant:done:0"
            ]
        );
    }

    #[test]
    fn a_prompt_sent_mid_turn_folds_at_the_next_round() {
        let (mut fold, mut ctx) = (TurnFold::default(), Context::new());
        prompt(&mut fold, &mut ctx, "go");
        fold.push_call(&mut ctx, call("c1"), None);
        prompt(&mut fold, &mut ctx, "steer");
        fold.tool_output(&mut ctx, "c1", Vec::new());
        fold.push_text(&mut ctx, "ok");
        prompt(&mut fold, &mut ctx, "after");
        fold.done(&mut ctx);
        assert_eq!(
            shape(&ctx),
            [
                "User:go:0",
                "Assistant::1",
                "tool:c1",
                "User:steer:0",
                "Assistant:ok:0",
                "User:after:0"
            ]
        );
        assert!(fold.in_turn, "the popped stash starts the next turn");
    }

    #[test]
    fn a_cancel_drops_the_uncommitted_stream_but_keeps_an_emitted_batch() {
        let (mut fold, mut ctx) = (TurnFold::default(), Context::new());
        prompt(&mut fold, &mut ctx, "go");
        fold.push_text(&mut ctx, "partial");
        fold.cancelled(&mut ctx, false);
        assert_eq!(shape(&ctx), ["User:go:0"]);

        prompt(&mut fold, &mut ctx, "again");
        fold.push_call(&mut ctx, call("c1"), None);
        fold.cancelled(&mut ctx, false);
        assert_eq!(shape(&ctx), ["User:go:0", "User:again:0", "Assistant::1"]);
        assert!(fold.into_parked_turn(&mut ctx).is_none());
    }

    #[test]
    fn a_logged_stop_ends_a_turn_that_never_streamed() {
        let (mut fold, mut ctx) = (TurnFold::default(), Context::new());
        prompt(&mut fold, &mut ctx, "go");
        fold.cancelled(&mut ctx, false);
        assert!(fold.in_turn, "no stop, nothing open: not a cancel");
        fold.stop();
        fold.cancelled(&mut ctx, false);
        assert!(!fold.in_turn);
        prompt(&mut fold, &mut ctx, "two");
        assert_eq!(shape(&ctx), ["User:go:0", "User:two:0"]);
    }

    #[test]
    fn a_pause_is_a_cancel_only_after_a_stop() {
        let (mut fold, mut ctx) = (TurnFold::default(), Context::new());
        prompt(&mut fold, &mut ctx, "go");
        fold.push_call(&mut ctx, call("c1"), None);
        fold.tool_output(&mut ctx, "c1", Vec::new());
        fold.cancelled(&mut ctx, true);
        assert!(fold.batch.is_some(), "a bare pause keeps the turn");
        fold.stop();
        fold.cancelled(&mut ctx, true);
        assert!(fold.batch.is_none() && !fold.in_turn);
    }

    #[test]
    fn an_idle_stop_arms_nothing() {
        let mut fold = TurnFold::default();
        fold.stop();
        assert!(!fold.stop_requested);
    }

    #[test]
    fn a_trailing_status_after_done_is_not_a_cancel() {
        let (mut fold, mut ctx) = (TurnFold::default(), Context::new());
        prompt(&mut fold, &mut ctx, "go");
        fold.push_text(&mut ctx, "hi");
        fold.done(&mut ctx);
        prompt(&mut fold, &mut ctx, "next");
        fold.cancelled(&mut ctx, false);
        assert!(fold.in_turn);
        assert_eq!(shape(&ctx), ["User:go:0", "Assistant:hi:0", "User:next:0"]);
    }
}
