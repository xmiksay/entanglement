# 0130. `rhai` `exec`/`bash` bindings marshal `workdir`

- Status: Accepted
- Date: 2026-07-22
- Amends: [ADR-0115](0115-rhai-exec-bindings-call-bash.md), [ADR-0116](0116-workdir-scoped-permission-rules-for-bash-call.md)

## Context

Ledger row 2 ([#396](https://github.com/xmiksay/entanglement/issues/396) epic,
filed as [#480](https://github.com/xmiksay/entanglement/issues/480)), sourced
from [ADR-0116](0116-workdir-scoped-permission-rules-for-bash-call.md)'s own
"the rhai binding grade is not touched" carve-out: "Extending the bindings
with their own `workdir` parameter, if ever wanted, is separate future work."

[ADR-0115](0115-rhai-exec-bindings-call-bash.md) (#419) gave `rhai` scripts
`exec(command)`/`exec(command, args)`/`bash(command)`, each marshalling
`{command, args?, timeout}` into the delegated `call`/`bash` tool's JSON
input — no `workdir` field, ever. [ADR-0116](0116-workdir-scoped-permission-rules-for-bash-call.md)
(#425) then added workdir-scoped permission rules (`tool{pattern}`,
`PermissionProfile::resolve_scoped`) for `bash`/`call`, extracted from that
same JSON input by `permission::permission_workdir`. Because the rhai
bindings never marshalled the field, `permission_workdir` always returned
`None` for a binding call — a `bash{/tmp/*}: allow`/`bash{/etc/*}: deny` rule
a profile author wrote assuming it covered every `bash` invocation silently
never fired for one issued from a script. `BindingPolicy::decide`
(`script.rs`) compounded this: even had the field been present, `decide`
called `PermissionProfile::resolve(tool, arg)` — the two-argument entry point
that is *defined* as `resolve_scoped(.., workdir: None)` — never the
three-argument `resolve_scoped` the direct-call path (`policy.rs`'s
`ProfileResolver::resolve`) already used. Two independent gaps, not one.

Distinct from [ADR-0119](0119-rhai-bindings-route-through-the-escape-root-gate.md)
(#446, shipped): that ADR routes an *out-of-root* binding path/workdir through
the escape-root approval gate — the containment boundary. This ADR is about a
workdir-scoped permission *rule* resolving at all for an in-root call, and
about a script being able to choose a `workdir` in the first place (previously
impossible for `exec`, and only reachable for `bash` by embedding a shell `cd`
in the command string, defeating any workdir-scoped rule matching the literal
`workdir` field instead).

## Decision

### `exec`/`bash` gain `workdir` overloads

`script.rs`'s `register_bindings` adds:

- `exec(command, args, workdir)` — a three-argument overload alongside the
  existing `exec(command)`/`exec(command, args)`, avoiding any ambiguity with
  the two-argument `args`-only form (a `rhai::Array` vs. a `&str` third
  parameter is unambiguous to Rhai's overload resolution).
- `bash(command, workdir)` — a two-argument overload alongside `bash(command)`,
  registered only when `bash_enabled` (unchanged gating).

Both marshal `workdir` into the delegated tool's own `workdir` field —
`{command, args, workdir, timeout}` / `{command, workdir, timeout}` — the
identical field name/shape a direct `call`/`bash` tool call already accepts
(#170/#386). The workdir-less overloads are unchanged: they omit the key
entirely rather than marshalling `workdir: null`, so `permission_workdir`/
`escape_root_target` see the same "absent" state as before this change for
every existing script.

### `BindingPolicy::decide` resolves through `resolve_scoped`, not `resolve`

`decide` now extracts `workdir = permission_workdir(tool, input)` alongside
the existing `arg = grading_arg(...)` and folds the chain with
`p.resolve_scoped(tool, arg.as_deref(), workdir.as_deref())` instead of
`p.resolve(tool, arg.as_deref())`. This is the second, independently-necessary
half of the fix: without it, a workdir-scoped rule would still never fire
even once the field rode along in the marshalled input, since `resolve`
hard-codes `workdir: None` by definition. `permission_chain`'s captured
`PermissionProfile`s (session + ancestors + the config-base ceiling, folded
least-privilege) are otherwise untouched — this is a one-line swap of which
entry point each is queried through.

With both halves landed, `bash{/tmp/*}: allow`/`bash{/etc/*}: deny` now grades
a `bash("...", "/tmp/x")` binding call exactly as it would a direct `bash`
tool call with `workdir: "/tmp/x"` — including the config ceiling
(`clamp_to_base`, #172) and the sub-agent ancestor clamp, both of which
`permission_chain`/`BindingPolicy::capture` already folded in before this
change; only the per-call grading query gained the parameter.

### Escape-root gate needed no new plumbing

`EscapeRoot::escaping`/`escape_root_target`'s `"bash" | "call"` arm already
delegated to `permission_workdir` ([ADR-0116](0116-workdir-scoped-permission-rules-for-bash-call.md)'s
own "share the extractor" decision) — so once a binding call marshals a real
`workdir`, an out-of-root value is detected and gated by the
[ADR-0119](0119-rhai-bindings-route-through-the-escape-root-gate.md) approval
flow for free, with no change to `service_binding`. Before this ADR the
`workdir` arm was reachable in principle but never actually populated for a
binding call, since no binding ever put a value there.

### Approval cache key gains the same scoping fix

`service_binding`'s per-run `approved` cache — keyed per call by
`approval_cache_key` — collapsed distinct calls to the same command line onto
one entry regardless of `workdir` (#419 fix A only scoped the key by
*command*, since no binding had a `workdir` to distinguish by at the time).
Once a workdir-scoped rule can grade two calls to the same command
differently, that collapse becomes a real bypass: approving
`bash("rm -rf .", "/tmp/scratch")` (asked because `/tmp/scratch` graded `Ask`)
would silently pre-clear `bash("rm -rf .", "/etc")` later in the same run if
`/etc` also graded `Ask` — a different, more dangerous target the user never
saw. `approval_cache_key` now folds `workdir` into the key
(`"{tool}:{arg}:{workdir}"`), mirroring fix A's original reasoning one level
further.

## Consequences

- **Positive.** Closes the ledger row: a profile author's `tool{pattern}`
  workdir-scoped rule now applies uniformly to direct and scripted `bash`/
  `call` invocations — no silently-weaker enforcement for script-initiated
  exec, matching this ADR's originating concern.
- **Positive.** A script gains real `cd`-equivalent semantics for both
  `exec` and `bash` without shelling into `sh -c "cd ... && ..."` — the
  `call` tool stays shell-less (argv exec) even with a chosen working
  directory.
- **Positive.** The escape-root gate ([ADR-0119](0119-rhai-bindings-route-through-the-escape-root-gate.md))
  needed zero code changes — proof that ADR-0116's "share the extractor, not
  the policy" plumbing choice paid off exactly as designed.
- **Neutral.** `PermissionProfile::resolve`'s two-argument shape, every other
  binding's grading path, and every pre-#480 script with no `workdir` call are
  byte-for-byte unchanged — this is additive (`resolve_scoped` swapped in
  where a workdir concept already existed for `bash`/`call`; every other tool
  in `BINDING_TOOLS` still resolves through the identical `arg`-only query
  path since `permission_workdir` returns `None` for them).
- **Negative / cost.** Two new script-facing overloads to document (`rhai_spec`'s
  advertised tool description) and two more binding shapes to keep in sync
  with the host tools' own `workdir` semantics if either ever changes.

## Alternatives considered

- **Leave the carve-out as permanent, documented behavior** (ADR-0116's
  original stance). Rejected once a concrete ledger-tracked ask (#480)
  surfaced: the gap is a real enforcement weakening for any profile that
  scopes exec by workdir, not a cosmetic limitation, and the fix is small
  (two overloads + one query-entry-point swap) with no new core surface.
- **Fold `workdir` into `exec`'s/`bash`'s existing argument via a separator or
  an options map** (e.g. `exec(command, {args: [...], workdir: "..."})`)
  instead of a positional overload. Rejected: Rhai overload resolution on
  argument arity/type already gives a clean, unambiguous three/two-argument
  form with no new script-facing syntax (a map argument) to document or
  parse; ADR-0116 separately rejected string-composition of `workdir` for the
  core `resolve_scoped` parameter for the same "keep it a distinct typed
  value" reason, which applies here too.
- **Normalize a relative `workdir` root-relative before grading** (mirroring
  [ADR-0125](0125-permission-arguments-for-path-tools-are-normalized-root-relative.md)'s
  treatment of path-arg tools). Rejected as out of scope: ADR-0125 explicitly
  scoped its root-relative folding to the path-arg tools
  (`read`/`edit`/`write`/`apply_patch`/`glob`/`grep`), never `bash`/`call` —
  `workdir` grading already matched the raw string for a direct tool call
  before this ADR, so a binding call now matches identically, introducing no
  new normalization inconsistency.
