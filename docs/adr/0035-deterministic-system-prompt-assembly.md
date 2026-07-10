# ADR-0035: Deterministic system-prompt assembly

- Status: Accepted
- Date: 2026-07-10
- Issue: [#113](https://github.com/xmiksay/entanglement/issues/113) (epic
  [#111](https://github.com/xmiksay/entanglement/issues/111))
- Supersedes: none (extends [ADR-0034](0034-file-based-agent-definitions.md))

## Context

Before #113 the system prompt was a single opaque string: the agent-definition
body became `AgentProfile.system_prompt` verbatim, and core shipped it straight
into `LlmRequest.system`. Anything an agent needed the model to know — safety
invariants, the project brief, the working directory, the available skills — had
to be hand-written into every agent body. That is fragile and non-composable:

- **Shared invariants silently drop.** Claude Code does *not* re-apply a shared
  preamble to subagents; the moment an agent supplies its own body, global safety
  and output rules vanish for it.
- **Environment facts get guessed.** cwd/platform/date hand-written into a body
  go stale; the model otherwise hallucinates them.
- **Skill/brief boilerplate is duplicated** across every definition and drifts.

We want composition to be an explicit, deterministic **harness** function — no
model involvement, unit-testable — and to keep core a pure pass-through.

## Decision

Compose the system prompt from up to five ordered, individually-optional parts:

1. **shared preamble** — invariants every agent must honour (safety, tool-use,
   output). Applied to *every* agent, including subagents — the opt-out is an
   empty preamble file, not "an agent defined its own body".
2. **agent body** — the markdown body of the definition.
3. **project brief** — a project-instructions file (`.entanglement/BRIEF.md` or
   `AGENTS.md`), folded in only when the definition sets `include_brief: true`.
4. **environment block** — cwd/root, platform, date; *generated* by the harness,
   never model-guessed.
5. **skill index** — tier-1 disclosure lines (`name` + `description` only)
   generated from the skill registry, never authored into a body.

The pure function `entanglement_runtime::system_prompt::assemble(body,
include_brief, mode, ctx)` joins the present parts with blank lines. A
**subagent** (`AgentMode::Subagent`) gets `preamble + body (+ brief)` only — the
env block and skill index are reserved for primary/`all` sessions, and a child is
composed from *its own* body and *its own* `include_brief` flag, never the
parent's assembled prompt.

Composition is baked into each `AgentProfile.system_prompt` at load time
(`load_registry(root, &PromptContext)`), so every downstream consumer — session
start, `SetAgent`, spawn — reads the already-assembled prompt and core stays a
verbatim pass-through into `LlmRequest.system`.

`PromptContext::load(root)` resolves the inputs once at startup: the shared
preamble (built-in default, overridable by `ENTANGLEMENT_PREAMBLE_FILE` / a
project or user `preamble.md`), the project brief (`ENTANGLEMENT_BRIEF_FILE` /
`.entanglement/BRIEF.md` / `AGENTS.md`), and the generated env block. The skill
index is empty until the skill registry lands (#115); once it does, callers
filter by the agent's tool mask (omit when `load_skill` is masked, #116) and drop
`user_only` skills before handing the list to `assemble`.

## Rejected alternatives

- **Assemble in core at turn time.** Keeps the parts on `EngineConfig` and
  concatenates inside `run_turn`. Rejected: composition is a runtime/harness
  concern (ADR-0006 dependency hygiene); core reading brief files or generating an
  env block re-imports policy core shed in #59. Pre-baking into the registry keeps
  the single consumption site (`LlmRequest.system = &profile.system_prompt`)
  untouched.
- **Per-session composition keyed on primary-vs-subagent at spawn.** The registry
  is already per-profile, and `AgentMode` distinguishes a subagent leaf — so the
  reduced form falls out of the mode without threading spawn context through core.
- **A dedicated env/skill message variant.** Overkill: these are static per
  session and belong in the system string, not a protocol frame.

## Consequences

- New frontmatter flag `include_brief: bool` (default `false`) on agent
  definitions — opt-in, deny-unknown-fields still holds.
- `load_registry` gains a `&PromptContext` parameter; `PromptContext::default()`
  is the identity composition (raw body) used by the parsing tests.
- The env block's date is snapshotted at startup (process-lifetime constant) —
  acceptable; a long-lived session does not need a live clock in its prompt.
- Skill filtering (#116) and the skill registry (#115) remain follow-ups; the
  assembler already renders a skill list, so wiring them is additive.
