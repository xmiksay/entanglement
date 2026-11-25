# 0044. Agents, skills & system prompt — epic synthesis (data-not-code subsystem)

- Status: Accepted
- Date: 2026-07-11

## Context

Epic [#111](https://github.com/xmiksay/entanglement/issues/111) turned three
concerns that used to be **hardcode** into a single **data-driven** subsystem:

- agent profiles were three built-ins baked into a `ProfileRegistry`;
- the system prompt was a static string on `AgentProfile`;
- there was no skills mechanism at all.

It landed as eleven sub-issues, each with its own Accepted ADR (agent-tool family
[#120](https://github.com/xmiksay/entanglement/issues/120)/[ADR-0033](0033-agent-tool-family-and-blocking-agent.md);
file-defined agents [#112](https://github.com/xmiksay/entanglement/issues/112)/[ADR-0034](0034-file-based-agent-definitions.md);
prompt assembly [#113](https://github.com/xmiksay/entanglement/issues/113)/[ADR-0035](0035-deterministic-system-prompt-assembly.md);
skill discovery [#114](https://github.com/xmiksay/entanglement/issues/114)/[ADR-0036](0036-skill-discovery-and-registry.md);
`load_skill` [#115](https://github.com/xmiksay/entanglement/issues/115)/[ADR-0037](0037-load-skill-tool-deterministic-resolution.md);
tool mask [#116](https://github.com/xmiksay/entanglement/issues/116)/[ADR-0038](0038-physical-per-agent-tool-restriction.md);
spawn control [#119](https://github.com/xmiksay/entanglement/issues/119)/[ADR-0040](0040-per-profile-spawn-control.md);
`owns_plan` [#140](https://github.com/xmiksay/entanglement/issues/140)/[ADR-0041](0041-update-plan-ownership-default-closed.md);
`propose_plan` [#141](https://github.com/xmiksay/entanglement/issues/141)/[ADR-0042](0042-plan-acceptance-via-propose-plan-approval-roundtrip.md);
skill preload/access [#117](https://github.com/xmiksay/entanglement/issues/117)/[ADR-0043](0043-skill-preload-vs-access-independent-mechanisms.md)).

Each sub-ADR records a *local* decision. What none of them records is the
**cross-cutting model** that governed all eleven and the invariants that only
become visible when they are read together. A future reader will reasonably ask
*"why is this subsystem split across eleven pieces, and what holds it together?"*
This ADR is that answer — it ratifies the six epic principles as binding design
constraints and maps each to the mechanism that physically enforces it, so the
principles cannot silently erode as the subsystem evolves. It records no new code;
it locks the shape the sub-issues collectively realized.

## Decision

Adopt the Claude-Code-derived **"agents and skills are data, disclosed
progressively, assembled deterministically"** model as the subsystem's binding
architecture, enforced by the mechanisms below. The six epic principles are
requirements, not aspirations; each has a named enforcement point.

### Principle → enforcement map

| # | Principle | Enforced by |
| --- | --- | --- |
| 1 | Model decides *whether*, harness decides *how* | Selection is LLM reasoning over `description` text (no keyword/embedding router — [ADR-0036](0036-skill-discovery-and-registry.md)); path resolution, prompt assembly, authorization, tool-list enforcement are deterministic runtime code ([ADR-0035](0035-deterministic-system-prompt-assembly.md)/[ADR-0037](0037-load-skill-tool-deterministic-resolution.md)) |
| 2 | Never spoof authorship | `load_skill` and preload return `tool_result` / system-prompt content, never a fake `user` message ([ADR-0037](0037-load-skill-tool-deterministic-resolution.md)/[ADR-0043](0043-skill-preload-vs-access-independent-mechanisms.md)) |
| 3 | Authorization uniform, per resolved call | `load_skill` is a real host tool through the same permission + mask gate as `read`; skill/agent/tool calls share one path ([ADR-0037](0037-load-skill-tool-deterministic-resolution.md)/[ADR-0038](0038-physical-per-agent-tool-restriction.md)/[ADR-0024](0024-subagent-permission-gating.md)) |
| 4 | Progressive disclosure, recursively | Tier table below — index → body-on-demand → child prompt at spawn → child preloads only if its definition says so ([ADR-0036](0036-skill-discovery-and-registry.md)/[ADR-0035](0035-deterministic-system-prompt-assembly.md)/[ADR-0043](0043-skill-preload-vs-access-independent-mechanisms.md)) |
| 5 | Physical restriction over prompted | `tools`/`disallowed_tools` mask the advertised specs *and* dispatch; a read-only agent has no write tool to call, not a persona told not to ([ADR-0038](0038-physical-per-agent-tool-restriction.md)) |
| 6 | Predefined but editable | Embedded built-ins parse through the *same* loader as user/project files; override precedence project > user > built-in mirrors the provider catalog ([ADR-0034](0034-file-based-agent-definitions.md)/[ADR-0036](0036-skill-discovery-and-registry.md)) |

### Cross-cutting invariant A — progressive-disclosure tiers

Disclosure is recursive and strictly tiered; each tier costs more context and is
entered only on demand:

| Tier | What the model sees | When | Cost |
| --- | --- | --- | --- |
| Agents 0 | `name: description` of spawn targets | in the `agent`/`agent_spawn` tool schema, scoped to who the profile may spawn | enum + a line each |
| Skills 1 | `name: description` of non-`user_only` skills | in the assembled system prompt at load | ~100 tokens/skill |
| Skills 2 | full skill body + `available_refs` | on the model's `load_skill` call, **or** preloaded at load if `skills:` lists it | body once |
| Agents body | the child's *own* assembled system prompt | at spawn — the definition body **becomes** the child prompt | child context |

A subagent inherits neither the parent's prompt nor the tier-1 index; it is
composed from its own body (+ brief + any preloaded skills). Disclosure never
leaks downward implicitly.

### Cross-cutting invariant B — enforcement-locus split

The subsystem deliberately puts different gates in different crates. The rule:
**a gate lives where it can actually see the call.**

| Gate | Semantics | Enforcement | Why there |
| --- | --- | --- | --- |
| Tool mask (`tools`/`disallowed_tools`) | core `AgentProfile` | runtime `tool_masked` + core `run_turn` spec filter | host tools round-trip to the runtime, so the runtime can refuse them |
| Spawn control (`can_spawn`/`spawnable_agents`) | core `AgentProfile` | runtime `spawn_refusal` + core `profile_tool_specs` scoping | spawn is a runtime-intercepted tool |
| Permission clamp (`Allow`/`Ask`/`Deny`) | core protocol shape | runtime `tool_runner`, clamped down the ancestor chain | dispatch decision is a runtime concern ([ADR-0024](0024-subagent-permission-gating.md)) |
| `owns_plan` (plan authority) | core `AgentProfile` | **core** `run_turn` + `handle_tool_call` | `update_plan` is a session-state built-in that never round-trips, so `tool_masked` can't see it — core must gate it |
| `propose_plan` accept | runtime-owned tool | runtime executor force-parks on `Ask` | user approval *is* the semantics; rides the existing approval round-trip |

`owns_plan` being enforced in **core** (not the runtime, unlike every other
mask/gate) is the one place the split is not "core = shape, runtime = decision" —
recorded here so it is not mistaken for an inconsistency.

### Cross-cutting invariant C — orthogonal axes, never merged

Two pairs the epic kept deliberately independent because merging each loses a
corner:

- **Skill preload vs access** ([ADR-0043](0043-skill-preload-vs-access-independent-mechanisms.md)):
  `skills:` preloads bodies; the `load_skill` mask governs runtime access.
  Composed, they express "preload X, block the rest" and "preload nothing, request
  on demand".
- **Tool mask vs permission** ([ADR-0038](0038-physical-per-agent-tool-restriction.md)/[ADR-0003](0003-agent-and-permission-profiles.md)):
  the mask decides *which tools exist* for an agent; permission grades
  `Allow`/`Ask`/`Deny` *among the survivors*. A tool absent from the mask is never
  reached by a permission rule.

### Editability is one mechanism, three layers

Agents (`*.md` frontmatter+body), skills (`SKILL.md` directories), and the
provider catalog (#118) share one defaults+override discipline: embedded default
< user (`${config_dir}/entanglement/…`) < project (`<root>/.entanglement/…`),
later wins on the identity key (`name`/`id`), a malformed override is a loud error
(never a silent fallback), and editing a built-in is dropping a same-name file in
a higher layer. Nothing is special about the built-ins except that they ship
embedded.

## Consequences

### Positive

- The six principles are pinned to concrete enforcement points; a change that
  weakens one (e.g. moving skill authorization off the host-tool gate, or
  advertising a masked tool) is visibly a violation of a recorded decision, not a
  quiet drift.
- The tier and enforcement-locus tables give one place to answer "where does X
  happen" without reading eleven ADRs.
- The `owns_plan`-in-core exception and the two orthogonal-axes pairs are recorded
  as intentional, so a later reader does not "fix" them into false consistency.

### Negative / neutral

- This ADR must be superseded, not edited, if a principle's enforcement point
  moves — it is a live map, and a stale map is worse than none. The per-ADR
  cross-links keep the cost of that low.
- It records no new code; its value is entirely as connective documentation.

### Deferred follow-ups (out of epic scope, tracked here so they are not lost)

- **Skill provenance + skill-scoped `allowed_tools` enforcement.** Frontmatter
  `allowed_tools` on a `SKILL.md` is parsed but not enforced — it needs a
  `skill_id` carried onto tool calls made while a skill is active, distinct from
  the #116 *agent* tool mask ([ADR-0036](0036-skill-discovery-and-registry.md)/[ADR-0037](0037-load-skill-tool-deterministic-resolution.md)).
- **Skill-index masking by the agent tool mask.** The #116 mask filters tool
  *specs*, not the tier-1 skill index; an agent that can't `load_skill` still sees
  the index today ([ADR-0035](0035-deterministic-system-prompt-assembly.md)).
- **Filesystem isolation for child roots.** Sub-sessions share the parent root;
  a separate child root is a future sandbox ADR ([ADR-0023](0023-subagent-spawn-limits.md)/[ADR-0024](0024-subagent-permission-gating.md)).

## Alternatives considered

- **No epic-level record — let the eleven sub-ADRs stand alone.** Rejected: the
  cross-cutting invariants (tier recursion, the core-vs-runtime split, the
  `owns_plan` exception, the two orthogonal-axes pairs) are exactly what is *not*
  visible from any single sub-ADR, and are the parts most likely to be eroded by a
  later well-meaning change.
- **Fold this into `docs/architecture.md` §3 only.** Rejected in part: the arch
  doc carries the *what is* (and gains an at-a-glance overview in the same change);
  the *why these six principles bind and where each is enforced* is decision-log
  material, which is what an ADR is for. Both are updated, per the parallel-track
  convention.
- **A new umbrella crate/module to physically co-locate the subsystem.** Rejected:
  the enforcement-locus split (invariant B) is deliberate — core holds shape,
  runtime holds filesystem/decision — and collapsing it into one module would
  reintroduce the UI/transport-in-core hazard the layering gate ([ADR-0006](0006-core-dependency-hygiene-gate.md))
  exists to prevent.

## References

- Epic [#111](https://github.com/xmiksay/entanglement/issues/111): agents, skills
  & system prompt — file-defined profiles, progressive disclosure, deterministic
  assembly
- Sub-ADRs [0033](0033-agent-tool-family-and-blocking-agent.md)–[0038](0038-physical-per-agent-tool-restriction.md),
  [0040](0040-per-profile-spawn-control.md)–[0043](0043-skill-preload-vs-access-independent-mechanisms.md)
  (the eleven local decisions this synthesizes)
- [ADR-0032](0032-yaml-provider-model-catalog.md): the provider catalog whose
  defaults+override shape agents/skills mirror (#118)
- [ADR-0006](0006-core-dependency-hygiene-gate.md): the layering gate that forces
  the enforcement-locus split
