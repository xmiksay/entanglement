//! Live-vs-resumed history harness: drive a scripted session through `Holly`,
//! record its events the way the persistence tap pairs them, then compare the
//! history the live session sends on its next request with the history a
//! fresh engine resumed from that log sends.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use entanglement_core::{
    AgentState, ContentPart, EngineConfig, Holly, InMsg, Llm, LlmEvent, LlmRequest, LlmStream,
    Message, MessageRole, OutEvent, SessionId, StopReason, ToolCall, ToolSpec, Usage,
};
use futures::{stream, StreamExt};
use tokio::sync::broadcast::Receiver;

pub type Log = Vec<(Option<InMsg>, OutEvent)>;
type Requests = Arc<Mutex<Vec<String>>>;

/// One scripted stream item.
#[derive(Clone)]
pub enum Step {
    Text(&'static str),
    Reasoning(&'static str),
    Block(ContentPart),
    Call(ToolCall),
    Finish(Option<StopReason>),
    /// A terminal stream error.
    Fail(&'static str),
    /// Never yields again — a target for `Stop`.
    Hang,
}

struct ScriptLlm {
    scripts: Arc<Mutex<VecDeque<Vec<Step>>>>,
    requests: Requests,
}

#[async_trait]
impl Llm for ScriptLlm {
    async fn stream(&mut self, req: LlmRequest<'_>) -> anyhow::Result<LlmStream> {
        // The mode notice (ADR-0207 §9) now travels as `trailing_notice`, out
        // of band from `messages` — the prompt-cache fix this harness's own
        // callers depend on (`assert_resume_from` still pops it off as the
        // synthesized last message, so its "history minus the notice" shape
        // assertions stay unchanged).
        let mut probed = req.messages.to_vec();
        if let Some(notice) = &req.trailing_notice {
            probed.push(Message::user(notice.clone()));
        }
        let history = serde_json::to_string(&probed)?;
        self.requests.lock().unwrap().push(history);
        let steps = self
            .scripts
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| vec![Step::Text("ok"), Step::Finish(Some(StopReason::EndTurn))]);
        let mut items = Vec::new();
        let mut hang = false;
        for step in steps {
            items.push(match step {
                Step::Text(t) => Ok(LlmEvent::Text(t.into())),
                Step::Reasoning(t) => Ok(LlmEvent::Reasoning(t.into())),
                Step::Block(part) => Ok(LlmEvent::ContentBlock(part)),
                Step::Call(call) => Ok(LlmEvent::ToolCall(call)),
                Step::Finish(stop_reason) => Ok(LlmEvent::Finish {
                    stop_reason,
                    usage: Usage::default(),
                }),
                Step::Fail(e) => Err(anyhow::anyhow!(e)),
                Step::Hang => {
                    hang = true;
                    continue;
                }
            });
        }
        let head = stream::iter(items);
        Ok(if hang {
            head.chain(stream::pending()).boxed()
        } else {
            head.boxed()
        })
    }
}

pub fn text_round(text: &'static str) -> Vec<Step> {
    vec![Step::Text(text), Step::Finish(Some(StopReason::EndTurn))]
}

pub fn call(id: &str, name: &str, input: &str) -> ToolCall {
    ToolCall::new(id, name, input)
}

pub fn tool_round(calls: Vec<ToolCall>) -> Vec<Step> {
    let mut steps: Vec<Step> = calls.into_iter().map(Step::Call).collect();
    steps.push(Step::Finish(Some(StopReason::ToolUse)));
    steps
}

pub struct Run {
    holly: Holly,
    sub: Receiver<OutEvent>,
    pub sid: SessionId,
    pub log: Log,
    /// Inbound messages awaiting a recorded event to pair with, first in
    /// first out like `pair_records` — only `Prompt`/`Stop`, the ones replay
    /// reads.
    pending_in: VecDeque<InMsg>,
    requests: Requests,
    specs: Vec<ToolSpec>,
    tweak: fn(&mut EngineConfig),
    /// Inbound fan-out, mirroring what the runtime's persistence tap watches.
    inbound: Receiver<InMsg>,
    /// Seed prompt of each `Spawn` seen inbound. The tap synthesizes an
    /// `InMsg::Prompt` record for a spawned session from this (ADR-0113), and
    /// a compaction successor's whole starting history *is* that seed — so a
    /// harness that skipped it would "prove" a replay equality the real store
    /// does not have.
    spawn_seeds: HashMap<SessionId, String>,
}

impl Run {
    pub fn start(scripts: Vec<Vec<Step>>, specs: Vec<ToolSpec>) -> Self {
        Self::with_config(scripts, specs, |_| {})
    }

    /// `tweak` adjusts the engine config, for the live and resumed engine alike.
    pub fn with_config(
        scripts: Vec<Vec<Step>>,
        specs: Vec<ToolSpec>,
        tweak: fn(&mut EngineConfig),
    ) -> Self {
        let scripts = Arc::new(Mutex::new(VecDeque::from(scripts)));
        let requests: Requests = Arc::default();
        let (s, r) = (scripts.clone(), requests.clone());
        let mut cfg = EngineConfig {
            llm_factory: Arc::new(move || {
                Box::new(ScriptLlm {
                    scripts: s.clone(),
                    requests: r.clone(),
                }) as Box<dyn Llm>
            }),
            tool_specs: specs.clone(),
            ..EngineConfig::default()
        };
        tweak(&mut cfg);
        let holly = Holly::spawn(cfg);
        Self {
            sub: holly.subscribe(),
            inbound: holly.subscribe_inbound(),
            holly,
            sid: SessionId::new("eq"),
            log: Vec::new(),
            pending_in: VecDeque::new(),
            requests,
            specs,
            tweak,
            spawn_seeds: HashMap::new(),
        }
    }

    pub async fn send(&mut self, msg: InMsg) {
        if matches!(msg, InMsg::Prompt { .. } | InMsg::Stop { .. }) {
            self.pending_in.push_back(msg.clone());
        }
        self.holly.send(msg).await.unwrap();
    }

    pub async fn prompt(&mut self, text: &str) {
        self.send(InMsg::prompt(self.sid.clone(), text)).await;
    }

    pub async fn result(&mut self, id: &str, output: &str) {
        self.send(InMsg::tool_result(self.sid.clone(), id, output))
            .await;
    }

    pub async fn result_content(&mut self, id: &str, content: Vec<ContentPart>) {
        let msg = InMsg::ToolResult {
            session: self.sid.clone(),
            request_id: id.into(),
            content,
            is_error: false,
            duration_ms: None,
            exit_code: None,
        };
        self.send(msg).await;
    }

    pub async fn stop(&mut self) {
        self.send(InMsg::Stop {
            session: self.sid.clone(),
        })
        .await;
    }

    /// Record `sid`'s events until one matches `pred`, inclusive.
    pub async fn until(&mut self, pred: impl Fn(&OutEvent) -> bool) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
        loop {
            self.drain_inbound();
            let Ok(Ok(ev)) = tokio::time::timeout_at(deadline, self.sub.recv()).await else {
                panic!("timed out; recorded so far: {:#?}", self.log);
            };
            if ev.session() != Some(&self.sid) {
                continue;
            }
            let hit = pred(&ev);
            self.log.push((self.pending_in.pop_front(), ev));
            if hit {
                return;
            }
        }
    }

    fn drain_inbound(&mut self) {
        while let Ok(msg) = self.inbound.try_recv() {
            if let InMsg::Spawn {
                session, prompt, ..
            } = msg
            {
                self.spawn_seeds.insert(session, prompt);
            }
        }
    }

    /// Drive until this session compacts into a successor (ADR-0205), then
    /// retarget the run at that successor.
    ///
    /// Returns the retired predecessor's log and leaves `self.log` holding the
    /// successor's own — each is a separate root, so the real store gives them
    /// separate files and neither replays the other's records. The successor's
    /// log opens exactly as persistence would write it: its `SessionStarted`,
    /// then the seed prompt synthesized from its `Spawn` (ADR-0113).
    pub async fn until_forked(&mut self) -> Log {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            self.drain_inbound();
            let Ok(Ok(ev)) = tokio::time::timeout_at(deadline, self.sub.recv()).await else {
                panic!("timed out waiting for a fork; recorded: {:#?}", self.log);
            };
            if let OutEvent::SessionStarted {
                session: successor,
                predecessor: Some(source),
                ..
            } = &ev
            {
                if *source == self.sid {
                    let successor = successor.clone();
                    self.drain_inbound();
                    let predecessor_log = std::mem::take(&mut self.log);
                    self.sid = successor.clone();
                    self.log.push((None, ev));
                    let seed = self
                        .spawn_seeds
                        .remove(&successor)
                        .expect("the successor's Spawn carried its seed prompt");
                    self.pending_in.push_back(InMsg::prompt(successor, seed));
                    // Don't hand back a successor whose seed turn is still in
                    // flight: a caller probing it then would race the very
                    // prompt that gives it its history, and read an empty one.
                    self.until_done().await;
                    return predecessor_log;
                }
            }
            if ev.session() == Some(&self.sid) {
                self.log.push((self.pending_in.pop_front(), ev));
            }
        }
    }

    pub async fn until_status(&mut self, want: AgentState) {
        self.until(|ev| matches!(ev, OutEvent::Status { state, .. } if *state == want))
            .await;
    }

    /// The log recorded so far — a crash snapshot, when the session goes on.
    pub fn take_log(&mut self) -> Log {
        std::mem::take(&mut self.log)
    }

    pub async fn until_done(&mut self) {
        self.until(|ev| matches!(ev, OutEvent::Done { .. })).await;
    }

    pub async fn until_exec(&mut self, id: &str) {
        self.until(|ev| matches!(ev, OutEvent::ToolExec { request_id, .. } if request_id == id))
            .await;
    }

    /// The history the session sends on its next request.
    async fn probe(&mut self) -> String {
        let before = self.requests.lock().unwrap().len();
        self.prompt("probe").await;
        self.until_done().await;
        self.requests.lock().unwrap()[before].clone()
    }
}

/// Probe `live`'s next request, resume a fresh engine from its log, probe that
/// too, and require byte-identical histories. Returns the live history.
pub async fn assert_resume_is_byte_identical(mut live: Run) -> Vec<Message> {
    let log = live.take_log();
    assert_resume_from(live, log).await
}

/// As [`assert_resume_is_byte_identical`], resuming from `log` — taken
/// earlier, with the live session since brought to rest.
pub async fn assert_resume_from(mut live: Run, log: Log) -> Vec<Message> {
    let live_history = live.probe().await;
    let mut resumed = Run::with_config(Vec::new(), live.specs.clone(), live.tweak);
    resumed.holly.resume(live.sid.clone(), log).await.unwrap();
    let resumed_history = resumed.probe().await;
    assert_eq!(
        resumed_history, live_history,
        "a resumed session must send the live session's history byte for byte"
    );
    let mut history: Vec<Message> = serde_json::from_str(&live_history).unwrap();
    // Every request carries a trailing mode notice (ADR-0207 §9), rebuilt
    // fresh from `Session::mode` per round — never part of persisted `ctx`,
    // so it plays no part in *this* harness's job (conversation-content
    // replay fidelity, which the byte-identical assert above already covers,
    // notice included). Dropped here so callers' shape assertions stay about
    // the conversation, not this orthogonal, separately-tested addition (see
    // `set_mode.rs`).
    let notice = history.pop();
    assert!(
        notice
            .as_ref()
            .is_some_and(|m| m.text().starts_with("[mode: ")),
        "every probed request ends with the mode notice, got {notice:?}"
    );
    history
}

/// The history `id`'s own log replays to — how a **retired** session (a
/// compaction predecessor) reads back.
///
/// Folds the log directly rather than resuming and probing: a predecessor was
/// retired precisely *because* its history overflowed the window, so running a
/// turn against it would just compact it again and never answer. What matters
/// here is the reconstructed history itself, which is exactly what the fold
/// produces.
pub fn replayed_messages(tweak: fn(&mut EngineConfig), id: &SessionId, log: &Log) -> Vec<Message> {
    let mut cfg = EngineConfig::default();
    tweak(&mut cfg);
    let session = entanglement_core::session::Session::replay(log, &cfg, id)
        .expect("the retired session's log replays");
    session.ctx.messages().to_vec()
}

/// A compact rendering of a history for shape assertions.
pub fn shape(history: &[Message]) -> Vec<String> {
    history
        .iter()
        .map(|m| match m.role {
            MessageRole::User => format!("user:{}", m.text()),
            MessageRole::Tool => format!("tool:{}", m.tool_call_id.as_deref().unwrap_or("")),
            _ => {
                let ids: Vec<&str> = m.tool_calls.iter().map(|c| c.id.as_str()).collect();
                format!("assistant:{}{:?}", m.text(), ids)
            }
        })
        .collect()
}
