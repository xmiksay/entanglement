# 0067. MCP client is a runtime-side tool provider

- Status: Accepted
- Date: 2026-07-14
- Builds on the `Tool` trait + `ToolRegistry` living in the runtime ([0059](0059-tool-trait-and-registry-live-in-the-runtime.md)/#206) and the layered user config ([0047](0047-local-trust-boundary.md)/#172); part of the embedding-gap audit epic #196 (#198).

## Context

`grep -i mcp` across all crates returned zero hits. For a headless *engine* whose
selling point is embedding, the missing MCP client — no way to attach an external
tool server — was a major extensibility gap (#198): every capability the agent had
was one the runtime shipped. The Model Context Protocol is the de-facto standard
for exposing tools to an LLM host, so speaking it lets a user bolt on any of the
existing MCP server ecosystem (filesystem, git, databases, SaaS APIs) with zero
code change.

The seam already existed. Since #206 the runtime owns the `Tool` trait and the
`ToolRegistry`; `build_config` fills the registry with the host quintet and
derives `EngineConfig.tool_specs` from `registry.specs()`, and `tool_runner`
answers the `ToolExec → ToolResult` round-trip against that same registry. Core
holds no executable tools and makes no policy call — it only advertises schemas
and round-trips each call back to the runtime.

That means an external tool and a host tool are the *same shape* to everything
above the registry: a `dyn Tool` with a name, a description, and an
`inputSchema`. Nothing in the protocol, the engine turn loop, or the permission
model needs to know a tool's execution happens in another process.

## Decision

**Add an MCP client as a runtime-side tool provider: spawn each configured server,
discover its `tools/list`, and register every tool into the same `ToolRegistry` as
a `dyn Tool`. No core change.**

- **Transport** (`mcp::client::McpClient`) — one JSON-RPC 2.0 session per server
  over the server's **stdio**, newline-delimited framing (the MCP stdio
  transport). A background reader task demultiplexes responses to their callers
  by JSON-RPC `id`; notifications are ignored. The handshake is `initialize` +
  `notifications/initialized`, then `tools/list` / `tools/call`. Requests carry a
  60s timeout so a hung server surfaces a tool-failure result instead of parking
  a turn forever, and the reader drains all pending requests with an error on EOF
  so a crashed server can't hang a caller. The subprocess is held for the client's
  lifetime with `kill_on_drop`, so keeping the registered tools alive keeps the
  server alive — no separate handle to retain.
- **Proxy** (`mcp::tool::McpTool`) — adapts one remote tool to the `Tool` trait.
  Its `schema()` returns the server's `inputSchema` verbatim, so the model sees
  the real arguments; its `run()` JSON-decodes the model's input to the `arguments`
  object, calls `tools/call`, and flattens the result's text content blocks (v1 is
  text-only; a non-text block is noted). An `isError` result is prefixed so the
  model reads it as a failure. The advertised name is
  **`mcp__<server>__<tool>`**, sanitized to the providers' `^[A-Za-z0-9_-]+$`
  tool-name rule, so it can never collide with a host tool or another server's
  tool.
- **Config** — servers are declared in the layered user config's `mcp:` section
  (`{config_dir}/entanglement/config.yml` < `.entanglement/config.yml`), a map of
  server name → `{command, args, env, disabled}`, `deny_unknown_fields`-validated
  by the same loader as `permissions`/`hooks`. Empty by default (a no-op).
- **Wiring** — `build_config` becomes `async` and calls `mcp::connect` after the
  host tools are registered but before `tool_specs` is derived, so MCP tools flow
  into both the advertised schemas and the executor's registry with the existing
  code. Connection is **best-effort per server**: a spawn/handshake/`tools/list`
  failure is logged and skipped, never fatal — an external dependency being down
  must not stop the engine from starting.

Because an MCP tool is just a registry entry, it is governed by the **same
permission profiles** as any host tool (it takes the generic `Intercept::Permission`
route in `tool_runner`), and it participates in the `FileChange`/approval/hook
machinery exactly like `read` or `bash`.

The whole feature lives in the **lean library** (`mcp` module): tokio's process
support + `serde_json` are already lean deps, so an embedder building
`--no-default-features` gets external tool servers with no CLI/TUI/transport
dependency.

## Alternatives rejected

- **A new core "tool source" abstraction.** Rejected: the `ToolRegistry` already
  *is* the tool-source abstraction ([0059](0059-tool-trait-and-registry-live-in-the-runtime.md)).
  An MCP tool is a `dyn Tool`; adding a parallel seam in core would duplicate the
  registry and drag process-spawning across the `provider ← core ← runtime` seam.
- **HTTP/SSE transport first.** Rejected for v1: stdio is the canonical MCP
  transport, needs no network config or auth model, and covers the local
  server-subprocess case the embedding gap is about. An HTTP transport can be a
  second `McpClient` constructor later without touching the proxy or the wiring.
- **Multiplexing MCP tools behind a single `mcp` dispatcher tool.** Rejected:
  hiding N tools behind one call would strip their individual schemas from the
  model and break per-tool permission rules. Registering each tool by its own
  namespaced name keeps schema fidelity and the existing permission model.
- **Inlining MCP image/resource content into multimodal results.** Deferred: the
  `ContentPart` image path exists ([0065](0065-read-emits-image-content-blocks.md)/#221),
  but v1 keeps MCP results text-only to bound scope; the proxy notes a non-text
  block so the omission is visible, and `run_content` can grow the image path later.
- **A blocking connect that fails startup on a bad server.** Rejected: external
  servers are unreliable by nature; a down server must degrade to "that tool is
  absent," not "the agent won't start."
