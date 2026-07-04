# brain — Architecture

How the headless engine is structured and how the four interfaces share one
contract. Vision & roadmap live in [`../PLAN.md`](../PLAN.md); overview in
[`../README.md`](../README.md). The *why* behind each choice here is recorded in
the [decision log](adr/README.md) (ADRs); this document describes the current
*what is*.

## 1. The actor model (the ABI) — [ADR-0001](adr/0001-actor-model-abi.md)

`brain-core` exposes one engine, [`Brain`][brain], as an async actor:

```
                       ┌──────────── brain-core ────────────┐
  ABI (direct) ───────►│  inbox   mpsc<Sender<InMsg>>        │
  stdio (NDJSON) ─────►│  ────────────────────────► engine   │
  WebSocket ──────────►│  outbox  broadcast<Sender<OutEvent>│────► subscribe()
  TUI ────────────────►│  (seq'd, session-multiplexed)       │
                       └────────────────────────────────────┘
```

- `brain.send(InMsg)` — push a typed message in (zero serialization).
- `brain.subscribe()` — get a `broadcast::Receiver<OutEvent>` (fan-out to N
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

This is why `brain` has *both* the opencode agent-profile axis *and* structured
events: profiles control **what the agent is instructed/permitted to do**;
structured events give every head a native plan/task panel to render.

## 5. Per-session engine (`session.rs`)

Each session is a lazily-spawned tokio task owning: `Context` (message history +
token estimate), its own `Llm` instance (from `EngineConfig::llm_factory`), the
active `AgentProfile`, the `TaskList`, the `Plan`, and a per-session `seq`.

Turn loop: send `LlmRequest { system, messages, tools }` → stream `TextDelta` →
for each tool call, dispatch by built-in vs host-tool-vs-permission → loop until
the model returns no tool calls → `Done`. Approval waits park the task on its
inbox; any non-matching message (e.g. a new prompt) is stashed and processed
after the turn.

## 6. Heads — ADRs [0005](adr/0005-ndjson-stdio-head.md) (stdio), 0001 (ABI)

- **ABI** — `brain.send()` / `brain.subscribe()`. Done.
- **stdio** (`brain-stdio`): `brain run [--format text|json] [--agent <name>]`
  one-shot; `brain pipe` bidirectional NDJSON (`InMsg` in, `OutEvent` out).
- **WebSocket** _(next)_: axum `GET /ws`, in-band auth first frame, stateless
  handler, one `subscribe()` per socket, inbound frame → `InMsg` → `send()`,
  30s ping, `continue` on `broadcast::Lagged`. (Recipe lifted from `agent`.)
- **TUI** _(next)_: opencode-style terminal UI over `subscribe()`.

## 7. Hygiene gate — [ADR-0006](adr/0006-core-dependency-hygiene-gate.md)

`brain-core` must stay free of UI/transport deps. Enforced by
`make tree` (`cargo tree -p brain-core` — must show no `clap`/`axum`/`crossterm`/
`tonic`). Current core deps: `tokio`, `serde`, `serde_json`, `async-trait`,
`anyhow`, `thiserror`, `tracing`, `futures`.

[brain]: ../brain-core/src/brain.rs
[profile]: ../brain-core/src/protocol.rs
[perm]: ../brain-core/src/protocol.rs
