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
- No web framework in core; `entanglement-ws` will bring `axum`.

## Workspace

| Crate | Role | Hard rule |
| --- | --- | --- |
| `entanglement-core` | actor engine: `Holly`, protocol, session loop, permission dispatch, built-in tools, `Context`, the `Llm` **trait** | **Zero UI/transport deps** (`clap`/`axum`/`reqwest`/`crossterm` forbidden). `make tree` enforces. |
| `entanglement-llm` | concrete LLM backends: one generic OpenAI-compat client (z.ai GLM — primary, OpenAI, Ollama) + separate Anthropic client, all via `reqwest`; implements `entanglement_core::Llm` | may depend on transport crates (`reqwest`); never depended on by `entanglement-core` |
| `entanglement-stdio` | stdio head (binary `skutter`): `skutter run` (text/`--format json`), `skutter pipe` (NDJSON); selects provider via `ENTANGLEMENT_PROVIDER` or key auto-detect | — |
| `entanglement-ws` | _(next)_ axum WebSocket head | — |
| `entanglement-cli` | _(next)_ opencode-style TUI | — |

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

z.ai/OpenAI/Ollama share one `entanglement-llm::OpenAiLlm`; Anthropic has its own client
(distinct content-block format). No key → `DummyLlm`. Detail in
[`../docs/architecture.md`](../docs/architecture.md) §5b.

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
`input_schema`); `host_tools(root)` (see ADR-0008 + ADR-0009) assembles the
host-tool quintet (`read`/`glob`/`grep`/`edit`/`bash`) that the profiles gate.

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

- Host tool quintet (`read`/`glob`/`grep`/`edit`/`bash`) is done in
  `entanglement-core::host` behind `host_tools(root)` ([ADR-0008](../docs/adr/0008-host-tools-workdir-and-bounded-output.md)
  trio + [ADR-0009](../docs/adr/0009-edit-and-bash-host-tools.md) `edit`/`bash`);
  the `skutter` binary wires it from the cwd. `bash` is **not** sandboxed —
  permission profiles are the only gate; a real sandbox is the next
  security-focused ADR.
- `entanglement-ws` (axum) and `entanglement-cli` (TUI) heads.

LLM providers are wired (`entanglement-llm`, ADR-0007): `Llm` is a streaming trait
returning `BoxStream<LlmEvent>`; one generic OpenAI-compat client serves z.ai
(primary)/OpenAI/Ollama, with a separate Anthropic client. `skutter` picks
one via `ENTANGLEMENT_PROVIDER` or key auto-detect, else `DummyLlm`.
