//! History propagation tests — verify that conversation context survives
//! across turns in the TUI's flow (Prompt → Done → Prompt, no Stop in between).
//! Uses EchoLlm which returns a text summary of the messages it received,
//! making history observable without a real provider.

use std::time::Duration;

use entanglement_core::{EngineConfig, Holly, InMsg, OutEvent, SessionId};

/// Collect events for `sid` until `Done`, with a safety timeout.
async fn collect(
    mut sub: tokio::sync::broadcast::Receiver<OutEvent>,
    sid: &SessionId,
) -> Vec<OutEvent> {
    let mut out = Vec::new();
    loop {
        let Ok(recv) = tokio::time::timeout(Duration::from_secs(3), sub.recv()).await else {
            break;
        };
        match recv {
            Ok(ev) if ev.session() == sid => {
                let done = matches!(ev, OutEvent::Done { .. });
                out.push(ev);
                if done {
                    break;
                }
            }
            Ok(_) => {}
            Err(_) => break,
        }
    }
    out
}

/// Collect `TextDelta` texts for `sid` until the deadline, across as many
/// turns as happen. (Used for overlapping-prompt tests where the second
/// prompt lands before the first turn's Done.)
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

#[tokio::test]
async fn prompt_done_prompt_echoes_full_history() {
    let holly = Holly::spawn(EngineConfig::default());
    let sid = SessionId::new("s1");

    // Turn 1: Prompt("alpha"), await Done.
    let sub1 = holly.subscribe();
    holly
        .send(InMsg::Prompt {
            session: sid.clone(),
            text: "alpha".into(),
        })
        .await
        .unwrap();
    let e1 = collect(sub1, &sid).await;
    let reply1: String = e1
        .iter()
        .filter_map(|e| match e {
            OutEvent::TextDelta { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert!(
        reply1.contains("messages=1"),
        "turn 1 should echo 1 message; got: {reply1}"
    );
    assert!(
        reply1.contains("alpha"),
        "turn 1 should echo 'alpha'; got: {reply1}"
    );

    // Turn 2: Prompt("beta"), await Done. The EchoLlm must now see [User:"alpha", Assistant:echo1, User:"beta"].
    let sub2 = holly.subscribe();
    holly
        .send(InMsg::Prompt {
            session: sid.clone(),
            text: "beta".into(),
        })
        .await
        .unwrap();
    let e2 = collect(sub2, &sid).await;
    let reply2: String = e2
        .iter()
        .filter_map(|e| match e {
            OutEvent::TextDelta { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert!(
        reply2.contains("messages=3"),
        "turn 2 should echo 3 messages (user, assistant, user); got: {reply2}"
    );
    assert!(
        reply2.contains("alpha"),
        "turn 2 should still echo 'alpha' from history; got: {reply2}"
    );
    assert!(
        reply2.contains("beta"),
        "turn 2 should echo 'beta'; got: {reply2}"
    );
}

#[tokio::test]
async fn overlapping_prompt_echoes_prior_history() {
    let holly = Holly::spawn(EngineConfig::default());
    let sid = SessionId::new("s1");
    let sub = holly.subscribe();

    // Send both prompts without waiting for Done in between.
    // The second Prompt lands in the session inbox mid-stream and is
    // stashed per ADR-0018, then replayed after turn 1 finishes.
    holly
        .send(InMsg::Prompt {
            session: sid.clone(),
            text: "alpha".into(),
        })
        .await
        .unwrap();
    holly
        .send(InMsg::Prompt {
            session: sid.clone(),
            text: "beta".into(),
        })
        .await
        .unwrap();

    // Collect all text deltas across both turns (deadline-based).
    let texts = collect_texts_for(sub, &sid, Duration::from_secs(3)).await;
    let combined = texts.join("");
    assert!(
        combined.contains("alpha"),
        "should see 'alpha' in some reply; got: {combined}"
    );
    assert!(
        combined.contains("beta"),
        "should see 'beta' in some reply; got: {combined}"
    );

    // The last reply should be for the stashed Prompt("beta") and should
    // reflect full history (both 'alpha' and 'beta' present, 3+ messages).
    let last = texts.last().expect("at least one reply");
    assert!(
        last.contains("beta"),
        "last reply should be for 'beta'; got: {last}"
    );
    assert!(
        last.contains("alpha"),
        "last reply should still echo 'alpha' from history; got: {last}"
    );
    assert!(
        last.contains("messages=3"),
        "last reply should show 3 messages after stashed replay; got: {last}"
    );
}
