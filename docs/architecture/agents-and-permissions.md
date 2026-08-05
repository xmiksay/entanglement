# entanglement Architecture — Agent profiles, permissions, skills & system prompt

> Part of the [architecture overview](../architecture.md). The *why* behind each choice is in the [decision log](../adr/README.md).

## 3. Agent profiles + permissions (opencode-style) — [ADR-0003](../adr/0003-agent-and-permission-profiles.md)

A session runs under exactly one [`AgentProfile`][profile]:
`{ name, description, mode, system_prompt, model?, provider?, permission, tools?,
disallowed_tools, can_spawn?, spawnable_agents?, sandbox? }`. `mode` is
`primary | subagent | all`; `description` drives delegation matching (§8, the
only field a spawning model sees). The `tools`/`disallowed_tools`/`can_spawn`/
`spawnable_agents` quartet are the physical restrictions layered over
`permission`: the `tools`/`disallowed_tools` mask
(#116, [ADR-0038](../adr/0038-physical-per-agent-tool-restriction.md)) governs which tools *exist*
for the profile, and `can_spawn`/`spawnable_agents` gate sub-agent spawning
(#119, [ADR-0040](../adr/0040-per-profile-spawn-control.md)) — both detailed
below. (There is no `owns_plan` field: plan authorship rides the tool mask now,
#231, [ADR-0049](../adr/0049-plan-task-tools-as-runtime-state-tools.md).)
`sandbox: Option<String>` (`bwrap`/`none`/`inherit`, #479,
[ADR-0134](../adr/0134-per-profile-sandbox-scoping-and-spawn-chain-clamp.md))
is opaque to core, exactly like `permission`'s rules — it overrides the
process-global `ENTANGLEMENT_SANDBOX` bubblewrap default (§8 of
[gates & host tools](gates-and-host-tools.md)) for this profile's `bash`/`call`
calls; interpreted entirely by the runtime's `host::sandbox`.

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
  until it acts: spawn-target `name: description` in the `agent` tool
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
  **runtime** — every tool, including `propose_plan`/`update_tasks`, round-trips
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
  command for `bash`/`call`, the target path for `edit`/`write`/`read`/`apply_patch`
  (#455), the
  `pattern` (itself a path glob) for `glob`, and the optional file filter for
  `grep` (a path, distinct from `grep`'s regex `pattern`; absent → `None`, #417
  — a prerequisite for #416 phase B's arg-scoped read fan-out) — so
  `bash(git *): allow`, `bash(rm *): deny`, `edit(src/*): allow`,
  `grep(src/*): allow` all refine a coarse `bash: ask`/`grep: ask`. The runtime
  extracts the argument from the call input (`runtime::permission::permission_arg`)
  where the JSON is already in hand; argument-less rules and name-only callers
  (inspect/TUI posture panels) resolve exactly as before. **Path-arg tools grade
  root-relative** (✅ #485, [ADR-0125](../adr/0125-permission-arguments-for-path-tools-are-normalized-root-relative.md)):
  rules like `read(src/*)` are authored relative to the project root, but a
  model may spell an in-root path absolutely — `permission_arg`'s verbatim
  extraction alone can't match `bash(git *)`-style rules against
  `/root/src/main.rs`. `runtime::permission_path::grading_arg` wraps
  `permission_arg` with lexical `.`/`..`/`//` folding plus a root-prefix strip
  (for `read`/`edit`/`write`/`apply_patch`/`glob`/`grep` only — `bash`/`call`'s
  command line is never touched) whenever a project root is wired
  (`ProfileResolver`'s `root`, `tool_runner::dispatch`'s escape-root-derived
  root, `script::BindingPolicy`'s `root`); an absolute path *outside* root
  stays verbatim, since a root-relative rule matching it would be a privilege
  escalation, and the escape-root gate below already owns that case.
  `permission_arg` itself is unchanged and keeps driving the TUI transcript's
  literal display. A second, independent
  clause `tool{pattern}` (✅ #425, [ADR-0116](../adr/0116-workdir-scoped-permission-rules-for-bash-call.md))
  scopes `bash`/`call` by **working directory** instead of command line —
  `bash{/tmp/*}: allow`, `bash{/etc/*}: deny` — extracted by
  `runtime::permission::permission_workdir` and resolved via
  `PermissionProfile::resolve_scoped(name, arg, workdir)`; `resolve` stays the
  two-argument entry point every other tool uses, equivalent to
  `resolve_scoped(.., workdir: None)`, so a `tool{pattern}` rule is simply inert
  for a tool with no workdir concept. Both clauses compose in one ordered rule
  list via the same last-match-wins semantics — no compound-key grammar. The
  graded decision drives:
  - `Allow` → run the tool, reply `ToolResult` → core emits `ToolOutput`.
  - `Ask` → emit `ToolRequest`, park at `WaitingApproval` until `Approve`/`Reject`;
    on approve, run the tool and reply `ToolResult`; on reject, reply
    `ToolResult("…rejected…")`.
  - `Deny` → reply `ToolResult("…denied…")` without running the tool.
- **Capability-level permission keys (✅ #418, [ADR-0114](../adr/0114-capability-level-permission-keys.md),
  part of the #416 epic):** a rule key may also be a **capability** —
  `read`/`write`/`call` — instead of a literal tool name. Expanded at **parse
  time** in the single chokepoint both surfaces share,
  `agents::permission_from_value` (agent frontmatter *and* the user-config
  ceiling below), into the literal per-tool rules `PermissionProfile::resolve`
  actually matches — core stays capability-unaware (ADR-0006). The membership
  table (`tool_names::CAPABILITIES`) is `read`⇒`read`/`grep`/`glob`,
  `write`⇒`edit`/`write`/`apply_patch` (#455), `call`⇒`bash`; the literal `call` tool and `rhai`
  are `tool_names::MULTI_GROUP` — general-purpose tools that can themselves
  read, write, or execute — so their grade isn't taken from any one
  capability's fan-out. Instead a pre-scan takes the least-privileged (`min`)
  of every *bare* `read`/`write`/`call` grade set (plus any bare literal
  `rhai:` grade, which tightens the same computation) and emits it first as
  `call`/`rhai`, regardless of the keys' order in the source map — leaving room
  for a later arg-scoped `call(...)` rule to still refine `call` via ordinary
  last-match-wins (nothing refines `rhai`, which has no argument). A bare
  capability key (`read: allow`) fans out to its single-group members only; an
  arg-scoped capability key (`read(src/*): allow`) fans out `member(pattern)`
  per member, with `call`'s arg-scoped list additionally including the literal
  `call` tool (`call(git *)` ⇒ both `call(git *)` and `bash(git *)`). Command
  sets stay flat `call(pattern): grade` lines expanding to `call`+`bash`, not a
  nested YAML shape. A workdir-scoped capability key (✅ #425, ADR-0116) fans
  out the same way through the `{pattern}` clause — `call{/tmp/*}: allow` ⇒
  both `call{/tmp/*}` and `bash{/tmp/*}` — sharing the identical member list as
  the arg-scoped case. `plan.md`'s pre-existing `read: allow` is now a capability
  key too, so it also flips `grep`/`glob` from the profile's `ask` default to
  `allow` — an accepted, test-pinned behavior change, not a silent diff.
  **MCP tools join the fan-out via a config-side hint** (✅ #426,
  [ADR-0117](../adr/0117-mcp-tool-capability-fan-out.md)): an MCP tool
  (`mcp__<server>__<tool>`) carries no protocol-level capability of its own, so
  a bare `read`/`write`/`call` key used to fall straight through it. A `mcp:`
  server block now accepts an optional `capabilities: {tool: read|write|call}`
  map (raw tool name), folded by `mcp::capability_index` into an
  `McpCapabilityIndex` (capability → namespaced tool names, reusing `McpTool`'s
  own naming helper so it can never drift from what actually registers).
  `expand_capabilities` takes this index as a parameter and extends only the
  **bare** capability case with it — scoped (`read(pattern)`/`call{pattern}`)
  keys and the `call`/`rhai` multi-group are untouched, since an MCP tool has
  no command/workdir argument to scope against and isn't a general-purpose host
  tool. The index is built once at startup from config alone (no live server
  connection required) and threaded into `agents::load_registry`, the ceiling
  parse below, and the live-reload watcher's static snapshot — matching how
  the ceiling itself is already startup-only, not live; an annotation naming a
  tool the server never registers is simply inert. `skutter inspect agents`/
  `prompt_report`/`built_in_registry` deliberately keep an empty index (a debug
  view that already doesn't reflect the ceiling clamp either).
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
  `(tool, argument)` — the same (root-relative, ✅ #485, ADR-0125) argument
  #173 resolves against, computed once in `dispatch` and threaded into the
  post-approval record rather than re-derived, so the pre-prompt lookup and
  the record provably share one key. **After** the full
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
- **Session-scoped directory grants (✅ #486, [ADR-0126](../adr/0126-session-scoped-directory-grants.md)):**
  a fourth `ApprovalScope::SessionDir` — session-only like `Session`, but
  widened to every later call whose grading argument falls under the approved
  call's directory (`grants::dir_covers`, a plain path-component-prefix check
  on the #485-normalized argument) instead of matching one exact call.
  Restricted to the read-only triad (`read`/`grep`/`glob`, the ADR-0114 `read`
  capability's members, reused via `tool_names::is_read_capability_member` so
  the grant store, the TUI's `[d]` key gate, and its footer hint can never
  drift apart); any other tool — or an escape-forced prompt, in
  `ExtraRootStore` — degrades it to an exact `Session` grant rather than
  widening. `FileGrantStore` gains a separate, never-persisted
  `session_dirs: HashMap<SessionId, BTreeSet<String>>` (no `Always`-directory
  scope, so `grants.yml`'s shape is untouched); `grants::dir_for(tool, arg)`
  derives the directory an approved call implies (parent dir for
  `read`/`edit`/`write`/`apply_patch`, the path filter verbatim for `grep`,
  the literal non-wildcard prefix for `glob`). `GrantStore::grant_session_dir`
  is default-implemented (a no-op echo), so the #311 seam's existing custom
  implementations keep compiling untouched — only `DefaultGrantStore`
  overrides it for real. Two TUI surfaces: `[d]` on an approval prompt
  (`tui/event_loop.rs`, gated on the pending tool being read-like) and a
  proactive `/allow <path>` command (`tui/allow_command.rs`, normalizing the
  path against the head's root and rejecting anything outside it) — both call
  `grant_session_dir` synchronously through a cloned `Arc<DefaultGrantStore>`
  handle threaded into the TUI, introducing no new wire surface (`Approve`
  was already wire-allowed).
- **Per-user permission ceiling + grants (✅ #522, [ADR-0147](../adr/0147-multi-user-mode-embedder-api.md)):**
  built entirely on the #311 seams above, no core change. A session now
  carries an optional `UserId` (`Session.user`, spawn-time-fixed like
  `parent` — a child/compaction-successor inherits its parent's/predecessor's
  user rather than being re-told). `entanglement-runtime::multi_user::permission`
  gives a multi-user embedder `PerUserPermissionResolver<R: PermissionResolver>`
  (wraps an inner resolver — typically `ProfileResolver`, so the process-global
  #172 ceiling still applies first — and clamps its result a *second* time by
  the resolving session's own user's ceiling, via the same `clamp_to_base`
  least-privilege composition #172 itself uses) and `PerUserGrantStore` (keys
  `Always`-scope grants by `UserId` instead of one flat process-wide set, so
  the storage key itself is what makes "one user's grant never leaks to
  another" true; `Session` scope stays keyed by `SessionId` as normal, since a
  session belongs to exactly one user already). Both consult a small
  `SessionUserRegistry` the embedder populates itself — it already knows the
  session→user mapping, having chosen `user` when it sent the session's
  `InMsg::Spawn`. In-memory reference implementations only; a production
  multi-tenant embedder with its own DB is expected to implement
  `GrantStore`/`PermissionResolver` directly instead, per the traits' own
  doc guidance. Reachable only through the embedder library API — `serve`
  stays single-user (ADR-0048).
- **Escape-root access via approval (✅ #escape-root, [ADR-0109](../adr/0109-escape-root-access-via-approval.md)):**
  root containment (ADR-0054) is no longer absolute. A `read`/`edit`/`write`/`apply_patch`
  path or a `bash`/`call` `workdir` that resolves **outside** root is detected in
  the executor (`permission::escape_root_target` + `host::escaping_path`) and
  forces an approval prompt even when the profile would `Allow` (a `Deny` floor
  still wins). The approval is recorded in a **separate** store from the
  permission grants above — `runtime::extra_roots::ExtraRootStore`, managed file
  `${config_dir}/entanglement/extra-roots.yml` (override
  `ENTANGLEMENT_EXTRA_ROOTS_FILE`) — keyed by `(tool, resolved-absolute-path)`,
  **per tool** (a `read` grant never unlocks `write`), at `Once` (single-use,
  additionally bound to the approving call's `request_id` so a concurrent
  in-flight call can't spend it, #449,
  [ADR-0120](../adr/0120-once-scoped-escape-root-grant-bound-to-request-id.md)) /
  `Session` (process-lifetime) / `Always` (persisted) scope. The host tools
  consult it via `resolve_under_root_or_grant` to relax containment for the
  approved path (matched against the symlink-canonicalized target). Reuses the
  `ToolRequest`/`Approve{scope}` wire (no new variant); `glob`/`grep` stay
  strictly root-contained. The store is separate from `grants.yml` because the
  key spaces differ — a permission grant upgrades `Ask→Allow` on a `tool(command)`
  key, an escape grant relaxes *containment* on a `(tool, absolute-path)` key.
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
  every agent — and capability keys (✅ #418, ADR-0114) exactly like agent
  frontmatter, since both share `agents::permission_from_value` — a config
  `call: deny` ceiling denies both the literal `call` tool and its `bash`
  member. The `permissions` section stays a pure ceiling (it only *tightens*);
  the orthogonal "always allow" grants (✅ #174) that *raise* an `Ask` live in a
  separate managed file, not here (see the approval-scope bullet above). Because
  the **project** layer merges last (trusted, ADR-0047), a repo can also
  *re-loosen* a key the user's own layer set — that stays legal, but the loader
  now warns loudly about it (`config::ceiling_warn`): one `tracing::warn!` per
  `permissions` key the project file sets to a different value than the earlier
  layers resolved to, so a hostile repo's silent `bash: ask → allow` flip is at
  least visible at startup (ADR-0047's mitigation is inspection, not
  restriction). Loaded in the
  runtime only (core has neither `dirs` nor `serde_yaml`). On first run, if the
  user file is missing, the runtime scaffolds a **fully-commented** starter
  template next to it (✅ #219, `config::scaffold_if_missing` writing
  `config/template.yml`) — every setting commented out, so it parses to `Null`,
  is skipped in the merge (`read_layer`), and changes nothing until edited; it
  only exists as a discoverable starting point. Best-effort: a write failure is
  logged, never fatal.
- **Live tool-overlay grades compose with the ceiling too (✅ #498/#539,
  originally [ADR-0133](../adr/0133-live-bash-enablement-graded-by-permission.md),
  generalized by
  [ADR-0163](../adr/0163-live-bash-enablement-is-a-tool-overlay-entry.md),
  #611):** a live `/enable tool bash --allow` grade — a session
  `ToolOverlayEntry` — overrides the session's own profile for that tool
  specifically via `tool_runner`'s generic overlay-grade dispatch
  (`permission::overlay_entry_grade`), but the result still passes
  through `clamp_to_base` unconditionally — a config ceiling of `bash: deny`
  still wins over a live `Allow`, same as it wins over any agent profile's own
  `Allow`. This composes for any tool the overlay enables, not only `bash`.
  The grade override reaches past the exact session that set it, too (✅
  #539/#628): `permission::overlay_grade_entry` walks the same
  nearest-link-first ancestor chain the mask's `tool_masked` already does,
  and the `rhai` `BindingPolicy` snapshot consults the identical lookup —
  closing the deferral where a script's `bash()` binding or a spawned child
  with no overlay of its own still graded through the plain profile chain.
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
  `explore`/`debug`/`research`, shipped as `include_str!` `.md` and parsed
  through the *same* loader)
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
  authorship (`propose_plan`, ✅ #231/#513, below) and the plan-accept handoff
  (#141) complete the agent hierarchy. The built-ins are defined **once**, here as
  markdown (#201): core carries only the `build` profile its `resolve()` fallback
  needs (it can't parse frontmatter, so it holds no `plan`/`explore`/`debug`/
  `research` copy to drift from these files). Embedders using core directly get that single
  `build` fallback via `ProfileRegistry::new()`; the runtime rebuilds the full set
  from the embedded markdown (`entanglement_runtime::agents::built_in_registry`).
  Add your own with `ProfileRegistry::insert`.
- **Default restriction map (the built-in quintet, at a glance):** what a
  session can actually do out of the box, per profile — the product of the
  tool mask (which tools *exist*) and the permission ladder (how an existing
  tool grades). Source of truth: `entanglement-runtime/src/agents/*.md`.

  | Profile | mode | tools mask | permission | spawn |
  | --- | --- | --- | --- | --- |
  | `build` (default) | primary | none — every registered tool exists | `default: allow` — everything Allow | may spawn `explore`/`debug` |
  | `plan` | primary | `read, glob, grep, agent, agent_send, poll, ask_user, load_skill, propose_plan, write, edit, call, bash` — `call`/`bash` are on the mask only so a spawned `explore` child keeps its own access ([ADR-0159](../adr/0159-plan-mask-widened-for-explore-delegation.md), #597); `agent_send` (#609, [ADR-0162](../adr/0162-agent-send-supervising-a-sub-agent.md)) is what lets the plan agent re-engage the same sponsored `build` child for another review round instead of spawning a fresh one each phase | `default: ask`; `read: allow` (capability fan-out covers `grep`/`glob`); `write: deny` with `write(.entanglement/plans/*.md): allow` — the plans-folder carve-out (#524, [ADR-0142](../adr/0142-trusted-scratch-dir-and-plans-folder-carve-outs.md)), fanning out to `edit`/`apply_patch` too | may spawn |
  | `explore` | subagent | `read, glob, grep, call, bash, poll, rhai` | `default: deny`; read triad Allow, exec set at `Ask` (escalates to user, never runs silently; [ADR-0137](../adr/0137-explore-ask-grade-shell-access.md)) — `poll` rides along with `bash` so a background job it starts is actually readable (#615/#605); `poll` is intercepted before permission resolution, so it carries no grade of its own | cannot spawn |
  | `debug` | subagent | none — every registered tool exists | `default: allow` | cannot spawn |
  | `research` | all | `read, glob, grep, agent, poll, ask_user, load_skill, call, bash, rhai` — no write tools, no `propose_plan` | `default: ask`; `read: allow`; `write: deny` with **no** carve-out; exec at `Ask` via `call(*): ask` + a literal `rhai: ask` (the [ADR-0159](../adr/0159-plan-mask-widened-for-explore-delegation.md) grading pattern; posture per [ADR-0137](../adr/0137-explore-ask-grade-shell-access.md)) — the global read-only Q&A entry agent ([ADR-0167](../adr/0167-embedded-research-agent-profile.md)) | may spawn **only** `research` — the self-only allowlist transitively closes the subtree |

  Three cross-cutting facts complete the picture: **(1)** `bash` is
  opt-in — until registered (startup `ENTANGLEMENT_ENABLE_BASH=1`, or live
  `/enable tool bash`, #498/#611,
  [ADR-0163](../adr/0163-live-bash-enablement-is-a-tool-overlay-entry.md))
  it doesn't exist for *any* profile and the only exec tool is `call` (single
  argv, no shell). **(2)** an active skill's `allowed_tools` (ADR-0106) is a
  **literal exact-name** mask — no capability fan-out — so an exec-capable
  skill must list `call` explicitly, and a skill that edits must list
  `write`/`edit`/`apply_patch`/`glob`/`load_skill` too or loading it mid-turn
  disarms editing for the rest of the turn (#554); it layers *after* the
  profile mask and also reaches `rhai` bindings (ADR-0129). **(3)** the
  user-config permission
  ceiling defaults to `default: allow` — a no-op clamp until the user
  tightens it (#172).
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
- **Per-profile generation-parameter persistence (✅ #374, [ADR-0094](../adr/0094-reasoning-effort-and-per-profile-generation-persistence.md)):**
  mirrors the model pin above — same three-tier precedence (session memory >
  persisted > current binding), applied at the same two loci (`SetAgent`,
  session start) — but through a **separate** seam:
  `EngineConfig.generation_resolver: Option<GenerationResolver>`, a
  `Fn(&str) -> Option<GenerationParams>` keyed by profile *name* rather than a
  field baked into `AgentProfile`. `GenerationParams` carries
  `temperature: Option<f32>`, which has no total `Eq`, so it cannot join
  `AgentProfile`'s `PartialEq + Eq` derive the way the pin's `provider`/`model`
  fields do — the resolver indirection is the price of keeping
  `GenerationParams` a plain `Copy` value type. `Session.profile_generation`
  (session memory) and the resolver's return (the persisted tier) are both
  **full** `GenerationParams` snapshots, applied by direct assignment — unlike
  the partial-merge `GenerationParams::apply_overrides` a live
  `InMsg::SetGeneration` itself uses. **Persistence:** the runtime's
  `entanglement_runtime::config::agent_generation::AgentGenerationStore`
  (`${config_dir}/entanglement/agent-generation.yml`, override
  `ENTANGLEMENT_AGENT_GENERATION_FILE`, sibling of `agent-models.yml`) has the
  same `load`/`get`/`set`/`reload` shape and the same fail-open/locked-write
  behavior — but **no** `apply(&mut ProfileRegistry)`: there's nothing on
  `AgentProfile` to overlay, so `AgentGenerationStore::resolver(store)` builds
  the `GenerationResolver` closure directly (resolved fresh on every call, so a
  `set`/`reload` is visible without rebuilding it). **TUI surface (✅ #376,
  [ADR-0095](../adr/0095-tui-set-show-generation-persist-on-confirmation.md)):**
  `/set <key> <value>` (`temperature`/`effort`/`thinking_budget`/`max_tokens`)
  sends `InMsg::SetGeneration` and records a pending persist; the confirming
  `GenerationChanged` commits an atomic write to `agent-generation.yml`, an
  `Error` clears it without writing. `/show` is a no-override `SetGeneration`
  query that renders the current params as a status line. Both are reachable by
  typing `/set …`/`/show` directly, or from the `Ctrl+P` palette (a palette
  pick of `/set` prefills the input with `/set ` since the palette carries no
  trailing args, while `/show` runs immediately).
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
  picked up once created (known v1 limit). Separately, every managed file —
  `grants.yml`, `agent-models.yml`, `agent-generation.yml`, `aux-models.yml`,
  `mcp-tokens.yml`, `extra-roots.yml`, the provider-key `.env`, and
  `config.yml` itself for the surgical `mcp:` edits (`config::mcp_persist`) —
  is advisory-locked across concurrent `skutter` instances via
  `config::lock::with_locked_file` (an `fd-lock` on a sibling `.lock` file): each
  write re-reads the current on-disk state under the lock and merges before
  writing, so a second instance's own concurrent update survives instead of being
  clobbered by a write from stale in-memory state; `write_grants` moved onto the
  shared `atomic_write`. A debounced `notify` firing is *not* on its own proof
  that anything actually changed — on some filesystems a bare content `read()`
  of a watched file (which `reload()`'s own loaders do on every pass) is
  itself observable to `notify`, which without a guard makes the watcher
  perpetually re-trigger itself (reload → reads the watched files → fires
  `notify` → reload → …), surfacing as an unbounded stream of "definitions
  reloaded" notices. `watch::spawn_watcher` guards against this with a **content
  fingerprint restricted to the definition/config files** (agent/skill `*.md`,
  managed `*.yml`/`*.yaml`/`.env`): a `path → (mtime, size, sha256)` map. It is
  **two-stage** — the mtime+size pair is a cheap gate to skip re-hashing an
  untouched file, and the SHA-256 is the actual arbiter of "did the content
  change". A firing reloads (and emits the "definitions reloaded" notice)
  **only if** some tracked file's *hash* differs, so a same-content re-save (an
  editor rewrite, a `touch` that only bumps mtime) is a no-op, and — crucially —
  a write to a **non-definition** file under a watched tree (e.g. a `call`/`bash`
  output artifact under `.entanglement/tmp/`) never enters the map and never
  triggers a reload. That non-definition write was the main source of reload spam
  before the restriction; the file-set filter plus the hash gate (not just
  `stat()`) eliminate it.
- **Physical tool restriction (✅ #116, [ADR-0038](../adr/0038-physical-per-agent-tool-restriction.md)):**
  an agent's `tools` allowlist / `disallowed_tools` denylist masks its tool set —
  `registry ∩ allowlist − denylist` — on *both* sides of the core↔runtime seam,
  orthogonal to `permission` (which grades `Allow`/`Ask`/`Deny` among the tools
  that survive the mask). The mask rides the core `AgentProfile`
  (`tools`/`disallowed_tools` + `advertises_tool`), so it travels per session with
  no new protocol surface. **Mask entries are `*`/`?` wildcard patterns**
  (✅ #537, [ADR-0148](../adr/0148-glob-patterns-in-the-agent-tool-mask.md),
  superseding ADR-0038's "no globbing" consequence): matched by the same
  `glob_match` the #173/#425 permission scopes use, a literal entry degenerating
  to exact equality — so a masked profile can finally hold MCP tools whose
  `mcp__<server>__<tool>` names are unknowable at authoring time and don't even
  exist at parse time (`tools: [read, "mcp__*"]` for all servers,
  `"mcp__docs__*"` for one, `disallowed_tools: ["mcp__*"]` to strip MCP from an
  inherit-all profile; quote `*` entries in YAML). Matching is dynamic at
  advertisement/dispatch time (never parse-time expansion), confined to
  `advertises_tool` — which now delegates to the public associated
  `AgentProfile::mask_allows(tools, disallowed, tool)` so heads holding only a
  mask projection reuse the predicate — and therefore covers the spec filter,
  `tool_masked`'s ancestor clamp (a parent's `"mcp__*"` admits a child's MCP
  call, per profile per link), and the rhai binding mirror from one place.
  `tools: ["*"]` ≡ inherit-all, with one carve-out: plan authorship
  (`plan_tasks::explicitly_allowlists`) stays literal-exact, so a wildcard
  widens the mask without granting `propose_plan`, mirroring `tools: None`.
  Skill `allowed_tools` (#400), permission tool-name keys, and
  `spawnable_agents` deliberately stay exact; built-in `plan`/`explore`/
  `research` masks are unchanged (an MCP tool can be arbitrarily
  write-capable) — enabling MCP
  for them is a user/project-layer override adding a pattern to `tools:`.
  **A session-scoped escape hatch layers on top** (✅ #539,
  [ADR-0149](../adr/0149-per-session-tool-overlay.md); live bash enablement
  folded in, #611,
  [ADR-0163](../adr/0163-live-bash-enablement-is-a-tool-overlay-entry.md)):
  the live **tool overlay** — `InMsg::SetToolOverlay` (trusted-only) replaces
  a per-session list of `ToolOverlayEntry { pattern, allow, deny, arg_pattern? }`
  overriding the profile's mask in both directions (an enable entry makes
  matching tools exist past allowlist *and* denylist; a deny entry withdraws
  even profile-advertised ones — deny > enable > profile), surviving
  `SetAgent` and replay. Core's advertisement filter and `tool_masked` apply
  the same disposition — per chain link, so a parent's overlay covers its
  spawn sub-tree; the grade override reaches the same way (✅ #628,
  `permission::overlay_grade_entry`): the nearest ancestor-chain link
  (session's own first) carrying an enable entry replaces the chain's grade
  with that entry's `Ask` (default)/`Allow`, the latter optionally narrowed
  by `arg_pattern` (ADR-0163) to an argument-scoped `tool(arg_pattern):
  allow` rule instead of a blanket grant — still ceiling-clamped — for both
  the generic dispatch route and the `rhai` `BindingPolicy` snapshot (a
  script's `bash()` binding grades identically to a direct `bash` call).
  When an enable
  entry's name-glob matches a closed, runtime-fixed table of
  lazily-registrable built-ins (`bash` is the only member today, ADR-0163
  §2), it also registers that built-in into the process-wide `SharedRegistry`
  on demand — the fold-in of the pre-ADR-0163 bespoke `BashEnable`/
  `BashDisable` pair. The TUI drives it via `/enable mcp <server>` /
  `/enable tool <name>` [`--allow [<pattern>]`] and `/disable` (upserts a
  deny; bare = reset) — `/enable tool bash [--allow [<pattern>]]` is now the
  one command surface for live bash enablement too, superseding the old
  `/bash on|off` — the bare-`/enable`
  session-tools checklist dialog (checkboxes over the full roster seeded
  from effective availability; `Enter` submits the overlay as a diff
  against the profile), and the `/mcp` panel's `e`/`d` keys on the
  highlighted server; see the protocol doc for the wire shape. **(a) Advertisement:** core's turn loop (`run_round`) filters both
  `EngineConfig.tool_specs` and the active profile's `profile_tool_specs` entry by
  the mask — a masked tool's schema never reaches the model. `propose_plan`/
  `update_tasks` are ordinary runtime state/orchestration tools now (✅ #231/#513,
  below): they ride those specs and this mask like any host tool, no
  plan-authority special casing in core. **Per-session base specs (✅ #308, [ADR-0076](../adr/0076-per-session-dynamic-tool-specs.md)):**
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
  the `agent`/`agent_send`/`poll`/`ask_user` interceptions and permission —
  so a hallucinated masked call is a hard boundary, and the mask **intersects down
  the ancestor chain** (a child never gains a tool an ancestor lacked, mirroring
  ADR-0024's privilege ceiling). A sibling `tool_mask_source`
  ([ADR-0159](../adr/0159-plan-mask-widened-for-explore-delegation.md), #597)
  runs the identical walk but returns *which* link (`Option<SessionId>`, the
  session itself or the clamping ancestor) did the masking — `tool_masked` is
  now a thin `.is_some()` wrapper over it — so the runtime's refusal message
  can say "restricted by its own profile" vs "restricted by ancestor agent
  `<name>`'s profile" instead of a blanket, unattributed "restricted by
  profile". `explore` is the reference read-only agent: `tools: [read, glob,
  grep, call, bash, poll, rhai]` — no `edit`/`write`/`agent`, but
  `call`/`bash`/`rhai` are graded `Ask` (ADR-0137) rather than
  masked out, so a research child isn't hard-blocked from shell access, only
  approval-gated on it. `poll` rides along with `bash`/`call` (#615/#605/#606) so a
  background job started via `bash{background: true}` (or `call{background: true}`) is actually
  readable, not a write-only dead end — `poll` itself is intercepted before
  permission resolution, so it carries no grade of its own.
  It is also the **default** `agent` target (`DEFAULT_SUBAGENT` in
  `entanglement_runtime::subagent`) when the caller omits `agent` — the safe
  choice for an unscoped delegation. But it is also, by design, the *only*
  built-in `mode: subagent` leaf with an empty allowlist: a spawned agent that
  needs to reproduce, fix, and *verify* a bug (compile, run tests) has nothing
  spawnable to reach for. `debug` closes that gap: a second `mode: subagent`
  leaf carrying `build`'s own permission (`default: allow`, no tool mask, so it
  inherits `build`'s allow-everything/plan-authority-closed shape exactly) —
  full read/write/execute, still never selected unless the caller names it
  explicitly (`{"agent": "debug", ...}`).
- **Startup warning for stale tool names in a mask or permission rule (✅ #623,
  [ADR-0166](../adr/0166-migration-note-and-startup-warning-for-stale-tool-names.md)):**
  a `tools`/`disallowed_tools` mask entry (above) or a `permission:` rule key
  (#418/#425 above) that matches nothing doesn't error — it just silently stops
  masking/grading anything, which is exactly what happens to a config written
  against a tool a later rename removed (e.g. #605/#606's `bash_output`/
  `agent_poll`/`agent_spawn` → `poll`/`agent`; ADR-0033's "renames are free"
  covers session logs, not these). `tool_names::is_recognized_mask_entry`
  checks each entry against a fixed, compile-time vocabulary of every literal
  tool name in the codebase (deliberately independent of what's registered
  *this run* — `bash`/`rhai` are env/feature-gated and MCP tools connect after
  profiles load, so deriving "known" from the live `ToolRegistry` would
  false-positive an inactive-but-real tool), a capability key, a `*`/`?` glob
  (ADR-0148), or an `mcp__*` name. `agents::warn_unrecognized_mask_entries`
  runs on every parsed profile (every layer, including the built-ins) and
  `tracing::warn!`s each miss — visible by default (`warn` is the fallback log
  level) but never a load error, since an unrecognized entry is a heuristic
  hit, not a certainty. Persisted grants (`grants.yml`) are deliberately left
  unchecked: a stale grant key is inert by construction (exact-match against a
  live call), so nothing degrades quietly there the way a mask/rule does.
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
  the profile's current effective mask via `AgentProfile::mask_allows` itself
  (#537 — a wildcard entry shows its matches checked; saving still emits the
  concrete checked set, so a glob does not survive the checklist round-trip —
  hand-edit the frontmatter to keep a live pattern); `Space` toggles, `Enter`
  saves + records a transcript status line, `Esc` discards. The write applies
  on the next restart — there is no live registry reload yet (a separate watcher
  issue); `skutter inspect agents` still reports the winning layer and what it
  shadowed, so provenance stays visible.
- **Per-profile spawn control (✅ #119, [ADR-0040](../adr/0040-per-profile-spawn-control.md)):**
  spawning is a per-profile capability declared in the definition — *whether* a
  profile may spawn (`can_spawn`, default: open for primaries/`all`, closed for a
  `subagent` leaf) and *which* profiles it may spawn (`spawnable_agents`, omitted ⇒
  any spawnable target). Both ride the core `AgentProfile` with helpers
  (`may_spawn`, `spawn_target_allowed`, `spawnable_as_subagent`); core = semantics,
  runtime = enforcement. **Structural half:** the `agent`
  spec moves out of the shared `tool_specs` into
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
- **Task state tool (✅ #231, [ADR-0049](../adr/0049-plan-task-tools-as-runtime-state-tools.md)):**
  `update_tasks` is a **runtime** state tool, not a core built-in. It replaces
  the session's *display* task outline; the runtime executor emits the
  `OutEvent::TaskList` snapshot (a fresh per-session seq, #157) and acks the
  model — the engine holds no task state. It round-trips via `ToolExec`/
  `ToolResult` and resolves through the **ordinary** `Allow`/`Ask`/`Deny` path
  + #116 mask, with **no** special casing (it falls through `tool_runner`'s
  generic `dispatch`; `run_and_reply` emits the snapshot instead of hitting
  the host `ToolRegistry`, since it touches no host resource). This closes
  **#175**: a read-only `explore` has `update_tasks` outside its allowlist
  (mask refusal) *and* permission-denied, so it can't mutate task state.
  `update_tasks` rides the shared `tool_specs` (general bookkeeping, no
  cross-agent authority) — unlike plan authorship below, every unmasked
  profile advertises it.
- **One plan tool — `propose_plan` (✅ #141/#513, [ADR-0042](../adr/0042-plan-acceptance-via-propose-plan-approval-roundtrip.md),
  amended by [ADR-0138](../adr/0138-sponsored-build-child-and-propose-plan-cycle.md)
  and [ADR-0145](../adr/0145-one-plan-tool-file-backed-plans-and-blocking-review-loop.md)):**
  `update_plan` is **gone** — `propose_plan(content: Option<String>, path:
  Option<String>)` is the sole plan-authorship tool, still gated by the same
  default-closed explicit-allowlist membership ADR-0049 established
  (`plan_tasks::explicitly_allowlists`, now generic over any tool name).
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
  binding, e.g. a user-seeded file, #514) is never stale.
  Acceptance rides the **existing tool-approval round-trip** (#59): the
  executor (`propose_plan.rs`) intercepts it on `ToolExec` after the #116
  mask check (same interception family as `ask_user`) and **force-parks it
  on the `Ask` path unconditionally, every phase** — a permission profile can
  never `Allow` it, since user approval *is* the tool's semantics. **Approve**
  → spawns a **sponsored** `build` child of the plan session (ADR-0138): the
  `SpawnGuard` mutation (sponsor check + `record_sponsored_start`) happens in
  the tool executor's single-threaded loop before the detached task, so the
  child is marked a permission root — its own profile stands, no ancestor
  clamp. The plan text reaches the child via `wrap_plan` as its first prompt;
  the child also receives an `OutEvent::Plan` snapshot so its outline renders
  the plan. The plan session parks on `WaitingAgent` (ADR-0139) while the
  build runs — this whole task (the Ask-wait *and* the blocking build-wait)
  is now registered with `crate::cancel::CancelRegistry`, so a `Stop` on the
  plan session detaches (aborts the wait; the sponsored child, an
  independent session, keeps running untouched) instead of being ignored
  once past the Ask phase; a head wanting the child stopped too just sends
  it a second, ordinary `Stop` (cascade — no new protocol). The build's
  **full final report** (prefixed with the plan file's location) folds back
  as the `propose_plan` tool result, so the plan agent can review it, update
  the plan file via `write`/`edit`, and `propose_plan` the next phase —
  a multi-phase plan → build → review loop. **Reject + reason** → the
  existing fold-back (`tool \`propose_plan\` rejected (plan file: <path>):
  <reason>`); the model revises and re-proposes in the same turn. One-shot
  heads (`run`/`pipe`) can't park an interactive approval, so they
  auto-reject with a "non-interactive head" reason.
  Built-in `plan` names only `propose_plan` in its allowlist (no `update_plan`
  entry — see #524's carve-out below) and stays physically read-only apart
  from one carve-out (#524,
  [ADR-0142](../adr/0142-trusted-scratch-dir-and-plans-folder-carve-outs.md)):
  `tools: [read, glob, grep, agent, agent_send, poll, ask_user,
  load_skill, propose_plan, write, edit, call, bash]` unmasks `write`/`edit`,
  but its permission rules (`write: deny` plus the argument-scoped
  `write(.entanglement/plans/*.md): allow`, fanned out to `edit`/`apply_patch`
  by the `write` capability key, #418) grade every write outside
  `.entanglement/plans/*.md` as `Deny` — the opencode-style plans-folder
  exception `propose_plan`'s `content` mode writes into, everything else
  stays physically unreachable. `call`/`bash` are on the mask too
  ([ADR-0159](../adr/0159-plan-mask-widened-for-explore-delegation.md), #597):
  not for `plan` to run shell itself, but so the ancestor-clamp intersection
  below doesn't erase them from an `explore` child it delegates research to —
  `explore.md` grants its own `call`/`bash` at `Ask`, and without `plan`'s own
  mask also carrying them the intersection would silently drop both regardless
  of what the child's own definition allows. The mask alone isn't enough,
  though: `call` is a `MULTI_GROUP` tool (#418, ADR-0114) whose *bare*
  (no-argument) grade is always the least-privileged of every bare capability
  grade in the profile — `plan.md`'s own `write: deny` pulls it to `Deny`
  regardless of any bare `call: ask` written alongside it, which would still
  clamp a real child dispatch. `plan.md`'s permission block instead grades
  `call(*): ask` — an arg-scoped capability key, which ADR-0114 lets refine
  `call`'s multi-group floor by command pattern — fanning out to both
  `call(*)`/`bash(*)`; a real invocation always carries its command as the
  argument, so this is what actually governs dispatch, while the coarse
  no-argument view legitimately stays `Deny`. A clamp its spawned children
  inherit.
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
  same embedder-owned snapshot-cache pattern; no protocol/wire change. The
  runtime wires this seam itself (#566) to keep the env block's baked `Date:`
  line accurate: `entanglement_runtime::env_date::date_resolver()` patches just
  that line to today's date and returns `None` — falling back to the unmodified
  baked prompt — whenever the date hasn't actually changed, so the prompt stays
  byte-identical, and therefore provider-cache-safe, for as long as it's
  accurate.
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
  (a *skill-scoped* tool mask, **enforced** while the skill is active, #400 —
  distinct from the #116 agent tool mask, see below). Each `SkillMeta` resolves its
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
  (`agent`/`agent_send`/`ask_user`/`poll`), it **reads the filesystem**, so it is a
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
  authorship trail stays honest.
- **Skill-scoped `allowed_tools` enforcement (✅ #400, [ADR-0106](../adr/0106-skill-scoped-allowed-tools-enforcement.md)):**
  a `load_skill` result's `skill_id:` header is the provenance signal — on a
  successful load, `tool_runner` looks the skill up in the live
  `SkillRegistry` and records `ActiveSkill { skill_id, allowed_tools }` for
  that **session** (`entanglement_runtime::permission::skill_masked`), not a
  core-protocol field on `ToolCall`/`ToolExec` (avoiding the "protocol change
  with no behaviour" ADR-0037 flagged before enforcement existed). Checked in
  `ToolExec` handling strictly *after* the #116 agent mask (`tool_masked`) — a
  tool must survive both — with **no exemption for `load_skill` itself**: a
  skill whose `allowed_tools` omits it blocks switching skills mid-turn.
  Unlike the agent mask, the skill mask does **not** clamp down the
  ancestor/spawn chain — a skill's scope is the loading session's current
  turn, not an inheritable profile trait, so a spawned child starts unmasked
  by a parent's loaded skill. It clears on that session's next `Done` (or the
  session ending) — matching "for the duration" without an explicit unload
  tool. `OutEvent::SkillActive { session, seq, skill_id: Option<String>,
  allowed_tools: Option<Vec<String>> }` mirrors `FileChange`'s shape as the
  wire-facing posture surface (a fresh per-session seq, no `Session::replay`
  fold, persisted for free); the stdio `run --format text` head and the TUI
  transcript both render it as a one-line notice.
- **The skill mask reaches `rhai` bindings too (✅ #477, [ADR-0129](../adr/0129-thread-the-skill-mask-into-rhai-binding-resolution.md),
  amending ADR-0106):** `Intercept::Rhai`'s `BindingPolicy` snapshot (§8) now
  folds in `skill_masked` alongside the agent mask, so a binding
  (`read`/`glob`/`grep`/`edit`/`write`/`call`/`bash`) the active skill's
  `allowed_tools` excludes refuses with the identical message a direct call
  gets ("not available while skill `X` is active…"), checked strictly after
  the agent mask, same ordering as generic dispatch. `BindingPolicy::capture`
  takes the session's `active_skill` map as a one-time **snapshot** rather
  than a live read — sound because `load_skill` is not itself a binding, so
  nothing inside a running script can activate or change a skill mid-run.
  Clears on the session's next `Done`, exactly as for generic dispatch, since
  it is the same `ActiveSkill` map both routes read.
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
ps **fail-closed**: an unseen session resolves
  to `Deny` (`permission_for`) and to *masked* (`tool_masked`) — degraded but safe.
