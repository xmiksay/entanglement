# Agents & permissions §3 — Advertisement, enforcement & discovery

> Part of the [agents & permissions module](../agents-and-permissions.md) of the [architecture overview](../../architecture.md).

- **Advertisement is universal and session-stable — enforcement lives entirely
  at dispatch** (✅ #116/#560, [ADR-0038](../../adr/archive/0038-physical-per-agent-tool-restriction.md)/[ADR-0148](../../adr/archive/0148-glob-patterns-in-the-agent-tool-mask.md)/[ADR-0192](../../adr/archive/0192-universal-advertisement-enforcement-at-dispatch.md)/[ADR-0198](../../adr/archive/0198-out-of-mask-tool-calls-are-approvable.md),
  all superseded by [ADR-0207](../../adr/archive/0207-permission-modes-replace-agent-borne-authority.md)):
  there is no more agent tool mask to decouple from advertisement — core's
  turn loop (`run_round`) advertises `EngineConfig.tool_specs` (or the
  resolver's replacement, below) **verbatim**, with no filter of any kind.
  WHY (unchanged from ADR-0192/0038): any mid-session change to the tools
  array busts the provider's prompt cache from the tools block onward, so a
  surface that is stable **within** a session keeps that cache warm; what is
  traded away is only "the model cannot even attempt the call" — every
  attempt is now visibly graded instead, and no capability changes hands.
  The universal, session-stable surface is the compiled-in `KERNEL`
  ([ADR-0226](../../adr/archive/0226-one-advertised-surface-mcp-envelope-registered-is-available.md); see [engine](../engine/turn-loop.md) §turn loop): `read`/`edit`/`write`/
  `apply_patch`/`bash`/`web_fetch`, `rhai` when the feature is on,
  `update_tasks`, `ask_user`, `poll`, `propose_plan`, `request_mode`,
  `agent`/`agent_send`, the `mcp` envelope, and the six discovery tools
  `list_agents`/`list_models`/`list_skills`/`load_skill`/`list_mcp`/
  `mcp_tools`. The discovery tools are **non-maskable**: no mode rule or
  ceiling rule can withdraw them even at dispatch (the ADR-0190 `poll`
  pattern; a config rule naming one warns, #843). They are not the only
  ones — the full non-maskable set (`tool_names::NON_MASKABLE_TOOLS`,
  §non-maskable names below) also holds the bookkeeping Control tools, and `agent`/`agent_send`
  are deliberately outside it ([ADR-0232](../../adr/archive/0232-spawning-is-graded-bookkeeping-control-tools-non-maskable.md)). `glob`/`grep` stay registered as
  script-only `rhai` bindings, graded under their own names. With `Agent` carrying no more authority, **nothing
  varies the array by agent any more** — `agent_specs()` is provably
  identical regardless of which session asks (any agent may spawn any agent
  now, bounded only by the mode's `max_depth`/`max_agents`, neither of which
  is schema-visible), and `propose_plan`/`request_mode` are pushed into the
  base `tool_specs` unconditionally rather than gated per profile. The one
  acknowledged dynamic seam left is **MCP tools** (`mcp__*`), since a
  server's tools are unknowable until it connects. **Per-session base specs
  (✅ #308, [ADR-0076](../../adr/archive/0076-per-session-dynamic-tool-specs.md)):** an
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
  ([ADR-0207](../../adr/archive/0207-permission-modes-replace-agent-borne-authority.md)
  §4, retiring [ADR-0198](../../adr/archive/0198-out-of-mask-tool-calls-are-approvable.md)'s
  mask-miss approval ladder — "there is no mask left to miss"): a call grades
  through the session's permission mode (§permission modes in [modes](modes.md)) before it
  runs. `Ask` emits `ToolRequest` and parks an ordinary approval — the
  outcome ADR-0198 generalized *every* refusal into. `Deny` is now
  **absolute** again, but for a different reason than the pre-ADR-0198 mask:
  it names the mode and the way out (`` tool `x` denied by mode `research` —
  use /mode to switch ``) rather than a per-authority attribution table,
  because there is only ever **one** authority to attribute a `Deny` to — the
  session's own mode (uniform over its whole spawn sub-tree, §6) or the
  config ceiling clamping it further — the mode is the one permission path.
  The bookkeeping
  `Capability::Control` tools bypass this whole ladder (§permission modes
  in [modes](modes.md)) — never graded, never declined for authority reasons. `agent`/
  `agent_send` climb it before launching like any tool ([ADR-0232](../../adr/archive/0232-spawning-is-graded-bookkeeping-control-tools-non-maskable.md)):
  a mode `Deny` refuses with an error result and starts
  no child, a `prompt` rule parks an approval card; past the ladder they are
  bounded by the mode's `max_depth`/`max_agents` (an `is_error`) and refused
  for an unknown target or model.
- **Non-maskable names** (#843, `tool_names::NON_MASKABLE_TOOLS`): names a
  rule can spell but never match. `mcp` (core unwraps the envelope to
  `mcp__<server>__<tool>` before dispatch — per-tool control is a
  `mcp__<server>__<tool>` rule, and a server the user never wants is removed
  from config), the six discovery tools (always available, not maskable —
  below), the script-only read variants `read_raw`/`glob_json`/`grep_json`
  (graded as `read`/`glob`/`grep`) and the **bookkeeping Control tools**
  (#849, [ADR-0232](../../adr/archive/0232-spawning-is-graded-bookkeeping-control-tools-non-maskable.md)): `poll`,
  `ask_user` and `update_tasks` only collect or report work the mode
  already authorized ([ADR-0190](../../adr/archive/0190-poll-is-always-on-non-maskable-internal-tool.md)'s
  argument — withdrawing one strands work, it never reduces capability);
  `request_mode` only asks the user for more authority (always an approval
  card, refused in `auto`); `propose_plan` is gated by its `plan` class
  alone — the rule matcher skips a rule *naming* any non-maskable tool, so
  `deny: [plan]` (the built-in `research`/`build`/`auto` rule) is what keeps
  plan authorship in `plan` mode, and that class rule never warns. `agent`/
  `agent_send` are ordinary names: a mode `deny: [agent]` stops spawning. A
  `modes:` or `permissions:` rule naming a non-maskable tool warns at config
  load like a stale name (`config::mode_warn`), as does a rule whose tool
  part is a name glob (rule keys never glob the tool part; only a bare `*`
  is a wildcard), that scopes a tool with no argument (`mcp__gh__x(foo)`,
  #878) or with no workdir (`read{/tmp/*}` — only `bash` has one, #881), or
  whose scope the matcher can't read (`bash(git *){/x}` — one scope per
  key, #881). A per-tool rule (`mcp__<server>__<tool>`) grades an envelope
  call and a script's `mcp::<server>::<tool>()` binding identically — both
  reach the same dispatch seam under the inner name.
- **`/tools`** (#560 P9,
  [ADR-0199](../../adr/archive/0199-session-tool-listing-and-enablement-drive-advertisement.md))
  is a read-only *browser* over the session's whole reachable surface (host
  tools, MCP servers + their tools, skills, endpoints via
  `discover::index_rows`): each row shows its kind and advertising status
  (`advertised` when the name is in the session's surface snapshot, `hidden`
  otherwise — script-only tools, MCP tools reached through `mcp` — `n/a`
  before the first round; from the same `AdvertisingState` the resolver
  writes); a typeahead filter plus a `Tab`-cycled category narrow the list.
  What a session may call is its mode's decision, not the view's.
- **Discovery is six plain tools** ([ADR-0226](../../adr/archive/0226-one-advertised-surface-mcp-envelope-registered-is-available.md), amending
  [ADR-0207](../../adr/archive/0207-permission-modes-replace-agent-borne-authority.md)
  §12's `explore`/`describe` kinds): `list_agents` (name, description, model
  pin), `list_models` (the usable catalog: provider/id, display name, context
  window), `list_skills` (name + description), `load_skill`, `list_mcp` and
  `mcp_tools` (see [gates & host tools](../gates/mcp.md) §MCP) — each
  with no argument or one `name`, all `Capability::Control`, always-on,
  non-maskable ([ADR-0190](../../adr/archive/0190-poll-is-always-on-non-maskable-internal-tool.md)
  pattern), available in every mode. The old `modes` kind is covered by the
  static modes preamble in the system prompt; the old `pending` kind's
  in-flight recovery is the handle-less `poll` (a session's own outstanding
  jobs, scripts and sub-agents, then, when non-empty, its sub-tree's retained
  outputs, open questions and parked approvals, #829).
- **`request_mode` — a blocked model's escape hatch**
  ([ADR-0207](../../adr/archive/0207-permission-modes-replace-agent-borne-authority.md)
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
  [ADR-0207](../../adr/archive/0207-permission-modes-replace-agent-borne-authority.md)):**
  [ADR-0166](../../adr/archive/0166-migration-note-and-startup-warning-for-stale-tool-names.md)'s
  `tool_names::is_recognized_mask_entry`/`agents::warn_unrecognized_mask_entries`
  — a startup `tracing::warn!` for any agent-mask entry or `permission:` rule
  key that matched no known tool name, e.g. a config written against a tool a
  later rename removed — is deleted along with the mask it warned about, and
  nothing replaced it for the new surface: `mode::tune::apply` treats a
  tuning rule naming an unrecognized tool as carrying **no capability**
  (`capability_of` returns `None`), which the tuning guard ([approvals & grants](approvals-and-grants.md)) then
  simply never rejects — a config `modes: {research: {allow: ["bash_output(*)"]}}`
  written against a since-renamed tool loads silently and grades nothing,
  with no diagnostic at all. Persisted grants (`grants.yml`) stay unchecked
  as before: a stale grant key is inert by construction (exact-match against
  a live call), so nothing degrades quietly there.
- **In-app tool-allowlist editing is retired, not replaced (✅ #330 →
  [ADR-0083](../../adr/archive/0083-in-app-tool-allowlist-editing-as-user-layer-materialization.md),
  amended by [ADR-0207](../../adr/archive/0207-permission-modes-replace-agent-borne-authority.md)
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
  change, ADR-0207 §12). There is no live per-session editing mechanism
  either: `/mode` is the one live control.

---

Previous: [§3 — Agent definitions, built-in modes, pinning & live reload](definitions.md) · Next: [§3 — Spawn bounds, task state & the plan tool](spawn-tasks-plans.md)
