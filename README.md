# entanglement

Headless, Rust-based AI coding agent **engine**. The reasoning + tool-execution
loop is decoupled from any UI and exposed as an async **actor**: a typed inbox of
`InMsg` and a broadcast outbox of `OutEvent`. Every interface is a thin adapter
over those two methods.

- Architecture & interfaces: [`docs/architecture.md`](docs/architecture.md)

## Status

Actor core + stdio head + TUI, with real LLM backends wired
(`entanglement-provider`: z.ai/OpenAI/Ollama + Anthropic). WebSocket `serve` is
the next head. The three-layer re-architecture (core / provider / runtime) has
landed — see [`docs/adr/0006`](docs/adr/0006-core-dependency-hygiene-gate.md)
and the crate table below.

## The contract (one set of types, every head)

```
InMsg    : Prompt | Approve | Reject | Stop | SetAgent                          (harness → engine)
OutEvent : Status | AgentChanged | Plan | TextDelta | ToolRequest | ToolOutput
           | TaskList | Error | Done                                            (engine → harness)
```

Every frame is **session-scoped** (one connection multiplexes many sessions via
`SessionId`) and content frames carry a monotonic `seq` for dedup/ordering.

## Four interfaces, one ABI

| Head | Status | What it is |
| --- | --- | --- |
| **ABI (direct)** | ✅ | Hold a `Holly`, call `holly.send(InMsg)` / `holly.subscribe()`. Zero serialization. The foundation. |
| **stdio** (`skutter run` / `skutter pipe`) | ✅ | NDJSON over stdin/stdout — one-shot `run` (text or `--format json`, à la `opencode run`) and bidirectional `pipe`. `skutter sessions` lists past sessions; `skutter inspect prompt --agent <name> [--parts]` prints an agent's assembled system prompt (no engine); `skutter inspect agents [name]` shows the resolved agent registry with layer provenance — a table (name, mode, model, layer, source, mask) or one agent's full resolved profile + what lower layers it overrode; `skutter inspect skills [name] [--disclosures]` does the same for skills — a table (name, user_only, layer, root_dir, description), the exact tier-1 disclosure block the model gets (`--disclosures`), or a dry-run of the `load_skill` path substitution for one skill. |
| **TUI** (`skutter tui`) | ✅ | opencode-style terminal UI streaming `OutEvent`, tool-approval prompts, plan/task panels. |
| **WebSocket** (`skutter serve`) | next | axum `/ws`, in-band auth first frame, `broadcast` fan-out, multiplexed by `SessionId`. Model from the `agent`/`design` references. |

## Agent profiles (opencode-style)

A session runs under an **agent profile** = `{ system prompt, model, permission
profile, mode }`. Switch with `SetAgent` (Build ↔ Plan ↔ Explore). The permission
profile (`Allow | Ask | Deny` per tool) drives the approval flow — `Plan` denies
edits, `Build` allows everything. Built-ins: `build`, `plan`, `explore`.

Session snapshots (`OutEvent::Plan`, `OutEvent::TaskList` — both markdown
`content`) are orthogonal — emitted by the runtime's `update_plan` /
`update_tasks` state tools (ordinary host tools, gated by the permission path;
ADR-0049), so every head can render plan/task panels natively.

## Crates

Three crates, two seams (core ↔ provider, core ↔ runtime). Dependency direction
is `provider (leaf) ← core ← runtime` ([ADR-0053](docs/adr/0053-invert-core-provider-seam.md)).

| Crate | Role | Hard rule |
| --- | --- | --- |
| `entanglement-provider` | **leaf** crate owning the LLM ABI: the `Llm` **trait** + DTOs (`LlmRequest`/`Event`/`Stream`, `LlmSession`, `ToolCall`/`ToolSpec`) + wire `Message`; z.ai/OpenAI/Ollama + Anthropic clients; connection pool, retry, rate-limit, reasoning stream. Usable **standalone** for raw LLM queries. | no `entanglement-*` deps; owns `reqwest`. |
| `entanglement-core` | actor engine: `Holly`, `InMsg`/`OutEvent`, agent turn loop, the `Tool` **trait**, `Context`. Depends on provider, drives `dyn Llm`, re-exports the ABI. | **No UI/web-server deps** (`clap`/`axum`/`crossterm`/`ratatui` forbidden); `reqwest` is transitive via provider (ADR-0053). Enforced via `make tree`. |
| `entanglement-runtime` | the head crate (binary `skutter`): host-tool impls (✅), tool execution + permission dispatch (✅ #58/#59), approval, user sessions, all transports (stdio ✅, TUI ✅, WS 🚧). Selects the concrete provider + glues it to core. Feature-gated `cli`/`tui` (`default = ["tui"]`); `--no-default-features` is a lean embeddable library (ADR-0025). | `--no-default-features` stays CLI/TUI-free; `make check-lean` enforces. |

## Build & develop

Requires stable Rust (pinned via `rust-toolchain.toml`). Build jobs capped at 4
in `.cargo/config.toml`.

```bash
make run          # one dummy turn, text output
make run-json     # one dummy turn, NDJSON events
make test         # unit + integration
make lint         # clippy --all-targets -D warnings
make verify       # check-fmt + tree + check-lean + lint + test (CI-equivalent)
make tree         # cargo tree -p entanglement-core (UI/web-server dep hygiene gate)
make check-lean   # runtime --no-default-features stays CLI/TUI-free (ADR-0025 + ADR-0053)
make coverage     # cargo llvm-cov --workspace, fails under COV_MIN% (release gate)
make build | check | fmt | clean
```

Drive commands through `make`, not raw `cargo`.

## CI

GitHub Actions drives the same `make` targets ([`.github/workflows/`](.github/workflows/)):

- **`ci.yml`** — runs `make verify` on every PR and every push to `master`,
  putting the `tree` (ADR-0006) and `check-lean` (ADR-0025) hygiene gates under
  automation.
- **`release.yml`** — on a `v*` tag, runs `make verify` **plus** `make coverage`
  (`cargo llvm-cov --workspace`, fails under `COV_MIN`%) and uploads the
  lcov/Cobertura reports as an artifact, so a release is blocked on green tests.

`make coverage` needs `cargo-llvm-cov` locally: `cargo install cargo-llvm-cov
--locked`.

## License

MIT — see [LICENSE](LICENSE).
