# 0007. `entanglement-provider`: streaming `Llm` trait, pooling, retry, rate-limit, reasoning

- Status: Accepted — `Llm` trait placement / dependency direction superseded by [0053](0053-invert-core-provider-seam.md)
- Date: 2026-07-07

> **Amended by [ADR-0053](0053-invert-core-provider-seam.md) (2026-07-13).** The
> `Llm` trait + its DTOs (and `Message`/`MessageRole`) moved OUT of
> `entanglement-core` INTO `entanglement-provider`, which is now a leaf crate;
> core depends on provider. The streaming contract, pooling, retry, and
> rate-limit design below are unchanged — only the crate the trait lives in and
> the dependency direction changed.

## Context

The engine needs a real model backend, and a backend needs an HTTP client
(`reqwest`) — which [ADR-0006][0006] forbids in `entanglement-core`. So the
backend lives in a separate crate while the abstract seam (the `Llm` trait)
stays in core. This crate is **`entanglement-provider`**: it owns *all* LLM I/O,
which is more than "send one request." A production coding agent hammers the
provider — many turns, long streams, concurrent sessions — against APIs that
impose per-minute rate limits and transient failures. The provider layer is
where connection reuse, retry, rate-limit backoff, and reasoning/thinking
streams belong; core must stay ignorant of all of it.

`entanglement` follows opencode, which **streams** (Vercel AI SDK `doStream`):
`OutEvent::TextDelta` implies token-by-token deltas, so the trait must be
streaming, not a buffered `complete()`.

## Decision

**Streaming `Llm` trait in core; everything else in `entanglement-provider`.**

### The trait (in core)

```rust
pub enum LlmEvent {
    Text(String),
    Reasoning(String),          // thinking / reasoning tokens, streamed
    ToolCall(ToolCall),
    Finish { input_tokens: Option<u32>, output_tokens: Option<u32> },
}

#[async_trait]
pub trait Llm: Send {
    async fn stream(&mut self, req: LlmRequest<'_>)
        -> anyhow::Result<BoxStream<'static, anyhow::Result<LlmEvent>>>;
}
```

- Setup/transport errors return as the `Err` of `stream()`; mid-stream errors
  arrive as `Err` items in the box stream.
- The stream is `'static` (owns its state) so the turn loop holds it across
  `.await` without borrowing the backend.
- `LlmRequest` carries `model: Option<&str>` (per-profile; `None` = backend
  default).
- **`LlmEvent::Reasoning`** is first-class so extended-thinking output is
  surfaced, not dropped. Core re-emits it as a reasoning `OutEvent` for heads
  to render distinctly from answer text.

### The provider crate (out of core)

`entanglement-provider` depends on `entanglement-core` **plus** `reqwest`
(allowed there). It owns:

1. **The concrete backends**, split by *wire format*, not vendor:

   | client | wire format | serves | auth |
   | --- | --- | --- | --- |
   | `OpenAiLlm` (`openai.rs`) | `/chat/completions` SSE | z.ai (GLM, primary), OpenAI, Ollama `/v1` | `Bearer` / none |
   | `AnthropicLlm` (`anthropic.rs`) | `/v1/messages` SSE | Anthropic | `x-api-key` |

   Hand-rolled over `reqwest` (no SDK crate). `OpenAiLlm` is one generic client
   `{ base_url, api_key: Option, default_model }` with preset base constants
   (`ZAI_CODING_PLAN_BASE` — default, `ZAI_GENERAL_BASE`, `OPENAI_BASE`,
   `OLLAMA_BASE`). Both parse reasoning deltas (Anthropic `thinking` /
   `redacted_thinking` blocks; OpenAI `reasoning_content`) into
   `LlmEvent::Reasoning`.

2. **A connection pool** — a shared, tuned `reqwest::Client` (keep-alive, idle
   pool) reused across sessions rather than a fresh client per turn, so TLS
   handshakes and sockets amortize under concurrent load.

3. **Retry with backoff** — transient failures (connection resets, 5xx, stream
   drops before `Finish`) retry with exponential backoff + jitter, bounded by a
   max-attempts budget.

4. **Rate-limit handling** — respect HTTP 429 and `Retry-After`, plus a
   client-side requests-per-minute throttle so a burst of turns/sessions does
   not trip provider limits. Rate-limit waits surface as status, not silent
   stalls.

5. **A models-per-provider registry** — the set of available models each
   provider exposes, so heads can present a real model picker instead of a free
   text field.

6. **A live session/connection handle** ("unified session") — a stateful object
   the provider hands core that carries the pool/retry/rate-limit context for a
   session's lifetime. Core wraps it per turn; the *conversation history*
   (`Context`) stays in core, but the *connection* state is the provider's. This
   is implemented as `LlmSession` (a newtype around `Box<dyn Llm>`), which makes
   the architectural separation explicit.

**Provider selection** stays a head concern (see [ADR-0010][0010]):
`ENTANGLEMENT_PROVIDER` (`zai|openai|ollama|anthropic`) or key auto-detect,
else `DummyLlm`.

## Consequences

- **(+)** Core stays pure: the seam (`Llm` trait) is in core; all I/O, pooling,
  retry, and rate-limit logic is quarantined in `entanglement-provider`.
  `make tree` keeps passing.
- **(+)** Live, token-by-token UI feedback (text *and* reasoning) is
  first-class, matching opencode.
- **(+)** Concurrent sessions share one connection pool and one rate-limit
  budget, so the agent degrades gracefully under provider limits instead of
  erroring out mid-run.
- **(+)** New providers drop in as further modules behind the same trait.
- **(−)** The `'static` box stream requires draining `reqwest`'s borrowed
  `bytes_stream` on a detached task into an owned-byte channel — one extra task
  per turn, negligible.
- **(−)** Retry/rate-limit adds hidden latency a caller can't see turn-by-turn;
  mitigated by surfacing waits as status.

## Alternatives considered

- **Buffered `complete()` (the reference projects' shape).** Rejected:
  `entanglement` streams; buffering makes `TextDelta` a lie and forces a trait
  rewrite later.
- **`reqwest` inside `entanglement-core`.** Rejected: violates [ADR-0006][0006]
  and destroys the headless seam.
- **A fresh `reqwest::Client` per session/turn (the prior code).** Rejected:
  wastes TLS handshakes and connections under load; a shared pool is the point
  of a provider layer.
- **No retry / rate-limit (bail on the first non-2xx, the prior behavior).**
  Rejected: a coding agent that dies on a single 429 or transient reset is not
  usable; resilience belongs here, once, not re-implemented per head.
- **Provider SDK crates.** Rejected: hand-rolling against the two wire formats
  is ~200 lines each, keeps the dep surface to `reqwest`, and avoids coupling to
  a third party's streaming model.
- **Dropping reasoning tokens (the prior behavior).** Rejected: extended
  thinking is valuable signal; a dedicated `LlmEvent::Reasoning` costs one enum
  variant and one `OutEvent`.

[0006]: 0006-core-dependency-hygiene-gate.md
[0010]: 0010-single-head-crate-and-bash-opt-in.md
