# LLM I/O §5b — Provider/model catalog & advanced wire facts

> Part of the [llm i/o module](../provider.md) of the [architecture overview](../../architecture.md).

**Provider/model catalog (`entanglement-provider::catalog`, #118,
[ADR-0032](../../adr/archive/0032-yaml-provider-model-catalog.md)):** the
provider + model list is **YAML, not code** — an embedded default
(`src/defaults.yml`, `include_str!`) deep-merged with an optional user override at
`${config_dir}/entanglement/providers.yml` (override the path via
`ENTANGLEMENT_PROVIDERS_FILE`). The merge runs at the `serde_yaml::Value` level
*before* deserializing, so field-level override falls out for free: `providers`
merge by `name`, `models` by `id`, mappings recurse, other scalars/sequences are
replaced; the final `Catalog` deserialize is `deny_unknown_fields` (typos are
loud). A `wire: openai | anthropic | gemini | openai_responses` tag on each
provider is what makes user-defined providers work with **zero code
change** — any OpenAI-compatible endpoint (proxy, local vLLM, new vendor) is
`wire: openai` + `base_url` + `key_env`. `openai_responses` (ADR-0196 §3, P7)
is OpenAI's distinct Responses API — flat input/output items instead of
Chat Completions' role+content messages; opt-in per catalog entry, never a
default (`openai` itself stays on Chat Completions). An entry authenticates
with its static `key_env` key, or none (keyless); there is no LLM-endpoint
OAuth (#917 — an `oauth:` key on a provider entry is rejected like any
unknown field). The catalog carries no MCP servers — those are the user
`config.yml` `mcp:` map alone (#915). `ModelEntry`
carries capability flags (`supports_thinking`, `supports_vision` (image input;
gates TUI image paste, #736),
`supports_temperature`, `default_temperature`, `max_output_tokens`,
`thinking_budget_tokens`, `thinking_style`, `thinking_format`,
`replay_thinking`, `effort_tiers`, `thinking_required`; provider-level
`thinking_control`) and **pricing**
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
`default_reasoning_effort` catalog field (unset by default except on the z.ai
entries' thinking-capable models, which default to `high`). The runtime
resolves it for the chosen model onto `EngineConfig::generation`; core threads
it onto every `LlmRequest { …, generation }`. Each client maps the present
knobs to its wire and omits the rest: `OpenAiLlm` sends `temperature` +
`max_tokens` + `reasoning_effort` (its native wire field, clamped to the
model's tiers — no thinking-budget channel — plus z.ai's `thinking` object
under `thinking_control: zai`); `AnthropicLlm` uses `max_output_tokens` in place of its
`DEFAULT_MAX_TOKENS` fallback and emits one of **two mutually exclusive
thinking shapes** (below), else passes `temperature` through; `GeminiLlm` maps
onto `generationConfig.thinkingConfig.thinkingBudget`. Neither Anthropic nor
Gemini has a native effort field on the budget shape (#374,
[ADR-0094](../../adr/archive/0094-reasoning-effort-and-per-profile-generation-persistence.md)):
an explicit `thinking_budget_tokens` always wins; absent one, `reasoning_effort`
derives a budget from a fixed tier (`High` — shared by `XHigh`/`Max` — or
`Medium`; `Low`/unset leaves thinking off) — conservative per-client constants, not catalog-driven, since
the real per-model ceiling varies.

### Advanced catalog (wire facts)

The knobs below describe **what an endpoint or model accepts on the wire** —
not preferences — so they are catalog data only (`providers.yml`, no env var,
no `config.yml` key, [ADR-0213](../../adr/archive/0213-prune-env-vars-and-catalog-knobs.md)): `thinking_style`, `thinking_format`,
`thinking_control`, `thinking_required`, `replay_thinking`, `effort_tiers`,
`supports_temperature`, `prompt_cache_key`, and the
`limits: {rpm, tpm, concurrency}` block (provider and model,
[resilience](surface-and-resilience.md)). The former `tool_advertising`/`discovery` knobs are
retired ([ADR-0226](../../adr/archive/0226-one-advertised-surface-mcp-envelope-registered-is-available.md)): a user file still carrying either, at provider or model level,
loads with one warning per key.
A user only touches them to describe an endpoint the embedded defaults don't
know (or know wrongly). `skutter inspect config` lists every non-default value
per provider/model under "advanced catalog", so the effective facts stay
visible without reading YAML.

**Anthropic thinking shapes (`ModelEntry::thinking_style`).** Anthropic replaced
the fixed-budget form with an adaptive one and the newer models *reject*
`budget_tokens` with a 400, so which shape is legal is a per-model catalog fact,
not a client constant:

| `thinking_style` | Emitted | Enabled by |
| --- | --- | --- |
| `budget` (default) | `thinking { type: enabled, budget_tokens }`, bumping `max_tokens` above the budget and dropping `temperature` | `thinking_budget_tokens`, else a `reasoning_effort` tier |
| `adaptive` | `thinking { type: adaptive }` + `output_config.effort` clamped to `effort_tiers` (empty set ⇒ no `output_config`); never a disable shape | `reasoning_effort` only — a stale `thinking_budget_tokens` is ignored rather than 400ing, and there is no `max_tokens` bump |

`budget` is the default so every pre-existing user `providers.yml` is unchanged;
the embedded defaults ship `thinking_style: adaptive` on every 4.6+ model (Fable
5.1/5, Opus 5/4.8/4.7/4.6, Sonnet 5/4.6), while `claude-sonnet-4-5` and
`claude-haiku-4-5` keep the budget shape. The default Anthropic model is
`claude-sonnet-5`.

**Anthropic model facts are bound at construction (`AnthropicModelSpec`).**
`Catalog::anthropic_model_spec(provider, model)` resolves the thinking shape,
`replay_thinking`, `effort_tiers` and `supports_temperature` once for the
client's model and threads them through `anthropic_factory` (an unlisted model
gets the default: budget shape, replay on, no clamp, temperature allowed). The
adaptive shape clamps `output_config.effort` to the tiers, and
`supports_temperature: false` drops `temperature` from the body even when a
live `SetGeneration` set one — Fable, Opus 5/4.8/4.7 and Sonnet 5 400 on any
sampling parameter. A `thinking` disable shape is never emitted (Fable 400s on
it; its `thinking_required` only documents that its thinking is always on).
**Caveat:** unlike the per-request OpenAI-wire resolvers (#550), a
`model:`-only profile pin that sends a request under a different model id still
uses the construction model's facts, so a
profile crossing Anthropic generations should pin `provider:` alongside
`model:` (which rebinds the client).

**Effort tiers, required thinking, z.ai thinking control** ([ADR-0203](../../adr/archive/0203-catalog-precision-effort-tiers-zai-thinking-control-dual-endpoints.md)).
`ReasoningEffort` has five ordered tiers (`low|medium|high|xhigh|max`).
`ModelEntry::effort_tiers` lists the ones a model accepts: unset passes any
tier through, a list clamps to the nearest listed tier (ties round down, so a
model never silently costs more than asked), and the empty list sends no effort
field at all. Every wire that names a tier applies the clamp — OpenAI-compat
`reasoning_effort` and Responses `reasoning.effort` per request through the
`ThinkingSpecResolver`, Anthropic adaptive `output_config.effort` through
`AnthropicModelSpec`; the wires with no effort ladder (Anthropic's budget
shape, Gemini) give `xhigh`/`max` `high`'s budget. `thinking_required: true`
marks a model that cannot run with thinking off: with no effort resolved, the
lowest listed tier is sent. `ProviderEntry::thinking_control: zai` is the z.ai
request dialect: every request carries `thinking: {type: enabled}` (plus the
clamped effort) when an effort resolves or the model requires thinking, else
`{type: disabled}` — z.ai's own default with no object is thinking at `max`.
A provider without it never sees a `thinking` object (OpenAI proper 400s on
unknown fields). The
embedded z.ai entries default every thinking-capable model to `high` (GLM-5.3 /
5.3-Flash take only `low|high|max` and require thinking; the 4.x family takes
`effort_tiers: []`), with `glm-5.3` as the default model. z.ai ships as **two
entries sharing `ZAI_API_KEY`**: `zai` (Coding Plan endpoint, first in catalog
order so auto-detect keeps selecting it) and `zai_paas` (pay-as-you-go
endpoint, selected explicitly; env overrides follow the entry name —
`ZAI_PAAS_MODEL`, `ZAI_PAAS_API_BASE`), sharing one model list via a YAML
anchor — two endpoint pools, since the base URL differs.

---

Previous: [§5b — Timeouts, throttle visibility & cross-process state](timeouts-and-shared-state.md) · Next: [§5b — Extended thinking, provider selection & multi-user](thinking-and-selection.md)
