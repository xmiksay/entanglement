# Migration notes — 0.7 → 0.8: crate split (#931)

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
