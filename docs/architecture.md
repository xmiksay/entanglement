# entanglement — Architecture

How the headless engine is structured and how the four interfaces share one
contract. Overview & roadmap in [`../README.md`](../README.md). The *why* behind
each choice here is recorded in the [decision log](adr/README.md) (ADRs).

This document describes the current *what is*, with the three-layer direction
([ADR-0006](adr/0006-core-dependency-hygiene-gate.md)) marked inline:
**✅ shipped** vs **🚧 decided but pending** (tracked in GitHub issues).

## 0. Layers: core / provider / runtime — [ADR-0006](adr/0006-core-dependency-hygiene-gate.md)

Three crates, two seams. Heads depend on core; core never depends on a head.

```
┌──────────── entanglement-runtime (head, binary `skutter`) ─────────────┐
│ user sessions · host tools · tool execution · permission dispatch ·    │
│ approval UX · persistence · transports (stdio ✅, TUI ✅, WS 🚧)        │
└─────────▲──────────────────────────────────────────────▲───────────────┘
          │ send()/subscribe() (ABI)      tool exec + approval (protocol)
┌─────────┴──────────────── entanglement-core (engine) ───┴───────────────┐
│ Holly actor · InMsg/OutEvent · agent turn loop · Tool *trait* · Context │
└─────────▲────────────────────────────────────────────────────────────────┘
          │ Llm trait: stream() + session handle
 ┌─────────┴──────────── entanglement-provider (LLM I/O) ────────────────────┐
│ OpenAI-compat + Anthropic clients · pool · retry · rate-limit ·           │
│ reasoning stream · models-per-provider                                    │
└────────────────────────────────────────────────────────────────────────────┘
```

- **core** — the reasoning engine: actor, protocol, turn loop, the `Tool` *trait*
  (not implementations), `Context`. Pure, reusable, zero UI/transport deps (§7).
- **provider** — all LLM I/O behind the `Llm` trait (§5b).
- **runtime** — the head: host tools + their execution, permission dispatch +
  approval, user sessions, every transport (§6, §8). Feature-gated
  ([ADR-0025](adr/0025-runtime-cargo-feature-gates.md)): `default = ["tui"]` is
  the full `skutter` binary, while `--no-default-features` is a **lean library**
  — `host` + `tool_runner` + `permission` + `subagent` + `persistence` +
  `session_store` over core + tokio + glob/regex, with no CLI/TUI/transport deps
  (`make check-lean` enforces, §7). The `cli` feature (clap + providers) sits
  between the two, leaving room for a `ws = ["cli", …]` sibling.

**Responsibility relocation is mostly landed:** the host-tool *implementations*
now live in `entanglement-runtime` (✅ #57, §8), and tool *execution* moved there
too — core emits `OutEvent::ToolExec` and the runtime answers with
`InMsg::ToolResult` (✅ #58, §3, §8). *Permission dispatch* (the `Allow|Ask|Deny`
decision + approval wait) also moved to the runtime (✅ #59, §3): core emits
`ToolExec` for *every* host tool and no longer consults `PermissionProfile`; the
runtime tool executor resolves the permission and drives approval. Core's
`Session` is now slimmed to loop + turn state (✅ #61): it holds the `Context`,
the provider session handle (`llm`, #55), the profile, the plan/tasks snapshots,
and the loop counters — no cached tool set (the schemas come from
`EngineConfig.tool_specs` at turn time).

## 1. The actor model (the ABI) — [ADR-0001](adr/0001-actor-model-abi.md)

`entanglement-core` exposes one engine, [`Holly`][holly], as an async actor:

```
                       ┌──────────── entanglement-core ────────────┐
  ABI (direct) ───────►│  inbox   mpsc<Sender<InMsg>>        │
  stdio (NDJSON) ─────►│  ────────────────────────► engine   │
  WebSocket ──────────►│  outbox  broadcast<Sender<OutEvent>│────► subscribe()
  TUI ────────────────►│  (seq'd, session-multiplexed)       │
                       └────────────────────────────────────┘
```

- `holly.send(InMsg)` — push a typed message in (zero serialization).
- `holly.subscribe()` — get a `broadcast::Receiver<OutEvent>` (fan-out to N
  subscribers).

This **is** the ABI. The other three heads are adapters that translate their
wire format to/from `InMsg`/`OutEvent`. Adding a head never touches the engine.

## 2. Wire protocol (`protocol.rs`) — [ADR-0002](adr/0002-session-multiplexed-protocol.md)

One set of serde-tagged types crosses every transport:

```
#[serde(tag = "kind", rename_all = "snake_case")]
InMsg    = Prompt{session,text} | Approve{session,request_id}   // approval →
         | Reject{session,request_id,reason?}                   // runtime, not core (#59)
         | ToolResult{session,request_id,output}   // runtime → core: tool ran (#58)
         | AnswerQuestion{session,request_id,answer}  // ask_user answer → runtime (#90)
         | Stop{session}
         | SetTasks{session,tasks} | SetPlan{session,content} | SetAgent{session,agent}
         | Spawn{session,parent,agent,prompt}   // start a child session (sub-agent) (#60)
         | ListSessions{session}   // supervisor-global query; session = correlation id (#21)
         | CloseSession{session}   // explicit destroy → SessionEnded (#21)
         | Resume{session,records}   // internal, not serialized (#[serde(skip)]); replay log → session (§6b)

OutEvent = SessionStarted{session,parent?,profile,model?,root,ts}   // lifecycle, no seq
         | SessionEnded{session,ts}           // lifecycle, no seq
         | SessionList{session,sessions:[SessionInfo]}   // reply to ListSessions, no seq (#21)
         | Status{session,state}              // point-in-time, no seq
         | AgentChanged{session,agent}        // point-in-time, no seq
         | Plan{session,seq,content}          // markdown prose snapshot
         | TextDelta{session,seq,text}
         | ReasoningDelta{session,seq,text}   // reasoning/thinking stream (#54)
         | ToolCall{session,seq,request_id,tool,input}      // display-only, every call (before exec)
         | ToolRequest{session,seq,request_id,tool,input}   // Ask prompt, from runtime (#59)
         | ToolExec{session,seq,request_id,tool,input}      // core → runtime: dispatch it (#58/#59)
         | UserQuestion{session,seq,request_id,question,options,allow_free_form}  // ask_user prompt (#90)
         | ToolOutput{session,seq,request_id,tool,output}
         | TaskList{session,seq,tasks}        // full outline snapshot
         | Error{session,seq,message}
         | Done{session,seq}
         | FileChange{session,seq,path,before?,after?,change_kind}   // file-change audit record (#41)
```

`AnswerQuestion` mirrors `Approve`/`Reject`: the supervisor drops it off the
inbound fan-out (core never routes it) and the `ask_user` executor consumes it
(§8, [ADR-0027](adr/0027-ask-user-interactive-prompt.md)).

**Session lifecycle** (✅ #21, [ADR-0028](adr/0028-session-lifecycle-enumeration-and-backpressure.md)).
`ListSessions` and `CloseSession` are **supervisor-global**: the supervisor
answers/acts on them directly rather than routing to a session task.
`ListSessions` returns one `SessionList` snapshot of the live
`SessionInfo{session,parent?,profile,root}` set — a reconnecting head enumerates
in one round-trip instead of folding the whole broadcast; its `session` field is
a correlation id the reply echoes. `CloseSession` drops the session's command
channel so its task exits and emits `SessionEnded` — the explicit destroy `Stop`
(cancel-semantics, ADR-0017) does not perform. Session ids are single-use: after
`SessionEnded`, mint a fresh `SessionId::new_uuid()` rather than reuse a closed
id (which would restart `seq` at 0). The supervisor routes to sessions with a
non-blocking `try_send` + bounded retry, shedding to a saturated session rather
than parking its single loop and stalling every other session.

- **Session-multiplexed** like the `agent` reference's `task_id`: one connection
  routes many sessions by `SessionId`.
- **Monotonic `seq`** on content events so a head can dedupe against replayed
  history (`agent`'s pattern); lifecycle frames (`Status`, `AgentChanged`)
  carry no `seq`.

## 3. Agent profiles + permissions (opencode-style) — [ADR-0003](adr/0003-agent-and-permission-profiles.md)

A session runs under exactly one [`AgentProfile`][profile]:
`{ name, mode, system_prompt, model?, permission }`.

- Switch with `InMsg::SetAgent { agent }`; engine emits `AgentChanged`.
- [`PermissionProfile`][perm] resolves `Allow | Ask | Deny` per tool
  (last-matching-rule-wins, `*` wildcard), **in the runtime tool executor** (✅ #59):
  - `Allow` → run the tool, reply `ToolResult` → core emits `ToolOutput`.
  - `Ask` → emit `ToolRequest`, park at `WaitingApproval` until `Approve`/`Reject`;
    on approve, run the tool and reply `ToolResult`; on reject, reply
    `ToolResult("…rejected…")`.
  - `Deny` → reply `ToolResult("…denied…")` without running the tool.
- Built-ins: `build` (all allow), `plan` (ask, read allow), `explore` (deny,
  read/glob/grep allow). Add your own via `ProfileRegistry::insert`.
- **Where dispatch runs (✅ #59):** the `AgentProfile` *shape* stays a core
  protocol type, but the `Allow|Ask|Deny` decision + the approval wait are a
  **runtime** concern ([ADR-0003](adr/0003-agent-and-permission-profiles.md) /
  [ADR-0010](adr/0010-single-head-crate-and-bash-opt-in.md)). Core emits
  `ToolExec` for *every* host tool and parks on `ToolResult` (§8); it never reads
  `PermissionProfile`. The runtime `tool_runner` (§8) tracks each session's active
  profile (folded from `SessionStarted`/`AgentChanged` against a `ProfileRegistry`
  copy it holds), resolves the permission, and — for `Ask` — emits the
  `ToolRequest` prompt and awaits `Approve`/`Reject`/`Stop` off the engine's
  **inbound fan-out** (`Holly::subscribe_inbound()`), so every head stays a thin
  protocol adapter (it just sends the same frames; the runtime, not core, acts on
  them).

## 4. Structured outputs (orthogonal to profiles) — [ADR-0004](adr/0004-structured-plan-and-task-events.md)

Two artifacts the engine owns and re-emits as **full snapshots** on every change
(the `agent`/`design` "snapshot on change" pattern — idempotent, trivial to
render/dedupe):

- **Plan** — markdown strategy prose (`OutEvent::Plan`).
- **TaskList** — statusful outline of `TaskItem { id, content, status }`
  (`OutEvent::TaskList`, `TaskStatus = pending|in_progress|completed|cancelled`).

Both are written two ways:
1. A **built-in engine tool** the model calls — `update_plan` (input = markdown)
   and `update_tasks` (input = JSON array). These bypass permissions (they only
   mutate session state) and never need approval.
2. A **harness message** — `InMsg::SetPlan` / `InMsg::SetTasks` (user edits).

This is why `entanglement` has *both* the opencode agent-profile axis *and* structured
events: profiles control **what the agent is instructed/permitted to do**;
structured events give every head a native plan/task panel to render.

## 5. Per-session engine (`session.rs`)

Each session is a lazily-spawned tokio task owning: `Context` (message history +
token estimate), an `LlmSession` handle (from `EngineConfig::llm_factory`), the
active `AgentProfile`, the `TaskList`, the `Plan`, and a per-session `seq`.
The `LlmSession` is a **provider-owned session/connection handle**
([ADR-0007](adr/0007-streaming-llm-and-provider-crate.md)): the *conversation
history* stays in core's `Context`, but the *connection* state (pool, retry,
rate-limit budget) belongs to the provider. The factory hands core a pooled
session handle that wraps the streaming backend.

Turn loop: send `LlmRequest { system, model, messages, tools }` → consume the
streamed `LlmEvent`s (emit `TextDelta` per `Text` chunk, gather `ToolCall`s,
note `Finish`) → for each tool call, run built-ins inline or hand host tools to
the runtime (emit `ToolExec`, park on `ToolResult`) → loop until the model
returns no tool calls → `Done`. Permission dispatch and approval no longer run
here — the runtime tool executor owns them (§3, §8, ✅ #59). The tool-result
wait parks the task on its inbox; any non-matching message (e.g. a new prompt) is
stashed and processed after the turn. Setup/mid-stream backend errors surface as
`Error` + `Done` without committing a partial assistant message. The same
stash discipline applies inside the streaming loop and between tool calls
(ADR-0018): mid-turn `try_recv` polls route `Stop` to interrupt and push every
other queued command (`Prompt`, `SetAgent`, …) onto the replay stash, so a
follow-up sent while the engine is busy is never silently dropped.

**Stop is cancel-semantics, not destroy** (ADR-0017). `InMsg::Stop` interrupts
the in-flight turn (the streaming loop and tool dispatch poll `try_recv` for
it; the tool-result wait returns cancelled) but does *not* evict the
session from the supervisor map or end its task. The session's `Context` is
preserved across a Stop+Prompt round-trip — Esc-in-approval or a stray Stop
between turns no longer causes amnesia. The supervisor map entry is only
removed on global inbox close (engine shutdown).

**Sub-agent spawn** (✅ #60, [ADR-0022](adr/0022-subagent-spawn.md), builds on the
[ADR-0021](adr/0021-hierarchical-session-model.md) tree). The model calls a
runtime-owned `agent_spawn { agent, prompt }` tool (renamed from `spawn_agent`,
✅ #120, [ADR-0033](adr/0033-agent-tool-family-and-blocking-agent.md)). The
runtime executor
intercepts it (bypassing per-tool approval, like core's built-ins), mints a
child `SessionId`, and sends `InMsg::Spawn { session: child, parent, agent,
prompt }`. The **supervisor** records `parent_links[child] = parent` and starts
the child `session_loop` under the requested profile with the prompt queued — so
the child's `SessionStarted` carries the parent link and the tree-walk helpers
(`children_of` / `root_of`) reflect reality. Spawn is **non-blocking** (✅ #89,
[ADR-0026](adr/0026-async-subagent-spawn-and-poll.md), supersedes ADR-0022's
synchronous relay): `agent_spawn` replies to the parent *immediately* with the
child handle (`agent_id`) instead of parking the turn on the child's `Done`, so
one turn can launch several sub-agents that then run concurrently. The launch
task keeps watching the child and records its final answer + duration into a
shared `AgentRegistry` (`runtime::agent_poll`) keyed by the handle. The parent
collects a result with a second runtime-owned tool, `agent_poll { agent_id,
timeout_secs }` — also intercepted before permission resolution (it starts no
session and touches no host resource): it blocks up to `timeout_secs` for that
child and returns its answer (with elapsed time) as the tool `ToolOutput`, or a
still-running status on timeout so the model can poll again or do other work.
For the single-delegation case, a third tool `agent { agent, prompt }` (✅ #120,
[ADR-0033](adr/0033-agent-tool-family-and-blocking-agent.md)) **blocks**: it runs
the exact `agent_spawn` launch path (same guard, clamp, `Spawn`), then parks on
the child's `Done` and folds its answer directly into the `ToolOutput` — one call
instead of launch-then-poll. It still records into the `AgentRegistry`, so a
parent `Stop` while parked leaves the child collectable via `agent_poll`.
Refusals (depth, budget, capability) are identical across `agent` and
`agent_spawn` — one shared guard path.
All three reuse the #58 round-trip, so core's turn loop needs no notion of a
"child session". The runtime executor bounds the spawn
tree (✅ #76, [ADR-0023](adr/0023-subagent-spawn-limits.md)): a `SpawnGuard`
folds parent links from `SessionStarted` and, before each spawn, refuses past a
depth cap (`MAX_SPAWN_DEPTH`) or a cumulative per-root budget
(`MAX_SPAWNS_PER_ROOT`) — replying with a clear refusal `ToolOutput` instead of
starting a child. Spawn is also **permission-gated** (✅ #77,
[ADR-0024](adr/0024-subagent-permission-gating.md), `runtime::permission`): a
`Subagent`-mode leaf profile (read-only `explore`) is refused the spawn
capability outright, and every child's per-tool permission is clamped to the
least-privileged rule across its whole ancestor chain (`Deny < Ask < Allow`), so
a child can never touch the shared tree in ways a parent couldn't. Filesystem
isolation (a separate child root) and bidirectional session-to-session messaging
are still deferred (see ADR-0022/0024).

**Ask-user prompt** (✅ #90, [ADR-0027](adr/0027-ask-user-interactive-prompt.md)).
The model calls a runtime-owned `ask_user { question, options, allow_free_form }`
tool. The runtime executor (`ask_user.rs`) intercepts it on `ToolExec` — before
permission resolution, like `agent_spawn` — emits a dedicated
`OutEvent::UserQuestion` and parks at `WaitingApproval`. The head renders the labelled choices
Claude-style (the TUI adds a `PendingQuestion` interaction state alongside
`ApprovalMode`, with an "Other" entry that opens free-text input) and replies
`InMsg::AnswerQuestion { request_id, answer }`. Like `Approve`/`Reject`, the
supervisor drops it off the inbound fan-out and the executor consumes it, then
folds the answer (the picked label or typed text, verbatim) back as the
`ask_user` `ToolOutput` — reusing the #58 round-trip, so core needs no new turn
logic. A `Stop` while pending unwinds silently (core cancels the turn). The
non-interactive `run` head auto-answers (first option, else a canned note) so it
never parks; `pipe` forwards the question and accepts the answer as-is.

## 5b. LLM I/O (`entanglement-provider`) — [ADR-0007](adr/0007-streaming-llm-and-provider-crate.md)

The `Llm` **trait** lives in `entanglement-core` (the seam); all LLM I/O lives in
**`entanglement-provider`**, a separate crate that *may* depend on transport
crates (`reqwest`) — `entanglement-core` may not.

```rust
enum LlmEvent {
    Text(String),
    Reasoning(String),   // thinking/reasoning tokens, streamed distinctly
    ToolCall(ToolCall),
    Finish { input_tokens?, output_tokens? },
}
trait Llm: Send { async fn stream(req) -> Result<BoxStream<'static, Result<LlmEvent>>> }
```

- Streaming mirrors opencode (Vercel AI SDK `doStream`): live token-by-token
  deltas, not a buffered whole-reply. The box stream is `'static`.
- **`LlmEvent::Reasoning`** surfaces extended-thinking output (Anthropic
  `thinking`/`redacted_thinking` blocks, OpenAI `reasoning_content`) instead of
  dropping it; core re-emits it as a reasoning `OutEvent` heads render distinctly
  from answer text.

**Provider topology** — split by *wire format*, not by vendor:

| client (`entanglement-provider`) | wire format | serves | auth |
| --- | --- | --- | --- |
| `OpenAiLlm` (`openai.rs`) | `/chat/completions` SSE | **z.ai** (GLM, entanglement's primary), **OpenAI**, **Ollama** `/v1` | `Bearer` or none (Ollama) |
| `AnthropicLlm` (`anthropic.rs`) | `/v1/messages` SSE | Anthropic | `x-api-key` |

- `OpenAiLlm` is one generic client `{ base_url, api_key: Option, default_model }`
  hand-rolled over `reqwest` (no SDK crate). Preset base constants
  (`ZAI_CODING_PLAN_BASE`, `ZAI_GENERAL_BASE`, `OPENAI_BASE`, `OLLAMA_BASE`) still
  exist, but the *default* base per provider now comes from the catalog (below);
  `openai_factory(base, key, model)` builds an `LlmFactory`.
- `AnthropicLlm` is separate because Anthropic's format genuinely differs (system
  top-level, tool results merged into one user turn, `input_json_delta`
  fragments). `anthropic_factory(key, model)`.
- `ToolSpec.schema` surfaces as `input_schema` (Anthropic) / `parameters`
  (OpenAI-compat); `Message.tool_call_id` → `tool_use_id` / `tool_call_id`.

**Resilience the provider layer owns:** a shared, tuned connection **pool**
(reused across sessions, not a client-per-turn); **retry** with exponential
backoff + jitter on transient failures and dropped streams; **rate-limit**
handling (HTTP 429 + `Retry-After`, plus a client-side RPM throttle, surfaced as
status not silent stalls).

**Provider/model catalog (`entanglement-provider::catalog`, #118,
[ADR-0032](adr/0032-yaml-provider-model-catalog.md)):** the
provider + model list is **YAML, not code** — an embedded default
(`src/defaults.yml`, `include_str!`) deep-merged with an optional user override at
`${config_dir}/entanglement/providers.yml` (override the path via
`ENTANGLEMENT_PROVIDERS_FILE`). The merge runs at the `serde_yaml::Value` level
*before* deserializing, so field-level override falls out for free: `providers`
merge by `name`, `models` by `id`, mappings recurse, other scalars/sequences are
replaced; the final `Catalog` deserialize is `deny_unknown_fields` (typos are
loud). A `wire: openai | anthropic` tag on each provider is what makes
user-defined providers work with **zero code change** — any OpenAI-compatible
endpoint (proxy, local vLLM, new vendor) is `wire: openai` + `base_url` +
`key_env`. `ModelEntry` carries capability flags (`supports_thinking`,
`supports_temperature`, `default_temperature`) and **pricing** (USD/M tokens:
`input`/`output`/`cached_input`/`cache_write`, all optional). Lookups:
`Catalog::{builtin,load,load_from}`, `provider(name)`, `model(provider,id)`,
`model_by_id(id)`.

**Provider selection (`skutter`):** the catalog loads once at startup; a
malformed user file is a loud error, never a silent fallback. `ENTANGLEMENT_PROVIDER=<name>`
looks `<name>` up **in the catalog** (so custom providers work; `echo` stays a
built-in stub), erroring loudly if its key env is missing; if unset, auto-detect
by iterating catalog order and picking the first provider whose `key_env` is set
and non-empty (keyless Ollama is skipped) — preserving z.ai → OpenAI → Anthropic;
else `EchoLlm`. Precedence overall is **env > user YAML > embedded defaults**.
Per-provider env still wins: `<PROV>_API_KEY` (name from the entry's `key_env`),
`<PROV>_MODEL`, `<PROV>_BASE`/`<PROV>_API_BASE`. Default models come from each
provider's `default_model` (`glm-5.2` / `gpt-4o` / `llama3.1` /
`claude-sonnet-4-5`). The TUI model picker + context bar read the same catalog.

## 6. Heads — ADRs [0005](adr/0005-ndjson-stdio-head.md) (stdio), 0001 (ABI), [0010](adr/0010-single-head-crate-and-bash-opt-in.md) (packaging), [0011](adr/0011-tui-head-ratatui-crossterm.md)–[0015](adr/0015-rich-text-pipeline-syntect.md) (TUI)

All heads live in one crate, **`entanglement-runtime`** (✅ #56; binary
`skutter`), as subcommands. The "four interfaces"
(in-process ABI + three transports) are a design concept, not a packaging
boundary — the real seam is `entanglement-core` ↔ everything else (ADR-0006,
ADR-0010).

The heads (and the `skutter` binary that carries them) need the crate's
**default features** — `default = ["tui"]` pulls clap + the providers + the
render stack, and `[[bin]] skutter` declares `required-features = ["cli","tui"]`.
Building the crate with `default-features = false` yields an **embeddable
library** — the tool-execution loop, permission dispatch, sub-agent spawn, and
persistence machinery with none of the CLI/TUI/transport weight
([ADR-0025](adr/0025-runtime-cargo-feature-gates.md), §7).

- **ABI** — `holly.send()` / `holly.subscribe()`. Done.
- **stdio** (`skutter run` / `skutter pipe`): one-shot `run [--format text|json]
  [--agent <name>] [--session <id> | --resume <id>]`; bidirectional `pipe` NDJSON
  (`InMsg` in, `OutEvent` out). `skutter sessions` lists past root sessions for
  the cwd (see §6b).
- **WebSocket** (`skutter serve`, _next_): axum `GET /ws`, in-band auth first
  frame, stateless handler, one `subscribe()` per socket, inbound frame →
  `InMsg` → `send()`, 30s ping, `continue` on `broadcast::Lagged`. (Recipe
  lifted from `agent`.)
- **TUI** (`skutter tui`): opencode-style terminal UI over `subscribe()`. Uses
  ratatui + crossterm (ADR-0011), leader-key bindings with which-key popup
  (ADR-0013), inline tool approval cards (ADR-0014), and rich markdown
  rendering with pulldown-cmark + syntect (ADR-0015). Event buffering and
  multiplexed-session rendering follow ADR-0012. Mouse capture is on by default
  (opt out with `ENTANGLEMENT_TUI_NO_MOUSE=1`, which restores native text
  selection): the wheel scrolls the chat (or the open modal's selection), and a
  left click hit-tests the chat area to toggle a transcript block — reasoning
  runs render collapsed as a `▸ Thinking (N lines)` header, expanded on click
  (or via the leader `t` key). **Attention signals** (issue #14, `tui::attention`):
  a `Status` transition into `WaitingApproval`, `Done`, or `Error` rings the
  terminal bell — and, opt-in via `ENTANGLEMENT_TUI_NOTIFY=1`, emits an OSC 9
  desktop notification (iTerm2/kitty/WezTerm; silently dropped elsewhere). Core
  emits `Status` only on a state change, so signalling on those states *is*
  signalling on the transitions; `Done`/`Error` also arrive as their own
  `OutEvent` variants but only `Status` is watched, so a turn end rings once.
  Focus reporting (crossterm `EnableFocusChange`) mutes signals while the
  terminal is focused, but best-effort only — terminals that never report focus
  always signal. **External editor + export** (✅ #13,
  [ADR-0029](adr/0029-external-editor-and-markdown-export.md), `tui::editor` +
  `tui::export`): `<leader>e` / `/editor` suspends the TUI and opens `$EDITOR`
  (`$VISUAL`→`$EDITOR`→`vi`) on the input draft, reading the result back into the
  input box; `<leader>E` / `/export` writes the transcript to
  `<session>-<unix_secs>.md` and opens it. Both defer through a `UiEffect` on
  `App` that the event loop (terminal owner) runs, restoring the alternate screen
  symmetrically; an editor failure is logged, not fatal. **`@file` mentions +
  `!bash` passthrough** (✅ #15,
  [ADR-0030](adr/0030-tui-file-mentions-and-bash-passthrough.md), `tui::mention`):
  typing `@` opens a fuzzy file-completion popup over a startup snapshot of the
  working dir (`host::list_files`, minus `target`/`node_modules`/… trees);
  Tab/Enter inserts the pick as `@path` prompt text (the model reads it via the
  `read` tool — no content pre-expansion). An input starting with `!` is a
  head-side shell escape: the command runs through the existing `BashTool` and its
  output is injected into the transcript as a `!bash` tool call/output pair, local
  only (never sent to the engine). Gated on `ENTANGLEMENT_ENABLE_BASH=1`, the same
  opt-in as the model-facing `bash` tool (ADR-0010).

## 6b. Session persistence & resume (`persistence` + `session_store`)

Sessions are event-sourced to disk, one JSONL file per **root** session under
`<data_dir>/entanglement/sessions/<safe-cwd>/<root_id>.jsonl` (`session_store`).
`spawn_persistence_subscriber` (`persistence`) taps **both** directions of the
ABI — `holly.subscribe()` for `OutEvent`s and `holly.subscribe_inbound()` for
`InMsg`s — and appends each frame as a `LogRecord { ts, session, payload }` where
`payload` is `LogPayload::In(InMsg) | Out(OutEvent) | Gap { dropped }` (the last
is a tombstone, below). Logging inbound messages is
what makes a session resumable: `Session::replay` reconstructs user turns from
the logged `InMsg::Prompt` records, so without them a resumed context holds only
assistant/tool messages and the model appears to forget the conversation.

- **Inbound is biased ahead of outbound** so a prompt lands on disk before the
  events it produces (`pair_records` pairs each `Out` with the preceding `In`).
  `InMsg::Resume` is skipped (it carries the whole prior log → recursion/bloat)
  and `InMsg::Spawn` is skipped (a child's turns are already captured in the
  root's file via out events; logging the spawn would create a stray child root).
- **Spawned children fold into the root file** via a `roots` map built from
  `SessionStarted { root, parent }`, so each root file is a self-contained,
  replayable record of the whole session tree.
- **Resume** reads the file, `pair_records` builds the `(Option<InMsg>, OutEvent)`
  stream, and `Holly::resume` seeds a session from `Session::replay`. The CLI
  exposes `skutter run --resume <id>` and `skutter sessions` (lists past root
  sessions for the cwd); the TUI `/resume` modal restores the full visible
  transcript (`restore_from_records`) *and* reseeds engine context.
- **One-shot flush**: a `run` invocation ends the moment the turn does, so `main`
  aborts the tool executor and drops its `Holly` handle to close the broadcast
  channels, then awaits the persistence task so buffered events reach disk before
  the process exits.
- **Log integrity — never resume a hole** (#104). The persistence tap reads
  Holly's *lossy* broadcast, so a fast turn that outruns disk appends can drop a
  contiguous run of events (`RecvError::Lagged`) — a well-formed file whose
  history is silently incomplete. On lag the tap writes a `Gap { dropped }`
  tombstone into every known root file (a lag can't say *which* session lost
  records, so all are marked); `integrity_gap` detects it and both resume paths
  (`skutter run --resume`, the TUI modal) **refuse** rather than fold an
  incomplete context. `session_store::read` likewise distinguishes a
  crash-truncated *tail* line (tolerated with a warning) from *interior*
  corruption (a hole → hard error), and `list_sessions` skips-and-warns per bad
  file instead of aborting the whole enumeration.

## 7. Hygiene gates — [ADR-0006](adr/0006-core-dependency-hygiene-gate.md) (`tree`), [ADR-0025](adr/0025-runtime-cargo-feature-gates.md) (`check-lean`)

`entanglement-core` must stay free of UI/transport deps. Enforced by
`make tree`, which runs `cargo tree -p entanglement-core` and **fails** if any of
`clap`/`axum`/`tower`/`tonic`/`crossterm`/`ratatui`/`reqwest`/`hyper` appear. It
is part of `make verify`. Current core deps: `tokio`, `serde`, `serde_json`,
`async-trait`, `anyhow`, `thiserror`, `tracing`, `futures`, `uuid`. `glob`/`regex`
(which back the host tools, §8) and `diffy` moved out with the host-tool
implementations to `entanglement-runtime` (✅ #57); the `reqwest` both LLM
backends use lives in `entanglement-provider`, not core — see ADR-0007.

A second gate, **`make check-lean`** (ADR-0025), protects the runtime's lean
library surface: it runs `cargo tree -p entanglement-runtime
--no-default-features -e normal` and **fails** if `clap`/`ratatui`/`crossterm`/
`syntect`/`pulldown-cmark`/`diffy`/`reqwest`/`hyper`/`tracing-subscriber` leak
into the no-default-features build, then runs lean `clippy --all-targets` (which
type-checks the lib + the integration tests with the bin auto-skipped via
`required-features` — the load-bearing check). It joins `tree` in `make verify`.

**CI (issue #107).** Both gates now run in GitHub Actions
([`.github/workflows/`](../.github/workflows/)), driven through the same `make`
targets. `ci.yml` runs `make verify` (`check-fmt` + `tree` + `check-lean` +
`lint` + `test`) on every PR and every push to `master` — the first time the
`tree`/`check-lean` hygiene gates run automatically rather than at developer
discretion. `release.yml` fires on a `v*` tag: it runs `make verify` and then a
coverage job, `make coverage` (`cargo llvm-cov --workspace`, fails under
`COV_MIN`% — baselined from the first measured run and ratcheted up, never
lowered), uploading the lcov + Cobertura reports as an artifact so a release is
blocked on green tests with a coverage report attached. Both cache cargo
artifacts (`Swatinem/rust-cache`) and inherit the committed `CARGO_BUILD_JOBS=4`
cap from `.cargo/config.toml`.

## 8. Host tools — [ADR-0008](adr/0008-host-tools-workdir-and-bounded-output.md) (trio), [ADR-0009](adr/0009-edit-and-bash-host-tools.md) (`edit`/`bash`), [ADR-0010](adr/0010-single-head-crate-and-bash-opt-in.md) (`bash` opt-in)

Concrete filesystem + shell tools, dispatched under the active permission
profile ([ADR-0003](adr/0003-agent-and-permission-profiles.md)). Core defines the
`Tool` **trait**; the implementations live in **`entanglement-runtime::host`**
(✅ #57) and are assembled by `host_tools(root: PathBuf) -> ToolRegistry`.
Execution *and* permission dispatch now run in the runtime (✅ #58, #59):
`entanglement-runtime::tool_runner` subscribes to the engine, resolves each
`ToolExec`'s `Allow|Ask|Deny` against the session's active profile (§3), runs the
cleared tool against the registry, and replies with `InMsg::ToolResult`. `Ask`
emits the `ToolRequest` prompt and waits for the head's decision on
`Holly::subscribe_inbound()` (the engine's inbound `InMsg` fan-out). Core only
advertises the tool *schemas* (`EngineConfig.tool_specs`) — it holds no executable
tools and makes no policy decision:

| tool | input | output |
| --- | --- | --- |
| `read` | `{path, offset?, limit?}` | file contents, `{lineno}: {line}`, 1-based, line-ranged |
| `glob` | `{pattern}` | matching paths (relative to root), one per line |
| `grep` | `{pattern, path?}` | matches as `path:lineno:line` over files matched by `path` (default `**/*`) |
| `edit` | `{path, oldString, newString, replaceAll?}` | exact-string replace; empty `oldString` creates (refused if exists → hints `write`); non-unique match errors unless `replaceAll` |
| `write` | `{path, content}` | whole-file create/overwrite; missing parent dirs created; `created <path> (N lines)` / `overwrote <path> (N lines, was M)` — confirmation only, never echoes content (ADR-0031) |
| `bash` ⚠ | `{command, timeout?}` | `sh -c` rooted at root; `[exit N]` + stdout + `[stderr]`; default 120 s timeout, capped at 600, `kill_on_drop` reaps on expiry |

- **Working directory:** each tool holds a `root`; model-supplied paths resolve
  against it and are rejected on `..` escape. Lexical containment only (no
  symlink defense) — ADR-0008. `bash` sets only the **cwd** — it is explicitly
  *not* sandboxed and runs with the engine's full privileges (ADR-0009);
  permission profiles gate whether it runs at all.
- **Bounded output:** 32 KiB byte cap with a truncation notice; `read` defaults
  to 2000 lines; `glob`/`grep` cap at 1000 results. Prevents a huge file/tree
  from blowing the context window.
- **Empty-result contract (ADR-0016):** a host tool may not return a silent
  zero-output when multiple distinguishable underlying states produce it.
  `list_files` returns `FileList { files, matched_dirs, skipped_errors }`;
  per-entry walk errors are `warn!`-logged and counted, not swallowed. When
  `glob`'s result would be empty but the pattern matched something (the common
  bare-`**` trap, which matches only directories), it returns a hint like
  *"`**` matched 7 directories but no files — try `**/*`"* so the model can
  self-correct mechanically. `grep` consumes the same `FileList` but stays
  silent on zero matches (a clean no-match is a single well-defined state).
- **Schema advertisement:** `Tool::schema()` feeds `ToolRegistry::specs()`, so
  the model sees a real `input_schema` per host tool (not an empty object).
- **Wiring (ADR-0010):** `host_tools(root)` registers the **root-contained
  quintet** (`read`/`glob`/`grep`/`edit`/`write`; `write` added in ADR-0031).
  `bash` is opt-in — the `skutter`
  binary registers `BashTool` only when `ENTANGLEMENT_ENABLE_BASH=1`, because
  it runs unsandboxed (ADR-0009). `EngineConfig::default()` ships an empty
  registry (embedders opt in via `host_tools`).

`edit`/`write`/`bash` slot into the existing permission profiles with no profile
changes: `build` auto-allows them (default `Allow`), `plan` asks (default
`Ask`), `explore` denies (default `Deny`). The opt-in gate is
orthogonal to the permission profile: it controls *registration* (whether the
tool is advertised at all), the profile controls *dispatch* (Allow/Ask/Deny
when the model calls it).

Four **runtime-owned orchestration tools** are *not* in the registry — the
`tool_runner` intercepts them on `ToolExec` before permission resolution (they
touch no host resource) and advertises their schemas separately: the `agent_*`
family (§5, ADR-0033) —
`agent_spawn { agent, prompt }` (renamed from `spawn_agent`, ADR-0022), its
non-blocking join `agent_poll { agent_id, timeout_secs }` (ADR-0026), and the
blocking `agent { agent, prompt }` (spawn-and-wait in one call) — plus
`ask_user { question, options, allow_free_form }` (§5, ADR-0027).

[holly]: ../entanglement-core/src/holly.rs
[profile]: ../entanglement-core/src/protocol.rs
[perm]: ../entanglement-core/src/protocol.rs
