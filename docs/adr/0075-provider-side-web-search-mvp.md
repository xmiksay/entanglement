# 0075. Provider-side web search MVP — opt-in config, results on the reasoning channel

- Status: Accepted
- Date: 2026-07-15
- Builds on the local trust boundary of [0047](0047-local-trust-boundary.md)
  (the config file is trusted), the multimodal message content blocks of
  [0064](0064-message-content-blocks.md), the realtime model/provider switch of
  [0063](0063-realtime-model-provider-switch.md), and the frozen wire of
  [0069](0069-trusted-untrusted-wire-frame-split.md) /
  [0072](0072-protocol-warts-settled-before-serve.md). Issue #305 (part of #302).

## Context

Both of entanglement's first-class providers can execute a web search
**mid-turn, server-side** — the provider runs the search and cites the results
with **no client tool round-trip**:

- **z.ai** (OpenAI-compat wire): a `{"type":"web_search","web_search":{…}}` entry
  in the request's `tools` array.
- **Anthropic**: a `{"type":"web_search_20250305","name":"web_search"}` server
  tool whose activity streams back as `server_tool_use` and
  `web_search_tool_result` content blocks.

Today neither is requestable, and the Anthropic SSE parser silently drops those
block types (`content_block_start` only handled `tool_use`; a `server_tool_use`
or `web_search_tool_result` block fell through and its results vanished
invisibly).

Wiring this as a **first-class client tool** would mean new protocol events
(`WebSearch*` on the wire) and a core change — but the wire is deliberately
frozen ([0069](0069-trusted-untrusted-wire-frame-split.md) /
[0072](0072-protocol-warts-settled-before-serve.md)), and a *server-executed*
search is not a client tool round-trip at all (no `ToolExec`, no permission
prompt, no `ToolResult`). Forcing it through the tool protocol would be a
category error.

## Decision

**Web search is client-construction-time config, with zero core/protocol
change.** Core never sees it: `LlmRequest` / `ToolSpec` / `LlmEvent` /
`OutEvent` are untouched.

- **Config type.** `WebSearchConfig { enabled, max_uses, allowed_domains }` lives
  in the leaf provider crate (`entanglement-provider::web_search`),
  `deny_unknown_fields`, every field defaulted. It is re-exported through
  `entanglement-core` so the runtime's lean library (where the config loader
  lives) can name it without turning on the `provider` feature.
- **Config surface.** A `#[serde(default)] web_search:` section on the layered
  user `config.yml` (embedded < user < project, [0047](0047-local-trust-boundary.md)),
  following the `mcp:` pattern. The scaffolded `template.yml` block states
  plainly that **enabling is consent**: the search runs provider-side, *outside*
  the runtime permission ladder — no per-call approval gates it.
- **Threading.** The runtime maps the config to `Option<WebSearchConfig>`
  (`Some` only when `enabled`) and hands it to the client factories
  (`openai_factory` / `anthropic_factory`) and the live-switch model resolver
  ([0063](0063-realtime-model-provider-switch.md)) identically, so startup and a
  mid-session `/model` switch bind web search the same way. Each client stores it
  and, in `build_body`, pushes its wire-specific server-tool entry (riding the
  same `tools` array, so it is requestable even with no function tools present).
- **Results surface on the reasoning channel.** A search is not conversation the
  model authored; its sources stream back as
  [`LlmEvent::Reasoning`](../architecture/provider.md) lines
  (`[web_search] {title} — {url}`), which flow to `OutEvent::ReasoningDelta` and
  are **not** committed into `Message` history. Anthropic `server_tool_use`
  blocks are tracked with an `is_server` flag and surface their query as
  `Reasoning` on stop — **never** a `ToolCall`; `web_search_tool_result` blocks
  (and their error variant) render as `Reasoning`. z.ai's cited answer text
  already flows as `Text`; the `web_search` source array (placement in the
  streaming wire unverified) is parsed **defensively** — worst case is today's
  cited-text-only floor.

## Consequences

- No wire change, no core change, no new permission surface — the whole feature
  is a provider-crate concern plus one config section. `serve` and every head
  render the reasoning lines with no adapter work.
- The permission ladder is bypassed **by design**: the config file is trusted
  ([0047](0047-local-trust-boundary.md)), so opting in *is* the grant. There is no
  per-search approval prompt.
- Search results are ephemeral: they are shown, not persisted. Citations and
  Anthropic's search-result cache pricing are lost across turns.

## Accepted MVP limitations (follow-ups)

- **Not persisted into history.** Search blocks never enter `Message` content, so
  citations and Anthropic search-cache pricing don't survive a turn — a follow-up
  on the [0064](0064-message-content-blocks.md) content-block path.
- **`pause_turn` ends the turn.** A search that trips Anthropic's `pause_turn`
  stop reason finishes the turn rather than continuing — continuation is a
  follow-up.
- **z.ai streaming placement unverified.** The `web_search` array's exact location
  in the streaming chunks isn't confirmed; the parser scans defensively and
  degrades to cited-text-only if it never matches.
- **Anthropic `_20260209` tool version.** The newer server-tool version needs
  4.6+ models; a follow-up gates it behind a `ModelEntry` capability flag rather
  than hardcoding `_20250305`.

## Rejected alternatives

- **First-class `WebSearch` protocol events + a core-driven tool.** Breaks the
  frozen wire ([0069](0069-trusted-untrusted-wire-frame-split.md) /
  [0072](0072-protocol-warts-settled-before-serve.md)) and models a
  server-executed search as a client round-trip it isn't. Deferred until the wire
  is intentionally reopened.
- **A client-side web-search host tool.** Would run in-runtime under the
  permission ladder, but re-implements what the provider already does natively and
  loses provider-side citation. Orthogonal to this feature.
- **On by default.** Web search has cost and privacy implications; it must be an
  explicit opt-in, and the opt-in doubles as the consent that justifies bypassing
  the permission ladder.
