# 0119. `rhai` file/exec bindings route through the escape-root gate

- Status: Accepted
- Date: 2026-07-20
- Amends: [ADR-0109](0109-escape-root-access-via-approval.md)

## Context

[ADR-0109](0109-escape-root-access-via-approval.md) wired the escape-root
detection, forced-`Ask`, warning text, and `ExtraRootStore::record` steps into
exactly one place: `tool_runner::dispatch`, reached only by the generic
`Intercept::Permission` route. `rhai`'s bindings (`read`/`edit`/`write`/`exec`/
`bash`, the latter pair added by [ADR-0115](0115-rhai-exec-bindings-call-bash.md))
never go through `dispatch` — `Intercept::Rhai` resolves permission itself
(`script::BindingPolicy`/`service_binding`) and executes a call straight
through `ToolRegistry::execute`, bypassing the executor entirely (#446).

Two asymmetries against a direct tool call followed:

1. **New escapes failed closed, silently.** A binding targeting an out-of-root
   path could never *obtain* an escape approval: `escape_root_target`/
   `escaping_path`/the forced-`Ask` override lived only in `dispatch`, so the
   delegated host tool's own containment check
   (`resolve_under_root_or_grant`) hard-refused with no chance to prompt —
   even under a profile the user would happily have approved past.
2. **A pre-existing durable grant was honored with no signal.** A `Session`/
   `Always` escape recorded earlier by a direct approved call *is* consulted
   by the host tool's own containment check (it's the same `ToolRegistry`
   instance the bridge holds a snapshot of), so a script could silently ride
   it — reaching outside the project root with no approval card and no
   "outside the project root" warning for that particular script action.
   Permission grades still applied (this was never a way to exceed a
   profile's grade), so it was defense-in-depth erosion, not a bypass.

Zero tests exercised a rhai binding against an out-of-root path with the
escape-root policy wired at all.

## Decision

**Route the file/exec bindings through the same escape-root gate a direct
tool call uses**, rather than documenting the asymmetry as a permanent
limitation.

- `tool_runner::EscapeRoot`'s `escaping(tool, input)` helper (previously
  private to `dispatch`) becomes `pub(crate)` so `script.rs` can call it too.
- `EscapeRoot` is threaded one hop further: `spawn_tool_executor_with_policy`
  already carries `Option<EscapeRoot>`; the `Intercept::Rhai` arm now clones
  it (mirroring the existing clone in the `Intercept::Permission` arm) and
  passes it into `run_rhai` → `execute_script` → `service_binding`.
- `service_binding` computes the same `escape` value `dispatch` does —
  `escape_root.escaping(tool, input)`, filtered by
  `!store.is_durably_allowed(tool, abs)` — for every binding call graded
  `Allow` or `Ask` (never for `Deny`, matching `dispatch`'s
  `.filter(|_| perm != Permission::Deny)`). An escaping call:
  - **forces the approval path** even when `BindingPolicy::decide` graded
    `Allow`, bypassing the coarse per-run `approved` cache (that cache exists
    to avoid re-prompting an ordinary `Ask`, not to authorize a fresh
    out-of-root target it never saw);
  - **carries the ADR-0109 warning** on the nested approval card:
    `"{input}\n\n⚠ accesses a path OUTSIDE the project root: {abs}"`, the
    same text `dispatch` emits, appended to the existing `"{tool} (rhai)"`
    card label;
  - **records the grant on approval** — `Approval` (the binding's internal
    outcome enum) now carries the chosen `ApprovalScope`, and an approved
    escaping call calls `store.record(tool, &abs, scope)` instead of
    inserting into the local `approved` cache, mirroring
    `tool_runner::await_decision`'s escape branch.
- A durably-allowed escape (`Session`/`Always`, recorded by an earlier direct
  *or* rhai-approved call) still resolves silently — `escape` becomes `None`
  once `is_durably_allowed` is true, so the call takes the same fast path as
  an ordinary in-root `Allow`. This is intentional and matches `dispatch`
  byte-for-byte: a durable grant means "stop asking", not "stop warning once
  more". Asymmetry (2) above is therefore *preserved*, not eliminated — it is
  the documented, accepted behavior of a durable grant, now identically true
  whether the call was direct or scripted.
- `Option<EscapeRoot>` stays the opt-in seam it always was: every existing
  test and the default `spawn_tool_executor`/`spawn_tool_executor_with_hooks`
  wrappers pass `None`, so `rhai` bindings keep the pre-#446 hard-fail
  behavior (`root_escape_is_refused_by_the_binding`) unless a caller wires an
  `EscapeRoot` explicitly — exactly as a direct call did before this change,
  and exactly how `main.rs`'s single wiring point already worked.

Two new integration tests (`entanglement-runtime/tests/rhai.rs`) exercise the
combination no test previously covered: `escape_root_wired_prompts_with_warning_and_runs_on_approve`
(a first-time escape now prompts with the warning, and approval both runs the
binding and durably records the grant) and
`pre_existing_durable_grant_is_honored_without_a_new_prompt` (a grant recorded
before the run — simulating an earlier direct approval — is honored with no
new `ToolRequest`, documenting asymmetry (2) as intentional rather than an
oversight).

## Consequences

- **Positive.** Closes asymmetry (1): a script can now obtain the same
  out-of-root access a direct call could, under the same explicit consent —
  no functional gap between scripted and direct tool use.
- **Positive.** The approval card for an escaping binding call now always
  carries the ADR-0109 warning, closing the "no signal" half of asymmetry
  (2) for the call that *creates* a grant — a user approving a script's
  first escape sees exactly what they're approving, same as a direct call.
- **Neutral / accepted.** A *subsequent* durably-granted access — direct or
  scripted — still runs silently with no repeated warning. This is
  `dispatch`'s existing, deliberate behavior for `Session`/`Always` scope
  (the entire point of a durable grant is to stop prompting), not a gap this
  change introduces or needed to close.
- **Neutral.** No wire or protocol change: reuses the existing
  `ToolRequest`/`Approve{scope}` round-trip and `ExtraRootStore`, exactly as
  ADR-0109 designed it — `service_binding`'s nested approval already ran the
  same round-trip for an ordinary `Ask`-graded binding.
- **Negative / cost.** `run_rhai`/`execute_script`/`service_binding` each
  gained one parameter (`Option<EscapeRoot>`/`Option<&EscapeRoot>`), and the
  `Approval` enum's `Approved` variant now carries `ApprovalScope` (previously
  discarded) — a small, mechanical widening of the bridge's internal
  plumbing, not a new concept.

## Alternatives considered

- **Document the asymmetry instead of closing it** (the fix direction ADR-0109's
  originating issue offered as an alternative). Rejected: the fail-closed half
  (1) has no workaround for a legitimate script use case other than "don't use
  rhai for this", which is a worse experience than the direct-call path offers
  for no security benefit — the binding is already graded by the identical
  `Allow`/`Ask`/`Deny` chain, so gating the *containment* exception the same
  way is a natural extension, not a new privilege surface.
- **Give rhai its own escape-approval bookkeeping instead of sharing
  `tool_runner::EscapeRoot`/`ExtraRootStore`.** Rejected: a second store would
  fragment grants by call origin (a `Session` escape approved via a direct
  call wouldn't cover the identical path from a script, and vice versa) for
  no isolation benefit — the host tool's own containment check already
  consults one shared store regardless of caller, so a script-only store
  would just be inconsistent with what execution already does.
- **Also warn on a durably-allowed (silent) escape inside a script**, e g. a
  one-time transcript note the first time a run reuses a grant. Rejected:
  this would diverge from `dispatch`'s behavior for the identical case (a
  direct call also stays silent on a durable grant) — the point of `Session`/
  `Always` scope is precisely to stop prompting, and singling out rhai for
  extra noise here would make an otherwise-identical grant behave
  differently depending on the caller, the opposite of this ADR's goal.
