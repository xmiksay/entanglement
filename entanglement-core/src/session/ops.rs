//! Single-shot session ops (#324, ADR-0082 → ADR-0101): `InMsg::Oneshot`'s
//! generic `op` string dispatched here — `run_oneshot` matches on it, no plugin
//! registry. `"compact"` (session compaction via LLM summarization) is the
//! first and only op; an unknown `op` is a recoverable `Error`. Separable from
//! the turn loop (`session/turn.rs`): a oneshot never streams tool calls and
//! never parks — it either completes in one round-trip or fails cleanly.
//!
//! `compact` is **copy-on-write** (ADR-0101): it never mutates the source
//! session. It summarizes the transcript and forks a **successor** seeded with
//! the summary, retiring the source unchanged (ADR-0110, now the one
//! compaction lifecycle — ADR-0205). A botched (truncated) summary is rejected
//! outright (never forked, never mutating) — the source history is always
//! recoverable. Both the summarization (`super::summarize::summarize`) and the
//! fork (`super::fork`) are shared with the automatic overflow path
//! `session/turn.rs` runs (#398, ADR-0103); this module only decides *when* to
//! run one.

use tokio::sync::broadcast;

use super::compaction_request::SessionPrefix;
use super::emit::{emit_turn_done, emit_turn_error, emit_usage};
use super::fork::{fork_successor, Compaction, Forked};
use super::round_inputs::{resolve_specs, resolve_system_prompt};
use super::summarize::{compose_report, summarize, AuxBackend, SummarizeOutcome};
use super::Session;
use crate::protocol::{AgentState, CompactionMode, OutEvent, SessionId, UsagePurpose};
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
) -> Forked {
    let _ = events.send(OutEvent::Status {
        session: session.clone(),
        state: AgentState::Thinking,
    });
    match op.as_str() {
        "compact" => compact_op(session, s, events, cfg, args).await,
        other => {
            emit_turn_error(
                session,
                &s.seq,
                events,
                format!("unknown oneshot op: {other}"),
            );
            Forked::No
        }
    }
}

/// Summarize the whole live history with the active model, announce it via
/// `OutEvent::Compacted`, and fork a successor seeded with the summary. The
/// source `Context` is left **unchanged** and the source session is retired at
/// that point (ADR-0101/0110/0205). A truncated summary
/// (`StopReason::MaxTokens`) is rejected with `Error` and never forked — the
/// source history is always recoverable.
async fn compact_op(
    session: &SessionId,
    s: &mut Session,
    events: &broadcast::Sender<OutEvent>,
    cfg: &EngineConfig,
    args: serde_json::Value,
) -> Forked {
    let instructions = args.get("instructions").and_then(|v| v.as_str());
    let requested_kept = args.get("kept").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
    // Model resolution mirrors turn.rs's request field: a live switch (#218)
    // overrides the profile's pinned model; `None` falls back to the backend's
    // own default.
    let model = s.model.as_deref().or(s.profile.model.as_deref());
    // Resolved exactly as a turn round resolves them, so the session-backend
    // summary request replays the same cached prefix (ADR-0202).
    let specs = resolve_specs(cfg, session, s);
    let system_prompt = resolve_system_prompt(cfg, session, s);
    let prefix = SessionPrefix {
        system_prompt: &system_prompt,
        specs: &specs,
        cache_key: session.0.as_str(),
    };
    // A `summarize` aux-model pin (Issue 5) routes compaction to its own
    // backend; unset, this resolves straight back to the session's.
    let mut aux = AuxBackend::for_summarize(cfg);
    let backend = aux.resolve(&mut *s.llm, model, s.generation);
    // Owned: `backend.model` borrows `s.llm`, and the fork below needs `&mut s`
    // again once summarization is done.
    let model = backend.model.map(str::to_string);

    match summarize(&s.ctx, backend, prefix, requested_kept, instructions).await {
        Ok(SummarizeOutcome {
            summary,
            kept,
            tail_rendered,
            finish,
        }) => {
            // Copy-on-write: the source is never mutated (ADR-0101). The
            // successor's seed is a single flat prompt, so the kept tail
            // (#397, ADR-0102) is composed into it as rendered text.
            let report = compose_report(&summary, kept, tail_rendered.as_deref());
            let fork = fork_successor(
                session,
                s,
                events,
                cfg,
                Compaction {
                    seed: &report,
                    kept,
                    auto: false,
                    mode: CompactionMode::Summary,
                },
            );
            let Some(fork) = fork else {
                emit_turn_error(
                    session,
                    &s.seq,
                    events,
                    "cannot compact: this session cannot fork a successor".to_string(),
                );
                return Forked::No;
            };
            if let Some((_, usage)) = finish {
                // Pricing mirrors turn.rs: model → profile.model → the
                // backend's resolved default (the request field itself stops
                // at the profile, since `None` there means "backend default").
                let priced_model = model.as_deref().or(cfg.default_model.as_deref());
                let cost = priced_model
                    .and_then(|m| cfg.pricing.get(m))
                    .map(|p| p.cost_usd(&usage));
                emit_usage(session, s, events, &usage, cost, UsagePurpose::Compaction);
            }
            // `Done` still closes the op so a one-shot head unblocks, and it is
            // the source's last record — dispatching only afterwards keeps the
            // successor's own events strictly after it on the wire.
            emit_turn_done(session, &s.seq, events);
            fork.dispatch();
            Forked::Yes
        }
        Err(e) => {
            emit_turn_error(session, &s.seq, events, e.to_string());
            Forked::No
        }
    }
}
