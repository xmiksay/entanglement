# 0182. MCP OAuth: device-code flow, and closing the cross-process refresh race

- Status: Accepted
- Date: 2026-08-10
- Amends: [ADR-0153](0153-mcp-server-oauth.md) "Consequences" (two of its four
  accepted-for-v1 gaps close here; the other two stay deferred, narrower)
- Issue: [#631](https://github.com/xmiksay/entanglement/issues/631)
  (orig. tui-ux-batch Issue 3), part of [#624](https://github.com/xmiksay/entanglement/issues/624)

## Context

ADR-0153's "Consequences" listed four edges scoped out of MCP OAuth v1:

- **(a)** no device-code flow, so a host reachable only over SSH — no
  browser, and no way to forward the ephemeral loopback port — cannot
  authorize at all. The printed authorize URL only helps when the port *is*
  forwarded; the redirect still has to land on the host running `skutter`.
- **(b)** credentials are process-global, keyed by server name — the
  multi-user embedder API ([ADR-0147](0147-multi-user-mode-embedder-api.md))
  has per-user providers/keys/grants but not per-user MCP tokens.
- **(c)** cross-process refresh is racy: the token file's `fd-lock`
  (`entanglement-runtime/src/config/lock.rs`) serializes the *write* in
  `McpTokenStore::save`, but the token *exchange* — the network POST in
  `entanglement-provider/src/mcp/auth/token.rs::refresh_token` — ran entirely
  outside it. Two `skutter` instances refreshing the same rotating grant at
  the same instant could both redeem the same refresh token; the loser got a
  failed request and had to re-authorize by hand.
- **(d)** OAuth is wired for MCP servers only, not LLM provider endpoints,
  even though the mechanism lives in `entanglement-provider` where such a
  consumer would sit.

Tracked as deferred-work-ledger row 11 (`docs/deferred-work-ledger.md`) once
#624 gave every ledger row a live issue. None of the four is a correctness
defect on its own — (a)/(d) were simply unbuilt scope, (b) is bounded by the
single-user `serve` trust model ([ADR-0048](0048-serve-head-local-trust-model.md)),
and (c) self-healed by re-authorization — but (a) is a real usability dead
end for the headless-SSH case the OAuth mechanism exists to serve, and (c) is
a latent correctness gap that only *happens* not to bite because it heals
itself. Both are now closed.

## Decision

### (a) RFC 8628 device-code flow ships as `/mcp connect <name> --device-code`

A new `DeviceFlow` (`entanglement-provider/src/mcp/auth/device.rs`) sits
alongside `flow.rs`'s `AuthFlow`, sharing discovery and dynamic client
registration but replacing "open a browser and catch a loopback redirect"
with RFC 8628's device-authorization grant: the client posts to a
`device_authorization_endpoint` (an RFC 8414 field discovery didn't parse
before this; `Endpoints`/`AuthServerMetadata` now carry it, with a
`oauth.device_authorization_url` config override for a server that doesn't
publish it) and gets back a `device_code`, a short `user_code`, and a
`verification_uri` the user opens on *any* device — not the host running
`skutter`. The client then polls the token endpoint
(`grant_type=urn:ietf:params:oauth:grant-type:device_code`) until the user
finishes, honoring `authorization_pending` (keep polling at the declared
`interval`), `slow_down` (`interval += 5s`, per RFC 8628 §3.5), and treating
every other error response as terminal. No PKCE: RFC 8628 has no redirect for
PKCE to protect.

Dynamic client registration ([RFC 7591](0153-mcp-server-oauth.md)) needed
generalizing: `dcr::register` took a hardcoded `redirect_uris`/
`response_types: ["code"]` shape tied to the authorization-code grant. It now
takes `redirect_uri: Option<&str>` and an explicit `grant_types: &[&str]`, so
a device-only registration correctly declares
`["urn:ietf:params:oauth:grant-type:device_code", "refresh_token"]` with no
redirect URI at all.

This is **opt-in via an explicit flag**, not auto-detected. Heuristically
guessing "no browser reachable" would need to probe for a display/browser or
a working loopback round-trip — unreliable over SSH port-forwarding setups —
and ADR-0153 already established that opening the user's browser on `/mcp
connect` is the obvious default, not something to second-guess. A user who
knows their host is headless asks for the device flow explicitly.

Wire/UX surface: `McpAuthAction` gains `ConnectDeviceCode`; `McpAuthStatus`
gains `user_code: Option<String>` on the interim `McpAuthChanged` event
(`authorize_url` carries the plain `verification_uri`, always shown — a
headless session has nowhere to open a pre-filled complete-URL anyway;
`skutter` still *tries* opening `verification_uri_complete` when the server
offers one, exactly as ADR-0153's "always report the URL whether or not the
launch succeeded" already does for the browser flow). Every head renders
`user_code` next to the URL when it's present.

### (c) The store's cross-process lock now covers the exchange, not just the write

`TokenStore` gains a new object-safe method,
`with_exclusive(&self, server, f: Box<dyn FnOnce(Option<StoredAuth>) -> Result<StoredAuth>>)`,
whose default implementation is today's behavior (load, call `f`, save — no
extra locking, correct for an embedder whose store isn't shared across
processes). `McpTokenStore` overrides it to take the file lock **once** for
the whole load → maybe-refresh → save sequence, reusing its own
lock-internal read/persist helpers directly rather than nesting a second
`with_locked_file` call through `save()` (which would deadlock: `fd_lock`
locks are per open-file-description, not reentrant even within one process).

The refresh itself (`token::refresh_locked`) runs on a `spawn_blocking`
thread: acquiring the cross-process lock can block for as long as another
process's own exchange takes, and — since a `fd_lock` guard is tied to a
borrow of the `RwLock` it came from and can't be held across an `.await`
without a self-referential struct — the whole critical section runs
synchronously, bridging into the async `refresh_token()` call via a small
nested single-threaded Tokio runtime built on that blocking-pool thread. This
is the standard sync-to-async bridging pattern (building and `block_on`-ing a
runtime from a `spawn_blocking` closure is safe precisely because tokio's
"already inside a runtime" guard is a thread-local set only on worker
threads, never on the blocking pool). A racing process that loses the lock
acquisition re-checks the token it's handed inside the critical section
before touching the network — if the winner already refreshed it, the loser
returns that token and never spends an exchange at all.

`StoredTokenSource::access_token` keeps its own in-process
`tokio::sync::Mutex` fast path ahead of this (same-process callers still
avoid the `spawn_blocking` + nested-runtime cost when the token is already
fresh), and `check()`'s (`/mcp check`) unprotected refresh now routes through
the same `refresh_locked` path — it had the identical race.

### (b) and (d) stay deferred, with narrower scope

Neither is touched here:

- **(b) per-user MCP tokens.** `McpTokenStore` stays process-global, keyed by
  server name only. The direction for the build is fixed by
  [ADR-0181](0181-userid-leaves-the-runtime-crate.md): **not** another
  `UserId`-keyed runtime module — the provider crate hosts the universal
  user-aware auth/token interface (this ADR's `TokenStore` is where it
  grows), the runtime consults `SessionId`-keyed embedder-supplied seams, and
  a null/constant user degrades to today's process-global behavior.
  Re-planned as #684 on the #686/#687 base. **Revisit trigger:** an embedder
  actually wiring multi-user MCP, not just multi-user LLM providers/keys.
- **(d) OAuth for LLM provider endpoints.** Every catalog entry
  (zai/openai/ollama/anthropic/gemini) authenticates with a static API key
  today; none needs OAuth. The mechanism (`AuthFlow`, `DeviceFlow`,
  discovery, DCR, `TokenStore`) is generic OAuth 2.1 plumbing that happens to
  live under `mcp::auth`, not MCP-specific in shape — an LLM consumer could
  reuse it directly. **Revisit trigger:** a concrete OAuth-protected LLM
  endpoint to support.

Both remain deferred-work-ledger row 11, narrowed, with a live tracking issue
(`docs/deferred-work-ledger.md`) since #631 itself closes with this change.

## Consequences

- A headless host reachable only over SSH can now authorize an MCP server:
  `/mcp connect <name> --device-code` prints a short code and a URL to open
  on any other device.
- `dcr::register`'s signature changed (`redirect_uri` became `Option`,
  `grant_types` became an explicit parameter) — every caller in-tree was
  updated; an out-of-tree embedder calling it directly would need the same.
- The cross-process refresh race ADR-0153 accepted for v1 is closed: two
  `skutter` instances can no longer both spend the same rotating refresh
  token. The fix adds a `spawn_blocking` hop + a nested single-threaded
  runtime to the refresh path — a small latency cost only paid when a
  refresh is actually needed (the fast, already-valid path is unchanged).
- `TokenStore` gained `with_exclusive`; any existing out-of-tree implementor
  keeps compiling unchanged (it's a default method) but should consider
  overriding it if its backing store is shared across processes the way
  `McpTokenStore`'s is.
- (b) and (d) remain open, revisit-triggered rather than scheduled.

## Alternatives considered

- **Holding the `fd_lock` guard across the `.await`** via a self-referential
  struct (or an `unsafe` lifetime transmute) instead of the
  `spawn_blocking` + nested-runtime bridge. Rejected: this workspace avoids
  `unsafe` and self-referential-struct tricks for exactly the kind of subtle
  soundness risk this would introduce, for a mechanism (advisory file
  locking around an infrequent token refresh) that isn't latency-sensitive
  enough to justify it.
- **Auto-falling-back to device-code** when the loopback flow's redirect
  never arrives, instead of a `--device-code` flag the user chooses
  up front. Rejected: the failure signal (a timeout) is indistinguishable
  from "the user just hasn't finished authorizing yet," so a fallback would
  either fire too eagerly or too late; an explicit flag is unambiguous and
  matches how the user already knows their own host has no browser.
- **PKCE on the device grant.** Some authorization servers advertise it as
  an extension, but RFC 8628 doesn't require it (there's no redirect to
  protect against injection the way there is for the authorization-code
  grant), so it was left out rather than adding an optional code path for a
  protection the base spec doesn't call for.
