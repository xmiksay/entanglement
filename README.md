# entanglement

Headless, Rust-based AI coding agent **engine**. The reasoning + tool-execution
loop is decoupled from any UI and exposed as an async **actor**: a typed inbox of
`InMsg` and a broadcast outbox of `OutEvent`. Every interface is a thin adapter
over those two methods.

- Architecture & interfaces: [`docs/architecture.md`](docs/architecture.md)

## Status

Actor core + stdio head + TUI, with real LLM backends wired
(`entanglement-provider`: z.ai/OpenAI/Ollama + Anthropic). WebSocket `serve` is
the next head. A three-layer re-architecture (core / provider / runtime) is
in progress — see [`docs/adr/0006`](docs/adr/0006-core-dependency-hygiene-gate.md)
and the crate table below.

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
| **ABI (direct)** | ✅ | Hold a `Holly`, call `holly.send(InMsg)` / `holly.subscribe()`. Zero serialization. The foundation. |
| **stdio** (`skutter run` / `skutter pipe`) | ✅ | NDJSON over stdin/stdout — one-shot `run` (text or `--format json`, à la `opencode run`) and bidirectional `pipe`. |
| **WebSocket** (`skutter serve`) | next | axum `/ws`, in-band auth first frame, `broadcast` fan-out, multiplexed by `SessionId`. Model from the `agent`/`design` references. |
| **TUI** (`skutter tui`) | next | opencode-style terminal UI streaming `OutEvent`, tool-approval prompts, plan/task panels. Design & issue breakdown tracked in [GitHub issue #1](https://github.com/xmiksay/entanglement/issues/1). |

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

Three crates, two seams (core ↔ provider, core ↔ runtime). Names in **bold**
are the target of an in-progress rename (🚧).

| Crate | Role | Hard rule |
| --- | --- | --- |
| `entanglement-core` | actor engine: `Holly`, `InMsg`/`OutEvent`, agent turn loop, the `Tool` **trait**, `Context`. | **Zero UI/transport deps** (`clap`/`axum`/`crossterm`/`reqwest` forbidden). Enforced via `make tree`. |
| **`entanglement-provider`** _(from `entanglement-llm`)_ | all LLM I/O behind the `Llm` trait: z.ai/OpenAI/Ollama + Anthropic clients; connection pool, retry, rate-limit, reasoning stream (🚧). | may depend on `reqwest`; never depended on by core. |
| **`entanglement-runtime`** _(from `entanglement-cli`)_ | the head crate (binary `skutter`): host tools + execution, permission dispatch + approval, user sessions, all transports (stdio ✅, WS 🚧, TUI). | — |

## Build & develop

Requires stable Rust (pinned via `rust-toolchain.toml`). Build jobs capped at 4
in `.cargo/config.toml`.

```bash
make run          # one dummy turn, text output
make run-json     # one dummy turn, NDJSON events
make test         # unit + integration
make lint         # clippy --all-targets -D warnings
make verify       # check-fmt + clippy + test (CI-equivalent)
make tree         # cargo tree -p entanglement-core (UI-dep hygiene gate)
make build | check | fmt | clean
```

Drive commands through `make`, not raw `cargo`.

## License

MIT — see [LICENSE](LICENSE).
