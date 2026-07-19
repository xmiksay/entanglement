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
//! never mutating) — the source history is always recoverable. The
//! summarization itself (`super::summarize::summarize`) is shared with the
//! automatic in-place path `session/turn.rs` runs on context overflow (#398,
//! ADR-0103) — this module only decides what happens to the *result*.

use tokio::sync::broadcast;

use super::emit::{emit_turn_done, emit_turn_error, emit_usage, next_seq};
use super::summarize::{compose_report, summarize, SummarizeOutcome};
use super::Session;
use crate::protocol::{AgentState, OutEvent, SessionId};
use crate::EngineConfig;

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
    let instructions = args.get("instructions").and_then(|v| v.as_str());
    let requested_kept = args.get("kept").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
    // Model resolution mirrors turn.rs's request field: a live switch (#218)
    // overrides the profile's pinned model; `None` falls back to the backend's
    // own default.
    let model = s.model.as_deref().or(s.profile.model.as_deref());

    match summarize(
        &s.ctx,
        &mut *s.llm,
        model,
        s.generation,
        requested_kept,
        instructions,
    )
    .await
    {
        Ok(SummarizeOutcome {
            summary,
            kept,
            tail_rendered,
            finish,
        }) => {
            // Copy-on-write: report the summary without mutating the source
            // (ADR-0101). The head forks it into a new session — the fork's
            // seed is a single flat string, so the kept tail (#397, ADR-0102)
            // is composed into the report as rendered text here (unlike the
            // auto in-place path, which preserves it structurally instead).
            let report = compose_report(&summary, kept, tail_rendered.as_deref());
            let _ = events.send(OutEvent::Compacted {
                session: session.clone(),
                seq: next_seq(&s.seq),
                summary: report,
                kept: kept as u64,
                auto: false,
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
            emit_turn_done(session, &s.seq, events);
        }
        Err(e) => emit_turn_error(session, &s.seq, events, e.to_string()),
    }
}
