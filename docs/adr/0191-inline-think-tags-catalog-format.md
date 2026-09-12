# 0191. Inline `<think>` tags: a catalog-declared thinking format for the OpenAI-compat wire

- Status: Accepted
- Date: 2026-09-11
- Related: [ADR-0160](0160-extended-thinking-round-trip.md) (whose capture/replay
  rail this reuses), [ADR-0118](0118-ambiguous-stop-reason-bounded-retry.md)
  (the same qwen3.5-class local-model report), #118 ("catalog data, not
  hardcode")

## Context

Reported symptom: a qwen3.5-class model served over a local OpenAI-compatible
endpoint stops working once its own thinking is sent back — the conversation
derails mid-turn.

The OpenAI-compat wire has two ways a reasoning model's thinking can arrive,
and the client handled only the first:

1. **Structured delta fields** — `choices[].delta.reasoning` /
   `reasoning_content`, what z.ai, a vLLM with a reasoning parser, and a
   parsed Ollama model emit. The client already routed those to the
   display-only `LlmEvent::Reasoning` channel, and they never round-trip.
2. **Inline tags** — `<think>…</think>` spans inside
   `choices[].delta.content`, what a *parser-less* server emits for the same
   models (llama.cpp without a reasoning format, a bare vLLM, many local
   servers). The client treated `content` as ordinary text, so the model's
   thinking committed to history as if it had been said aloud and was sent
   back verbatim on every subsequent request — feeding a reasoning model its
   own (or an earlier turn's) thinking as user-visible speech, which is the
   reported breakage.

The existing per-model replay gate, `ModelEntry::replay_thinking`
(ADR-0160), only governed the *captured block* replay path on the
Anthropic wire; the OpenAI-compat client captured no block at all and had
no knob that could change what `content` meant.

## Decision

A per-model catalog field, `thinking_format`, decides how a model emits its
thinking on the OpenAI-compat wire:

- `fields` (the default): structured `reasoning`/`reasoning_content` delta
  fields — behavior byte-identical to before, so every pre-existing catalog
  entry is untouched.
- `inline_tags`: `<think>…</think>` spans inline in `content`. The client
  then:

  1. **Splits at capture.** A streaming state machine
     (`openai/think.rs::ThinkSplitter`) routes span text to
     `LlmEvent::Reasoning` and only the spoken remainder to
     `LlmEvent::Text`. Tags may straddle network chunks; the splitter holds
     back the longest partial-tag suffix and flushes it at stream end as
     whichever rail is active. Unbalanced input degrades safely: an
     unterminated opener means "everything after it is thinking" (a cut
     stream), and a stray closer with no opener passes through as text
     (some builds emit it un-negotiated; dropping it would silently eat
     output).
  2. **Captures a block.** At finish, the whole think text mints one
     `ContentPart::Reasoning { provider: "openai", text, data }` — the
     ADR-0160 rail — so the round's reasoning persists and is renderable.
     `data` is `{"format":"inline_tags","model":…}`: there is no signature
     to preserve, but the payload must be non-empty so replay consumers can
     tell a captured block from a malformed one.
  3. **Strips on replay.** Assistant text already in history (committed
     before the flag existed, or by another client) is stripped of
     `<think>…</think>` spans before rendering — the safety net that fixes
     existing sessions. A captured block replays as the assistant
     message's `reasoning_content` field **only** when the same
     `replay_thinking` knob opts in (`ModelEntry::replays_thinking`, wire
     default **off** on this wire, clamped by `supports_thinking`); with
     replay off the block is dropped, never degraded to text
     (ADR-0160's provider-match and no-degradation rules hold unchanged).

The pair — format + resolved replay — is a `ThinkingSpec` resolved **per
request** against the request's actual model id via
`Catalog::thinking_spec_resolver` (the #550 property: a `model:`-only
profile pin must not fall back to the client's construction-time model). A
model absent from the catalog resolves to the default spec (fields, no
replay): exactly the pre-ADR-0191 behavior.

## Consequences

- The reported qwen3.5 failure mode is fixed by one line of user catalog:
  `thinking_format: inline_tags` on the model's entry. Thinking never
  returns to the model (the default), and it stops polluting the visible
  transcript as assistant speech.
- An endpoint that *does* want thinking back can opt in with
  `replay_thinking: true` — the same knob, one wire-default flip, no code
  change (the #118 property).
- History written before the flag keeps replaying correctly (the strip is
  applied at request-build time, not by rewriting logs).
- Anthropic and Gemini wires are untouched: the field is ignored there.
- One more enum + resolver on the catalog surface; every exhaustive match
  had to decide.

## Alternatives considered

- **Strip tags in the turn loop (core) instead of the provider.** Core would
  then need to know a model's wire format — a provider fact — and the strip
  would apply to every wire uniformly, wrong for a model whose `<think>`
  text is genuinely content. The seam belongs at the provider.
- **Strip tags unconditionally on the OpenAI-compat wire.** A model that
  legitimately emits angle-bracket pseudo-markup would be silently mangled.
  The catalog flag keeps the default byte-identical.
- **Reuse `replay_thinking` alone as the switch.** Replay off already means
  "don't send the block back", but it cannot stop `content`-borne thinking
  from *being* assistant text in the first place — capture and replay are
  different problems, and only capture fixes the transcript pollution.
- **A `<think>`-sniffing heuristic with no catalog opt-in.** False positives
  on ordinary text containing the literal tags; heuristics without a
  declared contract are exactly what #118 rejected for providers.
