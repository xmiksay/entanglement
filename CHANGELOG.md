# Changelog

All notable changes to this project are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project
follows [Conventional Commits](https://www.conventionalcommits.org/) — the
`<type>(<scope>): <subject>` history is the source these entries summarize.
Versioning is [Semantic Versioning](https://semver.org/). The *why* and rejected
alternatives behind each design decision live in the ADRs under
[`docs/adr/`](docs/adr/); the referenced `ADR-####` tags link there.

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

[0.4.0]: https://github.com/xmiksay/entanglement/releases/tag/v0.4.0
[0.3.0]: https://github.com/xmiksay/entanglement/releases/tag/v0.3.0
[0.2.0]: https://github.com/xmiksay/entanglement/releases/tag/v0.2.0
