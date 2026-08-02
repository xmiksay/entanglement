# Changelog

All notable changes to this project are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project
follows [Conventional Commits](https://www.conventionalcommits.org/) — the
`<type>(<scope>): <subject>` history is the source these entries summarize.
Versioning is [Semantic Versioning](https://semver.org/). The *why* and rejected
alternatives behind each design decision live in the ADRs under
[`docs/adr/`](docs/adr/); the referenced `ADR-####` tags link there.

> **Changelog policy:** this file is written **once per release**, in the
> release commit — generated from `git log <last-tag>..HEAD` and the release's
> closed issues (see [`docs/releasing.md`](docs/releasing.md)). Feature/fix PRs
> must never edit it; there is deliberately no `[Unreleased]` section to append
> to, because concurrent PRs each extending one conflict on every merge.

## [0.6.0] - 2026-08-02

A major feature release: multi-user mode, MCP OAuth, provider-bundled MCP
servers, the plan tool + sponsored build child, per-purpose aux models, settable
session metadata, pause/resume, shared endpoint state across instances, wire-
visible throttle transitions, per-model concurrency caps, per-session tool
overlay, glob patterns in agent tool masks, and ask_user list/retract/replace.
Plus a robustness-fix batch (TUI UTF-8 panic handling, unbounded MCP buffer
caps, enable TOCTOU race fix, registry cycle detection, detached task Holly
clone abort at shutdown, Retry-After overflow clamping, mutex-poison labeling,
reqwest builder failure propagation, built_in_registry parse failure surfacing).

> **Wire-shape changes:** All backward-compatible — new `InMsg`/`OutEvent`
> variants are additive, and new fields carry `#[serde(default)]`/`skip_serializing_if`
> so old logs/clients still deserialize.
> - New `InMsg` variants: `PauseSession`, `ResumeSession`, `ListQuestions`,
>   `RetractQuestion`, `ReplaceQuestion`, `SetSessionMeta`, `SetToolOverlay`,
>   `McpAuth`.
> - New `OutEvent` variants: `Throttle`, `QuestionList`, `SessionMetaChanged`,
>   `ToolOverlayChanged`, `McpAuthChanged`.
> - New additive fields: `SessionStarted.user`, `Spawn.user`, `SessionInfo.user`
>   (#522 multi-user); `Throttle.waiters` + `shared_leases` (#552/#523);
>   `Plan.path` (#513); `McpServerStatus.auth` (ADR-0153).

### Added

- **Multi-user mode** (#522, ADR-0147): per-user providers, API keys, RPM limits,
  and permissions via an embedder-supplied `UserId` type. The embedder owns user
  creation/deletion and passes a `user` to `Holly::spawn_root`; sessions inherit
  their user from their root (or from their predecessor on resume), and spawns
  carry a `user` field that's ignored for children (they inherit). The runtime
  sides of the API (key store, request limits, MCP tokens, endpoint leases) are
  already keyed by user id; the protocol changes are additive only.
- **MCP server OAuth** (#560, ADR-0153): an optional `oauth:` block on an MCP
  server entry switches it from static-header auth to a browser-obtained bearer
  token via RFC 9728/RFC 8414/RFC 7591 discovery + dynamic client registration +
  PKCE S256. New `InMsg::McpAuth { name, action: Connect|Check|Disconnect }` /
  `OutEvent::McpAuthChanged` are trusted-only (wire-refused), sharpening
  ADR-0124. Credentials persist in managed `mcp-tokens.yml`. A `/mcp connect
  <name>` TUI command launches the browser.
- **Provider-bundled MCP servers** (#542, ADR-0152): the provider catalog can
  advertise built-in MCP servers under a new `mcp_servers` map (keyed by name,
  value is an `McpServerSpec`), enabled by default but disabled by a
  `disabled: true` list or the existing server-level `disabled`. A three-state
  enablement model (enabled/allowed/available-unconnected) rides in
  `McpServerStatus`. Startup auto-connects all enabled servers; a `/mcp enable
  <name>`/`disable <name>` pair toggles mid-session.
- **One plan tool — file-backed plans and blocking review loop** (#513,
  ADR-0145): `update_plan` is gone; the unified `plan` tool reads/writes
  markdown files under `.entanglement/plans/` (root-contained, opt-in scratch
  carve-out ADR-0142). A `/plan` command opens the TUI modal; submitting a plan
  spawns a sponsored build child (ADR-0138) that parks until the user approves
  the plan via `propose_plan` — a blocking approval cycle (the head sees a
  parked `WaitingAgent` state, ADR-0139). The build's answer folds back as the
  `plan` tool result, enabling plan/build cycling. Plan mask widened for explore
  (ADR-0159).
- **Per-purpose auxiliary models** (#560, ADR-0154): a managed `aux-models.yml`
  maps purpose → `{provider, model}` for side transformations (`summarize`,
  `session_title`). The runtime-side session-title generator and compaction both
  use cheaper models when pinned, falling back to the primary or session model
  respectively. Compaction honors the aux pin on context overflow (ADR-0103).
  Session-title aux calls defer under contended primary concurrency (ADR-0158).
- **Settable session metadata** (#553, ADR-0151): `InMsg::SetSessionMeta { name?,
  action?, if_unset=false }` merges display metadata onto `Session.name` (a
  session title, e.g. derived from the first prompt) and `Session.action` (what
  the agent is doing now). Applied immediately (never stashed), always acks
  with `OutEvent::SessionMetaChanged` carrying the full merged values. The TUI
  `/name <text>` sets `name` for the active session; the title generator respects
  `if_unset` and never clobbers a `/name` or a name restored by resume.
- **Per-session tool overlay** (#539, ADR-0149): `InMsg::SetToolOverlay { entries:
  [ToolOverlayEntry { pattern, allow, deny }] }` replaces the session's live
  tool overlay — enable entries exist past the agent mask (graded `Ask|Allow`),
  deny entries withdraw even profile-advertised tools. Full replacement, empty
  clears. Trusted-only, wire-refused. Acked by `OutEvent::ToolOverlayChanged`.
  The TUI `/enable`/`/disable` commands send over `Holly::send`.
- **Glob patterns in the agent tool mask** (#537, ADR-0148): the
  `disallowed_tools` denylist now accepts glob patterns (`*`/`?`), so a single
  rule can block a whole tool family (e.g. `*bash*` blocks `bash`/`bash_output`
  and even `call` bindings marshalling to bash). Mask resolution is now a
  dependency-free glob engine in core (`tools::mask`).
- **Pause/resume** (#516, ADR-0144): `InMsg::PauseSession`/`ResumeSession`
  drive a `Session.paused: bool` that gates the next `Prompt`/`SetAgent`/`SetModel`/
  `SetGeneration`/`Oneshot` onto the turn-stash queue (idle case) or holds the
  drained batch's continuation into the next model round-trip (parked case). New
  `AgentState::Paused` rides the TUI ship-cruise. Mid-stream pause is
  unchanged — it rides the existing stash-and-replay mechanism. `Stop`/`Hibernate`
  always win and neither clears it.
- **Shared endpoint state across instances** (#523, ADR-0144 file-backed): two
  `skutter` processes talking to the same `(endpoint, API key)` now share an
  fd-lock-guarded file state under `${data_dir}/entanglement/endpoints/` covering
  the RPM ledger, a lease-based in-flight concurrency count (crash-safe via TTL),
  and the `Retry-After` cool-down. Falls back to pure in-process gating when the
  state directory is unwritable or via `ENTANGLEMENT_NO_SHARED_ENDPOINT_STATE=1`.
  (Note: number collision with ADR-0144 pause/resume; both accepted 2026-07-31.)
- **Wire-visible throttle transitions** (#517, ADR-0141): the provider's per-
  endpoint resilience pool was invisible to stdio/WS heads — a 429 stall showed
  as an opaque `Thinking`. `OutEvent::Throttle { endpoint, throttled, in_flight,
  cap, waiters, shared_leases?, retry_in_ms?, pacing_in_ms? }` joins the protocol
  as an engine-global, no-`seq` lifecycle event emitted only on a transition,
  not every poll. `spawn_throttle_responder` polls every 500ms.
- **Per-model concurrency cap** (#521, ADR-0140): layered on the endpoint cap.
  `ModelEntry` gains an optional `concurrency` catalog entry (YAML-only, no env
  override). `EndpointState` gains a second semaphore per `(endpoint, model)`;
  a request acquires the model permit *first*, then the endpoint permit, so a
  tighter model never starves looser siblings sharing the endpoint. `ThrottleStatus`
  reports whichever cap is tighter.
- **Ask-user list/retract/replace** (#515, ADR-0146): `InMsg::ListQuestions {
  correlation_id, session? }` queries every open `ask_user` question (or one
  session's when `session` is set), answered by `OutEvent::QuestionList {
  correlation_id, questions: [PendingQuestion {session,request_id,questions}] }`.
  `InMsg::RetractQuestion { session,request_id }` withdraws an open question
  without cancelling the turn — the orchestrator still replies with a withdrawal
  note, unlike `Stop`'s silent unwind. `InMsg::ReplaceQuestion { session,request_id,
  questions }` swaps the content in place and re-parks under the same `request_id`.
- **Trusted scratch dir + plans-folder carve-out** (#524, ADR-0142): the
  runtime's own scratch dir (`session_store::scratch_dir`) is consulted first by
  `ExtraRootStore`, so the default `call`-output target needs no approval for
  any tool in any profile. Separately, the built-in `plan` profile adds a
  `write(.entanglement/plans/*.md): allow` rule carving out the plans folder for
  the unified plan tool (ADR-0145) while keeping the rest of the read-only mask.
- **Explore gains Ask-grade shell access** (#522, ADR-0137): the built-in
  `explore` profile widens its mask to `[read,glob,grep,call,bash,rhai]` with the
  exec triad graded `Ask` — each exec call parks at `WaitingApproval` for
  explicit user approval, preserving read-only safety while providing an
  escalation path for one-command inspections.
- **Search tool CLI ergonomics** (#560, ADR-0150): `grep`/`glob` now default to
  searching the working directory instead of requiring an explicit `.` argument,
  matching common shell tool behavior (amends ADR-0016's empty-result contract).

### Changed

- **Trim advertised tool specs + conditional call-artifact header + 32 KiB MCP
  result cap** (#582): `ToolSpec` advertisements trim the inner `input` schema
  to 32 KiB before serialization (the actual JSON payload is untruncated), and
  `ToolRequest` emits a header only when a tool call actually rides. MCP
  results are capped at 32 KiB to prevent unbounded response buffering.
- **MCP client mechanism moved into the provider** (#560, ADR-0153): the
  streamable-HTTP transport + shared JSON-RPC helpers move to
  `entanglement-provider::mcp`, while config/registry/permissions/token
  file/browser stay runtime-side. The runtime drops its direct `reqwest` dep
  and `mcp-http` becomes a pure compile gate.
- **Normalize and stabilize the endpoint pool key** (#560, ADR-0156): endpoint
  pool keys normalize `endpoint` URLs and API keys before hashing, so spelling
  variations don't fragment resilience state. The hash is now stable across
  restarts (`DefaultHasher` fixed).
- **MCP HTTP transport shares the endpoint pool** (#560, ADR-0157): MCP HTTP
  requests ride the same `HttpClient`/`EndpointPool` resilience layer as LLM
  traffic, so the shared RPM/concurrency cap/Retry-After cool-down applies to MCP
  calls too.

### Fixed

- **TUI diff UTF-8 panic handling**: unbounded MCP buffer caps, enable TOCTOU
  race fix, registry cycle detection, detached task Holly clone abort at
  shutdown, `Retry-After` overflow panic clamped (network-controlled).
- **Abort detached tasks holding Holly clones at shutdown** to prevent orphaned
  references.
- **`Retry-After` overflow panic** (network-controlled) clamped.
- **Cap `Retry-After` park**, race `stream()` vs `Stop`, sync lease release.
- **Surface swallowed I/O errors in tool-exec + hook piping** instead of
  silently dropping them.
- **Label mutex-poison panics on MCP + tool-dispatch hot paths** for clearer
  debugging.
- **Propagate reqwest builder failure instead of panicking**.
- **`built_in_registry` surfaces parse failure as `Result`** instead of panicking.
- **TUI badge race** — subscribe before bootstrap `SetAgent`.
- **Plan mask no longer erases explore's call/bash**.
- **MCP required-param validation** now rejects missing required fields at tool
  call time.
- **Quarantine corrupt `mcp-tokens.yml`** on parse failure instead of crashing.
- **Static-bearer MCP UX** — improved error messages and handling.
- **Default-setup usability** (build/commit/run) improvements.
- **Session-title generator no longer clobbers `/name`**.
- **Errored sub-agent turn parks for steering** (ADR-0155): a child whose turn
  errored now parks the parent turn instead of silently failing, allowing user
  steering via mid-turn prompts.
- **Per-model cap resolved per-request** — cap is enforced on each individual
  request.
- **Shared endpoint lease acquired after in-process permits** — correct ordering
  to avoid deadlocks.
- **Prompt-caching `cache_control` breakpoints** fixed for proper cache flushing.
- **Page Up/Down in dialogs** now works correctly.
- **`call` rejects shell-line input** — argv-only enforced.

### Docs

- **Two pre-tag audit fixes** (Phase 1a, 1b): `docs/architecture/protocol.md`
  contract fields synced with `protocol.rs` (added 6 additive fields), and 28
  missing ADR amendment back-links added to individual ADRs plus README status
  cells.

## [0.5.0] - 2026-07-24

The TUI attention panel + session-panel overhaul (background approvals are no
longer invisible, ADR-0136), web search post-MVP follow-ups, and the batch of
changes landed since 0.4.0 was tagged (session-scoped directory grants,
`ask_user` v2, permission-arg path normalization, `glob`/`grep` escape-root
search via durable grant, live bash enablement, an MCP HTTP docs-only
leak-surface finding, OpenAI-compat stream robustness fixes, per-profile
bubblewrap sandbox scoping, and the tokio/rhai/syntect build-speed trims).

> **Wire-shape changes:** `ask_user` v2 (#488) replaces the v1 single-question
> payload — `OutEvent::UserQuestion` now carries `questions: Vec<Question>`
> (per-question `multi_select`, no `allow_free_form`); a serde shim still
> reads pre-v2 logs. New trusted-only `InMsg::BashEnable`/`BashDisable` +
> `OutEvent::BashChanged` (#498) are refused on the untrusted wire
> (`send_from_wire`), like the ADR-0124 MCP mutation ops.

### Added

- **TUI attention panel + session-panel overhaul** (ADR-0136): a background
  session parked on an approval or `ask_user` question is no longer invisible
  — a one-line panel above the input box (absent while nothing waits) names
  the oldest waiting session, its agent, and what it asks; `Ctrl+G` or a
  click jumps there, where the existing approval/question UI takes over. The
  status-bar `!` became a `⚠ N` count. Sessions are now identifiable: 8-char
  short ids replace full UUIDs in the sidebar/status bar/sessions modal, the
  sidebar shows a dim first-prompt description line per session and distinct
  `needs approval`/`question` state words, the sessions modal gains a
  `❓ question` badge, and sidebar session rows (plus the panel) are
  click-to-select.
- **Provider-side web-search results now persist into history** instead of
  living only on the ephemeral reasoning channel — citations, and (Anthropic)
  the search-result cache-pricing benefit, survive into a later turn. A new
  `ContentPart::ProviderSearch { provider, summary, data }` block round-trips
  its opaque `data` verbatim only to the provider that minted it (mirrors
  `ToolCall.provider_meta`); every other converter renders `summary` as plain
  text. Anthropic `pause_turn` (a long-running search pausing rather than
  ending the turn) is now continued client-side instead of ending the turn.
  The Anthropic web-search server-tool version (`web_search_20250305` vs a
  newer variant) is now catalog data (`ModelEntry.web_search_tool_version`)
  instead of hardcoded (#481, ADR-0131 amending ADR-0075).
- **Session-scoped directory grants**: approving one call under a directory
  (`[d]` on an approval prompt, or the new TUI `/allow <path>` command) now
  widens the grant to every later call under that directory for the
  read-only `read`/`grep`/`glob` triad, instead of only the exact call (#486,
  ADR-0126).
- **`ask_user` v2**: one call can now ask multiple questions in a single
  round-trip (`questions: Vec<Question>`), free-text answers are always
  available (the old `allow_free_form` flag is gone), and `multi_select` is
  per-question (#488, ADR-0127 amending ADR-0027).
- **TUI**: tool-call/approval/output entries share one header idiom, and
  approval decisions are recorded in the transcript (#487).
- **`glob`/`grep` can search outside the project root** by riding an existing
  durable (`Session`/`Always`) `read`-tool grant — no new approval prompt: a
  search never forces its own `Ask`, it only widens containment for a match
  already covered by a `read` grant on that directory (or an ancestor of it).
  `Once` grants deliberately stay excluded, since a search's match count is
  unbounded (#482, ADR-0132 amending ADR-0109).
- **Live bash enablement**: `bash`/`bash_output` can now be registered
  mid-session (TUI `/bash on [--allow [<pattern>]|--ask] | off`), graded
  through the permission model rather than a bare on/off — `Ask` (safe
  default) or `Allow`, optionally narrowed to a command pattern
  (`bash(git *): allow`). Still clamped by the config permission ceiling: a
  `bash: deny` ceiling wins over a live `Allow` (#498, ADR-0133).
- **Per-profile sandbox scoping for `bash`/`call`**: an agent profile can now
  set its own `sandbox: bwrap | none` frontmatter override instead of the
  bubblewrap confinement being one process-global on/off switch — a trusted
  profile can run unconfined beside a confined sub-agent profile in the same
  process. A spawned child's confinement is clamped to its parent's effective
  policy (a confined parent can't spawn an unconfined child), mirroring the
  existing sub-agent permission ceiling (#479, ADR-0134 amending ADR-0104).

### Changed

- **Build-speed trims (no behavior change)**: each crate's `tokio` dependency
  now declares its own minimal feature list instead of the workspace-wide
  `features = ["full"]`; the sandboxed `rhai` script tool moved behind a new
  default-on `entanglement-runtime` feature (`rhai`), so a lean
  (`--no-default-features`) embedder that never registers it can drop one of
  the heaviest always-compiled deps; `syntect` (behind `tui`) trims
  `default-fancy` down to the features the TUI's markdown highlighter
  actually uses (#502, ADR-0135 amending ADR-0025).

### Fixed

- **Permission arguments for path tools are normalized root-relative**: an
  absolute in-root path (`/root/src/main.rs`) now grades and grant-keys
  identically to its relative spelling (`src/main.rs`) for
  `read`/`edit`/`write`/`apply_patch`/`glob`/`grep` (#485, ADR-0125).
- **OpenAI-compat streaming robustness**: `data: [DONE]` is now the
  protocol-correct terminator (stops reading immediately instead of relying
  on connection close), a final unterminated SSE frame is flushed at EOF
  instead of silently dropped, and the Ollama catalog entries gained an
  explicit `max_output_tokens` (its own unset-`max_tokens` default was a
  primary source of the ADR-0118 "announced intent then stream died"
  symptom) (#483).

### Docs

- **MCP HTTP `${VAR}` header expansion** is documented as a consented leak
  surface, not a bug — a header naming a provider secret sends that key's
  live value to the configured remote server (#478, ADR-0128 amending
  ADR-0080). No code change.

## [0.4.0] - 2026-07-21

The `apply_patch` host tool, engine-robustness fixes (ambiguous-stop retry,
provider stream fixes), and the 2026-07-21 security-audit hardening (MCP
stdio key scrub, wire-refused MCP mutation) on top of 0.3.0.

> **Wire-behavior change:** `InMsg::McpAdd`/`McpRemove` are now refused on the
> untrusted wire (`send_from_wire`) — a WS/pipe client can no longer add or
> remove MCP servers (ADR-0124). `McpList` and trusted in-process heads (the
> TUI `/mcp` command, embedders using `Holly::send`) are unaffected.

### Added

- **`apply_patch` host tool** — multi-hunk unified-diff apply beside
  `edit`/`write`, the first producer of the previously-reserved
  `FileChangeKind::ApplyDiff`. A small hand-rolled parser/applier
  (`host::unified_diff`), root-contained and escape-root-gated like the rest
  of the file sextet (#455).
- **`agent_poll` `timeout_secs: 0` waits for the child's completion** instead
  of returning a useless still-running status immediately — the same
  hang-safe unbounded wait the blocking `agent` tool uses; positive timeouts
  keep the 600 s cap (ADR-0123).
- **Request-send retry + throttle status** in the provider pool: transient
  request-send faults retry like 5xx, and endpoint throttling is surfaced so
  heads can show it — the TUI gains a throttle indicator, plus a persisted
  external-editor choice and drag-select copy (`feat(tui)`, `feat(provider)`).
- **400-line file-cap gate** — `make file-cap` (in `make verify`) enforces the
  cap with a shrinking grandfathered allowlist
  (`scripts/file-cap-allowlist.txt`, #451).

### Security

- **rhai file/exec bindings route through the escape-root gate.** A script's
  `read`/`edit`/`write`/`exec`/`bash` binding hitting an out-of-root path now
  gets the same forced approval + grant recording as a direct tool call,
  instead of hard-failing with no prompt (#446, ADR-0119).
- **`Once`-scoped escape-root grants are bound to the approving
  `request_id`**, so a concurrent call to the same escaping path can no
  longer consume a single-use token it was never approved for (#449,
  ADR-0120).
- **Unknown tool names are rejected before the permission ladder**: a
  hallucinated tool name under an `Ask` grade could previously prompt the
  user to approve a call that could only fail — and even record an
  `Always`-scoped grant for a tool that doesn't exist. `dispatch()` now
  checks the registry snapshot first and replies immediately on a miss.

- **MCP stdio servers no longer inherit the provider API keys.** The spawned
  subprocess env gets the same scrub `bash`/`call` children have had since
  #164 (`catalog.key_envs()` removed before the per-server `env:` map is
  applied — an explicit per-server entry still wins) (#472, ADR-0124).
- **`McpAdd`/`McpRemove` are trusted-only.** An unapproved `McpAdd` spawns an
  arbitrary local subprocess, so the mutating MCP ops are refused off the
  untrusted wire (`send_from_wire`); the read-only `McpList` and the TUI
  `/mcp` command are unaffected. `InMsg::wire_allowed` is now an explicit
  fail-closed allowlist `match`, so a future variant is wire-refused until
  deliberately opted in (#472, ADR-0124 amending ADR-0069/ADR-0097).

### Fixed

- **Bounded retry on an ambiguous LLM stop.** A round that ends with no tool
  calls and an ambiguous `stop_reason` (`None`/`Other`, or a contradictory
  `ToolUse` with zero actual calls — the Ollama-class "announced intent, then
  the stream died" symptom) now retries in place with a synthetic nudge
  instead of silently ending the turn, bounded by
  `EngineConfig::max_ambiguous_stop_retries` (default 2). Persisted as
  `OutEvent::AmbiguousRetry` so replay reconstructs the exact round boundary
  (ADR-0118).
- **SSE streams are framed on raw bytes**, so a multi-byte UTF-8 character
  split across chunks no longer corrupts a streamed response (#443).
- **Gemini:** image content blocks are carried through tool results (#447),
  and parallel same-tool calls get synthesized unique `ToolCall` ids (#444).
- **OpenAI-compat:** tool-call flush unified on the validating path, so the
  end-of-stream fallback can no longer emit a call the streaming path would
  have rejected (#445); the stream-end handler no longer warns on every
  ordinary tool-use turn.
- **Executor:** in-flight dedupe entries are pruned on a `Stop`-driven abort —
  a cancelled call unwound with no resolving `ToolOutput`, leaking its
  `request_id` in the per-session in-flight set forever (#448).
- **TUI:** logs route to the file sink for the *default* (bare `skutter`) TUI
  head too, not just the explicit `tui` subcommand — a mid-session WARN on
  stderr corrupted the raw-mode interface.

## [0.3.0] - 2026-07-18

Capability-level tool permissions, provider concurrency/backpressure, and
session-lineage robustness on top of 0.2.0.

### Added

- **Capability-level permission keys.** A profile writes `read`/`write`/`call`
  once and it fans out at parse time to every member tool (`read` ⇒
  `read`/`grep`/`glob`, `write` ⇒ `edit`/`write`, `call` ⇒ `bash`), with
  `call`/`rhai` graded at the least-privileged bare grade — core stays
  capability-unaware (ADR-0114). Config-side `mcp.<server>.capabilities:` hints
  extend the same fan-out to external MCP tools (ADR-0117).
- **`rhai` exec bindings.** `rhai` scripts can drive `call`/`bash` under the
  Call capability, with approval-cache and timeout fixes (ADR-0115).
- **Workdir-scoped permission rules** for `bash`/`call` — a `call{pattern}`
  rule keyed on working directory (ADR-0116).
- **Per-endpoint concurrency cap + coordinated 429 backpressure.** A shared
  per-endpoint concurrency semaphore (permit held across the whole stream so
  spawned sub-agents queue instead of 429-storming), an AIMD adaptive pacing
  gate, and a bounded 429 retry that parks every concurrent session's window
  (ADR-0111). The cap is catalog data mirroring `rpm` — `ProviderEntry.concurrency`,
  env `{NAME}_CONCURRENCY`, user `providers.yml`, embedded default (#414).

### Fixed

- `Resume` cascades over the spawn sub-tree; fixes predecessor loss on a
  resumed compaction successor (ADR-0112).
- A spawned child's initiating prompt is now persisted, so it survives replay
  (ADR-0113).
- `permission_arg` extracts a path for `grep`/`glob`, giving the read-search
  tools argument-scoped rules (#417).

## [0.2.0] - 2026-07-17

First tagged release. Builds on the 0.1.0 crates.io baseline with session
compaction, live MCP and model/generation control, and a wider tool-permission
surface.

### Added

- **Session compaction.** `/compact` renders the transcript, summarizes it with
  a tool-less LLM call, and forks a copy-on-write *successor* session that
  retires the source — the source `Context` is never mutated
  (ADR-0101/ADR-0110). `--keep N` carries the trailing messages into the fork
  verbatim, clamped to a safe turn boundary (ADR-0102). On context overflow the
  turn loop auto-summarizes in place instead of a lossy prune, gated by
  `auto_compact` (ADR-0103). Delivered on the generic `InMsg::Oneshot` one-shot
  op envelope (ADR-0082).
- **Live MCP server management.** `InMsg::McpList`/`McpAdd`/`McpRemove` and the
  TUI `/mcp list|add|remove` command connect, register, and persist external MCP
  tool servers with no restart (ADR-0096/ADR-0097/ADR-0100), over stdio or the
  new streamable-HTTP transport (ADR-0080).
- **Live model, provider, and generation control.** Realtime `SetModel`
  provider/model switch without an engine restart (ADR-0063); per-agent-profile
  model pinning with rebind on `SetAgent` (ADR-0081); live `SetGeneration` with
  TUI `/set` and `/show`, plus `reasoning_effort` and per-profile persistence
  (ADR-0094/ADR-0095).
- **Access outside the project root, approval-gated.** A `read`/`edit`/`write`
  path or `bash`/`call` workdir resolving outside root forces an approval prompt
  and records a per-tool grant in a managed `extra-roots.yml` (ADR-0109). `call`
  default output moved to a runtime-owned per-session scratch dir.
- **Skill-scoped `allowed_tools` enforcement** — a `SKILL.md`'s `allowed_tools`
  frontmatter now gates tool calls while that skill is active (ADR-0106).
- **Idle-TTL auto-hibernation** exposed as a `config.yml` `idle_ttl_secs`
  setting for every head (ADR-0105), on top of session hibernation as evictable,
  resumable state (ADR-0077/ADR-0090).
- **Opt-in bubblewrap OS sandbox** for `bash`/`call`
  (`ENTANGLEMENT_SANDBOX=bwrap`, fail-closed) (ADR-0104).
- **WebSocket `serve` head per-connection approval ownership** — session-scoped,
  first-writer-wins `Approve`/`Reject`/`AnswerQuestion` (ADR-0107).
- **Live reload** of agent/skill/config definitions via inotify, content-gated
  so non-definition writes don't spam reloads (ADR-0084).
- **Release automation.** `make tag` cuts a version-checked annotated tag; the
  `release.yml` workflow gates a tag on `make verify` + coverage, then publishes
  all three crates to crates.io leaf-first via OIDC Trusted Publishing.

### Changed

- **Definitions are data, cross-vendor discoverable.** Agents/skills/catalog/
  config layer embedded < user < project, and also scan `~/.claude` /
  `.claude` / `.agents` dirs (ADR-0074). In-app tool-allowlist edits
  materialize a native user-layer override file (ADR-0083).
- `call` no longer rides the `ENTANGLEMENT_ENABLE_BASH` gate and gains
  `workdir` / `input_file` / `output_file`; `bash` gains `workdir` and
  `run_in_background` polled via `bash_output`.
- TUI: all transcript content wraps (no horizontal scroll); multiline input
  overhaul (newlines, cursor row, dynamic height, word/doc jumps); orchestration
  tool calls render as prose; `skutter` defaults to launching the TUI.

### Fixed

- `bash` closes stdin instead of inheriting the engine's real fd 0.
- `grep`'s file-scan cap decoupled from its output cap (no silent skips).
- TUI new-session ids minted as opaque UUIDs; first-run config/env scaffold
  notices surfaced past the default log level.

## [0.1.0]

Initial (untagged) crates.io publish — the three-layer engine foundation
(`entanglement-provider` → `entanglement-core` → `entanglement-runtime`),
streaming LLM providers, the stdio/TUI/`serve` heads, and the root-contained
host tools.

[0.6.0]: https://github.com/xmiksay/entanglement/compare/v0.5.0...v0.6.0
[0.5.0]: https://github.com/xmiksay/entanglement/releases/tag/v0.5.0
[0.4.0]: https://github.com/xmiksay/entanglement/releases/tag/v0.4.0
[0.3.0]: https://github.com/xmiksay/entanglement/releases/tag/v0.3.0
[0.2.0]: https://github.com/xmiksay/entanglement/releases/tag/v0.2.0
