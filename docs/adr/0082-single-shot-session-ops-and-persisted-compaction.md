# 0082. Single-shot session ops (`InMsg::Oneshot`) and persisted compaction

- Status: Superseded by [0101](0101-compaction-forks-into-a-new-session-copy-on-write.md)
  (the wire shape and `Compacted` variant stay; the in-place `apply_compaction`
  mutation is replaced with copy-on-write forking)
- Date: 2026-07-15
- Builds on the context-window compaction of
  [0055](0055-usage-cost-and-stop-reason-surfacing.md)-adjacent `Context::compact`
  (#178, prune-only, no ADR of its own — documented in
  [engine.md](../architecture/engine.md)), the parked-turn-state protocol
  round-trip of [0061](0061-parked-turn-state-batch-tool-resolution.md) (the
  seq-bearing persisted-content-event pattern this reuses), the shared-seq
  contract of [0068](0068-shared-per-session-seq-counter.md), and the
  trusted/untrusted wire split of
  [0069](0069-trusted-untrusted-wire-frame-split.md). Issue #324.

## Context

`Context::compact` (`core/context.rs`) only prunes old tool outputs to a
placeholder when a turn would overflow the model's context window — a
mechanical, lossy fallback with no summarization, and its pruning is not a
wire event: a resumed session re-folds the *full* pre-prune history from the
log, since `Session::replay` only ever sees `Prompt`/`TextDelta`/`ToolCall`/
`ToolOutput`/`Done` records. There was also no way to run a single out-of-band
LLM call outside the turn loop at all — the `Llm` trait
(`entanglement-provider::llm`) is streaming-only and its one caller is the
engine's per-turn round-trip (`session/stream.rs`); nothing let a head ask for
"one shot: summarize this conversation" without faking a user prompt (which
would itself get folded into history as user/assistant turns, defeating the
point of shrinking it).

The goal: a session-scoped one-shot LLM op, with **compaction (LLM
summarization)** as its first concrete use, whose result is **durable** —
persisted like any other content event and folded on resume — so a compacted
session stays compacted across a crash/restart, not just for the life of the
in-memory `Context`.

## Decision

**The wire shape is generic; the behavior is not.** `InMsg::Oneshot { session,
op: String, args: serde_json::Value }` is a single new protocol variant whose
`op` is an opaque string and `args` an opaque JSON blob — core does not grow a
plugin/handler registry. `session::ops::run_oneshot` is a plain `match op.as_str()
{ "compact" => ..., other => Error }`. The genericity is in the **wire
envelope** (so a future op needs no new `InMsg` variant, no new
`wire_allowed()`/`variant_name()` entries, no new `SessionCmd`), not in an
extensibility mechanism inside core — see "Rejected alternatives" below.

### Protocol

- `InMsg::Oneshot { session, op, args }` — wire-allowed (mutates only the
  caller's own session, like `SetAgent`/`SetModel`). `args` defaults to
  `Value::Null` when omitted.
- `OutEvent::Compacted { session, seq, summary: String, kept: u64 }` — a
  **persisted, seq-bearing** content event, so it rides the existing generic
  machinery for free: the persistence subscriber (`persistence.rs`) taps every
  `OutEvent` with `session().is_some()` regardless of variant; the `ReplayFrom`
  history responder (`history.rs`) includes every event with `seq().is_some()`.
  Neither needed a line of Oneshot-specific code. `kept` is how many trailing
  messages were preserved verbatim after the summary; v1 always sends `0`
  (below).

### Engine (core)

- `SessionCmd::Oneshot(String, serde_json::Value)`, mapped from `InMsg::Oneshot`
  in `holly::routing::msg_to_cmd` exactly like `SetModel`.
- `session_loop`'s `Oneshot` arm uses the same **defer-via-stash** gate as
  `SetAgent`/`SetModel`: `if s.turn.is_some() { stash.push_back(...); continue; }`
  — a oneshot never runs concurrently with a live turn.
- `session/ops.rs::run_oneshot` emits `Status::Thinking`, dispatches on `op`,
  and (for `"compact"`) renders the transcript, builds a tool-less
  `LlmRequest` (`tools: &[]`), and **reuses `&mut *s.llm` directly** — sound
  only because the stash gate above guarantees no turn is mid-stream. This is
  the one place core calls `Llm::stream` outside `session/stream.rs`; it does
  not race the session inbox (no `tokio::select!` against `Stop`) because a
  oneshot is a single non-interruptible round-trip, not a multi-round turn.
- On success: `Context::apply_compaction(&summary, kept)` replaces the whole
  history with one **user-role** summary message (`system` has no in-history
  wire mapping; an assistant-authored summary is trusted less reliably than a
  user-authored one by some providers) plus any preserved tail, then
  `Compacted`/`Usage`/`Done`/`Status::Done` fire — the same terminal sequence
  as a clean turn, so a one-shot head (`skutter run`) still unblocks on `Done`.
  On failure: the ordinary `emit_turn_error` triple (`Error`/`Done`/
  `Status::Error`), and **`Context` is left untouched** — a failed
  summarization must never lose history.
- `Session::replay`'s `Compacted` arm flushes any pending assistant/tool
  buffers (identical to the `Done` arm — a truncated replay window can end
  between `Done` and the next record) then calls the same
  `Context::apply_compaction`, so live and replayed sessions reach identical
  context from identical logs. Records that follow the `Compacted` fold on top
  normally: the summary is just a new starting point, not a special mode.

## Consequences

- A resumed/replayed session stays compacted — the crash-then-resume case no
  longer silently un-compacts by re-folding the full pre-compaction transcript.
- Adding a second op (e.g. a future "distill decisions" or "extract TODOs")
  needs one new `match` arm in `run_oneshot`, zero protocol/wire/persistence
  changes.
- The old prune-only `Context::compact` is unchanged and stays wired as the
  **automatic pre-round fallback** (#178) — cheap, synchronous, no LLM
  round-trip, used when a turn would overflow the window *right now*. The new
  `"compact"` op is a **deliberate, user/head-triggered** action (TUI
  `/compact`, or any head sending the `InMsg` directly); nothing auto-invokes
  it yet (see below).
- `kept` is real wire surface today even though v1 always sends `0`: a
  keep-tail implementation only has to start populating the field, not touch
  the protocol.

## Rejected alternatives

- **A typed op enum** (`enum OneshotOp { Compact { instructions: Option<String>
  } }`) instead of `op: String` + `args: Value`. Rejected because it re-opens
  the wire on every new op — a new enum variant is a breaking/additive protocol
  change core has to version, exactly the churn ADR-0072 settled the wire to
  avoid. A string tag + opaque args keeps the **wire** stable while `run_oneshot`
  (an internal `match`) grows in ordinary Rust, no serde/wire implications.
- **Runtime-side execution** (the runtime intercepts `Oneshot` off the inbound
  fan-out, like the tool executor intercepts `Approve`/`Reject`). Rejected: the
  runtime has no handle to mutate a session's `Context` — that's core-private
  state, reachable only from inside `session_loop`. Compaction fundamentally
  needs to run where `Context` lives.
- **A generic `OpResult` envelope** (`OutEvent::OneshotResult { op, result: Value
  }`) instead of a dedicated `Compacted` variant. Rejected: `Compacted` is a
  **persisted, replay-folded** event — `Session::replay` needs to pattern-match
  it specifically to call `apply_compaction`, so a generic untyped payload would
  just push the "what shape is `result`" problem from the wire into the fold
  logic, with worse type safety. A future op that needs replay-fold behavior
  gets its own `OutEvent` variant the same way; one that's purely informational
  (no context mutation) could reuse a generic envelope then — that bridge isn't
  crossed yet.
- **Streaming the compaction's summary text** (`TextDelta`s as it generates,
  like a normal turn). Rejected for v1: a oneshot is meant to be a quick,
  boring background operation, not a piece of visible assistant output: the
  visible transcript in the TUI is a one-line notice, not a delta stream, and
  the model text-delta channel now belongs unambiguously to turn output.
  Nothing stops the `oneshot_text` helper (`session/ops.rs`) from surfacing
  progress via `ReasoningDelta`-shaped events later if a long compaction proves
  worth showing incrementally.
- **Keep-tail in v1** (preserving the last N messages verbatim instead of `kept:
  0`). Deferred: a `Tool`-role message replayed without its immediately
  preceding `Assistant` parent (the one that issued the tool call) breaks
  Anthropic's `tool_use`/`tool_result` block pairing requirement — keeping a
  tail safely means keeping *whole turns*, not a flat message-count suffix, and
  that boundary-detection logic isn't built yet. `kept` ships as a field now so
  the wire never needs to grow it later.
- **Auto-invoking `"compact"` on context-window pressure** instead of/alongside
  the existing prune-only `Context::compact`. Out of scope for this issue —
  auto-summarize-on-threshold is a natural, separately-scoped follow-up once
  the manual op has proven itself; today `"compact"` only runs when explicitly
  requested (`InMsg::Oneshot`, the TUI `/compact` command).
