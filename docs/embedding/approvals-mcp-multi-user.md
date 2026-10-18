# Embedding §5–§7 — Approvals across restart, MCP & multi-user

> Part of the [embedding guide](../embedding.md).

## 5. Approval-across-restart semantics

A turn that ends on tool calls **parks** as explicit session state
(`Session.turn: Option<TurnState>`,
[ADR-0061](../adr/0061-parked-turn-state-batch-tool-resolution.md)) rather than
living only on an async stack — this is what makes it safe to persist a
mid-turn session and pick it up later, in a different process, after a crash
or a deploy. Three things follow that a custom executor/resolver must honor:

- **Re-offer on resume, at-least-once.** `Holly::resume` re-emits every
  pending call as a fresh `ToolExec` (same `request_id`, new `seq`). If your
  previous process instance already ran that call and the result never made it
  to your (possibly async/batched) sink, it runs again after resume — your tool
  implementations must tolerate a repeat, or dedupe themselves.
- **Idempotency is keyed on `request_id`, not the tool + args.** The reference
  executor tracks an in-flight `HashSet<String>` per session and skips a
  `ToolExec` whose `request_id` it's already running
  ([ADR-0071](../adr/0071-parked-turn-reoffer-timer.md)) — this is what makes the
  *in-process* re-offer timer (60s of silence on a parked turn) safe without a
  restart. A custom executor needs the equivalent: track in-flight
  `request_id`s yourself if you don't reuse `ToolExecutor::builder`'s
  executor as-is.
- **Single-instance assumption.** Nothing in the protocol coordinates *two*
  processes racing to resolve the same parked turn — `Holly::resume` assumes
  the caller is the sole authority resuming that session id. If your deployment
  can run more than one instance against the same store, you need your own
  lock/lease per session id before calling `resume`; entanglement doesn't
  arbitrate that for you.

**`Holly::hibernate` is the embedder-driven counterpart to all of this**
([ADR-0077](../adr/0077-session-hibernation-evictable-resumable.md)): it tears
down a session's in-memory state — same as a crash, but on purpose, to cap
memory across many idle tenants — without tombstoning the id, so a later
`resume` rebuilds it from your log exactly like the restart path above,
re-offering any pending call the same way. `EngineConfig.idle_ttl`
([ADR-0090](../adr/0090-idle-ttl-auto-hibernation.md)) automates the sweep if you'd
rather not call `hibernate` yourself.
[`examples/embedded_lifecycle.rs`](../../entanglement-runtime/examples/embedded_lifecycle.rs)
drives the whole `hibernate` → `pair_records` → `resume` → continue round-trip
against its own `RecordSink`.

## 6. Attaching MCP servers programmatically

The YAML `mcp:` config section (`mcp::AvailableMcp::from_config`, connected
lazily on first use — [ADR-0067](../adr/0067-mcp-client-as-runtime-tool-provider.md),
[ADR-0226](../adr/0226-one-advertised-surface-mcp-envelope-registered-is-available.md)) is one static
`command`/`url` per server, shared by every session — not what a multi-tenant
embedder needs when each tenant has their own remote server or their own
per-user auth token. `mcp::HttpClient::connect(server, url, headers, http, api_key)`
([ADR-0080](../adr/0080-mcp-streamable-http-transport.md), behind the `mcp-http`
feature) is public for exactly this: build a client straight from a token you
already looked up (`http` is the shared endpoint pool, `api_key` an optional
pool-key identity — `None` for a per-tenant server), wrap it in
`McpClient::Http`, and register its tools with
`McpTool::new(client, server, def, capabilities)` into any `ToolRegistry` — the same runtime
tool-provider path a YAML-configured server goes through, with zero core
change and no config file in the middle.

```rust
let http = HttpClient::connect("acme", &url, &per_tenant_headers, pool.clone(), None).await?;
let client: Arc<McpClient> = Arc::new(McpClient::Http(http));
for def in client.list_tools().await? {
    registry.register(McpTool::new(client.clone(), "acme", def, &HashMap::new()));
}
```

[`examples/mcp_http.rs`](../../entanglement-runtime/examples/mcp_http.rs) compiles
this path on every `make lint` run (it needs `mcp-http`, on by default, so it
can't ride the lean `--no-default-features` build the other two examples
target); set `MCP_HTTP_URL` to actually connect it to a server.

Hand-registering like this shares one client across every session. When each
*user* needs their own servers and credentials routed per session, use the
managed seam instead — `mcp::scoped::McpScopes` in §7 (#684) does the caching,
lazy connecting, and per-session discovery/dispatch replacement for you.

## 7. Multi-user: per-user providers, permissions, tokens

Multi-user mode is an **embedder API**, and by
[ADR-0181](../adr/0181-userid-leaves-the-runtime-crate.md) the runtime crate
never names `UserId` (`make userid` enforces it): the type lives in
`entanglement-provider` (re-exported by core, riding `InMsg::Spawn { user }`),
and every per-user seam is something *you* implement and hand in. You already
know which user each session belongs to — you chose `user` when you sent the
session's `Spawn` — so keep your own `HashMap<SessionId, UserId>` (or derive
it from a session-id convention like §1's `{tenant}:{uuid}`) and close your
seam implementations over it. Per seam:

- **Providers, API keys, RPM/concurrency budgets** —
  [`entanglement_provider::multi_user`](../adr/0184-provider-hosted-multi-user-seams.md):
  implement `UserProviderStore` over your tenant storage and wire
  `build_user_model_resolver(store, http_client, web_search)` onto
  `EngineConfig::model_resolver`. Each user gets their own `Catalog` + keys
  (never `std::env`); two users sharing one literal key each get their own
  admission slice ([ADR-0175](../adr/0175-per-user-admission-gate-on-a-shared-literal-key.md)).
  To start a session on a user-chosen model, put it on the session's own
  `Spawn` — `model: Some(ModelChoice::new(provider, model))` (#725). It
  resolves through that same resolver under the session's user before the
  first round, wins over the agent's pin, and refuses the spawn with an
  `Error` if it can't resolve. Don't send a follow-up `SetModel` for this (it
  queues behind the initiating prompt, so turn one runs on the default) and
  don't pin a sentinel agent model plus a side map keyed by session — both
  race concurrent creates.
- **Permission ceiling + grants** — implement the #311
  `PermissionResolver`/`GrantStore` traits directly over your session→user
  map ([`examples/embedded.rs`](../../entanglement-runtime/examples/embedded.rs)
  compiles the per-tenant shape). Compose least-privilege the way the
  in-tree resolver does: resolve the process-global profile first, then
  clamp by the user's own ceiling (`PermissionProfile::clamp_to_base`); key
  `Always`-scope grants by the user so one user's grant structurally cannot
  reach another's session (`Session` scope stays per-session — a session
  belongs to exactly one user).
- **Aux-model pins** — no module either
  ([ADR-0183](../adr/0183-narrate-purpose-and-per-user-aux-pins.md)): snapshot
  the user's pins via `AuxModelStore::in_memory` and close your multi-user
  `ModelResolver` over the user before constructing a plain
  `AuxLlmRegistry`.
- **OAuth-protected LLM endpoints** —
  [ADR-0189](../adr/0189-oauth-for-llm-provider-endpoints.md): a catalog entry
  carrying an `oauth:` block sends an `Authorization: Bearer` from an
  `AccessTokenSource` instead of a static key. Register each user's source on
  their context:
  `ctx.with_token_source("myproxy", Arc::new(StoredTokenSource::new("myproxy",
  user_scoped(store, user))))` — the same `UserTokenStore` your MCP
  credentials live in works here (mint the tokens with `WebFlow` exactly as
  for an MCP server). An `oauth:` entry with no registered source is a hard
  resolve error; with one, no static key is needed.
- **MCP OAuth credentials** — implement
  `entanglement_provider::oauth::UserTokenStore` over your storage;
  `user_scoped(store, user)` presents one user's slice as the plain
  per-connection `TokenStore` every auth consumer takes
  ([ADR-0184](../adr/0184-provider-hosted-multi-user-seams.md)). Minting the
  tokens from a web app is `WebFlow`
  ([ADR-0187](../adr/0187-mcp-oauth-web-redirect-flow-for-embedders.md)) — two
  handlers on your own domain:

  ```rust,ignore
  // Handler 1 — "connect kb" button:
  let pending = WebFlow::begin("kb", mcp_url, &oauth_cfg, None,
                               "https://app.example/oauth/mcp/callback",
                               "my-app").await?;
  store_pending(pending.state(), serde_json::to_string(&pending)?); // TTL ~10 min
  Redirect::to(pending.authorize_url())
  // Handler 2 — the callback, possibly on another replica:
  let pending: PendingWebAuthorization = load_and_delete_pending(&params.state)?;
  let auth = pending.complete(&params.code, &params.state).await?;
  user_token_store.save(&user, "kb", &auth)?;
  ```

- **Per-user MCP connections** — `mcp::scoped::McpScopes`
  ([ADR-0188](../adr/0188-session-keyed-per-user-mcp-scopes.md)) routes a scoped
  session's `mcp__*` calls over that user's own servers and credentials
  (replace semantics — the global MCP set disappears for scoped sessions;
  same-named servers across users stay distinct via the `(scope key, server)`
  connection cache). Close an `McpScopeResolver` over your session→user map,
  and hand the `McpScopes` to `ToolExecutor::builder(..).mcp_scopes(scopes)`
  (leaving the setter out keeps single-user behavior). MCP schemas are never
  advertised ([ADR-0226](../adr/0226-one-advertised-surface-mcp-envelope-registered-is-available.md)), so there is no spec overlay to wire: a scoped
  session's `list_mcp`/`mcp_tools` resolve through its scope and its
  `mcp(...)` calls dispatch over it.

  ```rust,ignore
  let scopes = McpScopes::new(Arc::new(move |session| {
      let user = users.get(session)?;                    // your own map
      Some(McpScope {
          key: user.0.clone(),                           // opaque cache key
          servers: my_db.mcp_servers_of(user),           // the config `mcp:` shape
          token_store: Some(user_scoped(store.clone(), user.clone())),
      })
  }), http_client.clone(), secret_env);
  ```

  The resolver runs per discovery read and per MCP dispatch — keep it an
  in-memory lookup, never I/O. Optionally await `scopes.prewarm(&session)`
  between `Spawn` and the first `Prompt` so `list_mcp` already shows the
  scope's tools (otherwise the first `mcp_tools`/`mcp(...)` connects); call
  `scopes.evict_scope(key)` on logout or whenever a user's server set or
  credentials change (the cache keys on `key` alone). An `oauth:` server
  whose slice holds no token fails the call with
  ``MCP server `name` requires authorization for this user; …`` — catch that
  string shape to prompt the user through the `WebFlow` handlers above.
  Capability hints (`capabilities:` on a scope's server entries) don't reach
  the in-tree profile expansion, which is process-global — fold
  `mcp::capability_index(&scope.servers)` into your own per-user
  `PermissionResolver` if you grade by capability.

---

Previous: [§4 — Custom tool execution / policy](tool-execution.md) · Next: [Migration notes](migrations.md)
