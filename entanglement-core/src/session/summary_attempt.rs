//! Running one compaction summarization (ADR-0202 §4): drain the reply to
//! text, and when the structured shape's reply calls a tool anyway — nothing
//! but the instruction text forbids it — retry once on the rendered
//! transcript, which advertises no tools. Both attempts' usage is reported.

use super::compaction_request::CompactionRequest;
use super::summarize::SummarizeError;
use entanglement_provider::{
    GenerationParams, Llm, LlmEvent, LlmRequest, Message, RetryConfig, StopReason, Usage,
};
use futures::StreamExt;

pub(crate) type Finish = Option<(Option<StopReason>, Usage)>;

/// The per-call knobs every attempt reuses.
#[derive(Clone, Copy)]
pub(crate) struct Knobs<'a> {
    pub model: Option<&'a str>,
    pub generation: Option<GenerationParams>,
    pub retry: Option<RetryConfig>,
}

/// What the rendered fallback needs to rebuild the request.
pub(crate) struct Head<'a> {
    pub messages: &'a [Message],
    pub instructions: Option<&'a str>,
    pub limit: usize,
}

/// Send `request`, falling back to the rendered transcript once when a
/// structured reply calls a tool. Returns the summary text and the final
/// attempt's stop reason with the usage of every attempt summed.
pub(crate) async fn run(
    llm: &mut dyn Llm,
    request: CompactionRequest<'_>,
    knobs: Knobs<'_>,
    head: Head<'_>,
) -> Result<(String, Finish), SummarizeError> {
    let first = drain(
        llm,
        request.llm_request(knobs.model, knobs.generation, knobs.retry),
    )
    .await
    .map_err(SummarizeError::Llm)?;
    if !first.called_tool {
        return Ok((first.text, first.finish));
    }
    if !request.is_structured() {
        return Err(tool_call_error());
    }
    tracing::debug!(
        session = request.cache_key().unwrap_or_default(),
        "compaction: the structured summary called a tool, retrying on the rendered transcript"
    );
    let rendered = CompactionRequest::rendered(head.messages, head.instructions, head.limit)?;
    let second = drain(
        llm,
        rendered.llm_request(knobs.model, knobs.generation, knobs.retry),
    )
    .await
    .map_err(SummarizeError::Llm)?;
    if second.called_tool {
        return Err(tool_call_error());
    }
    Ok((second.text, merge_finish(first.finish, second.finish)))
}

fn tool_call_error() -> SummarizeError {
    SummarizeError::Llm(anyhow::anyhow!(
        "compaction failed: the summarizer called a tool instead of replying with text"
    ))
}

struct Drained {
    text: String,
    finish: Finish,
    called_tool: bool,
}

/// Drain one non-streamed-to-the-UI completion: concatenate its `Text` chunks,
/// keep the `Finish` payload (for usage/cost), and note any tool call.
async fn drain(llm: &mut dyn Llm, req: LlmRequest<'_>) -> anyhow::Result<Drained> {
    let mut stream = llm.stream(req).await?;
    let mut out = Drained {
        text: String::new(),
        finish: None,
        called_tool: false,
    };
    while let Some(ev) = stream.next().await {
        match ev? {
            LlmEvent::Text(delta) => out.text.push_str(&delta),
            LlmEvent::ToolCall(_) | LlmEvent::ToolCallDelta { .. } => out.called_tool = true,
            LlmEvent::Finish { stop_reason, usage } => {
                out.called_tool |= stop_reason == Some(StopReason::ToolUse);
                out.finish = Some((stop_reason, usage));
            }
            _ => {}
        }
    }
    Ok(out)
}

/// The second attempt's stop reason, with both attempts' usage summed so a
/// discarded attempt is still priced.
fn merge_finish(first: Finish, second: Finish) -> Finish {
    match (first, second) {
        (Some((_, a)), Some((stop, b))) => Some((stop, sum_usage(&a, &b))),
        (Some((_, a)), None) => Some((None, a)),
        (None, second) => second,
    }
}

fn sum_usage(a: &Usage, b: &Usage) -> Usage {
    let add = |x: Option<u64>, y: Option<u64>| match (x, y) {
        (None, None) => None,
        (x, y) => Some(x.unwrap_or(0) + y.unwrap_or(0)),
    };
    Usage {
        input_tokens: add(a.input_tokens, b.input_tokens),
        output_tokens: add(a.output_tokens, b.output_tokens),
        cached_input_tokens: add(a.cached_input_tokens, b.cached_input_tokens),
        cache_write_tokens: add(a.cache_write_tokens, b.cache_write_tokens),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use entanglement_provider::{LlmStream, ToolCall};

    fn usage(input: u64, cached: Option<u64>) -> Usage {
        Usage {
            input_tokens: Some(input),
            output_tokens: Some(1),
            cached_input_tokens: cached,
            cache_write_tokens: None,
        }
    }

    #[test]
    fn usage_sums_field_wise_and_keeps_absent_fields_absent() {
        let sum = sum_usage(&usage(10, Some(90)), &usage(40, None));
        assert_eq!(sum.input_tokens, Some(50));
        assert_eq!(sum.output_tokens, Some(2));
        assert_eq!(sum.cached_input_tokens, Some(90));
        assert_eq!(sum.cache_write_tokens, None);
    }

    #[test]
    fn merge_keeps_the_second_stop_reason_and_prices_both() {
        let merged = merge_finish(
            Some((Some(StopReason::ToolUse), usage(10, None))),
            Some((Some(StopReason::EndTurn), usage(5, None))),
        );
        let (stop, u) = merged.expect("a finish");
        assert_eq!(stop, Some(StopReason::EndTurn));
        assert_eq!(u.input_tokens, Some(15));
        assert!(merge_finish(None, None).is_none());
    }

    struct Scripted(Vec<LlmEvent>);

    #[async_trait]
    impl Llm for Scripted {
        async fn stream(&mut self, _req: LlmRequest<'_>) -> anyhow::Result<LlmStream> {
            Ok(futures::stream::iter(self.0.clone().into_iter().map(Ok)).boxed())
        }
    }

    async fn drained(events: Vec<LlmEvent>) -> Drained {
        let req = LlmRequest {
            system: "",
            model: None,
            messages: &[],
            tools: &[],
            generation: None,
            cache_key: None,
            trailing_notice: None,
            retry: None,
        };
        drain(&mut Scripted(events), req).await.expect("drains")
    }

    fn finish(stop: StopReason) -> LlmEvent {
        LlmEvent::Finish {
            stop_reason: Some(stop),
            usage: Usage::default(),
        }
    }

    #[tokio::test]
    async fn drain_flags_every_tool_call_signal() {
        let text = drained(vec![
            LlmEvent::Text("sum".into()),
            finish(StopReason::EndTurn),
        ])
        .await;
        assert!(!text.called_tool);
        assert_eq!(text.text, "sum");
        let delta = LlmEvent::ToolCallDelta {
            id: "c".into(),
            name: "read".into(),
            delta: "{".into(),
        };
        assert!(drained(vec![delta]).await.called_tool);
        let call = LlmEvent::ToolCall(ToolCall::new("c", "read", "{}"));
        assert!(drained(vec![call]).await.called_tool);
        assert!(drained(vec![finish(StopReason::ToolUse)]).await.called_tool);
    }
}
