# 0200. Fields-format reasoning round-trip and transcript-only discovery

- Status: Accepted
- Date: 2026-09-14
- Related: [ADR-0160](0160-extended-thinking-round-trip.md) (the capture/replay
  rail this extends to the Fields wire), [ADR-0191](0191-inline-think-tags-catalog-format.md)
  (the sibling `inline_tags` capture path this mirrors), [ADR-0196](0196-tool-search-and-lazy-discovery-replace-the-invoke-envelope.md)
  (the `client_side` encoding this amends), [ADR-0199](0199-session-tool-listing-and-enablement-drive-advertisement.md)
  (part 2's overlay-enable⇒advertisement write, also amended), [ADR-0192](0192-universal-advertisement-enforcement-at-dispatch.md)
  (dispatch-side enforcement — why an unadvertised registered call still runs)

## Context

A local server (a qwen-derivative model behind an OpenAI-compat endpoint under
our control) keeps an all-or-nothing prompt-cache snapshot: any byte
difference from the previous request's prefix forces a full re-prefill instead
of a cache hit. Two independent client behaviors were busting that prefix on
this setup.

**Reasoning capture gap.** `openai/sse.rs`'s `handle_chunk` already turns
`delta.reasoning_content`/`delta.reasoning` (the `ThinkingFormat::Fields`
wire — the catalog default) into display-only `LlmEvent::Reasoning`, exactly
as ADR-0191 describes for the pre-inline-tags baseline. But nothing captured
those deltas into a persisted `ContentPart::Reasoning` block the way
`ThinkSplitter::into_reasoning_block` does for `InlineTags`. So the next
request re-sent the assistant turn **without** the reasoning the server had
generated and KV-cached for that exact prefix — byte divergence, full
re-prefill, every turn.

**Client-side advertisement growth.** Independent of the above, `ToolSearch`
mode's `client_side` encoding (ADR-0196 §3) appends a `describe()`d tool's
full schema into the session's advertised `tools` array — a deliberate,
documented one-off cache bust per discovery, and ADR-0199 part 2 added a
second writer (overlay-enable) of the same append. On a server under our
control, this growth buys nothing: ADR-0192 already made permission
enforcement dispatch-side only, so a registered tool the client never
advertised still executes fine when called — the model already has the
schema, delivered as the `describe()` tool-result text sitting in the
transcript. The array need never grow to make that schema usable.

A note on scope, since the first finding is easy to over-read: qwen3.5/ornith-
class models are typically **trained with prior-turn thinking stripped from
history** — the chat template that produced their SFT/RL data never showed the
model its own past reasoning at message N when training message N+1. Replaying
captured reasoning back to such a model is therefore not a missing feature to
turn on; it would feed the model an input shape it never saw in training.
`ModelEntry::replay_thinking` already defaults `false`, and that is the
*correct* posture for this class of model — this ADR does not change it or
suggest flipping it. What this ADR's first fix closes is strictly the
**capture** gap: the block must exist (unconditionally) so that whether to
replay it is a per-model choice at all, rather than a foregone "can't, nothing
was ever captured."

That leaves a real consequence: even with capture-only (replay off) correctly
matching how these models were trained, the assistant's own last-turn text
still lands one byte later in the next request than where the server's cache
entry ended generation — the server generated and cached through the end of
its own reasoning + text for that turn, but the client's next request replays
only the text. That tail divergence is inherent to a capture-only client and
is not a bug this ADR's Fix A can close; it is a **server-side** placement
question — snapshot at end-of-prefill, before generation starts, so the next
request only re-prefills the short stripped tail rather than the whole
prefix — and out of scope for this codebase. For this class of setup, the
**advertise_discovered** knob below (Fix B) is the primary and sufficient
client-side cache fix: it keeps the *rest* of the prompt (system + tools +
history prefix) byte-stable across the whole session, which is where the
actual multi-turn savings live.

## Decision

**Fix A — capture Fields-format reasoning unconditionally.** The OpenAI
client accumulates every `LlmEvent::Reasoning` delta seen while
`thinking.format == Fields` (`think: None`, mirroring `InlineTags`' own
`think: Some(splitter)` gate — the two paths are mutually exclusive per
stream) and, at stream finish, mints one
`ContentPart::Reasoning { provider: "openai", text, data: {"format":"fields","model":…} }`
— same provider tag as `inline_tags`, same shape, same downstream replay path
(`request.rs`'s existing `thinking.replay` gate and `WIRE_NAME`-matched
`reasoning_content` emission needed no changes). Capture is **unconditional**,
exactly like ADR-0191's `InlineTags` capture; replay stays gated per model by
the pre-existing `replay_thinking` flag, defaulting `false`. **For a model
trained on stripped history (qwen3.5/ornith-class), `replay_thinking` must
stay `false`** — capture-only is correct there, not a stepping stone to
turning replay on.

**Fix B — a catalog knob to stop growing the advertised array.**
`ProviderEntry::advertise_discovered: Option<bool>` (`None` behaves as
`true`, today's behavior). When `false` on a `ToolSearch`/`client_side`
session:

- `describe()` still resolves schemas and still marks names into
  `AdvertisingState.discovered` — that bookkeeping also drives the
  arg-validate delivered-schema dedup guard (ADR-0196 §6) and stays useful
  telemetry regardless of this knob.
- The `tool_spec_resolver`'s `client_side` branch (`main.rs`) does **not**
  append those discovered names' specs onto the advertised array — pulled
  into a small, independently-tested `tool_advertising::append_discovered_tail`
  helper that is a hard no-op when the flag is off. The array stays frozen at
  the kernel + profile-defining specs for the whole session.
- ADR-0199 part 2's overlay-enable write becomes a no-op too
  (`overlay.rs::advertise_new_overlay_enables` gains the same check) — enable
  still unmasks the tool for dispatch (its independent effect), only the
  advertisement append is suppressed.
- `Full` mode and the `anthropic_native`/`responses_native` encodings are
  unaffected — this knob only ever touches the `client_side` append path.

The flag is resolved once per session, at the same pin as `Encoding`
(`AdvertisingInputs::pin_session_start`), via a two-tier provider-name /
bare-model-id lookup mirroring `resolve_encoding`/`resolve_encoding_by_id` —
a plain catalog field, not wire-derived, so it carries no config/env
precedence tier of its own.

One misattribution worth recording explicitly, since it was floated and
rejected during design: an `Approve { scope: Once }` (ADR-0052/ADR-0198) never
mutates the advertised array either way — it is a permission-ladder decision,
orthogonal to advertisement. The array-growth writers are exactly the two
named above (`describe()`, overlay-enable), both already gated by this flag.

## Consequences

- A local server's session now keeps a byte-stable prefix through the whole
  session's reasoning-bearing turns (Fix A closes the divergence Fix A
  targets) and through however many tools get discovered (Fix B) — the two
  fixes are independent and additive; either alone is a partial win, both
  together is what a snapshot-cached local server needs. Per the scope note
  above, Fix B is the fix that matters most for a stripped-history model.
- `replay_thinking` remains `false` by catalog default and must stay `false`
  for qwen3.5/ornith-class models — this ADR does not add pressure to flip it.
- `describe()`'s reply text is unchanged in every case (it always carried the
  full schema); only whether that schema *also* lands in the advertised array
  changes.
- One more `Option<bool>` on the catalog surface (mirrors `prompt_cache_key`'s
  precedent exactly: off/unset preserves prior behavior, a user `providers.yml`
  opts a specific endpoint in).
- Anthropic/Gemini converters were already provider-tag-matched and already
  drop a foreign `ContentPart::Reasoning` unmodified (verified by regression
  test, no code change needed there).

## Alternatives considered

- **Flip `replay_thinking` on for Fields-format models as part of this
  change.** Rejected outright per the scope correction above — replay is a
  per-model training-shape question, not a wire-format one, and turning it on
  for a stripped-history model would be actively wrong, not merely unhelpful.
- **Solve the last-turn tail divergence client-side** (e.g. trim the client's
  request to match wherever the server's cache ended). The server owns where
  its own snapshot sits; a client-side guess at that boundary is fragile and
  server-specific. Left as a server-side (out-of-scope) concern.
- **A session-wide "freeze advertisement after N discoveries" heuristic**
  instead of a catalog opt-in. Same #118 objection ADR-0191 raised for a
  tag-sniffing heuristic: an implicit trigger with no declared contract, and
  wrong for a server that *isn't* snapshot-cached (most hosted endpoints,
  where the append-only growth is genuinely fine).
- **Stop marking `describe()`'s `DiscoveredSet` entries when the flag is
  off.** Rejected: the set also drives the arg-validate dedup guard
  (ADR-0196 §6), which has nothing to do with advertisement — conflating the
  two would regress that guard's behavior for no benefit.
