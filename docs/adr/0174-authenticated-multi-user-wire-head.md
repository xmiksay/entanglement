# 0174. Authenticated multi-user wire head — design (not yet built)

- Status: Accepted
- Date: 2026-08-06
- Amends: none. Fulfills the deferral [ADR-0147](0147-multi-user-mode-embedder-api.md)
  named explicitly ("wiring a bearer-token-to-`UserId` authenticated wire head
  is a distinct, orthogonal design problem... that deserves its own ADR once a
  concrete deployment needs it"). `serve` itself is **unchanged** by this
  ADR — [ADR-0048](0048-serve-head-local-trust-model.md) still describes its
  default (and only shipped) posture.
- Related: [ADR-0107](0107-ws-per-connection-approval-ownership.md)
  (per-connection approval ownership this composes with), [ADR-0069](0069-trusted-untrusted-wire-frame-split.md)/[ADR-0124](0124-wire-refused-mcp-mutation-and-stdio-key-scrub.md)
  (the wire-refused allowlist this design leaves untouched), [ADR-0050](0050-per-endpoint-connection-pool-retry-rate-limit.md)
  (per-user rate-limit isolation, already free once a per-user key reaches the
  request path)

## Context

[ADR-0147](0147-multi-user-mode-embedder-api.md) (#522) shipped session-scoped
`UserId` identity, per-user provider/permission seams, and `SessionUserRegistry`
— but as an **embedder library API only**. `serve` was deliberately left
exactly as [ADR-0048](0048-serve-head-local-trust-model.md) scoped it: local,
single-user, loopback-bound, no authentication. Every piece of the multi-user
machinery is reachable today only by an in-process embedder that already knows
which `UserId` a request belongs to and calls the privileged `Holly::send`
directly — no in-tree *wire* transport (`serve`, `pipe`) can bind an untrusted
connection to a `UserId` at all.

[Issue #633](https://github.com/xmiksay/entanglement/issues/633) (ledger row
13, part of the #624 tracking epic) names the three concrete gaps a wire head
would need to close:

1. **Authentication** — some credential a wire client presents that resolves
   to a `UserId`.
2. **`SessionUserRegistry` population from credentials** — today an embedder
   populates this itself because it *is* the trusted caller that sent
   `InMsg::Spawn { user, .. }`; a wire client is never that caller.
3. **Per-user approval ownership** — [ADR-0107](0107-ws-per-connection-approval-ownership.md)'s
   `SessionOwners` already arbitrates `Approve`/`Reject`/`AnswerQuestion`
   between connections, but explicitly only as *robustness* ("two cooperating
   local clients... don't race," not "authenticate who is allowed to approve
   what" — its own Context section names this as the residual gap left to a
   future authenticated head).

Two facts from the existing wire-trust design shape every option here:

- **`InMsg::Spawn` is wire-refused unconditionally** ([ADR-0069](0069-trusted-untrusted-wire-frame-split.md)):
  `send_from_wire` never routes it regardless of connection state, full stop —
  "a forged `Spawn` would bypass the tool path's spawn-refusal gate." A wire
  client cannot mint a root session's `user` field itself under any design
  that keeps this refusal (and this ADR does not propose relaxing it).
- **A wire client never sends `Spawn` today anyway.** `serve`'s root sessions
  come from the **lazy-`Prompt`-creates-a-root-session** path (an unknown
  session id auto-creates a blank session) — the "single-user CLI convenience,
  never the multi-user entry point" ADR-0147 §1 names explicitly. `serve` (the
  **head**, not the client) already holds the privileged `Holly` handle every
  trusted embedder holds; it has simply never had a reason to call `Spawn`
  itself.

That second fact is the load-bearing observation this design rests on: a
wire head does not need a *protocol* change to become a trusted `Spawn`
author — it already can be one. It only needs an authentication step to know
*which* `UserId` to author it with.

## Decision

**`serve` gains an opt-in authenticated mode** (`--auth <backend>`, config
key, or equivalent — the exact CLI surface is for the implementation, not this
ADR); with it unset, `serve` behaves byte-for-byte as ADR-0048 describes
today. Four pieces, each landing on an existing seam:

### 1. Connection-scoped authentication, not per-frame

The WS upgrade handshake requires a credential (e.g. `Authorization: Bearer
<token>` header). A connection whose credential doesn't resolve is refused at
the HTTP upgrade (401) — it never reaches the WS loop, so an unauthenticated
peer can't even open a socket to probe `wire_allowed()`. A successful
resolution binds a `UserId` to that connection's existing `ConnId`
([ADR-0107](0107-ws-per-connection-approval-ownership.md)'s process-lifetime
counter) for the connection's whole lifetime — mirroring ADR-0147's own
"session is the identity boundary, not a per-frame trust boundary" stance,
lifted one level to the connection that will go on to own one or more
sessions. No re-authentication per frame.

Credential resolution is a pluggable `WireAuthenticator` trait
(`fn authenticate(&self, credential: &str) -> Option<UserId>`), the same
trait-seam precedent `PermissionResolver`/`GrantStore`/`UserProviderStore`
already set (ADR-0147 §3): entanglement ships a reference
`StaticTokenAuthenticator` (`HashMap<token, UserId>`) for tests/small
deployments; a real multi-tenant deployment implements it against its own
identity store (a DB, OIDC introspection).

### 2. The head becomes the trusted `Spawn` author

`InMsg::Spawn` stays exactly as wire-refused as it is today — zero change to
`wire_allowed()`, zero change to ADR-0069/ADR-0124's allowlist. Instead, the
authenticated connection handler, on a `Prompt` targeting an unrecognized
session id, calls the **privileged** `holly.send(InMsg::Spawn { session:
<new id>, user: Some(resolved_uid), parent: None, .. })` itself — before
relaying the client's `Prompt` through the ordinary `send_from_wire` path —
exactly the call ADR-0147's `main.rs`/embedder-library callers already make,
just triggered by a wire event instead of embedder code. A second `Prompt`
reusing an already-live session id is a no-op resolve, not a second `Spawn`.
This needs no core protocol change: the wire client's frames are unchanged
(still a plain `Prompt`), and the trust boundary that already exists (head
code is trusted, wire bytes are not) is the exact one doing the work.

### 3. `SessionUserRegistry` populated by the head

Right after the synthesized `Spawn` resolves (`SessionStarted.user` echoes
it back), the connection handler calls `SessionUserRegistry::register(session,
user)` — the same call ADR-0147 documented as "the embedder populates this
itself... having chosen `user` when it sent the session's `InMsg::Spawn`."
`serve` *is* that embedder now, for exactly the sessions it authenticated.
`forget(session)` on `SessionEnded`/`SessionHibernated` mirrors the existing
lifecycle contract unchanged.

### 4. Per-user approval ownership, layered on ADR-0107

[ADR-0107](0107-ws-per-connection-approval-ownership.md)'s `SessionOwners`
(first-writer-wins, `ConnId`-keyed) is untouched — it still arbitrates
*same-user* multi-connection robustness (two tabs, a tab and a script) exactly
as today. This design adds one prior, stricter check specifically in
authenticated mode: before `touch` runs, a gated frame (`Approve`/`Reject`/
`AnswerQuestion`) is refused outright if `SessionUserRegistry::user_for
(session)` disagrees with the sending connection's own bound `UserId` — a hard
tenant boundary layered ahead of ADR-0107's soft, cooperative one, which keeps
doing its original job unchanged *within* one user's own connections.
Unauthenticated `serve` (the default) never binds a `UserId` to any
connection, so this check is structurally a no-op there — identical behavior
to today.

### 5. Non-loopback bind is a possible follow-up, not designed here

ADR-0048's "loopback is the one required control" reasoning was contingent on
*no authentication existing*, so any local process reaching the socket was
implicitly the one trusted user. Once real authentication exists, binding
non-loopback becomes defensible in principle — but this ADR takes no position
on it. TLS termination, token issuance/rotation/revocation, and rate-limiting
the auth endpoint are the authenticated head's own implementation surface,
explicitly out of scope here (see Consequences). The loopback-only default
stays exactly ADR-0048's for every deployment that never sets `--auth`.

## Consequences

- **(+)** Closes all three gaps #633 named, each by composing an existing seam
  — `UserId`/`SessionUserRegistry`/pluggable-store precedent from ADR-0147,
  `ConnId`/`SessionOwners` from ADR-0107, the unmodified wire-refused `Spawn`
  allowlist from ADR-0069/ADR-0124 — rather than inventing new protocol
  surface. `InMsg`/`OutEvent` need no new variant or field.
- **(+)** `serve`'s default, unauthenticated posture is untouched byte-for-byte;
  ADR-0048 keeps meaning exactly what it always meant for any deployment that
  never opts in.
- **(+)** Per-user approval ownership is strictly additive to ADR-0107 — same-
  user multi-tab robustness is unaffected either way.
- **(−)** **This ADR ships no code.** It fixes the design so a follow-up
  implementation issue has a stable spec to build against; until that lands,
  ledger row 13's underlying limitation ("multi-user reachable only by
  embedding the library") remains true in the shipped binary.
- **(−)** Token lifecycle (issuance, rotation, revocation, expiry) is out of
  scope — delegated entirely to whatever `WireAuthenticator` a deployment
  plugs in; this ADR only fixes the shape of the trait and the connection-scope
  binding.
- **(−)** TLS/transport security for a non-loopback bind is out of scope — a
  deployment terminating TLS in front of it (reverse proxy) is the assumed
  posture, not a native TLS listener this design adds.
- **(−)** A leaked/stolen bearer token grants full access to that user's
  sessions for the token's effective lifetime; no additional per-request
  re-auth is designed (deliberate, mirrors ADR-0147's session-scoped-not-per-
  frame identity stance — see Alternatives).
- **(−)** `WireAuthenticator::authenticate` runs once per connection, so a
  revoked token already bound to a live connection keeps working until that
  connection closes — a deployment needing faster revocation must close the
  connection itself (e.g. from its own credential store's revocation hook),
  which this design doesn't provide a mechanism for.

## Alternatives considered

- **Per-frame credential** (every `InMsg` wire-carries a token, re-validated
  each call) instead of once-per-connection binding. Rejected: mirrors
  ADR-0147's own rejected "thread `UserId` per-frame" alternative — a
  connection is already a continuous identity boundary once authenticated (the
  WS connection's own lifetime), so re-checking every frame adds cost with no
  isolation benefit a connection-scoped bind doesn't already give.
- **Widen `wire_allowed()` to admit `Spawn` when a connection is
  authenticated.** Rejected: `wire_allowed()` is a single global function with
  no per-connection context (by design — [ADR-0124](0124-wire-refused-mcp-mutation-and-stdio-key-scrub.md)
  made it an explicit, connection-agnostic allowlist specifically so a new
  variant is refused-by-default). Threading connection state into it would
  reopen the exact forgery ADR-0069 closed for every *unauthenticated* `pipe`/
  `serve` connection too. Making the **head** the trusted `Spawn` author (§2)
  gets the same outcome — a wire-triggered root session carrying the right
  `user` — with no core change and no new attack surface: it's the identical
  privileged path every embedder already uses.
- **A wholly separate binary/subcommand for the authenticated head** instead
  of an opt-in `serve --auth` mode. Rejected: would duplicate the axum router,
  `SessionOwners`, ping loop, and origin-check machinery ADR-0048/ADR-0107
  already built. An opt-in flag keeps one WS implementation with two postures
  — the same pattern the codebase already uses for `--allow-origin` and
  `ENTANGLEMENT_SANDBOX` — instead of forking the transport per posture.
- **A single, built-in auth mechanism** (e.g. a fixed static-token file)
  instead of a pluggable `WireAuthenticator` trait. Rejected: a deployment
  serious enough to need this ADR at all almost certainly already has an
  identity provider (OIDC, a users table); hardcoding one mechanism would
  immediately need its own escape hatch. Follows the `PermissionResolver`/
  `GrantStore`/`UserProviderStore` trait-seam precedent directly instead of
  inventing a narrower one.
- **Ship the implementation in this same change.** Rejected per the issue's
  own scoping — #633 asks specifically for the ADR, not the build; the
  implementation is substantial (new connection-auth plumbing, a trait +
  reference impl, `ServeState` changes, tests for the ownership composition)
  and deserves its own PR against a spec that isn't also moving underneath it.

## References

- Issue [#633](https://github.com/xmiksay/entanglement/issues/633) (ledger row
  13, part of the [#624](https://github.com/xmiksay/entanglement/issues/624)
  tracking epic): "authenticated multi-user wire head needs its own ADR"
- [ADR-0147](0147-multi-user-mode-embedder-api.md): the embedder-library-only
  v1 this ADR's design builds on directly — `UserId`, `Session.user`
  inheritance, `SessionUserRegistry`, the pluggable-store precedent
- [ADR-0048](0048-serve-head-local-trust-model.md): `serve`'s local/single-
  user/unauthenticated default — unchanged by this ADR
- [ADR-0107](0107-ws-per-connection-approval-ownership.md): per-connection
  `ConnId`/`SessionOwners` first-writer-wins ownership this design layers a
  hard per-user check ahead of
- [ADR-0069](0069-trusted-untrusted-wire-frame-split.md)/[ADR-0124](0124-wire-refused-mcp-mutation-and-stdio-key-scrub.md):
  the trusted/untrusted frame split and fail-closed wire allowlist this design
  leaves completely unmodified
- [ADR-0050](0050-per-endpoint-connection-pool-retry-rate-limit.md): per-
  endpoint pool keyed by base URL + API-key hash — gives per-user rate-limit
  isolation for free once a per-user key (already how ADR-0147's per-user
  provider context works) reaches the request path over this head too
