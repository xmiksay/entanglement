//! Live action narrator (#635, deferred from ADR-0154's "Consequences" —
//! ledger row 15). The `narrate` purpose was floated during Issue 5 and
//! deferred as "a stream concern, not an LLM call"; this module is the
//! reconsideration: an aux LLM call *is* the simplest way to turn a raw tool
//! call into a short human phrase, and `Session.action` (ADR-0151) already had
//! no in-tree producer to write it.
//!
//! On every [`OutEvent::ToolCall`] the engine broadcasts (display-only,
//! emitted once per call before execution), asks the aux `narrate` LLM for a
//! short present-tense phrase describing what the agent is doing and sends it
//! back via `InMsg::SetSessionMeta { action: Some(..), .. }` — the
//! mid-turn-mutable half of session display metadata. Purely runtime-side,
//! mirroring `session_title.rs`: core never sees the aux `Llm`, and an unset
//! `narrate` pin falls back to the primary model exactly like
//! `session_title`'s no-pin case.
//!
//! Best-effort throughout: a failed/empty generation is logged and dropped —
//! the action text simply doesn't update for that tool call. At most one
//! narration call is in flight per session at a time (a `HashSet` guard,
//! cleared when the task finishes): a burst of tool calls skips narrating the
//! ones that land while a call is already pending, rather than piling up aux
//! requests behind a slow model.
//!
//! Behind the `provider` feature, like `session_title.rs`: the aux `Llm` is a
//! provider-backed client, so a lean-library build compiles the narrator out.

use std::collections::HashSet;

use entanglement_core::{Holly, InMsg, LlmEvent, LlmRequest, Message, OutEvent, SessionId};
use futures::StreamExt;
use tokio::sync::broadcast::error::RecvError;

use crate::aux_llm::AuxLlmRegistry;
use crate::config::aux_models::Purpose;

/// The system prompt for the narrator: short, opinionated, asks for a
/// present-tense phrase describing a single tool call. Display-only metadata,
/// so the model's output never reaches the turn loop or tool execution.
const NARRATE_SYSTEM: &str = "\
You describe, in a short present-tense phrase, what a coding agent is doing \
right now based on the tool call it just made. Reply with ONLY the phrase: \
2-6 words, no quotes, no trailing punctuation, no preface. Example tool calls \
→ phrases:\n\
- read(\"src/main.rs\") → \"Reading src/main.rs\"\n\
- bash(\"cargo test\") → \"Running cargo test\"\n\
- edit(\"src/lib.rs\") → \"Editing src/lib.rs\"\n\
- grep(\"TODO\") → \"Searching for TODO\"";

/// Cap on the tool-input text fed to the narrator model: a huge `write`/`edit`
/// payload's tail rarely changes the gist, and capping keeps the one-shot
/// cheap (the whole point of an aux model).
const NARRATE_INPUT_CHAR_CAP: usize = 500;

/// Cap on the generated action length: a model that ignores the system prompt
/// and rambles gets truncated rather than flooding the session status line.
const NARRATE_OUTPUT_CHAR_CAP: usize = 60;

/// Spawns the action narrator (#635). Subscribes to the engine's outbound
/// `OutEvent` fan-out and, on every `ToolCall`, spawns a detached task that
/// calls the aux `narrate` LLM and sends the result back as `SetSessionMeta`.
/// The outer task is aborted at shutdown alongside the other runtime
/// responders (`main.rs`); each inner per-call task is tracked in a `JoinSet`
/// rather than a bare detached `tokio::spawn` (mirrors `session_title.rs`) —
/// one still holding its own `Holly` clone while parked on a throttled/slow
/// aux call would otherwise keep the engine's channels open indefinitely.
#[cfg(feature = "provider")]
pub fn spawn_action_narrator(
    holly: &Holly,
    registry: AuxLlmRegistry,
) -> tokio::task::JoinHandle<()> {
    let holly = holly.clone();
    tokio::spawn(async move {
        let mut outbound = holly.subscribe();
        // Sessions with a narration call currently in flight — a later
        // `ToolCall` for the same session is skipped rather than queued, so a
        // burst never piles up aux requests behind a slow model.
        let mut narrating: HashSet<SessionId> = HashSet::new();
        let mut inflight: tokio::task::JoinSet<SessionId> = tokio::task::JoinSet::new();
        loop {
            tokio::select! {
                // Reap finished per-call tasks so `inflight`/`narrating` don't
                // grow unbounded over a long-running process; guarded so this
                // branch never fires (and busy-loops) while the set is empty.
                Some(done) = inflight.join_next(), if !inflight.is_empty() => {
                    if let Ok(session) = done {
                        narrating.remove(&session);
                    }
                }
                ev = outbound.recv() => {
                    match ev {
                        Ok(OutEvent::ToolCall { session, tool, input, .. }) => {
                            if narrating.contains(&session) {
                                continue;
                            }
                            narrating.insert(session.clone());
                            let holly = holly.clone();
                            let registry = registry.clone();
                            let done_session = session.clone();
                            // Tracked (not bare-detached): the narrator must not
                            // block the outbound fan-out (a slow aux model would
                            // stall every later `OutEvent`), but the task must
                            // still be reachable for abort at shutdown. A
                            // failure is logged + dropped.
                            inflight.spawn(async move {
                                let action = match generate_action(&registry, &tool, &input).await {
                                    Ok(a) => a,
                                    Err(e) => {
                                        tracing::debug!(
                                            "narrate: generation failed for {}: {e:#}",
                                            session
                                        );
                                        return done_session;
                                    }
                                };
                                if let Some(action) = action {
                                    let _ = holly
                                        .send(InMsg::SetSessionMeta {
                                            session,
                                            name: None,
                                            action: Some(action),
                                            if_unset: false,
                                        })
                                        .await;
                                }
                                done_session
                            });
                        }
                        Ok(_) => {}
                        Err(RecvError::Lagged(_)) => continue,
                        Err(RecvError::Closed) => break,
                    }
                }
            }
        }
    })
}

/// Ask the aux `narrate` LLM for a short action phrase describing `tool`
/// called with `input`. Returns `Ok(None)` when the model returned no usable
/// text (empty / only whitespace after trimming); `Err` for a stream/transport
/// failure. The input is capped at [`NARRATE_INPUT_CHAR_CAP`] and the output
/// at [`NARRATE_OUTPUT_CHAR_CAP`].
#[cfg(feature = "provider")]
async fn generate_action(
    registry: &AuxLlmRegistry,
    tool: &str,
    input: &str,
) -> anyhow::Result<Option<String>> {
    let capped_input: String = input.chars().take(NARRATE_INPUT_CHAR_CAP).collect();
    let prompt = format!("{tool}({capped_input})");
    let messages = [Message::user(&prompt)];
    let req = LlmRequest {
        system: NARRATE_SYSTEM,
        model: None,
        messages: &messages,
        tools: &[],
        generation: None,
        // One-shot aux request: a distinct prefix, so no session cache key.
        cache_key: None,
    };
    let mut llm = registry.resolve(Purpose::Narrate);
    let mut stream = llm.stream(req).await?;
    let mut text = String::new();
    while let Some(ev) = stream.next().await {
        match ev {
            Ok(LlmEvent::Text(delta)) => text.push_str(&delta),
            Ok(_) => {}
            Err(e) => return Err(e),
        }
    }
    let action = clean_action(&text);
    Ok((!action.is_empty()).then_some(action))
}

/// Normalize a raw model reply into an action phrase: collapse inner
/// whitespace to single spaces, drop trailing punctuation, strip surrounding
/// quotes, cap at [`NARRATE_OUTPUT_CHAR_CAP`] chars on a word boundary. An
/// empty/whitespace-only result stays empty (the caller treats it as "no
/// action").
fn clean_action(raw: &str) -> String {
    let mut s = raw.trim().to_string();
    // Collapse internal whitespace (a streaming model may emit newlines).
    s = s.split_whitespace().collect::<Vec<_>>().join(" ");
    // Drop a trailing sentence terminator; keep internal punctuation.
    while let Some(stripped) = s.strip_suffix(|c: char| matches!(c, '.' | ',' | ';' | ':' | '!')) {
        s = stripped.trim_end().to_string();
    }
    // Strip surrounding quotes if the whole phrase is wrapped in them. The
    // curly-quote pair is multi-byte, so strip from the front + back by char
    // rather than by byte index.
    let stripped = |s: &str| -> Option<String> {
        let (open, close) = (s.chars().next()?, s.chars().last()?);
        let matched =
            (open == close && (open == '"' || open == '\'')) || (open == '“' && close == '”');
        if matched && s.chars().count() >= 2 {
            let inner: String = s.chars().skip(1).take(s.chars().count() - 2).collect();
            Some(inner.trim().to_string())
        } else {
            None
        }
    };
    if let Some(inner) = stripped(&s) {
        s = inner;
    }
    // Cap on a word boundary so the phrase doesn't end mid-word.
    if s.chars().count() > NARRATE_OUTPUT_CHAR_CAP {
        let truncated: String = s.chars().take(NARRATE_OUTPUT_CHAR_CAP).collect();
        s = match truncated.rfind(' ') {
            Some(idx) => truncated[..idx].trim_end().to_string(),
            None => truncated.trim_end().to_string(),
        };
    }
    s
}

#[cfg(all(test, feature = "provider"))]
mod tests {
    use super::*;

    #[test]
    fn clean_action_normalizes_common_model_quirks() {
        // Collapses internal whitespace + newlines.
        assert_eq!(clean_action("Reading\nsrc/main.rs"), "Reading src/main.rs");
        // Drops trailing punctuation.
        assert_eq!(clean_action("Running cargo test."), "Running cargo test");
        assert_eq!(clean_action("Editing lib.rs!"), "Editing lib.rs");
        // Strips surrounding quotes.
        assert_eq!(clean_action("\"Reading main.rs\""), "Reading main.rs");
        assert_eq!(clean_action("“Reading main.rs”"), "Reading main.rs");
        // Empty / whitespace-only → empty.
        assert_eq!(clean_action("   "), "");
        assert_eq!(clean_action(""), "");
    }

    #[test]
    fn clean_action_caps_on_a_word_boundary() {
        let long = "Doing an excessively long and rambling thing that goes well past the cap";
        let cleaned = clean_action(long);
        assert!(cleaned.chars().count() <= NARRATE_OUTPUT_CHAR_CAP);
        assert!(!cleaned.ends_with(' '), "no trailing space after cap");
        // The cap lands on a word boundary, not mid-word.
        assert!(long.starts_with(&cleaned));
    }
}
