# LLM I/O §5b — The `Llm` trait, TLS & provider topology

> Part of the [llm i/o module](../provider.md) of the [architecture overview](../../architecture.md).

## 5b. LLM I/O (`entanglement-provider`) — [ADR-0007](../../adr/0007-streaming-llm-and-provider-crate.md), [ADR-0053](../../adr/0053-invert-core-provider-seam.md)

The `Llm` **trait** — together with its DTOs (`LlmRequest`/`LlmResponse`/
`LlmEvent`/`LlmStream`, `LlmFactory`, `ToolCall`, `ToolSpec`,
`stream_from_response`, the last two in `src/response.rs`), the stub backends
(`DummyLlm`/`EchoLlm`, in `src/llm/stub.rs`), and the wire message types (`Message`/`MessageRole` plus the
multimodal `ContentPart`/`ImageSource`, in `src/message.rs` — a `Message`'s body
is `content: Vec<ContentPart>`, #197/[ADR-0064](../../adr/0064-message-content-blocks.md))
— lives **in `entanglement-provider`**. Since
[ADR-0053](../../adr/0053-invert-core-provider-seam.md) inverted the seam, the
provider is a **leaf crate** (no `entanglement-*` deps) that owns this LLM ABI;
`entanglement-core` *depends on* provider, consumes the `Llm` trait from its turn
loop, and re-exports these types for its heads. The provider *may* depend on
transport crates (`reqwest` 0.13; `form` for the OAuth POSTs) and is usable **standalone** for raw LLM queries
with no engine.

```rust
enum StopReason { EndTurn, ToolUse, MaxTokens, StopSequence, Other }
struct Usage { input_tokens?, output_tokens?, cached_input_tokens?, cache_write_tokens? }
enum LlmEvent {
    Text(String),
    Reasoning(String),   // thinking/reasoning tokens, streamed distinctly
    ToolCallDelta { id, name, delta },   // streamed tool-arg fragment, before ToolCall (#194)
    ToolCall(ToolCall),
    ContentBlock(ContentPart),   // server-minted persisted block (web search, signed thinking)
    Finish { stop_reason: StopReason?, usage: Usage },   // normalized (#192)
}
struct LlmResponse { text, tool_calls, reasoning, content_blocks, stop_reason: StopReason?, usage }
trait Llm: Send {
    async fn stream(req) -> Result<BoxStream<'static, Result<LlmEvent>>>;
    async fn complete(req) -> Result<LlmResponse>   // default: drains stream() (#728)
}
```

- **`Llm::complete`** (#728) is a default method for embedders with a
  non-streaming loop: it drains `stream()` into one `LlmResponse` via
  `response::collect` / `LlmResponse::absorb` (text and reasoning chunks
  concatenated, assembled tool calls + content blocks in arrival order, the
  `Finish` stop reason + usage; `ToolCallDelta` fragments are dropped since the
  assembled `ToolCall` supersedes them). A stream that errored mid-way fails the whole
  call — never a partial reply, matching the turn loop, which drops a
  mid-stream-errored round's tool calls. A stream that ends cleanly without a
  `Finish` still returns (the turn loop likewise still executes its tool calls)
  with `stop_reason: None` — embedders should treat that as an ambiguous stop
  ([ADR-0118](../../adr/0118-ambiguous-stop-reason-bounded-retry.md)), not a
  confident end of turn. `stream_from_response`
  is its inverse for scripted backends (a `None` stop reason is derived from
  the tool calls). The turn loop keeps its own fold: it interleaves display
  events, mid-stream cancellation and the #181 retry-before-output with the
  accumulation.

- **TLS** (#728): every outbound client — the LLM `HttpClient`, the MCP HTTP
  transport riding it, the OAuth client — starts from `client::client_builder()`,
  a reqwest builder preloaded with one rustls `ClientConfig`: the **ring**
  crypto provider and the **bundled webpki roots** (no runtime reliance on the
  OS trust store, so minimal/musl containers need no `ca-certificates`; no
  aws-lc C build in ARM/musl cross-builds; `rustls-platform-verifier` is still
  compiled — `rustls-no-provider` pulls it — but unused). reqwest is built with
  `rustls-no-provider`, so a bare `reqwest::Client::new()` would panic for want
  of a process-wide crypto provider — never build one directly. Cargo feature
  unification carries this into an embedder's build, with two hazards
  (detail + cure in [embedding § TLS](../../embedding/tls-and-pinning.md#tls-and-the-rustls-crypto-provider)):
  an embedder that also enables `rustls/aws_lc_rs` (stock reqwest 0.13) gets
  both providers compiled, so rustls' implicit provider selection
  (`CryptoProvider::from_crate_features()`) yields `None` and its own
  `rustls::ClientConfig::builder()` / tokio-tungstenite / sqlx rustls paths
  panic — its own reqwest `Client::new()` still works; and an embedder on
  reqwest 0.13 with `default-features = false` and no TLS feature gets
  `__rustls` unified in, so its `Client::new()` now panics (it used to fail
  HTTPS only at request time). The opt-in cure is
  `client::install_default_crypto_provider()` (installs ring process-wide,
  first caller wins; the engine never calls it) early in `main`; building
  reqwest clients from `client::client_builder()` cures only the reqwest side —
  in case (a) other rustls users (tokio-tungstenite, sqlx,
  `rustls::ClientConfig::builder()`) still need the install or an explicit
  provider.
- Streaming mirrors opencode (Vercel AI SDK `doStream`): live token-by-token
  deltas, not a buffered whole-reply. The box stream is `'static`.
- **`LlmEvent::Reasoning`** surfaces extended-thinking output (Anthropic
  `thinking`/`redacted_thinking` blocks, OpenAI `reasoning_content`) instead of
  dropping it; core re-emits it as a reasoning `OutEvent` heads render distinctly
  from answer text. This is the *display* rail — it is deliberately never folded
  into `Context`. The *replay* rail is a separate
  `LlmEvent::ContentBlock(ContentPart::Reasoning)`
  ([ADR-0160](../../adr/0160-extended-thinking-round-trip.md)); both are emitted for
  the same thinking. See [**Extended thinking: capture and replay**](thinking-and-selection.md#extended-thinking-capture-and-replay).
- **`LlmEvent::ToolCallDelta`** (#194) streams a tool call's JSON arguments as
  they arrive — OpenAI `tool_calls[].function.arguments` fragments, Anthropic
  `input_json_delta.partial_json` — *before* the assembled `ToolCall` that both
  clients still emit on flush / `content_block_stop`. Correlated to that final
  call by `id`; core re-emits it as `OutEvent::ToolCallDelta` so a head can render
  file-sized `edit`/`write` arguments live. Additive: a consumer that ignores it
  still gets the full `ToolCall` (replay reconstructs state from that, not the
  fragments).
- **`LlmEvent::Finish`** is normalized (#192,
  [ADR-0055](../../adr/0055-usage-cost-and-stop-reason-surfacing.md)): `StopReason`
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
| `OpenAiLlm` (`openai/`) | `/chat/completions` SSE | **z.ai** (GLM, entanglement's primary — `zai` Coding Plan + `zai_paas` pay-as-you-go), **OpenAI**, **Ollama** `/v1` | `Bearer` or none (Ollama) |
| `AnthropicLlm` (`anthropic/`) | `/v1/messages` SSE | Anthropic | `x-api-key` |
| `GeminiLlm` (`gemini.rs`) | `:streamGenerateContent?alt=sse` | Google Gemini | `x-goog-api-key` |

- **Implicit-cache routing hint** (#673) — `LlmRequest` carries an optional
  `cache_key` (core sets the session's own id on every main-turn request;
  aux one-shots pass `None`), and the OpenAI client forwards it as the
  `prompt_cache_key` body field so a multi-instance endpoint routes one
  conversation's requests to the same cache shard. Catalog-gated per provider
  (`ProviderEntry::prompt_cache_key`, default **off**; the embedded defaults
  enable it only for `openai` — z.ai/Ollama stay off until verified to
  tolerate the extra field, and a user `providers.yml` can flip any entry).
  The other wires ignore the field entirely.
- `OpenAiLlm` is one generic client `{ base_url, api_key: Option, default_model }`
  hand-rolled over `reqwest` (no SDK crate). Preset base constants
  (`ZAI_CODING_PLAN_BASE`, `ZAI_GENERAL_BASE`, `OPENAI_BASE`, `OLLAMA_BASE`) still
  exist, but the *default* base per provider now comes from the catalog ([catalog](catalog.md));
  `openai_factory(base, key, model, rpm, concurrency, model_concurrency,
  web_search, prompt_cache_key, thinking_spec)` builds an `LlmFactory`, the
  `thinking_spec` resolver carrying the model's inline-think handling
  (ADR-0191) plus its effort tiers, `thinking_required` and the provider's
  `thinking_control` (ADR-0203). Split into `openai/{mod,request,sse,think}.rs`
  (#481) to stay under the 400-line file cap — `mod.rs` owns the client +
  streaming loop, `request.rs` request-body construction, `sse.rs` chunk
  parsing, `think.rs` the `<think>` splitter.
- `AnthropicLlm` is separate because Anthropic's format genuinely differs (system
  top-level, tool results merged into one user turn, `input_json_delta`
  fragments). `anthropic_factory(base_url, key, model, rpm, concurrency,
  model_concurrency, web_search, web_search_tool_version, model_spec)` — `base_url`
  defaults to `ANTHROPIC_BASE` (mirroring `OPENAI_BASE`/`GEMINI_BASE`); a
  catalog `wire: anthropic` entry's `base_url` (a proxy/gateway speaking the
  Anthropic wire) overrides it end to end — the request URL *and* the pool
  key — since #551 (previously hard-coded and silently ignored). Split into
  `anthropic/{mod,request,sse}.rs` (#481)
  the same way as `openai/` (which since #745 also moves its response
  decoding into `openai/stream.rs`) — `mod.rs` additionally owns the `pause_turn`
  continuation loop ([surface & resilience](surface-and-resilience.md)).
- **Explicit `cache_control` breakpoints** (#566) — Anthropic caching is opt-in
  per content block, unlike z.ai/OpenAI-compat's implicit whole-prefix caching;
  without a marker every round re-bills the full system + tool schemas + growing
  history at the uncached rate. `anthropic::request::build_body` places four:
  the last `system` block (also covers the `tools` array before
  it in the fixed tools → system → messages render order), the last `tools`
  entry, and the last content block of the **last** and third-to-last
  `user`-role messages (`place_history_breakpoint`,
  [ADR-0202](../../adr/0202-prompt-cache-discipline-anchors-deferral-replay-compaction-date.md)).
  The near anchor sits on the newest turn, so everything a request sends is
  written now and read back next round — the earlier second-to-last placement
  billed that tail uncached this round *and* as a cache write the next (~2.25×
  vs 1.25×) to spare a steered/edited retry at most one wasted write. The
  deeper anchor (#673) guarantees a cache match even when one round appends
  more user-role messages (merged tool-result turns from a big parallel batch)
  than the ~20-block lookback Anthropic scans upstream of a marker. Anchoring
  the last turn is only cache-stable because thinking now replays on every
  assistant turn (see [*Extended thinking*](thinking-and-selection.md#extended-thinking-capture-and-replay)). Four is exactly the API's
  marker cap, locked in by test.
- `GeminiLlm` is native, **not** Gemini's OpenAI-compat surface (#309,
  [ADR-0085](../../adr/0085-gemini-native-wire-and-opaque-provider-meta.md)): the
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
  A resource replaced by a key change is `DELETE`d in a detached task,
  failures only debug-logged, so it stops accruing storage before its 1h TTL.
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
  [ADR-0064](../../adr/0064-message-content-blocks.md)): text-only user content stays
  a plain string (OpenAI) / string content (Anthropic); an image part switches to
  the block array — OpenAI `image_url` with a `data:` URL, Anthropic an `image`
  block with a base64 `source` (incl. image `tool_result`s, the #221 path); Gemini
  has no image slot on `functionResponse.response` at all, so a tool result's
  image blocks ride as trailing `inlineData` parts alongside the
  `functionResponse` part in the same turn (#447).

---

Next: [§5b — Advertised surface, web search & per-endpoint resilience](surface-and-resilience.md)
