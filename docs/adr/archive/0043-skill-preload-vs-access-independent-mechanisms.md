# 0043. Skill preload (`skills:`) vs access (`load_skill` mask) — independent mechanisms

- Status: Accepted
- Date: 2026-07-11

## Context

Skill progressive disclosure has two tiers ([ADR-0036](0036-skill-discovery-and-registry.md),
[ADR-0037](0037-load-skill-tool-deterministic-resolution.md)): tier-1 is a
`name: description` index in the system prompt; tier-2 is the full body, loaded on
demand by the `load_skill` host tool. An agent definition needs to influence two
*different* things about skills:

- **Preload** — inject a skill's full body into an agent's context up front, so it
  is present without a `load_skill` round-trip (e.g. a `committer` agent that should
  always have the commit skill loaded).
- **Access** — whether the agent may load *other* skills at runtime at all.

A single knob (e.g. "`skills:` is the agent's skill allowlist, and its bodies are
preloaded") conflates the two and loses two corner cases:

- "preload X **but block everything else**" — needs preload without granting
  general access;
- "preload nothing, **let it request what it needs**" — needs access without any
  preload.

## Decision

Keep the two as **independent mechanisms** on the agent definition.

- **Preload** is `skills: [name, …]` frontmatter (`AgentDefinition.skills`). At
  agent-load time each listed skill's body is resolved via
  `SkillRegistry::preload_body` — the *same* substitution pipeline as `load_skill`
  (`load_skill::render_skill`: `${SKILL_DIR}` + relative-path absolutization,
  `available_refs` listing) — and injected into the assembled `system_prompt` as a
  dedicated "Preloaded skills" section by `system_prompt::assemble`. It is preload
  **only**, never an allowlist.
- **Access** is the existing [ADR-0038](0038-physical-per-agent-tool-restriction.md)
  tool mask. `load_skill` is a real host tool, so an agent that must not load skills
  at runtime simply doesn't advertise it (`disallowed_tools: [load_skill]`, or a
  `tools:` allowlist omitting it) — refused from the advertised specs (core's
  `run_turn` filter) and at dispatch (`runtime::permission::tool_masked`). No new
  code: this fell out of #116 the moment `load_skill` became a host tool.

The two compose to preserve both corners, and the default stays **permissive** — a
subagent may discover + load any skill via the same LLM gate as a primary unless
masked.

### Preload is mode-independent

`assemble` gates the env block and the tier-1 index behind `mode != Subagent`
(ADR-0035), but preload is **not** gated by mode: it is author-requested, and the
subagent-spawn case is precisely what it is for. A spawned subagent with `skills:`
gets the body even though its tier-1 index is withheld. Preload is also **additive,
not an allowlist** — the tier-1 index still discloses every other non-`user_only`
skill.

### Two deliberate differences from model-facing `load_skill`

`SkillRegistry::preload_body` reuses `load_skill`'s rendering but not its policy:

- a `user_only` skill **is** preloadable — `user_only` blocks *model* self-trigger,
  and preload is *author* config, not a model action;
- an unknown skill name is a **loud load-time error** (agent definitions never
  silently drop a typo'd field), not a runtime `tool_result` error.

## Consequences

### Positive

- Both expressiveness corners survive; neither mechanism is overloaded onto the
  other.
- Zero new protocol surface and zero new enforcement path: preload is a load-time
  system-prompt composition; access is the already-live #116 mask.
- Preload reuses the exact `load_skill` substitution, so a preloaded skill reads
  identically to one the model loads itself (same absolute paths, same
  `available_refs`).

### Negative / neutral

- Preload bakes the body into `system_prompt` at load, so it costs tokens on every
  turn for that agent — the intended trade for an always-needed skill. Agents that
  only *sometimes* need a skill should leave it to `load_skill`.
- Skill-scoped `allowed_tools` enforcement (ADR-0036/0037's deferred provenance
  work) is unchanged and still deferred; it is orthogonal to both axes here.

## Alternatives considered

- **Merge preload and access into one `skills:` allowlist.** Rejected: loses the
  "preload X but block the rest" and "preload nothing, request on demand" corners,
  as above.
- **A separate `skill_access`/allowlist field.** Rejected: redundant with the #116
  tool mask — masking `load_skill` already *is* the access control, with the same
  advertise-and-dispatch enforcement as every other tool.
- **Reject `user_only` skills in preload (reuse `load` verbatim).** Rejected: an
  author explicitly listing a `user_only` skill in `skills:` is an opt-in, not a
  model self-trigger — the guard doesn't apply.

## References

- Issue #117: runtime skill preload (`skills:`) vs access (`load_skill` mask)
- Epic #111: agents/skills/system-prompt
- [ADR-0035](0035-deterministic-system-prompt-assembly.md): deterministic
  system-prompt assembly (the `assemble` composition preload extends)
- [ADR-0036](0036-skill-discovery-and-registry.md): skill discovery + registry
  (tier-1; `user_only`)
- [ADR-0037](0037-load-skill-tool-deterministic-resolution.md): `load_skill`
  (tier-2; the substitution pipeline preload reuses)
- [ADR-0038](0038-physical-per-agent-tool-restriction.md): physical per-agent tool
  restriction (the access mask this reuses unchanged)
