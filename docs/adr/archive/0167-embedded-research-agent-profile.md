# 0167. Embedded `research` agent profile — a global read-only Q&A entry agent

- Status: Accepted
- Date: 2026-08-04
- Related: [ADR-0038](0038-physical-per-agent-tool-restriction.md) (tool mask), [ADR-0040](0040-per-profile-spawn-control.md) (spawn control), [ADR-0114](0114-capability-level-permission-keys.md) (capability keys), [ADR-0137](0137-explore-ask-grade-shell-access.md) (ask-grade shell precedent), [ADR-0159](0159-plan-mask-widened-for-explore-delegation.md) (multi-group floor + arg-scoped refinement)

## Context

A recurring workflow is *thinking a problem through with an agent* — asking
open questions, having it investigate the codebase and report — with a hard
assurance that nothing gets written to the project. Neither existing built-in
fits:

- `plan` is shaped around **plan authorship**: it carries `propose_plan`, the
  `.entanglement/plans/*.md` write carve-out (ADR-0142), and the
  approve-→-`build`-handoff cycle (ADR-0145). Selecting it for a plain
  research question drags all of that machinery — and a real write path —
  into a conversation that wants neither.
- `explore` has exactly the right permission posture (read triad `Allow`,
  exec `Ask`, mutation denied twice over) but is a `mode: subagent` **leaf**:
  it is filtered out of the `/agent` picker and the primary cycle, cannot be
  a session's entry agent except via explicit `--agent`, and cannot spawn —
  so it cannot fan a broad question out into parallel sub-investigations.

The gap is an **entry-capable** read-only agent that can also delegate — and
whose delegation cannot widen into a write-capable profile.

## Decision

Ship a fifth embedded built-in, `entanglement-runtime/src/agents/research.md`:

```yaml
name: research
mode: all
include_brief: true
tools: [read, glob, grep, agent, poll, ask_user, load_skill, call, bash, rhai]
spawnable_agents: [research]
permission:
  default: ask
  read: allow
  write: deny
  call(*): ask
  rhai: ask
```

The load-bearing choices:

- **`mode: all`** — one profile, both roles. It appears in the `/agent`
  picker (entry roster = `primary | all`) so it can be *the* session agent,
  and it is a legal spawn target (`spawnable_as_subagent`), so it can
  delegate to itself. It deliberately stays out of the Tab-cycle ring
  (primaries only, #322): reachable, not underfoot.
- **Self-only spawn closure** — `spawnable_agents: [research]`. The spawn
  allowlist is checked per spawner, not transitively (ADR-0040), but a list
  that contains only the profile itself *is* transitively closed: every
  descendant is `research` again, so the subtree can never widen into
  `build`/`debug` write access. The omitted-`agent` default target
  (`explore`) falls outside the allowlist and is refused — the prompt body
  instructs the model to always name `agent: research` explicitly (and the
  spawn tool's schema `enum` offers nothing else).
- **Write is denied, exec is ask-graded** — the ADR-0137 posture, not a
  hard mask-out: `write: deny` fans over `edit`/`write`/`apply_patch`
  (which the mask also omits — belt and suspenders), while
  `call`/`bash`/`rhai` stay advertised and escalate to the user per
  invocation. The guarantee is therefore *"nothing writes without explicit
  per-command approval"*, not *"exec is impossible"* — a `git log`/`git
  blame` doesn't dead-end the agent.
- **The ADR-0159 grading pattern** — `write: deny` drags the multi-group
  `call`/`rhai` bare grades to `Deny` (the capability floor, ADR-0114); the
  arg-scoped `call(*): ask` re-grades every real `call`/`bash` dispatch
  (which always carries its command as the argument), and the later literal
  `rhai: ask` out-ranks the floor for `rhai` by last-match. Identical
  mechanics to `plan.md`, pinned by the same style of shape test.
- **No plan authorship** — `propose_plan` is absent from the mask and
  `default: ask` never grants it implicitly (plan authority is
  literal-exact, ADR-0049/ADR-0145). Research reports findings as text;
  producing an executable plan remains `plan`'s job.

"Global" needs no new mechanism: an embedded built-in exists in every
project out of the box, and the standard layer precedence (ADR-0034 —
user `${config_dir}/entanglement/agents/research.md` or a project
`.entanglement/agents/research.md` shadows it wholesale) is the tweak path,
verified by an integration test.

## Consequences

- The built-in set grows to a quintet: `build`, `plan`, `explore`, `debug`,
  `research`. Docs listing the set (arch doc restriction map, README, core
  `ProfileRegistry` doc comment) are updated in the same change.
- Because masks intersect down the spawn chain (ADR-0038/ADR-0159), a
  `research` child spawned under a parent whose mask lacks `rhai` (e.g.
  `plan`) loses `rhai` — accepted; the read triad and `call`/`bash` survive
  under every built-in parent that can spawn at all.
- A user approving an exec command with `Always` scope persists a grant
  (ADR-0052) that auto-allows that exact command later. Grants raise
  `Ask → Allow` only and never override `Deny`, so the write posture is
  unaffected.
- `build` (open `spawnable_agents`) can now also delegate to `research` as a
  read-only worker — a strictly narrower alternative to `debug`.

## Rejected

- **A user-layer-only profile** (no code change): works for one machine, but
  every install would have to re-author it; an embedded default with the
  existing override path gives the same flexibility plus a shared baseline.
- **`mode: primary` + a separate subagent twin**: two definitions to keep in
  sync where `mode: all` does both jobs.
- **Hard-denying exec** (`explore`'s pre-ADR-0137 shape): re-creates the
  dead-end that ADR-0137 fixed — one `git log` away from "I can't run that".
- **Widening `spawnable_agents` to `[research, explore]`**: explore is also
  read-only, but the omitted-`agent` default would then silently succeed and
  the subtree posture would rest on *two* profiles staying read-only instead
  of one self-closed loop.
