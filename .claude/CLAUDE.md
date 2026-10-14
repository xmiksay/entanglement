# entanglement — Project Brief

Headless, Rust-based AI coding agent **engine**. The reasoning + tool-execution
loop is decoupled from any UI and exposed as an async actor: a typed `InMsg`
inbox and a broadcast `OutEvent` outbox. Every interface (ABI, stdio, WebSocket,
TUI) is a thin adapter over `holly.send()` / `holly.subscribe()`.

Architecture & the four interfaces:
[`../docs/architecture.md`](../docs/architecture.md). Overview:
[`../README.md`](../README.md). Embedding guide:
[`../docs/embedding.md`](../docs/embedding.md). Decisions:
[`../docs/adr/`](../docs/adr/) (numbered, immutable — the index
[`README.md`](../docs/adr/README.md) flags superseded/amended entries).
Intentional deferrals and docs-drift findings live as GitHub issues, one each
(#689).

## Stack

- **Rust** (stable, `../rust-toolchain.toml`; MSRV is the workspace `Cargo.toml` `rust-version`, CI-tested by `make msrv`).
- Async: **Tokio** (`mpsc` inbox, `broadcast` outbox). Errors: `anyhow` + `thiserror`.
- Logging: `tracing`. Serde everywhere (the wire protocol).
- No web framework in core; the runtime's `serve` subcommand brings `axum`
  (behind its own `serve` feature).

## Workspace

Three crates, dependency direction `provider (leaf) ← core ← runtime`
([ADR-0053](../docs/adr/0053-invert-core-provider-seam.md)); layering detail in
[layers & ABI](../docs/architecture/layers-and-abi.md).

| Crate | Role | Hard rule |
| --- | --- | --- |
| `entanglement-provider` | leaf: the `Llm` trait + DTOs, all LLM clients (OpenAI-compat, Anthropic, Gemini, Responses), per-endpoint pool/retry/rate-limit, the YAML catalog, the MCP client mechanism (HTTP transport + OAuth). [provider](../docs/architecture/provider.md) | no `entanglement-*` deps; owns `reqwest`. |
| `entanglement-core` | actor engine: `Holly`, protocol, turn loop, `Context`. Advertises tool *schemas* only — no executable tools, no policy. Re-exports the provider ABI. [engine](../docs/architecture/engine.md) | no UI/web-server deps (`clap`/`axum`/`crossterm`/`ratatui`); `make tree` enforces. |
| `entanglement-runtime` | head crate (binary `skutter`): `Tool`/`ToolRegistry`, host tools, permission dispatch, `run`/`pipe`/`tui`/`serve` + `sessions`/`inspect`/`config`; also a lean embedder library. Features `cli`/`provider`/`tui`/`serve`/`mcp-http`/`rhai`/`web-fetch`, default `tui`+`serve`+`mcp-http`+`rhai`+`web-fetch` ([ADR-0025](../docs/adr/0025-runtime-cargo-feature-gates.md)). [heads & persistence](../docs/architecture/heads-and-persistence.md), [gates & host tools](../docs/architecture/gates-and-host-tools.md) | `--no-default-features` stays CLI/TUI/transport-free; `make check-lean` enforces. |

## Commands — drive through `make`

```bash
make help          # list every target with its one-line description
make build | check | clean | install       # install the `skutter` binary into $CARGO_HOME/bin
make run           # one turn; no key → hint + exit 2, ENTANGLEMENT_PROVIDER=echo for the offline stub
make run-json      # one turn as NDJSON events (opencode run --format json)
make run-tui       # terminal UI (first-run setup dialog when no provider is configured)
make pipe          # stdio pipe head — InMsg NDJSON on stdin, OutEvent NDJSON on stdout
make serve         # local WebSocket head on 127.0.0.1 (ARGS='--port 4517')
make sessions      # list past (resumable) sessions
make inspect       # resolved state, no engine (ARGS='prompt --agent general' | agents | 'skills --disclosures' | config | aux-models | mcp-tokens | mcp | 'session <id>')
make test          # unit + integration (also test-unit / test-integration), HOME/XDG-sandboxed (#747)
make test-live     # live z.ai discovery/cache probes (skip without ZAI_API_KEY)
make test-live-matrix # live z.ai tool-use matrix (#826): every GLM model × call encodings × discovery shapes; results under scratch/
make lint          # clippy, warnings = errors
make fmt | check-fmt
make verify        # check-fmt + tree + check-lean + file-cap + userid + lint + test  (CI-equivalent gate)
make tree          # entanglement-core dep hygiene gate (fails on UI/web-server crates)
make check-lean    # lean runtime (--no-default-features) stays CLI/TUI-free (ADR-0025)
make file-cap      # 400-line file cap gate (grandfathered debt in scripts/file-cap-allowlist.txt)
make userid        # UserId-free entanglement-runtime gate (ADR-0181)
make test-gates    # dep-gate + userid + file-cap + test-isolated self-tests, default + strict POSIX (scripts/dep-gate.test.sh)
make msrv          # cargo check (full + lean runtime) on the MSRV toolchain; msrv-version prints it
make coverage      # workspace line coverage via llvm-cov, fail under COV_MIN%
make tag           # cut a release tag (VERSION=vX.Y.Z): refuses dirty tree / red verify, does not push
```

Build jobs capped at 4 via `../.cargo/config.toml` (also links with `lld`);
dev-profile tuning lives in the workspace `../Cargo.toml`.

## Providers (`skutter`)

Set `ENTANGLEMENT_PROVIDER`, or let it auto-detect by key (z.ai first). No key
→ the TUI's first-run dialog, every other head a hint + exit 2; `EchoLlm` only
via `ENTANGLEMENT_PROVIDER=echo`
([ADR-0222](../docs/adr/0222-no-silent-echo-first-run-onboarding-and-init.md)).

| `ENTANGLEMENT_PROVIDER` | wire | key env | model env (default) | base env |
| --- | --- | --- | --- | --- |
| `zai` (primary) | OpenAI-compat | `ZAI_API_KEY` | `ZAI_MODEL` (`glm-5.3`) | `ZAI_API_BASE` (Coding Plan) |
| `zai_paas` | OpenAI-compat | `ZAI_API_KEY` | `ZAI_PAAS_MODEL` (`glm-5.3`) | `ZAI_PAAS_API_BASE` (pay-as-you-go) |
| `openai` | OpenAI-compat | `OPENAI_API_KEY` | `OPENAI_MODEL` (`gpt-4o`) | `OPENAI_API_BASE` |
| `ollama` | OpenAI-compat, keyless | — | `OLLAMA_MODEL` (`llama3.1`) | `OLLAMA_API_BASE` |
| `anthropic` | `/v1/messages` | `ANTHROPIC_API_KEY` | `ANTHROPIC_MODEL` (`claude-sonnet-5`) | — |
| `gemini` | `:streamGenerateContent` | `GEMINI_API_KEY` | `GEMINI_MODEL` (`gemini-2.5-flash`) | `GEMINI_API_BASE` |
| `openai_responses` (opt-in) | Responses API | `OPENAI_API_KEY` | `gpt-5.4` | — |

The table is **catalog data** (`entanglement-provider/src/defaults.yml`
deep-merged with `${config_dir}/entanglement/providers.yml`; precedence env >
user YAML > embedded): a `wire:` tag adds any OpenAI-compatible endpoint, an
`oauth:` block authenticates one
([ADR-0189](../docs/adr/0189-oauth-for-llm-provider-endpoints.md)), and the
per-model wire-fact knobs are catalog-only
([ADR-0213](../docs/adr/0213-prune-env-vars-and-catalog-knobs.md)) — see
[provider §advanced catalog](../docs/architecture/provider.md#advanced-catalog-wire-facts).
Resilience (pool, retry, RPM/concurrency caps, AIMD pacing, 429 park,
per-model caps, cross-process shared state) is **per endpoint**:
[provider](../docs/architecture/provider.md),
[ADR-0050](../docs/adr/0050-per-endpoint-connection-pool-retry-rate-limit.md)/[ADR-0144](../docs/adr/0144-file-backed-shared-endpoint-state-across-instances.md).

### Env vars — the one-place index

Each is documented inline at the feature that reads it.
[ADR-0213](../docs/adr/0213-prune-env-vars-and-catalog-knobs.md) pruned the
list; a retired var still set prints one warning naming its replacement
(`config::retired`).

| Env var | Purpose |
| --- | --- |
| `ENTANGLEMENT_PROVIDER` | select provider (`zai`/`zai_paas`/`openai`/`ollama`/`anthropic`/`gemini`/`echo`); else auto-detect by key |
| `<NAME>_API_KEY` / `<NAME>_MODEL` / `<NAME>_API_BASE` | per-provider key/model/base; `<NAME>` is the uppercased catalog entry name, the key var is the entry's `key_env` |
| `ENTANGLEMENT_PROVIDERS_FILE` / `ENTANGLEMENT_CONFIG_FILE` / `ENTANGLEMENT_ENV_FILE` | override the provider catalog / layered `config.yml` / managed key `.env` path |
| `ENTANGLEMENT_AGENTS_DIR` / `ENTANGLEMENT_SKILLS_DIR` / `ENTANGLEMENT_COMMANDS_DIR` | replace the whole user agents/skills/slash-commands layer (also the cross-vendor opt-out) |
| `ENTANGLEMENT_PREAMBLE_FILE` / `ENTANGLEMENT_BRIEF_FILE` | override the system-prompt preamble / project-brief file |
| `ENTANGLEMENT_STATE_DIR` | relocate all eight managed-state files (`grants`, `agent-models`, `agent-generation`, `aux-models`, `extra-roots`, `mcp-tokens`, `llm-tokens`, `mcp-tools` `.yml`; default `${config_dir}/entanglement`) |
| `ENTANGLEMENT_DEBUG` | comma list: `bodies` (LLM request-body logging), `echo` (`EchoLlm` appends the full system text), `no-shared-state` (no cross-process endpoint sharing); unknown token warns once |
| `ENTANGLEMENT_SANDBOX=bwrap` / `ENTANGLEMENT_SANDBOX_NETWORK=1` | bubblewrap-confine `bash`/`call`; only ever *tightens* the mode's own sandbox posture (ADR-0207 §6) |
| `ENTANGLEMENT_HOOK_EVENT` / `ENTANGLEMENT_SESSION_ID` / `ENTANGLEMENT_TOOL_NAME` | set on every hook child by the runtime (read-only context) |

Moved to `config.yml`/catalog by ADR-0213: `session_retention_days`,
`tui: {notify, mouse}` (also `theme`/`keybindings`/`statusline`),
`rpm`/`concurrency` (providers.yml). Retired outright (one warning each) by
ADR-0213: `discovery:`; by ADR-0226: `tool_advertising:`, the catalog
`discovery`/`tool_advertising` knobs, MCP `state:`; by ADR-0227: `endpoints:`.
`ENTANGLEMENT_SHARED_STATE_DIR` is only an undocumented subprocess-test hook.
The permission **mode** has no env var by design (`--mode` > `config.yml`
`mode:` > agent `default_mode` > `build`).

## The contract (read before touching the engine)

`entanglement-core/src/protocol/in_msg.rs` and `out_event.rs` are the single
set of types every head uses ([protocol](../docs/architecture/protocol.md)):

```
InMsg    : Prompt | Approve | Reject | ToolResult | AnswerQuestion | RetractQuestion | ReplaceQuestion | Stop
          | PauseSession | ResumeSession
          | SetMode | SetModel | SetGeneration | SetSessionMeta | SetToolOverlay | Oneshot | Spawn
          | ListSessions | ListQuestions | ListOperations | ListPending | ReplayFrom | CloseSession
          | McpList | McpAdd | McpRemove | McpAuth
          | RevertFiles | ForkSession | SetWorkdir | HibernateSession | Resume (internal, not serialized)
OutEvent : SessionStarted | SessionEnded | SessionHibernated | SessionList | QuestionList | OperationList | PendingList
          | History | Status | AgentChanged | ModeChanged | ModelChanged | GenerationChanged | SessionMetaChanged
          | ToolOverlayChanged | WorkdirChanged | McpList | McpChanged | McpAuthChanged | Throttle
          | PersistenceDegraded | PersistenceRecovered
          | Plan | TextDelta | ReasoningDelta | ToolCallDelta | ToolCall | ToolRequest | ToolExec
          | UserQuestion | ToolOutput | TaskList | Usage | Error | Done | Compacted | FileChange | PlanChanged
          | FilesReverted | SessionForked | SkillActive | AmbiguousRetry | SearchResult | ReasoningBlock
```

Load-bearing invariants — one claim each; **the detail lives in the linked
doc, never here**:

- **Tool execution is a protocol round-trip, parked as data** — a `ToolExec`
  batch parks the turn as serde `TurnState`; results (structured
  `is_error`/`duration_ms`/`exit_code`/`envelope`) resolve in any order
  ([engine](../docs/architecture/engine.md), [ADR-0061](../docs/adr/0061-parked-turn-state-batch-tool-resolution.md)/[ADR-0176](../docs/adr/0176-structured-tool-result-is-error-and-duration-fields.md)/[ADR-0186](../docs/adr/0186-exit-code-joins-the-structured-tool-result-side-channel.md)).
- **One advertised surface** — the compiled-in `KERNEL`, snapshotted at a
  session's first resolution, identical on every wire, never changed
  mid-session (any change busts the prompt cache); no mode, no knob
  ([gates §one surface](../docs/architecture/gates-and-host-tools.md), [ADR-0226](../docs/adr/0226-one-advertised-surface-mcp-envelope-registered-is-available.md)/[ADR-0202](../docs/adr/0202-prompt-cache-discipline-anchors-deferral-replay-compaction-date.md)).
- **Authority is the session's permission mode, not the agent** —
  `research`/`plan`/`build`/`auto` (`auto` = allow all but its deny/prompt lists),
  graded at the runtime's dispatch gate (a rule naming the tool outranks a
  class rule; a `*` rule meets a named one by length, a class one by
  strictness; longest within a kind; a rule naming a non-maskable tool never
  matches); there is no agent tool mask
  ([agents & permissions §3](../docs/architecture/agents-and-permissions.md), [ADR-0207](../docs/adr/0207-permission-modes-replace-agent-borne-authority.md)/[ADR-0209](../docs/adr/0209-auto-mode-defaults-to-prompt-not-deny.md)/[ADR-0218](../docs/adr/0218-agent-default-mode-is-a-starting-hint.md)/[ADR-0228](../docs/adr/0228-rhai-path-and-args-skill-tools-frontmatter-removed.md)/[ADR-0232](../docs/adr/0232-spawning-is-graded-bookkeeping-control-tools-non-maskable.md)).
- **The tool overlay is the user's own trusted voice** — per-session,
  mode-scoped enable/deny entries over the mode's grade
  ([agents & permissions](../docs/architecture/agents-and-permissions.md), [ADR-0149](../docs/adr/0149-per-session-tool-overlay.md)/[ADR-0177](../docs/adr/0177-wire-allowed-deny-only-tool-overlay.md)).
- **Trusted/untrusted frame split** — `Holly::send` is privileged;
  `send_from_wire` is a fail-closed allowlist (`InMsg::wire_allowed`); every
  refused variant + why: [protocol §2 trusted/untrusted split](../docs/architecture/protocol.md) ([ADR-0069](../docs/adr/0069-trusted-untrusted-wire-frame-split.md)).
- **Session-multiplexed** — every frame carries `SessionId`; `(session, seq)`
  is unique per authored event; `List*`/`McpList` are correlation-id queries
  ([protocol](../docs/architecture/protocol.md), [ADR-0068](../docs/adr/0068-shared-per-session-seq-counter.md)).
- **Model/generation switching is live; aux models resolve per purpose**
  ([engine](../docs/architecture/engine.md), [heads §6d](../docs/architecture/heads-and-persistence.md), [ADR-0063](../docs/adr/0063-realtime-model-provider-switch.md)/[ADR-0154](../docs/adr/0154-per-purpose-auxiliary-models.md)).
- **Compaction always forks a successor session** — no path rewrites a live
  context, so every log replays exactly; `/fork` is the non-compacting copy
  ([engine](../docs/architecture/engine.md), [ADR-0205](../docs/adr/0205-every-compaction-forks-a-successor-session.md)/[ADR-0223](../docs/adr/0223-non-compacting-session-fork.md)).
- **Session lifecycle** — hibernate is eviction, not termination; pause/resume,
  idle TTL, unknown-session policy, tenant-prefixed ids
  ([engine](../docs/architecture/engine.md), [heads §6b](../docs/architecture/heads-and-persistence.md), [ADR-0077](../docs/adr/0077-session-hibernation-evictable-resumable.md)/[ADR-0208](../docs/adr/0208-pause-resume-a-hold-between-cancel-and-hibernate.md)/[ADR-0210](../docs/adr/0210-unknown-session-policy.md)/[ADR-0211](../docs/adr/0211-idgen-context-and-tenant-prefix-inheritance.md)).
- **A session has a working directory** — relative path args are rewritten
  onto it *before* grading; the model learns it from the per-round notice
  ([gates §8 session working directory](../docs/architecture/gates-and-host-tools.md), [engine](../docs/architecture/engine.md), [ADR-0224](../docs/adr/0224-session-working-directory.md)).
- **Filesystem containment** — symlink-safe root containment, approval-gated
  escape hatch, optional bubblewrap sandbox as a mode fact
  ([gates §8](../docs/architecture/gates-and-host-tools.md), [ADR-0054](../docs/adr/0054-canonicalizing-symlink-safe-root-containment.md)/[ADR-0109](../docs/adr/0109-escape-root-access-via-approval.md)/[ADR-0104](../docs/adr/0104-bubblewrap-sandbox-for-bash-call.md)).
- **Undo/rewind is per-write backups, not git** — exec changes are never
  tracked; replay never touches the disk
  ([gates §undo](../docs/architecture/gates-and-host-tools.md), [ADR-0212](../docs/adr/0212-undo-rewind-via-per-write-file-backups.md)).
- **`rhai` is a sandboxed script tool whose every binding runs the executor's
  dispatch seam** — `mcp::<server>::<tool>`/`skill::<skill>::<fn>`
  namespaces, `http()` with user-layer-only `http_auth` credentials; inline
  `script` or a script file `path`, `args` a constant map; the launch is
  graded like any tool on its `path` (Allow → Ask per command, Ask → one Run
  all / Ask per command / Reject card); a `rhai.trusted` folder's script runs
  force-allowed with no card; writes of trusted scripts and skill files
  always ask, in every mode
  ([gates §script surface](../docs/architecture/gates-and-host-tools.md), [ADR-0046](../docs/adr/0046-rhai-sandboxed-script-tool.md)/[ADR-0185](../docs/adr/0185-rhai-joins-background-and-poll.md)/[ADR-0227](../docs/adr/0227-one-binding-path-namespaces-http-skill-libraries.md)/[ADR-0228](../docs/adr/0228-rhai-path-and-args-skill-tools-frontmatter-removed.md)).
- **MCP** — called only through `mcp(server, tool, args)` (core unwraps to
  `mcp__<server>__<tool>`), schemas never advertised; registered = available
  (`disabled: true` the one opt-out), lazy connect, live add/remove,
  provider-bundled servers, OAuth, per-user scopes
  ([gates §10](../docs/architecture/gates-and-host-tools.md), [ADR-0067](../docs/adr/0067-mcp-client-as-runtime-tool-provider.md)/[ADR-0226](../docs/adr/0226-one-advertised-surface-mcp-envelope-registered-is-available.md)/[ADR-0152](../docs/adr/0152-provider-bundled-mcp-servers-three-state-enablement.md)/[ADR-0153](../docs/adr/0153-mcp-server-oauth.md)/[ADR-0188](../docs/adr/0188-session-keyed-per-user-mcp-scopes.md)).
- **Discovery is six `Control` tools** — `list_agents`/`list_models`/
  `list_skills`/`load_skill`/`list_mcp`/`mcp_tools`, no arg or one `name`;
  `mcp_tools` reads (and lazily connects) one server's catalog; they, `mcp`,
  the script-only read variants (`read_raw`/`*_json`) and the bookkeeping
  Control tools (`poll`/`ask_user`/`update_tasks`/`request_mode`/
  `propose_plan`) are non-maskable — an overlay entry or rule naming one is
  refused/warned, never a silent no-op (#843); `agent`/`agent_send` are
  graded like any tool
  ([gates §one surface](../docs/architecture/gates-and-host-tools.md), [ADR-0226](../docs/adr/0226-one-advertised-surface-mcp-envelope-registered-is-available.md)/[ADR-0232](../docs/adr/0232-spawning-is-graded-bookkeeping-control-tools-non-maskable.md)).
- **Skills are additive-only and all model-visible; slash commands are
  head-side templates** — a skill ships `lib.rhai` functions and script
  files; `tools:`/`user_only` frontmatter removed (`tools:` skips the skill,
  `user_only` loads with a warning)
  ([agents & permissions](../docs/architecture/agents-and-permissions.md), [ADR-0194](../docs/adr/0194-skills-are-additive-only.md)/[ADR-0221](../docs/adr/0221-user-slash-commands-are-head-side-prompt-templates.md)/[ADR-0228](../docs/adr/0228-rhai-path-and-args-skill-tools-frontmatter-removed.md)/[ADR-0229](../docs/adr/0229-every-skill-is-model-visible-user-only-removed.md)).
- **Definitions are data, layered** embedded < user < project (project
  trusted); managed files are advisory-locked and live-reloaded
  ([agents & permissions](../docs/architecture/agents-and-permissions.md), [heads §6c](../docs/architecture/heads-and-persistence.md), [ADR-0047](../docs/adr/0047-local-trust-boundary.md)/[ADR-0084](../docs/adr/0084-runtime-live-reload-and-managed-file-locking.md)).
- **Plans** — `propose_plan` (only in `plan` mode) is file-backed; approval
  switches the session's mode in place, to the approver's choice
  ([agents & permissions](../docs/architecture/agents-and-permissions.md), [ADR-0145](../docs/adr/0145-one-plan-tool-file-backed-plans-and-blocking-review-loop.md)/[ADR-0207](../docs/adr/0207-permission-modes-replace-agent-borne-authority.md)).
- **Approval cards are data** — `ToolRequest.reason` says why a card
  parked and `scopes` what an approval honors; heads never parse card text
  ([protocol](../docs/architecture/protocol.md), [ADR-0230](../docs/adr/0230-structured-card-reason-and-honest-scopes-on-tool-request.md)).
- **`ask_user` questions are listable/retractable/replaceable**
  ([protocol](../docs/architecture/protocol.md), [ADR-0146](../docs/adr/0146-ask-user-list-retract-replace.md)).
- **An ambiguous LLM stop retries in place**, bounded and persisted
  ([engine](../docs/architecture/engine.md), [ADR-0118](../docs/adr/0118-ambiguous-stop-reason-bounded-retry.md)).
- **Throttle and persistence outages are wire-visible transitions**
  ([provider](../docs/architecture/provider.md), [heads §6b](../docs/architecture/heads-and-persistence.md), [ADR-0141](../docs/adr/0141-wire-visible-throttle-transitions.md)).
- **Reasoning has two rails** — `ReasoningDelta` renders, `ReasoningBlock`
  replays ([provider §extended thinking](../docs/architecture/provider.md#extended-thinking-capture-and-replay), [ADR-0160](../docs/adr/0160-extended-thinking-round-trip.md)).
- **Multi-user is an embedder library API** — `UserId` never appears in
  `entanglement-runtime` (`make userid`); no in-tree head is multi-user
  ([embedding §7](../docs/embedding.md), [ADR-0147](../docs/adr/0147-multi-user-mode-embedder-api.md)/[ADR-0181](../docs/adr/0181-userid-leaves-the-runtime-crate.md)).

| Topic | Module |
| --- | --- |
| crate layering, the actor ABI | [layers & ABI](../docs/architecture/layers-and-abi.md) |
| `InMsg`/`OutEvent`, Plan/TaskList events | [protocol](../docs/architecture/protocol.md) |
| agent profiles, permission modes, spawn gating, plan authority, skills, slash commands, prompt assembly | [agents & permissions](../docs/architecture/agents-and-permissions.md) |
| turn loop, tool round-trip, steering, cancellation, compaction, fork, workdir | [engine](../docs/architecture/engine.md) |
| streaming clients, catalog, pool/retry/rate-limit, thinking | [provider](../docs/architecture/provider.md) |
| stdio/TUI/`serve` heads, persistence, managed files, aux models | [heads & persistence](../docs/architecture/heads-and-persistence.md) |
| dependency gates, host tools, undo, discovery, hooks, MCP | [gates & host tools](../docs/architecture/gates-and-host-tools.md) |

Debugging: `skutter inspect prompt|agents|skills|commands|config|modes|aux-models|mcp-tokens|mcp|session`
re-runs load-time discovery with **no engine** and prints the resolved state,
including the winning layer; the TUI shows the same via `/inspect` (`<leader>i`).
Trust & scope: [ADR-0047](../docs/adr/0047-local-trust-boundary.md) (repo
trusted; system < user < repo) and
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
  *why*. When a message variant, profile, crate, env var or command changes,
  update the owning architecture doc (and the contract block / env index /
  command list above) in the same change. A new invariant gets **one** short
  bullet here — claim + links — never a re-statement of the docs.
- **Every intentional deferral and docs-drift finding gets its own GitHub
  issue** in the same change that defers it (#689 retired the old
  deferred-work ledger file — issues are the only tracker; an issue must be
  fully implemented by the change that closes it, never left half-open).

## Open work

Release history lives in [`../CHANGELOG.md`](../CHANGELOG.md) and the closed
GitHub issues; open work is the open issue list. Current focus: cutting the
**0.7.0** release (issue #741).
