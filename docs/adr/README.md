# Architecture Decision Records

An **ADR** is a short, immutable record of *why* an architecture decision was
made. The [architecture doc](../architecture.md) describes the current state
(*what is*); ADRs are the decision log (*how we got here, and what else we
considered*). The two run in parallel: a decision lands here first, then the
arch doc is updated to reflect it.

## When to write one

Write an ADR for any decision that is **hard to reverse** or that a reader would
reasonably ask *"why?"* about — protocol shapes, crate boundaries, a chosen
pattern over an obvious alternative, security/permission models. Don't write one
for trivial refactors or local naming.

## Format

File name: `NNNN-kebab-case-title.md` (zero-padded, monotonically numbered).
Each record:

```
# NNNN. Title
- Status: Proposed | Accepted | Superseded by ADR-XXXX | Deprecated
- Date: YYYY-MM-DD

## Context
Why this came up — forces, constraints, what the reference projects do.

## Decision
What we chose, precisely.

## Consequences
Positive / negative / neutral effects.

## Alternatives considered
The options rejected and why. (This is the part the arch doc can't carry.)
```

## Status lifecycle

`Proposed` → `Accepted` → (`Superseded by ADR-XXXX` | `Deprecated`). Never edit
an accepted ADR in place — supersede it with a new one that links back.

## Index

| # | Title | Status |
| --- | --- | --- |
| [0001](0001-actor-model-abi.md) | Actor model is the integration ABI | Accepted |
| [0002](0002-session-multiplexed-protocol.md) | Session-multiplexed wire protocol | Accepted |
| [0003](0003-agent-and-permission-profiles.md) | Agent + permission profiles (opencode-style) | Accepted |
| [0004](0004-structured-plan-and-task-events.md) | Structured Plan & TaskList events (profiles + events, both) | Accepted (`TaskList` half superseded by [0039](0039-markdown-task-list.md)) |
| [0005](0005-ndjson-stdio-head.md) | NDJSON stdio head (`run` + `pipe`) | Accepted |
| [0006](0006-core-dependency-hygiene-gate.md) | Layering: core / provider / runtime + core hygiene gate | Accepted (direction superseded by [0053](0053-invert-core-provider-seam.md)) |
| [0007](0007-streaming-llm-and-provider-crate.md) | `entanglement-provider`: streaming `Llm` trait, pooling, retry, rate-limit, reasoning | Accepted (trait placement superseded by [0053](0053-invert-core-provider-seam.md)) |
| [0008](0008-host-tools-workdir-and-bounded-output.md) | Host tools: working-directory root + bounded output | Accepted (lexical-only containment superseded-by-addition by [0054](0054-canonicalizing-symlink-safe-root-containment.md)) |
| [0009](0009-edit-and-bash-host-tools.md) | Host tools: `edit` (search/replace) and `bash` (subprocess + timeout) | Accepted |
| [0010](0010-single-head-crate-and-bash-opt-in.md) | `entanglement-runtime`: the head crate — tools, execution, permissions, sessions | Accepted |
| [0011](0011-tui-head-ratatui-crossterm.md) | TUI head: ratatui + crossterm in `entanglement-runtime` | Accepted |
| [0012](0012-tui-event-buffering-rendering.md) | TUI event-buffering & rendering model | Accepted |
| [0013](0013-keybinding-leader-which-key.md) | Keybinding scheme: leader key + which-key | Accepted |
| [0014](0014-tool-approval-inline-modal.md) | Tool approval UX: inline card vs modal | Accepted |
| [0015](0015-rich-text-pipeline-syntect.md) | Rich-text pipeline: pulldown-cmark → ratatui Text, syntect for code blocks | Accepted |
| [0016](0016-host-tool-empty-result-contract.md) | Host tools: empty-result contract (no silent zero-output) | Accepted |
| [0017](0017-stop-cancels-turn-not-session.md) | `InMsg::Stop` cancels the turn, not the session | Accepted |
| [0018](0018-turn-loop-stash-discipline.md) | Turn-loop command stash discipline | Accepted |
| 0019 | — _(number skipped; no ADR-0019)_ | — |
| [0020](0020-event-sourced-session-persistence.md) | Event-sourced session persistence | Accepted |
| [0021](0021-hierarchical-session-model.md) | Hierarchical session data model | Accepted |
| [0022](0022-subagent-spawn.md) | Sub-agent spawn and parent→child answer relay | Accepted |
| [0023](0023-subagent-spawn-limits.md) | Sub-agent spawn recursion / fan-out limits | Accepted |
| [0024](0024-subagent-permission-gating.md) | Sub-agent spawn permission gating and privilege ceiling | Accepted |
| [0025](0025-runtime-cargo-feature-gates.md) | `entanglement-runtime` cargo feature gates (`cli`/`tui`) for lean library embedding | Accepted (lean-transport claim amended by [0053](0053-invert-core-provider-seam.md)) |
| [0026](0026-async-subagent-spawn-and-poll.md) | Non-blocking sub-agent spawn with handle + `agent_poll` | Accepted |
| [0027](0027-ask-user-interactive-prompt.md) | `ask_user` tool — model-driven user decision prompt | Accepted |
| [0028](0028-session-lifecycle-enumeration-and-backpressure.md) | Session lifecycle: enumeration, explicit close, non-blocking routing | Accepted |
| [0029](0029-external-editor-and-markdown-export.md) | External `$EDITOR` compose + Markdown transcript export | Accepted |
| [0030](0030-tui-file-mentions-and-bash-passthrough.md) | TUI `@file` mention completion + `!bash` passthrough | Accepted |
| [0031](0031-write-host-tool-whole-file.md) | Host tool `write`: whole-file create/overwrite (quartet → quintet) | Accepted |
| [0032](0032-yaml-provider-model-catalog.md) | YAML provider/model catalog: embedded defaults + user override | Accepted |
| [0033](0033-agent-tool-family-and-blocking-agent.md) | `agent_*` tool family: rename `spawn_agent` → `agent_spawn`, add blocking `agent` | Accepted |
| [0034](0034-file-based-agent-definitions.md) | File-based agent definitions: discovery, frontmatter, registry | Accepted |
| [0035](0035-deterministic-system-prompt-assembly.md) | Deterministic system-prompt assembly: preamble + body + brief + env + skills | Accepted |
| [0036](0036-skill-discovery-and-registry.md) | Skill discovery + registry: SKILL.md frontmatter, tier-1 disclosure | Accepted |
| [0037](0037-load-skill-tool-deterministic-resolution.md) | `load_skill` tool: deterministic resolution + path substitution | Accepted |
| [0038](0038-physical-per-agent-tool-restriction.md) | Physical per-agent tool restriction: allowlist/denylist masks specs + dispatch | Accepted |
| [0039](0039-markdown-task-list.md) | Markdown task list: structured `Vec<TaskItem>` → plain snapshot | Accepted |
| [0040](0040-per-profile-spawn-control.md) | Per-profile spawn control: `can_spawn` + spawnable-agents allowlist + target-mode gate | Accepted |
| [0041](0041-update-plan-ownership-default-closed.md) | `update_plan` ownership: `owns_plan` default-closed plan authority + physical read-only `plan` | Superseded by [0049](0049-plan-task-tools-as-runtime-state-tools.md) |
| [0042](0042-plan-acceptance-via-propose-plan-approval-roundtrip.md) | Plan acceptance via `propose_plan` approval round-trip: accept → fresh root `build` session (head-policy handoff) | Accepted |
| [0043](0043-skill-preload-vs-access-independent-mechanisms.md) | Skill preload (`skills:` frontmatter) vs access (`load_skill` tool mask) as two independent agent-definition mechanisms | Accepted |
| [0044](0044-agents-skills-system-prompt-epic-synthesis.md) | Agents, skills & system prompt — epic synthesis: six principles → enforcement map, disclosure tiers, enforcement-locus split, deferred follow-ups | Accepted |
| [0045](0045-call-host-tool-argv-exec-tailed-output.md) | Host tool `call`: argv exec (no shell) with auto-tailed output (`tail=30`, `tail=0` = full) | Accepted |
| [0046](0046-rhai-sandboxed-script-tool.md) | `rhai` host tool: embedded capability-sandboxed script engine; quintet bindings permission-checked per call via a sync/async bridge | Accepted |
| [0047](0047-local-trust-boundary.md) | Local trust boundary: repo is trusted, config precedence system < user < repo, inspection over enforcement | Accepted |
| [0048](0048-serve-head-local-trust-model.md) | `serve` head: local-only WebSocket protocol interface (Vue SPA primary/non-exclusive; browser surface out of scope; loopback + opt-in handshake) | Accepted |
| [0049](0049-plan-task-tools-as-runtime-state-tools.md) | `update_plan`/`update_tasks` as runtime state tools: out of core, gated by the ordinary permission path; plan authorship default-closed via explicit allowlist membership (supersedes 0041) | Accepted |
| [0050](0050-per-endpoint-connection-pool-retry-rate-limit.md) | Provider resilience keyed **per `(endpoint, api-key)`**: RPM budget + `Retry-After` window per base URL + key hash (isolates throttling; multiple keys each get their own limit); retry classifies the response status inside the loop (fixes dead-code #193); `LlmSession` references per-endpoint state (#195) | Accepted |
| [0051](0051-argument-scoped-permission-rules.md) | Argument-scoped permission rule keys `tool(pattern)`: `PermissionProfile::resolve(name, arg)` + dependency-free `*`/`?` glob in core; runtime extracts the argument (command for `bash`/`call`, path for `edit`/`write`/`read`) and threads it through the ancestor clamp, config ceiling, and rhai bindings (#173) | Accepted |
| [0052](0052-approval-scope-and-persisted-grants.md) | Approval scope `Once \| Session \| Always` on `InMsg::Approve` (default `Once`, wire-additive) + a runtime `GrantStore` that upgrades a resolved `Ask` → `Allow` for an exact `(tool, arg)` already granted (never overrides `Deny`, not sub-agent-inherited); `Always` persists to a managed `grants.yml` sibling of `config.yml`, not its ceiling `permissions` section (#174) | Accepted |
| [0053](0053-invert-core-provider-seam.md) | Invert the core↔provider seam: `entanglement-provider` becomes a leaf owning the `Llm` trait + DTOs + `Message` (usable standalone for raw LLM); `entanglement-core` depends on provider and is no longer transport-free; `make tree`/`make check-lean` gates narrowed to UI/web-server + CLI/TUI (supersedes direction of 0006/0007, amends 0025) | Accepted |
| [0054](0054-canonicalizing-symlink-safe-root-containment.md) | Canonicalizing, symlink-safe host-tool root containment: canonicalize `root` once at startup; `resolve_under_root` canonicalizes the resolved target's deepest existing ancestor and requires it under canonical root (blocks final- and middle-component symlink escape for `read`/`edit`/`write` while keeping the create path); `list_files` drops any `glob`/`grep` match whose canonical path escapes — supersedes-by-addition the lexical-only containment of 0008 point 3 (#163) | Accepted |
| [0055](0055-usage-cost-and-stop-reason-surfacing.md) | Usage/cost surfacing: normalize `LlmEvent::Finish` to `{ stop_reason: StopReason, usage: Usage }` (cache dimensions split so each prices once), price it via `ModelPricing::cost_usd`, fold into `SessionUsage`, and emit per-round-trip `OutEvent::Usage { …, cost_usd }`; `MaxTokens` surfaces as a warning; `OutEvent`/`InMsg` drop `Eq` (float cost) (#192) | Accepted |
| [0056](0056-closesession-cascades-over-spawn-subtree.md) | `CloseSession` cascades over the spawn sub-tree: the supervisor breadth-first walks `parent_links` (child→parent) and retires the target plus every transitive descendant — dropping each command channel (→ `SessionEnded`) and tombstoning the id — so an orphaned sub-agent can't keep burning provider tokens with no consumer; explicit-destroy path only, parent `Stop` still does not cascade ([0026](0026-async-subagent-spawn-and-poll.md)) (#180) | Accepted |
| [0057](0057-mid-stream-error-partial-commit-and-retry.md) | Mid-stream LLM error: keep committed context aligned with the display — re-request the turn once if the stream fails before any `TextDelta`/`ReasoningDelta` (`STREAM_RETRIES = 1`, covering the first-byte drop the provider's connect-level retry [0050](0050-per-endpoint-connection-pool-retry-rate-limit.md) can't); if a delta was already shown, commit the partial assistant message with an appended `\n\n[interrupted]` marker (streamed as a final `TextDelta`) before `Error` + `Done`, dropping any half-assembled tool calls — no new wire variant (#181) | Accepted |
| [0058](0058-mid-turn-prompt-folds-into-live-turn.md) | Mid-turn `Prompt` folds into the live turn: drain stashed `Prompt`s into `Context` (`push_user`) at the top of each inner-loop iteration, before the next `LlmRequest`, so mid-turn guidance steers the running turn on the next round-trip instead of replaying as a fresh turn after `Done`; fold site reached only when the prior round called tools, so a post-answer prompt still starts a new turn via the stash; non-`Prompt` commands keep ADR-0018's replay-after-turn discipline (refines [0018](0018-turn-loop-stash-discipline.md), #182) | Accepted |
| [0059](0059-tool-trait-and-registry-live-in-the-runtime.md) | `Tool` trait + `ToolRegistry` move from core to `entanglement-runtime` (re-exported at its crate root): core holds no executable tools, only advertises schemas (`ToolSpec`) and round-trips each call, so the trait/registry are pure runtime vocabulary; delete the dead `Holly.cfg`/`Holly.root` fields and `Session::replay`'s unused `_root`; `ToolSpec`/`ToolCall` stay in provider (re-exported), resolving the placement question 0053 left open — a neutral types crate is the deferred fallback (refines [0006](0006-core-dependency-hygiene-gate.md)/[0010](0010-single-head-crate-and-bash-opt-in.md), resolves [0053](0053-invert-core-provider-seam.md)'s open question, #206) | Accepted |
| [0060](0060-filechange-audit-via-executor-as-path-kind-hash.md) | `OutEvent::FileChange` (dead surface since [0031](0031-write-host-tool-whole-file.md)) is emitted for real by the `tool_runner` executor as `{ session, seq, path, change_kind, hash }` — a SHA-256 of the after-content, never whole-file bytes (which the `broadcast` outbox would clone per subscriber); `edit`/`write` `record` the change into a task-local capture scope the executor stamps with the in-flight call's session/seq, keeping `Tool::run` unchanged; the dead `with_on_edit`/`with_on_write` hooks and `host_tools_with_callbacks` are deleted; `rhai`/direct-`run` paths record nothing (no scope) (part of #200, #202) | Accepted |
| [0061](0061-parked-turn-state-batch-tool-resolution.md) | Parked turn state: the in-flight turn is explicit, serde-serializable session state (`Session.turn: Option<TurnState { pending, iterations }>`) instead of an async-stack continuation — a round ending in tool calls batch-emits every (`ToolCall`, `ToolExec`) pair up front and returns to the session loop, which resolves `ToolResult`s against the pending set in any order (outputs fold on arrival) and re-enters on drain; batch calls thereby execute concurrently (deliberate change from serial in-call-order); replay reconstructs a mid-turn tail as a parked `TurnState` and resume re-offers pending `ToolExec`s at-least-once (fresh `seq`, same `request_id`); the embedder persistence seam is the event log + `Holly::resume` (no snapshot API, no DB in-repo — the runtime JSONL of [0020](0020-event-sourced-session-persistence.md) is the reference); protocol unchanged (refines [0003](0003-agent-and-permission-profiles.md)/[0017](0017-stop-cancels-turn-not-session.md)/[0058](0058-mid-turn-prompt-folds-into-live-turn.md), extends [0020](0020-event-sourced-session-persistence.md), #269-#273, epic #276) | Accepted |
| [0062](0062-collapse-llmsession-placeholder-newtype.md) | Collapse the `LlmSession` placeholder newtype: it wrapped `Box<dyn Llm>`, delegated `stream` straight through, and had an uncalled `inner_mut` — the "per-session retry/rate-limit" state it advertised never existed (that state is **per endpoint**, in the provider's `EndpointPool` keyed by `(base URL, api-key hash)`, deliberately shared across sessions on one endpoint since #217/[0050](0050-per-endpoint-connection-pool-retry-rate-limit.md)). `LlmFactory` becomes `Arc<dyn Fn() -> Box<dyn Llm>>` and `Session::llm` a plain `Box<dyn Llm>`; a session-scoped budget was rejected (would re-fragment what #217 unified). Re-introduce the newtype only when real per-session state arrives (KISS). Resolves the `LlmSession` sub-decision of [0050](0050-per-endpoint-connection-pool-retry-rate-limit.md); provider-internal after [0053](0053-invert-core-provider-seam.md) (#195, epic #190) | Accepted |
| [0063](0063-realtime-model-provider-switch.md) | Realtime model/provider switch with no engine restart: new `InMsg::SetModel { provider, model }` + `OutEvent::ModelChanged`, resolved through a runtime-supplied `EngineConfig::model_resolver: Option<ModelResolver>` (`Arc<dyn Fn(&str,&str) -> Result<ResolvedModel, String>>`) that captures the `Catalog` + warm per-endpoint `HttpClient` (#217) and reuses the startup wire/base/key helpers. The session loop rebuilds `Session::llm` and updates per-session `model`/`generation` + re-budgets the context window (`Context::set_window`); `EngineConfig::pricing` already keys every model so pricing needs no per-session state. Catalog-qualified fields cover model-only and provider switches uniformly; deferred mid-turn like `SetAgent`; replay re-applies it. Chose two `Session` fields over re-introducing the `LlmSession` newtype [0062](0062-collapse-llmsession-placeholder-newtype.md) foresaw (model/generation aren't backend state; KISS). TUI `/model` picker now drives it (#218, epic #190) | Accepted |
| [0064](0064-message-content-blocks.md) | `Message`/`InMsg::Prompt` carry multimodal content blocks: `text: String` → `content: Vec<ContentPart>` (`Text`/`Image` tagged by `type`, `ImageSource::Base64` today) in `entanglement-provider`, threaded through `Context`/`SessionCmd`/the converters. A serde back-compat shim keeps the legacy `text: "…"` shape deserializable (`Message` via a `MessageRepr` `from`, `Prompt::content` via `alias = "text"` + a string-or-array `deserialize_with`) so pre-migration logs still replay; new writes emit `content`. Constructors keep the text ergonomics + normalize empty → no parts; `Message::text()`/`content_text` re-join the text parts. OpenAI renders `image_url`/`data:` URLs, Anthropic `image` base64 blocks (incl. image `tool_result`s). Spawn stays text-only. Migrated early — before persisted logs accumulate — ahead of image capture (#197, epic #196; unblocks #221) | Accepted |
| [0065](0065-read-emits-image-content-blocks.md) | `read` emits image files (`png`/`jpg`/`jpeg`/`gif`/`webp` by extension) as base64 image content blocks: the whole tool-result path goes multimodal, mirroring [0064](0064-message-content-blocks.md)'s `Prompt` migration. `Tool` grows a defaulted `run_content -> Vec<ContentPart>` (only `read` overrides — the other ten tools keep text `run`); `InMsg::ToolResult { output }` → `{ content }` (serde `alias = "output"` shim); `OutEvent::ToolOutput` grows a skip-when-empty `content` field so **replay** rebuilds the image instead of the `[image: …]` display placeholder; `Context::push_tool_content`. OpenAI's `role:"tool"` can't hold an image, so the image rides a trailing `role:"user"` `image_url` message; Anthropic reuses the [0064](0064-message-content-blocks.md) image `tool_result` block array; `rhai` collapses a delegated result to its text parts. Rejected: migrating every tool's `run` (ripple for one producer), text-only `ToolOutput` (loses images on resume), base64-as-text (#221, epic #196) | Accepted |
