# 0034. File-based agent definitions — discovery, frontmatter, registry

- Status: Accepted
- Date: 2026-07-10

## Context

Agent profiles ([ADR-0003](0003-agent-and-permission-profiles.md)) shipped as a
hardcoded `build`/`plan`/`explore` trio built by Rust constructors in
`entanglement-core`, with custom profiles only addable in-process via
`ProfileRegistry::insert`. This is the first structural change of the
agents/skills/system-prompt epic (#111): agents must become **user-authorable
files**, editable without recompiling — and a built-in must be editable the same
way a custom agent is created, so there is no privileged "stock" code path.

The provider/model catalog (#118, [ADR-0032](0032-yaml-provider-model-catalog.md))
already solved the shape of this problem: an embedded default deep-merged with an
optional user override, precedence **env > user > embedded**, one mechanism for
built-in and custom. Agent definitions should follow the same pattern.

## Decision

Agents are **markdown files with YAML frontmatter**, discovered at startup by the
**runtime** (`entanglement_runtime::agents`) and folded into a core
`ProfileRegistry`. The frontmatter is the config bundle; the body below the
closing `---` is the system prompt.

```
---
name: explore                 # unique id — agent_spawn { agent } / SetAgent key
description: Read-only …       # delegation matching; the ONLY field disclosed to a spawner
mode: subagent                # primary | subagent | all
model: inherit                # provider model override, or inherit (default)
permission:                   # tool → allow|ask|deny; `default` sets the fallback (ADR-0003 shape)
  default: deny
  read: allow
---
You are a read-only exploration agent. …          ← body = system prompt
```

**Three layers, later wins on a `name` collision:**

1. **built-in** — embedded `include_str!` `.md` files (`build`/`plan`/`explore`),
   parsed through the *same* loader. Editing a built-in is dropping a same-`name`
   file in a higher layer — no special "edit built-ins" path.
2. **user** — `${config_dir}/entanglement/agents/*.md` (override:
   `ENTANGLEMENT_AGENTS_DIR`).
3. **project** — `<root>/.entanglement/agents/*.md`.

A malformed file in any layer is a **loud error** (like the catalog), never a
silent skip; embedded built-ins are guarded by a unit test so their parse is
infallible. Discovery + I/O live in the runtime (filesystem, lean-library safe —
no CLI/TUI deps); `ProfileRegistry`/`AgentProfile` stay core protocol types.

**Two channels, merged only in the harness.** The definition body is the
child's system prompt (static, selected by `name`); the `prompt` argument of
`agent`/`agent_spawn` is the child's first user message (dynamic). A sub-agent
does **not** inherit the parent's system prompt or history — `system_prompt =
agent.body`, not `default_system + body`.

**Disclosure.** `description` is the only field a spawning model sees: the
`agent`/`agent_spawn` tool descriptions list one `name: description` line per
registered agent, and the `agent` argument's schema constrains the name to an
`enum` of the registered set. No roster prose in the system prompt.

**Protocol change.** `AgentProfile` grows `description: String`; `AgentMode`
grows `All` (usable as both primary and spawnable, spawns like `Primary`). Both
are additive and `#[serde(default)]`-safe.

**Declared-but-deferred frontmatter.** `tools`/`disallowed_tools` (tool mask) and
`can_spawn`/`spawnable_agents` (spawn control) are parsed and validated by the
loader — so a definition file is a stable contract today — but their
**enforcement** is tracked in follow-up sub-issues of #111. They need per-session
tool specs (#116/#119) to bite; until then they do not reach `AgentProfile`.

## Consequences

- **(+)** Agents are authorable/editable as files; built-in edits use the exact
  create-a-custom-agent mechanism (drop a same-`name` file), no privileged path.
- **(+)** Same defaults+override mental model as the provider catalog.
- **(+)** `description` becomes load-bearing (delegation matching + the disclosed
  enum), so the loaded roster actually drives what a parent can spawn.
- **(+)** Discovery stays in the runtime; core keeps a zero-I/O `ProfileRegistry`
  and a hardcoded fallback trio for embedders that don't load files.
- **(−)** Two sources of built-in text: core's Rust fallback trio and the
  runtime's embedded `.md`. Kept in sync by hand; the runtime's file layer wins
  whenever `skutter` runs, so the Rust trio is only a bare-core safety net.
- **(−)** A frontmatter surface to document and validate; `deny_unknown_fields`
  keeps typos loud but means adding a field is a deliberate schema change.

## Alternatives considered

- **Keep Rust constructors, add a `from_file` alongside.** Rejected: two code
  paths (stock vs. custom) is exactly what the epic wants to avoid.
- **A single `agents.yml` list** (like the catalog's one file). Rejected: the
  agent body is free-form markdown prose (the system prompt), which is far nicer
  one-file-per-agent than embedded in a YAML scalar; per-file also matches how
  users think about "an agent."
- **Put discovery in core.** Rejected: filesystem I/O + XDG dirs belong in the
  runtime ([ADR-0006](0006-core-dependency-hygiene-gate.md)); core stays pure and
  only holds the `ProfileRegistry` shape.
- **Add tool-mask / spawn-control enforcement now.** Rejected: enforcement needs
  per-session tool specs (#116/#119). Parsing the fields now keeps the file
  contract stable without shipping half-wired behavior.

[0003]: 0003-agent-and-permission-profiles.md
[0006]: 0006-core-dependency-hygiene-gate.md
[0032]: 0032-yaml-provider-model-catalog.md
