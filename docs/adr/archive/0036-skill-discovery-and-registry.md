# 0036. Skill discovery + registry — SKILL.md frontmatter, tier-1 disclosure

- Status: Accepted
- Date: 2026-07-10

## Context

Deterministic system-prompt assembly ([ADR-0035](0035-deterministic-system-prompt-assembly.md))
left a **skill index** slot in the composed prompt but shipped it empty: the
skill registry that feeds it did not exist yet. This ADR fills that slot — tier 1
of the progressive-disclosure design in the agents/skills/system-prompt epic
(#111).

A **skill** is a directory with a `SKILL.md` (YAML frontmatter + markdown body)
plus optional supporting files (`references/*.md`, `scripts/*`). The problem is
the same one the provider catalog (#118, [ADR-0032](0032-yaml-provider-model-catalog.md))
and file-based agents (#112, [ADR-0034](0034-file-based-agent-definitions.md))
already solved: an embedded default set, overridable per user and per project,
with one loader for stock and custom alike.

## Decision

Skills are discovered at startup by the **runtime**
(`entanglement_runtime::skills::load_registry`) into a `SkillRegistry` keyed by
`name`. Frontmatter is the tier-1 contract; the body + payload are tier-2, loaded
on demand — never preloaded.

```
---
name: commit                  # unique id — the tier-2 load / invocation key
description: Write a …         # the ONLY field (with name) disclosed to the model
user_only: true               # optional — only explicit user invocation can trigger it
allowed_tools: [bash, read]   # optional — tool mask active while the skill is loaded
---
# markdown body … (tier-2, loaded on demand)
```

**Three layers, later wins on a `name` collision (project > user > built-in):**

1. **built-in** — embedded `include_str!` `SKILL.md` files, parsed through the
   *same* loader. Stock skills are **single-file** (body only, no on-disk
   `references/`/`scripts/`); anything needing supporting files lives on disk.
   Editing a stock skill = dropping a same-`name` `SKILL.md` in a higher layer.
2. **user** — `${config_dir}/entanglement/skills/**/SKILL.md` (override:
   `ENTANGLEMENT_SKILLS_DIR`).
3. **project** — `<root>/.entanglement/skills/**/SKILL.md`.

Discovery is a **recursive walk** for `SKILL.md` markers (so `<name>/SKILL.md`
and deeper nesting both work). Symlinked duplicates — a link to an already-seen
`SKILL.md`, or a directory-cycle symlink — are deduped by canonical path, which
also breaks link cycles. A malformed `SKILL.md` in any layer is a **loud error**,
never a silent skip; the embedded stock skills are guarded by a unit test so
their parse is infallible.

**`root_dir` resolved once.** Each `SkillMeta` records the directory holding its
`SKILL.md` (and its `references/`/`scripts/` payload) at discovery time; nothing
downstream re-derives it. Built-ins have `root_dir = None` (no on-disk home).

**Disclosure — only `name` + `description`.** The registry renders one
`name: description` line per skill into the assembled system prompt's skill index
(~100 tokens/skill). Bodies are never in the prompt. `user_only` skills are
**withheld** from the disclosure list so the model cannot self-trigger a
destructive/deploy skill — those are reachable only by explicit user invocation.

**Selection stays LLM reasoning.** No keyword router, no embedding gate: the
model matches its task against the `description` text in its own forward pass, so
description quality is the contract (stock skills carry explicit trigger
phrasing). A keyword/embedding pre-filter is acceptable *later* only as a
candidate shortlist if the library outgrows the context budget — never as the
final gate (it breaks semantic matching and is trivially triggerable by untrusted
text the model reads).

**Declared-but-deferred frontmatter.** `allowed_tools` is parsed and carried on
`SkillMeta` now — so a `SKILL.md` is a stable contract — but its **enforcement**
(masking the session's tools while a skill is loaded) is tier-2 work tracked in a
follow-up (#116, per-session tool specs). Likewise `SkillRegistry::get` is the
tier-2 resolution path (`load_skill`, #115) and is exercised by tests until that
consumer lands.

## Consequences

- **(+)** Skills are authorable/editable as files with the same defaults+override
  mental model as agents and the provider catalog; one loader, no privileged
  stock path.
- **(+)** Tier-1 cost is bounded (`name` + `description` only); bodies stay out of
  the prompt until explicitly loaded.
- **(+)** `root_dir` captured once keeps tier-2 payload resolution (references,
  scripts) unambiguous and cheap.
- **(+)** `user_only` gives destructive/deploy skills a home the model can't reach
  on its own.
- **(−)** The recursive walk + canonicalization does real filesystem work at
  startup; bounded by the skills tree size, and a broken symlink is skipped, not
  fatal.
- **(−)** A frontmatter surface to document and validate; `deny_unknown_fields`
  keeps typos loud but makes adding a field a deliberate schema change.

## Alternatives considered

- **A single `skills.yml` list** (like the catalog's one file). Rejected: a skill
  body is free-form markdown and carries a payload directory (references/scripts);
  one-directory-per-skill matches how users think about "a skill" and where its
  files live.
- **Keyword/embedding router to pick skills.** Rejected as the *gate*: it breaks
  semantic matching and is trivially triggerable by untrusted text the model
  reads. Allowed later only as a pre-filter shortlist, never the final decision.
- **Preload bodies into the prompt.** Rejected: defeats progressive disclosure —
  the whole point is that only tier-1 metadata is always-on.
- **Put discovery in core.** Rejected: filesystem I/O + XDG dirs belong in the
  runtime ([ADR-0006](0006-core-dependency-hygiene-gate.md)); core stays pure.
  `SkillMeta`/`SkillRegistry` live in the runtime and feed `PromptContext.skills`,
  which core ships verbatim in `LlmRequest.system`.

[0006]: 0006-core-dependency-hygiene-gate.md
[0032]: 0032-yaml-provider-model-catalog.md
[0034]: 0034-file-based-agent-definitions.md
[0035]: 0035-deterministic-system-prompt-assembly.md
