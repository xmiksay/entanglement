# entanglement Architecture — Agent profiles, permissions, skills & system prompt

> Part of the [architecture overview](../architecture.md). The *why* behind each choice is in the [decision log](../adr/README.md).

## 3. Agent profiles + permissions (opencode-style) — [ADR-0003](../adr/0003-agent-and-permission-profiles.md)

A session runs under exactly one [`AgentProfile`][profile]:
`{ name, description, mode, system_prompt, model?, permission }`. `mode` is
`primary | subagent | all`; `description` drives delegation matching (§8, the
only field a spawning model sees).

**At a glance (epic [#111](https://github.com/xmiksay/entanglement/issues/111), synthesized in [ADR-0044](../adr/0044-agents-skills-system-prompt-epic-synthesis.md)).**
Agents and skills are **data, not code** — discovered from files, disclosed
progressively, and assembled into system prompts deterministically. The pieces
below realize one model:

- **Data, not code** — agents (`*.md` frontmatter+body), skills (`SKILL.md` dirs),
  and the provider catalog share one loader: embedded default < user
  (`${config_dir}/entanglement/…`) < project (`<root>/.entanglement/…`), later
  wins on `name`; a malformed override is a loud error. Editing a built-in is
  dropping a same-`name` file in a higher layer. This precedence is uniform (the
  user config/settings file follows it too) and the project layer is **trusted** —
  running inside a repo means the repo is trusted, with inspection (`skutter
  inspect`) as the mitigation rather than an enforced boundary
  ([ADR-0047](../adr/0047-local-trust-boundary.md)).
- **Progressive disclosure, recursively** — the model sees only *descriptions*
  until it acts: spawn-target `name: description` in the `agent`/`agent_spawn`
  schema (agents) → tier-1 `name: description` index in the prompt (skills) →
  full body on `load_skill` **or** preload (skills tier-2) → the definition body
  *becomes* a child's own assembled prompt at spawn.
- **Model decides *whether*, harness decides *how*** — selection is LLM reasoning
  over `description` text (no keyword/embedding router); path resolution, prompt
  assembly, authorization, and tool-list enforcement are deterministic runtime
  code. Injected content is always a `tool_result` / prompt section, never a
  spoofed `user` message.
- **Physical over prompted** — a read-only agent has no write tool *advertised or
  executable* (the #116 mask), not a persona told not to write.
- **Enforcement-locus split** — a gate lives where it can see the call: the tool
  mask, spawn control, and permission clamp are **runtime** (host tools /
  spawns round-trip there); `owns_plan` is **core** (the `update_plan` built-in
  never round-trips). See ADR-0044 for the full principle→enforcement map and the
  deferred follow-ups (skill provenance, skill-index masking, child-root
  isolation).

- Switch with `InMsg::SetAgent { agent }`; engine emits `AgentChanged`.
- [`PermissionProfile`][perm] resolves `Allow | Ask | Deny` per tool
  (last-matching-rule-wins, `*` wildcard), **in the runtime tool executor** (✅ #59):
  - `Allow` → run the tool, reply `ToolResult` → core emits `ToolOutput`.
  - `Ask` → emit `ToolRequest`, park at `WaitingApproval` until `Approve`/`Reject`;
    on approve, run the tool and reply `ToolResult`; on reject, reply
    `ToolResult("…rejected…")`.
  - `Deny` → reply `ToolResult("…denied…")` without running the tool.
- **File-defined (✅ #112, [ADR-0034](../adr/0034-file-based-agent-definitions.md)):**
  profiles are markdown files with YAML frontmatter (the config bundle) + a body
  (the system prompt), discovered at startup by the **runtime**
  (`entanglement_runtime::agents::load_registry`) into a `ProfileRegistry`. Three
  layers, later wins on a `name` collision: embedded built-ins (`build`/`plan`/
  `explore`, shipped as `include_str!` `.md` and parsed through the *same* loader)
  < user (`${config_dir}/entanglement/agents/*.md`) < project
  (`<root>/.entanglement/agents/*.md`). Editing a built-in = dropping a same-`name`
  file in a higher layer — one mechanism for all three, same defaults+override
  shape as the provider catalog (#118). A malformed file is a loud error. The
  frontmatter also declares `tools`/`disallowed_tools` (the tool mask, **enforced**
  ✅ #116, below) and `can_spawn`/`spawnable_agents` (fine-grained spawn control,
  **enforced** ✅ #119, below). The spawn boundary is now both spawner- and
  target-side: a profile must `may_spawn` and its *target* must be spawnable-mode
  (`subagent`/`all`) and on its `spawnable_agents` allowlist — so `build`/`plan`
  (primaries) are unreachable spawn targets from mode defaults alone. `update_plan`
  ownership (`owns_plan`, ✅ #140, below) and the plan-accept handoff (#141)
  complete the agent hierarchy. Embedders using core directly still get a
  hardcoded
  `build`/`plan`/`explore` fallback via `ProfileRegistry::new()`; add your own with
  `ProfileRegistry::insert`.
- **Physical tool restriction (✅ #116, [ADR-0038](../adr/0038-physical-per-agent-tool-restriction.md)):**
  an agent's `tools` allowlist / `disallowed_tools` denylist masks its tool set —
  `registry ∩ allowlist − denylist` — on *both* sides of the core↔runtime seam,
  orthogonal to `permission` (which grades `Allow`/`Ask`/`Deny` among the tools
  that survive the mask). The mask rides the core `AgentProfile`
  (`tools`/`disallowed_tools` + `advertises_tool`), so it travels per session with
  no new protocol surface. **(a) Advertisement:** core's `run_turn` filters
  `EngineConfig.tool_specs` by the active profile's mask before appending the
  `update_plan`/`update_tasks` built-ins (session-state tools, never routed
  through the tool mask) — a masked tool's schema never reaches the model.
  `update_plan` is instead authority-gated (`owns_plan`, ✅ #140, below), while
  `update_tasks` is always advertised. **(b) Enforcement:**
  `runtime::permission::tool_masked` refuses a masked `ToolExec` **first** — before
  the `agent_spawn`/`agent`/`agent_poll`/`ask_user` interceptions and permission —
  so a hallucinated masked call is a hard boundary, and the mask **intersects down
  the ancestor chain** (a child never gains a tool an ancestor lacked, mirroring
  ADR-0024's privilege ceiling). `explore` is now the reference read-only agent:
  `tools: [read, glob, grep]` — no `edit`/`write`, no `bash`, no `agent_spawn`.
- **Per-profile spawn control (✅ #119, [ADR-0040](../adr/0040-per-profile-spawn-control.md)):**
  spawning is a per-profile capability declared in the definition — *whether* a
  profile may spawn (`can_spawn`, default: open for primaries/`all`, closed for a
  `subagent` leaf) and *which* profiles it may spawn (`spawnable_agents`, omitted ⇒
  any spawnable target). Both ride the core `AgentProfile` with helpers
  (`may_spawn`, `spawn_target_allowed`, `spawnable_as_subagent`); core = semantics,
  runtime = enforcement. **Structural half:** the `agent_spawn`/`agent`/`agent_poll`
  triple moves out of the shared `tool_specs` into
  `EngineConfig.profile_tool_specs` (a `HashMap<profile, Vec<ToolSpec>>` the runtime
  fills via `subagent::spawn_specs_for`); `run_turn` appends the active profile's
  entry (roster + `agent` enum scoped to who *it* may spawn, empty when it may not),
  so an out-of-list spawn is a schema violation before an executor refusal.
  **Executor half:** `runtime::permission::spawn_refusal(spawner, target, registry)`
  layers four checks before the ADR-0023 budget + ADR-0024 clamp — `!may_spawn`
  (absorbs the old capability gate) → unknown target → target not spawnable-mode
  (a `primary` is never a valid target) → target off the `spawnable_agents` list —
  each a clear `ToolOutput` with no child minted. The allowlist is checked per
  spawning session against *its own* profile (**not transitive**). Supervisor
  hardening: `InMsg::Spawn` with an unknown name now `get()`s + errors instead of
  silently escalating to `build`. The TUI `/agent` picker/Tab-cycle is
  registry-driven, filtered to `mode ∈ {primary, all}`.
- **`update_plan` ownership (✅ #140, [ADR-0041](../adr/0041-update-plan-ownership-default-closed.md)):**
  authoring the session plan is a per-profile authority, `AgentProfile.owns_plan`
  (default **false**). Unlike the #116 mask and #119 spawn control (semantics core,
  enforcement runtime), plan authority is enforced **entirely in core** — the
  built-ins are session-state tools that never round-trip to the runtime, so
  `tool_masked` cannot see them. **Advertisement:** `run_turn` appends the
  `update_plan` spec only when the active profile `owns_plan` (`update_tasks` stays
  unconditional — per-session bookkeeping, no cross-agent authority).
  **Enforcement:** `handle_tool_call` refuses a hallucinated non-owner `update_plan`
  via a refusal `ToolOutput` — no plan mutation, no `OutEvent::Plan`, turn
  continues. `InMsg::SetPlan` stays head/user authority. Built-in `plan` gains
  `owns_plan: true` **plus** a physical read-only mask
  (`tools: [read, glob, grep, agent, agent_spawn, agent_poll, ask_user, load_skill]`):
  it authors the plan and delegates research, and — via the mask's ancestor
  intersection — every child it spawns is clamped to that read-only set too.
  `build`/`explore` are unchanged (default-false = they simply stop advertising
  `update_plan`).
- **Plan acceptance — `propose_plan` (✅ #141, [ADR-0042](../adr/0042-plan-acceptance-via-propose-plan-approval-roundtrip.md)):**
  the plan agent's *finalize* step (`update_plan` stays for working snapshots). A
  runtime-owned tool `propose_plan { plan }`, advertised only to a profile that
  `owns_plan` (via the #119 `profile_tool_specs` seam; `plan.md`'s `tools:`
  allowlist also lists it) — the same default-closed-authority gate as #140.
  Acceptance rides the **existing tool-approval round-trip** (#59): the executor
  (`propose_plan.rs`) intercepts it on `ToolExec` after the #116 mask check (same
  interception family as `ask_user`) and **force-parks it on the `Ask` path
  unconditionally** — a permission profile can never `Allow` it, since user
  approval *is* the tool's semantics. A standard `OutEvent::ToolRequest` reaches
  the head. **Approve** → record the plan (`InMsg::SetPlan`, engine state
  consistent for every head) + reply `ToolOutput("plan accepted by the user")` (the
  plan agent learns the outcome and ends its turn); the head then performs the
  **handoff** (see §5c). **Reject + reason** → the existing fold-back (`tool
  \`propose_plan\` rejected: <reason>`); the model revises and re-proposes in the
  same turn. One-shot heads (`run`/`pipe`) can't park an interactive approval, so
  they auto-reject with a "non-interactive head" reason.
- **System-prompt assembly (✅ #113, [ADR-0035](../adr/0035-deterministic-system-prompt-assembly.md)):**
  the definition body is *not* stored as the raw `system_prompt`. As each profile
  is loaded, `entanglement_runtime::system_prompt::assemble` composes up to five
  ordered, optional parts — **shared preamble** (safety/tool-use/output invariants
  applied to *every* agent) + **agent body** + **project brief** (the standard
  `AGENTS.md` / `.agents/AGENTS.md` / `.claude/CLAUDE.md` / `CLAUDE.md`, first
  found wins — no bespoke file — only when the frontmatter sets
  `include_brief: true`) + **generated env block** (cwd/root, platform, date —
  never model-guessed) + **skill index** (tier-1 `name`+`description` disclosure
  lines from the skill registry) + **preloaded skill bodies** (frontmatter
  `skills: [name, …]`, ✅ #117, below). Inputs come from `PromptContext::load(root)`
  (preamble overridable via `ENTANGLEMENT_PREAMBLE_FILE`; brief via
  `ENTANGLEMENT_BRIEF_FILE`). A **subagent** gets `preamble + body (+ brief)` +
  any preloaded bodies — no env/skill-index, and never the parent's assembled
  prompt (each agent is composed from *its own* body + `include_brief` flag).
  Composition is a pure, unit-tested harness function baked into
  `AgentProfile.system_prompt` at load time, so session start / `SetAgent` / spawn
  all read the finished prompt and core stays a verbatim pass-through into
  `LlmRequest.system`. The skill index is populated from the skill registry
  (✅ #114, below); filtering that skill index by a per-agent tool mask is a
  separate follow-up (the #116 tool mask covers tool *specs*, not the skill index).
- **Skill discovery + registry (✅ #114, [ADR-0036](../adr/0036-skill-discovery-and-registry.md)):**
  tier 1 of progressive disclosure. A **skill** is a directory with a `SKILL.md`
  (YAML frontmatter + markdown body) plus optional supporting files
  (`references/*.md`, `scripts/*`). The **runtime**
  (`entanglement_runtime::skills::load_registry`) discovers them into a
  `SkillRegistry` — three layers, later wins on a `name` collision: embedded stock
  skills (single-file, `include_str!` `SKILL.md`, parsed through the *same* loader)
  < user (`${config_dir}/entanglement/skills/**/SKILL.md`, override
  `ENTANGLEMENT_SKILLS_DIR`) < project (`<root>/.entanglement/skills/**/SKILL.md`).
  Discovery is a recursive walk for `SKILL.md` markers; symlinked duplicates and
  directory cycles are deduped by canonical path; a malformed file is a loud
  error. Frontmatter: `name` + `description` (required), `user_only` (only explicit
  user invocation — withheld from the model's disclosure list), and `allowed_tools`
  (a *skill-scoped* tool mask, enforcement deferred — it needs skill provenance,
  distinct from the #116 agent tool mask). Each `SkillMeta` resolves its
  `root_dir` **once** at discovery. **Disclosure is tier-1 only**: `SkillRegistry::disclosures`
  emits one `name: description` line per non-`user_only` skill into the assembled
  system prompt (~100 tokens/skill); bodies are never preloaded. **Selection stays
  the model's own reasoning** — no keyword router or embedding gate; the model
  matches its task against the `description` in its forward pass, so description
  quality is the contract. Bodies + payload (`references/`/`scripts/`) are tier-2,
  loaded on demand (`load_skill`, ✅ #115, below).
- **Tier-2 skill loading (✅ #115, [ADR-0037](../adr/0037-load-skill-tool-deterministic-resolution.md)):**
  one generic `load_skill { skill_name }` tool (not one-per-skill) resolves a
  skill's body on demand. Unlike the orchestration-only runtime tools
  (`agent_spawn`/`ask_user`/`agent_poll`), it **reads the filesystem**, so it is a
  *real host tool* in the `ToolRegistry` (`entanglement_runtime::skills::load_skill::LoadSkillTool`,
  holding a shared `Arc<SkillRegistry>`) and flows through the *same* per-call
  gates as `read` — the permission profile and the #116 tool mask — with no
  orchestration-tool exemption. A read-only `explore` (mask `[read, glob, grep]`)
  therefore refuses it as unavailable. The handler resolves **deterministically** (never model reasoning):
  look the `SkillMeta` up by name; reject a `user_only` skill (withheld from
  disclosure, only an explicit user command may trigger it); then **substitute
  every relative payload path to an absolute one** before the text reaches the
  model — closing Claude Code's bug class where the *model* resolves
  `references/x.md` against the wrong base (anthropics/claude-code#17741, #11011).
  `SKILL_DIR` and the project root stay two strictly separate coordinate systems: a
  ref that does not resolve under the skill dir (a project-root path) is left
  untouched; no implicit CWD fallback; a `${SKILL_DIR}` placeholder is the
  author's explicit escape hatch. The result is an ordinary `tool_result` carrying
  `skill_id`, the substituted body, and `available_refs` (supporting files listed
  as absolute paths, **not** loaded) — never a spoofed user message, so the
  authorship trail stays honest. Provenance (carrying `skill_id` onto tool calls
  made while a skill is active, to scope its `allowed_tools` mask) is a
  tool-execution-record field for a **separate** follow-up — distinct from the
  #116 *agent* tool mask, which is now live; `skill_id` is surfaced in the result
  today.
- **Skill preload vs access — two independent mechanisms (✅ #117, [ADR-0043](../adr/0043-skill-preload-vs-access-independent-mechanisms.md)):** an agent
  definition controls skills along two orthogonal axes, deliberately *not* merged
  (merging loses expressiveness). **Preload** is `skills: [name, …]` frontmatter:
  the listed skills' full bodies are injected into that agent's assembled system
  prompt at load, through the *same* substitution pipeline as `load_skill`
  (`SkillRegistry::preload_body` → `load_skill::render_skill`) — it is preload
  *only*, never an allowlist, and is mode-independent (a spawned subagent gets the
  body even though its tier-1 index is withheld). Two differences from the
  model-facing `load_skill`: a `user_only` skill *is* preloadable (author config,
  not model self-trigger), and an unknown name is a loud load-time error.
  **Access** is the orthogonal #116 tool mask: an agent that must not load skills
  at runtime simply doesn't advertise `load_skill` (`disallowed_tools: [load_skill]`
  or an allowlist omitting it), refused both from the advertised specs (core's
  `run_turn` filter) and at dispatch (`tool_masked`). The two compose to preserve
  both corners: "preload X but block everything else" (`skills: [x]` + `load_skill`
  masked out) and "preload nothing, request on demand" (no `skills:`, `load_skill`
  available). Default stays permissive — a subagent may discover + load any skill
  via the same LLM gate as a primary unless masked.
- **Where dispatch runs (✅ #59):** the `AgentProfile` *shape* stays a core
  protocol type, but the `Allow|Ask|Deny` decision + the approval wait are a
  **runtime** concern ([ADR-0003](../adr/0003-agent-and-permission-profiles.md) /
  [ADR-0010](../adr/0010-single-head-crate-and-bash-opt-in.md)). Core emits
  `ToolExec` for *every* host tool and parks on `ToolResult` (§8); it never reads
  `PermissionProfile`. The runtime `tool_runner` (§8) tracks each session's active
  profile (folded from `SessionStarted`/`AgentChanged` against a `ProfileRegistry`
  copy it holds), resolves the permission, and — for `Ask` — emits the
  `ToolRequest` prompt and awaits `Approve`/`Reject`/`Stop` off the engine's
  **inbound fan-out** (`Holly::subscribe_inbound()`), so every head stays a thin
  protocol adapter (it just sends the same frames; the runtime, not core, acts on
  them).
