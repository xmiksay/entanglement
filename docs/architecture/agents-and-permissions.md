# entanglement Architecture — Agent profiles, permissions, skills & system prompt

> Part of the [architecture overview](../architecture.md). The *why* behind each choice is in the [decision log](../adr/README.md).

## 3. Agent profiles + permission modes — [ADR-0207](../adr/0207-permission-modes-replace-agent-borne-authority.md)

**Authority is a second, independent axis from identity** — a session is a
pair `(agent, mode)`. The agent supplies *who the session is* — a persona,
chosen once at session start and fixed for that session's life. The mode
supplies *what it may do* — every permission fact — and is orthogonal:
switching mode never touches the agent, and vice versa. This replaces the
pre-[ADR-0207](../adr/0207-permission-modes-replace-agent-borne-authority.md)
world, where `Agent` carried both in one struct (`permission`,
`tools`/`disallowed_tools`, `can_spawn`/`spawnable_agents`, `sandbox`, `mode:
primary|subagent|all`) and nothing could change posture without changing
persona, or persona without changing posture
([ADR-0003](../adr/0003-agent-and-permission-profiles.md),
[ADR-0038](../adr/0038-physical-per-agent-tool-restriction.md),
[ADR-0040](../adr/0040-per-profile-spawn-control.md),
[ADR-0134](../adr/0134-per-profile-sandbox-scoping-and-spawn-chain-clamp.md) —
all superseded).

A session runs under exactly one core [`Agent`][profile], now
**identity only**: `{ name, description, system_prompt, model?, provider?,
default_mode? }`.
`description` drives delegation matching (§8, the only field a spawning model
sees); `model`/`provider` are the existing per-profile model pin (below,
unaffected by ADR-0207). The file-level definition (frontmatter + body, below)
additionally carries `include_brief`/`skills` — baked into `system_prompt` at
load time, so they never survive as separate fields on the core struct. Any
agent may be a session root or a spawn target now; there is no more
`primary`/`subagent`/`all` distinction to gate that.

Authority lives entirely in the runtime, as the session's **permission mode**
— see §permission modes below for the full rule engine
([ADR-0207](../adr/0207-permission-modes-replace-agent-borne-authority.md)).

**Starting mode** ([ADR-0218](../adr/0218-agent-default-mode-is-a-starting-hint.md),
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
  case). The user layer is a `layers::UserLayer` parameter — `FromEnv` (that
  override, else the real home/config dirs) in every head, or `Dir(path)` via
  the `*_in` loader variants (`skills::load_registry_in`,
  `agents::load_registry_in`/`resolve_registry_in`/`prompt_report_in`,
  `slash_commands::resolve_registry_in`) for tests and embedders that must not
  touch process env or the real user dirs (#773). **Cross-vendor dirs are scanned too**
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
  until it acts: `name: description` rows from `list_agents` (agents) and
  `list_skills` (skills) — no roster in the `agent` schema or the prompt any
  more, so both stay byte-stable ([ADR-0226](../adr/0226-one-advertised-surface-mcp-envelope-registered-is-available.md)) →
  full body on `load_skill` **or** preload (skills tier-2) → the definition body
  *becomes* a child's own assembled prompt at spawn.
- **Model decides *whether*, harness decides *how*** — selection is LLM reasoning
  over `description` text (no keyword/embedding router); path resolution, prompt
  assembly, authorization, and tool-list enforcement are deterministic runtime
  code. Injected content is always a `tool_result` / prompt section, never a
  spoofed `user` message.
- **Physical over prompted** — a call a mode denies is *refused by code* at
  dispatch, not a persona told not to make it. The tool's schema is still
  advertised (§permission modes below covers why advertisement and
  enforcement are separate) — the guarantee is "the call cannot succeed", not
  "the model cannot see the tool".
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
  ([ADR-0207](../adr/0207-permission-modes-replace-agent-borne-authority.md)
  §9: a persona is text already sent, so "switching" would only ever append a
  second one; a spawned sub-agent is a fresh session with its own system
  prompt, which is what delegating to a different persona actually needs). A
  profile carrying a **model pin** still rebinds the session's backend at that
  same moment, with a following `ModelChanged` (see *Per-profile model
  pinning* below) — `SetModel`/`SetGeneration` themselves are unaffected and
  stay live.
- **Permission modes are the rule engine** — `entanglement-runtime::mode`
  ([ADR-0207](../adr/0207-permission-modes-replace-agent-borne-authority.md)
  §2/§4, superseding [ADR-0003](../adr/0003-agent-and-permission-profiles.md)/[ADR-0038](../adr/0038-physical-per-agent-tool-restriction.md)/[ADR-0040](../adr/0040-per-profile-spawn-control.md)/[ADR-0114](../adr/0114-capability-level-permission-keys.md)/[ADR-0134](../adr/0134-per-profile-sandbox-scoping-and-spawn-chain-clamp.md)/[ADR-0148](../adr/0148-glob-patterns-in-the-agent-tool-mask.md)/[ADR-0192](../adr/0192-universal-advertisement-enforcement-at-dispatch.md)/[ADR-0198](../adr/0198-out-of-mask-tool-calls-are-approvable.md)).
  A `Mode` is `{ name, default: Permission, rules: Rules, limits: Limits,
  sandbox: Option<String>, sandbox_network: bool }`; a `ModeTable` is a named
  set of them. `skutter` compiles in exactly four — `research`, `plan`,
  `build`, `auto` — and reads no `modes/` directory: a user can tune one via
  `config.yml` `modes:` (below) but never define, add, or remove one; an
  embedder supplies its own table via `ModeTable::new` (the seam
  multi-tenant/custom-posture embedding needs, mirroring how `Agent`
  registries are pluggable). `entanglement-core` carries only an opaque
  `mode: String` on the session — it persists it, replays it, and emits
  `OutEvent::ModeChanged`, but never evaluates it; `Permission`/
  `PermissionProfile` (unchanged shapes: `Allow`/`Ask`/`Deny`, a
  `(pattern, grade)` rule list + a `default`) stay core types — no longer
  carried by `Agent`, which dropped its `permission` field entirely —
  now serving only as the `config.yml` `permissions:` ceiling's wire shape
  (below); `Capability` is new and lives entirely in the runtime
  (`entanglement-runtime::capability`), never in core.
  Every registered `Tool` declares `fn capabilities(&self) -> &'static
  [Capability]` — `Read`/`Write`/`Exec`/`Plan`/`Control`, a **slice** because
  `call`/`rhai` are genuinely multi-capability (`Read`+`Write`+`Exec`); the
  handful of runtime-owned pseudo-tools with no `ToolRegistry` entry
  (`agent`/`agent_send`/`poll`/`ask_user`/`list_agents`/`list_models`/
  `list_skills`/`list_mcp`/`mcp_tools`/`update_tasks`/`request_mode` ⇒
  `Control`; the registry tool `load_skill` is `Control` too, `propose_plan` ⇒ `Plan`) resolve
  through a small hardcoded table (`capability::runtime_owned`) instead. A
  **`Capability::Control`-only tool is never graded** — checked by capability,
  not a second interception table, so it automatically covers every current
  and future orchestration tool with nothing to update at grading time; it
  reads/writes/executes nothing on the host itself (enabling an MCP server
  only makes its own tools *dispatchable*, and each of those is still graded
  on its own merits). `request_mode` is the one Control tool whose semantics
  still force-park an approval (§permission-widening below), mirroring
  `propose_plan`.
  **Grading**: a mode's `rules` is one flat list of `deny`/`allow`/`prompt`
  entries (`prompt` is the YAML spelling of `Permission::Ask` — the config
  surface uses the word a user thinks in, the enum keeps core's name), each
  keyed by a bare tool name, `*`, an **argument-scoped** `tool(pattern)`
  (unchanged ADR-0051/#173 grammar — the `*`/`?` glob matches the command for
  `bash`/`call`, the target path for `edit`/`write`/`read`/`apply_patch`,
  the `pattern` for `glob`, the file filter for `grep`, and the script
  `path` for `rhai` — canonicalized, so a link grades as where it points
  ([ADR-0228](../adr/0228-rhai-path-and-args-skill-tools-frontmatter-removed.md));
  path args grade **root-relative**, ADR-0125), a workdir-scoped `tool{pattern}` (unchanged
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
  [ADR-0228](../adr/0228-rhai-path-and-args-skill-tools-frontmatter-removed.md)
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
  grading is gone with skill `tools:`, [ADR-0228](../adr/0228-rhai-path-and-args-skill-tools-frontmatter-removed.md).)
  MCP tools grade like any other tool by name
  or pattern; today every `mcp__*` tool's `Capability` is hardcoded to
  `Write` (`McpTool::capabilities`, a deliberate fail-safe — an external
  server is opaque, it could do anything) with no per-server capability hint
  yet, unlike the retired ADR-0117 fan-out — a research-mode session
  therefore prompts/denies an MCP tool by the mode's `default`/`exec`-class
  rules unless a mode rule names it explicitly.
- **Tuning — `config.yml` `modes:`** ([ADR-0207](../adr/0207-permission-modes-replace-agent-borne-authority.md)
  §5): adds rules to one built-in mode in the same grammar, keyed by mode
  name; **no removal syntax** — longest-match already makes a shipped rule
  overridable by adding a longer, more specific one (to stop `bash(wc *)`
  being pre-allowed, add a longer `deny` covering the case, rather than
  deleting the shipped `allow`). Tuning may never set `default` (a loud
  error) and may never **weaken a capability-class `deny`**: because
  longest-match lets a long scoped rule out-rank a short class name,
  `research: allow: ["write(*)"]` would otherwise grant exactly what
  `deny: [write]` forbids. `mode::tune::apply` closes this by resolving each
  tuned rule's named tool to its real `Capability` set (via a live
  `ToolRegistry`-backed resolver — `skutter inspect modes` uses the same
  no-registry static capability fallback) and rejecting
  the rule outright if any of them is class-denied in that mode. The
  `config.yml` `permissions:` ceiling is a `PermissionProfile` re-interpreted
  through `Mode::from_permission_profile` into the identical longest-match
  engine (not its own struct's older last-matching-rule-wins `resolve`), so
  it grades by capability class exactly like a real mode and still clamps
  least-privilege over every mode's own grade.
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
  the record provably share one key. **Mode-scoped** since
  [ADR-0207](../adr/0207-permission-modes-replace-agent-borne-authority.md)
  §8: `GrantKey` also carries the mode it was earned in and matches only that
  mode exactly — a grant earned in `build` is inert in `research`, closing the
  old cross-agent leak where a grant earned under a permissive profile
  silently upgraded `Ask → Allow` under a restrictive one (a legacy
  pre-ADR-0207 grant with no mode on its entry loads as `mode: None`, which by
  construction never equals a live call's `Some(mode)`, so it simply stops
  matching rather than being silently re-honored). **After** the full
  resolution (the session's mode grade → config ceiling), a call that lands
  on `Ask` is upgraded to `Allow` when the store already grants it for this
  mode, so the *identical* later call skips the prompt. A grant **only raises
  `Ask` → `Allow`** — it never
  overrides a `Deny` (the ceiling still clamps first), is matched by exact equality
  (no pattern widening), is dropped on `SessionEnded` for `Session` scope, and is
  never inherited by a sub-agent. `Session` lives in memory; `Always` persists to a
  **managed** file `<state dir>/grants.yml` (the managed-state dir,
  `${config_dir}/entanglement` unless `ENTANGLEMENT_STATE_DIR` relocates all
  eight managed files, [ADR-0213](../adr/0213-prune-env-vars-and-catalog-knobs.md)) — a top-level `grants:` list of `tool(arg)` rule keys,
  loaded at startup and re-written on each new grant (`FileGrantStore::load_from` /
  `DefaultGrantStore::load_from` take an explicit path, or `None` for an
  in-memory-only store — the `ToolExecutor::builder` default every test
  uses, so they never read or write the developer's real file, #747; the
  real path is unreachable under `cfg(test)`). Like the provider-key env file
  (#220) it sits *beside* `config.yml`, not inside it: the runtime rewrites it
  freely, so it never clobbers the hand-edited, commented config. A missing/malformed
  store loads empty and a write failure is logged — both fail *closed* (ask again),
  the safe direction. The TUI modal offers `y` once / `s` session / `a` always /
  `n` reject / `e` edit-reason / `Esc` interrupt.
- **Session-scoped directory grants (✅ #486, [ADR-0126](../adr/0126-session-scoped-directory-grants.md)):**
  a fourth `ApprovalScope::SessionDir` — session-only like `Session`, but
  widened to every later call whose grading argument falls under the approved
  call's directory (`grants::dir_covers`, a plain path-component-prefix check
  on the #485-normalized argument — no symlink resolution, so a granted
  directory can cover an arg whose path component is an in-root symlink
  pointing elsewhere, skipping the *prompt* but never the filesystem
  boundary: host tools re-canonicalize and stay root-contained; an accepted
  prompt-UX nuance, 2026-07-23 audit) instead of matching one exact call.
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
  (`tui/event_loop/approval_keys.rs`, gated on the pending tool being read-like) and a
  proactive `/allow <path>` command (`tui/allow_command.rs`, normalizing the
  path against the head's root and rejecting anything outside it) — both call
  `grant_session_dir` synchronously through a cloned `Arc<DefaultGrantStore>`
  handle threaded into the TUI, introducing no new wire surface (`Approve`
  was already wire-allowed).
- **Per-user permission ceiling + grants (#522, [ADR-0147](../adr/0147-multi-user-mode-embedder-api.md);
  reframed embedder-side by [ADR-0181](../adr/0181-userid-leaves-the-runtime-crate.md)/[ADR-0184](../adr/0184-provider-hosted-multi-user-seams.md)):**
  built entirely on the #311 seams above, no core change. A session carries an
  optional `UserId` (`Session.user`, spawn-time-fixed like `parent` — a
  child/compaction-successor inherits its parent's/predecessor's user rather
  than being re-told). The runtime ships **no per-user module** (`make
  userid` enforces that the crate never names `UserId`): a multi-user
  embedder implements `PermissionResolver`/`GrantStore` directly over its own
  session→user map — it already knows the mapping, having chosen `user` when
  it sent the session's `InMsg::Spawn`. The recipe (documented in
  [`../embedding.md`](../embedding.md) §7, sketched compiling in
  `examples/embedded.rs`): wrap an inner resolver — typically
  `ModeResolver`, so the process-global #172 ceiling still applies first —
  and clamp its result a *second* time by the resolving session's own user's
  ceiling via the same `clamp_to_base` least-privilege composition #172
  itself uses; key `Always`-scope grants by the user (the storage key itself
  is what makes "one user's grant never leaks to another" true; `Session`
  scope stays keyed by `SessionId`, since a session belongs to exactly one
  user already). Reachable only through the embedder library API — `serve`
  stays single-user (ADR-0048).
- **Escape-root access via approval (✅ #escape-root, [ADR-0109](../adr/0109-escape-root-access-via-approval.md)):**
  root containment (ADR-0054) is no longer absolute. A `read`/`edit`/`write`/`apply_patch`
  path, a `rhai` script `path` or a `bash`/`call` `workdir` that resolves **outside** root is detected in
  the executor (`permission::escape_root_target` + `host::escaping_path`) and
  forces an approval prompt even when the profile would `Allow` (a `Deny` floor
  still wins). The approval is recorded in a **separate** store from the
  permission grants above — `runtime::extra_roots::ExtraRootStore`, managed file
  `<state dir>/extra-roots.yml` (`ENTANGLEMENT_STATE_DIR`, ADR-0213) — keyed by `(tool, resolved-absolute-path)`,
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
  logged, never fatal. **Curated read-only Allow rules (ADR-0195,
  [0195](../adr/0195-bash-is-the-default-exec-and-curated-read-only-rules.md)):**
  the embedded defaults additionally ship a small set of exact-prefix
  argument-scoped **Allow** rules — `bash(find *)`, `bash(grep *)`,
  `call(rg *)`, and ls/cat/head/tail/wc for both exec tools — so a read-only
  command stops costing an approval round-trip. They are ordinary
  `tool(pattern)` entries (#173/#418): user/project layers can tighten or
  widen them, and the ceiling still clamps least-privilege over every grade
  (a user `bash: ask` ceiling forces the prompt back over a curated Allow).
  The list is deliberately short and exact-prefix — no `git *` (it writes),
  no `echo *` (redirection writes), no class patterns — and grows only by a
  reviewed embedded-defaults change. Since [ADR-0197](../adr/0197-compound-bash-commands-grade-per-segment.md)
  these Allow rules grade each top-level segment of a compound command
  independently (`find . | grep x | wc -l` allows with no prompt) instead of
  the whole raw string, closing the over-match a trailing `*` used to open
  (`find . && rm -rf /` no longer rides `bash(find *)`).
- **Live tool-overlay grades compose with the ceiling too (✅ #498/#539,
  originally [ADR-0133](../adr/0133-live-bash-enablement-graded-by-permission.md),
  generalized by
  [ADR-0163](../adr/0163-live-bash-enablement-is-a-tool-overlay-entry.md),
  #611; the registration half retired by
  [ADR-0195](../adr/0195-bash-is-the-default-exec-and-curated-read-only-rules.md)):**
  a live `/enable tool bash --allow` grade — a session
  `ToolOverlayEntry` — overrides the session's own **mode** grade for that
  tool specifically via `tool_runner`'s generic overlay-grade dispatch
  (`permission::overlay_entry_grade`), but the result still passes
  through `clamp_to_base` unconditionally — a config ceiling of `bash: deny`
  still wins over a live `Allow`, same as it wins over any mode's own
  `Allow`. This composes for any tool the overlay enables, not only `bash`.
  The overlay is now **mode-scoped**
  ([ADR-0207](../adr/0207-permission-modes-replace-agent-borne-authority.md)
  §8): dropped wholesale on any live `SetMode` (an entry enabled under one
  mode carries no meaning in another), and an **enable** entry still overrides
  even a mode `Deny` for that session — the model can't reach the overlay
  directly (an enable entry is trusted-frame-only, ADR-0177), so this is the
  human's own escape hatch surviving past a mode's own class deny, not a
  model-reachable one. The grade override still reaches past the exact
  session that set it: `permission::overlay_grade_entry` walks the same
  nearest-link-first ancestor chain [ADR-0024](../adr/0024-subagent-permission-gating.md)'s
  privilege ceiling already does (`permission::ancestor_chain`) — the whole
  spawn sub-tree shares one **mode** now (§6), so this walk exists purely for
  the overlay/grant layer riding on top of it, not for re-deriving the mode
  itself at each link — and a `rhai` binding runs the same dispatch seam
  (ADR-0227), so a script's `bash()` grades exactly like a direct `bash`
  call.
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
- **File-defined (✅ #112, [ADR-0034](../adr/0034-file-based-agent-definitions.md),
  roster collapsed by [ADR-0207](../adr/0207-permission-modes-replace-agent-borne-authority.md)
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
  and skipped ([ADR-0074](../adr/0074-cross-vendor-skill-and-agent-discovery.md)).
  `build`, `explore` and `research` are **retired as agent names** — their
  postures are permission modes now, not personas — so a session log naming
  one of them (and absent from the catalog) resumes, per session, under the
  last catalog-known agent its log named, else `general`, with a one-line
  note; a retired `explore`/`research` session stays read-only, resuming in
  `research` mode unless `--mode` is explicit, and that narrowing rides the
  replay so a re-offered call is already graded under it
  ([ADR-0214](../adr/0214-retired-agent-logs-resume-as-general.md), amended
  by [ADR-0215](../adr/0215-resume-fallback-per-session-and-read-only-narrowing.md);
  [ADR-0216](../adr/0216-resume-modes-ride-the-replay.md); see
  [heads & persistence](heads-and-persistence.md)); `general` (the `build`
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
  on (spawning is bounded by the session's permission mode instead, below).
  Plan authorship (`propose_plan`, ✅ #231/#513, below) and the plan-approval
  mode switch (below) complete the picture. The built-ins are defined
  **once**, here as markdown (#201): core carries only the `general` profile
  its `resolve()` fallback needs (`DEFAULT_AGENT = "general"`; it can't
  parse frontmatter, so it holds no `plan`/`debug` copy to drift from these
  files). Exec-tool choice (`bash` vs `call` vs `rhai`) is deliberately
  **not** in any body: it lives once, in the `bash` tool description
  ([gates & host tools](gates-and-host-tools.md), #740). Embedders using core directly get that single fallback via
  `AgentCatalog::new()`; the runtime rebuilds the full set from the
  embedded markdown (`entanglement_runtime::agents::built_in_registry`). Add
  your own with `AgentCatalog::insert`.
- **The four built-in modes, at a glance** — what a session can actually do,
  independent of which agent runs it. Source of truth:
  `entanglement-runtime/src/mode/builtin/*.yml`
  ([ADR-0207](../adr/0207-permission-modes-replace-agent-borne-authority.md)
  §2/§4/§11; `skutter inspect modes [name]` prints the live, tuned table).

  | Mode | default | notable rules | run limits |
  | --- | --- | --- | --- |
  | `research` | `prompt` | `deny: [write, plan]`; `allow: [read]` + the shared read-only exec set; `prompt` + the shared api-mutating escalation set — read-only investigation, no plan authorship | `max_depth: 2`, `max_agents: 4` |
  | `plan` | `prompt` | `deny: [write]`; `allow: [read, plan, "write(.entanglement/plans/*.md)"]` + the shared read-only exec set; `prompt` + the shared api-mutating escalation set — the plans-folder carve-out out-ranks the class deny by longest match; anything outside the shared set falls through to `prompt` | `max_depth: 2`, `max_agents: 4` |
  | `build` (default) | `prompt` — a deliberate change from the old `build` agent's `default: allow` | `deny: [plan, "bash(rm -rf /*)", "bash(rm -rf ~*)", "bash(git push --force*)", "bash(git push -f*)", "bash(git reset --hard*)", "bash(git clean -f*)", "bash(git branch -D*)"]`; `allow: [read, write, exec]`; `prompt: [rhai, "bash(git push*)", gh/glab issue-or-PR/MR "create"/"merge", "bash(npm publish*)", "bash(cargo publish*)"]` + the shared api-mutating escalation set — broad allow, a short destructive-command deny list stays absolute regardless, and the network-mutating `prompt` entries out-rank the bare `exec` allow the same way; plan authorship denied (revise via `/mode plan`, not a mid-build overwrite); a `rhai` launch asks (the bare `rhai` rule names the tool, so it outranks the class allows) | `max_depth: 4`, `max_agents: 8` |
  | `auto` | `allow` — everything runs but what the deny and prompt lists catch ([ADR-0228](../adr/0228-rhai-path-and-args-skill-tools-frontmatter-removed.md), amending [ADR-0209](../adr/0209-auto-mode-defaults-to-prompt-not-deny.md)); `question_timeout` bounds whatever asks | `deny: [plan, "bash(rm -rf /*)", "bash(rm -rf ~*)", "bash(git push*)", "bash(git reset --hard*)", "bash(git clean -f*)", "bash(git branch -D*)", gh/glab issue-or-PR/MR "create"/"merge", every mutating `gh api`/`glab api` spelling, "bash(npm publish*)", "bash(cargo publish*)"]`; `allow: [read, write, "bash(cargo *)", "bash(make *)"]` + the shared read-only exec set (documentation now — the default already allows); an unenumerated command, tool, MCP call, `web_fetch` or `rhai` launch runs. What still asks: its own `prompt` rules (below), what no rule grades — the escape-root gate and a credentialed `http()` host — the write floor, and any `prompt:` rule a user adds. Unattended, such a rule's call collapses to a refusal first and parks a bounded approval on an identical repeat (`run_limits`); an escape-root card parks bounded at once; a credentialed host is refused unless an explicit user-layer `http(...)` allow names it — every park expiring as a denial after 60s. The explicit `deny` list — destructive and network-mutating commands — stays absolute. `prompt` rules ask for destructive families that must not run silently (`gh`/`glab` `* delete`, `cargo yank`, `npm unpublish`, `docker push`, recursive `rm`). The trusted-script / skill-file write floor applies here too | `max_depth: 2`, `max_agents: 4`, `question_timeout: 60` |

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
  ([ADR-0195](../adr/0195-bash-is-the-default-exec-and-curated-read-only-rules.md))
  and **share one rule set** — a rule written for either grades both. **(2)**
  skills are **additive-only** — the skill `allowed_tools` mask (ADR-0106) is
  removed ([ADR-0194](../adr/0194-skills-are-additive-only.md)): a skill adds
  capabilities and never restricts the session's tool set, so loading one
  mid-turn can no longer disarm editing (the #554 footgun); the control is the
  session's permission mode alone. **(3)** the user-config `permissions:`
  ceiling defaults to `default: allow` — a no-op clamp until the user tightens
  it (#172) — and grades through the identical longest-match engine as a real
  mode (`Mode::from_permission_profile`), so it still clamps least-privilege
  over every mode's own grade. **(4)** advertisement is universal and
  session-stable regardless of mode — every session sees the same tool specs
  (§permission modes above); a `Deny`'d tool is still advertised, and a call
  to it is declined at dispatch with the mode's own attributed refusal, never
  hidden from the model.
- **Per-profile model pinning (✅ #323, [ADR-0081](../adr/0081-per-profile-model-pinning-and-rebind-on-set-agent.md),
  narrowed by [ADR-0207](../adr/0207-permission-modes-replace-agent-borne-authority.md)
  §9 — `SetModel`/`SetGeneration` themselves survive; only the `SetAgent`
  rebind trigger is gone):**
  a profile's frontmatter may set `provider:` beside `model:`. Both set = a
  **model pin** (`Agent::model_pin()`): starting a session under the
  profile re-binds the session's whole backend to that `(provider, model)` —
  through the same `model_resolver` seam a live `/model` (`SetModel`) switch
  uses ([ADR-0063](../adr/0063-realtime-model-provider-switch.md)) — so a
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
- **Per-profile generation-parameter persistence (✅ #374, [ADR-0094](../adr/0094-reasoning-effort-and-per-profile-generation-persistence.md)):**
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
  [ADR-0095](../adr/0095-tui-set-show-generation-persist-on-confirmation.md)):**
  `/set <key> <value>` (`temperature`/`effort`/`thinking_budget`/`max_tokens`)
  sends `InMsg::SetGeneration` and records a pending persist; the confirming
  `GenerationChanged` commits an atomic write to `agent-generation.yml`, an
  `Error` clears it without writing. `/show` is a no-override `SetGeneration`
  query that renders the current params as a status line. Both are reachable by
  typing `/set …`/`/show` directly, or from the `Ctrl+P` palette (a palette
  pick of `/set` prefills the input with `/set ` since the palette carries no
  trailing args, while `/show` runs immediately).
- **No tool-advertising mode** ([ADR-0226](../adr/0226-one-advertised-surface-mcp-envelope-registered-is-available.md)): ADR-0196's `Full | ToolSearch` knob
  (orthogonal to the permission mode) is removed along with its
  `config.yml`/catalog keys — one surface, pinned per session, identical on
  every wire; a stale `tool_advertising:` warns once and is ignored.
- **Live reload + managed-file locking (✅ #329, [ADR-0084](../adr/0084-runtime-live-reload-and-managed-file-locking.md)):**
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
  [ADR-0081](../adr/0081-per-profile-model-pinning-and-rebind-on-set-agent.md)
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
  `stat()`) eliminate it. `.entanglement/plans/` is deliberately **not** one of
  the watched trees here — a plan-file edit is a different reload action
  entirely (nothing to reload, just a live notice), covered by its own
  dedicated watch (`plan_watch.rs`, #627) that reuses only the
  `spawn_debounced_watcher` primitive below, not `LiveDefinitions`; see
  [engine](engine.md) for that watcher's own detail.
- **Advertisement is universal and session-stable — enforcement lives entirely
  at dispatch** (✅ #116/#560, [ADR-0038](../adr/0038-physical-per-agent-tool-restriction.md)/[ADR-0148](../adr/0148-glob-patterns-in-the-agent-tool-mask.md)/[ADR-0192](../adr/0192-universal-advertisement-enforcement-at-dispatch.md)/[ADR-0198](../adr/0198-out-of-mask-tool-calls-are-approvable.md),
  all superseded by [ADR-0207](../adr/0207-permission-modes-replace-agent-borne-authority.md)):
  there is no more agent tool mask to decouple from advertisement — core's
  turn loop (`run_round`) advertises `EngineConfig.tool_specs` (or the
  resolver's replacement, below) **verbatim**, with no filter of any kind.
  WHY (unchanged from ADR-0192/0038): any mid-session change to the tools
  array busts the provider's prompt cache from the tools block onward, so a
  surface that is stable **within** a session keeps that cache warm; what is
  traded away is only "the model cannot even attempt the call" — every
  attempt is now visibly graded instead, and no capability changes hands.
  The universal, session-stable surface is the compiled-in `KERNEL`
  ([ADR-0226](../adr/0226-one-advertised-surface-mcp-envelope-registered-is-available.md); see [engine](engine.md) §turn loop): `read`/`edit`/`write`/
  `apply_patch`/`bash`/`web_fetch`, `rhai` when the feature is on,
  `update_tasks`, `ask_user`, `poll`, `propose_plan`, `request_mode`,
  `agent`/`agent_send`, the `mcp` envelope, and the six discovery tools
  `list_agents`/`list_models`/`list_skills`/`load_skill`/`list_mcp`/
  `mcp_tools` — which are **fully non-maskable**: no mode rule, ceiling
  rule or session overlay deny can withdraw them even at dispatch (the
  ADR-0190 `poll` pattern; the TUI refuses such an entry and a config rule
  naming one warns, #843). `glob`/`grep`/`call` stay registered as
  script-only `rhai` bindings, graded under their own names. With `Agent` carrying no more authority, **nothing
  varies the array by agent any more** — `agent_specs()` is provably
  identical regardless of which session asks (any agent may spawn any agent
  now, bounded only by the mode's `max_depth`/`max_agents`, neither of which
  is schema-visible), and `propose_plan`/`request_mode` are pushed into the
  base `tool_specs` unconditionally rather than gated per profile. The one
  acknowledged dynamic seam left is **MCP tools** (`mcp__*`), since a
  server's tools are unknowable until it connects. **Per-session base specs
  (✅ #308, [ADR-0076](../adr/0076-per-session-dynamic-tool-specs.md)):** an
  optional `EngineConfig.tool_spec_resolver: Option<Arc<dyn Fn(&SessionId,
  SessionModel<'_>) -> Vec<ToolSpec> + Send + Sync>>` (alias
  `ToolSpecResolver`) lets one `Holly` advertise a **different base tool
  surface per session** — the seam multi-tenant embedding needs so user A's
  discovered MCP-server tools never reach user B's sessions. `run_round`
  consults it *fresh every turn* (so a backing-store edit lands on the next
  turn, no respawn); its output **replaces** the engine-global `tool_specs`
  for that session. Nothing filters the result, so **the resolver is the
  only seam that shapes a session's base surface**: a multi-tenant embedder
  that must keep a tool off one tenant's wire has to *omit* it there — a
  mode's `Deny` only declines the call. Sync `Fn` by design (turn hot path);
  the documented pattern is an embedder-owned `Arc<RwLock<..>>` snapshot
  cache. `None` (the default) keeps the engine-global specs — a no-op for
  single-user heads.
- **Enforcement, now a single hard boundary: `Deny` declines, `Ask` prompts**
  ([ADR-0207](../adr/0207-permission-modes-replace-agent-borne-authority.md)
  §4, retiring [ADR-0198](../adr/0198-out-of-mask-tool-calls-are-approvable.md)'s
  mask-miss approval ladder — "there is no mask left to miss"): a call grades
  through the session's permission mode (§permission modes above) before it
  runs. `Ask` emits `ToolRequest` and parks an ordinary approval — the
  outcome ADR-0198 generalized *every* refusal into. `Deny` is now
  **absolute** again, but for a different reason than the pre-ADR-0198 mask:
  it names the mode and the way out (`` tool `x` denied by mode `research` —
  use /mode to switch ``) rather than a per-authority attribution table,
  because there is only ever **one** authority to attribute a `Deny` to — the
  session's own mode (uniform over its whole spawn sub-tree, §6) or the
  config ceiling clamping it further. A session tool-overlay **deny** entry
  (below) withdraws a tool outright with its own distinct message
  (`` tool \`T\` withdrawn for this session by /disable — /enable to restore
  ``) — the one other source of an unprompted refusal, and, unlike a mode
  `Deny`, reversible by the same human voice that set it. `Capability::Control`
  tools bypass this whole ladder (§permission modes above) — never graded,
  never declined for authority reasons, only for the mode's `max_depth`/
  `max_agents` spawn bounds (an `is_error`) or an unknown target/tool name.
- **The session tool overlay survives as the user's own direct voice**
  (✅ #539, [ADR-0149](../adr/0149-per-session-tool-overlay.md); live bash
  enablement folded in, #611,
  [ADR-0163](../adr/0163-live-bash-enablement-is-a-tool-overlay-entry.md);
  now **mode-scoped**,
  [ADR-0207](../adr/0207-permission-modes-replace-agent-borne-authority.md)
  §8): the live **tool overlay** — `InMsg::SetToolOverlay` (trusted-only for
  an enable entry; a deny-only overlay is wire-allowed, #634,
  [ADR-0177](../adr/0177-wire-allowed-deny-only-tool-overlay.md)) — is a
  per-session list of `ToolOverlayEntry { pattern, allow, deny, arg_pattern?
  }` overriding the session's mode grade in both directions: an **enable**
  entry makes a tool's grade `Allow` (or, `arg_pattern`-narrowed, an
  argument-scoped `tool(arg_pattern): allow`) even past a mode `Deny` — the
  model can't reach this itself, since an enable entry is trusted-frame-only
  (ADR-0177), so it is the human at the keyboard's own escape hatch, not a
  hole in the mode's guarantee; a **deny** entry withdraws a tool outright,
  even a mode-allowed one (deny > enable > mode). The overlay is
  **dispatch-only** (setting or clearing it leaves the advertised array
  untouched) and now **mode-scoped**: it is dropped wholesale on any live
  `SetMode`, since an entry enabled under one mode carries no meaning in
  another. The runtime's mirror and core's `Session::tool_overlay` both apply
  this drop, live and on replay, and a compaction successor inherits the
  overlay only under the mode it was set in (#776,
  [engine](engine.md)). The grade override still reaches down the spawn sub-tree via the
  same nearest-link-first ancestor-chain walk the ADR-0024 privilege ceiling
  uses (`permission::overlay_grade_entry`/`ancestor_chain`), and a `rhai`
  binding runs the same dispatch seam (ADR-0227 — a script's `bash()` grades
  identically to a direct `bash` call).
  The TUI drives it via `/enable tool <name>` [`--allow [<pattern>]`] and
  `/disable tool <name>` (upserts a deny; bare = reset) and the
  bare-`/enable` session-tools checklist dialog (checkboxes over the full
  roster seeded from effective availability; `Enter` submits the overlay as
  a diff against the mode's own grade); see the protocol doc for the wire
  shape. (`/enable mcp`/`/disable mcp` and the `/mcp` panel's `e` key are
  gone: an MCP server is available the moment it is registered, [ADR-0226](../adr/0226-one-advertised-surface-mcp-envelope-registered-is-available.md);
  the panel's `d` key stays — a per-session deny of the server's
  `mcp__<name>__*` tools.) **Names no entry can match** (#843,
  `tool_names::NON_MASKABLE_TOOLS`) are kept off the checklist, the `/set`
  Tools tab and `/tools`, and a typed `/enable`/`/disable` naming one is
  refused with the reason: `mcp` (core unwraps the envelope to
  `mcp__<server>__<tool>` before dispatch — per-tool control is a
  `mcp__<server>__<tool>` entry or rule, a server-wide `mcp__<server>__*`
  glob an overlay entry only, and a server the user never wants is removed
  from config), the six discovery tools (always
  available, not maskable — below) and the script-only read variants
  `read_raw`/`glob_json`/`grep_json` (graded as `read`/`glob`/`grep`). A
  wire `SetToolOverlay` entry naming one is still applied (core holds no tool
  policy) but the runtime logs that it has no effect when it folds the
  `ToolOverlayChanged`; a `modes:` or `permissions:` rule naming one warns
  at config load like a stale name (`config::mode_warn`), as does a rule
  whose tool part is a name glob (rule keys never glob the tool part; only
  a bare `*` is a wildcard). A per-tool rule or overlay entry
  (`mcp__<server>__<tool>`) or a server-wide overlay glob
  (`mcp__<server>__*`, overlay only — rules never glob) grades an envelope
  call and a script's `mcp::<server>::<tool>()` binding identically — both reach the same dispatch seam under
  the inner name. A
  typed `/enable <name-or-glob> [--allow […]]` also reports how many
  currently-registered tools the pattern matched, as a transcript status
  line (#560 P9,
  [ADR-0199](../adr/0199-session-tool-listing-and-enablement-drive-advertisement.md)).
  **`/tools`** (same ADR) is a separate, read-heavy *browser* — not an
  overlay editor — over the session's whole reachable surface (host tools,
  MCP servers + their tools, skills, endpoints via `discover::index_rows`):
  each row shows its kind, advertising status (`advertised` when the name is
  in the session's surface snapshot, `hidden` otherwise — script-only tools,
  MCP tools reached through `mcp` — `n/a` before the first round; from the
  same `AdvertisingState` the resolver writes), and the mode's own effective
  grade; a typeahead filter plus a `Tab`-cycled category narrow the list, and
  `Enter` on a row enables it through the same `/enable` path (closing the
  view). An enable never changes the advertised array — only how a call is
  graded.
- **Discovery is six plain tools** ([ADR-0226](../adr/0226-one-advertised-surface-mcp-envelope-registered-is-available.md), amending
  [ADR-0207](../adr/0207-permission-modes-replace-agent-borne-authority.md)
  §12's `explore`/`describe` kinds): `list_agents` (name, description, model
  pin), `list_models` (the usable catalog: provider/id, display name, context
  window), `list_skills` (name + description), `load_skill`, `list_mcp` and
  `mcp_tools` (see [gates & host tools](gates-and-host-tools.md) §MCP) — each
  with no argument or one `name`, all `Capability::Control`, always-on,
  non-maskable ([ADR-0190](../adr/0190-poll-is-always-on-non-maskable-internal-tool.md)
  pattern), available in every mode. The old `modes` kind is covered by the
  static modes preamble in the system prompt; the old `pending` kind's
  in-flight recovery is the handle-less `poll` (a session's own outstanding
  jobs, scripts and sub-agents).
- **`request_mode` — a blocked model's escape hatch**
  ([ADR-0207](../adr/0207-permission-modes-replace-agent-borne-authority.md)
  §10): a `Capability::Control` tool advertised unconditionally, so a blocked
  model can always name what it needs. It force-parks an approval exactly
  like `propose_plan` — widening a session's own authority is a decision
  only the user makes — after four checked-in-order refusal shapes, each
  replying immediately with **no** approval ever parked: malformed input (no
  `mode` field); the *current* mode is `auto` (refused outright,
  unconditionally — an unattended run cannot escalate its own authority,
  which is the property that makes `auto` safe to leave running); the
  *target* is `auto` (refused from any current mode — asking to switch off
  supervision is not a request anyone should be one keystroke from
  granting); or the pair doesn't widen (only `research`→`plan`,
  `research`→`build`, `plan`→`build` widen — narrowing is `/mode`'s job, not
  something the model requests on its own behalf). On approval it applies
  the switch exactly as a live `InMsg::SetMode` always has (cascading over
  the session's spawn sub-tree) and replies at once, continuing the same
  turn; on rejection the typed reason folds back so the model can adjust its
  ask.
- **`general`/`debug` — the two built-in spawn targets, at a glance:**
  `general` (the old `build` body, unchanged) is the **default** `agent`
  target (`DEFAULT_SUBAGENT`/`DEFAULT_AGENT = "general"`) when the caller
  omits `agent` — the safe, unscoped-delegation choice, and any agent may
  spawn it or be spawned by it now (there is no more `primary`/`subagent`
  distinction to gate that). `debug` carries a shorter fix-and-verify body
  — full read/write/execute posture is a property of the session's **mode**,
  not the agent, so nothing distinguishes `debug` from `general` beyond its
  name, description and prompt wording; it survives mainly so a caller can still name it
  explicitly (`{"agent": "debug", ...}`) for a debugging persona, e.g. one
  spawned under `build`/`auto` mode to reproduce, fix, and verify a bug
  (compile, run tests).
- **No startup warning for a stale tool name in a mode rule (a gap left by
  [ADR-0207](../adr/0207-permission-modes-replace-agent-borne-authority.md)):**
  [ADR-0166](../adr/0166-migration-note-and-startup-warning-for-stale-tool-names.md)'s
  `tool_names::is_recognized_mask_entry`/`agents::warn_unrecognized_mask_entries`
  — a startup `tracing::warn!` for any agent-mask entry or `permission:` rule
  key that matched no known tool name, e.g. a config written against a tool a
  later rename removed — is deleted along with the mask it warned about, and
  nothing replaced it for the new surface: `mode::tune::apply` treats a
  tuning rule naming an unrecognized tool as carrying **no capability**
  (`capability_of` returns `None`), which the tuning guard (above) then
  simply never rejects — a config `modes: {research: {allow: ["bash_output(*)"]}}`
  written against a since-renamed tool loads silently and grades nothing,
  with no diagnostic at all. Persisted grants (`grants.yml`) stay unchecked
  as before: a stale grant key is inert by construction (exact-match against
  a live call), so nothing degrades quietly there.
- **In-app tool-allowlist editing is retired, not replaced (✅ #330 →
  [ADR-0083](../adr/0083-in-app-tool-allowlist-editing-as-user-layer-materialization.md),
  amended by [ADR-0207](../adr/0207-permission-modes-replace-agent-borne-authority.md)
  §13):** the old TUI checklist (`tui::tools_dialog::ToolsDialog`, opened
  with `e` on the `/agent` picker's highlighted profile) wrote a per-agent
  `tools:`/`disallowed_tools:` frontmatter override via
  `agents::materialize::save_tools_override` — both the dialog and that
  writer are deleted along with the mask they edited. ADR-0207's own
  amendment describes the replacement as "in-app editing now targets the
  config `modes:` block," but as of this stage that writer does not exist:
  `config.yml` `modes:` tuning is hand-edited only, with no TUI surface that
  materializes a checklist edit into it (`skutter inspect modes` is
  read-only; the `/set` dialog's tools tab is explicitly left for a later
  change, ADR-0207 §12). The closest surviving *live* editing mechanism is
  the session tool overlay (`/enable`/`/disable`/`/tools`, above) — but it is
  ephemeral and mode-scoped, not a persisted config edit the way the old
  mask materialization was.
- **Spawn bounds come from the mode, not the agent** (✅ #119
  [ADR-0040](../adr/0040-per-profile-spawn-control.md), superseded by
  [ADR-0207](../adr/0207-permission-modes-replace-agent-borne-authority.md)
  §6): the old per-profile `can_spawn`/`spawnable_agents` quartet is gone —
  **any agent may spawn any registered agent now**, and `agent`/`agent_send`
  are `Capability::Control`, so spawning is never permission-graded at all.
  What bounds it instead are two mode facts, applying to the session's whole
  spawn sub-tree: `max_depth` (nesting, root = 0) and `max_agents`
  (concurrent children per root) — undefined means unlimited, exceeding
  either returns an `is_error` naming the limit
  (`entanglement-runtime/src/subagent/spawn_guard.rs`). A child counts from
  its spawn until its answer arrives (a `SpawnSlot` held by its launch task),
  so a session delegating one task at a time never runs out; an idle child
  re-woken by `agent_send` is not counted again. `runtime::permission::spawn_refusal(target,
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
- **Task state tool (✅ #231, [ADR-0049](../adr/0049-plan-task-tools-as-runtime-state-tools.md)):**
  `update_tasks` is a **runtime** state tool, not a core built-in. It replaces
  the session's *display* task outline; the runtime executor emits the
  `OutEvent::TaskList` snapshot (a fresh per-session seq, #157) and acks the
  model — the engine holds no task state. It rides `Capability::Control`
  ([ADR-0207](../adr/0207-permission-modes-replace-agent-borne-authority.md)
  §3), so it is now **never graded**: it runs (and acks) unconditionally in
  every mode, including `research`/`auto`, since it touches only session
  bookkeeping, not the host or a file — a real posture change from before
  ADR-0207, when a read-only `explore` had it masked out *and*
  permission-denied (#175). It rides the shared `tool_specs`, advertised to
  every session identically.
- **One plan tool — `propose_plan` (✅ #141/#513, [ADR-0042](../adr/0042-plan-acceptance-via-propose-plan-approval-roundtrip.md)/[ADR-0145](../adr/0145-one-plan-tool-file-backed-plans-and-blocking-review-loop.md),
  amended by [ADR-0207](../adr/0207-permission-modes-replace-agent-borne-authority.md)
  §7, which **retires** [ADR-0138](../adr/0138-sponsored-build-child-and-propose-plan-cycle.md)
  wholesale — no sponsored child, no permission root, no blocking
  report fold-back):**
  `update_plan` is **gone** — `propose_plan(content: Option<String>, path:
  Option<String>)` is the sole plan-authorship tool, now gated by
  `Capability::Plan`: advertised **unconditionally** to every session, but
  usable only where a mode grades `plan` (or the literal `propose_plan` tool)
  `Allow`/`Ask` — `plan` mode alone in the built-in table (§the four built-in
  modes above). A mode's `Deny` on the `plan` class declines the call flat,
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
  `OutEvent::PlanChanged` (#627, ADR-0173) — see [engine](engine.md) for the
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
  [ADR-0228](../adr/0228-rhai-path-and-args-skill-tools-frontmatter-removed.md)
  `auto` runs everything except its deny list and its prompt list of
  destructive deletions, bounded by its `max_turns`/`max_duration` budgets
  and `question_timeout` — an approver who wants each unlisted call seen
  picks `build`.
  `propose_plan` itself gains an optional `mode` argument (`"build"`/`"auto"`
  only) letting the model *suggest* which option the prompt pre-selects — a
  suggestion never decides, only the approver's own keystroke does. This is
  the deliberate asymmetry with `request_mode` below, which refuses `auto` as
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
  Built-in `plan` mode (§the four built-in modes above) is physically
  read-only apart from one carve-out (#524,
  [ADR-0142](../adr/0142-trusted-scratch-dir-and-plans-folder-carve-outs.md)):
  `deny: [write]` plus the argument-scoped `write(.entanglement/plans/*.md):
  allow` — out-ranking the class deny by longest match — grades every write
  outside `.entanglement/plans/*.md` as `Deny`, the opencode-style
  plans-folder exception `propose_plan`'s `content` mode writes into,
  everything else stays physically unreachable. Exec in `plan` mode gets
  the same shared read-only exec set as `research`/`auto` (spliced in from
  `readonly_exec.yml`, above); any other command falls through to `default:
  prompt`, matching the old `plan` agent's `call(*): ask`. Because mode now
  applies uniformly to the *whole* spawn sub-tree (§6) there is no more
  ancestor-clamp-intersection concern to work around: the old `plan.md` mask
  had to carry `call`/`bash` explicitly just so a spawned `explore` child's
  own shell access survived the intersection — that entire class of
  workaround (mask fields carried by a parent purely to avoid clamping a
  child) no longer exists, since a child inherits the exact same mode, not
  an intersection of two masks.
- **System-prompt assembly (✅ #113, [ADR-0035](../adr/0035-deterministic-system-prompt-assembly.md)):**
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
  roster that used to sit here moved to `list_skills`, [ADR-0226](../adr/0226-one-advertised-surface-mcp-envelope-registered-is-available.md)) + **preloaded skill bodies** (frontmatter
  `skills: [name, …]`, ✅ #117, below). Inputs come from `PromptContext::load(root)`
  (preamble overridable via `ENTANGLEMENT_PREAMBLE_FILE`; brief via
  `ENTANGLEMENT_BRIEF_FILE`). A **subagent** gets `preamble + body (+ brief)` +
  any preloaded bodies — and never the parent's assembled
  prompt (each agent is composed from *its own* body + `include_brief` flag).
  Composition is a pure, unit-tested harness function baked into
  `Agent.system_prompt` at load time, so session start / spawn
  both read the finished prompt and core stays a verbatim pass-through into
  `LlmRequest.system` — there is no more live `SetAgent` to re-read it on
  ([ADR-0207](../adr/0207-permission-modes-replace-agent-borne-authority.md)
  §9). `list_skills` reads the skill registry (✅ #114, below) and lists every
  discoverable skill regardless of session mode.
  **Per-turn prompt override (✅ #310, [ADR-0078](../adr/0078-per-turn-dynamic-system-prompt.md)):**
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
  today's date ([ADR-0202](../adr/0202-prompt-cache-discipline-anchors-deferral-replay-compaction-date.md)).
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
  read only `name`+`description`, ignoring every other key — Claude's
  `disable-model-invocation` and `allowed-tools` included,
  [ADR-0074](../adr/0074-cross-vendor-skill-and-agent-discovery.md)/[ADR-0229](../adr/0229-every-skill-is-model-visible-user-only-removed.md)).
  Frontmatter: `name` + `description` (required) and
  `allowed_tools` (parsed-but-ignored with a one-time load warning — the
  skill-scoped mask it named is removed by
  [ADR-0194](../adr/0194-skills-are-additive-only.md); skills are
  additive-only). The `tools:` key is retired
  ([ADR-0228](../adr/0228-rhai-path-and-args-skill-tools-frontmatter-removed.md)):
  a native `SKILL.md` still carrying it is **skipped** with an error-level log
  naming the replacement (`lib.rhai` functions, or a script file run with
  `rhai` `path`) — never a registry-load abort, so one stale skill cannot brick
  startup; a foreign dir drops it like any unknown key. `user_only` is removed
  ([ADR-0229](../adr/0229-every-skill-is-model-visible-user-only-removed.md)): **every skill is model-visible** — listed, loadable,
  its `lib.rhai` callable from any script; a native `SKILL.md` still carrying the
  key loads normally and warns once per sighting (`config::warn_once`: live
  reloads stay quiet, a removed-then-restored key warns again). A skill the model must not run is a permission-mode question (rules on
  the commands it performs), not a visibility one. Each `SkillMeta` resolves its
  `root_dir` **once** at discovery. **Disclosure is tier-1 only**: `SkillRegistry::disclosures`
  yields one `name: description` row per skill, which `list_skills`
  returns on request ([ADR-0226](../adr/0226-one-advertised-surface-mcp-envelope-registered-is-available.md) — no longer in the system prompt); bodies are never preloaded. **Selection stays
  the model's own reasoning** — no keyword router or embedding gate; the model
  matches its task against the `description` in its forward pass, so description
  quality is the contract. Bodies + payload (`references/`/`scripts/`) are tier-2,
  loaded on demand (`load_skill`, ✅ #115, below).
- **Tier-2 skill loading (✅ #115, [ADR-0037](../adr/0037-load-skill-tool-deterministic-resolution.md)):**
  one generic `load_skill { skill_name }` tool (not one-per-skill) resolves a
  skill's body on demand. Unlike the orchestration-only runtime tools
  (`agent`/`agent_send`/`ask_user`/`poll` — `Capability::Control`), it
  **reads the filesystem**, so it is a *real host tool* in the `ToolRegistry`
  (`entanglement_runtime::skills::load_skill::LoadSkillTool`, holding a
  shared `Arc<SkillRegistry>`), declares `Capability::Read`, and grades
  through the session's permission mode exactly like `read` — with no
  orchestration-tool exemption. It is therefore **allowed** in every one of
  the four built-in modes (each class-allows `read`, and `load_skill` shares
  that class): unlike the old read-only `explore` agent, which masked
  `load_skill` out entirely, a read-only *mode* today still lets a model
  load a skill's body — reading a skill is not writing. The handler resolves **deterministically** (never model reasoning):
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
  `///`/`/** */` doc comment ([ADR-0228](../adr/0228-rhai-path-and-args-skill-tools-frontmatter-removed.md)),
  then `available_refs` (supporting files listed as absolute paths, **not**
  loaded; a `.rhai` file among them runs via `rhai` `path`) — never a spoofed
  user message, so the authorship trail stays honest.
- **Skill libraries — `lib.rhai` (✅ #819, [ADR-0227](../adr/0227-one-binding-path-namespaces-http-skill-libraries.md)):**
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
  ([ADR-0228](../adr/0228-rhai-path-and-args-skill-tools-frontmatter-removed.md)).
  Writing a skill's `SKILL.md`/`lib.rhai` (any user or project skill dir)
  always asks, in every mode — the trusted-code write floor
  ([gates & host tools](gates-and-host-tools.md)).
- **Skills are additive-only — the `allowed_tools` mask is removed (✅ ADR-0194,
  [0194](../adr/0194-skills-are-additive-only.md), superseding
  [ADR-0106](../adr/0106-skill-scoped-allowed-tools-enforcement.md) and retiring
  [ADR-0129](../adr/0129-thread-the-skill-mask-into-rhai-binding-resolution.md)):**
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
  [protocol](protocol.md)). `SkillFrontmatter.allowed_tools` is
  parsed-but-ignored with a one-time load warning (`SkillFrontmatter` is
  `deny_unknown_fields`, so the key must stay known; hard removal later).
  The foreign (cross-vendor) layer already drops it, exactly as it drops
  Claude-style `allowed-tools`. The honest cost, recorded in the ADR: skill
  authors lose their only self-imposed guardrail — and, since
  [ADR-0207](../adr/0207-permission-modes-replace-agent-borne-authority.md)
  retired the per-agent `tools:` mask this bullet originally pointed an
  author at too, **the only standing control left is the session's
  permission mode**: an author wanting a restricted session picks a
  stricter mode (or tunes one via `config.yml` `modes:`), never an agent
  definition or skill frontmatter — neither carries authority any more.
- **Skill preload vs access — two independent mechanisms (✅ #117, [ADR-0043](../adr/0043-skill-preload-vs-access-independent-mechanisms.md)):** an agent
  definition controls skills along two orthogonal axes, deliberately *not* merged
  (merging loses expressiveness). **Preload** is `skills: [name, …]` frontmatter:
  the listed skills' full bodies are injected into that agent's assembled system
  prompt at load, through the *same* substitution pipeline as `load_skill`
  (`SkillRegistry::preload_body` → `load_skill::render_skill`) — it is preload
  *only*, never an allowlist, and is mode-independent (a spawned subagent gets the
  body even though its tier-1 index is withheld). Unlike the model-facing
  `load_skill`, an unknown name is a loud load-time error.
  **Access** is now the session's permission mode, not an agent field: since
  `load_skill` carries `Capability::Read`
  ([ADR-0207](../adr/0207-permission-modes-replace-agent-borne-authority.md)
  §3), it grades exactly like `read` — a mode that denies the `read` class,
  or a session tool overlay `/disable load_skill`, blocks it at dispatch; the
  schema stays advertised either way, so the refusal is always visible, never
  a silent absence. The two axes still compose to preserve both corners:
  "preload X but block everything else" (`skills: [x]` in the agent
  definition + a mode/overlay denying `load_skill`) and "preload nothing,
  request on demand" (no `skills:`, `load_skill` reachable). Every built-in
  mode class-allows `read`, so the default is permissive — a subagent may
  discover and load any skill exactly like its parent, unless the mode or an
  overlay says otherwise.
- **User slash commands — prompt templates (✅ #733, [ADR-0221](../adr/0221-user-slash-commands-are-head-side-prompt-templates.md)):**
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
  **runtime** concern ([ADR-0003](../adr/0003-agent-and-permission-profiles.md) /
  [ADR-0010](../adr/0010-single-head-crate-and-bash-opt-in.md), authority
  itself moved to the mode by
  [ADR-0207](../adr/0207-permission-modes-replace-agent-borne-authority.md)).
  Core emits `ToolExec` for *every* host tool — the whole batch up front
  since #270 ([ADR-0061](../adr/0061-parked-turn-state-batch-tool-resolution.md))
  — and parks the turn as explicit `TurnState` until each `ToolResult` lands
  (§8); it carries the opaque `mode: String` and never evaluates it, exactly
  as it never read `PermissionProfile` before. The runtime's
  `ModeResolver` (`policy.rs`) tracks each session's live permission mode
  against a `ModeTable` it holds, resolves the grade through
  `Mode::resolve`'s longest-match engine, and — for `Ask` — emits the
  `ToolRequest` prompt and awaits `Approve`/`Reject`/`Stop`, so every head
  stays a thin protocol adapter (it just sends the same frames; the runtime,
  not core, acts on them).
- **Authoritative gating, fail-closed (✅ #156, [ADR-0070](../adr/0070-authoritative-tool-exec-profile-and-fail-closed-fallback.md),
  carried into [ADR-0207](../adr/0207-permission-modes-replace-agent-borne-authority.md)
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
