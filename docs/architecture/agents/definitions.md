# Agents & permissions §3 — Agent definitions, built-in modes, pinning & live reload

> Part of the [agents & permissions module](../agents-and-permissions.md) of the [architecture overview](../../architecture.md).

- **File-defined (✅ #112, [ADR-0034](../../adr/0034-file-based-agent-definitions.md),
  roster collapsed by [ADR-0207](../../adr/0207-permission-modes-replace-agent-borne-authority.md)
  §1):** agents are markdown files with YAML frontmatter (the config bundle)
  + a body (the system prompt), discovered at startup by the **runtime**
  (`entanglement_runtime::agents::load_registry`) into a `AgentCatalog`.
  Three layers, later wins on a `name` collision: embedded built-ins —
  **`general`, `plan`, `debug`** — shipped as `include_str!` `.md` and parsed
  through the *same* loader) < user (`~/.claude/agents/*.md` then
  `${config_dir}/entanglement/agents/*.md`) < project (`.claude/agents` then
  `.agents/agents` then `<root>/.entanglement/agents/*.md`). Editing a
  built-in = dropping a same-`name` file in a higher layer — one mechanism
  for all three, same defaults+override shape as the provider catalog
  (#118). A malformed *native* file is a loud error; the cross-vendor dirs
  parse leniently — only `name`+`description` read, a malformed file warned
  and skipped ([ADR-0074](../../adr/0074-cross-vendor-skill-and-agent-discovery.md)).
  `build`, `explore` and `research` are **retired as agent names** — their
  postures are permission modes now, not personas — so a session log naming
  one of them (and absent from the catalog) resumes, per session, under the
  last catalog-known agent its log named, else `general`, with a one-line
  note; a retired `explore`/`research` session stays read-only, resuming in
  `research` mode unless `--mode` is explicit, and that narrowing rides the
  replay so a re-offered call is already graded under it
  ([ADR-0214](../../adr/0214-retired-agent-logs-resume-as-general.md), amended
  by [ADR-0215](../../adr/0215-resume-fallback-per-session-and-read-only-narrowing.md);
  [ADR-0216](../../adr/0216-resume-modes-ride-the-replay.md); see
  [heads & persistence](../heads-and-persistence.md)); `general` (the `build`
  body, unchanged, now the default and default spawn target) and `debug`
  (the fix-and-verify persona; what it may run is the session's mode) are
  the only two carried forward, plus `plan`. A
  definition naming any of the now-deleted authority keys (`tools`,
  `disallowed_tools`, `permission`, `sandbox`, `can_spawn`,
  `spawnable_agents`, `mode`) fails to parse like any other unrecognized key
  — `deny_unknown_fields` catches a stale file the same way as a typo (a
  strict-layer file gets one self-heal attempt first: backed up to
  `<file>.bak`, rewritten without the retired keys, with a warning naming
  the closest replacement mode inferred from what the dropped rules
  allowed). **Any agent may be a session root or a spawn target now** — the
  old `primary`/`subagent`/`all` distinction goes with the deleted `mode`
  field, since there is no more per-agent authority to gate a spawn boundary
  on (spawning is bounded by the session's permission mode instead, [spawn, tasks & plans](spawn-tasks-plans.md)).
  Plan authorship (`propose_plan`, ✅ #231/#513, [spawn, tasks & plans](spawn-tasks-plans.md)) and the plan-approval
  mode switch ([spawn, tasks & plans](spawn-tasks-plans.md)) complete the picture. The built-ins are defined
  **once**, here as markdown (#201): core carries only the `general` profile
  its `resolve()` fallback needs (`DEFAULT_AGENT = "general"`; it can't
  parse frontmatter, so it holds no `plan`/`debug` copy to drift from these
  files). Exec-tool choice (`bash` vs `call` vs `rhai`) is deliberately
  **not** in any body: it lives once, in the `bash` tool description
  ([gates & host tools](../gates-and-host-tools.md), #740). Embedders using core directly get that single fallback via
  `AgentCatalog::new()`; the runtime rebuilds the full set from the
  embedded markdown (`entanglement_runtime::agents::built_in_registry`). Add
  your own with `AgentCatalog::insert`.
- **The four built-in modes, at a glance** — what a session can actually do,
  independent of which agent runs it. Source of truth:
  `entanglement-runtime/src/mode/builtin/*.yml`
  ([ADR-0207](../../adr/0207-permission-modes-replace-agent-borne-authority.md)
  §2/§4/§11; `skutter inspect modes [name]` prints the live, tuned table).

  | Mode | default | notable rules | run limits |
  | --- | --- | --- | --- |
  | `research` | `prompt` | `deny: [write, plan]`; `allow: [read]` + the shared read-only exec set; `prompt` + the shared api-mutating escalation set — read-only investigation, no plan authorship | `max_depth: 2`, `max_agents: 4` |
  | `plan` | `prompt` | `deny: [write]`; `allow: [read, plan, "write(.entanglement/plans/*.md)"]` + the shared read-only exec set; `prompt` + the shared api-mutating escalation set — the plans-folder carve-out out-ranks the class deny by longest match; anything outside the shared set falls through to `prompt` | `max_depth: 2`, `max_agents: 4` |
  | `build` (default) | `prompt` — a deliberate change from the old `build` agent's `default: allow` | `deny: [plan, "bash(rm -rf /*)", "bash(rm -rf ~*)", "bash(git push --force*)", "bash(git push -f*)", "bash(git reset --hard*)", "bash(git clean -f*)", "bash(git branch -D*)"]`; `allow: [read, write, exec]`; `prompt: [rhai, "bash(git push*)", gh/glab issue-or-PR/MR "create"/"merge", "bash(npm publish*)", "bash(cargo publish*)"]` + the shared api-mutating escalation set — broad allow, a short destructive-command deny list stays absolute regardless, and the network-mutating `prompt` entries out-rank the bare `exec` allow the same way; plan authorship denied (revise via `/mode plan`, not a mid-build overwrite); a `rhai` launch asks (the bare `rhai` rule names the tool, so it outranks the class allows) | `max_depth: 4`, `max_agents: 8` |
  | `auto` | `allow` — everything runs but what the deny and prompt lists catch ([ADR-0228](../../adr/0228-rhai-path-and-args-skill-tools-frontmatter-removed.md), amending [ADR-0209](../../adr/0209-auto-mode-defaults-to-prompt-not-deny.md)); `question_timeout` bounds whatever asks | `deny: [plan, "bash(rm -rf /*)", "bash(rm -rf ~*)", "bash(git push*)", "bash(git reset --hard*)", "bash(git clean -f*)", "bash(git branch -D*)", gh/glab issue-or-PR/MR "create"/"merge", every mutating `gh api`/`glab api` spelling, "bash(npm publish*)", "bash(cargo publish*)"]`; `allow: [read, write, "bash(cargo *)", "bash(make *)"]` + the shared read-only exec set (documentation now — the default already allows); an unenumerated command, tool, MCP call, `web_fetch` or `rhai` launch runs. What still asks: its own `prompt` rules (below), what no rule grades — the escape-root gate and a credentialed `http()` host — the write floor, and any `prompt:` rule a user adds. Unattended — no head watches the session or an ancestor ([ADR-0247](../../adr/0247-attended-sessions-wait-for-their-approver.md)) — such a rule's call collapses to a refusal first and parks a bounded approval on a repeat graded by the same rule (`run_limits`); an escape-root card parks bounded at once; a credentialed host is refused unless an explicit user-layer `http(...)` allow names it — every park expiring as a denial after 60s. Attended (the TUI, a `serve` client owning the root), it parks at once like `build` and waits for the answer. The explicit `deny` list — destructive and network-mutating commands — stays absolute. `prompt` rules ask for destructive families that must not run silently (`gh`/`glab` `* delete`, `cargo yank`, `npm unpublish`, `docker push`, recursive `rm`). The trusted-script / skill-file write floor applies here too | `max_depth: 2`, `max_agents: 4`, `question_timeout: 60` |

  None of the four declares `sandbox` — ADR-0104 shipped bubblewrap
  confinement as an **opt-in** (`ENTANGLEMENT_SANDBOX=bwrap`); declaring it
  in every built-in would make it mandatory, which breaks ordinary work (a
  confined `git push` fails on `/etc/ssh/ssh_config.d/` ownership once the
  namespace changes what ssh sees). The mechanism still exists — a mode may
  declare `sandbox`, and `ENTANGLEMENT_SANDBOX` may only tighten it (§6) —
  the shipped defaults just leave it off.

  **The shared read-only exec set** (`entanglement-runtime/src/mode/builtin/readonly_exec.yml`,
  spliced into `research`/`plan`/`auto` at parse time; `build` needs none of
  it since it class-allows `exec` outright) is one list instead of the
  per-mode copies that used to drift — inspection (`ls`/`find`/`stat`/…),
  reading (`cat`/`head`/`tail`/…), search (`grep`/`rg`/`fd`/`ag`), text
  tools (`sed`/`awk`/`sort`/`diff`/`jq`/…), read-only `git` subcommands
  enumerated one by one (`status`/`log`/`diff`/`show`/`blame`/`branch`
  bare-or-`--list`/`tag` bare-or-`-l`/`remote`/`describe`/`rev-parse`/
  `ls-files`/`stash list`/`shortlog`/`config --get*`) rather than a broad
  `git *`, which would also open `push`/`reset`/`checkout`/`clean`/`rebase`,
  and read-only `gh`/`glab` subcommands the same way (`issue|pr|mr list`/
  `view`, `pr diff`/`checks`/`status`, `repo view`, `run|ci list`/`view`,
  `release list`/`view`, `label list`, `workflow list`, `search`,
  `auth status`) plus the broad `gh api *`/`glab api *` (most API calls are
  reads). Deliberately excluded: `env`/`printenv`/`export`/`set` (they would
  dump provider API keys into the transcript). Deliberately **not**
  deny-listed: the write-capable forms these patterns still reach
  (`find -delete`, `sed -i`, `awk > file`) — an accepted trade-off, not an
  oversight.

  **The shared api-mutating escalation set**
  (`entanglement-runtime/src/mode/builtin/readonly_exec_prompt.yml`, spliced
  into `research`/`plan`/`build`'s `prompt` list) holds the longer, more
  specific rules that out-rank a broader allow for one narrow case: a
  mutating `gh api`/`glab api` call (`-X`/`--method` `POST`/`PUT`/`PATCH`/
  `DELETE`, both spellings). Without it, `gh api -X DELETE ...` would ride
  the broad `gh api *` allow (`research`/`plan`) or the bare `exec` class
  allow (`build`) straight through. `auto` doesn't splice this list in —
  a `prompt` nobody answers just times out — it carries the same commands
  as explicit `deny` entries in its own YAML instead, alongside the rest of
  its network-mutating deny list (`gh`/`glab` issue-or-PR/MR `create`/
  `merge`, `npm publish`, `cargo publish`); `build` carries that same
  non-`api` list as its own `prompt` entries, since there a human is present
  to ask.

  Four cross-cutting facts complete the picture: **(1)** `bash` and `call`
  are both registered at startup unconditionally
  ([ADR-0195](../../adr/0195-bash-is-the-default-exec-and-curated-read-only-rules.md))
  and **share one rule set** — a rule written for either grades both. **(2)**
  skills are **additive-only** — the skill `allowed_tools` mask (ADR-0106) is
  removed ([ADR-0194](../../adr/0194-skills-are-additive-only.md)): a skill adds
  capabilities and never restricts the session's tool set, so loading one
  mid-turn can no longer disarm editing (the #554 footgun); the control is the
  session's permission mode alone. **(3)** the user-config `permissions:`
  ceiling defaults to `default: allow` — a no-op clamp until the user tightens
  it (#172) — and grades through the identical longest-match engine as a real
  mode (`Mode::from_permission_profile`), so it still clamps least-privilege
  over every mode's own grade. **(4)** advertisement is universal and
  session-stable regardless of mode — every session sees the same tool specs
  (§permission modes in [modes](modes.md)); a `Deny`'d tool is still advertised, and a call
  to it is declined at dispatch with the mode's own attributed refusal, never
  hidden from the model.
- **Per-profile model pinning (✅ #323, [ADR-0081](../../adr/0081-per-profile-model-pinning-and-rebind-on-set-agent.md),
  narrowed by [ADR-0207](../../adr/0207-permission-modes-replace-agent-borne-authority.md)
  §9 — `SetModel`/`SetGeneration` themselves survive; only the `SetAgent`
  rebind trigger is gone):**
  a profile's frontmatter may set `provider:` beside `model:`. Both set = a
  **model pin** (`Agent::model_pin()`): starting a session under the
  profile re-binds the session's whole backend to that `(provider, model)` —
  through the same `model_resolver` seam a live `/model` (`SetModel`) switch
  uses ([ADR-0063](../../adr/0063-realtime-model-provider-switch.md)) — so a
  `plan` profile can run a different provider from `general`, and a spawned
  sub-agent pins its own cheaper model. `model:` **without** `provider:`
  keeps the legacy request-level fallback (`req.model` only, **no** rebind);
  `provider:` **without** `model:` is a loud load error. The rebind lives in
  **core's session-start path only** (`Holly::send`'s `SessionStarted`
  handling, `session/start.rs`) — with `SetAgent` gone, an agent is chosen exactly
  once (`--agent` / `config.yml` `agent:` / a spawn's own `agent` argument),
  so this is the *only* locus left to bind at; a resumed session already
  re-bound from its own `ModelChanged` log skips it. **Precedence:**
  per-session memory (a `/model` choice made while the profile was active,
  `Session.profile_models`) **>** the static frontmatter pin **>** keep the
  current binding (a pin-less profile with no memory changes nothing — no
  `ModelChanged`). A resolver failure warns and keeps the startup default; the
  `AgentChanged`/`SessionStarted` still succeed.
  **Persistence:** picking a model via the TUI `/model` picker while a profile is
  active writes the pin to a **managed** `<state dir>/agent-models.yml`
  (`ENTANGLEMENT_STATE_DIR`, ADR-0213; shape `agents: { general: { provider,
  model } }`), overlaid onto matching profiles at startup — **persisted file >
  frontmatter**. Managed (not layered) like the grants + env files: the runtime
  rewrites it, so it stays out of the hand-edited `config.yml`. Missing/malformed
  → empty + warn (fail-open); a write failure is logged, never fatal
  (`entanglement_runtime::config::agent_models`).
- **Per-profile generation-parameter persistence (✅ #374, [ADR-0094](../../adr/0094-reasoning-effort-and-per-profile-generation-persistence.md)):**
  mirrors the model pin above — same three-tier precedence (session memory >
  persisted > current binding), applied at the same **session-start** locus —
  but through a **separate** seam:
  `EngineConfig.generation_resolver: Option<GenerationResolver>`, a
  `Fn(&str) -> Option<GenerationParams>` keyed by profile *name* rather than a
  field baked into `Agent`. `GenerationParams` carries
  `temperature: Option<f32>`, which has no total `Eq`, so it cannot join
  `Agent`'s `PartialEq + Eq` derive the way the pin's `provider`/`model`
  fields do — the resolver indirection is the price of keeping
  `GenerationParams` a plain `Copy` value type. `Session.profile_generation`
  (session memory) and the resolver's return (the persisted tier) are both
  **full** `GenerationParams` snapshots, applied by direct assignment — unlike
  the partial-merge `GenerationParams::apply_overrides` a live
  `InMsg::SetGeneration` itself uses. **Persistence:** the runtime's
  `entanglement_runtime::config::agent_generation::AgentGenerationStore`
  (`<state dir>/agent-generation.yml`, sibling of `agent-models.yml`) has the
  same `load`/`get`/`set`/`reload` shape and the same fail-open/locked-write
  behavior — but **no** `apply(&mut AgentCatalog)`: there's nothing on
  `Agent` to overlay, so `AgentGenerationStore::resolver(store)` builds
  the `GenerationResolver` closure directly (resolved fresh on every call, so a
  `set`/`reload` is visible without rebuilding it). **TUI surface (✅ #376,
  [ADR-0095](../../adr/0095-tui-set-show-generation-persist-on-confirmation.md)):**
  `/set <key> <value>` (`temperature`/`effort`/`thinking_budget`/`max_tokens`)
  sends `InMsg::SetGeneration` and records a pending persist; the confirming
  `GenerationChanged` commits an atomic write to `agent-generation.yml`, an
  `Error` clears it without writing. `/show` is a no-override `SetGeneration`
  query that renders the current params as a status line. Both are reachable by
  typing `/set …`/`/show` directly, or from the `Ctrl+P` palette (a palette
  pick of `/set` prefills the input with `/set ` since the palette carries no
  trailing args, while `/show` runs immediately).
- **No tool-advertising mode** ([ADR-0226](../../adr/0226-one-advertised-surface-mcp-envelope-registered-is-available.md)): ADR-0196's `Full | ToolSearch` knob
  (orthogonal to the permission mode) is removed along with its
  `config.yml`/catalog keys — one surface, pinned per session, identical on
  every wire; a stale `tool_advertising:` warns once and is ignored.
- **Live reload + managed-file locking (✅ #329, [ADR-0084](../../adr/0084-runtime-live-reload-and-managed-file-locking.md)):**
  a runtime-side `watch.rs` watches every resolvable agent/skill dir plus
  `${config_dir}/entanglement/`, the managed-state dir (`ENTANGLEMENT_STATE_DIR`)
  and `<root>/.entanglement/` (`notify`, debounced
  500ms so a burst of edits collapses into one reload) and, on change, re-runs the
  skill + agent loaders and swaps the result into **runtime-held mirrors**
  (`watch::LiveDefinitions { agents, skills, commands, agent_models, grants,
  mcp_capabilities, file_backups }`, built via `LiveDefinitions::new` +
  `with_file_backups`; the config's `undo:` block is re-applied onto
  `file_backups` on every change, #787) — never
  core's `EngineConfig.profiles`, which stays pinned for the process lifetime (the
  [ADR-0081](../../adr/0081-per-profile-model-pinning-and-rebind-on-set-agent.md)
  "live registry mutation" rejection applies identically here). Permission
  resolution (`tool_runner`'s `ToolExec` self-heal), `load_skill`, and the TUI's
  `/agent` picker all read through these live handles, so a
  definitions edit lands for the *next* new session or spawn — a
  turn already in flight keeps its already-resolved system prompt unchanged.
  Every content change sends the head a `watch::ReloadNotice` —
  `Reloaded(summary)`, or `Failed(error)` when a malformed definition kept
  the previous state — and the TUI re-applies its `tui:` block on either, so
  a bad agent file never blocks a `config.yml` edit (#785); each firing
  shows one notice — a toast on success, else a single warning naming every
  failure, a malformed `config.yml` included (#813). The config warnings each
  load re-runs (project ceiling overrides, stale `modes:` tool names) log
  only findings new since that root's previous load (`config::warn_once`),
  so a fixed-then-reintroduced finding warns again and a second root in one
  process gets its own warnings.
  The permission-mode table is **not** part of this live reload: like the old
  `EngineConfig.profiles`, `ModeTable` is built once at startup
  (`services.rs`, built-ins + `config.yml` `modes:` tuning validated against the
  live `ToolRegistry`) and held pinned for the process lifetime — a `modes:`
  edit needs a restart, unlike an agent/skill file. A directory that doesn't exist at watch-start needs a restart to be
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
  a skill's `lib.rhai` and `rhai/modules/**/*.rhai` —
  [ADR-0248](../../adr/0248-importable-rhai-library-modules.md) — managed
  `*.yml`/`*.yaml`/`.env`): a `path → (mtime, size, sha256)` map. It is
  **two-stage** — the mtime+size pair is a cheap gate to skip re-hashing an
  untouched file, and the SHA-256 is the actual arbiter of "did the content
  change". A firing reloads (and emits the "definitions reloaded" notice)
  **only if** some tracked file's *hash* differs, so a same-content re-save (an
  editor rewrite, a `touch` that only bumps mtime) is a no-op, and — crucially —
  a write to a **non-definition** file under a watched tree (e.g. a `call`/`bash`
  output artifact under `.entanglement/tmp/`) never enters the map and never
  triggers a reload. That non-definition write was the main source of reload spam
  before the restriction; the file-set filter plus the hash gate (not just
  `stat()`) eliminate it. `.entanglement/plans/` is deliberately **not** one of
  the watched trees here — a plan-file edit is a different reload action
  entirely (nothing to reload, just a live notice), covered by its own
  dedicated watch (`plan_watch.rs`, #627) that reuses only the
  `spawn_debounced_watcher` primitive (`entanglement-runtime/src/watch.rs`),
  not `LiveDefinitions`; see [engine §5 sub-agents &
  plans](../engine/subagents-and-plans.md) for that watcher's own detail.

---

Previous: [§3 — Mode tuning, approvals, grants & the permission ceiling](approvals-and-grants.md) · Next: [§3 — Advertisement, enforcement, overlay & discovery](advertisement-and-enforcement.md)
