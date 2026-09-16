# 0202. Prompt-cache discipline: last-turn anchors, session-long deferral, full thinking replay, cached-prefix compaction, session-pinned date

- Status: Accepted
- Date: 2026-09-15
- Issue: #560 (pre-release audit umbrella)
- Relates to: [ADR-0196](0196-tool-search-and-lazy-discovery-replace-the-invoke-envelope.md)
  (§3's `anthropic_native`/`responses_native` deferral rule, amended here:
  a discovered tool now *stays* deferred for the session), [ADR-0160](0160-extended-thinking-round-trip.md)
  (thinking capture/replay, extended from last-turn-only to every assistant
  turn), [ADR-0101](0101-compaction-forks-into-a-new-session-copy-on-write.md)/[ADR-0103](0103-auto-summarize-on-context-overflow.md)
  (the two compaction paths whose *request shape* changes here; their
  fork/in-place semantics are untouched here, and later changed by
  [ADR-0205](0205-every-compaction-forks-a-successor-session.md), which
  makes every compaction fork a successor), [ADR-0055](0055-usage-cost-and-stop-reason-surfacing.md)
  (`OutEvent::Usage`, which gains a `purpose`).

## Context

An audit of the Anthropic request builder against the provider's caching
rules (prefix match; render order `tools → system → messages`; any byte
change invalidates everything after it; four `cache_control` markers per
request; a ~20-block lookback upstream of each marker) found five places
where the engine paid for its own prefix again:

1. **Discovery un-deferred the tool it had just delivered.** Under the
   `anthropic_native` encoding a `describe()` reply carries `tool_reference`
   blocks, which the API expands *after* the cached prefix and keeps expanded
   for as long as the block stays in history. On the next request the runtime
   flipped that tool's `defer_loading` off, inserting its definition into the
   name-sorted `tools` array — bytes at position zero of the prompt — so
   every discovery busted the tools, system and history caches at once. The
   schema was also delivered twice (rendered definition + expansion + the
   reply's own JSON text). ADR-0196 §3 said "until discovered" by design; the
   design contradicted the caching rule it existed to serve.
2. **The history breakpoint skipped the newest turn** (second- and
   fourth-to-last user messages, "the final turn is most likely to change on
   a steered retry"). The last assistant reply and the last user/tool-result
   batch were therefore sent uncached this round (1.0×) and written next round
   when the anchor advanced (1.25×) — ~2.25× versus 1.25× with the standard
   last-block-of-last-user-turn placement, and a large parallel tool-result
   batch makes that tail big. The retry it protected costs at most one wasted
   cache write.
3. **Thinking replayed only on the last assistant turn.** On the
   preserved-thinking models (Opus 4.5+, Sonnet 4.6+, Fable) the server keeps
   prior-turn thinking, so stripping it client-side edits history at every
   earlier assistant position. This was masked only because the anchor of
   item 2 sat *before* the last assistant message; fixing 2 alone would bust
   the messages cache every round, and on Fable 5.1 edited history is a 400
   for new accounts. Older models ignore prior-turn blocks unbilled, so
   replaying everywhere is safe there.
4. **Compaction summarized from a text render with no cache key.** The
   head of the history was re-rendered into a capped transcript under the
   summarizer's own system string, `tools: []`, `cache_key: None` — a
   full-price re-read of the whole conversation at the moment the context is
   largest, on the same model that had just served it from cache.
5. **The `<env>` date was re-stamped every turn**, rewriting the single
   cached system block once per UTC midnight.

Separately, the TUI's "cached" percentage divided cache reads by the
*uncached* input count (the engine normalizes `input_tokens` as uncached
only), producing figures like "350% cached", showed no context size at all,
and dropped cache-write tokens from every total.

## Decision

1. **A discovered tool stays `defer_loading` for the whole session** on both
   native encodings. `mark_defer_loading` no longer consults the discovered
   set; delivery is the transcript's `tool_reference` (Anthropic) or
   `tool_search_output` (Responses) block, never a mutation of the `tools`
   array. The `anthropic_native` `describe()` reply is the references plus one
   line naming what was loaded — no schema text. ADR-0196 §3 is amended in
   place (branch-local, unreleased).
2. **History anchors move to the last user message (near) and the
   third-to-last (deep).** Marker budget stays system 1 + tools 1 + history
   ≤2 = 4, locked by test.
3. **`replay_thinking` replays every assistant turn's captured block**, still
   gated per model by `ModelEntry::replay_thinking` and per block by the
   minting provider (ADR-0160's "opaque to anyone but its author" rule).
4. **Compaction on the session's own backend reuses the cached prefix**: the
   request is the session's resolved system prompt, the round's advertised
   tool specs, the head messages verbatim, one trailing user instruction,
   and the session `cache_key` — no tool-choice override, so the body is
   byte-identical to a turn's up to the instruction. The instruction itself
   forbids tools ("Do not call any tools; reply with the summary text
   only."); a reply that calls one anyway is discarded and summarization
   re-runs once on the rendered transcript (below), both attempts' usage
   summed into the one compaction `Usage`. A `tool_choice: none` was
   rejected: Anthropic documents that changing `tool_choice` invalidates the
   messages cache, and z.ai's chat-completions `tool_choice` accepts only
   `auto`. The head is a
   strict prefix of the live history, so tools → system → history up to the
   last anchor inside the head are cache hits, and the summarizer sees real
   tool calls, results and thinking blocks instead of a 2 000-char-capped
   rendering — higher fidelity, not just cheaper. A **pinned** `summarize`
   aux model keeps the rendered-transcript shape: a different model is a
   different cache namespace, and a foreign wire may reject the history's
   signed thinking blocks or tool-call id format. The structured shape is
   budgeted against the model's **real** context window (minus the output
   cap and a 2% margin), not the engine's compaction trigger: overflow
   compaction fires precisely because the context exceeds that trigger, and
   mid-turn the kept tail collapses to zero, so the head always exceeds it.
   A head that does not fit the real window falls back to the rendered
   transcript (which keeps its own guard and per-message cap); prune-only
   stays the last resort.
5. **The `<env>` date is pinned per session at its first resolution.** A
   session spanning midnight keeps its start date; a new session gets
   today's. The runtime forgets the pin on session end.
6. **`OutEvent::Usage` carries `purpose: turn | compaction`** (serde default
   `turn`, so pre-existing logs replay unchanged). A head totals every
   round's spend but reads context size only off turn rounds — a compaction
   round prices the summarizer's request, not the live context.
7. **The TUI status bar shows context and cost, not cumulative in/out**:
   `ctx <last turn prompt total>/<window> <pct> | $<session cost> | last: <prompt
   total> in (<cached share>) · out <n>`, where every share is
   `cached / (uncached + cached + cache_write)`. Cumulative totals, the
   per-purpose split, per-model rows and the child-session rollup move to a
   `/cost` command.

## Consequences

- Cache hit rate per round on Anthropic rises from "everything but the last
  turn" to "everything but the new prompt", and a `describe()` no longer
  costs a full re-write of the prefix. Verify with `cache_read_input_tokens`
  across a discovery round; the TUI's corrected share makes a regression
  visible as a red full-miss round.
- Replaying every thinking block sends more input bytes on older Anthropic
  models (unbilled: the server strips them) and is a hard requirement on the
  preserved-thinking ones. A user can still force it off per model.
- `Context` gains a real-window accessor next to its trigger limit; the two
  are distinct numbers and must stay so.
- Compaction on the session backend is priced mostly at the cache-read rate
  and its summary is grounded in the exact history; the pinned-aux path is
  unchanged. Both paths report `purpose: compaction`.
- A steered/edited retry of the last turn now wastes one cache write instead
  of avoiding it — accepted; it is the rare path.
- `LlmRequest` has no tool-choice field at all: every request, the
  summarizer's included, runs under the provider default `auto`. A model that
  ignores the no-tools instruction costs one discarded (still priced,
  debug-logged) attempt before the rendered fallback; a tool call on the
  rendered attempt, which advertises no tools, fails compaction like an LLM
  error.
- A session that runs across midnight tells the model yesterday's date until
  it ends. Accepted: a stable prefix is worth more than a calendar tick; a
  turn that needs the wall clock has `bash`.
