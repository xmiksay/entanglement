# entanglement Architecture — LLM I/O (provider crate)

> Part of the [architecture overview](../architecture.md). The *why* behind each choice is in the [decision log](../adr/README.md).

## 5b. LLM I/O (`entanglement-provider`) — [ADR-0007](../adr/0007-streaming-llm-and-provider-crate.md)

The `Llm` **trait** lives in `entanglement-core` (the seam); all LLM I/O lives in
**`entanglement-provider`**, a separate crate that *may* depend on transport
crates (`reqwest`) — `entanglement-core` may not.

```rust
enum LlmEvent {
    Text(String),
    Reasoning(String),   // thinking/reasoning tokens, streamed distinctly
    ToolCall(ToolCall),
    Finish { input_tokens?, output_tokens? },
}
trait Llm: Send { async fn stream(req) -> Result<BoxStream<'static, Result<LlmEvent>>> }
```

- Streaming mirrors opencode (Vercel AI SDK `doStream`): live token-by-token
  deltas, not a buffered whole-reply. The box stream is `'static`.
- **`LlmEvent::Reasoning`** surfaces extended-thinking output (Anthropic
  `thinking`/`redacted_thinking` blocks, OpenAI `reasoning_content`) instead of
  dropping it; core re-emits it as a reasoning `OutEvent` heads render distinctly
  from answer text.

**Provider topology** — split by *wire format*, not by vendor:

| client (`entanglement-provider`) | wire format | serves | auth |
| --- | --- | --- | --- |
| `OpenAiLlm` (`openai.rs`) | `/chat/completions` SSE | **z.ai** (GLM, entanglement's primary), **OpenAI**, **Ollama** `/v1` | `Bearer` or none (Ollama) |
| `AnthropicLlm` (`anthropic.rs`) | `/v1/messages` SSE | Anthropic | `x-api-key` |

- `OpenAiLlm` is one generic client `{ base_url, api_key: Option, default_model }`
  hand-rolled over `reqwest` (no SDK crate). Preset base constants
  (`ZAI_CODING_PLAN_BASE`, `ZAI_GENERAL_BASE`, `OPENAI_BASE`, `OLLAMA_BASE`) still
  exist, but the *default* base per provider now comes from the catalog (below);
  `openai_factory(base, key, model)` builds an `LlmFactory`.
- `AnthropicLlm` is separate because Anthropic's format genuinely differs (system
  top-level, tool results merged into one user turn, `input_json_delta`
  fragments). `anthropic_factory(key, model)`.
- `ToolSpec.schema` surfaces as `input_schema` (Anthropic) / `parameters`
  (OpenAI-compat); `Message.tool_call_id` → `tool_use_id` / `tool_call_id`.

**Resilience the provider layer owns — per endpoint** (#217,
[ADR-0050](../adr/0050-per-endpoint-connection-pool-retry-rate-limit.md)): one
tuned `reqwest::Client` is shared (it already pools TCP connections per host),
but the **rate-limit budget and retry/backoff state are keyed by endpoint** (the
provider's base URL) in `HttpClient`'s `EndpointPool`. Each endpoint owns a
token-bucket RPM throttle (default 50 RPM, `RetryConfig::rpm`) and its own
`Retry-After` cool-down window, so a throttled endpoint never starves another —
before #217 a single global 50-RPM `Semaphore` was shared across *all* providers.
**Retry** classifies the *response* status inside the loop — a 429/5xx response
(not just a `reqwest::Error`) is retried with exponential backoff + jitter,
honoring `Retry-After` per endpoint; before #217 those responses came back as
`reqwest::Ok` and were never retried (#193). `RetryConfig` (`max_attempts`,
`initial_backoff`, `max_backoff`, `rpm`) tunes it; `HttpClient::with_config` +
`RetryConfig::no_retry()` build variants (tests use the latter). The
provider-owned `LlmSession` handle (#195) references this per-endpoint state
through its boxed backend.

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
malformed user file is a loud error, never a silent fallback. `ENTANGLEMENT_PROVIDER=<name>`
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
