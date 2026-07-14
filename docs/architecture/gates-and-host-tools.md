# entanglement Architecture — Hygiene gates & host tools

> Part of the [architecture overview](../architecture.md). The *why* behind each choice is in the [decision log](../adr/README.md).

## 7. Hygiene gates — [ADR-0006](../adr/0006-core-dependency-hygiene-gate.md) + [ADR-0053](../adr/0053-invert-core-provider-seam.md) (`tree`), [ADR-0025](../adr/0025-runtime-cargo-feature-gates.md) + [ADR-0053](../adr/0053-invert-core-provider-seam.md) (`check-lean`)

`entanglement-core` must stay free of UI/web-server deps. Enforced by
`make tree`, which runs `cargo tree -e normal -p entanglement-core` and **fails**
if a forbidden crate appears — ADR-0053's named set
(`clap`/`axum`/`tonic`/`crossterm`/`ratatui`) plus the web/websocket stacks a
name blocklist must also cover (`warp`/`actix`/`rocket`/`tungstenite`/`ureq`,
issue #207). Since [ADR-0053](../adr/0053-invert-core-provider-seam.md) inverted
the seam, core depends on `entanglement-provider`, so `reqwest`/`hyper`/`tower`
(the LLM transport) are now **legitimately** in core's transitive tree and are
not forbidden. It is part of `make verify`. Current core direct deps:
`entanglement-provider`, `tokio`, `serde`, `serde_json`, `async-trait`, `anyhow`,
`thiserror`, `tracing`, `futures`, `uuid`. `glob`/`regex` (which back the host
tools, §8) and `diffy` moved out with the host-tool implementations to
`entanglement-runtime` (✅ #57); the `Llm` trait + DTOs + the `reqwest` LLM
backends live in `entanglement-provider`, the leaf crate — see ADR-0053.

A second gate, **`make check-lean`** (ADR-0025, amended by ADR-0053), protects the
runtime's lean library surface: it runs `cargo tree -e normal -p
entanglement-runtime --no-default-features` and **fails** if `clap`/`ratatui`/
`crossterm`/`syntect`/`pulldown-cmark`/`diffy`/`tracing-subscriber` leak into the
no-default-features build (`reqwest`/`hyper` now ride in via core → provider and
are no longer flagged — ADR-0053), then runs lean `clippy --all-targets` (which
type-checks the lib + the integration tests with the bin auto-skipped via
`required-features` — the load-bearing check). It joins `tree` in `make verify`.

Both gates share one mechanism, [`scripts/dep-gate.sh`](../../scripts/dep-gate.sh)
(issue #207): the Makefile supplies the forbidden set (`CORE_FORBIDDEN` /
`LEAN_FORBIDDEN`) and the `cargo tree` selectors; the script unifies edge policy
(normal edges only — build/dev/proc-macro deps are excluded so they neither trip
nor mask the gate) and **hard-fails on a `cargo tree` error or empty output**.
That last point fixes the gates' original defect: they piped `cargo tree` through
`2>/dev/null` and never checked its exit status, so a *failed* `cargo tree`
grepped clean and passed **vacuously**. `make test-gates` runs
[`scripts/dep-gate.test.sh`](../../scripts/dep-gate.test.sh), a stubbed-`cargo`
self-test that pins the vacuous-pass fix. `cargo-deny` bans (ADR-0006's stated
future) were considered but **not** adopted: they evaluate the whole workspace
graph and can't scope a rule to one crate's subtree, so they cannot express
"forbidden in core but fine in runtime" (`clap`/`crossterm`/`ratatui` live
legitimately in the full runtime graph, and `axum` is reserved for the future
`serve` head) — the per-crate `cargo tree -p` subgraph is exactly what they lack.

**CI (issue #107).** Both gates now run in GitHub Actions
([`.github/workflows/`](../../.github/workflows/)), driven through the same `make`
targets. `ci.yml` runs `make verify` (`check-fmt` + `tree` + `check-lean` +
`lint` + `test`) on every PR and every push to `master` — the first time the
`tree`/`check-lean` hygiene gates run automatically rather than at developer
discretion. `release.yml` fires on a `v*` tag: it runs `make verify` and then a
coverage job, `make coverage` (`cargo llvm-cov --workspace`, fails under
`COV_MIN`% — baselined from the first measured run and ratcheted up, never
lowered), uploading the lcov + Cobertura reports as an artifact so a release is
blocked on green tests with a coverage report attached. Both cache cargo
artifacts (`Swatinem/rust-cache`) and inherit the committed `CARGO_BUILD_JOBS=4`
cap from `.cargo/config.toml`.

## 8. Host tools — [ADR-0008](../adr/0008-host-tools-workdir-and-bounded-output.md) (trio), [ADR-0009](../adr/0009-edit-and-bash-host-tools.md) (`edit`/`bash`), [ADR-0010](../adr/0010-single-head-crate-and-bash-opt-in.md) (exec opt-in), [ADR-0045](../adr/0045-call-host-tool-argv-exec-tailed-output.md) (`call`)

Concrete filesystem + shell tools, dispatched under the active permission
profile ([ADR-0003](../adr/0003-agent-and-permission-profiles.md)). The
`Tool` **trait** and `ToolRegistry` live in **`entanglement-runtime`**
(`entanglement-runtime::tools`, ✅ #206, [ADR-0059](../adr/0059-tool-trait-and-registry-live-in-the-runtime.md)) —
core holds no executable tools, only advertises schemas (§tool round-trip);
the implementations live in **`entanglement-runtime::host`**
(✅ #57) and are assembled by `host_tools(root: PathBuf) -> ToolRegistry`.
Execution *and* permission dispatch now run in the runtime (✅ #58, #59):
`entanglement-runtime::tool_runner` subscribes to the engine, resolves each
`ToolExec`'s `Allow|Ask|Deny` against the session's active profile (§3), runs the
cleared tool against the registry, and replies with `InMsg::ToolResult`. `Ask`
emits the `ToolRequest` prompt and waits for the head's decision on
`Holly::subscribe_inbound()` (the engine's inbound `InMsg` fan-out). Core only
advertises the tool *schemas* (`EngineConfig.tool_specs`) — it holds no executable
tools and makes no policy decision:

| tool | input | output |
| --- | --- | --- |
| `read` | `{path, offset?, limit?}` | text file → contents as `{lineno}: {line}`, 1-based, line-ranged; an **image file** (`.png`/`.jpg`/`.jpeg`/`.gif`/`.webp`, by extension) → a base64 **image content block** the provider renders natively (Anthropic `image` / OpenAI `image_url`), routed through the `ToolResult` `content` path (`offset`/`limit` ignored) — #221 |
| `glob` | `{pattern}` | matching paths (relative to root), one per line |
| `grep` | `{pattern, path?}` | matches as `path:lineno:line` over files matched by `path` (default `**/*`) |
| `edit` | `{path, oldString, newString, replaceAll?}` | exact-string replace; empty `oldString` creates (refused if exists → hints `write`); non-unique match errors unless `replaceAll` |
| `write` | `{path, content}` | whole-file create/overwrite; missing parent dirs created; `created <path> (N lines)` / `overwrote <path> (N lines, was M)` — confirmation only, never echoes content (ADR-0031) |
| `bash` ⚠ | `{command, timeout?, workdir?, run_in_background?}` | `sh -c` rooted at root (or at `workdir`, a subdir validated under root by the same symlink-safe containment as the fs tools, #170); `[exit N]` + stdout + `[stderr]`; default 120 s timeout, capped at 600; spawned in its **own process group** (`process_group(0)`) so an expiry SIGKILLs the whole tree — grandchildren (a launched server/pipeline) can't orphan (#168); a `Stop`-driven task abort drops the wait future, whose group-kill guard SIGKILLs the same group so cancellation matches the timeout's containment rather than orphaning under bare `kill_on_drop` (#167). Output is drained incrementally, so a timeout returns the **partial output buffered before the kill** under a `[killed: timed out after Ns]` header instead of discarding it (#169). Oversized output is capped **head + tail** (¼ head / ¾ tail, `truncate_head_tail`) so the trailing error survives — head-only truncation dropped exactly what a failing build needs (#170). `run_in_background: true` spawns the command **detached** and returns a job id instead of blocking — poll it with `bash_output` (#170) |
| `bash_output` ⚠ | `{job_id, kill?}` | poll a background `bash` job (started with `run_in_background`) for the output produced **since the last poll**, plus status (`running` / `exited N` / `exited (killed)`). Buffers are drained per poll (`mem::take`) so memory is reclaimed and each read is incremental; between polls each stream is capped at 256 KiB dropping the **oldest** bytes (the live tip is kept) with a `[N bytes … dropped]` notice. `kill: true` SIGKILLs the job's whole process group before reading. Registered as a pair with `bash` under the same opt-in gate (#170) |
| `call` ⚠ | `{command, args?, tail?, timeout?}` | **argv, no shell** — `command`+`args` exec verbatim (no `sh -c`, so no pipe/glob/`$VAR`/metachar interpretation); output tailed to the last `tail` lines per stream (default 30, `tail=0` = full, byte-cap still applies), with a `(… N earlier lines omitted, tail=30 — rerun with tail=0 …)` notice; same envelope as `bash` (`[exit N]` + stdout + `[stderr]`, 120 s/600 s, own-process-group kill on timeout #168, partial output preserved on timeout #169) — ADR-0045 |
| `rhai` | `{script, timeout?}` | run a Rhai script ([rhai.rs](https://rhai.rs)) in a **capability-sandboxed** engine — no fs/network/process/env access; the only host bindings are `read`/`glob`/`grep`/`edit`/`write`, each routed through that tool's permission check; last-expression value serialized + captured `print(...)`; bounded by op/string/array/map caps + wall-clock (default 5 s, max 30) — [ADR-0046](../adr/0046-rhai-sandboxed-script-tool.md) |

- **Working directory:** each tool holds a `root` (the cwd, **canonicalized once
  at startup**); model-supplied paths resolve against it and are rejected on `..`
  escape **and on symlink escape** — `resolve_under_root` canonicalizes the
  resolved target's deepest existing ancestor and requires it under the canonical
  root, so a `root/link -> /etc` symlink can't be followed out of tree by
  `read`/`edit`/`write` (the create path still works: only the existing ancestor
  is canonicalized), and `glob`/`grep` (`list_files`) drop any match whose
  canonical path escapes — ADR-0008 upgraded by [ADR-0054](../adr/0054-canonicalizing-symlink-safe-root-containment.md)
  (#163). Not TOCTOU-tight (an OS sandbox via `openat2(RESOLVE_BENEATH)` is
  deferred). `bash`/`call` set only the **cwd** — they are
  explicitly *not* sandboxed and run with the engine's full privileges
  (ADR-0009/ADR-0045); permission profiles gate whether they run at all. `call`
  is the injection-free sibling: a fixed argv can't be shell-injected, so a
  profile may `Allow` `call` while keeping `bash` at `Ask`/`Deny`.
- **Secret scrubbing (#164):** both exec tools `env_remove` the catalog's
  provider API-key env vars (`Catalog::key_envs()` — `ZAI_API_KEY`,
  `OPENAI_API_KEY`, `ANTHROPIC_API_KEY`, …) from the child before spawn, so a
  model-authored `env`/`printenv` can't read the engine's credentials. `call`'s
  no-shell design doesn't help — a plain `env` still inherits them — so the scrub
  covers both. `rhai` is exempt (no env binding). The head wires the set via
  `BashTool::new(root).with_secret_env(catalog.key_envs())` (same for `CallTool`);
  a broader env-allowlist policy can ride the future sandbox ADR.
- **Bounded output:** 32 KiB byte cap with a truncation notice; `read` defaults
  to 2000 lines; `glob`/`grep` cap at 1000 results. Prevents a huge file/tree
  from blowing the context window. `bash`/`bash_output` cap **head + tail**
  (`truncate_head_tail`) rather than head-only — build/test output puts the
  load-bearing error at the end (#170).
- **Empty-result contract (ADR-0016):** a host tool may not return a silent
  zero-output when multiple distinguishable underlying states produce it.
  `list_files` returns `FileList { files, matched_dirs, skipped_errors }`;
  per-entry walk errors are `warn!`-logged and counted, not swallowed. When
  `glob`'s result would be empty but the pattern matched something (the common
  bare-`**` trap, which matches only directories), it returns a hint like
  *"`**` matched 7 directories but no files — try `**/*`"* so the model can
  self-correct mechanically. `grep` consumes the same `FileList` but stays
  silent on zero matches (a clean no-match is a single well-defined state).
- **Schema advertisement:** `Tool::schema()` feeds `ToolRegistry::specs()`, so
  the model sees a real `input_schema` per host tool (not an empty object).
- **Wiring (ADR-0010):** `host_tools(root)` registers the **root-contained
  quintet** (`read`/`glob`/`grep`/`edit`/`write`; `write` added in ADR-0031).
  the exec set is opt-in — the `skutter`
  binary registers `BashTool`, `CallTool`, **and** `BashOutputTool` (the
  background-job poller, #170) only when `ENTANGLEMENT_ENABLE_BASH=1` (one gate,
  whole set), because they run unsandboxed (ADR-0009/ADR-0045). `bash` and
  `bash_output` share one `JobRegistry` so background jobs are pollable across the
  pair. `EngineConfig::default()` ships an empty registry (embedders opt in via
  `host_tools`).

`edit`/`write`/`bash`/`bash_output`/`call` are advertised only to the inherit-all
`build` profile (`tools: None`), which auto-allows them (default `Allow`). The `plan`
and `explore` profiles set an explicit `tools` allowlist that omits them
(#116/#140, [ADR-0038](../adr/0038-physical-per-agent-tool-restriction.md)), so
the tools are **masked out** of those profiles entirely — never advertised, so
no `Allow`/`Ask`/`Deny` default is reached for them there. The opt-in gate is
orthogonal to both mask and profile: it controls *registration* (whether the
tool is advertised at all), the mask controls *existence* per profile, and the
profile controls *dispatch* (Allow/Ask/Deny when the model calls a tool that
survives the mask).

Five **runtime-owned orchestration tools** are *not* in the registry — the
`tool_runner` intercepts them on `ToolExec` before permission resolution (they
touch no host resource) and advertises their schemas separately: the `agent_*`
family (§5, ADR-0033) —
`agent_spawn { agent, prompt }` (renamed from `spawn_agent`, ADR-0022), its
non-blocking join `agent_poll { agent_id, timeout_secs }` (ADR-0026), and the
blocking `agent { agent, prompt }` (spawn-and-wait in one call) —
`ask_user { question, options, allow_free_form }` (§5, ADR-0027), and
`propose_plan { plan }`, the plan agent's finalize step, force-parked on the
user-approval round-trip since acceptance *is* its semantics (#141,
[ADR-0042](../adr/0042-plan-acceptance-via-propose-plan-approval-roundtrip.md);
advertised only to a profile that explicitly allowlists it, #231). The `rhai`
script tool (table above) is intercepted the same way but is **not** a bypass:
it resolves its own `Allow`/`Ask`/`Deny` live inside the sandboxed script task
(#122, [ADR-0046](../adr/0046-rhai-sandboxed-script-tool.md)).

## 9. Lifecycle hooks — [ADR-0066](../adr/0066-lifecycle-hooks-as-runtime-interceptors.md) (#199)

User-configured external commands run around tool execution and on prompt
ingress, for policy, telemetry, and formatting side-effects. Hooks are a
**runtime interceptor** (`entanglement-runtime::hooks`), not a core concept:
core neither knows nor cares that a command runs before a tool. They hang off the
two seams the runtime already owns — the `tool_runner` dispatch of a `ToolExec`
and the inbound `InMsg::Prompt` fan-out — so no new protocol surface is added.

| point | fires | can block? | payload |
| --- | --- | --- | --- |
| `pre_tool_use` | top of the generic `dispatch` (`Intercept::Permission`), **before** the `Allow`/`Ask`/`Deny` decision | **yes** — a non-zero exit vetoes: the tool neither prompts nor runs, and the hook's output becomes the `ToolResult` | `{event, session, tool, input}` |
| `post_tool_use` | in `run_and_reply` after the tool result, before it folds back | no — observational (exit code logged, never fed to the model); it cannot rewrite the result | `{event, session, tool, input, output}` |
| `user_prompt_submit` | when an `InMsg::Prompt` reaches the engine (the executor's inbound `Stop` watcher) | no — observational | `{event, session, prompt}` |

- **Config:** the `hooks:` section of the layered user config (§ADR-0047/#172).
  `Config.hooks: Hooks` is three `Vec<HookSpec>` deep-merged and
  `deny_unknown_fields`-validated by the same loader as `permissions`. A
  `HookSpec` is `{command, tools?, timeout_secs?}`; `tools` is an optional
  name-filter for the tool hooks (empty ⇒ every tool), ignored by
  `user_prompt_submit`. Empty section ⇒ no hooks (the norm).
- **Execution:** each hook is an `sh -c <command>` child fed the JSON payload on
  stdin and given `ENTANGLEMENT_HOOK_EVENT` / `ENTANGLEMENT_SESSION_ID` /
  `ENTANGLEMENT_TOOL_NAME` (tool hooks) env vars. It runs under `timeout_secs`
  (default 30) in its **own process group**, reusing the exec tools' containment
  (`host::exec`, §8/#168) so a hook that spawns children can't orphan them. A
  timeout or a spawn failure counts as a **failure**, so a `pre_tool_use` hook
  that can't launch **fails closed** (vetoes the tool) rather than letting it
  through.
- **Scope:** only the generic host-tool dispatch route. The orchestration tools
  (`agent`/`ask_user`/`propose_plan`, which touch no host resource) and the
  self-permissioning `rhai` tool bypass hooks — matching the issue's "around
  `tool_runner::dispatch`" scope.
- **Wiring:** `spawn_tool_executor_with_hooks(holly, tools, profiles, base, hooks)`
  is the seam `main.rs` uses; the historical `spawn_tool_executor` is a no-hook
  wrapper (existing callers/tests unchanged). The inbound subscription is hoisted
  synchronous before the executor task spawns so a first `Prompt` can't race the
  `user_prompt_submit` watcher.

## 10. MCP client — external tool servers — [ADR-0067](../adr/0067-mcp-client-as-runtime-tool-provider.md) (#198)

Attach any external [MCP](https://modelcontextprotocol.io) tool server as a
**runtime-side tool provider**, with **no core change**. Since the `Tool` trait +
`ToolRegistry` live in the runtime (§ADR-0059), an external tool is the same shape
as a host tool: a `dyn Tool` with a name, description, and `inputSchema`. The MCP
client (`entanglement-runtime::mcp`) spawns each server, discovers its tools, and
registers them into the same registry — so they ride `EngineConfig.tool_specs`
(schemas) and the `ToolExec`/`ToolResult` round-trip (execution) unchanged, under
the same permission profiles as `read`/`bash`.

- **Transport (`mcp::client::McpClient`):** one JSON-RPC 2.0 session per server
  over its **stdio**, newline-delimited frames (the MCP stdio transport). Handshake
  is `initialize` + `notifications/initialized`; then `tools/list` (discovery) and
  `tools/call` (execution). A background reader task demultiplexes responses to
  callers by JSON-RPC `id`; notifications are dropped. A **60 s** per-request
  timeout keeps a hung server from parking a turn, and the reader **drains all
  pending requests with an error on EOF** so a crashed server can't hang a caller.
  The subprocess is held for the client's lifetime (`kill_on_drop`); keeping the
  registered tools alive keeps the server alive.
- **Proxy (`mcp::tool::McpTool`):** adapts one remote tool. `schema()` returns the
  server's `inputSchema` verbatim; `run()` JSON-decodes the model's input to the
  `arguments` object, calls `tools/call`, and flattens the result's text content
  (v1 is text-only — a non-text block is noted, an `isError` result prefixed).
  Advertised name **`mcp__<server>__<tool>`**, sanitized to the providers'
  `^[A-Za-z0-9_-]+$` rule, so it can't collide with a host tool or another server.
- **Config:** the `mcp:` section of the layered user config (§ADR-0047/#172), a map
  of server name → `{command, args, env, disabled}`, `deny_unknown_fields`-validated
  by the same loader as `permissions`/`hooks`. Empty ⇒ no servers (the norm).
  `skutter inspect config` lists the configured servers.
- **Wiring:** `build_config` is `async` and calls `mcp::connect(&config.mcp, &mut
  tools)` after the host tools are registered but before `tool_specs` is derived, so
  MCP tools flow into both the advertised schemas and the executor's registry with
  the existing code. Connection is **best-effort per server**: a spawn / handshake /
  `tools/list` failure is logged and skipped — a down server degrades to "that tool
  is absent," never a startup failure. The whole module lives in the **lean
  library** (tokio process + `serde_json`), so an embedder gets external tool
  servers with no CLI/TUI/transport dependency.

[holly]: ../entanglement-core/src/holly.rs
[profile]: ../entanglement-core/src/protocol.rs
[perm]: ../entanglement-core/src/protocol.rs
