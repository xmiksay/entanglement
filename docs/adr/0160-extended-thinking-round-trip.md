# 0160. Extended thinking round-trips as a persisted `ContentPart`, replay gated per model

- Status: Accepted
- Date: 2026-08-03
- Related: [ADR-0007](0007-streaming-llm-and-provider-crate.md) (established
  `LlmEvent::Reasoning` as a display channel — this narrows that "not committed
  to history" consequence), [ADR-0075](0075-provider-side-web-search-mvp.md) /
  #481 (`ContentPart::ProviderSearch`, whose opaque-round-trip rail is reused
  verbatim), [ADR-0061](0061-parked-turn-state-batch-tool-resolution.md) (the
  parked turn that makes this necessary), [ADR-0085](0085-gemini-native-wire-and-opaque-provider-meta.md)
  (Gemini's `thoughtSignature`, unchanged), and
  [ADR-0064](0064-message-content-blocks.md) (the `ContentPart` seam)

## Context

Reasoning was a display-only channel. `LlmEvent::Reasoning` →
`OutEvent::ReasoningDelta` streamed, rendered and persisted, but
`session/replay.rs` deliberately dropped it from `Context` and
`session/stream.rs` never fed it into `text_buf`, so nothing ever went back to a
provider. `ContentPart` had no variant it *could* have been stored in.

That is correct for two of the three wires and wrong for the third. With
extended thinking enabled, Anthropic requires the final assistant message to
begin with the unmodified `thinking` (or `redacted_thinking`) block, signature
intact, whenever tool results are sent back. That is exactly the shape of every
parked turn (ADR-0061): a round ends in tool calls, results resolve, the
continuation request carries the assistant turn that made them. The client
discarded `signature_delta` outright and captured no thinking block at all, so
enabling thinking together with tools on Anthropic could not work — a limitation
acknowledged in-code at `anthropic/sse.rs` and scoped there only to the narrower
`pause_turn` case.

Two further facts shaped the design. First, the requirement is **turn-scoped,
not conversation-scoped**: Anthropic strips earlier turns' thinking server-side,
so only the last assistant message needs one. Second, on current models
`thinking.display` defaults to omitted — blocks arrive with **empty text but a
live signature**, and such a block is still required on replay, so "capture only
when there is reasoning text" would silently reintroduce the bug.

## Decision

**A new `ContentPart::Reasoning { provider, text, data }`**, carried in history
like any other content part, reusing the ADR-0075/#481 `ProviderSearch` rail end
to end: `LlmEvent::ContentBlock(ContentPart)` → the round's `content_blocks` →
appended to the committed `Message` → a seq-bearing persisted event → folded
back by `Session::replay`. No new `LlmEvent` variant was needed.

`data` is opaque JSON in the minting provider's own wire shape. Anthropic's
`signature`, and whether the block was `redacted_thinking`, live **inside** it
rather than as fields on the variant — the same choice `ProviderSearch` made,
keeping provider-shaped detail out of the core contract. `text` is the
human-readable rendering and may legitimately be empty.

**A new `OutEvent::ReasoningBlock`** rather than reusing `SearchResult`.
Reasoning genuinely is a different kind of block, and renaming `SearchResult` to
something generic would have required a serde alias to keep pre-existing session
logs (`"type":"search_result"`) replayable — a compatibility hazard for no gain.

**Capture is unconditional; replay is a catalog decision.**
`ModelEntry::replay_thinking: Option<bool>` gates only whether a captured block
is *sent back*. Blocks are always assembled, always persisted, always rendered,
so toggling the flag never rewrites a session log or changes what a head shows.
`None` derives from the wire — on Anthropic, on whenever thinking is enabled,
because the API requires the block and a silent `false` surfaces as a 400 the
user cannot diagnose; every other wire off. An explicit value always wins, so a
user can force replay off (to cut input tokens) or on (an OpenAI-compatible
endpoint that does accept reasoning back) with no code change — the same
"catalog data, not hardcode" property #118 established for `wire:`.

**Per-wire behaviour:**

| Wire | Capture | Replay when enabled |
| --- | --- | --- |
| Anthropic | `thinking` assembled across `thinking_delta` + `signature_delta`; `redacted_thinking` captured whole | verbatim from `data`, **first** in the assistant block list, on the **last** assistant message only |
| Gemini | thought-text parts | none — the load-bearing `thoughtSignature` already round-trips via `ToolCall::provider_meta` (ADR-0085), untouched |
| OpenAI-compat | none available on the wire | none |

Three rules hold regardless of the flag:

- **Provider match is a hard gate.** A block whose `provider` differs from the
  target renders *nothing*, even with replay on. This is deliberately stricter
  than `ProviderSearch`, which degrades to its `summary` text: reasoning is not
  answer content, so leaking it into history on a live `/model` switch would
  corrupt the conversation.
- **Reasoning is never text.** `ContentPart::as_text` returns `None` for it, so
  it stays out of `content_text` — the token estimator, compaction, and the
  text-only converters never see it.
- **An unsigned thinking block is display-only.** Anthropic rejects one whose
  signature is missing, so a block that never received a `signature_delta` is
  not captured for replay.

Ordering is restored in `anthropic_blocks` rather than constrained in core: the
turn loop appends content blocks after the round's text, and Anthropic wants the
thinking block first, so the converter reorders. Lifetime is enforced in
`convert_messages`, which passes the replay flag only for the last assistant
message — history keeps every block (replay fidelity), the wire doesn't resend
blocks the provider discards on arrival.

Capturing the block also closes the `pause_turn` gap: `assembled_blocks` now
includes thinking, so a pause landing mid-thinking-block no longer loses it.

## Consequences

- Enabling thinking together with tools works on the Anthropic wire. Previously
  it could not.
- One more variant on `ContentPart` and one on `OutEvent`; every exhaustive
  match had to make an explicit decision, which is the intended forcing function
  (the ADR-0064 rationale).
- A resumed parked turn reconstructs the same block the live turn had, because
  the persistence rail and the display rail are separate events.
- Reasoning blocks accumulate in history for models that think. They cost
  nothing on the wire (only the last assistant turn replays) but they do sit in
  the session log.

## Rejected

- **Opaque blocks on the parked `TurnState` instead of a `ContentPart`.** Less
  contract surface, but the block would then exist only while a turn is parked —
  invisible to compaction, to `/inspect`, and to any future wire that wants it.
- **Flat `{ text, signature, redacted }` fields on the variant.** Puts
  Anthropic's wire shape into a core type shared by three wires; `ProviderSearch`
  had already settled this the other way.
- **Renaming `SearchResult` to a generic `ContentBlock` event.** Would need a
  serde alias to keep existing logs replaying, with a silent-breakage failure
  mode if it were ever wrong.
- **Replaying reasoning on every assistant turn.** Anthropic strips all but the
  last, so it is pure input-token cost.
- **Degrading a foreign provider's reasoning to text** (the `ProviderSearch`
  fallback). Would inject the model's private reasoning into history as if it
  had said it aloud.
- **A hardcoded per-wire replay rule with no catalog knob.** Same objection #118
  raised for providers generally: a user adding an endpoint should not need a
  code change to make it behave.
