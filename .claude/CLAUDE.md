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
| `entanglement-provider` | all LLM I/O: generic OpenAI-compat client (z.ai GLM — primary, OpenAI, Ollama) + separate Anthropic client, via `reqwest`; **per-endpoint** connection pool + retry + rate-limit (keyed by base URL + API-key hash, so multiple keys each get their own limit, [ADR-0050](../docs/adr/0050-per-endpoint-connection-pool-retry-rate-limit.md)), reasoning stream, models-per-provider, provider-owned session handle; implements `entanglement_core::Llm` | may depend on transport crates (`reqwest`); never depended on by `entanglement-core` |
| `entanglement-runtime` | the head crate (binary `skutter`): **host tools** (impls moved from core ✅), **tool execution** (`tool_runner`, moved from core ✅ #58), **permission dispatch + approval** (moved from core ✅ #59), user sessions, stdio `run`/`pipe` + `tui` today, `serve` (WS) next. Selects provider via `ENTANGLEMENT_PROVIDER` or key auto-detect. All transports packaged here ([ADR-0010](../docs/adr/0010-single-head-crate-and-bash-opt-in.md)). Feature-gated: `cli`/`tui` (`default = ["tui"]`) build the binary; the crate also exposes a lean library ([ADR-0025](../docs/adr/0025-runtime-cargo-feature-gates.md)). | `--no-default-features` must stay CLI/TUI/transport-free; `make check-lean` enforces ([ADR-0025](../docs/adr/0025-runtime-cargo-feature-gates.md)). |

Heads depend on core, **never** the reverse.

## Commands — drive through `make`

```bash
make run           # stdio head, one turn (text)
make run-json      # one turn, NDJSON events (opencode run --format json)
make test          # unit + integration
make test-unit | make test-integration
make lint          # clippy --all-targets -D warnings
make fmt | check-fmt
make verify        # check-fmt + tree + check-lean + lint + test  (CI-equivalent gate)
make tree          # entanglement-core dep hygiene gate (fails on UI/transport crates)
make check-lean    # runtime --no-default-features stays CLI/TUI/transport-free (ADR-0025)
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

That table is now **catalog data, not hardcode** (✅ #118): the provider/model
list is YAML — an embedded default (`entanglement-provider/src/defaults.yml`)
deep-merged with an optional user override at
`${config_dir}/entanglement/providers.yml` (path override:
`ENTANGLEMENT_PROVIDERS_FILE`). Merge is by `name` (providers) / `id` (models) at
the `serde_yaml::Value` level, `deny_unknown_fields` on the final parse. A
`wire: openai | anthropic` tag lets a user add **any** OpenAI-compatible endpoint
(proxy, vLLM, new vendor) with zero code change; `ENTANGLEMENT_PROVIDER=<name>`
resolves against the catalog, so custom providers are selectable. `ModelEntry`
adds capability flags (`supports_thinking`/`supports_temperature`/
`default_temperature`) + **pricing** (USD/M: input/output/cached_input/
cache_write). Precedence: **env > user YAML > embedded defaults**. See
`entanglement-provider::catalog`.

z.ai/OpenAI/Ollama share one `entanglement-provider::OpenAiLlm`; Anthropic has its own client (distinct content-block
format). No key → `EchoLlm`. Detail in
[`../docs/architecture.md`](../docs/architecture/provider.md). **Per-endpoint**
connection pool, retry/backoff, rate-limit (429/`Retry-After`/RPM keyed by base
URL + API-key hash, ✅ #217, [ADR-0050](../docs/adr/0050-per-endpoint-connection-pool-retry-rate-limit.md)),
reasoning/thinking stream events, the YAML provider/model catalog, and the
provider-owned session handle all live in this crate now (✅ #52–#55, #118, #217,
[ADR-0007](../docs/adr/0007-streaming-llm-and-provider-crate.md)).

## The contract (read before touching the engine)

`entanglement-core/src/protocol.rs` is the single set of types every head uses:

```
InMsg    : Prompt | Approve | Reject | ToolResult | AnswerQuestion | Stop
          | SetAgent | Spawn | ListSessions | CloseSession
          | Resume (internal, not serialized)
OutEvent : SessionStarted | SessionEnded | SessionList | Status | AgentChanged
          | Plan | TextDelta | ReasoningDelta | ToolCall | ToolRequest | ToolExec
          | UserQuestion | ToolOutput | TaskList | Error | Done | FileChange
```

Load-bearing invariants (details in the split architecture docs — do **not**
re-document them here):

- **Tool execution is a protocol round-trip** (#58): core emits `ToolExec` for
  every host tool and awaits the runtime's `ToolResult`. Core holds no executable
  tools and makes no policy call — only schemas (`EngineConfig.tool_specs` +
  per-profile `profile_tool_specs`, #119).
- **Permission lives entirely in the runtime** (#59): `tool_runner` resolves
  `Allow`/`Ask`/`Deny` per call, emits `ToolRequest` on `Ask`, consumes
  `Approve`/`Reject` off `Holly::subscribe_inbound()`. Core never reads
  `PermissionProfile`. A user config file (#172) adds a global permission
  **ceiling** clamped least-privilege over every grade (`clamp_to_base`); see
  `entanglement-runtime/src/config`.
- **Session-multiplexed**: every frame carries `SessionId`; content frames carry
  monotonic `seq`. Supervisor-global vs session-scoped routing is explicit.
- **Definitions are data, layered** embedded < user < project, later wins; the
  project layer is **trusted** ([ADR-0047](../docs/adr/0047-local-trust-boundary.md)).
  Agents, skills, the provider catalog, and the **user config file** (#172,
  `${config_dir}/entanglement/config.yml` < `.entanglement/config.yml`) all share
  this loader.

| Topic | Module |
| --- | --- |
| `InMsg`/`OutEvent`, Plan/TaskList events | [protocol](../docs/architecture/protocol.md) |
| profiles, tool mask, spawn gating, plan authority, skills, prompt assembly | [agents & permissions](../docs/architecture/agents-and-permissions.md) |
| turn loop, tool round-trip, steering, cancellation | [engine](../docs/architecture/engine.md) |
| streaming client, catalog, pool/retry/rate-limit | [provider](../docs/architecture/provider.md) |
| stdio/TUI/`serve` heads, event-sourced persistence | [heads & persistence](../docs/architecture/heads-and-persistence.md) |
| dependency gates, the quintet + exec tools (`bash`/`call`/`rhai`) | [gates & host tools](../docs/architecture/gates-and-host-tools.md) |

Debugging: `skutter inspect prompt|agents|skills|config` re-runs the load-time
discovery with **no engine** and prints the resolved prompt / registries / user
config, including the layer that won an override (✅ #184/#185/#186, #172). The TUI exposes the same three
views in-session via `/inspect` (or `<leader>i`) as a read-only overlay over the
active session's resolved state (✅ #214). Trust & scope decisions:
[ADR-0047](../docs/adr/0047-local-trust-boundary.md) (repo trusted; config
precedence system < user < repo) and
[ADR-0048](../docs/adr/0048-serve-head-local-trust-model.md) (local-only `serve`).

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
  the relevant [`../docs/architecture/`](../docs/architecture/) module to reflect the new *what
  is*, and add an inline ADR link at the relevant section. Never edit an accepted
  ADR in place — supersede it. Drift check: `/arch check`.
- **Keep this brief + the `docs/architecture/` modules in sync.** When a message variant,
  profile, crate, or command changes, update both in the same change.

## Open work (current phase)

The three-layer re-architecture (epic #50), the agents/skills/system-prompt
epic (#111), and the inspection & debuggability epic (#183 — `inspect`
prompt/agents/skills, `RUST_LOG`/file-sink tracing, `EchoLlm` prompt echo,
per-resolution permission tracing, in-session TUI overlay) are **complete**.
Current phase is the July 2026 audit backlog — thematic epics tracked on GitHub
with P0/P1/P2 labels and blocked-by links:
#171 (user config & permissions),
#190 (provider seam + per-endpoint pool), #176 (engine robustness),
#166 (exec-tool maturity), #200 (architecture cleanup), #209 (docs), with
WebSocket `serve` (#153) deliberately last.

Shipped foundations: streaming `Llm` providers ([ADR-0007](../docs/adr/0007-streaming-llm-and-provider-crate.md))
— z.ai (primary)/OpenAI/Ollama via one OpenAI-compat client + a separate
Anthropic client; `ENTANGLEMENT_PROVIDER` or key auto-detect, else `EchoLlm`.
Heads: stdio `run`/`pipe`, `tui`, and the `sessions`/`inspect` subcommands. Tools:
the root-contained quintet, the opt-in exec pair `bash`/`call`
(`ENTANGLEMENT_ENABLE_BASH=1`), and the sandboxed `rhai` tool. `skutter serve`
(axum WS, local-only, [ADR-0048](../docs/adr/0048-serve-head-local-trust-model.md))
is the next head.
