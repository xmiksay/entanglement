# 0166. Migration note + startup warning for stale tool names in masks and permission rules

- Status: Accepted
- Date: 2026-08-04
- Amends: [ADR-0161](0161-unified-async-work-background-flag-and-one-poll.md) ("Config churn" —
  promises this exact change "as part of the change, not a follow-up")

## Context

ADR-0161 removed `bash_output`, `agent_poll` and `agent_spawn` outright (`poll`/`agent` replace
them, no aliases). ADR-0033 already established that tool names are opaque strings on the wire, so
a rename is free for **session logs** — the name is just a label on `ToolExec`/`ToolOutput` and
replays fine either way. That reasoning does not extend to the three places a tool name is also a
*live config key*:

- an agent's `tools`/`disallowed_tools` mask (`entanglement-runtime/src/agents/*.md`, ADR-0038/0148)
- a `permission:` rule key, in an agent's own frontmatter or the user config's `permissions:`
  section (identical shape, ADR-0114/0116)
- a persisted grant in `grants.yml` (ADR-0052)

A mask/rule keyed on a name nothing calls anymore doesn't error — it just stops matching. `tools:
[read, agent_spawn]` silently drops to "no spawn tool advertised at all" rather than "spawn via
`agent`"; a `permission: { agent_poll: ask }` rule silently never fires. Both degrade a working
config into a subtly broken one with no signal at load time — exactly the "stale config degrades
quietly" failure ADR-0161 named and deferred to this issue (#623, part of #604).

## Decision

### 1. A compile-time tool-name vocabulary, independent of what's registered this run

`tool_names::KNOWN_TOOL_NAMES` lists every literal tool name that exists in the codebase — the
root-contained quintet, `apply_patch`, `bash`/`call`, `rhai`, and every runtime-owned tool
(`ask_user`/`poll`/`propose_plan`/`agent`/`update_tasks`/`load_skill`/`read_raw`/`mcp_enable`).
This is deliberately **not** derived from the live `ToolRegistry`: `bash` is env-gated
(`ENTANGLEMENT_ENABLE_BASH`), `rhai` is a Cargo feature, and MCP tools connect *after* agent
profiles load — a registry-driven check would flag `plan.md`'s own `bash` entry as "unknown" on an
ordinary run with bash disabled. A fixed vocabulary sidesteps that entirely; it only drifts when a
tool is actually added or removed in source, which is a one-line addition to the list, not a
runtime wiring concern.

`tool_names::is_recognized_mask_entry(entry)` is the single predicate: true for a known literal
name, a capability key (`read`/`write`/`call`, [ADR-0114](0114-capability-level-permission-keys.md)),
a `*`/`?` wildcard pattern ([ADR-0148](0148-glob-patterns-in-the-agent-tool-mask.md) — matched
dynamically, so it can't be checked against a fixed list by construction), or an MCP tool
(`mcp__<server>__<tool>`, unknowable until the server connects,
[ADR-0117](0117-mcp-tool-capability-fan-out.md)). Anything else is flagged.

### 2. A load-time warning, not a load error

`agents::warn_unrecognized_mask_entries` runs on every parsed `AgentProfile` — built-in, user, and
project layers alike, so it also catches a stale *built-in* definition, not just user overrides —
checking `tools`, `disallowed_tools`, and every `permission:` rule key (after capability expansion,
so the check sees the same literal tool names `PermissionProfile::resolve` matches against). Each
unrecognized entry gets one `tracing::warn!` naming the agent, the field (`tools` /
`disallowed_tools` / `permission`), and the offending entry. `warn` is visible by default — the
fallback log filter is `warn` even with no `--verbose`/`RUST_LOG`
(`entanglement-runtime/src/logging.rs`) — so this is a startup-visible signal with no extra flag to
remember.

This stays a warning, never a `bail!`, unlike the strict-layer "malformed frontmatter aborts"
behavior [`agents/mod.rs`](../architecture/agents-and-permissions.md) otherwise uses for a native
layer. A mask entry this check can't vouch for is *suspicious*, not *provably wrong* — the same
"unowned data, be lenient" posture ADR-0074 already takes for foreign agent files applies here for
a different reason: aborting startup over one stale line in an otherwise-working config is a worse
failure mode than degrading that one entry and saying so loudly.

### 2b. Grant files are not validated

`grants.yml` keys are checked only for exact equality against the tool name of a live call
(`grants::is_granted`) — a stale key naming a removed tool simply never matches anything again. It
costs nothing but a harmless dead line in the file; unlike a mask or permission rule, there is no
degraded-but-still-functioning state to warn about, so no check was added for it. The migration
note below still tells an operator to prune it, purely for file hygiene.

### 3. Migration note — what to check when upgrading past this rename

If an agent definition, the user `permissions:` config, or `grants.yml` predates the ADR-0161
rename, search for `bash_output`, `agent_poll`, `agent_spawn`:

- **`agent_spawn` → `agent`.** Any `tools:`/`disallowed_tools:`/`permission:` entry naming
  `agent_spawn` should name `agent` instead; there is no behavioral difference to configure around —
  `agent { background: true }` is a call-time argument, not a separate tool to mask/grade.
- **`agent_poll` / `bash_output` → `poll`.** Same substitution; `poll` is runtime-owned (like
  `ask_user`) and rides every profile's shared tool set rather than being individually masked in the
  common case, but a profile that explicitly denylisted `bash_output`/`agent_poll` should denylist
  `poll` if the intent was "this agent must not join background work."
- **`grants.yml`** (`${config_dir}/entanglement/grants.yml`) may carry dead `bash_output(...)` /
  `agent_poll` / `agent_spawn` lines — harmless (§2b), safe to delete on sight.
- **After the change:** run `skutter inspect agents` (or just start `skutter`) and check stderr /
  the TUI log file (`<data_dir>/entanglement/logs/skutter.log`) for `agent tool mask names an
  unrecognized tool` / `agent permission rule names an unrecognized tool` — the new warning from §2
  names exactly which agent, field, and entry to fix.

## Consequences

- **(+)** The `explore`/`plan` built-ins would have caught themselves: had this shipped alongside
  ADR-0161's own rename, the embedded `plan.md` mask's stale `agent_spawn`/`agent_poll` entries
  (fixed in the same PR that introduced `poll`/`agent`) would have logged a warning instead of
  silently under-advertising the plan agent's tools.
- **(+)** No new load-time failure mode — every existing config that parses today still parses
  after this change; at most it gains stderr/log noise pointing at something to fix.
- **(−)** The vocabulary in `tool_names::KNOWN_TOOL_NAMES` is a second place (besides each tool's
  own `Tool::name()`) that must be updated when a tool is added — accepted because it is a
  one-line, compile-checked addition (a missed one only produces a false-positive warning on a
  brand-new tool name, never a false negative on a removed one) and the alternative (deriving it
  from the live registry) reintroduces the `bash`/`rhai`/MCP false-positive problem §1 rejected.
- **(−)** `grants.yml` stays unvalidated (§2b) — an operator who wants a clean file has to prune it
  by hand; no tooling was added to do it automatically, since the cost of a stale line is zero and
  automating removal of a file the user directly edits felt like overreach for what it buys.

## Alternatives considered

- **Derive the "known" set from the live `ToolRegistry` at the point profiles and the registry are
  both built.** Rejected (§1): `bash`/`rhai` are conditionally registered and MCP tools connect
  after profiles load, so this would false-positive on `plan.md`'s own `bash` entry under an
  ordinary bash-disabled run — worse than the problem it solves.
- **Make an unrecognized entry a load error (`bail!`), matching the strict-layer malformed-frontmatter
  behavior.** Rejected: "unrecognized" is a heuristic, not a certainty (the vocabulary itself can
  lag a brand-new tool), and bricking startup over one stale mask line is a disproportionate
  failure mode for what is, at worst, a silently-narrower tool set.
- **Validate `grants.yml` too.** Rejected (§2b): a stale grant key is inert by construction
  (exact-match against live calls), so there is no "degrades quietly" failure to catch — only file
  hygiene, which the migration note covers without new code.
- **Alias the removed names instead of just warning.** Rejected by ADR-0161 itself ("No aliases");
  reopening that here would undercut the same "clean rename costs nothing for session logs" argument
  ADR-0033 made, and would leave `bash_output`/`agent_poll`/`agent_spawn` live in the tool-name
  vocabulary indefinitely instead of surfacing the drift once and letting the operator fix it.

## References

- Issue #623: chore — migration for removed tool names in masks/rules/grants (part of #604)
- [ADR-0161](0161-unified-async-work-background-flag-and-one-poll.md): the rename this migrates,
  "Config churn" consequence
- [ADR-0033](0033-agent-tool-family-and-blocking-agent.md): "tool names are opaque, renames are
  free" — the claim this ADR scopes to session logs only
- [ADR-0074](0074-cross-vendor-skill-and-agent-discovery.md): the existing warn-and-continue
  posture for a foreign-layer definition this borrows for a different reason
- [ADR-0148](0148-glob-patterns-in-the-agent-tool-mask.md),
  [ADR-0117](0117-mcp-tool-capability-fan-out.md): why globs and MCP tool names are excluded from
  the check
- `entanglement-runtime/src/tool_names.rs`: `KNOWN_TOOL_NAMES`/`is_recognized_mask_entry`
- `entanglement-runtime/src/agents/mod.rs`: `warn_unrecognized_mask_entries`
- `entanglement-runtime/src/logging.rs`: default `warn`-level filter that makes this visible
  without `--verbose`/`RUST_LOG`
