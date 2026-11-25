# 0189. OAuth for LLM provider endpoints

- Status: Accepted
- Date: 2026-08-13
- Closes the (d) edge of
  [#684](https://github.com/xmiksay/entanglement/issues/684) (deferred by
  [ADR-0153](0153-mcp-server-oauth.md)/[ADR-0182](0182-mcp-oauth-device-code-flow-and-closed-refresh-race.md)).
  Constrained by [ADR-0156](0156-normalize-and-stabilize-the-endpoint-pool-key.md)
  (the pool key) and mirroring the MCP transport's bearer handling
  ([ADR-0153](0153-mcp-server-oauth.md)).

## Context

Every catalog entry authenticated with a static `key_env` key baked into the
wire client at construction. The deferred revisit trigger fired: the #684
embedders front LLMs behind their own OAuth (an OpenAI-compatible proxy is
the typical shape), and the auth stack under `mcp::auth` is already generic
OAuth 2.1 plumbing. Two hard constraints shaped the design: (a)
`execute_with_retry`'s `request_fn` closure is sync-to-build with a
`reqwest::Error` future — a token fetch cannot happen inside it; (b) the
endpoint-pool key hashes the credential (`endpoint#sha256(key)`), and pool
entries are never evicted — keying by a *rotating* bearer would mint a fresh
rate-limit bucket (and cross-process `.state` file, ADR-0144) on every
refresh, silently escaping active 429 cool-downs and leaking state.

## Decision

1. **The stack is promoted to `provider::oauth`** (from `mcp::auth`, its
   birthplace): with the LLM wires as a second consumer, "MCP's auth module"
   is the wrong home for what ADR-0181 already called the crate's universal
   auth interface. One canonical path — `crate::oauth` — and the `mcp` module
   consumes it like everything else; no compatibility re-export is kept
   (in-tree consumers and live docs are updated; older ADRs naming
   `mcp::auth` are history).
2. **Catalog**: `ProviderEntry` gains `oauth: Option<OauthConfig>` — present
   (even empty), the endpoint authenticates with `Authorization: Bearer`
   instead of `key_env`; the same override fields as the `mcp:` blocks
   short-circuit discovery for an endpoint without RFC 9728/8414 metadata.
   No embedded default carries one (production zai/openai/anthropic/gemini
   stay static-keyed; a test pins that).
3. **All three wires** take an optional `Arc<dyn AccessTokenSource>`
   (`with_auth` on the client, a parameter on the factory). The token is
   fetched *before* `execute_with_retry` (the MCP transport's shape), cached
   until expiry by `StoredTokenSource`, and replaces the wire's static header
   (`bearer_auth` / `x-api-key` / `x-goog-api-key`; Gemini's context-cache
   call carries the same credential). On a `401` the wire retries exactly
   once with `force_refresh` — a second 401 is a terminal error. Anthropic's
   `pause_turn` continuation loop re-fetches per POST, so a long turn's
   continuations ride refreshed tokens.
4. **Pool identity decouples from the secret**: an OAuth endpoint passes
   `None` (pool keyed by normalized endpoint alone) — the bearer never
   reaches `pool_key`. Per-user fairness on a shared OAuth endpoint stays
   ADR-0175's admission gate, exactly as for a shared literal key.
5. **skutter (single-user)**: credentials live in the managed
   `llm-tokens.yml` (`ENTANGLEMENT_LLM_TOKENS_FILE`), the same file format,
   locking, and quarantine as `mcp-tokens.yml` (one shared implementation,
   `McpTokenStore::load_llm()` — a separate file so neither surface's writes
   contend with the other's credentials), keyed by provider name. Minting is
   `skutter config connect <provider> [--device-code]` (browser loopback or
   RFC 8628; `disconnect` revokes best-effort), the LLM twin of
   `/mcp connect` as a pre-engine CLI fast path. A factory resolving an
   `oauth:` entry with no stored token errs with that exact hint; provider
   auto-detect stays key-based (an OAuth provider is selected explicitly).
6. **Multi-user embedders**: `UserProviderContext::with_token_source(name,
   source)` registers a per-user bearer source (typically
   `StoredTokenSource::new(provider, user_scoped(store, user))`);
   `resolve_for_user` hard-errs on an `oauth:` entry without one and needs
   no static key with one.

## Consequences

- **(+)** Any OAuth-protected OpenAI-compatible proxy (or Anthropic/Gemini
  -wire gateway) is one `providers.yml` entry away — the #118 "catalog data,
  not hardcode" property extended to auth.
- **(+)** Refresh-race safety is inherited: `StoredTokenSource` +
  `TokenStore::with_exclusive` (ADR-0182) serialize refreshes in-process and
  across processes; the managed-file store reuses the proven implementation.
- **(−)** A token refreshed mid-retry-ladder isn't picked up until the next
  request (the fetch is pre-`execute_with_retry`) — acceptable: the ladder
  retries transport faults, and the 401 path re-enters with a fresh fetch.
- **(−)** `StoredTokenSource`'s refresh-failure hint still says
  `/mcp connect <name>` — for an LLM provider the right command is
  `skutter config connect <name>`; cosmetic, noted for polish.
- **(neutral)** The `serve`/TUI heads gain no in-session connect surface —
  LLM OAuth connects are a CLI/pre-engine concern (`/key` stays the static
  surface), revisit on demand.

## Alternatives considered

- **Key the pool by the bearer.** Rejected — the churn described in Context;
  ADR-0156 already rejected evicting `EndpointState` on rotation.
- **Fetch the token inside `request_fn`.** Rejected: the closure's error type
  is `reqwest::Error` and it is built synchronously; widening
  `execute_with_retry`'s contract for one consumer is worse than the MCP
  transport's proven pre-fetch shape.
- **OpenAI-compat wire only.** Rejected by the user: all three wires carry
  the mechanism so an Anthropic/Gemini-wire gateway needs no future seam
  change; the incremental cost was the per-wire header swap + 401 loop.
- **Storing LLM tokens inside `mcp-tokens.yml` under a namespace.** Rejected:
  the two surfaces would contend on one lock file, and a parse quarantine of
  one would take the other's credentials with it.

## References

- [ADR-0153](0153-mcp-server-oauth.md) / [ADR-0182](0182-mcp-oauth-device-code-flow-and-closed-refresh-race.md) — the mechanism reused.
- [ADR-0156](0156-normalize-and-stabilize-the-endpoint-pool-key.md) / [ADR-0144](0144-file-backed-shared-endpoint-state-across-instances.md) — why the pool identity must be stable.
- [ADR-0175](0175-per-user-admission-gate-on-a-shared-literal-key.md) — per-user fairness on a shared endpoint.
- [#684](https://github.com/xmiksay/entanglement/issues/684).
