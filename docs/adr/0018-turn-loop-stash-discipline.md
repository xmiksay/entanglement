# 0018. Turn-loop command stash discipline

- Status: Accepted
- Date: 2026-07-07

## Context

The turn loop (`session.rs::run_turn`) polls the session inbox with
`rx.try_recv()` at two points mid-turn: inside the streaming-consumer loop
(between `LlmEvent`s) and inside the tool-dispatch loop (between tool calls).
Both polls were added in #36 to let `SessionCmd::Stop` interrupt an in-flight
turn — but the implementation only matched `Stop` and **silently discarded
every other command**:

```rust
if let Ok(cmd) = rx.try_recv() {
    if matches!(cmd, SessionCmd::Stop) { /* interrupt */ return Ok(()); }
    // implicit drop — Prompt / SetAgent / SetPlan / SetTasks all vanish here
}
```

A `Prompt` sent while the engine was mid-turn vanished without trace. The
user's follow-up question was lost, with no event in the broadcast stream and
no log line — the engine simply continued the in-flight turn as if nothing
had arrived. The head's transcript showed the user's typed prompt (it had
been added optimistically), but the engine never processed it.

This contradicted the engine's own established pattern: `wait_approval`
(`session.rs:446-461`) already stashes non-matching commands via
`Some(other) => stash.push_back(other)` and replays them after the turn ends
(`session_loop`'s `if let Some(c) = stash.pop_front()` at the top of each
iteration). The two `try_recv` sites added later forgot to follow the same
rule.

## Decision

1. **Both `try_recv` sites now `while let Ok(cmd) = rx.try_recv()` and route
   every command:** `Stop` interrupts the turn as before; any other command
   (`Prompt`, `Approve`, `Reject`, `SetAgent`, `SetPlan`, `SetTasks`) is
   pushed onto the existing `stash: &mut VecDeque<SessionCmd>` for replay
   after the turn ends. Nothing is dropped.

2. **`while let` instead of `if let`** so the poll drains all queued
   commands in one pass, not just one. If three commands arrived between two
   stream events, all three are stashed in order.

3. **The stash is the single source of truth for "deferred commands."**
   `wait_approval`, the streaming loop, and the tool-dispatch loop all push
   to the same `&mut VecDeque`. `session_loop` already consumes it before
   `rx.recv().await`, so the replay path needs no new wiring.

4. **A `tracing::debug!` line accompanies every stash** so the replay is
   observable in `RUST_LOG=entanglement_core::session=debug` output (cmd
   variant + which site it landed at).

## Consequences

- **(+)** A `Prompt` (or any command) sent while the engine is mid-turn is
  now reliably processed, in order, after the in-flight turn ends. No silent
  drops.
- **(+)** The engine's three "what do we do with an inbox command during a
  busy state" sites now follow the same rule, instead of two-of-three
  dropping on the floor.
- **(+)** `while let` means a burst of commands is fully drained each tick
  of the streaming loop, so the inbox doesn't back up.
- **(−)** A stashed `Prompt` runs after the in-flight turn produces its full
  reply, not as an interruption — the user can't "redirect" the model
  mid-thought, only queue a follow-up. This matches `wait_approval`'s
  established behavior; if true interruption becomes a requirement it will
  need a separate `Cancel`-and-replace semantic.
- **(−)** No bound on the stash. A malicious or buggy head could queue
  unbounded commands during a very long turn. Accepted for now (heads are
  trusted); a cap with `Lagged`-style feedback could be added later.

## Alternatives considered

- **Drop the mid-turn polls entirely** and rely on the outer `session_loop`
  to process commands between turns. Rejected: that's exactly the bug #36
  was fixing — without the mid-turn `try_recv`, `Stop` only took effect
  after the turn ended naturally, which could be never for a runaway
  tool-calling loop.
- **Process non-Stop commands immediately** (e.g. apply `SetAgent` mid-turn).
  Rejected: mid-turn profile changes would race with the in-flight
  `LlmRequest` (already built from the old profile's system prompt and tool
  list), and a mid-turn `Prompt` has no clean insertion point in the
  conversation. Stashing defers the change to a well-defined boundary (turn
  end), preserving invariant: "one turn = one profile."
- **A dedicated `InMsg::Queue` ABI** that makes the defer-explicit at the
  protocol level. Rejected: the inbox semantics are already
  queue-and-receive; adding a layer doesn't help and would require every
  head to learn a new command.

## Out of scope

The orphaned-message problem (a cancelled or errored turn leaves a `User`
message in `Context` with no paired assistant reply, or an assistant
`tool_calls` message with no `tool` result) is **not** addressed by this
ADR. It's a separate concern about what `Context` contains after a
cancelled turn, not about how commands queue. Will be addressed in a future
ADR if it becomes painful in practice.
