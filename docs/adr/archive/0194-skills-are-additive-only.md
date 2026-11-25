# 0194. Skills are additive-only — the `allowed_tools` mask is removed

- Status: Accepted
- Date: 2026-09-13
- Supersedes: [ADR-0106](0106-skill-scoped-allowed-tools-enforcement.md) (whole —
  the session-keyed `ActiveSkill` map, `skill_masked`, the activation parse,
  and the load-time `allowed_tools` semantics are retired; the
  `OutEvent::SkillActive` wire event itself survives as posture-only, below).
- Retires: [ADR-0129](0129-thread-the-skill-mask-into-rhai-binding-resolution.md)'s
  threading — `BindingPolicy`'s `active_skill` parameter and
  `Decision::SkillMasked` go with the mask they carried; the ADR's stated
  rationale ("no built-in or documented skill combines `allowed_tools` with
  `rhai`") turned out to be the general truth: no skill's one-turn mask was
  ever a real boundary.
- Relates to: [ADR-0192](0192-universal-advertisement-enforcement-at-dispatch.md)
  (the dispatch gate loses one of its three mask layers; the attributed-decline
  table drops its skill row), [ADR-0193](0193-two-tool-call-modes-and-lazy-tool-discovery.md)
  (the ladder the unwrap sits atop gets simpler), [ADR-0037](0037-load-skill-tool-deterministic-resolution.md)
  (the frontmatter field's original parse-only posture — restored, with a
  warning), [ADR-0036](0036-skill-discovery-and-registry.md) (the frontmatter
  field itself).

## Context

ADR-0106 made a `SKILL.md`'s `allowed_tools` frontmatter enforce something:
while a skill is active (a resolved `load_skill` until that turn's `Done`),
`permission::skill_masked` refuses any tool call outside the list, layered
after the #116 agent mask. ADR-0129 then threaded the same mask into `rhai`'s
`BindingPolicy` snapshot to close the binding-route asymmetry. Both were
built carefully. Neither bought a boundary.

Three observations, in escalating weight:

1. **The scope is one turn of one session.** A skill's mask lasts from
   `load_skill` until the next `Done` — the model that loaded the skill is
   the same model the mask restrains, and it clears on turn end without an
   unload verb. A restriction the restricted party triggers and outlives by
   finishing its turn is not a security boundary; it is a speed bump.
2. **The real control is elsewhere and always was.** The agent mask
   (#116/ADR-0038, ancestor-clamped), the permission profile, and the
   config ceiling are the standing boundaries; a skill can never *widen*
   them (it layers after and only narrows). ADR-0106's own framing — "a
   *narrower*, *shorter-lived* restriction layered on top" — is accurate,
   and is also the argument for deleting it: the layer adds a third
   enforcement surface whose entire effect is subtractive within a scope no
   adversary model matches.
3. **It costs real behavior.** A skill with `allowed_tools: [bash, read,
   grep]` disarms editing for the rest of the loading turn (#554 was filed
   against exactly this); every skill author must enumerate the complete
   tool set or silently break the turn — the same "complete set" footgun
   ADR-0106 itself noted when it declined to exempt `load_skill`.

Meanwhile ADR-0192 already removed the mask's *advertisement* half (the
skills roster stopped filtering specs), leaving `skill_masked` as one more
arm in the dispatch ladder — one more `decline.rs` wording, one more thing
every change to the ladder must thread (ADR-0129 was the second such
threading), for a restriction with no boundary to enforce.

## Decision

**Skills are additive-only.** A skill adds capabilities (its body, endpoint
refs, rhai-backed tools, aliases — ADR-0193's Phase 8 surface); it never
restricts the session's tool set. The `allowed_tools` mask is removed
outright:

- `permission::ActiveSkill`/`skill_masked` and the activation hook (the
  `load_skill` result-header parse that recorded it) are deleted; the
  dispatch ladder's skill-mask arm goes with them, and the attributed-decline
  table (ADR-0192) drops its `Declined by skill …` row.
- `script.rs`'s ADR-0129 threading is removed with it: `BindingPolicy`
  loses the `active_skill` parameter and `Decision::SkillMasked`.
- **`OutEvent::SkillActive` stays on the wire** as posture-only (a head can
  still show "skill X active"), but `SkillActive.allowed_tools` goes
  **vestigial**: still serialized when present (wire compat for replay of
  existing logs), never read, never enforced. The field is retained for
  log-replay compatibility only.
- **`SkillFrontmatter.allowed_tools` is parsed-but-ignored with a one-time
  load warning** — `SkillFrontmatter` is `deny_unknown_fields`, so the field
  must stay a known key or every existing skill with the frontmatter errors
  at load. A skill carrying it gets one `tracing::warn!` per load
  ("`allowed_tools` is no longer enforced; skills are additive-only"),
  pointing authors at deleting it. Hard removal (unknown-field error) is a
  later, separate change once the warning has run its course.
- **The foreign (cross-vendor) layer already drops it** — ADR-0074's lenient
  parse reads only `name`+`description` and ignores unknown keys, exactly as
  it already drops Claude-style `allowed-tools`. No foreign-layer change.
- **The additive direction is strict-layers-only.** What skills *add* — the
  Phase-8 `tools:` frontmatter (endpoint refs, rhai-backed tools, aliases)
  on the [ADR-0193](0193-two-tool-call-modes-and-lazy-tool-discovery.md)
  plan — is honored **only in the strict/native layers** (embedded, user
  `entanglement`, project `.entanglement`), never in the cross-vendor dirs:
  the lenient foreign parse drops `tools:` exactly as it already drops
  Claude-style `allowed-tools`. A foreign skill file can carry prose (its
  body, loadable via `load_skill`) but cannot define executable capability;
  importing tool definitions from an ecosystem file is a trust decision the
  native layers' explicit placement already makes.

## Consequences

### Positive

- The ladder simplifies: one fewer mask layer at dispatch, one fewer
  `BindingPolicy` parameter, one fewer decline wording, one fewer thing the
  ADR-0129 class of change has to thread.
- The #554 footgun is gone: loading a skill mid-turn can never disarm
  editing (or anything else) for the rest of the turn. Skills become purely
  safe to load, matching how they are described.
- The permission story is honest again: the standing boundaries (agent mask,
  permission profile, ceiling) are the *whole* story, and each has a real
  enforcement scope.

### Negative / accepted

- **Skill authors lose their only self-imposed guardrail.** ADR-0106's
  positive consequence — "a skill author can hand a model a narrowly-scoped
  capability set and the runtime enforces it physically" — is withdrawn.
  An author who wants a restricted model must now express it as an agent
  profile (a `tools:` mask) and delegate to it, not as skill frontmatter.
  This is the real cost of the ADR and it is deliberate: a guardrail that
  only restrains the party that invoked it, for one turn, was not carrying
  weight. The author's honest replacement is an agent definition, whose mask
  is standing and ancestor-clamped.
- A vestigial wire field (`SkillActive.allowed_tools`) is carried until hard
  removal. Chosen over dropping it now to keep replay of existing logs
  deserializing without a shim.
- The one-time warning is log noise for every existing skill that carries
  the field — bounded (once per load), and self-extinguishing as fields are
  deleted.

## Alternatives considered

- **Keep the mask but fix the ergonomics** (capability fan-out, implicit
  `load_skill` exemption — the ADR-0106 review notes both). Rejected: both
  patches make the layer *smarter* without giving it a boundary. The problem
  is the scope, not the enumeration.
- **Keep the mask, dispatch-only, as ADR-0192 left it** (status quo). Rejected
  for observation 3: a standing footgun (#554) plus a second threading
  surface (ADR-0129) for zero boundary. "Enforced but meaningless" is worse
  than "not enforced" — it teaches authors the wrong model of what a skill
  can promise.
- **Repurpose `allowed_tools` as prompt guidance** (persona text, not code).
  Rejected: ADR-0044's "physical over prompted" principle cuts the other way
  here — a prompted restriction that looks like the (removed) enforced one
  would be worse than none: same author confusion, no honesty about what it
  is. Deleting the semantics and warning is cleaner than redefining them as
  a suggestion.
- **Move the mask to agent-profile scope** (a skill implies a profile
  switch). Rejected: it reinvents `SetAgent` inside `load_skill`, couples
  two orthogonal mechanisms (ADR-0043's preload-vs-access split exists
  precisely because they are orthogonal), and would re-introduce mid-turn
  profile mutation — the thing the mask's own turn-scoped clearing was
  designed to avoid.

## References

- [ADR-0106](0106-skill-scoped-allowed-tools-enforcement.md): the mask this
  supersedes
- [ADR-0129](0129-thread-the-skill-mask-into-rhai-binding-resolution.md):
  the binding threading retired with it
- [ADR-0192](0192-universal-advertisement-enforcement-at-dispatch.md): the
  dispatch ladder that loses a layer; the decline table that loses a row
- [ADR-0193](0193-two-tool-call-modes-and-lazy-tool-discovery.md): the
  additive surface (endpoint refs, skill tools) skills now only ever add to
- [ADR-0036](0036-skill-discovery-and-registry.md)/[ADR-0037](0037-load-skill-tool-deterministic-resolution.md):
  the frontmatter field's parse-only origin — restored with a warning
- [ADR-0074](0074-cross-vendor-skill-and-agent-discovery.md): the lenient
  foreign parse that already drops `allowed-tools` and now drops `tools:`
  for the same reason (ADR-0193 Phase 8)
- [ADR-0043](0043-skill-preload-vs-access-independent-mechanisms.md):
  preload vs access — the orthogonality the agent-scope alternative would
  have collapsed
