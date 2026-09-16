# 0207. Permission modes replace agent-borne authority

- Status: Proposed
- Date: 2026-09-16
- Issue: #560 (pre-release audit umbrella)
- Relates to: [ADR-0003](0003-agent-and-permission-profiles.md)
  (**superseded**: the agent *is* the permission profile),
  [ADR-0038](0038-physical-per-agent-tool-restriction.md) (**superseded**: the
  `tools`/`disallowed_tools` mask), [ADR-0040](0040-per-profile-spawn-control.md)
  (**superseded**: `can_spawn`/`spawnable_agents`),
  [ADR-0041](0041-update-plan-ownership-default-closed.md) /
  [ADR-0049](0049-plan-task-tools-as-runtime-state-tools.md) (**amended**: plan
  authorship is a capability, not mask membership),
  [ADR-0114](0114-capability-level-permission-keys.md) (**superseded**:
  capabilities are declared by tools, not inferred from a name table),
  [ADR-0134](0134-per-profile-sandbox-scoping-and-spawn-chain-clamp.md)
  (**superseded**: sandbox is a mode fact),
  [ADR-0138](0138-sponsored-build-child-and-propose-plan-cycle.md)
  (**superseded**: no sponsored child, no permission root),
  [ADR-0148](0148-glob-patterns-in-the-agent-tool-mask.md) (**superseded** with
  the mask it globs), [ADR-0149](0149-per-session-tool-overlay.md)
  (**amended**: the overlay is mode-scoped),
  [ADR-0192](0192-universal-advertisement-enforcement-at-dispatch.md) /
  [ADR-0198](0198-out-of-mask-tool-calls-are-approvable.md) (**superseded**:
  there is no mask left to miss),
  [ADR-0023](0023-subagent-spawn-limits.md) /
  [ADR-0024](0024-subagent-permission-gating.md) (**amended**: depth and
  fan-out are mode facts; the ancestor clamp loses its only hole),
  [ADR-0083](0083-in-app-tool-allowlist-editing-as-user-layer-materialization.md)
  (**amended**: in-app editing targets the config `modes:` block),
  [ADR-0145](0145-one-plan-tool-file-backed-plans-and-blocking-review-loop.md)
  (**amended**: approval switches mode instead of spawning a build child),
  [ADR-0147](0147-multi-user-mode-embedder-api.md) /
  [ADR-0184](0184-provider-hosted-multi-user-seams.md) (an embedder supplies
  its own mode table), [ADR-0196](0196-tool-search-and-lazy-discovery-replace-the-invoke-envelope.md)
  (**amended**: `explore`/`describe` gain non-tool kinds),
  [ADR-0202](0202-prompt-cache-discipline-anchors-deferral-replay-compaction-date.md)
  (the cache-prefix discipline this preserves).

## Context

A session's authority came from its agent. `AgentProfile` carried, in one
struct, both *who the session is* — name, description, system prompt, model
and provider pins — and *what it may do* — `tools`, `disallowed_tools`,
`permission`, `sandbox`, `can_spawn`, `spawnable_agents`. `InMsg::SetAgent`
swapped all twelve fields in a single assignment.

Nothing could change posture without changing persona, or persona without
changing posture. That coupling produced a specific, enumerable set of
defects:

1. **Read-only was not expressible as data.** No tool declared what it does.
   The only read/write notion was a closed compile-time table of five
   built-ins (ADR-0114), so "read-only" was spelled as a ~20-line curated
   allowlist of `bash` sub-commands duplicated verbatim in two agent files,
   with nothing verifying the copies agreed.
2. **Importing a foreign agent silently granted full write.** A Claude Code
   definition parsed through the lenient shape kept only `name` and
   `description`; its `tools:` restriction was dropped and the absent
   `permission:` defaulted to `Allow` (ADR-0074).
3. **Three matchers on one field.** The mask globbed; permission rule keys
   never globbed their tool part; plan authorship required literal exact
   membership.
4. **Mask and grade had converged.** ADR-0198 made a mask miss park an
   approval, so a masked tool already behaved as `Ask` — a redundant second
   grade table with a different matcher and different refusal text.
5. **Plan mode was not a mode.** No type existed. It was one markdown file,
   one hardcoded interception, and a hardcoded handoff profile name, with the
   sponsored build child (ADR-0138) as the single largest privilege jump in
   the system and the only hole in ADR-0024's ancestor clamp.
6. **`Always` grants leaked across agents.** The persisted grant set was
   global — no session key, no agent key — so a grant earned under a
   permissive agent silently upgraded `Ask` to `Allow` under a restrictive
   one.
7. **The overlay was a de-facto mode of the wrong shape.** It survived
   `SetAgent` by design, *replaced* rather than intersected the profile
   grade, and ADR-0198 wrote new enable entries on every session-scope
   approval — so a session accumulated a monotonically widening,
   agent-independent permission set.
8. **Switching agent for permission reasons cost a cache miss.** Per-profile
   spawn and `propose_plan` specs varied the advertised array by agent, and
   after ADR-0202 that array is the cache prefix.
9. **The struct's halves diverged.** Core's profile registry is frozen for
   the process while live reload swaps only the runtime mirror, so after an
   edit `SetAgent` bound the stale prompt while dispatch used the fresh
   rules.

## Decision

**Authority leaves the agent entirely and becomes a second, independent axis:
the session *mode*.** A session is a pair — `(agent, mode)` — where the agent
supplies identity and the mode supplies every permission fact.

### 1. `AgentProfile` is identity only

It keeps `name`, `description`, `system_prompt`, `model?`, `provider?`,
`include_brief` and `skills?`. Removed: `tools`, `disallowed_tools`,
`permission`, `sandbox`, `can_spawn`, `spawnable_agents`, and `mode`/
`AgentMode` — the primary/subagent/all distinction goes with them, so any
agent may be a session root or a spawn target.

The built-in roster collapses to three personas: `general` (the default and
the default spawn target), `plan`, `debug`. `build`, `explore` and `research`
are retired as names; their postures are modes now. Sessions logged under a
retired name do not resume.

### 2. Four modes, owned by the runtime

`research`, `plan`, `build`, `auto`. The table is **code**, not
configuration: `skutter` compiles in these four and reads no `modes/`
directory, so a user can never define, add or remove one. An embedder
building on the library supplies its own table — permission has always lived
in the runtime, and this is where it stays.

`entanglement-core` carries an opaque `mode: String` on the session, persists
it, replays it and emits `OutEvent::ModeChanged` — and never evaluates it.
`Permission`, `PermissionProfile` and the new `Capability` move out of the
protocol into `entanglement-runtime`.

### 3. Tools declare their capability

`Tool` gains `fn capability(&self) -> Capability`, with
`Capability { Read, Write, Exec, Plan, Control }`. `call` and `rhai` are
multi-capability. Config-declared `endpoint__*` tools are `Exec`;
rhai-backed skill tools are multi; an alias inherits its target's capability,
resolved before grading so it cannot launder. MCP tools are ordinary graded
tools matched by name or pattern; their servers connect in the background at
session start so the roster is known, and a server that is down warns and
declines truthfully (ADR-0201's path).

`Control` — `ask_user`, `poll`, `explore`, `describe`, `update_tasks`,
`load_skill`, `mcp_enable`, `agent`, `agent_send`, `request_mode` — is never
graded. None of these can read, write or execute anything; enabling an MCP
server makes tools *dispatchable*, and every one of them is still graded.

A mode denying a capability class is the guarantee: a tool added later with
`Capability::Write` is refused by `research` with no list to update.

### 4. Rules are grade-keyed lists

```yaml
research:
  default: ask
  deny:  [write]
  allow: [read, bash(find *), bash(grep *), bash(rg *), bash(git log *)]
  max_depth: 2
  max_agents: 4
  sandbox: bwrap
```

`deny` is **absolute** — a flat decline with no prompt, naming the mode and
the way out. `ask` parks an ordinary approval. The argument-scoped
`tool(pattern)` and workdir-scoped `tool{pattern}` grammars are unchanged
(ADR-0051, ADR-0116); only the surrounding shape changes, which removes the
`|`-alternation hack that existed solely because rule keys had to be unique.

`build` ships `default: ask` with a broad allow list and a destructive deny
list — a change from the old `build` agent's `default: allow`.

### 5. Two config blocks, two jobs

`config.yml` `modes:` **tunes** one mode: it may add or remove individual
tool and argument-scoped rules, but may not change `default` and may not
weaken a capability-class `deny`. `research: allow: [bash(cargo check)]` is
accepted; `research: allow: [write]` is a load error. "Research cannot write"
therefore holds on every machine.

`config.yml` `permissions:` remains the absolute ceiling clamping every mode,
and adopts the same grade-keyed grammar.

### 6. Mode applies to the whole spawn sub-tree

Children inherit the session's mode; there is no per-spawn override. Spawning
is never graded — `agent`/`agent_send` are `Control` — and is bounded instead
by two mode facts: `max_depth` (nesting, root = 0) and `max_agents`
(concurrent children per root). Undefined means unlimited; exceeding either
returns an `is_error` naming the limit. The `agent` tool gains a `model`
parameter validated against the catalog.

### 7. Plan authorship is a capability; approval is a mode switch

`propose_plan` is advertised unconditionally and carries `Capability::Plan`,
allowed only in `plan` mode. It still force-parks on `Ask` — approval *is*
its semantics. On approval the session tree switches to `build` and the tool
returns immediately; the same turn continues with the plan in context.

No sponsored child, no permission root, no blocking report fold-back.
ADR-0024's ancestor privilege clamp loses its only exemption.

### 8. Grants and the overlay are mode-scoped

`GrantKey` gains the mode it was earned in and matches exactly — a grant from
`build` does not fire in `research`. Overlay entries likewise record their
mode and are dropped when the mode changes.

The overlay survives as the user's direct voice: an explicitly typed
`/enable tool X --allow` overrides even a mode `deny` for that session. The
*model* cannot reach it — an enable entry is trusted-frame-only (ADR-0177) —
so the guarantee holds against the model while the human at the keyboard
keeps an escape hatch that dies with the next mode change.

The mask machinery is deleted: `tools`/`disallowed_tools`, `tool_masked`,
`MaskSource`/`MaskAuthority`, `mask_request.rs` and ADR-0198's approval
ladder.

### 9. The model is told, without costing the cache

The assembled system prompt carries a short **static** section describing what
modes exist and what each forbids — identical for every session, so it sits in
the cached prefix. The *current* mode arrives as an appended message at
session start and on every switch. Because history only appends, a mode change
invalidates nothing.

Combined with a constant spawn roster and an always-advertised
`propose_plan`, the advertised tools array no longer varies by agent or by
mode: **`SetAgent` and `SetMode` are both free of prompt-cache invalidation**,
which `SetAgent` is not today.

### 10. `request_mode`

A `Control` tool that force-parks like `propose_plan`. It may only widen —
`research`→`plan`, `research`→`build`, `plan`→`build` — never narrow (that is
the user's action) and never into `auto`. It is refused outright in `auto`: an
unattended run cannot escalate its own authority.

### 11. Auto is an unattended posture

`default: deny` with explicit allow and deny lists; an `ask` grade collapses
to a denial, since there is no one to ask. A repeated *identical* denied call
parks an approval on its second occurrence.

`question_timeout` (`0` = infinite, the default for every other mode; `60s`
in `auto`) governs both questions and parked approvals: a question with
options expires to its default option, a free-text question expires as an
`is_error`, and an approval expires as a **denial** — silence is never consent
for a privileged action. `max_turns` and `max_duration` bound the run, which
ends with a stated reason.

`--yes` is retired; it errors with a pointer to `--mode auto`.

### 12. Surfaces

`InMsg::SetMode { session, mode }` (trusted-only, deferred mid-turn like
`SetAgent`) and `OutEvent::ModeChanged { session, mode }`. A `/mode` command
and picker, `--mode` on `run`/`tui`, and `config.yml` `mode:` and `agent:`
defaults. Setting one axis never changes the other.

`explore`/`describe` gain non-tool kinds — `agents`, `skills`, `models`,
`modes` — so the model can list what exists and explain why it is blocked.
`skutter inspect agents` shrinks to identity and provenance; `inspect modes`
shows rules and per-tool outcome. The `/set` dialog's tools tab is left for a
later change.

### 13. Migration

An agent file still carrying `tools:`/`permission:` is rewritten without those
keys, a `.bak` is kept, and the warning names the closest built-in mode based
on what the rules allowed. A `config.yml` with the old rule-key `permissions:`
shape is rewritten to the new grammar the same way.

## Consequences

- A mode name means the same thing on every machine, and "research cannot
  write" is a property of the binary rather than of five markdown files
  agreeing with each other.
- Importing a foreign agent can no longer grant authority, because agent
  definitions carry none.
- One matcher, one rule language, one refusal vocabulary.
- Switching posture is one logged event that costs no cache.
- Cost: every existing agent file loses its rules, old session logs naming a
  retired agent do not resume, and `build` starts prompting where it did not.
- The runtime gains a table it did not have; core loses the permission types
  it never evaluated.

## Alternatives considered

- **Mode clamps the agent** (agents keep rules, mode intersects). Backwards
  compatible, but keeps two authority carriers and every defect that follows
  from the redundancy.
- **Keep name-based classification, centralized.** Fixes the duplication
  without making a newly added tool safe by default — the guarantee stays a
  list someone must remember to update.
- **User-definable modes as layered files.** Rejected: a mode whose meaning
  varies per machine cannot be the thing a guarantee is stated in terms of.
  Tuning within a mode, bounded by the class-deny rule, gives the flexibility
  without the ambiguity.
- **Keeping the sponsored build child.** Rejected: it is an exemption from
  the ancestor clamp that exists only because posture was welded to persona;
  with a mode axis the transition expresses itself directly.
