# Wire protocol §4 — Structured outputs

> Part of the [wire protocol module](../protocol.md) of the [architecture overview](../../architecture.md).

## 4. Structured outputs (orthogonal to profiles) — [ADR-0004](../../adr/0004-structured-plan-and-task-events.md)

Two artifacts the engine owns and re-emits as **full snapshots** on every change
(the `agent`/`design` "snapshot on change" pattern — idempotent, trivial to
render/dedupe):

- **Plan** — markdown strategy prose (`OutEvent::Plan { content, path }`, `path`
  added #513/[ADR-0145](../../adr/0145-one-plan-tool-file-backed-plans-and-blocking-review-loop.md) —
  the plan is a **file** now, `.entanglement/plans/<short-id>.md` by default;
  `#[serde(default)]` on `path` keeps a pre-#513 persisted log replayable).
- **TaskList** — markdown task outline, typically a `- [ ]`/`- [x]` checklist
  (`OutEvent::TaskList`). Plain `content` like the plan (✅ #142,
  [ADR-0039](../../adr/0039-markdown-task-list.md), supersedes ADR-0004's structured
  `Vec<TaskItem>`): the outline is **user-facing progress info** — the engine
  never consumed the item structure and the list is not fed back to the model,
  so the per-item id/status JSON envelope was pure model overhead.

Both are written by **runtime state/orchestration tools** the model calls —
`propose_plan(content: Option<String>, path: Option<String>)` (✅ #141/#513,
exactly one of the two — file-backed, not an in-memory snapshot; see the
engine doc's "Plan acceptance" section) and `update_tasks { content }`
(markdown, ✅ #231,
[ADR-0049](../../adr/0049-plan-task-tools-as-runtime-state-tools.md)). Neither is
an engine built-in: `update_tasks` carries `Capability::Control`
([ADR-0207](../../adr/0207-permission-modes-replace-agent-borne-authority.md)
§3), so it is **never graded** — it runs (and the runtime executor emits its
`OutEvent::TaskList` snapshot) unconditionally in every mode, since it
touches only session bookkeeping, not the host (the engine holds no task
state either way) — `propose_plan` carries `Capability::Plan` instead and
additionally force-parks on `Ask` unconditionally (see the engine doc's
"Plan acceptance" section), since its `OutEvent::Plan` snapshot is only one
part of a larger approval + mode-switch flow (no sponsored build child any
more, ADR-0207 §7 retires that mechanism wholesale). Plan authorship is
graded via the `plan` mode class instead of tool-mask allowlist membership
now: `propose_plan` is advertised **unconditionally** to every session, but
usable only where the session's mode allows `Capability::Plan` (`plan` mode
alone, in the built-in table) — a mode denying it declines the call flat,
before any file is touched. A read-only mode (`research`) denies both the
`write` class `update_tasks` never needed and the `plan` class
`propose_plan` does, which is the #175 fix's current shape.

This is why `entanglement` has *both* an agent-identity axis *and* structured
events: the permission **mode** controls **what the session is permitted to
do**; structured events give every head a native plan/task panel to render.

**Usage & cost** (✅ #192, [ADR-0055](../../adr/0055-usage-cost-and-stop-reason-surfacing.md)).
The provider normalizes each round-trip's terminal `LlmEvent::Finish` to
`{ stop_reason: StopReason, usage: Usage }` — `StopReason` collapses both wire
vocabularies (`EndTurn | ToolUse | MaxTokens | StopSequence | Other`), and `Usage`
splits the token counts so each maps to one catalog pricing dimension without
double-counting (`input_tokens` is the *uncached* input; the OpenAI client
subtracts its cache reads out of `prompt_tokens`, Anthropic already separates
them). The engine prices the round-trip via `ModelPricing::cost_usd` (effective
model = `profile.model` else `EngineConfig.default_model`, looked up in
`EngineConfig.pricing`), folds it into the session's `SessionUsage` running total,
and emits `OutEvent::Usage` — the **per-round-trip delta**, so a head sums deltas
for its own total. `cost_usd` is `None` when no catalog pricing covers the model.
A `MaxTokens` finish additionally emits a recoverable `OutEvent::Error`
(truncation warning) — the reply still commits, but no longer silently. Because
`cost_usd` is a float, `OutEvent` (and `InMsg`, via `Resume`) are `PartialEq` but
not `Eq`.

---

Previous: [§2 — Live generation, metadata & tool overlay](live-settings.md)
