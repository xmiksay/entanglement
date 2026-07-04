# brain — Project Brief

Headless, Rust-based AI coding agent **engine**. The reasoning + tool-execution
loop is decoupled from any UI and exposed as an async actor: a typed `InMsg`
inbox and a broadcast `OutEvent` outbox. Every interface (ABI, stdio, WebSocket,
TUI) is a thin adapter over `brain.send()` / `brain.subscribe()`.

Full design: [`../PLAN.md`](../PLAN.md). Architecture & the four interfaces:
[`../docs/architecture.md`](../docs/architecture.md). Overview:
[`../README.md`](../README.md).

## Stack

- **Rust** (stable, `../rust-toolchain.toml`).
- Async: **Tokio** (`mpsc` inbox, `broadcast` outbox). Errors: `anyhow` + `thiserror`.
- Logging: `tracing`. Serde everywhere (the wire protocol).
- No web framework in core; `brain-ws` will bring `axum`.

## Workspace

| Crate | Role | Hard rule |
| --- | --- | --- |
| `brain-core` | actor engine: `Brain`, protocol, session loop, permission dispatch, built-in tools, `Context` | **Zero UI/transport deps** (`clap`/`axum`/`crossterm` forbidden). `make tree` enforces. |
| `brain-stdio` | stdio head: `brain run` (text/`--format json`), `brain pipe` (NDJSON) | — |
| `brain-ws` | _(next)_ axum WebSocket head | — |
| `brain-cli` | _(next)_ opencode-style TUI | — |

Heads depend on core, **never** the reverse.

## Commands — drive through `make`

```bash
make run           # stdio head, one turn (text)
make run-json      # one turn, NDJSON events (opencode run --format json)
make test          # unit + integration
make test-unit | make test-integration
make lint          # clippy --all-targets -D warnings
make fmt | check-fmt
make verify        # check-fmt + clippy + test  (CI-equivalent gate)
make tree          # cargo tree -p brain-core (UI-dep hygiene gate)
make build | check | clean
```

Build jobs capped at 4 via `../.cargo/config.toml`.

## The contract (read before touching the engine)

`brain-core/src/protocol.rs` defines the single set of types every head uses:

```
InMsg    : Prompt | Approve | Reject | Stop | SetTasks | SetPlan | SetAgent
OutEvent : Status | AgentChanged | Plan | TextDelta | ToolRequest | ToolOutput
           | TaskList | Error | Done
```

Session-multiplexed (every frame carries `SessionId`); content frames carry
monotonic `seq`. Agent profiles (`build`/`plan`/`explore` + custom) drive
permission dispatch (`Allow`/`Ask`/`Deny`). `Plan` and `TaskList` are
session-owned snapshots, written by built-in tools or harness `Set*` messages.

## Conventions (project-specific)

- **Tests ship with the change.** Pure logic → unit tests in-module
  (`#[cfg(test)] mod tests`); actor/protocol behavior → `brain-core/tests/`.
- **No panicking operators on I/O/user/network/config paths** in `brain-core` —
  propagate with `?` (+ `.context()`). `.unwrap()`/`.expect()` only in tests or
  provably-unreachable spots (then `.expect("invariant …")`).
- **Comments: WHY, not WHAT.**
- **Conventional Commits** (`feat(engine): …`), fast-forward only, never commit
  to `master`. No `Co-Authored-By`.
- **Architecture decisions run ADR + arch doc in parallel.** Any hard-to-reverse
  design choice (protocol shape, crate boundary, pattern picked over an obvious
  alternative, security/permission model) gets an ADR in
  [`../docs/adr/`](../docs/adr/) (numbered, immutable; see its `README.md`) — the
  *why* and rejected alternatives live there. Then update
  [`../docs/architecture.md`](../docs/architecture.md) to reflect the new *what
  is*, and add an inline ADR link at the relevant section. Never edit an accepted
  ADR in place — supersede it. Drift check: `/arch check`.
- **Keep this brief + `docs/architecture.md` in sync.** When a message variant,
  profile, crate, or command changes, update both in the same change.

## Open work (current phase)

- Real `Llm` with Anthropic SSE streaming (the protocol is already shaped for it
  via `LlmRequest { system, messages, tools }`).
- Concrete host tools (`read`, `edit`, `bash`, `glob`, `grep`) so the `build`/
  `plan`/`explore` permission profiles actually gate something.
- `brain-ws` (axum) and `brain-cli` (TUI) heads.

See [`../PLAN.md`](../PLAN.md) §5.
