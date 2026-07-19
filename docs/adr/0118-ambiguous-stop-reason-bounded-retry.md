# 0118. Bounded retry on an ambiguous LLM stop, instead of ending the turn

- Status: Accepted
- Date: 2026-07-19

## Context

Reported symptom (local/Ollama-backed models, e.g. a `qwen3.5`-class model
served over `ENTANGLEMENT_PROVIDER=ollama`): the assistant announces intent to
act ("Creating it now, let's get started!") and the turn simply ends — no
tool call, sometimes mid-sentence. The user has to say "continue", which
starts a **new** turn with no memory of being interrupted, so the model
repeats the same "announce intent, then stop" pattern indefinitely.

Root cause traced to two places:

1. `entanglement-core/src/session/turn.rs::run_round` — the *only* signal
   that ends a turn is `tool_calls.is_empty()`. `stop_reason`
   (`Option<StopReason>` from the provider) was inspected only for
   `StopReason::MaxTokens` (a truncation warning, ✅ #192,
   [ADR-0055](0055-usage-cost-and-stop-reason-surfacing.md)) — every other
   value (`None`, `Some(EndTurn)`, `Some(Other)`, even a self-contradictory
   `Some(ToolUse)` with zero actual tool calls) was treated identically to a
   deliberate, clean stop, and the partial/truncated text was committed to
   context as a *complete* final reply.
2. `entanglement-provider/src/openai.rs` — the client shared by z.ai/OpenAI/
   Ollama (no provider-specific wire handling exists for any of them). When
   the stream closes with no `finish_reason` ever observed — which Ollama is
   known to do on a truncated/aborted generation — `stop_reason` resolved to
   bare `None`. Separately, `has_pending_tools` was computed from the raw
   in-flight tool-call accumulator *before* the malformed/incomplete-JSON
   drop loop ran, so a single tool call dropped for bad JSON still produced
   `stop_reason = Some(ToolUse)` even though zero `LlmEvent::ToolCall`s were
   ever yielded — a contradiction the turn loop had no way to detect.

`StopReason` (`entanglement-provider/src/llm.rs`) has five variants:
`EndTurn`, `ToolUse`, `MaxTokens`, `StopSequence`, `Other`. No existing ADR,
test, or issue covered "reply has text but zero tool calls and an ambiguous
stop reason" — `entanglement-core/tests/turn_loop.rs` only exercises the
opposite failure mode (every reply is another tool call, a runaway loop).

## Decision

Classify a round that ends with **empty `tool_calls`** by its `stop_reason`,
via `session::turn::is_confident_stop`:

- **Confident** (`EndTurn`, `MaxTokens`, `StopSequence`) → end the turn as
  before: emit `Done`/`Status::Done`. `MaxTokens` keeps its existing
  truncation-warning `Error` (ADR-0055), unchanged.
- **Ambiguous** (`None`, `Other`, or a contradictory `ToolUse` with no actual
  tool calls) → don't end the turn. Inject a short synthetic user-role nudge
  ("your previous response may have been cut off... call a tool now, or
  confirm you're finished") into context and retry the round **in place** —
  same `run_round` call, no new park, no round-trip through the runtime tool
  executor — bounded by a new `TurnState::ambiguous_retries` counter capped
  by `EngineConfig::max_ambiguous_stop_retries` (default 2). Any round that
  *does* produce real tool calls resets the counter to 0 — only a
  *persistently* ambiguous model exhausts the budget. Exhausting it ends the
  turn with a distinct warning `Error` ("model stop was ambiguous ... after N
  retries — response may be incomplete") followed by the normal
  `Done`/`Status::Done`, instead of silently succeeding.

`max_ambiguous_stop_retries` is a separate knob from `max_turns` — an
ambiguous retry still increments `TurnState::iterations` too, so `max_turns`
remains the hard outer backstop regardless of how the new knob is configured
(including set to 0, which disables the retry entirely and restores the
pre-ADR-0118 behavior).

Companion fixes shipped in the same change (both feed the classification
above, not separable without leaving it half-working):

- `entanglement-provider/src/openai.rs`: track `emitted_any_tool_call`
  (what was actually yielded) instead of gating the `ToolUse` fallback on
  `has_pending_tools` (what was merely *accumulated* before the malformed-JSON
  filter ran) — so a dropped tool call correctly produces an ambiguous
  signal instead of a false `ToolUse`.
- `entanglement-provider/src/llm.rs`: `DummyLlm`, `EchoLlm`, and
  `stream_from_response` (the stub/test `Llm` backends, used directly by
  ~20 integration test files and by `EchoLlm` as `EngineConfig::default()`'s
  own backend) now report an honest `Some(StopReason::EndTurn)` /
  `Some(StopReason::ToolUse)` instead of a bare `None`. Required: once `None`
  is load-bearing as "ambiguous, retry", every existing stub-backed test and
  every embedder relying on `EchoLlm`/`DummyLlm` for a real reply would
  otherwise start retrying (and eventually warning) on an entirely ordinary
  completion.

## Consequences

- Positive: a model that gets cut off mid-thought (announced intent, stream
  died before completing or even starting a tool call) now gets a bounded
  chance to actually finish the action, with no user intervention — directly
  fixing the reported symptom.
- Positive: `max_turns` still bounds the worst case unconditionally, so this
  introduces no new infinite-loop surface — "do not end loops early, but do
  not allow infinite cycles" is satisfied by two independent, composable
  counters.
- Positive: the stub `Llm` backends now model a real provider's stop-reason
  contract honestly, which the rest of the engine (this change and future
  ones) can lean on instead of treating `None` as meaningless.
- Negative: a persistently ambiguous model now costs up to
  `max_ambiguous_stop_retries` extra round-trips (latency + token spend)
  before the turn gives up — mitigated by a small default (2) and the knob
  being settable to 0 to opt out entirely.
- Negative: a custom `Llm` implementation that never sets `stop_reason` (i.e.
  always reports `None`) now sees retries on every text-only reply where
  before it saw none. Mitigated by the small bounded default and by this ADR
  documenting the expectation that a backend report `EndTurn`/`ToolUse`
  honestly, the same expectation the built-in clients (OpenAI-compat,
  Anthropic, Gemini) already meet on a clean stream.

## Alternatives considered

- **Heuristic text-content sniffing** ("does the assistant's text sound like
  it announced an intent to act"). Rejected: fragile across languages and
  writing styles, easy to false-positive/negative, and a clean structural
  signal (`stop_reason`) was already available and simply being ignored.
- **Unbounded retry until a confident stop.** Rejected outright — violates
  "do not allow infinite cycles"; a persistently confused model, or a
  backend that never reports `stop_reason`, would loop forever.
- **Retry on any empty `tool_calls`, regardless of `stop_reason`.** Rejected:
  would retry even a deliberate `EndTurn`/`MaxTokens`/`StopSequence` reply —
  the overwhelmingly common case — adding latency and cost to every ordinary
  "the model is just done" turn.
- **Fix only `entanglement-provider/src/openai.rs`** (the wire-level bug) and
  leave `turn.rs`'s termination logic untouched. Rejected: the true
  structural gap is in the engine's turn-ending decision, not one provider's
  stream reconciliation — Anthropic and Gemini clients can also report
  `None`/`Other` on their own ambiguous endings, and a fix scoped to one
  provider would not generalize to them.
- **Bundle the `[DONE]`-as-authoritative-terminator and trailing-buffer-flush
  fixes for `openai.rs`, and an Ollama `max_output_tokens` catalog default,
  into this change.** Deferred: once `turn.rs` treats a bare `None`
  `stop_reason` as ambiguous-and-retried, every scenario those two wire-level
  gaps could produce degrades to that already-handled case — there is no
  remaining silently-`Done` outcome for them to cause, so fixing them is a
  pure robustness improvement with no attached user-visible bug. The catalog
  default is a separate product judgment call (a wrong cap either truncates
  legitimate long generations or does nothing) that also wouldn't help a
  model absent from the catalog entirely (the more common real Ollama setup),
  so it doesn't belong bundled with an engine-logic fix.
