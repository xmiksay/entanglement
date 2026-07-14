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
tool exec/approval over the protocol). Dependency direction is
`provider (leaf) ← core ← runtime`: the `Llm` trait + DTOs live in **provider**,
core depends on provider ([ADR-0053](../docs/adr/0053-invert-core-provider-seam.md),
inverting [ADR-0006](../docs/adr/0006-core-dependency-hygiene-gate.md)/[ADR-0007](../docs/adr/0007-streaming-llm-and-provider-crate.md)).

| Crate | Role | Hard rule |
| --- | --- | --- |
| `entanglement-provider` | **leaf** crate, owns the LLM ABI: the `Llm` **trait** + DTOs (`LlmRequest`/`Event`/`Stream`, `LlmFactory`, `ToolCall`, `ToolSpec`, `Message`/`MessageRole`, `Dummy`/`EchoLlm`); all LLM I/O — generic OpenAI-compat client (z.ai GLM — primary, OpenAI, Ollama) + separate Anthropic client, via `reqwest`; **per-endpoint** connection pool + retry + rate-limit (keyed by base URL + API-key hash, [ADR-0050](../docs/adr/0050-per-endpoint-connection-pool-retry-rate-limit.md)), reasoning stream, models-per-provider. Per-session state is deliberately absent: the `llm` a session owns is a plain `Box<dyn Llm>`, the former `LlmSession` newtype collapsed since resilience is per-endpoint not per-session ([ADR-0062](../docs/adr/0062-collapse-llmsession-placeholder-newtype.md), #195). Usable **standalone** for raw LLM queries. | no `entanglement-*` deps; owns `reqwest`. |
| `entanglement-core` | actor engine: `Holly`, protocol, **agent turn loop**, `Context` (built on provider's `Message`). Advertises tool *schemas* (`ToolSpec`) only — holds no executable tools. Depends on provider, drives `dyn Llm`, re-exports the ABI. | **No UI/web-server deps** (`clap`/`axum`/`crossterm`/`ratatui` forbidden); `reqwest`/`hyper`/`tower` are transitive via provider ([ADR-0053](../docs/adr/0053-invert-core-provider-seam.md)). `make tree` enforces. |
| `entanglement-runtime` | the head crate (binary `skutter`): the **`Tool` trait + `ToolRegistry`** (moved from core ✅ #206, [ADR-0059](../docs/adr/0059-tool-trait-and-registry-live-in-the-runtime.md)), **host tools** (impls moved from core ✅), **tool execution** (`tool_runner`, moved from core ✅ #58), **permission dispatch + approval** (moved from core ✅ #59), user sessions, stdio `run`/`pipe`, `tui`, and the `sessions`/`inspect` subcommands today, `serve` (WS) next. Selects the concrete provider via `ENTANGLEMENT_PROVIDER` or key auto-detect and glues it to core. All transports packaged here ([ADR-0010](../docs/adr/0010-single-head-crate-and-bash-opt-in.md)). Feature-gated: `cli` (clap + log init) / `provider` (LLM providers, split from `cli` in #208) / `tui` (`default = ["tui"]`) build the binary; the crate also exposes a lean library ([ADR-0025](../docs/adr/0025-runtime-cargo-feature-gates.md)). `main.rs` imports the library modules from the lib crate — only `pipe`/`run`/`tui` stay bin-local (#208). | `--no-default-features` must stay CLI/TUI-free (`reqwest` rides in via core); `make check-lean` enforces ([ADR-0025](../docs/adr/0025-runtime-cargo-feature-gates.md) + [ADR-0053](../docs/adr/0053-invert-core-provider-seam.md)). |

`entanglement-runtime` depends on core; core depends on provider; provider
depends on neither.

## Commands — drive through `make`

```bash
make run           # stdio head, one turn (text)
make run-json      # one turn, NDJSON events (opencode run --format json)
make run-tui       # launch the terminal UI
make test          # unit + integration
make test-unit | make test-integration
make coverage      # workspace line coverage via llvm-cov, fail under COV_MIN%
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
`default_temperature`/`max_output_tokens`/`thinking_budget_tokens`) + **pricing**
(USD/M: input/output/cached_input/cache_write). Those flags are no longer
write-only (✅ #191): `ModelEntry::generation_params()` gates them into a
`GenerationParams { temperature, max_output_tokens, thinking_budget_tokens }` the
runtime resolves onto `EngineConfig::generation` and core threads onto every
`LlmRequest`; each client maps the present knobs to its wire and omits the rest
(OpenAI: `temperature`+`max_tokens`; Anthropic: `max_output_tokens` +
`thinking` when a budget is set, else `temperature`). Precedence: **env > user
YAML > embedded defaults**. See `entanglement-provider::catalog`.

z.ai/OpenAI/Ollama share one `entanglement-provider::OpenAiLlm`; Anthropic has its own client (distinct content-block
format). No key → `EchoLlm`. Detail in
[`../docs/architecture.md`](../docs/architecture/provider.md). **Per-endpoint**
connection pool, retry/backoff, rate-limit (429/`Retry-After`/RPM keyed by base
URL + API-key hash, ✅ #217, [ADR-0050](../docs/adr/0050-per-endpoint-connection-pool-retry-rate-limit.md)),
reasoning/thinking stream events, the YAML provider/model catalog, and the
provider-owned LLM backend (a plain `Box<dyn Llm>` — the empty `LlmSession`
placeholder was collapsed, ✅ #195/[ADR-0062](../docs/adr/0062-collapse-llmsession-placeholder-newtype.md))
all live in this crate now (✅ #52–#55, #118, #195, #217,
[ADR-0007](../docs/adr/0007-streaming-llm-and-provider-crate.md)).

## The contract (read before touching the engine)

`entanglement-core/src/protocol.rs` is the single set of types every head uses:

```
InMsg    : Prompt | Approve | Reject | ToolResult | AnswerQuestion | Stop
          | SetAgent | SetModel | Spawn | ListSessions | CloseSession
          | Resume (internal, not serialized)
OutEvent : SessionStarted | SessionEnded | SessionList | Status | AgentChanged | ModelChanged
          | Plan | TextDelta | ReasoningDelta | ToolCallDelta | ToolCall | ToolRequest | ToolExec
          | UserQuestion | ToolOutput | TaskList | Usage | Error | Done | FileChange
```

Load-bearing invariants (details in the split architecture docs — do **not**
re-document them here):

- **Tool execution is a protocol round-trip, parked as data** (#58, #270,
  [ADR-0061](../docs/adr/0061-parked-turn-state-batch-tool-resolution.md)): a
  round ending in tool calls batch-emits `ToolExec` for **every** call up front
  and parks the turn as explicit serde state (`Session.turn: Option<TurnState>`,
  pending set + round counter); `ToolResult`s resolve in **any order** (the
  runtime executor or any external resolver answers), the turn re-enters on
  drain. Replay reconstructs a mid-turn tail; resume re-offers pending calls
  at-least-once — the event log + `Holly::resume` is the embedder persistence
  seam (no DB in-repo). Core holds no executable
  tools and makes no policy call — only schemas (`EngineConfig.tool_specs` +
  per-profile `profile_tool_specs`, #119).
- **Permission lives entirely in the runtime** (#59): `tool_runner` resolves
  `Allow`/`Ask`/`Deny` per call, emits `ToolRequest` on `Ask`, consumes
  `Approve`/`Reject` off `Holly::subscribe_inbound()`. Core never reads
  `PermissionProfile`. Rule keys are name-or-`*` **or** argument-scoped
  `tool(pattern)` (#173: command for `bash`/`call`, path for `edit`/`write`/
  `read`), matched against the call input the runtime extracts
  (`permission::permission_arg`) — the `PermissionProfile::resolve(name, arg)`
  glob is the only core surface. A user config file (#172) adds a global
  permission **ceiling** clamped least-privilege over every grade
  (`clamp_to_base`); see `entanglement-runtime/src/config`. `Approve` carries a
  `scope` (#174, [ADR-0052](../docs/adr/0052-approval-scope-and-persisted-grants.md)):
  `Session`/`Always` record an exact `(tool, arg)` grant in `runtime::grants`
  that upgrades a later resolved `Ask` → `Allow` (never a `Deny`, applied *after*
  the ceiling); `Always` persists to a managed `${config_dir}/entanglement/grants.yml`
  (sibling of `config.yml`, not its ceiling section).
- **Session-multiplexed**: every frame carries `SessionId`; content frames carry
  monotonic `seq`. Supervisor-global vs session-scoped routing is explicit.
- **Model/provider switch is live, not a restart** (#218,
  [ADR-0063](../docs/adr/0063-realtime-model-provider-switch.md)): `SetModel {
  provider, model }` re-resolves against a runtime-supplied resolver held on
  `EngineConfig::model_resolver` (`Option<ModelResolver>`, the core↔runtime seam —
  the entry→`Llm` mapping lives in the runtime, so core calls a captured closure),
  rebuilds `Session::llm`, and retargets the per-session effective model +
  `generation` + context-window budget without restarting the engine. Emits
  `ModelChanged` (unknown provider / missing key → `Error`); deferred during a live
  turn like `SetAgent`, and replay re-applies it to re-bind a resumed session. The
  TUI `/model` picker now drives it end-to-end. The former `LlmSession` placeholder
  ([ADR-0062](../docs/adr/0062-collapse-llmsession-placeholder-newtype.md)) stayed
  collapsed: the switch lives on `Session` fields, not a re-introduced newtype.
- **Definitions are data, layered** embedded < user < project, later wins; the
  project layer is **trusted** ([ADR-0047](../docs/adr/0047-local-trust-boundary.md)).
  Agents (`ENTANGLEMENT_AGENTS_DIR`), skills (`ENTANGLEMENT_SKILLS_DIR`), the
  provider catalog (`ENTANGLEMENT_PROVIDERS_FILE`), and the **user config file**
  (#172, `${config_dir}/entanglement/config.yml` < `.entanglement/config.yml`) all
  share this loader. Provider API **keys** live in a sibling managed env file (#220,
  `${config_dir}/entanglement/.env`, override `ENTANGLEMENT_ENV_FILE`): scaffolded
  commented on first run, loaded at startup into the process env for vars the real
  env left unset (env > file), kept out of any repo. The config's `hooks:` section
  (#199, [ADR-0066](../docs/adr/0066-lifecycle-hooks-as-runtime-interceptors.md))
  wires **lifecycle hooks** — `sh -c` commands run as a **runtime interceptor**
  around the generic tool dispatch (`pre_tool_use` non-zero exit *vetoes* the
  call; `post_tool_use` is an observational side-effect) and off the inbound
  `Prompt` fan-out (`user_prompt_submit`), each in its own process group. Scoped
  to the generic `Intercept::Permission` route (orchestration + `rhai` bypass);
  wired via `tool_runner::spawn_tool_executor_with_hooks`.

| Topic | Module |
| --- | --- |
| `InMsg`/`OutEvent`, Plan/TaskList events | [protocol](../docs/architecture/protocol.md) |
| profiles, tool mask, spawn gating, plan authority, skills, prompt assembly | [agents & permissions](../docs/architecture/agents-and-permissions.md) |
| turn loop, tool round-trip, steering, cancellation | [engine](../docs/architecture/engine.md) |
| streaming client, catalog, pool/retry/rate-limit | [provider](../docs/architecture/provider.md) |
| stdio/TUI/`serve` heads, event-sourced persistence | [heads & persistence](../docs/architecture/heads-and-persistence.md) |
| dependency gates, the quintet + exec tools (`bash`/`call`/`bash_output`/`rhai`), lifecycle hooks | [gates & host tools](../docs/architecture/gates-and-host-tools.md) |

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
epic (#111), the inspection & debuggability epic (#183 — `inspect`
prompt/agents/skills, `RUST_LOG`/file-sink tracing, `EchoLlm` prompt echo,
per-resolution permission tracing, in-session TUI overlay), and the permission
model & user-configuration epic (#171 — layered user config file + permission
ceiling #172, argument-scoped rule keys #173, approval scope + persisted "always
allow" grants #174, `update_plan`/`update_tasks` demoted to permission-gated
runtime state tools #175/#231, first-run config scaffold #219, managed
provider-key env file #220), and the engine-robustness epic (#176 — inner-loop
`MAX_TURNS` reset per prompt #177, per-model context budget with tool-output
compaction + refuse-over-window #178, `Stop` raced against the stream via
`tokio::select!` #179, `CloseSession` cascade over the spawn sub-tree #180,
interrupted-partial commit + single mid-stream retry #181, mid-turn `Prompt`
folded into the live turn #182/[ADR-0058](../docs/adr/0058-mid-turn-prompt-folds-into-live-turn.md)),
and the security & filesystem-containment epic (#161 — project-local definitions
trusted-by-design #162/[ADR-0047](../docs/adr/0047-local-trust-boundary.md)
(the mitigation is inspection, not restriction), canonicalizing symlink-safe root
containment for `read`/`edit`/`write` + `glob`/`grep`
#163/[ADR-0054](../docs/adr/0054-canonicalizing-symlink-safe-root-containment.md),
provider API keys scrubbed from `bash`/`call` child env #164, opt-in symmetric
request-body logging behind `ENTANGLEMENT_LOG_BODIES` #165),
and the architecture, seams & build-hygiene epic (#200 — built-in profile trio
deduped to a single source #201, `OutEvent::FileChange` given a real emitter in
the executor #202/[ADR-0060](../docs/adr/0060-filechange-audit-via-executor-as-path-kind-hash.md),
`tool_runner`'s interception ladder made an explicit pipeline #203, registry
loaders unified with a shared env-override-honoring loader #204, seam plumbing
deduped — one `reply`/approval-park helper + a `tool_names` module #205,
`Tool`/`ToolRegistry` moved to the runtime with the dead core surface dropped
#206/[ADR-0059](../docs/adr/0059-tool-trait-and-registry-live-in-the-runtime.md),
hygiene gates fixed to fail loudly and widened past ADR-0006 via shared
`scripts/dep-gate.sh` #207, and `main.rs` reworked to import the lib modules with
the `cli`/`provider` features split #208),
and the command-execution-maturity epic (#166 — exec tools (`bash`/`call`)
spawned in their **own process group** (`process_group(0)`) so a timeout/cancel
SIGKILLs the whole tree and grandchildren can't orphan #168, timeouts return the
**partial output buffered before the kill** instead of discarding it #169, `Stop`
aborts the in-flight tool task whose drop-guard group-kills the same tree #167,
and `bash` gains `workdir` + `run_in_background` (detached, polled via
`bash_output`) with head+tail truncation so the trailing error survives #170),
and the provider / `Llm` seam epic (#190 — generation-parameter channel wired
from catalog capabilities into every `LlmRequest` #191, `LlmEvent::Finish`
usage/stop-reason surfaced end-to-end via `OutEvent::Usage` #192, ADR-0007
retry/backoff + per-endpoint rate-limit made live #193/#217/[ADR-0050](../docs/adr/0050-per-endpoint-connection-pool-retry-rate-limit.md),
streaming tool-arg deltas via `LlmEvent::ToolCallDelta`/`OutEvent::ToolCallDelta`
#194, the empty `LlmSession` placeholder collapsed to a plain `Box<dyn Llm>`
#195/[ADR-0062](../docs/adr/0062-collapse-llmsession-placeholder-newtype.md), and
realtime model/provider switch without an engine restart
#218/[ADR-0063](../docs/adr/0063-realtime-model-provider-switch.md))
are **complete**.
Current phase is the July 2026 audit backlog — thematic epics tracked on GitHub
with P0/P1/P2 labels and blocked-by links:
#209 (docs), the parked-turn-state epic #276 (turns park as explicit serde
`TurnState`, batch-parallel tool resolution, mid-turn replay/resume,
[ADR-0061](../docs/adr/0061-parked-turn-state-batch-tool-resolution.md)),
with WebSocket `serve` (#153) deliberately last.

Shipped foundations: streaming `Llm` providers ([ADR-0007](../docs/adr/0007-streaming-llm-and-provider-crate.md))
— z.ai (primary)/OpenAI/Ollama via one OpenAI-compat client + a separate
Anthropic client; `ENTANGLEMENT_PROVIDER` or key auto-detect, else `EchoLlm`.
Heads: stdio `run`/`pipe`, `tui`, and the `sessions`/`inspect` subcommands. Tools:
the root-contained quintet (`read` on an image file — `png`/`jpg`/`jpeg`/`gif`/
`webp` — emits a base64 **image content block** through a now-multimodal
`ToolResult`/`ToolOutput` path, #221/[ADR-0065](../docs/adr/0065-read-emits-image-content-blocks.md),
built on the `Message`/`Prompt` content-block migration #197/[ADR-0064](../docs/adr/0064-message-content-blocks.md)),
the opt-in exec set `bash`/`call`/`bash_output`
(`ENTANGLEMENT_ENABLE_BASH=1`; `bash` gains `workdir` + `run_in_background`, polled
via `bash_output`, #170), and the sandboxed `rhai` tool. `skutter serve`
(axum WS, local-only, [ADR-0048](../docs/adr/0048-serve-head-local-trust-model.md))
is the next head.
