# 0080. MCP streamable-HTTP transport with per-server headers/auth

- Status: Accepted
- Date: 2026-07-15
- Extends the stdio MCP client ([0067](0067-mcp-client-as-runtime-tool-provider.md)/#198); rides the feature-gate discipline of [0025](0025-runtime-cargo-feature-gates.md) and the local trust boundary of [0047](0047-local-trust-boundary.md); part of the multi-tenant embedding epic #307 (#312).

## Context

The MCP client ([0067](0067-mcp-client-as-runtime-tool-provider.md)) speaks
**stdio-subprocess only** (`McpServerConfig { command, args, env }`). But remote
MCP servers — every claude.ai-style integration, and the site's own per-user
servers — use the **streamable-HTTP transport**: JSON-RPC `POST`ed to one endpoint,
answered with either a lone JSON body or an SSE stream, authenticated with request
headers. The site currently keeps a parallel rmcp-based HTTP pool inside its
embedder code; a first-class HTTP transport here retires that duplicate and lets a
multi-tenant embedder ([0076](0076-per-session-dynamic-tool-specs.md)) assemble
per-user tool registries against per-user tokens.

The seam already fits. `McpTool` ([0067](0067-mcp-client-as-runtime-tool-provider.md))
holds an `Arc<McpClient>` and only ever calls `list_tools`/`call_tool`; it does not
care *how* the bytes move. Discovery, permission, the `ToolExec`/`ToolResult`
round-trip, and `mcp__<server>__<tool>` naming are all transport-agnostic already.

The one tension is dependencies: HTTP needs `reqwest`, and the stdio client
deliberately lives in the **lean library** (tokio process + `serde_json`, no
CLI/TUI/transport dep — [0025](0025-runtime-cargo-feature-gates.md)) so an embedder
gets external tools without pulling a web stack.

## Decision

**Make `McpClient` an enum over two transports — `Stdio` (#198) and `Http` (new) —
chosen per server by a `command` XOR `url` config. `McpTool` is unchanged.** The
HTTP transport is behind a new **`mcp-http`** cargo feature (`reqwest` + `futures`),
in `default`, keeping the lean/`--no-default-features` build transport-free.

- **Config (`McpServerConfig`):** one flat block, one transport. `{command, args,
  env}` (stdio) **XOR** `{url, headers}` (HTTP), plus a shared `disabled`.
  `McpServerConfig::transport() -> Result<Transport>` resolves the choice and
  **rejects both-set or neither-set** as a config error (best-effort per server, so
  the error is logged-and-skipped, never fatal). `deny_unknown_fields` unchanged.
- **`mcp::http::HttpClient`:** each request is a discrete `reqwest` `POST` carrying
  `Accept: application/json, text/event-stream`. A `text/event-stream` response is
  drained (SSE `data:` events, blank-line-framed) until the event whose JSON-RPC
  `id` matches the request; a lone JSON body is decoded directly. The `initialize`
  → `notifications/initialized` handshake and the 60 s per-request timeout mirror
  the stdio client; the JSON-RPC result/error split is shared
  (`client::jsonrpc_payload`, `client::parse_tool_def`).
- **Auth + session:** static per-server `headers` authenticate every request, with
  `${VAR}` expanded from the process environment so a token is never written into
  the config file in the clear. An `Mcp-Session-Id` handed back on `initialize` is
  echoed on every later request, as is the negotiated `MCP-Protocol-Version`.
- **Embedder surface:** `HttpClient` (and `StdioClient`, `McpClient`, `Transport`)
  are **public**. An embedder building tools programmatically — per-tenant servers
  with per-user tokens — calls `HttpClient::connect(name, url, headers)`, wraps it
  `McpClient::Http`, and registers `McpTool::new(...)`, bypassing the YAML path.
- **Trust:** enabling a server *is* consent, per [0047](0047-local-trust-boundary.md)
  (the config file is trusted). The HTTP transport adds no new permission surface —
  every remote tool still round-trips through the same profiles as `read`/`bash`.

The internal split is `mcp::stdio` (the former `client.rs` body, renamed
`StdioClient`, now returning `Self` so the enum owns the `Arc`), `mcp::http`
(new, gated), and a slimmed `mcp::client` holding the `McpClient` enum + the shared
JSON-RPC helpers — keeping every file under the 400-line cap.

## Consequences

- Remote MCP servers (`POST /mcp` with `Authorization`) work end-to-end:
  `tools/list` over SSE, `tools/call` over JSON, session-id round-trip — covered by
  a live axum integration test (`tests/mcp_http.rs`). An unreachable HTTP server is
  logged and skipped; the engine still starts.
- stdio servers are byte-for-byte unaffected (same handshake, framing, timeout,
  EOF-drain). `make check-lean` stays green: `reqwest`/`futures` ride `mcp-http`,
  so the lean library carries no *direct* HTTP dep.
- A build compiled without `mcp-http` that is handed a `url:` server logs a clear
  "compiled without the `mcp-http` feature" skip rather than failing to build.

## Rejected alternatives

- **A `transport:` tag / serde-untagged enum** instead of `command` XOR `url`. The
  issue specifies the bare-field form; a flat block with an explicit XOR resolver
  gives the clearest config error ("sets both", "sets neither") and keeps
  `deny_unknown_fields` simple.
- **`reqwest` unconditionally (no feature).** It is already in the lean *tree*
  transitively via core→provider, but adding it as a **direct** runtime dep would
  put a web stack in the lean library's own surface — against
  [0025](0025-runtime-cargo-feature-gates.md). A gate keeps the opt-out real.
- **A generic async `Transport` trait object** rather than an enum. Two known
  transports, dispatched in two methods — an enum is the KISS choice; a trait is
  premature abstraction (extract on the third transport).
- **A shared connection pool like the provider's** ([0050](0050-per-endpoint-connection-pool-retry-rate-limit.md)).
  MCP calls are low-rate tool invocations, not a hot LLM stream; `reqwest`'s own
  per-client pooling suffices. Revisit if rate-limit/backoff becomes a real need.
