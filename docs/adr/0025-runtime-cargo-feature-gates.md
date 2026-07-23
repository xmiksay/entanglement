# 0025. `entanglement-runtime` cargo feature gates (`cli`/`tui`) for lean library embedding

- Status: Accepted — lean-transport claim amended by [0053](0053-invert-core-provider-seam.md); `cli`/`provider` split + module-import cleanup amended by #208; tokio feature scope + a second "zero `#[cfg(feature)]`" exception amended by [0135](0135-deferred-build-speed-trims-tokio-rhai-syntect.md)
- Date: 2026-07-09

> **Amended by [ADR-0135](0135-deferred-build-speed-trims-tokio-rhai-syntect.md)
> (2026-07-23).** Two corrections to claims made below: (1) "`tokio` stays
> `features = ["full"]`; trimming it is out of scope (KISS)" (under
> Consequences) no longer holds — each crate now declares its own minimal
> tokio feature list. (2) The new `rhai` feature (gating `entanglement-runtime`'s
> sandboxed script tool, default-on) is a second deliberate exception to the
> "zero `#[cfg(feature)]` in code" property, alongside the `cli`-gated
> `logging` module #208 already carved out below.

> **Amended by [ADR-0053](0053-invert-core-provider-seam.md) (2026-07-13).** Since
> `entanglement-core` now depends on `entanglement-provider`, the lean
> (`--no-default-features`) runtime carries `reqwest` transitively through core.
> The feature-gate mechanism below stands; only the "lean runtime is
> transport-free" property changed — `make check-lean` now asserts CLI/TUI-free,
> not transport-free.

> **Amended by issue #208 (2026-07-13).** Two refinements to the `cli` feature
> below: (1) the LLM providers (`dep:entanglement-provider`) split out of `cli`
> into their own **`provider`** feature, so `cli` is now clap + tracing-subscriber
> only and a future `ws`/`serve` head can depend on `provider` without dragging
> clap into a WebSocket server; `tui` gains `provider` and the bin's
> `required-features` becomes `["cli", "provider", "tui"]`. (2) `main.rs` stopped
> re-declaring the library modules as `mod` (which compiled the library source a
> second time and let a bin-only `mod` slip past `check-lean`) — it now imports
> them from the lib crate, keeping only `pipe`/`run`/`tui` as bin modules. This
> moved `config`/`inspect`/`logging` into `lib.rs`; `logging` (tracing-subscriber)
> is `#[cfg(feature = "cli")]`, the one deliberate exception to the "zero
> `#[cfg(feature)]` in code" property stated below.

## Context

The reuse need [ADR-0010](0010-single-head-crate-and-bash-opt-in.md) foresaw has
arrived (issue #82): other projects want the runtime's tool-execution loop,
permission dispatch, and sub-agent spawn machinery **without** compiling the
TUI/CLI stack (ratatui, syntect, clap, reqwest…). ADR-0010 explicitly deferred
cargo features for exactly this — "solves a dep-weight problem that does not
exist yet". It does now.

Two shapes were weighed:

- **Move the machinery into core.** Rejected. The *definitions* are already in
  core (`Tool` trait, `ToolExec`/`ToolResult` round-trip, `Permission`/`AgentProfile`,
  `InMsg::Spawn`, `subscribe_inbound`). Moving the *executing/policy* half back
  would reverse [ADR-0006](0006-core-dependency-hygiene-gate.md)/[0008](0008-host-tools-workdir-and-bounded-output.md)/[0010](0010-single-head-crate-and-bash-opt-in.md)/[0022](0022-subagent-spawn.md)/[0023](0023-subagent-spawn-limits.md):
  core would again own filesystem/shell I/O, a fixed toolset, and permission
  policy.
- **Cargo feature flags in the runtime.** Chosen. Key enabler: `tool_runner.rs`,
  `subagent.rs`, and `permission.rs` import only core types +
  `tokio::sync::broadcast`; `host/` adds only `glob`/`regex`; `persistence.rs`
  and `session_store.rs` add only serde/anyhow/dirs/std; and `src/lib.rs` already
  exposes exactly that library surface, while `tui/`, `run.rs`, `pipe.rs`, and
  `main.rs` are **binary-only modules**. So the gating happens entirely in
  `Cargo.toml` with **zero `#[cfg(feature)]` in code**.

Grep-verified dep map: ratatui/crossterm/syntect/pulldown-cmark/diffy/unicode-width
→ only `src/tui/`; clap + tracing-subscriber → only `main.rs`;
`entanglement-provider` (drags reqwest) → only `main.rs` + `tui/`; `dirs` → only
`session_store.rs`. The integration tests import only
`entanglement_runtime::{host, tool_runner}` (+ the subagent/permission surface)
plus core — so they compile lean.

## Decision

Two features on `entanglement-runtime`:

- `cli = ["dep:clap", "dep:tracing-subscriber", "dep:entanglement-provider"]` —
  head plumbing: arg parsing, log init, LLM providers (reqwest). `cli` alone
  stays meaningful for a future lean stdio-only build (`run`/`pipe` without the
  render stack).
- `tui = ["cli", "dep:ratatui", "dep:crossterm", "dep:pulldown-cmark",
  "dep:syntect", "dep:diffy", "dep:unicode-width"]` — the terminal UI head. It
  **implies `cli`**: the TUI is only reachable through the clap CLI, and `tui/`
  itself imports `entanglement-provider` (`ModelInfo`, `models_for`, `HttpClient`).
- `default = ["tui"]` — every existing command behaves identically.
- `[[bin]] skutter` gets `required-features = ["cli", "tui"]` — cargo skips the
  bin when the features are off, so **`main.rs` is untouched** and no clap
  subcommand needs cfg-gating.
- `persistence` and `session_store` are promoted to the lib (`pub mod`) — the
  ADR-0020 persistence machinery a library consumer wants next to `tool_runner`.

The **lean surface** (`--no-default-features`) is:
`host` + `tool_runner` + `subagent` + `permission` + `persistence` +
`session_store`, over `entanglement-core` + tokio + `glob` + `regex` +
serde/anyhow/tracing/`dirs`.

A `make check-lean` gate wires this into `verify`.

## Consequences

### Positive

- A library consumer gets the execution/policy/spawn/persistence machinery with
  none of the CLI/TUI/transport weight; the reuse case ADR-0010 deferred is met
  without a new crate or a core regression.
- Zero `#[cfg(feature)]` in code — the split is a `Cargo.toml` fact, resting on
  the existing lib/bin module boundary. `main.rs` and every subcommand are
  untouched; default builds and tests are byte-for-byte the same.
- `Cargo.lock` is unchanged — no new dependencies, only `optional = true` markers.

### Negative / neutral

- Feature gating alone is **weakly enforced**: a stray `use ratatui` in a lib
  module compiles fine under default features and only fails a lean build.
  Mitigated by `check-lean` in `verify` — its lean clippy `--all-targets`
  type-checks the lib **and** the integration tests in lean config (the bin is
  auto-skipped via `required-features`); that compile check is the load-bearing
  gate, and the grep mirrors the ADR-0006 `tree` gate.
- `dirs` becomes an unconditional dependency (tiny; accepted) rather than being
  gated with the head.
- `tokio` stays `features = ["full"]`; trimming it is out of scope (KISS).
- Keeping `cli` separate from `tui` leaves room for `ws = ["cli", …]` siblings
  when the WebSocket head lands, without dragging syntect into it.

## Alternatives considered

- **A new library crate.** Deferred until a second consumer exists; ADR-0008
  called a premature split "a boundary with no consumer". Features get the reuse
  today with no crate to maintain.
- **Move the machinery into core.** The ADR-0006 rationale (above) — rejected.
- **A single monolithic feature.** Couples syntect to the future `ws` head;
  keeping `cli` separate leaves room for `ws = ["cli", …]` siblings.
- **cfg-gating the clap subcommands.** "Fiddly" per ADR-0010;
  `required-features` on `[[bin]]` avoids touching `main.rs` entirely.

## References

- Issue #82: runtime cargo feature gates (cli/tui) for lean library embedding
- [ADR-0010](0010-single-head-crate-and-bash-opt-in.md): single head crate;
  deferred these cargo features (amended here, never edited)
- [ADR-0006](0006-core-dependency-hygiene-gate.md): core dependency hygiene gate
- [ADR-0008](0008-host-tools-workdir-and-bounded-output.md): host tools boundary
- [ADR-0020](0020-event-sourced-session-persistence.md): event-sourced persistence
- [ADR-0022](0022-subagent-spawn.md) / [ADR-0023](0023-subagent-spawn-limits.md)
  / [ADR-0024](0024-subagent-permission-gating.md): sub-agent spawn machinery reused
