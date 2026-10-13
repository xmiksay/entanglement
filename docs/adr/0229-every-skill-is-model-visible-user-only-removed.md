# 0229. Every skill is model-visible — `user_only` and the `disable-model-invocation` mapping are removed

- Status: Accepted
- Date: 2026-10-14
- Issue: #845 (found in the epic #822 post-merge review)
- Supersedes: the `user_only` parts of
  [ADR-0036](0036-skill-discovery-and-registry.md) (the frontmatter key and
  its disclosure withholding),
  [ADR-0037](0037-load-skill-tool-deterministic-resolution.md) step 2 (the
  `load_skill` refusal),
  [ADR-0043](0043-skill-preload-vs-access-independent-mechanisms.md)'s
  "`user_only` skill is preloadable" difference, and
  [ADR-0074](0074-cross-vendor-skill-and-agent-discovery.md)'s
  "`disable-model-invocation` maps to `user_only`", and
  [ADR-0228](0228-rhai-path-and-args-skill-tools-frontmatter-removed.md)
  §3's "`list_skills` … `user_only` skills withheld"; the
  [ADR-0035](0035-deterministic-system-prompt-assembly.md)/[ADR-0044](0044-agents-skills-system-prompt-epic-synthesis.md)
  mentions of dropping `user_only` skills from the roster fall with them

## Context

ADR-0036 gave a skill a `user_only: true` frontmatter key — "only explicit
user invocation can trigger it" — for destructive/deploy skills. The runtime
honoured it in two places: `SkillRegistry::disclosures` withheld the skill
from the model's roster (`list_skills`), and `load_skill` refused it
(ADR-0037 §2). ADR-0074 mapped Claude Code's `disable-model-invocation` onto
the same flag for foreign skills.

ADR-0227 then gave a skill a third model-reachable surface: its `lib.rhai`
registers in every script as `skill::<skill>::*`. Neither ADR-0227 nor
ADR-0228 considered `user_only`, and the namespace registration has no
filter — so a `user_only` `deploy` skill shipping `fn ship()` was callable
by a model-written `rhai {"script": "skill::deploy::ship()"}` with no user
involvement, its nested calls force-allowed under Run all or a trusted
folder (#845).

Patching that hole (register a `user_only` lib only once the skill is
active) would have kept a flag whose premise does not hold:

- **There is no user-invocation channel.** ADR-0037 already noted that the
  headless engine has no "explicit user command" that loads a skill;
  ADR-0221's slash commands are head-side prompt templates, not skill
  loaders. The only way a `user_only` skill ever reached a session was an
  agent's `skills:` preload — author config, not a user action.
- **Hiding is not authority.** Since ADR-0207 the session's permission mode
  grades every effect a skill's instructions or library can cause, and
  ADR-0194 made skills additive-only. A skill the model must not run is a
  permission question (`deny`/`prompt` rules on the commands it performs),
  not a visibility one. The `load_skill` refusal did stop a model that
  guessed the name, but a plain `read` of the `SKILL.md` got the same body
  past it — and the effects it describes were graded by the mode either
  way.
- **Every new surface needs the filter again.** Disclosure, `load_skill`
  and now script namespaces each had to remember the flag; the next
  surface would have had the same bug.

## Decision

1. **`user_only` is removed.** Every discovered skill is model-visible: it is
   listed by `list_skills` (and `skutter inspect skills --disclosures`),
   `load_skill` renders it, and its `lib.rhai` is callable as
   `skill::<skill>::*` from any script — the same for every skill, with no
   per-skill visibility state. `SkillMeta::user_only` is gone.
2. **A native skill still declaring `user_only` loads** (the key is taken out
   by presence, so the strict `deny_unknown_fields` schema does not abort the
   load) and gets **one** warning naming the removal (ADR-0229) and the
   alternatives — remove the key, or the skill if the model must not reach
   it. Sightings (skill name + source file) are collected per registry load
   and deduped through the shared load-warning mechanism (`config::warn_once`,
   #785/#813) scoped to the load's root and user layer: the scope's set is
   replaced on every load, so the live-reload watcher re-parsing on every
   watched write does not repeat the warning, while a key removed and later
   restored warns again. Unlike ADR-0228's `tools:`, the skill is not skipped: the key
   only ever restricted visibility, so loading it changes nothing the
   permission mode does not already govern.
3. **A foreign skill's `disable-model-invocation` is ignored** like any other
   unknown key of a lenient cross-vendor file (ADR-0074) — no warning; the
   file is not entanglement's to correct.
4. **Preload is unchanged** — any skill can be preloaded by an agent's
   `skills:`; there is no longer a difference from `load_skill` to call out.
5. **Inspect views drop the column** — `skutter inspect skills` and the TUI
   inspect overlay no longer show a `USER_ONLY` column / `(user_only)` marker.

## Consequences

- **(+)** The #845 hole closes by construction: there is no hidden skill
  whose library could leak, and the lib-registration fix the issue proposed
  (register only once active) is moot.
- **(+)** One rule for every skill surface; future surfaces need no filter.
- **(+)** Control over a dangerous skill sits where the rest of authority
  sits — the session's permission mode, the tool overlay and grants.
- **(−)** A skill author who relied on `user_only` to keep a deploy skill
  away from the model loses that; the warning names the change, and the
  migration is a mode rule (e.g. `prompt:` on the deploy command) or moving
  the skill out of the discovered dirs.
- **(−)** Foreign Claude Code skills marked `disable-model-invocation` become
  model-visible in entanglement, unlike in Claude Code; the same permission
  mode grades what they do.
- **(−) New exposure.** In `auto` — or any mode where a skill's commands
  match no `prompt`/`deny` rule — a formerly `user_only` skill can now be
  chosen by the model, loaded, and its `lib.rhai` run from an inline `rhai`
  script with no approval card at any step. A user's existing
  `~/.claude/skills` marked `disable-model-invocation` become visible
  **silently** (no warning — foreign files). Opt-outs: remove the skill (or
  its directory) from the discovered dirs, or set `ENTANGLEMENT_SKILLS_DIR`,
  which replaces the whole user layer and so drops `~/.claude/skills`
  (ADR-0074); otherwise add `prompt`/`deny` rules for the commands the
  skill performs.

## Alternatives considered

- **Register a `user_only` lib only while the skill is active** (the issue's
  first fix). Rejected by the user: it keeps a third copy of a flag with no
  user-invocation channel behind it, and leaves the next surface to repeat
  the bug.
- **Skip a `user_only` skill entirely** (as ADR-0228 does for `tools:`).
  Rejected: the skill's content is valid; only the visibility claim is gone,
  and skipping would silently drop a preloaded skill an agent depends on.
- **Map `disable-model-invocation` to a warning.** Rejected: lenient foreign
  files ignore unknown keys silently by ADR-0074's design; a warning per
  Claude Code skill would be noise the user cannot act on in a file they may
  not own.
