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
- No web framework in core; the runtime head's `serve` subcommand brings `axum` (behind its own `serve` feature, ✅ #153).

## Workspace

Three crates, two seams (core↔provider via the `Llm` trait, core↔runtime for
tool exec/approval over the protocol). Dependency direction is
`provider (leaf) ← core ← runtime`: the `Llm` trait + DTOs live in **provider**,
core depends on provider ([ADR-0053](../docs/adr/0053-invert-core-provider-seam.md),
inverting [ADR-0006](../docs/adr/0006-core-dependency-hygiene-gate.md)/[ADR-0007](../docs/adr/0007-streaming-llm-and-provider-crate.md)).

| Crate | Role | Hard rule |
| --- | --- | --- |
| `entanglement-provider` | **leaf** crate, owns the LLM ABI: the `Llm` **trait** + DTOs (`LlmRequest`/`Event`/`Stream`, `LlmFactory`, `ToolCall`, `ToolSpec`, `Message`/`MessageRole`, `Dummy`/`EchoLlm`); all LLM I/O — generic OpenAI-compat client (z.ai GLM — primary, OpenAI, Ollama) + separate Anthropic client + native Gemini client (#309), via `reqwest`; **per-endpoint** connection pool + retry + rate-limit (keyed by base URL + API-key hash, [ADR-0050](../docs/adr/0050-per-endpoint-connection-pool-retry-rate-limit.md)), reasoning stream, models-per-provider. Per-session state is deliberately absent: the `llm` a session owns is a plain `Box<dyn Llm>`, the former `LlmSession` newtype collapsed since resilience is per-endpoint not per-session ([ADR-0062](../docs/adr/0062-collapse-llmsession-placeholder-newtype.md), #195). Usable **standalone** for raw LLM queries. | no `entanglement-*` deps; owns `reqwest`. |
| `entanglement-core` | actor engine: `Holly`, protocol, **agent turn loop**, `Context` (built on provider's `Message`). Advertises tool *schemas* (`ToolSpec`) only — holds no executable tools. Depends on provider, drives `dyn Llm`, re-exports the ABI. | **No UI/web-server deps** (`clap`/`axum`/`crossterm`/`ratatui` forbidden); `reqwest`/`hyper`/`tower` are transitive via provider ([ADR-0053](../docs/adr/0053-invert-core-provider-seam.md)). `make tree` enforces. |
| `entanglement-runtime` | the head crate (binary `skutter`): the **`Tool` trait + `ToolRegistry`** (moved from core ✅ #206, [ADR-0059](../docs/adr/0059-tool-trait-and-registry-live-in-the-runtime.md)), **host tools** (impls moved from core ✅), **tool execution** (`tool_runner`, moved from core ✅ #58), **permission dispatch + approval** (moved from core ✅ #59), user sessions, stdio `run`/`pipe`, `tui`, the `sessions`/`inspect` subcommands, and the local WebSocket `serve` head (axum, ✅ #153, [ADR-0048](../docs/adr/0048-serve-head-local-trust-model.md)). Selects the concrete provider via `ENTANGLEMENT_PROVIDER` or key auto-detect and glues it to core. All transports packaged here ([ADR-0010](../docs/adr/0010-single-head-crate-and-bash-opt-in.md)). Feature-gated: `cli` (clap + log init) / `provider` (LLM providers, split from `cli` in #208) / `tui` / `serve` (axum WS, implies `cli`+`provider`) / `mcp-http` (streamable-HTTP MCP transport, [ADR-0080](../docs/adr/0080-mcp-streamable-http-transport.md)); `default = ["tui", "serve", "mcp-http"]` builds the binary; the crate also exposes a lean library ([ADR-0025](../docs/adr/0025-runtime-cargo-feature-gates.md)). `main.rs` imports the library modules from the lib crate — only `pipe`/`run`/`tui` stay bin-local (#208; `serve` lives in the lib as `runtime::serve`). | `--no-default-features` must stay CLI/TUI/transport-free (`reqwest` rides in via core; `axum` stays behind `serve`); `make check-lean` enforces ([ADR-0025](../docs/adr/0025-runtime-cargo-feature-gates.md) + [ADR-0053](../docs/adr/0053-invert-core-provider-seam.md)). |

`entanglement-runtime` depends on core; core depends on provider; provider
depends on neither.

## Commands — drive through `make`

```bash
make help         # list every target with its one-line description
make run           # stdio head, one turn (text)
make run-json      # one turn, NDJSON events (opencode run --format json)
make run-tui       # launch the terminal UI
make pipe          # stdio pipe head — InMsg NDJSON on stdin, OutEvent NDJSON on stdout
make serve         # local WebSocket head on 127.0.0.1 (ARGS='--port 4517')
make sessions      # list past (resumable) sessions
make inspect       # resolved prompt/agents/skills, no engine (ARGS='prompt --agent build')
make install       # install the `skutter` binary into $CARGO_HOME/bin
make test          # unit + integration
make test-unit | make test-integration
make coverage      # workspace line coverage via llvm-cov, fail under COV_MIN%
make lint          # clippy --all-targets -D warnings
make fmt | check-fmt
make verify        # check-fmt + tree + check-lean + file-cap + lint + test  (CI-equivalent gate)
make tree          # entanglement-core dep hygiene gate (fails on UI/transport crates)
make check-lean    # runtime --no-default-features stays CLI/TUI/transport-free (ADR-0025)
make file-cap      # 400-line file cap gate (issue #451; grandfathered debt in scripts/file-cap-allowlist.txt)
make test-gates    # dep-gate self-test (scripts/dep-gate.test.sh)
make tag           # cut a release tag (VERSION=vX.Y.Z): refuses dirty tree / red verify
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
| `gemini` | Gemini `:streamGenerateContent` | `GEMINI_API_KEY` | `GEMINI_MODEL` (`gemini-2.5-flash`) | `GEMINI_API_BASE` |

That table is now **catalog data, not hardcode** (✅ #118): the provider/model
list is YAML — an embedded default (`entanglement-provider/src/defaults.yml`)
deep-merged with an optional user override at
`${config_dir}/entanglement/providers.yml` (path override:
`ENTANGLEMENT_PROVIDERS_FILE`). Merge is by `name` (providers) / `id` (models) at
the `serde_yaml::Value` level, `deny_unknown_fields` on the final parse. A
`wire: openai | anthropic | gemini` tag lets a user add **any** OpenAI-compatible
endpoint (proxy, vLLM, new vendor) with zero code change; `ENTANGLEMENT_PROVIDER=<name>`
resolves against the catalog, so custom providers are selectable. `ModelEntry`
adds capability flags (`supports_thinking`/`supports_temperature`/
`default_temperature`/`max_output_tokens`/`thinking_budget_tokens`/
`default_reasoning_effort`) + **pricing** (USD/M: input/output/cached_input/cache_write).
Those flags are no longer write-only (✅ #191): `ModelEntry::generation_params()`
gates them into a `GenerationParams { temperature, max_output_tokens,
thinking_budget_tokens, reasoning_effort }` the runtime resolves onto
`EngineConfig::generation` and core threads onto every `LlmRequest`; each client
maps the present knobs to its wire and omits the rest (OpenAI: `temperature`+
`max_tokens`+`reasoning_effort` (its native field); Anthropic: `max_output_tokens`
+ `thinking` when a budget resolves, else `temperature`; Gemini:
`generationConfig.thinkingConfig.thinkingBudget`). `reasoning_effort`
(`Low|Medium|High`, ✅ #374,
[ADR-0094](../docs/adr/0094-reasoning-effort-and-per-profile-generation-persistence.md))
is OpenAI-native; Anthropic/Gemini have no effort concept, so each maps it onto
a fixed thinking-budget tier when no explicit `thinking_budget_tokens` is set.
Precedence: **env > user YAML > embedded defaults**. See
`entanglement-provider::catalog`.

Runtime env vars (full surface — each is documented inline at the feature that
reads it; this table is the one-place index):

| Env var | Purpose |
| --- | --- |
| `ENTANGLEMENT_PROVIDER` | select provider (`zai`/`openai`/`ollama`/`anthropic`/`gemini`/`echo`); else auto-detect by key |
| `<NAME>_API_KEY` / `<NAME>_MODEL` / `<NAME>_BASE` | per-provider key/model/base (the catalog `key_env`, e.g. `ZAI_API_KEY`) |
| `<NAME>_RPM` / `<NAME>_CONCURRENCY` | per-provider endpoint RPM / in-flight cap (#414), overriding the catalog `rpm`/`concurrency`; `None` ⇒ client default |
| `ENTANGLEMENT_MAX_CONCURRENCY` | last-resort process-wide concurrency override (default 3) |
| `ENTANGLEMENT_LOG_BODIES=1` | opt-in symmetric LLM request-body logging (#165) |
| `ENTANGLEMENT_PROVIDERS_FILE` | override the provider-catalog user file path |
| `ENTANGLEMENT_CONFIG_FILE` | override the layered user config file path (`config.yml`) |
| `ENTANGLEMENT_ENV_FILE` | override the managed provider-key env file path (`.env`) |
| `ENTANGLEMENT_AGENTS_DIR` / `ENTANGLEMENT_SKILLS_DIR` | replace the whole user agents/skills layer (also the cross-vendor opt-out) |
| `ENTANGLEMENT_GRANTS_FILE` / `ENTANGLEMENT_AGENT_MODELS_FILE` / `ENTANGLEMENT_AGENT_GENERATION_FILE` / `ENTANGLEMENT_EXTRA_ROOTS_FILE` | override the four managed runtime files |
| `ENTANGLEMENT_PREAMBLE_FILE` / `ENTANGLEMENT_BRIEF_FILE` | override the system-prompt preamble / project-brief file |
| `ENTANGLEMENT_ENABLE_BASH=1` | opt-in: register the `bash`/`bash_output` exec pair |
| `ENTANGLEMENT_SANDBOX=bwrap` / `ENTANGLEMENT_SANDBOX_NETWORK=1` | bubblewrap-confine `bash`/`call`; opt-in to keep network (#399) |
| `ENTANGLEMENT_ECHO_FULL=1` | `EchoLlm` appends the full system text (debugging) |
| `ENTANGLEMENT_TUI_NOTIFY=1` / `ENTANGLEMENT_TUI_NO_MOUSE` | TUI desktop-notification opt-in / mouse opt-out |
| `ENTANGLEMENT_HOOK_EVENT` / `_SESSION_ID` / `_TOOL_NAME` | set on every hook child's env by the runtime (read-only context, not user-set) |

z.ai/OpenAI/Ollama share one `entanglement-provider::OpenAiLlm`; Anthropic has its own client (distinct content-block
format); **Gemini** has a native `GeminiLlm` (✅ #309,
[ADR-0085](../docs/adr/0085-gemini-native-wire-and-opaque-provider-meta.md)) — not
the OpenAI-compat surface, which drops the `thoughtSignature` a 2.5 thinking model
must round-trip; that opaque per-call token rides the new generic
`ToolCall.provider_meta: Option<Value>` slot (persisted with the ADR-0064 shim,
never inspected by core). No key → `EchoLlm`. Detail in
[`../docs/architecture.md`](../docs/architecture/provider.md). **Per-endpoint**
connection pool, retry/backoff, rate-limit (429/`Retry-After`/RPM keyed by base
URL + API-key hash, ✅ #217, [ADR-0050](../docs/adr/0050-per-endpoint-connection-pool-retry-rate-limit.md);
the shared pool coordinates across sessions ([ADR-0111](../docs/adr/0111-adaptive-endpoint-pacing-and-429-retry-until-clear.md)):
a per-endpoint **concurrency cap** (default 3, permit held across the whole
stream so spawned sub-agents queue instead of 429-storming) that is now
**catalog data mirroring `rpm`** (✅ #414): the provider entry's optional
`concurrency` (env `{NAME}_CONCURRENCY` > user `providers.yml` > embedded
default), falling back to the client default (`RetryConfig::concurrency`, 3,
itself still overridable process-wide via `ENTANGLEMENT_MAX_CONCURRENCY` as the
last-resort fallback), an **adaptive pacing gate** (AIMD `penalize`/`relax` self-tunes RPM),
and a 429 that **parks every concurrent session's window and retries** (5s→10min)
**bounded by ≈15min then surfaces as an error** (so a saturated endpoint fails a
sub-agent's turn rather than hanging its parent)),
reasoning/thinking stream events, the YAML provider/model catalog, and the
provider-owned LLM backend (a plain `Box<dyn Llm>` — the empty `LlmSession`
placeholder was collapsed, ✅ #195/[ADR-0062](../docs/adr/0062-collapse-llmsession-placeholder-newtype.md))
all live in this crate now (✅ #52–#55, #118, #195, #217,
[ADR-0007](../docs/adr/0007-streaming-llm-and-provider-crate.md)).
**Opt-in provider-side web search** (✅ #305,
[ADR-0075](../docs/adr/0075-provider-side-web-search-mvp.md)): a
`WebSearchConfig { enabled, max_uses, allowed_domains }` (`web_search.rs`,
re-exported through core) bound onto a client at build time — never seen by core.
A `#[serde(default)] web_search:` `config.yml` section is threaded as
`Option<WebSearchConfig>` into both client factories **and** the live `/model`
resolver; when present `build_body` pushes the provider's **server-executed**
search tool (z.ai `web_search` entry, Anthropic `web_search_20250305` server tool)
and results surface on the `Reasoning`→`ReasoningDelta` channel (**not** persisted
to history; Anthropic `server_tool_use` → `Reasoning`, never a `ToolCall`).
Enabling *is* consent — it runs **outside** the permission ladder
([ADR-0047](../docs/adr/0047-local-trust-boundary.md)).

## The contract (read before touching the engine)

`entanglement-core/src/protocol.rs` is the single set of types every head uses:

```
InMsg    : Prompt | Approve | Reject | ToolResult | AnswerQuestion | Stop
          | SetAgent | SetModel | SetGeneration | Oneshot | Spawn | ListSessions | ReplayFrom | CloseSession
          | McpList | McpAdd | McpRemove
          | HibernateSession (trusted-only) | Resume (internal, not serialized)
OutEvent : SessionStarted | SessionEnded | SessionHibernated | SessionList | History | Status | AgentChanged | ModelChanged | GenerationChanged
          | McpList | McpChanged
          | Plan | TextDelta | ReasoningDelta | ToolCallDelta | ToolCall | ToolRequest | ToolExec
          | UserQuestion | ToolOutput | TaskList | Usage | Error | Done | Compacted | FileChange
          | SkillActive | AmbiguousRetry
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
  seam (no DB in-repo). In-process, a parked turn also **re-offers** its pending
  batch after `EngineConfig::reoffer_interval` of silence (#274,
  [ADR-0071](../docs/adr/0071-parked-turn-reoffer-timer.md), default 60s) so an
  offer dropped under `broadcast` lag self-heals without a restart; sound only
  because the runtime executor dedupes by `request_id` (per-session in-flight
  set, cleared on the resolving `ToolOutput` / `SessionEnded`) — a re-offer to a
  call it is still running is a no-op. Core holds no executable
  tools and makes no policy call — only schemas (`EngineConfig.tool_specs` +
  per-profile `profile_tool_specs`, #119).
- **Permission lives entirely in the runtime** (#59): `tool_runner` resolves
  `Allow`/`Ask`/`Deny` per call, emits `ToolRequest` on `Ask`, consumes
  `Approve`/`Reject` off `Holly::subscribe_inbound()`. Core never reads
  `PermissionProfile`. Rule keys are name-or-`*` **or** argument-scoped
  `tool(pattern)` (#173: command for `bash`/`call`, path for `edit`/`write`/
  `read`/`glob`, optional file filter for `grep` (#417 — a path, distinct from
  its regex `pattern`; absent → no match)), matched against the call input the
  runtime extracts (`permission::permission_arg`) — the
  `PermissionProfile::resolve(name, arg)` glob is the only core surface. **Path
  tools grade root-relative** (#485,
  [ADR-0125](../docs/adr/0125-permission-arguments-for-path-tools-are-normalized-root-relative.md)):
  `permission_path::grading_arg` wraps `permission_arg` with lexical
  `.`/`..`/`//` folding + a root-prefix strip for the path-arg tools
  (`read`/`edit`/`write`/`apply_patch`/`glob`/`grep`, never `bash`/`call`) when
  a project root is wired, so an absolute in-root path (`/root/src/main.rs`)
  grades and grant-keys identically to its relative spelling (`src/main.rs`);
  an absolute *out*-of-root path stays verbatim. `permission_arg` itself is
  unchanged (still the TUI transcript's literal-display source). A rule
  key may also be a **capability** — `read`/`write`/`call` (#418,
  [ADR-0114](../docs/adr/0114-capability-level-permission-keys.md), part of the
  #416 epic) — fanned out at **parse time** in the shared
  `agents::permission_from_value` (agent frontmatter + the #172 ceiling below)
  into the same literal per-tool rules, so core stays capability-unaware:
  `read`⇒read/grep/glob, `write`⇒edit/write, `call`⇒bash; the literal `call`
  tool and `rhai` are multi-group (`tool_names::MULTI_GROUP`) and graded by the
  least-privileged bare `read`/`write`/`call`/literal-`rhai` grade instead of
  any one capability's fan-out. A user
  config file (#172) adds a global
  permission **ceiling** clamped least-privilege over every grade
  (`clamp_to_base`); see `entanglement-runtime/src/config`. `Approve` carries a
  `scope` (#174, [ADR-0052](../docs/adr/0052-approval-scope-and-persisted-grants.md)):
  `Session`/`Always` record an exact `(tool, arg)` grant in `runtime::grants`
  that upgrades a later resolved `Ask` → `Allow` (never a `Deny`, applied *after*
  the ceiling); `Always` persists to a managed `${config_dir}/entanglement/grants.yml`
  (sibling of `config.yml`, not its ceiling section). A fourth scope,
  `SessionDir` (#486, [ADR-0126](../docs/adr/0126-session-scoped-directory-grants.md)),
  widens a grant to every later call under the approved call's directory
  instead of an exact match — restricted to the read-only triad
  (`read`/`grep`/`glob`; any other tool, or an escape-forced prompt, degrades
  it to an exact `Session` grant) and never persisted (session-only, no
  `Always`-directory scope). Reachable via `[d]` on an approval prompt or the
  TUI `/allow <path>` command. Both policy sources are
  **pluggable seams** (#311, `runtime::policy`): `spawn_tool_executor_with_policy`
  drives an `Arc<dyn PermissionResolver>` (per-call `Allow|Ask|Deny`, async) + an
  `Arc<dyn GrantStore>` (always-allow persistence), so a multi-tenant embedder
  swaps both for its DB without forking the executor — the ancestor clamp
  (ADR-0024) + spawn/mask gating stay in the ladder *on top of* the resolver
  (least privilege still wins). The CLI defaults (`ProfileResolver` +
  `DefaultGrantStore` over `grants::FileGrantStore`) are byte-identical.
  **Execution itself is session-aware too** (#360,
  [ADR-0088](../docs/adr/0088-session-aware-tool-execution.md)):
  `ToolRegistry::execute(&self, call: &ToolCall, session: &SessionId)` threads
  the caller's `SessionId` through to a new default-delegating
  `Tool::run_for_session` (falls back to `run_content`, so every in-tree tool
  is unaffected) — the seam a multi-tenant embedder's own `Tool` needs to
  dispatch per-tenant MCP endpoints or scope a DB-backed tool's writes to the
  caller through one shared registry, closing the gap #311 left between
  session-aware policy and session-blind execution.
- **`rhai` gains exec bindings, gated by the Call capability** (#419, part of
  the #416 epic, [ADR-0115](../docs/adr/0115-rhai-exec-bindings-call-bash.md)
  amending [ADR-0046](../docs/adr/0046-rhai-sandboxed-script-tool.md)):
  `exec(command)`/`exec(command, args)` (marshalled to the `call` tool) and
  `bash(command)` (marshalled to `bash`, bound only when the host `bash` tool
  is registered) join `tool_names::BINDING_TOOLS` (5→7), graded through the
  same chain as the quintet. Named `exec`, not `call`, in script source — `call`
  is a hard-reserved Rhai keyword the interpreter special-cases ahead of any
  same-named registered function — but the dispatched tool name/permission
  grade stay the literal `call`. Two bridge fixes ship alongside: the `Ask`
  approval cache moves from a bare-tool-name key to
  `"{tool}:{permission_arg(tool, input)}"` for `call`/`bash` (approving one
  command can no longer silently pre-clear a different one in the same run),
  and each `exec`/`bash` call derives its `timeout` from the script's own
  remaining wall-clock budget instead of the tool's much longer default,
  since rhai's timeout interrupt can't reach a binding call parked on the
  sync/async bridge.
- **Workdir-scoped permission rules for `bash`/`call`** (#425, part of the #416
  epic, [ADR-0116](../docs/adr/0116-workdir-scoped-permission-rules-for-bash-call.md),
  deferred by #418/[ADR-0114](../docs/adr/0114-capability-level-permission-keys.md)):
  a rule key gains a second, independent scope clause `tool{pattern}` (a
  sibling of the argument-scoped `tool(pattern)`, #173) matching a `bash`/
  `call` call's **`workdir`** instead of its command line — `bash{/tmp/*}:
  allow`, `bash{/etc/*}: deny`. `PermissionProfile::resolve_scoped(name, arg,
  workdir)` is the new three-argument entry point; the existing two-argument
  `resolve` is unchanged, defined as `resolve_scoped(.., workdir: None)`, so a
  `tool{pattern}` rule is inert for any tool with no workdir concept — safe by
  construction. `runtime::permission::permission_workdir` extracts the value
  (mirroring `permission_arg`), threaded through `permission_for`/
  `clamp_to_base`/`effective_permission`; the `PermissionResolver` trait itself
  is untouched (`ProfileResolver::resolve` extracts it internally from the raw
  JSON `input` it already receives). The capability fan-out (#418) mirrors the
  arg-scoped case: `call{pattern}` expands to both `call{pattern}` and
  `bash{pattern}`. The rhai `exec`/`bash` bindings (#419) are **not** covered —
  they marshal no `workdir` field, so a workdir-scoped rule never fires for a
  binding call.
- **MCP tools join the capability fan-out via a config-side hint** (#426, part
  of the #416 epic, [ADR-0117](../docs/adr/0117-mcp-tool-capability-fan-out.md),
  deferred by #418/[ADR-0114](../docs/adr/0114-capability-level-permission-keys.md)):
  an external MCP tool (`mcp__<server>__<tool>`) isn't self-describing — no MCP
  protocol field states its capability — so a bare `read`/`write`/`call` rule
  used to fall through it entirely, grading it only by its own literal name.
  Each `mcp:` server block now accepts an optional `capabilities: {tool:
  read|write|call}` annotation (raw, un-namespaced tool name); `mcp::
  capability_index` folds every server's map into an `McpCapabilityIndex`
  (capability → namespaced tool names, reusing `McpTool`'s own naming helper).
  `agents::expand_capabilities` takes this index as a new parameter and extends
  only the **bare** capability case — `read: allow` also allows an annotated
  MCP tool — leaving argument-/workdir-scoped keys and the `call`/`rhai`
  multi-group untouched (an MCP tool has no command/workdir argument to scope
  against). Computed once at startup from config alone (no live connection
  needed) and threaded into `agents::load_registry`, the config ceiling, and
  the live-reload watcher's snapshot — matching how the ceiling itself is
  already startup-only, not live. An annotation naming a tool the server never
  registers is simply inert. `skutter inspect agents`/`prompt_report`/
  `built_in_registry` keep an empty index — out of scope, since those already
  don't reflect the ceiling clamp either.
- **Trusted/untrusted frame split** (#155,
  [ADR-0069](../docs/adr/0069-trusted-untrusted-wire-frame-split.md)): `Holly::send`
  is the **privileged in-process** inbox (executor/head, trusted for any frame);
  a wire head deserializing untrusted bytes uses `Holly::send_from_wire`, which
  enforces the `InMsg::wire_allowed()` allowlist — since #472/[ADR-0124](../docs/adr/0124-wire-refused-mcp-mutation-and-stdio-key-scrub.md)
  an **explicit fail-closed `match`** (a new variant is wire-refused until
  deliberately opted in) — and refuses (`WireError`) the trusted-only set:
  `ToolResult` (a forged one resolves a parked turn on
  `request_id` alone, bypassing execution + permission), `Spawn` (bypasses
  `spawn_refusal`), `Resume` (internal), `HibernateSession` (#318), and
  `McpAdd`/`McpRemove` (ADR-0124: an unapproved `McpAdd` spawns an arbitrary
  local subprocess; the read-only `McpList` stays wire-allowed and the TUI
  `/mcp` path is unaffected — it sends over the privileged `Holly::send`).
  The executor folds results back over the
  named privileged `Holly::submit_tool_result` handle (via `seam::reply_content`);
  `pipe` calls `send_from_wire`. Local single-user scope
  ([ADR-0048](../docs/adr/0048-serve-head-local-trust-model.md)) → robustness/UX,
  not remote-attacker defence; the WS `serve` head both calls `send_from_wire`
  and implements per-connection `Approve` ownership (#402,
  [ADR-0107](../docs/adr/0107-ws-per-connection-approval-ownership.md)):
  session-scoped, first-writer-wins (the first connection to send any frame for
  a session claims its `Approve`/`Reject`/`AnswerQuestion` decisions; a later
  connection's decision frame is refused, logged, connection unaffected),
  released on disconnect so a still-parked approval doesn't deadlock behind a
  client that went away.
- **Session-multiplexed**: every frame carries `SessionId`; content frames carry
  monotonic `seq`. Supervisor-global vs session-scoped routing is explicit.
  `(session, seq)` is **unique across every authored content event** (#157,
  [ADR-0068](../docs/adr/0068-shared-per-session-seq-counter.md)): the seq comes
  from one per-session counter (`Session.seq: Arc<AtomicU64>`) shared by the core
  session task and the runtime via a supervisor-held registry, so a
  runtime-authored event minted while the session is parked — an approval
  `ToolRequest`/`UserQuestion`, a `Plan`/`TaskList` snapshot, a `FileChange` —
  mints a fresh seq via `Holly::emit_for_session` instead of reusing the parked
  `ToolExec` seq; seq-less `Status` goes through `Holly::emit_status` (the raw
  outbound sender is no longer exposed). The one exemption: a supervisor
  lifecycle `Error` for an id with **no live session** carries `seq == 0` (a value
  core never mints), which heads render unconditionally (the seq-`0` bypass)
  rather than dropping under a `seq > last` dedupe — this is what made
  supervisor-shed errors TUI-invisible (absorbs #159).
- **Wire settled before `serve` freezes it** (#160,
  [ADR-0072](../docs/adr/0072-protocol-warts-settled-before-serve.md)):
  `ListSessions`/`SessionList` carry a `correlation_id: String` (not an overloaded
  `SessionId`), so `InMsg::session()`/`OutEvent::session()` are `Option` (`None`
  for these session-less queries) and `OutEvent::seq()` is `Option<u64>` (`None`
  for lifecycle/query events, so the real seq-`0` sentinel stays a distinct
  `Some(0)`). `AgentState::WaitingAnswer` marks a parked `ask_user` question
  distinctly from `WaitingApproval`; every cancel path already acks with
  `Status::Idle`. `msg_to_cmd` returns `Option<SessionCmd>` (log-and-drop) instead
  of an `unreachable!` that would panic the whole supervisor. A wire-allowed
  `ReplayFrom { session, correlation_id, after_seq }` late-subscriber query is
  answered **out-of-core** by a runtime history responder (beside the persistence
  subscriber) that reads the log and broadcasts `OutEvent::History` (content past
  the cursor) via a seq-less `Holly::emit_history`; neither is persisted/replayed.
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
- **Per-agent-profile model pinning + rebind on `SetAgent`** (#323,
  [ADR-0081](../docs/adr/0081-per-profile-model-pinning-and-rebind-on-set-agent.md)):
  `AgentProfile` gains `provider: Option<String>` beside `model` — both set is a
  *model pin* (`AgentProfile::model_pin()`). Core's `SetAgent` (and session start)
  now re-binds the backend to a profile's pin through the same `model_resolver`
  seam as `SetModel` (the `SetModel` success arm is factored into `Session::rebind`),
  so switching agents can switch endpoints — one locus covers Tab cycle / `/agent`
  / `--agent` / spawn / wire, and replay stays consistent. Precedence: per-session
  memory (`Session.profile_models`, a live `/model` choice under a profile) **>**
  the static pin **>** keep current binding (a pin-less profile emits no
  `ModelChanged`; a live override survives an agent switch). `model` without
  `provider` stays the legacy request-level fallback; `provider` without `model`
  is a loud load error. The TUI persists a `/model` pick for the active profile to
  a **managed** `${config_dir}/entanglement/agent-models.yml`
  (`ENTANGLEMENT_AGENT_MODELS_FILE`), overlaid onto the registry at startup
  (persisted file > frontmatter); core stays policy-free (the runtime resolves
  which model wins). `atomic_write` now lives in shared `config::atomic`.
- **Live generation-parameter changes + per-profile persistence** (#374,
  [ADR-0094](../docs/adr/0094-reasoning-effort-and-per-profile-generation-persistence.md)):
  `InMsg::SetGeneration { session, overrides: GenerationParams }` merges a
  **partial** `GenerationParams` onto `Session.generation` via the new
  `GenerationParams::apply_overrides` (a `None` field leaves it untouched) —
  unlike `SetModel` there's no resolver to fail against, so it always succeeds
  and **always** emits `OutEvent::GenerationChanged { session, generation }`
  with the full merged result, recorded into `Session.profile_generation`
  (the generation analogue of `profile_models`). `GenerationParams` also
  gains `reasoning_effort: Option<ReasoningEffort>` (`Low|Medium|High`) — see
  above. **Per-profile persistence mirrors #323/ADR-0081's precedence**
  (session memory > persisted > current binding) but through a **separate**
  seam, `EngineConfig.generation_resolver: Option<GenerationResolver>` keyed
  by profile *name*, not baked into `AgentProfile` like the model pin:
  `GenerationParams`'s `f32` fields have no total `Eq`, so they can't join
  `AgentProfile`'s `PartialEq + Eq` derive. The runtime's
  `AgentGenerationStore` (`${config_dir}/entanglement/agent-generation.yml`,
  `ENTANGLEMENT_AGENT_GENERATION_FILE`, sibling of `agent-models.yml`) has no
  `apply(&mut ProfileRegistry)` — its `resolver(...)` builds the
  `GenerationResolver` closure directly instead.
- **TUI `/set`/`/show` + persist-on-confirmation** (#376,
  [ADR-0095](../docs/adr/0095-tui-set-show-generation-persist-on-confirmation.md)):
  `/set <key> <value>` (`temperature`/`effort`/`thinking_budget`/`max_tokens`,
  the `/compact`-style raw-text re-parse, since `parse_command` drops trailing
  args) sends `InMsg::SetGeneration` and records
  `pending_generation_persist = (agent, overrides)`; `/show` sends a no-override
  `SetGeneration` as a query (no pending recorded) — reusing the merge's
  always-reply behavior rather than adding a read event. The confirming
  `OutEvent::GenerationChanged` is matched by "does it reflect every field the
  pending override set" (not an exact-tuple match, since only `/set`'s named
  fields are known in advance); on a match the TUI commits the write via
  `AgentGenerationStore::set` and renders a transcript status line; an `Error`
  clears the pending without writing; a `GenerationChanged` with no pending
  (a `/show` query, or a `SetAgent`/session-start generation overlay) is
  rendered but never persisted.
- **Dynamic tool registry + live MCP server management** (#372/#375,
  [ADR-0096](../docs/adr/0096-dynamic-toolregistry-sharedregistry.md)/
  [ADR-0097](../docs/adr/0097-live-mcp-server-management.md)):
  `entanglement_runtime::SharedRegistry` (`Arc<std::sync::RwLock<ToolRegistry>>`)
  replaces the owned-by-value `ToolRegistry` the tool executor used to freeze at
  startup — every dispatch snapshots it fresh (`registry.read().unwrap().clone()`,
  cheap: values are `Arc<dyn Tool>`), and `EngineConfig.tool_spec_resolver`
  (ADR-0076) re-snapshots it every turn, so a live add's tools reach both
  execution and the model's advertised schemas with no restart. `InMsg::McpList
  { correlation_id }`/`McpAdd { name, config: McpServerSpec }`/`McpRemove { name
  }` are engine-global exactly like `ListSessions` (`session()` → `None`,
  `msg_to_cmd` → no session task; only the read-only `McpList` is
  wire-allowed — `McpAdd`/`McpRemove` are trusted-only since #472/ADR-0124,
  and a stdio server's child env gets the #164 provider-key scrub), answered by
  `OutEvent::McpList { correlation_id, servers: Vec<McpServerStatus> }`/
  `McpChanged { name, action: McpAction }` — not by the core supervisor (which
  answers `ListSessions` from its own live-session directory) but by a new
  runtime-side `mcp::spawn_mcp_responder` off `Holly::subscribe_inbound()`,
  mirroring `history::spawn_history_responder`'s answer to `ReplayFrom`, since
  the runtime alone holds the `SharedRegistry` + `ActiveServers`/`ServerConfigs`
  these ops read and mutate. `mcp::live::mcp_add`/`mcp_remove`/`mcp_list` do the
  connect/register/unregister/persist work (`mcp_add` upserts — dropping any
  prior connection under the same name first — and never holds the registry's
  write lock across a network/subprocess `.await`; `mcp_remove` relies on
  dropping the last `Arc<McpClient>` to kill the subprocess/close the HTTP
  session via `StdioClient`'s `kill_on_drop`). Persistence
  (`config::save_mcp`) is a **surgical `serde_yaml::Value` edit** of
  `config.yml`'s `mcp:` key — not a new sibling managed file like grants/
  agent-models/agent-generation/the env file — since MCP servers are meant to
  stay part of the primary hand-edited config; locked + atomic, but does not
  preserve comments. A failed add/remove is logged, not a new `OutEvent` (no
  session to attach one to).
- **TUI `/mcp` command** (#373,
  [ADR-0100](../docs/adr/0100-tui-mcp-command.md), Phase 5/final of the MCP
  umbrella): `Command::Mcp` — `/mcp list` (bare `/mcp` and the command-palette
  pick both default to `list`), `/mcp add <name> -- <command> [args...]`
  (stdio), `/mcp add <name> --url <url> [--header KEY:VALUE]...` (streamable
  HTTP), `/mcp remove <name>` — the same raw-text re-parse pattern as
  `/compact`/`/set`. Parsing (`parse_mcp_args`) and the async wire senders
  live in a new sibling `tui/mcp_command.rs` (`commands.rs`/`event_loop.rs`
  were already past the 400-line cap). `/mcp list` sends `InMsg::McpList`
  with a fresh correlation id recorded on `tui::mcp_panel::McpPanel`; only
  the matching `OutEvent::McpList` opens the read-only popup panel
  (`modals::draw_mcp_panel`, `Esc` closes) listing each server's transport,
  connected/error status, and namespaced tools — a stray reply is dropped,
  never popping the panel with the wrong snapshot. `add`/`remove`
  confirmations (`OutEvent::McpChanged`) and parse errors render as a
  transcript status line on the active session, mirroring `/key`'s save
  notice. No new wire surface.
- **Single-shot session ops + persisted compaction** (#324,
  [ADR-0082](../docs/adr/0082-single-shot-session-ops-and-persisted-compaction.md)):
  `InMsg::Oneshot { session, op: String, args: Value }` is a generic **wire
  envelope** for a single out-of-band LLM call outside the turn loop — not a
  plugin registry, the genericity is in the wire shape, `session::ops::run_oneshot`
  is a plain `match op.as_str()`. `"compact"` (session compaction via LLM
  summarization) is the first op: routed like `SetAgent`/`SetModel`
  (`SessionCmd::Oneshot`, deferred via the same stash gate while a turn is live),
  it renders the transcript, asks the model to summarize it with a tool-less
  request, and emits `OutEvent::Compacted { session, seq, summary, kept, auto }`
  — a **persisted, seq-bearing** content event (persistence and `ReplayFrom`
  cover it for free; both are variant-agnostic over any seq-bearing event).
  **Copy-on-write (ADR-0101, supersedes ADR-0082); forks a *successor* that
  retires the source ([ADR-0110](../docs/adr/0110-compaction-successor-closes-predecessor.md),
  amends ADR-0101):** the source session's `Context` is **never mutated** — the
  summary rides only in the event, and the head forks it into a **new root**
  session via `InMsg::Spawn` (now `parent: Option<SessionId>` — `None` here — plus
  a new `predecessor: Some(source)` for lineage, mirrored onto `SessionStarted`),
  then **closes the source** (`InMsg::CloseSession`) so its interactive session is
  retired while its log is preserved. The successor is a root, not a child of the
  source, so closing the source doesn't cascade onto it. A truncated summary
  (`StopReason::MaxTokens`) is refused outright, and an oversized transcript is
  rejected before the request. `Session::replay`'s `Compacted` fold is still a
  no-op for `auto: false`, but the source being closed means its implicit undo is
  no longer reachable interactively (only via the persisted log); `"compact"` only
  runs on request (`InMsg::Oneshot`, TUI `/compact [--keep N] [instructions]`).
  Auto-compaction (`auto: true`, ADR-0103) is unchanged — in-place, no fork, no
  close. Lineage is now two-way on `Session` (`parent` + a new `children:
  Vec<SessionId>` mirror populated via internal `SessionCmd::ChildSpawned`/
  `ChildClosed` and rebuilt on replay by inverting parent edges, + `predecessor`);
  new session ids are always fresh v4 UUIDs, incl. the TUI new-session path (the
  former `{root}-{ordinal}` suffix removed).
  **Keep-tail (#397,
  [ADR-0102](../docs/adr/0102-compact-keep-tail-verbatim-in-the-fork-prompt.md)):**
  `args.kept: u64` (default `0`) requests the last `kept` messages ride into
  the fork **verbatim** instead of being paraphrased. `Context::safe_kept`
  clamps the request to the nearest safe turn boundary — the tail must start
  at a `User` message, or a `Tool` reply could replay without its paired
  `Assistant` tool-call half, breaking providers' `tool_use`/`tool_result`
  pairing (ADR-0082's deferred-to-v1 blocker). `compact_op` summarizes only
  the *head*, then composes the tail's rendered transcript after the summary
  (`summarize::compose_report`) — the composed text ships inside the same
  `summary` field, so this needed **no wire change**; `kept` now reports the
  real (clamped) count instead of a hardcoded `0`.
  **Auto-summarize on context overflow (#398,
  [ADR-0103](../docs/adr/0103-auto-summarize-on-context-overflow.md)):** the
  turn loop's overflow guard (`session/turn.rs`, #178) no longer falls straight
  to the lossy prune-only `Context::compact` — gated by
  `EngineConfig::auto_compact` (default `true`), it first tries the same
  `session/summarize.rs::summarize` core `compact_op` uses (requesting a small
  fixed keep-tail, clamped by `safe_kept` exactly as #397 does) and, on
  success, mutates the session's `Context` **in place** via
  `Context::apply_compaction` — the fundamental split from `/compact`'s
  copy-on-write: a turn mid-flight has no head to fork into, so the only sound
  recovery is compacting the live context and continuing the same turn. Emits
  the same wire variant marked `Compacted { auto: true, .. }`; `Session::replay`
  folds it by replaying the identical `apply_compaction` call (flushing
  whatever pending assistant/tool state has accumulated first), unlike the
  manual path's no-op fold. Falls through to `Context::compact` (then refusal)
  when auto-summarize is disabled, its own guard trips (oversized transcript/
  tail, an LLM error, a truncated summary), or the result still doesn't fit —
  byte-identical to the pre-#398 behavior in that case. Heads must not fork on
  `auto: true` (the TUI renders an in-place notice on the same view instead of
  `handle_compacted`'s new-session fork; the stdio `run` head's one-line render
  likewise branches on `auto`). **The prune fallback stays silent, by design**
  (#450, [ADR-0121](../docs/adr/0121-prune-only-compact-stays-silent.md)):
  `Context::compact` mutates `Session.ctx` in place exactly like
  `apply_compaction` but emits no `OutEvent`, so `Session::replay` never
  replays it and a resumed session can briefly see more history than the live
  session had — accepted, since `enforce_context_window` re-derives the
  identical prune from the raw log on the very next round either way (replay
  included), unlike the summary branch's LLM-authored rewrite that can't be
  recomputed and so must be recorded.
- **In-app tool-allowlist editing materializes a user-layer override** (#330,
  [ADR-0083](../docs/adr/0083-in-app-tool-allowlist-editing-as-user-layer-materialization.md)):
  no separate mask store — editing a profile's `tools:`/`disallowed_tools:`
  writes `${config_dir}/entanglement/agents/<name>.md` (native user layer,
  `ENTANGLEMENT_AGENTS_DIR` override), the same shadow a hand-authored file would
  be. `agents::materialize::save_tools_override(root, name, allowed)` seeds from
  the *currently effective* definition's raw text (`winning_raw_text`, same
  precedence as `load_registry` — a built-in's embedded source or an existing
  override's exact text), rewrites only the `tools:`/`disallowed_tools:`
  frontmatter keys via a `serde_yaml::Mapping` round-trip
  (`rewrite_tools` — order-preserving, everything else untouched), and writes
  atomically via `config::atomic::atomic_write`. TUI: `e` on the `/agent`
  picker's highlighted profile opens a checklist dialog
  (`tui::tools_dialog::ToolsDialog`) over the full advertised tool roster
  (`EngineConfig.tool_specs`, captured before `Holly::spawn` consumes it) seeded
  from the profile's current mask; `Space` toggles, `Enter` saves, `Esc`
  discards. Applies on next restart — no live registry reload yet.
- **Session hibernation is eviction, not termination** (#318,
  [ADR-0077](../docs/adr/0077-session-hibernation-evictable-resumable.md)): a third
  lifecycle state between `live` and the terminal tombstone. `HibernateSession {
  session }` (**trusted-only**, not wire-allowed — joins the
  `ToolResult`/`Spawn`/`Resume` refused set; `Holly::hibernate` is the wrapper)
  tears down the session task + its spawn sub-tree (cascade like `CloseSession`)
  and drops each `Context`, but records **no** tombstone — so the id stays
  resumable: `Holly::resume` rebuilds it from the embedder's event log exactly like
  the restart path, re-offering a turn parked mid-approval
  ([ADR-0061](../docs/adr/0061-parked-turn-state-batch-tool-resolution.md)/[ADR-0071](../docs/adr/0071-parked-turn-reoffer-timer.md)).
  The task emits a distinct lifecycle `SessionHibernated { session, ts }` (no
  `seq`); the runtime executor releases its per-session bookkeeping on it as on
  `SessionEnded`. Mid-stream hibernate = **stop-then-hibernate** (the supervisor's
  sender-drop cancels the round; the uncommitted text-only tail is discarded
  exactly as `Session::replay` drops it, so resume is lossless vs the log);
  closed ids stay terminal (`resume` still refused). Core snapshots nothing —
  eviction + log replay reuse one seam (no DB in core). **Auto-hibernation on an
  optional idle TTL is now built in** (#363,
  [ADR-0090](../docs/adr/0090-idle-ttl-auto-hibernation.md)):
  `EngineConfig.idle_ttl: Option<Duration>` (`None` by default — byte-identical
  to before, eviction stays embedder-driven when unset) arms a supervisor-level
  sweep (`tokio::select!` branch, only present when configured) polling at
  `max(idle_ttl / 4, 30s)`. Settledness is `Session::turn.is_none()` alone — no
  runtime `AgentState` needed, since both the approval-wait and `ask_user`-wait
  are just pending `TurnState` entries — published by each session task to a
  shared `ActivityRegistry` (mirrors `SeqRegistry`'s sharing pattern,
  `tokio::time::Instant` so paused-clock tests stay deterministic). A sweep tick
  judges every **root** by its whole spawn sub-tree (`collect_subtree`): every
  member must be settled, and the idle clock starts at the *latest* member's
  settle time, so one parked child pins the whole ancestry live. A qualifying
  root hibernates through the same `hibernate_subtree` helper
  `HibernateSession` uses — stricter than a manual hibernate (which
  stop-then-hibernates on request): a background timer only ever evicts a
  session already at rest, never one mid-stream. **Exposed as a runtime config
  setting** (#401, [ADR-0105](../docs/adr/0105-expose-idle-ttl-via-runtime-config.md)):
  `config.yml`'s `idle_ttl_secs: Option<u64>` (whole seconds — no
  duration-string crate in the tree) is copied onto `EngineConfig.idle_ttl` in
  `build_config`, the same wiring point as `max_turns`. One engine-global
  setting shared by every head (`run`/`pipe`/`tui`/`serve`, since `Holly::spawn`
  runs once before the subcommand match) rather than a `serve`-only CLI flag —
  the sweep's own settledness guard is what makes auto-hibernation safe for any
  head. Unset (the default) stays `None`, byte-identical to before.
- **`Resume` cascades over the spawn sub-tree** (#415,
  [ADR-0112](../docs/adr/0112-resume-cascades-over-the-spawn-subtree.md)):
  mirrors `CloseSession`/`HibernateSession`'s teardown cascade in reverse — a
  root's log already carries every spawned child's interleaved records, so
  resuming the root also recursively `Session::replay`s + re-spawns every
  child still "live" in the log (a `SessionStarted` with no matching
  `SessionEnded`/`SessionHibernated`), re-registering `parent_links` as it
  goes, instead of leaving them to a lazy blank respawn on first touch.
  `Session::replay` gained an explicit `target: &SessionId` param (was always
  "the log's own root") so the same fold reconstructs any session in a shared
  root log, root or descendant. Also fixed: the resumed session's
  re-announced `SessionStarted` now carries the replay-resolved `predecessor`
  instead of the always-`None` resume parameter (a persisted log could
  otherwise regress its own lineage on a second resume).
- **Persistence synthesizes a spawned child's initiating prompt** (#421,
  [ADR-0113](../docs/adr/0113-persistence-synthesizes-a-spawned-childs-initiating-prompt.md)):
  `InMsg::Spawn` delivers its `prompt` straight to the child's session-command
  channel, bypassing the inbound broadcast the persistence tap observes, so no
  `InMsg::Prompt` record ever existed for it — replay/resume reconstructed the
  assistant's eventual reply but not the user-role instruction that produced
  it. The tap now caches a `Spawn`'s `prompt` (`pending_spawn_prompts`) instead
  of dropping it, and once the child's `SessionStarted` resolves `roots` (the
  point `InMsg::Spawn` itself still can't be persisted verbatim without
  becoming a stray bogus-root file), synthesizes `InMsg::prompt(child, prompt)`
  right after it so `Session::replay` folds it as the child's opening user
  message. Consumed on first use, so a resumed child's re-announced
  `SessionStarted` never re-synthesizes or duplicates the record.
- **Skill-scoped `allowed_tools` enforcement** (#400,
  [ADR-0106](../docs/adr/0106-skill-scoped-allowed-tools-enforcement.md)): a
  `SKILL.md`'s `allowed_tools` frontmatter (parsed since #114 but unenforced)
  now gates tool calls while that skill is active. A resolved `load_skill`
  result's `skill_id:` header is the provenance signal — `tool_runner` looks
  the skill up in the live `SkillRegistry` and records
  `ActiveSkill { skill_id, allowed_tools }` per **session**
  (`runtime::permission::skill_masked`), never a core-protocol field on
  `ToolCall`/`ToolExec`. Checked in `ToolExec` handling strictly *after* the
  #116 agent mask (`tool_masked`) — a tool must survive both, with no
  exemption for `load_skill` itself (a skill whose `allowed_tools` omits it
  blocks switching skills mid-turn). Unlike the agent mask it does **not**
  clamp the ancestor/spawn chain — a skill's scope is the loading session's
  current turn, not an inheritable profile trait — and it clears on that
  session's next `Done` (or the session ending), not an explicit unload tool.
  `OutEvent::SkillActive { session, seq, skill_id: Option<String>,
  allowed_tools: Option<Vec<String>> }` mirrors `FileChange`'s shape as the
  wire-facing posture; the stdio `run --format text` head and the TUI
  transcript both render it as a one-line notice.
- **Definitions are data, layered** embedded < user < project, later wins; the
  project layer is **trusted** ([ADR-0047](../docs/adr/0047-local-trust-boundary.md)).
  Agents (`ENTANGLEMENT_AGENTS_DIR`), skills (`ENTANGLEMENT_SKILLS_DIR`), the
  provider catalog (`ENTANGLEMENT_PROVIDERS_FILE`), and the **user config file**
  (#172, `${config_dir}/entanglement/config.yml` < `.entanglement/config.yml`) all
  share this loader. Agents/skills also discover **cross-vendor dirs**
  ([ADR-0074](../docs/adr/0074-cross-vendor-skill-and-agent-discovery.md)):
  user `~/.claude/<kind>` and project `.claude/<kind>` < `.agents/<kind>` scan
  *before* the native dir of the same layer (native wins on `name`), parsed
  leniently (`name`+`description` only; malformed foreign files warn-and-skip,
  never abort; foreign agents default `mode: all`; skill
  `disable-model-invocation` → `user_only`). The `ENTANGLEMENT_*_DIR` override
  replaces the whole user layer — it is the cross-vendor opt-out. Provider API **keys** live in a sibling managed env file (#220,
  `${config_dir}/entanglement/.env`, override `ENTANGLEMENT_ENV_FILE`): scaffolded
  commented on first run, loaded at startup into the process env for vars the real
  env left unset (env > file), kept out of any repo. A **shared writer**
  (#304, [ADR-0073](../docs/adr/0073-managed-env-file-writer-and-key-surfaces.md),
  `config::env_key`) backs two key surfaces: a pure `upsert` (replace the first
  live `KEY=` line — first-occurrence-wins, matching `load()` — else the `#KEY=`
  placeholder, else append; other lines byte-for-byte; idempotent) + `set_key`
  (atomic temp-file-in-dir + rename, `0o600` on unix, reject empty/`\n`). `skutter
  config set-key <provider> [--key V]` (`config::keys`, pre-engine fast path, key
  from `--key`/hidden `rpassword` prompt/piped stdin, never echoed) and the TUI
  `/key` dialog (`tui::key_dialog`, two-stage modal after `/model`, masked input)
  both drive it — the TUI additionally `set_var`s so the live model resolver
  binds the key on the next `/model` switch with no restart. The config's `hooks:` section
  (#199, [ADR-0066](../docs/adr/0066-lifecycle-hooks-as-runtime-interceptors.md))
  wires **lifecycle hooks** — `sh -c` commands run as a **runtime interceptor**
  around the generic tool dispatch (`pre_tool_use` non-zero exit *vetoes* the
  call; `post_tool_use` is an observational side-effect) and off the inbound
  `Prompt` fan-out (`user_prompt_submit`), each in its own process group. Scoped
  to the generic `Intercept::Permission` route (orchestration + `rhai` bypass);
  wired via `tool_runner::spawn_tool_executor_with_hooks`. The config's `mcp:`
  section (#198, [ADR-0067](../docs/adr/0067-mcp-client-as-runtime-tool-provider.md);
  #312, [ADR-0080](../docs/adr/0080-mcp-streamable-http-transport.md)) declares
  **external MCP tool servers**, each per-server block choosing one transport —
  **`command` XOR `url`**: `{command, args, env}` (stdio subprocess, #198) or
  `{url, headers}` (streamable HTTP, #312, behind the `mcp-http` feature; static
  headers `${VAR}`-expanded, `Mcp-Session-Id` round-trip), plus a shared
  `disabled`, plus an optional `capabilities: {tool: read|write|call}` hint (#426,
  see the capability-fan-out bullet above). `McpClient` is an enum over both
  transports and `McpTool` adapts whichever backs it; its `tools/list` is
  registered into the `ToolRegistry` as `mcp__<server>__<tool>` — a runtime-side
  tool provider, no core change, governed by the same permission profiles as
  any host tool; a server that fails to connect is logged and skipped.
  `HttpClient` is public so a multi-tenant embedder can assemble per-user
  registries with per-user tokens without the YAML path.
- **Live reload + managed-file locking** (#329, [ADR-0084](../docs/adr/0084-runtime-live-reload-and-managed-file-locking.md)):
  a runtime `watch.rs` (inotify via `notify`/`notify-debouncer-mini`, 500ms debounce)
  watches the agent/skill dirs above plus `${config_dir}/entanglement/` and
  `<root>/.entanglement/`, reloading into **runtime-held mirrors**
  (`watch::LiveDefinitions`) that `tool_runner` permission resolution, `load_skill`,
  and the TUI `/agent` picker read live — never core's `EngineConfig.profiles`,
  which stays pinned per session for the process lifetime (same "live registry
  mutation rejected" reasoning as [ADR-0081](../docs/adr/0081-per-profile-model-pinning-and-rebind-on-set-agent.md)).
  The three managed files above (`grants.yml`/`agent-models.yml`/the env file) are
  now advisory-locked across concurrent `skutter` instances
  (`config::lock::with_locked_file`, an `fd-lock` on a sibling `.lock` file,
  read-current-then-merge under the lock) so two instances no longer clobber each
  other's write. **Reload is content-gated:** the debounced firing reloads (and
  emits the "definitions reloaded" notice) **only if a definition/config file's
  content actually changed** — the fingerprint is a `path → (mtime, size, sha256)`
  map restricted to agent/skill `*.md` + managed `*.yml`/`*.yaml`/`.env`, checked
  two-stage (cheap mtime gate → SHA-256 arbiter). So a same-content re-save and,
  crucially, a write to a **non-definition** file under a watched tree (a
  `call`/`bash` output artifact under `.entanglement/tmp/`) are no-ops instead of
  spamming reload notices.
- **Access outside the project root, approval-gated** ([ADR-0109](../docs/adr/0109-escape-root-access-via-approval.md)):
  root containment (ADR-0054) is no longer absolute. A `read`/`edit`/`write` path
  or a `bash`/`call` `workdir` resolving **outside** root is detected in the
  executor (`permission::escape_root_target` + `host::escaping_path`), forces an
  approval prompt even when the profile would `Allow` (a `Deny` floor still wins),
  and — on approval — is recorded in a **separate** `runtime::extra_roots::ExtraRootStore`
  (managed `extra-roots.yml`, override `ENTANGLEMENT_EXTRA_ROOTS_FILE`) keyed by
  `(tool, resolved-absolute-path)`, **per tool** (a `read` grant never unlocks
  `write`), at `Once`/`Session`/`Always` scope. The host tools consult it via
  `resolve_under_root_or_grant` to relax containment for the approved path
  (matched against the symlink-canonicalized target). Reuses the
  `ToolRequest`/`Approve{scope}` wire (no new variant); `glob`/`grep` stay
  strictly root-contained. **`call` default output** also moved out of the repo:
  a no-`output_file` artifact now lands in a runtime-owned per-project scratch dir
  (`session_store::scratch_dir` → `<data_dir>/entanglement/sessions/<cwd>/tmp/`,
  via `CallTool::with_scratch_base`), not `<root>/.entanglement/tmp/`.
  **`rhai` file/exec bindings route through the identical gate** (#446,
  [ADR-0119](../docs/adr/0119-rhai-bindings-route-through-the-escape-root-gate.md)):
  the `Intercept::Rhai` route never called `dispatch`, so a script's
  `read`/`edit`/`write`/`exec`/`bash` binding used to hard-fail on a first-time
  out-of-root access with no chance to prompt. `EscapeRoot` (now `pub(crate)`
  on its `escaping` helper) threads one hop further into `run_rhai` →
  `execute_script` → `service_binding`, which forces the same approval +
  warning for an escaping call (bypassing the coarse per-run `Ask` cache) and
  records the grant into the same `ExtraRootStore` on approval — a durably
  granted path still resolves silently, matching a direct call exactly.
  **A `Once` grant is bound to its approving `request_id`** (#449,
  [ADR-0120](../docs/adr/0120-once-scoped-escape-root-grant-bound-to-request-id.md)):
  per-call executor tasks are detached and run concurrently, so keying `once`
  by `(tool, path)` alone let a different in-flight call to the same escaping
  path consume a single-use token it was never approved for.
  `ExtraRootStore::record`/`take_allowance` now key `Once` on
  `(tool, path, request_id)` — `Session`/`Always` are unchanged, still
  `(tool, path)` only. `Tool::run_for_session` gains a `request_id: &str`
  parameter (`ToolRegistry::execute` forwards `call.id`, which it already had);
  default delegates to `run_content` unchanged, so this is source-compatible —
  only the six escape-root-capable host tools override it. `script.rs`
  threads its per-binding `bind_rid` into both `record` and the delegated
  `exec()` call so a script-obtained `Once` grant is redeemed by that exact
  binding invocation too.
- **An ambiguous LLM stop retries in place instead of ending the turn**
  (post-0.3.0, [ADR-0118](../docs/adr/0118-ambiguous-stop-reason-bounded-retry.md)):
  `session::turn::is_confident_stop` classifies a round that ends with empty
  `tool_calls` by its `stop_reason` — `EndTurn`/`MaxTokens`/`StopSequence` end
  the turn as before, but `None`/`Other`/a contradictory `ToolUse` with no
  actual calls (the Ollama-class "announced intent then stream died"
  symptom) commit the partial round, inject a synthetic user-role nudge, and
  retry the same round in place, bounded by
  `EngineConfig::max_ambiguous_stop_retries` (default 2, separate from
  `max_turns`) and reset to 0 by any round that does produce real tool calls.
  Emits a persisted, seq-bearing `OutEvent::AmbiguousRetry { session, seq,
  nudge }` (modeled on `Compacted`) *before* the mutation so `Session::replay`
  reconstructs the exact `assistant(partial)/user(nudge)/assistant(recovered)`
  shape instead of merging retry rounds' `TextDelta`s into one message and
  dropping the nudge; an empty ambiguous round commits nothing (the strict
  Anthropic/Gemini clients reject empty-content messages), and a shared
  `coalesce_same_role` post-pass merges the nudge with an adjacent same-role
  turn. Companion fixes: the OpenAI-compat client now tracks
  `emitted_any_tool_call` instead of gating on the pre-JSON-filter
  accumulator, and the stub `Llm` backends (`DummyLlm`/`EchoLlm`) report an
  honest `stop_reason` instead of bare `None`, since `None` is now
  load-bearing as "ambiguous, retry."

| Topic | Module |
| --- | --- |
| `InMsg`/`OutEvent`, Plan/TaskList events | [protocol](../docs/architecture/protocol.md) |
| profiles, tool mask, spawn gating, plan authority, skills, prompt assembly | [agents & permissions](../docs/architecture/agents-and-permissions.md) |
| turn loop, tool round-trip, steering, cancellation | [engine](../docs/architecture/engine.md) |
| streaming client, catalog, pool/retry/rate-limit | [provider](../docs/architecture/provider.md) |
| stdio/TUI/`serve` heads, event-sourced persistence | [heads & persistence](../docs/architecture/heads-and-persistence.md) |
| dependency gates, the sextet (incl. `apply_patch`) + exec tools (`bash`/`call`/`bash_output`/`rhai`), lifecycle hooks, MCP client (external tool servers) | [gates & host tools](../docs/architecture/gates-and-host-tools.md) |

Debugging: `skutter inspect prompt|agents|skills|config` re-runs the load-time
discovery with **no engine** and prints the resolved prompt / registries / user
config, including the layer that won an override (✅ #184/#185/#186, #172). The TUI exposes the same three
views in-session via `/inspect` (or `<leader>i`) as a read-only overlay over the
active session's resolved state (✅ #214); the Agents and Skills tabs are
**two-level** (✅ #331): a selectable list where `Enter` drills into the per-item
detail pane rendered by the same per-name code path the CLI uses, `Esc`/`Backspace`
returns to the list. Trust & scope decisions:
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
- **Track intentional deferrals and docs drift in the ledger.** When a design
  explicitly defers a piece of work to a follow-up (an ADR or code comment
  saying "deferred"/"future"), file it as a row in
  [`../docs/deferred-work-ledger.md`](../docs/deferred-work-ledger.md) (backed
  by issue #396) so it doesn't fall out of tracking once its originating issue
  closes. Same ledger for docs/implementation drift findings (a doc claiming a
  shipped feature is "not yet built").

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
#218/[ADR-0063](../docs/adr/0063-realtime-model-provider-switch.md)),
and the extensibility epic (#196 — `Message`/`Prompt` migrated to multimodal
content blocks (`text: String` → `content: Vec<ContentPart>`, serde back-compat
shim for pre-migration logs) #197/[ADR-0064](../docs/adr/0064-message-content-blocks.md),
`read` emits image files (`png`/`jpg`/`jpeg`/`gif`/`webp`) as base64 image
content blocks through the now-multimodal `ToolResult`/`ToolOutput` path
#221/[ADR-0065](../docs/adr/0065-read-emits-image-content-blocks.md), lifecycle
hooks (`pre_tool_use`/`post_tool_use`/`user_prompt_submit`) as a runtime
interceptor around tool dispatch + prompt ingress, configured in the layered
user config's `hooks:` section
#199/[ADR-0066](../docs/adr/0066-lifecycle-hooks-as-runtime-interceptors.md), and
an MCP client attaching external tool servers (config `mcp:` section, JSON-RPC/
stdio, registered as `mcp__<server>__<tool>`) as a runtime-side tool provider
with no core change
#198/[ADR-0067](../docs/adr/0067-mcp-client-as-runtime-tool-provider.md))
are **complete**.
The July 2026 audit backlog — thematic epics tracked on GitHub with P0/P1/P2
labels and blocked-by links — is now **fully closed** (no open issues), and its
work ships in the **0.2.0** release (the project's first tagged release; see
[`../CHANGELOG.md`](../CHANGELOG.md)). The follow-on **0.3.0** release adds the
capability-level tool-permission epic (#416: capability keys `read`/`write`/`call`
with parse-time fan-out incl. MCP tools, `rhai` exec bindings, workdir-scoped
rules — [ADR-0114](../docs/adr/0114-capability-level-permission-keys.md)–[ADR-0117](../docs/adr/0117-mcp-tool-capability-fan-out.md)),
per-endpoint concurrency cap + adaptive pacing + bounded 429 backpressure
(#413/#414, [ADR-0111](../docs/adr/0111-adaptive-endpoint-pacing-and-429-retry-until-clear.md)),
and session-lineage robustness fixes ([ADR-0112](../docs/adr/0112-resume-cascades-over-the-spawn-subtree.md)/[ADR-0113](../docs/adr/0113-persistence-synthesizes-a-spawned-childs-initiating-prompt.md)).
The **0.4.0** release (see [`../CHANGELOG.md`](../CHANGELOG.md)) adds the
`apply_patch` host tool (#455: unified-diff apply beside `edit`/`write`, the
first producer of the reserved `FileChangeKind::ApplyDiff`), the bounded
ambiguous-stop retry
([ADR-0118](../docs/adr/0118-ambiguous-stop-reason-bounded-retry.md),
`OutEvent::AmbiguousRetry`), `agent_poll timeout_secs: 0` as
wait-for-completion ([ADR-0123](../docs/adr/0123-agent-poll-zero-timeout-waits-for-notification.md)),
provider stream/throttle fixes, and the 2026-07-21 security-audit hardening
(#472/[ADR-0124](../docs/adr/0124-wire-refused-mcp-mutation-and-stdio-key-scrub.md):
MCP stdio provider-key scrub, `McpAdd`/`McpRemove` wire-refused, fail-closed
wire allowlist).
The 0.2.0 backlog covered
#209 (docs), the parked-turn-state epic #276 (turns park as explicit serde
`TurnState`, batch-parallel tool resolution, mid-turn replay/resume,
[ADR-0061](../docs/adr/0061-parked-turn-state-batch-tool-resolution.md); the
in-process re-offer timer + executor `request_id` dedupe that self-heals a turn
stranded by a `broadcast`-lag drop landed here, #274/[ADR-0071](../docs/adr/0071-parked-turn-reoffer-timer.md)).
The pre-`serve` hardening epic #153 is **complete** — all six findings (#274,
#155, #156, #157, #158, #160) landed, and the local WebSocket `serve` head they
gated shipped last, per [ADR-0048](../docs/adr/0048-serve-head-local-trust-model.md).
The generic one-shot op framework (#324, `InMsg::Oneshot`, session compaction
as its first op, [ADR-0082](../docs/adr/0082-single-shot-session-ops-and-persisted-compaction.md)),
copy-on-write forking ([ADR-0101](../docs/adr/0101-compaction-forks-into-a-new-session-copy-on-write.md)),
keep-tail (#397, [ADR-0102](../docs/adr/0102-compact-keep-tail-verbatim-in-the-fork-prompt.md)),
auto-summarize on context overflow (#398,
[ADR-0103](../docs/adr/0103-auto-summarize-on-context-overflow.md)), and the
optional bubblewrap OS sandbox for `bash`/`call` (#399,
[ADR-0104](../docs/adr/0104-bubblewrap-sandbox-for-bash-call.md)), exposing
`idle_ttl` as a `config.yml` setting for `serve` (#401,
[ADR-0105](../docs/adr/0105-expose-idle-ttl-via-runtime-config.md)), and
skill-scoped `allowed_tools` enforcement (#400,
[ADR-0106](../docs/adr/0106-skill-scoped-allowed-tools-enforcement.md)) are
**complete**.

Shipped foundations: streaming `Llm` providers ([ADR-0007](../docs/adr/0007-streaming-llm-and-provider-crate.md))
— z.ai (primary)/OpenAI/Ollama via one OpenAI-compat client + a separate
Anthropic client; `ENTANGLEMENT_PROVIDER` or key auto-detect, else `EchoLlm`.
Heads: stdio `run`/`pipe`, `tui`, the `sessions`/`inspect` subcommands, and the
local WebSocket `serve` head (`skutter serve --port <N>`, loopback-bound axum
HTTP+WS, ✅ #153). Tools:
the root-contained sextet (`read` on an image file — `png`/`jpg`/`jpeg`/`gif`/
`webp` — emits a base64 **image content block** through a now-multimodal
`ToolResult`/`ToolOutput` path, #221/[ADR-0065](../docs/adr/0065-read-emits-image-content-blocks.md),
built on the `Message`/`Prompt` content-block migration #197/[ADR-0064](../docs/adr/0064-message-content-blocks.md);
`apply_patch` joins `edit`/`write` as a multi-hunk unified-diff apply, the
first producer of the previously-reserved `FileChangeKind::ApplyDiff`, #455 —
a small hand-rolled parser/applier in `host::unified_diff`, not the `diffy`
crate, since `diffy` is `tui`-feature-gated and forbidden from the lean
`--no-default-features` build `apply_patch` lives in),
the always-registered `call` (argv exec, no shell — registered independent of
`ENTANGLEMENT_ENABLE_BASH` since #386/[ADR-0093](../docs/adr/0093-call-registration-independent-of-bash-opt-in.md);
gains a `workdir` param, mirroring `bash`'s) and the opt-in exec pair
`bash`/`bash_output` (`ENTANGLEMENT_ENABLE_BASH=1`; `bash` gains `workdir` +
`run_in_background`, polled via `bash_output`, #170) — both run unsandboxed by
default but may be confined via **bubblewrap**
(`ENTANGLEMENT_SANDBOX=bwrap`, `ENTANGLEMENT_SANDBOX_NETWORK=1` to keep network
— fail-closed, #399/[ADR-0104](../docs/adr/0104-bubblewrap-sandbox-for-bash-call.md)),
and the sandboxed `rhai`
tool. **External MCP tool
servers** attach as a runtime-side tool provider (#198,
[ADR-0067](../docs/adr/0067-mcp-client-as-runtime-tool-provider.md); #312,
[ADR-0080](../docs/adr/0080-mcp-streamable-http-transport.md)): the user config's
`mcp:` section declares servers over **stdio** (`command`) **or streamable HTTP**
(`url` + auth `headers`, behind the `mcp-http` feature), its `tools/list`
registered into the `ToolRegistry` as `mcp__<server>__<tool>` — no core change,
same permission profiles as any host tool. `skutter serve`
(axum WS, local-only, loopback-bound, opt-in `--allow-origin`,
[ADR-0048](../docs/adr/0048-serve-head-local-trust-model.md)) is the fourth head,
a thin adapter over `holly` that relays `OutEvent`s out and routes inbound frames
through the untrusted `send_from_wire` path (✅ #153).
