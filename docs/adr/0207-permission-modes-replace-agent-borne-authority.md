# 0207. Permission modes replace agent-borne authority

- Status: Accepted
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
  (**superseded**: it edited a per-agent `tools:` allowlist that no longer
  exists; the `config.yml` `modes:` block is hand-edited, with no in-app
  writer),
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
`Capability` is a runtime type and never enters the protocol. `Permission` and
`PermissionProfile` stay in core, no longer as an agent's authority but as the
wire shape of the `config.yml` `permissions:` ceiling, reinterpreted into the
same longest-match engine a mode uses.

### 3. Tools declare their capability

`Tool` gains `fn capabilities(&self) -> &'static [Capability]`, with
`Capability { Read, Write, Exec, Plan, Control }`. A slice, not a single
value, because `call` and `rhai` genuinely do all three. Config-declared
`endpoint__*` tools are `Exec`; rhai-backed skill tools are multi; an alias
inherits its target's capability, resolved before grading so it cannot
launder. MCP tools are ordinary graded
tools matched by name or pattern; their servers connect in the background at
session start so the roster is known, and a server that is down warns and
declines truthfully (ADR-0201's path).

`Control` — `ask_user`, `poll`, `explore`, `describe`, `update_tasks`,
`load_skill`, `mcp_enable`, `agent`, `agent_send`, `request_mode` — is never
graded. None of these can read, write or execute anything; enabling an MCP
server makes tools *dispatchable*, and every one of them is still graded.

A mode denying a capability class is the guarantee: a tool added later with
`Capability::Write` is refused by `research` with no list to update.

### 4. Rules are grade-keyed lists, longest match wins

Three keys — `allow`, `deny`, `prompt` — plus a `default`. `prompt` is the
config spelling of the internal `Permission::Ask`; the config surface uses the
word a user thinks in, the enum keeps core's name.

```yaml
research:
  default: prompt
  deny:  [write]
  allow: [read, "bash(find *)", "bash(grep *)", "bash(rg *)"]
  max_depth: 2
  max_agents: 4
  sandbox: bwrap
```

**The longest matching rule wins.** Not a tier order, not first or last match:
the most specific rule is simply the longest one, so `write(docs/*)` beats
`write`, and a reader can determine the outcome by inspection without knowing
an evaluation order. Capability-class entries participate on the same footing,
which is why the tuning guard in §5 exists — a long scoped rule would
otherwise out-rank a class `deny`.

`deny` is **absolute**: a flat decline with no prompt, naming the mode and the
way out. `prompt` parks an ordinary approval. The argument-scoped
`tool(pattern)` and workdir-scoped `tool{pattern}` grammars are unchanged
(ADR-0051, ADR-0116).

**`bash` and `call` share one rule set.** A rule written for either applies to
both — they are two spellings of the same capability, and grading them apart
invites a rule that looks restrictive while the other spelling walks around it.
Compound commands grade per segment for both (ADR-0197's `&&`/`||` splitting,
now covering `call`), and **syntax that cannot be parsed falls back to
`default`** rather than being graded on a guess.

`build` ships `default: prompt` with a broad allow list and a destructive deny
list — a change from the old `build` agent's `default: allow`.

### 5. Two config blocks, two jobs

`config.yml` `modes:` **tunes** one mode by adding rules in the same
`allow`/`deny`/`prompt` grammar. There is deliberately **no removal syntax**:
longest-match makes one unnecessary, since a more specific rule simply
out-ranks a shipped one. To stop `bash(wc *)` being pre-allowed, add a longer
`deny` that covers the case you care about — the shipped rule stays visible
and the override reads as an override.

Tuning **may not weaken a capability-class `deny`**, and because longest-match
lets a long scoped rule out-rank a short class name, that guard cannot be a
string comparison. `research: allow: ["write(*)"]` names no class, yet grants
exactly what `deny: [write]` forbids. The guard must therefore **resolve the
tool each rule names to its real `Capability` set** and reject the rule when
any of them is class-denied in that mode — which means tuning is validated
where a `ToolRegistry` is in hand, not in isolation. "Research cannot write"
is only true on every machine if this holds.

`config.yml` `permissions:` remains the absolute ceiling clamping every mode,
and adopts the same grammar.

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
the cached prefix. Core does not author that text (it does not own the mode
table); the runtime supplies it as `EngineConfig::modes_preamble` and core
folds it in once.

The **current** mode rides as the last message of **every request**, rebuilt
from the session's mode at request time — not at session start and on switch
only, which needs special-casing, but uniformly every round, which does not.

It is **never pushed into `Context`**. A persisted push desyncs live from
replayed history: the log's pairing step queues only `Prompt`/`Stop` for the
next persisted `Out` record, so a session's first prompt always folds into the
context before any later record — a start-time notice would land *before* the
prompt live and *after* it on replay. Deriving the notice at request time
removes the hazard entirely, leaving replay only the session's mode to
reconstruct, which is a plain overwrite fold.

Because the notice is last and nothing before it moves, the cached prefix is
untouched and **`SetMode` invalidates nothing**. Combined with a constant spawn
roster and an always-advertised `propose_plan`, the advertised tools array
stops varying by agent or by mode as well.

**The system message is session-invariant: no agent, no skill, no tool text.**
It carries only the preamble, the project brief, and the mode vocabulary —
none of which change while a session runs. The agent's own body and its
preloaded `skills:` move out of it and ride an appended message, refreshed on
`SetAgent` exactly as the mode notice is refreshed on `SetMode`.

WHY, and it is the rule the whole design answers to: **never invalidate the
cache unless the change actually requires it.** Switching persona does not
require re-reading the conversation. It only did because the persona sat in
the system block, and the render order is tools → system → messages — so
rewriting system invalidated system *and every message after it*, leaving only
the tools block cached. Appending instead costs one block and keeps the entire
prefix. A dynamically loaded skill already works this way (`load_skill`
returns its body as a tool result), which is the precedent: with this change
every axis a session can switch — agent, mode, skill — is append-only, and
nothing a user does mid-session invalidates a prefix.

**`InMsg::SetAgent` is deleted.** An agent is chosen when a session starts —
`--agent`, `config.yml`'s `agent:`, or the `agent` tool's own argument for a
spawned child — and is fixed for that session's life.

Switching persona mid-session never made sense once authority left the agent:
mode is state consulted at dispatch, but a persona is *text already sent*, so
"switching" would append a second persona while the first still sits in the
transcript. And it is unnecessary — **a spawned sub-agent is a new session with
its own system prompt**, so delegating to a different persona costs no
invalidation at all, because there is no prefix yet to invalidate. `SetAgent`
was solving a problem `agent()` already solves for free.

With it gone the system message is session-invariant **by construction**: the
agent body and preloaded `skills:` can stay exactly where they are, because
nothing can change them while the session runs. `SetAgent` was the only thing
that made that block unsafe.

`SetModel` and `SetGeneration` stay (ADR-0063): a model switch invalidates by
necessity — it is a different model with a different cache — and generation
params never touch the prefix.

### 10. `request_mode`

A `Control` tool that force-parks like `propose_plan`. It may only widen —
`research`→`plan`, `research`→`build`, `plan`→`build` — never narrow (that is
the user's action) and never into `auto`. It is refused outright in `auto`: an
unattended run cannot escalate its own authority.

### 11. Auto is an unattended posture

`default: deny`, and that default is load-bearing rather than decorative:
exec is an **explicit command list, never the `exec` capability class**. A
class allow would mean `default: deny` never fires and the destructive deny
list became the only guard — leaving anything harmful nobody thought to
enumerate permitted, in the one mode where no human is present to catch it.
A `prompt` grade likewise collapses to a denial here, since there is no one
to ask. A repeated *identical* denied call parks an approval on its second
occurrence.

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

They also gain **`pending`**: everything in flight for the session's spawn
sub-tree — running sub-agents, background jobs and scripts, retained outputs,
open questions, parked approvals. Today nothing enumerates these. `poll` is
the only model-facing path and it requires a handle already held, which makes
a handle something the model must hoard in context to avoid losing.

That is a real failure, not a convenience gap. Every compaction forks a
successor seeded with a summary plus the kept tail (ADR-0205), and a handle —
`x-…` for a job, an `agent_id` for a sub-agent — is just text in the
transcript. If it falls outside the kept tail it is unreachable: the work
keeps running, finishes, and its result is silently orphaned. The same applies
across a hibernate/resume cycle. Listing makes handles **recoverable** instead
of hoarded.

Like the rest of the discovery pair it is `Capability::Control` — read-only
session introspection that starts nothing and touches no host resource — so it
is never graded and is available in every mode, `auto` included.

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
