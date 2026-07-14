# entanglement Architecture — LLM I/O (provider crate)

> Part of the [architecture overview](../architecture.md). The *why* behind each choice is in the [decision log](../adr/README.md).

## 5b. LLM I/O (`entanglement-provider`) — [ADR-0007](../adr/0007-streaming-llm-and-provider-crate.md), [ADR-0053](../adr/0053-invert-core-provider-seam.md)

The `Llm` **trait** — together with its DTOs (`LlmRequest`/`LlmResponse`/
`LlmEvent`/`LlmStream`, `LlmFactory`, `ToolCall`, `ToolSpec`,
`stream_from_response`), the stub backends (`DummyLlm`/`EchoLlm`, in
`src/llm.rs`), and the wire message types (`Message`/`MessageRole`, in
`src/message.rs`) — lives **in `entanglement-provider`**. Since
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
| `OpenAiLlm` (`openai.rs`) | `/chat/completions` SSE | **z.ai** (GLM, entanglement's primary), **OpenAI**, **Ollama** `/v1` | `Bearer` or none (Ollama) |
| `AnthropicLlm` (`anthropic.rs`) | `/v1/messages` SSE | Anthropic | `x-api-key` |

- `OpenAiLlm` is one generic client `{ base_url, api_key: Option, default_model }`
  hand-rolled over `reqwest` (no SDK crate). Preset base constants
  (`ZAI_CODING_PLAN_BASE`, `ZAI_GENERAL_BASE`, `OPENAI_BASE`, `OLLAMA_BASE`) still
  exist, but the *default* base per provider now comes from the catalog (below);
  `openai_factory(base, key, model, rpm)` builds an `LlmFactory`.
- `AnthropicLlm` is separate because Anthropic's format genuinely differs (system
  top-level, tool results merged into one user turn, `input_json_delta`
  fragments). `anthropic_factory(key, model, rpm)`.
- `ToolSpec.schema` surfaces as `input_schema` (Anthropic) / `parameters`
  (OpenAI-compat); `Message.tool_call_id` → `tool_use_id` / `tool_call_id`.

**Resilience the provider layer owns — per endpoint** (#217,
[ADR-0050](../adr/0050-per-endpoint-connection-pool-retry-rate-limit.md)): one
tuned `reqwest::Client` is shared (it already pools TCP connections per host),
but the **rate-limit budget and retry/backoff state are keyed by `(endpoint,
api-key)`** — the provider's base URL plus a *hash* of the API key (if any) — in
`HttpClient`'s `EndpointPool`. Each such bucket owns a token-bucket RPM throttle
and its own `Retry-After` cool-down window, so a throttled endpoint never starves
another — and **multiple keys on the same endpoint each get their own budget**
(different keys have different limits). The key is hashed, never stored raw in
the map. Before #217 a single global 50-RPM `Semaphore` was shared across *all*
providers. The bucket's RPM is **catalog data** (#241): the provider entry's
optional `rpm` (env `{NAME}_RPM` > user `providers.yml` > embedded default),
threaded through `openai_factory`/`anthropic_factory` → `execute_with_retry` →
`EndpointState::new`; when unset it falls back to the client default
(`RetryConfig::rpm`, 50).

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
(not just a `reqwest::Error`) is retried with exponential backoff + jitter,
honoring `Retry-After` per endpoint; before #217 those responses came back as
`reqwest::Ok` and were never retried (#193). `RetryConfig` (`max_attempts`,
`initial_backoff`, `max_backoff`, `rpm`) tunes it; `HttpClient::with_config` +
`RetryConfig::no_retry()` build variants (tests use the latter). This
per-endpoint state is the reason a session carries **no** per-session connection
handle: the `LlmSession` newtype was collapsed to a plain `Box<dyn Llm>` (#195,
[ADR-0062](../adr/0062-collapse-llmsession-placeholder-newtype.md)) — resilience
belongs to the endpoint, shared across sessions, not to the conversation.

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
`supports_temperature`, `default_temperature`) and **pricing** (USD/M tokens:
`input`/`output`/`cached_input`/`cache_write`, all optional). Lookups:
`Catalog::{builtin,load,load_from}`, `provider(name)`, `model(provider,id)`,
`model_by_id(id)`.

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
