# Tool Search / Deferred Tool Loading — Wire-Level Reference

Research date: 2026-09-14. Sources are official docs unless marked UNVERIFIED /
NOT FOUND. JSON quoted verbatim from source docs where possible.

## Executive summary

| Provider | Feature | Status |
|---|---|---|
| Anthropic Messages API | `tool_search_tool_regex_20251119` / `tool_search_tool_bm25_20251119` + `defer_loading` | **GA**, no beta header. Fully documented, verbatim JSON below (§1). |
| OpenAI Responses API | `tool_search` + `defer_loading` (hosted + client-executed modes) | **GA on `gpt-5.4`+**, no beta header. Documented; some streaming-event field lists unverified (§2). |
| z.ai / Zhipu (GLM) | none on any hosted surface | **NOT FOUND.** OpenAI-compat + Anthropic-compat endpoints exist, no Responses surface; hosted tool-calling contract has no search/defer mechanism. GLM-5.1's open-weight chat template has `defer_loading`/`tool_reference` template plumbing, but that's unverified at the hosted-API level (§3). |
| OpenRouter | `openrouter:tool_search` (native, cross-provider) + Anthropic-native passthrough | **GA**, Responses-style and Messages-style surfaces only (400 on Chat Completions). Two independent modes — see §4. |
| Gemini | none | **NOT FOUND.** Confirmed open feature request (`googleapis/python-genai#2185`), no roadmap commitment (§5). |

---

## 1. Anthropic Messages API — Tool Search

Source: `https://platform.claude.com/docs/en/agents-and-tools/tool-use/tool-search-tool`,
`https://platform.claude.com/docs/en/agents-and-tools/tool-use/tool-reference`,
`https://platform.claude.com/docs/en/agents-and-tools/tool-use/overview`,
`https://platform.claude.com/docs/en/build-with-claude/streaming`.

### 1.1 Status: GA, no beta header

Per the official **Tool reference** table, both tool-search variants have
**Beta header: `None`**:

| Tool | `type` | Execution | Beta header |
|---|---|---|---|
| Tool search tool | `tool_search_tool_regex_20251119` `tool_search_tool_bm25_20251119` | Server | None |

Undated aliases `tool_search_tool_regex` / `tool_search_tool_bm25` are also
accepted and resolve to the latest dated version. `tool_search_tool_regex_20251119`
and `tool_search_tool_bm25_20251119` are described as "Variant, not version" —
two search algorithms released together; neither supersedes the other.

### 1.2 Model compatibility

Supported on: Claude Fable 5.1, Mythos 5.1, Fable 5, Mythos 5, Opus 5, Opus
4.8, Opus 4.7, Opus 4.6, Sonnet 4.6, Opus 4.5 (`claude-opus-4-5-20251101`),
Sonnet 4.5 (`claude-sonnet-4-5-20250929`), Haiku 4.5
(`claude-haiku-4-5-20251001`). **Not** supported on Opus 4.1 and earlier.

### 1.3 Declaring the search tool

```json
{ "type": "tool_search_tool_regex_20251119", "name": "tool_search_tool_regex" }
```
```json
{ "type": "tool_search_tool_bm25_20251119", "name": "tool_search_tool_bm25" }
```

- **Regex variant**: Claude writes Python `re.search()` patterns (case-insensitive
  matching). Max pattern length **200 characters**. Input field name: `pattern`.
- **BM25 variant**: Claude writes natural-language queries. Max query length
  **500 characters**. Input field name: `query`.
- Both search tool names, descriptions, argument names, and argument
  descriptions of every deferred tool.
- Never set `defer_loading: true` on the search tool itself.
- **At least one tool must remain non-deferred** (normally the search tool
  itself) — violating this is a 400.

### 1.4 Marking ordinary tools deferred

```json
{
  "name": "get_weather",
  "description": "Get current weather for a location",
  "input_schema": {
    "type": "object",
    "properties": {
      "location": { "type": "string" },
      "unit": { "type": "string", "enum": ["celsius", "fahrenheit"] }
    },
    "required": ["location"]
  },
  "defer_loading": true
}
```

Key semantics:
- `defer_loading` controls what enters the **context window**, not what you
  send in the request — **every** tool's full definition (including deferred
  ones) must be sent in `tools` on **every** request; the API needs them
  server-side to run the search and expand `tool_reference` blocks.
- Tools without `defer_loading` load into context immediately.
- Tools with `defer_loading: true` load only when Claude discovers them
  through search.
- Recommendation: keep your 3–5 most frequently used tools non-deferred.
- **Limit**: max **10,000 tools** with `defer_loading: true` per request.
- A deferred tool **cannot** also carry `cache_control` — 400 error. Put the
  cache breakpoint on a non-deferred tool instead.
- `defer_loading: true` tools are stripped from the rendered tools section
  **before the cache key is computed** — they never enter the system-prompt
  prefix, so adding/changing deferred tools doesn't invalidate the cache.
  When tool search later expands a `tool_reference`, the full definition is
  spliced inline into the conversation body (not the prefix).
- The strict-mode grammar is built from the **full** toolset up front, so
  `defer_loading` and `strict: true` compose without grammar recompilation.
- Tool-use examples (`input_examples`) on a deferred tool are expanded
  alongside its definition when discovered.

### 1.5 Full request example (quick start)

```bash
curl https://api.anthropic.com/v1/messages \
    -H "x-api-key: $ANTHROPIC_API_KEY" \
    -H "anthropic-version: 2023-06-01" \
    -H "content-type: application/json" \
    -d '{
        "model": "claude-opus-5",
        "max_tokens": 2048,
        "messages": [
            {"role": "user", "content": "What is the weather in San Francisco?"}
        ],
        "tools": [
            {"type": "tool_search_tool_regex_20251119", "name": "tool_search_tool_regex"},
            {
                "name": "get_weather",
                "description": "Get the weather at a specific location",
                "input_schema": {
                    "type": "object",
                    "properties": {
                        "location": {"type": "string"},
                        "unit": {"type": "string", "enum": ["celsius", "fahrenheit"]}
                    },
                    "required": ["location"]
                },
                "defer_loading": true
            },
            {
                "name": "search_files",
                "description": "Search through files in the workspace",
                "input_schema": {
                    "type": "object",
                    "properties": {
                        "query": {"type": "string"},
                        "file_types": {"type": "array", "items": {"type": "string"}}
                    },
                    "required": ["query"]
                },
                "defer_loading": true
            }
        ]
    }'
```

### 1.6 Response format — non-streaming

```json
{
  "role": "assistant",
  "content": [
    { "type": "text", "text": "I'll search for tools to help with the weather information." },
    {
      "type": "server_tool_use",
      "id": "srvtoolu_01ABC123",
      "name": "tool_search_tool_regex",
      "input": { "pattern": "weather", "limit": 10 }
    },
    {
      "type": "tool_search_tool_result",
      "tool_use_id": "srvtoolu_01ABC123",
      "content": {
        "type": "tool_search_tool_search_result",
        "tool_references": [{ "type": "tool_reference", "tool_name": "get_weather" }]
      }
    },
    { "type": "text", "text": "I found a weather tool. Let me get the weather for San Francisco." },
    {
      "type": "tool_use",
      "id": "toolu_01XYZ789",
      "name": "get_weather",
      "input": { "location": "San Francisco", "unit": "fahrenheit" }
    }
  ],
  "stop_reason": "tool_use"
}
```

Block-by-block:
- **`server_tool_use`**: Claude's call to the search tool itself. Runs on
  Anthropic's servers. **Never return a `tool_result` for its `srvtoolu_...`
  ID** — the API rejects the request if you do. `input` holds `pattern`
  (regex variant) or `query` (BM25 variant) plus an optional `limit` (integer
  1–10,000, default **5**) that caps how many matching tools the search
  returns.
- **`tool_search_tool_result`**: nests a `tool_search_tool_search_result`
  object with a `tool_references` array. Keep this block in history verbatim.
  A search matching nothing returns an **empty** `tool_references` array —
  not an error.
- **`tool_reference`**: `{ "type": "tool_reference", "tool_name": "..." }`.
  The API auto-expands these into full tool definitions before Claude sees
  them — **you never expand them yourself**, as long as every referenced
  tool's full definition is present in your `tools` parameter.
- **`tool_use`**: Claude's call to a now-loaded discovered tool. Execute and
  return a standard `tool_result`, same as any tool call.

### 1.7 Continuing the conversation

- On the next request, pass the assistant's content back **unchanged**,
  including the `server_tool_use` and `tool_search_tool_result` blocks.
- Add your `tool_result` for the discovered tool in a user message.
- Send the **same** `tools` array again: search tool + every deferred
  definition (full defs are required on every request regardless of
  discovery state).
- Do **not** send a `tool_result` for the `srvtoolu_...` id.
- The API re-expands `tool_reference` blocks throughout history on every
  request, so Claude can reuse a discovered tool in later turns without
  re-searching.

### 1.8 Client-side / custom tool search implementation

You can implement your **own** search (e.g. embeddings/semantic search) as a
regular custom tool. When Claude calls it, return a **standard `tool_result`**
containing `tool_reference` blocks — **not** the server's
`tool_search_tool_result` shape (that shape is internal-only, for the
built-in variants):

```json
{
  "type": "tool_result",
  "tool_use_id": "toolu_your_tool_id",
  "content": [{ "type": "tool_reference", "tool_name": "discovered_tool_name" }]
}
```

Every referenced tool must have a full definition in the top-level `tools`
parameter, normally with `defer_loading: true`. The API expands the returned
`tool_reference` blocks the same way as with the built-in search tools. This
lets you plug in any retrieval method (embeddings, etc.) while reusing the
same deferred-loading/expansion machinery. Anthropic ships a cookbook recipe
for an embeddings-based implementation ("tool search with embeddings").

### 1.9 Streaming (SSE)

```
event: content_block_start
data: {"type": "content_block_start", "index": 1, "content_block": {"type": "server_tool_use", "id": "srvtoolu_xyz789", "name": "tool_search_tool_regex"}}

event: content_block_delta
data: {"type": "content_block_delta", "index": 1, "delta": {"type": "input_json_delta", "partial_json": "{\"pattern\":\"weather\"}"}}

// Pause while search executes

event: content_block_start
data: {"type": "content_block_start", "index": 2, "content_block": {"type": "tool_search_tool_result", "tool_use_id": "srvtoolu_xyz789", "content": {"type": "tool_search_tool_search_result", "tool_references": [{"type": "tool_reference", "tool_name": "get_weather"}]}}}

// Claude continues with discovered tools
```

Notes:
- `server_tool_use` streams like any `tool_use`-shaped block: a
  `content_block_start` with empty `input: {}`, then `input_json_delta`
  chunks of `partial_json`, then `content_block_stop`.
- `tool_search_tool_result` is **not** streamed incrementally — it arrives
  as a single `content_block_start` carrying the complete `content` object
  (search runs server-side and the whole result lands at once), followed by
  `content_block_stop`.
- General SSE event flow (applies to every Messages API stream, not just
  tool search): `message_start` → repeated (`content_block_start` →
  1+ `content_block_delta` → `content_block_stop`) → 1+ `message_delta` →
  `message_stop`. `ping` events may appear anywhere. `usage` in
  `message_delta` is **cumulative**. See §1.12 below for a general
  tool-use SSE trace.

### 1.10 `tool_choice` interaction

The official Anthropic tool-search doc does **not** explicitly document a
`tool_choice` restriction. **UNVERIFIED against Anthropic's own docs** — but
OpenRouter's docs (which alias straight through to Anthropic's
`tool_search_tool_regex[_20251119]`) state a concrete constraint:

> "`tool_choice` conflicts with `openrouter:tool_search`. Deferred tools are
> revealed through `tool_choice`, so it must be omitted or set to
> `{"type": "allowed_tools", ...}`."

Treat this as strong circumstantial evidence of the same restriction on raw
Anthropic (since OpenRouter's Anthropic-passthrough surface only re-exposes
Anthropic's own semantics for the regex variant), but it is not a verbatim
quote from `platform.claude.com`. Practical implication for a client
implementation: forcing `tool_choice: {"type": "tool", "name": "..."}` or
`{"type": "any"}` while any tool has `defer_loading: true` is a likely
footgun — a forced tool that hasn't been discovered yet can't be called, and
forcing prevents the search step from running at all. Recommend defaulting to
`tool_choice: {"type": "auto"}` (or omitting `tool_choice`) whenever
`defer_loading` is in play, and testing `allowed_tools` scoping separately.

### 1.11 Token accounting for deferred tools

Quoting the official doc's Usage section verbatim:

> "Tool search isn't metered as a separate server tool. The response's
> `usage.server_tool_use` object has no tool search field, and the tool
> definitions that search loads into context count as input tokens like any
> other tool definition."

Combined with §1.4 ("Tools with `defer_loading: true` load only when Claude
discovers them through search" and the tool-reference page's cache-key
language — deferred tools are "stripped from the rendered tools section
before the cache key is computed" and "don't appear in the system-prompt
prefix at all"): **deferred tool definitions do NOT count as input tokens
until Claude discovers and the API expands them.** Only the search tool
itself and any non-deferred tools count against input tokens up front.

### 1.12 Error handling

**400 (blocks the request):**

```json
{
  "type": "error",
  "error": {
    "type": "invalid_request_error",
    "message": "At least one tool must have defer_loading=false. All tools cannot be deferred."
  }
}
```
```json
{
  "type": "error",
  "error": {
    "type": "invalid_request_error",
    "message": "Tool reference 'unknown_tool' not found in available tools"
  }
}
```

**200 with in-band tool-search error** (search itself fails at runtime):

```json
{
  "type": "tool_search_tool_result",
  "tool_use_id": "srvtoolu_01ABC123",
  "content": {
    "type": "tool_search_tool_result_error",
    "error_code": "invalid_tool_input",
    "error_message": "Invalid regular expression pattern: missing ) at position 1"
  }
}
```

`error_code` values: `invalid_tool_input` (malformed regex / over-length
pattern), `unavailable` (timeout / service issue), `too_many_requests`
(rate-limited), `execution_time_exceeded`.

### 1.13 MCP integration

If tools come from MCP servers via the MCP connector, you don't set
`defer_loading` per tool definition. Instead set it once on the
`mcp_toolset` entry's `default_config` for the whole server, or per-tool in
its `configs`.

### 1.14 Client toolsets (computer use / browser use) — defer_loading quirk

`computer_toolset_20260801` / `browser_toolset_20260801` take `defer_loading`
**per member tool inside the entry's `configs` object**, never on the entry
itself (rejected as `invalid_request_error` if set at entry level). Every
*enabled* member must resolve to the **same** `defer_loading` value — the
toolset defers and expands as a single unit. Not directly relevant to a
custom-tool client but worth flagging if the crate ever wraps these toolsets.

### 1.15 Limits summary

| Limit | Value |
|---|---|
| Max deferred tools per request | 10,000 |
| Default search result count | 5 |
| Max `limit` Claude can request | 10,000 |
| Regex pattern max length | 200 chars |
| BM25 query max length | 500 chars |

### 1.16 When to use (Anthropic's own guidance)

Use tool search when: 10+ tools available, tool definitions exceed ~10k
tokens, selection accuracy is dropping, aggregating multiple MCP servers
(200+ tools), or the tool library grows over time. Skip it when: fewer than
10 tools, every tool used every request, or definitions are small (<100
tokens total). Reported effect: a typical 5-server MCP setup (GitHub, Slack,
Sentry, Grafana, Splunk) costs ~55k tokens of tool defs up front; tool search
typically cuts that >85%, loading only the 3–5 tools actually needed.

### 1.17 Platform availability caveats

- Amazon Bedrock: server-side tool search is available **only** through the
  `InvokeModel` API, **not** the Converse API.
- Claude Platform on AWS: works identically to the Claude API (no
  InvokeModel/Converse split — it uses the Anthropic Messages API directly).
- Batch requests: tool search can be included in the Messages Batches API.

---

## 2. OpenAI Responses API — Tool Search

Sources: `developers.openai.com/api/docs/guides/tools-tool-search` (official
OpenAI guide, primary), `learn.microsoft.com/en-us/azure/foundry/openai/how-to/tool-search`
(Azure mirror — used for fuller worked JSON where the OpenAI page's fetched
excerpt was thin; flagged inline), `developers.openai.com/api/docs/guides/function-calling`,
`developers.openai.com/api/docs/guides/migrate-to-responses`,
`developers.openai.com/api/docs/guides/conversation-state`,
`developers.openai.com/api/docs/guides/streaming-responses`.

Note: `platform.openai.com/docs/...` URLs now 301-redirect to
`developers.openai.com/api/docs/...` — treat them as the same canonical
source.

### 2.1 Model support & opt-in

**Only `gpt-5.4` and later** support `tool_search` in the Responses API. No
beta header / opt-in flag documented — it's a plain `tools` array entry,
gated purely by model version (unlike some other OpenAI beta features that
require an `OpenAI-Beta` header, e.g. Assistants API).

### 2.2 Declaring the search tool

Hosted (server-executed) mode — minimal form:

```json
{ "type": "tool_search" }
```

Client-executed mode — you also give it a description/schema for the search
call shape:

```json
{
  "type": "tool_search",
  "execution": "client",
  "description": "Find project tools needed to continue the task.",
  "parameters": {
    "type": "object",
    "properties": { "goal": { "type": "string" } },
    "required": ["goal"],
    "additionalProperties": false
  }
}
```

Omitting `execution` (or hosted mode) needs no such schema — OpenAI performs
the search internally.

### 2.3 Marking a function tool deferred

```json
{
  "type": "function",
  "name": "list_open_orders",
  "description": "List open orders for a customer ID.",
  "defer_loading": true,
  "parameters": {
    "type": "object",
    "properties": { "customer_id": { "type": "string" } },
    "required": ["customer_id"],
    "additionalProperties": false
  }
}
```

You can also defer whole **namespaces** or MCP servers (grouping is
preferred for token efficiency — guidance: "keep each namespace to fewer
than 10 functions"). For a namespace, `defer_loading` applies to the
individual functions **inside** it, not to the namespace object itself:

```json
{
  "tools": [
    {
      "type": "namespace",
      "name": "crm",
      "description": "CRM tools for customer lookup and order management.",
      "tools": [
        {
          "type": "function",
          "name": "list_open_orders",
          "description": "List open orders for a customer ID.",
          "defer_loading": true,
          "parameters": {
            "type": "object",
            "properties": { "customer_id": { "type": "string" } },
            "required": ["customer_id"],
            "additionalProperties": false
          }
        }
      ]
    },
    { "type": "tool_search" }
  ]
}
```

### 2.4 Hosted mode — `tool_search_call` / `tool_search_output` output items

In hosted mode (`execution: "server"`), the API performs the search itself
and emits **two** output items before the resulting `function_call`:

```json
[
  {
    "type": "tool_search_call",
    "execution": "server",
    "call_id": null,
    "status": "completed",
    "arguments": { "paths": ["crm"] }
  },
  {
    "type": "tool_search_output",
    "execution": "server",
    "call_id": null,
    "status": "completed",
    "tools": [
      {
        "type": "namespace",
        "name": "crm",
        "description": "CRM tools for customer lookup and order management.",
        "tools": [
          {
            "type": "function",
            "name": "list_open_orders",
            "description": "List open orders for a customer ID.",
            "defer_loading": true,
            "parameters": {
              "type": "object",
              "properties": { "customer_id": { "type": "string" } },
              "required": ["customer_id"],
              "additionalProperties": false
            }
          }
        ]
      }
    ]
  },
  {
    "type": "function_call",
    "name": "list_open_orders",
    "namespace": "crm",
    "call_id": "call_abc123",
    "arguments": "{\"customer_id\":\"CUST-12345\"}"
  }
]
```

In hosted mode, `tool_search_call.arguments` holds a `paths` array (the
namespace/server paths searched), not free text, and `call_id` is always
`null`.

### 2.5 Client-executed mode — the round trip

Turn 1: the model **stops** after emitting only the search call (no
auto-continuation):

```json
[
  {
    "type": "tool_search_call",
    "execution": "client",
    "call_id": "call_abc123",
    "status": "completed",
    "arguments": { "goal": "Find the shipping ETA tool for order_42." }
  }
]
```

Your app performs the lookup, then sends back a `tool_search_output`
**input item**, echoing the same `call_id`:

```json
[
  {
    "type": "tool_search_output",
    "execution": "client",
    "call_id": "call_abc123",
    "status": "completed",
    "tools": [
      {
        "type": "function",
        "name": "get_shipping_eta",
        "description": "Look up shipping ETA details for an order.",
        "defer_loading": true,
        "parameters": {
          "type": "object",
          "properties": { "order_id": { "type": "string" } },
          "required": ["order_id"],
          "additionalProperties": false
        }
      }
    ]
  }
]
```

Next turn — the loaded tool is now directly callable:

```json
[
  {
    "type": "function_call",
    "name": "get_shipping_eta",
    "namespace": "get_shipping_eta",
    "call_id": "call_xyz456",
    "arguments": "{\"order_id\":\"order_42\"}"
  }
]
```

Key semantics:
- **Hosted:** `execution: "server"`, `call_id` always `null`.
- **Client:** `execution: "client"`, `call_id` is a real string that must be
  echoed back on the `tool_search_output` you send.
- `tool_search_output.tools` is exactly the set the model can call going
  forward; tools not in that array stay unavailable.
- Once loaded via client mode, the model can call the tool in **later
  turns without re-loading it** — it stays "known" once returned.
- Both modes append newly-discovered tools **at the end of the context
  window** specifically to preserve prompt caching; changing the loaded
  tool set busts the cache from that point forward (same caching rationale
  as Anthropic's approach, different mechanism).
- An `additional_tools` input item (`role: "developer"` + `tools: [...]`)
  for injecting tools at a specific point in history outside the search
  flow was seen only on the Azure mirror doc — **UNVERIFIED** as an
  OpenAI-native item vs. an Azure-specific addition.

### 2.6 General Responses API surface (from-scratch client notes)

#### `input` item shapes — multi-turn tool exchange

Turn 1 request:

```json
[{ "role": "user", "content": "What is the weather in Paris?" }]
```

Model's output includes a `function_call` item — note the split `id`
(the item's own id) vs. `call_id` (the correlation id you echo back):

```json
{
  "id": "fc_12345xyz",
  "call_id": "call_12345xyz",
  "type": "function_call",
  "name": "get_weather",
  "arguments": "{\"location\":\"Paris, France\"}"
}
```

You append the `function_call` item **and** your `function_call_output`
before re-sending:

```json
[
  { "role": "user", "content": "What is the weather in Paris?" },
  {
    "type": "function_call",
    "call_id": "call_weather",
    "name": "get_weather",
    "arguments": "{\"location\":\"Paris\"}"
  },
  {
    "type": "function_call_output",
    "call_id": "call_weather",
    "output": "{\"city\":\"Paris\",\"temperature_c\":18}"
  }
]
```

`function_call_output` fields: `type`, `call_id`, `output` (a string — JSON
or plain text). Multiple parallel `function_call` items in one turn are
each executed and answered with a batch of `function_call_output` items
before the next request.

This is a flat, typed **item array** (`input`/`output`), unlike Chat
Completions' single `messages` array mixing role+content+tool_calls into
one object shape. Responses also has a distinct `reasoning` item type (own
`id`) with no Chat Completions equivalent:

```json
{
  "id": "resp_68af4030592c81938ec0a5fbab4a3e9f05438e46b5f69a3b",
  "object": "response",
  "created_at": 1756315696,
  "model": "gpt-5.5",
  "output": [
    { "id": "rs_68af4030baa48193b0b43b4c2a176a1a05438e46b5f69a3b", "type": "reasoning", "content": [], "summary": [] },
    {
      "id": "msg_68af40337e58819392e935fb404414d005438e46b5f69a3b",
      "type": "message",
      "status": "completed",
      "content": [{ "type": "output_text", "annotations": [], "logprobs": [], "text": "..." }],
      "role": "assistant"
    }
  ]
}
```

#### `tools` array format — flat, not nested

Function tools are **flat**: `type`, `name`, `description`, `parameters`
(and optional `strict`) sit directly on the tool object — unlike Chat
Completions' `{"type": "function", "function": {"name": ..., "parameters": ...}}`
nesting:

```json
{
  "type": "function",
  "name": "get_weather",
  "description": "Retrieves current weather for the given location.",
  "parameters": {
    "type": "object",
    "properties": {
      "location": { "type": "string", "description": "City and country e.g. Bogotá, Colombia" },
      "units": { "type": ["string", "null"], "enum": ["celsius", "fahrenheit"], "description": "Units the temperature will be returned in." }
    },
    "required": ["location", "units"],
    "additionalProperties": false
  },
  "strict": true
}
```

Hosted tools (e.g. web search) are declared the same flat way with just a
`type`, e.g. `{"type": "web_search"}`.

#### Streaming event names (SSE `type` field)

Confirmed via the streaming guide's typed event union:

`response.created`, `response.in_progress`, `response.completed`,
`response.failed`, `response.output_item.added`, `response.output_item.done`,
`response.content_part.added`, `response.content_part.done`,
`response.output_text.delta`, `response.output_text.annotation_added`,
`response.text.done`, `response.refusal.delta`, `response.refusal.done`,
`response.function_call_arguments.delta`, `response.function_call_arguments.done`,
`response.file_search_call.in_progress`, `response.file_search_call.searching`,
`response.file_search_call.completed`, `response.code_interpreter.in_progress`,
`response.code_interpreter_call.code.delta`, `response.code_interpreter_call.code.done`,
`response.code_interpreter_call.interpreting`, `response.code_interpreter_call.completed`,
`error`.

Confirmed JSON (from official reference snippets):

```json
{
  "type": "response.function_call_arguments.delta",
  "item_id": "item-abc",
  "output_index": 0,
  "delta": "{ \"arg\":",
  "sequence_number": 1
}
```

**UNVERIFIED** (seen only via secondary search snippets, not a clean page
fetch — event name itself is confirmed by the guide's union type, exact
field list is not):

```json
{
  "type": "response.output_item.added",
  "output_index": 0,
  "item": { "id": "msg_123", "status": "in_progress", "type": "message", "role": "assistant", "content": [] },
  "sequence_number": 1
}
```

`response.output_text.delta`'s exact field list is **UNVERIFIED** (its
reference sub-page returned HTTP 403) — expect `type`, `item_id`,
`output_index`, `content_index`, `delta`, `sequence_number` by analogy with
`function_call_arguments.delta`, but confirm before coding the parser.

**No `response.tool_search_call.*` streaming delta events were found
documented anywhere** (no analog to the `file_search_call.in_progress` /
`.completed` pattern for tool search specifically) — **UNVERIFIED** whether
tool_search calls stream incrementally at all; may need a direct live check.

#### `previous_response_id` vs. stateless `input` replay

- **Stateful (`previous_response_id`)**: pass `"previous_response_id": "<prior response.id>"` plus just the new turn's `input`; OpenAI reconstructs
  context server-side. **Gotcha**: it does **not** carry over the prior
  response's top-level `instructions` — resend those explicitly every call
  if they must persist.
- **Stateless**: reconstruct and resend the entire item history yourself in
  `input` each call — full control over exactly what context is included
  (drop/edit/redact prior turns).
- **Mixing**: examples treat these as mutually exclusive per request.
  **UNVERIFIED** whether combining both in one call is rejected outright or
  simply redundant — no explicit statement found either way; treat "don't
  combine them" as the safe working assumption.
- **`store` parameter**: defaults to `true`. `store: true` persists the
  response server-side for 30 days (needed for `previous_response_id`
  chaining / later retrieval by id). `store: false` skips persistence.
  Responses attached to a `conversation` object bypass the 30-day TTL and
  persist indefinitely.
- **Billing**: with `previous_response_id` chaining, "all previous input
  tokens for responses in the chain are billed as input tokens" each turn —
  no token cost savings vs. resending the array yourself; the benefit is
  payload/convenience, not cost, unless combined with prompt caching.

### 2.7 Recommended follow-ups before coding against this (per research agent)

1. Directly verify the full field list for `response.output_text.delta` and
   confirm the `response.output_item.added` shape above — both came from
   indirect/summarized fetches.
2. Confirm whether any `tool_search_call` streaming delta events exist.
3. Confirm explicitly whether the API rejects or ignores
   `previous_response_id` + a non-empty full `input` history sent together.
4. A community report flags a possible edge case: combining `tool_search` +
   `web_search` + a deferred function tool named `web` reportedly triggers a
   500 (OpenAI Developer Community thread, anecdotal/unverified — worth a
   defensive test if the crate ever combines tool search with hosted web
   search on OpenAI).

---

## 3. z.ai (Zhipu) — API surfaces & tool search equivalent

Sources: `docs.z.ai/api-reference/introduction`, `docs.z.ai/guides/overview/quick-start`,
`docs.z.ai/scenario-example/develop-tools/claude`, `docs.z.ai/devpack/tool/zcode`,
`docs.z.ai/devpack/tool/others`, `docs.z.ai/guides/capabilities/function-calling`,
`docs.z.ai/api-reference/llm/chat-completion`, plus HuggingFace GLM-5.1 model-card
discussions (flagged separately as non-z.ai-official).

### 3.1 API surfaces

| Surface | Base URL | Notes |
|---|---|---|
| OpenAI-compatible Chat Completions (general) | `https://api.z.ai/api/paas/v4/chat/completions` (base `https://api.z.ai/api/paas/v4/`) | Drop-in for the OpenAI SDK via `base_url` swap. |
| OpenAI-compatible Chat Completions (GLM Coding Plan) | `https://api.z.ai/api/coding/paas/v4` | Also chat/completions-shaped, not Responses-shaped; used by ZCode and other coding-tool integrations. |
| Anthropic-compatible `/v1/messages`-style endpoint | `https://api.z.ai/api/anthropic` | Documented for Claude Code / Goose / Factory Droid / ZCode integration via `ANTHROPIC_BASE_URL=https://api.z.ai/api/anthropic` + `ANTHROPIC_AUTH_TOKEN=<zai key>`. Meant as a swap-in for Anthropic's own Messages API wire format. Model aliasing via `ANTHROPIC_DEFAULT_OPUS_MODEL` / `ANTHROPIC_DEFAULT_SONNET_MODEL` / `ANTHROPIC_DEFAULT_HAIKU_MODEL` → GLM model names (the exact roster drifts; one docs variant shows only GLM-5.3/GLM-5.3-Flash reachable here). |
| Responses-API-style endpoint | — | **NOT FOUND IN DOCS.** No `/responses` path anywhere in `docs.z.ai/api-reference` or `docs.z.ai/llms.txt`. Third-party integration notes (e.g. OpenAI Codex config guidance) explicitly set `wire_api = "chat"` for z.ai specifically because it doesn't serve the Responses shape — treat "no Responses surface" as well-supported by omission + explicit third-party workaround, though no z.ai page states it negatively in so many words. |

### 3.2 GLM tool/function calling — no documented tool-search or deferred-loading on the hosted API

The **OpenAI-compatible endpoint's** documented tool contract (`docs.z.ai/guides/capabilities/function-calling`, `docs.z.ai/api-reference/llm/chat-completion`) is a conventional, minimal OpenAI-shaped contract:

- `tools`: "A list of tools the model may call. Currently, only functions are supported as a tool... A max of **128 functions** are supported." (Text requests: `FunctionToolSchema`, `RetrievalToolSchema`, `WebSearchToolSchema`; vision requests: `FunctionToolSchema` only.)
- `tool_choice`: "Used to control how the model selects which function to call... The default value is auto, and **only auto is supported**." (No `none`/`required`/named-tool forcing documented.)
- `tool_stream` (bool, default `false`): streams function-call deltas — "Only supported by the GLM-5.3, GLM-5.2, GLM-5.1, GLM-5, GLM-4.7, and GLM-4.6 series."

**No `parallel_tool_calls` field, no tool-search mechanism, and no large-tool-count guidance appear anywhere in this schema.** (Third-party blog claims about parallel tool calls exist but are UNVERIFIED against the primary API reference, which has no such field.)

### 3.3 Does the Anthropic-compatible endpoint accept `defer_loading` / tool-search server tools?

**NOT FOUND IN DOCS — genuinely unverifiable from documentation alone.**

- `docs.z.ai/scenario-example/develop-tools/claude` (the primary "use Claude Code with z.ai" page) documents only base URL, auth token, and model-alias env vars plus MCP add-ons. **No mention** of `defer_loading`, `tool_search_tool_regex_20251119`, `tool_search_tool_bm25_20251119`, or any field-level Anthropic-compatibility statement.
- Unlike the OpenAI-compatible endpoint (which has a full field-by-field API reference page), there is **no equivalent schema reference page** for `/api/anthropic` — a direct guess at `docs.z.ai/api-reference/anthropic/chat-completion` returned HTTP 404.
- Whether the endpoint silently ignores unknown Anthropic fields or errors on them is **UNVERIFIED** — no documentation states either behavior, and this doc doesn't test live requests.

**Suggestive but non-authoritative evidence at the model-weights layer:** HuggingFace discussions on the open-weight GLM-5.1 release show its **chat template** has explicit `defer_loading`/`tool_reference` plumbing modeled on Anthropic's convention:
- `zai-org/GLM-5.1-FP8` HF discussion #3 — maintainer comment: "the new branch is too broad — it also captures `tool_reference` content (used by our `defer_loading` feature), which causes that path to silently produce an empty `<tool_response></tool_response>`."
- Unsloth's GLM-5.1 "How to Run Locally" doc, "Chat Template Update" section: "Supports Claude's search tool. Tools with `defer_loading=True` are omitted from the system prompt and shown in tool results instead."

This confirms the **model/template** understands Claude-style deferred loading well enough to render prompts correctly — but this is model-template behavior observed by third parties self-hosting the open weights (vLLM/SGLang), **not** a documented contract of z.ai's *hosted* API. Do not assume the hosted `/api/anthropic` endpoint forwards or honors `defer_loading` without a live test against it; this is inference, not a documented fact.

### 3.4 Large tool-count guidance

**NOT FOUND IN DOCS.** No page in `docs.z.ai/guides/capabilities/function-calling`, `docs.z.ai/api-reference/llm/chat-completion`, or `docs.z.ai/devpack/tool/others` gives guidance on handling many tools in one request. The only hard number documented is the 128-function cap on `tools` (§3.2); no guidance on approaching it.

### 3.5 Bottom line for entanglement-provider

z.ai has no documented tool-search or deferred-loading feature on any hosted
surface. If `defer_loading` is ever sent to `api.z.ai/api/anthropic`, its
behavior (pass-through honored / silently ignored / 400) is **undocumented
and untested** — treat as a live-request question, not a docs question, and
re-check `docs.z.ai` periodically since the Anthropic-compat endpoint is the
newest/least-documented of the three surfaces.

---

## 4. OpenRouter — `openrouter:tool_search`

Source: `https://openrouter.ai/docs/guides/features/server-tools/tool-search`,
cross-checked against Anthropic's and OpenAI's own tool-search docs.

### 4.1 Surface restriction

> "Tool search is available through the Responses API and the Messages API.
> Requesting it on the Chat Completions API returns a `400` error."

So OpenRouter's tool search does **not** work on its Chat-Completions-style
surface at all — only Responses-style and Messages-style requests.

### 4.2 Request shape — two independent modes

**Mode 1: OpenRouter-managed search/deferral** — trigger:
`{"type": "openrouter:tool_search"}` present in `tools`. Works on **any**
model/provider OpenRouter proxies.

Responses-style surface (`type` aliases accepted: `openrouter:tool_search`
or `tool_search`):

```json
{
  "model": "openai/gpt-5.2",
  "messages": [{ "role": "user", "content": "What's the weather in Tokyo?" }],
  "tools": [
    { "type": "openrouter:tool_search" },
    {
      "type": "function",
      "name": "get_weather",
      "description": "Get the current weather for a city.",
      "parameters": {
        "type": "object",
        "properties": { "city": { "type": "string" } },
        "required": ["city"]
      },
      "defer_loading": true
    }
  ]
}
```

Messages-style surface (`type` aliases accepted: `openrouter:tool_search`,
`tool_search_tool_regex`, or `tool_search_tool_regex_20251119`) — same
`defer_loading` placement, mirroring Anthropic's own field layout.

Search semantics under this mode: regex, case-insensitive, against tool
name/description/parameter names/parameter descriptions, max pattern length
200 chars (mirrors Anthropic's native regex variant limits). **BM25 is not
implemented by OpenRouter** — "only regex matching is implemented" — even
though Anthropic itself offers `tool_search_tool_bm25_20251119` natively.
The search tool itself can never carry `defer_loading: true`; at least one
callable/search tool must stay non-deferred.

**Mode 2: provider-native passthrough** — trigger: Anthropic-native
`tool_search_tool_regex_20251119` fields sent **without** the
`openrouter:tool_search` wrapper type. Per OpenRouter's docs:

> "Using `defer_loading` *without* `openrouter:tool_search` remains valid
> and is unchanged: those requests route to a provider whose gateway
> expands deferred tools itself, and your own search tool is an ordinary
> function tool that provider recognizes. This provider-managed path is
> only available on Anthropic models and Anthropic-compatible endpoints
> that implement deferral."

```json
{
  "model": "claude-opus-5",
  "max_tokens": 2048,
  "messages": [{ "role": "user", "content": "What is the weather in San Francisco?" }],
  "tools": [
    { "type": "tool_search_tool_regex_20251119", "name": "tool_search_tool_regex" },
    {
      "name": "get_weather",
      "description": "Get the weather at a specific location",
      "input_schema": {
        "type": "object",
        "properties": {
          "location": { "type": "string" },
          "unit": { "type": "string", "enum": ["celsius", "fahrenheit"] }
        },
        "required": ["location"]
      },
      "defer_loading": true
    }
  ]
}
```

| Mode | Trigger | Works on |
|---|---|---|
| OpenRouter-managed search/deferral | `{"type": "openrouter:tool_search"}` present | any model/provider |
| Provider-native passthrough | Anthropic-native `tool_search_tool_regex_20251119` fields, no `openrouter:tool_search` wrapper | Anthropic models / Anthropic-compatible endpoints only |

**Response format** under either mode mirrors Anthropic's native shape
(`server_tool_use` → `tool_search_tool_result` → expanded `tool_reference`):

```json
{
  "type": "server_tool_use",
  "id": "srvtoolu_01ABC123",
  "name": "tool_search_tool_regex",
  "input": { "pattern": "weather", "limit": 10 }
},
{
  "type": "tool_search_tool_result",
  "tool_use_id": "srvtoolu_01ABC123",
  "content": {
    "type": "tool_search_tool_search_result",
    "tool_references": [{ "type": "tool_reference", "tool_name": "get_weather" }]
  }
}
```

**UNVERIFIED**: whether OpenRouter's Responses-style surface uses
byte-identical block names to the above when the underlying model is
non-Anthropic (e.g. GPT-5.x via OpenRouter) — could not confirm from
OpenRouter's docs directly whether it normalizes to Anthropic-shaped blocks
or to OpenAI Responses-shaped `tool_search_call`/`tool_search_output` items
in that case.

### 4.3 `tool_choice` warning (verbatim)

> "`tool_choice` conflicts with `openrouter:tool_search`. Deferred tools are
> revealed through `tool_choice`, so it must be omitted or set to
> `{"type": "allowed_tools", ...}`."

Other `tool_choice` values (e.g. forcing a specific named tool) cause a
**400 error** — not silently ignored or overridden. OpenRouter automatically
widens `tool_choice` as tools are discovered through search. Practical
implication for a Rust client: any code path that force-selects a tool by
name must be short-circuited client-side whenever `openrouter:tool_search`
is present in the same request, or the call hard-fails at OpenRouter.

This specific wording is **OpenRouter-only** — no equivalent explicit
warning was found in Anthropic's own native tool-search docs (§1.10) or in
OpenAI's tool_search docs, though the underlying constraint (a deferred tool
can't be force-called before being discovered) is presumably shared logic.

### 4.4 Passthrough vs. native abstraction — summary

**OpenRouter-native, cross-provider abstraction** for Mode 1: "Tool search
works on any model and any provider, not only those with native support for
it." Mode 2 (bare Anthropic-native fields, no `openrouter:tool_search`
wrapper) is pure passthrough, Anthropic-only. OpenRouter's own best-practice
guidance: use tool search when tool defs exceed ~10k tokens or you have
more than ~10 tools; keep your 3–5 most-used tools non-deferred (same
numbers Anthropic recommends natively).

---

## 5. Gemini — tool search / deferred tool equivalent

Sources: `ai.google.dev/gemini-api/docs/function-calling`,
`ai.google.dev/gemini-api/docs/generate-content/function-calling`,
GitHub issue `googleapis/python-genai#2185`.

### 5.1 NOT FOUND — no equivalent feature exists today

Gemini's official function-calling docs contain **no mention** of tool
search, deferred tool loading, `defer_loading`, on-demand tool loading, or
namespace-scoped tool grouping. The only related guidance found is a blunt
manual-curation recommendation:

> "Keep active set to 10-20 tools maximum."

I.e. Google's stated mitigation for large tool counts is caller-side
pre-filtering/curation, not any server-side search/defer mechanism.

**Compositional function calling** (chaining sequential/parallel function
calls within a single turn) is a distinct, unrelated feature — it composes
tool *calls*, not tool *catalog visibility*, and has nothing to do with
deferred loading or context-window tool-definition cost.

### 5.2 Confirmed as an open gap, not just an oversight

A community feature request makes the gap explicit:
[`googleapis/python-genai#2185`](https://github.com/googleapis/python-genai/issues/2185),
"Feature Request: Support defer_loading and tool_search for dynamic tool
discovery (parity with Anthropic & OpenAI)." The issue itself states Gemini
"currently lacks this capability," citing the same problems Anthropic/OpenAI
solve this way (all tool schemas consuming context tokens upfront, 50k+
token multi-MCP-server tool definitions, accuracy degradation past ~30
tools, and pre-filtering breaking prefix caching). No maintainer response or
roadmap commitment was found — it remains open and unresolved as of the
research date.

Both the legacy and current Gemini function-calling doc URLs were checked;
neither exposes anything resembling `tool_search`/`defer_loading`. Vertex
AI's Gemini surface was **not independently fetched** — **UNVERIFIED**
whether Vertex differs from the direct Gemini API here, but no evidence
surfaced suggesting a divergence, and no separate Vertex tool-search doc was
found in search results either.

### 5.3 Bottom line for entanglement-provider

Gemini has no tool-search or deferred-tool-loading primitive, documented or
otherwise, as of 2026-09-14. For a large/dynamic Gemini tool catalog, the
only Google-sanctioned mitigation is client-side curation down to ~10-20
active tools per request — there is no wire-level feature to build client
support for on this provider today.

---

## Appendix: Anthropic general Messages API facts useful for this implementation

- **`tool_choice` types**: `auto` (default), `any`, `tool` (forces a specific
  named tool), `none`. `auto` and `any` both accept
  `disable_parallel_tool_use: true/false`. An `allowed_tools` variant exists
  (referenced by the OpenRouter tool_choice-widening doc) for scoping to a
  subset without fully forcing one tool.
- **SSE event flow** (general, non-tool-search): `message_start` (Message
  with empty `content`) → for each content block: `content_block_start` →
  1+ `content_block_delta` → `content_block_stop` → one or more
  `message_delta` (top-level changes, cumulative `usage`) → `message_stop`.
  `ping` events can appear anywhere; unknown event types must be handled
  gracefully (forward-compat).
- **`tool_use` block delta type**: `input_json_delta` with a `partial_json`
  string field — accumulate the strings and parse once complete
  (`content_block_stop`). Current models emit one complete key/value pair of
  `input` at a time per delta chunk.
- **`server_tool_use` block**: same shape/streaming behavior as `tool_use`
  but for server-executed tools (web_search, tool_search, etc.); has its own
  `srvtoolu_...` id prefix vs. `toolu_...` for client tool calls.
- Server tool errors return **HTTP 200** with an error object embedded in the
  result content block — never raised as an exception/non-200. Applies to
  `tool_search_tool_result` (see §1.12), `web_search_tool_result`,
  `web_fetch_tool_result`.
