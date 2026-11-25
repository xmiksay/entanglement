# 0188. Session-keyed per-user MCP scopes

- Status: Accepted
- Date: 2026-08-13
- Implements [ADR-0181](0181-userid-leaves-the-runtime-crate.md)'s direction
  for MCP; builds on [ADR-0184](0184-provider-hosted-multi-user-seams.md)'s
  storage seam. Closes the (b) edge of
  [#684](https://github.com/xmiksay/entanglement/issues/684) (deferred by
  ADR-0153/0182). Reuses the #556 lazy-connect discipline
  ([ADR-0152](0152-provider-bundled-mcp-servers-three-state-enablement.md)).

## Context

ADR-0184 shipped `user_scoped(store, user)` with zero consumers: the runtime's
single token-store injection point (`connect.rs`) and all MCP clients/tools
were process-global. The #684 trigger fired — multi-user web embedders want
each user to have their *own MCP server set* (not just tokens: two users may
run same-named servers at different endpoints) routed per session. ADR-0181
fixes the constraints: no `UserId` anywhere in `entanglement-runtime`
(`make userid`), runtime seams are `SessionId`-keyed embedder closures, and it
explicitly rejected tool-name namespacing (`mcp__<user>__…` — registry
cardinality × users, identity leaked to the model) and a core `EngineConfig`
MCP seam (core never dispatches MCP).

## Decision

1. **`runtime::mcp::scoped::McpScopes`**, constructed by the embedder around an
   `McpScopeResolver = Arc<dyn Fn(&SessionId) -> Option<McpScope>>` where
   `McpScope { key: String, servers: HashMap<String, McpServerConfig>,
   token_store: Option<Arc<dyn TokenStore>> }`. The `key` is an opaque string
   the embedder derives from its own user identity; the store is typically
   `user_scoped(store, user)`. The resolver runs on the sync advertisement
   path and per MCP dispatch — it must be an in-memory lookup.
2. **Replace semantics**: a scoped session's `mcp__*` namespace is entirely
   scope-owned. `overlay_specs` (called from the embedder's
   `tool_spec_resolver`) strips global `mcp__*` specs and advertises the
   scope's cached listings; `overlay_registry_for_call` does the same to the
   executor's dispatch snapshot. Name collisions resolve *structurally* — the
   `(scope key, server)` connection cache — never nominally; tool names stay
   `mcp__<server>__<tool>`. A null resolver (and every in-tree head: skutter
   passes `None`) is byte-identical to global single-user behavior.
3. **Lazy per-scope connections**: first need connects (double-checked
   per-key guard, 60s timeout — the #556 pattern), cached per
   `(scope key, server)` for the process lifetime. **Eviction is explicit
   only** (`evict_scope`, on logout or config change — the cache keys on
   `key` alone, so drift under an unchanged key requires it); this matches
   the global connections' process-lifetime precedent. `prewarm(&session)`
   between `Spawn` and the first prompt connects + lists concurrently so the
   sync advertisement path has specs to serve; the dispatch-side lazy connect
   is the backstop.
4. **Executor wiring**: `spawn_tool_executor_with_policy` gains a final
   `Option<Arc<McpScopes>>` parameter (the #627 precedent). The Permission
   arm overlays inside the already-detached task before `dispatch`; an
   overlay refusal is the call's tool error. The `rhai` arm overlays
   **cached-only** — a script sees the scope's connected tools but can never
   block the executor loop on a lazy connect.
5. **Auth-required is a clean tool error**: an `oauth:` server whose scope
   slice holds no token refuses *before* any connect (mirroring the global
   startup quiet-skip) with ``MCP server `name` requires authorization for
   this user; …`` — the string shape an embedder keys its web-OAuth prompt
   (ADR-0187) off. A live 401 classifies to the same message via
   `is_auth_required`.
6. **Connect plumbing**: `connect_client_with_store` generalizes
   `connect_client` with an explicit `Option<Arc<dyn TokenStore>>`; `None`
   (every pre-existing caller) keeps the process-global managed file.
   Permission grading, masks, grants, and capability fan-out are untouched —
   names are unchanged, and a scope's `capabilities:` hints feed the same
   `capability_index` an embedder folds into its own per-user
   `PermissionResolver` (the ADR-0184 §3 recipe pattern).

## Consequences

- **(+)** Two users' same-named `kb` servers (and a global `kb`) coexist,
  each session dispatching over its own authenticated client; the
  `make userid` gate stays green by construction (opaque `String` keys).
- **(+)** Single-user behavior is provably unchanged (null-resolver identity
  is unit-tested; skutter never constructs an `McpScopes`).
- **(−)** Per-user tools share one *policy* identity per name (a profile rule
  for `mcp__kb__x` applies to every user's `kb`) — the accepted ADR-0181
  trade; per-user policy is the embedder's `PermissionResolver`.
- **(−)** A never-prewarmed scoped session advertises no MCP tools until a
  dispatch connects one; a background prewarm off `SessionStarted` is
  possible future work.
- **(−)** Per-scope *stdio* servers spawn one child per `(scope, server)` —
  HTTP transports are recommended for per-user servers; LRU eviction is
  future work if scope cardinality bites.

## Alternatives considered

- **Overlay (merge) instead of replace.** Rejected: the registry is
  name-keyed, so same-named tools from the scope and the global set cannot
  coexist in one dispatch snapshot; merge would make "which `kb`?" depend on
  insertion order. Replace gives one deterministic meaning per session.
- **A session-aware `AccessTokenSource` on shared clients.** Rejected: the
  transport's `access_token()` has no session context, and concurrent
  sessions of different users would race one client's bearer.
- **Params-struct refactor instead of the 16th executor parameter.**
  Deferred: a real cleanup, but orthogonal — and the only out-of-tree
  consumers are the embedders this change unblocks.

## References

- [ADR-0181](0181-userid-leaves-the-runtime-crate.md) /
  [ADR-0184](0184-provider-hosted-multi-user-seams.md) — direction + storage.
- [ADR-0187](0187-mcp-oauth-web-redirect-flow-for-embedders.md) — how the
  tokens get minted.
- [#684](https://github.com/xmiksay/entanglement/issues/684).
