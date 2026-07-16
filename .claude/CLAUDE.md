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
make run           # stdio head, one turn (text)
make run-json      # one turn, NDJSON events (opencode run --format json)
make run-tui       # launch the terminal UI
make serve         # local WebSocket head on 127.0.0.1 (ARGS='--port 4517')
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
format); **Gemini** has a native `GeminiLlm` (✅ #309,
[ADR-0085](../docs/adr/0085-gemini-native-wire-and-opaque-provider-meta.md)) — not
the OpenAI-compat surface, which drops the `thoughtSignature` a 2.5 thinking model
must round-trip; that opaque per-call token rides the new generic
`ToolCall.provider_meta: Option<Value>` slot (persisted with the ADR-0064 shim,
never inspected by core). No key → `EchoLlm`. Detail in
[`../docs/architecture.md`](../docs/architecture/provider.md). **Per-endpoint**
connection pool, retry/backoff, rate-limit (429/`Retry-After`/RPM keyed by base
URL + API-key hash, ✅ #217, [ADR-0050](../docs/adr/0050-per-endpoint-connection-pool-retry-rate-limit.md)),
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
          | SetAgent | SetModel | Oneshot | Spawn | ListSessions | ReplayFrom | CloseSession
          | HibernateSession (trusted-only) | Resume (internal, not serialized)
OutEvent : SessionStarted | SessionEnded | SessionHibernated | SessionList | History | Status | AgentChanged | ModelChanged
          | Plan | TextDelta | ReasoningDelta | ToolCallDelta | ToolCall | ToolRequest | ToolExec
          | UserQuestion | ToolOutput | TaskList | Usage | Error | Done | Compacted | FileChange
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
  `read`), matched against the call input the runtime extracts
  (`permission::permission_arg`) — the `PermissionProfile::resolve(name, arg)`
  glob is the only core surface. A user config file (#172) adds a global
  permission **ceiling** clamped least-privilege over every grade
  (`clamp_to_base`); see `entanglement-runtime/src/config`. `Approve` carries a
  `scope` (#174, [ADR-0052](../docs/adr/0052-approval-scope-and-persisted-grants.md)):
  `Session`/`Always` record an exact `(tool, arg)` grant in `runtime::grants`
  that upgrades a later resolved `Ask` → `Allow` (never a `Deny`, applied *after*
  the ceiling); `Always` persists to a managed `${config_dir}/entanglement/grants.yml`
  (sibling of `config.yml`, not its ceiling section). Both policy sources are
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
- **Trusted/untrusted frame split** (#155,
  [ADR-0069](../docs/adr/0069-trusted-untrusted-wire-frame-split.md)): `Holly::send`
  is the **privileged in-process** inbox (executor/head, trusted for any frame);
  a wire head deserializing untrusted bytes uses `Holly::send_from_wire`, which
  enforces the `InMsg::wire_allowed()` allowlist and refuses (`WireError`) the
  runtime-authored trio — `ToolResult` (a forged one resolves a parked turn on
  `request_id` alone, bypassing execution + permission), `Spawn` (bypasses
  `spawn_refusal`), `Resume` (internal). The executor folds results back over the
  named privileged `Holly::submit_tool_result` handle (via `seam::reply_content`);
  `pipe` calls `send_from_wire`. Local single-user scope
  ([ADR-0048](../docs/adr/0048-serve-head-local-trust-model.md)) → robustness/UX,
  not remote-attacker defence; WS `send_from_wire` + per-connection `Approve`
  ownership deferred to #153.
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
- **Single-shot session ops + persisted compaction** (#324,
  [ADR-0082](../docs/adr/0082-single-shot-session-ops-and-persisted-compaction.md)):
  `InMsg::Oneshot { session, op: String, args: Value }` is a generic **wire
  envelope** for a single out-of-band LLM call outside the turn loop — not a
  plugin registry, the genericity is in the wire shape, `session::ops::run_oneshot`
  is a plain `match op.as_str()`. `"compact"` (session compaction via LLM
  summarization) is the first op: routed like `SetAgent`/`SetModel`
  (`SessionCmd::Oneshot`, deferred via the same stash gate while a turn is live),
  it renders the transcript, asks the model to summarize it with a tool-less
  request, and replaces the whole history via the new
  `Context::apply_compaction(summary, kept)` — one **user-role** summary message
  plus `kept` preserved trailing messages (always `0` in v1; keep-tail deferred).
  The result is `OutEvent::Compacted { session, seq, summary, kept }`, a
  **persisted, seq-bearing** content event — persistence and `ReplayFrom` cover
  it for free (both are variant-agnostic over any seq-bearing event) —
  and `Session::replay`'s `Compacted` fold calls the same `apply_compaction`, so
  a resumed session stays compacted. The old prune-only `Context::compact`
  (#178) is unchanged and still the automatic pre-round fallback; `"compact"`
  only runs on request (`InMsg::Oneshot`, TUI `/compact [instructions]`).
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
  eviction + log replay reuse one seam (no DB in core). `EngineConfig.idle_ttl`
  auto-hibernation is deferred to the embedder's policy.
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
  `disabled`. `McpClient` is an enum over both transports and `McpTool` adapts
  whichever backs it; its `tools/list` is registered into the `ToolRegistry` as
  `mcp__<server>__<tool>` — a runtime-side tool provider, no core change, governed
  by the same permission profiles as any host tool; a server that fails to connect
  is logged and skipped. `HttpClient` is public so a multi-tenant embedder can
  assemble per-user registries with per-user tokens without the YAML path.
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
  other's write.

| Topic | Module |
| --- | --- |
| `InMsg`/`OutEvent`, Plan/TaskList events | [protocol](../docs/architecture/protocol.md) |
| profiles, tool mask, spawn gating, plan authority, skills, prompt assembly | [agents & permissions](../docs/architecture/agents-and-permissions.md) |
| turn loop, tool round-trip, steering, cancellation | [engine](../docs/architecture/engine.md) |
| streaming client, catalog, pool/retry/rate-limit | [provider](../docs/architecture/provider.md) |
| stdio/TUI/`serve` heads, event-sourced persistence | [heads & persistence](../docs/architecture/heads-and-persistence.md) |
| dependency gates, the quintet + exec tools (`bash`/`call`/`bash_output`/`rhai`), lifecycle hooks, MCP client (external tool servers) | [gates & host tools](../docs/architecture/gates-and-host-tools.md) |

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
Current phase is the July 2026 audit backlog — thematic epics tracked on GitHub
with P0/P1/P2 labels and blocked-by links:
#209 (docs), the parked-turn-state epic #276 (turns park as explicit serde
`TurnState`, batch-parallel tool resolution, mid-turn replay/resume,
[ADR-0061](../docs/adr/0061-parked-turn-state-batch-tool-resolution.md); the
in-process re-offer timer + executor `request_id` dedupe that self-heals a turn
stranded by a `broadcast`-lag drop landed here, #274/[ADR-0071](../docs/adr/0071-parked-turn-reoffer-timer.md)).
The pre-`serve` hardening epic #153 is **complete** — all six findings (#274,
#155, #156, #157, #158, #160) landed, and the local WebSocket `serve` head they
gated shipped last, per [ADR-0048](../docs/adr/0048-serve-head-local-trust-model.md).
The generic one-shot op framework (#324, `InMsg::Oneshot`, session compaction
as its first op, [ADR-0082](../docs/adr/0082-single-shot-session-ops-and-persisted-compaction.md))
is **complete**; auto-summarize-on-context-threshold is a natural follow-up
issue, not yet scheduled.

Shipped foundations: streaming `Llm` providers ([ADR-0007](../docs/adr/0007-streaming-llm-and-provider-crate.md))
— z.ai (primary)/OpenAI/Ollama via one OpenAI-compat client + a separate
Anthropic client; `ENTANGLEMENT_PROVIDER` or key auto-detect, else `EchoLlm`.
Heads: stdio `run`/`pipe`, `tui`, the `sessions`/`inspect` subcommands, and the
local WebSocket `serve` head (`skutter serve --port <N>`, loopback-bound axum
HTTP+WS, ✅ #153). Tools:
the root-contained quintet (`read` on an image file — `png`/`jpg`/`jpeg`/`gif`/
`webp` — emits a base64 **image content block** through a now-multimodal
`ToolResult`/`ToolOutput` path, #221/[ADR-0065](../docs/adr/0065-read-emits-image-content-blocks.md),
built on the `Message`/`Prompt` content-block migration #197/[ADR-0064](../docs/adr/0064-message-content-blocks.md)),
the opt-in exec set `bash`/`call`/`bash_output`
(`ENTANGLEMENT_ENABLE_BASH=1`; `bash` gains `workdir` + `run_in_background`, polled
via `bash_output`, #170), and the sandboxed `rhai` tool. **External MCP tool
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
