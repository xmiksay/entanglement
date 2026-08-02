# 0157. The MCP HTTP transport shares the LLM endpoint pool

- Status: Accepted
- Date: 2026-08-02
- Amends: [ADR-0050](0050-per-endpoint-connection-pool-retry-rate-limit.md) (widens
  who goes through the pool — the LLM clients are no longer its only callers),
  [ADR-0153](0153-mcp-server-oauth.md) (the transport's `connect`/
  `connect_authenticated` signatures, unchanged since that ADR, now take the
  pool client)

## Context

Issue #559: `McpHttpClient::connect_with` (`entanglement-provider/src/mcp/http.rs`)
built its own bare `reqwest::Client` per server connection — no `HttpClient`, no
RPM/concurrency/`Retry-After` participation, no connection pooling, and (before
this change) a flat 60s whole-request timeout with none of the endpoint pool's
patient 429 handling. `entanglement-provider/src/mcp/auth/mod.rs` builds another
bare client for its own, much lower-volume OAuth bookkeeping (discovery, DCR,
token refresh) — deliberately out of scope here, see Alternatives.

The provider-bundled z.ai MCP servers (`defaults.yml`, ADR-0152) — `web_search_prime`/
`web_reader`/`zread` — hit `https://api.z.ai/api/mcp/*` with the **same
`ZAI_API_KEY`** the LLM endpoint (`https://api.z.ai/api/coding/paas/v4`) is
carefully rate-limiting (ADR-0050/ADR-0111/ADR-0140/ADR-0144). A search-heavy
turn's MCP tool calls therefore issued completely unmetered requests against a
per-key provider limit the LLM traffic was pacing for — systematically
under-counting the real budget, and opening an independent TCP/TLS pool per MCP
server on top of it.

## Decision

**`McpHttpClient` takes the caller's `HttpClient` (the pool) and an optional
`api_key`**, instead of building its own `reqwest::Client`. `connect`/
`connect_authenticated`/the internal `connect_with` gain `http: HttpClient` and
`api_key: Option<String>` parameters (breaking, deliberately — every in-tree
call site and the `mcp_http` example are updated in this change; an out-of-tree
embedder needs the same). Every actual request (`post_once`) now goes through
`HttpClient::execute_with_retry`, exactly like `OpenAiLlm`/`AnthropicLlm`/
`GeminiLlm` — the returned `StreamGuard` is held for the whole
`request`/`notify` call (through the SSE drain or JSON body read), mirroring
the LLM clients' `spawn_byte_stream`.

**Pool-key identity is `(self.url, self.api_key)`** — the MCP server's *own*
URL, not the LLM endpoint's. This keeps each bundled server in its own
`EndpointState` bucket, isolated from its provider's LLM endpoint (ADR-0050
already rejected keying by host alone: a shared host can front distinct
rate-limit domains), while still hashing/normalizing/cross-process-sharing
under the same key-hash conventions the LLM client uses for that provider's
key. `rpm`/`concurrency` are left `None` (pool defaults) for v1 — `defaults.yml`
sets neither for `mcp_servers` today; a future issue can add per-server catalog
overrides the same way ADR-0050/#414 did for LLM endpoints.

**The old flat 60s whole-request timeout is gone.** `execute_with_retry`'s own
bounded behavior (capped backoff for transient failures, a patient
`rate_limit_max_elapsed` ≈ 900s for 429s) now governs, the same tradeoff the
LLM clients already accept: a 429 is retried patiently rather than treated as a
hang. This is a deliberate consistency choice — MCP traffic behaves exactly
like LLM traffic sharing the endpoint pool, no special-cased shorter fuse.

**The provider key is resolved from `AvailableServer.key_env`**, which already
existed (ADR-0152) but was dropped at the startup/connect boundary. Fixed by
widening `AvailableMcp::partition`'s startup-set return type from a bare
`HashMap<String, McpServerConfig>` to `HashMap<String, AvailableServer>` — the
same shape the `allowed`-roster side already used — so the `key_env` linkage
survives into `mcp::connect`. `enable_for_session` (the lazy `/enable mcp`/
`mcp_enable`-tool path — the *default* activation state for a bundled server,
ADR-0152) already had `&AvailableServer` in hand at its connect point, so it
resolves `api_key` the same way with no further plumbing. A live `/mcp add`
(user-declared server) and an OAuth reconnect (`/mcp connect`, bearer-token
authenticated) always pass `api_key: None` — neither carries a provider-shared
key; they still ride the shared `HttpClient` (pooled, connection-reused,
RPM/concurrency-capped), just under an unkeyed bucket, same as an unkeyed LLM
endpoint today.

**One process-wide `HttpClient` is threaded to every connect call site** —
`mcp::connect` (startup), `enable_for_session` (lazy enable, both the
`mcp_enable` tool and the TUI's `/enable`/session-tools-dialog path),
`mcp_add`/`mcp_reconnect` (live add, OAuth post-authorization reconnect) — the
same instance `main.rs` already builds once and passes to every LLM wire
client. `entanglement-core` gains a `HttpClient` re-export (alongside its
existing `McpHttpClient` re-export) so the `mcp-http`-only build (without the
`provider` feature — see ADR-0153's crate split) keeps naming no direct
`entanglement-provider` dependency.

**`mcp/mod.rs`'s connect machinery moves to a sibling `connect.rs`** (`#[path]`
file, mirroring #556's `available_enable.rs` split) to stay under the 400-line
file cap — `connect`/`needs_auth` re-exported at their original public paths,
the rest `pub(crate)` for the existing `super::`-qualified call sites.

## Consequences

### Positive

- A bundled MCP server's traffic against a shared provider key now counts
  against the same RPM/concurrency budget the LLM endpoint enforces —
  `ZAI_API_KEY`'s real limit is finally the one thing both traffic types
  respect together.
- MCP HTTP connections reuse the shared pool's tuned `reqwest::Client`
  (`pool_max_idle_per_host`/`pool_idle_timeout`) instead of opening an
  independent TCP/TLS pool per server.
- A saturated MCP endpoint gets the same patient 429 handling (AIMD pacing,
  bounded park, cross-process sharing when enabled) as an LLM endpoint,
  instead of a bare timeout.

### Negative / neutral

- `McpHttpClient::connect`/`connect_authenticated` are breaking signature
  changes for embedders building a per-tenant client directly (the doc'd #364
  seam) — both gain `http`/`api_key` parameters.
- The old flat 60s ceiling on a hung-but-connected server (accepted a
  connection, never answered) is gone; only `CONNECT_TIMEOUT` (30s, connection
  establishment) and `execute_with_retry`'s failure-path bounds apply now —
  the same gap the LLM clients already live with. Considered acceptable: MCP
  traffic behaving identically to LLM traffic is the point of routing it
  through the same pool.
- `AvailableMcp::partition`'s startup-set return type changed
  (`HashMap<String, McpServerConfig>` → `HashMap<String, AvailableServer>`) —
  contained to `main.rs` and the in-tree tests; no other caller existed.

## Alternatives considered

- **At minimum, just an RPM gate for provider-bundled servers** (the issue's
  fallback suggestion). Rejected: a bespoke, MCP-only rate limiter would
  duplicate the pool's RPM/concurrency/backoff/429/cross-process machinery
  (ADR-0050/0111/0140/0144) rather than reuse it, and would still leave MCP
  connections opening their own TCP/TLS pool per server.
- **Key MCP traffic into the *same* `EndpointState` bucket as its provider's
  LLM endpoint** (share the exact pool key, not just the key-hash
  conventions). Rejected: the LLM endpoint and the MCP path are different URLs
  or even different documented rate-limit domains at the provider — ADR-0050
  already rejected keying by host alone for exactly this reason (Coding Plan
  vs pay-as-you-go tiers on one host). Each MCP server keeps its own bucket.
- **Also route `mcp/auth/mod.rs`'s OAuth bookkeeping client through the
  pool.** Rejected as out of scope: that module's own doc comment already
  argues these are short, infrequent requests to a handful of discovery/token
  endpoints — not the metered, potentially search-heavy tool-call traffic
  #559 is about — and pooling it would gain nothing while adding an unrelated
  signature change to every OAuth call site.
- **Resolve the provider/key linkage by re-scanning the catalog inside
  `mcp::connect`** instead of widening `partition`'s return type. Rejected:
  two independent lookups (partition's own linkage, plus a re-derived one at
  connect time) can drift; carrying `AvailableServer` through in one place
  keeps it as flowing data instead.

## References

- Issue #559: bundled MCP traffic bypasses the endpoint pool
- [ADR-0050](0050-per-endpoint-connection-pool-retry-rate-limit.md): the pool
  this change routes MCP traffic through
- [ADR-0152](0152-provider-bundled-mcp-servers-three-state-enablement.md):
  `AvailableServer.key_env`, the three-state enablement whose lazy-enable path
  is the *default* activation this change covers
- [ADR-0153](0153-mcp-server-oauth.md): the transport this change modifies,
  and the mechanism/policy split (`entanglement-provider` vs runtime) it
  preserved
- [ADR-0156](0156-normalize-and-stabilize-the-endpoint-pool-key.md): the
  `pool_key` normalization/hashing every new caller gets for free
