//! The compaction request shape (ADR-0202 §4). On the session's own backend a
//! summarization replays the session's cached prefix — its resolved system
//! prompt, advertised tools and `cache_key` — then the head messages verbatim,
//! one trailing instruction that forbids tool calls. The capped rendered
//! transcript (summarizer system string, no tools, no cache key) is kept for a
//! pinned aux model and for a head too large for the real window.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use entanglement_core::{
    stream_from_response, EngineConfig, Holly, InMsg, Llm, LlmRequest, LlmResponse, LlmStream,
    Message, MessageRole, OutEvent, ResolvedModel, SessionId, ToolCall, ToolSpec,
};

use crate::common::{collect_until_done, spawn_tool_executor};

const SYS: &str = "SESSION SYSTEM PROMPT";
const WINDOW: usize = 4_000; // input limit = 3400 tokens
const INPUT_LIMIT: usize = 3_400;

/// One recorded request.
struct Captured {
    system: String,
    tools: Vec<String>,
    cache_key: Option<String>,
    /// The last message is the structured shape's trailing instruction.
    structured: bool,
    messages: Vec<Message>,
}

impl Captured {
    fn is_rendered_summary(&self) -> bool {
        self.system.contains("summarization assistant")
    }
}

type Log = Arc<Mutex<Vec<Captured>>>;

/// Records every request; answers a compaction request with "SUMMARY" and
/// every other one with the next scripted turn reply ("done" once drained).
struct CapturingLlm {
    log: Log,
    turns: Arc<Mutex<VecDeque<LlmResponse>>>,
}

#[async_trait]
impl Llm for CapturingLlm {
    async fn stream(&mut self, req: LlmRequest<'_>) -> anyhow::Result<LlmStream> {
        let captured = Captured {
            system: req.system.to_string(),
            tools: req.tools.iter().map(|t| t.name.clone()).collect(),
            cache_key: req.cache_key.map(str::to_string),
            structured: req
                .messages
                .last()
                .is_some_and(|m| m.text().starts_with("Summarize the conversation above")),
            messages: req.messages.to_vec(),
        };
        let summary = captured.structured || captured.is_rendered_summary();
        self.log.lock().unwrap().push(captured);
        let reply = if summary {
            text("SUMMARY")
        } else {
            self.turns
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| text("done"))
        };
        Ok(stream_from_response(reply))
    }
}

fn text(t: &str) -> LlmResponse {
    LlmResponse {
        text: t.into(),
        tool_calls: vec![],
    }
}

fn call(id: &str, n: u8) -> LlmResponse {
    LlmResponse {
        text: String::new(),
        tool_calls: vec![ToolCall::new(id, "probe", format!("{{\"n\":{n}}}"))],
    }
}

fn capturing(log: &Log, turns: Vec<LlmResponse>) -> entanglement_core::LlmFactory {
    let log = log.clone();
    let turns = Arc::new(Mutex::new(VecDeque::from(turns)));
    Arc::new(move || {
        Box::new(CapturingLlm {
            log: log.clone(),
            turns: turns.clone(),
        }) as Box<dyn Llm>
    })
}

/// A session with a counted `system_prompt_resolver`, one advertised tool and
/// a 4k window.
fn config(log: &Log, turns: Vec<LlmResponse>, resolver_calls: &Arc<AtomicUsize>) -> EngineConfig {
    let calls = resolver_calls.clone();
    EngineConfig {
        llm_factory: capturing(log, turns),
        tool_specs: vec![ToolSpec::new("probe", "a test tool")],
        context_window: Some(WINDOW),
        system_prompt_resolver: Some(Arc::new(move |sid, _profile| {
            // Count only the session under test: a compaction forks a
            // successor (ADR-0205), and that successor resolving its *own*
            // system prompt is not the double-fetch this counter guards
            // against.
            if sid.0 == "s1" {
                calls.fetch_add(1, Ordering::SeqCst);
            }
            Some(SYS.to_string())
        })),
        ..EngineConfig::default()
    }
}

fn estimate(messages: &[Message]) -> usize {
    let chars: usize = messages
        .iter()
        .map(|m| {
            m.text().chars().count()
                + m.tool_calls
                    .iter()
                    .map(|c| c.input.chars().count())
                    .sum::<usize>()
        })
        .sum();
    (chars as f64 / 3.5).ceil() as usize
}

fn assert_structured(req: &Captured, sid: &SessionId) {
    assert!(req.structured, "the structured instruction");
    assert_eq!(req.system, SYS, "the session's resolved system prompt");
    assert_eq!(req.tools, vec!["probe".to_string()], "the advertised specs");
    assert_eq!(req.cache_key.as_deref(), Some(sid.0.as_str()));
    let last = req.messages.last().expect("an instruction");
    assert_eq!(last.role, MessageRole::User);
    assert!(last.text().contains("Do not call any tools"));
}

fn assert_rendered(req: &Captured) {
    assert!(req.is_rendered_summary(), "summarizer system string");
    assert!(req.tools.is_empty(), "no tools");
    assert_eq!(req.cache_key, None, "no cache key");
    assert_eq!(req.messages.len(), 1, "one rendered transcript message");
    assert!(req.messages[0].text().contains("transcript below"));
}

/// Three 500-char turns, then an 11k-char prompt overflows the 3400-token
/// input limit at turn start. `safe_kept(4)` lands the tail on turn 3's prompt,
/// so the head is turns 1–2. Returns the log, the overflow turn's events, and
/// how often the resolver ran during that turn.
async fn turn_start_overflow() -> (Log, Vec<OutEvent>, usize) {
    let log: Log = Arc::default();
    let resolver_calls = Arc::new(AtomicUsize::new(0));
    let holly = Holly::spawn(config(&log, vec![], &resolver_calls));
    let sid = SessionId::new("s1");
    for i in 0..3 {
        let sub = holly.subscribe();
        let prompt = format!("turn-{i}: {}", "y".repeat(490));
        holly
            .send(InMsg::prompt(sid.clone(), prompt))
            .await
            .unwrap();
        collect_until_done(sub, &sid).await;
    }
    let before = resolver_calls.load(Ordering::SeqCst);
    let sub = holly.subscribe();
    holly
        .send(InMsg::prompt(sid.clone(), "x".repeat(11_000)))
        .await
        .unwrap();
    let events = collect_until_done(sub, &sid).await;
    let during = resolver_calls.load(Ordering::SeqCst) - before;
    (log, events, during)
}

/// One prompt whose turn calls `probe` twice; the second result is
/// `output_chars` long, so the third round overflows mid-turn — where the kept
/// tail collapses to 0 and the head is the whole over-limit context.
async fn mid_turn_overflow(output_chars: usize, aux: Option<&Log>) -> (Log, Vec<OutEvent>) {
    let log: Log = Arc::default();
    let mut cfg = config(
        &log,
        vec![call("c1", 1), call("c2", 2)],
        &Arc::new(AtomicUsize::new(0)),
    );
    if let Some(aux_log) = aux {
        let factory = capturing(aux_log, vec![]);
        cfg.aux_llm_resolver = Some(Arc::new(move |_purpose: &str| {
            Some(ResolvedModel {
                provider: "aux-provider".to_string(),
                model: "aux-model".to_string(),
                llm_factory: factory.clone(),
                generation: None,
                context_window: None,
            })
        }));
    }
    let holly = Holly::spawn(cfg);
    spawn_tool_executor(&holly, move |_tool, input| {
        if input.contains('2') {
            "x".repeat(output_chars)
        } else {
            "small".to_string()
        }
    });
    let sid = SessionId::new("s1");
    let sub = holly.subscribe();
    holly.send(InMsg::prompt(sid.clone(), "go")).await.unwrap();
    let events = collect_until_done(sub, &sid).await;
    (log, events)
}

/// The summary the fork carries starts with the model's text and may have the
/// ADR-0102 kept tail composed after it, so match on the prefix.
fn auto_compacted(events: &[OutEvent]) -> bool {
    events.iter().any(|e| {
        matches!(e, OutEvent::Compacted { auto: true, summary, .. } if summary.starts_with("SUMMARY"))
    })
}

/// (a) Session-backend auto-compaction replays the session's prefix: the head
/// verbatim — a strict prefix of the live history — plus one instruction.
#[tokio::test]
async fn session_backend_compaction_replays_the_cached_prefix() {
    let (log, events, _) = turn_start_overflow().await;
    assert!(auto_compacted(&events), "{events:?}");
    let log = log.lock().unwrap();
    let summaries: Vec<&Captured> = log.iter().filter(|c| c.structured).collect();
    assert_eq!(summaries.len(), 1);
    let req = summaries[0];
    assert_structured(req, &SessionId::new("s1"));
    // The last pre-overflow turn sent [t0, a0, t1, a1, t2]; the head is its
    // first four messages (turn 3's prompt onward is the kept tail).
    let last_turn = &log[2].messages;
    assert_eq!(req.messages.len(), 5, "4 head messages + the instruction");
    assert_eq!(&req.messages[..4], &last_turn[..4]);
    assert!(log.iter().all(|c| !c.is_rendered_summary()));
}

/// (g) The resolver runs once for the overflowing turn: compaction reuses the
/// round's resolved prompt rather than fetching it again.
#[tokio::test]
async fn system_prompt_resolver_runs_once_even_when_auto_compaction_runs() {
    let (_log, events, during) = turn_start_overflow().await;
    assert!(auto_compacted(&events), "{events:?}");
    assert_eq!(during, 1);
}

/// (b) + (c) Mid-turn: the head ends on a `Tool` message and is over the
/// input limit, yet fits the real window, so it still goes structured.
#[tokio::test]
async fn mid_turn_head_over_the_limit_but_within_the_window_goes_structured() {
    let (log, events) = mid_turn_overflow(12_000, None).await;
    assert!(auto_compacted(&events), "{events:?}");
    // The compaction forks (ADR-0205), so this session is retired at the fork
    // and the turn goes on in its successor — no `Done` here any more.
    assert!(
        events
            .iter()
            .any(|e| matches!(e, OutEvent::SessionEnded { .. })),
        "the compacted session is retired: {events:?}"
    );
    let log = log.lock().unwrap();
    let req = log
        .iter()
        .find(|c| c.structured)
        .expect("a structured summary request");
    assert_structured(req, &SessionId::new("s1"));
    let head = &req.messages[..req.messages.len() - 1];
    let roles: Vec<MessageRole> = head.iter().map(|m| m.role).collect();
    use MessageRole::{Assistant, Tool, User};
    assert_eq!(roles, vec![User, Assistant, Tool, Assistant, Tool]);
    assert!(
        estimate(head) > INPUT_LIMIT,
        "the head is over ctx.limit(): {}",
        estimate(head)
    );
    // Strict prefix of the live history: round 2 sent exactly head[..3] (plus
    // its own trailing mode notice, ADR-0207 §9, orthogonal to this shape
    // check — never part of `Context`/the compaction request, so `req`/`head`
    // above never carry it; only `log[1]`'s raw capture does).
    assert_eq!(&head[..3], &log[1].messages[..3]);
}

/// (d) A head over the real-window budget falls back to the rendered
/// transcript, whose per-tool-message cap brings it under the input limit.
#[tokio::test]
async fn head_over_the_real_window_falls_back_to_the_rendered_transcript() {
    let (log, events) = mid_turn_overflow(14_000, None).await;
    assert!(auto_compacted(&events), "{events:?}");
    let log = log.lock().unwrap();
    assert!(log.iter().all(|c| !c.structured), "no structured request");
    let rendered: Vec<&Captured> = log.iter().filter(|c| c.is_rendered_summary()).collect();
    assert_eq!(rendered.len(), 1);
    assert_rendered(rendered[0]);
}

/// (e) A pinned `summarize` aux model gets the rendered transcript even when
/// the structured shape would fit.
#[tokio::test]
async fn pinned_aux_backend_gets_the_rendered_transcript() {
    let aux_log: Log = Arc::default();
    let (log, events) = mid_turn_overflow(12_000, Some(&aux_log)).await;
    assert!(auto_compacted(&events), "{events:?}");
    let aux_log = aux_log.lock().unwrap();
    assert_eq!(
        aux_log.len(),
        1,
        "exactly the summary lands on the aux model"
    );
    assert_rendered(&aux_log[0]);
    let log = log.lock().unwrap();
    assert!(log
        .iter()
        .all(|c| !c.structured && !c.is_rendered_summary()));
}

/// (f) Manual `/compact` (the `Oneshot` op) takes the structured path too,
/// with `instructions` appended to the trailing instruction.
#[tokio::test]
async fn manual_compact_takes_the_structured_path() {
    let log: Log = Arc::default();
    let holly = Holly::spawn(config(&log, vec![], &Arc::new(AtomicUsize::new(0))));
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
            args: serde_json::json!({ "instructions": "keep file paths" }),
        })
        .await
        .unwrap();
    let events = collect_until_done(sub, &sid).await;
    assert!(
        events
            .iter()
            .any(|e| matches!(e, OutEvent::Compacted { auto: false, .. })),
        "{events:?}"
    );
    let log = log.lock().unwrap();
    // The successor's own first turn lands in this same log, so pick the
    // compaction request by shape rather than by position.
    let req = log
        .iter()
        .find(|c| c.structured)
        .expect("the compaction request");
    assert_structured(req, &sid);
    let texts: Vec<String> = req.messages.iter().map(Message::text).collect();
    assert_eq!(texts[..2], ["hello".to_string(), "done".to_string()]);
    assert_eq!(texts.len(), 3);
    assert!(texts[2].ends_with("Additional instructions: keep file paths"));
}
