# Embedding Migration notes

> Part of the [embedding guide](../embedding.md).

## Migrating to one advertised surface (ADR-0226, #818)

[ADR-0226](../adr/0226-one-advertised-surface-mcp-envelope-registered-is-available.md) removed the advertising modes and MCP's three states. What an
embedder has to change:

- **Removed types/consts:** `ToolAdvertising`, `Discovery`, `INVOKE_TOOL`,
  `TOOL_SEARCH_CALL_TOOL`, `McpServerState`, `ToolSpec.defer_loading` (drop
  the field from any struct literal), `tool_advertising::AdvertisingInputs`
  (and its pin/repin/encoding API), `mcp::McpEnableTool`, `mcp::connect` (there
  is no startup connect), `McpScopes::overlay_specs` (MCP schemas are never
  advertised).
- **Added:** `MCP_TOOL` (`"mcp"`, re-exported by core) — the envelope name
  core unwraps; `discover::{mcp_spec, list_*_spec, mcp_tools_spec}`;
  `mcp::McpToolCache` (the managed `mcp-tools.yml`); `McpScopes::rows`.
- **Renamed/reshaped:** `AvailableMcp::partition(..) -> (startup, roster)` →
  `AvailableMcp::from_config(..) -> roster`. It reads OAuth credentials from
  the managed `mcp-tokens.yml` by default (`.with_token_store(..)` pins
  another store), but records no last-seen tool names until you chain
  `.with_tool_cache(McpToolCache::managed())` (as `skutter` does); the free
  `mcp::needs_auth` → `AvailableMcp::needs_auth`; `ToolExecutorBuilder::
  advertising_inputs(..)` → `.catalog(Arc<Catalog>)` (the deprecated
  `spawn_tool_executor_with_policy` takes `catalog: Option<Arc<Catalog>>` in
  the same slot); `subagent::agent_specs()` takes no argument (the spec is
  session-invariant); `tool_advertising::surface::SurfaceSources` is
  `{ tools, advertising, agent_specs }`; `PromptContext.skills` →
  `tool_conventions: Option<&'static str>` (the skill roster left the prompt
  for `list_skills`); `McpServerConfig.state` → `description` (`disabled` is
  the one opt-out); `ProviderMcpServer.state` → `description` + `disabled`.
- **Retired config/catalog keys** (ignored with one warning each):
  `config.yml` `tool_advertising:`, `mcp.<name>.state:` (and `discovery:`,
  already retired by [ADR-0213](../adr/0213-prune-env-vars-and-catalog-knobs.md));
  `providers.yml` provider/model `discovery`/`tool_advertising` and a bundled
  server's `state`.
- **Wire:** `McpServerStatus.state` stays serde-defaulted for old logs but is
  always `None`; heads should read `connected` and `auth`.
  `OutEvent::McpChanged` gains a serde-defaulted `error: Option<String>` — set
  when an `McpAdd`/`McpRemove` was refused or failed (a match on the variant
  needs `..` or the new field). Old logs carrying
  `invoke` envelopes still deserialize and replay.

## Migrating to one binding path (ADR-0227, #819)

[ADR-0227](../adr/0227-one-binding-path-namespaces-http-skill-libraries.md) routes every `rhai` binding through the executor's dispatch seam
and removes the declared endpoint tools. What an embedder has to change:

- **Removed:** `script::BindingPolicy` (bindings grade through the executor's
  own resolver, overlay, ceiling, grants and hooks — nothing to wire);
  `entanglement_runtime::endpoint` (`EndpointConfig`, `EndpointTool`,
  `register_endpoints`); `entanglement_core::{call_endpoint, EndpointMethod,
  EndpointResponse, ENDPOINT_RESPONSE_CAP}` and the provider's `endpoint`
  module; `Config.endpoints`; the skill-tool `endpoint` kind.
- **Reshaped:** `skills::tools::register_skill_tools(&mut tools, &skills)`
  drops its `HttpClient` argument; `SkillMeta` gains `lib:
  Option<SkillLib>` (add `lib: None` to a struct literal); `Config` gains
  `http_auth`.
- **Added:** `http_gate::HttpGate` + `ToolExecutorBuilder::http_gate(..)` —
  the builder registers the `http` tool (behind `rhai`; `HttpTool::new` is
  crate-private) over that same gate, so injection and the forced per-host
  approval share one source; without it `http()` is unbound in scripts.
  Loading a `Config` (and building a gate) scrubs every `${VAR}` its
  `http_auth` references from every child's environment (`secret_env`); the
  managed `.env` file stays readable by exec tools (#832);
  `WebFetcher::request` (any method, per-hop `check_hop`/`credentials` seams)
  and `web_fetch::{HttpRequest, HTTP_METHODS, parse_method, scheme_and_host}`;
  `rhai_ident::{rhai_ident, mcp_path}`.
- **Behaviour:** hooks and `FileChange` now fire for a script's nested calls;
  a script's approval covers an identical repeat only (tool + full input);
  a credentialed `http()` call asks once per host per session in every mode
  and is refused in an unattended one without an explicit `http(...)` rule.
- **Retired config key:** `config.yml` `endpoints:` (ignored with one
  warning) — express an HTTP API as a skill `lib.rhai` function calling
  `http()`, with credentials in the user-layer `http_auth:`.

## Migrating to `rhai(path)` and skill tools' removal (ADR-0228, #820)

[ADR-0228](../adr/0228-rhai-path-and-args-skill-tools-frontmatter-removed.md)
gives `rhai` a `path`/`args` input and removes skill `tools:` frontmatter.
What an embedder has to change:

- **Removed:** `skills::tools` (`register_skill_tools`, `SkillToolDef`) and
  `SkillMeta::tools` (drop `tools: Vec::new()` from a struct literal);
  `Tool::alias_rewrite` (an impl overriding it no longer compiles — nothing
  consults it); the `skill__*` advertisement exception.
- **Reshaped:** `skills::lib_rhai::SkillLib::signatures: Vec<String>` →
  `functions: Vec<LibFn { signature, doc }>`; `rhai`'s schema no longer
  requires `script` (exactly one of `script`/`path`, checked at dispatch).
  `InMsg::Approve` and `seam::Decision::Approve` gain `run_all: bool` (a
  struct literal needs `run_all: false`; the wire field is serde-defaulted, so
  old frames parse); `Config` gains `rhai_trusted`
  (`rhai_trust::TrustedFolders` — `project`, `user`, the modes where the
  user layer's trust is suppressed, and `user_suppressed_everywhere` — a
  project `permissions:` `prompt` list or `default: prompt`).
- **Added:** `ToolExecutorBuilder::rhai_trusted(folders)` (a
  `TrustedFolders`, or a plain `Vec` of canonical paths trusted like the
  project layer; `Config::rhai_trusted` is what `config.yml` `rhai.trusted`
  resolves to), `ToolExecutorBuilder::skill_dirs(dirs)` (the skill-file
  write floor — `skills::skill_dirs(root, &UserLayer::FromEnv)`; none wired,
  no floor) and `skills::rejected_skills`.
- **Behaviour:** the `rhai` launch is graded like any tool — overlay
  (deny and enable), mode rules on the script's canonical `path`
  (`permission_arg` returns it; `rhai(scripts/*)` rules work, an inline
  script matches only a bare `rhai` rule), ceiling, grants, `pre_tool_use`.
  Deny refuses (trusted paths too); Allow runs with no card, Ask per
  command; Ask parks one approval card (`ApprovalKind::Rhai`, tool `rhai`,
  the script source as its input) — a head must offer **Run all**
  (`Approve{run_all: true}`) and **Ask per command** (`run_all: false`, also
  what an older head's plain approve means), and may offer a scope: a
  `Session`/`Always` approval grants the exact path (never `run_all`, never
  an inline script). With the mode's `question_timeout` set, an unanswered
  card runs the script Ask per command. A trusted script runs with no card
  and its nested calls force-allowed; only overlay/hook/mode/ceiling denies
  stop them — and that includes credentialed `http()` for a project-layer
  trusted folder. An overlay deny entry for `rhai` is an ordinary deny (no
  supervisor `Error` any more). A non-trusted out-of-root `path` warns on
  the card (`ToolRequest.reason` is `launch_escape`, ADR-0230); with no
  `.escape_root(..)` wired, **every** out-of-root `path` — trusted or not —
  is refused outright. `skutter run` answers a launch card with Ask per
  command and rejects nested cards. Rule precedence changed for every tool,
  in modes and the `permissions:` ceiling: a rule naming the tool outranks a
  capability-class rule (was: longest key, so `deny: [write]` +
  `allow: [edit]` now allows `edit`); a `*` rule competes with a named rule
  by key length and with a class rule by strictness (was: length, so a long
  `*(…)` allow no longer beats a class deny); within each kind the longest
  key wins; ties go to the stricter grade. A scoped `*` allow/prompt is
  refused in a mode class-denying `read`/`write`. The built-in `auto` mode
  now defaults to `allow`, with `prompt` rules for destructive families.
  A `write`/`edit`/`apply_patch` of a `*.rhai` file inside a trusted folder,
  or of a skill's `SKILL.md`/`lib.rhai`, is graded at least `Ask` (in every mode),
  whatever a mode rule, grant or overlay enable says
  — inside a Run-all script too (an explicit deny still denies; `bash` and
  MCP filesystem servers are not covered, so keep trusted folders out of the
  agent's write reach). For every tool, an escape
  approval that also cleared an `Ask` records both grants. A native `SKILL.md` with `tools:` is
  skipped with an error-level log — `load_registry` still succeeds.
  `load_skill`'s result orders body → documented signatures → refs.
- **Skill authors:** a `kind: rhai` tool becomes a script file run with
  `rhai {"path": "<skill_dir>/x.rhai", "args": {…}}`; a `kind: alias` tool
  becomes a `lib.rhai` function calling the wrapped binding with the preset
  values.

## Migrating to every skill model-visible (ADR-0229, #845)

[ADR-0229](../adr/0229-every-skill-is-model-visible-user-only-removed.md)
removes `user_only`. What an embedder has to change:

- **Removed:** `SkillMeta::user_only` (drop `user_only: …` from a struct
  literal).
- **Behaviour:** `SkillRegistry::disclosures` returns every skill, so
  `list_skills` lists it; `load_skill` renders it; its `lib.rhai` is
  callable as `skill::<skill>::*` from any script. A native `SKILL.md`
  still declaring `user_only:` loads with one warning (repeated only if the
  key is removed and later restored); a foreign skill's
  `disable-model-invocation` is ignored. `skutter inspect skills` drops
  its `USER_ONLY` column.
- **Skill authors:** delete `user_only:`. In `auto`, or any mode where the
  skill's commands match no `prompt`/`deny` rule, the model can now pick a
  formerly `user_only` skill, load it and run its `lib.rhai` from an inline
  script with no card — and a `~/.claude/skills` skill marked
  `disable-model-invocation` becomes visible with no warning. To prevent
  that, grade its effects in the permission mode (e.g. a `prompt:` or
  `deny:` rule on the deploy command), remove the skill (or its dir) from
  the discovered skill dirs, or set `ENTANGLEMENT_SKILLS_DIR`, which
  replaces the whole user layer ([ADR-0074](../adr/0074-cross-vendor-skill-and-agent-discovery.md)).

## Structured card reason and honest scopes (ADR-0230, #836/#846)

- **Wire:** `OutEvent::ToolRequest` gains `reason: Option<CardReason>`
  (`root_escape` / `floored_write` / `script_launch` / `inline_launch` /
  `launch_escape`; absent on an ordinary card) and `scopes:
  Vec<ApprovalScope>`. These are the scopes an `Approve` honors, starting with
  `Once`. Both are serde-defaulted and omitted when empty, so an old frame or
  log record still parses (and re-serializes byte-identically); a current
  runtime always sends `scopes`. The runtime enforces them: an `Approve`
  with a scope the card did not offer is clamped to `Once`.
  `PendingApproval` gains `card_reason` under the same rule. Build approval
  options from `scopes` and branch on `reason`; never parse `input`. Treat an
  empty `scopes` (an older producer) as `[Once]`.
- **Behaviour:** a floored write and an inline `rhai` launch now advertise
  `[Once]` only, since neither ever recorded a wider grant.
- **Changed:** `pending::approval` takes a trailing `Option<CardReason>`;
  `pending::scopes_for` takes the card reason too. **Added:**
  `pending::headless_starts(reason)`, the "nobody to ask" decision `skutter
  run` uses. **Removed:** `rhai_trust::LAUNCH_ESCAPE_NOTE`. It was added
  earlier in this release cycle; match on `CardReason::LaunchEscape`
  instead.
- **Constructing `OutEvent::ToolRequest`** (for example, in a test head)
  needs the two new fields: `reason: None, scopes: Vec::new()`.

## Live-only nested-call events (ADR-0233, #821)

- **Wire:** two new `OutEvent` variants, `NestedCall` (one binding call of a
  running `rhai` script: `parent_request_id`, 1-based `index`, `tool`, a
  bounded credential-free `args` summary, `duration_ms`, `is_error`) and
  `NestedCallSummary` (`suppressed`/`errors`: the calls past the 50-per-run
  cap, sent once at the script's end). Both carry `session` but no `seq`.
- **Live only:** `OutEvent::is_live_only()` is true for exactly these two.
  The persistence tap never hands them to a `RecordSink`, and replay ignores
  them, so a sink of your own needs no change. If you persist events outside
  the tap, skip `is_live_only()` events too.
- **Exhaustive matches** over `OutEvent` need the two new arms. Pair a report
  with its script by `parent_request_id`, never `request_id`.
- **Added:** `Holly::emit_live(ev)` broadcasts a live-only event and refuses
  (logs and drops) any other variant.

## Detached cards (ADR-0237, #872)

- **Wire:** `OutEvent::ToolRequest` and `PendingApproval` gain `detached:
  bool`, `true` on a background `rhai` script's nested card. It is
  serde-defaulted and omitted when `false`, so ordinary cards and old logs
  are unchanged. A head that drops its cards on a terminal `Status` should
  keep the detached ones: the script still waits on them, with no timeout
  in an attended mode.
  `Stop` unwinds them too.
- **API:** `PendingDecisions::register_approval(approval, detail)` takes the
  detached flag from `approval.detached`.
- **Constructing `OutEvent::ToolRequest` or `PendingApproval`** needs
  `detached: false`.

## Cards moved after a compaction (ADR-0238, #871)

- **Wire:** after a compaction, a background script's card is announced a
  second time: a fresh `ToolRequest` on the successor, with the same
  `request_id`, content and `detached: true`. `ListPending` then reports it
  under the successor, so a `root`-filtered listing of the successor finds
  it. A head that keeps cards per session should drop the predecessor's
  copy when the successor announces the same `request_id`
  (`SessionStarted.predecessor` names the pair).
- **Answering:** `Approve`/`Reject` reaches the moved card under the
  predecessor's id or the successor's; a cached predecessor id is never a
  dead card, and it never reaches a card the successor parked itself.
  `PendingDecisions::forget(session, id)` now removes only that exact key,
  and only once its waiter is gone.
  `Stop` on the successor unwinds the moved card. A session never shows
  two live cards with one id, so a `ToolRequest` whose id a head already
  queues for that session is the same card: replace it in place.
- **Duplicate parks:** `PendingDecisions::register`/`register_approval`
  let only a turn card supersede a live turn card under the same key (the
  earlier waiter resolves to `Decision::Stop`). Any other park sharing a
  live card's key, or its id on its session, gets an immediate
  `Decision::Reject` with a reason and emits no card.

## A card's end on the wire (ADR-0240, #883)

- **Wire:** a new `OutEvent` variant, `ApprovalResolved { session, seq,
  request_id, outcome }`, with `outcome` one of `approved` / `rejected` /
  `expired` / `stopped` (`ApprovalOutcome`). The runtime emits it whenever a
  `ToolRequest` card leaves `PendingDecisions` — a decision from any head, a
  `question_timeout` expiry, a `Stop`, a session end/hibernate (its turn's
  cards), a superseding re-offer — on the card's home session, always after
  the card. Drop the card by `request_id`, only past your seq watermark for
  that session, and along the session's compaction predecessors if you keep
  per-session copies. An end at seq 0 (its session has no sequence) is folded by id without moving the watermark. `ask_user`
  questions never get one.
- **Persisted:** it is a content event with a seq, so the persistence tap
  logs it and `ReplayFrom` returns it (a seq-0 end excepted: `History` keeps
  only records past `after_seq`); core replay ignores it. A sink of your
  own needs no change.
- **Exhaustive matches** over `OutEvent` need the new arm. A wire consumer
  built against an older `OutEvent` fails to parse the new `kind`: decode the
  `kind` tag first and skip kinds you do not know.
- **API:** `PendingDecisions::announcing(holly)` builds a registry that emits
  it (the executor's own); `PendingDecisions::default()` stays silent. If you
  park through `register_approval` and emit the `ToolRequest` yourself, emit
  it straight after registering, or a `Stop`/timeout in between reports the
  end before the card.

## Heals walk the whole compaction chain (ADR-0242, #888/#887)

- **API:** new `Holly::predecessors_of(id)` returns every session `id`
  succeeded, nearest first, read from the supervisor's directory (so it
  survives lost lifecycle events). It is the backwards counterpart of
  `successor_of`; `lineage_head` is its last element.
- **Answering:** `Approve`/`Reject` now reach a moved card under any session
  it was shown on: the key it was parked under, its current home, or any
  home a move took it off. A head holding a stale copy on a middle link of a
  chain of compactions is never left with a dead card.
- **Registry:** `PendingDecisions` keeps no lineage of its own any more. A
  detached card registered through `register_approval` is homed on its own
  key; only `park_approval_as` homes a detached card on the live end of
  core's chain.

---

Previous: [§5–§7 — Approvals across restart, MCP & multi-user](approvals-mcp-multi-user.md) · Next: [TLS & dependency pinning](tls-and-pinning.md)
