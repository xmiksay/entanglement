# 0074. Cross-vendor skill & agent discovery (`~/.claude`, `.claude/`, `.agents/`)

- Status: Accepted
- Date: 2026-07-15
- Extends the layered-definition model of [0034](0034-file-based-agent-definitions.md)/[0036](0036-skill-discovery-and-registry.md) and the shared loader of #204; trust framing per [0047](0047-local-trust-boundary.md). Mirrors the brief-chain precedent of [0035](0035-deterministic-system-prompt-assembly.md).

## Context

Skill and agent definitions were discovered from exactly two location families:
the native user dir (`${config_dir}/entanglement/<kind>`, overridable via
`ENTANGLEMENT_{SKILLS,AGENTS}_DIR`) and the native project dir
(`.entanglement/<kind>`). Meanwhile the ecosystem converged on other homes for
the *same file shapes*: Claude Code keeps user skills in
`~/.claude/skills/<name>/SKILL.md` and agents in `~/.claude/agents/*.md`
(frontmatter `name` + `description` — identical tier-1 contract), and the
cross-vendor convention puts project definitions under `.agents/`. A user with
an existing Claude Code setup got **nothing**: the TUI and every head silently
saw only built-ins. The brief chain already honors these conventions
(`AGENTS.md` / `.agents/AGENTS.md` / `.claude/CLAUDE.md`); definitions did not.

Scanning foreign dirs naively is hazardous: both parsers are
`deny_unknown_fields` and a malformed file aborts the whole load. Real Claude
Code files carry keys entanglement rejects — agents with `tools: Read, Grep` (a
comma-separated *string*), `model: sonnet`, `color`; skills with
`allowed-tools`, `license`, `argument-hint` — so a strict scan of `~/.claude`
would brick startup on files entanglement never owned.

## Decision

Foreign dirs slot into the **existing** `Layer::User` / `Layer::Project` slots,
scanned before the native dir of the same layer so native wins on a `name`
collision. The shared loader (`layers::candidate_dirs`) yields, in order:

1. built-in (embedded)
2. user: `~/.claude/<kind>` (**lenient**) → `${config_dir}/entanglement/<kind>` (strict)
3. project: `.claude/<kind>` (**lenient**) → `.agents/<kind>` (**lenient**) → `.entanglement/<kind>` (strict)

- `~/.claude` resolves via `dirs::home_dir()` — Claude Code hardcodes `$HOME`,
  not XDG. `.agents` outranks `.claude` for the same reason the brief chain
  ranks `.agents/AGENTS.md` above `.claude/CLAUDE.md`: cross-vendor beats
  vendor-specific.
- **Strict native, lenient foreign.** Native dirs keep `deny_unknown_fields` +
  abort-on-malformed — loud authoring feedback on files written *for*
  entanglement. Foreign dirs parse through permissive mirror structs (only
  `name` + `description` read, unknown keys ignored) and a malformed file is
  `warn!`ed and **skipped**, never fatal.
- **`ENTANGLEMENT_{SKILLS,AGENTS}_DIR` replaces the whole user layer** (foreign
  + native), not just the native dir. This keeps every test hermetic (they
  point the vars at nonexistent dirs; otherwise the suite would leak the
  developer's real `~/.claude`) and doubles as the opt-out for cross-vendor
  discovery.
- **Foreign agents default `mode: all`** (native default: `primary`): a Claude
  Code agent is a delegation target, so it must be spawnable; `all` keeps it
  selectable as a primary too. Everything else defaults open (allow-all
  permission, no tool mask, no brief, no preload) — shadow with a native
  definition to restrict.
- **Skill `disable-model-invocation` maps to `user_only`** (same semantics: the
  model must not self-trigger it). Claude's `allowed-tools` is deliberately
  dropped: its tool names (`Bash(git:*)`, `Read`) do not map onto
  entanglement's, and `allowed_tools` enforcement is deferred anyway —
  fail-open, consistent with the native field's current status.

Trust follows [0047](0047-local-trust-boundary.md) unchanged: `~/.claude` is
user-trusted exactly like `${config_dir}/entanglement`; project `.claude/` and
`.agents/` are project-layer-trusted exactly like `.entanglement/`. The
mitigation stays inspection (`skutter inspect skills|agents` and the TUI
`/inspect` overlay show winning layer + source path + shadowed definitions;
the `replaces=` debug log now names the shadowed *file*, not just its layer).

## Consequences

- A Claude Code setup works out of the box: `~/.claude/skills`, `~/.claude/agents`,
  and a repo's `.agents/skills` are discovered with zero config.
- A foreign `name` colliding with a built-in (e.g. a `~/.claude/skills/commit`)
  silently shadows it — visible via inspect provenance and the `replaces=` log,
  same as any native override.
- Foreign skills are model-invocable unless `disable-model-invocation` is set —
  matches Claude Code's own default.
- The env override's meaning widened from "replace the native user dir" to
  "replace the user layer"; since foreign user dirs did not exist before, no
  existing setup changes behavior.

## Alternatives rejected

- **A fourth `Layer` variant per foreign dir** (`UserClaude`, …): ripples through
  the `Ord`-based precedence, `label()`, every inspect table and test, for no
  semantic gain — the trust class is the same as the layer it slots into; the
  `source` path already tells dirs apart.
- **Lenient parsing everywhere**: would soften the "loud, never a silent
  fallback" doctrine for files the user wrote *for* entanglement, where a typo'd
  key should fail the load, not vanish.
- **Config-file opt-in for foreign dirs**: convention-over-configuration; the
  whole point is that an existing Claude setup works with zero config, and the
  env override remains the opt-out.
