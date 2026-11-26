# LLM I/O §5b — Advertised surface, web search & per-endpoint resilience

> Part of the [llm i/o module](../provider.md) of the [architecture overview](../../architecture.md).

**One advertised surface, no deferral** ([ADR-0226](../../adr/archive/0226-one-advertised-surface-mcp-envelope-registered-is-available.md), superseding
[ADR-0196](../../adr/archive/0196-tool-search-and-lazy-discovery-replace-the-invoke-envelope.md)
§1–§4) — every wire receives the same pinned `tools` array, so the provider
crate carries no tool-search machinery: `ToolSpec` has no `defer_loading`, the
Anthropic client sends no `defer_loading`/`tool_reference`, and the
`openai_responses` client declares no `tool_search` primitive. The
`ContentPart::ToolReference`/`ToolSearchOutput` variants survive only so old
logs deserialize; every wire degrades them to text on replay. The leaf crate
owns one shared name for the runtime and core: `MCP_TOOL` (`"mcp"`), the
`mcp(server, tool, args)` envelope core unwraps. The `openai_responses`
client (opt-in catalog `wire:`, never a default) speaks OpenAI's flat
`input`/`output` item shape: a `function_call`/`function_call_output` pair
correlates by `call_id`. Split across `mod.rs` (client + streaming loop),
`request` (request-body construction), `sse` (SSE event parsing) — the
`openai/` file-cap pattern. This section stays brief by design: the seam is
what belongs here, the full wire contracts (request/response JSON, streaming
event shapes, per-provider quirks) live in
`scratch/tool-search-wire-reference.md`.

**The general request behind `web_fetch` and `http()`** (`web_fetch::request`,
[ADR-0227](../../adr/archive/0227-one-binding-path-namespaces-http-skill-libraries.md)) —
`WebFetcher::request(HttpRequest{method, url, headers, body, max_bytes},
check_hop, credentials)`: any method from the closed `HTTP_METHODS` set, the
same SSRF guard and manual re-validated redirects as `fetch` (which now
delegates to it as a GET). Two caller seams run per hop: `check_hop(method,
url)` may stop the request before it is sent, and `credentials(url)` supplies
the hidden headers for the URL about to be requested — asked again on every
hop, so the runtime drops them on a cross-host or non-https redirect. 303
(and 301/302 for a non-GET) become a bodyless GET; 307/308 keep method and
body. Header values never appear in an error. `scheme_and_host` gives the
normalized scheme/host the runtime matches `http_auth` entries against. (The
#560 P8 endpoint mechanism — `endpoint::call`, `EndpointMethod`, pooled
per-URL — is removed with the declared endpoint tools.)

**Provider-side web search** (#305,
[ADR-0075](../../adr/archive/0075-provider-side-web-search-mvp.md); post-MVP follow-ups
#481, [ADR-0131](../../adr/archive/0131-web-search-post-mvp-follow-ups.md)) — opt-in,
**client-construction-time** config, **no core/protocol change**.
`WebSearchConfig { enabled, max_uses, allowed_domains }` (`web_search.rs`,
`deny_unknown_fields`) is bound onto a client by its factory as an `Option` (the
runtime hands it `Some` only when a `web_search:` `config.yml` section is
enabled; the live `/model` resolver captures it too, so a switch re-binds
identically). When present, `build_body` pushes the provider's **server-executed**
search tool onto the same `tools` array (so it rides even with no function
tools): z.ai a `{"type":"web_search","web_search":{…}}` entry — only on a
provider carrying the z.ai dialect (`thinking_control: zai`, i.e. `zai` /
`zai_paas`), since the runtime binds one config onto every OpenAI-wire provider
and OpenAI proper 400s on a non-`function` tool type — Anthropic a
`{"type":"<version>","name":"web_search"}` server tool (+ optional
`max_uses`/`allowed_domains`) — `<version>` is `ModelEntry.web_search_tool_version`
(#481, catalog data) when the active model sets one, else the client's
`web_search_20250305` fallback, so a model requiring the newer `_20260209` tool
works via catalog config with no code change (the embedded defaults set it on
every 4.6+ Anthropic model; like the thinking facts it is bound for the client's
construction model — see `AnthropicModelSpec` in [catalog](catalog.md)). The provider runs the search
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
(#625, [ADR-0171](../../adr/archive/0171-zai-streaming-web-search-placement-confirmed.md),
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
the runtime permission ladder ([ADR-0047](../../adr/archive/0047-local-trust-boundary.md)).

**Resilience the provider layer owns — one limiter per endpoint** (#918,
building on #217/[ADR-0050](../../adr/archive/0050-per-endpoint-connection-pool-retry-rate-limit.md)):
one tuned `reqwest::Client` is shared (it already pools TCP connections per
host), and every request admits through its endpoint's one `Limiter`
(`client/limiter.rs`). Endpoints are keyed by `(endpoint, api-key)` — the
base URL plus a *hash* of the API key — in `HttpClient`'s `EndpointPool`, so a
throttled endpoint never starves another and **multiple keys on the same
endpoint each get their own limits**. The key is hashed, never stored raw.
`client::pool_key`'s endpoint half is normalized first — trailing `/` trimmed,
host lowercased (path left case-sensitive) — so a trailing-slash or
host-casing difference can't split one endpoint in two; the API-key half is
hashed with `sha2::Sha256` (#551) because it also becomes the cross-process
shared-state **file name** ([timeouts & shared state](timeouts-and-shared-state.md)).
Two users of a multi-user embedder sharing one literal key share that key's
limiter — which is what the provider enforces; there is no per-user gate.

**One config shape: `RateLimits { rpm, tpm, concurrency }`** — the catalog's
`limits:` block, on a provider (the endpoint) and optionally on a model (that
model on its provider's endpoint; z.ai caps concurrency per model:
`GLM-4.7-Flash` 1, `GLM-5.2` 5, on one base URL and key). Every wire client
takes one `LimitsResolver` (`Catalog::limits_resolver(provider)`, or
`fixed_limits(RateLimits)` without a catalog) and resolves it **per request**
against the request's own model (#550 — a `model:`-only profile pin can send a
request under a different model than the client's `default_model`), producing
a `RequestLimits { endpoint, model }`. An endpoint's unset axes fall back to the
client's defaults — `RateLimits::DEFAULT` (50 rpm, 3 in flight, no `tpm`)
unless the embedder built the client with `HttpClient::with_default_limits`
(MCP endpoints, which no catalog entry describes, run on these). A model's
unset axes are simply unlimited for that model; it stays bound by its
endpoint. A caller bringing different limits for an endpoint or model than
the live gate was built with (an admin edited the catalog) replaces that gate
with a `tracing::warn!`, rather than being bound by whoever came first.

Each key (the endpoint, each model with limits) is one `Gate`:

- a **sliding 60 s window** over requests (`rpm`) and estimated input tokens
  (`tpm`; the request body's serialized bytes / 4, measured only when a `tpm`
  is set — output tokens are unknown at admission). A request waits until the
  oldest entry leaves the window; one larger than the whole `tpm` still goes
  through an empty window;
- a **semaphore** over in-flight requests (`concurrency`). The permit rides an
  opaque `StreamGuard` that `spawn_byte_stream` holds until the **streamed
  body** ends — so the cap counts *open streams*, the unit a provider limits,
  and a spawn-storm of sub-agents queues instead of 429-storming.

`Limiter::admit` takes them **narrowest first**: the model's permit and window,
then the endpoint's, then the cross-process lease (#546) — a caller blocked on
its own saturated model never holds the endpoint slot (or a sibling process's
lease) hostage. Every retry attempt admits afresh, so retries count against
`rpm`/`tpm` like any request.

**429 and `Retry-After` live in the same limiter.** A 429 parks the endpoint's
cool-down (even with no header) so every caller of it backs off together, and
is retried on a patient schedule (`rate_limit_initial_backoff` ≈5s →
`rate_limit_max_backoff` ≈10 min; a server `Retry-After` wins, clamped to a 24h
ceiling so a huge delta-seconds or far-future HTTP-date can't overflow the
`Instant + Duration` arithmetic, #548) **until it clears or
`rate_limit_max_elapsed` (≈15 min) is spent** — then the 429 itself comes back
for the caller to error on, so a saturated endpoint *fails* a sub-agent's turn
rather than hanging its parent forever. Genuine failures (transport faults,
retryable 5xx, which also honor a `Retry-After`) stay bounded by
`max_attempts`. `RetryConfig` carries only these retry knobs; admission limits
are never in it.

**The endpoint-wide park is clamped separately from the offending caller's own
budget** (#547): what `Limiter::set_retry_after`/`SharedGate::mark_retry_after`
*store* — the deadline every *other* caller waits on via
`wait_for_retry_after`, in-process and (once persisted) across a restart — is
clamped to `rate_limit_max_backoff`, even when the server's raw `Retry-After`
is far larger. The offending caller's own give-up decision still honors the
raw, `MAX_RETRY_AFTER`-clamped value. Every wait a call makes —
`wait_for_retry_after` and `SharedGate::acquire`'s poll loop — is bounded by
*this call's own* `rate_limit_max_elapsed`: waiting further returns
`RetryError::RateLimited` instead of sleeping past it.

`HttpClient` is not LLM-only: since #559
([ADR-0157](../../adr/archive/0157-mcp-http-transport-shares-the-endpoint-pool.md)) the
MCP streamable-HTTP transport (`entanglement-provider::mcp::McpHttpClient`,
§gates/mcp.md §10) rides the same pool, keyed by its own URL, at the client's
default limits.

`Catalog::effective_concurrency(provider, model)` is a plain `Option<usize>`
snapshot (the model's own cap, else its provider's) for a caller that isn't
building a live client — the runtime's `AuxLlmRegistry` uses it to decide
whether firing an aux call alongside a primary-model call risks contending for
the same permit (see the *Per-purpose auxiliary models* section of the [heads
& persistence doc](../heads-and-persistence.md),
[ADR-0158](../../adr/archive/0158-defer-session-title-aux-call-under-contended-primary-concurrency.md)).

---

Previous: [§5b — The `Llm` trait, TLS & provider topology](llm-trait.md) · Next: [§5b — Timeouts, throttle visibility & cross-process state](timeouts-and-shared-state.md)
