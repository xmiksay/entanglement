# 0037. `load_skill` tool — deterministic resolution, path substitution, provenance

- Status: Accepted
- Date: 2026-07-10

## Context

Skill discovery ([ADR-0036](0036-skill-discovery-and-registry.md)) built tier 1
of progressive disclosure: the `SkillRegistry` and the `name: description`
disclosure list folded into each agent's system prompt. Bodies + payload
(`references/`/`scripts/`) were declared **tier-2, loaded on demand** — but the
loading mechanism did not exist. This ADR is that mechanism (#115, epic #111).

Three forces shape it:

1. **One tool, not one-per-skill.** A per-skill tool would balloon the advertised
   tool list and leak every skill's existence past the `user_only` withholding.
   Skills already select by the model matching its task against a `description`;
   loading should be a single generic call parameterised by `skill_name`.
2. **Resolution must be deterministic, in the handler — never model reasoning.**
   Claude Code's known bug class (anthropics/claude-code#17741, #11011) is the
   *model itself* resolving a `references/x.md` mentioned in a skill body against
   the wrong base directory and guessing. The fix is to resolve every relative
   payload path to an absolute one **before the text reaches the model**, keeping
   `SKILL_DIR` and the project root as two strictly separate coordinate systems.
3. **`load_skill` touches the filesystem**, unlike the orchestration-only
   runtime tools (`agent_spawn`/`ask_user`/`agent_poll`) that bypass permission.
   Reading a skill's on-disk body + payload is a host-resource access, so it must
   be gated by the *same* per-call permission profile as `read` — no exemption.

## Decision

`load_skill { skill_name }` is a **real host tool** in the `ToolRegistry`
(`entanglement_runtime::skills::load_skill::LoadSkillTool`), constructed with a
shared `Arc<SkillRegistry>` and registered in `build_config` alongside the host
quintet. Because it is a registry tool, it flows through the *unchanged* #58/#59
path: core emits `ToolExec`, the runtime executor resolves
`Allow`/`Ask`/`Deny` for the name `load_skill` against the active profile, and
only then runs it. A read-only `explore` profile (default `deny`) refuses it
exactly as it refuses `write` — the "gated like `read`" requirement falls out for
free, with zero executor-interception code.

The handler (`load`) is deterministic over the registry + filesystem:

1. **Look up** `skill_name` in the startup index; unknown → an error the model
   reads and can recover from.
2. **Reject `user_only`.** A `user_only` skill is withheld from disclosure and
   may only be triggered by an explicit user command — a channel the headless
   engine has no model-driven equivalent for — so a model-issued `load_skill` for
   one is refused. (A future user-command path can call `load` with an
   explicit-origin flag; today there is no such caller.)
3. **Substitute paths.** The body's `${SKILL_DIR}`/`$SKILL_DIR` placeholder → the
   absolute skill dir; and every relative path token that **resolves to an
   existing entry under the skill dir** → its absolute path. A token that does
   not resolve there (a project-root ref like `src/main.rs`) is a different
   coordinate system and is left untouched — there is no implicit CWD fallback.
   Existence-at-load-time is the disambiguator, so the rule is deterministic and
   needs no per-skill path schema.
4. **Return an ordinary `tool_result`** carrying `skill_id`, the substituted
   body, and `available_refs` (the supporting files, listed as absolute paths,
   **not** loaded). Never a spoofed user message — the authorship trail stays
   honest.

Built-in skills have no on-disk home (`root_dir == None`): they are single-file,
so there is nothing to substitute and no refs to list; their body is returned
verbatim.

### Provenance

Carrying `skill_id` onto the *tool calls made while a skill is active* (so nested
reads/script runs inherit it, to scope the skill's `allowed_tools` mask and feed
the audit trail) is a field on the runtime's **tool-execution record**, not a
core-protocol change. It is only meaningful once the mask is *enforced*, which is
explicitly deferred to #116 (per-session tool specs). So this ADR ships the
visible half — `skill_id` in the result — and leaves the record-tagging to land
with enforcement, avoiding a protocol change that would carry no behaviour today.

## Consequences

- **Positive.** No new executor interception, no core surface: `load_skill` reuses
  the existing permission dispatch and `ToolExec`/`ToolResult` round-trip. Path
  substitution closes the model-guesses-the-base bug class at the source. `SKILL_DIR`
  and project root stay separate coordinate systems.
- **Positive.** One generic tool keeps the advertised tool list flat and preserves
  `user_only` withholding (the tool refuses names the model was never shown).
- **Negative / deferred.** `allowed_tools` masking and full provenance propagation
  onto nested tool calls are not enforced yet (#116). A `user_only` skill is
  currently unreachable (no user-command caller), which is acceptable while that
  path does not exist.
- **Neutral.** Substitution is heuristic on free-text bodies: a token is rewritten
  iff it resolves under the skill dir. Authors wanting an unambiguous absolute
  reference can use the `${SKILL_DIR}` placeholder.

## Alternatives considered

- **One tool per skill.** Rejected: floods the tool list, leaks `user_only` skills,
  and duplicates schemas. The `skill_name` parameter is strictly simpler.
- **A runtime-owned interception tool (like `ask_user`) that bypasses permission.**
  Rejected: `load_skill` reads host files, so bypassing the profile would let a
  read-only agent exfiltrate skill payloads the profile means to deny. Gating it as
  an ordinary host tool is the security-correct choice.
- **Substitute by hardcoded `references/`/`scripts/` prefixes.** Rejected as less
  general than existence-under-skill-dir, which also handles arbitrary payload
  layouts while still leaving genuine project-root refs alone.
- **Inject the body as a synthesized user message.** Rejected: it forges authorship
  and corrupts the audit trail. A `tool_result` is the honest channel and reuses the
  #58 round-trip with no new semantics.
- **A core-protocol `skill_id` field on tool calls now.** Rejected: with no
  enforcement (#116) it would be dead protocol weight; deferred to land together.
