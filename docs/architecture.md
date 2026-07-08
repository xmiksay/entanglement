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
  approval, user sessions, every transport (§6, §8).

**Responsibility relocation is mostly landed:** the host-tool *implementations*
now live in `entanglement-runtime` (✅ #57, §8), and tool *execution* moved there
too — core emits `OutEvent::ToolExec` and the runtime answers with
`InMsg::ToolResult` (✅ #58, §3, §8). *Permission dispatch* (the `Allow|Ask|Deny`
decision + approval wait) also moved to the runtime (✅ #59, §3): core emits
`ToolExec` for *every* host tool and no longer consults `PermissionProfile`; the
runtime tool executor resolves the permission and drives approval. What remains
is slimming core's `Session` to loop + turn state (#61).

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
         | Stop{session}
         | SetTasks{session,tasks} | SetPlan{session,content} | SetAgent{session,agent}
         | Spawn{session,parent,agent,prompt}   // start a child session (sub-agent) (#60)

OutEvent = Status{session,state}              // point-in-time, no seq
         | AgentChanged{session,agent}        // point-in-time, no seq
         | Plan{session,seq,content}          // markdown prose snapshot
         | TextDelta{session,seq,text}
         | ToolRequest{session,seq,request_id,tool,input}   // Ask prompt, from runtime (#59)
         | ToolExec{session,seq,request_id,tool,input}      // core → runtime: dispatch it (#58/#59)
         | ToolOutput{session,seq,request_id,output}
         | TaskList{session,seq,tasks}        // full outline snapshot
         | Error{session,seq,message}
         | Done{session,seq}
```

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
runtime-owned `spawn_agent { agent, prompt }` tool. The runtime executor
intercepts it (bypassing the permission profile, like core's built-ins), mints a
child `SessionId`, and sends `InMsg::Spawn { session: child, parent, agent,
prompt }`. The **supervisor** records `parent_links[child] = parent` and starts
the child `session_loop` under the requested profile with the prompt queued — so
the child's `SessionStarted` carries the parent link and the tree-walk helpers
(`children_of` / `root_of`) reflect reality. The runtime watches the child's
events and, on the child's `Done`, relays its final answer back to the parent as
the `spawn_agent` `ToolOutput` — reusing the #58 tool round-trip, so core's turn
loop needs no notion of a "child session". Isolation, recursion limits, and
bidirectional session-to-session messaging are deferred (see ADR-0022).

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
  (`ZAI_CODING_PLAN_BASE` — default, `ZAI_GENERAL_BASE`, `OPENAI_BASE`,
  `OLLAMA_BASE`); `openai_factory(base, key, model)` builds an `LlmFactory`.
- `AnthropicLlm` is separate because Anthropic's format genuinely differs (system
  top-level, tool results merged into one user turn, `input_json_delta`
  fragments). `anthropic_factory(key, model)`.
- `ToolSpec.schema` surfaces as `input_schema` (Anthropic) / `parameters`
  (OpenAI-compat); `Message.tool_call_id` → `tool_use_id` / `tool_call_id`.

**Resilience the provider layer owns:** a shared, tuned connection **pool**
(reused across sessions, not a client-per-turn); **retry** with exponential
backoff + jitter on transient failures and dropped streams; **rate-limit**
handling (HTTP 429 + `Retry-After`, plus a client-side RPM throttle, surfaced as
status not silent stalls); a **models-per-provider** registry so heads present a
real model picker.

**Provider selection (`skutter`):** `ENTANGLEMENT_PROVIDER` env selects
`zai | openai | ollama | anthropic` explicitly (errors loudly if the matching key
is missing); if unset, auto-detect by key presence with z.ai first, then OpenAI,
then Anthropic; else `DummyLlm`. Per-provider env: `<PROV>_API_KEY` (z.ai/OpenAI/
Anthropic; Ollama is keyless), `<PROV>_MODEL`, `<PROV>_BASE`/`<PROV>_API_BASE`.
Default models: `glm-5.2` / `gpt-4o` / `llama3.1` / `claude-sonnet-4-5`.

## 6. Heads — ADRs [0005](adr/0005-ndjson-stdio-head.md) (stdio), 0001 (ABI), [0010](adr/0010-single-head-crate-and-bash-opt-in.md) (packaging), [0011](adr/0011-tui-head-ratatui-crossterm.md)–[0015](adr/0015-rich-text-pipeline-syntect.md) (TUI)

All heads live in one crate, **`entanglement-runtime`** (✅ #56; binary
`skutter`), as subcommands. The "four interfaces"
(in-process ABI + three transports) are a design concept, not a packaging
boundary — the real seam is `entanglement-core` ↔ everything else (ADR-0006,
ADR-0010).

- **ABI** — `holly.send()` / `holly.subscribe()`. Done.
- **stdio** (`skutter run` / `skutter pipe`): one-shot `run [--format text|json]
  [--agent <name>]`; bidirectional `pipe` NDJSON (`InMsg` in, `OutEvent` out).
- **WebSocket** (`skutter serve`, _next_): axum `GET /ws`, in-band auth first
  frame, stateless handler, one `subscribe()` per socket, inbound frame →
  `InMsg` → `send()`, 30s ping, `continue` on `broadcast::Lagged`. (Recipe
  lifted from `agent`.)
- **TUI** (`skutter tui`): opencode-style terminal UI over `subscribe()`. Uses
  ratatui + crossterm (ADR-0011), leader-key bindings with which-key popup
  (ADR-0013), inline tool approval cards (ADR-0014), and rich markdown
  rendering with pulldown-cmark + syntect (ADR-0015). Event buffering and
  multiplexed-session rendering follow ADR-0012.

## 7. Hygiene gate — [ADR-0006](adr/0006-core-dependency-hygiene-gate.md)

`entanglement-core` must stay free of UI/transport deps. Enforced by
`make tree`, which runs `cargo tree -p entanglement-core` and **fails** if any of
`clap`/`axum`/`tower`/`tonic`/`crossterm`/`ratatui`/`reqwest`/`hyper` appear. It
is part of `make verify`. Current core deps: `tokio`, `serde`, `serde_json`,
`async-trait`, `anyhow`, `thiserror`, `tracing`, `futures`, `uuid`. `glob`/`regex`
(which back the host tools, §8) and `diffy` moved out with the host-tool
implementations to `entanglement-runtime` (✅ #57); the `reqwest` both LLM
backends use lives in `entanglement-provider`, not core — see ADR-0007.

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
| `edit` | `{path, oldString, newString, replaceAll?}` | exact-string replace; empty `oldString` creates (refused if exists); non-unique match errors unless `replaceAll` |
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
  quartet** (`read`/`glob`/`grep`/`edit`). `bash` is opt-in — the `skutter`
  binary registers `BashTool` only when `ENTANGLEMENT_ENABLE_BASH=1`, because
  it runs unsandboxed (ADR-0009). `EngineConfig::default()` ships an empty
  registry (embedders opt in via `host_tools`).

`edit`/`bash` slot into the existing permission profiles with no profile
changes: `build` auto-allows both (default `Allow`), `plan` asks for both
(default `Ask`), `explore` denies both (default `Deny`). The opt-in gate is
orthogonal to the permission profile: it controls *registration* (whether the
tool is advertised at all), the profile controls *dispatch* (Allow/Ask/Deny
when the model calls it).

[holly]: ../entanglement-core/src/holly.rs
[profile]: ../entanglement-core/src/protocol.rs
[perm]: ../entanglement-core/src/protocol.rs
