# entanglement-runtime

The **embeddable runtime library** of
[entanglement](https://github.com/xmiksay/entanglement) — the runtime half of
the engine seam: the `Tool` trait + `ToolRegistry`, tool execution,
permission dispatch + approval, sub-agents and the event-sourcing persistence
tap (you bring the sink). Hold a `Holly`, call `send()` / `subscribe()`, and
wire the executor around it. It stores no session log, loads no agent or
skill file, runs no aux model and keeps no global MCP roster: those are the
CLI's. No binary and no CLI/TUI/web-server deps: the
`skutter` coding CLI (stdio `run`/`pipe`, the TUI, the WebSocket `serve`
head) is a separate, unpublished crate in the same repository, built on this
library's public API.

## Tools

The library ships **no host tool** and keeps no filesystem-root logic: an
embedder registers its own tools, and each declares what the permission
pipeline needs through the `Tool` trait — its capabilities, its permission
argument, the path it touches, whether it is advertised. The `skutter` CLI's
file, search, shell and fetch tools (with their containment, escape-root
check and bubblewrap sandbox) are the worked example. The library's own
routes are the sandboxed `rhai` scripting tool, `poll`, the orchestration
and discovery tools and the `mcp` tool. External **MCP servers** attach
their tools as `mcp__<server>__<tool>`; credentials come only through a
`TokenStore`.
The session's permission **mode** (`research` | `plan` | `build` | `auto`;
`Allow | Ask | Deny` per tool, capability class, or argument-scoped rule) plus
"always allow" grants, a ceiling and the embedder's `PermissionResolver`
govern every tool — an agent profile is identity only (prompt + model pin)
and carries no authority (ADR-0207). A resolver may deny or ask for its own
reason (`Grade::deny`, `Grade::Ask(Some(AskReason))`), which the model or the approval card
then shows. The library runs no lifecycle hooks (#964).
Every session advertises one surface, identical on every wire and pinned for
the session (ADR-0226): the registered tools that set `Tool::advertised`
plus the executor's own routes — the discovery tools (`list_agents` /
`list_models` / `list_skills` / `list_mcp` / `mcp_tools`), `rhai`, the
orchestration tools, and the `mcp` tool. Every MCP tool is called through
`mcp` with `{server, tool, args}`, its schema read with `mcp_tools`, never
advertised. A registered tool without the flag stays registered and
dispatchable but is not on the surface.

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
| `provider` | the throttle relay, which polls the `entanglement-provider` endpoint pool |
| `mcp-http` | the streamable-HTTP MCP transport + its OAuth token refresh |
| `rhai` | the sandboxed `rhai` scripting tool |

All three are default. With `--no-default-features` the crate is the **lean
library** — tool execution, permission dispatch, spawn, the persistence tap. No feature
set pulls a CLI, TUI or web-server crate.

## Docs

Architecture: [docs/architecture.md](https://github.com/xmiksay/entanglement/blob/master/docs/architecture.md)
([embedding](https://github.com/xmiksay/entanglement/blob/master/docs/embedding.md)
· [heads & persistence](https://github.com/xmiksay/entanglement/blob/master/docs/architecture/heads-and-persistence.md)
· [agents & permissions](https://github.com/xmiksay/entanglement/blob/master/docs/architecture/agents-and-permissions.md)
· [gates & host tools](https://github.com/xmiksay/entanglement/blob/master/docs/architecture/gates-and-host-tools.md))

## License

MIT — see [LICENSE](https://github.com/xmiksay/entanglement/blob/master/LICENSE).
