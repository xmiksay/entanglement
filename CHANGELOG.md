# Changelog

All notable changes to this project are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project
follows [Conventional Commits](https://www.conventionalcommits.org/) — the
`<type>(<scope>): <subject>` history is the source these entries summarize.
Versioning is [Semantic Versioning](https://semver.org/). The *why* and rejected
alternatives behind each design decision live in the ADRs under
[`docs/adr/`](docs/adr/); the referenced `ADR-####` tags link there.

## [Unreleased]

### Fixed

- **Bounded retry on an ambiguous LLM stop.** A round that ends with no tool
  calls and an ambiguous `stop_reason` (`None`/`Other`, or a contradictory
  `ToolUse` with zero actual calls — the Ollama-class "announced intent, then
  the stream died" symptom) now retries in place with a synthetic nudge
  instead of silently ending the turn, bounded by
  `EngineConfig::max_ambiguous_stop_retries` (default 2). Persisted as
  `OutEvent::AmbiguousRetry` so replay reconstructs the exact round boundary
  (ADR-0118).

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

[0.3.0]: https://github.com/xmiksay/entanglement/releases/tag/v0.3.0
[0.2.0]: https://github.com/xmiksay/entanglement/releases/tag/v0.2.0
