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
| `entanglement-provider` | all LLM I/O: generic OpenAI-compat client (z.ai GLM — primary, OpenAI, Ollama) + separate Anthropic client, via `reqwest`; connection pool, retry, rate-limit, reasoning stream, models-per-provider, provider-owned session handle; implements `entanglement_core::Llm` | may depend on transport crates (`reqwest`); never depended on by `entanglement-core` |
| `entanglement-runtime` | the head crate (binary `skutter`): **host tools** (impls moved from core ✅), **tool execution** (`tool_runner`, moved from core ✅ #58), **permission dispatch + approval** (moved from core ✅ #59), user sessions, stdio `run`/`pipe` + `tui` today, `serve` (WS) next. Selects provider via `ENTANGLEMENT_PROVIDER` or key auto-detect. All transports packaged here ([ADR-0010](../docs/adr/0010-single-head-crate-and-bash-opt-in.md)). | — |

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
[`../docs/architecture.md`](../docs/architecture.md) §5b. Connection pool,
retry/backoff, rate-limit (429/`Retry-After`/RPM), reasoning/thinking stream
events, the models-per-provider registry, and the provider-owned session handle
all live in this crate now (✅ #52–#55, [ADR-0007](../docs/adr/0007-streaming-llm-and-provider-crate.md)).

## The contract (read before touching the engine)

`entanglement-core/src/protocol.rs` defines the single set of types every head uses:

```
InMsg    : Prompt | Approve | Reject | ToolResult | Stop | SetTasks | SetPlan | SetAgent | Spawn
OutEvent : Status | AgentChanged | Plan | TextDelta | ToolRequest | ToolExec
          | ToolOutput | TaskList | Error | Done
```

Tool execution is a protocol round-trip (#58): core emits `ToolExec` for *every*
host tool and awaits the runtime's `ToolResult`. Permission dispatch and approval
moved out too (#59): core no longer reads `PermissionProfile` — the runtime
`tool_runner` resolves `Allow`/`Ask`/`Deny` per call, emits the `ToolRequest`
prompt on `Ask`, and consumes `Approve`/`Reject` off the engine's inbound
fan-out (`Holly::subscribe_inbound()`). Core holds no executable tools and makes
no policy decision — only tool schemas (`EngineConfig.tool_specs`).

Session-multiplexed (every frame carries `SessionId`); content frames carry
monotonic `seq`. Agent profiles (`build`/`plan`/`explore` + custom) drive
permission dispatch (`Allow`/`Ask`/`Deny`), resolved in the runtime. `Plan` and `TaskList` are
session-owned snapshots, written by built-in tools or harness `Set*` messages.
The `Tool` trait carries `schema()` (feeds `ToolSpec.schema` → the model's
`input_schema`); `host_tools(root)` (see ADR-0008 + ADR-0009 + ADR-0010)
assembles the root-contained quartet (`read`/`glob`/`grep`/`edit`);
`BashTool` is opt-in at the head (`ENTANGLEMENT_ENABLE_BASH=1`).

Sub-agent spawn (#60, [ADR-0022](../docs/adr/0022-subagent-spawn.md)): the
runtime-owned `spawn_agent { agent, prompt }` tool issues `InMsg::Spawn`; the
supervisor records `parent_links[child]=parent` and starts the child under the
requested profile, then the runtime relays the child's final answer back to the
parent as the tool's `ToolOutput` (reusing the #58 round-trip). Bypasses
permissions like the built-ins; isolation/recursion limits deferred.

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
Permission dispatch now lives in the runtime (✅ #59); sub-agent spawn landed
(✅ #60); core's `Session` is slimmed to loop + turn state (✅ #61 — no cached
tool set, schemas sourced from `EngineConfig.tool_specs` at turn time). The
three-layer split is complete; no core-slimming backlog remains.

Landed: **provider track** — crate renamed from `entanglement-llm` (#51),
connection pool + retry + rate-limit (#52), models-per-provider (#53),
reasoning/thinking stream events (#54), provider-owned session handle (#55).
**Runtime track** — crate renamed from `entanglement-cli` (#56), host-tool impls
moved out of core (#57), tool execution relocated to `runtime::tool_runner` via
the `ToolExec`/`ToolResult` round-trip (#58), permission dispatch + approval
relocated to `runtime::tool_runner` via a per-session profile map + the engine's
inbound `InMsg` fan-out (#59), sub-agent spawn via `InMsg::Spawn` + the
`spawn_agent` tool relaying the child's answer back to the parent (#60,
[ADR-0022](../docs/adr/0022-subagent-spawn.md)). **Cleanup** — orphaned `apply_diff.rs` + `audit.rs`
removed (#63); docs drift guard (#62) is a standing checklist flipping the
🚧 markers in `docs/architecture.md` as each child lands.

Already shipped: `skutter run`/`pipe` (stdio) and `tui`; LLM providers wired
([ADR-0007](../docs/adr/0007-streaming-llm-and-provider-crate.md)) — `Llm` is a
streaming trait returning `BoxStream<LlmEvent>`; one generic OpenAI-compat client
serves z.ai (primary)/OpenAI/Ollama, plus a separate Anthropic client;
`ENTANGLEMENT_PROVIDER` or key auto-detect, else `DummyLlm`. `skutter serve`
(axum WS) is the next head. `bash` stays opt-in (`ENTANGLEMENT_ENABLE_BASH=1`),
unsandboxed — a real sandbox is a future security-focused ADR.
