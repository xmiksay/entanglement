# entanglement-core

Headless AI coding agent **engine** — the reasoning + tool-execution loop of
[entanglement](https://github.com/xmiksay/entanglement), strictly decoupled
from any UI.

The contract is an **actor**: a `Holly` holds a typed inbox of `InMsg` and a
broadcast outbox of `OutEvent`. Every interface — in-process ABI, stdio NDJSON,
WebSocket, TUI — is a thin adapter over `holly.send()` / `holly.subscribe()`.
The heads and host tools live in
[`entanglement-runtime`](https://crates.io/crates/entanglement-runtime); the
LLM backends in
[`entanglement-provider`](https://crates.io/crates/entanglement-provider),
whose ABI (the `Llm` trait + DTOs) this crate re-exports.

## The contract (one set of types, every head)

```text
InMsg    : Prompt | Approve | Reject | ToolResult | AnswerQuestion | Stop
          | SetAgent | SetModel | Spawn | ListSessions | ReplayFrom | CloseSession
          | Resume (internal, not serialized)
OutEvent : SessionStarted | SessionEnded | SessionList | History | Status
          | AgentChanged | ModelChanged | Plan | TextDelta | ReasoningDelta
          | ToolCallDelta | ToolCall | ToolRequest | ToolExec | UserQuestion
          | ToolOutput | TaskList | Usage | Error | Done | FileChange
```

Every frame is session-scoped (`SessionId`); content frames carry a monotonic
`seq` for dedup/ordering.

## Design invariants

- **No executable tools, no policy.** The engine advertises tool *schemas*
  (`ToolSpec`) only. Tool execution is a protocol round-trip: a round ending in
  tool calls parks the turn as explicit serde state and batch-emits `ToolExec`;
  `ToolResult`s resolve in any order and the turn re-enters on drain. Permission
  resolution and approval live entirely in the runtime.
- **Trusted / untrusted inbox split.** `Holly::send` is the privileged
  in-process inbox; a wire head deserializing untrusted bytes uses
  `Holly::send_from_wire`, which enforces an allowlist and refuses
  runtime-authored frames (`ToolResult`, `Spawn`, `Resume`).
- **Event-sourced persistence seam.** The event log + `Holly::resume` is the
  embedder persistence seam — no database in the crate. Replay reconstructs a
  mid-turn tail; resume re-offers pending tool calls at-least-once.
- **No UI or web-server deps.** `clap` / `axum` / `crossterm` / `ratatui` are
  forbidden in the dependency tree (enforced by a CI hygiene gate); HTTP only
  rides in transitively via the provider ABI.

## Docs

Architecture: [docs/architecture.md](https://github.com/xmiksay/entanglement/blob/master/docs/architecture.md)
([protocol](https://github.com/xmiksay/entanglement/blob/master/docs/architecture/protocol.md)
· [engine](https://github.com/xmiksay/entanglement/blob/master/docs/architecture/engine.md))
· ADRs: [docs/adr](https://github.com/xmiksay/entanglement/tree/master/docs/adr)

## License

MIT — see [LICENSE](https://github.com/xmiksay/entanglement/blob/master/LICENSE).
