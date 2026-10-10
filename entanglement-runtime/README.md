# entanglement-runtime

The **head crate** of [entanglement](https://github.com/xmiksay/entanglement)
— ships the `skutter` binary and the runtime half of the engine seam: the
`Tool` trait + `ToolRegistry`, host tools, tool execution, permission dispatch
+ approval, user config, event-sourced persistence, and every transport.

```bash
cargo install entanglement-runtime   # installs the `skutter` binary
```

## Four heads, one engine

| Head | Command | What it is |
| --- | --- | --- |
| stdio | `skutter run` / `skutter pipe` | one-shot run (text or `--format json` NDJSON) and bidirectional NDJSON pipe |
| TUI | `skutter tui` | terminal UI: streaming output, tool-approval prompts, plan/task panels, `/model` picker, `/inspect` overlay |
| WebSocket | `skutter serve --port <N>` | loopback-bound axum HTTP+WS (`/ws`, `/healthz`), JSON frames, local single-user |
| ABI | (library) | hold a `Holly`, call `send()` / `subscribe()` directly |

Plus `skutter sessions` (list past sessions) and `skutter inspect
prompt|agents|skills|modes|config|…` (re-run load-time discovery with no
engine, show the resolved state and which layer won each override).

## Host tools

The root-contained sextet `read` / `write` / `edit` / `glob` / `grep` /
`apply_patch` (canonicalizing, symlink-safe containment; `read` emits images
as content blocks), plus the exec pair — `call` (argv, no shell) and `bash`
(both always registered; own process group, timeout returns partial output) —
a background `call`/`bash` job (`background=true`) is joined with
the runtime-owned `poll` tool — and the sandboxed `rhai` scripting tool.
External **MCP servers**
declared in the user config attach their tools as `mcp__<server>__<tool>`.
The session's permission **mode** (`research` | `plan` | `build` | `auto`;
`Allow | Ask | Deny` per tool, capability class, or argument-scoped rule) plus
persisted "always allow" grants and a user-config ceiling govern every tool —
an agent profile is identity only (prompt + model pin) and carries no
authority (ADR-0207); lifecycle
hooks (`pre_tool_use` / `post_tool_use` / `user_prompt_submit`) wrap dispatch,
and observe-only lifecycle hooks (`session_start` / `session_end` / `stop` /
`pre_compact` / `subagent_stop` / `notification`) follow the event stream.
A session advertises this surface in one of two modes (`full` or the default
`tool_search`, a lean kernel plus `explore`/`describe` discovery — ADR-0196);
every tool is still called by its real name either way, never through an
envelope.

## Providers

Set `ENTANGLEMENT_PROVIDER` (`zai` | `zai_paas` | `openai` | `ollama` |
`anthropic` | `gemini` | any catalog entry) or let key auto-detection pick; no key → offline `EchoLlm`. The
provider/model list is YAML data — see
[`entanglement-provider`](https://crates.io/crates/entanglement-provider).

## Feature gates

| Feature | Adds |
| --- | --- |
| `cli` | clap arg parsing + log init |
| `provider` | real LLM backends (via `entanglement-provider`) |
| `tui` | the terminal UI (ratatui/crossterm) |
| `serve` | the WebSocket head (axum; implies `cli` + `provider`) |
| `mcp-http` | the streamable-HTTP MCP transport + its OAuth flow |
| `rhai` | the sandboxed `rhai` scripting tool |

`default = ["tui", "serve", "mcp-http", "rhai"]` builds the full binary. With
`--no-default-features` the crate is a **lean embeddable library** — tool
execution, permission dispatch, spawn, persistence — with no CLI/TUI/transport
deps.

## Docs

Architecture: [docs/architecture.md](https://github.com/xmiksay/entanglement/blob/master/docs/architecture.md)
([heads & persistence](https://github.com/xmiksay/entanglement/blob/master/docs/architecture/heads-and-persistence.md)
· [agents & permissions](https://github.com/xmiksay/entanglement/blob/master/docs/architecture/agents-and-permissions.md)
· [gates & host tools](https://github.com/xmiksay/entanglement/blob/master/docs/architecture/gates-and-host-tools.md))

## License

MIT — see [LICENSE](https://github.com/xmiksay/entanglement/blob/master/LICENSE).
