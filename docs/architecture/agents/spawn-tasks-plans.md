# Agents & permissions §3 — Spawn bounds, task state & the plan tool

> Part of the [agents & permissions module](../agents-and-permissions.md) of the [architecture overview](../../architecture.md).

- **Spawn bounds come from the mode, not the agent** (✅ #119
  [ADR-0040](../../adr/0040-per-profile-spawn-control.md), superseded by
  [ADR-0207](../../adr/0207-permission-modes-replace-agent-borne-authority.md)
  §6): the old per-profile `can_spawn`/`spawnable_agents` quartet is gone —
  **any agent may spawn any registered agent now**. `agent`/`agent_send` are
  `Capability::Control` but graded like any tool ([ADR-0232](../../adr/0232-spawning-is-graded-bookkeeping-control-tools-non-maskable.md), amending
  ADR-0207 §6): the overlay and the mode's rules reach them, and the
  built-ins class-allow `control`. Their permission argument is the target
  ([ADR-0235](../../adr/0235-scoped-spawn-rules-grade-the-target.md)) —
  the profile a spawn runs (`deny: ["agent(debug)"]`; an omitted `agent`
  grades as `general`) and `agent_send`'s trimmed `agent_id` handle (a
  minted id, so in practice `agent_send(<tenant>:*)` or a bare rule) — so a
  scoped rule grades only its target, a grant is earned per target (an
  `agent_send` card offers only `Once`/`Session` — a minted handle can't
  carry a persisted grant), the `agent` card heads with the graded profile,
  and an unattended repeat (ADR-0207 §11) means the same target
  again, not any second spawn. Past that grade, spawning is bounded by
  two mode facts, applying to the session's whole
  spawn sub-tree: `max_depth` (nesting, root = 0) and `max_agents`
  (concurrent children per root) — undefined means unlimited, exceeding
  either returns an `is_error` naming the limit
  (`entanglement-runtime/src/subagent/spawn_guard.rs`). A child counts from
  its spawn until its answer arrives (a `SpawnSlot` held by its launch task),
  so a session delegating one task at a time never runs out; an idle child
  re-woken by `agent_send` is not counted again. **The spawn tree survives
  compaction** ([ADR-0234](../../adr/0234-live-sub-agents-survive-compaction.md)):
  a compacting session's live children move under its successor
  (`OutEvent::ParentChanged`) instead of dying in its close cascade, and a
  compacting sub-agent's successor starts under the same parent; the parent's
  handle keeps resolving, `agent_send` reaches the live session, and a root
  successor shares its predecessor's `max_agents` count. `runtime::permission::spawn_refusal(target,
  registry)` checks only whether `target` is a known agent name — the old
  `spawner`-side `may_spawn`/allowlist checks are gone with the fields they
  read. The `agent` tool also gains a `model` parameter, validated against
  the catalog, so a spawn can pin its child's model without a profile-level
  pin. Supervisor hardening is unchanged: `InMsg::Spawn` with an unknown name
  `get()`s + errors instead of silently escalating to a default. The TUI
  `/agent` picker (`Ctrl+A`) is registry-driven but now **read-only**
  (ADR-0207 §9): it lists every agent for inspection, but resolving a
  selection has nothing to send — there is no live `SetAgent` any more, and
  the old Tab-cycle-through-agents ring is retired along with it (bare `Tab`
  now only accepts a mention/slash suggestion or opens the command palette).
  Choosing a starting agent lives at session start (`--agent` /
  `config.yml` `agent:`) or in a spawn's own `agent` argument instead.
- **Task state tool (✅ #231, [ADR-0049](../../adr/0049-plan-task-tools-as-runtime-state-tools.md)):**
  `update_tasks` is a **runtime** state tool, not a core built-in. It replaces
  the session's *display* task outline; the runtime executor emits the
  `OutEvent::TaskList` snapshot (a fresh per-session seq, #157) and acks the
  model — the engine holds no task state. It rides `Capability::Control`
  ([ADR-0207](../../adr/0207-permission-modes-replace-agent-borne-authority.md)
  §3), so it is now **never graded**: it runs (and acks) unconditionally in
  every mode, including `research`/`auto`, since it touches only session
  bookkeeping, not the host or a file — a real posture change from before
  ADR-0207, when a read-only `explore` had it masked out *and*
  permission-denied (#175). It rides the shared `tool_specs`, advertised to
  every session identically.
- **One plan tool — `propose_plan` (✅ #141/#513, [ADR-0042](../../adr/0042-plan-acceptance-via-propose-plan-approval-roundtrip.md)/[ADR-0145](../../adr/0145-one-plan-tool-file-backed-plans-and-blocking-review-loop.md),
  amended by [ADR-0207](../../adr/0207-permission-modes-replace-agent-borne-authority.md)
  §7, which **retires** [ADR-0138](../../adr/0138-sponsored-build-child-and-propose-plan-cycle.md)
  wholesale — no sponsored child, no permission root, no blocking
  report fold-back):**
  `update_plan` is **gone** — `propose_plan(content: Option<String>, path:
  Option<String>)` is the sole plan-authorship tool, now gated by
  `Capability::Plan`: advertised **unconditionally** to every session, but
  usable only where a mode grades `plan` (or the literal `propose_plan` tool)
  `Allow`/`Ask` — `plan` mode alone in the built-in table (§the four built-in
  modes in [definitions](definitions.md)). A mode's `Deny` on the `plan` class declines the call flat,
  naming the mode and the way out (`use request_mode`/`/mode`), **before**
  any file is materialized or an approval is ever parked — a real
  short-circuit ahead of the rest of this tool's logic, unlike the old
  allowlist-membership check it replaces
  (`plan_tasks::explicitly_allowlists`, now retired with the mask).
  **Exactly one** of `content`/`path`; both/neither, a non-`.md`/missing
  `path`, or a stale `path` (see below) replies **immediately with no
  approval prompt** — a self-correctable model error, not a decision for the
  human. `content` **materializes** (or overwrites) a file at
  `.entanglement/plans/<short-session-id>.md`; `path` **binds** an existing
  in-root `.md` file. Either way the resolved content rides an
  `OutEvent::Plan { content, path }` snapshot for the plan session itself
  (before the approval prompt — plans are files now, so this always resolves
  a real location) and the `ToolRequest.input` JSON `{content, path}`, so a
  `path`-mode approval still shows the full text.
  A **staleness guard** (`path` mode only, `entanglement-runtime/src/plan_files.rs`)
  refuses a resubmit of a file the *user* edited out of band since the
  session last touched it: tracked as a session-scoped content hash, kept
  fresh both by `propose_plan`'s own reads/writes and passively by a
  background listener on the executor's `OutEvent::FileChange` audit (#202,
  ADR-0060) — so the agent's own `write`/`edit` between build phases (the
  intended review loop) never trips it, while an edit the runtime never saw
  execute does. `content` mode is exempt (an explicit full overwrite is
  "last writer wins" by construction); a first touch of a `path` (no prior
  binding, e.g. a user-seeded file, #514) is never stale. The guard itself
  only fires at the *next* `propose_plan(path=...)` call; a dedicated
  debounced plans-folder watch surfaces the same detection live as
  `OutEvent::PlanChanged` (#627, ADR-0173) — see [engine](../engine/subagents-and-plans.md) for the
  watcher's own detail.
  Acceptance still rides the **existing tool-approval round-trip** (#59): the
  executor (`propose_plan.rs`) intercepts it on `ToolExec` after the mode
  grade check above (same interception family as `ask_user`) and
  **force-parks it on the `Ask` path unconditionally, every phase** — a mode
  can never `Allow` past it, since user approval *is* the tool's semantics
  (even `plan` mode's own `Allow` on the `plan` class still force-parks; the
  grade only decides whether the call is reachable at all). **Approve** —
  the whole ADR-0138 mechanism this replaces is gone: no `SpawnGuard`
  mutation, no sponsored `build` child, no `WaitingAgent` block, no folded-back
  final report. Instead the executor sends `InMsg::SetMode { session, mode }`
  on the **same** session and replies immediately — core cascades that
  `SetMode` over the session's whole live spawn sub-tree (§6: mode applies to
  the whole spawn sub-tree, no per-spawn override), so a plan session with
  running children switches them too, in place, with the same turn
  continuing.

  **Which mode is the *approver's* choice, not a hardcoded `build` (#560):**
  `InMsg::Approve` carries an optional `mode` (`"build"` or `"auto"`) — the
  approval prompt offers both, defaulting a bare accept (`mode: None`) to
  `auto` ("go implement this" is usually what an accept means) with an
  explicit opt-in to `build` available too, and a reject that carries the
  approver's reason back to the model (TUI `[u]`/`[b]`/`[n]`/`[e]`). Since
  [ADR-0228](../../adr/0228-rhai-path-and-args-skill-tools-frontmatter-removed.md)
  `auto` runs everything except its deny list and its prompt list of
  destructive deletions, bounded by its `max_turns`/`max_duration` budgets
  and `question_timeout` — an approver who wants each unlisted call seen
  picks `build`.
  `propose_plan` itself gains an optional `mode` argument (`"build"`/`"auto"`
  only) letting the model *suggest* which option the prompt pre-selects — a
  suggestion never decides, only the approver's own keystroke does. This is
  the deliberate asymmetry with `request_mode` ([advertisement & enforcement](advertisement-and-enforcement.md)), which refuses `auto` as
  a target outright: there the model asks with no human in the loop, so
  granting itself the unsupervised posture is never safe; here a human is
  answering the prompt, so a model's `auto` suggestion is just a hint the
  human can ignore. The reply names the mode actually landed on: `"plan file:
  <path>\\n\\nplan approved — this session's mode switched to \`<mode>\`.
  Continue the same turn, implementing the plan directly."` A multi-phase
  plan → build → review loop is now just: work in `build` mode, `/mode plan`
  (or `request_mode`) back when the plan needs revising, edit the file,
  `propose_plan` again. **Reject + reason** → the existing fold-back (`tool
  \`propose_plan\` rejected (plan file: <path>): <reason>`); the model
  revises and re-proposes in the same turn. One-shot heads (`run`/`pipe`)
  can't park an interactive approval, so they auto-reject with a
  "non-interactive head" reason — unconditionally, `--mode auto` included, so
  an unattended run never starts auto-accepting its own plans.
  Built-in `plan` mode (§the four built-in modes in [definitions](definitions.md)) is physically
  read-only apart from one carve-out (#524,
  [ADR-0142](../../adr/0142-trusted-scratch-dir-and-plans-folder-carve-outs.md)):
  `deny: [write]` plus the argument-scoped `write(.entanglement/plans/*.md):
  allow` — out-ranking the class deny by longest match — grades every write
  outside `.entanglement/plans/*.md` as `Deny`, the opencode-style
  plans-folder exception `propose_plan`'s `content` mode writes into,
  everything else stays physically unreachable. Exec in `plan` mode gets
  the same shared read-only exec set as `research`/`auto` (spliced in from
  `readonly_exec.yml`, [definitions](definitions.md)); any other command falls through to `default:
  prompt`, matching the old `plan` agent's `call(*): ask`. Because mode now
  applies uniformly to the *whole* spawn sub-tree (§6) there is no more
  ancestor-clamp-intersection concern to work around: the old `plan.md` mask
  had to carry `call`/`bash` explicitly just so a spawned `explore` child's
  own shell access survived the intersection — that entire class of
  workaround (mask fields carried by a parent purely to avoid clamping a
  child) no longer exists, since a child inherits the exact same mode, not
  an intersection of two masks.

---

Previous: [§3 — Advertisement, enforcement, overlay & discovery](advertisement-and-enforcement.md) · Next: [§3 — System prompt, skills, slash commands & dispatch](prompt-and-skills.md)
