# 0079. Pluggable `PermissionResolver` + `GrantStore` seams in the tool executor

- Status: Accepted
- Date: 2026-07-15
- Extends the runtime permission model of [#59] and the persisted-grants model of
  [ADR-0052](0052-approval-scope-and-persisted-grants.md); complements the
  runtime-tool-provider seam of [ADR-0067](0067-mcp-client-as-runtime-tool-provider.md).

## Context

`spawn_tool_executor` hard-coded two single-user policy sources: a static
`ProfileRegistry` + base `PermissionProfile` for the `Allow|Ask|Deny` decision,
and `grants::GrantStore::load()` reading an always-allow file from the config
dir. A multi-tenant embedder stores its rules per user in its own DB (allow /
deny / prompt rows, priority-ordered, `*`-prefixed wildcards; an "always allow"
writes a DB rule). To swap those two lookups it had to fork the whole ~350-line
executor, losing the shared interception ladder, spawn/mask gating, hooks, rhai,
and the plan/tasks tools.

The executor already runs each call's decision in a detached async task, and the
sub-agent privilege ceiling (ADR-0024) + physical tool mask (#116) are runtime
policy layered *on top of* the per-call grade. So the seam we need is narrow:
replace *where the grade comes from* and *where an always-allow write goes*,
without moving the clamp/mask/spawn machinery.

## Decision

Introduce two trait objects the executor drives (`entanglement-runtime::policy`):

```rust
#[async_trait] pub trait PermissionResolver {
    async fn resolve(&self, session: &SessionId, tool: &str, input: &str) -> Permission;
}
#[async_trait] pub trait GrantStore {
    fn is_granted(&self, session: &SessionId, tool: &str, arg: Option<&str>) -> bool;
    async fn record(&self, session: &SessionId, tool: &str, arg: Option<&str>, scope: ApprovalScope);
    fn forget_session(&self, session: &SessionId);
}
```

- `spawn_tool_executor_with_policy(holly, tools, profiles, base, active, resolver,
  grants, hooks)` is the low-level entry point; `spawn_tool_executor` /
  `spawn_tool_executor_with_hooks` are thin wrappers that plug in the defaults, so
  the CLI is unchanged.
- **The resolver decides one session's own grade.** The executor snapshots the
  call's ancestor chain (`permission::ancestor_chain`) in its single-threaded loop
  — ordered with the lifecycle fold and the `ToolExec.agent` self-heal — then, in
  the detached task, takes the least-privileged resolver grade across the chain
  (`resolve_effective`). So the ADR-0024 privilege ceiling and spawn/mask gating
  stay in the ladder **on top of** the resolver: a tenant rule can widen or narrow
  a session's own grade but can never widen a child beyond its parent. `apply_grant`
  then upgrades a resolved `Ask` → `Allow` from a grant, after the clamp, exactly
  as before.
- **`record` is async, reads are sync.** An `ApprovalScope::Always` write may hit a
  DB; `is_granted` is a fast pre-prompt check. A multi-tenant store writes its
  "always" rule to the DB and surfaces it on the *next* call through its own
  resolver, so its `is_granted` can just return `false` — the read side is
  deliberately the resolver's job, not a second lookup to keep in sync.
- **Defaults are byte-identical.** `ProfileResolver` shares the same
  `Arc<Mutex<active-profile map>>` the executor folds lifecycle events into and
  returns own-profile clamped by the base ceiling. Because `clamp_to_base` is
  monotonic (`min(clamp(a), clamp(b)) == clamp(min(a, b))`), min-of-clamped over
  the chain equals the pre-seam `effective_permission` + `clamp_to_base`.
  `DefaultGrantStore` wraps the managed-file store, renamed `grants::GrantStore` →
  `grants::FileGrantStore` to free the trait name.

## Consequences

- A multi-tenant embedder swaps two `Arc<dyn …>` and keeps everything else: the
  interception ladder, `agent`/`agent_poll`, hooks, rhai, plan/tasks, cancellation.
- The per-call grade now resolves in the detached task (the DB hit belongs there),
  not synchronously in the loop. The leaf profile is self-healed in the loop before
  the task spawns and ancestors are stable, so the default's read is unaffected;
  only a mid-flight `AgentChanged` for the *same* session could be observed a beat
  later, which is benign.
- `active` is now an `Arc<Mutex<HashMap<SessionId, AgentProfile>>>` shared with the
  default resolver. The loop is the sole writer and never holds the lock across an
  await, so the brief locks never contend.
- `rhai` keeps the profile/base path: its inner bindings resolve permission through
  a separate synchronous `BindingPolicy` snapshot, so routing them through an async
  resolver is out of scope. Its own `Ask` still upgrades from the (sync) grant read.

## Alternatives rejected

- **Resolver returns the fully-clamped effective grade (walks the chain itself).**
  Then a custom resolver — which does not know the runtime's spawn tree — would
  skip the ADR-0024 clamp, letting a tenant rule widen a child beyond its parent.
  The clamp must live in the executor, so the executor walks the chain and calls
  the resolver per session.
- **A single `record_always(session, tool, decision)` on `GrantStore`** (the issue
  sketch). Too narrow to back the default file store, which also needs session-
  scoped grants, the `is_granted` read, and per-session cleanup. The three-method
  trait is the honest surface the executor uses; a multi-tenant store trivially
  no-ops the reads.
- **Keep resolution synchronous in the loop, make only the custom path async.** A
  trait cannot be "sometimes sync", and a DB resolver awaited in the loop would
  serialize all tool dispatch behind one call. Resolving in the detached task is
  what "the ladder already runs in an async task" buys us.
- **Fold the config ceiling and rhai through the resolver too.** Would make the
  seam total but ripple the async boundary through `BindingPolicy`'s synchronous
  per-binding resolution for no acceptance-driven gain. Deferred.

[#59]: https://github.com/xmiksay/entanglement/issues/59
