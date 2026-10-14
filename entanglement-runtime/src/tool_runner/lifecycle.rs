//! The executor's lifecycle-folding event loop (issue #712, split out of
//! `tool_runner.rs`): consumes the engine's outbox, folds session lifecycle
//! events into the executor's bookkeeping, and hands each `ToolExec` to the
//! interception ladder ([`super::ladder::route_tool_exec`]) — the loop only
//! routes, every handler runs on its own task.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use entanglement_core::{Agent, AgentState, Holly, OutEvent, SessionId};
use tokio::sync::broadcast::{self, error::RecvError};

use super::ladder::{self, Intercept, LadderCtx};

/// Run the executor loop until the engine's outbox closes. `active` is the
/// shared per-session agent map this loop is the sole writer of.
pub(super) async fn run(
    mut sub: broadcast::Receiver<OutEvent>,
    ctx: &LadderCtx,
    active: &Mutex<HashMap<SessionId, Agent>>,
) {
    // Active agent per session. Folded from lifecycle events, but the fold
    // is a *lossy* broadcast — so it is authoritatively self-healed on every
    // `ToolExec` from the agent name that event carries (#156). See the
    // `ToolExec` arm below. Shared (`Arc<Mutex<..>>`, a param) with the
    // default `ModeResolver` (#311) so it reads the same folded view; this
    // loop is the sole writer, so the brief locks never contend.
    //
    // Per-session *in-flight* request_id dedupe (#274, ADR-0071): the set of
    // `ToolExec` request ids this executor has dispatched but not yet seen
    // resolved. Core arms a re-offer timer while a turn is parked and re-emits
    // the pending batch after a stretch of silence (its recovery for an offer
    // dropped under broadcast lag), so the *same* `ToolExec` can arrive twice
    // — once as the original, once as a re-offer while the first is still
    // running. Running it twice would double-execute a `bash`/`edit`/spawn, so
    // an id still in flight is skipped. An id is dropped again on the
    // `ToolOutput` core emits when the call resolves (its result was folded),
    // so a *later* round that legitimately reuses the same id — core matches
    // by id only within the current round's pending set — is not wrongly
    // skipped. This loop is single-threaded (it routes before spawning the
    // detached handler, and consumes `ToolExec`/`ToolOutput` in broadcast
    // order), so the check is race-free without a lock. Cleared per session on
    // `SessionEnded`.
    let mut in_flight: HashMap<SessionId, HashSet<String>> = HashMap::new();
    // Per-session live tool overlay (#539, ADR-0149), folded from
    // `ToolOverlayChanged` — the dispatch-side mirror of core's
    // `Session::tool_overlay`. Consulted for the Ask/Allow grade override
    // the generic route applies in `dispatch` (an enable entry can
    // override even a mode `deny` for that session, ADR-0207 §8) and
    // dropped wholesale on a mode change (the overlay is mode-scoped).
    // Loop-owned: the per-call overlay entry is resolved before the
    // detached task is spawned, so no sharing is needed. Cleared on
    // `SessionEnded`/`SessionHibernated`.
    let mut overlays: HashMap<SessionId, Vec<entanglement_core::ToolOverlayEntry>> = HashMap::new();
    // Bounds the spawn tree (#76): tracks parent links from lifecycle events
    // and per-root running sub-agent counts. Lives in this single-threaded loop, so the
    // spawn decision below is race-free.
    let mut spawn_guard = crate::subagent::SpawnGuard::new();
    loop {
        match sub.recv().await {
            Ok(OutEvent::SessionStarted {
                session,
                parent,
                predecessor,
                agent,
                ..
            }) => {
                spawn_guard.record_start(session.clone(), parent.clone());
                // A compaction successor takes over its retired
                // predecessor's background work (ADR-0231). Only a
                // compaction sets `predecessor` — a `/fork` copy leaves its
                // source alive and owning. This eager fold keeps a head's
                // `ListOperations` right; a lagged drop of this event is
                // healed by `poll`, which re-runs the move from the
                // supervisor's directory before it reads anything.
                if let Some(p) = &predecessor {
                    ctx.jobs.reassign_owner(p, &session);
                    ctx.scripts.reassign_owner(p, &session);
                    ctx.retained.reassign_owner(p, &session);
                }
                // A head-driven resume (ADR-0112) re-emits `SessionStarted`
                // for a previously-hibernated child (#609, ADR-0162 §4) — a
                // no-op for any other session, since a fresh registration is
                // already `Live` and an untracked id has no entry to update.
                ctx.registry.mark_live(&session);
                let started_agent = ctx
                    .agents
                    .read()
                    .expect("agent-catalog lock poisoned")
                    .get(&agent)
                    .cloned();
                if let Some(p) = started_agent.clone() {
                    active
                        .lock()
                        .expect("active-agent mutex poisoned")
                        .insert(session.clone(), p);
                }
            }
            Ok(OutEvent::AgentChanged { session, agent }) => {
                if let Some(p) = ctx
                    .agents
                    .read()
                    .expect("agent-catalog lock poisoned")
                    .get(&agent)
                    .cloned()
                {
                    active
                        .lock()
                        .expect("active-agent mutex poisoned")
                        .insert(session, p);
                }
            }
            // The session's permission mode changed (ADR-0207 stage 4):
            // fold the same way `active` folds `AgentChanged`, so
            // `ModeResolver` reads the current mode. Fires once at
            // session start (mirroring `AgentChanged`) and again on every
            // `InMsg::SetMode`. The session's own tool overlay is
            // mode-scoped (ADR-0207 §8, "the overlay becomes mode-scoped")
            // — it is dropped on any *actual* mode change, since an entry
            // enabled under one mode carries no meaning in another; a
            // duplicate `ModeChanged` for the same value (replay, a
            // no-op `SetMode`) leaves it untouched.
            Ok(OutEvent::ModeChanged { session, mode }) => {
                let previous = ctx
                    .perm_modes
                    .lock()
                    .expect("permission-mode mutex poisoned")
                    .insert(session.clone(), mode.clone());
                if previous.as_deref() != Some(mode.as_str()) {
                    overlays.remove(&session);
                }
            }
            // A hibernated session (#318) tore down just like an ended one, so
            // its executor-side bookkeeping is equally moot — release it. Its
            // persisted "always" grants survive; a resume rebuilds the rest.
            // The session's live tool overlay changed (#539, ADR-0149):
            // mirror core's full-replacement semantics — an empty list
            // clears the entry entirely.
            Ok(OutEvent::ToolOverlayChanged { session, entries }) => {
                // An enable never touches the advertised array (ADR-0226):
                // it only changes how a call is graded.
                super::overlay_warn::warn_inert_entries(&session, overlays.get(&session), &entries);
                if entries.is_empty() {
                    overlays.remove(&session);
                } else {
                    overlays.insert(session, entries);
                }
            }
            Ok(ev @ OutEvent::SessionEnded { .. })
            | Ok(ev @ OutEvent::SessionHibernated { .. }) => {
                let session = ev
                    .session()
                    .cloned()
                    .expect("SessionEnded/SessionHibernated always carry a session");
                // Fold the lifecycle transition into the agent registry
                // (#609, ADR-0162 §4) so `agent_send` can refuse a closed
                // or hibernated child instead of letting the supervisor's
                // lazy-`Prompt` path silently respawn it blank. A no-op
                // for a session this registry never tracked.
                if matches!(ev, OutEvent::SessionHibernated { .. }) {
                    ctx.registry.mark_hibernated(&session);
                } else {
                    ctx.registry.mark_closed(&session);
                }
                // Drop the closed session's in-memory grants (#174); persisted
                // "always" grants survive.
                ctx.grants.forget_session(&session);
                // The live tool overlay dies with the session (#539) — a
                // resume replays core's `ToolOverlayChanged` records, which
                // re-emit on the broadcast and re-fold here.
                overlays.remove(&session);
                // Its in-flight tool bookkeeping is moot once the session ends.
                ctx.cancels.forget_session(&session);
                // Unwind the cards of its turn (ADR-0232): a parked spawn
                // would otherwise pin its `max_agents` slot forever, and a
                // resume re-offers a parked `ToolExec` (the dedupe set is
                // cleared below), which parks a fresh card. Unlike a `Stop`,
                // a background script's card stays: the script outlives the
                // session (a compaction successor inherits it, ADR-0231) and
                // nothing re-offers it.
                ctx.pending.stop_attached(&session);
                // Drop the re-offer dedupe set (#274): its request ids can
                // never recur once the session is gone.
                in_flight.remove(&session);
                // The active-skill posture tracking is moot once the
                // session is gone too (#400) — no `Done` will follow to
                // clear it otherwise.
                ctx.active_skill
                    .lock()
                    .expect("active-skill mutex poisoned")
                    .remove(&session);
                // No sandbox cache to drop any more (ADR-0207 §6, stage
                // 5b): confinement reads `perm_modes` directly, and that
                // map's own entry is left in place like `active`'s — a
                // resume re-emits `ModeChanged` before anything reads it.
                // The plan-file staleness binding (#513) is moot too.
                ctx.plan_files.forget_session(&session);
                ctx.workdirs.forget(&session);
                // And its surface snapshot, delivered-schema set and
                // `<env>` date (ADR-0226, ADR-0202 §5) — a resume
                // re-snapshots at its first round.
                ctx.advertising.forget(&session);
                // The loop-breaker's last-call tracker (#560, ADR-0196
                // §6) is equally session-scoped — nothing to break a
                // loop against once the session is gone.
                ctx.validation.forget(&session);
                // The repeat-denial tracker (ADR-0207 §11) is per-turn
                // scoped, so it's moot once the session itself is gone.
                ctx.denials.clear(&session);
            }
            // A skill's "active" posture scopes one model turn (#400,
            // ADR-0106; posture-only since ADR-0194): clear it here so a
            // later turn can `load_skill` a different one (or none)
            // cleanly, and tell any listening head via
            // `OutEvent::SkillActive { skill_id: None, .. }`. The
            // repeat-denial tracker (ADR-0207 §11) shares this same
            // per-turn scope.
            Ok(OutEvent::Done { session, .. }) => {
                clear_active_skill(&ctx.holly, &ctx.active_skill, &session);
                ctx.denials.clear(&session);
            }
            // A `Stop` that lands while a batch is parked unwinds with no
            // `ToolResult`/`ToolOutput` for its still-running calls (#448):
            // core clears the parked turn state and emits a terminal
            // `Status` without ever resolving them, so the #274 dedupe set
            // above would otherwise leak their request ids for the rest of
            // the session's life. Core only ever sends `Idle` on session
            // start (before any `ToolExec`, so the set is already empty) or
            // — since ADR-0139 — `Done` on a `Stop` (never mid-turn), so
            // dropping the session's whole set is exactly "no call is in
            // flight any more", not an approximation. `Idle` is kept for
            // the genuine start case; both states fold to the same cleanup.
            Ok(OutEvent::Status {
                session,
                state: AgentState::Idle | AgentState::Done,
                ..
            }) => {
                in_flight.remove(&session);
            }
            // A resolved call (#274): core folded its result and emitted this
            // `ToolOutput`, so the id is no longer in flight — drop it from the
            // dedupe set. This frees the id for a later round to reuse (core
            // matches by id only within a round's pending set) while keeping an
            // *unresolved* in-flight call guarded against a double-run re-offer.
            Ok(OutEvent::ToolOutput {
                session,
                request_id,
                ..
            }) => {
                if let Some(set) = in_flight.get_mut(&session) {
                    set.remove(&request_id);
                }
            }
            // The parked `ToolExec` seq is deliberately ignored (#157): every
            // event the runtime authors around this call (an approval
            // `ToolRequest`/`UserQuestion`, a `Plan`/`TaskList` snapshot, a
            // `FileChange`) mints a fresh per-session seq via
            // `Holly::emit_for_session`, so `(session, seq)` stays unique.
            Ok(OutEvent::ToolExec {
                session,
                seq,
                request_id,
                tool,
                input,
                agent,
                envelope,
                workdir,
            }) => {
                // Idempotence for core's re-offer timer (#274, ADR-0071):
                // skip a request id whose call is still in flight. Core
                // re-offers a parked batch after silence to recover an offer
                // dropped under broadcast lag; a re-offer of a call this
                // executor is already running must not run a second time. The
                // first offer records the id; a re-offer while it is unresolved
                // is a no-op. The id is dropped on the resolving `ToolOutput`
                // below, so a later round reusing it still dispatches.
                if !in_flight
                    .entry(session.clone())
                    .or_default()
                    .insert(request_id.clone())
                {
                    tracing::debug!(
                        %request_id,
                        "skipping re-offered ToolExec (still in flight)"
                    );
                    continue;
                }
                // Authoritative self-heal (#156): the emitting session's
                // active agent rides on the `ToolExec` itself, so resolve it
                // from the registry and overwrite the folded entry *before*
                // any permission decision (spawn gating still reads
                // `active`; sandboxing reads
                // `perm_modes` directly, ADR-0207 §6). The lifecycle
                // fold above is a lossy broadcast — under burst a dropped
                // `SessionStarted`/`AgentChanged` would leave a restricted
                // session unseen and (pre-#156) fail *open*. This makes the
                // leaf's gate authoritative regardless of that drop; the
                // fail-closed `ModeResolver` default (an unseen session's
                // mode) covers only the residual unknown case.
                if let Some(p) = ctx
                    .agents
                    .read()
                    .expect("agent-catalog lock poisoned")
                    .get(&agent)
                    .cloned()
                {
                    active
                        .lock()
                        .expect("active-agent mutex poisoned")
                        .insert(session.clone(), p);
                }
                // The tool mask is retired (ADR-0207 §8, "the mask
                // machinery is deleted"): every tool advertised is
                // dispatchable, graded by the session's permission mode
                // instead of withheld by an agent allowlist. The
                // routes below are a `match` (mutually exclusive), so
                // adding one is a compiler-checked exhaustiveness change,
                // not an ordering hazard. Each handler runs on its own
                // task; the loop only routes.
                // Relative paths resolve against the call's workdir (#764):
                // rewritten root-relative before any route grades them.
                ctx.workdirs.observe(&session, workdir.as_deref());
                let wd = workdir.as_deref().map(std::path::Path::new);
                let input =
                    crate::workdir::rewrite(&ctx.plan_root, wd, &tool, &input).unwrap_or(input);
                let route = Intercept::classify(&tool);
                tracing::trace!(
                    %tool,
                    ?route,
                    bypasses_permission = route.bypasses_permission(),
                    "routing tool exec"
                );
                ladder::route_tool_exec(
                    ctx,
                    &mut spawn_guard,
                    &overlays,
                    route,
                    envelope,
                    session,
                    seq,
                    request_id,
                    tool,
                    input,
                )
                .await;
            }
            Ok(_) => {}
            // A lagging executor drops broadcast events; the affected turn
            // stays parked, but that's preferable to executing stale calls.
            Err(RecvError::Lagged(n)) => {
                tracing::warn!(skipped = n, "tool executor lagged; some ToolExec dropped");
            }
            Err(RecvError::Closed) => break,
        }
    }
}

/// Clear `session`'s skill-active posture (#400, ADR-0106) — the turn's
/// `Done`, the natural end of a skill's scope. A no-op (no wire event) when
/// no skill was active, matching [`activate_skill`]'s "only tell a head about
/// a real change" shape.
fn clear_active_skill(
    holly: &Holly,
    active_skill: &Arc<Mutex<HashSet<SessionId>>>,
    session: &SessionId,
) {
    if active_skill
        .lock()
        .expect("active-skill mutex poisoned")
        .remove(session)
    {
        holly.emit_for_session(session, |seq| OutEvent::SkillActive {
            session: session.clone(),
            seq,
            skill_id: None,
            allowed_tools: None,
        });
    }
}
