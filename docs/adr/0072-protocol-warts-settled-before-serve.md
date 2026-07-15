# 0072. Protocol warts settled before `serve` freezes the wire

- Status: Accepted
- Date: 2026-07-15
- Refines the session-multiplexed protocol of [0002](0002-session-multiplexed-protocol.md); complements the seq contract of [0068](0068-shared-per-session-seq-counter.md) and the trusted/untrusted split of [0069](0069-trusted-untrusted-wire-frame-split.md). Part of #153 (pre-`serve` hardening). Issue #160.

## Context

The WebSocket `serve` head (#153) will be the first head to expose the wire to
**external clients** that pin the JSON shape. A cluster of protocol expedients
that are cosmetic in-process become expensive once a client depends on them:

- `InMsg::ListSessions { session: SessionId }` overloaded a `SessionId` as a
  correlation token — a supervisor-global query names no session, and routing it
  by that field risks conjuring a phantom per-session view head-side.
- `OutEvent::seq()` returned a fake `0` for lifecycle events, indistinguishable
  from the real seq-`0` sentinel a supervisor-shed `Error` carries ([0068](0068-shared-per-session-seq-counter.md)).
- `AgentState::WaitingApproval` was reused for `ask_user` questions, so a head
  could not tell a permission decision from a model-driven question.
- The `InMsg → SessionCmd` map backstopped its "non-routable variant" invariant
  with `unreachable!` — a contract slip would panic the supervisor task and take
  down **every** live session, not just the offending frame.
- There was no late-subscriber history fetch: a head that connects after a turn
  started had no way to recover the events it missed, though the persistence log
  already stores them.

## Decision

Settle the shapes now, while the only heads are in-repo and free to change.

- **Correlation, not overloading.** `InMsg::ListSessions { correlation_id:
  String }` and `OutEvent::SessionList { correlation_id, sessions }` carry an
  opaque echo token. `InMsg::session()` and `OutEvent::session()` return
  `Option<&SessionId>` — `None` for `ListSessions` / `SessionList`, which name no
  single session. The TUI's event router drops a session-less event instead of
  keying a phantom view by the correlation id.
- **`Option<u64>` seqs.** `OutEvent::seq()` returns `Option<u64>` — `None` for a
  point-in-time lifecycle/query event, `Some(n)` for content. The seq-`0`
  sentinel stays a real `Some(0)`, now distinct from "no seq".
- **`WaitingAnswer` state.** A new `AgentState::WaitingAnswer` is emitted while an
  `ask_user` question is parked; `WaitingApproval` is left to genuine permission
  decisions (`Approve`/`Reject`, `propose_plan`, the `rhai`/script approvals).
  Cancel paths already emit `Status::Idle` (mid-stream and parked-turn `Stop`),
  so a head always gets a clean idle ack when a turn is cancelled.
- **`Option<SessionCmd>`, not `unreachable!`.** `msg_to_cmd` returns `Option`;
  the supervisor logs-and-drops a `None` rather than panicking, so a stray
  non-routable frame can never crash the loop.
- **`ReplayFrom` + `History`.** `InMsg::ReplayFrom { session, correlation_id,
  after_seq }` is a wire-allowed head query. It is answered **out-of-core** — the
  event log is the runtime's persistence seam, so a runtime history responder
  (spawned beside the persistence subscriber) reads the session's log off the
  inbound fan-out and broadcasts `OutEvent::History { correlation_id, session,
  events }` with every persisted content event whose `seq` exceeds `after_seq`.
  `History` is seq-less and emitted via a new `Holly::emit_history` (sibling of
  `emit_status`), keeping the raw outbound sender closed. Both the query and its
  reply are transient — neither is persisted nor folded on replay.

## Consequences

- The wire is stable for `serve` to freeze: correlation tokens are explicit,
  seqs are honestly optional, states are unambiguous, and a late subscriber has a
  first-class recovery path.
- `session()`/`seq()` returning `Option` ripples to their callers (persistence
  routing, replay fold, head routing, tests), each of which now handles the
  session-less / seq-less case explicitly rather than dereferencing a fake.
- `ReplayFrom` is deliberately answered as a **broadcast** `History` matched by
  `correlation_id`, symmetric to `SessionList`. Per-connection delivery (sending
  the reply to only the requesting socket) is the WS `serve` head's concern
  (#153); a child session's history maps to its root file there too.

## Rejected alternatives

- **Keep overloading `SessionId` as a correlation id.** Cheap in-process, but it
  pins a lie into the wire and forces every head to special-case a "session that
  isn't a session".
- **Leave `seq()` returning `0`.** Collides the lifecycle case with the real
  seq-`0` supervisor-error sentinel; a strict `seq > last` reconnect dedupe can't
  distinguish them.
- **A dedicated `Stopped` ack for every `Stop` duty.** The existing
  `Status::Idle` on the cancel paths already gives heads the ack; a new terminal
  variant would duplicate it.
- **Answer `ReplayFrom` in core.** Core holds no event log — the log is the
  runtime's persistence seam by design ([0002](0002-session-multiplexed-protocol.md)).
  Threading a log reader into the supervisor would drag persistence into core.
- **Per-connection replay now.** That needs the WS head's connection registry
  (#153); a `correlation_id`-matched broadcast is the in-process stand-in.
