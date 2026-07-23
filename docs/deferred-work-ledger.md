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

Row 3 is the remainder of the items surfaced by the 2026-07-21 whole-codebase
audit and its post-remediation pass ([#473](https://github.com/xmiksay/entanglement/issues/473));
the rest of that batch has moved to the Resolved table below. Row 4 was filed
by the 2026-07-23 revisit audit. Filed against
[#396](https://github.com/xmiksay/entanglement/issues/396).

| # | Deferred item | Documented at | Verified state |
| --- | --- | --- | --- |
| 3 ([#481](https://github.com/xmiksay/entanglement/issues/481)) | **Web search MVP limitations** (four sub-items): ~~search results not persisted to history~~; ~~`pause_turn` ends the turn rather than continuing~~; z.ai streaming `web_search` placement unverified; ~~the newer Anthropic `_20260209` server-tool version gated on a `ModelEntry` capability flag instead of hardcoded `_20250305`~~. | [ADR-0075](adr/0075-provider-side-web-search-mvp.md) §"Accepted MVP limitations (follow-ups)" (lines 83–96) — all four explicitly called "follow-up." | **3 of 4 shipped** ([ADR-0131](adr/0131-web-search-post-mvp-follow-ups.md) amends 0075): persistence (`ContentPart::ProviderSearch` + `OutEvent::SearchResult`), `pause_turn` continuation (client-owned in `anthropic::mod::stream()`), and the `ModelEntry.web_search_tool_version` capability flag all landed with tests. The z.ai streaming-placement item stays open — verification against a live key was attempted but blocked by no `ZAI_API_KEY`/network access in the implementing environment; parser unchanged, worst case still cited-text-only. **Row stays open until item 3 lands** (kept as row 3, not moved to Resolved, per #481's own acceptance criteria). |
| 4 ([#502](https://github.com/xmiksay/entanglement/issues/502)) | **Build-speed trims beyond the safe set** (three sub-items): tokio `features=["full"]` → per-crate actual feature sets; `rhai` behind a default-on runtime feature for lean embedders; `syntect` `default-fancy` trim behind `tui`. The safe set (dev-profile tuning, lld linker, `rand` dep dropped) shipped with the 2026-07-23 revisit. | 2026-07-23 revisit audit (dependency/build-speed pass). | Not shipped (intentional) — each trim is mechanical but needs its own `make verify` + lean-build validation, and the `rhai` gate touches the [ADR-0025](adr/0025-runtime-cargo-feature-gates.md) feature matrix. |

## Accepted risks (recorded, no action planned)

Security-posture notes from the 2026-07-23 revisit audit — reviewed, judged
consistent with the trust model, and deliberately left as-is. Recorded here so
the decision doesn't have to be re-derived by the next audit.

- **WS `serve` accepts any browser `Origin` unless `--allow-origin` is set**
  (`serve.rs::origin_allowed`). A malicious web page can open
  `ws://127.0.0.1:<port>/ws`, create its *own* session, and self-approve its
  tool calls — the per-connection approval ownership
  ([ADR-0107](adr/0107-ws-per-connection-approval-ownership.md)) only defends
  *existing* sessions against a second client. In scope of
  [ADR-0048](adr/0048-serve-head-local-trust-model.md)'s local single-user
  trust model (the WS is a general local protocol interface; origin checking
  is opt-in by design). Revisit if `serve` ever grows beyond loopback.
- **`SessionDir` grant coverage is a lexical prefix match** on the #485
  root-relative normalized arg (`grants.rs::dir_covers`), with no symlink
  resolution — a granted directory can cover an arg whose path component is a
  symlink pointing elsewhere in-root, skipping the *prompt* (never the
  filesystem boundary: host tools re-canonicalize and stay root-contained,
  and the scope is restricted to the read-only `read`/`grep`/`glob` triad,
  [ADR-0126](adr/0126-session-scoped-directory-grants.md)). Prompt-UX nuance,
  not a containment hole.

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
| [#478](https://github.com/xmiksay/entanglement/issues/478) | **Wire-trust doc note for MCP HTTP `${VAR}` expansion.** `expand_env()` resolves `${VAR}` in a configured server's static headers from the engine's whole process env with no allowlist, so a header naming a provider secret leaks its value to that server. Recorded as accepted (consent, per ADR-0047), not a bug — no code change; documents the surface and the "redact on any future header logging" constraint | [ADR-0128](adr/0128-mcp-http-var-header-expansion-leak-surface.md) amending [ADR-0080](adr/0080-mcp-streamable-http-transport.md) |
| [#483](https://github.com/xmiksay/entanglement/issues/483) | **OpenAI-compat stream robustness.** `data: [DONE]` is now the protocol-correct terminator (stops reading immediately, ignoring anything the endpoint sends afterward instead of relying on connection close); a final SSE frame with no trailing delimiter is flushed at EOF instead of silently dropped (can carry the closing `finish_reason`); the Ollama catalog entries gained an explicit `max_output_tokens` (Ollama's own unset-`max_tokens` default, `num_predict: 128`, was the primary source of the ADR-0118 "announced intent then stream died" symptom on local models) | [ADR-0118](adr/0118-ambiguous-stop-reason-bounded-retry.md) §"Alternatives considered" (lines 162–169) |
| [#477](https://github.com/xmiksay/entanglement/issues/477) | **Skill `allowed_tools` mask now reaches rhai bindings.** `BindingPolicy::capture` folds a one-time `skill_masked` snapshot alongside the existing agent mask — a `rhai` binding excluded by the active skill's `allowed_tools` refuses with the same message shape a direct call gets, checked after the agent mask, clears at the session's next `Done` | [ADR-0129](adr/0129-thread-the-skill-mask-into-rhai-binding-resolution.md) amending [ADR-0106](adr/0106-skill-scoped-allowed-tools-enforcement.md) |
| [#480](https://github.com/xmiksay/entanglement/issues/480) | **Rhai `exec`/`bash` binding `workdir` scoping.** The bindings now marshal an optional `workdir` (`exec(command, args, workdir)`/`bash(command, workdir)`), so a `tool{pattern}` workdir-scoped permission rule fires for a binding call exactly as it does for a direct `bash`/`call` tool call; `BindingPolicy::decide` also switched from `resolve` to `resolve_scoped` so the rule is actually consulted, and the per-run `approved` cache key now folds in `workdir` alongside the command line | [ADR-0130](adr/0130-rhai-exec-bindings-marshal-workdir.md) amending [ADR-0115](adr/0115-rhai-exec-bindings-call-bash.md) / [ADR-0116](adr/0116-workdir-scoped-permission-rules-for-bash-call.md) |
| [#482](https://github.com/xmiksay/entanglement/issues/482) | **`glob`/`grep` escape-root access via approval.** Search never forces its own approval prompt — `ExtraRootStore::is_durably_allowed_under` lets `glob`/`grep` ride an existing `Session`/`Always` `read`-tool grant on a directory (or an ancestor of it), widening `list_files`'s containment check; `Once` grants are structurally excluded (a search's match count is unbounded, unlike a single file read) | [ADR-0132](adr/0132-glob-grep-escape-root-search-via-durable-grant.md) amending [ADR-0109](adr/0109-escape-root-access-via-approval.md) |
| [#479](https://github.com/xmiksay/entanglement/issues/479) | **Per-profile sandbox scoping for `bash`/`call`.** `AgentProfile` gains an opaque `sandbox: Option<String>` frontmatter override (`bwrap`/`none`/`inherit`); the exec tools resolve it per session via a pluggable `policy::SandboxResolver` instead of the old process-global fixed `SandboxPolicy` field, so two profiles in one process can run confined and unconfined respectively. A spawned child's confinement is clamped to its parent's *effective* policy at spawn time (`most_confined`, the confinement-axis mirror of ADR-0024's permission ceiling) | [ADR-0134](adr/0134-per-profile-sandbox-scoping-and-spawn-chain-clamp.md) amending [ADR-0104](adr/0104-bubblewrap-sandbox-for-bash-call.md) |

## Docs-drift findings log

No open findings. Record entries here as `file:line — stale claim — current
truth — issue` when filed, and drop the row once fixed.

Findings of the 2026-07-23 revisit audit, fixed in the same change:

- `.claude/CLAUDE.md` "The contract" block and `README.md` contract block —
  both missing `InMsg::BashEnable`/`BashDisable` + `OutEvent::BashChanged`
  (#498/[ADR-0133](adr/0133-live-bash-enablement-graded-by-permission.md));
  the brief's own prose and `docs/architecture/protocol.md` already listed
  them. Same drift class as the #454 batch below — the summary blocks lag the
  prose again.
- `.claude/CLAUDE.md` provider section — link label said
  `../docs/architecture.md` while the href pointed at
  `../docs/architecture/provider.md` (target correct, label wrong).
- `.claude/CLAUDE.md` epic-history — "built-in profile trio" (#201-era) with
  no note that the set is now a quartet (`build`/`plan`/`explore`/`debug`,
  `agents/mod.rs::BUILT_INS`).
- This ledger's own Open table — row 2 (#480) still said "Not shipped
  (intentional)" while the Resolved table, [ADR-0130](adr/0130-rhai-exec-bindings-marshal-workdir.md),
  and `script.rs` all record it shipped; the intro's "Items 1–6 … item 7"
  numbering no longer matched the table. Both corrected.

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

Findings of the 2026-07-21 **post-remediation** pass ([#473](https://github.com/xmiksay/entanglement/issues/473)),
fixed in the same change:

- `docs/architecture/protocol.md:82-83` — claimed the WS head's
  `send_from_wire` + per-connection `Approve` ownership were "deferred to
  #153" — both shipped (#402,
  [ADR-0107](adr/0107-ws-per-connection-approval-ownership.md)). (Fixed in
  the #472 PR, whose ADR-0124 edit rewrote the same paragraph.)
- `docs/architecture/protocol.md:58` — the `FileChange` comment omitted
  `apply_patch` (#455), which code (`protocol.rs`) already documents as the
  third emitter beside `edit`/`write`.
- `CHANGELOG.md` `[Unreleased]` — recorded only `AmbiguousRetry` while ~14
  user-facing changes had landed since v0.3.0 (`apply_patch` #455, the
  escape-root fixes #446/#449, the provider stream fixes #443–#445/#447, the
  executor leak fix #448, unknown-tool rejection, and PR #471's batch).
  Backfilled.
- ADR back-links: [ADR-0109](adr/0109-escape-root-access-via-approval.md) not
  marked amended by 0119/0120, [ADR-0101](adr/0101-compaction-forks-into-a-new-session-copy-on-write.md)
  not marked amended by 0110, [ADR-0111](adr/0111-adaptive-endpoint-pacing-and-429-retry-until-clear.md)
  carrying no pointer to [ADR-0122](adr/0122-per-provider-concurrency-and-rpm-as-catalog-data.md)
  (and 0122 no `Supersedes` field) — status lines + README index cells now
  link forward, matching the 0046→0115 precedent.
- [ADR-0086](adr/0086-recordsink-pluggable-persistence-append-target.md) was
  referenced nowhere outside the ADR index — now linked from
  `docs/architecture/heads-and-persistence.md`'s `RecordSink` bullet.
- `.claude/CLAUDE.md` commands block — missing `make sessions`/`inspect`/
  `test-gates`/`tag`. Added.

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

