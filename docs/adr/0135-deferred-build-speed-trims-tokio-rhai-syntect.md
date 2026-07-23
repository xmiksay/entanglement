# 0135. Deferred build-speed trims: per-crate tokio features, optional `rhai`, trimmed `syntect`

- Status: Accepted — amends [ADR-0025](0025-runtime-cargo-feature-gates.md) (tokio feature scope, "zero `#[cfg(feature)]`" claim) and touches [ADR-0046](0046-rhai-sandboxed-script-tool.md) (the `rhai` tool is no longer unconditionally compiled)
- Date: 2026-07-23

## Context

Deferred-work-ledger row 4 (issue [#502](https://github.com/xmiksay/entanglement/issues/502),
filed by the 2026-07-23 revisit audit): the safe build-speed set (dev-profile
tuning, `lld` linker, the single-use `rand` dep dropped) shipped in the same
audit, but three larger trims were explicitly deferred pending their own
`make verify` + lean-build validation:

1. **`tokio = { features = ["full"] }`** in the workspace `[workspace.dependencies]`
   pulled every tokio feature into all three crates, even though
   `entanglement-provider` and `entanglement-core` use only a fraction of the
   surface (no `fs`/`net`/`process`/`signal` in either).
2. **`rhai`** (the sandboxed script tool, ADR-0046) was a non-optional runtime
   dependency — one of the heaviest always-compiled deps in the tree — so a
   lean (`--no-default-features`) embedder that never registers the `rhai`
   tool paid its full compile cost anyway.
3. **`syntect = { features = ["default-fancy"] }`** (behind the `tui` feature)
   pulled `html`/`plist-load`/`yaml-load`/`dump-create` alongside the
   `parsing`/`default-syntaxes`/`default-themes`/`regex-fancy` the TUI's
   markdown code-block highlighter (`tui/markdown.rs`) actually calls
   (`SyntaxSet::load_defaults_newlines`, `ThemeSet::load_defaults`,
   `syntect::easy::HighlightLines` against the bundled dumps — never HTML
   output, never a loaded `.tmTheme`/`.sublime-syntax` file).

None of the three change behavior; each is a `Cargo.toml`-level (plus, for
`rhai`, a small amount of `#[cfg(feature)]`) trim validated by the existing
gates.

## Decision

**1. Per-crate tokio features.** The workspace `tokio` dependency drops
`features = ["full"]` down to a bare `tokio = { version = "1" }`; each crate's
own `[dependencies]` entry adds only the features its own (grep-verified) API
surface needs, via `features = [...]` on its `workspace = true` line (additive
to the empty base, not a replacement):

| Crate | Features | Why |
| --- | --- | --- |
| `entanglement-provider` | `rt`, `macros`, `sync`, `time`, `net`, `io-util` | `tokio::spawn`/`#[tokio::test]` (`rt`/`macros`), the retry semaphore + sleep/timeout backoff (`sync`/`time`), and `tests/streaming.rs`'s hand-rolled loopback SSE server (`net`/`io-util` — no separate `tokio` dev-dependency exists to scope those two to tests only) |
| `entanglement-core` | `rt`, `macros`, `sync`, `time` | `tokio::spawn`/`task::yield_now` (`rt`), `select!`/`#[tokio::test]` (`macros`), `broadcast`/`mpsc` (`sync`), `sleep`/`timeout`/`interval`/`Instant` (`time`); no fs/net/process/signal use |
| `entanglement-runtime` | `rt`, `rt-multi-thread`, `macros`, `sync`, `time`, `fs`, `process`, `net`, `io-util`, `io-std`, `signal` | the head crate exercises nearly the whole surface: `#[tokio::main]` (`src/main.rs`) plus one `flavor = "multi_thread"` test (`src/extra_roots.rs`) need `rt-multi-thread`; `fs`/`process` back the host tools; `net` backs `serve` + the HTTP MCP transport's tests; `io-util`/`io-std` back the exec tools' piping and `pipe`'s stdin; `signal` backs `ctrl_c()` in `serve`/`tui` |

`entanglement-core`'s `[dev-dependencies] tokio = { features = ["test-util"] }`
(already present, for `tests/idle_ttl.rs`'s `start_paused = true` cases) is
unchanged — Cargo unifies `[dependencies]`/`[dev-dependencies]` features for
the same package within one build, so it composes with the trimmed base
above rather than replacing it. No crate uses `tokio_util`, and none
constructs a `Runtime`/`Builder` manually (`#[tokio::main]`/`#[tokio::test]`
cover every case) — confirmed by a full grep pass, not by inspection of a
subset.

**2. `rhai` behind a default-on `entanglement-runtime` feature.** A new
`rhai = ["dep:rhai"]` feature joins `default = ["tui", "serve", "mcp-http",
"rhai"]` — every existing build (including the `skutter` binary) is
byte-identical, but `--no-default-features` now excludes the crate entirely.
This is the ADR-0025 lean-library mechanism's first real exception to its own
"zero `#[cfg(feature)]` in code" property (which the `serve` feature's
`#[cfg(feature = "serve")] pub mod serve;` already predates in practice — see
`entanglement-runtime/src/lib.rs`): `rhai`'s single-crate footprint (only
`script.rs` imports `rhai::*`) made cfg-gating cheap, unlike a hypothetical
gate on the executor's whole interception ladder.

- `pub mod script;` in `lib.rs` gains `#[cfg(feature = "rhai")]`, mirroring
  `serve`'s existing pattern exactly.
- The two `script::rhai_spec()` call sites in `main.rs` (advertising the tool
  into `EngineConfig.tool_specs` and into the live `tool_spec_resolver`
  snapshot) are gated the same way — a lean-without-`rhai` build never
  advertises the tool to a model.
- `tool_runner.rs`'s `Intercept::Rhai` route — the executor's pure-name-based
  classification that intercepts a `rhai` `ToolExec` before the generic
  `Allow | Ask | Deny` dispatch — gets its variant, its `classify` match arm,
  and its handling arm each individually `#[cfg(feature = "rhai")]`'d. With
  the feature off, `Intercept::classify("rhai")` falls through to the
  wildcard `_ => Self::Permission` arm, so a call literally named `rhai`
  (the tool isn't registered/advertised, but nothing stops a model from
  hallucinating the name) takes the ordinary generic-dispatch path and is
  refused there exactly like any other unknown tool name — no new refusal
  path, no behavior change beyond "the tool doesn't exist" for a lean build
  that opted out.
- A handful of items become conditionally dead without the feature
  (`AtomicBool`/`clamp_to_base`/`effective_permission` imports in
  `tool_runner.rs`, the `base: PermissionProfile` parameter of
  `spawn_tool_executor_with_policy` — used only inside the Rhai arm's own
  ceiling clamp — and `permission::permission_chain`, called only from
  `script.rs` outside its own unit test): each gets a narrowly-scoped
  `#[cfg(feature = "rhai")]` (imports) or `#[cfg_attr(not(feature = "rhai"),
  allow(...))]` (the parameter and the function, both still exercised by
  code/tests that *are* unconditionally compiled) rather than a blanket
  `#[allow(dead_code)]` at the module level.
- `tests/rhai.rs` (the tool's own integration suite, 20 tests) gets a
  `[[test]] required-features = ["rhai"]` entry — the same pattern
  `[[example]] mcp_http` already uses for the `mcp-http` feature — so `cargo
  test --no-default-features` simply skips building it instead of failing.

`LEAN_FORBIDDEN` (`make check-lean`'s forbidden-crate set) is **not** changed:
`rhai` was never forbidden there — it was a legitimate lean dependency before
this ADR and stays legitimate (still default-on) after it. The trim is that a
lean build *can* now drop it, not that it must.

**3. Trimmed `syntect` features.** `default-fancy` (`parsing`,
`default-syntaxes`, `default-themes`, `html`, `plist-load`, `yaml-load`,
`dump-load`, `dump-create`, `regex-fancy`) shrinks to `["parsing",
"default-syntaxes", "default-themes", "regex-fancy"]` — `default-syntaxes`
and `default-themes` each already pull in `dump-load` transitively (verified
against syntect 5.3.0's own `[features]` table), so the only functional loss
is `html`/`plist-load`/`yaml-load`/`dump-create`, none of which
`tui/markdown.rs` calls. `regex-fancy` (pure-Rust `fancy-regex`, not
`regex-onig`) is kept deliberately: `onig` needs the oniguruma C library,
which is the wrong trade for a project that cross-compiles the TUI to ARM/RISC-V
targets via `cross` (per the workspace's own cross-compilation conventions) —
this ADR trims *feature scope*, not the regex engine.

## Consequences

### Positive

- Faster incremental/CI compiles for `entanglement-provider` and
  `entanglement-core` in isolation (fewer tokio features to monomorphize/link
  against), and for `entanglement-runtime`'s `tui` feature (smaller syntect
  surface).
- A lean embedder that has no use for the sandboxed-script tool can now shed
  one of the heaviest deps in the tree via a single feature flag, with zero
  change to any existing default build.
- `Cargo.lock` is unchanged in shape — every trim is `features = [...]`
  narrowing plus `optional = true` on `rhai`, no new/removed crate.

### Negative / neutral

- `entanglement-provider`'s `net`/`io-util` tokio features are exercised only
  by `tests/streaming.rs`, not `src/`, but have to live in the crate's single
  `[dependencies]` entry (no `tokio` dev-dependency exists yet to scope them
  to tests only) — accepted as the smallest diff; splitting it out is a
  separate, unrequested structural change.
- `entanglement-runtime`'s tokio feature list is barely smaller than `full`
  (missing only `parking_lot` and `test-util`, neither exercised) — the head
  crate's surface area genuinely spans fs/process/net/io/signal/multi-thread,
  so this trim's real payoff is provider/core, not runtime's own tokio build.
- ADR-0025's "zero `#[cfg(feature)]` in code" property is now doubly
  qualified (the pre-existing `serve`/`cli` exceptions, plus this `rhai` one)
  — still true in spirit (the split is overwhelmingly a `Cargo.toml` fact),
  but no longer literal. Documented here rather than silently drifting.
- `rhai` becoming optional (even though default-on) means a downstream
  embedder building with a custom feature set must remember to opt back in if
  it wants the tool — mitigated by staying in `default`, so only an explicit
  `--no-default-features` invocation is affected.

## Alternatives considered

- **Leave `tokio` at `features = ["full"]`.** Rejected — the whole point of
  the deferred item; `full` on three crates whose combined surface excludes
  `parking_lot`/`test-util`/(for provider+core) `fs`/`process`/`signal` is
  needless compile weight with no correctness benefit, since feature
  unification means the *runtime* binary build sees the same union either
  way — the payoff is standalone/isolated builds of provider and core.
- **Route `Intercept::Rhai` through the generic `Permission` dispatch
  unconditionally (drop the dedicated variant) instead of cfg-gating it.**
  Rejected: `rhai` resolves permission live inside the script task against a
  captured binding-policy snapshot (ADR-0046/ADR-0115/ADR-0129/ADR-0130), not
  through the pluggable `PermissionResolver` seam
  ([ADR-0079](0079-pluggable-permission-resolver-and-grant-store.md), #311)
  the generic path uses — collapsing the routes would be a behavior change
  for the feature-on case, not just a lean-build trim.
- **A blanket `#[cfg(feature = "rhai")]` (or `#[allow(dead_code)]`) at the
  top of `tool_runner.rs`/`permission.rs`.** Rejected in favor of narrowly
  scoping each cfg to the specific import/parameter/function actually made
  dead by the feature's absence — keeps `cargo clippy --no-default-features
  -D warnings` (the `check-lean` gate) meaningful rather than silenced
  wholesale.
- **`syntect` `default-onig` instead of trimming `default-fancy`'s feature
  list.** Rejected — `onig` requires the oniguruma C library, in tension with
  the project's ARM/RISC-V cross-compilation targets (`cross`); this ADR
  keeps the pure-Rust `regex-fancy` engine and only drops the unused
  html/plist/yaml/dump-create features around it.

## References

- Issue [#502](https://github.com/xmiksay/entanglement/issues/502): this ADR's issue
- [Deferred-work ledger](../deferred-work-ledger.md) row 4 (moved to Resolved by this change)
- [ADR-0025](0025-runtime-cargo-feature-gates.md): `entanglement-runtime` cargo feature gates (amended: tokio feature scope, "zero cfg(feature)" claim)
- [ADR-0046](0046-rhai-sandboxed-script-tool.md): `rhai` sandboxed script tool (the tool itself is unchanged; only its compile-time optionality)
- [ADR-0115](0115-rhai-exec-bindings-call-bash.md) / [ADR-0129](0129-thread-the-skill-mask-into-rhai-binding-resolution.md) / [ADR-0130](0130-rhai-exec-bindings-marshal-workdir.md): the `rhai` binding machinery this ADR gates as a unit, unmodified in behavior
