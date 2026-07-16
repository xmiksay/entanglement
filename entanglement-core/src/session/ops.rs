//! Single-shot session ops (#324, ADR-0082 → ADR-0101): `InMsg::Oneshot`'s
//! generic `op` string dispatched here — `run_oneshot` matches on it, no plugin
//! registry. `"compact"` (session compaction via LLM summarization) is the
//! first and only op; an unknown `op` is a recoverable `Error`. Separable from
//! the turn loop (`session/turn.rs`): a oneshot never streams tool calls and
//! never parks — it either completes in one round-trip or fails cleanly.
//!
//! `compact` is **copy-on-write** (ADR-0101): it never mutates the source
//! session. It summarizes the transcript and emits `OutEvent::Compacted`
//! carrying the summary — a *report* ("summary ready, source untouched"), not a
//! confirmation of mutation. The head that issued the compaction forks the
//! summary into a new session; the original stays idle, intact, independently
//! resumable. A botched (truncated) summary is rejected outright (never forked,
//! never mutating) — the source history is always recoverable.

use tokio::sync::broadcast;

use super::emit::{emit_turn_error, emit_usage, next_seq};
use super::Session;
use crate::protocol::{AgentState, OutEvent, SessionId};
use crate::EngineConfig;
use entanglement_provider::{
    GenerationParams, Llm, LlmEvent, LlmRequest, Message, MessageRole, StopReason, Usage,
};
use futures::StreamExt;

/// Per-tool-message transcript cap (head+tail chars) fed into the compaction
/// prompt, so one oversized tool output doesn't blow the summarizer's own
/// context window.
const TRANSCRIPT_TOOL_MESSAGE_CAP: usize = 2_000;

/// Dispatch a session-scoped one-shot op. Emits `Status::Thinking` up front —
/// every op is a synchronous round-trip from the caller's point of view, so
/// this mirrors `drive_turn`'s opening status flip.
pub(crate) async fn run_oneshot(
    session: &SessionId,
    s: &mut Session,
    events: &broadcast::Sender<OutEvent>,
    cfg: &EngineConfig,
    op: String,
    args: serde_json::Value,
) {
    let _ = events.send(OutEvent::Status {
        session: session.clone(),
        state: AgentState::Thinking,
    });
    match op.as_str() {
        "compact" => compact_op(session, s, events, cfg, args).await,
        other => emit_turn_error(
            session,
            &s.seq,
            events,
            format!("unknown oneshot op: {other}"),
        ),
    }
}

/// Summarize the whole live history with the active model and emit it via
/// `OutEvent::Compacted` — a **report** ("summary ready, source untouched"),
/// not a mutation (ADR-0101). The source `Context` is left **unchanged**: the
/// head that issued the compaction forks the summary into a new session. A
/// truncated summary (`StopReason::MaxTokens`) is rejected with `Error` and
/// never emitted — the source history is always recoverable.
async fn compact_op(
    session: &SessionId,
    s: &mut Session,
    events: &broadcast::Sender<OutEvent>,
    cfg: &EngineConfig,
    args: serde_json::Value,
) {
    if s.ctx.messages().is_empty() {
        emit_turn_error(
            session,
            &s.seq,
            events,
            "cannot compact: no conversation history".to_string(),
        );
        return;
    }

    let instructions = args.get("instructions").and_then(|v| v.as_str());
    let transcript = render_transcript(s.ctx.messages());

    // Guard an oversized transcript (#178, ADR-0101): if the rendered input
    // alone already blows the source session's context budget, shipping it
    // would just burn a paid round-trip and 4xx at the provider. Reject before
    // the request — same posture as `turn.rs`'s window-overrun guard.
    let transcript_tokens = estimate_tokens(&transcript);
    if transcript_tokens > s.ctx.limit() {
        emit_turn_error(
            session,
            &s.seq,
            events,
            format!(
                "cannot compact: transcript (~{transcript_tokens} tokens) exceeds \
                 the {}-token context budget — start a new session or shorten the \
                 conversation",
                s.ctx.limit()
            ),
        );
        return;
    }

    let mut prompt = format!(
        "Summarize the conversation transcript below so it can fully replace \
         the conversation history while a coding agent continues the work. \
         Preserve: the user's goals, decisions made, files/paths touched, \
         commands run, and outstanding next steps. Be concise but complete.\n\n\
         {}",
        transcript
    );
    if let Some(extra) = instructions {
        prompt.push_str(&format!("\n\nAdditional instructions: {extra}"));
    }

    const SYSTEM: &str = "You are a summarization assistant compacting a coding \
                          agent's conversation history into a dense, information-\
                          preserving summary.";
    let messages = [Message::user(prompt)];
    // Model resolution mirrors turn.rs's request field: a live switch (#218)
    // overrides the profile's pinned model; `None` falls back to the backend's
    // own default.
    let model = s.model.as_deref().or(s.profile.model.as_deref());

    match oneshot_text(&mut *s.llm, SYSTEM, &messages, model, s.generation).await {
        Ok((summary, finish)) => {
            // Refuse a truncated summary (ADR-0101, mirrors turn.rs:221-229):
            // a `max_tokens`-cut-off fragment would fork a useless new session
            // (or, under the old in-place design, destroy the live history).
            // The source `Context` is never touched either way.
            if let Some((Some(StopReason::MaxTokens), _)) = &finish {
                emit_turn_error(
                    session,
                    &s.seq,
                    events,
                    "compaction failed: the summary was truncated (stop reason: \
                     max_tokens) — refusing to fork a cut-off summary; the \
                     original session is unchanged"
                        .to_string(),
                );
                return;
            }
            // Copy-on-write: report the summary without mutating the source
            // (ADR-0101). The head forks it into a new session.
            let _ = events.send(OutEvent::Compacted {
                session: session.clone(),
                seq: next_seq(&s.seq),
                summary,
                kept: 0,
            });
            if let Some((_, usage)) = finish {
                // Pricing mirrors turn.rs: model → profile.model → the
                // backend's resolved default (the request field itself stops
                // at the profile, since `None` there means "backend default").
                let priced_model = model.or(cfg.default_model.as_deref());
                let cost = priced_model
                    .and_then(|m| cfg.pricing.get(m))
                    .map(|p| p.cost_usd(&usage));
                emit_usage(session, s, events, &usage, cost);
            }
            let _ = events.send(OutEvent::Done {
                session: session.clone(),
                seq: next_seq(&s.seq),
            });
            let _ = events.send(OutEvent::Status {
                session: session.clone(),
                state: AgentState::Done,
            });
        }
        Err(e) => emit_turn_error(session, &s.seq, events, e.to_string()),
    }
}

/// Run one tool-less, non-streamed-to-the-UI completion: build the request,
/// drain the stream concatenating `Text` chunks, and return the assembled text
/// plus the `Finish` payload (for usage/cost). Reuses the session's live `llm`
/// — sound only because the caller (the stash gate in `session_loop`)
/// guarantees no turn is in flight.
async fn oneshot_text(
    llm: &mut dyn Llm,
    system: &str,
    messages: &[Message],
    model: Option<&str>,
    generation: Option<GenerationParams>,
) -> anyhow::Result<(String, Option<(Option<StopReason>, Usage)>)> {
    let req = LlmRequest {
        system,
        model,
        messages,
        tools: &[],
        generation,
    };
    let mut stream = llm.stream(req).await?;
    let mut text = String::new();
    let mut finish = None;
    while let Some(ev) = stream.next().await {
        match ev? {
            LlmEvent::Text(delta) => text.push_str(&delta),
            LlmEvent::Finish { stop_reason, usage } => finish = Some((stop_reason, usage)),
            _ => {}
        }
    }
    Ok((text, finish))
}

/// Rough token estimate for an arbitrary string, mirroring
/// `Context::estimated_tokens`'s `CHARS_PER_TOKEN` heuristic (3.5 chars/token).
/// Used to pre-flight the compaction input against the context budget before
/// burning a paid round-trip the provider would 4xx.
fn estimate_tokens(text: &str) -> usize {
    let chars = text.chars().count();
    ((chars as f32) / 3.5).ceil() as usize
}

/// Render the history as a plain-text transcript for the summarization prompt.
/// Each `Tool`-role message beyond [`TRANSCRIPT_TOOL_MESSAGE_CAP`] chars is
/// truncated head+tail so one oversized tool output can't blow the
/// summarizer's own context window.
fn render_transcript(messages: &[Message]) -> String {
    let mut out = String::new();
    for msg in messages {
        let role = match msg.role {
            MessageRole::User => "user",
            MessageRole::Assistant => "assistant",
            MessageRole::Tool => "tool",
        };
        let text = msg.text();
        let body = if msg.role == MessageRole::Tool {
            truncate_head_tail(&text, TRANSCRIPT_TOOL_MESSAGE_CAP)
        } else {
            text
        };
        out.push_str(&format!("[{role}]\n{body}\n\n"));
    }
    out
}

/// Truncate `text` to at most `cap` chars, keeping the first and last `cap/2`
/// chars with a marker in between. A no-op under the cap.
fn truncate_head_tail(text: &str, cap: usize) -> String {
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= cap {
        return text.to_string();
    }
    let half = cap / 2;
    let head: String = chars[..half].iter().collect();
    let tail: String = chars[chars.len() - half..].iter().collect();
    let dropped = chars.len() - cap;
    format!("{head}\n... [{dropped} chars truncated] ...\n{tail}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_head_tail_is_a_noop_under_the_cap() {
        assert_eq!(truncate_head_tail("short", 100), "short");
    }

    #[test]
    fn truncate_head_tail_keeps_head_and_tail() {
        let text = "a".repeat(50) + &"b".repeat(50);
        let truncated = truncate_head_tail(&text, 40);
        assert!(truncated.starts_with(&"a".repeat(20)));
        assert!(truncated.ends_with(&"b".repeat(20)));
        assert!(truncated.contains("truncated"));
    }

    #[test]
    fn render_transcript_truncates_only_oversized_tool_messages() {
        let messages = vec![
            Message::user("short user text"),
            Message::tool("t1", "x".repeat(5_000)),
        ];
        let out = render_transcript(&messages);
        assert!(out.contains("[user]\nshort user text"));
        assert!(out.contains("truncated"));
        assert!(!out.starts_with("[tool]"));
    }
}
