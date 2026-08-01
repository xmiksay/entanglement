# Changelog

All notable changes to this project are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project
follows [Conventional Commits](https://www.conventionalcommits.org/) — the
`<type>(<scope>): <subject>` history is the source these entries summarize.
Versioning is [Semantic Versioning](https://semver.org/). The *why* and rejected
alternatives behind each design decision live in the ADRs under
[`docs/adr/`](docs/adr/); the referenced `ADR-####` tags link there.

## [Unreleased]

> **Wire-shape change:** `OutEvent::Plan` gains a `path: String` field
> (`#[serde(default)]`, so a pre-#513 persisted log still replays).

### Added

- **List/retract/replace an open `ask_user` question** (#515, ADR-0146):
  `InMsg::ListQuestions { correlation_id, session? }` → `OutEvent::QuestionList`
  enumerates every open question (or one session's), mirroring `ListSessions`/
  `McpList`. `InMsg::RetractQuestion { session, request_id }` withdraws a
  question without cancelling the rest of the turn — the `ask_user`
  orchestrator still replies with a withdrawal note, unlike a session-wide
  `Stop`. `InMsg::ReplaceQuestion { session, request_id, questions }` swaps a
  parked question's content in place, re-emitting `OutEvent::UserQuestion`
  under the same `request_id` rather than resolving the call. All four
  variants are wire-allowed.
- **`PauseSession`/`ResumeSession` — a hold between cancel and hibernate**
  (#516, ADR-0144): a new `AgentState::Paused` plus a `Session.paused` flag
  (never persisted) holds a session's next turn without cancelling in-flight
  work or evicting memory. An idle paused session defers its next
  `Prompt`/`SetAgent`/`SetModel`/`SetGeneration`/`Oneshot` on the existing
  turn-stash queue; a parked tool batch still resolves `ToolResult`s
  immediately, but its continuation into the next model round-trip waits for
  `ResumeSession`, which continues with no re-prompt. `Stop`/`HibernateSession`
  always win regardless of `paused` — `Stop` reports `Paused` instead of
  `Done` if the hold is still in effect. Both variants are wire-allowed.
- **Wire-visible throttle transitions** (#517, ADR-0141): a stdio/WS head
  used to see an opaque stall of up to ~15 minutes under a 429, since only
  the TUI polled `HttpClient::throttle_status()` directly. `OutEvent::Throttle
  { endpoint, throttled, in_flight, cap, retry_in_ms?, pacing_in_ms? }` joins
  the protocol as an engine-global, no-`seq` lifecycle event, emitted by a new
  `entanglement-runtime::throttle::spawn_throttle_responder` only on a state
  transition (entering/leaving a 429 cool-down, the AIMD pacing gate
  penalizing/relaxing) — not on every 500ms poll tick.
- **Per-model concurrency cap, layered on the endpoint cap** (#521,
  ADR-0140): z.ai enforces concurrency per model (e.g. `glm-4.7-flash: 1` vs
  `glm-5.2: 5`), not just per endpoint, so a mixed-model workload sharing one
  endpoint could 429-storm the tighter model or needlessly serialize the
  looser one. `ModelEntry` gains an optional `concurrency` (catalog data,
  YAML-only); `EndpointState` now acquires a model permit before the
  endpoint-wide one (released in reverse), so a caller blocked on its own
  saturated model never holds the endpoint permit hostage. The endpoint cap
  stays the ceiling on the sum across every model.
- **Shared endpoint resilience state across `skutter` instances** (#523,
  ADR-0144 — note: this ADR number collides with #516's above; both merged
  from independent PRs on the same day and were left as-is rather than
  renumbering an already-accepted ADR, see `docs/adr/README.md`): two
  processes talking to the same `(endpoint, API key)` used to each apply the
  full RPM/concurrency budget, together sending up to N× the configured
  rate. A file-backed cross-process gate (`client::shared_state::SharedGate`,
  `fd-lock`-guarded under `${data_dir}/entanglement/endpoints/`) now covers
  the RPM ledger, a lease-based in-flight concurrency count, and the
  `Retry-After` cool-down; the AIMD pacing gate stays per-process. Falls back
  to pure in-process gating when the state directory is unwritable, or via
  `ENTANGLEMENT_NO_SHARED_ENDPOINT_STATE=1`.
- **Trusted scratch dir + plans-folder carve-outs** (#524, ADR-0142): the
  runtime's own scratch dir (the default `call`-output target) no longer
  forces an escape-root approval prompt for `read`/`edit`/`write`/
  `apply_patch`, `glob`/`grep`, or a `bash`/`call` workdir, in any profile —
  a directory-prefix check consulted before the ordinary per-path grant
  lookup, not persisted. The generated `<env>` system-prompt block now names
  the scratch dir, steering the model off `/tmp`. Separately, the built-in
  `plan` profile's mask gains a plans-folder carve-out
  (`write(.entanglement/plans/*.md): allow`) so the unified plan tool (#513)
  can write its plan file while the rest of the read-only mask stays intact.

### Changed

- **One plan tool, file-backed, with a blocking review loop** (#513,
  ADR-0145, superseding ADR-0049's `update_plan` half and amending ADR-0138):
  `update_plan` is removed — `propose_plan(content | path)` is the sole
  plan-authorship tool. `content` materializes (or overwrites)
  `.entanglement/plans/<short-session-id>.md`; `path` binds an existing
  in-root `.md` file, refused with a re-read-required error if it changed
  out of band since the session last touched it (a content-hash staleness
  guard that never trips on the agent's own `write`/`edit` between build
  phases). Every `propose_plan` force-parks on approval, every phase; on
  approve, the sponsored `build` child spawn (#511/ADR-0138) is now
  registered for cancellation, so `Stop` on the plan session actually
  detaches from the blocking wait (the child keeps running; a second `Stop`
  on the child cascades). The TUI's "Plan Outline" side panel is dropped for
  a one-line `Plan: <path> (pending|accepted)` indicator, and `/plan` now
  opens the bound file in `$EDITOR` instead of revealing the sidebar.
- **TUI: `ask_user` answers are drafts until an explicit Submit** (#518,
  ADR-0143): each question's `Enter`/number-pick used to commit and send
  immediately once it was the batch's last question, with no way to revise
  an earlier answer short of aborting the turn. Answers are now written in
  place as drafts; reaching the end of the batch opens a review/submit step
  listing every question with its draft, whose `Enter` is the one explicit
  Submit and whose `Esc`/`←`/`Backspace` steps back to revise instead of
  sending. A single-question call gets the same review step. No wire/
  protocol change.

### Fixed

- **TUI approval prompts showed an empty or truncated body for several
  tools** (#519): approval bodies fell back to raw JSON, a truncated
  header-only arg, or nothing at all — an approval you can't inspect isn't
  meaningful consent. `render_expansion` now gives every tool a real body:
  `read`/`edit` show the full untruncated path (+ offset/limit for `read`),
  `write` diffs against the on-disk file at approval time, `apply_patch`
  renders its patch as a real unified diff instead of escaped JSON,
  `bash`/`call` show the full multiline/wrapped command plus workdir, and
  `glob`/`grep` get a structured pattern/path/exclude body instead of raw
  JSON.
- **TUI `grep`/`glob` output rendering was lossy** (#520): `render_grep_output`
  kept only lines containing `:`, silently dropping `host/grep.rs`'s skip/cap
  notices (ADR-0091) and the `truncate_output` truncation marker;
  `render_glob_output` re-derived a directory count by scraping digits out of
  its own prose hint instead of rendering it verbatim. Both now pass every
  notice through untouched, and `glob`'s "no files" hints render as-is with no
  digit-scraping. Split into a new `tool_render/search_output.rs` module
  (shared by the standalone `ToolOutput` block and the `grep`/`glob`
  expansion bodies) to keep `tool_render.rs` under the 400-line cap.

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

[0.5.0]: https://github.com/xmiksay/entanglement/releases/tag/v0.5.0
[0.4.0]: https://github.com/xmiksay/entanglement/releases/tag/v0.4.0
[0.3.0]: https://github.com/xmiksay/entanglement/releases/tag/v0.3.0
[0.2.0]: https://github.com/xmiksay/entanglement/releases/tag/v0.2.0
