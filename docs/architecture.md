# entanglement — Architecture

How the headless engine is structured and how the four interfaces share one
contract. Vision & roadmap live in [`../PLAN.md`](../PLAN.md); overview in
[`../README.md`](../README.md). The *why* behind each choice here is recorded in
the [decision log](adr/README.md) (ADRs); this document describes the current
*what is*.

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
InMsg    = Prompt{session,text} | Approve{session,request_id}
         | Reject{session,request_id,reason?} | Stop{session}
         | SetTasks{session,tasks} | SetPlan{session,content} | SetAgent{session,agent}

OutEvent = Status{session,state}              // point-in-time, no seq
         | AgentChanged{session,agent}        // point-in-time, no seq
         | Plan{session,seq,content}          // markdown prose snapshot
         | TextDelta{session,seq,text}
         | ToolRequest{session,seq,request_id,tool,input}
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
  (last-matching-rule-wins, `*` wildcard):
  - `Allow` → run immediately, emit `ToolOutput`.
  - `Ask` → emit `ToolRequest`, park at `WaitingApproval` until `Approve`/`Reject`.
  - `Deny` → emit `ToolOutput("…denied…")`, never run.
- Built-ins: `build` (all allow), `plan` (ask, read allow), `explore` (deny,
  read/glob/grep allow). Add your own via `ProfileRegistry::insert`.

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
token estimate), its own `Llm` instance (from `EngineConfig::llm_factory`), the
active `AgentProfile`, the `TaskList`, the `Plan`, and a per-session `seq`.

Turn loop: send `LlmRequest { system, model, messages, tools }` → consume the
streamed `LlmEvent`s (emit `TextDelta` per `Text` chunk, gather `ToolCall`s,
note `Finish`) → for each tool call, dispatch by built-in vs host-tool-vs-
permission → loop until the model returns no tool calls → `Done`. Approval waits
park the task on its inbox; any non-matching message (e.g. a new prompt) is
stashed and processed after the turn. Setup/mid-stream backend errors surface as
`Error` + `Done` without committing a partial assistant message.

## 5b. Model backends (`entanglement-llm`) — [ADR-0007](adr/0007-streaming-llm-and-provider-crate.md)

The `Llm` **trait** lives in `entanglement-core` (the seam); concrete backends live in
**`entanglement-llm`**, a separate crate that *may* depend on transport crates
(`reqwest`) — `entanglement-core` may not.

```rust
enum LlmEvent { Text(String), ToolCall(ToolCall), Finish { input_tokens?, output_tokens? } }
trait Llm: Send { async fn stream(req) -> Result<BoxStream<'static, Result<LlmEvent>>> }
```

- Streaming mirrors opencode (Vercel AI SDK `doStream`): live token-by-token
  deltas, not a buffered whole-reply. The box stream is `'static`.

**Provider topology mirrors opencode / the AI SDK** — split by *wire format*,
not by vendor:

| client (`entanglement-llm`) | wire format | serves | auth |
| --- | --- | --- | --- |
| `OpenAiLlm` (`openai.rs`) | `/chat/completions` SSE | **z.ai** (GLM, entanglement's primary), **OpenAI**, **Ollama** `/v1` | `Bearer` or none (Ollama) |
| `AnthropicLlm` (`anthropic.rs`) | `/v1/messages` SSE | Anthropic | `x-api-key` |

- `OpenAiLlm` is one generic client `{ base_url, api_key: Option, default_model }`
  hand-rolled over `reqwest` (no SDK crate). The three OpenAI-shape providers
  differ only by config, so preset base constants exist (`ZAI_CODING_PLAN_BASE`
  — entanglement default, `ZAI_GENERAL_BASE`, `OPENAI_BASE`, `OLLAMA_BASE`).
  `openai_factory(base, key, model)` builds an `LlmFactory`. Tool calls stream as
  per-index `function.arguments` deltas, flushed on `finish_reason: "tool_calls"`;
  tool results round-trip as one `role: "tool"` message each.
- `AnthropicLlm` is separate because Anthropic's format genuinely differs: system
  is a top-level field; tool results are merged into one user turn; tool input
  arrives as `input_json_delta` fragments. `anthropic_factory(key, model)`.
- `ToolSpec.schema` surfaces as `input_schema` (Anthropic) / `parameters`
  (OpenAI-compat); `Message.tool_call_id` surfaces as `tool_use_id` (Anthropic) /
  `tool_call_id` (OpenAI-compat).

**Provider selection (`skutter`):** `ENTANGLEMENT_PROVIDER` env selects
`zai | openai | ollama | anthropic` explicitly (errors loudly if the matching key
is missing); if unset, auto-detect by key presence with z.ai first, then OpenAI,
then Anthropic; else `DummyLlm`. Per-provider env: `<PROV>_API_KEY` (z.ai/OpenAI/
Anthropic; Ollama is keyless), `<PROV>_MODEL`, `<PROV>_BASE`/`<PROV>_API_BASE`.
Default models: `glm-5.2` / `gpt-4o` / `llama3.1` / `claude-sonnet-4-5`.

## 6. Heads — ADRs [0005](adr/0005-ndjson-stdio-head.md) (stdio), 0001 (ABI)

- **ABI** — `holly.send()` / `holly.subscribe()`. Done.
- **stdio** (`skutter`): `skutter run [--format text|json] [--agent <name>]`
  one-shot; `skutter pipe` bidirectional NDJSON (`InMsg` in, `OutEvent` out).
- **WebSocket** _(next)_: axum `GET /ws`, in-band auth first frame, stateless
  handler, one `subscribe()` per socket, inbound frame → `InMsg` → `send()`,
  30s ping, `continue` on `broadcast::Lagged`. (Recipe lifted from `agent`.)
- **TUI** _(next)_: opencode-style terminal UI over `subscribe()`.

## 7. Hygiene gate — [ADR-0006](adr/0006-core-dependency-hygiene-gate.md)

`entanglement-core` must stay free of UI/transport deps. Enforced by
`make tree`, which runs `cargo tree -p entanglement-core` and **fails** if any of
`clap`/`axum`/`tower`/`tonic`/`crossterm`/`ratatui`/`reqwest`/`hyper` appear. It
is part of `make verify`. Current core deps: `tokio`, `serde`, `serde_json`,
`async-trait`, `anyhow`, `thiserror`, `tracing`, `futures`. The `reqwest` both
backends use lives in `entanglement-llm`, not core — see ADR-0007.

[holly]: ../entanglement-core/src/holly.rs
[profile]: ../entanglement-core/src/protocol.rs
[perm]: ../entanglement-core/src/protocol.rs
