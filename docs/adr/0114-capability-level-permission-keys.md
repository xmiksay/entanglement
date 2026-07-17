# 0114. Capability-level permission keys (`read`/`write`/`call`)

- Status: Accepted
- Date: 2026-07-17

## Context

Part of the capability-level tool permissions epic ([#416](https://github.com/xmiksay/entanglement/issues/416)).
A permission profile grades tools **literally** — `bash`, `edit`, `grep`, one
name at a time ([ADR-0003](0003-agent-and-permission-profiles.md),
[ADR-0051](0051-argument-scoped-permission-rules.md)). Writing a profile that
wants "read-only" means spelling out `read: allow`, `grep: allow`, `glob:
allow` every time, and a new read-only tool added later silently falls outside
any existing `read: allow` a profile already wrote. #416 wants `read`/`write`/
`call` as **capability keys** that fan out to every tool sharing that
capability, so a profile author writes intent once.

The membership is small and closed (five host tools + `call`/`rhai` as
general-purpose wildcards), and the fan-out is pure string-key rewriting — it
does not need to reach `entanglement-core`, which must stay dependency-free and
capability-unaware ([ADR-0006](0006-core-dependency-hygiene-gate.md)). The
single chokepoint both surfaces already share —
`entanglement-runtime::agents::permission_from_value` (agent frontmatter *and*
the user-config ceiling, #172) — is where the expansion belongs.

Two tools complicate a clean partition: `call` (argv exec, no shell) and `rhai`
(a sandboxed script bound to the root-contained quintet) can each read, write,
or execute depending on what they're asked to do. Neither is cleanly a member
of exactly one capability — grading them via a single capability's fan-out
would either miss the other two or double-count.

## Decision

### Expansion at parse time, not in `resolve`

`permission_from_value` expands capability keys into literal per-tool
`(pattern, Permission)` rules **before** they ever reach a core
`PermissionProfile` — `PermissionProfile::resolve` is unchanged and stays
capability-unaware. Two new consts in `tool_names.rs` are the whole membership
table:

```rust
pub const CAPABILITIES: &[(&str, &[&str])] = &[
    ("read",  &["read", "grep", "glob"]),
    ("write", &["edit", "write"]),
    ("call",  &["bash"]),
];
pub const MULTI_GROUP: &[&str] = &["call", "rhai"];
```

`call`'s single-group members are `bash` only — the literal `call` tool itself
is [`MULTI_GROUP`], resolved separately (below), because it isn't purely an
exec-capability tool: an unscoped `call` invocation can just as easily read or
write a file as run a command.

### Two-pass expansion

1. **Pre-scan**: collect the grade of every *bare* (no `(...)`) `read`/`write`/
   `call` capability key that's set, plus any bare literal `rhai:` grade (it
   tightens the same computation). Emit their least-privileged (`min`,
   `Deny < Ask < Allow`) grade **first**, as `call: mg` and `rhai: mg`. Doing
   this in a dedicated pre-pass — independent of where in the map each
   capability key sits — is what makes the multi-group grade order-independent;
   emitting it *first* leaves room for a later arg-scoped `call(...)` rule to
   still refine `call` through core's ordinary last-match-wins. Nothing refines
   `rhai` this way (it has no argument), but a later *literal* `rhai: ...` key
   is not a capability key, so it is pushed verbatim in the pass below and can
   still override the pre-scan's `rhai` grade at its own position — the doubled
   push is harmless (a literal key that already needed to be the pre-scan's
   minimum re-asserts the same grade; a literal key that was *not* the minimum
   still gets its own explicitly-requested grade, since a plain tool-name key's
   pre-#418 semantics — verbatim, no filtering — are preserved unconditionally).
2. **In file order**, for each remaining entry: a non-capability key (a literal
   tool name, `*`, or an arg-scoped literal like `edit(src/*)`) is pushed
   verbatim, unchanged from before this ADR; a **bare** capability key pushes
   its single-group members only (`read`⇒read/grep/glob, `write`⇒edit/write,
   `call`⇒bash — never re-emitting `call`/`rhai`, which the pre-scan already
   covered); an **arg-scoped** capability key `cap(g)` pushes `member(g)` for
   each of `cap`'s members, where `call`'s arg-scoped member list additionally
   includes the literal `call` tool (`call`⇒call/bash) — an argument-scoped
   rule can meaningfully restrict `call` by command pattern even though its
   bare grade comes only from the multi-group pre-scan.

Splitting a key into its capability/tool part and optional argument glob reuses
core's `split_rule_key` *semantics* via a small runtime-local mirror
(`agents::split_capability_key`) rather than exposing the private core
function — the expansion logic itself must not leak into core.

### Resolution semantics

- **Single-group members** (`read`/`grep`/`glob`, `edit`/`write`, `bash`)
  resolve through core's existing last-match-wins: a later literal `grep: ask`
  still overrides an earlier `read: allow` fan-out, with no core change needed.
- **Multi-group** (`call`, `rhai`) is least-privilege by construction: their
  grade is the `min` of whatever bare capability/literal-`rhai` grades were
  set, computed once regardless of key order.
- **Command sets** are flat `call(pattern): grade` lines expanding to
  `call`+`bash`, not a nested `{allow: […], deny: […]}` YAML shape — matches
  the existing `tool(pattern)` rule-key ergonomics
  ([ADR-0051](0051-argument-scoped-permission-rules.md)) instead of
  introducing a second rule shape.

### `plan.md`'s behavior change is accepted, not worked around

`plan.md`'s `read: allow` was already a literal, deliberate grant. Under this
ADR it becomes a capability key and now also flips `grep`/`glob` from the
profile's `ask` default to `allow` — both are read-only and already advertised
to the plan agent, so this is accepted as the intended fan-out, pinned by a
unit test (`plan.permission.for_tool("grep") == Allow`) rather than silently
absorbed into the diff.

## Consequences

- Positive: a profile author writes `read: allow` once and every present and
  future read-only tool is covered — no separate `grep: allow`/`glob: allow`
  lines to remember, and no core protocol change (`PermissionProfile` and
  `resolve` are untouched; only the runtime's YAML-to-rules step changed).
- Positive: both consumers of `permission_from_value` (agent frontmatter,
  user-config ceiling #172) get the fan-out for free from one chokepoint —
  no risk of the two surfaces drifting.
- Positive: least-privilege for `call`/`rhai` is a real security property, not
  just convenience — a profile that only means to grant `read`/`write` can't
  accidentally leave the two general-purpose escape hatches wide open.
- Neutral: `call` (the tool) and `call` (the capability key) share a name but
  different member lists (bare: `bash` only; arg-scoped: `call`+`bash`) — this
  conflation is intentional (`call`'s capability key *is* named after the tool
  it's built around) but is exactly the kind of asymmetry that needs the
  comment trail this ADR and the inline docs on `tool_names::CAPABILITIES`/
  `MULTI_GROUP` provide.
- Negative: a profile author who writes a bare literal `rhai:` grade alongside
  a looser `read`/`write`/`call` bare grade may be surprised that `rhai`'s
  *own* key still resolves to its own requested value (only `call` is
  tightened by the multi-group minimum in that case) — a deliberate but subtle
  asymmetry pinned by
  `a_looser_literal_rhai_grade_still_wins_for_rhai_itself` in
  `agents::mod::tests`.
- Deferred (tracked in [`../deferred-work-ledger.md`](../deferred-work-ledger.md)):
  `call`'s capability fan-out has no file-path scoping (only command-pattern
  scoping, since `call`/`bash` have no notion of a target path independent of
  their command line); MCP tools (`mcp__<server>__<tool>`) are not assigned to
  any capability — they fall through as literal, ungrouped tool names.

## Alternatives considered

- **Expand capabilities inside `PermissionProfile::resolve` (core).** Would
  need core to carry the membership table and know which tools are
  multi-group — exactly the capability-unaware, dependency-free posture
  [ADR-0006](0006-core-dependency-hygiene-gate.md) protects. Rejected: the
  fan-out is pure syntax sugar over the existing rule-key shape, entirely
  representable as a parse-time rewrite.
- **A nested YAML shape** (`call: {allow: ["git *"], deny: ["rm *"]}`) for
  command sets. Rejected: introduces a second, incompatible rule shape
  alongside `tool(pattern): grade` for no real gain — a flat list of
  `call(pattern): grade` lines already expresses the same policy and reuses
  the last-match-wins resolution every other rule already has.
- **Static tie-break for multi-group `Ask`.** Considered resolving a mixed
  `read: allow, write: ask` down to a single deterministic grade for
  `call`/`rhai` via some priority order. Rejected: `Ask` already means "surface
  a prompt at execution" — there is nothing to statically tie-break; `min`
  naturally resolves to the most conservative grade and lets an actual `Ask`
  surface when warranted.
- **Treat `call` (capability) and `call` (tool) as fully separate namespaces**
  (e.g. rename the capability key `exec`). Rejected: #416's membership table
  and issue text specify `call` as the capability name; renaming it would
  read naturally in isolation but disagree with the epic's public vocabulary
  for no functional benefit.
