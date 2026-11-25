# 0071. In-process parked-turn re-offer timer + executor request_id dedupe

- Status: Accepted
- Date: 2026-07-15
- Refines the parked-turn recovery of [0061](0061-parked-turn-state-batch-tool-resolution.md); pairs with the lag-proof decision delivery of [0070](0070-authoritative-tool-exec-profile-and-fail-closed-fallback.md). Supersedes the closed #154. Part of #153 (pre-`serve` hardening). Issue #274.

## Context

`OutEvent::ToolExec` rides the outbound `broadcast` channel, which is lossy by
design: a subscriber that falls behind gets `RecvError::Lagged(n)` and skips `n`
events. The runtime tool executor is one such subscriber. On a lag it logs and
drops the `ToolExec` (`tool_runner.rs`), and the affected turn — parked as
[`TurnState`](0061-parked-turn-state-batch-tool-resolution.md) waiting on that
call's `ToolResult` — stays parked with no in-process recovery.

ADR-0061 already made this **restart-recoverable, not a permanent deadlock**: a
suspended turn is durable serde state, and `Holly::resume` re-offers every
pending call (same `request_id`, fresh `seq`) when an embedder replays the log.
That resume path is the mitigation of record for a crash. What it does *not*
cover is a running process that dropped an offer under transient broadcast
pressure — there, nothing re-drives the call until someone restarts and resumes.
A multi-client `serve` head (#153) raises that broadcast pressure and makes the
`Lagged` drop likelier, so the in-process gap is worth closing before `serve`.

## Decision

**Arm a re-offer timer while a turn is parked, and make the executor idempotent
by `request_id` so a re-offer can never double-execute.**

The two halves are inseparable — the timer is *only* sound with the dedupe.

### Core: the re-offer timer (`session.rs`)

- `EngineConfig` gains `reoffer_interval: Option<Duration>` (default `Some(60s)`;
  `None` disables it — park indefinitely, the pre-#274 behavior).
- The session loop already parks on `rx.recv().await` exactly while
  `s.turn.is_some()` (a live turn that returned to the loop is parked on tool
  results). That bare `recv` becomes a `tokio::time::timeout(interval, recv)`.
  On elapse — `interval` of **silence**, i.e. no `ToolResult` arriving — it
  re-emits every `turn.pending` call via the same `emit_tool_exec` the resume
  path uses (same `request_id`, fresh `seq`), then loops. Any arriving command
  resets the timer. Once the batch drains the turn ends and the timer is gone.

This reuses the ADR-0061 resume machinery — re-offering pending calls is exactly
what resume does; the timer just triggers it in-process on silence instead of on
replay.

### Runtime: executor `request_id` dedupe (`tool_runner.rs`)

- The executor loop keeps a per-session `HashMap<SessionId, HashSet<String>>` of
  request ids whose calls are **in flight** — dispatched but not yet resolved.
  The loop is single-threaded (it routes, then spawns a detached handler, and
  consumes `ToolExec`/`ToolOutput` in broadcast order), so the check is race-free
  without a lock.
- The first thing the `ToolExec` arm does is `insert` the id; if it was already
  present, the offer is a re-offer of a call still running and is **skipped**.
  Only genuinely-new ids (including one whose first offer was lost to a `Lagged`
  drop — the executor never saw it) dispatch.
- An id is **dropped again on the `ToolOutput`** core emits when the call
  resolves (its result was folded). This matters: `request_id` is the provider's
  tool-call id, and core matches a `ToolResult` by id **only within the current
  round's pending set** — so a later round may legitimately reuse an id (a scripted
  or simple model does; and nothing forbids it). Holding the id for the whole
  session would wrongly skip that reuse. Scoping it to *in flight* guards exactly
  the double-run window and no more.
- The set is also cleared per session on `SessionEnded`.

A re-offer to a *merely-slow* executor — the call is running, its `ToolResult`
just hasn't been sent yet — is thus a no-op rather than a second `bash`/`edit`/
spawn. Without the dedupe the timer would double-execute, so it does not ship
without it. Retaining resolved ids ("done") is unnecessary for safety: core stops
re-offering an id the instant it folds its result (the id leaves `pending`), and
that fold is what emits the `ToolOutput` the executor keys the drop on — the two
travel the same ordered broadcast, so a resolved id is never re-offered.

## Consequences

- An in-process turn stranded by a dropped `ToolExec` self-heals after
  `reoffer_interval` instead of waiting for a restart + resume.
- **At-least-once**, like resume (ADR-0061): a call whose result was never folded
  gets re-driven; the dedupe guarantees at-most-once *execution* per session for
  a given id, so the net effect is exactly-once in the common case and a benign
  re-drive when an offer was truly lost.
- Because the id is dropped on resolution, the dedupe imposes **no** uniqueness
  requirement on `request_id` across rounds/turns — a model that reuses ids (a
  scripted test, a simple provider) still works, matching core's own by-round id
  matching. The set holds only currently-running ids, so it never grows unbounded.
- The default 60s interval is long relative to normal tool latency, so it never
  fires on a healthy turn; only a genuinely silent parked turn trips it.

## Alternatives rejected

- **A dedicated reliable `mpsc` for `ToolExec`** (#154's original direction).
  Would remove the lossy-broadcast drop at the source, but splits the outbound
  event stream into two channels with different delivery guarantees, complicating
  every subscriber (heads, replay) for one event kind. The re-offer timer keeps a
  single outbound contract and rides the existing resume machinery. Still a valid
  option if broadcast pressure proves pathological under `serve`.
- **Removing the executor's own broadcast subscription in favor of the pending
  registry** (as ADR-0070 did for *decisions*). `ToolExec` is content, fanned to
  every subscriber; it is not a point-to-point decision, so the registry shape
  doesn't fit.
- **A shorter default interval.** Cheaper recovery, but risks firing on a slow
  legitimate tool and adds broadcast traffic. 60s is comfortably past normal tool
  latency; embedders that want faster recovery set `reoffer_interval`.
