# Migration notes — 0.7 → 0.8: providers, tools & config (#915–#920)

> Part of the [0.7 → 0.8 migration notes](../0.7-to-0.8.md) of the [embedding guide](../../../embedding.md).

## Provider-bundled MCP servers removed (#915)

A provider catalog entry can no longer ship MCP servers. The user's
`config.yml` `mcp:` map (or an embedder's own `McpTool` registration / MCP
scopes) is the only way an MCP server reaches the runtime. z.ai's
`web_search_prime`/`web_reader`/`zread` are no longer available out of the
box. What an embedder or user has to change:

- **Removed types/fields:** `entanglement_provider::provider_mcp` (module) and
  `ProviderMcpServer` (also its re-exports `entanglement_provider::catalog::
  ProviderMcpServer` and `entanglement_core::ProviderMcpServer`);
  `ProviderEntry.mcp_servers` (drop it from a struct literal);
  `entanglement_runtime::mcp::AvailableServer`;
  `AvailableMcp::is_bundled`; `mcp::available::disconnect`;
  `mcp::available::bundled_capability_configs`;
  `mcp::capability_index_with_catalog` (call `mcp::capability_index(&user_mcp)`).
- **Reshaped:**
  - `AvailableMcp::from_config(&catalog, &user_mcp, secret_env)` →
    `AvailableMcp::from_config(&user_mcp, secret_env)`.
  - `AvailableMcp::get(name)` returns `Option<McpServerConfig>` (was
    `Option<AvailableServer>`; drop the `.config`).
  - The HTTP transport loses its `api_key` argument, which only identified a
    bundled server's provider key: `McpHttpClient::connect(server, url,
    headers, http)` (also `mcp::HttpClient::connect`),
    `McpHttpClient::connect_authenticated(server, url, headers, auth, http)`,
    `McpClient::connect(server, cfg, secret_env, http)`,
    `McpClient::connect_with_auth(server, cfg, secret_env, auth, http)`. Drop
    the trailing `None`.
- **Wire:** `McpServerStatus.state` (retired by ADR-0226, always `None`) is
  gone. An old log carrying it still deserializes — the field is ignored.
- **Config:** a `providers.yml` provider entry with `mcp_servers:` is now
  rejected at load like any unknown key (`deny_unknown_fields`). Move the
  servers you want into `config.yml` `mcp:`. To keep z.ai web search, for
  example:

  ```yaml
  mcp:
    web_search_prime:
      description: z.ai web search
      url: https://api.z.ai/api/mcp/web_search_prime/mcp
      headers:
        Authorization: "Bearer ${ZAI_API_KEY}"
      capabilities:
        webSearchPrime: read
  ```

  A `config.yml` `mcp:` entry that only overrode a bundled server (e.g.
  `zread: { disabled: true }`) is now an ordinary entry with no transport —
  delete it.
- **`skutter inspect mcp`** no longer prints a server's bundling provider:
  rows read `<name> (<transport>)`.

## The `call` tool (argv exec) removed (#916)

`bash` is the one exec tool. The argv-exec `call` host tool and the `rhai`
`exec(...)` binding that reached it are gone; scripts use
`bash(command)` / `bash(command, workdir)` instead. What an embedder or user
has to change:

- **Removed types:** `entanglement_runtime::host::CallTool` and the whole
  `host::call` module (`call::validate`, `call::output`, `call::format`, the
  foreground/background spawn paths). Register `BashTool` only.
- **Removed / reshaped API:**
  - `RetainedOutputRegistry::register_file` is gone (only `call`'s
    `output_file` used it); `retained_output::Page::output_file` and
    `RetainedInfo::is_file` are removed with it. `register_text` is unchanged.
  - `tool_names::BINDING_TOOLS` is `[&str; 6]` (no `"call"`; removed
    outright by #933).
  - `tool_names::is_recognized_mask_entry` no longer accepts the ADR-0114
    capability key `call` (it only named the removed tool's class).
- **Rhai scripts:** `exec(command)`, `exec(command, args)` and
  `exec(command, args, workdir)` are unknown functions now. Rewrite
  `exec("git", ["log", "--oneline"])` as `bash("git log --oneline")`
  (quote arguments for `sh -c` yourself).
- **Tool calls / wire:** a `ToolExec`/`ToolCall` naming `call` hits the
  unknown-tool path like any unregistered name. Old session logs that recorded
  `call` calls still replay — the tool name is only data there.
- **Permissions / config:** `bash` and `call` no longer share a rule set, and
  there is no `call` tool to grade. A mode or ceiling rule naming `call`
  (`call`, `call(git *)`, `call{/srv/*}`) grades nothing and now warns at
  config load as a stale tool name — rewrite it as the matching `bash(...)`
  rule, or as the `exec` class for a bare `call` key. The legacy
  `permissions:` migration no longer translates a bare `call:` key to `exec`.
- **Workdir grants:** `/cd` (`SetWorkdir`) to a directory outside the root
  records its durable escape-root grant for `bash` only.
- **Unchanged:** an MCP server's `capabilities:` annotation still accepts
  `read`/`write`/`call` (ADR-0117's vocabulary: `call` there means the `Exec`
  class, not the removed tool).

## OAuth for LLM endpoints removed (#917)

An LLM provider authenticates with its static `key_env` key (or none, for a
keyless endpoint) — the catalog `oauth:` block, the per-request bearer and
its forced-refresh-on-`401` retry are gone from every wire. MCP OAuth is
unchanged: `entanglement_provider::oauth` (`AccessTokenSource`,
`StoredTokenSource`, `TokenStore`, `UserTokenStore`, `user_scoped`,
`AuthFlow`, `WebFlow`, …), `McpTokenStore::load()`/`at()`, `/mcp connect`
and `mcp-tokens.yml` all stay. What an embedder or user has to change:

- **Removed fields/methods:** `ProviderEntry.oauth` (drop it from a struct
  literal); `OpenAiLlm::with_auth`, `OpenAiResponsesLlm::with_auth`,
  `AnthropicLlm::with_auth`, `GeminiLlm::with_auth`;
  `UserProviderContext::with_token_source`.
- **Reshaped:** the four factories lose their `auth` argument (the third
  positional one) — drop the `None`:
  `openai_factory(base, key, model, rpm, concurrency, model_concurrency,
  web_search, send_prompt_cache_key, thinking_spec, http)`,
  `openai_responses_factory(base, key, model, rpm, concurrency,
  model_concurrency, thinking_spec, http)`,
  `anthropic_factory(base, key, model, …)`,
  `gemini_factory(base, key, model, rpm, concurrency, model_concurrency,
  http)`.
- **Runtime:** `entanglement_runtime::config::llm_connect` (`connect`,
  `disconnect`), `McpTokenStore::load_llm` and
  `config::state_dir::LLM_TOKENS_FILE` are gone;
  `config::state_dir::MANAGED_STATE_FILES` is `[&str; 7]`.
- **CLI:** `skutter config connect <provider> [--device-code]` and
  `skutter config disconnect <provider>` are gone (`config set-key` stays).
- **Config:** a `providers.yml` provider entry with `oauth:` is now rejected
  at load like any unknown key (`deny_unknown_fields`). Put the endpoint's
  key in `key_env` instead; a gateway that only takes OAuth bearers needs a
  key-issuing front (or the embedder's own proxy) now.
- **Files / env:** `llm-tokens.yml` in the managed-state dir is no longer
  read — delete it (it holds live tokens). `ENTANGLEMENT_STATE_DIR` relocates
  seven files. `ENTANGLEMENT_LLM_TOKENS_FILE` no longer prints a retired-var
  warning; it is simply ignored.

## One rate/concurrency limiter (#918)

Every request admits through one limiter per endpoint (base URL + API-key
hash), configured by one shape, `RateLimits { rpm, tpm, concurrency }` — on
a catalog provider (the endpoint) and optionally on a model (that model on
its endpoint). The adaptive (AIMD) pacing gate, the separate per-model
concurrency slot and the per-user admission gate are gone: `rpm`/`tpm` are a
sliding 60 s window (a burst up to `rpm` goes at once; `concurrency`, held
across the streamed body, is the spawn-storm guard), and two users sharing
one literal key share that key's limits. The 429 / `Retry-After` handling
and the cross-process lease file are unchanged. What an embedder or user has
to change:

- **Config:** `providers.yml` `rpm:` / `concurrency:` on a provider and
  `concurrency:` on a model are rejected at load like any unknown key. Move
  them under `limits:`, which also takes a new `tpm:` (tokens per minute,
  counted from each request's estimated input size):

  ```yaml
  providers:
    - name: zai
      limits: { rpm: 120, concurrency: 5 }
      models:
        - id: glm-4.7-flash
          limits: { concurrency: 1 }
  ```

- **Removed types/items:** `entanglement_provider::UserBudget` and
  `HttpClient::with_user_budget`; `ModelConcurrencyResolver` and
  `fixed_model_concurrency`; `Catalog::model_concurrency_resolver`;
  `RetryConfig.rpm` and `RetryConfig.concurrency`;
  `ThrottleStatus.penalized`.
- **Reshaped fields:** `ProviderEntry.rpm` + `ProviderEntry.concurrency` →
  `ProviderEntry.limits: RateLimits`; `ModelEntry.concurrency` →
  `ModelEntry.limits: RateLimits` (struct literals use
  `RateLimits::default()` for "unset").
- **Reshaped factories:** the three `rpm, concurrency, model_concurrency`
  arguments collapse into one `limits: LimitsResolver` —
  `catalog.limits_resolver(provider)`, or `fixed_limits(RateLimits { … })`
  for a backend without a catalog (site passes
  `fixed_limits(RateLimits { rpm, concurrency, tpm: None })` from its
  provider row):
  `openai_factory(base, key, model, limits, web_search,
  send_prompt_cache_key, thinking_spec, http)`,
  `openai_responses_factory(base, key, model, limits, thinking_spec, http)`,
  `anthropic_factory(base, key, model, limits, web_search,
  web_search_tool_version, model_spec, http)`,
  `gemini_factory(base, key, model, limits, http)` — and the matching
  `*Llm::new`.
- **`HttpClient::execute_with_retry`** is `(endpoint, api_key, model,
  limits: RequestLimits, tokens, retry, request_fn)`.
- **New:** `RateLimits` (also re-exported from `entanglement_core`),
  `RequestLimits`, `LimitsResolver`, `fixed_limits`,
  `Catalog::limits_resolver`, and `HttpClient::with_default_limits(limits)`
  — the fallback for an endpoint's unset axes (default
  `RateLimits::DEFAULT`: 50 rpm, 3 in flight), replacing the removed
  `RetryConfig.rpm`/`concurrency` pool defaults; it returns a client with a
  fresh endpoint pool.
- **Behavior:** the AIMD gate's even spacing (≈1.2 s apart at 50 rpm) is
  gone. The sliding window lets a busy agent loop **burst** `rpm` requests at
  once and then **stall** for up to ~60 s until the oldest leaves the window —
  a latency spike, and a 429 risk on a provider that enforces a per-second
  bucket; set a lower `rpm` or a `concurrency` for such a provider.
  A caller bringing different limits for a live endpoint or model applies
  them to its one gate in place (window and in-flight count kept) instead of
  being bound by the first caller's — site no longer needs a fresh
  `HttpClient` per refresh for an admin edit to apply.
  `ThrottleStatus.next_request_in` (wire: `Throttle.pacing_in_ms`) now means
  "the endpoint's `rpm` window is full, or a request is waiting on `tpm`;
  this long until the oldest entry frees".
- **Multi-user:** `build_user_model_resolver` attaches no per-user gate;
  each user's catalog `limits` apply to their endpoint key.

## Provider-side web search removed (#919)

The provider layer no longer asks an LLM endpoint to run a web search itself.
Web access belongs to host tools or an MCP server (site and nexial already
register their own `web_search` tool; z.ai's `web_search_prime` MCP server is
the drop-in for skutter users, see the #915 section above). What an embedder
or user has to change:

- **Removed types/items:** `entanglement_provider::web_search` (module) and
  `WebSearchConfig` (also its re-export `entanglement_core::WebSearchConfig`);
  `ContentPart::ProviderSearch` and `ContentPart::provider_search`;
  `OutEvent::SearchResult`; `ModelEntry.web_search_tool_version` (drop it
  from a struct literal); `entanglement_runtime::config::Config.web_search`.
- **Reshaped factories:** the web-search arguments are gone — drop the
  `None`s:
  `openai_factory(base, key, model, limits, send_prompt_cache_key,
  thinking_spec, http)`,
  `anthropic_factory(base, key, model, limits, model_spec, http)` (site's
  `web_search_tool_version` `None` goes with it), and the matching
  `OpenAiLlm::new` / `AnthropicLlm::new`;
  `multi_user::build_user_model_resolver(store, http_client)`.
- **Wire:** the Anthropic client requests no server tool, so it no longer
  continues a `pause_turn` stop in place; should one ever arrive it surfaces
  as `StopReason::Other` and the turn loop's ambiguous-stop retry handles it.
  `LlmEvent::ContentBlock` now only carries reasoning blocks, and the engine
  persists each as `OutEvent::ReasoningBlock`.
- **Session logs:** a log that recorded a provider-side search (a
  `search_result` event, or a `provider_search` content part) no longer
  deserializes, so that session cannot be resumed. Web search was opt-in and
  off by default; logs without it are unaffected.
- **Config:** a `config.yml` `web_search:` section and a `providers.yml`
  model `web_search_tool_version:` are rejected at load like any unknown key
  (`deny_unknown_fields`). Delete them; register a web-search tool or MCP
  server instead.
- **`skutter inspect config`** no longer prints a "web search" section, nor
  `web_search_tool_version` among a model's advanced catalog facts.

## Config migration shims removed (#920)

Every shim that read, rewrote or warned about a pre-0.8 config shape is gone.
A key an older release read is now an unknown field like any typo: the load
fails with a plain parse error naming it, and no file is ever rewritten. What
an embedder or user has to change:

- **Removed items:** `entanglement_runtime::config::retired` (module, with
  `env_warnings` and `process_env_warnings`);
  `entanglement_runtime::skills::rejected_skills` and `rejected_skills_in`.
  `config::Resolved.warnings` still carries the load's other non-fatal
  findings (project-layer `http_auth:` / `rhai.trusted`, keybinding clashes,
  …), never a retired-key line.
- **`config.yml`:** `discovery:`, `tool_advertising:`, `endpoints:` and an MCP
  server's `state:` are rejected at load (`deny_unknown_fields`) instead of
  being stripped with a warning. Delete them; an old `state: disabled` becomes
  `disabled: true`. The free-form `permissions:` map (`bash: ask`) is no
  longer rewritten in place into the `default`/`allow`/`deny`/`prompt`
  grammar — rewrite it by hand (`ask` is spelled `prompt`):

  ```yaml
  permissions:
    default: allow
    prompt: [bash]
    deny: [write]
  ```

- **`providers.yml`:** `advertise_discovered`, `discovery` and
  `tool_advertising` on a provider or model are rejected at load. Delete them.
- **Agent files:** a native agent definition still naming a pre-ADR-0207
  authority key (`tools`, `disallowed_tools`, `permission`, `mode`,
  `can_spawn`, `spawnable_agents`, `sandbox`) is no longer backed up to
  `<file>.md.bak` and rewritten — the load fails naming the file. Delete the
  keys; the session's permission mode (`--mode`, `/mode`) is the authority.
- **Skill files:** a native `SKILL.md` still declaring `tools:` (ADR-0228) is
  no longer skipped with an error log, and `user_only:` (ADR-0229) and
  `allowed_tools:` (ADR-0194) no longer load with a warning — any of them
  fails the registry load naming the file. Delete them.
  `SkillMeta.allowed_tools` is removed, and `OutEvent::SkillActive.allowed_tools`
  is always `None` (the wire field itself goes with #922). Foreign (cross-vendor) skill dirs still ignore unknown keys.
  `skutter inspect skills` loses its "rejected (not loaded)" section.
- **Env vars:** the variables ADR-0213 retired (`ENTANGLEMENT_LOG_BODIES`,
  `ENTANGLEMENT_TUI_NO_MOUSE`, `ENTANGLEMENT_*_FILE`, `<NAME>_BASE`,
  `<NAME>_RPM`, `<NAME>_CONCURRENCY`, …) are silently not read — no startup
  warning names a replacement any more; the
  [0.6 → 0.7 note](../agents-modes-and-permissions.md) lists what replaced
  each. An unknown `ENTANGLEMENT_DEBUG` token still warns once.
- **CLI:** `--yes` is an unknown flag (clap's own error) instead of a
  dedicated exit pointing at `--mode auto`.
