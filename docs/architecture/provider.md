# entanglement Architecture — LLM I/O (provider crate)

> Part of the [architecture overview](../architecture.md). The *why* behind each choice is in the [decision log](../adr/README.md).

## 5b. LLM I/O (`entanglement-provider`) — [ADR-0007](../adr/0007-streaming-llm-and-provider-crate.md), [ADR-0053](../adr/0053-invert-core-provider-seam.md)

The `Llm` **trait** — together with its DTOs (`LlmRequest`/`LlmResponse`/
`LlmEvent`/`LlmStream`, `LlmFactory`, `ToolCall`, `ToolSpec`,
`stream_from_response`), the stub backends (`DummyLlm`/`EchoLlm`, in
`src/llm.rs`), and the wire message types (`Message`/`MessageRole` plus the
multimodal `ContentPart`/`ImageSource`, in `src/message.rs` — a `Message`'s body
is `content: Vec<ContentPart>`, #197/[ADR-0064](../adr/0064-message-content-blocks.md))
— lives **in `entanglement-provider`**. Since
[ADR-0053](../adr/0053-invert-core-provider-seam.md) inverted the seam, the
provider is a **leaf crate** (no `entanglement-*` deps) that owns this LLM ABI;
`entanglement-core` *depends on* provider, consumes the `Llm` trait from its turn
loop, and re-exports these types for its heads. The provider *may* depend on
transport crates (`reqwest`) and is usable **standalone** for raw LLM queries
with no engine.

```rust
enum StopReason { EndTurn, ToolUse, MaxTokens, StopSequence, Other }
struct Usage { input_tokens?, output_tokens?, cached_input_tokens?, cache_write_tokens? }
enum LlmEvent {
    Text(String),
    Reasoning(String),   // thinking/reasoning tokens, streamed distinctly
    ToolCallDelta { id, name, delta },   // streamed tool-arg fragment, before ToolCall (#194)
    ToolCall(ToolCall),
    Finish { stop_reason: StopReason?, usage: Usage },   // normalized (#192)
}
trait Llm: Send { async fn stream(req) -> Result<BoxStream<'static, Result<LlmEvent>>> }
```

- Streaming mirrors opencode (Vercel AI SDK `doStream`): live token-by-token
  deltas, not a buffered whole-reply. The box stream is `'static`.
- **`LlmEvent::Reasoning`** surfaces extended-thinking output (Anthropic
  `thinking`/`redacted_thinking` blocks, OpenAI `reasoning_content`) instead of
  dropping it; core re-emits it as a reasoning `OutEvent` heads render distinctly
  from answer text. This is the *display* rail — it is deliberately never folded
  into `Context`. The *replay* rail is a separate
  `LlmEvent::ContentBlock(ContentPart::Reasoning)`
  ([ADR-0160](../adr/0160-extended-thinking-round-trip.md)); both are emitted for
  the same thinking. See **Extended thinking: capture and replay** below.
- **`LlmEvent::ToolCallDelta`** (#194) streams a tool call's JSON arguments as
  they arrive — OpenAI `tool_calls[].function.arguments` fragments, Anthropic
  `input_json_delta.partial_json` — *before* the assembled `ToolCall` that both
  clients still emit on flush / `content_block_stop`. Correlated to that final
  call by `id`; core re-emits it as `OutEvent::ToolCallDelta` so a head can render
  file-sized `edit`/`write` arguments live. Additive: a consumer that ignores it
  still gets the full `ToolCall` (replay reconstructs state from that, not the
  fragments).
- **`LlmEvent::Finish`** is normalized (#192,
  [ADR-0055](../adr/0055-usage-cost-and-stop-reason-surfacing.md)): `StopReason`
  collapses `finish_reason`/`stop_reason` across both wires, and `Usage` splits the
  token counts so each maps to one pricing dimension — `input_tokens` is the
  *uncached* input (the OpenAI client subtracts `prompt_tokens_details.cached_tokens`
  out of `prompt_tokens`; Anthropic already reports `cache_read_input_tokens` /
  `cache_creation_input_tokens` separately). `ModelPricing::cost_usd(&Usage)`
  prices a round-trip; the engine emits it as `OutEvent::Usage` and warns on
  `MaxTokens`.

**Provider topology** — split by *wire format*, not by vendor:

| client (`entanglement-provider`) | wire format | serves | auth |
| --- | --- | --- | --- |
| `OpenAiLlm` (`openai/`) | `/chat/completions` SSE | **z.ai** (GLM, entanglement's primary), **OpenAI**, **Ollama** `/v1` | `Bearer` or none (Ollama) |
| `AnthropicLlm` (`anthropic/`) | `/v1/messages` SSE | Anthropic | `x-api-key` |
| `GeminiLlm` (`gemini.rs`) | `:streamGenerateContent?alt=sse` | Google Gemini | `x-goog-api-key` |

- `OpenAiLlm` is one generic client `{ base_url, api_key: Option, default_model }`
  hand-rolled over `reqwest` (no SDK crate). Preset base constants
  (`ZAI_CODING_PLAN_BASE`, `ZAI_GENERAL_BASE`, `OPENAI_BASE`, `OLLAMA_BASE`) still
  exist, but the *default* base per provider now comes from the catalog (below);
  `openai_factory(base, key, model, rpm, concurrency, model_concurrency,
  web_search)` builds an `LlmFactory`. Split into `openai/{mod,request,sse}.rs`
  (#481) to stay under the 400-line file cap — `mod.rs` owns the client +
  streaming loop, `request.rs` request-body construction, `sse.rs` chunk
  parsing.
- `AnthropicLlm` is separate because Anthropic's format genuinely differs (system
  top-level, tool results merged into one user turn, `input_json_delta`
  fragments). `anthropic_factory(base_url, key, model, rpm, concurrency,
  model_concurrency, web_search, web_search_tool_version)` — `base_url`
  defaults to `ANTHROPIC_BASE` (mirroring `OPENAI_BASE`/`GEMINI_BASE`); a
  catalog `wire: anthropic` entry's `base_url` (a proxy/gateway speaking the
  Anthropic wire) overrides it end to end — the request URL *and* the pool
  key — since #551 (previously hard-coded and silently ignored). Split into
  `anthropic/{mod,request,sse}.rs` (#481)
  the same way as `openai/` — `mod.rs` additionally owns the `pause_turn`
  continuation loop (below).
- **Explicit `cache_control` breakpoints** (#566) — Anthropic caching is opt-in
  per content block, unlike z.ai/OpenAI-compat's implicit whole-prefix caching;
  without a marker every round re-bills the full system + tool schemas + growing
  history at the uncached rate. `anthropic::request::build_body` places the
  standard three: the last `system` block (also covers the `tools` array before
  it in the fixed tools → system → messages render order), the last `tools`
  entry, and the last content block of the second-to-last `user`-role message
  (`place_history_breakpoint`) — the final turn is left unmarked since it's the
  one most likely to still change on a steered/edited retry.
- `GeminiLlm` is native, **not** Gemini's OpenAI-compat surface (#309,
  [ADR-0085](../adr/0085-gemini-native-wire-and-opaque-provider-meta.md)): the
  compat endpoint drops `thoughtSignature`, the opaque per-call token a 2.5
  thinking model must echo back verbatim or the API 4xxs on replayed history. It
  streams `candidates[].content.parts[]` (text / `thought:true` reasoning /
  `functionCall`), maps history to `contents` (assistant → `role: model`, tool
  result → a `user` `functionResponse` keyed by call **name** — Gemini itself
  matches a response to its call by name, it has no call-id concept). Gemini
  emitting two parallel calls to the *same* tool would otherwise give both
  `ToolCall`s the identical id, colliding on the wire's `request_id` dedupe and
  wedging the turn (#444); `function_call_to_tool_call` instead synthesizes
  `ToolCall.id` as `name#ordinal` (a per-stream counter threaded through
  `handle_chunk`), while `ToolCall.name` stays bare — `gemini::tool_name_from_id`
  strips the `#ordinal` suffix back off when building the `functionResponse` so
  the reply still keys by the bare name Gemini expects. Also sanitizes the tool
  `parameters` schema (Gemini rejects `$schema`/`additionalProperties`/
  union-`type`/dangling `required`), and stashes/restores the signature via
  `ToolCall.provider_meta` (below). `gemini_factory(base, key, model, rpm,
  concurrency, model_concurrency, http)` — no web-search knob.
  Request-body assembly lives in the `gemini::request` submodule.
- **Explicit `cachedContents` caching** (#587) — mirrors Anthropic's
  `cache_control` breakpoints (above) for the backend that otherwise has no
  equivalent: `gemini::cache::CacheHandle` (one per session, held on the
  `GeminiLlm` clone that session owns) hashes the resolved `model` + `system`
  + `tools` before every `stream` call, creating a `cachedContents` resource
  via a `POST .../cachedContents` the first time a given combination is seen
  (or after it changes) and reusing the returned resource name — sent as
  `cachedContent` on the `streamGenerateContent` body in place of the inline
  `systemInstruction`/`tools`, which Gemini rejects alongside a cache
  reference — on every subsequent turn. Unlike Anthropic's breakpoints, the
  growing message history is never folded into the cache (it changes every
  turn, so caching it would just thrash); only the stable system+tools prefix
  is. Best-effort throughout: a prefix under `MIN_CACHEABLE_CHARS` (a
  char-count proxy for Gemini's undocumented per-model minimum token count)
  or any create-call failure resolves to `None`, falling back to inlining
  `system`/`tools` exactly as before — cache creation never fails the turn.
- **Opaque `provider_meta`** (#309) — `ToolCall.provider_meta: Option<Value>` is a
  provider-private slot that must round-trip **verbatim** through history persistence
  + replay; core never inspects it. Gemini stashes `thoughtSignature` there; the
  slot is generic (any future wire's reasoning/thinking state fits). Persisted with
  the ADR-0064 back-compat shim (`#[serde(default, skip_serializing_if)]`), so
  pre-#309 logs with no `provider_meta` still deserialize (→ `None`) and replay
  unchanged. Carrying `serde_json::Value` (not `Eq`) means `ToolCall`/`LlmEvent`/
  `LlmResponse` are `PartialEq` but no longer `Eq`.
- `ToolSpec.schema` surfaces as `input_schema` (Anthropic) / `parameters`
  (OpenAI-compat, Gemini); `Message.tool_call_id` → `tool_use_id` / `tool_call_id`
  / Gemini `functionResponse.name` (the bare name recovered from the synthesized
  `name#ordinal` id, above).
- A `Message`'s `content: Vec<ContentPart>` renders per wire (#197,
  [ADR-0064](../adr/0064-message-content-blocks.md)): text-only user content stays
  a plain string (OpenAI) / string content (Anthropic); an image part switches to
  the block array — OpenAI `image_url` with a `data:` URL, Anthropic an `image`
  block with a base64 `source` (incl. image `tool_result`s, the #221 path); Gemini
  has no image slot on `functionResponse.response` at all, so a tool result's
  image blocks ride as trailing `inlineData` parts alongside the
  `functionResponse` part in the same turn (#447).

**Provider-side web search** (#305,
[ADR-0075](../adr/0075-provider-side-web-search-mvp.md); post-MVP follow-ups
#481, [ADR-0131](../adr/0131-web-search-post-mvp-follow-ups.md)) — opt-in,
**client-construction-time** config, **no core/protocol change**.
`WebSearchConfig { enabled, max_uses, allowed_domains }` (`web_search.rs`,
`deny_unknown_fields`) is bound onto a client by its factory as an `Option` (the
runtime hands it `Some` only when a `web_search:` `config.yml` section is
enabled; the live `/model` resolver captures it too, so a switch re-binds
identically). When present, `build_body` pushes the provider's **server-executed**
search tool onto the same `tools` array (so it rides even with no function
tools): z.ai a `{"type":"web_search","web_search":{…}}` entry, Anthropic a
`{"type":"<version>","name":"web_search"}` server tool (+ optional
`max_uses`/`allowed_domains`) — `<version>` is `ModelEntry.web_search_tool_version`
(#481, catalog data) when the active model sets one, else the client's
`web_search_20250305` fallback, so a model requiring the newer `_20260209` tool
works via catalog config with no code change. The provider runs the search
*mid-turn*, no client round-trip; results still stream live on the **reasoning
channel** (`LlmEvent::Reasoning`, unchanged since #305) but are now **also**
persisted (#481): the Anthropic parser tracks a `server_tool_use` block with
`is_server` and, on stop, emits both a `Reasoning` line and an
`LlmEvent::ContentBlock(ContentPart::ProviderSearch { provider, summary, data })`
— `data` is the block's raw Anthropic JSON, opaque outside this provider; each
`web_search_tool_result` entry (or its error) renders the same way. The engine's
turn loop (`session/round.rs`) appends every `ContentBlock` after the round's text
when it commits the assistant `Message`, and emits a persisted, seq-bearing
`OutEvent::SearchResult { part }` per block (mirrors `AmbiguousRetry`) so
`Session::replay` reconstructs the exact content — `anthropic::request::
anthropic_blocks` replays a `ProviderSearch` block's `data` **verbatim only when
`provider == "anthropic"`** (mirrors `ToolCall.provider_meta`'s opaque round-trip
contract; this is the search-result half of Anthropic's prompt-cache benefit);
every other converter (Anthropic on a foreign-provider block, OpenAI-compat,
Gemini) reads only `summary`, rendered as plain text — z.ai's cited answer
already flows as `Text`, and its own `web_search` source array now also emits a
`ContentBlock(provider: "zai")` alongside the existing `Reasoning` lines, so
citations from either provider survive into a later turn's history instead of
vanishing with the round. The z.ai array's streaming placement is **confirmed**
(#625, [ADR-0171](../adr/0171-zai-streaming-web-search-placement-confirmed.md),
verified live against a working Coding Plan key): it is a top-level sibling of
`choices`, delivered once on the same final chunk that carries
`finish_reason`/`usage`, never nested under `choices[0].delta` — `handle_chunk`
scans only that top-level site. Verification also surfaced that invocation is
model-decided, not guaranteed by the tool being offered: a turn can stream to
completion with no `web_search` key at all if the model chooses not to call the
search tool, which is the concrete instance of the cited-text-only floor ADR-0075
already accepted as the worst case.
A long-running search can end an Anthropic response with `stop_reason:
"pause_turn"` instead of a confident stop; `anthropic::mod` owns continuing it
entirely client-side (#481) — `sse::handle_frame` accumulates every finalized
content block into a raw array as the stream runs, and on `pause_turn`,
`stream()` re-POSTs with that array appended as a fresh assistant turn,
continuing the *same* `LlmStream` (no `Finish` in between) until a confident stop
or a bounded continuation cap (6) is hit; core never observes `pause_turn`. If
the cap is hit, the client's own `Finish` still reports it (mapped to
`StopReason::Other`), and the turn loop's ADR-0118 ambiguous-stop retry is the
fallback safety net — the pre-#481 behavior this replaces as the primary path.
Enabling web search *is* consent — the search runs provider-side, **outside**
the runtime permission ladder ([ADR-0047](../adr/0047-local-trust-boundary.md)).

**Resilience the provider layer owns — per endpoint** (#217,
[ADR-0050](../adr/0050-per-endpoint-connection-pool-retry-rate-limit.md)): one
tuned `reqwest::Client` is shared (it already pools TCP connections per host),
but the **rate-limit budget and retry/backoff state are keyed by `(endpoint,
api-key)`** — the provider's base URL plus a *hash* of the API key (if any) — in
`HttpClient`'s `EndpointPool`. Each such bucket owns an **adaptive pacing gate**
and its own `Retry-After` cool-down window, so a throttled endpoint never starves
another — and **multiple keys on the same endpoint each get their own budget**
(different keys have different limits). The key is hashed, never stored raw in
the map. `client::pool_key`'s endpoint half is normalized first — trailing `/`
trimmed, host lowercased (path left case-sensitive) — so a trailing-slash or
host-casing difference between an env override and the catalog default can't
split one real endpoint's budget in two; the API-key half is hashed with
`sha2::Sha256` (not `DefaultHasher`, whose output is unspecified across Rust
toolchains) since #551, because it also becomes the cross-process shared-state
**file name** below and so must be stable across separately-built `skutter`
processes, not merely process-local. Before #217 a single global 50-RPM
`Semaphore` was shared across *all* providers. The bucket's RPM is **catalog data** (#241): the provider entry's
optional `rpm` (env `{NAME}_RPM` > user `providers.yml` > embedded default),
threaded through `openai_factory`/`anthropic_factory` → `execute_with_retry` →
`EndpointState::new`; when unset it falls back to the client default
(`RetryConfig::rpm`, 50).

`HttpClient` is not LLM-only: since #559
([ADR-0157](../adr/0157-mcp-http-transport-shares-the-endpoint-pool.md)) the
MCP streamable-HTTP transport (`entanglement-provider::mcp::McpHttpClient`,
§gates-and-host-tools.md §10) rides the same pool, keyed by its own URL plus
its bundling provider's API key when known — so a provider-bundled MCP server
sharing a key with its provider's LLM endpoint (e.g. z.ai's `web_search_prime`/
`web_reader`/`zread` against `ZAI_API_KEY`) counts against that key's real
budget instead of bypassing it.

The concurrency cap + pacing gate + 429 policy
([ADR-0111](../adr/0111-adaptive-endpoint-pacing-and-429-retry-until-clear.md)) is
what makes the pool coordinate *across sessions* — the property that "spawn many
sub-agents" needs and ADR-0050 lacked. The **primary** guard is a per-endpoint
`concurrency` semaphore. **Also catalog data, mirroring `rpm`** (#414): the
provider entry's optional `concurrency` (env `{NAME}_CONCURRENCY` > user
`providers.yml` > embedded default), threaded through the same
`openai_factory`/`anthropic_factory`/`gemini_factory` → `execute_with_retry` →
`HttpClient::endpoint` → `EndpointState::new` path as `rpm`; when unset it falls
back to the client's default (`RetryConfig::concurrency`, 3, itself overridable
process-wide via `ENTANGLEMENT_MAX_CONCURRENCY` — the pre-#414 pool-global
knob, now the last-resort fallback rather than the only lever). A permit is
acquired before the request and returned as an opaque `StreamGuard` that
`spawn_byte_stream` holds until the **streamed body** ends — so the cap counts
*open streams* (a slow thinking generation included), the unit a provider really
limits. Many spawned sub-agents queue and run a few at a time instead of all
opening streams at once and 429-storming. On top of that, `RateLimiter` is a
**spacing gate** (not a bucket that starts full): `acquire` reserves the next slot
`interval` after the last, **adaptive (AIMD)** — each 429 doubles it (`penalize`,
capped at `SLOWDOWN_CAP × base`), each success steps it back toward `base = 60s/rpm`
(`relax`). Every 429 **also** parks the shared `Retry-After` window (even with no
header) so all concurrent callers back off together, and is retried on a patient
schedule (`rate_limit_initial_backoff` ≈5s → `rate_limit_max_backoff` ≈10 min; a
server `Retry-After` wins, clamped to a 24h ceiling so a hostile/misbehaving
endpoint's huge delta-seconds or far-future HTTP-date can't overflow the
`Instant + Duration` arithmetic, #548) **until it clears or `rate_limit_max_elapsed` (≈15 min)
is spent** — then it surfaces as an error, so a saturated endpoint *fails* a
sub-agent's turn rather than hanging its parent forever. Genuine failures
(transport faults, retryable 5xx) stay bounded by `max_attempts`.

**The endpoint-wide park is clamped separately from the offending caller's own
budget** (#547): what `EndpointState::set_retry_after`/`SharedGate::mark_retry_after`
*store* — the deadline every *other* caller of the endpoint waits on via
`wait_for_retry_after`, in-process and (once persisted) across a restart — is
clamped to `rate_limit_max_backoff`, even when the server's raw `Retry-After`
is far larger (a `Retry-After: 3600` must not park every sibling caller for an
hour). The offending caller's own give-up decision still honors the raw,
`MAX_RETRY_AFTER`-clamped value. On top of that, every wait this call makes —
`wait_for_retry_after` and `SharedGate::acquire`'s poll loop — is bounded by
*this call's own* `rl_deadline` (`rate_limit_max_elapsed`): waiting further
returns `RetryError::RateLimited` instead of sleeping past it, so a cool-down
set by a different caller (or read back from the persisted shared file after a
restart) can never park a caller longer than its own budget allows.

**Per-model concurrency, layered on top of the endpoint cap** (#521,
[ADR-0140](../adr/0140-per-model-concurrency-cap-layered-on-endpoint-cap.md)):
some providers — z.ai in particular — cap concurrency **per model**, not just
per endpoint (documented: `GLM-4.7-Flash` allows only 1 in-flight request,
`GLM-5.2` allows 5, on the same base URL/key), and per-profile model pinning
([ADR-0081](../adr/0081-per-profile-model-pinning-and-rebind-on-set-agent.md))
makes a mixed-model workload on one endpoint the normal case. `ModelEntry`
gains an optional `concurrency` (catalog data, mirroring `ProviderEntry`'s;
YAML-only, no env override in v1 — model ids contain `.`/`-` with no
established env-name mangling). `EndpointState` gains a second,
lazily-created `Semaphore` per `(endpoint, model)`
(`model_concurrency: Mutex<HashMap<String, Arc<ModelSlot>>>`);
`HttpClient::execute_with_retry` takes `model`/`model_concurrency` params and,
when the latter is `Some`, acquires **that model's permit first, then the
endpoint-wide one** (released in reverse) — the endpoint cap is unchanged as
the ceiling on the *sum* of in-flight requests across every model on it
(every call still takes an endpoint permit regardless of model), but a
caller blocked on its own saturated model never holds that scarce endpoint
permit hostage while it waits, which would otherwise starve unrelated
sibling models sharing the endpoint even with room to spare. Both permits
ride the same `StreamGuard` and release together when the streamed body
ends. A model with no catalog cap never acquires a model permit at all —
byte-identical to pre-#521; a model cap configured *wider* than its
endpoint's own is legal (the narrower endpoint cap simply binds first) but
logs a `tracing::warn!` as a likely misconfiguration rather than erroring.
The `{name}_factory`/`{Name}Llm::new` constructors each gained a
`model_concurrency: ModelConcurrencyResolver` parameter — **resolved per
request** against the request's own model (#550), not baked in once at
construction. The original v1 shape resolved the cap once per `(entry,
model)` at factory-build time, the same point `resolve_rpm`/`resolve_concurrency`
resolve the provider-level knobs; that model is the *client's* — the startup
default or whatever `SetModel` last rebound to — and diverges from a given
request's actual model whenever a profile pins `model:` **without**
`provider:` (the documented request-level fallback: `AgentProfile::model_pin`
returns `None`, so `SetAgent` doesn't rebind the client). The mismatch paired
the wrong model's cap with the request's real model at `execute_with_retry`'s
`model`/`model_concurrency` call site, and — because `EndpointState::model_slot`
only sized a model's semaphore on the *first* caller — that wrong cap then
stuck for the rest of the process, immune to a later, correct `/model` switch.
`Catalog::model_concurrency_resolver(provider)` builds the resolver (an
`Arc<dyn Fn(&str) -> Option<usize>>`, cheap to clone into every session's
backend clone), closing over a cloned `Catalog` so each client can look its
*actual* per-request model up against the real catalog data. `model_slot`
itself now also **corrects** an already-cached slot when a later caller
supplies a different cap, instead of latching the first value seen forever —
belt-and-suspenders against any other path (a config reload, a race) that
still manages to resolve the wrong cap first. The endpoint-wide `Retry-After`
cool-down and pacing gate stay shared across every model on the endpoint
(v1) — a 429 still parks the whole endpoint regardless of which model
triggered it.

`Catalog::effective_concurrency(provider, model)` is the one-shot sibling of
`model_concurrency_resolver` (#589): a plain `Option<usize>` snapshot (model's
own cap, else its provider's) for a caller that isn't building a live client
and just wants to judge a `(provider, model)` pair's cap once — the runtime's
`AuxLlmRegistry` uses it to decide whether firing an aux call alongside a
primary-model call risks contending for the same permit (see the *Per-purpose
auxiliary models* section of the [heads & persistence
doc](heads-and-persistence.md), [ADR-0158](../adr/0158-defer-session-title-aux-call-under-contended-primary-concurrency.md)).

**Timeouts — connect + await-headers + idle-gap, not whole-request** (#241,
#658): the shared `reqwest::Client` is built with `connect_timeout` only (30s
to establish TCP+TLS). A fixed whole-request `.timeout()` would abort a long
*healthy* LLM stream mid-turn (and its partials, already consumed, aren't
retryable) — and its 300s ceiling was also what capped `Stop` cancel latency
(#179). Between TCP+TLS establishment and the streamed body sits a third gap
`connect_timeout` and the idle-gap watchdog don't cover: a server that accepts
the connection and takes the request but never writes response headers. Before
#658 that hung `execute_with_retry` forever, pinning the endpoint's
concurrency permit and cross-process lease. `execute_with_retry` now wraps the
await-headers `request_fn()` call in `tokio::time::timeout(response_header_timeout,
…)` (`RetryConfig::response_header_timeout`, default 120s, aligned with
`STREAM_IDLE_TIMEOUT`); an elapsed timeout drops the silent connection and is
classified exactly like a transient transport fault, so it retries through the
same path up to `max_attempts` before surfacing `RetryError::HeaderTimeout`.
Once headers do arrive, liveness on the streamed body is enforced per chunk:
`client::spawn_byte_stream` forwards the SSE bytes over an mpsc channel under a
`tokio::time::timeout(STREAM_IDLE_TIMEOUT, …)` watchdog (120s idle gap), so a
slow-but-alive stream runs to completion while a hung one dies fast. Both
`OpenAiLlm`, `AnthropicLlm`, and `GeminiLlm` use this one helper. **The pump
also races every read against the consumer dropping its
receiver** (`tokio::select!` against `tx.closed()`, #552): before this, the
`StreamGuard` it holds (the endpoint/model permits and the cross-process lease)
released only on the pump's *next* chunk-send failure or the idle-gap timeout
— neither of which fires while the body is silently paused, so a `Stop` mid
reasoning pause, or the OpenAI-compat parser's `[DONE]`-triggered `break 'outer`
while a keep-alive proxy holds the connection open, both left the consumer's
receiver dropped with the pump none the wiser, holding the guard for up to the
full 120s. Racing the read against `tx.closed()` frees it the moment the
consumer is gone. **Retry** classifies the *response* status inside the loop — a 429/5xx response
(not just a `reqwest::Error`) is retried, not silently dropped; before #217 those
responses came back as `reqwest::Ok` and were never retried (#193). A 5xx retries
with exponential backoff + jitter up to `max_attempts`; a 429 retries until clear
(ADR-0111, above). Transport `reqwest::Error`s are classified by
`is_transient_error`: connect/timeout faults, retryable statuses, **and
request-send faults** (`is_request()` — a dropped keep-alive connection reset
*between* requests, which reqwest renders as `"error sending request for url …"`
and is *not* `is_connect()`; safe to resend since no response body was consumed)
all retry; anything else is permanent.

**Throttle visibility.** `HttpClient::throttle_status() -> Option<ThrottleStatus>`
is a read-only snapshot over the live pool (in-flight/cap, `Retry-After` remaining,
whether the pacing gate is penalized) for the most-throttled endpoint, or `None`
when every endpoint is at rest. Since #521 the reported in-flight/cap is
whichever is **binding** — the endpoint's own, or a per-model slot whose
occupancy ratio is currently tighter (`model: Option<String>` names it when
so); an idle or never-created model slot never shadows a genuine
endpoint-wide cool-down. It feeds no request logic — the TUI polls it each
frame to show a throttle indicator only while backing off (see heads doc),
rendering the model id alongside the host when it is the binding constraint.
`ThrottleStatus` also carries `next_request_in: Option<Duration>` — the AIMD
pacing gate's own `next_slot` countdown, surfaced only while `penalized`
(#517, [ADR-0141](../adr/0141-wire-visible-throttle-transitions.md)) — so the
TUI's "pacing" label can show a live wait, not just the bare word.
**Cross-process facts are folded in too (#552):** before this, `ThrottleStatus`
read only in-process state, so a peer process's parked 429, a lease it held
via the shared gate, or a caller of *this* process blocked inside
`SharedGate::acquire` all read as "at rest" — the incidents that most needed
surfacing showed as nothing wrong. `backoff_remaining` now takes the max of
this process's own cool-down and whatever `SharedGate::peek` reads back from
the shared state file (a lock-free `fs::read` — the file is always replaced
via an atomic rename, so a concurrent writer is never observed mid-write,
only up to one write cycle stale, which is fine for a status label though not
for `acquire`'s admission decision). `shared_leases: Option<usize>` reports
the live cross-process lease count (`None` when sharing is disabled or the
file is unreadable) and factors into both `is_throttled()` and the runtime
throttle responder's `classify()`, so a sibling saturating the shared cap
reads as busy even while this process's own semaphore has room. `waiters:
usize` (deferred-work-ledger row 5) is a display-only counter — bumped just
before, and dropped via a `WaiterGuard` right after, this endpoint's own
`concurrency.acquire_owned().await` — reporting how many callers are queued
behind the permit, not yet admitted. The TUI's `throttle_label` gains a
`(shared X/cap)` suffix when the shared lease count disagrees with the local
`in_flight`, and a `· Nq` suffix while callers are queued.
`HttpClient::throttle_statuses() -> Vec<ThrottleStatus>` is the sibling that
snapshots **every** resolved endpoint (not just the most-throttled): the
runtime's `throttle::spawn_throttle_responder` polls it every 500ms and emits
a wire-visible `OutEvent::Throttle` on each endpoint's own enter/exit
transition, so a stdio/WS head sees the same stall the TUI renders directly
(ADR-0141 — engine-global, not per-session, matching this pool's own
per-endpoint model). `RetryConfig` (`max_attempts`, `initial_backoff`,
`max_backoff`, `rpm`) tunes the *failure* path; `HttpClient::with_config` +
`RetryConfig::no_retry()` build variants (tests use the latter).
`execute_with_retry` also takes a **per-call** `retry: Option<RetryConfig>`
override (#660) — `None` uses the pool's own config unchanged (every LLM
client passes `None`); a caller with a legitimately different patience
budget (the MCP startup handshake, [ADR-0169](../adr/0169-startup-mcp-connect-is-concurrent-and-fast-fail.md))
can swap just the failure-path knobs while still riding the same endpoint's
RPM/concurrency/429 admission as everyone else. This
per-endpoint state is the reason a session carries **no** per-session connection
handle: the `LlmSession` newtype was collapsed to a plain `Box<dyn Llm>` (#195,
[ADR-0062](../adr/0062-collapse-llmsession-placeholder-newtype.md)) — resilience
belongs to the endpoint, shared across sessions, not to the conversation. A
**live model/provider switch** (#218,
[ADR-0063](../adr/0063-realtime-model-provider-switch.md)) rebuilds that
`Box<dyn Llm>` from a `ResolvedModel` the runtime resolves against this catalog +
the warm per-endpoint client, so switching mid-session neither restarts the engine
nor cold-starts the pool.

**Shared across processes, not just across sessions** (#523,
[ADR-0144](../adr/0144-file-backed-shared-endpoint-state-across-instances.md)):
everything above coordinates every session *within one `skutter` process* —
but two processes talking to the same `(endpoint, api-key)` used to each run a
fully independent `EndpointPool`, so N processes collectively sent up to N×
the configured RPM and held N× the concurrency cap against a provider with no
idea more than one client existed. `EndpointState` gains a
`client::shared_state::SharedGate` — a file-backed cross-process ledger at
`${data_dir}/entanglement/endpoints/<sha256(pool_key)>.state`, guarded by an
advisory `fd-lock` read-modify-write exactly like the managed config files
(#329, ADR-0084; independently re-implemented in `entanglement-provider`
rather than depended on, since the provider crate is the leaf and takes no
`entanglement-*` dependency, ADR-0053). The on-disk shape and locked
read-modify-write mechanics live in a sibling `client::shared_store` module
(split out from `shared_state` purely to keep both under the 400-line file
cap, #552); `shared_state` owns `SharedGate`/`SharedLease` and the admission
policy. Shared: the RPM token-bucket ledger, a
lease-based in-flight concurrency count (each admitted request holds a lease —
id, owning pid, expiry — renewed on a heartbeat and pruned by TTL if its
process dies without releasing it, so a crash recovers the slot rather than
leaking it permanently), and the `Retry-After` cool-down deadline (one
process's 429 parks every sibling's next `acquire`). **Not** shared in v1: the
AIMD pacing gate stays per-process — once the budget itself is bounded
correctly in aggregate, per-process pacing converges on the same signal (see
the ADR for the full reasoning). `execute_with_retry`'s loop calls
`endpoint.shared.acquire(rpm, concurrency, rl_deadline)` **last** — after
`wait_for_retry_after`/`limiter.acquire` *and* after both in-process permits
(model, then endpoint) — not before them (#546, fixing a starvation bug:
acquiring the shared lease first let a caller queued on its own model's
semaphore sit holding this scarcer, process-wide resource for as long as it
waited, starving sibling `skutter` processes even though the provider had
room; it also meant the shared RPM ledger, stamped on admission, was stamped
at the start of that wait rather than at the actual send). `acquire` is
itself bounded by `rl_deadline` (#547) — waiting past it returns `Err(())`
rather than polling forever on a persisted cool-down or a saturated cap. The
returned lease is held in `StreamGuard` alongside the endpoint/model permits
and all three release together; a 429 also `.await`s
`endpoint.shared.mark_retry_after(delay)` so the cool-down reaches siblings,
not just this process — awaited rather than fired off as a detached
`tokio::spawn` (#547), since a detached task can be dropped mid-flight by a
short-lived one-shot `run` exiting right after. Falls back silently to pure
in-process gating (today's pre-#523 behavior) when the state directory is
unwritable or disabled via `ENTANGLEMENT_NO_SHARED_ENDPOINT_STATE=1` — an
operator who wants genuinely separate per-instance budgets already gets that
for free by giving each instance its own key/base URL, since the pool key
itself isolates them.

**Lease release is synchronous, and the TTL backstop is tighter** (#547):
`SharedLease::drop` used to only cancel its background renewal task, which
then asynchronously removed the lease from the shared file — a detached
`tokio::spawn` a short-lived one-shot `run` (or a SIGINT/SIGTERM shutdown)
could tear the tokio runtime down before it ever got scheduled, leaving a
live-looking lease for the next launch to block on. `Drop` now removes the
lease **synchronously**, in-line, covering every clean exit path (normal
completion, a `.abort()`-driven task teardown, SIGINT/SIGTERM). `LEASE_TTL`
is kept close to `LEASE_RENEW_INTERVAL` (~2×: 120s vs. 60s, was 180s) rather
than generously above it — the TTL is now purely the backstop for the one
case synchronous release can't cover: a `SIGKILL`, where nothing runs at all.

**Orphaned shared-state files are swept, not left to accumulate** (#551):
nothing evicts a `.state`/`.lock` pair when its pool key stops being used — a
`/key` rotation, a catalog `base_url` edit, a decommissioned provider — so
without an explicit sweep they pile up under the state directory forever.
`client::prune_stale(max_idle)` walks the directory and removes any pair that
is both **idle** (`.state` mtime older than `max_idle`) and **empty** (no live
lease, no pending cool-down, no request in the trailing RPM window) — an
endpoint still in real use always fails the "empty" check regardless of file
age, so a live budget can never be swept out from under it. The runtime calls
this twice: a best-effort startup sweep (`max_idle = 1h`, `main.rs`, mirroring
`session_store::prune`'s role for session logs) and a short-`max_idle` (5s)
sweep fired from the TUI's `/key` submit handler
(`entanglement-runtime::tui::app::key`, `tokio::task::spawn_blocking` — the
sweep locks and `fsync`s files, kept off the keypress path) so a rotated-away
key's file doesn't wait a full hour once every session bound to it has moved
off. This does **not** force an already-bound session to rebind — that stays
an explicit `/model` switch (ADR-0063); it only reclaims the file once nothing
references it anymore.

**Request-body logging is opt-in and symmetric** (#165): every client emits a
`debug!` *summary* per request (model, message/tool counts — no payload). The
full request body — system prompt, the **entire conversation**, tool schemas
(repo/user data; API keys never appear, they ride in headers) — is logged only
through the shared `client::log_request_body(provider, &body)` helper, gated
behind `ENTANGLEMENT_LOG_BODIES=1` and truncated to 8 KiB on a UTF-8 boundary.
Raising `RUST_LOG` verbosity alone will **not** emit it; the flag is a separate,
explicit opt-in. Both `OpenAiLlm` and `AnthropicLlm` route through the one helper
so body logging is identical across backends.

**Provider/model catalog (`entanglement-provider::catalog`, #118,
[ADR-0032](../adr/0032-yaml-provider-model-catalog.md)):** the
provider + model list is **YAML, not code** — an embedded default
(`src/defaults.yml`, `include_str!`) deep-merged with an optional user override at
`${config_dir}/entanglement/providers.yml` (override the path via
`ENTANGLEMENT_PROVIDERS_FILE`). The merge runs at the `serde_yaml::Value` level
*before* deserializing, so field-level override falls out for free: `providers`
merge by `name`, `models` by `id`, mappings recurse, other scalars/sequences are
replaced; the final `Catalog` deserialize is `deny_unknown_fields` (typos are
loud). A `wire: openai | anthropic` tag on each provider is what makes
user-defined providers work with **zero code change** — any OpenAI-compatible
endpoint (proxy, local vLLM, new vendor) is `wire: openai` + `base_url` +
`key_env`. A provider entry may also **bundle MCP servers** (#542,
[ADR-0152](../adr/0152-provider-bundled-mcp-servers-three-state-enablement.md)):
`mcp_servers: {name → ProviderMcpServer}` — transport (`command` XOR `url`,
validated runtime-side), `${VAR}` headers, the #426 capability hint, and a
default `McpServerState` (`None` ⇒ `allowed`). The embedded defaults ship
z.ai's `web_search_prime`/`web_reader`/`zread` this way, key-gated on
`ZAI_API_KEY`; the runtime (`entanglement-runtime::mcp::available`) owns all
interpretation — the provider crate only carries the data. `ModelEntry`
carries capability flags (`supports_thinking`,
`supports_temperature`, `default_temperature`, `max_output_tokens`,
`thinking_budget_tokens`, `thinking_style`, `replay_thinking`) and **pricing**
(USD/M tokens:
`input`/`output`/`cached_input`/`cache_write`, all optional). Lookups:
`Catalog::{builtin,load,load_from}`, `provider(name)`, `model(provider,id)`,
`model_by_id(id)`.

**Generation-parameter channel (#191).** Those capability flags used to be
write-only — the YAML promised temperature/thinking behavior no client sent.
`ModelEntry::generation_params()` now turns them into a `GenerationParams`
`{ temperature, max_output_tokens, thinking_budget_tokens, reasoning_effort }`,
gated on the flags: temperature only when `supports_temperature`, a thinking
budget only when `supports_thinking` (and a budget is configured — the
embedded defaults leave it unset, so extended thinking is *reachable*, not
forced on), `reasoning_effort` from the optional
`default_reasoning_effort` catalog field (also unset by default). The runtime
resolves it for the chosen model onto `EngineConfig::generation`; core threads
it onto every `LlmRequest { …, generation }`. Each client maps the present
knobs to its wire and omits the rest: `OpenAiLlm` sends `temperature` +
`max_tokens` + `reasoning_effort` (its native wire field — no thinking-budget
channel); `AnthropicLlm` uses `max_output_tokens` in place of its
`DEFAULT_MAX_TOKENS` fallback and emits one of **two mutually exclusive
thinking shapes** (below), else passes `temperature` through; `GeminiLlm` maps
onto `generationConfig.thinkingConfig.thinkingBudget`. Neither Anthropic nor
Gemini has a native effort field on the budget shape (#374,
[ADR-0094](../adr/0094-reasoning-effort-and-per-profile-generation-persistence.md)):
an explicit `thinking_budget_tokens` always wins; absent one, `reasoning_effort`
derives a budget from a fixed tier (`High`/`Medium`; `Low`/unset leaves
thinking off) — conservative per-client constants, not catalog-driven, since
the real per-model ceiling varies.

**Anthropic thinking shapes (`ModelEntry::thinking_style`).** Anthropic replaced
the fixed-budget form with an adaptive one and the newer models *reject*
`budget_tokens` with a 400, so which shape is legal is a per-model catalog fact,
not a client constant:

| `thinking_style` | Emitted | Enabled by |
| --- | --- | --- |
| `budget` (default) | `thinking { type: enabled, budget_tokens }`, bumping `max_tokens` above the budget and dropping `temperature` | `thinking_budget_tokens`, else a `reasoning_effort` tier |
| `adaptive` | `thinking { type: adaptive }` + `output_config.effort` | `reasoning_effort` only — a stale `thinking_budget_tokens` is ignored rather than 400ing, and there is no `max_tokens` bump |

`budget` is the default so every pre-existing user `providers.yml` is unchanged;
the embedded defaults ship `thinking_style: adaptive` on the current models
(`claude-opus-5`, `claude-opus-4-8`, `claude-sonnet-5`).

### Extended thinking: capture and replay

With thinking enabled, Anthropic requires the unmodified `thinking` /
`redacted_thinking` block — signature intact — on the **final** assistant
message whenever tool results are returned, which is exactly a parked turn
([ADR-0061](../adr/0061-parked-turn-state-batch-tool-resolution.md)). So
reasoning cannot be display-only on that wire
([ADR-0160](../adr/0160-extended-thinking-round-trip.md)).

`ContentPart::Reasoning { provider, text, data }` carries it in history: `data`
is the provider's own wire shape (Anthropic's `signature`, and whether the block
was redacted, live inside it), `text` is the human-readable rendering and may be
empty — current models omit thinking text by default while still signing the
block, and such a block still has to replay.

**Capture is unconditional. Replay is gated by `ModelEntry::replay_thinking`**
(`Option<bool>`; `None` derives from the wire — Anthropic on whenever thinking
is enabled, others off — and an explicit value always wins). The flag never
affects capture, persistence, or rendering, so toggling it cannot rewrite a
session log.

| Wire | Capture | Replay when enabled |
| --- | --- | --- |
| Anthropic | `thinking` assembled across `thinking_delta` + `signature_delta`; `redacted_thinking` whole | verbatim, **first** in the block list, **last** assistant message only |
| Gemini | thought-text parts | none — the load-bearing `thoughtSignature` round-trips via `ToolCall::provider_meta` (ADR-0085) |
| OpenAI-compat | none on the wire | none |

Three rules hold regardless of the flag: a block whose `provider` differs from
the target renders **nothing** (stricter than `ProviderSearch`'s summary
fallback — reasoning is not answer content); `ContentPart::as_text` returns
`None` so reasoning stays out of `content_text`, compaction, and token
estimation; and an unsigned thinking block is display-only, since Anthropic
rejects one. Capturing the block also closes the older `pause_turn`
mid-thinking-block gap — `assembled_blocks` now includes thinking.

**Ollama `max_output_tokens` catalog default (#483):** the embedded `ollama`
entries set `max_output_tokens` explicitly (8192/2048/4096 for
`llama3.1`/`llama3`/`mistral`) rather than leaving it unset like every other
built-in entry. Unset, `OpenAiLlm` sends no `max_tokens`, so Ollama falls back
to its own `num_predict` default — 128 tokens — which was the leading cause of
the ADR-0118 "announced intent then stream died" ambiguous-stop symptom on
local models. Local generation has no per-token cost, so the values are
generous, just kept under each model's `context_window`; a `providers.yml`
override wins as usual.

**Provider selection (`skutter`):** the catalog loads once at startup; a
malformed user file is a loud error, never a silent fallback — and so is an
explicit `ENTANGLEMENT_PROVIDERS_FILE` that points at a missing file (a mistyped
override no longer silently yields the embedded defaults, #204; the *default*
`${config_dir}` path being absent stays the normal "no user override" case).
`ENTANGLEMENT_PROVIDER=<name>`
looks `<name>` up **in the catalog** (so custom providers work; `echo` stays a
built-in stub), erroring loudly if its key env is missing; if unset, auto-detect
by iterating catalog order and picking the first provider whose `key_env` is set
and non-empty (keyless Ollama is skipped) — preserving z.ai → OpenAI → Anthropic;
else `EchoLlm`. Precedence overall is **env > user YAML > embedded defaults**.

The `EchoLlm` stub echoes a one-line summary of the request it received —
message count, user-text snippets, the assembled system prompt (`system_len` +
an 8-hex `system_sha` SHA-256 fingerprint) and the advertised `tools=[names]` —
so `ENTANGLEMENT_PROVIDER=echo skutter run` doubles as a prompt-assembly smoke
test (which prompt/tool set actually reached the backend). Set
`ENTANGLEMENT_ECHO_FULL=1` to append the full system text.
Per-provider env still wins: `<PROV>_API_KEY` (name from the entry's `key_env`),
`<PROV>_MODEL`, `<PROV>_BASE`/`<PROV>_API_BASE`. Default models come from each
provider's `default_model` (`glm-5.2` / `gpt-4o` / `llama3.1` /
`claude-sonnet-4-5`). The TUI model picker + context bar read the same catalog.

**Multi-user provider context (#522, [ADR-0147](../adr/0147-multi-user-mode-embedder-api.md)):**
everything above is the **single-user** story — one process-global `Catalog`,
API keys loaded from the managed `.env` file (#220) into `std::env`. A
multi-user embedder instead builds `ModelResolver`s per user via
`entanglement-runtime::multi_user::provider` (behind the `provider` feature):
`ModelResolver` itself widened to `Fn(Option<&UserId>, &str, &str) -> Result<ResolvedModel, String>`
so its three call sites in `entanglement-core/src/session.rs` (session-start
pin, `SetAgent` pin rebind, `SetModel`) can resolve against the *resolving
session's own user* — single-user callers (`main.rs::build_model_resolver`)
simply ignore the parameter. `build_user_model_resolver` looks the session's
`UserId` up in an embedder-supplied `UserProviderStore`, resolving each
`UserProviderContext`'s own `Catalog` (same shape as `providers.yml` — so
**per-user RPM/concurrency budgets are just per-user catalog data**, no new
plumbing) and API keys (an in-memory map, **never** written to `std::env`).
The shared `HttpClient` connection pool still isolates rate-limit state per
`(base_url, sha256(api_key))` (ADR-0050), so two users with distinct keys on
the same provider already get independent `EndpointState`s with no further
change — two users sharing one literal key currently share that key's budget
too (an accepted v1 gap). `serve` is unaffected — it stays single-user
(ADR-0048); this seam is reachable only through the embedder library API.
