//! Regression tests for the turn-loop stash discipline (ADR-0018).
//!
//! Commands arriving on the session inbox *during* a turn (mid-stream or
//! between tool calls) must be stashed and replayed after the turn ends, not
//! silently dropped. Before ADR-0018, the `try_recv` polls only matched
//! `SessionCmd::Stop` and discarded everything else — so a `Prompt` sent
//! while the engine was mid-turn vanished without trace, and the user's
//! follow-up question was lost.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use entanglement_core::{
    stream_from_response, AgentMode, AgentProfile, EngineConfig, Holly, InMsg, Llm, LlmRequest,
    LlmResponse, LlmSession, LlmStream, OutEvent, Permission, PermissionProfile, SessionId,
    ToolCall,
};

mod common;
use common::{spawn_tool_executor, unknown_tool};

/// Collect `TextDelta` texts for `sid` until the deadline, across as many
/// turns as happen. (Unlike `actor.rs::collect`, this does *not* break on
/// `Done` — we want to see follow-on turns.)
async fn collect_texts_for(
    mut sub: tokio::sync::broadcast::Receiver<OutEvent>,
    sid: &SessionId,
    dur: Duration,
) -> Vec<String> {
    let mut texts = Vec::new();
    let deadline = tokio::time::Instant::now() + dur;
    loop {
        let Ok(recv) = tokio::time::timeout_at(deadline, sub.recv()).await else {
            break;
        };
        match recv {
            Ok(OutEvent::TextDelta { text, session, .. }) if session == *sid => {
                texts.push(text);
            }
            Ok(_) => {}
            Err(_) => break,
        }
    }
    texts
}

/// `ScriptedLlm` variant that sleeps before each `stream()` call returns, so
/// a test can reliably inject inbox commands during the streaming window
/// (the `try_recv` polls run inside the consumer loop, which only starts once
/// `stream()` has returned the `LlmStream`).
struct SlowScriptedLlm {
    responses: Mutex<Vec<LlmResponse>>,
    delay: Duration,
}

impl SlowScriptedLlm {
    fn new(mut responses: Vec<LlmResponse>, delay: Duration) -> Self {
        responses.reverse();
        Self {
            responses: Mutex::new(responses),
            delay,
        }
    }
}

#[async_trait]
impl Llm for SlowScriptedLlm {
    async fn stream(&mut self, _req: LlmRequest<'_>) -> anyhow::Result<LlmStream> {
        tokio::time::sleep(self.delay).await;
        let resp = {
            let mut responses = self.responses.lock().unwrap();
            responses.pop().unwrap_or_else(|| LlmResponse {
                text: "ok".into(),
                tool_calls: vec![],
            })
        };
        Ok(stream_from_response(resp))
    }
}

/// Regression: a `Prompt` arriving while the engine is mid-stream must be
/// stashed and replayed once the in-flight turn ends, not dropped.
#[tokio::test]
async fn prompt_arriving_during_streaming_is_stashed_and_replayed() {
    let delay = Duration::from_millis(100);
    let scripted = Arc::new(vec![
        LlmResponse {
            text: "first-reply".into(),
            tool_calls: vec![],
        },
        LlmResponse {
            text: "second-reply".into(),
            tool_calls: vec![],
        },
    ]);
    let cfg = EngineConfig {
        llm_factory: Arc::new(move || {
            LlmSession::new(Box::new(SlowScriptedLlm::new((*scripted).clone(), delay)))
        }),
        ..EngineConfig::default()
    };
    let holly = Holly::spawn(cfg);
    let sid = SessionId::new("s1");
    let sub = holly.subscribe();

    holly
        .send(InMsg::Prompt {
            session: sid.clone(),
            text: "first".into(),
        })
        .await
        .unwrap();

    // Wait until the first turn is inside stream() (which sleeps for `delay`).
    // Sending the second Prompt during this window means it lands in the
    // inbox before the streaming consumer's first try_recv poll.
    tokio::time::sleep(Duration::from_millis(20)).await;
    holly
        .send(InMsg::Prompt {
            session: sid.clone(),
            text: "second".into(),
        })
        .await
        .unwrap();

    let texts = collect_texts_for(sub, &sid, Duration::from_secs(2)).await;
    assert!(
        texts.iter().any(|t| t == "first-reply"),
        "first turn should produce its reply; got {texts:?}"
    );
    assert!(
        texts.iter().any(|t| t == "second-reply"),
        "stashed Prompt must be replayed after the first turn ends; got {texts:?}"
    );
}

/// Regression (#182): a `Prompt` arriving mid-turn — while the inner LLM→tool
/// loop is still running — must fold into the *live* turn's context before the
/// next model request, not replay as a separate turn after `Done`. The first
/// response is a tool call, so the inner loop iterates and reaches the fold
/// site; the steering prompt sent during that first stream is drained into
/// `ctx` and the turn continues as one, producing exactly one `Done`.
#[tokio::test]
async fn prompt_arriving_mid_turn_folds_into_the_live_turn() {
    let delay = Duration::from_millis(100);
    let scripted = Arc::new(vec![
        // First round: a tool call, so the inner loop iterates and hits the
        // fold site on the next iteration.
        LlmResponse {
            text: "".into(),
            tool_calls: vec![ToolCall {
                id: "t1".into(),
                name: "unknown-tool".into(),
                input: "{}".into(),
            }],
        },
        // Second round (post-fold): plain text, so the turn ends here.
        LlmResponse {
            text: "folded-reply".into(),
            tool_calls: vec![],
        },
    ]);
    let cfg = EngineConfig {
        llm_factory: Arc::new(move || {
            LlmSession::new(Box::new(SlowScriptedLlm::new((*scripted).clone(), delay)))
        }),
        ..EngineConfig::default()
    };
    let holly = Holly::spawn(cfg);
    spawn_tool_executor(&holly, unknown_tool);
    let sid = SessionId::new("s1");
    let mut sub = holly.subscribe();

    holly
        .send(InMsg::Prompt {
            session: sid.clone(),
            text: "first".into(),
        })
        .await
        .unwrap();

    // Inject the steering prompt while the first response is still streaming,
    // so it lands in the inbox and is stashed mid-turn.
    tokio::time::sleep(Duration::from_millis(20)).await;
    holly
        .send(InMsg::Prompt {
            session: sid.clone(),
            text: "steer".into(),
        })
        .await
        .unwrap();

    // Count `Done`s and collect text over a window covering both rounds. A fold
    // keeps this a single turn (one `Done`); the old replay behavior produced a
    // second turn (two `Done`s) that ran the steering prompt on its own.
    let mut dones = 0;
    let mut texts = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    while let Ok(Ok(ev)) = tokio::time::timeout_at(deadline, sub.recv()).await {
        match ev {
            OutEvent::Done { session, .. } if session == sid => dones += 1,
            OutEvent::TextDelta { text, session, .. } if session == sid => texts.push(text),
            _ => {}
        }
    }
    assert!(
        texts.iter().any(|t| t == "folded-reply"),
        "the folded turn should produce its reply; got {texts:?}"
    );
    assert_eq!(
        dones, 1,
        "a mid-turn prompt must fold into the live turn (one Done), not replay as a \
         separate turn (two Dones); saw {dones}"
    );
}

/// `Llm` that always emits a tool call, driving the inner LLM→tool loop
/// forever. Counts how many times it was streamed so a test can assert the
/// loop was actually bounded (not merely slow). #177.
struct LoopingLlm {
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl Llm for LoopingLlm {
    async fn stream(&mut self, _req: LlmRequest<'_>) -> anyhow::Result<LlmStream> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(stream_from_response(LlmResponse {
            text: String::new(),
            tool_calls: vec![ToolCall {
                id: "loop".into(),
                name: "unknown-tool".into(),
                input: "{}".into(),
            }],
        }))
    }
}

/// Regression (#177): a model wedged in a tool loop — every reply is another
/// tool call — must be bounded by `MAX_TURNS` within a single prompt. Before
/// the fix the counter bounded *prompts*, not the inner loop, so this ran
/// forever. We assert the engine emits the "maximum turn limit" Error and that
/// the LLM was streamed a bounded number of times.
#[tokio::test]
async fn runaway_tool_loop_is_bounded_within_a_single_prompt() {
    let calls = Arc::new(AtomicUsize::new(0));
    let calls_for_factory = calls.clone();
    let cfg = EngineConfig {
        llm_factory: Arc::new(move || {
            LlmSession::new(Box::new(LoopingLlm {
                calls: calls_for_factory.clone(),
            }))
        }),
        ..EngineConfig::default()
    };
    let holly = Holly::spawn(cfg);
    spawn_tool_executor(&holly, unknown_tool);
    let sid = SessionId::new("s1");
    let mut sub = holly.subscribe();

    holly
        .send(InMsg::Prompt {
            session: sid.clone(),
            text: "spin".into(),
        })
        .await
        .unwrap();

    // The loop must terminate on its own with the turn-limit Error.
    let mut saw_limit = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while let Ok(Ok(ev)) = tokio::time::timeout_at(deadline, sub.recv()).await {
        if let OutEvent::Error {
            session, message, ..
        } = ev
        {
            if session == sid && message.contains("maximum turn limit") {
                saw_limit = true;
                break;
            }
        }
    }
    assert!(
        saw_limit,
        "a runaway tool loop must hit the turn-limit Error instead of spinning forever"
    );
    // 50 iterations run; the 51st trips the cap before streaming. The exact
    // bound isn't the contract, only that it *is* bounded well under any real
    // conversation length.
    assert!(
        calls.load(Ordering::SeqCst) <= 51,
        "inner loop should stop near MAX_TURNS, streamed {} times",
        calls.load(Ordering::SeqCst)
    );
}

/// Regression: a `SetAgent` arriving between tool calls is stashed and
/// applied after the turn. (Any non-Stop command exercises the same stash
/// path; `SetAgent` is a convenient one because its effect — switching the
/// profile — is observable on the next turn.)
#[tokio::test]
async fn setagent_arriving_between_tool_calls_is_stashed_and_applied() {
    // First turn: a tool call (no preamble) so the engine enters the
    // tool-dispatch loop where the second try_recv site lives. The tool is
    // unknown to the registry, which surfaces as a ToolOutput string — the
    // turn completes normally and the stashed SetAgent is then applied.
    let delay = Duration::from_millis(100);
    let scripted = Arc::new(vec![
        LlmResponse {
            text: "".into(),
            tool_calls: vec![entanglement_core::ToolCall {
                id: "t1".into(),
                name: "unknown-tool".into(),
                input: "{}".into(),
            }],
        },
        // Second turn: just text, so we can assert it lands.
        LlmResponse {
            text: "post-setagent-reply".into(),
            tool_calls: vec![],
        },
    ]);
    let mut cfg = EngineConfig {
        llm_factory: Arc::new(move || {
            LlmSession::new(Box::new(SlowScriptedLlm::new((*scripted).clone(), delay)))
        }),
        ..EngineConfig::default()
    };
    // Core carries only the `build` built-in (#201); register a second profile to
    // switch to, so the stashed SetAgent has a real target to resolve.
    cfg.profiles.insert(AgentProfile {
        name: "reviewer".into(),
        description: String::new(),
        mode: AgentMode::Primary,
        system_prompt: "Review the changes.".into(),
        model: None,
        permission: PermissionProfile::new(Permission::Ask),
        tools: None,
        disallowed_tools: Vec::new(),
        can_spawn: None,
        spawnable_agents: None,
    });
    let holly = Holly::spawn(cfg);
    // The tool call is an unknown tool; execution is now a runtime round-trip
    // (#58) so the turn only completes once the executor answers.
    spawn_tool_executor(&holly, unknown_tool);
    let sid = SessionId::new("s1");
    let sub = holly.subscribe();

    holly
        .send(InMsg::Prompt {
            session: sid.clone(),
            text: "first".into(),
        })
        .await
        .unwrap();
    // Inject SetAgent mid-turn. Before ADR-0018 this was silently dropped;
    // the next Prompt would still run under the `build` profile.
    tokio::time::sleep(Duration::from_millis(20)).await;
    // Subscribe BEFORE sending SetAgent so we don't miss the AgentChanged
    // event when the engine replays the stashed command after turn 1 ends.
    let mut sub2 = holly.subscribe();
    holly
        .send(InMsg::SetAgent {
            session: sid.clone(),
            agent: "reviewer".into(),
        })
        .await
        .unwrap();

    // Watch for the AgentChanged event (fires when the stashed SetAgent is
    // replayed after turn 1 completes).
    let mut saw_reviewer = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    while let Ok(Ok(ev)) = tokio::time::timeout_at(deadline, sub2.recv()).await {
        if let OutEvent::AgentChanged { agent, session, .. } = ev {
            if session == sid && agent == "reviewer" {
                saw_reviewer = true;
                break;
            }
        }
    }
    assert!(
        saw_reviewer,
        "stashed SetAgent should have switched the session to the reviewer profile"
    );

    // Now send a real follow-up Prompt; it runs on the still-alive session.
    holly
        .send(InMsg::Prompt {
            session: sid.clone(),
            text: "second".into(),
        })
        .await
        .unwrap();

    // And confirm the second turn's reply also surfaced via the original sub.
    let texts = collect_texts_for(sub, &sid, Duration::from_millis(500)).await;
    assert!(
        texts.iter().any(|t| t == "post-setagent-reply"),
        "second turn (post-stash-replay) should produce its reply; got {texts:?}"
    );
}
