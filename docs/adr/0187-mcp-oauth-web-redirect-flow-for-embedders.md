# 0187. MCP OAuth web-redirect flow for embedders

- Status: Accepted
- Date: 2026-08-13
- Amends [ADR-0153](0153-mcp-server-oauth.md)/[ADR-0182](0182-mcp-oauth-device-code-flow-and-closed-refresh-race.md)
  (a third authorization mode joins the loopback and device-code flows).
  Part of [#684](https://github.com/xmiksay/entanglement/issues/684), on the
  storage seam [ADR-0184](0184-provider-hosted-multi-user-seams.md) shipped.

## Context

ADR-0184 gave a multi-user embedder per-user MCP credential *storage*
(`UserTokenStore` + `user_scoped`), but no way to *mint* a credential from a
web app. Both in-tree flows assume a local process: `AuthFlow` binds a
loopback listener and blocks `complete()` on the browser redirect to
`127.0.0.1`; `DeviceFlow` works headless but requires RFC 8628 on the
authorization server and trades UX for it. A server-side embedder (the #684
consumers are Rust/Axum web apps) instead needs the redirect to land on its
own HTTPS callback endpoint, with the flow split across two of its HTTP
requests — possibly served by two different replicas.

## Decision

1. **`provider::mcp::auth::web::WebFlow`** — a sibling module (the `DeviceFlow`
   precedent: one authorization mode = one module).
   `WebFlow::begin(server, mcp_url, cfg, hint, redirect_uri, client_name)`
   prepares the same authorization-code + PKCE request as `AuthFlow` but
   against the caller's `redirect_uri`, binding nothing and never blocking on
   the user; `cfg.redirect_port` is ignored. The DCR `client_name` is
   caller-supplied — the consent screen names the embedder's product, not
   "skutter" (`AuthFlow` keeps its constant).
2. **The shared pre-human half is extracted, not duplicated**:
   `flow::prepare(…) -> PreparedAuthorization` (discovery → client resolution
   → PKCE/state → authorize URL), `pub(super)`, used by both redirect flows.
   `AuthFlow::begin` delegates to it; behavior is byte-identical.
3. **`PendingWebAuthorization` is serializable plain data** (`Serialize`/
   `Deserialize`, flattened fields mirroring `StoredAuth`'s shape): a
   multi-replica embedder cannot guarantee the callback request lands on the
   replica that ran `begin`, so the pending entry must round-trip through its
   shared store, keyed by `state()`. That store then briefly holds the PKCE
   verifier and any `client_secret` — accepted, because the same store holds
   strictly more sensitive material long-term (`StoredAuth` persists the
   `client_secret` and refresh tokens) and the verifier is single-use,
   worthless without the state-bound code. The compensating control is a
   hand-written `Debug` redacting both (the ADR-0153 invariant). Pending-entry
   TTL is the embedder's job — nothing here expires it.
4. **`complete(code, state)` verifies `state` before any network I/O** (CSRF /
   stale-replayed callback → refused with no token-endpoint contact), then
   exchanges and returns the same `StoredAuth` the other flows produce — the
   embedder saves it into its per-user store (`user_scoped`).
5. **Core does not re-export it.** Like `user_store`, `WebFlow` is
   embedder-facing; the runtime/TUI never drive it (they keep loopback +
   device-code). Embedders depend on `entanglement-provider` directly
   (ADR-0181 boundary).

## Consequences

- **(+)** A web embedder mints per-user MCP credentials with two small
  handlers (redirect out, callback in) — recipe in `docs/embedding.md` §7.
- **(+)** `flow.rs`/`web.rs` share discovery/DCR/PKCE/URL-building; the split
  keeps both under the file cap.
- **(−)** A pending entry at rest in the embedder's store carries the PKCE
  verifier; the serde derive makes that a deliberate, documented contract
  (a unit test pins the field's presence in the JSON).
- **(neutral)** `device.rs` is not refolded onto `prepare` (different grant
  types, no redirect) — optional cleanup, not blocking.

## References

- [ADR-0153](0153-mcp-server-oauth.md) / [ADR-0182](0182-mcp-oauth-device-code-flow-and-closed-refresh-race.md) — the flows this joins.
- [ADR-0184](0184-provider-hosted-multi-user-seams.md) — the per-user store the result lands in.
- [#684](https://github.com/xmiksay/entanglement/issues/684) — the consumer trigger.
