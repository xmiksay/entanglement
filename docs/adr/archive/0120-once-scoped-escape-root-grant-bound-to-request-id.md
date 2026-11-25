# 0120. `Once`-scoped escape-root grants are bound to the approving `request_id`

- Status: Accepted
- Date: 2026-07-20
- Amends: [ADR-0109](0109-escape-root-access-via-approval.md)

## Context

[ADR-0109](0109-escape-root-access-via-approval.md)'s `ExtraRootStore` records
an approved out-of-root grant keyed by `(tool, resolved-absolute-path)` alone.
For `Session`/`Always` that key space is exactly right — the whole point of a
durable grant is that *any later call* to that `(tool, path)` should ride it
silently. For `Once` it is wrong: a single-use token is meant to authorize
*the one call that was just approved*, but nothing recorded which call that
was — `ExtraRootStore::take_allowance` matched (and removed) the first `once`
entry for `(tool, path)`, full stop.

Tool execution runs on detached, per-call executor tasks
(`tool_runner::dispatch` spawns one per `ToolExec`; a model turn batch-emits
every call up front, [ADR-0061](0061-parked-turn-state-batch-tool-resolution.md)),
so multiple calls to the identical `(tool, path)` in flight at once is a normal
shape, not a corner case — e.g. two `read` calls against the same out-of-root
file in one batch, each independently forcing its own `Ask` since neither is
durably allowed yet. Whichever of their `Once` approvals happened to `insert`
into the (deduping) `once: HashSet<(tool, path)>` last silently overwrote
nothing extra, and whichever call's `resolve_under_root_or_grant` happened to
call `take_allowance` first consumed the single set entry — possibly the call
that was *not* the one just approved, or, since insertion is idempotent on a
`HashSet`, only one of two separately-approved `Once` grants existed at all,
so a legitimately-approved second call could see the token already spent by
the first. In no case did an unrelated caller widen its own privileges past
what the ladder graded it — the exposure is entirely within the escape-root
consent flow — but the single-use invariant the user's "just this once" choice
implies did not actually hold under concurrency. Low severity under the local
single-user trust model ([ADR-0047](0047-local-trust-boundary.md)/
[ADR-0048](0048-serve-head-local-trust-model.md)), narrow window, but a real
gap: `Once` is documented as "consumed by *the* next access", not "consumed by
*a* next access".

## Decision

**Bind a `Once` grant to the `request_id` of the call it was approved for, and
match on it at consumption. `Session`/`Always` are unchanged — they keep
matching `(tool, path)` alone, which is the correct semantics for a grant
meant to cover every later call.**

- `ExtraRootStore`'s internal `once` set changes from `HashSet<(tool, path)>`
  to `HashSet<(tool, path, request_id)>`. `record(tool, path, scope,
  request_id)` and `take_allowance(tool, path, request_id)` both gain the new
  parameter; `request_id` is ignored for `Session`/`Always` (recorded/matched
  by `(tool, path)` only, as before) and load-bearing only for `Once`.
- `host::resolve_under_root_or_grant`/`resolve_workdir_or_grant` gain a
  `request_id: &str` parameter threaded straight to `take_allowance` — no
  behavior change for the contained (non-escaping) fast path, which never
  touches the store at all.
- The `request_id` a `Once` grant binds to is the same identifier the
  protocol already uses for the `ToolExec`/`ToolResult` round-trip
  (`entanglement_provider::ToolCall::id`), not a new concept: `tool_runner`'s
  `await_decision` already has it in scope at the point it calls
  `store.record(..)`, and `ToolRegistry::execute` already receives it on the
  `ToolCall` it is handed — it simply wasn't being forwarded past that point.
- **`Tool::run_for_session` gains the missing hop**: `(session, input) ->
  (session, request_id, input)`. `ToolRegistry::execute` forwards `call.id`
  verbatim (the id it already had, now no longer dropped); the default
  implementation ignores the new parameter and delegates to
  [`run_content`][Tool::run_content] exactly as before, so this is a
  source-compatible widening for every tool that doesn't reach for
  `request_id` — confirmed by grep, no tool outside `tools.rs` overrode
  `run_for_session` before this change. The six escape-root-capable host
  tools (`read`/`edit`/`write`/`apply_patch`/`bash`/`call`) now override it:
  each keeps its existing `run`/`run_content` (used by tests and any
  standalone caller) delegating to a private `request_id`-taking helper with
  an empty-string sentinel, and adds a `run_for_session` override that calls
  the same helper with the real id.
- `script.rs`'s rhai bridge mirrors the same binding: `service_binding`
  already mints a per-binding id (`bind_rid = "{request_id}:rhai:{tool}"`,
  distinct from the outer `rhai` call's own id) for its nested approval
  round-trip — that id is now computed once up front and threaded into both
  `store.record(call.tool, abs, scope, &bind_rid)` and the `exec()` call that
  immediately follows it (previously `exec` minted its own, unrelated
  `"rhai:{tool}"` id for the delegated `ToolCall`, so even a same-process,
  same-binding record→consume pair didn't share an id). A `Once` grant a
  script obtains is now redeemed by that exact binding invocation, the same
  guarantee a direct call gets.

## Consequences

- **Positive.** A `Once` approval's single-use invariant now actually holds
  under concurrency: a different in-flight call to the identical `(tool,
  path)` — whether racing, batched, or scripted — can never consume a token
  it wasn't the one approved. Two calls to the same escaping path can each be
  separately approved `Once` and each redeem their own token independently
  (previously the second approval could be a silent no-op against the
  `HashSet`, or the "wrong" call could win the race).
- **Positive.** `Session`/`Always` are untouched in both behavior and
  contract — they were never the source of the gap (a durable grant is
  supposed to cover every later call), so this change deliberately narrows
  its blast radius to the one scope that needed it, per the issue's own fix
  direction ("fall back to path-only matching for durable scopes").
- **Neutral.** [ADR-0109](0109-escape-root-access-via-approval.md)'s
  "Alternatives considered" rejected "thread the approved path into the tool
  via the execution call instead of a shared store" as too invasive for what
  it bought. This ADR does widen `run_for_session` by one parameter, but it
  is a narrower thing than what ADR-0109 rejected: not an approved-path set,
  just the call's own already-existing identifier, added once at the trait
  boundary with a source-compatible default. The shared `ExtraRootStore`
  remains the single source of truth for grants; only the consumption key
  changed shape for `Once`.
- **Negative / cost.** Six host tools each gained a small, mechanical
  `run_for_session` override plus a private helper indirection (`run` stays
  the trait-required entry point for tests/standalone use, now calling the
  helper with an empty-string request id — safe, since a bare
  `ReadTool::new(..).run(..)` call in a test never has a `Once` grant
  recorded against it in the first place).

## Alternatives considered

- **Make `once` a multiset (count) instead of binding to `request_id`.**
  Rejected: this would fix the "second approval is a silent HashSet no-op"
  half of the bug, but not the actual security property wanted — it still
  lets an unrelated concurrent call to the same path spend someone else's
  token, just not lose it to double-insertion. Binding to the caller's own
  identity is the only way to guarantee the token is redeemed by the call it
  was approved for, which is what "approved once, for this" means.
- **Carry the approved-path grant through the execution call instead of
  widening `run_for_session`** (a `tokio::task_local!` set by the executor
  around `tools.execute(..)`, read ambiently by `resolve_under_root_or_grant`).
  Rejected: every tool call already does run inside its own detached task
  (the doc comment on `tool_runner::spawn_tool_executor_with_policy` says so
  explicitly), so a task-local would work, but it makes the request identity
  an invisible, ambient dependency of `host::mod`'s containment check instead
  of an explicit parameter — harder to unit-test without a live tokio runtime
  and task-local scope, and inconsistent with this codebase's preference for
  explicit seams (`SessionId` through `run_for_session`,
  [ADR-0088](0088-session-aware-tool-execution.md); the `EscapeRoot`/
  `ExtraRootStore` structs themselves) over ambient state.
