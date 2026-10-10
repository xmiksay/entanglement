//! Per-session engine: the conversation loop and the tool-request round-trip to
//! the runtime.
//!
//! Permission dispatch (`Allow`/`Ask`/`Deny`) and the approval wait no longer
//! live here (#59): core batch-emits `OutEvent::ToolExec` for every tool call
//! of a round and parks the turn as explicit [`TurnState`] data (#270,
//! ADR-0061); the runtime tool executor — or any external resolver — answers
//! each call with `InMsg::ToolResult`, in any order. The runtime owns the
//! policy decision and the approval UX (ADR-0003/0010).
//! `update_plan`/`update_tasks` are ordinary runtime state tools too now
//! (#231, ADR-0049) — the engine holds no plan/task state and makes no
//! plan-authority call.
//!
//! Split along the natural seam (#109): [`replay`] reconstructs a session from
//! a persisted log; [`turn`] is the live reasoning turn's per-round setup and
//! retry driver; [`round`] is one streamed attempt plus the ADR-0118
//! ambiguous-stop retry (#436); [`stream`] is the streamed round-trip;
//! [`turn_state`] is the parked-turn state; [`emit`] is the outbound-event
//! helpers; [`ops`] is single-shot ops (#324, ADR-0082); [`summarize`] is the
//! LLM-summarization core `ops` (copy-on-write) and `turn` (in-place
//! auto-compact, #398/ADR-0103) both call; [`compaction_request`] picks its
//! request shape (ADR-0202) and [`transcript`] renders the fallback text;
//! [`round_inputs`] resolves the system prompt + tool specs a round and a
//! compaction share; [`cmd`] is the supervisor→session command set and its
//! capped stash; [`start`] builds and announces a starting session; and
//! [`commands`] applies the loop's state-changing commands.

mod cmd;
mod commands;
mod compaction_request;
mod emit;
mod fork;
mod immediate;
mod invoke_envelope;
mod mode;
mod ops;
mod replay;
mod replay_pending;
mod round;
mod round_inputs;
mod start;
mod state;
mod stream;
mod summarize;
mod summary_attempt;
mod transcript;
mod turn;
mod turn_state;

pub(crate) use cmd::SessionCmd;
pub use state::{Session, SessionUsage};
pub use turn_state::TurnState;

use cmd::{cap_meta_field, stash_or_reject};
use fork::Forked;

use std::collections::VecDeque;

use tokio::sync::{broadcast, mpsc};

use crate::holly::{ActivityRegistry, SeqRegistry};
use crate::protocol::{Agent, AgentState, InMsg, OutEvent, SessionId};
use crate::EngineConfig;
use entanglement_provider::UserId;

use emit::{emit_tool_output, reoffer_pending};
use ops::run_oneshot;
use start::{start_session, SessionStart};
use turn::drive_turn;

/// Runs one session until `Stop` / inbox close. Emits `SessionStarted`, `Idle` status
/// and `AgentChanged` so a head knows the starting agent.
///
/// If `initial_session` is provided, it's used as the starting state (for resume);
/// otherwise, a fresh session is created.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn session_loop(
    session: SessionId,
    mut rx: mpsc::Receiver<SessionCmd>,
    events: broadcast::Sender<OutEvent>,
    cfg: EngineConfig,
    agent: Agent,
    initial_session: Option<Session>,
    parent: Option<SessionId>,
    predecessor: Option<SessionId>,
    user: Option<UserId>,
    // The mode a fresh spawn starts under — the parent's live mode at spawn
    // time (ADR-0207 §6: mode applies to the whole spawn sub-tree), or
    // `DEFAULT_MODE` for a root. Ignored on the resume path (a replayed
    // session already carries the correct value in `s.mode`) — the caller
    // passes `DEFAULT_MODE` there too, mirroring the `None`/`false` it passes
    // for `predecessor`/`user`.
    initial_mode: String,
    seqs: SeqRegistry,
    activity: ActivityRegistry,
    forks: mpsc::Sender<InMsg>,
) {
    let mut s = start_session(
        &session,
        &events,
        &cfg,
        &seqs,
        forks,
        SessionStart {
            agent,
            initial_session,
            parent,
            predecessor,
            user,
            initial_mode,
        },
    );
    let mut stash: VecDeque<SessionCmd> = VecDeque::new();

    // A session resumed mid-turn (#271/#272, ADR-0061): re-offer every pending
    // call — same `request_id`, fresh `seq` — so the tool executor (or an
    // external resolver) answers it exactly like a first offer, then fall into
    // the loop parked. At-least-once by design: a tool that ran before the
    // crash but whose result was never logged runs again. Display `ToolCall`
    // events are not re-emitted — heads rebuild those from the log. A drained
    // tail (every result logged, next round never streamed) has nothing to
    // re-offer; continue the turn directly.
    let mut forked = false;
    if let Some(turn) = s.turn.as_ref() {
        if turn.pending.is_empty() {
            forked = drive_turn(&session, &mut rx, &mut s, &events, &mut stash, &cfg).await
                == Forked::Yes;
        } else {
            let _ = events.send(OutEvent::Status {
                session: session.clone(),
                state: AgentState::Thinking,
            });
            reoffer_pending(&events, &session, turn, &s.agent.name, &s.seq);
        }
    }

    while !forked {
        // Publish settledness for the idle-TTL sweep (#363): `Some(now)` the
        // instant this session is genuinely at rest (about to pop a stash entry
        // or block on `rx.recv()`), `None` while parked on unresolved tool calls
        // (mid-turn or waiting on an approval/question result). Using tokio's
        // clock (not `std::time::Instant`) keeps the sweep test-friendly under a
        // paused/advanced runtime clock.
        activity
            .lock()
            .expect("activity registry mutex poisoned")
            .insert(
                session.clone(),
                s.turn.is_none().then(tokio::time::Instant::now),
            );

        // Pop the stash only when idle *and not paused* (#516, ADR-0208): a
        // command stashed during a live turn replays after the turn ends
        // (ADR-0018). While parked, or while paused, popping a stashed command
        // here would only re-stash it below — a busy loop.
        let cmd = if s.turn.is_none() {
            if s.paused {
                rx.recv().await
            } else if let Some(c) = stash.pop_front() {
                Some(c)
            } else {
                rx.recv().await
            }
        } else if s.paused {
            // Parked and held (#516): suspend the reoffer timer too — a paused
            // session shouldn't keep nagging the runtime executor while held.
            rx.recv().await
        } else {
            // Parked on unresolved tool calls (#274, ADR-0071). Bound the wait:
            // after `reoffer_interval` of silence (no `ToolResult` arriving)
            // re-offer the pending batch — re-emit each `ToolExec` with the same
            // `request_id` and a fresh `seq` — so an in-process offer the runtime
            // executor dropped under outbound-broadcast lag (`RecvError::Lagged`)
            // can't strand the turn until a restart/resume. At-least-once by
            // design; the executor dedupes by `request_id`, so a re-offer to a
            // still-in-flight call is a no-op there, not a double-run. `None`
            // disables the timer (park indefinitely, the pre-#274 behavior).
            match cfg.reoffer_interval {
                Some(interval) => match tokio::time::timeout(interval, rx.recv()).await {
                    Ok(cmd) => cmd,
                    Err(_elapsed) => {
                        if let Some(turn) = s.turn.as_ref() {
                            reoffer_pending(&events, &session, turn, &s.agent.name, &s.seq);
                        }
                        continue;
                    }
                },
                None => rx.recv().await,
            }
        };
        match cmd {
            Some(SessionCmd::Prompt(content)) => {
                if s.turn.is_some() || s.paused {
                    // Mid-turn steering (#182, ADR-0058) or a paused idle
                    // session (#516, ADR-0208): stash it — the next round, or
                    // `Unpause`'s resulting idle pop, folds a stashed prompt
                    // into the live context before the model request.
                    stash_or_reject(
                        &mut stash,
                        SessionCmd::Prompt(content),
                        &session,
                        &events,
                        &s.seq,
                    );
                } else {
                    s.ctx.push_user_content(content);
                    s.turn = Some(TurnState::default());
                    // Flip to busy *before* the round runs, not just at the next
                    // loop top (#363): the top-of-loop publish above ran while
                    // this session was still idle, and `drive_turn` may stream
                    // for a long time — an idle-TTL sweep must never see a stale
                    // "settled" timestamp for a session that just started a turn.
                    activity
                        .lock()
                        .expect("activity registry mutex poisoned")
                        .insert(session.clone(), None);
                    forked = drive_turn(&session, &mut rx, &mut s, &events, &mut stash, &cfg).await
                        == Forked::Yes;
                }
            }
            // Live mode switch (ADR-0207): applied immediately, turn live or
            // paused or not — unlike `SetModel`/`SetGeneration`/
            // `SetToolOverlay` below, a mode is never deferred. Those defer
            // because they touch something a live round is actively using
            // (the backend, generation knobs riding the in-flight request,
            // the advertised tool array) — a mid-round edit there would be
            // incoherent or bust the provider cache. A mode is neither: it is
            // a **label** the runtime's own `perm_modes` fold reads only at
            // the moment it grades the *next* tool call (`tool_runner.rs`),
            // and the model-visible notice is rebuilt from `s.mode` fresh
            // every round (`stream.rs`) rather than pushed into `ctx` — see
            // `mode::mode_notice`'s doc for why, and how that keeps a switch
            // free of the provider prompt-cache miss a mid-session tools/
            // system edit would cost (ADR-0202). So there is nothing live to
            // protect by waiting.
            //
            // This matters concretely for `propose_plan` approval (#560,
            // ADR-0207 §7): the approval's `SetMode` is sent *from inside*
            // the very turn it must reshape, immediately followed by the
            // `ToolResult` that resumes it. Deferring the switch (the old
            // behavior, copied from `SetAgent`'s now-deleted stash — a
            // persona swap genuinely does need to wait, since it rewrites
            // the system prompt) left that continuing turn's next tool call
            // graded under the *old*, plan-denying mode: a plan accepted
            // into `build`/`auto` still had its first edit refused. Applying
            // here means the outer loop's `ModeChanged` broadcast lands
            // before the paired `ToolResult` is even processed, so
            // `tool_runner`'s `perm_modes` fold sees the new mode before it
            // ever grades the next call.
            //
            // A narrowing switch issued mid-turn (e.g. `/mode research` while
            // `build` is running) is strictly safer this way too: it now
            // closes the gap on the very next tool call instead of leaving
            // every call still in flight for the rest of the turn running
            // under the old, wider mode.
            //
            // Paused: a held session has nothing actively grading either —
            // whatever is parked was already dispatched under whatever mode
            // was live at the time, and nothing new dispatches until
            // `ResumeSession` — so applying now vs. after `Unpause` changes
            // nothing observable, and immediate is simpler than one more
            // deferred-command special case. `SetSessionMeta` and the
            // lineage mirror apply immediately too — see `immediate.rs`.
            Some(
                cmd @ (SessionCmd::SetMode(_)
                | SessionCmd::SetSessionMeta(..)
                | SessionCmd::ChildSpawned(_)
                | SessionCmd::ChildClosed(_)),
            ) => {
                let _ = immediate::try_apply(cmd, immediate::fields!(s), &session, &events);
            }
            // A deferrable command (`SetModel`/`SetGeneration`/`SetToolOverlay`/
            // `Oneshot`) arriving during a live turn or a pause is stashed and
            // replays once the turn ends or the hold lifts (stash replay).
            Some(
                cmd @ (SessionCmd::SetModel(..)
                | SessionCmd::SetGeneration(_)
                | SessionCmd::SetToolOverlay(_)
                | SessionCmd::Oneshot(..)),
            ) if s.turn.is_some() || s.paused => {
                stash_or_reject(&mut stash, cmd, &session, &events, &s.seq);
            }
            // Live model/provider switch (#218): re-resolve against the runtime's
            // catalog-backed resolver, rebuild the backend, and retarget the
            // request model + generation + context-window budget — no restart.
            // Deferred during a live turn (stash replay), like `SetMode`.
            Some(SessionCmd::SetModel(provider, model)) => {
                commands::set_model(&mut s, &session, &events, &cfg, provider, model);
            }
            // Live generation-parameter adjustment (#374, ADR-0094): unlike
            // `SetModel`, there is no resolver to fail against, so this always
            // succeeds. Deferred during a live turn (stash replay), like
            // `SetMode`/`SetModel`.
            Some(SessionCmd::SetGeneration(overrides)) => {
                commands::set_generation(&mut s, &session, &events, overrides);
            }
            // Live tool-overlay replacement (#539, ADR-0149): like
            // `SetGeneration` there is nothing to fail against — always
            // succeeds, always confirms with the full effective list. Deferred
            // during a live turn (stash replay) so the advertised tool surface
            // never changes under a round already in flight.
            Some(SessionCmd::SetToolOverlay(entries)) => {
                commands::set_tool_overlay(&mut s, &session, &events, entries);
            }
            Some(SessionCmd::Oneshot(op, args)) => {
                forked =
                    run_oneshot(&session, &mut s, &events, &cfg, op, args).await == Forked::Yes;
            }
            // A result for the parked batch (#270): fold it into context on
            // arrival — arrival order, matching replay's `ToolOutput`-order
            // fold — and continue the turn once the batch drains. No match:
            // stale (late result after a cancel), duplicate, or unknown id —
            // drop it rather than corrupt context. While paused (#516,
            // ADR-0208) the fold still happens — a resolver isn't blocked by a
            // hold, and stashing this would deadlock (the batch could never
            // drain if its own resolution waited on `s.turn` going idle) — but
            // the drained batch does *not* re-enter `drive_turn`: the next
            // model round-trip is exactly what `Paused` holds back, and
            // `Unpause` continues it without a fresh prompt.
            Some(SessionCmd::ToolResult(id, content, is_error, duration_ms, exit_code)) => {
                match s.turn.as_mut().and_then(|t| t.resolve(&id)) {
                    Some((call, envelope)) => {
                        emit_tool_output(
                            &events,
                            &session,
                            &call.id,
                            &call.name,
                            content.clone(),
                            is_error,
                            duration_ms,
                            exit_code,
                            envelope,
                            &s.seq,
                        );
                        s.ctx.push_tool_content(&call.id, content);
                        if !s.paused && s.turn.as_ref().is_some_and(TurnState::is_drained) {
                            forked =
                                drive_turn(&session, &mut rx, &mut s, &events, &mut stash, &cfg)
                                    .await
                                    == Forked::Yes;
                        }
                    }
                    None => {
                        tracing::debug!(request_id = %id, "dropping stale/unknown ToolResult");
                    }
                }
            }
            // Cancel semantics (ADR-0017): a parked turn is cancelled by
            // clearing its state — the committed assistant message and any
            // already-arrived outputs stay in Context. Idle Stop is a no-op
            // (a mid-stream Stop is caught inside the streamed round).
            Some(SessionCmd::Stop) => commands::stop(&mut s, &session, &events),
            // Hold the session at `Paused` (#516, ADR-0208) — see
            // `SessionCmd::Pause`'s doc for what this defers. Idempotent: no
            // duplicate `Status` for an already-paused session.
            Some(SessionCmd::Pause) => commands::pause(&mut s, &session, &events),
            // Lift a hold placed by `Pause` (#516, ADR-0208). A drained-but-
            // undriven parked batch (every `ToolResult` already folded while
            // paused) continues the turn immediately — no new prompt needed;
            // otherwise report the state the session is actually resting in
            // now (`Working` if still parked on unresolved calls, `Done` if
            // idle — matching `Stop`'s resting-state convention, ADR-0139).
            Some(SessionCmd::Unpause) => {
                if commands::unpause(&mut s, &session, &events) {
                    forked = drive_turn(&session, &mut rx, &mut s, &events, &mut stash, &cfg).await
                        == Forked::Yes;
                }
            }
            // Memory eviction without tombstoning (#318, ADR-0077). Drop `Session`
            // (Context/history) and the shared seq counter, then emit the distinct
            // `SessionHibernated` and exit. A parked-on-approval turn is safe: its
            // pending `ToolExec`s live in the embedder's log and resume re-offers
            // them (ADR-0061/0071). A mid-stream turn reaches here via the
            // supervisor's sender-drop (stream cancels, the stashed `Hibernate`
            // pops when idle) — stop-then-hibernate, discarding the uncommitted
            // round exactly as replay drops a text-only tail.
            Some(SessionCmd::Hibernate) => {
                let ts = commands::retire(&session, &seqs, &activity);
                let _ = events.send(OutEvent::SessionHibernated {
                    session: session.clone(),
                    ts,
                });
                return;
            }
            None => break,
        }
    }

    // End of the line, by either route: the inbox closed (`None` above), or a
    // compaction forked this session away and it is now retired (ADR-0205 —
    // the supervisor's `CloseSession` tombstones the id right behind us). Both
    // end the session identically, so the teardown lives here once.
    let ts = commands::retire(&session, &seqs, &activity);
    let _ = events.send(OutEvent::SessionEnded {
        session: session.clone(),
        ts,
    });
}
