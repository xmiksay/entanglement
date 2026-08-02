# 0139. Visible session states — `Working` + `WaitingAgent`, and `Done` as the resting state

- Status: Accepted — Amended by [0144] (pause/resume)
- Date: 2026-07-30
- Amends: [ADR-0072](0072-protocol-warts-settled-before-serve.md) (the `AgentState` enum widens)

## Context

`AgentState` ([`entanglement-core/src/protocol.rs`](../../entanglement-core/src/protocol.rs)) is the lifecycle state a head renders for a session. Before this ADR it had six variants: `Idle`, `Thinking`, `WaitingApproval`, `WaitingAnswer`, `Done`, `Error`.

Three gaps:

1. **No `Working`.** When `Allow`-permission tools ran (after `ToolExec`, before `ToolResult`s resolved), the session was `Thinking`. A head couldn't tell "the LLM is generating" from "a bash command is executing" — both rendered as "thinking".
2. **No `WaitingAgent`.** When parked on a sub-agent result (the blocking `agent` tool, or the sponsored build child of [ADR-0138](0138-sponsored-build-child-and-propose-plan-cycle.md)), the session was `Thinking`. A long-running build looked identical to the plan agent staring at the wall. The sponsored build child made this gap load-bearing: without a distinct state, the plan agent's park on the build was invisible.
3. **`Idle` after `Stop`.** A cancelled turn emitted `Idle` — the same state as a session that had never run. The user preferred `Done` as the resting state everywhere: a cancelled turn is still a completed interaction, and "never-run-yet" is the only thing `Idle` should mean.

## Decision

### New variants

```rust
pub enum AgentState {
    Idle,
    Thinking,
    Working,              // NEW — tool execution in progress
    WaitingAgent,         // NEW — parked on a sub-agent result
    WaitingApproval,
    WaitingAnswer,
    Done,
    Error,
}
```

`Working` and `WaitingAgent` are inserted between `Thinking` and the existing wait states. Serde `rename_all = "snake_case"` → wire forms `working`, `waiting_agent`. All exhaustive `match` sites gained arms (compiler-driven — no catch-all `_ =>`).

### `Working`

Emitted by `drive_turn` in `entanglement-core/src/session/turn.rs` after `RoundOutcome::Parked` — the batch's `ToolExec`s have all been emitted and the turn is parked on `ToolResult`s. This covers both `Allow` tools (executing) and `Ask` tools (about to flip to `WaitingApproval`). The runtime's `WaitingApproval` emit lands just after, so the user sees a brief flash of `Working` that correctly precedes the approval prompt — the known `#273` cosmetic flap, documented and deliberate.

The TUI's ship-cruise animation (`tick_thinking`) animates during both `Thinking` and `Working` — busy is busy.

### `WaitingAgent`

Emitted by `subagent::launch` (the blocking `agent` tool) right before parking on the child's result, and by `propose_plan::launch_sponsored_build` (the [ADR-0138](0138-sponsored-build-child-and-propose-plan-cycle.md) sponsored build child) right after spawning the child. A head can now show "waiting for sub-agent" instead of the ambiguous `Thinking`.

### `Done` as the resting state

`SessionCmd::Stop` (mid-turn cancel in `session.rs`) and the mid-stream `Stop` cancellation (in `stream.rs`) now emit `Done` instead of `Idle`. `Idle` is reserved for the genuine never-run-yet case — emitted once at session start (`session_loop`). The tool executor's in-flight dedupe (`tool_runner.rs`) was widened to clear on both `Idle` and `Done`, since either terminal state means no call is in flight any more.

### TUI rendering

The sidebar state-word match (`sidebar.rs`) gained `Working => "working"` and `WaitingAgent => "waiting for agent"`. The `attention_word` helper (`format.rs`) — derived from pending queues, not `AgentState` — already handles approval/question and takes precedence, so the flap-documented `#273` behavior is unaffected. The input panel badge (`input_panel.rs`) and the attention signal logic (`attention.rs`) gained the new arms; `Working` and `WaitingAgent` are not attention-worthy (no bell).

## Consequences

- **(+)** A head can distinguish model generation (`Thinking`) from tool execution (`Working`) from sub-agent parking (`WaitingAgent`) — three states that were all one before.
- **(+)** The sponsored build child's park (ADR-0138) is visible: the plan session shows "waiting for agent" while the build runs.
- **(+)** `Done` as the resting state means a completed interaction is never rendered the same as never-run-yet.
- **(+)** The wire protocol widens by two variants; `serde` with `rename_all = "snake_case"` makes the JSON stable, and a client matching exhaustively (as the TUI does) gets a compile error pointing at every site to update.
- **(−)** One more cosmetic flap (`Working` → `WaitingApproval` around an `Ask` tool), but this is the same shape as the existing `#273` flap and is documented; `attention_word` remains the reliable display signal.
- **(−)** Two new `AgentState` arms in every exhaustive match. Compiler-enforced; no catch-all `_ =>` allowed, so a future addition stays loud.

## Alternatives considered

- **Derive `Working` in the head from `ToolExec`/`ToolOutput` pairing, not a new state.** Rejected: that forces every head to reconstruct the same inference from content events, and pipe/WS heads would each re-implement it. A dedicated state is one source of truth.
- **Keep `Idle` as the resting state; add only `Working`/`WaitingAgent`.** Rejected: the user explicitly preferred `Done` everywhere `Idle` was used for "turn ended". The semantic split (`Idle` = never-run-yet, `Done` = completed) is clearer than overloading `Idle` for both.
- **A single `Busy` state covering both `Working` and `WaitingAgent`.** Rejected: a tool executing is a different thing to render than a sub-agent parked — the former might stream output, the latter is a pure wait. Two states keep the rendering signal honest.
