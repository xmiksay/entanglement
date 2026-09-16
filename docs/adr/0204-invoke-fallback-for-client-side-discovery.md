# 0204. Client-side discovery: native-first with an `invoke` fallback, never a mutated tools array

- Status: Accepted
- Date: 2026-09-15
- Issue: #560 (pre-release audit umbrella)
- Relates to: [ADR-0196](0196-tool-search-and-lazy-discovery-replace-the-invoke-envelope.md)
  (its `client_side` encoding's append-on-discovery and its rejection of the
  universal `invoke` envelope are both revisited here, for the client-side
  wires only — the native Anthropic/Responses encodings are unchanged),
  [ADR-0200](0200-fields-reasoning-capture-and-advertise-discovered.md)
  (its `advertise_discovered` bool is replaced by the three-valued
  `discovery` knob below), [ADR-0193](0193-two-tool-call-modes-and-lazy-tool-discovery.md)
  (the envelope shape and edge rules are reused, not its universal scope),
  [ADR-0202](0202-prompt-cache-discipline-anchors-deferral-replay-compaction-date.md)
  (the cache discipline this completes for the non-native wires).

## Context

ADR-0196's `client_side` encoding appends a discovered tool's spec to the
advertised `tools` array. Measured live on z.ai (GLM-5.2, 9.8k-token prompt,
2026-09-15; pinned by `make test-live`):

| change between requests | cached |
| --- | --- |
| identical repeat | 100% |
| conversation turn appended | 100% |
| one tool appended to the end of `tools` | 2–5%, then 100% on repeat |
| one tool added at the start of `tools` | 0% |
| thinking disabled → enabled, same effort | 100% |
| `reasoning_effort` low → high | 0% |

So every discovery on z.ai is a full cache miss. The obvious fix — leave
`tools` alone and deliver the schema only in the transcript (ADR-0200's
`advertise_discovered: false`) — depends on the model calling a tool that is
not in its `tools` array. z.ai's API accepts such a call (HTTP 200, cached),
but the models mostly refuse to make it. Probe: kernel `describe`/`read`,
the `describe` result says "call `get_weather` directly by name":

| model | called the undeclared tool | re-called `describe`/`read` |
| --- | --- | --- |
| glm-4.7-flash, glm-4.5-flash | every run | — |
| glm-5.3, 5.3-flash, 5.2, 5.1, 5, 4.7, 4.6, 4.5, 4.5-air | none | every run |

With an `invoke {name, args}` kernel tool added as an explicit fallback,
the same probe reached the tool in 43 of 44 runs, thinking on and off: the
two flash models natively (8/8), every other model through `invoke` (35/36,
one `describe` loop on glm-5.1), all with correct arguments. A follow-up
turn whose history records the earlier call **as the model emitted it**
(`invoke`) called `invoke` again 6/6; the same history **rewritten to the
native name** made GLM-5.2/5.3 re-`describe` 5/6 — so the model-facing
history must never be rewritten.

The other client-side wires differ: Gemini ends a round with
`UNEXPECTED_TOOL_CALL` when the model calls an undeclared function (API
reference), and OpenAI Chat Completions documents neither behaviour, so both
are treated as must-declare. Neither has a deferral primitive.

ADR-0196 rejected the envelope for four reasons. Re-weighed for this
narrower use: *discovery unsolved* no longer applies (`explore`/`describe`
exist); *JSON-in-JSON* is avoided by declaring `args` as an object (a
stringified object is still accepted); *out of distribution* and *no
per-tool constrained decoding* remain real, which is why the envelope is a
**fallback** and never the only path where a native call works.

## Decision

1. **A `discovery` catalog knob** replaces `advertise_discovered`, on
   `ProviderEntry` with a per-`ModelEntry` override:
   `append` (ADR-0196's behaviour) | `native_first` | `invoke`. It applies to
   the `client_side` encoding only and is pinned per session with the
   encoding (a model switch keeps it). Embedded defaults: `zai`/`zai_paas`
   `native_first`; `gemini` and `openai` `invoke`; `ollama` `append` (open,
   unprobed). Anthropic and Responses keep their native encodings.
2. **`native_first` and `invoke` never mutate `tools`.** The kernel gains
   one `invoke {name: string, args: object}` spec; nothing is appended on
   `describe`, overlay enable, or an `arg_validate` decline. The only reply
   difference: `native_first` tells the model to call the tool by its real
   name and use `invoke` only if it cannot; `invoke` tells it to call through
   `invoke`. The `ToolSearch` system-prompt note says the same, per session.
3. **Core unwraps, the model's history does not change.** When a round's
   advertised specs contain `invoke`, core unwraps an `invoke` call before
   emitting its events: `ToolCall`/`ToolExec`/`ToolOutput` name the inner tool
   with the inner args, and carry `envelope: {tool, input}` — the call exactly
   as the model emitted it. `Context` keeps the emitted `invoke` call byte for
   byte, and replay rebuilds it from `envelope`, so a resumed session sends
   the same bytes. Every head, gate, hook, approval, renderer and the
   narrator see an ordinary tool call; nothing downstream knows about the
   envelope.
4. **Edge rules** (ADR-0193's, kept): missing `args` → `{}`; `args` as a JSON
   string is parsed; a missing/non-string `name`, a non-object `args`, or an
   inner name of `invoke` or `responses_tool_search` is not unwrapped and is
   declined by the runtime with `invoke`'s schema (`is_error`). `explore`,
   `describe` and `poll` may be invoked. A call named `invoke` in a session
   whose specs don't advertise it stays an unknown tool.

5. **Enabling a tool never changes the array.** An overlay enable or a live
   MCP server enable only makes a tool discoverable (explore index, dispatch
   gate). Under `append` the array grows only when `describe()` or an
   `arg_validate` decline actually delivers the schema; under `native_first`
   and `invoke` it never grows. `Full` mode fixes its surface at session
   start: tools enabled later are discoverable until described, then appended
   once, and a tool that disappears (server removed) stays advertised and
   declines at dispatch — the array is never shrunk or reordered.
6. **Heads show the real call.** Every tool call — including one made
   through `invoke` and the `explore`/`describe` discovery calls — renders in
   the user's transcript by its real name with a human-readable summary of
   its arguments and result, never as raw envelope JSON.

7. **Pin at first resolution, not on an event.** Mode, encoding, discovery
   strategy, the `full` surface snapshot and the env date are pinned the first
   time a round resolves a session's tools, not when the tool executor happens
   to receive `SessionStarted`. Core's first round can run before that
   broadcast is handled, and a proof test showed the tools array changing
   between round 1 and round 2 for both a `native_first` and a `full` session
   — a full cache miss on every new session. The resolver seam therefore
   receives the session's bound model: `ToolSpecResolver` becomes
   `Fn(&SessionId, SessionModel<'_>)` with `SessionModel { provider, model }`
   (both `None` = the startup backend), amending ADR-0076's signature. The
   executor only logs on `SessionStarted`/`ModelChanged`. A TUI live re-pin
   (`AdvertisingState::repin`, behind the `/set` dialog's confirmation) is the
   only mid-session change, and it restarts the discovered set when the
   array's shape changes.

## Consequences

- A discovery on z.ai no longer costs a cache miss, and GLM-5.x reaches
  discovered tools reliably; the flash models keep their native call shape.
- Gemini and OpenAI Chat pay the envelope's off-distribution cost on every
  discovered-tool call, in exchange for a stable `tools` array (one Gemini
  `cachedContents` resource for the session instead of one per discovery).
- The protocol gains an optional `envelope` field on three events; old logs
  deserialize unchanged.
- `ToolSpecResolver` embedders must accept the new `SessionModel` argument
  (a compile-time break, documented in `docs/embedding.md`).
- `advertise_discovered` is removed (branch-local, unreleased); a user file
  still carrying it is ignored.
- Ollama stays `append` until it is probed; the opt-in live tests make that
  a one-command check.
- A `reasoning_effort` change mid-session is still a full miss on z.ai and
  Anthropic; the TUI warns once instead of flagging it as a regression.
