# 0078. Native Gemini wire + opaque `provider_meta` on `ToolCall`

- Status: Accepted
- Date: 2026-07-15
- Adds a third provider wire (`Wire::Gemini`) beside the OpenAI-compat and
  Anthropic clients ([0007](0007-streaming-llm-and-provider-crate.md)), and a
  generic opaque metadata slot on the LLM ABI. Reuses the per-endpoint pool/retry
  seam of [0050](0050-per-endpoint-connection-pool-retry-rate-limit.md) and the
  serde back-compat shim pattern of [0064](0064-message-content-blocks.md). Part
  of #307. Issue #309.

## Context

The provider catalog covers the OpenAI-compatible wire (z.ai/OpenAI/Ollama) and
native Anthropic SSE, but no Google Gemini. Gemini's OpenAI-compat endpoint is
**not** sufficient: a Gemini 2.5 thinking model attaches a `thoughtSignature` — an
opaque, provider-private token — to each function call, and the next turn must
echo that signature back **verbatim** or the API rejects the request (4xx) on the
replayed history. The compat surface does not expose the signature at all, so a
multi-turn tool-using conversation with a thinking model is impossible through it.

The signature is not the only case of its kind: reasoning/thinking tokens and
other wire-private state are the same shape — data the provider produces, the
engine must persist and hand back untouched, and no consumer should ever
interpret. entanglement's `Message`/`ToolCall` had nowhere to put it, which is a
"our types can't represent provider X" class of blocker.

## Decision

**Two changes, one generic and one wire-specific.**

1. **Opaque `provider_meta` on the ABI.** `ToolCall` gains
   `provider_meta: Option<serde_json::Value>` — provider-private state that
   round-trips verbatim through history persistence + replay, never inspected by
   core. It is persisted with the [0064](0064-message-content-blocks.md) shim
   (`#[serde(default, skip_serializing_if = "Option::is_none")]`): old logs with no
   field deserialize to `None` and replay unchanged; a `None` serializes away, so a
   turn that carries nothing is byte-identical to a pre-#309 log. Because
   `serde_json::Value` is not `Eq`, `ToolCall`/`LlmEvent`/`LlmResponse` drop the
   `Eq` derive (they stay `PartialEq`) — none were used as a `HashMap`/`HashSet`
   key or embedded in an `Eq`-deriving type, so the change is contained.

2. **`GeminiLlm`, a native streaming client.** Implements `Llm::stream` against
   `:streamGenerateContent?alt=sse`: candidate parts map to
   `Text`/`Reasoning`(`thought:true`)/`ToolCall`, `usageMetadata` → `Usage` (the
   cached read split out of `promptTokenCount` so pricing doesn't double-count),
   and `finishReason` → `StopReason` (`STOP` upgraded to `ToolUse` when a call was
   emitted, since Gemini has no distinct tool-use reason). The `thoughtSignature`
   on a `functionCall` is stashed into `provider_meta` on the way out and restored
   when rebuilding `contents` from history. A `gemini` provider (`wire: gemini`,
   `key_env: GEMINI_API_KEY`) is added to the embedded catalog; the runtime's
   startup + live-switch resolver gain a `Wire::Gemini` arm. It reuses the shared
   `HttpClient` (per-endpoint pool/retry/rate-limit).

Gemini matches a `functionResponse` back to its call by **name**, so the
`ToolCall.id` is set to the call name (the runtime echoes `tool_call_id` as that
name). The site's working non-streaming adapter (`site/src/ai/llm/gemini.rs`,
incl. the schema sanitizer and signature handling) was the port reference.

## Consequences

- Multi-turn tool use with a Gemini 2.5 thinking model works: signatures survive
  persistence + replay, so no 4xx on replayed history.
- `provider_meta` is a generic slot future wires reuse for reasoning/thinking
  state — the "can't represent provider X" blocker is gone without another ABI
  change per provider.
- Dropping `Eq` is a minor API break for any external consumer that relied on it;
  in-repo nothing did.
- **Rejected — Gemini via the OpenAI-compat endpoint** (`wire: openai` +
  `base_url`, zero code): can't round-trip `thoughtSignature`, so it silently
  breaks thinking-model tool loops. The whole point of #309.
- **Rejected — a Gemini-specific `thought_signature` field** instead of a generic
  `provider_meta`: solves only this vendor and re-opens the blocker for the next.

## Same-name parallel calls

Because `id == name`, two parallel calls to the *same* function in one turn share
an id. This matches the ported production adapter and Gemini's own name-keyed
`functionResponse` model; the runtime's per-session in-flight dedupe is by
`request_id`, so the collision is a known, accepted edge (rare in practice), not a
silent correctness bug. A future disambiguation (name + index) would need the
`functionResponse` name mapping to travel separately from the id.
