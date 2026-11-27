# Migration notes — 0.7 → 0.8: crate split (#931, #964)

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
(`skutter::hooks::spawn`). No new trait replaces them. Site passes
`Hooks::default()` to `spawn_tool_executor_with_policy` and nexial names no
hook item. What changes:

- **Removed API:** the `entanglement_runtime::hooks` module (`Hooks`,
  `HookSpec`, `HookEvent`); `ToolExecutorBuilder::hooks`;
  `tool_runner::spawn_tool_executor_with_hooks`; the `hooks` argument of
  `tool_runner::spawn_tool_executor_with_policy`.
- **Changed API:** `config::Config::hooks` is the `hooks:` section as raw
  `serde_yaml::Value` (`Null` when absent), no longer a parsed `Hooks`: the
  library no longer validates it, so a typo in it is no longer a config
  load error in the library.
- **Changed behaviour (`skutter`):** a `pre_tool_use` veto grades the call
  `Deny` — the model sees the plain `denied by mode` text, the hook's own
  output is logged instead of returned; the hooks run wherever the resolver
  is asked (once per session in a sub-agent call's ancestor chain,
  `propose_plan` and an in-call redirect hop included). `post_tool_use`
  fires on the `ToolOutput` of every call the runtime was asked to run
  (`ToolExec`) — a refused one included, a script's nested calls not — and
  runs after the result folded back instead of before. `user_prompt_submit`
  runs on the first event a prompt causes.
- **Instead:** drop `Hooks::default()` from `spawn_tool_executor_with_policy`
  calls and any `.hooks(..)` builder call. An embedder that wants a veto
  wraps its resolver the way `HookResolver` does; one that wants to observe
  calls subscribes to `ToolExec`/`ToolOutput` itself.
