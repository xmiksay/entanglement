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
  from answer text.
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
  `openai_factory(base, key, model, rpm, concurrency, web_search)` builds an
  `LlmFactory`. Split into `openai/{mod,request,sse}.rs` (#481) to stay under the
  400-line file cap — `mod.rs` owns the client + streaming loop, `request.rs`
  request-body construction, `sse.rs` chunk parsing.
- `AnthropicLlm` is separate because Anthropic's format genuinely differs (system
  top-level, tool results merged into one user turn, `input_json_delta`
  fragments). `anthropic_factory(key, model, rpm, concurrency, web_search,
  web_search_tool_version)`. Split into `anthropic/{mod,request,sse}.rs` (#481)
  the same way as `openai/` — `mod.rs` additionally owns the `pause_turn`
  continuation loop (below).
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
  concurrency, http)` — no web-search knob.
  Request-body assembly lives in the `gemini::request` submodule.
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
vanishing with the round. The z.ai array's streaming placement is still parsed
defensively (checked at the top level and inside `choices[0].delta`); #481
attempted live verification against a real Coding Plan key but neither a key nor
network egress was available in that environment, so the shape stays
**unconfirmed** — the parser is unchanged from the #305 MVP and the worst case is
still the cited-text-only floor; a future run with `ENTANGLEMENT_LOG_BODIES=1`
against a live key should close this out (tracked at
[ADR-0131](../adr/0131-web-search-post-mvp-follow-ups.md) §3).
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
the map. Before #217 a single global 50-RPM `Semaphore` was shared across *all*
providers. The bucket's RPM is **catalog data** (#241): the provider entry's
optional `rpm` (env `{NAME}_RPM` > user `providers.yml` > embedded default),
threaded through `openai_factory`/`anthropic_factory` → `execute_with_retry` →
`EndpointState::new`; when unset it falls back to the client default
(`RetryConfig::rpm`, 50).

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
server `Retry-After` wins) **until it clears or `rate_limit_max_elapsed` (≈15 min)
is spent** — then it surfaces as an error, so a saturated endpoint *fails* a
sub-agent's turn rather than hanging its parent forever. Genuine failures
(transport faults, retryable 5xx) stay bounded by `max_attempts`.

**Timeouts — connect + idle-gap, not whole-request** (#241): the shared
`reqwest::Client` is built with `connect_timeout` only (30s to establish TCP+TLS).
A fixed whole-request `.timeout()` would abort a long *healthy* LLM stream
mid-turn (and its partials, already consumed, aren't retryable) — and its 300s
ceiling was also what capped `Stop` cancel latency (#179). Instead liveness on
the streamed body is enforced per chunk: `client::spawn_byte_stream` forwards the
SSE bytes over an mpsc channel under a `tokio::time::timeout(STREAM_IDLE_TIMEOUT,
…)` watchdog (120s idle gap), so a slow-but-alive stream runs to completion while
a hung one dies fast. Both `OpenAiLlm` and `AnthropicLlm` use this one helper.
**Retry** classifies the *response* status inside the loop — a 429/5xx response
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
when every endpoint is at rest. It feeds no request logic — the TUI polls it each
frame to show a throttle indicator only while backing off (see heads doc). `RetryConfig` (`max_attempts`, `initial_backoff`,
`max_backoff`, `rpm`) tunes the *failure* path; `HttpClient::with_config` +
`RetryConfig::no_retry()` build variants (tests use the latter). This
per-endpoint state is the reason a session carries **no** per-session connection
handle: the `LlmSession` newtype was collapsed to a plain `Box<dyn Llm>` (#195,
[ADR-0062](../adr/0062-collapse-llmsession-placeholder-newtype.md)) — resilience
belongs to the endpoint, shared across sessions, not to the conversation. A
**live model/provider switch** (#218,
[ADR-0063](../adr/0063-realtime-model-provider-switch.md)) rebuilds that
`Box<dyn Llm>` from a `ResolvedModel` the runtime resolves against this catalog +
the warm per-endpoint client, so switching mid-session neither restarts the engine
nor cold-starts the pool.

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
`key_env`. `ModelEntry` carries capability flags (`supports_thinking`,
`supports_temperature`, `default_temperature`, `max_output_tokens`,
`thinking_budget_tokens`) and **pricing** (USD/M tokens:
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
`DEFAULT_MAX_TOKENS` fallback and emits `thinking { type: enabled,
budget_tokens }` whenever a budget resolves (bumping `max_tokens` above the
budget and dropping `temperature`, per Anthropic's constraints), else passes
`temperature` through; `GeminiLlm` maps onto
`generationConfig.thinkingConfig.thinkingBudget`. Neither Anthropic nor Gemini
has a native effort field (#374,
[ADR-0094](../adr/0094-reasoning-effort-and-per-profile-generation-persistence.md)):
an explicit `thinking_budget_tokens` always wins; absent one, `reasoning_effort`
derives a budget from a fixed tier (`High`/`Medium`; `Low`/unset leaves
thinking off) — conservative per-client constants, not catalog-driven, since
the real per-model ceiling varies.

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
