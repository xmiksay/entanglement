# 0181. `UserId` leaves the runtime crate — multi-user seams are embedder-side and session-keyed

- Status: Accepted
- Date: 2026-08-12
- Supersedes: [ADR-0174](0174-authenticated-multi-user-wire-head.md) (whole —
  the authenticated multi-user `serve` head is removed, #686);
  [ADR-0147](0147-multi-user-mode-embedder-api.md) **in part** — its
  runtime-module prescription (`entanglement-runtime::multi_user::provider`,
  `multi_user::permission`, `SessionUserRegistry`) is superseded (#687), while
  its protocol/identity core (the `UserId` newtype in the provider crate,
  `Session.user` / `InMsg::Spawn.user` wire identity, spawn-time inheritance,
  per-user isolation goals) stands unchanged.
- Related: [ADR-0175](0175-per-user-admission-gate-on-a-shared-literal-key.md)
  (the admission gate itself lives provider-side and stays; only its runtime
  wiring moves), [ADR-0153](0153-mcp-server-oauth.md) (the provider-owned
  OAuth mechanism this decision extends into a universal interface),
  [ADR-0048](0048-serve-head-local-trust-model.md) (the posture `serve`
  returns to), #684 (per-user MCP tokens, the work that surfaced the drift).

## Context

[ADR-0147](0147-multi-user-mode-embedder-api.md) shipped multi-user mode as an
*embedder library API* — the right scope — but materialized it as
`UserId`-keyed modules **inside the runtime crate**: `multi_user::provider`,
`multi_user::permission`, and the `SessionUserRegistry` session→user
directory. [ADR-0174](0174-authenticated-multi-user-wire-head.md) then built
an authenticated multi-user `serve` head on top (#674), binding `UserId` to
WebSocket connections in-tree.

The pattern started compounding: open PR #683 adds a fourth `UserId`-keyed
runtime module (`multi_user::aux`), the plan for per-user MCP OAuth tokens
(#684) was about to add a fifth (`multi_user::mcp`), and PR #685's ADR text
prescribes that same shape for future MCP work. Each new per-user feature was
growing another `UserId`-keyed store in the runtime.

But the runtime has no intrinsic use for user identity: **no host tool needs
to know a user** — `skutter`'s own heads (TUI, `run`, `pipe`, default
`serve`) are single-user, and every genuinely multi-user caller is an
out-of-crate embedder that *already knows* which user each session belongs to
(it chose `user` when it sent `InMsg::Spawn`). Keeping per-user policy stores
in the runtime duplicates the embedder's knowledge behind a registry it must
keep synchronized, and turns every new per-user feature into a new runtime
module instead of a closure the embedder hands in.

## Decision

1. **`UserId` does not appear anywhere in `entanglement-runtime`** — neither
   in library seams nor in heads. The type stays where it belongs: defined in
   `entanglement-provider` (`llm.rs`), re-exported by core, riding the wire
   in `InMsg::Spawn { user }` / `OutEvent::SessionStarted { user }` for
   embedders. A dep-gate-style check (grep over `entanglement-runtime/src`)
   enforces this alongside `make tree` / `make check-lean` once #687 lands.
2. **Runtime seams are `SessionId`-keyed closures/traits.** Where the runtime
   must make a per-tenant decision at call time (tool execution, permission
   resolution, token lookup), it consults an embedder-supplied closure/trait
   keyed by `&SessionId`; the session→user mapping is the embedder's private
   concern. A null / empty / constant user must degrade byte-identically to
   today's single-user behavior — single-user `skutter` never notices.
3. **The provider crate hosts the universal auth interface.** The OAuth/token
   mechanism [ADR-0153](0153-mcp-server-oauth.md) already placed in
   `entanglement-provider` extends into a universal, user-aware auth/token
   interface (per-user token storage as a provider-level interface). The
   runtime only plugs its wiring in. This reverses the earlier guardrail
   "per-user stays a runtime concern": *mechanism and its interface* are
   provider-side; the runtime carries neither.
4. **The authenticated multi-user `serve` head is removed** (#686, reverting
   #674). `serve` returns exactly to
   [ADR-0048](0048-serve-head-local-trust-model.md)'s local, single-user,
   loopback posture. No in-tree head ships multi-user; a deployment that
   needs an authenticated wire head builds it as an embedder, out of tree,
   with its own identity store.
5. **The existing runtime modules migrate** (#687):
   `multi_user::provider` / `multi_user::permission` / `SessionUserRegistry`
   are removed in favor of the seams in (2)/(3).
   [ADR-0175](0175-per-user-admission-gate-on-a-shared-literal-key.md)'s
   per-user admission gate is unaffected in substance — `HttpClient::
   with_user_budget` is already provider-side; only the runtime-side
   `resolve_for_user` wiring moves to the embedder.
6. **Open PRs conform before merge.** PR #683 drops `multi_user::aux` (its
   per-user aux pins ride a session-keyed seam); PR #685's ADR text replaces
   its "mirror the `UserProviderStore` pattern, thread `UserId` through
   `McpAuth`" revisit note with this ADR's direction. #684 (per-user MCP
   tokens) is re-planned on the migrated base.

## Consequences

- **(+)** One rule instead of a per-feature judgment call: a per-user feature
  adds a provider interface + an embedder closure, never a runtime module.
  The #684 design collapses accordingly (no `multi_user::mcp`, no
  `UserMcpTokenStore` in the runtime).
- **(+)** The embedder stops double-bookkeeping: it already knows the
  session→user mapping; no in-tree registry to populate and keep in sync.
- **(+)** Single-user `skutter` is untouched at every step — the null/
  constant-user degradation is a hard acceptance criterion of #686/#687.
- **(−)** **Feature removal**: `serve --auth-tokens` (shipped by #674) goes
  away. Multi-user over a wire requires writing an embedder until someone
  builds an out-of-tree head against the seams.
- **(−)** Two open PRs (#683, #685) need rework before merge; both must also
  renumber their ADRs (each independently claimed 0181 — this record takes
  the number, being first to `master`).
- **(neutral)** ADR-0147's wire identity is untouched: `UserId` on
  `InMsg::Spawn`, spawn-time inheritance, and per-user isolation goals all
  stand; only *where the per-user machinery lives* changes.
- **(neutral)** This ADR ships no code (the ADR-0174 precedent): #686 and
  #687 implement it; architecture docs update with those changes, not here.

## Alternatives considered

- **Keep extending the `multi_user::*` pattern** (the original #684 plan:
  `multi_user::mcp` mirroring PR #683's `multi_user::aux`). Rejected: every
  per-user feature would keep adding a `UserId`-keyed runtime store for
  knowledge the embedder already holds, and the runtime — whose own heads are
  single-user — becomes the de-facto tenant-policy layer.
- **Exempt heads ("a head is an in-process embedder") and keep the
  authenticated `serve`.** Rejected: defensible in principle, but it keeps
  `UserId` in the crate, keeps the `SessionUserRegistry` alive as shared
  plumbing, and keeps in-tree pressure to grow multi-user features. A wire
  head serious enough to authenticate users has an identity store and can be
  an embedder.
- **A per-session MCP resolution seam on core's `EngineConfig`** (like
  `model_resolver`). Rejected: MCP tool execution and the OAuth responder
  live entirely in the runtime — core never dispatches MCP — so the core
  config would only be a parcel shelf the runtime fishes a closure out of.
- **Tool-name namespacing for per-user MCP** (`mcp__<user>__<server>__...`).
  Rejected in the #684 planning already: blows up registry cardinality by
  users × tools and leaks user identity to the model.

## References

- Issues [#686](https://github.com/xmiksay/entanglement/issues/686)
  (serve-head removal), [#687](https://github.com/xmiksay/entanglement/issues/687)
  (seam migration), [#684](https://github.com/xmiksay/entanglement/issues/684)
  (per-user MCP tokens, re-planned on the new base)
- PRs [#683](https://github.com/xmiksay/entanglement/pull/683),
  [#685](https://github.com/xmiksay/entanglement/pull/685) — reworked to
  conform before merge
- [ADR-0147](0147-multi-user-mode-embedder-api.md),
  [ADR-0174](0174-authenticated-multi-user-wire-head.md),
  [ADR-0175](0175-per-user-admission-gate-on-a-shared-literal-key.md),
  [ADR-0153](0153-mcp-server-oauth.md),
  [ADR-0048](0048-serve-head-local-trust-model.md)
