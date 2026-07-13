# 0055. Usage/cost surfacing + normalized stop reason (`OutEvent::Usage`)

- Status: Accepted
- Date: 2026-07-13

## Context

Both provider clients already populated `LlmEvent::Finish { input_tokens,
output_tokens }` (`openai.rs`, `anthropic.rs`), but the engine discarded it:
`Ok(LlmEvent::Finish { .. }) => {}` in `session/turn.rs`. Consequences
([#192](https://github.com/xmiksay/entanglement/issues/192), part of the
provider-seam epic #190):

- **No usage-bearing `OutEvent`.** A head could not show tokens or cost, and the
  catalog `pricing` block (with `cached_input`/`cache_write`,
  [ADR-0050](0050-per-endpoint-connection-pool-retry-rate-limit.md)/#118) had
  nothing to multiply. The TUI shipped `add_input_tokens`/`add_output_tokens`
  with **zero callers**.
- **No stop reason at all.** A `max_tokens`-truncated reply committed as a clean
  turn — indistinguishable from a natural completion.
- **`Finish` carried raw per-provider counts.** OpenAI's `prompt_tokens`
  *includes* its cached reads while Anthropic reports cache reads/writes
  separately, so a naive `input * price` double-counts cached tokens on OpenAI
  and misses the cache dimensions entirely on Anthropic.

## Decision

Fold `Finish` into the engine and re-emit it as a first-class protocol event.

### Provider: normalize `Finish`

`LlmEvent::Finish` becomes `{ stop_reason: Option<StopReason>, usage: Usage }`:

- **`StopReason`** (new leaf enum) normalizes both wire vocabularies:
  `EndTurn | ToolUse | MaxTokens | StopSequence | Other`. `StopReason::from_openai`
  maps `finish_reason` (`stop`/`tool_calls`/`length`/…); `from_anthropic` maps
  `stop_reason` (`end_turn`/`tool_use`/`max_tokens`/`stop_sequence`).
- **`Usage`** (new struct) carries `input_tokens`, `output_tokens`,
  `cached_input_tokens`, `cache_write_tokens` (each `Option<u64>`), **normalized
  so each maps to exactly one pricing dimension without double-counting**:
  `input_tokens` is the *uncached* input. The OpenAI client subtracts
  `prompt_tokens_details.cached_tokens` from `prompt_tokens`; Anthropic already
  reports `input_tokens` / `cache_read_input_tokens` /
  `cache_creation_input_tokens` separately, so no split is needed. OpenAI does not
  bill cache writes, so `cache_write_tokens` stays `None` there.
- **`ModelPricing::cost_usd(&Usage)`** lives beside the pricing data (in the
  provider `catalog`): each dimension × its per-million rate, a missing rate
  counting as zero.

### Core: fold + emit `OutEvent::Usage`

`run_turn` captures the `Finish` of each LLM round-trip and, before committing the
assistant message:

1. **Prices it.** The effective model is `profile.model` else the backend's
   `EngineConfig.default_model`; its `EngineConfig.pricing` entry (a
   `HashMap<model_id, ModelPricing>` the runtime fills from the catalog) yields
   `cost_usd`, or `None` when no entry covers the model.
2. **Folds it into session state.** `Session.usage` (`SessionUsage`) accumulates
   the tokens + dollar cost across the whole session.
3. **Emits `OutEvent::Usage { session, seq, input_tokens, output_tokens,
   cached_input_tokens, cache_write_tokens, cost_usd: Option<f64> }`** — the
   **per-round-trip delta** (a turn with tool calls emits one per round-trip), so
   a head accumulates deltas for its own total.
4. **Warns on truncation.** A `StopReason::MaxTokens` finish emits a recoverable
   `OutEvent::Error` ("model response truncated … max_tokens"); the reply still
   commits, but the truncation is no longer silent.

Because `cost_usd` is an `f64`, `OutEvent` (and `InMsg`, which embeds it via
`Resume`) drop the `Eq` derive and keep `PartialEq` only.

### Runtime: display

The stdio `run` head renders a `$ usage:` line; the TUI wires the long-dormant
`add_input_tokens`/`add_output_tokens` (plus a new accumulated `cost_usd`) off the
`Usage` event and shows `N in / N out ($cost)` in the input panel.

## Consequences

- Positive: tokens and a priced `cost_usd` reach every head; the catalog pricing
  (including the cache dimensions) is finally exercised, and cached tokens price
  once because `Usage` is pre-normalized.
- Positive: a `max_tokens` truncation surfaces as a warning instead of masquerading
  as a clean turn — the load-bearing fix.
- Positive: additive on the wire. `OutEvent::Usage` is a new tagged variant; older
  logs/heads without it are unaffected, and replay ignores it (display-only, like
  `Plan`/`TaskList`).
- Neutral: cost lives in core, not the runtime — core already depends on the
  provider (ADR-0053), so `ModelPricing`/`Usage` are in reach and the engine owns
  the one place that knows a round-trip's effective model. The runtime only
  supplies the catalog-derived pricing table.
- Negative: `OutEvent`/`InMsg` lose `Eq` (float cost). No caller relied on it (no
  `HashSet`/map-key use); `PartialEq` covers the `assert_eq!` tests.

## Alternatives considered

- **Compute cost in the runtime, off the `Usage` event.** The runtime would have
  to re-derive each round-trip's *effective* model (profile pin vs. backend
  default) that core already resolves. Pricing the turn where the model is known
  keeps one source of truth; the runtime just hands core the catalog table.
- **Carry raw provider counts in `Finish` and normalize in core.** Spreads
  provider-specific quirks (OpenAI's cache-in-`prompt_tokens`) into the engine.
  Normalizing at the provider boundary keeps core provider-agnostic — the whole
  point of the seam.
- **Cumulative totals in the event.** Emitting the running session total per event
  couples heads to core's accumulator and mis-renders on replay/reconnect.
  Per-round-trip deltas let each head sum what it saw; core keeps its own total in
  `SessionUsage` for completeness.
- **Abort the turn on `MaxTokens`.** The truncated text is still useful and the
  turn is already effectively over; a non-fatal warning informs without discarding
  work.
