# Deferred-work ledger & docs/implementation drift

Standing ledger for two recurring failure modes found by the 2026-07-16
whole-codebase audit:

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

None open — see the resolved table below for #426, the last item filed
against this ledger.

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
