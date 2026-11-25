# 0137. `explore` gains `Ask`-grade shell access (`call`/`bash`/`rhai`)

- Status: Accepted
- Date: 2026-07-30
- Amends: [ADR-0038](0038-physical-per-agent-tool-restriction.md) (the `explore` mask), [ADR-0024](0024-subagent-permission-gating.md) (the read-only reference agent's posture)

## Context

The built-in `explore` profile
(`entanglement-runtime/src/agents/explore.md`) was the reference read-only
agent: a `mode: subagent` leaf with `tools: [read, glob, grep]` and
`permission: { default: deny, read: allow, glob: allow, grep: allow }`
(ADR-0038). Read tools alone were its entire executable surface, and every
other tool — including `call`/`bash`/`rhai` — was hard-denied twice over: not
advertised by the mask, and `default: deny` refusing anything the mask didn't
list.

That dead-ended a common read-only delegation: an `explore` sub-agent asked to
"summarize the recent changes" or "what does this branch touch?" needs exactly
one `git status` / `git diff` / `git log`, none of which the read triad can
produce. With `call`/`bash` hard-denied, the agent had no escalation path at
all — it could only return "I can't run that", and the caller (the spawning
model) had to either re-delegate to the much more permissive `debug` profile or
give up. The same one-command inspection need that a primary `build` session
satisfies with a silent `Allow` was structurally impossible for the safest
delegation target.

The sibling `debug` profile already grants full shell (`default: allow`, no
mask — its test pins `for_tool("bash") == Allow`). So the gap was specifically
*read-only + wants one harmless command*, with nothing in between "read-only,
no shell at all" and "full read/write/execute".

## Decision

`explore` advertises the exec tools and grades them at `Ask`, while keeping
`default: deny` and every file-mutation tool denied. Concretely the frontmatter
becomes:

```yaml
tools: [read, glob, grep, call, bash, rhai]
permission:
  default: deny
  read: allow
  glob: allow
  grep: allow
  call: ask
  bash: ask
  rhai: ask
```

The safety property this preserves is **no silent execution or mutation**:

- `Ask` ([ADR-0052](0052-approval-scope-and-persisted-grants.md)) parks the
  call at `WaitingApproval` until the user explicitly approves — `y` (once),
  `s` (session), `a` (always), or rejects. An exec tool is never auto-run.
- `edit`/`write`/`apply_patch` stay hard-`Deny` (they are not in the mask and
  `default: deny` refuses them): `explore` never mutates files, period. An
  approved `call`/`bash` could in principle run a shell command that writes a
  file, but that is the user's explicit, per-call (or scoped) decision — the
  *profile* still offers no file-mutation tool, and the approved command line is
  shown verbatim in the approval prompt.
- `agent_spawn` stays out of the mask and denied: `explore` still cannot
  reproduce.
- `default: deny` remains the floor, so any tool not enumerated above (including
  `update_plan`/`update_tasks`, MCP tools) is still refused — the widening is
  exactly the three named exec tools, nothing more.

`rhai` joins the exec triad because its `exec`/`bash` bindings
([ADR-0115](0115-rhai-exec-bindings-call-bash.md)) route through the same
permission grade; without `Ask`-grading `rhai`, a script could reach execution
only through a binding the profile would then deny at dispatch — cleaner to
grade it consistently and let the user approve the whole script.

The profile body is updated to tell the model the exec tools exist and that
each call escalates for approval, so it reaches for read tools first and treats
shell as the exception.

## Consequences

- **(+)** A read-only `explore` delegation needing one `git status` / `git diff`
  is no longer a hard dead-end — it asks, the user approves, done.
- **(+)** The safety posture of "read-only, never mutates, never spawns" is
  preserved at the profile level: `edit`/`write`/`agent_spawn` are still
  mask-absent and `Deny`; only exec escalates, and only per user decision.
- **(+)** `debug` is unchanged — its `default: allow` already grants shell; this
  fills the gap *below* it, not above.
- **(−)** `explore` is no longer *strictly* read-only in the sense of "cannot
  execute anything even with user consent" — it can now execute, conditional on
  approval. Operators who relied on the stronger property (e.g. a ceiling
  pinning `call: deny`) should set that in their `permissions` ceiling
  ([ADR-0047](0047-local-trust-boundary.md)), which clamps `Ask` back down to
  `Deny` for every agent.
- **(−)** One more approval prompt per read-only delegation that reaches for
  shell — a deliberate, in-the-loop cost, not a silent one.

## Alternatives considered

- **Leave `explore` strictly read-only; route one-command inspections to
  `debug`.** Rejected: `debug` carries `default: allow` and no mask — full
  read/write/execute — so using it for a read-only `git status` is a large
  privilege escalation for a small need, and the spawning model has to know to
  switch targets. The gap was specifically "read-only + one harmless command".
- **Grade exec `Allow` (silent) instead of `Ask`.** Rejected: that makes
  `explore` silently execute arbitrary commands, inverting the read-only
  guarantee the profile exists to provide. `Ask` keeps the user in the loop on
  every exec, which is the whole point of a read-only delegation target.
- **Add only `call` (argv, no shell), not `bash`/`rhai`.** Rejected: `git`
  subcommands work fine through `call`, but a real pipeline (`git diff | grep`,
  redirect to a file the user then reads) needs `bash`, and `rhai`'s bindings
  route through the same grade — admitting only `call` would leave the profile
  asymmetric and still deny the exact `rhai`/`bash` paths a user might approve.
  The `Ask` grade is the safety boundary, not the tool list.
- **A new `inspect` profile with shell, leaving `explore` untouched.** Rejected:
  multiplies the built-in surface for no safety gain (the new profile would be
  strictly more permissive than `explore`), and the spawning model's
  `description`-driven selection already has `explore` as the read-only default
  — splitting it would push the choice back onto the model.
