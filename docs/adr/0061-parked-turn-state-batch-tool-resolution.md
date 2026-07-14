# 0061. Parked turn state: batch tool resolution as explicit session state

- Status: Accepted
- Date: 2026-07-14
- Refines the tool round-trip of [0003](0003-agent-and-permission-profiles.md)/#58 and the cancel semantics of [0017](0017-stop-cancels-turn-not-session.md); extends the replay model of [0020](0020-event-sourced-session-persistence.md); preserves the fold site of [0058](0058-mid-turn-prompt-folds-into-live-turn.md). Epic #276.

## Context

The tool seam was already pure protocol: core emits `OutEvent::ToolExec` and the
runtime executor — one broadcast subscriber among possibly many — answers with
`InMsg::ToolResult` matched by `request_id` (#58/#59, ADR-0059). But the *wait*
was an async-stack continuation: `run_turn` executed a reply's tool calls
strictly serially, each blocking inside `wait_tool_result` on the session's own
inbox. The mid-turn state — which call was outstanding, the round counter —
existed only as locals in that stack frame.

Three costs followed:

1. **A suspended turn was unrepresentable.** `Session::replay` (ADR-0020) could
   only reconstruct completed `ToolCall`/`ToolOutput` pairs, flushing at
   `Prompt`/`Done` boundaries; a log ending between `ToolExec` and `ToolResult`
   silently dropped the tail. A crash mid-tool lost the turn.
2. **Embedders had no persistence seam for in-flight work.** An external user of
   `entanglement-core` (the engine is a library) could not store a session with
   unresolved tool calls and resolve them later against its own state — a DB, a
   queue, a human — because "unresolved" was a stack frame, not data.
3. **A lost `ToolExec` wedged the turn forever.** The executor reads a lossy
   broadcast; on `RecvError::Lagged` a dropped request left the turn parked with
   no recovery (`tool_runner.rs`).

## Decision

**Reify the parked phase of a turn as explicit, serde-serializable session
state, and resolve tool calls as a batch.**

- `Session` gains `turn: Option<TurnState>`; `TurnState { pending: Vec<ToolCall>,
  iterations }` (`session/turn_state.rs`) is `Some` exactly while a turn is live.
- A round ending in tool calls emits the whole batch up front — the per-call
  (`ToolCall`, `ToolExec`) pair for every call — records it as `pending`, and
  **returns to the session loop** (`RoundOutcome::Parked`). The loop resolves
  each `ToolResult` against the pending set (any order; output folds into
  `Context` on arrival, in arrival order) and re-enters the turn when the set
  drains. `wait_tool_result`/`session/tools.rs` are gone.
- Only the *parked* phase is reified. Streaming stays an async fn
  (`session/stream.rs`): a network stream is inherently non-resumable and its
  Stop semantics (biased `select!`, ADR-0057/#179) are already correct.
- `MAX_TURNS` (#177) moves from a `run_turn` local to `TurnState::iterations` —
  still reset per prompt (a fresh `TurnState` per `Prompt`), still not reset by
  a folded mid-turn prompt (ADR-0058; the fold site is unchanged, at the top of
  each round).
- **Replay reconstructs a mid-turn tail** (#271). `ToolCall` events are emitted
  only after a completed stream, so a non-empty pending set in the tail means
  the round finished streaming: commit the assistant message, fold logged
  outputs, park the remainder as `TurnState`. A text-only tail (mid-stream
  crash) stays dropped — the live engine never committed it either.
  `iterations` restarts at 0 (a runaway guard, not a quota). The tail is
  guarded to the resumed root's own events (a root log interleaves children —
  the general fold's session-blindness is tracked as #275). Runtime `Gap`
  tombstones never reach core: both resume paths refuse on `integrity_gap`
  first (ADR-0020/#104), unchanged.
- **Resume re-offers pending calls at-least-once** (#272). The session-loop
  preamble re-emits one `ToolExec` per pending call — same `request_id`, fresh
  `seq` above the replayed max — and parks; a drained-but-unfinished tail
  continues the turn directly. Display `ToolCall` events are not re-emitted, so
  a twice-resumed log folds idempotently (pending derives from `ToolCall`
  events minus output ids). A tool that ran before the crash but whose result
  was never logged **runs again** — by design.
- **The embedder persistence seam is the event log + `Holly::resume`.** Records
  are serde values an embedder can store anywhere (DB rows, a queue) and feed
  back; `TurnState` is a `pub` serde field inspectable after `Session::replay`.
  No database enters this repo: the runtime's JSONL store (ADR-0020) is the
  in-repo reference implementation of the same seam.

The protocol is unchanged — `ToolExec`/`ToolResult` already carry `request_id`.

## Consequences

- **Batch tool calls execute concurrently** — a deliberate behavior change from
  serial in-call-order dispatch. Models emitting multi-call batches assume
  independence (industry norm), but ordering within a batch is no longer
  guaranteed. `ToolOutput` events (and context tool messages) follow *arrival*
  order; both wire formats key results by `tool_call_id`, so sibling order is
  semantically irrelevant.
- Heads must cope with **N simultaneous `ToolRequest`s per session** — the TUI's
  single approval slot becomes a FIFO queue (#273). `seam::await_decision`
  already filters by `(session, request_id)`. Known cosmetic flap: with two
  parked Asks, `Status` flips `WaitingApproval` → `Thinking` when the first
  resolves while the second still waits.
- Crash-resume is **at-least-once** for side-effectful tools; a result that
  never reached the log re-executes. Acceptable for a local coding agent;
  embedders needing exactly-once must dedupe by `request_id` on their side.
- The lossy-broadcast wedge is now **restart-recoverable** (the pending set is
  durable; resume re-offers). An in-process re-offer timer needs executor-side
  `request_id` dedupe first — deferred to #274.
- Stale/duplicate/unknown `ToolResult`s are dropped with a debug trace instead
  of corrupting the pending set. `Stop` while parked clears `TurnState` and
  keeps the session + context alive (ADR-0017), including outputs that already
  arrived — the same dangling-`tool_use` posture as the old mid-batch cancel.
- The #177 wart (turn-limit trip emits `Error` without `Done`) is preserved
  as-is, not silently changed.
- Replayed sessions reset `iterations`, so a resumed runaway turn gets a fresh
  50-round budget.

## Alternatives considered

- **A direct `SessionSnapshot` serde API** instead of log + replay: a second
  source of truth that drifts from the fold, and it would have to carve around
  the non-serializable `LlmSession` handle. The log already round-trips.
- **Reifying the whole turn as a state machine, streaming included**: streaming
  is non-resumable network state; the abstraction would be dead weight (the
  `Cancelled`/`Failed` outcomes already capture its exits).
- **New protocol messages** (pending-query / re-offer variants): unnecessary —
  `request_id` on the existing pair suffices, and heads already ignore events
  they don't know.
- **Keeping serial dispatch with only a serializable wait**: forfeits batch
  parallelism and still blocks the loop; the pending set costs the same and
  buys both.
