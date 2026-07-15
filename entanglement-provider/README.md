# entanglement-provider

Concrete LLM backends for the [entanglement](https://github.com/xmiksay/entanglement)
agent engine — and the crate that **owns the LLM ABI**: the `Llm` trait and its
DTOs (`LlmRequest` / `LlmEvent` / `LlmStream`, `LlmFactory`, `Message` /
`ContentPart`, `ToolCall` / `ToolSpec`, `GenerationParams`, `Usage`).

It is the **leaf** crate of the workspace (`provider ← core ← runtime`): it
depends on no other `entanglement-*` crate and is usable **standalone** for raw
streaming LLM queries.

## Backends

| Provider | Wire | Key env | Model env (default) |
| --- | --- | --- | --- |
| z.ai GLM (primary) | OpenAI-compat | `ZAI_API_KEY` | `ZAI_MODEL` (`glm-5.2`) |
| OpenAI | OpenAI-compat | `OPENAI_API_KEY` | `OPENAI_MODEL` (`gpt-4o`) |
| Ollama | OpenAI-compat, keyless | — | `OLLAMA_MODEL` (`llama3.1`) |
| Anthropic | `/v1/messages` | `ANTHROPIC_API_KEY` | `ANTHROPIC_MODEL` (`claude-sonnet-4-5`) |

z.ai / OpenAI / Ollama share one `OpenAiLlm` client; Anthropic has its own
client (distinct content-block format). No key configured → `EchoLlm`, a
deterministic offline backend for tests and demos.

## What's inside

- **Streaming first** — every backend yields `LlmEvent`s (text deltas,
  reasoning/thinking deltas, streaming tool-call argument deltas, finish with
  usage + stop reason) over an async `LlmStream`.
- **Provider/model catalog as data** — an embedded `defaults.yml` deep-merged
  with an optional user override (`${config_dir}/entanglement/providers.yml`,
  env override `ENTANGLEMENT_PROVIDERS_FILE`). A `wire: openai | anthropic` tag
  lets you add any OpenAI-compatible endpoint (proxy, vLLM, new vendor) with
  zero code change. Model entries carry capability flags (thinking,
  temperature, max output tokens) and pricing.
- **Per-endpoint resilience** — connection pool, retry with backoff, and
  rate-limit handling (429 / `Retry-After` / RPM), keyed by base URL +
  API-key hash.
- **Multimodal messages** — `Message` content is a list of `ContentPart`s
  (text + base64 images).

## Docs

Architecture: [provider module](https://github.com/xmiksay/entanglement/blob/master/docs/architecture/provider.md)
· Repo: [xmiksay/entanglement](https://github.com/xmiksay/entanglement)

## License

MIT — see [LICENSE](https://github.com/xmiksay/entanglement/blob/master/LICENSE).
