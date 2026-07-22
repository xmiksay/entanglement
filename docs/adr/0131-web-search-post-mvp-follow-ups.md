# 0131. Web search post-MVP follow-ups — persisted results, `pause_turn` continuation, version flag

- Status: Accepted
- Date: 2026-07-22
- Amends [0075](0075-provider-side-web-search-mvp.md) (provider-side web search
  MVP), closing three of its four "Accepted MVP limitations" — persistence,
  `pause_turn`, and the hardcoded Anthropic tool version. Builds on the opaque
  round-trip precedent of `ToolCall.provider_meta`
  ([0085](0085-gemini-native-wire-and-opaque-provider-meta.md)) and the
  multimodal content-block channel
  ([0064](0064-message-content-blocks.md)). Issue #481 (part of the #396
  ledger epic).

## Context

ADR-0075 shipped provider-side web search with four explicitly-accepted MVP
gaps (its "Accepted MVP limitations" section):

1. Search results ride the `Reasoning` channel only — never committed to
   `Message` history, so citations and Anthropic's search-result cache
   pricing are lost across turns.
2. Anthropic's `stop_reason: "pause_turn"` (a long-running server-side search
   pausing rather than finishing the turn) just ends the turn.
3. The z.ai `web_search` source array's exact placement in the streaming wire
   was never confirmed against a live API — the parser scans defensively.
4. The Anthropic server-tool type is hardcoded `web_search_20250305`; the
   newer `_20260209` variant needs a 4.6+ model and was deferred behind a
   `ModelEntry` capability flag.

This ADR closes 1, 2, and 4. Item 3 remains open — see "Verification result"
below.

## Decision

### 1. Persisted search results (`ContentPart::ProviderSearch`)

A new content-block variant,
`ContentPart::ProviderSearch { provider, summary, data }`, joins `Text`/`Image`
(`entanglement-provider::message`). `data` is opaque `serde_json::Value` in the
minting provider's own wire shape — exactly the precedent
`ToolCall.provider_meta` set for Gemini's `thoughtSignature`: it round-trips
**verbatim only to the provider that minted it**. `summary` is a
human-readable rendering (the same `[web_search] …` text the `Reasoning`
channel already streamed) every other converter falls back to.

The wire gains a matching `LlmEvent::ContentBlock(ContentPart)` (additive to
the `Llm` trait's event enum) and a persisted, seq-bearing
`OutEvent::SearchResult { part }` (additive to `OutEvent`, mirroring
`AmbiguousRetry`/`Compacted` — no wire freeze violated, this is a *new*
variant, not a change to an existing one, [0069](0069-trusted-untrusted-wire-frame-split.md)/[0072](0072-protocol-warts-settled-before-serve.md)
only froze the *existing* set). The Anthropic client emits `ContentBlock` for
both `server_tool_use` (on `content_block_stop`, alongside the existing
`Reasoning` line) and `web_search_tool_result` (on `content_block_start`); the
z.ai (OpenAI-compat) client emits one for its `web_search` source array. The
turn loop (`session/round.rs`) appends every round's `ContentBlock`s after its
text when committing the assistant `Message`, and emits their `SearchResult`
events; `session/replay.rs` accumulates `SearchResult` alongside `TextDelta`
the same way, folding into an identical `Message::assistant_content` at every
existing commit point (`Prompt`/`Done`/`Compacted`/`AmbiguousRetry`/the
mid-turn tail).

Each provider's converter treats a `ProviderSearch` block per the opaque
contract:

- **Anthropic** (`anthropic::request::anthropic_blocks`): replays `data`
  verbatim when `provider == "anthropic"` — this is the cache-benefit path
  ADR-0075 named. A block from a different provider (crossed over via a live
  `/model` switch) is silently dropped rather than sent as malformed Anthropic
  wire.
- **OpenAI-compat** (`openai::request::assistant_text`/`openai_content`): no
  block-array assistant-message format exists on this wire, so every
  `ProviderSearch` block — regardless of minting provider — renders as its
  `summary` appended as plain text. No opaque `data` ever reaches this wire.
- **Gemini** (`gemini::request::content_parts`): same as OpenAI-compat —
  `summary` as a plain `{ text }` part, `data` never inspected.

### 2. `pause_turn` continuation, client-owned

Per Anthropic's contract, a `pause_turn` stop is resumed by resending the
paused turn's content blocks verbatim as the next request's trailing
assistant message. The Anthropic client owns this entirely
(`anthropic::mod::stream()`) — core never observes `pause_turn`:

- `sse::handle_frame` accumulates every finalized content block (text,
  `tool_use`, `server_tool_use`, `web_search_tool_result`) into a raw
  `Vec<Value>` in arrival order, alongside its existing per-block `LlmEvent`
  emission. Anthropic streams blocks strictly sequentially (one
  `content_block_stop` before the next `content_block_start`), so a single
  "current block" tracker suffices — no per-index bookkeeping needed.
- On `stop_reason: "pause_turn"`, `stream()` rebuilds the request's
  `messages` array as *original wire messages* + one fresh
  `{"role":"assistant","content": accumulated_blocks}` turn, re-POSTs, and
  keeps yielding from the same `LlmStream` — no `Finish` in between. The
  accumulator keeps growing across continuations (each resend carries the
  *whole* paused turn's content so far, not just the latest segment).
- Bounded by `MAX_PAUSE_CONTINUATIONS` (6) so a pathologically repeating
  pause can't loop forever. If exhausted, the client's own eventual `Finish`
  still reports the raw stop reason (mapped to `StopReason::Other`) — the
  turn loop's ADR-0118 ambiguous-stop retry becomes the fallback safety net,
  same as it already silently was for every `pause_turn` before this change.
- Extended-thinking blocks are **not** captured into the continuation array
  (the signature needed to replay one isn't tracked at all today — a
  pre-existing gap this doesn't widen). A `pause_turn` landing mid-thinking-
  block loses that block on continuation; accepted as a narrow, existing
  limitation rather than a new one.

Usage across continuations is summed (`cumulative_usage`), not overwritten,
so the final `OutEvent::Usage` reports the true multi-request total.

### 3. z.ai streaming placement — verification result

**Attempted, not achieved.** The environment #481 was implemented in had
neither a `ZAI_API_KEY` nor outbound network access, so
`ENTANGLEMENT_LOG_BODIES=1` could not be run against a live Coding Plan
endpoint. The defensive parser (`openai::sse::handle_chunk` scanning both the
chunk's top level and `choices[0].delta` for a `web_search` array) is
unchanged from the #305 MVP; the worst-case behavior is still the
cited-text-only floor ADR-0075 accepted. This item stays open — see the
deferred-work ledger row for #481 — pending a run with real credentials.
Whoever closes it should tighten `handle_chunk` to the confirmed placement
(dropping whichever scan site turns out unused) and update this ADR's status
line to record the confirmed shape, or supersede this section with a new ADR
if the confirmed shape changes the persisted `ContentPart::ProviderSearch`
`data` payload's contract.

### 4. Anthropic web-search tool version as catalog data

`ModelEntry` gains `web_search_tool_version: Option<String>` — catalog data
(`defaults.yml` / a user `providers.yml` override), like every other
capability flag. `None` (every embedded default today) keeps the client's own
`web_search_20250305` fallback constant
(`anthropic::request::DEFAULT_WEB_SEARCH_TOOL_VERSION`); a user (or a future
embedded default once 4.6+ models exist) sets
`web_search_tool_version: web_search_20260209` on a model entry with no code
change. Resolved in the runtime at the same two call sites `WebSearchConfig`
itself is resolved (`main.rs`'s `anthropic_wire_config` and
`build_model_resolver`), threaded as an extra `anthropic_factory`/
`AnthropicLlm::new` parameter alongside — not folded into — `WebSearchConfig`
itself, since the version is a per-model catalog capability, not user
web-search *config* (mirrors how `generation_params()` already reads
capability flags off `ModelEntry`, not off a user-supplied knob).

## Consequences

- `Message`/`Context` history now carries provider-native search data, which a
  head or a future summarization pass could surface (e.g. a citations panel);
  none does yet — out of scope here.
- `entanglement-provider/src/anthropic.rs` and `openai.rs` (already
  grandfathered over the 400-line cap, `scripts/file-cap-allowlist.txt`) were
  split into `anthropic/{mod,request,sse}.rs` and `openai/{mod,request,sse}.rs`
  to land this change without growing an over-cap file further (the file-cap
  gate's own rule) — both allowlist entries are removed, a net debt reduction.
- The Anthropic client's `stream()` now owns a request-issuing loop instead of
  one POST per call; `pause_turn` handling adds real (if bounded) latency and
  request-count variance to a turn that trips it, but this is strictly better
  than the prior silent end-of-turn.
- Item 3 (z.ai streaming placement) stays unverified — tracked, not closed.
  The deferred-work ledger row for #481 stays open until it lands.

## Rejected alternatives

- **Surfacing `pause_turn` to the turn loop as a new `StopReason` variant and
  letting `session/round.rs` drive the continuation.** Rejected per the ADR-
  0075 "zero core/protocol change" posture for web search generally: a
  provider-specific continuation protocol has no meaning to core, which
  already treats an unconfident stop generically via ADR-0118. Keeping it
  entirely client-side means core's turn loop needs no Anthropic-specific
  knowledge, and the ADR-0118 retry composes for free as the fallback when
  the client's own bound is exhausted.
- **Modeling `ContentPart::ProviderSearch`'s `data` as a typed struct per
  provider** instead of opaque `serde_json::Value`. Rejected for the same
  reason `ToolCall.provider_meta` is opaque: the shape is provider-wire-
  specific and only that provider's own converter ever needs to reconstruct
  it; a typed enum would need a variant per provider's exact block shape and
  buys nothing since no other code path inspects `data`'s fields.
- **Threading `web_search_tool_version` through `WebSearchConfig`** (making it
  user-settable from `config.yml`'s `web_search:` section) instead of a
  separate `ModelEntry` field. Rejected because the version is a per-*model*
  capability (which models support `_20260209`), not a per-*search* user
  preference — `WebSearchConfig` already models the latter (`max_uses`,
  `allowed_domains`). Conflating them would let a user accidentally request
  an unsupported tool version for an older model with no catalog signal
  telling them not to.
