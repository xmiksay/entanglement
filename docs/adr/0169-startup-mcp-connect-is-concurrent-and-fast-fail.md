# 0169. Startup MCP connect is concurrent, and the startup handshake fails fast

- Status: Accepted
- Date: 2026-08-04
- Amends: [ADR-0157](0157-mcp-http-transport-shares-the-endpoint-pool.md) — narrows
  its "no special-cased shorter fuse" decision to exclude the one-time startup
  handshake; every other MCP HTTP call (`tools/list`, `tools/call`, and the
  handshake on a live `/mcp add`/`/mcp connect`/`mcp_enable`) is unaffected and
  still rides the pool's patient default exactly as ADR-0157 decided. Issue #660.

## Context

`entanglement-runtime::mcp::connect` (the startup connect, `main.rs` before any
head becomes interactive) walked its configured servers in a plain sequential
`for` loop, `.await`ing each one to completion before starting the next. Each
HTTP-transport server's `initialize` handshake rode the shared endpoint pool's
`HttpClient::execute_with_retry` — the same `RetryConfig` default LLM traffic
uses (ADR-0111): 5 attempts, backoff geometric from 200ms up to 30s. A server
refusing the connection outright (not running, wrong port) still burned the
full ladder — observed at ~10s wall-clock for one dead server before it was
skipped. With the sequential loop, N unreachable servers costs N times that,
all of it ahead of the head starting.

The retry ladder's patience is right for an in-turn LLM call, where retrying
through a transient blip is worth the wait. It's wrong for a startup probe of
a server that might simply not be running: startup only needs "reachable or
not," fast, and a server that fails here stays reachable via a later
`/mcp connect` or `mcp_enable`, which retains the patient ladder for its
explicit, user-initiated retry.

ADR-0157 deliberately routed MCP HTTP traffic through the same pool as LLM
traffic with "no special-cased shorter fuse," to keep RPM/concurrency/429
handling consistent for provider-bundled servers sharing a provider's key
budget. This ADR does not reverse that: the RPM/concurrency/429 admission
path (`EndpointState`, the AIMD pacing gate, the cross-process shared lease)
is untouched. What changes is narrower — the number of *failure-path*
attempts and the backoff schedule for exactly one call (`initialize` +
`notifications/initialized`) on exactly one connect path (startup).

## Decision

**`HttpClient::execute_with_retry` gains a per-call `retry: Option<RetryConfig>`
override.** `None` (every LLM client, and every MCP call except the startup
handshake) behaves exactly as before — the pool's own `RetryConfig` at
construction time. `Some(config)` swaps only the failure-path knobs
(`max_attempts`, `initial_backoff`, `max_backoff`, `response_header_timeout`)
for that one call; the endpoint lookup, RPM limiter, concurrency semaphore,
and cross-process shared lease are keyed off `endpoint`/`model`, not `config`,
so they're identical either way — a caller with a different patience budget
still rides the same admission gates as everyone else on that endpoint.

**`McpHttpClient::connect`/`connect_authenticated`/`connect_with` gain a
`handshake_retry: Option<RetryConfig>` parameter**, forwarded only into the
`initialize`/`notifications/initialized` calls inside `handshake` — `list_tools`
and `call_tool` always pass `None`. The parameter threads down through
`entanglement-runtime`'s `McpClient::connect`/`connect_with_auth` and
`mcp::connect_client` (the shared two-await helper every connect path funnels
through: startup, `mcp_add`, `mcp_reconnect`, `enable_for_session`) — only the
startup `connect()` passes `Some(...)`; the other three call sites pass `None`
and keep the patient default unchanged.

**The startup fast-fail budget** (`connect.rs::startup_handshake_retry`): 2
attempts, a short fixed 150ms backoff, and a 3s `response_header_timeout`
(down from the pool default's 120s, bounding the "accepted the connection but
never answered" case ADR-0157 otherwise left open-ended). A connection-refused
server now costs on the order of the one fixed backoff, not the LLM ladder's
~10s.

**Startup's per-server connect+handshake+`tools/list` now runs concurrently**
via `tokio::task::JoinSet`, replacing the sequential `for` loop. Each spawned
task only does the network I/O (`connect_client`, no registry access); the
main task drains completions with `join_next()` and does the synchronous
`register_tools` registry mutation itself, one result at a time — `registry`
is never touched from more than one place, so no new locking is needed. A
spawned task's panic is logged and skipped (`tracing::error!`), not
propagated — consistent with the existing "one broken server can't stop
startup" contract; nothing here should ever panic, but startup surviving a
bug in one server's connect path is exactly the failure mode this whole
function exists to contain.

## Consequences

### Positive

- One unreachable HTTP MCP server now costs a fraction of a second at startup
  instead of ~10s; N unreachable servers no longer serialize, since they
  connect concurrently with everything else.
- The RPM/concurrency/429 admission path — the actual subject of ADR-0157 — is
  untouched. A provider-bundled server's startup handshake still counts
  against its provider's shared budget exactly as before.
- The override mechanism is generic (`execute_with_retry`'s new parameter),
  not MCP-specific plumbing bolted on top — a future caller with a similarly
  different patience budget can reuse it without another signature change to
  the retry loop itself.

### Negative / neutral

- `McpHttpClient::connect`/`connect_authenticated` and
  `entanglement-runtime::mcp::client::McpClient::connect`/`connect_with_auth`
  gain a parameter — a breaking signature change for an out-of-tree embedder
  building a client directly (mirrors ADR-0157's own precedent; every in-tree
  call site is updated in this change).
- The fast-fail budget is a fixed constant, not configurable per server today
  — a server that's merely slow (not down) at the exact moment of startup
  gets the same short fuse as one that's actually dead, and only recovers via
  a later `/mcp connect`/`mcp_enable`. Considered acceptable: the failure mode
  is "the tool is unavailable until reconnected," not data loss or a stuck
  turn, and startup responsiveness matters more than catching every
  momentarily-slow server on the first try.
- Backgrounding the *whole* startup connect (starting heads before any MCP
  tool registers, per the original issue's option 3) is not part of this
  change — it would touch the roster-snapshot ordering in `main.rs` (tools
  registered before the `tool_specs` snapshot) for a larger, separately-scoped
  win. Concurrency + fast-fail alone already cut the common case (a handful of
  servers, at most one or two down) from potentially tens of seconds to
  near-instant.

## References

- Issue #660: runtime startup blocks ~10s per unreachable MCP server
- [ADR-0157](0157-mcp-http-transport-shares-the-endpoint-pool.md): the pool
  and "no special-cased shorter fuse" decision this ADR narrows
- [ADR-0111](0111-adaptive-endpoint-pacing-and-429-retry-until-clear.md): the
  LLM-tuned retry ladder (`RetryConfig` defaults) this ADR opts the startup
  handshake out of
- [ADR-0152](0152-provider-bundled-mcp-servers-three-state-enablement.md): the
  `mcp_enable` late-registration path a startup-skipped server can still reach
