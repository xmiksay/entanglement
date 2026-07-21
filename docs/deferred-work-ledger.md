# Deferred-work ledger & docs/implementation drift

Standing ledger for two recurring failure modes found by the 2026-07-16 and
2026-07-21 whole-codebase audits:

1. **Intentionally-deferred work** that falls out of tracking once its
   originating issue closes (a design landed with an explicit "X deferred to a
   follow-up" note, and then no open issue points at X anymore).
2. **Documentation drift**: docs describing a shipped feature as "not yet
   built" or "future" (the `/set` palette dead-end — a shipped feature whose
   only doc claimed it wasn't built — is the canonical miss this ledger exists
   to catch), dead wire surface, reserved-but-undocumented enum variants, or a
   seam that grew a comment but no enforcement.

Tracked as GitHub issue [#396](https://github.com/xmiksay/entanglement/issues/396)
(epic, living — no end date). This file is the durable record; the issue
thread is where new items get filed and discussed.

## How to use this ledger

- **Filing a new deferred item:** open an issue against #396, add a row below.
- **Filing a docs-drift finding:** open an issue against #396 with the
  `documentation` label, citing `file:line` + the stale text + the
  current-truth. Small fixes land directly in the same PR; larger ones get
  their own issue.
- **Re-audit cadence:** after any feature merge that ships something a doc or
  ADR called "future"/"deferred", check whether that doc needs updating. ADRs
  are immutable (supersede, never edit in place); `docs/architecture/*` and
  `.claude/CLAUDE.md` are mutable and should be corrected in the same change.
- **Closing a row:** when a deferred item ships, close its issue and move the
  row to the "Resolved" table below instead of deleting it — the resolved
  table is the audit trail proving the ledger doesn't lose items a second
  time.

## Open deferred items

Six items surfaced by the 2026-07-21 whole-codebase audit — each a real gap
explicitly marked "follow-up"/"deferred" in an Accepted ADR but not previously
tracked here. Filed against [#396](https://github.com/xmiksay/entanglement/issues/396).

| # | Deferred item | Documented at | Verified state |
| --- | --- | --- | --- |
| 1 | **Per-profile sandbox scoping** (bubblewrap is global-only today). A mixed run — one profile confined, another not — needs the sandbox policy threaded through `run_for_session` (ADR-0088). | [ADR-0104](adr/0104-bubblewrap-sandbox-for-bash-call.md) §3 & "Negative" (lines 53–69, 149): "per-profile scoping is the tracked next step." | `entanglement-runtime/src/host/sandbox.rs` comment confirms: "Global for now — see the ADR's per-profile follow-up." **Not shipped (intentional).** |
| 2 | **Rhai `exec`/`bash` binding `workdir` scoping.** The bindings marshal `{command, args, timeout}` only, so a `tool{pattern}` workdir-scoped permission rule never fires for a binding call. | [ADR-0116](adr/0116-workdir-scoped-permission-rules-for-bash-call.md) §"the rhai binding grade is not touched" (lines 92–97): "Extending the bindings with their own `workdir` parameter, if ever wanted, is separate future work." | `script.rs` `exec`/`bash` bindings carry `{command, args, timeout}` only. **Not shipped (intentional).** |
| 3 | **Web search MVP limitations** (four sub-items): search results not persisted to history; `pause_turn` ends the turn rather than continuing; z.ai streaming `web_search` placement unverified; the newer Anthropic `_20260209` server-tool version gated on a `ModelEntry` capability flag instead of hardcoded `_20250305`. | [ADR-0075](adr/0075-provider-side-web-search-mvp.md) §"Accepted MVP limitations (follow-ups)" (lines 83–96) — all four explicitly called "follow-up." | All four still open. **Not shipped.** |
| 4 | **`glob`/`grep` escape-root access via approval.** A recursive search descending into an approved external directory is a distinct, murkier capability ("which external root? the whole filesystem?") than approving one `read`/`edit`/`write` path. | [ADR-0109](adr/0109-escape-root-access-via-approval.md) §"Negative / accepted" (lines 95–101): "deferred until a concrete need — reading a specific external file via `read` + approval covers the practical case." | `read`/`edit`/`write`/`bash`/`call` wired with `with_extra_roots()`; `glob`/`grep` route through `list_files`, which still silently drops out-of-root matches. **Not shipped (intentional).** |
| 5 | **OpenAI `[DONE]`-as-terminator + trailing-buffer-flush + Ollama `max_output_tokens` catalog default.** Pure robustness improvements: once `turn.rs` treats a bare `None` `stop_reason` as ambiguous-and-retried (ADR-0118, shipped), every scenario these could produce degrades to that already-handled case. | [ADR-0118](adr/0118-ambiguous-stop-reason-bounded-retry.md) §"Alternatives considered" (lines 162–169): "Deferred: pure robustness improvement with no attached user-visible bug." | **Not shipped.** |
| 6 | **Wire-trust doc note for MCP HTTP `${VAR}` expansion.** Any `${VAR}` in a configured MCP server header is expanded from the engine's process env, so a named secret (`ZAI_API_KEY`, etc.) can leak to the MCP server in a header. Mitigations exist (servers are trusted-by-config per ADR-0047; enabling is explicit consent; no header logging found) but the leak surface is not documented in ADR-0080, and any future debug logging must redact expanded values. | `entanglement-runtime/src/mcp/http.rs:296-336` `expand_env()`; not yet called out in [ADR-0080](adr/0080-mcp-streamable-http-transport.md). | Mitigations in place; **doc gap not yet closed.** |

## Resolved (shipped since the 2026-07-16 audit)

All six items surfaced by the audit shipped before this ledger's own PR
merged:

| Issue | Deferred item | ADR/issue it descends from |
| --- | --- | --- |
| [#397](https://github.com/xmiksay/entanglement/issues/397) | Auto-summarize on context overflow (vs prune-only fallback) | [ADR-0103](adr/0103-auto-summarize-on-context-overflow.md) / #324 |
| [#398](https://github.com/xmiksay/entanglement/issues/398) | `/compact` keep-tail (`kept` > 0) | [ADR-0102](adr/0102-compact-keep-tail-verbatim-in-the-fork-prompt.md) / #324 |
| [#399](https://github.com/xmiksay/entanglement/issues/399) | Skill-scoped `allowed_tools` enforcement | [ADR-0106](adr/0106-skill-scoped-allowed-tools-enforcement.md) |
| [#400](https://github.com/xmiksay/entanglement/issues/400) | OS sandbox for `bash`/`call` exec pair | [ADR-0104](adr/0104-bubblewrap-sandbox-for-bash-call.md) |
| [#401](https://github.com/xmiksay/entanglement/issues/401) | Idle-TTL auto-hibernation for `serve` | [ADR-0090](adr/0090-idle-ttl-auto-hibernation.md) / [ADR-0105](adr/0105-expose-idle-ttl-via-runtime-config.md) |
| [#402](https://github.com/xmiksay/entanglement/issues/402) | WS `serve` `send_from_wire` + per-connection `Approve` ownership | [ADR-0107](adr/0107-ws-per-connection-approval-ownership.md) |
| [#414](https://github.com/xmiksay/entanglement/issues/414) | Per-provider endpoint **concurrency** as catalog data (`ProviderEntry.concurrency` + `{NAME}_CONCURRENCY`), instead of one global `ENTANGLEMENT_MAX_CONCURRENCY` default (3) | [ADR-0111](adr/0111-adaptive-endpoint-pacing-and-429-retry-until-clear.md) |
| [#421](https://github.com/xmiksay/entanglement/issues/421) | A spawned child's initiating task prompt is never persisted (delivered straight to the session-command channel, bypassing the inbound broadcast the persistence tap observes) — unrecoverable on replay/resume | [ADR-0113](adr/0113-persistence-synthesizes-a-spawned-childs-initiating-prompt.md) / [ADR-0112](adr/0112-resume-cascades-over-the-spawn-subtree.md) |
| [#419](https://github.com/xmiksay/entanglement/issues/419) | `rhai` exec bindings (`bash`/`call`), explicitly deferred by [ADR-0046](adr/0046-rhai-sandboxed-script-tool.md) pending "its own ADR" — unblocked by the Call capability giving exec a uniform permission grade | [ADR-0115](adr/0115-rhai-exec-bindings-call-bash.md) amending [ADR-0046](adr/0046-rhai-sandboxed-script-tool.md) / [ADR-0114](adr/0114-capability-level-permission-keys.md) / #416 |
| [#425](https://github.com/xmiksay/entanglement/issues/425) | `call` capability key has no file-path/`workdir` scoping — only command-pattern scoping, since `call`/`bash` have no fixed target path independent of their command line | [ADR-0116](adr/0116-workdir-scoped-permission-rules-for-bash-call.md) / [ADR-0114](adr/0114-capability-level-permission-keys.md) / #418 / #416 |
| [#426](https://github.com/xmiksay/entanglement/issues/426) | MCP tools (`mcp__<server>__<tool>`) are not assigned to any capability — capability fan-out only covers the fixed built-in host-tool set | [ADR-0117](adr/0117-mcp-tool-capability-fan-out.md) / [ADR-0114](adr/0114-capability-level-permission-keys.md) / #418 / #416 |
| [#472](https://github.com/xmiksay/entanglement/issues/472) | **Untracked security gap** (2026-07-21 audit): MCP stdio subprocesses inherited the engine's full env incl. provider API keys (the #164 scrub covered only `bash`/`call`), while `McpAdd` was wire-allowed with no approval — spawning an arbitrary local subprocess straight off the origin-unchecked `serve` WS — and `wire_allowed` was a fail-open blocklist. All three fixed: `secret_env` scrub threaded into `StdioClient::spawn`, `McpAdd`/`McpRemove` wire-refused, allowlist made an exhaustive fail-closed `match` | [ADR-0124](adr/0124-wire-refused-mcp-mutation-and-stdio-key-scrub.md) amending [ADR-0069](adr/0069-trusted-untrusted-wire-frame-split.md) / [ADR-0097](adr/0097-live-mcp-server-management.md) / #164 |

## Docs-drift findings log

No open findings. Record entries here as `file:line — stale claim — current
truth — issue` when filed, and drop the row once fixed.

Fixed in the same change once filed:

- `entanglement-runtime/src/skills/mod.rs:62,90` — comments called skill
  `allowed_tools` masking "tier-2 enforcement, deferred" / "enforcement is
  deferred anyway" — it shipped as `permission::skill_masked`, wired in
  `tool_runner.rs`, per [ADR-0106](adr/0106-skill-scoped-allowed-tools-enforcement.md)
  (#400). ([#452](https://github.com/xmiksay/entanglement/issues/452))
- `docs/architecture/protocol.md` §2 type block — presents itself as the
  exhaustive wire contract but was missing `InMsg::McpList`/`McpAdd`/
  `McpRemove` and `OutEvent::McpList`/`McpChanged`/`SkillActive`
  (`protocol.rs:656/662/667`, `967/973/1222`). ([#454](https://github.com/xmiksay/entanglement/issues/454))
- `.claude/CLAUDE.md` "The contract" `OutEvent` list — missing `SkillActive` +
  `AmbiguousRetry` (`protocol.rs:1222/1243`); also a link-label typo (call-
  registration bullet said "ADR-0094" while correctly linking
  `0093-call-registration-independent-of-bash-opt-in.md`). ([#454](https://github.com/xmiksay/entanglement/issues/454))
- `README.md` contract block — missing `SetGeneration` + the MCP trio
  (`InMsg`), and `GenerationChanged` + the MCP pair + `SkillActive` +
  `AmbiguousRetry` (`OutEvent`). ([#454](https://github.com/xmiksay/entanglement/issues/454))
- `CHANGELOG.md` had no `[Unreleased]` section — `AmbiguousRetry`/
  [ADR-0118](adr/0118-ambiguous-stop-reason-bounded-retry.md) shipped after
  0.3.0 tagged but skipped the brief-sync convention entirely (absent from
  `.claude/CLAUDE.md` too, now added alongside). ([#454](https://github.com/xmiksay/entanglement/issues/454))

Additional findings fixed in the 2026-07-21 audit pass (kept for one cycle as
the audit trail, then pruned):

- `.claude/CLAUDE.md:108-110` — described `ProviderEntry.concurrency` as shipped
  under #414, which the 2026-07-21 audit flagged as possible drift against
  ADR-0111's "Deferred" section. **Verified shipped** (catalog.rs field +
  test + `<NAME>_CONCURRENCY` env resolver in main.rs); ADR-0111's deferred
  framing is now superseded by [ADR-0122](adr/0122-per-provider-concurrency-and-rpm-as-catalog-data.md).
  No brief text change needed.
- `.claude/CLAUDE.md:38-52` — commands block was missing `make help`/`make
  install`/`make pipe`. **Fixed:** added all three to the block.
- `.claude/CLAUDE.md` — env-var surface was scattered inline with no one-place
  index; several vars (`ENTANGLEMENT_CONFIG_FILE`, `ENTANGLEMENT_GRANTS_FILE`,
  `ENTANGLEMENT_PREAMBLE_FILE`/`_BRIEF_FILE`, `ENTANGLEMENT_ECHO_FULL`,
  `ENTANGLEMENT_TUI_*`, hook-context vars) were not surfaced at all.
  **Fixed:** added a consolidated env-var reference table after the providers
  section.
- `README.md:42` — mentioned a "future Vue SPA" as a hypothetical client with
  no evidence any such SPA exists or is tracked. **Fixed:** reworded to "any
  future client".

