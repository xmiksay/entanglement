# entanglement

Headless, Rust-based AI coding agent **engine**. The reasoning + tool-execution
loop is decoupled from any UI and exposed as an async **actor**: a typed inbox of
`InMsg` and a broadcast outbox of `OutEvent`. Every interface is a thin adapter
over those two methods.

- Architecture & interfaces: [`docs/architecture.md`](docs/architecture.md)
- Deferred work & docs drift ledger: [`docs/deferred-work-ledger.md`](docs/deferred-work-ledger.md)

## Status

Actor core + stdio head + TUI + local WebSocket `serve` head, with real LLM
backends wired (`entanglement-provider`: z.ai/OpenAI/Ollama + Anthropic). All
four interfaces now ship. The three-layer re-architecture (core / provider /
runtime) has landed — see
[`docs/adr/0006`](docs/adr/0006-core-dependency-hygiene-gate.md) and the crate
table below.

## The contract (one set of types, every head)

```
InMsg    : Prompt | Approve | Reject | ToolResult | AnswerQuestion | Stop
          | SetAgent | SetModel | SetGeneration | Oneshot | Spawn | ListSessions | ReplayFrom | CloseSession
          | McpList | McpAdd | McpRemove
          | HibernateSession (trusted-only) | Resume (internal, not serialized) (harness → engine)
OutEvent : SessionStarted | SessionEnded | SessionHibernated | SessionList | History | Status
          | AgentChanged | ModelChanged | GenerationChanged | Plan | TextDelta | ReasoningDelta
          | ToolCallDelta | ToolCall | ToolRequest | ToolExec | UserQuestion
          | McpList | McpChanged
          | ToolOutput | TaskList | Usage | Error | Done | Compacted | FileChange
          | SkillActive | AmbiguousRetry (engine → harness)
```

Every frame is **session-scoped** (one connection multiplexes many sessions via
`SessionId`) and content frames carry a monotonic `seq` for dedup/ordering.

## Four interfaces, one ABI

| Head | Status | What it is |
| --- | --- | --- |
| **ABI (direct)** | ✅ | Hold a `Holly`, call `holly.send(InMsg)` / `holly.subscribe()`. Zero serialization. The foundation. |
| **stdio** (`skutter run` / `skutter pipe`) | ✅ | NDJSON over stdin/stdout — one-shot `run` (text or `--format json`, à la `opencode run`) and bidirectional `pipe`. `skutter sessions` lists past sessions; `skutter inspect prompt --agent <name> [--parts]` prints an agent's assembled system prompt (no engine); `skutter inspect agents [name]` shows the resolved agent registry with layer provenance — a table (name, mode, model, layer, source, mask) or one agent's full resolved profile + what lower layers it overrode; `skutter inspect skills [name] [--disclosures]` does the same for skills — a table (name, user_only, layer, root_dir, description), the exact tier-1 disclosure block the model gets (`--disclosures`), or a dry-run of the `load_skill` path substitution for one skill. |
| **TUI** (`skutter tui`) | ✅ | opencode-style terminal UI streaming `OutEvent`, tool-approval prompts, plan/task panels. |
| **WebSocket** (`skutter serve --port <N>`) | ✅ | axum `/ws` (+ `/healthz`), one `broadcast` fan-out per socket relayed as JSON text frames, inbound frames routed through the untrusted `send_from_wire` path, multiplexed by `SessionId`. **Local, single-user, loopback-bound** (`127.0.0.1` only) — the WS is a general protocol interface (a raw local script is as valid a client as any future client), so the `--allow-origin` check is opt-in, never mandatory ([ADR-0048](docs/adr/0048-serve-head-local-trust-model.md), #153). |

Building your own head — a multi-tenant server embedding the engine as a
library, rather than one of the four above — is covered in
[`docs/embedding.md`](docs/embedding.md): session-per-tenant namespacing, the
`send`/`send_from_wire` trust split, pluggable persistence and tool-execution
policy, dynamic per-session tool/prompt resolvers, hibernate/resume, and
approval-across-restart semantics, backed by three compiling examples —
[`embedded.rs`](entanglement-runtime/examples/embedded.rs),
[`embedded_lifecycle.rs`](entanglement-runtime/examples/embedded_lifecycle.rs),
and [`mcp_http.rs`](entanglement-runtime/examples/mcp_http.rs).

## Agent profiles (opencode-style)

A session runs under an **agent profile** — `{ name, description, mode, model,
system_prompt, permission, tools, disallowed_tools, can_spawn,
spawnable_agents }`. Switch the *primary* profile with `SetAgent`; the cycle is
`build ↔ plan` — `explore` and `debug` are `mode: subagent`, so they are
filtered out of the primary cycle and only reachable via spawn. Built-ins:
`build`, `plan`, `explore`, `debug`.

The permission profile (`Allow | Ask | Deny` per tool, name-or-`*` or
argument-scoped `tool(pattern)`) drives the approval flow. `build` allows
everything; `plan` **asks** by default and, since #140, its `tools:` allowlist
masks `edit`/`write` out of the toolset entirely (so it plans without touching
files) — `explore` is the deny profile (read-only), the default spawn target;
`debug` carries `build`'s own allow-everything permission (read/write/execute)
for a spawned sub-agent that actually needs to reproduce, fix, and verify a
bug. Permission resolution and approval live entirely in the runtime (#59).

Session snapshots (`OutEvent::Plan`, `OutEvent::TaskList` — both markdown
`content`) are orthogonal — emitted by the runtime's `update_plan` /
`update_tasks` state tools: permission-gated but carrying no host resource, so
the runtime intercepts them out of the tool registry and emits the snapshot
instead of dispatching (#231, ADR-0049). Every head renders plan/task panels
natively.

**Definitions are data, layered** (embedded < user < project, later wins).
Agents (`ENTANGLEMENT_AGENTS_DIR`) and skills (`ENTANGLEMENT_SKILLS_DIR`) are
`.md` files with YAML frontmatter — drop one in and it joins the registry with
no code change; `skutter inspect agents|skills` shows the resolved set with
layer provenance. Beyond the root-contained quintet (`read`/`write`/`edit`/
`glob`/`grep`), `call` (argv exec, no shell) and the sandboxed `rhai` scripting
tool are always registered too; only `bash` stays opt-in
(`ENTANGLEMENT_ENABLE_BASH=1`).

## Crates

Three crates, two seams (core ↔ provider, core ↔ runtime). Dependency direction
is `provider (leaf) ← core ← runtime` ([ADR-0053](docs/adr/0053-invert-core-provider-seam.md)).

| Crate | Role | Hard rule |
| --- | --- | --- |
| `entanglement-provider` | **leaf** crate owning the LLM ABI: the `Llm` **trait** + DTOs (`LlmRequest`/`Event`/`Stream`, `LlmFactory`, `ToolCall`/`ToolSpec`) + wire `Message`; z.ai/OpenAI/Ollama + Anthropic clients; connection pool, retry, rate-limit, reasoning stream. Usable **standalone** for raw LLM queries. | no `entanglement-*` deps; owns `reqwest`. |
| `entanglement-core` | actor engine: `Holly`, `InMsg`/`OutEvent`, agent turn loop, `Context`. Advertises tool *schemas* (`ToolSpec`) only — holds no executable tools. Depends on provider, drives `dyn Llm`, re-exports the ABI. | **No UI/web-server deps** (`clap`/`axum`/`crossterm`/`ratatui` forbidden); `reqwest` is transitive via provider (ADR-0053). Enforced via `make tree`. |
| `entanglement-runtime` | the head crate (binary `skutter`): the `Tool` **trait** + `ToolRegistry` (moved from core, ADR-0059), host-tool impls, tool execution + permission dispatch (✅ #58/#59/#206), approval, user sessions, all transports (stdio ✅, TUI ✅, WS ✅ #153). Selects the concrete provider + glues it to core. Feature-gated `cli`/`provider`/`tui`/`serve`/`mcp-http` (`default = ["tui", "serve", "mcp-http"]`); `--no-default-features` is a lean embeddable library (ADR-0025). | `--no-default-features` stays CLI/TUI/transport-free; `make check-lean` enforces. |

## Install

```bash
cargo install entanglement-runtime   # installs the `skutter` binary
```

## Set a provider key

Provider API keys live in a managed env file
(`${config_dir}/entanglement/.env`, override `ENTANGLEMENT_ENV_FILE`), loaded at
startup for any var the real environment left unset (env > file). Set one without
hand-editing the file (#304, [ADR-0073](docs/adr/0073-managed-env-file-writer-and-key-surfaces.md)):

```bash
skutter config set-key zai                 # hidden prompt (never echoed)
skutter config set-key openai --key sk-…   # or pass it directly
echo "sk-…" | skutter config set-key zai   # or pipe it (scripting/CI)
```

Inside the TUI, `/key` opens the same dialog (provider list → masked input); a key
set there is picked up on the next `/model` switch with no restart. No key set →
`skutter` falls back to the `EchoLlm` debug stub.

## Build & develop

Requires stable Rust (pinned via `rust-toolchain.toml`). Build jobs capped at 4
in `.cargo/config.toml`.

```bash
make run          # one dummy turn, text output
make run-json     # one dummy turn, NDJSON events
make run-tui      # launch the terminal UI
make serve        # local WebSocket head on 127.0.0.1 (ARGS='--port 4517')
make test         # unit + integration
make lint         # clippy --all-targets -D warnings
make verify       # check-fmt + tree + check-lean + lint + test (CI-equivalent)
make tree         # cargo tree -p entanglement-core (UI/web-server dep hygiene gate)
make check-lean   # runtime --no-default-features stays CLI/TUI-free (ADR-0025 + ADR-0053)
make coverage     # cargo llvm-cov --workspace, fails under COV_MIN% (release gate)
make install      # cargo install --path entanglement-runtime → `skutter` in $CARGO_HOME/bin
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
