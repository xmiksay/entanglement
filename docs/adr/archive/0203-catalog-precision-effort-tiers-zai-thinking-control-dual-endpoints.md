# 0203. Catalog precision: effort tiers, thinking-required models, z.ai thinking control, dual z.ai endpoints

- Status: Accepted
- Date: 2026-09-15
- Issue: #560 (pre-release audit umbrella)
- Relates to: [ADR-0094](0094-reasoning-effort-and-per-profile-generation-persistence.md)
  (`reasoning_effort` as a coarse three-tier knob — extended here to five
  tiers with a per-model clamp), [ADR-0160](0160-extended-thinking-round-trip.md)/[ADR-0191](0191-inline-think-tags-catalog-format.md)
  (the existing per-model thinking knobs `thinking_style`/`thinking_format`/`replay_thinking`
  this ADR sits beside), [ADR-0140](0140-per-model-concurrency-cap-layered-on-endpoint-cap.md)
  (the "only set what the provider documents" catalog rule, kept),
  [ADR-0050](0050-per-endpoint-connection-pool-retry-rate-limit.md)/[ADR-0156](0156-normalize-and-stabilize-the-endpoint-pool-key.md)
  (why two z.ai entries with one key are still two endpoint pools).

## Context

The embedded catalog had drifted from the providers it describes, and a
new user hit that drift first:

- Anthropic listed a retired id (`claude-3-5-sonnet-20241022`, gone since
  2025-10-28), priced Sonnet 5 at Sonnet 4.5's rate, defaulted to a
  previous-generation model, and lacked six current ids (Fable 5.1 / 5, Opus
  4.7 / 4.6, Sonnet 4.6, Haiku 4.5). The models that reject sampling
  parameters were not flagged `supports_temperature: false`, so a live
  `/set temperature` produced a 400. No entry named the newer web-search
  server-tool type.
- z.ai lacked GLM-5.3 / GLM-5.3-Flash, still listed a withdrawn id, and its
  only endpoint was the Coding Plan URL — a pay-as-you-go key fails there
  and nothing told the user to set `ZAI_API_BASE`.
- `ReasoningEffort` stopped at `high`. Anthropic's adaptive models take
  `xhigh`/`max`; GLM-5.3 takes *only* `low|high|max` and cannot disable
  thinking, so `medium` is a 400 there.
- The OpenAI-compat wire sent no `thinking` object, so z.ai applied its own
  default — thinking on at `max` — while the catalog header promised
  thinking "stays off until you opt in". Every GLM-5.x turn was paying for
  maximum reasoning silently.

## Decision

All of it is **catalog data**, per #118's "catalog data, not hardcode":

1. **Five effort tiers** (`low|medium|high|xhigh|max`, ordered). A model may
   declare `effort_tiers:` — the list it accepts. Absent = pass through;
   a list = clamp to the nearest listed tier (ties resolve downward); an
   empty list = the model takes no effort field at all (thinking on/off
   only). Wires with no effort ladder (Anthropic's fixed-budget shape,
   Gemini) map the two top tiers onto the deepest budget they know.
2. **`thinking_required: true`** marks a model that cannot run with thinking
   off (GLM-5.3 family, Fable). With no effort resolved, the lowest
   supported tier is sent so the request is valid; nothing ever sends a
   disable shape to such a model.
3. **`thinking_control: zai`** on a provider makes the OpenAI-compat wire
   send z.ai's `thinking: {type: enabled|disabled}` object: `enabled` plus
   the (clamped) `reasoning_effort` when an effort resolves, `disabled` when
   none does and the model allows it. Providers without the flag never see
   the field — OpenAI proper rejects unknown parameters. The embedded z.ai
   entries default every GLM-5.x model to `high`; the 4.x models get
   thinking enabled with no effort field.
4. **Two z.ai entries, one key**: `zai` (Coding Plan endpoint, first in
   catalog order so auto-detect keeps today's behaviour) and `zai_paas`
   (pay-as-you-go). Same `ZAI_API_KEY` and the same model list (a YAML
   anchor); two endpoint pools because the base URL differs (ADR-0050).
   The bundled MCP servers stay declared on `zai` only: the runtime keys
   bundled servers by server name across every catalog entry, so a second
   copy would shadow the first, and since both entries share the key the
   servers are available to either one anyway. Env overrides follow the
   uppercased entry name (`ZAI_PAAS_MODEL`, …).
5. **Catalog refresh**: Anthropic default `claude-sonnet-5`; every current
   id with its real pricing, context window, thinking shape, replay flag,
   `supports_temperature`, and `web_search_tool_version: web_search_20260209`
   where the model needs it; retired ids removed. z.ai default `glm-5.3`.
6. **`supports_temperature` is enforced at the wire**, not just when the
   catalog default is applied: the Anthropic client drops a live-set
   temperature for a model that rejects sampling parameters.

## Consequences

- A new user with either z.ai key type, or an Anthropic key, gets a working
  default model and a correct bill. Existing users on `zai` see the default
  move to `glm-5.3` (same price) and thinking become explicit at `high`
  instead of the provider's silent `max`; anyone who wants the old behaviour
  sets `default_reasoning_effort: max` (or `/set effort max`) in their
  `providers.yml`.
- A tier a model does not accept is clamped, never rejected: `/set effort
  medium` on GLM-5.3 sends `low` (a tie between `low` and `high` rounds
  down, so a model lacking the asked-for depth never silently costs more);
  `xhigh` on Opus 4.6 sends `high`.
- The user-facing effort vocabulary grows to five words in `/set effort`,
  the per-profile generation file and the TUI presets.
- The knobs travel with the per-request `ThinkingSpec` on the OpenAI-compat
  and Responses wires (resolved against the request's actual model, #550),
  not `GenerationParams`, so persisted per-profile generation files are
  untouched. The Anthropic client binds them at construction, like
  `thinking_style` before it: a `model:`-only profile pin that sends a
  different model id through that client uses the default model's tiers and
  temperature flag. Accepted for now; a per-request resolver on that wire is
  the follow-up if it bites.
- The two z.ai entries share one model list through a YAML anchor, which
  survives the catalog's pre-deserialization merge; a user override on one
  entry does not leak into the other, pinned by test.
