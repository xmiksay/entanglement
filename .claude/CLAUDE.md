# entanglement — Project Brief

Headless, Rust-based AI coding agent **engine**. The reasoning + tool-execution
loop is decoupled from any UI and exposed as an async actor: a typed `InMsg`
inbox and a broadcast `OutEvent` outbox. Every interface (ABI, stdio, WebSocket,
TUI) is a thin adapter over `holly.send()` / `holly.subscribe()`.

Architecture & the four interfaces:
[`../docs/architecture.md`](../docs/architecture.md). Overview:
[`../README.md`](../README.md).

## Stack

- **Rust** (stable, `../rust-toolchain.toml`).
- Async: **Tokio** (`mpsc` inbox, `broadcast` outbox). Errors: `anyhow` + `thiserror`.
- Logging: `tracing`. Serde everywhere (the wire protocol).
- No web framework in core; the runtime head's future `serve` subcommand will bring `axum`.

## Workspace

Three crates, two seams (core↔provider via the `Llm` trait, core↔runtime for
tool exec/approval over the protocol). Layering: [ADR-0006](../docs/adr/0006-core-dependency-hygiene-gate.md).

| Crate | Role | Hard rule |
| --- | --- | --- |
| `entanglement-core` | actor engine: `Holly`, protocol, **agent turn loop**, the `Tool` **trait** (not impls), `Context`, the `Llm` **trait** | **Zero UI/transport deps** (`clap`/`axum`/`reqwest`/`crossterm` forbidden). `make tree` enforces. |
| `entanglement-provider` | all LLM I/O: generic OpenAI-compat client (z.ai GLM — primary, OpenAI, Ollama) + separate Anthropic client, via `reqwest`; **connection pool, retry, rate-limit, reasoning stream, models-per-provider (🚧)**; implements `entanglement_core::Llm` | may depend on transport crates (`reqwest`); never depended on by `entanglement-core` |
| `entanglement-runtime` | the head crate (binary `skutter`): **host tools + execution, permission dispatch + approval, user sessions**, stdio `run`/`pipe` today, `serve` (WS) + `tui` next. Selects provider via `ENTANGLEMENT_PROVIDER` or key auto-detect. All transports packaged here ([ADR-0010](../docs/adr/0010-single-head-crate-and-bash-opt-in.md)). | — |

Heads depend on core, **never** the reverse.

## Commands — drive through `make`

```bash
make run           # stdio head, one turn (text)
make run-json      # one turn, NDJSON events (opencode run --format json)
make test          # unit + integration
make test-unit | make test-integration
make lint          # clippy --all-targets -D warnings
make fmt | check-fmt
make verify        # check-fmt + tree + clippy + test  (CI-equivalent gate)
make tree          # entanglement-core dep hygiene gate (fails on UI/transport crates)
make build | check | clean
```

Build jobs capped at 4 via `../.cargo/config.toml`.

## Providers (`skutter`)

Set `ENTANGLEMENT_PROVIDER` explicitly, or let it auto-detect by key (z.ai first):

| `ENTANGLEMENT_PROVIDER` | wire | key env | model env (default) | base env |
| --- | --- | --- | --- | --- |
| `zai` (primary) | OpenAI-compat | `ZAI_API_KEY` | `ZAI_MODEL` (`glm-5.2`) | `ZAI_API_BASE` (Coding Plan) |
| `openai` | OpenAI-compat | `OPENAI_API_KEY` | `OPENAI_MODEL` (`gpt-4o`) | `OPENAI_API_BASE` |
| `ollama` | OpenAI-compat, keyless | — | `OLLAMA_MODEL` (`llama3.1`) | `OLLAMA_BASE` |
| `anthropic` | `/v1/messages` | `ANTHROPIC_API_KEY` | `ANTHROPIC_MODEL` (`claude-sonnet-4-5`) | — |

z.ai/OpenAI/Ollama share one `entanglement-provider::OpenAiLlm`; Anthropic has its own client (distinct content-block
format). No key → `DummyLlm`. Detail in
[`../docs/architecture.md`](../docs/architecture.md) §5b. **Pending (🚧):**
connection pool, retry/backoff, rate-limit (429/RPM), and reasoning/thinking
stream events all belong to this crate but are not implemented yet ([ADR-0007](../docs/adr/0007-streaming-llm-and-provider-crate.md)).

## The contract (read before touching the engine)

`entanglement-core/src/protocol.rs` defines the single set of types every head uses:

```
InMsg    : Prompt | Approve | Reject | Stop | SetTasks | SetPlan | SetAgent
OutEvent : Status | AgentChanged | Plan | TextDelta | ToolRequest | ToolOutput
          | TaskList | Error | Done
```

Session-multiplexed (every frame carries `SessionId`); content frames carry
monotonic `seq`. Agent profiles (`build`/`plan`/`explore` + custom) drive
permission dispatch (`Allow`/`Ask`/`Deny`). `Plan` and `TaskList` are
session-owned snapshots, written by built-in tools or harness `Set*` messages.
The `Tool` trait carries `schema()` (feeds `ToolSpec.schema` → the model's
`input_schema`); `host_tools(root)` (see ADR-0008 + ADR-0009 + ADR-0010)
assembles the root-contained quartet (`read`/`glob`/`grep`/`edit`);
`BashTool` is opt-in at the head (`ENTANGLEMENT_ENABLE_BASH=1`).

## Conventions (project-specific)

- **Tests ship with the change.** Pure logic → unit tests in-module
  (`#[cfg(test)] mod tests`); actor/protocol behavior → `entanglement-core/tests/`.
- **No panicking operators on I/O/user/network/config paths** in `entanglement-core` —
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

**Three-layer re-architecture** — the big active effort, tracked by epic
[#50](https://github.com/xmiksay/entanglement/issues/50) ([ADR-0006](../docs/adr/0006-core-dependency-hygiene-gate.md)).
Today core owns too much (tool loop **and** execution **and** permission dispatch
**and** the host-tool impls **and** a per-session client); the target moves those
to their proper layers. Backlog:

- **Provider** ([ADR-0007](../docs/adr/0007-streaming-llm-and-provider-crate.md)):
  connection pool + retry + rate-limit (#52); models-per-provider (#53); reasoning/thinking stream
  events (#54, currently dropped); live session/connection handle (#55).
- **Runtime** ([ADR-0010](../docs/adr/0010-single-head-crate-and-bash-opt-in.md)):
  move host tools out of core (#57); relocate tool execution (#58) and permission dispatch (#59) out of
  core; inter-session agent messaging / subagent spawn (#60).
- **Core**: slim `Session` to loop + turn state (#61).
- **Cleanup**: docs drift guard (#62); orphaned `apply_diff.rs` + `audit.rs` (#63).

Already shipped: `skutter run`/`pipe` (stdio) and `tui`; LLM providers wired
([ADR-0007](../docs/adr/0007-streaming-llm-and-provider-crate.md)) — `Llm` is a
streaming trait returning `BoxStream<LlmEvent>`; one generic OpenAI-compat client
serves z.ai (primary)/OpenAI/Ollama, plus a separate Anthropic client;
`ENTANGLEMENT_PROVIDER` or key auto-detect, else `DummyLlm`. `skutter serve`
(axum WS) is the next head. `bash` stays opt-in (`ENTANGLEMENT_ENABLE_BASH=1`),
unsandboxed — a real sandbox is a future security-focused ADR.
