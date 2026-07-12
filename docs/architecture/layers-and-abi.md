# entanglement Architecture — Layers & the actor model (ABI)

> Part of the [architecture overview](../architecture.md). The *why* behind each choice is in the [decision log](../adr/README.md).

## 0. Layers: core / provider / runtime — [ADR-0006](../adr/0006-core-dependency-hygiene-gate.md)

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
  ([ADR-0025](../adr/0025-runtime-cargo-feature-gates.md)): `default = ["tui"]` is
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

## 1. The actor model (the ABI) — [ADR-0001](../adr/0001-actor-model-abi.md)

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
