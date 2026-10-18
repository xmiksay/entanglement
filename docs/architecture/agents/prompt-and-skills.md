# Agents & permissions §3 — System prompt, skills, slash commands & dispatch

> Part of the [agents & permissions module](../agents-and-permissions.md) of the [architecture overview](../../architecture.md).

- **System-prompt assembly (✅ #113, [ADR-0035](../../adr/0035-deterministic-system-prompt-assembly.md)):**
  the definition body is *not* stored as the raw `system_prompt`. As each profile
  is loaded, `entanglement_runtime::system_prompt::assemble` composes up to five
  ordered, optional parts — **shared preamble** (safety/tool-use/output invariants
  applied to *every* agent) + **agent body** + **project brief** (the standard
  `AGENTS.md` / `.agents/AGENTS.md` / `.claude/CLAUDE.md` / `CLAUDE.md`, first
  found wins — no bespoke file — only when the frontmatter sets
  `include_brief: true`) + **generated env block** (cwd/root, platform, date —
  never model-guessed) + **tool conventions** (one static line,
  `system_prompt::TOOL_CONVENTIONS`: call the advertised tools directly, MCP
  through `mcp(server, tool, args)`, `rhai` for anything else — the skill
  roster that used to sit here moved to `list_skills`, [ADR-0226](../../adr/0226-one-advertised-surface-mcp-envelope-registered-is-available.md)) + **preloaded skill bodies** (frontmatter
  `skills: [name, …]`, ✅ #117, below). Inputs come from `PromptContext::load(root)`
  (preamble overridable via `ENTANGLEMENT_PREAMBLE_FILE`; brief via
  `ENTANGLEMENT_BRIEF_FILE`). A **subagent** gets `preamble + body (+ brief)` +
  any preloaded bodies — and never the parent's assembled
  prompt (each agent is composed from *its own* body + `include_brief` flag).
  Composition is a pure, unit-tested harness function baked into
  `Agent.system_prompt` at load time, so session start / spawn
  both read the finished prompt and core stays a verbatim pass-through into
  `LlmRequest.system` — there is no more live `SetAgent` to re-read it on
  ([ADR-0207](../../adr/0207-permission-modes-replace-agent-borne-authority.md)
  §9). `list_skills` reads the skill registry (✅ #114, below) and lists every
  discoverable skill regardless of session mode.
  **Per-turn prompt override (✅ #310, [ADR-0078](../../adr/0078-per-turn-dynamic-system-prompt.md)):**
  an optional `EngineConfig.system_prompt_resolver: Option<Arc<dyn Fn(&SessionId,
  &Agent) -> Option<String> + Send + Sync>>` (type alias
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
  runtime wires this seam itself (#566) through `system_prompt_mode::resolver`,
  which pins the env block's baked `Date:` line **per session** at its first
  resolution (`env_date::EnvDatePins`, forgotten on session end): a session
  keeps its start date for its whole life, even across midnight, so its system
  prompt stays byte-identical and provider-cache-safe; a new session gets
  today's date ([ADR-0202](../../adr/0202-prompt-cache-discipline-anchors-deferral-replay-compaction-date.md)).
- **Skill discovery + registry (✅ #114, [ADR-0036](../../adr/0036-skill-discovery-and-registry.md)):**
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
  read only `name`+`description`, ignoring every other key — Claude's
  `disable-model-invocation` and `allowed-tools` included,
  [ADR-0074](../../adr/0074-cross-vendor-skill-and-agent-discovery.md)/[ADR-0229](../../adr/0229-every-skill-is-model-visible-user-only-removed.md)).
  Frontmatter: `name` + `description` (required) and
  `allowed_tools` (parsed-but-ignored with a one-time load warning — the
  skill-scoped mask it named is removed by
  [ADR-0194](../../adr/0194-skills-are-additive-only.md); skills are
  additive-only). The `tools:` key is retired
  ([ADR-0228](../../adr/0228-rhai-path-and-args-skill-tools-frontmatter-removed.md)):
  a native `SKILL.md` still carrying it is **skipped** with an error-level log
  naming the replacement (`lib.rhai` functions, or a script file run with
  `rhai` `path`) — never a registry-load abort, so one stale skill cannot brick
  startup; a foreign dir drops it like any unknown key. `user_only` is removed
  ([ADR-0229](../../adr/0229-every-skill-is-model-visible-user-only-removed.md)): **every skill is model-visible** — listed, loadable,
  its `lib.rhai` callable from any script; a native `SKILL.md` still carrying the
  key loads normally and warns once per sighting (`config::warn_once`: live
  reloads stay quiet, a removed-then-restored key warns again). A skill the model must not run is a permission-mode question (rules on
  the commands it performs), not a visibility one. Each `SkillMeta` resolves its
  `root_dir` **once** at discovery. **Disclosure is tier-1 only**: `SkillRegistry::disclosures`
  yields one `name: description` row per skill, which `list_skills`
  returns on request ([ADR-0226](../../adr/0226-one-advertised-surface-mcp-envelope-registered-is-available.md) — no longer in the system prompt); bodies are never preloaded. **Selection stays
  the model's own reasoning** — no keyword router or embedding gate; the model
  matches its task against the `description` in its forward pass, so description
  quality is the contract. Bodies + payload (`references/`/`scripts/`) are tier-2,
  loaded on demand (`load_skill`, ✅ #115, below).
- **Tier-2 skill loading (✅ #115, [ADR-0037](../../adr/0037-load-skill-tool-deterministic-resolution.md)):**
  one generic `load_skill { skill_name }` tool (not one-per-skill) resolves a
  skill's body on demand. It is a *real host tool* in the `ToolRegistry`
  (`entanglement_runtime::skills::load_skill::LoadSkillTool`, holding a
  shared `Arc<SkillRegistry>`), but since
  [ADR-0226](../../adr/0226-one-advertised-surface-mcp-envelope-registered-is-available.md)
  it is one of the six discovery tools: `Capability::Control`, ungraded, and
  non-maskable — it only reads the skill catalog the runtime already
  discovered, so it runs in every mode and an overlay entry or mode rule
  naming it is refused or warned about (see the non-maskable set in [advertisement & enforcement](advertisement-and-enforcement.md)).
  Unlike the old read-only `explore` agent, which masked `load_skill` out
  entirely, a read-only *mode* still lets a model load a skill's body —
  reading a skill is not writing. The handler resolves **deterministically** (never model reasoning):
  look the `SkillMeta` up by name (an unknown name is a tool error); then **substitute
  every relative payload path to an absolute one** before the text reaches the
  model — closing Claude Code's bug class where the *model* resolves
  `references/x.md` against the wrong base (anthropics/claude-code#17741, #11011).
  `SKILL_DIR` and the project root stay two strictly separate coordinate systems: a
  ref that does not resolve under the skill dir (a project-root path) is left
  untouched; no implicit CWD fallback; a `${SKILL_DIR}` placeholder is the
  author's explicit escape hatch. The result is an ordinary `tool_result` carrying
  `skill_id` + `skill_dir`, the substituted body, then — when the skill ships
  one — its `lib.rhai` public function signatures, each followed by its
  `///`/`/** */` doc comment ([ADR-0228](../../adr/0228-rhai-path-and-args-skill-tools-frontmatter-removed.md)),
  then `available_refs` (supporting files listed as absolute paths, **not**
  loaded; a `.rhai` file among them runs via `rhai` `path`) — never a spoofed
  user message, so the authorship trail stays honest.
- **Skill libraries — `lib.rhai` (✅ #819, [ADR-0227](../../adr/0227-one-binding-path-namespaces-http-skill-libraries.md)):**
  a `lib.rhai` beside `SKILL.md` — any layer, foreign skill dirs included
  (`root_dir` is pre-resolved by both parsers) — compiles once at discovery
  (`skills::lib_rhai`) into an encapsulated static module every `rhai` run
  registers as `skill::<skill>` (name mapped like an MCP segment: non-identifier
  → `_`, no letter before the first digit → `t_` prefix, reserved word
  `_`-suffixed, validity per Rhai itself; a colliding
  later skill is logged, skipped, and listed by `load_skill` as not callable — the same naming pass registration uses). Only `fn` definitions: a top-level
  statement would run on every script launch unasked, so it rejects the whole
  lib with a warning. A lib function calls every binding a script can —
  including `http()` with the user's `http_auth` credentials, which is why a
  credentialed call still forces a per-host approval. Live-reloaded with the
  skill registry; `load_skill` lists the public signatures with their doc
  comments (signature-first, #826). A skill's predefined *scripts* (the
  retired `tools:` `rhai` kind) are plain files run with
  `rhai {"path": "<skill_dir>/x.rhai", "args": {…}}` — a launch graded
  like any tool on that path (an `Ask` grade shows the launch card: Run all
  · Ask per command · Reject), or run unasked when the user lists the
  skill's folder in `config.yml` `rhai.trusted`
  ([ADR-0228](../../adr/0228-rhai-path-and-args-skill-tools-frontmatter-removed.md)).
  Writing a skill's `SKILL.md`/`lib.rhai` (any user or project skill dir)
  always asks, in every mode — the trusted-code write floor
  ([gates & host tools](../gates-and-host-tools.md)).
- **Skills are additive-only — the `allowed_tools` mask is removed (✅ ADR-0194,
  [0194](../../adr/0194-skills-are-additive-only.md), superseding
  [ADR-0106](../../adr/0106-skill-scoped-allowed-tools-enforcement.md) and retiring
  [ADR-0129](../../adr/0129-thread-the-skill-mask-into-rhai-binding-resolution.md)):**
  a skill adds capabilities (its body, its `lib.rhai` functions, its script
  files) and **never restricts** the session's tool set. The session-keyed
  `ActiveSkill` map, `permission::skill_masked`, the `load_skill`
  result-header activation parse, and the rhai binding policy's skill-mask
  snapshot are deleted; the dispatch ladder loses its skill arm. WHY: the mask's
  scope was one turn of one session, triggered and outlived by the same
  model it restrained — a speed bump, not a boundary — while the standing
  control was always the real story, and its observable effect was
  subtractive footguns (#554: a skill listing `[bash, read, grep]` disarmed
  editing for the rest of the turn). `OutEvent::SkillActive` stays on the
  wire as posture-only; its `allowed_tools` field is **vestigial** — still
  serialized when present (replay compat), never read or enforced (see
  [protocol](../protocol.md)). `SkillFrontmatter.allowed_tools` is
  parsed-but-ignored with a one-time load warning (`SkillFrontmatter` is
  `deny_unknown_fields`, so the key must stay known; hard removal later).
  The foreign (cross-vendor) layer already drops it, exactly as it drops
  Claude-style `allowed-tools`. The honest cost, recorded in the ADR: skill
  authors lose their only self-imposed guardrail — and, since
  [ADR-0207](../../adr/0207-permission-modes-replace-agent-borne-authority.md)
  retired the per-agent `tools:` mask this bullet originally pointed an
  author at too, **the only standing control left is the session's
  permission mode**: an author wanting a restricted session picks a
  stricter mode (or tunes one via `config.yml` `modes:`), never an agent
  definition or skill frontmatter — neither carries authority any more.
- **Skill preload vs access — two independent mechanisms (✅ #117, [ADR-0043](../../adr/0043-skill-preload-vs-access-independent-mechanisms.md)):** an agent
  definition controls skills along two orthogonal axes, deliberately *not* merged
  (merging loses expressiveness). **Preload** is `skills: [name, …]` frontmatter:
  the listed skills' full bodies are injected into that agent's assembled system
  prompt at load, through the *same* substitution pipeline as `load_skill`
  (`SkillRegistry::preload_body` → `load_skill::render_skill`) — it is preload
  *only*, never an allowlist, and is mode-independent (a spawned subagent gets the
  body even though its tier-1 index is withheld). Unlike the model-facing
  `load_skill`, an unknown name is a loud load-time error.
  **Access** is not an agent field: `load_skill` is a non-maskable discovery
  tool (`Capability::Control`, ungraded since
  [ADR-0226](../../adr/0226-one-advertised-surface-mcp-envelope-registered-is-available.md)),
  so no mode or overlay withdraws it; what a loaded skill's scripts and
  commands may *do* is graded call by call like any other tool.
  `skills: [x]` therefore only preloads — a subagent may still discover and
  load any other skill exactly like its parent; to keep a skill from running,
  grade its effects in the mode (a `prompt:`/`deny:` rule on what it does) or
  remove it from the discovered skill dirs.
- **User slash commands — prompt templates (✅ #733, [ADR-0221](../../adr/0221-user-slash-commands-are-head-side-prompt-templates.md)):**
  `entanglement_runtime::slash_commands` discovers every `*.md` under the
  layered `commands` dirs through the same `layers::candidate_dirs` as skills —
  user `~/.claude/commands`, `${config_dir}/opencode/command` and `…/commands`
  (lenient) < `${config_dir}/entanglement/commands` (strict; the whole user
  layer is replaced by `ENTANGLEMENT_COMMANDS_DIR`) < project
  `.claude/commands`, `.agents/commands`, `.opencode/command(s)` (lenient) <
  `.entanglement/commands` (strict); later wins by name, no embedded
  built-ins. A file's path relative to its dir is its name, subdirectories as
  `:` namespaces (`frontend/review.md` → `/frontend:review`); a file or
  directory whose own name contains `:` is skipped with a load warning, since
  `a:b.md` would collide with `a/b.md` (#798). Frontmatter is
  optional: `description` (default: the body's first non-empty line) and
  `argument-hint`; `agent`, `model` and `allowed-tools` parse but are ignored
  (a native file carrying one gets a load warning) — a command runs in the
  current session with its agent, model and permission mode, so like a skill
  it never narrows or widens authority. The body is pure text: `$ARGUMENTS`
  is the whole argument string, `$1`..`$9` shell-style positional args (quotes
  group; missing → empty); with no placeholder, non-empty args are appended
  as `ARGUMENTS: …`; Claude Code's `` !`cmd` `` and `@file` are **not**
  supported and go out literally. **Expansion is head-side**: the TUI (Enter
  on `/name args`) and `skutter run "/name args"` (or unquoted, `skutter run
  /name "a b"` — the shell's argv tokens are the positionals verbatim, so `"a b"` is one `$1`,
  #798) send the expanded text as
  an ordinary `InMsg::Prompt`, so core, the protocol and the session log
  never see a command; `pipe`/`serve` carry frames, not typed text, and do
  not expand. A user command named like a built-in TUI command
  (`RESERVED_NAMES`, pinned to the TUI roster by a unit test) is dropped with
  a load warning in every head. The definitions watcher reloads the registry
  live; `skutter inspect commands [name]` lists name, layer, source and
  description (or one command's provenance and raw template).
- **Where dispatch runs (✅ #59):** the `Agent` *shape* stays a core
  protocol type, but the `Allow|Ask|Deny` decision + the approval wait are a
  **runtime** concern ([ADR-0003](../../adr/0003-agent-and-permission-profiles.md) /
  [ADR-0010](../../adr/0010-single-head-crate-and-bash-opt-in.md), authority
  itself moved to the mode by
  [ADR-0207](../../adr/0207-permission-modes-replace-agent-borne-authority.md)).
  Core emits `ToolExec` for *every* host tool — the whole batch up front
  since #270 ([ADR-0061](../../adr/0061-parked-turn-state-batch-tool-resolution.md))
  — and parks the turn as explicit `TurnState` until each `ToolResult` lands
  (§8); it carries the opaque `mode: String` and never evaluates it, exactly
  as it never read `PermissionProfile` before. The runtime's
  `ModeResolver` (`policy.rs`) tracks each session's live permission mode
  against a `ModeTable` it holds, resolves the grade through
  `Mode::resolve`'s longest-match engine, and — for `Ask` — emits the
  `ToolRequest` prompt and awaits `Approve`/`Reject`/`Stop`, so every head
  stays a thin protocol adapter (it just sends the same frames; the runtime,
  not core, acts on them).
- **Authoritative gating, fail-closed (✅ #156, [ADR-0070](../../adr/0070-authoritative-tool-exec-profile-and-fail-closed-fallback.md),
  carried into [ADR-0207](../../adr/0207-permission-modes-replace-agent-borne-authority.md)
  per `ModeResolver`'s own doc comment):** the pre-ADR-0207 profile map was
  folded *only* from the **lossy** `SessionStarted`/`AgentChanged` broadcast,
  with a fail-*open* default — an unseen session resolved to `Allow` and
  *unmasked*, inverting the posture exactly when overload made a dropped
  frame most likely. The mode's own map (`perm_modes`,
  folded from `OutEvent::ModeChanged`, announced unconditionally at session
  start like `AgentChanged`) keeps the same fail-**closed** discipline
  today: `ModeResolver::resolve` denies outright — no whole-mode lookup,
  no capability check — when the session isn't in the map yet
  (`Permission::Deny`), and denies again (with a `warn!`, defense in depth
  against an unreachable path since only `InMsg::SetMode` writes this map
  and a real head validates the mode name against the same table first) if
  the mode name it does find isn't one `ModeTable` recognizes. Degraded but
  safe, same shape as before ADR-0207, just keyed by mode instead of by
  profile.

---

Previous: [§3 — Spawn bounds, task state & the plan tool](spawn-tasks-plans.md)
