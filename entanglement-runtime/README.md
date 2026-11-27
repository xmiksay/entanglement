# entanglement-runtime

The **embeddable runtime library** of
[entanglement](https://github.com/xmiksay/entanglement) — the runtime half of
the engine seam: the `Tool` trait + `ToolRegistry`, host tools, tool
execution, permission dispatch + approval, sub-agents, user config and
event-sourced persistence. Hold a `Holly`, call `send()` / `subscribe()`, and
wire the executor around it. No binary and no CLI/TUI/web-server deps: the
`skutter` coding CLI (stdio `run`/`pipe`, the TUI, the WebSocket `serve`
head) is a separate, unpublished crate in the same repository, built on this
library's public API.

## Host tools

The root-contained sextet `read` / `write` / `edit` / `glob` / `grep` /
`apply_patch` (canonicalizing, symlink-safe containment; `read` emits images
as content blocks), plus `bash` (always registered; own process group,
timeout returns partial output) — a background `bash` job
(`background=true`) is joined with
the runtime-owned `poll` tool — and the sandboxed `rhai` scripting tool.
External **MCP servers**
declared in the user config attach their tools as `mcp__<server>__<tool>`.
The session's permission **mode** (`research` | `plan` | `build` | `auto`;
`Allow | Ask | Deny` per tool, capability class, or argument-scoped rule) plus
persisted "always allow" grants and a user-config ceiling govern every tool —
an agent profile is identity only (prompt + model pin) and carries no
authority (ADR-0207). The library runs no lifecycle hooks: `skutter` runs
them (#964), a blocking `pre_tool_use` as a `PermissionResolver` wrapper and
every other point from a subscriber on the event stream.
Every session advertises one surface, identical on every wire and pinned for
the session (ADR-0226): the native kernel tools, the discovery tools
(`list_agents` / `list_models` / `list_skills` / `load_skill` / `list_mcp` /
`mcp_tools`), `rhai`, and the `mcp` tool — every MCP tool is called through it
with `{server, tool, args}`, its schema read with `mcp_tools`, never
advertised.

## Providers

Set `ENTANGLEMENT_PROVIDER` (`zai` | `zai_paas` | `openai` | `ollama` |
`anthropic` | `gemini` | any catalog entry) or let key auto-detection pick. With
no key, `skutter`'s TUI opens a first-run setup dialog and `run`/`pipe`/`serve` print a
hint and exit 2; the offline `EchoLlm` stub is explicit-only
(`ENTANGLEMENT_PROVIDER=echo`). The
provider/model list is YAML data — see
[`entanglement-provider`](https://crates.io/crates/entanglement-provider).

## Feature gates

| Feature | Adds |
| --- | --- |
| `provider` | the aux-LLM helpers that drain an `entanglement-provider` stream (session titles, the action narrator, the throttle relay) |
| `mcp-http` | the streamable-HTTP MCP transport + its OAuth flow |
| `rhai` | the sandboxed `rhai` scripting tool |
| `web-fetch` | the SSRF-guarded `web_fetch` tool + its HTML→Markdown converter (`htmd`) |

All four are default. With `--no-default-features` the crate is the **lean
library** — tool execution, permission dispatch, spawn, persistence. No feature
set pulls a CLI, TUI or web-server crate.

## Docs

Architecture: [docs/architecture.md](https://github.com/xmiksay/entanglement/blob/master/docs/architecture.md)
([embedding](https://github.com/xmiksay/entanglement/blob/master/docs/embedding.md)
· [heads & persistence](https://github.com/xmiksay/entanglement/blob/master/docs/architecture/heads-and-persistence.md)
· [agents & permissions](https://github.com/xmiksay/entanglement/blob/master/docs/architecture/agents-and-permissions.md)
· [gates & host tools](https://github.com/xmiksay/entanglement/blob/master/docs/architecture/gates-and-host-tools.md))

## License

MIT — see [LICENSE](https://github.com/xmiksay/entanglement/blob/master/LICENSE).
