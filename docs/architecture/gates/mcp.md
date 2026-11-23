# Gates & host tools §10 — MCP client: transports, OAuth, scopes & proxy

> Part of the [gates & host tools module](../gates-and-host-tools.md) of the [architecture overview](../../architecture.md).

## 10. MCP client — external tool servers — [ADR-0067](../../adr/0067-mcp-client-as-runtime-tool-provider.md) (#198)

Attach any external [MCP](https://modelcontextprotocol.io) tool server as a
**runtime-side tool provider**, with **no core change**. Since the `Tool` trait +
`ToolRegistry` live in the runtime (§ADR-0059), an external tool is the same shape
as a host tool: a `dyn Tool` with a name, description, and `inputSchema`. The MCP
client (`entanglement-runtime::mcp`) connects each server on first use,
discovers its tools, and registers them into the same registry — so they ride
the `ToolExec`/`ToolResult` round-trip (execution) unchanged, graded by the
same permission modes as `read`/`bash`. Their schemas are **never advertised**
([ADR-0226](../../adr/0226-one-advertised-surface-mcp-envelope-registered-is-available.md)): the model lists servers with `list_mcp`, reads one server's catalog with
`mcp_tools`, and calls through the `mcp(server, tool, args)` envelope.

- **Transport (`mcp::client::McpClient`):** an enum over two concrete transports,
  chosen per server by the `command` XOR `url` config (§ADR-0080/#312). `McpTool`
  holds an `Arc<McpClient>` and only calls `list_tools`/`call_tool`, so it adapts
  whichever backs a server. Both share the handshake (`initialize` +
  `notifications/initialized`) then `tools/list` (discovery) / `tools/call`
  (execution) and the JSON-RPC result/error split (`client::jsonrpc_payload`).
  Discovery is **paginated** (#727, `list_tools_paginated`): `nextCursor` is
  followed until absent/empty, capped at 100 pages so a server whose cursor
  loops fails the connect with a clear error instead of hanging it. A
  **server-initiated request** is answered rather than dropped
  (`server_request_reply`): `ping` gets the spec's empty result, anything else
  `-32601`; and a message carrying a `method` is never taken for a response,
  since each side numbers its ids independently and a server `ping` may reuse
  an id the client has in flight. No client-side keepalive ping is sent.
  stdio keeps a flat **60 s** per-request timeout so a hung subprocess can't
  park a turn; HTTP's per-request bound is now the shared endpoint pool's own
  ([config & live](mcp-config-and-live.md), [ADR-0157](../../adr/0157-mcp-http-transport-shares-the-endpoint-pool.md)).
  - **stdio (`mcp::stdio::StdioClient`, #198):** one JSON-RPC 2.0 session over the
    spawned subprocess's stdio, newline-delimited frames. A background reader task
    demultiplexes responses to callers by JSON-RPC `id`; notifications are dropped,
    a server request (`ping`) is answered on stdin from its own task (so a full
    stdin pipe can never stall the stdout drain), and on EOF the reader **drains all pending requests with an error** so a crashed
    server can't hang a caller. The subprocess is held for the client's lifetime
    (`kill_on_drop`); keeping the registered tools alive keeps the server alive.
    The child env is the inherited environment **minus the provider API-key
    vars** (`catalog.key_envs()`, the same #164 scrub `bash`/`call` apply —
    #472, [ADR-0124](../../adr/0124-wire-refused-mcp-mutation-and-stdio-key-scrub.md));
    an explicit per-server `env:` entry naming a key still wins, since writing
    it into the server's own config block is deliberate consent.
    Lives in the **lean library** (tokio process + `serde_json` only).
  - **streamable HTTP (`mcp::http::HttpClient`, #312, behind the `mcp-http`
    feature):** a remote server over `POST <url>` — the streamable-HTTP transport.
    Each request is a discrete `POST` with `Accept: application/json,
    text/event-stream`; the server answers with a lone JSON body **or** an SSE
    stream (drained until the event whose JSON-RPC `id` matches; a `ping`
    request met on that stream is answered with its own `POST` that
    deliberately bypasses the endpoint pool — the drain still holds the
    endpoint's concurrency permit, so a pooled reply could deadlock under a cap
    of 1 — best-effort, 10 s bound, failures only logged). Static per-server
    `headers` (e.g. `Authorization: Bearer …`) authenticate every request, with
    `${VAR}` expanded from the environment so a token stays out of the config file;
    the flip side of that expansion is a documented, accepted leak surface
    (§ADR-0080/[ADR-0128](../../adr/0128-mcp-http-var-header-expansion-leak-surface.md)):
    `expand_env` resolves `${VAR}` against the engine's whole process
    environment with no allowlist, so a header naming a provider secret sends
    that secret's live value to the configured server — consent, not a bug,
    since the config file is trusted and enabling a server is consent
    ([ADR-0047](../../adr/0047-local-trust-boundary.md)), same as the stdio
    transport's `env:` block. Any future logging of resolved request headers
    must redact expanded values; none exists today. An `Mcp-Session-Id`
    handed back on `initialize` is echoed on every later request
    (and the negotiated `MCP-Protocol-Version`). Since
    [ADR-0153](../../adr/0153-mcp-server-oauth.md) the transport itself lives in
    **`entanglement-provider::mcp`** (as `McpHttpClient`) and is reached through
    core's re-export, the same path `ProviderMcpServer` takes — mechanism in the leaf
    crate, policy (config, registry, permissions, the token file, the browser
    launch) in the runtime. The runtime therefore names no `reqwest` of its own;
    `mcp-http` is now a pure compile gate deciding whether the HTTP-MCP paths are
    built at all, and `reqwest` rides in via core→provider exactly as ADR-0025's
    lean gate already sanctions. Re-exported as `mcp::HttpClient` under its
    historical name, still **public** so an embedder can build a per-tenant client
    with a per-user token and register its tools without the YAML path.
    Every request rides the **shared per-endpoint pool** (✅ #559,
    [ADR-0157](../../adr/0157-mcp-http-transport-shares-the-endpoint-pool.md)) —
    `connect`/`connect_authenticated` take the caller's `entanglement_provider::
    HttpClient` (re-exported from core alongside `McpHttpClient` so the
    `mcp-http`-only lean build still names no direct provider dependency) and
    an optional `api_key`, and every `POST` goes through
    `HttpClient::execute_with_retry` exactly like the LLM wire clients — the
    same connection pool, RPM/concurrency caps, and 429/`Retry-After` handling,
    keyed by `(this server's own URL, api_key)` so it gets its own bucket,
    isolated from its provider's LLM endpoint. This closes the gap where a
    provider-bundled server sharing its provider's key ([config & live](mcp-config-and-live.md)) issued
    completely unmetered requests against that key's real rate limit. The
    provider key is resolved from `AvailableServer.key_env` ([config & live](mcp-config-and-live.md)) at the
    lazy connect; a live `/mcp add` or an OAuth reconnect passes
    `api_key: None` (still pooled, just unkeyed).
  - **OAuth (✅ [ADR-0153](../../adr/0153-mcp-server-oauth.md)):** a server entry may
    carry an optional `oauth:` block. Present — *even empty* — it switches that
    server from static-header auth to a browser-obtained bearer token, since most
    remote MCP servers are OAuth-protected and issue no pre-registered
    `client_id`. Endpoints come from **RFC 9728** protected-resource metadata (off
    the `401` `WWW-Authenticate: resource_metadata` pointer, else the well-known
    path) chained into **RFC 8414** authorization-server metadata; a client is
    minted on the fly by **RFC 7591** dynamic client registration as a *public*
    client (`token_endpoint_auth_method: none`), so a URL alone suffices. **PKCE
    S256** is mandatory and the only method offered. Every field in the block is an
    override; setting `authorization_url` + `token_url` skips discovery entirely.
    The redirect is caught by a one-request loopback listener hand-rolled on
    `tokio::net` (RFC 8252, bound to `127.0.0.1`, mismatched `state` refused
    outright — never `axum`, which must not reach the leaf crate). Credentials
    live in the managed `mcp-tokens.yml` (`0600`, atomic + `fd-lock`ed like the
    sibling managed files in the managed-state dir, `ENTANGLEMENT_STATE_DIR`) **together
    with the resolved endpoints and client id**, so a later (lazy) connect skips
    discovery/registration entirely — those run only during an explicit
    `/mcp connect`. Tokens refresh on expiry and once more on a `401`; every
    `Debug` impl redacts secrets. Driven by the trusted-only, **wire-refused**
    `InMsg::McpAuth { name, action: Connect|Check|Disconnect }` →
    `OutEvent::McpAuthChanged` (a forged connect would open a browser and mint a
    durable credential; even `Check` mutates state by refreshing), answered by the
    same `mcp::spawn_mcp_responder` that answers `McpList` — each op detached, since
    a connect parks up to five minutes on the browser. Startup **never** opens a
    browser: an unauthenticated OAuth server is skipped non-fatally and reported as
    `needs auth` in `/mcp list`. TUI: `/mcp connect|check|disconnect <name>` plus
    `c`/`t` on the panel's highlighted row; the authorize URL is always rendered as
    transcript content (never a toast) so a headless/SSH session can copy it when
    the browser launch fails. `/mcp disconnect` attempts RFC 7009 revocation when
    advertised, then deletes locally regardless. `/mcp connect` only ever works
    for a server whose deployment actually exposes RFC 9728/8414 discovery — a
    server that issues static bearer tokens has no such endpoints, so the flow
    can never complete no matter what the `oauth:` block contains; `connect`'s
    error says so and points at static `headers:` + the managed `.env` instead
    (#561). **Device-code flow (✅ [ADR-0182](../../adr/0182-mcp-oauth-device-code-flow-and-closed-refresh-race.md),
    #631):** `/mcp connect <name> --device-code` runs RFC 8628 instead — no
    browser, no loopback listener. `DeviceFlow` shares discovery/DCR with the
    browser flow (registration now declares an explicit `grant_types`, e.g.
    `["urn:ietf:params:oauth:grant-type:device_code", "refresh_token"]`, with no
    `redirect_uri`) and polls the token endpoint
    (`grant_type=urn:ietf:params:oauth:grant-type:device_code`) honoring
    `authorization_pending`/`slow_down` until the user finishes elsewhere —
    `McpAuthChanged`'s interim event carries a `user_code` alongside
    `authorize_url` (the plain `verification_uri`) for every head to render. The
    **cross-process refresh race** ADR-0153 accepted for v1 is closed the same
    ADR: `TokenStore::with_exclusive` lets `McpTokenStore` hold its file lock for
    the whole load→refresh→save section instead of just the write, so two
    `skutter` instances can no longer both redeem the same rotating refresh
    token — a losing racer picks up the winner's already-refreshed token instead
    of failing. **Web-redirect flow (✅ [ADR-0187](../../adr/0187-mcp-oauth-web-redirect-flow-for-embedders.md),
    #684):** a server-side web embedder can't use either in-tree flow — the
    loopback listener is CLI-shaped and device-code trades UX for it — so
    `provider::oauth::web::WebFlow` prepares the same authorization-code +
    PKCE request against the *embedder's own* HTTPS callback URI: `begin`
    shares discovery/DCR/PKCE with the browser flow (via the extracted
    `flow::prepare` helper; the DCR `client_name` is caller-supplied so the
    consent screen names the embedder's product) but binds nothing and never
    blocks; the returned `PendingWebAuthorization` is **serde-serializable**
    plain data (a multi-replica embedder round-trips it through its shared
    store between the two callback requests — it briefly holds the PKCE
    verifier and any `client_secret`, accepted because `StoredAuth` persists
    strictly more; `Debug` still redacts both), and `complete(code, state)`
    verifies `state` before any network I/O, then exchanges and returns the
    `StoredAuth` the embedder saves into its per-user `UserTokenStore`
    (ADR-0184). The runtime/TUI never drive it; core deliberately does not
    re-export it (embedder-facing, like `user_store`).
- **Session-keyed per-user scopes (✅ [ADR-0188](../../adr/0188-session-keyed-per-user-mcp-scopes.md),
  #684):** `mcp::scoped::McpScopes` gives a multi-user embedder per-user MCP
  server *sets* and credentials with no user identity in the runtime
  (ADR-0181): an embedder-supplied `McpScopeResolver` maps a `SessionId` to an
  `McpScope { key, servers, token_store }` — the key an opaque string derived
  from its own user identity, the servers the config `mcp:` shape (capability
  hints and `oauth:` blocks included), the store typically
  `user_scoped(store, user)`. **Replace semantics:** a scoped session's
  `mcp__*` namespace is entirely scope-owned — `list_mcp`/`mcp_tools` resolve
  through the scope (`McpScopes::rows`, `overlay_registry_for_call`; a scoped
  session never sees a global server and never reads the shared last-seen
  cache, [ADR-0226](../../adr/0226-one-advertised-surface-mcp-envelope-registered-is-available.md)) and global MCP tools are stripped from every dispatch snapshot
  (`overlay_registry_for_call`, applied in the executor's detached task before
  `dispatch`; a script binding resolves through the same call, its wait on
  a lazy connect bounded by the script's budget, #831) —
  which is what keeps same-named servers (user A's `kb`, user B's `kb`, the
  global `kb`) unambiguous behind unchanged `mcp__<server>__<tool>` names.
  Connections are lazy, cached per `(scope key, server)` with the #556
  double-checked guard, and live for the process lifetime; eviction is the
  embedder's explicit `evict_scope` (logout, config drift under an unchanged
  key). `prewarm(&session)` between `Spawn` and the first prompt connects the
  scope's servers so `list_mcp` already shows their tools (optional —
  `mcp_tools` and dispatch connect lazily anyway).
  An `oauth:` server with no stored credential in the scope's slice fails as a
  clean auth-required *tool error* before any connect — the embedder's cue to
  run its web-OAuth flow. A null resolver (and every in-tree head: skutter
  passes `None`) is byte-identical to the global single-user behavior.
- **Proxy (`mcp::tool::McpTool`):** adapts one remote tool. `schema()` returns the
  server's `inputSchema` verbatim; `run()` JSON-decodes the model's input to the
  `arguments` object, checks it against the schema's top-level `required` array
  before ever contacting the server — a missing field bails with `` tool `name`
  requires parameter `field` `` instead of a cryptic server-side JSON-RPC error
  (#594) — then calls `tools/call` and maps the result (`mcp::result`, #727):
  text blocks fold into a text part (32 KiB-capped like every host tool), a
  `resource`'s text renders under a `[resource <uri> (<mime>)]` header and a
  `resource_link` as a `[resource link: name <uri> (mime) — description]`
  line; `structuredContent` is rendered as JSON only when no text block
  carried the answer.
  **Binary is never inlined**: `image`, `audio` and a `resource` `blob` all
  become `[binary omitted: <mime>, <size>, uri=<uri>]` (uri only when the
  block has one; size computed from the base64 length without decoding,
  `size unknown` for malformed base64) —
  never an image `ContentPart`. WHY: binary through MCP bloats the context,
  and a part the provider rejects is replayed on every later request (and on
  resume), poisoning the session; an agent that needs the bytes fetches them
  through rhai's `http()` binding or `web_fetch` instead. `isError: true` sets
  `ToolRun::is_error`, so the call
  reaches `OutEvent::ToolOutput`/hooks with the structured `is_error` flag
  ([ADR-0176](../../adr/0176-structured-tool-result-is-error-and-duration-fields.md));
  the `MCP tool reported an error:` text prefix stays, since no provider wire
  carries `is_error` to the model.
  Registered (dispatch) name **`mcp__<server>__<tool>`** — never advertised
  ([ADR-0226](../../adr/0226-one-advertised-surface-mcp-envelope-registered-is-available.md)) —
  with every character outside `[A-Za-z0-9_-]` mapped to `_` by the one
  shared rule (`mcp_tool_name`) that both core's `mcp` unwrap and the
  runtime's registration use, so `my.docs` registers as `mcp__my_docs__…`.
  A tool's owning server is resolved by the longest matching sanitized
  `mcp__<server>__` prefix (`mcp::tool::owning_server`), never a split on
  `__`; mode rules and the `/mcp` panel's `d` deny build their
  `mcp__<server>__*` pattern from the same spelling. Two server names that
  sanitize identically (`my.docs` / `my_docs`) or whose prefixes nest (`a` /
  `a__b`) would make tool names and such patterns ambiguous, so the roster
  keeps only the first — bundles in catalog order, then user entries, each by
  name; only a server that can connect (not disabled, key resolved) claims a
  name — reports the ignored one as a startup config warning, and refuses a
  clashing live `/mcp add` with an `McpChanged` carrying `error`. A per-user
  scope applies the same rule to its own servers (first by name wins, the
  later is skipped with a warning).
  Governed by the same session permission mode as any host tool — there is
  no more agent tool mask to opt an MCP tool into or out of
  ([ADR-0207](../../adr/0207-permission-modes-replace-agent-borne-authority.md)
  retired the `tools:`/`disallowed_tools:` wildcard pattern this row used to
  describe, ADR-0148 with it); a mode rule names one MCP tool exactly
  (`mcp__<server>__<tool>`, optionally scoped) or uses the `*` wildcard —
  mode and ceiling rules have no server-wide `mcp__<server>__*` glob (only
  the session tool overlay matches name globs) — but there is nothing to *admit* separately
  from grading it — every registered tool is reachable by name, mode-graded
  the same as everything else. Today `McpTool::capabilities()` pins every
  MCP tool to `Capability::Write` unconditionally (a deliberate fail-safe —
  an external server is opaque, it could do anything), so a mode class-denying
  `write` (`research`) prompts/denies an MCP tool by its `default`/`exec`-class
  rules unless a mode rule names it explicitly — the per-server
  `capabilities:` hint ([config & live](mcp-config-and-live.md)) does **not** yet feed the new `Capability`
  system (a gap left by ADR-0207; see the `capabilities:` paragraph in [config & live](mcp-config-and-live.md) for
  what it still does).

---

Previous: [§9 — Lifecycle hooks & pluggable policy seams](hooks-and-seams.md) · Next: [§10 — MCP client: config, live management & lazy connect](mcp-config-and-live.md)
