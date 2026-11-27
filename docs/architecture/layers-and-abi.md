# entanglement Architecture — Layers & the actor model (ABI)

> Part of the [architecture overview](../architecture.md). The *why* behind each choice is in the [decision log](../adr/README.md).

## 0. Layers: core / provider / runtime / skutter — [ADR-0006](../adr/archive/0006-core-dependency-hygiene-gate.md), [ADR-0053](../adr/archive/0053-invert-core-provider-seam.md)

Four crates, three seams. Dependency direction is `provider (leaf) ← core ←
runtime ← skutter`: **provider** is a leaf crate (no `entanglement-*` deps),
**core** depends on provider, the **runtime** library on both, and the
**skutter** CLI on all three. Heads depend on core; core never depends on a
head. The core↔provider seam was inverted in
[ADR-0053](../adr/archive/0053-invert-core-provider-seam.md) (superseding the
dependency-direction decisions of [ADR-0006](../adr/archive/0006-core-dependency-hygiene-gate.md)
and [ADR-0007](../adr/archive/0007-streaming-llm-and-provider-crate.md)): provider now
**owns** the LLM ABI and core consumes it.

```
┌──────────────── skutter (CLI, binary `skutter`) ────────────────────────┐
│ heads: stdio `run`/`pipe` · TUI · WebSocket `serve` · slash commands    │
│ `inspect` · `sessions` · `config set-key` · provider selection · wiring │
└─────────▲───────────────────────────────────────────────────────────────┘
          │ public API only
┌─────────┴──────────── entanglement-runtime (library) ──────────────────┐
│ user sessions · `Tool` trait + `ToolRegistry` · host tools · tool exec │
│ permission dispatch · approval · persistence · config · watcher        │
└─────────▲──────────────────────────────────────────────▲───────────────┘
          │ send()/subscribe() (ABI)      tool exec + approval (protocol)
┌─────────┴──────────────── entanglement-core (engine) ───┴───────────────┐
│ Holly actor · InMsg/OutEvent · agent turn loop · Context · tool schemas │
│ drives `dyn Llm` from the turn loop · re-exports the provider ABI       │
└─────────┬────────────────────────────────────────────────────────────────┘
          │ depends on provider; consumes the `Llm` trait + DTOs
 ┌─────────▼──────────── entanglement-provider (leaf, LLM ABI + I/O) ────────┐
│ OWNS the `Llm` trait, LlmRequest/Response/Event/Stream, LlmFactory,       │
│ ToolCall/ToolSpec, Message/MessageRole · OpenAI-compat + Anthropic +      │
│ native Gemini clients (#309) · pool · retry · rate-limit · reasoning      │
│ stream · MCP client mechanism + OAuth stack (ADR-0153)                    │
└────────────────────────────────────────────────────────────────────────────┘
```

- **core** — the reasoning engine: actor, protocol, turn loop, `Context` (the
  rolling conversation history, built on provider's `Message`), `EngineConfig`.
  Advertises tool *schemas* (`ToolSpec`) only — the `Tool` trait + `ToolRegistry`
  are runtime vocabulary ([ADR-0059](../adr/archive/0059-tool-trait-and-registry-live-in-the-runtime.md), #206).
  Depends on provider and drives `dyn Llm`
  from the turn loop; re-exports the provider ABI (`pub use
  entanglement_provider::{Llm, Message, ToolSpec, …}`) for its heads. No UI/web
  deps, but **no longer transport-free** — `reqwest`/`hyper`/`tower` ride in
  transitively via provider (§7).
- **provider** — a **leaf crate** (no `entanglement-*` deps) that owns the LLM
  ABI *and* all LLM I/O behind the `Llm` trait (§5b). Usable standalone for raw
  LLM queries with no engine.
- **runtime** — the embeddable library: the `Tool` trait + `ToolRegistry`
  ([ADR-0059](../adr/archive/0059-tool-trait-and-registry-live-in-the-runtime.md), #206),
  tool execution, permission dispatch + approval, sub-agents, sessions +
  persistence (§8). It ships no host tool and keeps no filesystem-root logic
  (#968), reads no config or env file and resolves no user directory: the
  embedder hands it plain values (#967). No binary (#931). Features
  ([ADR-0025](../adr/archive/0025-runtime-cargo-feature-gates.md), changed by
  #931, #968): `default = ["provider", "mcp-http", "rhai"]`;
  `--no-default-features` drops the provider-backed aux-LLM helpers, the HTTP
  MCP transport and `rhai`. Neither build pulls a CLI, TUI or
  web-server crate (`make check-lean` gates both, §7); `reqwest` rides in
  transitively via core → provider.
- **skutter** — the coding CLI (#931, `publish = false`): every head (`run`/
  `pipe`/`tui`/`serve`), the host tools, their containment, the escape-root
  check, `/cd` and the bubblewrap sandbox (`host`, #968), user slash
  commands, lifecycle hooks (#964), the
  file-backed grants/MCP-credential/MCP-tool-name/escape-root stores and the
  `mcp:` write-back behind the runtime's in-memory seams (#966), the layered
  `config.yml`, the managed pin files, the definitions watcher and the key
  writer (`config`, `watch`, #967), `inspect`, the log setup and `config
  set-key`, built on the runtime's public API only. Its library (`host`,
  `serve`, `slash_commands`, `hooks`, `state`, `config`, `watch`, `inspect`,
  `logging`, `tool_vocab`) is
  what its integration tests drive; the binary holds the heads (`pipe`/`run`/`tui`, dispatched by
  `heads`), the clap surface (`args`) and the startup wiring (`providers`/
  `factories` → `engine_setup` → `boot` → `services`, #737).

**Responsibility relocation is mostly landed:** the host-tool *implementations*
now live in `entanglement-runtime` (✅ #57, §8) — as does the `Tool` trait +
`ToolRegistry` that types them (✅ #206, [ADR-0059](../adr/archive/0059-tool-trait-and-registry-live-in-the-runtime.md)),
so core carries no tool vocabulary beyond the advertised `ToolSpec`. Tool
*execution* moved there too — core emits `OutEvent::ToolExec` and the runtime answers with
`InMsg::ToolResult` (✅ #58, §3, §8). *Permission dispatch* (the `Allow|Ask|Deny`
decision + approval wait) also moved to the runtime (✅ #59, §3): core emits
`ToolExec` for *every* host tool and no longer consults `PermissionProfile`; the
runtime tool executor resolves the permission and drives approval. Core's
`Session` is now slimmed to loop + turn state (✅ #61): it holds the `Context`,
the LLM backend (`llm: Box<dyn Llm>`, #55; no per-session handle — [ADR-0062](../adr/archive/0062-collapse-llmsession-placeholder-newtype.md)), the profile, the plan/tasks snapshots,
and the loop counters — no cached tool set (the schemas come from
`EngineConfig.tool_specs` at turn time).

## 1. The actor model (the ABI) — [ADR-0001](../adr/archive/0001-actor-model-abi.md)

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
- `holly.shutdown().await` — stop every session and the supervisor and close
  both broadcasts (subscribers see `RecvError::Closed`) **however many `Holly`
  clones are still alive** (#700): a handle holds the broadcast senders only
  through a shared, takeable slot, so a stray clone (a detached task, a live
  `serve` socket) can no longer keep the outbox open. Idempotent; afterwards
  `send` fails and a fresh `subscribe()` is already closed. Dropping every
  clone without calling it still stops the engine (the inbox closes). `skutter`
  calls it at teardown, before the bounded persistence drain (#545).

This **is** the ABI. The other three heads are adapters that translate their
wire format to/from `InMsg`/`OutEvent`. Adding a head never touches the engine.
