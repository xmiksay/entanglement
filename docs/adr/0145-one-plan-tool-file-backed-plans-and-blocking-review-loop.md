# 0145. One plan tool: file-backed plans, a staleness guard, and a blocking review loop

- Status: Accepted
- Date: 2026-07-31
- Supersedes: [ADR-0049](0049-plan-task-tools-as-runtime-state-tools.md)'s `update_plan` half (its `update_tasks` half is unaffected)
- Amends: [ADR-0138](0138-sponsored-build-child-and-propose-plan-cycle.md) (the blocking wait gains a documented, cancellable Stop story), [ADR-0042](0042-plan-acceptance-via-propose-plan-approval-roundtrip.md) (`propose_plan`'s schema)

## Context

`update_plan` and `propose_plan` are two separate state tools that both
collapse to the same wire event, `OutEvent::Plan` — the split is pure
cognitive load for the single-agent flow. Beyond the split, a 2026-07-31
design review found three structural problems:

1. **Plans are not files.** `update_plan`'s payload is an in-memory string on
   the runtime's `SessionView`/wire snapshot — the user can't open it in an
   editor, and there is no on-disk source of truth the agent and user can
   both touch. The plans-folder carve-out (#524, [ADR-0142](0142-trusted-scratch-dir-and-plans-folder-carve-outs.md))
   already gives the `plan` profile write access to `.entanglement/plans/*.md`,
   but nothing wired `propose_plan` to actually use it.
2. **The plan agent does not wait for its own sponsored build.** ADR-0138 gave
   `propose_plan`'s approve path a sponsored `build` child with a parent link
   and already blocks on `collect_child_answer` — structurally the "blocking"
   half was done — but nothing registered that wait with
   `crate::cancel::CancelRegistry`, so a `Stop` sent to the plan session while
   parked on it had no effect: the task ran to completion regardless,
   contradicting every other in-flight tool task's Stop story (#167).
3. **The TUI plan side panel is too narrow to read prose**, and an outline
   view (headings only, 40-column truncated) is a poor fit for reviewing a
   real plan.

## Decision

### One tool: `propose_plan(content: Option<String>, path: Option<String>)`

`update_plan` is **removed entirely — clean break, no alias, no deprecation
period**: a profile's `tools:`/`permission:` referencing it must be updated
(the built-in `plan.md` is). `propose_plan` becomes the sole plan-authorship
tool, still gated by the same default-closed explicit-allowlist membership
ADR-0049 established (`plan_tasks::explicitly_allowlists`, now generic over
any tool name, not `update_plan`-specific).

- **Exactly one** of `content`/`path`. Both or neither is a validation error.
  Malformed calls (both/neither, a non-`.md` or missing `path`, or a stale
  `path` — see below) reply **immediately with no approval prompt at all**: a
  self-correctable model error, not a decision for the human. The JSON Schema
  advertised to the model carries no `required` array (an XOR isn't
  expressible there without provider-specific `oneOf` support quirks); the
  runtime validates and the tool description spells out the constraint in
  prose.
- `content` → the runtime **materializes a file** at
  `.entanglement/plans/<short-session-id>.md` (first 8 chars of the session
  id, mirroring `tui::format::short_id` — duplicated rather than shared
  since `propose_plan.rs` must work in a lean, TUI-less build). A resubmit
  with `content` **overwrites** that file unconditionally: an explicit
  full-content overwrite is "last writer wins" by construction, so `content`
  mode is never subject to the staleness guard below.
- `path` → an existing `.md` file, in-root (a leading `..` or an absolute
  out-of-root path is refused — `propose_plan` has no escape-root approval
  flow of its own; a plan file living outside the project root is out of
  scope for v1). Missing file → refused.
- Either way, the resolved content rides back on **two** channels **before**
  the approval prompt: an `OutEvent::Plan { content, path }` snapshot for the
  plan session itself (new `path: String` field, `#[serde(default)]` for
  backward-compat log replay — see below), and the `ToolRequest.input` JSON
  `{"content", "path"}` (always both, so a `path`-mode approval still shows
  the full text, not just a filename — `tui::tool_render`'s `propose_plan`
  arm reads this shape; a raw model-authored `ToolCall`'s input, rendered
  before resolution, may carry only `path`, in which case the file is named
  instead of left blank, #519). The eventual tool **result** (on both approve
  and reject) also names the file (`"plan file: <path>\n\n..."`), satisfying
  "materialize-and-return-location" without a new wire field on `ToolOutput`.

### Staleness guard (`path` mode only)

The agent must be the *last* party known to have touched the file it
resubmits via `path`. Tracked per session as a **content hash only** — not
the `(mtime, hash)` pair the original design sketch mentioned: the hash is
the authoritative comparison at check time regardless (a full re-read+hash
happens on every `path` resolve), so tracking mtime alongside it would add
filesystem-timestamp-resolution flakiness with no correctness benefit. A new
module, `entanglement-runtime/src/plan_files.rs` (`PlanFileRegistry`), holds
`SessionId → { rel_path, hash }`, kept fresh two ways:

1. `propose_plan` itself, after every successful materialize (`content`) or
   read (`path`).
2. Passively, by a background task (spawned once inside
   `tool_runner::spawn_tool_executor_with_policy`) that subscribes to the
   tool executor's own `OutEvent::FileChange` audit broadcast (#202,
   [ADR-0060](0060-filechange-audit-via-executor-as-path-kind-hash.md)) and
   refreshes the registry's hash whenever *this session's* `edit`/`write`/
   `apply_patch` touches the bound path.

This is the load-bearing design choice: distinguishing "the agent edited the
plan file via `write`/`edit` between build phases" (expected — the intended
review loop) from "the user edited it out of band" (must refuse) requires a
session-attributed signal for *what changed the file*, and `FileChange`
already carries exactly that — no new coupling between `propose_plan` and the
generic tool dispatch ladder was needed; the whole mechanism is a passive
broadcast listener, so `dispatch`/`run_and_reply`'s signatures are untouched.
A `path`-mode resolve with **no prior binding for that exact path** (a first
touch, or a rebind to a *different* file) is never stale — this is #514's
seeding story: a plan file the user (or a prior session) already wrote is
accepted on first reference with no re-read ceremony.

The guard's enforcement point is deliberately narrower than the design
sketch's "or an agent edit of it" framing: a plain `edit`/`write` call on the
plan file is **not itself** gated by this guard (the generic `edit`/`write`
host tools stay plan-unaware) — only `propose_plan(path=...)`'s own resolve
checks it. Extending the guard into the generic edit path would require
threading plan awareness into `host::edit`/`host::write`, which have no
concept of "the currently bound plan file" and shouldn't grow one.

### Approval → blocking build → review loop

Unchanged from ADR-0138's shape, confirmed correct here: **every**
`propose_plan` (once past validation) force-parks on `Ask` unconditionally —
a permission profile can never `Allow` it, because user approval *is* the
tool's semantics; each phase re-asks independently (a multi-phase test,
`every_phase_re_parks_on_ask_independently`, pins this — the profile stays
Allow-all throughout and the tool still re-prompts every time). On approve,
`launch_sponsored_build` sends `InMsg::Spawn` for a **sponsored** `build`
child (ADR-0138), parks the plan session on `WaitingAgent`
([ADR-0139](0139-waitingagent-and-workingtool-turn-substates.md)), and
`.await`s `collect_child_answer` — the **existing** blocking-wait
implementation, reused as-is. The child's **full final report** folds back
as the tool result (now prefixed with the plan file's path); the plan agent
is expected to review it, update the plan file's checkboxes via `write`/
`edit`, and either `propose_plan` the next phase (`path`, since the file
already carries the right content — no need to resend `content`) or stop.

### Stop during the blocking wait: detach by default, cascade is two `Stop`s

What's new here (closing gap 2 from Context): the *whole* `run_propose_plan`
task — the Ask-wait **and** the post-approval blocking build-wait — is now
registered with `crate::cancel::CancelRegistry` from
`tool_runner`'s `Intercept::ProposePlan` arm, exactly like the Permission and
`rhai` dispatch arms already were. A `Stop` targeting the plan session aborts
this task at any point; core's own turn cancellation on the same `Stop`
already means no `ToolResult` is owed (the same "unwind silently" contract
`await_decision`'s `Decision::Stop` arm already documented for the Ask
phase). Since the sponsored `build` child is a fully independent session with
its own tasks, aborting only the plan session's wait **is** detach — no new
code needed to "not stop the child," that's simply what an unregistered
future being dropped already does. **Cascade is not a new backend concept
either**: a head that also wants the child stopped sends it a second,
ordinary `Stop { session: child }` — ordinary because `Stop` already cancels
any session's in-flight work; propose_plan needed no special-casing for this
half. Two integration tests
(`stop_on_the_plan_session_detaches_the_build_child_which_keeps_running`,
`stop_on_both_sessions_cascades_and_stops_the_build_child_too`) pin both
paths, the first proving the child keeps running to completion untouched
after a lone `Stop{plan}`.

**Deferred**: a TUI confirm modal ("stop the build too?") that offers this
choice interactively. It is out of scope here — see Consequences.

### TUI: transcript + status line, no side panel

- The plan side panel ("Plan Outline", `tui/ui/sidebar.rs`'s heading-only
  40-col-truncated render via `pulldown_cmark`) is **removed**. The plan
  already renders full-width wherever the approval prompt renders (a
  pre-existing rendering path, untouched) — removing the panel is the only
  change this bullet needed; the panel was an *additional*, more-truncated
  view layered on top of that.
- `SessionView` gains `plan_path: Option<String>`, folded from
  `OutEvent::Plan`'s new `path` field alongside the existing `plan: Option<String>`
  content fold.
- `/plan` is **repointed**: it used to reveal the sidebar (`show_sidebar`,
  same as `/tasks`); it now requests a new `UiEffect::OpenPlanFile`, handled
  in `tui/editor.rs` exactly like `UiEffect::Export`'s "write, suspend the
  terminal, launch `$EDITOR`" pattern — reusing `launch_editor`/`suspended`
  as-is, just resolving the path from `App::plan_path()` + `App::root()`
  instead of a freshly exported transcript. No plan bound yet → a transcript
  notice via a new generic `App::record_notice(label, message)` (factored out
  of the `"reload"`-labeled `record_reload_status`, which now delegates to
  it) rather than a silent no-op.
- A compact one-line indicator replaces the panel: `sidebar.rs` renders
  `Plan: <path> (pending|accepted)` for the active session when a plan is
  bound, derived from whether its front `pending_tool_request()` is
  `propose_plan` — no new state needed, reusing the same approval queue the
  attention panel already reads.

**Deferred**: a watcher-driven "plan updated by user" transcript notice when
the file changes outside any tool call. See Consequences.

### `OutEvent::Plan` gains `path`

```rust
Plan {
    session: SessionId,
    seq: u64,
    content: String,
    #[serde(default)]
    path: String,
}
```

`#[serde(default)]` so a pre-#513 persisted log (no `path` field) still
replays — `path: ""`, which every consumer (`SessionView::plan_path()`,
`sidebar.rs`) treats as "unknown," never a real bound file. Non-TUI heads
(`run --format text`, `pipe`) get the resolved content on this event exactly
as before; `path` is additive.

## Consequences

### Positive

- A plan is a real file: editable outside the agent, diffable, greppable,
  survives a session restart independent of replay.
- The staleness guard makes the file safe for the user to edit concurrently
  without silently losing their edit to an agent that "knows better" — the
  agent is forced to re-read.
- `propose_plan`'s existing blocking-wait code needed no behavioral change to
  satisfy "blocking build" — only a `CancelRegistry` registration, closing a
  real gap (Stop previously did nothing once past the Ask phase).
- Zero new protocol messages: cascade is "send `Stop` twice," detach is
  "`Stop` targets only the parked session" — both already-general behaviors.

### Negative / neutral

- **Hash-only staleness, not `(mtime, hash)`.** A deliberate simplification
  (see Decision) — accepted as strictly stronger than mtime for correctness,
  weaker only in that it can't distinguish "touched but byte-identical" from
  "never touched," which the guard doesn't need to.
- **No TUI stop-cascade-vs-detach modal.** The TUI currently has **no general
  "interrupt the in-flight turn" keybinding at all** outside the
  approval/`ask_user`-question-parked `Esc` paths (`WaitingAgent` is neither)
  — `Ctrl+C` is a two-stage **quit**, not a turn-level `Stop`. Building the
  modal properly means first adding that missing general capability, which
  is its own scoped piece of work, not a `propose_plan`-specific one. Logged
  in [`../deferred-work-ledger.md`](../deferred-work-ledger.md). The backend
  primitive (detach vs. two-`Stop`-cascade) is implemented and tested either
  way; only the interactive prompt is deferred.
- **No watcher-driven "plan updated by user" transcript notice.** The #329
  watcher (`watch.rs`) is purpose-built for agent/skill/config *definition*
  reload (`LiveDefinitions`, a different reload action entirely) — bolting a
  plans-folder watch onto it would conflate two unrelated reload semantics.
  A dedicated lightweight watch (the existing `spawn_debounced_watcher`
  primitive is agent/skill-decoupled and the natural reuse point) is
  reasonable future work, logged in the same ledger entry.
- **`propose_plan`'s `path` mode has no escape-root support.** An out-of-root
  plan path is refused outright rather than approval-gated like `read`/
  `write`/`bash` (ADR-0109) — acceptable since a plan is expected to live
  under the project's own `.entanglement/plans/`, and `content` mode always
  lands there.

## Alternatives considered

- **Hook the staleness guard into the generic `edit`/`write` dispatch ladder**
  (threading a "is this the bound plan file" check into `dispatch`/
  `run_and_reply`). Rejected: couples every host tool's hot path to plan
  semantics for one caller's benefit; the passive `FileChange`-broadcast
  listener achieves the same freshness tracking with zero coupling.
- **Track `(mtime, hash)` as originally sketched.** Rejected: mtime adds
  filesystem-resolution flakiness (some filesystems round to whole seconds)
  with no correctness benefit once the check re-reads and hashes the file
  anyway.
- **Extend the staleness guard to a plain `edit`/`write` call on the plan
  file, refusing it too when stale.** Rejected: would require `host::edit`/
  `host::write` to know about "the currently bound plan file," which is a
  runtime/session concept those generic, root-scoped tools have no business
  holding.
- **A new `InMsg`/protocol surface for the stop-cascade decision** (e.g. a
  `Stop { cascade: bool }` payload). Rejected: `Stop` is a trusted, engine-
  understood signal with no per-tool awareness; modeling "also stop this
  other session" as two ordinary `Stop` sends needs no protocol change and
  matches how every other multi-session interaction (approve, spawn) already
  composes from single-session primitives.
- **Build the TUI stop-cascade modal now, gated crudely on `AgentState::WaitingAgent`
  alone.** Rejected: `WaitingAgent` is shared with the plain blocking `agent`
  tool call (ADR-0139) — a modal offering to "stop the build too" would be
  wrong/confusing for that unrelated case, and correctly distinguishing them
  needs either a protocol payload or TUI-side event correlation heuristics
  that are themselves nontrivial; better done as a follow-up once the
  general Stop-during-a-turn capability exists.

## References

- #513 (this issue), #512 (parent), #514 (closed — seeding, folded into the
  `path`-mode first-bind-never-stale rule)
- [ADR-0049](0049-plan-task-tools-as-runtime-state-tools.md): superseded (the
  `update_plan` half; `update_tasks` is untouched, still a runtime state tool)
- [ADR-0138](0138-sponsored-build-child-and-propose-plan-cycle.md): amended —
  the blocking wait's Stop story
- [ADR-0042](0042-plan-acceptance-via-propose-plan-approval-roundtrip.md):
  amended — `propose_plan`'s schema (`plan: string` → `content`/`path`)
- #524, [ADR-0142](0142-trusted-scratch-dir-and-plans-folder-carve-outs.md):
  the plans-folder write carve-out this issue's `content` mode writes into
- #329, [ADR-0084](0084-runtime-live-reload-and-managed-file-locking.md): the
  definitions watcher considered (and rejected) for the deferred
  watcher-edit-notice
- [ADR-0139](0139-waitingagent-and-workingtool-turn-substates.md):
  `WaitingAgent`, reused unchanged
- [ADR-0060](0060-filechange-audit-via-executor-as-path-kind-hash.md): the
  `FileChange` audit event the staleness guard's passive listener consumes
