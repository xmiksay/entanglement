# LLM I/O §5b — Advertised surface, web search & per-endpoint resilience

> Part of the [llm i/o module](../provider.md) of the [architecture overview](../../architecture.md).

**One advertised surface, no deferral** ([ADR-0226](../../adr/0226-one-advertised-surface-mcp-envelope-registered-is-available.md), superseding
[ADR-0196](../../adr/0196-tool-search-and-lazy-discovery-replace-the-invoke-envelope.md)
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
[ADR-0227](../../adr/0227-one-binding-path-namespaces-http-skill-libraries.md)) —
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
[ADR-0075](../../adr/0075-provider-side-web-search-mvp.md); post-MVP follow-ups
#481, [ADR-0131](../../adr/0131-web-search-post-mvp-follow-ups.md)) — opt-in,
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
(#625, [ADR-0171](../../adr/0171-zai-streaming-web-search-placement-confirmed.md),
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
the runtime permission ladder ([ADR-0047](../../adr/0047-local-trust-boundary.md)).

**Resilience the provider layer owns — per endpoint** (#217,
[ADR-0050](../../adr/0050-per-endpoint-connection-pool-retry-rate-limit.md)): one
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
**file name** ([timeouts & shared state](timeouts-and-shared-state.md)) and so must be stable across separately-built `skutter`
processes, not merely process-local. Before #217 a single global 50-RPM
`Semaphore` was shared across *all* providers. The bucket's RPM is **catalog data** (#241): the provider entry's
optional `rpm` (user `providers.yml` > embedded default — no env override
since [ADR-0213](../../adr/0213-prune-env-vars-and-catalog-knobs.md)),
threaded through `openai_factory`/`anthropic_factory` → `execute_with_retry` →
`EndpointState::new`; when unset it falls back to the client default
(`RetryConfig::rpm`, 50).

`HttpClient` is not LLM-only: since #559
([ADR-0157](../../adr/0157-mcp-http-transport-shares-the-endpoint-pool.md)) the
MCP streamable-HTTP transport (`entanglement-provider::mcp::McpHttpClient`,
§gates/mcp.md §10) rides the same pool, keyed by its own URL plus
its bundling provider's API key when known — so a provider-bundled MCP server
sharing a key with its provider's LLM endpoint (e.g. z.ai's `web_search_prime`/
`web_reader`/`zread` against `ZAI_API_KEY`) counts against that key's real
budget instead of bypassing it.

The concurrency cap + pacing gate + 429 policy
([ADR-0111](../../adr/0111-adaptive-endpoint-pacing-and-429-retry-until-clear.md)) is
what makes the pool coordinate *across sessions* — the property that "spawn many
sub-agents" needs and ADR-0050 lacked. The **primary** guard is a per-endpoint
`concurrency` semaphore. **Also catalog data, mirroring `rpm`** (#414): the
provider entry's optional `concurrency` (user `providers.yml` > embedded
default, catalog data only since ADR-0213), threaded through the same
`openai_factory`/`anthropic_factory`/`gemini_factory` → `execute_with_retry` →
`HttpClient::endpoint` → `EndpointState::new` path as `rpm`; when unset it falls
back to the client's default (`RetryConfig::concurrency`, 3; the pre-#414
process-wide `ENTANGLEMENT_MAX_CONCURRENCY` override is retired, ADR-0213). A
permit is
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
[ADR-0140](../../adr/0140-per-model-concurrency-cap-layered-on-endpoint-cap.md)):
some providers — z.ai in particular — cap concurrency **per model**, not just
per endpoint (documented: `GLM-4.7-Flash` allows only 1 in-flight request,
`GLM-5.2` allows 5, on the same base URL/key), and per-profile model pinning
([ADR-0081](../../adr/0081-per-profile-model-pinning-and-rebind-on-set-agent.md))
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
request's actual model whenever an agent pins `model:` **without**
`provider:` (the documented request-level fallback: `Agent::model_pin`
returns `None`, so session start doesn't rebind the client). The mismatch paired
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
doc](../heads-and-persistence.md), [ADR-0158](../../adr/0158-defer-session-title-aux-call-under-contended-primary-concurrency.md)).

---

Previous: [§5b — The `Llm` trait, TLS & provider topology](llm-trait.md) · Next: [§5b — Timeouts, throttle visibility & cross-process state](timeouts-and-shared-state.md)
