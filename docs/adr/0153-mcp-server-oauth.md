# 0153. MCP server OAuth, and moving the MCP client mechanism into the provider crate

- Status: Accepted — Amended by [0157]
- Date: 2026-08-01
- Issue: tui-ux-batch plan, Issue 3

## Context

The streamable-HTTP MCP transport (ADR-0080/#312) authenticates with static
per-server `headers` hand-written into `config.yml`, `${VAR}`-expanded from the
environment. That covers a server issuing a long-lived personal token, and
nothing else.

Most remote MCP servers — the claude.ai-style integrations this transport
exists to reach — are OAuth-protected, and critically they issue **no
pre-registered `client_id`**: there is no developer console to register an app
in. A client that cannot register itself dynamically cannot connect to them at
all, so a hand-configured `authorization_url`/`token_url`/`client_id` block
would have been a feature that works for almost none of the servers it targets.

Two further questions were open before any of this could be built:

1. **Where does the mechanism live?** The transport sat in
   `entanglement-runtime::mcp::http`, tangled with the runtime's config
   parsing, tool registry, and permission wiring. `entanglement-provider` has
   other consumers, and an authenticating MCP client is exactly the kind of
   reusable mechanism a leaf crate should own.
2. **How does a head start a flow?** The TUI holds no MCP server configs — the
   runtime's MCP responder does, and `/mcp list` already round-trips over the
   wire to reach it.

## Decision

### Mechanism moves to `entanglement-provider`, policy stays in the runtime

`entanglement-provider::mcp` now owns the streamable-HTTP transport
(`McpHttpClient`, formerly `runtime::mcp::http::HttpClient`), the shared
JSON-RPC/tool-definition helpers, and the new OAuth mechanism. The runtime
keeps everything that is *policy*: config parsing, the `ToolRegistry`, the
permission-governed `McpTool`, three-state activation (ADR-0152), the token
file, the browser launch, and the responder.

The runtime reaches these types through **core's re-export** (ADR-0053) — the
same path `McpServerState` already takes — so the lean
`--no-default-features` build gets them without naming the optional
`entanglement-provider` dependency. Consequently the runtime no longer names
`reqwest` at all: the `mcp-http` feature drops its direct `reqwest`/`futures`
deps and now only gates whether the HTTP-MCP code paths compile in. `reqwest`
riding in via core→provider is already explicitly sanctioned by ADR-0025's
lean gate.

The **stdio** transport deliberately stays in the runtime: it spawns a
subprocess and needs the provider-key scrub (#164/ADR-0124), which is policy.

`mcp::HttpClient` remains exported under its historical runtime name, so an
embedder assembling a per-tenant client is unaffected.

### Discovery and dynamic registration, not hand-configured endpoints

An MCP server entry gains an optional `oauth: OauthConfig` block. Its presence
— *even empty* — switches the server from static-header auth to OAuth. Every
field inside is an **override**, not a requirement:

1. **RFC 9728** protected-resource metadata, from the `401` challenge's
   `WWW-Authenticate: Bearer resource_metadata="…"` pointer, else the
   well-known path derived from the MCP URL. Names the authorization server(s)
   and, optionally, an RFC 8707 `resource` indicator.
2. **RFC 8414** authorization-server metadata (with the OpenID Connect
   well-known path as a fallback) supplies the authorization, token,
   registration, and revocation endpoints.
3. **RFC 7591** dynamic client registration mints a `client_id` when the config
   supplies none. We register as a **public client**
   (`token_endpoint_auth_method: "none"`): no client secret can be kept
   confidential in a local CLI, which is precisely the case OAuth 2.1 + PKCE is
   designed for. A server that issues a secret anyway gets it stored and used.
4. **PKCE S256** is mandatory and the only method offered; `plain` is not
   implemented. A server advertising `code_challenge_methods_supported` without
   `S256` is refused rather than downgraded.

Setting both `authorization_url` and `token_url` short-circuits discovery
entirely — the escape hatch for a server that publishes no metadata.

Randomness for the PKCE verifier and the CSRF `state` comes from `uuid`'s v4
generator (getrandom-backed), the crate core already uses for session ids — no
new dependency tree for two calls.

### The redirect catcher is hand-rolled on `tokio::net`

RFC 8252 loopback redirection needs a one-request HTTP server. That is written
directly over `TcpListener` rather than pulling `axum`/`hyper` into the leaf
crate: it binds `127.0.0.1` explicitly (never `0.0.0.0`), accepts until the
redirect arrives, and is dropped immediately after. A mismatched `state` is
**refused outright** rather than skipped — treating it as "keep waiting" would
defeat the CSRF check.

The listener is bound **before** registration, because the ephemeral port is
part of the `redirect_uri` that registration declares.

### Browser launch is the caller's job, and is not opt-in

`AuthFlow::begin` returns the authorization URL; the runtime opens it. This
keeps process spawning out of the provider crate and lets a headless head fall
back to printing the URL with no separate code path.

Launching is **not** gated behind an env var. The user typed `/mcp connect`;
opening their own browser to finish the flow they just asked for is the obvious
intent and stays inside the local trust model (ADR-0047/ADR-0048). The URL is
**always** reported as well, whether or not the launch succeeded — so a failed
launch degrades to "copy this link" rather than a dead end. In the TUI it is
rendered as transcript content, never a toast, precisely because it must
survive long enough to be copied.

### `InMsg::McpAuth` is trusted-only

`InMsg::McpAuth { name, action: Connect|Check|Disconnect }` →
`OutEvent::McpAuthChanged` is engine-global (`session()` → `None`) like
`McpList`/`McpChanged`, answered by the runtime's MCP responder, which alone
holds the server configs and the token store. Every head gets the feature; the
TUI sends over the privileged `Holly::send` exactly as `/mcp add` already does.

It is **wire-refused**, sharpening #472/ADR-0124's rationale: a forged
`Connect` opens a browser window on the user's desktop and mints a durable
credential on their behalf; a forged `Disconnect` destroys one. Neither may be
driven by an untrusted wire peer. Unlike `McpList`, not even `Check` is
wire-allowed — it performs a token refresh, which mutates stored state.

A `Connect` emits `McpAuthChanged` **twice**: an interim event carrying
`authorize_url`, then the terminal outcome. Each op runs detached from the
responder loop, since a connect parks for up to five minutes on the browser and
would otherwise stall every other MCP frame behind it.

### Credentials persist beside the other managed files

`${config_dir}/entanglement/mcp-tokens.yml` (override
`ENTANGLEMENT_MCP_TOKENS_FILE`), a sibling of `grants.yml`/`agent-models.yml`/
`aux-models.yml`: written `0600` through the existing `atomic_write`, mutated
read-merge-write under the same advisory file lock (#329/ADR-0084).

The record stores the **resolved endpoints and client id alongside the token**,
not just the token. That is what lets a *startup* connect skip discovery and
registration entirely — they run only during an explicit `/mcp connect`.

Secrets never reach the log: the `Debug` impls on `TokenSet`/`StoredAuth`
redact every credential field, and no error path prints a token value.

### Startup never opens a browser

An OAuth server with no stored credential is skipped **non-fatally and
quietly**, and reported in `/mcp list` with `auth: "needs auth"` — a distinct
posture from a transport failure, since the fix is `/mcp connect`, not
debugging a connection. Attempting the connect would only yield a `401` and a
more alarming log line. Auto-launching a browser at startup was rejected
outright: it is intrusive and breaks headless/CI runs.

### Revocation is best-effort, deletion is not

`/mcp disconnect` attempts RFC 7009 revocation when the authorization server
advertises a revocation endpoint, then deletes the local credential **whether
or not revocation succeeded** and drops the live connection. Keeping a token on
disk because the server was unreachable is the worse outcome.

## Consequences

- A remote MCP server is reachable from a URL alone. No console, no
  hand-copied token, no `client_id`.
- `entanglement-provider` gains a complete, authenticating MCP client its other
  consumers can use without a runtime dependency.
- The runtime drops its direct `reqwest` dependency; `mcp-http` becomes a pure
  compile gate.
- `mcp-tokens.yml` holds live credentials. It is `0600` and must never be
  committed — the file carries a header saying so.
- **Cross-process refresh is racy (accepted for v1).** The store's file lock
  serializes the *write* but not the token *exchange*, so two `skutter`
  instances refreshing the same rotating grant at the same instant can have one
  lose; it recovers by re-authorizing. A cross-process refresh lease is out of
  scope.
- Refresh is serialized per token source in-process by an async mutex, so
  concurrent MCP requests cannot both spend a single-use refresh token.
- `/mcp register` from the original plan is **not** implemented as a separate
  command: dynamic client registration happens automatically inside `connect`,
  which is where it is actually needed.
- Not covered: device-code flow (for a truly browser-less host), per-user
  credentials in the multi-user embedder API (ADR-0147 — the token store is
  process-global, keyed by server name only), and OAuth for any *LLM* provider
  endpoint. The mechanism now lives in the right crate for the last one.
