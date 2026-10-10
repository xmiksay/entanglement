//! The aggregate, non-streamed shape of one model round-trip: what
//! [`Llm::complete`](crate::Llm::complete) drains a stream into, and what
//! [`stream_from_response`] replays as a stream for scripted backends.

use crate::{ContentPart, LlmEvent, LlmStream, StopReason, ToolCall, Usage};
use futures::{stream, StreamExt};

/// A fully-formed model reply — every [`LlmEvent`] of one round-trip folded
/// together. `ToolCallDelta` fragments are not kept: the assembled
/// [`ToolCall`]s they rebuild are.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct LlmResponse {
    /// Concatenated [`LlmEvent::Text`] chunks.
    pub text: String,
    /// Assembled tool calls, in arrival order.
    pub tool_calls: Vec<ToolCall>,
    /// Concatenated [`LlmEvent::Reasoning`] chunks (display text only — the
    /// replayable signed thinking block, when a provider mints one, arrives in
    /// `content_blocks`).
    pub reasoning: String,
    /// Server-minted content blocks ([`LlmEvent::ContentBlock`]), in arrival
    /// order.
    pub content_blocks: Vec<ContentPart>,
    /// The `Finish` event's stop reason; `None` when the provider reported
    /// none or the stream closed without a `Finish`.
    pub stop_reason: Option<StopReason>,
    /// The `Finish` event's usage; all-`None` when no `Finish` arrived.
    pub usage: Usage,
}

impl LlmResponse {
    /// Fold one event in — the same accumulation the turn loop performs,
    /// minus its live display.
    pub fn absorb(&mut self, event: LlmEvent) {
        match event {
            LlmEvent::Text(delta) => self.text.push_str(&delta),
            LlmEvent::Reasoning(delta) => self.reasoning.push_str(&delta),
            LlmEvent::ToolCallDelta { .. } => {}
            LlmEvent::ToolCall(call) => self.tool_calls.push(call),
            LlmEvent::ContentBlock(part) => self.content_blocks.push(part),
            LlmEvent::Finish { stop_reason, usage } => {
                self.stop_reason = stop_reason;
                self.usage = usage;
            }
        }
    }
}

/// Drain `stream` into one [`LlmResponse`], failing on the first `Err` item.
pub async fn collect(mut stream: LlmStream) -> anyhow::Result<LlmResponse> {
    let mut out = LlmResponse::default();
    while let Some(event) = stream.next().await {
        out.absorb(event?);
    }
    Ok(out)
}

/// Build a one-shot stream from a full [`LlmResponse`] (reasoning, text, tool
/// calls, content blocks, then `Finish`). Convenience for scripted/test
/// backends. A `None` stop reason is derived from the tool calls (`ToolUse`
/// when there are any, else `EndTurn`) so a bare `{ text, tool_calls }` script
/// reads as a confident stop.
pub fn stream_from_response(resp: LlmResponse) -> LlmStream {
    let stop_reason = resp.stop_reason.unwrap_or(if resp.tool_calls.is_empty() {
        StopReason::EndTurn
    } else {
        StopReason::ToolUse
    });
    let mut events: Vec<anyhow::Result<LlmEvent>> = Vec::new();
    if !resp.reasoning.is_empty() {
        events.push(Ok(LlmEvent::Reasoning(resp.reasoning)));
    }
    if !resp.text.is_empty() {
        events.push(Ok(LlmEvent::Text(resp.text)));
    }
    events.extend(
        resp.tool_calls
            .into_iter()
            .map(|c| Ok(LlmEvent::ToolCall(c))),
    );
    events.extend(
        resp.content_blocks
            .into_iter()
            .map(|p| Ok(LlmEvent::ContentBlock(p))),
    );
    events.push(Ok(LlmEvent::Finish {
        stop_reason: Some(stop_reason),
        usage: resp.usage,
    }));
    stream::iter(events).boxed()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Llm, LlmRequest};
    use async_trait::async_trait;

    /// A fake backend replaying a fixed event script.
    struct Scripted(Vec<anyhow::Result<LlmEvent>>);

    #[async_trait]
    impl Llm for Scripted {
        async fn stream(&mut self, _req: LlmRequest<'_>) -> anyhow::Result<LlmStream> {
            Ok(stream::iter(std::mem::take(&mut self.0)).boxed())
        }
    }

    fn req() -> LlmRequest<'static> {
        LlmRequest {
            system: "",
            model: None,
            messages: &[],
            tools: &[],
            generation: None,
            cache_key: None,
            retry: None,
            trailing_notice: None,
        }
    }

    fn usage() -> Usage {
        Usage {
            input_tokens: Some(7),
            output_tokens: Some(3),
            cached_input_tokens: None,
            cache_write_tokens: None,
        }
    }

    #[tokio::test]
    async fn complete_folds_every_event_kind() {
        let call = ToolCall::new("c1", "read", r#"{"path":"a"}"#);
        let block = ContentPart::Text {
            text: "minted".into(),
        };
        let mut llm = Scripted(vec![
            Ok(LlmEvent::Reasoning("think ".into())),
            Ok(LlmEvent::Text("hel".into())),
            Ok(LlmEvent::Reasoning("more".into())),
            Ok(LlmEvent::Text("lo".into())),
            Ok(LlmEvent::ToolCallDelta {
                id: "c1".into(),
                name: "read".into(),
                delta: "{".into(),
            }),
            Ok(LlmEvent::ToolCall(call.clone())),
            Ok(LlmEvent::ContentBlock(block.clone())),
            Ok(LlmEvent::Finish {
                stop_reason: Some(StopReason::ToolUse),
                usage: usage(),
            }),
        ]);
        let resp = llm.complete(req()).await.expect("completes");
        assert_eq!(
            resp,
            LlmResponse {
                text: "hello".into(),
                tool_calls: vec![call],
                reasoning: "think more".into(),
                content_blocks: vec![block],
                stop_reason: Some(StopReason::ToolUse),
                usage: usage(),
            }
        );
    }

    #[tokio::test]
    async fn complete_fails_on_a_mid_stream_error() {
        let mut llm = Scripted(vec![
            Ok(LlmEvent::Text("partial".into())),
            Err(anyhow::anyhow!("connection reset")),
        ]);
        let err = llm.complete(req()).await.expect_err("mid-stream error");
        assert!(err.to_string().contains("connection reset"));
    }

    #[tokio::test]
    async fn complete_without_finish_leaves_stop_reason_unknown() {
        let mut llm = Scripted(vec![Ok(LlmEvent::Text("cut".into()))]);
        let resp = llm.complete(req()).await.expect("completes");
        assert_eq!(resp.text, "cut");
        assert_eq!(resp.stop_reason, None);
        assert_eq!(resp.usage, Usage::default());
    }

    #[tokio::test]
    async fn stream_from_response_round_trips_through_collect() {
        let resp = LlmResponse {
            text: "t".into(),
            tool_calls: vec![ToolCall::new("c", "glob", "{}")],
            reasoning: "r".into(),
            content_blocks: vec![ContentPart::Text { text: "b".into() }],
            stop_reason: Some(StopReason::ToolUse),
            usage: usage(),
        };
        let back = collect(stream_from_response(resp.clone())).await.unwrap();
        assert_eq!(back, resp);
    }

    #[tokio::test]
    async fn stream_from_response_derives_a_missing_stop_reason() {
        let text_only = LlmResponse {
            text: "hi".into(),
            ..Default::default()
        };
        let back = collect(stream_from_response(text_only)).await.unwrap();
        assert_eq!(back.stop_reason, Some(StopReason::EndTurn));
        let with_call = LlmResponse {
            tool_calls: vec![ToolCall::new("c", "glob", "{}")],
            ..Default::default()
        };
        let back = collect(stream_from_response(with_call)).await.unwrap();
        assert_eq!(back.stop_reason, Some(StopReason::ToolUse));
    }
}
