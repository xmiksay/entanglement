# 0028. Session lifecycle: enumeration, explicit close, non-blocking routing

- Status: Accepted
- Date: 2026-07-09

## Context

The engine is session-multiplexed for the state that matters (context, profile,
approvals, plan, tasks, `seq`). Sub-agent spawn ([ADR-0022](0022-subagent-spawn.md)),
spawn limits ([ADR-0023](0023-subagent-spawn-limits.md)) and the
`SessionStarted` / `SessionEnded` lifecycle events ([ADR-0021](0021-hierarchical-session-model.md))
landed, but a few lifecycle gaps (issue #21) make multi-session heads —
especially the coming WebSocket `serve` head — harder than they should be:

1. **No live enumeration.** A head can only infer which sessions exist by
   folding the `SessionStarted` / `SessionEnded` broadcast it happens to
   observe. There's no way to *ask* the engine "what's live right now?" — a new
   subscriber (a reconnecting WS client) starts blind.
2. **No way to destroy a session.** [ADR-0017](0017-stop-cancels-turn-not-session.md)
   deliberately made `Stop` cancel the *turn*, not the session. That left **no**
   message that ends a session: a session task, once created, lives for the whole
   engine lifetime (its command channel is held in the supervisor map and never
   dropped). Sessions accumulate; `SessionEnded` only ever fired on full engine
   shutdown.
3. **`seq` reuse under a re-created id.** If an id could be destroyed and then
   re-created, `seq` would restart at 0 under the "same" session, breaking heads
   that dedupe with `seq > last_seen`.
4. **Supervisor backpressure.** The supervisor routed with
   `session_tx.send(cmd).await` into a bounded (64) per-session channel. One
   flooded or stalled session could park the supervisor's single loop and delay
   routing to *every* other session.

## Decision

Add a small, transport-agnostic lifecycle surface to the protocol and make the
supervisor's routing non-blocking. No new crate, no head required to use it (the
WS head is the first consumer; the stdio/TUI heads ignore it, as they ignored
`ToolExec` before their executors existed).

### Enumeration — `ListSessions` → `SessionList`

- `InMsg::ListSessions { session }` is a **supervisor-global** query: it is
  answered directly by the supervisor and never routed to a session task. Its
  `session` field is a **correlation id** the reply echoes, so a multiplexed head
  can pair the snapshot with its request (every `InMsg` carries a `session`;
  reusing the field keeps `InMsg::session()` total).
- `OutEvent::SessionList { session, sessions: Vec<SessionInfo> }` is a
  point-in-time lifecycle event (no `seq`). `SessionInfo { session, parent,
  profile, root }` mirrors what a head would otherwise fold from `SessionStarted`.
- The supervisor keeps a `session_meta` directory in lockstep with its `sessions`
  map (the liveness source of truth — a task only exits when its channel drops).
  `profile` is the session's *starting* profile; per-turn `SetAgent` switches are
  followed by a head via `AgentChanged`, not re-reported here.

### Explicit close — `CloseSession`

- `InMsg::CloseSession { session }` removes the session from the supervisor map,
  dropping its command channel. The task's `rx.recv()` returns `None`; it emits
  `SessionEnded` and exits — the same clean path as engine shutdown. Unknown /
  already-closed ids are a no-op. This is the lifecycle destroy that `Stop` (per
  ADR-0017) intentionally does not perform; the two are complementary.

### `seq` reuse — by contract, not by retained state

Session ids are **single-use**: after `SessionEnded`, a head must mint a fresh id
(`SessionId::new_uuid()`) rather than reuse the closed one. The engine does not
retain a per-id `seq` high-water mark across close (that would leak state for
every id ever seen). UUID minting makes non-reuse the path of least resistance,
so gap 3 is closed by convention rather than unbounded bookkeeping.

### Non-blocking routing

The supervisor routes via `route_to_session`: a non-blocking `try_send`, retried
a bounded number of times (`ROUTE_ATTEMPTS`, yielding between attempts so a
merely-behind session drains), then **shed** with an `OutEvent::Error` rather
than parking the supervisor. A closed channel (session gone) is dropped silently.
In the common case the channel is never full and `try_send` succeeds on the first
attempt — behavior is unchanged; shedding only triggers under a genuinely stalled
or flooded session, and never at the expense of routing to healthy ones.

## Consequences

### Positive

- A reconnecting / newly-subscribed head can enumerate live sessions in one
  round-trip instead of replaying the whole broadcast.
- Sessions are now destroyable; `SessionEnded` becomes a routinely-fired event,
  not just a shutdown artifact.
- One wedged session can no longer stall routing to the rest.

### Negative

- Under sustained saturation a command can be shed (surfaced as an `Error` +
  a `tracing::warn!`). This is a deliberate trade: never block the supervisor.
  The shed `Error` carries `seq: 0` (the supervisor can't mint the session's
  monotonic `seq`), so a `seq`-deduping head may filter it — the `warn` log is
  the primary signal.
- Session-id non-reuse is a contract the engine does not enforce; a head that
  reuses a closed id re-creates a fresh session with `seq` from 0.

### Neutral

- `SessionInfo.profile` reflects the *starting* profile only; live profile is
  tracked via `AgentChanged`.
- `ListSessions` / `CloseSession` still fan out on the inbound `InMsg` broadcast
  ([ADR-0010](0010-single-head-crate-and-bash-opt-in.md) #59) like every other
  message; runtime services may observe them but none are required to.

## Alternatives considered

- **Fold liveness from the outbound broadcast instead of a query.** Works for a
  head present since start, but a late/reconnecting subscriber can't reconstruct
  what it never saw. A pull query is the missing primitive.
- **Reuse `Stop` as the destroy.** Rejected: ADR-0017 gives `Stop` turn-cancel
  semantics that heads rely on. Overloading it would conflate "abort this turn"
  with "end this session." A distinct `CloseSession` keeps both intents crisp.
- **Retain a per-id `seq` high-water mark to make id reuse safe.** Rejected:
  unbounded state for every id ever closed, to support a pattern (id reuse) that
  UUID minting makes unnecessary. Single-use ids are the simpler contract.
- **Grow the per-session channel / block with a timeout.** A bigger buffer only
  delays the stall; a blocking timeout still parks the supervisor for its
  duration. `try_send` + bounded retry + shed is the only option that keeps the
  single supervisor loop responsive to *other* sessions under load.
- **A dedicated non-session query channel / request-response API.** Heavier than
  the problem: the actor ABI is already `send(InMsg)` / `subscribe(OutEvent)`,
  and a correlation-id echo rides that model without a second transport.

## References

- Issue #21: Engine — session lifecycle gaps for multi-session heads
- ADR-0017: `Stop` cancels the turn, not the session
- ADR-0021: Hierarchical session data model (`SessionStarted` / `SessionEnded`)
- ADR-0002: Session-multiplexed wire protocol
