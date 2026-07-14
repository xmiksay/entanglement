# entanglement Architecture — Layers & the actor model (ABI)

> Part of the [architecture overview](../architecture.md). The *why* behind each choice is in the [decision log](../adr/README.md).

## 0. Layers: core / provider / runtime — [ADR-0006](../adr/0006-core-dependency-hygiene-gate.md), [ADR-0053](../adr/0053-invert-core-provider-seam.md)

Three crates, two seams. Dependency direction is `provider (leaf) ← core ←
runtime`: **provider** is a leaf crate (no `entanglement-*` deps), **core**
depends on provider, and the head depends on both. Heads depend on core; core
never depends on a head. The core↔provider seam was inverted in
[ADR-0053](../adr/0053-invert-core-provider-seam.md) (superseding the
dependency-direction decisions of [ADR-0006](../adr/0006-core-dependency-hygiene-gate.md)
and [ADR-0007](../adr/0007-streaming-llm-and-provider-crate.md)): provider now
**owns** the LLM ABI and core consumes it.

```
┌──────────── entanglement-runtime (head, binary `skutter`) ─────────────┐
│ user sessions · `Tool` trait + `ToolRegistry` · host tools · tool exec │
│ permission dispatch · approval UX · persistence · transports           │
│ (stdio ✅, TUI ✅, WS 🚧) · selects the concrete provider · glues core   │
└─────────▲──────────────────────────────────────────────▲───────────────┘
          │ send()/subscribe() (ABI)      tool exec + approval (protocol)
┌─────────┴──────────────── entanglement-core (engine) ───┴───────────────┐
│ Holly actor · InMsg/OutEvent · agent turn loop · Context · tool schemas │
│ drives `dyn Llm` from the turn loop · re-exports the provider ABI       │
└─────────┬────────────────────────────────────────────────────────────────┘
          │ depends on provider; consumes the `Llm` trait + DTOs
 ┌─────────▼──────────── entanglement-provider (leaf, LLM ABI + I/O) ────────┐
│ OWNS the `Llm` trait, LlmRequest/Response/Event/Stream, LlmSession,       │
│ LlmFactory, ToolCall/ToolSpec, Message/MessageRole · OpenAI-compat +      │
│ Anthropic clients · pool · retry · rate-limit · reasoning stream          │
└────────────────────────────────────────────────────────────────────────────┘
```

- **core** — the reasoning engine: actor, protocol, turn loop, `Context` (the
  rolling conversation history, built on provider's `Message`), `EngineConfig`.
  Advertises tool *schemas* (`ToolSpec`) only — the `Tool` trait + `ToolRegistry`
  are runtime vocabulary ([ADR-0059](../adr/0059-tool-trait-and-registry-live-in-the-runtime.md), #206).
  Depends on provider and drives `dyn Llm`
  from the turn loop; re-exports the provider ABI (`pub use
  entanglement_provider::{Llm, Message, ToolSpec, …}`) for its heads. No UI/web
  deps, but **no longer transport-free** — `reqwest`/`hyper`/`tower` ride in
  transitively via provider (§7).
- **provider** — a **leaf crate** (no `entanglement-*` deps) that owns the LLM
  ABI *and* all LLM I/O behind the `Llm` trait (§5b). Usable standalone for raw
  LLM queries with no engine.
- **runtime** — the head: the `Tool` trait + `ToolRegistry`
  ([ADR-0059](../adr/0059-tool-trait-and-registry-live-in-the-runtime.md), #206),
  host tools + their execution, permission dispatch +
  approval, user sessions, every transport (§6, §8). Feature-gated
  ([ADR-0025](../adr/0025-runtime-cargo-feature-gates.md)): `default = ["tui"]` is
  the full `skutter` binary, while `--no-default-features` is a **lean library**
  — `host` + `tool_runner` + `permission` + `subagent` + `persistence` +
  `session_store` (+ `config` + `inspect`, #208) over core + tokio + glob/regex,
  with no CLI/TUI deps (`make check-lean` enforces, §7). Since
  [ADR-0053](../adr/0053-invert-core-provider-seam.md)
  the lean library is CLI/TUI-free rather than transport-free — `reqwest` rides
  in transitively via core → provider. The `cli` feature (clap + log init) and
  the `provider` feature (the LLM providers, split out of `cli` in #208) sit
  between the two, leaving room for a `ws = ["provider", …]` sibling that pulls
  providers without clap. `main.rs` now imports the library modules from the lib
  crate instead of re-declaring `mod`, so only the bin heads (`pipe`/`run`/`tui`)
  live in the binary (#208).

**Responsibility relocation is mostly landed:** the host-tool *implementations*
now live in `entanglement-runtime` (✅ #57, §8) — as does the `Tool` trait +
`ToolRegistry` that types them (✅ #206, [ADR-0059](../adr/0059-tool-trait-and-registry-live-in-the-runtime.md)),
so core carries no tool vocabulary beyond the advertised `ToolSpec`. Tool
*execution* moved there too — core emits `OutEvent::ToolExec` and the runtime answers with
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
