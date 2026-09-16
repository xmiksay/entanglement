//! The compaction tool-call fallback (ADR-0202 §4). The structured request is
//! byte-identical to a turn's — same system, tools and cache key, no
//! tool-choice override — so only its instruction text forbids a tool call. A
//! reply that calls one anyway is discarded and summarization re-runs once on
//! the rendered transcript; both attempts are priced into one `Usage`.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use entanglement_core::{
    EngineConfig, Holly, InMsg, Llm, LlmEvent, LlmRequest, LlmStream, Message, OutEvent, SessionId,
    StopReason, ToolCall, ToolSpec, Usage, UsagePurpose,
};
use futures::{stream, StreamExt};

use crate::common::collect_until_done;

const SYS: &str = "SESSION SYSTEM PROMPT";

#[derive(Debug, Clone, Copy, PartialEq)]
enum Kind {
    Turn,
    Structured,
    Rendered,
}

struct Captured {
    kind: Kind,
    system: String,
    model: Option<String>,
    /// (name, description, schema) — `ToolSpec` has no `PartialEq`.
    tools: Vec<(String, String, String)>,
    cache_key: Option<String>,
    messages: Vec<Message>,
}

type Log = Arc<Mutex<Vec<Captured>>>;
type Reply = Arc<dyn Fn(Kind) -> Vec<LlmEvent> + Send + Sync>;

struct ScriptLlm {
    log: Log,
    reply: Reply,
}

#[async_trait]
impl Llm for ScriptLlm {
    async fn stream(&mut self, req: LlmRequest<'_>) -> anyhow::Result<LlmStream> {
        let kind = if req
            .messages
            .last()
            .is_some_and(|m| m.text().starts_with("Summarize the conversation above"))
        {
            Kind::Structured
        } else if req.system.contains("summarization assistant") {
            Kind::Rendered
        } else {
            Kind::Turn
        };
        self.log.lock().unwrap().push(Captured {
            kind,
            system: req.system.to_string(),
            model: req.model.map(str::to_string),
            tools: req
                .tools
                .iter()
                .map(|t| (t.name.clone(), t.description.clone(), t.schema.to_string()))
                .collect(),
            cache_key: req.cache_key.map(str::to_string),
            messages: req.messages.to_vec(),
        });
        Ok(stream::iter((self.reply)(kind).into_iter().map(Ok)).boxed())
    }
}

fn usage(input: u64, cached: u64, output: u64) -> Usage {
    Usage {
        input_tokens: Some(input),
        output_tokens: Some(output),
        cached_input_tokens: Some(cached),
        cache_write_tokens: None,
    }
}

fn text_reply(text: &str, usage: Usage) -> Vec<LlmEvent> {
    vec![
        LlmEvent::Text(text.to_string()),
        LlmEvent::Finish {
            stop_reason: Some(StopReason::EndTurn),
            usage,
        },
    ]
}

fn tool_reply(usage: Usage) -> Vec<LlmEvent> {
    vec![
        LlmEvent::ToolCallDelta {
            id: "c1".into(),
            name: "probe".into(),
            delta: "{}".into(),
        },
        LlmEvent::ToolCall(ToolCall::new("c1", "probe", "{}")),
        LlmEvent::Finish {
            stop_reason: Some(StopReason::ToolUse),
            usage,
        },
    ]
}

/// One turn ("hello" → "done"), then a manual `/compact`. Returns every
/// request and the compaction's events.
async fn compact_with(
    reply: impl Fn(Kind) -> Vec<LlmEvent> + Send + Sync + 'static,
) -> (Log, Vec<OutEvent>) {
    let log: Log = Arc::default();
    let (factory_log, reply): (Log, Reply) = (log.clone(), Arc::new(reply));
    let holly = Holly::spawn(EngineConfig {
        llm_factory: Arc::new(move || {
            Box::new(ScriptLlm {
                log: factory_log.clone(),
                reply: reply.clone(),
            }) as Box<dyn Llm>
        }),
        tool_specs: vec![ToolSpec::new("probe", "a test tool")],
        system_prompt_resolver: Some(Arc::new(|_sid, _profile| Some(SYS.to_string()))),
        ..EngineConfig::default()
    });
    let sid = SessionId::new("s1");
    let sub = holly.subscribe();
    holly
        .send(InMsg::prompt(sid.clone(), "hello"))
        .await
        .unwrap();
    collect_until_done(sub, &sid).await;
    let sub = holly.subscribe();
    holly
        .send(InMsg::Oneshot {
            session: sid.clone(),
            op: "compact".to_string(),
            args: serde_json::json!({}),
        })
        .await
        .unwrap();
    (log, collect_until_done(sub, &sid).await)
}

fn kinds(log: &Log) -> Vec<Kind> {
    log.lock().unwrap().iter().map(|c| c.kind).collect()
}

fn summary(events: &[OutEvent]) -> Option<&str> {
    events.iter().find_map(|e| match e {
        OutEvent::Compacted { summary, .. } => Some(summary.as_str()),
        _ => None,
    })
}

/// Turn replies "done"; the structured summary calls a tool; the rendered one
/// answers with text.
fn tool_then_text(kind: Kind) -> Vec<LlmEvent> {
    match kind {
        Kind::Turn => text_reply("done", Usage::default()),
        Kind::Structured => tool_reply(usage(100, 900, 7)),
        Kind::Rendered => text_reply("RENDERED SUMMARY", usage(50, 0, 20)),
    }
}

/// (a) The structured request replays the turn's system, model, tools and
/// cache key unchanged — nothing but the instruction forbids a tool call.
#[tokio::test]
async fn structured_request_matches_the_turn_and_forbids_tools_in_text() {
    let (log, events) = compact_with(|kind| match kind {
        Kind::Turn => text_reply("done", Usage::default()),
        _ => text_reply("SUMMARY", Usage::default()),
    })
    .await;
    assert_eq!(summary(&events), Some("SUMMARY"), "{events:?}");
    // Prefix, not equality: the compaction forks (ADR-0205) and the successor
    // runs its own first turn against this same scripted LLM, which may or may
    // not have landed by the time the source's events are drained.
    assert_eq!(
        kinds(&log)[..2],
        [Kind::Turn, Kind::Structured],
        "no fallback"
    );
    let log = log.lock().unwrap();
    let (turn, compaction) = (&log[0], &log[1]);
    assert_eq!(compaction.system, turn.system);
    assert_eq!(compaction.model, turn.model);
    assert_eq!(compaction.tools, turn.tools);
    assert_eq!(compaction.cache_key, turn.cache_key);
    assert_eq!(compaction.messages[0], turn.messages[0]);
    let instruction = compaction.messages.last().expect("an instruction").text();
    assert!(
        instruction.contains("Do not call any tools; reply with the summary text only."),
        "{instruction}"
    );
}

/// (b) A tool call on the structured attempt re-runs once on the rendered
/// transcript, whose text becomes the summary.
#[tokio::test]
async fn a_structured_tool_call_falls_back_to_the_rendered_transcript() {
    let (log, events) = compact_with(tool_then_text).await;
    assert_eq!(
        kinds(&log)[..3],
        [Kind::Turn, Kind::Structured, Kind::Rendered]
    );
    {
        let log = log.lock().unwrap();
        assert!(log[2].tools.is_empty(), "the fallback advertises no tools");
        assert_eq!(log[2].cache_key, None);
    }
    let summary = summary(&events).expect("a compaction");
    assert!(summary.starts_with("RENDERED SUMMARY"), "{summary}");
}

/// (c) Both attempts are priced: one compaction `Usage`, summed field-wise.
#[tokio::test]
async fn both_attempts_usage_is_summed_into_the_compaction_usage() {
    let (_log, events) = compact_with(tool_then_text).await;
    let compaction: Vec<(u64, u64, u64)> = events
        .iter()
        .filter_map(|e| match e {
            OutEvent::Usage {
                input_tokens,
                cached_input_tokens,
                output_tokens,
                purpose: UsagePurpose::Compaction,
                ..
            } => Some((*input_tokens, *cached_input_tokens, *output_tokens)),
            _ => None,
        })
        .collect();
    assert_eq!(compaction, [(150, 900, 27)], "{events:?}");
}

/// (d) A tool call on the rendered attempt — which advertised no tools — fails
/// the compaction like an LLM error: `Error` + `Done`, nothing compacted.
#[tokio::test]
async fn a_rendered_tool_call_fails_the_compaction() {
    let (log, events) = compact_with(|kind| match kind {
        Kind::Turn => text_reply("done", Usage::default()),
        _ => tool_reply(usage(10, 0, 1)),
    })
    .await;
    assert_eq!(
        kinds(&log)[..3],
        [Kind::Turn, Kind::Structured, Kind::Rendered]
    );
    assert_eq!(summary(&events), None, "{events:?}");
    assert!(events.iter().any(
        |e| matches!(e, OutEvent::Error { message, .. } if message.contains("called a tool"))
    ));
    assert!(events.iter().any(|e| matches!(e, OutEvent::Done { .. })));
    assert!(!events.iter().any(|e| matches!(e, OutEvent::Usage { .. })));
}
