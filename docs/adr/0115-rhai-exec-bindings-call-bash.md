# 0115. `rhai` exec bindings: `exec`/`bash`, gated by the Call capability

- Status: Accepted
- Date: 2026-07-17

## Context

[ADR-0046](0046-rhai-sandboxed-script-tool.md) shipped `rhai` bound to exactly
the root-contained quintet and explicitly deferred exec: *"No exec bindings
(`bash`/`call`) in v1 — that would let a script escalate past its sandbox
[…] revisit only with its own ADR."* The #416 epic's phase B
([ADR-0114](0114-capability-level-permission-keys.md)) now gives every exec
surface a uniform **Call capability** grade, closing the gap ADR-0046 was
worried about: a script's process-exec can be governed by the exact same
`Allow`/`Ask`/`Deny` + argument-scoped-rule machinery as a model-issued `call`/
`bash` tool call, not a bespoke always-on hole. This issue (#419, part of #416)
adds that binding and fixes two pre-existing sharp edges in the bridge that
matter far more once a binding can execute an arbitrary command.

Dispatch plumbing already exists end to end: rhai bindings run via
`service_binding → exec → tools.execute(&ToolCall{name, input}, session)`
(`entanglement-runtime/src/script.rs`), and `call`/`bash` are registered into
that same `ToolRegistry` the bridge already holds a snapshot of (`call`
unconditionally, `bash` behind `ENTANGLEMENT_ENABLE_BASH`). No new dispatch
surface was needed — only new bindings and their permission wiring.

Two structural risks came into focus while wiring the exec bindings in:

1. **The `approved` cache is keyed by bare tool name.** `service_binding`
   parks on `Ask` once and remembers `approved: HashSet<&'static str>` so a
   second call to the same *tool* in one script run doesn't re-prompt. That's
   fine for `edit`/`write`, whose approval card always covers "this tool, this
   run" at a granularity the user already accepted (any path). It is **not**
   fine for `call`/`bash`: approving `exec("git", ["status"])` would silently
   pre-clear a later `exec("rm", ["-rf", "/"])` in the same run — the user
   approved one specific command, not "any command for the rest of this run".
2. **The rhai wall-clock timeout can't reach a blocked binding call.** The
   script's own budget (default 5s, max 30s) is enforced by rhai's
   `on_progress` interrupt, which only fires between VM instructions — but a
   binding call blocks the whole engine thread on `oneshot::blocking_recv`
   while the async side runs the delegated tool. For `call`/`bash` that
   delegated tool has its **own**, much longer default (120s, capped at 600s).
   Without a fix, a 5-second script could hold a shell command open for ten
   minutes — the rhai timeout would be cosmetic for exec.

## Decision

**Bindings — `exec`, not `call`.** `register_bindings` gains
`exec(command)` / `exec(command, args: Array)`, marshalled to the `call`
tool's `{"command", "args"}` shape, and `bash(command)`, marshalled to
`{"command"}`. The script-facing function is named **`exec`**, not `call`:
`call` is a hard-reserved Rhai keyword (`KEYWORD_FN_PTR_CALL`) the interpreter
special-cases in `make_function_call` to always mean "invoke this `FnPtr`" —
registering a same-named function is silently shadowed (Rhai coerces the first
argument to a function pointer and throws a type-mismatch instead of ever
reaching ours). This is a naming accommodation only: the tool dispatched to
the bridge, its permission grade, its `BINDING_TOOLS`/capability membership,
and every user-facing permission-rule string (`call(git *): allow`) all stay
the literal `call` — matching the model-facing tool of the same name — so a
profile's `call`/Call-capability rules govern the binding identically to a
direct tool call. `bash` is registered only when the host `bash` tool itself
is (`bash_enabled = tools.contains("bash")`, snapshotted once before the
`spawn_blocking` closure moves the registry snapshot out of reach); off,
`bash(...)` is an ordinary unknown-function script error — catchable, not a
graded-then-refused binding — matching how an unregistered function behaves
everywhere else in the sandbox.

**Mask + grade.** `"call"`/`"bash"` join `BINDING_TOOLS` (5 → 7), so
`BindingPolicy::capture`/`decide` mask and grade them exactly like the
quintet: the #116 tool mask, the ancestor + config-ceiling chain
([ADR-0024](0024-subagent-permission-gating.md)), and `permission_arg`'s
existing `call`/`bash` extraction (the joined command line — unchanged, #419
added no new extraction logic). `rhai_spec`'s description documents both new
functions and the naming rationale.

**Fix A — approval-cache key.** `execute_script`'s `approved` set changes from
`HashSet<&'static str>` to `HashSet<String>`, populated by a new
`approval_cache_key(tool, input)`: for `call`/`bash` the key is
`"{tool}:{permission_arg(tool, input)}"` (the same command-line extraction the
permission grade itself uses); every other binding keeps the bare tool name,
unchanged. An arg-scoped `allow` rule (`call(git *): allow`) still pre-clears
without ever touching the cache — `Decision::Perm(Permission::Allow)` bypasses
`approved` entirely, as it always did.

**Fix B — derived timeout.** `register_bindings` receives the script's
`start: Instant` / `timeout: Duration` (already computed in `execute_script`
for the progress callback) and a new `remaining_timeout_secs(start, timeout)`
helper stamps a `"timeout"` field — `(timeout - elapsed).as_secs().max(1)` —
into every `exec`/`bash` binding call's marshalled JSON. The delegated tool's
own timeout parameter is caller-suppliable, so this is not a new mechanism,
just always supplying a value derived from the *actual* remaining budget
instead of leaving it to the tool's much larger default. `Stop`-driven
cancellation was already correct (the exec tools' own `kill_on_drop` +
`own_process_group` handle it) and needed no change.

## Consequences

- **Positive.** Closes the #416 epic's phase-B gap for scripts: a script can
  now shell out under the identical Allow/Ask/Deny + argument-scoped-rule
  chain a model-issued `call`/`bash` uses, with no separate policy surface to
  keep in sync.
- **Positive.** Both sharp edges the exec surface newly exposed (cross-command
  approval bleed, timeout escape) are closed in the same change rather than
  shipped as known gaps — the review that would have found them post-hoc
  found them pre-hoc instead.
- **Negative / cost.** The script-facing binding name (`exec`) diverges from
  the tool name it dispatches (`call`) — a `permission: {"call": ...}` rule
  reads correctly against the tool table, but a script author who expects a
  function literally named `call` needs the `rhai_spec` doc's callout to find
  `exec`. Judged worth it over any workaround (there is no clean way to make
  `call` itself callable — disabling the reserved symbol makes it a *parse*
  error instead of a shadowing bug, which is strictly worse).
- **Neutral.** `bash`'s conditional registration means `BindingPolicy::decide`
  can still grade a `"bash"` call that the engine never actually bound (mask/
  grade is argument-independent of runtime registration, by design — see
  `BINDING_TOOLS`'s doc comment) — harmless, since an ungated script can only
  ever reach `bash(...)` through the registered function, which doesn't exist
  when disabled.

## Alternatives considered

- **Keep the script function named `call`, work around the keyword.**
  Rejected: `Engine::disable_symbol("call")` turns the identifier into a parse
  error, not a normal symbol available for `register_fn` — there is no
  supported way to reclaim a hard-reserved keyword as a plain function name in
  this Rhai version. A different call-site trick (UFCS-only registration,
  wrapping in a module) still collides with the same tokenizer-level
  reservation, since the conflict is at parse time, before any function
  resolution runs.
- **Cache `approved` per-tool but exempt `call`/`bash` from caching
  entirely (always ask).** Rejected: strictly worse UX than command-scoped
  caching for no extra safety — a script issuing `git status` three times in a
  loop would prompt three times instead of once, while a scoped cache already
  gets the security property (a different command still asks) without that
  cost.
- **Bound the exec tool's own timeout to the script's *initial* budget
  (computed once, not derived per call).** Rejected: a script with two
  sequential `exec()` calls would let the second one see the *original* 30s
  budget even after the first already consumed 25 of it — recomputing
  `remaining` at each call site is the same cost and closes that gap too.
- **Enforce the timeout bound centrally in the bridge (wrap the tool
  execution in `tokio::time::timeout`) instead of passing a derived
  parameter.** Rejected: the delegated tool already owns correct
  cancellation (`kill_on_drop`, `own_process_group`) tuned to its own
  timeout semantics (partial-output-on-timeout, #169); wrapping the `.await`
  in a second, bridge-level timeout would race two independent cancellation
  paths for the same child process. Passing the derived value as the tool's
  own `timeout` input reuses the existing, already-correct mechanism.
