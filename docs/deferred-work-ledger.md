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

Row 3 (#625, orig. #481) — the last of the items surfaced by the 2026-07-21
whole-codebase audit and its post-remediation pass
([#473](https://github.com/xmiksay/entanglement/issues/473)) — shipped
([ADR-0171](adr/0171-zai-streaming-web-search-placement-confirmed.md)) and
moved to Resolved; the rest of that batch had already moved there. Row 4
(filed by the 2026-07-23 revisit audit) shipped and moved to Resolved. Row 5
(#517) shipped as part of #552 and moved to Resolved.

**2026-08-03 — every row now has a live tracking issue.** The ledger had hit
exactly the failure mode it exists to prevent: all of its originating issues
(#396, #473, #481, #513, #522, #539, #541, #542) had closed while the deferrals
themselves were still open in the code, so nothing on GitHub pointed at any of
them. Rows 6, 7 and 9 were spot-checked against the tree and confirmed still
open. Each row's `#` cell now carries its **live** issue first and its original
provenance second; the set is tracked by
[#624](https://github.com/xmiksay/entanglement/issues/624).

Rows 16 and 17 were added by the 2026-08-03 tool-surface review
([ADR-0161](adr/0161-unified-async-work-background-flag-and-one-poll.md)–[ADR-0164](adr/0164-short-sortable-kind-tagged-ids.md)).

| # | Deferred item | Documented at | Verified state |
| --- | --- | --- | --- |
| 6 ([#626](https://github.com/xmiksay/entanglement/issues/626) — orig. [#513](https://github.com/xmiksay/entanglement/issues/513)) | **No TUI stop-cascade-vs-detach confirm modal.** The backend fully supports both (a `Stop` on the plan session detaches from a sponsored `propose_plan` build child; a second `Stop` on the child cascades), but the TUI offers no way to *choose* — a `Stop` on the plan session always detaches. | [ADR-0145](adr/0145-one-plan-tool-file-backed-plans-and-blocking-review-loop.md) "Consequences" / "Alternatives considered". | **Open, prerequisite shipped** (ee85bc5, #6): the general in-turn `Stop` capability the modal was blocked on now exists — bare `Esc` in Normal mode sends `InMsg::Stop`, plus `/stop [--all]` and the sessions-modal `s` quick key. What remains is only the cascade-vs-detach confirm modal itself, including disambiguating `WaitingAgent`'s two callers (a plain blocking `agent` tool call vs. a sponsored `propose_plan` build) before it can safely offer the choice. |
| 7 ([#627](https://github.com/xmiksay/entanglement/issues/627) — orig. [#513](https://github.com/xmiksay/entanglement/issues/513)) | **No watcher-driven "plan updated by user" transcript notice.** `propose_plan`'s staleness guard already detects an out-of-band plan-file edit at the next `propose_plan(path=...)` call, but nothing notifies the TUI *as it happens* — the #329 watcher (`watch.rs`) is purpose-built for agent/skill/config definition reload, a different reload action entirely, so bolting a plans-folder watch onto it would conflate two unrelated semantics. | [ADR-0145](adr/0145-one-plan-tool-file-backed-plans-and-blocking-review-loop.md) "Consequences" / "Alternatives considered". | **Open.** The existing `spawn_debounced_watcher` primitive (agent/skill-decoupled) is the natural reuse point for a dedicated, lightweight plans-folder watch. |
| 8 ([#628](https://github.com/xmiksay/entanglement/issues/628) — orig. [#539](https://github.com/xmiksay/entanglement/issues/539)) | **Tool-overlay grade override doesn't reach rhai binding grades or child sessions.** The session tool overlay's *mask* half fully composes (a matching entry unmasks a `rhai` binding via `tool_masked`, and a parent's overlay admits tools for its spawn sub-tree per link), but the *grade* half (`Ask`/`Allow` replacing the profile chain) applies only on the generic dispatch route and only for the overlay session itself: a script's `bash()` binding under an overlay still grades through the profile chain (`BindingPolicy` snapshot), and a child's calls grade through its own chain even where the parent's overlay admits existence. | [ADR-0149](adr/0149-per-session-tool-overlay.md) "Consequences" — both explicitly deferred. | **Open.** Acceptable v1: the overlay's primary target (MCP tools) has no binding surface, and a child session can be given its own overlay. Revisit if real use hits either edge. [ADR-0163](adr/0163-live-bash-enablement-is-a-tool-overlay-entry.md)'s `arg_pattern` does **not** close this: it widens what an overlay grade can *say* (argument scoping), not where the grade is *consulted* — still the generic dispatch route only, never the `rhai` `BindingPolicy` snapshot or a child's own chain. |
| 11 ([#631](https://github.com/xmiksay/entanglement/issues/631) — orig. tui-ux-batch Issue 3) | **MCP OAuth — four scoped-out edges**: (a) no device-code flow, so a host with no browser *and* no way to reach the loopback port cannot authorize at all (the printed URL covers the common SSH case, since the redirect lands on the *user's* machine only if they forward the port); (b) credentials are process-global, keyed by server name — the multi-user embedder API ([ADR-0147](adr/0147-multi-user-mode-embedder-api.md)) has per-user providers/keys/grants but *not* per-user MCP tokens; (c) cross-process refresh is racy — the token file's `fd-lock` serializes the write but not the token *exchange*, so two `skutter` instances refreshing the same rotating grant simultaneously can have one lose (it recovers by re-authorizing); (d) OAuth is wired for MCP servers only, not for LLM provider endpoints, though the mechanism now lives in `entanglement-provider` where such a consumer would sit. | [ADR-0153](adr/0153-mcp-server-oauth.md) "Consequences". | **Open.** None is a correctness defect: (a) and (d) are unbuilt scope, (b) is bounded by the single-user `serve` trust model (ADR-0048), and (c) self-heals through re-authorization. |
| 12 ([#632](https://github.com/xmiksay/entanglement/issues/632) — orig. [#522](https://github.com/xmiksay/entanglement/issues/522)) | **Per-user RPM/concurrency on a *shared literal* API key is out of scope.** The multi-user embedder API gets per-user rate-limit isolation from ADR-0050's pool keying (base URL + API-key hash) — which means two users configured with the *same* literal key share one endpoint budget rather than each getting their own. | [ADR-0147](adr/0147-multi-user-mode-embedder-api.md) "Consequences" — explicitly scoped out. | **Open.** Per-user budgets on a shared key would need a user dimension in the pool key (or a per-user admission gate layered above it); today the embedder's recourse is issuing distinct keys per user. |
| 13 ([#633](https://github.com/xmiksay/entanglement/issues/633) — orig. [#522](https://github.com/xmiksay/entanglement/issues/522)) | **Authenticated multi-user wire head is a future ADR.** #522 shipped the embedder *library* seams only; `serve` stays exactly [ADR-0048](adr/0048-serve-head-local-trust-model.md)-scoped (local, single-user, loopback-bound) — no authentication, session-to-user binding, or per-user wire trust exists on any in-tree transport. | [ADR-0147](adr/0147-multi-user-mode-embedder-api.md) "Decision"/"Consequences". | **Open.** A multi-tenant wire head must come with its own ADR (authn, `SessionUserRegistry` population from credentials, per-user approval ownership); until then multi-user is reachable only by embedding the library. |
| 14 ([#634](https://github.com/xmiksay/entanglement/issues/634) — orig. [#539](https://github.com/xmiksay/entanglement/issues/539)) | **`serve`/`pipe` heads cannot set session tool overlays.** `InMsg::SetToolOverlay` is trusted-only/wire-refused (the `McpAdd` rationale: an enable entry can carry `Allow`, handing the model un-prompted tools — and, per [ADR-0163](adr/0163-live-bash-enablement-is-a-tool-overlay-entry.md), can now also register a lazily-registrable built-in like `bash`), so only an in-process head (the TUI's `/enable`/`/disable`, or an embedder over `Holly::send`) can use it — a WS/pipe client has no overlay surface at all. | [ADR-0149](adr/0149-per-session-tool-overlay.md) "Decision" (wire posture). | **Open.** Deliberate under the ADR-0124 fail-closed allowlist; a wire opt-in would need its own justification (e.g. restricted to deny-only entries, or riding an authenticated head per row 13). |
| 16 ([#636](https://github.com/xmiksay/entanglement/issues/636)) | **No structured tool-result channel.** Every tool result is `anyhow::Result<String>` → `text_parts` → `Vec<ContentPart>`; exit codes, truncation notices, denials and failures are all *textual markers inside the string* (`[exit N]`, `... [truncated: …]`, `tool \`X\` failed: …`). A failing tool is indistinguishable from a succeeding one at the type level, and `run_and_reply` derives `output_text` purely for the `post_tool_use` hook — which explicitly cannot rewrite `content`. | [ADR-0161](adr/0161-unified-async-work-background-flag-and-one-poll.md) "Explicitly out of scope" (the deeper form of the "it's all just text" observation that ADR motivated). | **Open, deliberately unscoped.** Adding `is_error`/exit code/duration as *fields* touches the `Tool` trait, `ToolRegistry::execute`, `InMsg::ToolResult` and every head's rendering — wants its own ADR and its own audit of what each head would do with the fields. |
| 17 ([#637](https://github.com/xmiksay/entanglement/issues/637)) | **`rhai` has no `background`/`poll` participation.** The other three launchers gain a `background` flag; `rhai` gets only the uniform result shape. Its engine runs under `tokio::task::spawn_blocking`, which cannot be aborted — the 30 s cap is enforced *inside* the engine by `on_progress`, and the cooperative `stop: Arc<AtomicBool>` cannot reach a binding already blocked in `exec`/`bash`. | [ADR-0161](adr/0161-unified-async-work-background-flag-and-one-poll.md) §5, with an explicit revisit trigger. | **Open by design.** Backgrounding it means raising the 30 s cap *and* accepting a detached task that is unkillable for the duration of an inner binding. Revisit trigger: a concrete need for a long-running script. |
| 15 ([#635](https://github.com/xmiksay/entanglement/issues/635) — orig. tui-ux-batch Issue 5) | **Aux-model deferrals: a `narrate` purpose + per-user aux pins.** The `Purpose` enum ships closed (`summarize`, `session_title`); a `narrate` purpose (live "what the agent is doing" `action` text via the aux seam) was considered and deferred, and the pin store is process-global — the multi-user embedder API ([ADR-0147](adr/0147-multi-user-mode-embedder-api.md)) has no per-user aux pins. | [ADR-0154](adr/0154-per-purpose-auxiliary-models.md) "Consequences". | **Open.** Adding a purpose is a closed-enum extension (parser + store + one consumer); per-user pins would ride the same embedder-store pattern as `UserProviderStore`. |

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
| [#502](https://github.com/xmiksay/entanglement/issues/502) | **Build-speed trims beyond the safe set** (three sub-items, row 4): per-crate `tokio` feature lists replacing the workspace-wide `features = ["full"]`; the sandboxed `rhai` tool moved behind a default-on `entanglement-runtime` feature so a lean embedder can drop it via `--no-default-features`; `syntect` (behind `tui`) trimmed from `default-fancy` to `parsing`/`default-syntaxes`/`default-themes`/`regex-fancy`. None change behavior; validated by the existing `tree`/`check-lean`/`fmt`/`lint`/`test` gates | [ADR-0135](adr/0135-deferred-build-speed-trims-tokio-rhai-syntect.md) amending [ADR-0025](adr/0025-runtime-cargo-feature-gates.md) |
| [#517](https://github.com/xmiksay/entanglement/issues/517) | **Throttle queue depth surfaced (row 5).** A `waiters: AtomicUsize` side-counter on `EndpointState`, bumped just before and dropped via an RAII `WaiterGuard` right after `concurrency.acquire_owned().await`, threaded into `ThrottleStatus`/`OutEvent::Throttle` as `waiters: usize` — purely for display, no request-path behavior changed, exactly as this row anticipated. Landed alongside #552's wider cross-process throttle-visibility fix. | This ledger, row 5 |
| [#629](https://github.com/xmiksay/entanglement/issues/629) | **`.gitignore` awareness in the `glob`/`grep` walk (row 9).** `list_files_with_extra_roots` now builds a second, `ignore`-crate-driven path set (`gitignore_allowed_paths`) and drops any glob-matched candidate not in it, the same unconditional way the `.git` filter already works — root and nested `.gitignore`, `.git/info/exclude`, and the global excludes file are all honored (`require_git(false)` so it applies even without an actual `.git` dir). Additive, not a replacement of the `glob`-crate walk: the bare-`**` trap, brace/dir-pattern expansion, containment, and extra-root widening (which now deliberately bypasses the granted directory's own `.gitignore`) are all untouched. | This ledger, row 9 / [ADR-0170](adr/0170-gitignore-aware-glob-grep-walk.md) amending [ADR-0008](adr/0008-host-tools-workdir-and-bounded-output.md) |
| [#625](https://github.com/xmiksay/entanglement/issues/625) (orig. [#481](https://github.com/xmiksay/entanglement/issues/481)) | **z.ai streaming `web_search` placement confirmed (former row 3, last of the web-search MVP's four limitations).** Verified live against a working Coding Plan key: `web_search` is a top-level sibling of `choices`, delivered once on the final chunk alongside `finish_reason`/`usage`, never nested under `choices[0].delta`. `openai::sse::handle_chunk` drops the never-matching delta-nested scan site; a regression test locks in the removal. Also found live: tool invocation is model-decided, not guaranteed by `enable: true` — an observed instance of the cited-text-only floor both ADR-0075 and ADR-0131 already accepted as the worst case. | [ADR-0171](adr/0171-zai-streaming-web-search-placement-confirmed.md) amending [ADR-0131](adr/0131-web-search-post-mvp-follow-ups.md) / [ADR-0075](adr/0075-provider-side-web-search-mvp.md) |
| [#630](https://github.com/xmiksay/entanglement/issues/630) | **Provider-bundled MCP enablement — child inheritance + `SessionEnded` cleanup (row 10, (a)-(b); (c) already closed by #558).** `AvailableMcp` now folds `SessionStarted`/`SessionEnded` off the engine-wide broadcast (`mcp::spawn_mcp_responder`, the only place that owns it across the whole engine lifetime): (a) a child→parent map lets `spec_visible`'s ancestor walk resolve a spawned child's inherited enablement live (`available_lifecycle::ancestor_enabled`, cycle-guarded like `permission::ancestor_chain`) — a second, independent copy of the links `subagent::SpawnGuard` tracks, since that one stays single-threaded inside the tool executor's own loop and isn't reachable from the per-session `tool_spec_resolver` closure; (b) `forget_session` drops an ended session's enablement marks and parent link from both maps, so neither grows for the process lifetime — deliberately not on `SessionHibernated`, since a lazy enable is never logged for replay the way the #539 tool overlay is. | [ADR-0152](adr/0152-provider-bundled-mcp-servers-three-state-enablement.md) "Consequences" / this ledger, row 10 |

## Docs-drift findings log

No open findings. Record entries here as `file:line — stale claim — current
truth — issue` when filed, and drop the row once fixed.

Findings of the 2026-08-01 pre-release audit, fixed in the same change:

- `README.md` contract block — missing the newest `InMsg`/`OutEvent` variants
  (the `McpAuth`/`McpAuthChanged` OAuth pair, `SetSessionMeta`/
  `SessionMetaChanged`, `SetToolOverlay`/`ToolOverlayChanged`,
  `PauseSession`/`ResumeSession`, the question-management trio). Same drift
  class as the #454 batch — the summary blocks lagging the prose again.
- `docs/architecture/protocol.md` — `InMsg::McpAuth`/`OutEvent::McpAuthChanged`
  ([ADR-0153](adr/0153-mcp-server-oauth.md)) absent from the wire-contract
  type block.
- `AGENTS.md` — still described the MCP client mechanism as runtime-owned;
  it moved into `entanglement-provider::mcp` (ADR-0153).
- `docs/architecture/layers-and-abi.md` — WS `serve` head status stale
  (still framed as pending pre-`serve` hardening; shipped in #153).
- `docs/architecture/` — no coverage of the aux-models seam
  ([ADR-0154](adr/0154-per-purpose-auxiliary-models.md)): core's
  `aux_llm_resolver`, the runtime `AuxLlmRegistry`/`aux-models.yml`, and the
  session-title generator were documented only in the project brief. Now
  owned by `engine.md` (*Auxiliary models*) and
  `heads-and-persistence.md` §6d.

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

