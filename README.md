# brain

Headless, Rust-based AI coding agent **engine**. The reasoning + tool-execution
loop is decoupled from any UI and exposed as an async **actor**: a typed inbox of
`InMsg` and a broadcast outbox of `OutEvent`. Every interface is a thin adapter
over those two methods.

- Design & roadmap: [`PLAN.md`](PLAN.md)
- Architecture & interfaces: [`docs/architecture.md`](docs/architecture.md)

## Status

**Phase 1 (foundation)** — actor core + stdio head, running end-to-end on a
`DummyLlm`. No real LLM networking yet. WS and the opencode-style TUI are the
next heads.

## The contract (one set of types, every head)

```
InMsg    : Prompt | Approve | Reject | Stop | SetTasks | SetPlan | SetAgent     (harness → engine)
OutEvent : Status | AgentChanged | Plan | TextDelta | ToolRequest | ToolOutput
           | TaskList | Error | Done                                            (engine → harness)
```

Every frame is **session-scoped** (one connection multiplexes many sessions via
`SessionId`) and content frames carry a monotonic `seq` for dedup/ordering.

## Four interfaces, one ABI

| Head | Status | What it is |
| --- | --- | --- |
| **ABI (direct)** | ✅ | Hold a `Brain`, call `brain.send(InMsg)` / `brain.subscribe()`. Zero serialization. The foundation. |
| **stdio** (`brain run` / `brain pipe`) | ✅ | NDJSON over stdin/stdout — one-shot `run` (text or `--format json`, à la `opencode run`) and bidirectional `pipe`. |
| **WebSocket** (`brain serve`) | next | axum `/ws`, in-band auth first frame, `broadcast` fan-out, multiplexed by `SessionId`. Model from the `agent`/`design` references. |
| **TUI** (`brain`) | next | opencode-style terminal UI streaming `OutEvent`, tool-approval prompts, plan/task panels. |

## Agent profiles (opencode-style)

A session runs under an **agent profile** = `{ system prompt, model, permission
profile, mode }`. Switch with `SetAgent` (Build ↔ Plan ↔ Explore). The permission
profile (`Allow | Ask | Deny` per tool) drives the approval flow — `Plan` denies
edits, `Build` allows everything. Built-ins: `build`, `plan`, `explore`.

Structured outputs (`OutEvent::Plan`, `OutEvent::TaskList`) are orthogonal —
populated by the built-in `update_plan` / `update_tasks` tools or the harness
`SetPlan` / `SetTasks` messages, so every head can render plan/task panels
natively.

## Crates

| Crate | Role | Hard rule |
| --- | --- | --- |
| `brain-core` | actor engine: `Brain`, `InMsg`/`OutEvent`, session loop, permission dispatch, built-in tools, `Context`. | **Zero UI/transport deps** (`clap`/`axum`/`crossterm` forbidden). Enforced via `make tree`. |
| `brain-stdio` | stdio head (`run`, `pipe`). | — |
| `brain-ws` | _(next)_ axum WebSocket head. | — |
| `brain-cli` | _(next)_ opencode-style TUI. | — |

## Build & develop

Requires stable Rust (pinned via `rust-toolchain.toml`). Build jobs capped at 4
in `.cargo/config.toml`.

```bash
make run          # one dummy turn, text output
make run-json     # one dummy turn, NDJSON events
make test         # unit + integration
make lint         # clippy --all-targets -D warnings
make verify       # check-fmt + clippy + test (CI-equivalent)
make tree         # cargo tree -p brain-core (UI-dep hygiene gate)
make build | check | fmt | clean
```

Drive commands through `make`, not raw `cargo`.

## License

MIT — see [LICENSE](LICENSE).
