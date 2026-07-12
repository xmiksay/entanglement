# entanglement Architecture — Hygiene gates & host tools

> Part of the [architecture overview](../architecture.md). The *why* behind each choice is in the [decision log](../adr/README.md).

## 7. Hygiene gates — [ADR-0006](../adr/0006-core-dependency-hygiene-gate.md) (`tree`), [ADR-0025](../adr/0025-runtime-cargo-feature-gates.md) (`check-lean`)

`entanglement-core` must stay free of UI/transport deps. Enforced by
`make tree`, which runs `cargo tree -p entanglement-core` and **fails** if any of
`clap`/`axum`/`tower`/`tonic`/`crossterm`/`ratatui`/`reqwest`/`hyper` appear. It
is part of `make verify`. Current core deps: `tokio`, `serde`, `serde_json`,
`async-trait`, `anyhow`, `thiserror`, `tracing`, `futures`, `uuid`. `glob`/`regex`
(which back the host tools, §8) and `diffy` moved out with the host-tool
implementations to `entanglement-runtime` (✅ #57); the `reqwest` both LLM
backends use lives in `entanglement-provider`, not core — see ADR-0007.

A second gate, **`make check-lean`** (ADR-0025), protects the runtime's lean
library surface: it runs `cargo tree -p entanglement-runtime
--no-default-features -e normal` and **fails** if `clap`/`ratatui`/`crossterm`/
`syntect`/`pulldown-cmark`/`diffy`/`reqwest`/`hyper`/`tracing-subscriber` leak
into the no-default-features build, then runs lean `clippy --all-targets` (which
type-checks the lib + the integration tests with the bin auto-skipped via
`required-features` — the load-bearing check). It joins `tree` in `make verify`.

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
profile ([ADR-0003](../adr/0003-agent-and-permission-profiles.md)). Core defines the
`Tool` **trait**; the implementations live in **`entanglement-runtime::host`**
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
| `read` | `{path, offset?, limit?}` | file contents, `{lineno}: {line}`, 1-based, line-ranged |
| `glob` | `{pattern}` | matching paths (relative to root), one per line |
| `grep` | `{pattern, path?}` | matches as `path:lineno:line` over files matched by `path` (default `**/*`) |
| `edit` | `{path, oldString, newString, replaceAll?}` | exact-string replace; empty `oldString` creates (refused if exists → hints `write`); non-unique match errors unless `replaceAll` |
| `write` | `{path, content}` | whole-file create/overwrite; missing parent dirs created; `created <path> (N lines)` / `overwrote <path> (N lines, was M)` — confirmation only, never echoes content (ADR-0031) |
| `bash` ⚠ | `{command, timeout?}` | `sh -c` rooted at root; `[exit N]` + stdout + `[stderr]`; default 120 s timeout, capped at 600, `kill_on_drop` reaps on expiry |
| `call` ⚠ | `{command, args?, tail?, timeout?}` | **argv, no shell** — `command`+`args` exec verbatim (no `sh -c`, so no pipe/glob/`$VAR`/metachar interpretation); output tailed to the last `tail` lines per stream (default 30, `tail=0` = full, byte-cap still applies), with a `(… N earlier lines omitted, tail=30 — rerun with tail=0 …)` notice; same envelope as `bash` (`[exit N]` + stdout + `[stderr]`, 120 s/600 s, `kill_on_drop`) — ADR-0045 |

- **Working directory:** each tool holds a `root`; model-supplied paths resolve
  against it and are rejected on `..` escape. Lexical containment only (no
  symlink defense) — ADR-0008. `bash`/`call` set only the **cwd** — they are
  explicitly *not* sandboxed and run with the engine's full privileges
  (ADR-0009/ADR-0045); permission profiles gate whether they run at all. `call`
  is the injection-free sibling: a fixed argv can't be shell-injected, so a
  profile may `Allow` `call` while keeping `bash` at `Ask`/`Deny`.
- **Bounded output:** 32 KiB byte cap with a truncation notice; `read` defaults
  to 2000 lines; `glob`/`grep` cap at 1000 results. Prevents a huge file/tree
  from blowing the context window.
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
  the exec pair is opt-in — the `skutter`
  binary registers `BashTool` **and** `CallTool` only when
  `ENTANGLEMENT_ENABLE_BASH=1` (one gate, whole pair), because they run
  unsandboxed (ADR-0009/ADR-0045). `EngineConfig::default()` ships an empty
  registry (embedders opt in via `host_tools`).

`edit`/`write`/`bash`/`call` slot into the existing permission profiles with no profile
changes: `build` auto-allows them (default `Allow`), `plan` asks (default
`Ask`), `explore` denies (default `Deny`). The opt-in gate is
orthogonal to the permission profile: it controls *registration* (whether the
tool is advertised at all), the profile controls *dispatch* (Allow/Ask/Deny
when the model calls it).

Four **runtime-owned orchestration tools** are *not* in the registry — the
`tool_runner` intercepts them on `ToolExec` before permission resolution (they
touch no host resource) and advertises their schemas separately: the `agent_*`
family (§5, ADR-0033) —
`agent_spawn { agent, prompt }` (renamed from `spawn_agent`, ADR-0022), its
non-blocking join `agent_poll { agent_id, timeout_secs }` (ADR-0026), and the
blocking `agent { agent, prompt }` (spawn-and-wait in one call) — plus
`ask_user { question, options, allow_free_form }` (§5, ADR-0027).

[holly]: ../entanglement-core/src/holly.rs
[profile]: ../entanglement-core/src/protocol.rs
[perm]: ../entanglement-core/src/protocol.rs
