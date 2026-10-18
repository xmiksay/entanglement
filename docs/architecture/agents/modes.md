# Agents & permissions §3 — Identity vs authority, starting mode & permission modes

> Part of the [agents & permissions module](../agents-and-permissions.md) of the [architecture overview](../../architecture.md).

## 3. Agent profiles + permission modes — [ADR-0207](../../adr/0207-permission-modes-replace-agent-borne-authority.md)

**Authority is a second, independent axis from identity** — a session is a
pair `(agent, mode)`. The agent supplies *who the session is* — a persona,
chosen once at session start and fixed for that session's life. The mode
supplies *what it may do* — every permission fact — and is orthogonal:
switching mode never touches the agent, and vice versa. This replaces the
pre-[ADR-0207](../../adr/0207-permission-modes-replace-agent-borne-authority.md)
world, where `Agent` carried both in one struct (`permission`,
`tools`/`disallowed_tools`, `can_spawn`/`spawnable_agents`, `sandbox`, `mode:
primary|subagent|all`) and nothing could change posture without changing
persona, or persona without changing posture
([ADR-0003](../../adr/0003-agent-and-permission-profiles.md),
[ADR-0038](../../adr/0038-physical-per-agent-tool-restriction.md),
[ADR-0040](../../adr/0040-per-profile-spawn-control.md),
[ADR-0134](../../adr/0134-per-profile-sandbox-scoping-and-spawn-chain-clamp.md) —
all superseded).

A session runs under exactly one core [`Agent`][profile], now
**identity only**: `{ name, description, system_prompt, model?, provider?,
default_mode? }`.
`description` drives delegation matching (§8, the only field a spawning model
sees); `model`/`provider` are the existing per-profile model pin ([definitions](definitions.md),
unaffected by ADR-0207). The file-level definition (frontmatter + body, [definitions](definitions.md))
additionally carries `include_brief`/`skills` — baked into `system_prompt` at
load time, so they never survive as separate fields on the core struct. Any
agent may be a session root or a spawn target now; there is no more
`primary`/`subagent`/`all` distinction to gate that.

Authority lives entirely in the runtime, as the session's **permission mode**
— see §permission modes below for the full rule engine
([ADR-0207](../../adr/0207-permission-modes-replace-agent-borne-authority.md)).

**Starting mode** ([ADR-0218](../../adr/0218-agent-default-mode-is-a-starting-hint.md),
#752/#763). `default_mode` is a starting hint, not authority.

- **Allowed values.** It must name a built-in mode no wider than `build`
  (`research`/`plan`/`build`). Anything else, `auto` included, warns at load
  and is dropped. The shipped `plan` agent sets `default_mode: plan`.
- **Fresh roots.** Precedence is `--mode` > config `mode:` > the agent's
  `default_mode` > `DEFAULT_MODE` (`build`).
  - `run`, the implicit one-shot and the TUI share one launch path,
    `heads::start_fresh`. It sends a single `Spawn` carrying the agent and
    the launch mode (`InMsg::Spawn.mode`), so the session announces exactly
    one starting mode and never shows a "changed from" transition.
  - With no launch mode, core starts the root in the hint
    (`holly/supervisor/starting_mode.rs`), clamped on
    `EngineConfig::mode_ladder` to no wider than `DEFAULT_MODE`.
  - A typo'd `--mode`/config `mode:` stops the launch in
    `LaunchMode::resolve` (non-zero exit, nothing started). Behind that, an
    off-ladder `Spawn.mode` refuses the spawn, and `start_fresh` waits for
    `SessionStarted`-or-`Error` so a refusal is fatal rather than falling
    through to a lazily created `general`/`build` root.
- **Resumes in the TUI.** The TUI applies the same launch mode to the
  sessions it resumes (`LaunchMode`, `resume_with_mode`).
- **Spawned children.** The ladder is `research` < `plan` < `build` <
  `auto`, the same one `request_mode` widens along. A child takes its
  requested mode, else its agent's hint, only when the ladder shows it is no
  wider than its parent's mode. Otherwise, including any pair not on the
  ladder, the child inherits the parent's mode, so neither can widen a
  sub-tree.
- **Compaction successors** keep their source's mode.
- **Resumed sessions** keep the mode their log replays; the hint never
  applies.

**At a glance (epic [#111](https://github.com/xmiksay/entanglement/issues/111), synthesized in [ADR-0044](../../adr/0044-agents-skills-system-prompt-epic-synthesis.md)).**
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
  case). The user layer is a `layers::UserLayer` parameter — `FromEnv` (that
  override, else the real home/config dirs) in every head, or `Dir(path)` via
  the `*_in` loader variants (`skills::load_registry_in`,
  `agents::load_registry_in`/`resolve_registry_in`/`prompt_report_in`,
  `slash_commands::resolve_registry_in`) for tests and embedders that must not
  touch process env or the real user dirs (#773). **Cross-vendor dirs are scanned too**
  ([ADR-0074](../../adr/0074-cross-vendor-skill-and-agent-discovery.md)): within
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
  ([ADR-0047](../../adr/0047-local-trust-boundary.md)).
- **Progressive disclosure, recursively** — the model sees only *descriptions*
  until it acts: `name: description` rows from `list_agents` (agents) and
  `list_skills` (skills) — no roster in the `agent` schema or the prompt any
  more, so both stay byte-stable ([ADR-0226](../../adr/0226-one-advertised-surface-mcp-envelope-registered-is-available.md)) →
  full body on `load_skill` **or** preload (skills tier-2) → the definition body
  *becomes* a child's own assembled prompt at spawn.
- **Model decides *whether*, harness decides *how*** — selection is LLM reasoning
  over `description` text (no keyword/embedding router); path resolution, prompt
  assembly, authorization, and tool-list enforcement are deterministic runtime
  code. Injected content is always a `tool_result` / prompt section, never a
  spoofed `user` message.
- **Physical over prompted** — a call a mode denies is *refused by code* at
  dispatch, not a persona told not to make it. The tool's schema is still
  advertised ([advertisement & enforcement](advertisement-and-enforcement.md)
  covers why advertisement and enforcement are separate) — the guarantee is
  "the call cannot succeed", not "the model cannot see the tool".
- **Enforcement-locus split** — a gate lives where it can see the call: the
  session's permission mode, spawn bounds, the config ceiling, and plan
  authorship are all **runtime** — every tool, including
  `propose_plan`/`update_tasks`, round-trips there. See ADR-0044 for the
  original principle→enforcement map (predating ADR-0207, whose §3/§4 are the
  current enforcement mechanism) and the deferred follow-ups (skill
  provenance, skill-index masking, child-root isolation).

- An agent is chosen once, at session start (`--agent`, `config.yml` `agent:`,
  or a spawning `agent` tool call's own argument), and is fixed for that
  session's life — there is no live `SetAgent` any more
  ([ADR-0207](../../adr/0207-permission-modes-replace-agent-borne-authority.md)
  §9: a persona is text already sent, so "switching" would only ever append a
  second one; a spawned sub-agent is a fresh session with its own system
  prompt, which is what delegating to a different persona actually needs). A
  profile carrying a **model pin** still rebinds the session's backend at that
  same moment, with a following `ModelChanged` (see *Per-profile model
  pinning* in [definitions](definitions.md)) — `SetModel`/`SetGeneration` themselves are unaffected and
  stay live.
- **Permission modes are the rule engine** — `entanglement-runtime::mode`
  ([ADR-0207](../../adr/0207-permission-modes-replace-agent-borne-authority.md)
  §2/§4, superseding [ADR-0003](../../adr/0003-agent-and-permission-profiles.md)/[ADR-0038](../../adr/0038-physical-per-agent-tool-restriction.md)/[ADR-0040](../../adr/0040-per-profile-spawn-control.md)/[ADR-0114](../../adr/0114-capability-level-permission-keys.md)/[ADR-0134](../../adr/0134-per-profile-sandbox-scoping-and-spawn-chain-clamp.md)/[ADR-0148](../../adr/0148-glob-patterns-in-the-agent-tool-mask.md)/[ADR-0192](../../adr/0192-universal-advertisement-enforcement-at-dispatch.md)/[ADR-0198](../../adr/0198-out-of-mask-tool-calls-are-approvable.md)).
  A `Mode` is `{ name, default: Permission, rules: Rules, limits: Limits,
  sandbox: Option<String>, sandbox_network: bool }`; a `ModeTable` is a named
  set of them. `skutter` compiles in exactly four — `research`, `plan`,
  `build`, `auto` — and reads no `modes/` directory: a user can tune one via
  `config.yml` `modes:` ([approvals & grants](approvals-and-grants.md)) but never define, add, or remove one; an
  embedder supplies its own table via `ModeTable::new` (the seam
  multi-tenant/custom-posture embedding needs, mirroring how `Agent`
  registries are pluggable). `entanglement-core` carries only an opaque
  `mode: String` on the session — it persists it, replays it, and emits
  `OutEvent::ModeChanged`, but never evaluates it; `Permission`/
  `PermissionProfile` (unchanged shapes: `Allow`/`Ask`/`Deny`, a
  `(pattern, grade)` rule list + a `default`) stay core types — no longer
  carried by `Agent`, which dropped its `permission` field entirely —
  now serving only as the `config.yml` `permissions:` ceiling's wire shape
  ([approvals & grants](approvals-and-grants.md)); `Capability` is new and lives entirely in the runtime
  (`entanglement-runtime::capability`), never in core.
  Every registered `Tool` declares `fn capabilities(&self) -> &'static
  [Capability]` — `Read`/`Write`/`Exec`/`Plan`/`Control`, a **slice** because
  `call`/`rhai` are genuinely multi-capability (`Read`+`Write`+`Exec`); the
  handful of runtime-owned pseudo-tools with no `ToolRegistry` entry
  (`agent`/`agent_send`/`poll`/`ask_user`/`list_agents`/`list_models`/
  `list_skills`/`list_mcp`/`mcp_tools`/`update_tasks`/`request_mode` ⇒
  `Control`; the registry tool `load_skill` is `Control` too, `propose_plan` ⇒ `Plan`) resolve
  through a small hardcoded table (`capability::runtime_owned`) instead. A
  **`Capability::Control`-only tool is never graded** — checked by capability
  (`capability::bypasses_grading`), not a second interception table, so it
  automatically covers every current and future bookkeeping tool with
  nothing to update at grading time; it reads/writes/executes nothing on the
  host itself. The exception is **spawning**: `agent`/`agent_send` stay
  `Control` (so a `control` class rule reaches them) but are graded like any
  tool, since a spawn starts a session that runs tools
  ([ADR-0232](../../adr/0232-spawning-is-graded-bookkeeping-control-tools-non-maskable.md)) — the built-in modes class-allow `control`, so spawning
  runs unprompted unless a rule says otherwise. The `control` class key
  matches only a tool whose capabilities are exactly `[Control]`: as the
  longest class name it would otherwise out-rank `write`/`exec` and widen a
  mixed tool past the mode's class deny. `request_mode` is the one
  ungraded Control tool whose semantics still force-park an approval
  (§permission-widening, [advertisement & enforcement](advertisement-and-enforcement.md)), mirroring `propose_plan`.
  **Grading**: a mode's `rules` is one flat list of `deny`/`allow`/`prompt`
  entries (`prompt` is the YAML spelling of `Permission::Ask` — the config
  surface uses the word a user thinks in, the enum keeps core's name), each
  keyed by a bare tool name, `*`, an **argument-scoped** `tool(pattern)`
  (unchanged ADR-0051/#173 grammar — the `*`/`?` glob matches the command for
  `bash`/`call`, the target path for `edit`/`write`/`read`/`apply_patch`,
  the `pattern` for `glob`, the file filter for `grep`, the script
  `path` for `rhai` — canonicalized, so a link grades as where it points
  ([ADR-0228](../../adr/0228-rhai-path-and-args-skill-tools-frontmatter-removed.md)),
  the normalized URL for `web_fetch`, `{METHOD} {url}` for `http`, the
  profile a spawn runs for `agent` (an omitted `agent` is the default
  `general`, so `agent(general)` covers it) and the trimmed `agent_id`
  handle for `agent_send`
  ([ADR-0235](../../adr/0235-scoped-spawn-rules-grade-the-target.md));
  path args grade **root-relative**, ADR-0125; any other tool — every
  `mcp__<server>__<tool>` among them — has no argument, so only its bare
  rule matches, and a scoped key on one warns at config load
  (`config::mode_warn`, #878)), a workdir-scoped `tool{pattern}` (unchanged
  ADR-0116/#425 grammar), or a bare capability-class name
  (`read`/`write`/`exec`/`plan`/`control` — a scoped key always names the
  literal tool instead, since scoping has no class grammar; this is how
  `plan` mode's plans-folder carve-out is written: `write(.entanglement/plans/*.md):
  allow` out-ranking the class `deny: [write]`). Both scoped grammars still
  run through core's own tested matcher (`mode::rules::tool_rule_matches`
  delegates to a single-rule `PermissionProfile::resolve_scoped`, so the
  glob/scope semantics are provably unchanged, not reimplemented).
  **The longest matching key wins** — not a tier order, not first-or-last
  declared: `write(docs/*)` (13 chars) out-ranks bare `write` (5 chars) with
  no separate "scoped beats bare" rule needed, and a reader determines the
  outcome by inspection alone. Refined by
  [ADR-0228](../../adr/0228-rhai-path-and-args-skill-tools-frontmatter-removed.md)
  (amending ADR-0207 §4), for modes and the `permissions:` ceiling alike:
  **a rule naming the tool outranks a capability-class rule; a `*` rule
  competes with a named rule by key length and with a class rule by
  strictness (the stricter wins); within each kind the longest key wins;
  ties go to the stricter grade** (`deny` > `prompt` > `allow`). Otherwise a
  multi-capability tool like `rhai` (read+write+exec) would be decided by
  whichever class key is longer, and `build` could never ask for scripts
  while allowing writes. A key spelling a tool's own name counts as naming
  it even when it is also a class (`read` for the `read` tool); the tuning
  guard refuses a scoped `*` allow/prompt in a mode that class-denies such
  a class. The `rhai` launch is graded like any tool, on its canonical
  script `path` (`rhai(scripts/deploy/*)`; an inline script matches only a
  bare `rhai` rule), and a script's nested calls by the same rules as direct
  calls — one rule set. **`bash` and `call` share
  one rule set** — two spellings of the same `Exec` capability, so a rule
  written for either grades both (`mode::rules::canonicalize_key`); a
  compound command (`&&`/`||`/`;`/`|`/`&`, ADR-0197, now covering `call` too)
  grades **per segment**, folding to the most restrictive result, and
  anything the conservative splitter can't fully parse (`SplitOutcome::Opaque`
  — redirection, substitution, a heredoc, subshell grouping) grades at the
  mode's `default` outright — never a whole-string guess, unlike the old
  bash-only `permission_bash.rs` resolver it now wraps. A `Deny` is
  **absolute**: a flat decline naming the mode and the way out (`` tool `x`
  denied by mode `research` — use /mode to switch ``), no prompt, no escape
  hatch except the user's own `/mode`/`request_mode`; `Ask` parks an ordinary
  approval exactly as before. (The skill-alias rewrite that used to precede
  grading is gone with skill `tools:`, [ADR-0228](../../adr/0228-rhai-path-and-args-skill-tools-frontmatter-removed.md).)
  MCP tools grade like any other tool by name
  or pattern; today every `mcp__*` tool's `Capability` is hardcoded to
  `Write` (`McpTool::capabilities`, a deliberate fail-safe — an external
  server is opaque, it could do anything) with no per-server capability hint
  yet, unlike the retired ADR-0117 fan-out — a research-mode session
  therefore prompts/denies an MCP tool by the mode's `default`/`exec`-class
  rules unless a mode rule names it explicitly.

[profile]: ../../../entanglement-core/src/protocol.rs

---

Next: [§3 — Mode tuning, approvals, grants & the permission ceiling](approvals-and-grants.md)
