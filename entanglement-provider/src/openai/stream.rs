//! Decoding one Chat Completions SSE response body into [`LlmEvent`]s: the
//! frame loop, the end-of-stream flushes (trailing frame, inline-think
//! splitter, fields-format reasoning capture, validated tool calls) and the
//! normalized `Finish`. Split out of `mod.rs` (#745) so the client there
//! keeps only request setup + transport.

use std::collections::BTreeMap;

use async_stream::try_stream;
use futures::StreamExt;
use tokio::sync::mpsc;

use super::sse::{
    drain_available_frames, flush_pending_tools, handle_chunk, note_finish_reason, parse_sse_line,
    SseEvent,
};
use super::think::ThinkSplitter;
use super::PendingTool;
use crate::{ContentPart, LlmEvent, LlmStream, StopReason, Usage};

/// Turn the watchdogged byte channel of one response into the event stream.
/// `think` is `Some` exactly for an inline-tags model (ADR-0191); the
/// captured reasoning block is stamped with `stream_model`.
pub(super) fn decode(
    rx: mpsc::Receiver<anyhow::Result<Vec<u8>>>,
    think: Option<ThinkSplitter>,
    stream_model: String,
) -> LlmStream {
    let stream = try_stream! {
        // Byte-buffered framing (#443): a multi-byte UTF-8 character can
        // straddle two network chunks, so decoding must wait for a complete
        // `\n`-terminated frame — see `sse_frame::SseFrameBuffer`.
        let mut frames = crate::sse_frame::SseFrameBuffer::new(b"\n");
        let mut tools: BTreeMap<u32, PendingTool> = BTreeMap::new();
        let mut usage = Usage::default();
        let mut seen_finish_reason: Option<String> = None;
        let mut rx = rx;
        let mut saw_done = false;
        let mut think = think;
        // Fields-format reasoning capture (ADR-0200): `think.is_none()` is
        // exactly the `ThinkingFormat::Fields` case (see `think`'s
        // construction above) — accumulate every `LlmEvent::Reasoning`
        // this round so it can be minted into a `ContentPart::Reasoning`
        // block at stream end, mirroring what `ThinkSplitter` already does
        // for the inline-tags format. Left empty (and so never emitted)
        // for `InlineTags`, whose own capture path is the block below.
        let mut fields_reasoning = String::new();

        'outer: while let Some(item) = rx.recv().await {
            let chunk = item?;
            frames.push(&chunk);
            let (events, done) = drain_available_frames(
                &mut frames,
                &mut tools,
                &mut usage,
                &mut seen_finish_reason,
                think.as_mut(),
            )?;
            for ev in events {
                if think.is_none() {
                    if let LlmEvent::Reasoning(text) = &ev {
                        fields_reasoning.push_str(text);
                    }
                }
                yield ev;
            }
            if done {
                // Protocol-correct terminator (#483): stop reading immediately,
                // ignoring anything the endpoint sends afterward instead of
                // relying on the connection to close.
                saw_done = true;
                break 'outer;
            }
        }
        // Flush a final unterminated frame at EOF (#483): a stream cut mid-frame,
        // or a server that omits the trailing delimiter on its last event, would
        // otherwise silently drop the closing chunk — which can carry
        // `finish_reason`, the difference between a confident stop and an
        // ambiguous-stop retry (ADR-0118). Skipped once `[DONE]` was already
        // seen: whatever is still buffered after it is protocol garbage, not an
        // event to parse.
        if !saw_done {
            if let Some(trailing) = frames.take_remaining() {
                if let SseEvent::Data(data) = parse_sse_line(&trailing) {
                    note_finish_reason(&data, &mut seen_finish_reason);
                    for ev in handle_chunk(&data, &mut tools, &mut usage, think.as_mut())? {
                        if think.is_none() {
                            if let LlmEvent::Reasoning(text) = &ev {
                                fields_reasoning.push_str(text);
                            }
                        }
                        yield ev;
                    }
                }
            }
        }
        // Inline-think-tags flush (ADR-0191): everything the splitter still
        // holds back for tag detection is model output and must not be
        // swallowed, and a non-empty think span mints the round's persisted
        // `Reasoning` content block — the capture side of the same flag that
        // gates replay on the request body (below in `build_body`).
        if let Some(mut splitter) = think.take() {
            let (reasoning, spoken) = splitter.finish();
            if !reasoning.is_empty() {
                yield LlmEvent::Reasoning(reasoning);
            }
            if !spoken.is_empty() {
                yield LlmEvent::Text(spoken);
            }
            if let Some(block) = splitter.into_reasoning_block(&stream_model) {
                yield LlmEvent::ContentBlock(block);
            }
        }
        // Fields-format capture (ADR-0200): mint the round's persisted
        // `Reasoning` block from whatever `reasoning_content`/`reasoning`
        // deltas were accumulated above — unconditional capture, replay
        // gated separately by `thinking.replay` in `request.rs`. Mirrors
        // `ThinkSplitter::into_reasoning_block`'s shape; `data` carries no
        // signature (plain text the model emitted) but stays non-empty so
        // replay code can tell it apart from a malformed block.
        if !fields_reasoning.is_empty() {
            yield LlmEvent::ContentBlock(ContentPart::reasoning(
                "openai",
                fields_reasoning,
                serde_json::json!({ "format": "fields", "model": stream_model }),
            ));
        }
        // Post-#445 `handle_chunk` no longer flushes eagerly, so a non-empty
        // `tools` map at stream end is the **normal** tool-use path, not an
        // anomaly — kept only to detect genuine data loss after the flush.
        let had_pending_tools = !tools.is_empty();
        // Single validating flush site (#445) for both the explicit
        // `finish_reason == "tool_calls"` case and the no-finish-reason
        // fallback — `tools` still holds everything assembled across the
        // whole stream here.
        let mut flushed = Vec::new();
        let emitted_any_tool_call = flush_pending_tools(&mut tools, &mut flushed);
        for ev in flushed {
            yield ev;
        }
        // Warn only on real data loss: tool calls were assembled but *every*
        // one had malformed/incomplete JSON arguments (e.g. a stream
        // truncated mid-arguments), so nothing valid was emitted. The normal
        // path — valid calls flushed just above — stays silent (this used to
        // warn on every tool-use turn, since #445 defers the flush to here).
        if had_pending_tools && !emitted_any_tool_call {
            tracing::warn!(
                finish_reason = seen_finish_reason.as_deref().unwrap_or("none"),
                "stream ended with tool calls whose arguments were incomplete - dropped (likely a truncated stream)"
            );
        }
        // A tool-flush with no valid call actually emitted (every pending
        // tool had malformed JSON args, or the stream ended without one)
        // must not report a confident-looking `ToolUse` stop with zero
        // `LlmEvent::ToolCall`s behind it — that contradiction left the
        // turn loop unable to tell a genuine tool-use stop from an
        // ambiguous one (ADR-0118). This applies equally whether
        // `finish_reason` was the explicit `"tool_calls"` or absent.
        let stop_reason = match seen_finish_reason.as_deref() {
            Some("tool_calls") if !emitted_any_tool_call => None,
            Some(r) => Some(StopReason::from_openai(r)),
            None if emitted_any_tool_call => Some(StopReason::ToolUse),
            None => None,
        };
        yield LlmEvent::Finish { stop_reason, usage };
    };
    stream.boxed()
}
