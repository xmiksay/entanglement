# 0007. Streaming `Llm` trait + out-of-core provider crate

- Status: Accepted
- Date: 2026-07-04

## Context

The engine needs a real model backend. Two questions had to be settled together
because they're load-bearing and hard to reverse:

1. **Streaming vs. buffered.** The original `Llm` trait (pre-0007) was buffered:
   `async fn complete(req) -> LlmResponse`. But `entanglement` is modeled on opencode,
   and opencode streams — it drives the Vercel AI SDK's `doStream` and surfaces
   `text/event-stream` deltas to the UI token-by-token. `OutEvent::TextDelta`
   was already in the protocol *implying* streaming, yet the backend handed back
   the whole reply at once. The two reference projects studied
   (`nexial/infra`, `f13/knowledge-base`) both converged on a buffered
   `LlmProvider::chat` trait — deliberately *not* the model `entanglement` follows.

2. **Where the backend lives.** A real backend needs an HTTP client (`reqwest`).
   ADR-0006 forbids `reqwest` (and all transport crates) in `entanglement-core`. So the
   backend cannot live in core — but the abstract `Llm` trait must.

## Decision

**Streaming trait in core, concrete backend in a new `entanglement-llm` crate.**

The `Llm` trait becomes streaming:

```rust
pub enum LlmEvent { Text(String), ToolCall(ToolCall), Finish { .. } }

#[async_trait]
pub trait Llm: Send {
    async fn stream(&mut self, req: LlmRequest<'_>)
        -> anyhow::Result<BoxStream<'static, anyhow::Result<LlmEvent>>>;
}
```

- Setup/transport errors (auth, HTTP 4xx, connection) return as the `Err` of
  `stream()`; mid-stream errors arrive as `Err` items in the box stream.
- The returned stream is `'static` (owns its state), so the session loop can
  hold it across `.await` points without borrowing the backend.
- `LlmRequest` gains `model: Option<&str>` (per-profile; `None` = backend
  default) since the factory is profile-agnostic but the model id is per-profile.

`entanglement-llm` is a new workspace member that depends on `entanglement-core` **plus**
`reqwest` (allowed there). It ships the Anthropic backend: a hand-rolled
`POST /v1/messages` with `stream: true`, parsing the SSE frames
(`message_start`, `content_block_delta`, `content_block_stop`, `message_delta`,
… ) into `LlmEvent`s. **No Anthropic SDK crate** — `reqwest` directly.

Tool inputs are JSON objects (Anthropic's `input_schema`). `ToolSpec` gained a
`schema: serde_json::Value` field surfacing as Anthropic's `input_schema`, and
the built-in `update_plan`/`update_tasks` tools grew proper object schemas. The
session parses fields tolerantly (`json_field`) so scripted/test backends that
hand in raw strings still work alongside structured providers.

`Message` gained `tool_call_id: Option<String>` on tool-role messages —
Anthropic's `tool_result` block **requires** `tool_use_id`, so the link from a
result back to its originating call is load-bearing (not just metadata).

## Consequences

- **(+)** Live, token-by-token UI feedback is first-class, matching opencode —
  no future trait reshaping needed to stream.
- **(+)** `entanglement-core` stays pure: the seam (the `Llm` trait) is in core, the
  I/O (reqwest, SSE) is quarantined in `entanglement-llm`. `make tree` keeps passing.
- **(+)** Other providers (OpenAI-compatible, Ollama, …) drop in as further
  modules in `entanglement-llm` or sibling crates, all behind the same trait.
- **(+)** Mid-stream failures are recoverable: partial text is already streamed;
  the failed turn surfaces as an `Error` + `Done` without committing a bogus
  assistant message to context.
- **(−)** `'static` box stream requires an indirection: `entanglement-llm` drains
  reqwest's borrowed `bytes_stream` on a detached task into an owned-byte mpsc
  channel the consumer stream owns. One extra task per turn — negligible cost.
- **(−)** Tool inputs are now JSON objects, so scripted backends/tests that
  bypass schemas must still feed parseable input. Mitigated by tolerant
  `json_field` extraction (raw strings fall through unchanged).

## Alternatives considered

- **Buffered `complete()` (the reference projects' shape).** Rejected: `entanglement`
  follows opencode, which streams. Keeping buffered would make `TextDelta` a lie
  (one full-text event per turn) and force a trait rewrite later.
- **`reqwest` inside `entanglement-core`.** Rejected outright: violates ADR-0006 and
  destroys the headless seam every embedder relies on.
- **An Anthropic SDK crate.** Rejected: hand-rolling against `/v1/messages` is
  ~200 lines, keeps the dep surface to `reqwest`, and avoids coupling to a
  third-party crate's streaming model. `reqwest` was chosen explicitly.
- **Streaming tool-input deltas (fine-grained-tool-streaming).** Deferred: the
  trait assembles a full `ToolCall` before emitting. Streaming partial JSON to
  the UI can be layered on later without changing the trait's shape.
