# 0032. YAML provider/model catalog: embedded defaults + user override

- Status: Accepted
- Date: 2026-07-09

## Context

The provider/model catalog was hardcoded in three places that had to be kept in
sync by hand: `entanglement-provider`'s `zai_models()`/`openai_models()`/… +
`models_for()`, the runtime's four near-identical `*_config()` fns (each
re-deriving key/model/base env and context window), and a third duplicated model
list in the TUI picker. `ModelInfo` carried only `{ id, display_name,
context_window }` — no capability flags, no pricing. Adding a provider (a proxy,
a local vLLM, a new vendor) or a model, or tweaking pricing, meant a code change
in every copy.

We want: the catalog as **data**, a user override without recompiling, richer
per-model metadata (thinking/temperature capability + pricing), and — the real
unlock — user-defined providers for any OpenAI-compatible endpoint.

## Decision

### 1. YAML, embedded default + deep-merged user override

One file shape (`entanglement-provider::catalog`) for both. `Catalog::builtin()`
parses an embedded `include_str!("defaults.yml")`; `Catalog::load()` resolves a
user file at `${config_dir}/entanglement/providers.yml` (path override:
`ENTANGLEMENT_PROVIDERS_FILE`) and, if present, deep-merges it **over** the
builtin. A malformed user file is a **loud error** (`.context()` with the path),
never a silent fallback. The embedded parse uses `.expect(...)` — provably
unreachable, guarded by a unit test.

### 2. Merge at the `serde_yaml::Value` level, before deserializing

The merge operates on parsed `Value`s, not on typed structs:

1. `providers` sequences merge **by `name`**, `models` sequences **by `id`** —
   matching entries merge recursively (kept in the base's position), user-only
   entries append (defaults first; order is the auto-detect priority).
2. Mappings merge key-wise recursively; scalars and other sequences are replaced
   by the user value.
3. The merged `Value` deserializes into `Catalog` with `deny_unknown_fields`, so
   a typo in the user file is rejected loudly instead of silently ignored.

Because the merge is field-level, overriding one price (`pricing: { input: 0.5 }`)
leaves every sibling field — including `bool`s that default via `#[serde(default)]`
— untouched. That property is the whole reason for merging pre-deserialize.

### 3. `wire` tag decouples client from vendor

Each `ProviderEntry` has `wire: openai | anthropic` (`#[serde(default)]` →
`Openai`). This is what makes user-defined providers work with **zero code
change**: any OpenAI-compatible endpoint is `wire: openai` + `base_url` +
`key_env`; the runtime dispatches on `wire`, not on a hardcoded provider name.

### 4. Richer `ModelEntry`

`{ id, display_name?, context_window?, max_output_tokens?, supports_thinking
(default false), supports_temperature (default true), default_temperature?,
pricing? }`, with `ModelPricing { input?, output?, cached_input?, cache_write? }`
in USD per million tokens — every field optional (defaults fill in only
well-documented public rates; unknowns and local Ollama carry none).

### 5. Runtime rewiring

`select_provider` loads the catalog once and resolves `ENTANGLEMENT_PROVIDER`
against it (custom names included; `echo` stays a built-in stub); unset →
auto-detect over catalog order, first provider with a non-empty `key_env`
(keyless Ollama skipped). The four `*_config()` clones collapse to two
wire-generic builders (`openai_wire_config` / `anthropic_wire_config`) reading
key from `entry.key_env`, model from `{NAME}_MODEL` else `entry.default_model`,
base from `{NAME}_API_BASE` else `{NAME}_BASE` else `entry.base_url`. Precedence
overall: **env > user YAML > embedded defaults**. The TUI picker + context bar
read the same catalog (`model_by_id` searches all providers).

### 6. Layering

The catalog lives in `entanglement-provider`, which the runtime pulls only behind
its `cli` feature — the lean `--no-default-features` build is untouched
(`make check-lean` stays green) and `entanglement-core` is untouched
(`make tree`). New deps `serde_yaml` + `dirs` land in `entanglement-provider`
only (`dirs` hoisted to a workspace dep, already used by the runtime).

## Consequences

- **(+)** Adding/adjusting a provider, a model, or pricing is a data edit; a
  user adds a provider without forking. The three hand-synced copies collapse to
  one source.
- **(+)** Field-level override + `deny_unknown_fields`: minimal user files, loud
  typos.
- **(+)** Capability + pricing metadata now has a home for downstream cost/UX
  features.
- **(−)** Two new deps in `entanglement-provider` (`serde_yaml`, `dirs`) and a
  hand-rolled `Value` merge to maintain. Bounded: ~two keyed-sequence rules.
- **(−)** Pricing in the embedded defaults can drift from real vendor rates; it's
  best-effort and user-overridable, not authoritative.

## Alternatives considered

- **Parallel "patch" structs merged after deserialize.** Every field becomes
  `Option`, and a whole-struct replace resets `bool`s (e.g. `supports_temperature`)
  to their defaults unless each is threaded by hand. Rejected: the `Value`-level
  merge gives field-level override for free with no second type hierarchy.
- **Keep the catalog in code, just add metadata.** Rejected: leaves the
  three-copy sync burden and blocks user-defined providers entirely — the main
  point of the feature.
- **A full config crate (`config`/`figment`) with layered sources.** Rejected as
  over-scoped: one embedded default + one optional override file is the whole
  requirement; a bespoke two-rule merge is smaller than adopting a framework.
- **TOML/JSON instead of YAML.** Rejected: YAML's nested-with-comments ergonomics
  suit a hand-edited catalog with optional pricing blocks, and `serde_yaml` gives
  the `Value`-level merge + `deny_unknown_fields` cleanly.
- **Silent fallback to defaults on a bad user file.** Rejected: a typo'd override
  that silently does nothing is a worse failure than a loud parse error naming
  the path.

[0007]: 0007-streaming-llm-and-provider-crate.md
[0025]: 0025-runtime-cargo-feature-gates.md
