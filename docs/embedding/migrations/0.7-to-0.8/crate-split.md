# Migration notes — 0.7 → 0.8: crate split (#931, #964, #965)

> Part of the [0.7 → 0.8 migration notes](../0.7-to-0.8.md).

## The `skutter` crate takes the binary and the heads (#931)

`entanglement-runtime` is a library only. The `skutter` binary and every
head moved to the new workspace crate `skutter`: the startup wiring, the
`run`/`pipe` stdio heads, the TUI, the WebSocket `serve` head, user slash
commands, `inspect`, the log setup and `config set-key`. `skutter` is
`publish = false`: the release publishes the three library crates only
([releasing](../../../releasing.md)). Neither site (`default-features = false`,
`mcp-http` + `provider`) nor nexial (`default-features = false`, `mcp-http`)
names anything below. What changes:

- **Removed binary:** `cargo install entanglement-runtime` no longer installs
  `skutter`. Install it from a checkout: `make install`
  (`cargo install --path skutter --locked`).
- **Removed features:** `cli`, `tui` and `serve`. The runtime's default
  features are now `provider`, `mcp-http`, `rhai` and `web-fetch`, and no
  feature pulls `clap`, `axum`, `crossterm`, `ratatui` or the TUI render
  stack (`make check-lean` gates the default build as well as
  `--no-default-features`).
- **Moved to `skutter` (no longer runtime API):** `serve` (`router`,
  `serve`, `Rejoin`, …), `inspect`, `slash_commands`, `logging`, and
  `config::keys` (`set_key`).
- **Changed API:** `watch::LiveDefinitions::new` drops its `commands`
  argument and `LiveDefinitions` its `commands` field: the runtime no longer
  knows about user slash commands. The watcher still fingerprints the
  layered `commands` dirs, so a head that keeps its own commands reloads
  them on the watcher's `ReloadNotice` (what `skutter`'s TUI does). The
  reload summary no longer counts commands. The env var that relocates
  the user commands layer is `watch::COMMANDS_DIR_ENV`, so a head loads
  the same dirs the watcher fingerprints.
- **Now public** (needed by `skutter`'s `inspect`): `mcp::transport_label`,
  `mode::PRECEDENCE`, `mode::Rules::iter`, `layers::walk_files`,
  `skills::load_skill::render_skill`.
- **Instead:** an embedder that drove the WebSocket head or the slash
  command registry depends on `skutter` by path or git (it is not on
  crates.io), or keeps its own copy. Drop `cli`/`tui`/`serve` from any
  `entanglement-runtime` feature list; drop the `commands` argument from
  `LiveDefinitions::new`.

## Hook execution out of the library executor (#964)

The library runs no hooks. Lifecycle hooks moved to `skutter`, which runs
them from the seams any embedder has: a blocking `pre_tool_use` is a
`PermissionResolver` wrapper (`skutter::hooks::HookResolver`), every other
point a subscriber on the `InMsg`/`OutEvent` fan-out
(`skutter::hooks::spawn`). No new trait replaces them; a resolver's `Deny`
can now carry its reason. Site passes `Hooks::default()` to
`spawn_tool_executor_with_policy` and nexial names no hook item; both
implement `PermissionResolver`, so both change its return type. What
changes:

- **Removed API:** the `entanglement_runtime::hooks` module (`Hooks`,
  `HookSpec`, `HookEvent`); `ToolExecutorBuilder::hooks`;
  `tool_runner::spawn_tool_executor_with_hooks`; the `hooks` argument of
  `tool_runner::spawn_tool_executor_with_policy`.
- **Changed API:** `config::Config::hooks` is the `hooks:` section as raw
  `serde_yaml::Value` (`Null` when absent), no longer a parsed `Hooks`: the
  library no longer validates it, so a typo in it is no longer a config
  load error in the library.
- **Changed API:** `PermissionResolver::resolve` and
  `resolve_with_capabilities` return `policy::Grade` (`Allow | Ask |
  Deny(Option<String>)`), not `Permission`. A `Deny(Some(reason))` reaches
  the model verbatim as the call's error result, in place of the
  `denied by mode … use /mode to switch` text, which a `Deny(None)` still
  gets. A resolver that grades by mode alone returns `permission.into()`;
  a vetoing one returns `Grade::deny(reason)`. `tool_runner::grade_in_scope`
  returns `(Grade, mode)`, so a redirect hop's refusal carries the reason too.
- **Changed behaviour (`skutter`, signed off on #964):** a `pre_tool_use`
  veto grades the call `Deny` with the hook's reason, so the model gets the
  same `tool X blocked by pre_tool_use hook: …` text as in 0.7. The hooks
  run wherever the resolver is asked (`propose_plan` and an in-call
  redirect hop included). Known limitation until #936: a sub-agent's call
  runs them once per session in its ancestor chain, each run naming that
  ancestor. They match the graded tool name (`read_raw` → `read`,
  `glob_json` → `glob`, `grep_json` → `grep`, a `call_grading` target), so a
  `tools: [read_raw]` filter never matches. `post_tool_use` is observe-only
  telemetry: it fires on the `ToolOutput` of a host-tool call the runtime ran
  (`ToolExec`, a measured `duration_ms`) — not a refused call, not a
  script's nested calls, not the executor's own routes (`agent`, `poll`,
  `ask_user`, … — as in 0.7) — and runs asynchronously after the result
  folded back instead of before, on its own worker, so a formatter hook
  races the agent's next call on the same file. Synchronous post hooks run
  inside `skutter`'s own tools, script calls included, once host tools
  (#968) and the file/exec bindings (#933) move there. Its `input` is still
  the workdir-rewritten one (`path_grading::rewrite`, now public).
  `user_prompt_submit` runs on the first event a prompt causes. Details:
  [hooks §9](../../../architecture/gates/hooks-and-seams.md).
- **Instead:** drop `Hooks::default()` from `spawn_tool_executor_with_policy`
  calls and any `.hooks(..)` builder call. An embedder that wants a veto
  wraps its resolver the way `HookResolver` does; one that wants to observe
  calls subscribes to `ToolExec`/`ToolOutput` itself.

## The advertised surface is a per-tool flag (#965)

The hard-coded `KERNEL` name list is gone. A registered tool is advertised
when its new defaulted `Tool::advertised` returns `true` (default `false`);
the executor's own routes (`discover::runtime_owned_specs`,
`subagent::agent_specs`) are always advertised. The built-in tools that were
`KERNEL` members (`read`, `edit`, `write`, `apply_patch`, `bash`,
`web_fetch`, `load_skill`) set it, so `skutter`'s first-request surface is
byte-identical to 0.7's. Neither site nor nexial names `KERNEL` or
`build_surface`, and neither installs `tool_advertising::surface`'s
resolver. What changes:

- **Removed API:** `tool_names::KERNEL`.
- **Changed API:** `tool_advertising::surface::build_surface` takes the
  `&ToolRegistry` instead of a `Vec<ToolSpec>` snapshot of its specs, so it
  can ask each tool for the flag.
- **Changed behaviour:** a tool is no longer advertised because of its
  name. An embedder tool registered as `read`, `bash`, … under the library's
  surface resolver was advertised in 0.7 and is not now unless it sets
  `advertised`; any other embedder tool can now join the surface.
- **Instead:** override `fn advertised(&self) -> bool { true }` on each
  tool the model should see directly; pass `&registry` to `build_surface`.
