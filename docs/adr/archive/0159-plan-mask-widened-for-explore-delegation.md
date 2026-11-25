# 0159. `plan`'s own mask carries `call`/`bash` so its `explore` delegation isn't self-defeating

- Status: Accepted
- Date: 2026-08-02
- Related: [ADR-0038](0038-physical-per-agent-tool-restriction.md) (the
  ancestor-clamp mechanism this collided with — unchanged here, only the
  `plan` built-in's own mask data and the mask helper's return type change)
  and [ADR-0138](0138-sponsored-build-child-and-propose-plan-cycle.md)
  (considered and rejected extending the sponsored-child exemption to this case)

## Context

`plan.md`'s system prompt tells the agent to "delegate research to exploration
agents", and `explore.md` grants itself `call`/`bash` at `Ask` grade precisely
so that research (checking a GitHub issue, a PR, CI status) isn't stuck on read
tools alone. But `tool_masked` ([ADR-0038](0038-physical-per-agent-tool-restriction.md))
intersects a child's advertised set with every ancestor's down the spawn chain —
`plan.md`'s own allowlist had no `call`/`bash` on it, so any `explore` child
`plan` spawned had both erased regardless of what `explore.md` itself granted.
Live in `skutter tui`: a `plan` session spawned `explore` to research issue
#594; `gh issue view 594` via `call` was refused outright with "tool `call` is
not available to this agent (restricted by profile)" — no approval round-trip,
just the mask's hard deny. The instruction to delegate and the mask that
erases what's needed to act on that delegation directly contradicted each
other (#597).

The refusal message also didn't say *whose* mask did it. `tool_masked` returned
a bare `bool`; from the outside "restricted by profile" reads identically
whether the session's own definition lacks the tool or an ancestor three hops
up clamped it away — a dead end with no signal that this is mask-ancestry, not
a broken tool or a config mistake.

## Decision

**Widen `plan.md`'s own mask** to include `call`/`bash` (narrowest of the two
directions the issue proposed). The mask (`tools:` allowlist) is only half the
fix: the ancestor-chain intersection only erases what an ancestor's mask
*lacks*, so once `call`/`bash` are on `plan`'s own allowlist an `explore`
child's own grant of them survives that walk.

**The permission grade needed a second, less obvious fix.** `plan.md` already
carries a bare `write: deny` (needed to keep `edit`/`write` denied outside the
plans-folder carve-out), and `call` is a [`MULTI_GROUP`](0114-capability-level-permission-keys.md)
tool (ADR-0114): its *bare* (no-argument) grade is always the least-privileged
of every bare capability grade in the profile, computed once regardless of
what a bare `call: ask` entry itself asks for — so as long as `write: deny` is
present, a bare `call: ask` is silently discarded and `call`'s coarse grade
stays `Deny`. Since `effective_permission`'s ancestor clamp resolves each link
with the *same* concrete argument as the actual dispatch (the call/bash
command is always present for a real invocation), this Deny would still clamp
an `explore` child's own `Ask` down to `Deny` even after the mask was fixed —
the mask fix alone would have swapped one hard-deny message for another.
ADR-0114 anticipated exactly this: an **arg-scoped** capability key
(`call(pattern)`) can still refine `call`'s multi-group floor per the actual
command, because a real invocation always carries one. `plan.md`'s permission
block therefore uses `call(*): ask` — matches any command, fans out to both
`call(*)` and `bash(*)` — instead of a bare `call: ask`/`bash: ask`. The
*coarse*, no-argument view of `plan.permission.for_tool("call")` legitimately
stays `Deny` (an accurate floor: an unscoped `call` could bypass `write: deny`
by shelling out), while every real dispatch — which always carries the
command as its argument — resolves through the scoped rule to `Ask`.

**Rejected: extend the ADR-0138 sponsored-child exemption to `plan`→`explore`.**
Sponsorship removes the ancestor walk *entirely* for the sponsored session
(both mask and privilege ceiling) and is deliberately scoped to one
authorization event — the user's `propose_plan` approval — not to ordinary
`agent_spawn` calls, which a plan session can issue without any user
round-trip beyond the spawn's own `Ask` grade. Reusing sponsorship here would
either (a) require inventing a second authorization event for a plain
delegate-spawn, widening the "sponsored" concept ADR-0138 kept deliberately
narrow, or (b) skip that requirement and grant unclamped permission on a
weaker basis than plan acceptance — a bigger, harder-to-reverse change for a
narrower problem than the mask mismatch actually is. Mask-widening is a
one-line, easily-audited fix; sponsorship-extension is a new policy axis.

**Name the deciding link in the refusal message.** `permission::tool_masked`
is now a thin wrapper over a new `tool_mask_source`, which returns
`Option<SessionId>` — the specific link in the chain (the session itself or an
ancestor) whose mask erased the tool, mirroring the existing
`resolve_with_source`/`effective_permission` split for the privilege ceiling.
`tool_runner`'s mask-deny path uses the source to distinguish "restricted by
its own profile" from "restricted by ancestor agent `<name>`'s profile",
looking the ancestor's `AgentProfile::name` up in the same `active` map it
already holds. The boolean `tool_masked` keeps its old signature and callers
(the `rhai` binding snapshot in `script.rs`) are unaffected.

## Consequences

- **(+)** A `plan`→`explore` delegation for research (`gh issue view`, `git
  log`, CI status) now reaches `explore`'s own `Ask`-graded `call`/`bash`
  instead of being hard-denied before any approval round-trip — verified at
  both layers: the mask (`tool_masked`) and the ancestor permission ceiling
  (`effective_permission`) for a concrete `call` dispatch.
- **(+)** `plan` remains structurally read-only for file mutation — only
  `call`/`bash` (approval-gated on the actual command, never blanket) are
  added; `write`/`edit` stay `deny` outside the plans-folder carve-out.
- **(+)** The scoped `call(*): ask` rule is a legitimate, narrower grant than a
  bare `call: ask` would have been (had `MULTI_GROUP` let one through): it
  only ever governs an actual command, never the coarse/no-argument case,
  which is exactly the shape a real dispatch has.
- **(+)** A mask refusal now names which link caused it, so a user hitting the
  mask sees "restricted by ancestor agent `plan`'s profile" instead of a
  bare, unattributed "restricted by profile" — actionable instead of opaque.
- **(−)** `plan` itself can now issue a direct `call`/`bash` (approval-gated)
  it previously couldn't, even though its prompt still steers it toward
  delegating. A narrow, audited widening, not a new capability class.
- **(−)** The sponsored-child mechanism (ADR-0138) stays a one-purpose
  exemption; a future delegation pattern that needs an unclamped child will
  need its own authorization story rather than reusing this one, by design.

## Alternatives considered

- **Extend ADR-0138 sponsorship to `plan`→`explore`.** Rejected above — wrong
  authorization shape and a bigger blast radius than the bug warrants.
- **Special-case `plan`→`explore` in `tool_masked` itself (a hardcoded
  agent-pair exemption).** Rejected: exactly the kind of profile-specific
  branch the permission ladder ([ADR-0070](0070-authoritative-tool-exec-profile-and-fail-closed-fallback.md))
  avoids, and it wouldn't generalize to any other agent that wants to
  delegate research (a future `review` or `debug` primary, say) — those would
  hit the identical mask collision. Widening the spawning agent's own mask is
  the general fix; this issue just needed it applied to `plan`.
- **Leave the refusal message unchanged.** Rejected: the issue explicitly
  flagged the ambiguity as part of the failure mode, and the fix is cheap
  given `resolve_with_source` already established the pattern for the
  privilege ceiling.

## References

- Issue #597: plan→explore delegation blocked by the ancestor tool-mask clamp
- [ADR-0038](0038-physical-per-agent-tool-restriction.md): physical per-agent
  tool restriction + ancestor clamp (the mechanism this collided with)
- [ADR-0138](0138-sponsored-build-child-and-propose-plan-cycle.md): sponsored
  build child (the exemption considered and rejected for this case)
- [ADR-0137](0137-explore-ask-grade-shell-access.md): `explore`'s own
  `Ask`-graded `call`/`bash`/`rhai`
