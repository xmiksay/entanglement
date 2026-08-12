# entanglement — Project Brief

Headless, Rust-based AI coding agent **engine**. The reasoning + tool-execution
loop is decoupled from any UI and exposed as an async actor: a typed `InMsg`
inbox and a broadcast `OutEvent` outbox. Every interface (ABI, stdio, WebSocket,
TUI) is a thin adapter over `holly.send()` / `holly.subscribe()`.

Architecture & the four interfaces:
[`../docs/architecture.md`](../docs/architecture.md). Overview:
[`../README.md`](../README.md). Decisions: [`../docs/adr/`](../docs/adr/)
(numbered, immutable — the index [`README.md`](../docs/adr/README.md) flags
superseded/amended entries). Intentional deferrals and docs-drift findings
live as GitHub issues, one each (#689).

## Stack

- **Rust** (stable, `../rust-toolchain.toml`; MSRV in the workspace `Cargo.toml`).
- Async: **Tokio** (`mpsc` inbox, `broadcast` outbox). Errors: `anyhow` + `thiserror`.
- Logging: `tracing`. Serde everywhere (the wire protocol).
- No web framework in core; the runtime head's `serve` subcommand brings `axum`
  (behind its own `serve` feature).

## Workspace

Three crates, two seams (core↔provider via the `Llm` trait, core↔runtime for
tool exec/approval over the protocol). Dependency direction is
`provider (leaf) ← core ← runtime` ([ADR-0053](../docs/adr/0053-invert-core-provider-seam.md)).

| Crate | Role | Hard rule |
| --- | --- | --- |
| `entanglement-provider` | **leaf** crate: the `Llm` **trait** + DTOs, all LLM I/O (OpenAI-compat client for z.ai/OpenAI/Ollama, separate Anthropic client, native Gemini client), per-endpoint pool/retry/rate-limit, the YAML provider/model catalog, and the **MCP client mechanism** (streamable-HTTP transport + OAuth stack, [ADR-0153](../docs/adr/0153-mcp-server-oauth.md) — mechanism only, policy stays in the runtime). Usable standalone. Detail: [provider](../docs/architecture/provider.md). | no `entanglement-*` deps; owns `reqwest`. |
| `entanglement-core` | actor engine: `Holly`, protocol, **agent turn loop**, `Context`. Advertises tool *schemas* only — holds no executable tools, makes no policy calls. Re-exports the provider ABI. Detail: [engine](../docs/architecture/engine.md). | **No UI/web-server deps** (`clap`/`axum`/`crossterm`/`ratatui` forbidden); `reqwest` is transitive via provider. `make tree` enforces. |
| `entanglement-runtime` | the head crate (binary `skutter`): `Tool` trait + `ToolRegistry`, host tools, tool execution + permission dispatch/approval, user sessions, the stdio `run`/`pipe`, `tui`, `serve` (local WS, [ADR-0048](../docs/adr/0048-serve-head-local-trust-model.md)), and `sessions`/`inspect`/`config` subcommands. Selects the provider and glues it to core. Features: `cli` / `provider` / `tui` / `serve` / `mcp-http` / `rhai`; `default = ["tui", "serve", "mcp-http", "rhai"]`; also a lean embedder library ([ADR-0025](../docs/adr/0025-runtime-cargo-feature-gates.md)). Detail: [heads & persistence](../docs/architecture/heads-and-persistence.md), [gates & host tools](../docs/architecture/gates-and-host-tools.md). | `--no-default-features` stays CLI/TUI/transport-free; `make check-lean` enforces. |

## Commands — drive through `make`

```bash
make help         # list every target with its one-line description
make run           # stdio head, one turn (text)
make run-json      # one turn, NDJSON events (opencode run --format json)
make run-tui       # launch the terminal UI
make pipe          # stdio pipe head — InMsg NDJSON on stdin, OutEvent NDJSON on stdout
make serve         # local WebSocket head on 127.0.0.1 (ARGS='--port 4517')
make sessions      # list past (resumable) sessions
make inspect       # resolved prompt/agents/skills/config, no engine (ARGS='prompt --agent build')
make install       # install the `skutter` binary into $CARGO_HOME/bin
make test          # unit + integration
make test-unit | make test-integration
make coverage      # workspace line coverage via llvm-cov, fail under COV_MIN%
make lint          # clippy --all-targets -D warnings
make fmt | check-fmt
make verify        # check-fmt + tree + check-lean + file-cap + lint + test  (CI-equivalent gate)
make tree          # entanglement-core dep hygiene gate (fails on UI/transport crates)
make check-lean    # runtime --no-default-features stays CLI/TUI/transport-free (ADR-0025)
make file-cap      # 400-line file cap gate (grandfathered debt in scripts/file-cap-allowlist.txt)
make userid        # UserId-free entanglement-runtime gate (ADR-0181)
make test-gates    # dep-gate + userid-gate self-test (scripts/dep-gate.test.sh)
make tag           # cut a release tag (VERSION=vX.Y.Z): refuses dirty tree / red verify
make build | check | clean
```

Build jobs capped at 4 via `../.cargo/config.toml` (also links with `lld`);
dev-profile tuning lives in the workspace `../Cargo.toml`.

## Providers (`skutter`)

Set `ENTANGLEMENT_PROVIDER` explicitly, or let it auto-detect by key (z.ai
first). No key → `EchoLlm`. Full detail (clients, catalog, resilience):
[provider](../docs/architecture/provider.md).

| `ENTANGLEMENT_PROVIDER` | wire | key env | model env (default) | base env |
| --- | --- | --- | --- | --- |
| `zai` (primary) | OpenAI-compat | `ZAI_API_KEY` | `ZAI_MODEL` (`glm-5.2`) | `ZAI_API_BASE` (Coding Plan) |
| `openai` | OpenAI-compat | `OPENAI_API_KEY` | `OPENAI_MODEL` (`gpt-4o`) | `OPENAI_API_BASE` |
| `ollama` | OpenAI-compat, keyless | — | `OLLAMA_MODEL` (`llama3.1`) | `OLLAMA_API_BASE` (or legacy `OLLAMA_BASE`) |
| `anthropic` | `/v1/messages` | `ANTHROPIC_API_KEY` | `ANTHROPIC_MODEL` (`claude-sonnet-4-5`) | — |
| `gemini` | Gemini `:streamGenerateContent` | `GEMINI_API_KEY` | `GEMINI_MODEL` (`gemini-2.5-flash`) | `GEMINI_API_BASE` |

That table is **catalog data, not hardcode** (#118): an embedded default
(`entanglement-provider/src/defaults.yml`) deep-merged with a user override at
`${config_dir}/entanglement/providers.yml`; a `wire:` tag lets a user add any
OpenAI-compatible endpoint with zero code change. `ModelEntry` carries
capability flags, pricing, an optional per-model `concurrency` cap
([ADR-0140](../docs/adr/0140-per-model-concurrency-cap-layered-on-endpoint-cap.md)),
generation params gated onto every `LlmRequest` (incl. `reasoning_effort`,
[ADR-0094](../docs/adr/0094-reasoning-effort-and-per-profile-generation-persistence.md)),
and the two extended-thinking knobs `thinking_style` (which Anthropic request
shape — the adaptive form is mandatory on current models, the fixed-budget form
400s there) and `replay_thinking` (whether captured thinking blocks are sent
back; [ADR-0160](../docs/adr/0160-extended-thinking-round-trip.md)).
Precedence: **env > user YAML > embedded defaults**.

Resilience is **per-endpoint** (keyed by a normalized base URL + a stable
sha256 API-key hash, [ADR-0050](../docs/adr/0050-per-endpoint-connection-pool-retry-rate-limit.md)/[ADR-0156](../docs/adr/0156-normalize-and-stabilize-the-endpoint-pool-key.md)):
connection pool, retry/backoff, RPM + concurrency caps, adaptive AIMD pacing,
bounded 429 park ([ADR-0111](../docs/adr/0111-adaptive-endpoint-pacing-and-429-retry-until-clear.md)),
per-model caps layered on top (ADR-0140), and a file-backed **cross-process**
gate ([ADR-0144](../docs/adr/0144-file-backed-shared-endpoint-state-across-instances.md),
opt-out `ENTANGLEMENT_NO_SHARED_ENDPOINT_STATE=1`), swept for orphaned
`.state`/`.lock` pairs on startup and on `/key` rotation (ADR-0156). Opt-in provider-side
**web search** ([ADR-0075](../docs/adr/0075-provider-side-web-search-mvp.md)/[ADR-0131](../docs/adr/0131-web-search-post-mvp-follow-ups.md))
runs outside the permission ladder — enabling *is* consent.

Runtime env vars (the one-place index; each is documented inline at the
feature that reads it):

| Env var | Purpose |
| --- | --- |
| `ENTANGLEMENT_PROVIDER` | select provider (`zai`/`openai`/`ollama`/`anthropic`/`gemini`/`echo`); else auto-detect by key |
| `<NAME>_API_KEY` / `<NAME>_MODEL` / `<NAME>_API_BASE` | per-provider key/model/base (the catalog `key_env`); the base also accepts the legacy `<NAME>_BASE` spelling (`_API_BASE` wins) |
| `<NAME>_RPM` / `<NAME>_CONCURRENCY` | per-provider endpoint RPM / in-flight cap (#414), overriding the catalog |
| `ENTANGLEMENT_MAX_CONCURRENCY` | last-resort process-wide concurrency override (default 3) |
| `ENTANGLEMENT_NO_SHARED_ENDPOINT_STATE=1` | opt out of cross-process RPM/concurrency/cool-down sharing (#523) |
| `ENTANGLEMENT_SHARED_STATE_DIR` | override the shared endpoint-state directory (default `${data_dir}/entanglement/endpoints`) |
| `ENTANGLEMENT_LOG_BODIES=1` | opt-in symmetric LLM request-body logging (#165) |
| `ENTANGLEMENT_PROVIDERS_FILE` | override the provider-catalog user file path |
| `ENTANGLEMENT_CONFIG_FILE` | override the layered user config file path (`config.yml`) |
| `ENTANGLEMENT_ENV_FILE` | override the managed provider-key env file path (`.env`) |
| `ENTANGLEMENT_AGENTS_DIR` / `ENTANGLEMENT_SKILLS_DIR` | replace the whole user agents/skills layer (also the cross-vendor opt-out) |
| `ENTANGLEMENT_GRANTS_FILE` / `ENTANGLEMENT_AGENT_MODELS_FILE` / `ENTANGLEMENT_AGENT_GENERATION_FILE` / `ENTANGLEMENT_AUX_MODELS_FILE` / `ENTANGLEMENT_MCP_TOKENS_FILE` / `ENTANGLEMENT_EXTRA_ROOTS_FILE` | override the six managed runtime files |
| `ENTANGLEMENT_PREAMBLE_FILE` / `ENTANGLEMENT_BRIEF_FILE` | override the system-prompt preamble / project-brief file |
| `ENTANGLEMENT_ENABLE_BASH=1` | opt-in: register `bash` at startup (the TUI `/enable tool bash` command, #498/#611, live-registers instead); its background jobs join with the always-available `poll` tool (#605), not a paired registry tool |
| `ENTANGLEMENT_SANDBOX=bwrap` / `ENTANGLEMENT_SANDBOX_NETWORK=1` | bubblewrap-confine `bash`/`call` process-wide; opt-in to keep network (#399, #479) |
| `ENTANGLEMENT_ECHO_FULL=1` | `EchoLlm` appends the full system text (debugging) |
| `ENTANGLEMENT_TUI_NOTIFY=1` / `ENTANGLEMENT_TUI_NO_MOUSE` | TUI desktop-notification opt-in / mouse opt-out |
| `ENTANGLEMENT_SESSION_RETENTION_DAYS` | session-log retention for the startup auto-prune (env > `config.yml` > `30`) |
| `ENTANGLEMENT_HOOK_EVENT` / `ENTANGLEMENT_SESSION_ID` / `ENTANGLEMENT_TOOL_NAME` | set on every hook child's env by the runtime (read-only context, not user-set) |

## The contract (read before touching the engine)

`entanglement-core/src/protocol.rs` is the single set of types every head uses:

```
InMsg    : Prompt | Approve | Reject | ToolResult | AnswerQuestion | RetractQuestion | ReplaceQuestion | Stop
          | PauseSession | ResumeSession
          | SetAgent | SetModel | SetGeneration | SetSessionMeta | SetToolOverlay | Oneshot | Spawn | ListSessions | ListQuestions | ReplayFrom | CloseSession
          | McpList | McpAdd | McpRemove | McpAuth
          | HibernateSession (trusted-only) | Resume (internal, not serialized)
OutEvent : SessionStarted | SessionEnded | SessionHibernated | SessionList | QuestionList | History | Status | AgentChanged | ModelChanged | GenerationChanged | SessionMetaChanged | ToolOverlayChanged
          | McpList | McpChanged | McpAuthChanged | Throttle
          | Plan | TextDelta | ReasoningDelta | ToolCallDelta | ToolCall | ToolRequest | ToolExec
          | UserQuestion | ToolOutput | TaskList | Usage | Error | Done | Compacted | FileChange | PlanChanged
          | SkillActive | AmbiguousRetry | SearchResult | ReasoningBlock
```

Load-bearing invariants — **the detail lives in the linked architecture doc,
never here**; each bullet is the claim + where to read it:

- **Tool execution is a protocol round-trip, parked as data**: a round ending in
  tool calls batch-emits `ToolExec` and parks the turn as serde `TurnState`;
  results resolve in any order; replay/resume reconstruct a mid-turn tail; a
  parked batch re-offers on a timer. `InMsg::ToolResult`/`OutEvent::ToolOutput`
  carry `is_error`/`duration_ms` as a **structured side channel** alongside the
  still-unchanged text `content`/`output` (#636, ADR-0176) — denied/masked/
  refused/unknown-tool/errored calls set `is_error`; `duration_ms` is measured
  once, generically, around the whole host-tool dispatch — and `exit_code`
  (#681, ADR-0186): a `bash`/`call` foreground exit (or a `poll` observing a
  job exit) as a real field via the defaulted `Tool::run_with_meta`, `None`
  for everything else incl. killed processes, orthogonal to `is_error`.
  [engine](../docs/architecture/engine.md),
  [ADR-0061](../docs/adr/0061-parked-turn-state-batch-tool-resolution.md)/[ADR-0071](../docs/adr/0071-parked-turn-reoffer-timer.md)/[ADR-0176](../docs/adr/0176-structured-tool-result-is-error-and-duration-fields.md)/[ADR-0186](../docs/adr/0186-exit-code-joins-the-structured-tool-result-side-channel.md).
- **Permission lives entirely in the runtime**; core only carries schemas and
  `PermissionProfile::resolve`. Rule keys: name-or-`*`, argument-scoped
  `tool(pattern)`, workdir-scoped `tool{pattern}`, and capability keys
  `read`/`write`/`call` fanned out at parse time (incl. MCP tools via a config
  hint). Path args grade root-relative; a config ceiling clamps
  least-privilege over every grade; `Approve` carries scope
  (`Once`/`Session`/`SessionDir`/`Always`, grants persisted); resolver +
  grant-store are pluggable seams and execution is session-aware.
  [agents & permissions](../docs/architecture/agents-and-permissions.md),
  [ADR-0052](../docs/adr/0052-approval-scope-and-persisted-grants.md)/[ADR-0114](../docs/adr/0114-capability-level-permission-keys.md)–[ADR-0117](../docs/adr/0117-mcp-tool-capability-fan-out.md)/[ADR-0125](../docs/adr/0125-permission-arguments-for-path-tools-are-normalized-root-relative.md)/[ADR-0126](../docs/adr/0126-session-scoped-directory-grants.md)/[ADR-0088](../docs/adr/0088-session-aware-tool-execution.md).
- **`rhai` is a sandboxed script tool with file/exec bindings** graded through
  the same permission chain (agent mask, skill mask, escape-root gate, workdir
  scopes); `background: true` detaches it like the other launchers (#637 —
  `x-` handle to `poll`, 120s/600s budget, cooperative kill only).
  [gates & host tools](../docs/architecture/gates-and-host-tools.md),
  [ADR-0046](../docs/adr/0046-rhai-sandboxed-script-tool.md)/[ADR-0115](../docs/adr/0115-rhai-exec-bindings-call-bash.md)/[ADR-0129](../docs/adr/0129-thread-the-skill-mask-into-rhai-binding-resolution.md)/[ADR-0130](../docs/adr/0130-rhai-exec-bindings-marshal-workdir.md)/[ADR-0185](../docs/adr/0185-rhai-joins-background-and-poll.md).
- **Trusted/untrusted frame split**: `Holly::send` is privileged;
  `send_from_wire` enforces a fail-closed allowlist (`ToolResult`, `Spawn`,
  `Resume`, `HibernateSession`, `McpAdd`/`McpRemove`, `McpAuth` refused;
  `SetToolOverlay` refused only for an enable entry — a deny-only overlay,
  including the empty clearing list, is wire-allowed, #634).
  [protocol](../docs/architecture/protocol.md),
  [ADR-0069](../docs/adr/0069-trusted-untrusted-wire-frame-split.md)/[ADR-0124](../docs/adr/0124-wire-refused-mcp-mutation-and-stdio-key-scrub.md)/[ADR-0177](../docs/adr/0177-wire-allowed-deny-only-tool-overlay.md).
- **Session-multiplexed**: every frame carries `SessionId`; `(session, seq)` is
  unique across authored content events (shared per-session counter); the
  seq-`0` bypass renders supervisor lifecycle errors; `ListSessions`/`McpList`/
  `ListQuestions` are correlation-id queries. [protocol](../docs/architecture/protocol.md),
  [ADR-0068](../docs/adr/0068-shared-per-session-seq-counter.md)/[ADR-0072](../docs/adr/0072-protocol-warts-settled-before-serve.md).
- **Model/provider/generation switching is live**: `SetModel` re-resolves via
  the runtime-supplied `model_resolver` seam; agent profiles can pin models
  (rebind on `SetAgent`); `SetGeneration` merges partial params; both persist
  per profile in managed files. **Aux models** (`summarize`, `session_title`,
  `narrate`) resolve per purpose via `aux_llm_resolver` / `AuxLlmRegistry`,
  falling back to the session's own backend; `narrate` drives `Session.action`
  live off every tool call (`narrate.rs`). A multi-user embedder builds
  per-user registries itself — `AuxModelStore::in_memory` + a resolver
  closure bound to its user (ADR-0181: no `UserId` in the runtime).
  [engine](../docs/architecture/engine.md),
  [heads & persistence](../docs/architecture/heads-and-persistence.md),
  [ADR-0063](../docs/adr/0063-realtime-model-provider-switch.md)/[ADR-0081](../docs/adr/0081-per-profile-model-pinning-and-rebind-on-set-agent.md)/[ADR-0094](../docs/adr/0094-reasoning-effort-and-per-profile-generation-persistence.md)/[ADR-0154](../docs/adr/0154-per-purpose-auxiliary-models.md)/[ADR-0183](../docs/adr/0183-narrate-purpose-and-per-user-aux-pins.md).
- **Compaction**: manual `/compact` is a `Oneshot` op that forks a successor
  session (copy-on-write, keep-tail) and retires the source; auto-compaction on
  overflow (`auto_compact`, default on) mutates the live context in place; the
  prune-only fallback stays silent by design. [engine](../docs/architecture/engine.md),
  [ADR-0082](../docs/adr/0082-single-shot-session-ops-and-persisted-compaction.md)/[ADR-0101](../docs/adr/0101-compaction-forks-into-a-new-session-copy-on-write.md)/[ADR-0102](../docs/adr/0102-compact-keep-tail-verbatim-in-the-fork-prompt.md)/[ADR-0103](../docs/adr/0103-auto-summarize-on-context-overflow.md)/[ADR-0110](../docs/adr/0110-compaction-successor-closes-predecessor.md)/[ADR-0121](../docs/adr/0121-prune-only-compact-stays-silent.md).
- **Session lifecycle**: hibernation is eviction-not-termination (resume
  replays the log, cascading over the spawn sub-tree); an optional idle TTL
  auto-hibernates settled roots; `PauseSession`/`ResumeSession` hold a session
  between cancel and hibernate; a spawned child's initiating prompt is
  synthesized into the log; display metadata (`name`/`action`) is settable
  live, with an auto session-title generator on first prompt.
  [engine](../docs/architecture/engine.md),
  [heads & persistence](../docs/architecture/heads-and-persistence.md),
  [ADR-0077](../docs/adr/0077-session-hibernation-evictable-resumable.md)/[ADR-0090](../docs/adr/0090-idle-ttl-auto-hibernation.md)/[ADR-0105](../docs/adr/0105-expose-idle-ttl-via-runtime-config.md)/[ADR-0112](../docs/adr/0112-resume-cascades-over-the-spawn-subtree.md)/[ADR-0113](../docs/adr/0113-persistence-synthesizes-a-spawned-childs-initiating-prompt.md)/[ADR-0144](../docs/adr/0144-pause-resume-a-hold-between-cancel-and-hibernate.md)/[ADR-0151](../docs/adr/0151-settable-session-metadata.md).
- **MCP**: external tool servers over stdio or streamable HTTP, registered as
  `mcp__<server>__<tool>` under the same permission profiles; live add/remove/
  list via engine-global messages answered by a runtime responder;
  provider-bundled servers with three-state activation
  (`enabled`/`allowed`/`disabled`, session-scoped lazy enablement); OAuth for
  protected servers (discovery + DCR + PKCE, tokens in a managed file,
  `/mcp connect`, or `/mcp connect --device-code` for RFC 8628 on a browser-less
  host — the cross-process refresh race is also closed, ADR-0182). The HTTP
  transport shares the LLM endpoint pool — same
  `HttpClient`, RPM/concurrency caps, 429 handling — keyed by the server's own
  URL plus its bundling provider's key when known, so a provider-bundled
  server's traffic counts against the same key budget its LLM endpoint
  enforces. [gates & host tools](../docs/architecture/gates-and-host-tools.md),
  [ADR-0067](../docs/adr/0067-mcp-client-as-runtime-tool-provider.md)/[ADR-0080](../docs/adr/0080-mcp-streamable-http-transport.md)/[ADR-0096](../docs/adr/0096-dynamic-toolregistry-sharedregistry.md)/[ADR-0097](../docs/adr/0097-live-mcp-server-management.md)/[ADR-0100](../docs/adr/0100-tui-mcp-command.md)/[ADR-0152](../docs/adr/0152-provider-bundled-mcp-servers-three-state-enablement.md)/[ADR-0153](../docs/adr/0153-mcp-server-oauth.md)/[ADR-0157](../docs/adr/0157-mcp-http-transport-shares-the-endpoint-pool.md)/[ADR-0182](../docs/adr/0182-mcp-oauth-device-code-flow-and-closed-refresh-race.md).
- **Agent tool masks**: entries are glob patterns; a per-session tool overlay
  (`SetToolOverlay`, trusted-only) injects/withdraws tools past the profile
  mask, an enable entry optionally `arg_pattern`-narrowed to an
  argument-scoped grade; in-app allowlist editing materializes a user-layer
  override file. **Live bash enablement** is folded into this same overlay
  (`/enable tool bash [--allow [<pattern>]]`, superseding the old bespoke
  `BashEnable`/`BashDisable` pair): an enable entry matching a closed table of
  lazily-registrable built-ins (`bash` only, today) also registers it into
  the shared tool registry on demand — registration is process-global, but
  its *advertisement* is session-scoped to the enabling overlay chain.
  [agents & permissions](../docs/architecture/agents-and-permissions.md),
  [gates & host tools](../docs/architecture/gates-and-host-tools.md),
  [ADR-0148](../docs/adr/0148-glob-patterns-in-the-agent-tool-mask.md)/[ADR-0149](../docs/adr/0149-per-session-tool-overlay.md)/[ADR-0083](../docs/adr/0083-in-app-tool-allowlist-editing-as-user-layer-materialization.md)/[ADR-0163](../docs/adr/0163-live-bash-enablement-is-a-tool-overlay-entry.md)/[ADR-0179](../docs/adr/0179-lazily-registered-built-ins-advertise-session-scoped.md).
- **Skills**: layered definitions with cross-vendor discovery; a loaded
  skill's `allowed_tools` gates the rest of the turn (agent mask still applies
  first). [agents & permissions](../docs/architecture/agents-and-permissions.md),
  [ADR-0074](../docs/adr/0074-cross-vendor-skill-and-agent-discovery.md)/[ADR-0106](../docs/adr/0106-skill-scoped-allowed-tools-enforcement.md).
- **Definitions are data, layered** embedded < user < project, later wins; the
  project layer is **trusted** ([ADR-0047](../docs/adr/0047-local-trust-boundary.md)).
  Provider keys live in a managed `.env` with two writer surfaces
  (`skutter config set-key`, TUI `/key`); lifecycle hooks wrap tool dispatch
  and prompt ingress; everything watched + live-reloaded with advisory-locked
  managed files. [agents & permissions](../docs/architecture/agents-and-permissions.md),
  [heads & persistence](../docs/architecture/heads-and-persistence.md),
  [ADR-0066](../docs/adr/0066-lifecycle-hooks-as-runtime-interceptors.md)/[ADR-0073](../docs/adr/0073-managed-env-file-writer-and-key-surfaces.md)/[ADR-0084](../docs/adr/0084-runtime-live-reload-and-managed-file-locking.md).
- **Filesystem containment**: symlink-safe root containment with an
  approval-gated escape hatch (per-tool, per-path grants; `glob`/`grep` ride
  durable `read` grants; the runtime scratch dir is pre-trusted; the `plan`
  profile gets a plans-folder write carve-out). Optional bubblewrap sandbox
  for `bash`/`call`, scopable per profile with a spawn-chain clamp.
  [gates & host tools](../docs/architecture/gates-and-host-tools.md),
  [ADR-0054](../docs/adr/0054-canonicalizing-symlink-safe-root-containment.md)/[ADR-0109](../docs/adr/0109-escape-root-access-via-approval.md)/[ADR-0119](../docs/adr/0119-rhai-bindings-route-through-the-escape-root-gate.md)/[ADR-0120](../docs/adr/0120-once-scoped-escape-root-grant-bound-to-request-id.md)/[ADR-0132](../docs/adr/0132-glob-grep-escape-root-search-via-durable-grant.md)/[ADR-0142](../docs/adr/0142-trusted-scratch-dir-and-plans-folder-carve-outs.md)/[ADR-0104](../docs/adr/0104-bubblewrap-sandbox-for-bash-call.md)/[ADR-0134](../docs/adr/0134-per-profile-sandbox-scoping-and-spawn-chain-clamp.md).
- **Plans**: `propose_plan` is the sole plan-authorship tool, file-backed under
  `.entanglement/plans/`, force-parked on `Ask`, sponsoring a blocking `build`
  child. A session-scoped content-hash staleness guard refuses a stale
  `path` resubmit at the *next* call; a dedicated debounced plans-folder
  watch (`plan_watch.rs`, reusing only `watch.rs`'s `spawn_debounced_watcher`
  primitive, never its unrelated agent/skill/config reload) surfaces the same
  out-of-band edit live as `OutEvent::PlanChanged`, self-healing the guard's
  registry so the next resubmit isn't also refused.
  [agents & permissions](../docs/architecture/agents-and-permissions.md),
  [engine](../docs/architecture/engine.md),
  [ADR-0145](../docs/adr/0145-one-plan-tool-file-backed-plans-and-blocking-review-loop.md)/[ADR-0138](../docs/adr/0138-sponsored-build-child-and-propose-plan-cycle.md)/[ADR-0173](../docs/adr/0173-watcher-driven-plan-file-changed-notice.md).
- **`ask_user` questions are listable/retractable/replaceable** after being
  asked (`ListQuestions`/`RetractQuestion`/`ReplaceQuestion`, runtime-owned
  registry). [protocol](../docs/architecture/protocol.md),
  [ADR-0146](../docs/adr/0146-ask-user-list-retract-replace.md).
- **An ambiguous LLM stop retries in place** (bounded, persisted
  `AmbiguousRetry` for exact replay) instead of ending the turn.
  [engine](../docs/architecture/engine.md),
  [ADR-0118](../docs/adr/0118-ambiguous-stop-reason-bounded-retry.md).
- **Endpoint throttle transitions are wire-visible** (`OutEvent::Throttle`,
  engine-global, transition-only). [provider](../docs/architecture/provider.md),
  [ADR-0141](../docs/adr/0141-wire-visible-throttle-transitions.md).
- **Reasoning has two rails**: `ReasoningDelta` renders (never folded into
  `Context`), `ContentPart::Reasoning` + `ReasoningBlock` replays. Anthropic
  requires the signed thinking block back on a parked turn's final assistant
  message; capture is unconditional, replay is per-model
  (`ModelEntry::replay_thinking`), and a foreign provider's block is dropped, not
  degraded to text. [provider](../docs/architecture/provider.md),
  [ADR-0160](../docs/adr/0160-extended-thinking-round-trip.md).
- **Multi-user mode is an embedder library API** (`UserId` on the wire,
  per-user catalogs/keys/budgets via `entanglement-provider::multi_user`,
  per-user MCP credentials via `provider::mcp::auth::UserTokenStore` +
  `user_scoped`, per-user ceilings/grants/aux pins as embedder
  implementations of the existing seams — the runtime crate never names
  `UserId`, enforced by `make userid`); `serve` stays local single-user.
  [ADR-0147](../docs/adr/0147-multi-user-mode-embedder-api.md)/[ADR-0181](../docs/adr/0181-userid-leaves-the-runtime-crate.md)/[ADR-0184](../docs/adr/0184-provider-hosted-multi-user-seams.md)/[ADR-0183](../docs/adr/0183-narrate-purpose-and-per-user-aux-pins.md),
  recipes: [embedding](../docs/embedding.md) §7.
  Two users sharing one **literal** API key each get their own rpm/concurrency
  slice via a per-user admission gate (`HttpClient::with_user_budget`) layered
  above the shared endpoint pool, mirroring ADR-0140's per-model gate.
  [ADR-0175](../docs/adr/0175-per-user-admission-gate-on-a-shared-literal-key.md).
  No in-tree head ships multi-user: the authenticated multi-user wire head
  designed by ADR-0174 was built (#674), then removed (#686) — `UserId` must
  not appear anywhere in `entanglement-runtime`; `serve` stays exactly
  ADR-0048's local posture. An authenticated wire head is an embedder's own
  build, out of tree. [ADR-0181](../docs/adr/0181-userid-leaves-the-runtime-crate.md)
  supersedes [ADR-0174](../docs/adr/0174-authenticated-multi-user-wire-head.md).

| Topic | Module |
| --- | --- |
| `InMsg`/`OutEvent`, Plan/TaskList events | [protocol](../docs/architecture/protocol.md) |
| profiles, tool mask, spawn gating, plan authority, skills, prompt assembly | [agents & permissions](../docs/architecture/agents-and-permissions.md) |
| turn loop, tool round-trip, steering, cancellation, compaction, aux models | [engine](../docs/architecture/engine.md) |
| streaming client, catalog, pool/retry/rate-limit | [provider](../docs/architecture/provider.md) |
| stdio/TUI/`serve` heads, event-sourced persistence, managed files | [heads & persistence](../docs/architecture/heads-and-persistence.md) |
| dependency gates, host tools (file sextet + exec), lifecycle hooks, MCP | [gates & host tools](../docs/architecture/gates-and-host-tools.md) |

Debugging: `skutter inspect prompt|agents|skills|config` re-runs the load-time
discovery with **no engine** and prints the resolved state, including the layer
that won an override. The TUI exposes the same views in-session via `/inspect`
(or `<leader>i`). Trust & scope decisions:
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
- **Never edit `CHANGELOG.md` in a feature/fix change.** The changelog is
  generated once, at release time, from `git log <last-tag>..HEAD` + the
  release's closed issues (see [`../docs/releasing.md`](../docs/releasing.md)) —
  per-PR `[Unreleased]` edits conflict on every concurrent merge.
- **Architecture decisions run ADR + arch doc in parallel.** Any hard-to-reverse
  design choice gets an ADR in [`../docs/adr/`](../docs/adr/) (numbered,
  immutable; supersede, never edit). Then update the relevant
  [`../docs/architecture/`](../docs/architecture/) module and add an inline ADR
  link. Drift check: `/arch check`.
- **Keep this brief + the `docs/architecture/` modules in sync.** The brief
  carries *pointers*; the architecture docs carry the *what is*; ADRs carry the
  *why*. When a message variant, profile, crate, or command changes, update the
  owning architecture doc (and the contract block above) in the same change —
  never grow this brief back into a re-statement of the docs.
- **Every intentional deferral and docs-drift finding gets its own GitHub
  issue** in the same change that defers it (#689 retired the old
  deferred-work ledger file — issues are the only tracker; an issue must be
  fully implemented by the change that closes it, never left half-open).

## Open work

Every epic through **0.5.0** is complete; the release history lives in
[`../CHANGELOG.md`](../CHANGELOG.md) (0.2.0–0.5.0 + `[Unreleased]`) and the
closed GitHub issues. Post-0.5.0 work (per-model concurrency #521, shared
endpoint state #523, pause/resume #516, plan tool #513, `ask_user` v2 #515,
multi-user API #522, bundled MCP #542, session metadata, aux models, MCP
OAuth, `auto_compact`) is in `[Unreleased]`. Current focus: the **2026-08-01
pre-release audit umbrella, issue #560** — P0 stability/provider-sharing fixes
and release mechanics gating the next tag.
