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
[`../docs/architecture.md`](../docs/architecture.md) §5b. Connection pool,
retry/backoff, rate-limit (429/`Retry-After`/RPM), reasoning/thinking stream
events, the YAML provider/model catalog, and the provider-owned session handle
all live in this crate now (✅ #52–#55, #118, [ADR-0007](../docs/adr/0007-streaming-llm-and-provider-crate.md)).

## The contract (read before touching the engine)

`entanglement-core/src/protocol.rs` defines the single set of types every head uses:

```
InMsg    : Prompt | Approve | Reject | ToolResult | AnswerQuestion | Stop
          | SetTasks | SetPlan | SetAgent | Spawn | ListSessions | CloseSession
          | Resume (internal, not serialized)
OutEvent : SessionStarted | SessionEnded | SessionList | Status | AgentChanged
          | Plan | TextDelta | ReasoningDelta | ToolCall | ToolRequest | ToolExec
          | UserQuestion | ToolOutput | TaskList | Error | Done | FileChange
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
permission dispatch (`Allow`/`Ask`/`Deny`), resolved in the runtime. Profiles are
**file-defined** (✅ #112, [ADR-0034](../docs/adr/0034-file-based-agent-definitions.md)):
markdown + YAML frontmatter (`name`/`description`/`mode`/`model`/`permission`,
body = system prompt), discovered by `entanglement_runtime::agents::load_registry`
into a `ProfileRegistry` — embedded built-ins < user
(`${config_dir}/entanglement/agents/*.md`) < project
(`<root>/.entanglement/agents/*.md`), later wins on `name` collision, same
defaults+override shape as the provider catalog (#118). Editing a built-in = a
same-`name` file in a higher layer. `description` is the one field disclosed to a
spawning model (roster in the `agent`/`agent_spawn` tool descriptions + name
enum). Frontmatter `tools`/`disallowed_tools`/`can_spawn`/`spawnable_agents` parse
now, enforcement deferred (needs per-session specs #116/#119). `AgentMode` gained
`all` (primary + spawnable). The stored `system_prompt` is **assembled**, not the
raw body (✅ #113, [ADR-0035](../docs/adr/0035-deterministic-system-prompt-assembly.md)):
`entanglement_runtime::system_prompt::assemble` composes shared preamble + agent
body + project brief (frontmatter `include_brief: true`, from the standard
`AGENTS.md`/`.agents/AGENTS.md`/`.claude/CLAUDE.md`/`CLAUDE.md`, first found wins)
+ generated env block (cwd/platform/date) +
skill index — each optional, in that fixed order — at load time. A subagent gets
`preamble + body (+ brief)` only (no env/skills, never the parent's prompt);
inputs come from `PromptContext::load` (overridable via
`ENTANGLEMENT_PREAMBLE_FILE`/`ENTANGLEMENT_BRIEF_FILE`). The skill index is
populated from the skill registry (✅ #114,
[ADR-0036](../docs/adr/0036-skill-discovery-and-registry.md)): a **skill** is a
directory with a `SKILL.md` (YAML frontmatter + markdown body) + optional
`references/*.md`/`scripts/*`, discovered by
`entanglement_runtime::skills::load_registry` into a `SkillRegistry` — embedded
stock skills (single-file) < user (`${config_dir}/entanglement/skills/**/SKILL.md`,
override `ENTANGLEMENT_SKILLS_DIR`) < project
(`<root>/.entanglement/skills/**/SKILL.md`), later wins on `name` collision, same
defaults+override shape as agents/catalog. Recursive walk for `SKILL.md` markers;
symlinked dups + dir cycles deduped by canonical path; malformed file = loud
error; `root_dir` resolved once at discovery. Frontmatter: `name`/`description`
required, `user_only` (only explicit user invocation — withheld from disclosure),
`allowed_tools` (mask, enforcement deferred #116). **Tier-1 disclosure only**:
`disclosures()` emits one `name: description` line per non-`user_only` skill
(~100 tokens each); bodies never preloaded. Selection stays LLM reasoning — no
keyword/embedding gate; description quality is the contract. Bodies + payload are
tier-2, loaded on demand (`load_skill`, #115). Core still ships `system_prompt`
verbatim as `LlmRequest.system`. `Plan` and `TaskList` are
session-owned snapshots, written by built-in tools or harness `Set*` messages.
The `Tool` trait carries `schema()` (feeds `ToolSpec.schema` → the model's
`input_schema`); `host_tools(root)` (see ADR-0008 + ADR-0009 + ADR-0010 + ADR-0031)
assembles the root-contained quintet (`read`/`glob`/`grep`/`edit`/`write` —
`write` is whole-file create/overwrite, ADR-0031); `BashTool` is opt-in at the
head (`ENTANGLEMENT_ENABLE_BASH=1`).

Sub-agent spawn (#60, [ADR-0022](../docs/adr/0022-subagent-spawn.md)): the
runtime-owned `agent_spawn { agent, prompt }` tool (renamed from `spawn_agent`,
✅ #120, [ADR-0033](../docs/adr/0033-agent-tool-family-and-blocking-agent.md))
issues `InMsg::Spawn`; the
supervisor records `parent_links[child]=parent` and starts the child under the
requested profile. Bypasses per-tool approval like the built-ins. Non-blocking
spawn (✅ #89, [ADR-0026](../docs/adr/0026-async-subagent-spawn-and-poll.md),
`runtime::agent_poll`): `agent_spawn` returns the child handle (`agent_id`)
*immediately* instead of parking the parent turn on the child's `Done`, so one
turn can launch several sub-agents that run concurrently. The launch task records
the child's answer + duration into a shared `AgentRegistry` keyed by the handle;
the parent collects it with a second runtime-owned tool
`agent_poll { agent_id, timeout_secs }` (also intercepted before permission),
which blocks up to `timeout_secs` and returns the answer + elapsed or a
still-running status (unknown handle → error). A third tool `agent { agent,
prompt }` (✅ #120) is the **blocking** single-delegation path: it runs the exact
`agent_spawn` launch (same guard/clamp/`Spawn`), then waits for the child's
answer and returns it directly — one call, no poll. Refusals are identical across
`agent`/`agent_spawn` (one shared guard path); a parent `Stop` while `agent` is
parked leaves the child collectable via `agent_poll`. Supersedes ADR-0022's
synchronous answer-relay; the TUI sessions list shows each sub-agent's live
spawn duration.
Spawn limits (✅ #76,
[ADR-0023](../docs/adr/0023-subagent-spawn-limits.md)): the runtime executor's
`SpawnGuard` folds parent links from `SessionStarted` and refuses a spawn past a
depth cap (`MAX_SPAWN_DEPTH`) or a cumulative per-root budget
(`MAX_SPAWNS_PER_ROOT`), replying with a clear refusal `ToolOutput`. Spawn
permission gating (✅ #77, [ADR-0024](../docs/adr/0024-subagent-permission-gating.md),
`runtime::permission`): a `Subagent`-mode leaf profile (read-only `explore`)
can't spawn at all, and each child's per-tool permission is clamped to the
least-privileged rule across its ancestor chain (`Deny < Ask < Allow`) — a child
is never more privileged than its parent. Filesystem isolation (a separate child
root) for sub-sessions still deferred.

Ask-user prompt (✅ #90, [ADR-0027](../docs/adr/0027-ask-user-interactive-prompt.md)):
the runtime-owned `ask_user { question, options, allow_free_form }` tool is
intercepted on `ToolExec` before permission resolution (like `agent_spawn`,
`runtime::ask_user`). It emits a dedicated `OutEvent::UserQuestion`, parks for
the head's `InMsg::AnswerQuestion` (consumed off the inbound fan-out like
`Approve`/`Reject`), and folds the picked label or free-form text back as the
tool's `ToolOutput`. The TUI adds a `PendingQuestion` interaction state (labelled
choices + an "Other" free-text escape) alongside `ApprovalMode`; the one-shot
`run` head auto-answers so it never parks.

Session lifecycle (✅ #21, [ADR-0028](../docs/adr/0028-session-lifecycle-enumeration-and-backpressure.md)):
two supervisor-global messages the supervisor answers/acts on directly (never
routed to a session). `ListSessions { session }` returns one
`OutEvent::SessionList { session, sessions: Vec<SessionInfo> }` snapshot of the
live sessions (`SessionInfo { session, parent, profile, root }`) — a
reconnecting head enumerates in one round-trip; `session` is a correlation id the
reply echoes. `CloseSession { session }` drops the session's command channel so
its task exits and emits `SessionEnded` — the explicit destroy `Stop`
(cancel-semantics, ADR-0017) does not perform. Session ids are single-use: mint a
fresh `SessionId::new_uuid()` after close rather than reuse (which restarts
`seq`). The supervisor routes with a non-blocking `try_send` + bounded retry,
shedding to a saturated session (an `Error` + `warn`) rather than blocking its
loop and stalling every other session.

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
[ADR-0022](../docs/adr/0022-subagent-spawn.md)), spawn tree bounded by depth +
per-root fan-out (#76, [ADR-0023](../docs/adr/0023-subagent-spawn-limits.md)),
spawn permission-gated — `Subagent`-mode leaves can't spawn + child permissions
clamped to the ancestor chain (#77,
[ADR-0024](../docs/adr/0024-subagent-permission-gating.md)), non-blocking spawn —
`spawn_agent` returns a handle immediately + `agent_poll` awaits it, for true
fan-out (#89, [ADR-0026](../docs/adr/0026-async-subagent-spawn-and-poll.md)).
**Cleanup** — orphaned `apply_diff.rs` + `audit.rs`
removed (#63); docs drift guard (#62) closed out the epic by flipping every
🚧 marker in `docs/architecture.md`/`README.md` to ✅ as each child landed.

Already shipped: `skutter run`/`pipe` (stdio) and `tui`; LLM providers wired
([ADR-0007](../docs/adr/0007-streaming-llm-and-provider-crate.md)) — `Llm` is a
streaming trait returning `BoxStream<LlmEvent>`; one generic OpenAI-compat client
serves z.ai (primary)/OpenAI/Ollama, plus a separate Anthropic client;
`ENTANGLEMENT_PROVIDER` or key auto-detect, else `DummyLlm`. `skutter serve`
(axum WS) is the next head. `bash` stays opt-in (`ENTANGLEMENT_ENABLE_BASH=1`),
unsandboxed — a real sandbox is a future security-focused ADR.
