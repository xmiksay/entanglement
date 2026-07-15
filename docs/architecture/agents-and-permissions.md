# entanglement Architecture — Agent profiles, permissions, skills & system prompt

> Part of the [architecture overview](../architecture.md). The *why* behind each choice is in the [decision log](../adr/README.md).

## 3. Agent profiles + permissions (opencode-style) — [ADR-0003](../adr/0003-agent-and-permission-profiles.md)

A session runs under exactly one [`AgentProfile`][profile]:
`{ name, description, mode, system_prompt, model?, provider?, permission, tools?,
disallowed_tools, can_spawn?, spawnable_agents? }`. `mode` is
`primary | subagent | all`; `description` drives delegation matching (§8, the
only field a spawning model sees). The last four fields are the physical
restrictions layered over `permission`: the `tools`/`disallowed_tools` mask
(#116, [ADR-0038](../adr/0038-physical-per-agent-tool-restriction.md)) governs which tools *exist*
for the profile, and `can_spawn`/`spawnable_agents` gate sub-agent spawning
(#119, [ADR-0040](../adr/0040-per-profile-spawn-control.md)) — both detailed
below. (There is no `owns_plan` field: plan authorship rides the tool mask now,
#231, [ADR-0049](../adr/0049-plan-task-tools-as-runtime-state-tools.md).)

**At a glance (epic [#111](https://github.com/xmiksay/entanglement/issues/111), synthesized in [ADR-0044](../adr/0044-agents-skills-system-prompt-epic-synthesis.md)).**
Agents and skills are **data, not code** — discovered from files, disclosed
progressively, and assembled into system prompts deterministically. The pieces
below realize one model:

- **Data, not code** — agents (`*.md` frontmatter+body), skills (`SKILL.md` dirs),
  and the provider catalog share one loader: embedded default < user
  (`${config_dir}/entanglement/…`) < project (`<root>/.entanglement/…`), later
  wins on `name`; a malformed override is a loud error. The agent and skill
  loaders share a runtime-local `layers` helper (`layers::load_layers`, #204):
  an *explicit* `ENTANGLEMENT_AGENTS_DIR`/`ENTANGLEMENT_SKILLS_DIR` override that
  points at a missing directory is `warn!`ed instead of silently swallowed (the
  default `${config_dir}` path being absent stays the normal "no user layer"
  case). **Cross-vendor dirs are scanned too**
  ([ADR-0074](../adr/0074-cross-vendor-skill-and-agent-discovery.md)): within
  the user layer `~/.claude/<kind>` before the native `${config_dir}` dir, and
  within the project layer `.claude/<kind>` then `.agents/<kind>` before the
  native `.entanglement/<kind>` — native always wins on a `name` collision, and
  foreign dirs parse **leniently** (only `name`+`description` read, unknown keys
  ignored, a malformed file warned and skipped rather than aborting; strict
  `deny_unknown_fields` + abort stays for native dirs). The env override
  replaces the *whole* user layer, doubling as the cross-vendor opt-out.
  Editing a built-in is dropping a same-`name` file in a higher layer. This precedence is uniform (the
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
  mask, spawn control, permission clamp, and (since #231) plan authorship are all
  **runtime** — every tool, including `update_plan`/`update_tasks`, round-trips
  there. See ADR-0044 for the full principle→enforcement map and the deferred
  follow-ups (skill provenance, skill-index masking, child-root isolation).

- Switch with `InMsg::SetAgent { agent }`; engine emits `AgentChanged` — and,
  when the target profile carries a **model pin**, a following `ModelChanged`
  (see *Per-profile model pinning* below).
- [`PermissionProfile`][perm] resolves `Allow | Ask | Deny` per tool call
  (last-matching-rule-wins, `*` wildcard), **in the runtime tool executor** (✅ #59).
  A rule key is a bare tool name, `*`, or an **argument-scoped** `tool(pattern)`
  (✅ #173, [ADR-0051](../adr/0051-argument-scoped-permission-rules.md)): the
  `*`/`?` glob `pattern` matches a tool-specific argument — the
  command for `bash`/`call`, the target path for `edit`/`write`/`read` — so
  `bash(git *): allow`, `bash(rm *): deny`, `edit(src/*): allow` all refine a
  coarse `bash: ask`. The runtime extracts the argument from the call input
  (`runtime::permission::permission_arg`) where the JSON is already in hand;
  argument-less rules and name-only callers (inspect/TUI posture panels) resolve
  exactly as before. The graded decision drives:
  - `Allow` → run the tool, reply `ToolResult` → core emits `ToolOutput`.
  - `Ask` → emit `ToolRequest`, park at `WaitingApproval` until `Approve`/`Reject`;
    on approve, run the tool and reply `ToolResult`; on reject, reply
    `ToolResult("…rejected…")`.
  - `Deny` → reply `ToolResult("…denied…")` without running the tool.
- **Lag-proof decision delivery (✅ #156, [ADR-0070](../adr/0070-authoritative-tool-exec-profile-and-fail-closed-fallback.md)):**
  the `Ask` park (and `ask_user`/`propose_plan`/each `rhai` binding) no longer holds
  its own `broadcast` subscription of the inbound fan-out — that per-task subscriber
  could *lag* under burst and silently drop the `Approve`/`Reject`/`AnswerQuestion`
  it waited for, parking the request forever. Instead each registers a `oneshot` in
  a shared `runtime::pending::PendingDecisions` map keyed by `(session, request_id)`
  *before* emitting its request, and a **single light inbound router** (the
  executor's `Stop`/`user_prompt_submit` watcher, now the sole inbound consumer for
  decisions) fans each decision to its waiter and unwinds a session's waiters on
  `Stop`.
- **Approval scope + persisted grants (✅ #174, [ADR-0052](../adr/0052-approval-scope-and-persisted-grants.md)):**
  `InMsg::Approve` carries a `scope: Once | Session | Always` (core enum, default
  `Once`, `skip_serializing_if` so a bare approve is wire-identical to the pre-#174
  shape — older heads omit it). Approval semantics stay runtime-only: a
  `GrantStore` trait object (#311; the default `DefaultGrantStore` wraps the
  managed-file `runtime::grants::FileGrantStore`, shared with its per-request
  dispatch tasks) records the wider scopes keyed by an exact
  `(tool, argument)` — the same argument #173 resolves against. **After** the full
  resolution (ancestor clamp → config ceiling), a call that lands on `Ask` is
  upgraded to `Allow` when the store already grants it, so the *identical* later
  call skips the prompt. A grant **only raises `Ask` → `Allow`** — it never
  overrides a `Deny` (the ceiling still clamps first), is matched by exact equality
  (no pattern widening), is dropped on `SessionEnded` for `Session` scope, and is
  never inherited by a sub-agent. `Session` lives in memory; `Always` persists to a
  **managed** file `${config_dir}/entanglement/grants.yml` (override
  `ENTANGLEMENT_GRANTS_FILE`) — a top-level `grants:` list of `tool(arg)` rule keys,
  loaded at startup and re-written on each new grant. Like the provider-key env file
  (#220) it sits *beside* `config.yml`, not inside it: the runtime rewrites it
  freely, so it never clobbers the hand-edited, commented config. A missing/malformed
  store loads empty and a write failure is logged — both fail *closed* (ask again),
  the safe direction. The TUI modal offers `y` once / `s` session / `a` always /
  `n` reject / `e` edit-reason / `Esc` interrupt.
- **User config file + permission ceiling (✅ #172, [ADR-0047](../adr/0047-local-trust-boundary.md)):**
  a general user settings file, same layered loader as everything else — embedded
  default (`entanglement-runtime/src/config/defaults.yml`) < user
  (`${config_dir}/entanglement/config.yml`, path override `ENTANGLEMENT_CONFIG_FILE`)
  < project (`<root>/.entanglement/config.yml`), deep-merged at the
  `serde_yaml::Value` level (a field override keeps its siblings) with
  `deny_unknown_fields` on the result. It carries the general settings `agent` /
  `provider` / `model` / `verbose` (each a *fallback*: an explicit CLI flag or env
  var still wins — env > config > embedded) and, as its first section,
  `permissions` (tool → `allow | ask | deny`, same shape as agent frontmatter). The
  `permissions` section is a **global ceiling**: the runtime executor clamps every
  resolved grade least-privilege against it
  (`runtime::permission::clamp_to_base`), so a user/repo `bash: ask` forces every
  agent to ask but never *loosens* what an agent restricts. The embedded default is
  allow-all, so an untouched config is a no-op. The ceiling honors argument-scoped
  rule keys too (✅ #173) — `bash(rm *): deny` in the config clamps that command for
  every agent. The `permissions` section stays a pure ceiling (it only *tightens*);
  the orthogonal "always allow" grants (✅ #174) that *raise* an `Ask` live in a
  separate managed file, not here (see the approval-scope bullet above). Loaded in the
  runtime only (core has neither `dirs` nor `serde_yaml`). On first run, if the
  user file is missing, the runtime scaffolds a **fully-commented** starter
  template next to it (✅ #219, `config::scaffold_if_missing` writing
  `config/template.yml`) — every setting commented out, so it parses to `Null`,
  is skipped in the merge (`read_layer`), and changes nothing until edited; it
  only exists as a discoverable starting point. Best-effort: a write failure is
  logged, never fatal.
- **Managed provider-key env file (✅ #220):** a sibling
  `${config_dir}/entanglement/.env` (path override `ENTANGLEMENT_ENV_FILE`) holds
  the provider API keys outside any repo (`entanglement-runtime/src/config/env_file.rs`).
  Startup scaffolds a **commented** template listing the catalog's known key vars
  (`catalog.key_envs()` — `ZAI_API_KEY` / `OPENAI_API_KEY` / `ANTHROPIC_API_KEY`, …)
  when the file is missing, then loads its `KEY=VALUE` lines into the process
  environment **only for vars the real environment left unset** — the process env
  always wins (env > file), matching standard dotenv no-override. Both steps are
  best-effort (a read-only home or a malformed line is logged, never fatal) and run
  right after the catalog loads, before `select_provider` reads any key. The file is
  distinct from `config.yml`: it carries only secrets, so it stays out of the YAML
  config and out of version control.
- **File-defined (✅ #112, [ADR-0034](../adr/0034-file-based-agent-definitions.md)):**
  profiles are markdown files with YAML frontmatter (the config bundle) + a body
  (the system prompt), discovered at startup by the **runtime**
  (`entanglement_runtime::agents::load_registry`) into a `ProfileRegistry`. Three
  layers, later wins on a `name` collision: embedded built-ins (`build`/`plan`/
  `explore`, shipped as `include_str!` `.md` and parsed through the *same* loader)
  < user (`~/.claude/agents/*.md` then `${config_dir}/entanglement/agents/*.md`)
  < project (`.claude/agents` then `.agents/agents` then
  `<root>/.entanglement/agents/*.md`). Editing a built-in = dropping a same-`name`
  file in a higher layer — one mechanism for all three, same defaults+override
  shape as the provider catalog (#118). A malformed *native* file is a loud
  error; the cross-vendor dirs parse leniently — only `name`+`description` read,
  a malformed file warned and skipped, and a foreign agent defaults to
  `mode: all` so it is spawnable as a delegation target
  ([ADR-0074](../adr/0074-cross-vendor-skill-and-agent-discovery.md)). The
  frontmatter also declares `tools`/`disallowed_tools` (the tool mask, **enforced**
  ✅ #116, below) and `can_spawn`/`spawnable_agents` (fine-grained spawn control,
  **enforced** ✅ #119, below). The spawn boundary is now both spawner- and
  target-side: a profile must `may_spawn` and its *target* must be spawnable-mode
  (`subagent`/`all`) and on its `spawnable_agents` allowlist — so `build`/`plan`
  (primaries) are unreachable spawn targets from mode defaults alone. Plan
  authorship (`update_plan`, ✅ #231, below) and the plan-accept handoff (#141)
  complete the agent hierarchy. The built-in trio is defined **once**, here as
  markdown (#201): core carries only the `build` profile its `resolve()` fallback
  needs (it can't parse frontmatter, so it holds no `plan`/`explore` copy to drift
  from these files). Embedders using core directly get that single `build`
  fallback via `ProfileRegistry::new()`; the runtime rebuilds the full trio from
  the embedded markdown (`entanglement_runtime::agents::built_in_registry`). Add
  your own with `ProfileRegistry::insert`.
- **Per-profile model pinning (✅ #323, [ADR-0081](../adr/0081-per-profile-model-pinning-and-rebind-on-set-agent.md)):**
  a profile's frontmatter may set `provider:` beside `model:`. Both set = a
  **model pin** (`AgentProfile::model_pin()`): switching to the profile re-binds
  the session's whole backend to that `(provider, model)` — through the same
  `model_resolver` seam a live `/model` (`SetModel`) switch uses
  ([ADR-0063](../adr/0063-realtime-model-provider-switch.md)) — so a `plan`
  profile can run a different provider from `build`, and a sub-agent (a cheap
  `explore`) pins its own cheaper model. `model:` **without** `provider:` keeps
  the legacy request-level fallback (`req.model` only, **no** rebind); `provider:`
  **without** `model:` is a loud load error. The rebind lives in **core's
  `SetAgent`** handler (one locus for Tab cycle / `/agent` / `--agent` / spawn /
  wire; replay stays consistent), and at **session start** for a pinned starting
  profile — core stays policy-free, the runtime injects *which* model wins into
  the assembled profile. **Precedence:** per-session memory (a `/model` choice
  made while a profile was active, `Session.profile_models`) **>** the static
  frontmatter pin **>** keep the current binding (a pin-less profile with no
  memory changes nothing — no `ModelChanged`). A resolver failure surfaces an
  `Error` and keeps the old binding; the `AgentChanged` still succeeds.
  **Persistence:** picking a model via the TUI `/model` picker while a profile is
  active writes the pin to a **managed** `${config_dir}/entanglement/agent-models.yml`
  (override `ENTANGLEMENT_AGENT_MODELS_FILE`, shape `agents: { build: { provider,
  model } }`), overlaid onto matching profiles at startup — **persisted file >
  frontmatter**. Managed (not layered) like the grants + env files: the runtime
  rewrites it, so it stays out of the hand-edited `config.yml`. Missing/malformed
  → empty + warn (fail-open); a write failure is logged, never fatal
  (`entanglement_runtime::config::agent_models`).
- **Live reload + managed-file locking (✅ #329, [ADR-0084](../adr/0084-runtime-live-reload-and-managed-file-locking.md)):**
  a runtime-side `watch.rs` watches every resolvable agent/skill dir plus
  `${config_dir}/entanglement/` and `<root>/.entanglement/` (`notify`, debounced
  500ms so a burst of edits collapses into one reload) and, on change, re-runs the
  skill + agent loaders and swaps the result into **runtime-held mirrors**
  (`watch::LiveDefinitions { profiles, skills, agent_models, grants }`) — never
  core's `EngineConfig.profiles`, which stays pinned for the process lifetime (the
  [ADR-0081](../adr/0081-per-profile-model-pinning-and-rebind-on-set-agent.md)
  "live registry mutation" rejection applies identically here). Permission
  resolution (`tool_runner`'s `ToolExec` self-heal), `load_skill`, and the TUI's
  `/agent` picker + Tab-cycle roster all read through these live handles, so a
  definitions edit lands for the *next* `SetAgent`/new session/picker pick — a
  turn already in flight keeps its already-resolved system prompt/tool mask
  unchanged. A directory that doesn't exist at watch-start needs a restart to be
  picked up once created (known v1 limit). Separately, the three managed files
  (`grants.yml`, `agent-models.yml`, the provider-key `.env`) are now
  advisory-locked across concurrent `skutter` instances via
  `config::lock::with_locked_file` (an `fd-lock` on a sibling `.lock` file): each
  write re-reads the current on-disk state under the lock and merges before
  writing, so a second instance's own concurrent update survives instead of being
  clobbered by a write from stale in-memory state; `write_grants` moved onto the
  shared `atomic_write`. Because those three files (plus a tool-allowlist
  override, [ADR-0083](../adr/0083-in-app-tool-allowlist-editing-as-user-layer-materialization.md))
  live inside the same watched `${config_dir}/entanglement/` tree, this
  process's *own* write to any of them also fires the watcher — redundantly,
  since the writer already updated its in-memory state synchronously.
  `config::atomic::atomic_write` (the shared funnel all four writers use)
  stamps a process-local last-write timestamp; `reload()` still refreshes
  state on every firing (so a genuinely concurrent external write is never
  missed) but suppresses the user-facing "definitions reloaded" notice when
  the firing landed within ~2s of this process's own stamp — closing what
  would otherwise read as a runaway reload loop during a session with several
  approvals/model picks.
- **Physical tool restriction (✅ #116, [ADR-0038](../adr/0038-physical-per-agent-tool-restriction.md)):**
  an agent's `tools` allowlist / `disallowed_tools` denylist masks its tool set —
  `registry ∩ allowlist − denylist` — on *both* sides of the core↔runtime seam,
  orthogonal to `permission` (which grades `Allow`/`Ask`/`Deny` among the tools
  that survive the mask). The mask rides the core `AgentProfile`
  (`tools`/`disallowed_tools` + `advertises_tool`), so it travels per session with
  no new protocol surface. **(a) Advertisement:** core's turn loop (`run_round`) filters both
  `EngineConfig.tool_specs` and the active profile's `profile_tool_specs` entry by
  the mask — a masked tool's schema never reaches the model. `update_plan`/
  `update_tasks` are ordinary runtime state tools now (✅ #231, below): they ride
  those specs and this mask like any host tool, no plan-authority special casing in
  core. **Per-session base specs (✅ #308, [ADR-0076](../adr/0076-per-session-dynamic-tool-specs.md)):**
  an optional `EngineConfig.tool_spec_resolver: Option<Arc<dyn Fn(&SessionId) ->
  Vec<ToolSpec> + Send + Sync>>` (alias `ToolSpecResolver`) lets one `Holly`
  advertise a **different base tool surface per session** — the seam multi-tenant
  embedding needs so user A's discovered MCP-server tools never reach user B's
  sessions and a site's per-session restriction is expressible without one engine
  per user. `run_round` consults it *fresh every turn* (so a backing-store edit
  lands on the next turn, no respawn); its output **replaces** the engine-global
  `tool_specs` for that session (the embedder composes if it wants both),
  `profile_tool_specs` still append, and the mask below still filters the result —
  the resolver **widens discovery, it never bypasses masking** (it runs *before*
  the mask). Sync `Fn` by design (turn hot path); the documented pattern is an
  embedder-owned `Arc<RwLock<..>>` snapshot cache. `None` (the default) keeps the
  engine-global specs — a no-op for single-user heads. **(b) Enforcement:**
  `runtime::permission::tool_masked` refuses a masked `ToolExec` **first** — before
  the `agent_spawn`/`agent`/`agent_poll`/`ask_user` interceptions and permission —
  so a hallucinated masked call is a hard boundary, and the mask **intersects down
  the ancestor chain** (a child never gains a tool an ancestor lacked, mirroring
  ADR-0024's privilege ceiling). `explore` is now the reference read-only agent:
  `tools: [read, glob, grep]` — no `edit`/`write`, no `bash`, no `agent_spawn`.
- **In-app tool-allowlist editing (✅ #330, [ADR-0083](../adr/0083-in-app-tool-allowlist-editing-as-user-layer-materialization.md)):**
  editing a mask materializes a user-layer override, not a new config surface —
  the layered loader already shadows a same-`name` definition, built-in
  included, so there is no separate "edit built-ins" path.
  `entanglement_runtime::agents::materialize::save_tools_override(root, name,
  allowed)` resolves the currently effective definition's raw text
  (`winning_raw_text`, same precedence as `load_registry`), rewrites only the
  `tools:`/`disallowed_tools:` frontmatter keys via a `serde_yaml::Mapping`
  round-trip (`rewrite_tools` — order-preserving, every other key and the body
  untouched), and writes atomically via `config::atomic::atomic_write` to
  `${config_dir}/entanglement/agents/<name>.md` (or `ENTANGLEMENT_AGENTS_DIR`).
  In the TUI, `e` on the `/agent` picker's highlighted profile opens a
  single-stage checklist dialog (`tui::tools_dialog::ToolsDialog`) over the full
  advertised tool roster — captured from `EngineConfig.tool_specs` in the
  runtime head (so it also covers runtime-owned specs like
  `update_tasks`/`ask_user`/`rhai`, not just `ToolRegistry` names), seeded from
  the profile's current effective mask; `Space` toggles, `Enter` saves +
  records a transcript status line, `Esc` discards. The write applies on the
  next restart — there is no live registry reload yet (a separate watcher
  issue); `skutter inspect agents` still reports the winning layer and what it
  shadowed, so provenance stays visible.
- **Per-profile spawn control (✅ #119, [ADR-0040](../adr/0040-per-profile-spawn-control.md)):**
  spawning is a per-profile capability declared in the definition — *whether* a
  profile may spawn (`can_spawn`, default: open for primaries/`all`, closed for a
  `subagent` leaf) and *which* profiles it may spawn (`spawnable_agents`, omitted ⇒
  any spawnable target). Both ride the core `AgentProfile` with helpers
  (`may_spawn`, `spawn_target_allowed`, `spawnable_as_subagent`); core = semantics,
  runtime = enforcement. **Structural half:** the `agent_spawn`/`agent`/`agent_poll`
  triple moves out of the shared `tool_specs` into
  `EngineConfig.profile_tool_specs` (a `HashMap<profile, Vec<ToolSpec>>` the runtime
  fills via `subagent::spawn_specs_for`); the turn loop appends the active profile's
  entry (roster + `agent` enum scoped to who *it* may spawn, empty when it may not),
  so an out-of-list spawn is a schema violation before an executor refusal.
  **Executor half:** `runtime::permission::spawn_refusal(spawner, target, registry)`
  layers four checks before the ADR-0023 budget + ADR-0024 clamp — `!may_spawn`
  (absorbs the old capability gate) → unknown target → target not spawnable-mode
  (a `primary` is never a valid target) → target off the `spawnable_agents` list —
  each a clear `ToolOutput` with no child minted. The allowlist is checked per
  spawning session against *its own* profile (**not transitive**). Supervisor
  hardening: `InMsg::Spawn` with an unknown name now `get()`s + errors instead of
  silently escalating to `build`. The TUI roster is registry-driven: the
  `/agent` picker (Ctrl+A) lists every entry agent (`mode ∈ {primary, all}`),
  while the implicit **Tab cycle** ring is `mode: primary` only (#322) — so
  cross-vendor `all`-mode agents (ADR-0074) don't flood it — with `Shift+Tab`
  (crossterm `BackTab`) reverse-cycling the same ring; if no primaries exist the
  ring falls back to the whole entry list so Tab is never empty. Explicit
  selection stays unrestricted: `--agent`, `user_config.agent`, and `SetAgent`
  accept any registered name; the filter governs only the implicit cycle.
- **Plan/task state tools (✅ #231, [ADR-0049](../adr/0049-plan-task-tools-as-runtime-state-tools.md)):**
  `update_plan` and `update_tasks` are **runtime** state tools, not core built-ins.
  Each replaces the session's *display* plan/task outline; the runtime executor
  emits the `OutEvent::Plan`/`OutEvent::TaskList` snapshot (reusing the `ToolExec`
  seq) and acks the model — the engine holds no plan/task state. They round-trip
  via `ToolExec`/`ToolResult` and resolve through the **ordinary** `Allow`/`Ask`/
  `Deny` path + #116 mask, with **no** plan-authority special casing (they fall
  through `tool_runner`'s generic `dispatch`; `run_and_reply` emits the snapshot
  instead of hitting the host `ToolRegistry`, since they touch no host resource).
  This closes **#175**: a read-only `explore` has `update_tasks` outside its
  allowlist (mask refusal) *and* permission-denied, so it can't mutate task state.
  **Plan authorship is default-closed via the tool mask** — `update_plan`/
  `propose_plan` are advertised (in `profile_tool_specs`) only to a profile that
  *explicitly* allowlists them; an inherit-all (`tools: None`) profile never gains
  them by accident (the replacement for the old `owns_plan` flag). `update_tasks`
  rides the shared `tool_specs` (general bookkeeping, no cross-agent authority).
  Built-in `plan` names `update_plan`/`propose_plan` in its allowlist + carries
  `update_plan: allow` (authoring isn't an approval prompt) and stays physically
  read-only (`tools: [read, glob, grep, agent, agent_spawn, agent_poll, ask_user,
  load_skill, update_plan, propose_plan]`), a clamp its spawned children inherit.
- **Plan acceptance — `propose_plan` (✅ #141, [ADR-0042](../adr/0042-plan-acceptance-via-propose-plan-approval-roundtrip.md)):**
  the plan agent's *finalize* step (`update_plan` stays for working snapshots). A
  runtime-owned tool `propose_plan { plan }`, advertised only to a profile that
  explicitly allowlists it (via the `profile_tool_specs` seam) — the same
  default-closed plan-authorship gate as the state tools (#231). Acceptance rides
  the **existing tool-approval round-trip** (#59): the executor (`propose_plan.rs`)
  intercepts it on `ToolExec` after the #116 mask check (same interception family
  as `ask_user`) and **force-parks it on the `Ask` path unconditionally** — a
  permission profile can never `Allow` it, since user approval *is* the tool's
  semantics. A standard `OutEvent::ToolRequest` reaches the head. **Approve** →
  reply `ToolOutput("plan accepted by the user")` (the engine holds no plan state
  to record; the working plan was already surfaced via `update_plan`), and the head
  performs the **handoff** from the tool input (see §5c). **Reject + reason** → the
  existing fold-back (`tool \`propose_plan\` rejected: <reason>`); the model revises
  and re-proposes in the same turn. One-shot heads (`run`/`pipe`) can't park an
  interactive approval, so they auto-reject with a "non-interactive head" reason.
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
  **Per-turn prompt override (✅ #310, [ADR-0078](../adr/0078-per-turn-dynamic-system-prompt.md)):**
  an optional `EngineConfig.system_prompt_resolver: Option<Arc<dyn Fn(&SessionId,
  &AgentProfile) -> Option<String> + Send + Sync>>` (type alias
  `SystemPromptResolver`) is consulted fresh at every turn build in `run_round`
  (`session/turn.rs`), resolved once and threaded into `stream_round` where
  `s.profile.system_prompt` was read directly. A `Some(prompt)` return **overrides**
  the profile's assembled prompt for that turn; `None` (or no resolver, the
  default) falls back to it — so an embedder whose prompt is user-editable content
  (a site serving it from a CMS page) picks up an edit on the **next turn** with no
  engine respawn. The `Fn` sees the running session's *own* id + resolved profile,
  so sub-agent turns resolve against **that child's** profile (per-profile prompts
  keep working) and a resolver can compose off `profile.system_prompt` rather than
  only replace it. Sibling of the `tool_spec_resolver` seam (ADR-0076) — sync `Fn`,
  same embedder-owned snapshot-cache pattern; no protocol/wire change.
- **Skill discovery + registry (✅ #114, [ADR-0036](../adr/0036-skill-discovery-and-registry.md)):**
  tier 1 of progressive disclosure. A **skill** is a directory with a `SKILL.md`
  (YAML frontmatter + markdown body) plus optional supporting files
  (`references/*.md`, `scripts/*`). The **runtime**
  (`entanglement_runtime::skills::load_registry`) discovers them into a
  `SkillRegistry` — three layers, later wins on a `name` collision: embedded stock
  skills (single-file, `include_str!` `SKILL.md`, parsed through the *same* loader)
  < user (`~/.claude/skills/**/SKILL.md` then
  `${config_dir}/entanglement/skills/**/SKILL.md`, override
  `ENTANGLEMENT_SKILLS_DIR` — replaces the whole user layer) < project
  (`.claude/skills` then `.agents/skills` then
  `<root>/.entanglement/skills/**/SKILL.md`).
  Discovery is a recursive walk for `SKILL.md` markers; symlinked duplicates and
  directory cycles are deduped by canonical path; a malformed file is a loud
  error in the native dirs, warned-and-skipped in the cross-vendor ones (which
  read only `name`+`description`, mapping Claude's `disable-model-invocation` to
  `user_only` and dropping its `allowed-tools`,
  [ADR-0074](../adr/0074-cross-vendor-skill-and-agent-discovery.md)).
  Frontmatter: `name` + `description` (required), `user_only` (only explicit
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
  `run_round` filter) and at dispatch (`tool_masked`). The two compose to preserve
  both corners: "preload X but block everything else" (`skills: [x]` + `load_skill`
  masked out) and "preload nothing, request on demand" (no `skills:`, `load_skill`
  available). Default stays permissive — a subagent may discover + load any skill
  via the same LLM gate as a primary unless masked.
- **Where dispatch runs (✅ #59):** the `AgentProfile` *shape* stays a core
  protocol type, but the `Allow|Ask|Deny` decision + the approval wait are a
  **runtime** concern ([ADR-0003](../adr/0003-agent-and-permission-profiles.md) /
  [ADR-0010](../adr/0010-single-head-crate-and-bash-opt-in.md)). Core emits
  `ToolExec` for *every* host tool — the whole batch up front since #270 ([ADR-0061](../adr/0061-parked-turn-state-batch-tool-resolution.md)) — and parks the turn as explicit `TurnState` until each `ToolResult` lands (§8); it never reads
  `PermissionProfile`. The runtime `tool_runner` (§8) tracks each session's active
  profile against a `ProfileRegistry` copy it holds, resolves the permission, and —
  for `Ask` — emits the `ToolRequest` prompt and awaits `Approve`/`Reject`/`Stop`,
  so every head stays a thin protocol adapter (it just sends the same frames; the
  runtime, not core, acts on them).
- **Authoritative gating, fail-closed (✅ #156, [ADR-0070](../adr/0070-authoritative-tool-exec-profile-and-fail-closed-fallback.md)):**
  the profile map was folded *only* from the **lossy** `SessionStarted`/`AgentChanged`
  broadcast, with a fail-*open* default — an unseen session resolved to `Allow` and
  *unmasked*. Under burst a dropped lifecycle frame therefore ran a restricted
  `explore` session allow-all/unmasked: the posture inverted under overload. Fixed
  two ways. `OutEvent::ToolExec` now carries `agent` (the emitting session's profile
  name); the executor **self-heals** its map from that field before any
  mask/permission decision, so the leaf's gate is authoritative regardless of a
  dropped `SessionStarted` (ancestors self-heal via their own spawn `ToolExec`s).
  And the residual-unknown fallback flips **fail-closed**: an unseen session resolves
  to `Deny` (`permission_for`) and to *masked* (`tool_masked`) — degraded but safe.
