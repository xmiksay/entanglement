# 0184. Provider-hosted multi-user seams: per-user token store + relocated provider context

- Status: Accepted
- Date: 2026-08-12
- Implements [ADR-0181](0181-userid-leaves-the-runtime-crate.md) for #687
  (first half — the provider-side seams; the runtime-side removal + grep gate
  land as the second half of #687). Amends
  [ADR-0153](0153-mcp-server-oauth.md) (the auth mechanism gains a per-user
  storage interface) and relocates
  [ADR-0147](0147-multi-user-mode-embedder-api.md)'s provider-context seam.

## Context

ADR-0181 fixed the direction: `UserId` must not appear anywhere in
`entanglement-runtime`; the provider crate hosts the universal user-aware
auth/token interface; per-user seams are things an embedder implements and
hands in. Two concrete seams need a home before the runtime's `multi_user::*`
modules can be deleted:

1. **Per-user MCP OAuth credentials** (the #684 prerequisite).
   [ADR-0153](0153-mcp-server-oauth.md)'s `TokenStore` is per-*server*: the
   shape every per-connection consumer (`StoredTokenSource`, `check`, the
   auth flows) binds at connect time, implemented once by a single-user
   process over one credential file. Multi-user needs the same contract
   widened by a user — without touching any existing consumer.
2. **Per-user provider context** (catalog + API keys + the ADR-0175 budget).
   `runtime::multi_user::provider` already held a working implementation —
   but in the wrong crate. Everything it names (`Catalog`, the three wire
   factories, `ModelResolver`, `ResolvedModel`, `HttpClient`, `UserBudget`,
   `UserId`) is provider-owned; the module never needed runtime types at all.

## Decision

1. **`provider::mcp::auth::user_store`** — the universal per-user credential
   interface:
   - `UserTokenStore`: `load`/`save`/`delete`/`servers` keyed
     `(&UserId, server)`, plus `with_exclusive` mirroring
     [ADR-0182](0182-mcp-oauth-device-code-flow-and-closed-refresh-race.md)'s
     refresh-race critical section per user (same plain load→save default).
   - **`user_scoped(store, user) -> Arc<dyn TokenStore>`** — the whole
     multi-user bridge: one user's slice presented as the plain per-connection
     `TokenStore` every existing consumer already takes. `TokenStore`/
     `AccessTokenSource` themselves are untouched; a constant user degrades to
     exactly the single-user behavior.
   - `InMemoryUserTokenStore` reference impl (embedders with real storage
     implement the trait directly).
2. **`provider::multi_user`** — `UserProviderContext`, `UserProviderStore`,
   `InMemoryUserProviderStore`, `build_user_model_resolver` move from
   `runtime::multi_user::provider` near-verbatim (imports go crate-local; the
   `ModelResolver` seam contract, the hard-`Err` on a missing user, and the
   ADR-0175 `with_user_budget` layering are unchanged). The runtime module is
   then deleted in #687's second half.
3. **What stays embedder-side as a recipe, not code**: session→user mapping
   (the embedder already knows it at `Spawn`; a `HashMap<SessionId, UserId>`
   of its own), and per-user *permission* ceiling/grants — those build on the
   runtime's `PermissionResolver`/`GrantStore` traits, which are already
   session-keyed pluggable seams; a per-user impl belongs to the embedder
   (documented in `docs/embedding.md`, sketched in `examples/embedded.rs`).
   Neither can live in the provider crate (they name runtime policy types),
   and putting them back in the runtime keyed by `UserId` is exactly what
   ADR-0181 forbids.

## Consequences

- **(+)** #684 (per-user MCP tokens) now has its storage seam: a per-user
  connection is built with `user_scoped(store, user)`; single-user keeps the
  file-backed store; nothing downstream can tell the difference.
- **(+)** The provider crate is self-contained for multi-user model
  resolution — an embedder needs only `entanglement-provider` types to build
  a per-user `ModelResolver` and hand it to the engine seam.
- **(+)** All existing auth consumers, wire clients, and single-user call
  sites are byte-identical — both additions are purely additive.
- **(−)** Per-user permission remains recipe-only (no compiled reference in a
  library crate); the `examples/embedded.rs` sketch compiles in CI, which is
  the guard against rot.
- **(neutral)** `provider::multi_user` deliberately keeps ADR-0147's
  semantics (missing user = hard `Err`) — the null/constant-user degradation
  ADR-0181 requires applies to *seams the runtime consults*; a multi-user
  resolver is only ever wired by an embedder that has users.

## Alternatives considered

- **Widen `TokenStore`/`AccessTokenSource` with an `Option<&UserId>`
  parameter.** Rejected: every existing consumer and implementor would carry
  a parameter that is `None` in all single-user code, and the per-connection
  binding (client + store fixed at connect time) already expresses "which
  user" structurally — the adapter encodes it once instead of threading it
  per call.
- **Keep the reference impls in the runtime behind a feature gate.**
  Rejected: a `UserId`-keyed runtime module is what ADR-0181 removed; a
  feature gate hides it from the default build but not from the architecture.
- **Move the per-user model resolution into `entanglement-core`.** Rejected:
  everything it names is provider-owned; core would only re-export it. The
  provider crate is the natural, dependency-free home (`UserId` is defined
  there).
- **Demote the provider-context code to `examples/`.** Rejected: it is real,
  tested, wire-complete construction logic (three factories, thinking-style
  and web-search plumbing) that every multi-user embedder would otherwise
  copy-paste and let drift; relocation keeps it compiled and tested.

## References

- [ADR-0181](0181-userid-leaves-the-runtime-crate.md) — the direction this
  implements; [#687](https://github.com/xmiksay/entanglement/issues/687).
- [ADR-0153](0153-mcp-server-oauth.md) /
  [ADR-0182](0182-mcp-oauth-device-code-flow-and-closed-refresh-race.md) —
  the auth mechanism + refresh-race section the per-user store mirrors.
- [ADR-0147](0147-multi-user-mode-embedder-api.md) /
  [ADR-0175](0175-per-user-admission-gate-on-a-shared-literal-key.md) — the
  provider-context semantics and admission gate carried over unchanged.
- [#684](https://github.com/xmiksay/entanglement/issues/684) — the consumer
  this unblocks.
