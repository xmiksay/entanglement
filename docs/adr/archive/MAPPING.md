# Archived ADRs — mapping table

ADR-0001 … ADR-0249 were moved here **unchanged** by #943 (part of the #914
cleanup). They are the historical decision log: read one for the *why* behind
the code, never as a description of current behaviour — that is
[`docs/architecture/`](../../architecture.md). The numbers are kept, so every
`ADR-NNNN` reference in code and docs resolves through this table.

## Rules

- **Never edit an archived ADR.** Change its row here instead.
- A PR that removes or changes behaviour an archived ADR decided updates that
  ADR's row in the same PR: append `removed by #N` or `changed by #N` to the
  status (separated by `; `), and delete code comments citing it along with
  removed code.
- When a [topic ADR](../README.md) takes over an archived ADR's subject, put its
  number in the **Topic ADR** column.
- Relative links inside an archived ADR that leave `docs/adr/` (e.g.
  `../embedding.md`) were written for the old location: add one `../`.

## Status values

| Value | Meaning |
| --- | --- |
| `current` | Still describes the code (`amended by NNNN`: read those too) |
| `partly superseded by NNNN` | The named ADR replaced a part; the rest stands |
| `superseded by NNNN` | No longer describes the system |
| `removed by #N` / `changed by #N` | Behaviour removed / changed by that cleanup PR |
| `inconsistent: …` | The ADR says one thing, the code does another; resolved by an epic issue |

## Table

| ADR | Title | Status | Topic ADR |
| --- | --- | --- | --- |
| [0001](0001-actor-model-abi.md) | Actor model is the integration ABI | current | — |
| [0002](0002-session-multiplexed-protocol.md) | Session-multiplexed wire protocol | current; amended by 0164, 0228, 0230, 0233, 0237, 0240 | — |
| [0003](0003-agent-and-permission-profiles.md) | Agent + permission profiles (opencode-style) | superseded by 0207 | — |
| [0004](0004-structured-plan-and-task-events.md) | Structured Plan & TaskList events (profiles + events, both) | current; partly superseded by 0039, 0049 | — |
| [0005](0005-ndjson-stdio-head.md) | NDJSON stdio head (`run` + `pipe`) | current | — |
| [0006](0006-core-dependency-hygiene-gate.md) | Layering: core / provider / runtime, and the core hygiene gate | current; partly superseded by 0053; amended by 0176; changed by #931 | — |
| [0007](0007-streaming-llm-and-provider-crate.md) | `entanglement-provider`: streaming `Llm` trait, pooling, retry, rate-limit, reasoning | current; partly superseded by 0053 | — |
| [0008](0008-host-tools-workdir-and-bounded-output.md) | Host tools: working-directory root + bounded output | current; partly superseded by 0054, 0091, 0170; amended by 0161 | — |
| [0009](0009-edit-and-bash-host-tools.md) | Host tools: `edit` (search/replace) and `bash` (subprocess + timeout) | current; partly superseded by 0054 | — |
| [0010](0010-single-head-crate-and-bash-opt-in.md) | `entanglement-runtime`: the head crate — tools, execution, permissions, sessions | current; partly superseded by 0093 (itself removed by #916), 0195 (§3 `bash` opt-in retired); amended by 0176; changed by #931 | — |
| [0011](0011-tui-head-ratatui-crossterm.md) | TUI head: ratatui + crossterm in `entanglement-runtime` | current; changed by #931 | — |
| [0012](0012-tui-event-buffering-rendering.md) | TUI event-buffering & rendering model | current | — |
| [0013](0013-keybinding-leader-which-key.md) | Keybinding scheme: leader key + which-key | current | — |
| [0014](0014-tool-approval-inline-modal.md) | Tool approval UX: inline card vs modal | current | — |
| [0015](0015-rich-text-pipeline-syntect.md) | Rich-text pipeline: pulldown-cmark → ratatui Text, syntect for code blocks | current | — |
| [0016](0016-host-tool-empty-result-contract.md) | Host tools: empty-result contract (no silent zero-output) | current; amended by 0150 | — |
| [0017](0017-stop-cancels-turn-not-session.md) | `InMsg::Stop` cancels the turn, not the session | current; amended by 0208 (itself removed by #921) | — |
| [0018](0018-turn-loop-stash-discipline.md) | Turn-loop command stash discipline | current | — |
| 0019 | _(number skipped, no record)_ | — | — |
| [0020](0020-event-sourced-session-persistence.md) | Event-sourced session persistence | current | — |
| [0021](0021-hierarchical-session-model.md) | Hierarchical session data model | current | — |
| [0022](0022-subagent-spawn.md) | Sub-agent spawn and parent→child answer relay | current | — |
| [0023](0023-subagent-spawn-limits.md) | Sub-agent spawn recursion / fan-out limits | current; amended by 0138, 0207 | — |
| [0024](0024-subagent-permission-gating.md) | Sub-agent spawn permission gating and privilege ceiling | current; amended by 0137, 0138, 0207 | — |
| [0025](0025-runtime-cargo-feature-gates.md) | `entanglement-runtime` cargo feature gates (`cli`/`tui`) for lean library embedding | current; amended by 0053, 0135; changed by #931 | — |
| [0026](0026-async-subagent-spawn-and-poll.md) | Non-blocking sub-agent spawn with handle + `agent_poll` | current; partly superseded by 0161 | — |
| [0027](0027-ask-user-interactive-prompt.md) | `ask_user` tool — model-driven user decision prompt | current; amended by 0127, 0146 | — |
| [0028](0028-session-lifecycle-enumeration-and-backpressure.md) | Session lifecycle: enumeration, explicit close, non-blocking routing | current | — |
| [0029](0029-external-editor-and-markdown-export.md) | External `$EDITOR` compose + Markdown transcript export | current | — |
| [0030](0030-tui-file-mentions-and-bash-passthrough.md) | TUI `@file` mention completion + `!bash` passthrough | current | — |
| [0031](0031-write-host-tool-whole-file.md) | Host tool `write`: whole-file create/overwrite (quartet → quintet) | current; partly superseded by 0054 | — |
| [0032](0032-yaml-provider-model-catalog.md) | YAML provider/model catalog: embedded defaults + user override | current | — |
| [0033](0033-agent-tool-family-and-blocking-agent.md) | `agent_*` tool family — rename `spawn_agent` → `agent_spawn`, add blocking `agent` | current; partly superseded by 0161 | — |
| [0034](0034-file-based-agent-definitions.md) | File-based agent definitions — discovery, frontmatter, registry | current | — |
| [0035](0035-deterministic-system-prompt-assembly.md) | Deterministic system-prompt assembly | current; partly superseded by 0229 | — |
| [0036](0036-skill-discovery-and-registry.md) | Skill discovery + registry — SKILL.md frontmatter, tier-1 disclosure | current; partly superseded by 0229 | — |
| [0037](0037-load-skill-tool-deterministic-resolution.md) | `load_skill` tool — deterministic resolution, path substitution, provenance | current; partly superseded by 0229; amended by 0228, 0249 | — |
| [0038](0038-physical-per-agent-tool-restriction.md) | Physical per-agent tool restriction (allowlist/denylist mask) | superseded by 0207 | — |
| [0039](0039-markdown-task-list.md) | Markdown task list (structured `Vec<TaskItem>` → plain snapshot) | current; partly superseded by 0049 | — |
| [0040](0040-per-profile-spawn-control.md) | Per-profile spawn control (can_spawn + spawnable-agents allowlist) | superseded by 0207 | — |
| [0041](0041-update-plan-ownership-default-closed.md) | update_plan ownership — default-closed plan authority | superseded by 0049 | — |
| [0042](0042-plan-acceptance-via-propose-plan-approval-roundtrip.md) | Plan acceptance via the propose_plan approval round-trip | current; amended by 0138, 0145 | — |
| [0043](0043-skill-preload-vs-access-independent-mechanisms.md) | Skill preload (`skills:`) vs access (`load_skill` mask) — independent mechanisms | current; partly superseded by 0229 | — |
| [0044](0044-agents-skills-system-prompt-epic-synthesis.md) | Agents, skills & system prompt — epic synthesis (data-not-code subsystem) | current; partly superseded by 0229 | — |
| [0045](0045-call-host-tool-argv-exec-tailed-output.md) | Host tool `call`: argv exec (no shell) with auto-tailed output | current; partly superseded by 0093; amended by 0161; removed by #916 | — |
| [0046](0046-rhai-sandboxed-script-tool.md) | `rhai` host tool — embedded, capability-sandboxed script engine | current; amended by 0115, 0206, 0227, 0228, 0248 (itself removed by #926) | — |
| [0047](0047-local-trust-boundary.md) | Local trust boundary — the repository is trusted; config precedence system < user < repo; inspection over enforcement | current | — |
| [0048](0048-serve-head-local-trust-model.md) | `serve` head — a local-only WebSocket protocol interface (Vue SPA primary, non-exclusive; browser surface out of scope) | current | — |
| [0049](0049-plan-task-tools-as-runtime-state-tools.md) | `update_plan`/`update_tasks` as runtime state tools | current; partly superseded by 0145; amended by 0142, 0207 | — |
| [0050](0050-per-endpoint-connection-pool-retry-rate-limit.md) | Per-endpoint connection pool, rate-limit, and retry | current; amended by 0141, 0156, 0157; changed by #918 | — |
| [0051](0051-argument-scoped-permission-rules.md) | Argument-scoped permission rule keys (`tool(pattern)`) | current; changed by #916 | — |
| [0052](0052-approval-scope-and-persisted-grants.md) | Approval scope (`Once \| Session \| Always`) + persisted tool grants | current | — |
| [0053](0053-invert-core-provider-seam.md) | Invert the core↔provider seam: provider owns the LLM ABI as a leaf crate | current | — |
| [0054](0054-canonicalizing-symlink-safe-root-containment.md) | Canonicalizing, symlink-safe host-tool root containment | current | — |
| [0055](0055-usage-cost-and-stop-reason-surfacing.md) | Usage/cost surfacing + normalized stop reason (`OutEvent::Usage`) | current | — |
| [0056](0056-closesession-cascades-over-spawn-subtree.md) | `CloseSession` cascades over the spawn sub-tree | current | — |
| [0057](0057-mid-stream-error-partial-commit-and-retry.md) | Mid-stream error: commit the partial, retry once before output | current | — |
| [0058](0058-mid-turn-prompt-folds-into-live-turn.md) | Mid-turn `Prompt` folds into the live turn | current | — |
| [0059](0059-tool-trait-and-registry-live-in-the-runtime.md) | `Tool` trait + `ToolRegistry` live in the runtime, not core | current | — |
| [0060](0060-filechange-audit-via-executor-as-path-kind-hash.md) | `FileChange` audit is emitted by the executor as `path + kind + hash` | current | — |
| [0061](0061-parked-turn-state-batch-tool-resolution.md) | Parked turn state: batch tool resolution as explicit session state | current | — |
| [0062](0062-collapse-llmsession-placeholder-newtype.md) | Collapse the `LlmSession` placeholder newtype | current | — |
| [0063](0063-realtime-model-provider-switch.md) | Realtime model/provider switch without engine restart | current; amended by 0207 | — |
| [0064](0064-message-content-blocks.md) | `Message`/`Prompt` carry multimodal content blocks | current | — |
| [0065](0065-read-emits-image-content-blocks.md) | `read` emits image files as image content blocks | current | — |
| [0066](0066-lifecycle-hooks-as-runtime-interceptors.md) | Lifecycle hooks are runtime interceptors around tool dispatch and prompt ingress | current; changed by #964 | — |
| [0067](0067-mcp-client-as-runtime-tool-provider.md) | MCP client is a runtime-side tool provider | current; amended by 0193 | — |
| [0068](0068-shared-per-session-seq-counter.md) | Shared per-session `seq` counter: runtime mints fresh seqs via `Holly::emit_for_session` | current; amended by 0239, 0244 | — |
| [0069](0069-trusted-untrusted-wire-frame-split.md) | Trusted/untrusted wire-frame split: privileged in-process `ToolResult`/`Spawn` | current; amended by 0124 | — |
| [0070](0070-authoritative-tool-exec-profile-and-fail-closed-fallback.md) | Authoritative `ToolExec` profile, fail-closed permission/mask fallback, lag-proof decision delivery | current | — |
| [0071](0071-parked-turn-reoffer-timer.md) | In-process parked-turn re-offer timer + executor request_id dedupe | current | — |
| [0072](0072-protocol-warts-settled-before-serve.md) | Protocol warts settled before `serve` freezes the wire | current; amended by 0139 | — |
| [0073](0073-managed-env-file-writer-and-key-surfaces.md) | One shared writer for the managed env file, with a CLI and TUI surface | current; changed by #931 | — |
| [0074](0074-cross-vendor-skill-and-agent-discovery.md) | Cross-vendor skill & agent discovery (`~/.claude`, `.claude/`, `.agents/`) | current; partly superseded by 0229 | — |
| [0075](0075-provider-side-web-search-mvp.md) | Provider-side web search MVP — opt-in config, results on the reasoning channel | current; amended by 0131, 0171; removed by #919 | — |
| [0076](0076-per-session-dynamic-tool-specs.md) | Per-session dynamic tool specs (`EngineConfig.tool_spec_resolver`) | current; amended by 0190, 0204 | — |
| [0077](0077-session-hibernation-evictable-resumable.md) | Session hibernation — evict in-memory state without tombstoning, resumable later | current; amended by 0168, 0208 (itself removed by #921), 0210 | — |
| [0078](0078-per-turn-dynamic-system-prompt.md) | Per-turn dynamic system prompt (`EngineConfig.system_prompt_resolver`) | current | — |
| [0079](0079-pluggable-permission-resolver-and-grant-store.md) | Pluggable `PermissionResolver` + `GrantStore` seams in the tool executor | current; changed by #964 | — |
| [0080](0080-mcp-streamable-http-transport.md) | MCP streamable-HTTP transport with per-server headers/auth | current; amended by 0128, 0153 | — |
| [0081](0081-per-profile-model-pinning-and-rebind-on-set-agent.md) | Per-agent-profile provider/model pinning, and rebind on `SetAgent` | current | — |
| [0082](0082-single-shot-session-ops-and-persisted-compaction.md) | Single-shot session ops (`InMsg::Oneshot`) and persisted compaction | partly superseded by 0101 (in-place compaction; `InMsg::Oneshot` and `Compacted` stay) | — |
| [0083](0083-in-app-tool-allowlist-editing-as-user-layer-materialization.md) | In-app tool-allowlist editing materializes a user-layer override | superseded by 0207 (README still marked it amended) | — |
| [0084](0084-runtime-live-reload-and-managed-file-locking.md) | Runtime-side live reload (inotify) and advisory locking for managed files | current | — |
| [0085](0085-gemini-native-wire-and-opaque-provider-meta.md) | Native Gemini wire + opaque `provider_meta` on `ToolCall` | current | — |
| [0086](0086-recordsink-pluggable-persistence-append-target.md) | `RecordSink` — pluggable persistence append target | current | — |
| [0087](0087-two-stage-ctrl-c.md) | Two-stage Ctrl+C: clear inputs first, then quit | current | — |
| [0088](0088-session-aware-tool-execution.md) | Session-aware tool execution: thread `SessionId` through `ToolRegistry::execute`/`Tool::run` | current | — |
| [0089](0089-user-configurable-max-turns.md) | User-configurable `max_turns` | current | — |
| [0090](0090-idle-ttl-auto-hibernation.md) | Optional `idle_ttl` — supervisor auto-hibernates settled sessions | current; amended by 0210, 0246 | — |
| [0091](0091-grep-file-scan-size-cap-decoupled-from-output-cap.md) | `grep` file-scan size cap decoupled from the output cap | current | — |
| [0092](0092-call-file-based-stdin-stdout.md) | `call` gains `input_file`/`output_file` for file-based stdin/stdout | current; removed by #916 | — |
| [0093](0093-call-registration-independent-of-bash-opt-in.md) | `call` registers independently of `bash`'s opt-in gate | current; removed by #916 | — |
| [0094](0094-reasoning-effort-and-per-profile-generation-persistence.md) | `reasoning_effort` model + per-profile generation persistence | current | — |
| [0095](0095-tui-set-show-generation-persist-on-confirmation.md) | TUI `/set`/`/show` for generation parameters + persist-on-confirmation | current | — |
| [0096](0096-dynamic-toolregistry-sharedregistry.md) | Dynamic `ToolRegistry`: `SharedRegistry` replaces value-ownership in the executor | current | — |
| [0097](0097-live-mcp-server-management.md) | Live MCP server management: wire ops, engine-global routing, config persistence | current; amended by 0124 | — |
| [0098](0098-rhai-json-yaml-loader-and-read-raw.md) | `rhai` JSON/YAML loader (`parse_json`/`to_json`/`parse_yaml`/`to_yaml`) and the `read_raw` binding | current; amended by 0227 | — |
| [0099](0099-glob-grep-exclude-and-default-git-exclusion.md) | `glob`/`grep`: default `.git` exclusion + caller `exclude` patterns | current | — |
| [0100](0100-tui-mcp-command.md) | TUI `/mcp` command — list, add, remove | current | — |
| [0101](0101-compaction-forks-into-a-new-session-copy-on-write.md) | Compaction forks into a new session (copy-on-write) | current; amended by 0102, 0110, 0205 | — |
| [0102](0102-compact-keep-tail-verbatim-in-the-fork-prompt.md) | `/compact` keep-tail: a verbatim tail composed into the fork prompt | current | — |
| [0103](0103-auto-summarize-on-context-overflow.md) | Auto-summarize on context overflow, in place — distinct from `/compact`'s copy-on-write | current; amended by 0205 | — |
| [0104](0104-bubblewrap-sandbox-for-bash-call.md) | OS-level process confinement for `bash`/`call` via bubblewrap — opt-in, fail-closed | current; amended by 0134; changed by #916 | — |
| [0105](0105-expose-idle-ttl-via-runtime-config.md) | Expose `EngineConfig.idle_ttl` via `config.yml` — an engine-global setting, not a `serve`-only flag | current | — |
| [0106](0106-skill-scoped-allowed-tools-enforcement.md) | Skill-scoped `allowed_tools` enforcement — a session-local runtime mask layered after the #116 agent mask | superseded by 0194 | — |
| [0107](0107-ws-per-connection-approval-ownership.md) | WS per-connection approval ownership — session-scoped, first-writer-wins | current | — |
| 0108 | _(number skipped, no record)_ | — | — |
| [0109](0109-escape-root-access-via-approval.md) | Access outside the project root via per-tool approval — containment becomes subordinate to an explicit grant | current; amended by 0119, 0120, 0132, 0142, 0228; changed by #916 | — |
| [0110](0110-compaction-successor-closes-predecessor.md) | `/compact` forks a *successor* and retires the source — and each session tracks its lineage | current; amended by 0234, 0243 | — |
| [0111](0111-adaptive-endpoint-pacing-and-429-retry-until-clear.md) | Per-endpoint concurrency cap, adaptive pacing, and bounded 429 retry | current; partly superseded by 0122; amended by 0141, 0155; changed by #918 | — |
| [0112](0112-resume-cascades-over-the-spawn-subtree.md) | `Resume` cascades over the spawn sub-tree; the re-announced `SessionStarted` carries the replayed `predecessor` | current | — |
| [0113](0113-persistence-synthesizes-a-spawned-childs-initiating-prompt.md) | Persistence synthesizes a spawned child's initiating prompt from the cached `Spawn` | current | — |
| [0114](0114-capability-level-permission-keys.md) | Capability-level permission keys (`read`/`write`/`call`) | superseded by 0207 | — |
| [0115](0115-rhai-exec-bindings-call-bash.md) | `rhai` exec bindings: `exec`/`bash`, gated by the Call capability | current; amended by 0130, 0227; changed by #916 | — |
| [0116](0116-workdir-scoped-permission-rules-for-bash-call.md) | Workdir-scoped permission rules for `bash`/`call` (`tool{pattern}`) | current; amended by 0130; changed by #916 | — |
| [0117](0117-mcp-tool-capability-fan-out.md) | Config-side capability hints fan MCP tools into `read`/`write`/`call` | current | — |
| [0118](0118-ambiguous-stop-reason-bounded-retry.md) | Bounded retry on an ambiguous LLM stop, instead of ending the turn | current | — |
| [0119](0119-rhai-bindings-route-through-the-escape-root-gate.md) | `rhai` file/exec bindings route through the escape-root gate | current; amended by 0227; changed by #916 | — |
| [0120](0120-once-scoped-escape-root-grant-bound-to-request-id.md) | `Once`-scoped escape-root grants are bound to the approving `request_id` | current; changed by #916 | — |
| [0121](0121-prune-only-compact-stays-silent.md) | Prune-only `Context::compact` stays silent — accepted live/replay divergence | current; partly superseded by 0205 | — |
| [0122](0122-per-provider-concurrency-and-rpm-as-catalog-data.md) | Per-provider concurrency and RPM as catalog data | current; amended by 0213; changed by #918 | — |
| [0123](0123-agent-poll-zero-timeout-waits-for-notification.md) | `agent_poll` `timeout_secs: 0` waits for the child's notification | current; amended by 0155, 0161 | — |
| [0124](0124-wire-refused-mcp-mutation-and-stdio-key-scrub.md) | MCP mutation is trusted-only, the wire gate fails closed, and stdio servers lose the provider keys | current; changed by #916 | — |
| [0125](0125-permission-arguments-for-path-tools-are-normalized-root-relative.md) | Permission arguments for path tools are normalized root-relative | current; changed by #916 | — |
| [0126](0126-session-scoped-directory-grants.md) | Session-scoped directory grants via `ApprovalScope::SessionDir` and TUI `/allow` | current; changed by #916; removed by #925 | — |
| [0127](0127-ask-user-v2-multi-question-envelope.md) | `ask_user` v2 — multi-question envelope, always-on free text, multi-select | current; amended by 0143, 0146 | — |
| [0128](0128-mcp-http-var-header-expansion-leak-surface.md) | MCP HTTP `${VAR}` header expansion is a documented, consented leak surface | current | — |
| [0129](0129-thread-the-skill-mask-into-rhai-binding-resolution.md) | Thread the skill `allowed_tools` mask into `rhai` binding resolution | superseded by 0194 (binding snapshot by 0227) | — |
| [0130](0130-rhai-exec-bindings-marshal-workdir.md) | `rhai` `exec`/`bash` bindings marshal `workdir` | current; amended by 0227; changed by #916 | — |
| [0131](0131-web-search-post-mvp-follow-ups.md) | Web search post-MVP follow-ups — persisted results, `pause_turn` continuation, version flag | current; amended by 0171; removed by #919 | — |
| [0132](0132-glob-grep-escape-root-search-via-durable-grant.md) | `glob`/`grep` reach outside the project root by riding an existing durable `read` grant | current | — |
| [0133](0133-live-bash-enablement-graded-by-permission.md) | Live bash enablement, graded by the permission model | superseded by 0163 | — |
| [0134](0134-per-profile-sandbox-scoping-and-spawn-chain-clamp.md) | Per-profile sandbox scoping for `bash`/`call`, with a spawn-chain confinement clamp | superseded by 0207 | — |
| [0135](0135-deferred-build-speed-trims-tokio-rhai-syntect.md) | Deferred build-speed trims: per-crate tokio features, optional `rhai`, trimmed `syntect` | current; amended by 0180; changed by #931 | — |
| [0136](0136-tui-attention-panel-signpost-and-jump.md) | TUI attention panel: a signpost + jump, not a second approval keymap | current | — |
| [0137](0137-explore-ask-grade-shell-access.md) | `explore` gains `Ask`-grade shell access (`call`/`bash`/`rhai`) | superseded by 0207 (the `explore` agent is retired; previously unmarked) | — |
| [0138](0138-sponsored-build-child-and-propose-plan-cycle.md) | Sponsored build child — plan acceptance spawns a clamp-exempt child | superseded by 0207 | — |
| [0139](0139-visible-session-states-working-and-waitingagent.md) | Visible session states — `Working` + `WaitingAgent`, and `Done` as the resting state | current; amended by 0208 (itself removed by #921) | — |
| [0140](0140-per-model-concurrency-cap-layered-on-endpoint-cap.md) | Per-model concurrency cap, layered on the endpoint cap | current; changed by #918 | — |
| [0141](0141-wire-visible-throttle-transitions.md) | Wire-visible throttle transitions: `OutEvent::Throttle`, engine-global not per-session | current; changed by #918 | — |
| [0142](0142-trusted-scratch-dir-and-plans-folder-carve-outs.md) | Trusted scratch dir + plans-folder carve-out | current | — |
| [0143](0143-ask-user-draft-until-submit.md) | `ask_user` answers are drafts until an explicit Submit | current | — |
| [0144](0144-file-backed-shared-endpoint-state-across-instances.md) | File-backed shared endpoint state across instances | current; amended by 0156, 0213; changed by #918; changed by #929 | — |
| [0145](0145-one-plan-tool-file-backed-plans-and-blocking-review-loop.md) | One plan tool: file-backed plans, a staleness guard, and a blocking review loop | current; amended by 0172, 0173, 0207 | — |
| [0146](0146-ask-user-list-retract-replace.md) | `ask_user` gains list/retract/replace of open questions | current; amended by 0245; changed by #922 | — |
| [0147](0147-multi-user-mode-embedder-api.md) | Multi-user mode: session-scoped identity + embedder-API per-user seams | current; partly superseded by 0181 | — |
| [0148](0148-glob-patterns-in-the-agent-tool-mask.md) | Glob patterns in the agent tool mask | superseded by 0207 | — |
| [0149](0149-per-session-tool-overlay.md) | Per-session tool overlay — live injection past the agent mask | current; amended by 0163, 0177, 0207, 0226; removed by #923 | — |
| [0150](0150-search-tool-cli-ergonomics.md) | Search-tool CLI ergonomics: directory auto-expansion, brace sets, and no silent empty results | current | — |
| [0151](0151-settable-session-metadata.md) | Settable session display metadata (name + action) | current; amended by 0154, 0183 | — |
| [0152](0152-provider-bundled-mcp-servers-three-state-enablement.md) | Provider-bundled MCP servers and the three-state enablement model | current; partly superseded by 0226; removed by #915 | — |
| [0153](0153-mcp-server-oauth.md) | MCP server OAuth, and moving the MCP client mechanism into the provider crate | current; amended by 0157, 0182, 0184, 0187 | — |
| [0154](0154-per-purpose-auxiliary-models.md) | Per-purpose auxiliary models for side transformations | current; amended by 0158, 0183 | — |
| [0155](0155-errored-subagent-turn-parks-for-steering.md) | An errored sub-agent turn parks the parent for steering, not unblocks it | current | — |
| [0156](0156-normalize-and-stabilize-the-endpoint-pool-key.md) | Normalize and stabilize the endpoint pool key | current | — |
| [0157](0157-mcp-http-transport-shares-the-endpoint-pool.md) | The MCP HTTP transport shares the LLM endpoint pool | current; amended by 0169; changed by #915 | — |
| [0158](0158-defer-session-title-aux-call-under-contended-primary-concurrency.md) | Defer the session-title aux call when the primary model's concurrency cap is contended | current | — |
| [0159](0159-plan-mask-widened-for-explore-delegation.md) | `plan`'s own mask carries `call`/`bash` so its `explore` delegation isn't self-defeating | superseded by 0207 (agent masks are gone; previously unmarked) | — |
| [0160](0160-extended-thinking-round-trip.md) | Extended thinking round-trips as a persisted `ContentPart`, replay gated per model | current; changed by #919 | — |
| [0161](0161-unified-async-work-background-flag-and-one-poll.md) | Unified async work: a `background` flag on every launcher, one `poll` to join | current; partly superseded by 0185; amended by 0166, 0190; changed by #916 | — |
| [0162](0162-agent-send-supervising-a-sub-agent.md) | `agent_send` — supervising a sub-agent instead of replacing it | current | — |
| [0163](0163-live-bash-enablement-is-a-tool-overlay-entry.md) | Live bash enablement is a tool-overlay entry, not its own message pair | superseded by 0195; removed by #923 | — |
| [0164](0164-short-sortable-kind-tagged-ids.md) | Short, sortable, kind-tagged ids behind a pluggable generator | current; amended by 0211; changed by #928 (the generator is no longer pluggable) | — |
| [0165](0165-background-bash-jobs-are-bounded-by-timeout.md) | Background `bash` jobs are bounded by `timeout`, not exempt from it | current | — |
| [0166](0166-migration-note-and-startup-warning-for-stale-tool-names.md) | Migration note + startup warning for stale tool names in masks and permission rules | current | — |
| [0167](0167-embedded-research-agent-profile.md) | Embedded `research` agent profile — a global read-only Q&A entry agent | superseded by 0207 (`research` is a mode, not an agent; previously unmarked) | — |
| [0168](0168-lazy-prompt-refuses-a-known-hibernated-child.md) | The lazy-`Prompt` path refuses a known hibernated sub-agent child instead of blank-respawning it | current | — |
| [0169](0169-startup-mcp-connect-is-concurrent-and-fast-fail.md) | Startup MCP connect is concurrent, and the startup handshake fails fast | superseded by 0226; changed by #915 | — |
| [0170](0170-gitignore-aware-glob-grep-walk.md) | `glob`/`grep`'s shared walk is `.gitignore`-aware | current; mechanism superseded by 0178 | — |
| [0171](0171-zai-streaming-web-search-placement-confirmed.md) | z.ai streaming `web_search` placement confirmed — top-level, final chunk | current; removed by #919 | — |
| [0172](0172-tui-stop-cascade-vs-detach-confirm-modal.md) | TUI stop cascade-vs-detach confirm modal | current | — |
| [0173](0173-watcher-driven-plan-file-changed-notice.md) | Watcher-driven plan-file-changed notice | current | — |
| [0174](0174-authenticated-multi-user-wire-head.md) | Authenticated multi-user wire head — design (not yet built) | superseded by 0181; implementation removed (#686) | — |
| [0175](0175-per-user-admission-gate-on-a-shared-literal-key.md) | Per-user admission gate on a shared literal API key | current; removed by #918 | — |
| [0176](0176-structured-tool-result-is-error-and-duration-fields.md) | Structured tool-result channel: `is_error`/`duration_ms` as fields | current; amended by 0186; inconsistent: Consequences say TUI rendering is untouched, but the ✗/✓ badge is wired | — |
| [0177](0177-wire-allowed-deny-only-tool-overlay.md) | Wire-allowed deny-only tool overlay | current; removed by #923 | — |
| [0178](0178-single-pass-gitignore-pruned-walk-with-scan-budget.md) | Single-pass `.gitignore`-pruned walk with a total scan budget | current | — |
| [0179](0179-lazily-registered-built-ins-advertise-session-scoped.md) | Lazily-registered built-ins advertise session-scoped, like lazy MCP servers | superseded by 0192 | — |
| [0180](0180-build-speed-trims-round-2.md) | Build-speed trims round 2: dependency cuts and integration-test consolidation | current | — |
| [0181](0181-userid-leaves-the-runtime-crate.md) | `UserId` leaves the runtime crate — multi-user seams are embedder-side and session-keyed | current | — |
| [0182](0182-mcp-oauth-device-code-flow-and-closed-refresh-race.md) | MCP OAuth: device-code flow, and closing the cross-process refresh race | current; amended by 0153, 0187 | — |
| [0183](0183-narrate-purpose-and-per-user-aux-pins.md) | A `narrate` aux purpose; per-user aux pins stay embedder-side | current | — |
| [0184](0184-provider-hosted-multi-user-seams.md) | Provider-hosted multi-user seams: per-user token store + relocated provider context | current; changed by #918; changed by #919 | — |
| [0185](0185-rhai-joins-background-and-poll.md) | `rhai` joins `background`/`poll` — cooperative kill, own handle kind | current; amended by 0228 | — |
| [0186](0186-exit-code-joins-the-structured-tool-result-side-channel.md) | `exit_code` joins the structured tool-result side channel | current; changed by #916 | — |
| [0187](0187-mcp-oauth-web-redirect-flow-for-embedders.md) | MCP OAuth web-redirect flow for embedders | current | — |
| [0188](0188-session-keyed-per-user-mcp-scopes.md) | Session-keyed per-user MCP scopes | current; amended by 0226, 0227 | — |
| [0189](0189-oauth-for-llm-provider-endpoints.md) | OAuth for LLM provider endpoints | current; removed by #917 | — |
| [0190](0190-poll-is-always-on-non-maskable-internal-tool.md) | `poll` is an always-on, non-maskable internal tool | current; partly superseded by 0192; amended by 0232 | — |
| [0191](0191-inline-think-tags-catalog-format.md) | Inline `<think>` tags: a catalog-declared thinking format for the OpenAI-compat wire | current | — |
| [0192](0192-universal-advertisement-enforcement-at-dispatch.md) | The advertised tool surface is universal and session-stable; masks enforce at dispatch | superseded by 0207 | — |
| [0193](0193-two-tool-call-modes-and-lazy-tool-discovery.md) | Two tool-call modes (`native` / `invoke`) + lazy tool discovery | superseded by 0196 | — |
| [0194](0194-skills-are-additive-only.md) | Skills are additive-only — the `allowed_tools` mask is removed | current; partly superseded by 0196, 0228; amended by 0193; changed by #920 | — |
| [0195](0195-bash-is-the-default-exec-and-curated-read-only-rules.md) | `bash` is the default exec + curated read-only Allow rules | current; partly superseded by 0196; amended by 0193; changed by #916; changed by #923 | — |
| [0196](0196-tool-search-and-lazy-discovery-replace-the-invoke-envelope.md) | Tool search and lazy discovery replace the invoke envelope | current; §1–§4 superseded by 0226; amended by 0202, 0204, 0207, 0213 | — |
| [0197](0197-compound-bash-commands-grade-per-segment.md) | Compound bash commands grade per segment, shell escape constructs fail closed | current; changed by #916; changed by #923 | — |
| [0198](0198-out-of-mask-tool-calls-are-approvable.md) | Out-of-mask tool calls are approvable | superseded by 0207 | — |
| [0199](0199-session-tool-listing-and-enablement-drive-advertisement.md) | Session tool listing and enablement drive advertisement | current; partly superseded by 0204, 0226; changed by #923 | — |
| [0200](0200-fields-reasoning-capture-and-advertise-discovered.md) | Fields-format reasoning round-trip and transcript-only discovery | current; partly superseded by 0204 | — |
| [0201](0201-unregistered-mcp-tool-dispatch-resolves-truthfully.md) | Unregistered-but-configured MCP tools resolve truthfully: enable or attributed decline | current; amended by 0226; changed by #915 | — |
| [0202](0202-prompt-cache-discipline-anchors-deferral-replay-compaction-date.md) | Prompt-cache discipline: last-turn anchors, session-long deferral, full thinking replay, cached-prefix compaction, session-pinned date | current; partly superseded by 0226 | — |
| [0203](0203-catalog-precision-effort-tiers-zai-thinking-control-dual-endpoints.md) | Catalog precision: effort tiers, thinking-required models, z.ai thinking control, dual z.ai endpoints | current; changed by #915; changed by #919 | — |
| [0204](0204-invoke-fallback-for-client-side-discovery.md) | Client-side discovery: native-first with an `invoke` fallback, never a mutated tools array | superseded by 0226 | — |
| [0205](0205-every-compaction-forks-a-successor-session.md) | Every compaction forks a successor session; no log is ever rewritten in place | current; amended by 0231, 0234, 0243 | — |
| [0206](0206-script-facing-structured-search-bindings.md) | Script-facing structured search bindings: `glob_json` / `grep_json` | current; amended by 0227 | — |
| [0207](0207-permission-modes-replace-agent-borne-authority.md) | Permission modes replace agent-borne authority | current; amended by 0209, 0214, 0218, 0226, 0228, 0232, 0247; changed by #916; changed by #920; changed by #923; changed by #924 | — |
| [0208](0208-pause-resume-a-hold-between-cancel-and-hibernate.md) | `PauseSession`/`ResumeSession` — a hold between cancel and hibernate | current; removed by #921 | — |
| [0209](0209-auto-mode-defaults-to-prompt-not-deny.md) | `auto` defaults to `prompt`, not `deny` | current; amended by 0228, 0247; changed by #924 | — |
| [0210](0210-unknown-session-policy.md) | Unknown-session policy: `SpawnBlank` \| `Refuse` \| `Resume(loader)` | current; amended by 0246; changed by #923; changed by #927 (`Refuse` removed) | — |
| [0211](0211-idgen-context-and-tenant-prefix-inheritance.md) | `IdGen` receives a lineage context; children and successors keep the tenant prefix | current; changed by #928 (no `IdGen`/`IdContext`; `mint_id` keeps the prefix) | — |
| [0212](0212-undo-rewind-via-per-write-file-backups.md) | Undo/rewind of file edits via per-write backups | current; amended by 0236 | — |
| [0213](0213-prune-env-vars-and-catalog-knobs.md) | Prune redundant env vars and catalog knobs | current; changed by #917; changed by #918; changed by #919; changed by #920 | — |
| [0214](0214-retired-agent-logs-resume-as-general.md) | Logs naming a retired agent resume as `general` | current; amended by 0215 | — |
| [0215](0215-resume-fallback-per-session-and-read-only-narrowing.md) | Resume fallback per session, with read-only narrowing | current; amended by 0216 | — |
| [0216](0216-resume-modes-ride-the-replay.md) | Resume modes ride the replay when any session is forced | current; amended by 0218 | — |
| [0217](0217-list-pending-query-for-parked-approvals.md) | `ListPending`: a correlation-id query for parked approvals | current; amended by 0230, 0237; changed by #925 | — |
| [0218](0218-agent-default-mode-is-a-starting-hint.md) | An agent's `default_mode` is a starting hint, not authority | current; amended by 0246 | — |
| [0219](0219-tui-themes-and-keybindings.md) | TUI themes and keybindings are named slots and validated action maps | current | — |
| [0220](0220-web-fetch-ssrf-guard-and-clipboard-image-paste.md) | `web_fetch` is an Exec-graded, SSRF-guarded host tool; the TUI pastes clipboard images for vision models | current; amended by 0225 | — |
| [0221](0221-user-slash-commands-are-head-side-prompt-templates.md) | User slash commands are head-side prompt templates | current; changed by #931 | — |
| [0222](0222-no-silent-echo-first-run-onboarding-and-init.md) | No silent echo: first-run onboarding and `/init` | current; changed by #917 | — |
| [0223](0223-non-compacting-session-fork.md) | Non-compacting session fork (`/fork`) | current; changed by #923 | — |
| [0224](0224-session-working-directory.md) | Session working directory (`/cd`, `Spawn.workdir`) | current; changed by #916 | — |
| [0225](0225-web-fetch-grades-redirect-hops.md) | `web_fetch` grades every redirect hop against the session's rules | current; amended by 0227; changed by #923; changed by #964 | — |
| [0226](0226-one-advertised-surface-mcp-envelope-registered-is-available.md) | One advertised surface: the `mcp` envelope, list-style discovery, and MCP "registered = available" | current; partly superseded by 0227, 0228; amended by 0232; changed by #915; changed by #916; changed by #920; changed by #923; changed by #965 | — |
| [0227](0227-one-binding-path-namespaces-http-skill-libraries.md) | One binding path: every rhai call through the dispatch seam, `mcp::`/`skill::` namespaces, `http()` with hidden credentials, endpoint tools removed | current; amended by 0228, 0233, 0243, 0249; changed by #920; changed by #964 | — |
| [0228](0228-rhai-path-and-args-skill-tools-frontmatter-removed.md) | `rhai` gains `path` and `args`; skill `tools:` frontmatter is removed; `load_skill` lists documented `lib.rhai` signatures | current; partly superseded by 0229; amended by 0230, 0247, 0248 (itself removed by #926); changed by #920; changed by #923 | — |
| [0229](0229-every-skill-is-model-visible-user-only-removed.md) | Every skill is model-visible — `user_only` and the `disable-model-invocation` mapping are removed | current; changed by #920; changed by #923 | — |
| [0230](0230-structured-card-reason-and-honest-scopes-on-tool-request.md) | `ToolRequest` carries a structured card `reason` and the `scopes` an approval honors | current; amended by 0237; changed by #925 | — |
| [0231](0231-compaction-successor-inherits-background-work.md) | A compaction successor inherits its predecessor's background work | current; amended by 0234, 0238, 0241, 0242 | — |
| [0232](0232-spawning-is-graded-bookkeeping-control-tools-non-maskable.md) | Spawning is graded; the bookkeeping Control tools are non-maskable | current; amended by 0235; changed by #923; changed by #964 | — |
| [0233](0233-live-only-nested-call-events-for-rhai-scripts.md) | Live-only `NestedCall` events report a script's binding calls | current | — |
| [0234](0234-live-sub-agents-survive-compaction.md) | Live sub-agents survive compaction | current; amended by 0236 | — |
| [0235](0235-scoped-spawn-rules-grade-the-target.md) | Scoped spawn rules grade the target | current | — |
| [0236](0236-rewind-follows-adopted-sub-agents.md) | Rewind follows adopted sub-agents' backups | current | — |
| [0237](0237-detached-flag-on-tool-request.md) | `ToolRequest` marks a card parked by detached work | current; amended by 0238, 0239, 0240 | — |
| [0238](0238-compaction-moves-a-background-scripts-card.md) | A compaction moves a background script's parked card to the successor | current; amended by 0239, 0240, 0241, 0242 | — |
| [0239](0239-retired-session-keeps-a-seq-high-water.md) | A retired session keeps a seq high-water | current; amended by 0244 | — |
| [0240](0240-approval-resolved-event.md) | `ApprovalResolved` tells every head a card is gone | current; amended by 0242, 0245, 0247 | — |
| [0241](0241-background-script-follows-compaction.md) | A background script follows its session's compaction | current; amended by 0243; changed by #923 | — |
| [0242](0242-compaction-heals-walk-the-whole-chain.md) | Compaction heals walk the whole chain | current | — |
| [0243](0243-running-script-reads-mode-and-overlay-epochs.md) | A running script re-reads its mode and drops overlay entries by mode epoch | current; changed by #923 | — |
| [0244](0244-seq-order-leases-and-one-task-per-id.md) | Seqs in delivery order, leased by detached work, one task per id | current; amended by 0246 | — |
| [0245](0245-a-decision-only-answers-its-own-kind-of-card.md) | A decision only answers its own kind of card | current; amended by 0247; changed by #922 | — |
| [0246](0246-resume-under-ancestors-and-the-teardown-tail.md) | A resumed sub-agent answers to its ancestors, and inherits its predecessor's teardown tail | current; changed by #921; changed by #923 | — |
| [0247](0247-attended-sessions-wait-for-their-approver.md) | Attended sessions wait for their approver | current | — |
| [0248](0248-importable-rhai-library-modules.md) | Importable `rhai` library modules | current; removed by #926 | — |
| [0249](0249-in-process-mcp-servers-and-per-session-skills.md) | Per-call tool grading, host script namespaces and per-session skills | current | — |
