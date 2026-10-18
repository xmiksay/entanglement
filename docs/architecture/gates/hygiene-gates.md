# Gates & host tools §7 — Hygiene gates

> Part of the [gates & host tools module](../gates-and-host-tools.md) of the [architecture overview](../../architecture.md).

## 7. Hygiene gates — [ADR-0006](../../adr/0006-core-dependency-hygiene-gate.md) + [ADR-0053](../../adr/0053-invert-core-provider-seam.md) (`tree`), [ADR-0025](../../adr/0025-runtime-cargo-feature-gates.md) + [ADR-0053](../../adr/0053-invert-core-provider-seam.md) (`check-lean`)

`entanglement-core` must stay free of UI/web-server deps. Enforced by
`make tree`, which runs `cargo tree -e normal -p entanglement-core` and **fails**
if a forbidden crate appears — ADR-0053's named set
(`clap`/`axum`/`tonic`/`crossterm`/`ratatui`) plus the web/websocket stacks a
name blocklist must also cover (`warp`/`actix`/`rocket`/`tungstenite`/`ureq`,
issue #207). Since [ADR-0053](../../adr/0053-invert-core-provider-seam.md) inverted
the seam, core depends on `entanglement-provider`, so `reqwest`/`hyper`/`tower`
(the LLM transport) are now **legitimately** in core's transitive tree and are
not forbidden. It is part of `make verify`. Current core direct deps:
`entanglement-provider`, `tokio`, `serde`, `serde_json`, `async-trait`, `anyhow`,
`thiserror`, `tracing`, `futures`, `uuid`. `glob`/`regex`/`ignore` (which back the host
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
`rhai` is deliberately **not** in `LEAN_FORBIDDEN` — the sandboxed script tool
lives behind its own default-on `rhai` feature (#502,
[ADR-0135](../../adr/0135-deferred-build-speed-trims-tokio-rhai-syntect.md)
amending ADR-0025; the tool itself is documented in §8, [host tools](host-tools.md)), so
`--no-default-features` now excludes it (a
lean embedder that never registers the tool sheds one of the heaviest
always-compiled deps) while every default build — including the `skutter`
binary — is unaffected. Each of the three crates' `tokio` dependency also
dropped `features = ["full"]` for a per-crate minimal list grep-verified
against its own API surface (ADR-0135); `syntect` (behind `tui`) trimmed
`default-fancy` down to `parsing`/`default-syntaxes`/`default-themes`/
`regex-fancy` (drops `html`/`plist-load`/`yaml-load`/`dump-create`, none used
by `tui/markdown.rs`) — neither trim touches this gate's forbidden-crate set.

Both gates share one mechanism, [`scripts/dep-gate.sh`](../../../scripts/dep-gate.sh)
(issue #207): the Makefile supplies the forbidden set (`CORE_FORBIDDEN` /
`LEAN_FORBIDDEN`) and the `cargo tree` selectors; the script unifies edge policy
(normal edges only — build/dev/proc-macro deps are excluded so they neither trip
nor mask the gate) and **hard-fails on a `cargo tree` error or empty output**.
That last point fixes the gates' original defect: they piped `cargo tree` through
`2>/dev/null` and never checked its exit status, so a *failed* `cargo tree`
grepped clean and passed **vacuously**. `make test-gates` runs
[`scripts/dep-gate.test.sh`](../../../scripts/dep-gate.test.sh), a stubbed-`cargo`
self-test that pins the vacuous-pass fix (plus the `userid`, `file-cap` and
`doc-cap` gates' self-tests). It runs with the default tools, then `POSIXLY_CORRECT=1`
(strict POSIX GNU grep/find), then `AWK='gawk --posix'` and `AWK=mawk` when
each is installed (mawk ignores `POSIXLY_CORRECT`, so strict-POSIX awk needs
its own pass, #779) — and CI installs both awks and runs it as its own step
(`REQUIRE_AWKS=1`: a missing awk fails rather than skips), so
a gate leaning on a GNU extension fails loudly
(#770: `userid-gate.sh`'s trailing GNU-only `--include` became a filename
operand under strict POSIX, grep's error exit read as "no match", and the gate
passed vacuously; it is now POSIX `find` + `grep` with grep errors fatal, and a
target directory with no `.rs` files fails too, #779).
`cargo-deny` bans (ADR-0006's stated
future) were considered but **not** adopted: they evaluate the whole workspace
graph and can't scope a rule to one crate's subtree, so they cannot express
"forbidden in core but fine in runtime" (`clap`/`crossterm`/`ratatui` live
legitimately in the full runtime graph, and `axum` is reserved for the future
`serve` head) — the per-crate `cargo tree -p` subgraph is exactly what they lack.

**Doc cap (#848).** `make doc-cap` ([`scripts/doc-cap-gate.sh`](../../../scripts/doc-cap-gate.sh),
in `make verify`) holds the prose docs to the same 400-line cap as source:
every `docs/**/*.md` except the immutable `docs/adr/` log, counted with plain
`wc -l`, with no allowlist. An over-cap doc is split along a section seam into
a sub-page directory named after it, and the original file stays as the
index, keeping its old section headings as pointer stubs so every inbound
anchor still resolves. The gate is fail-closed: paths reach the checker via
`find -exec`, never word splitting, so spaces, newlines and glob characters
are inert; `find -L` follows symlinks, so a symlinked `.md` is counted through
its target and a symlinked directory is descended into, and an unreadable
file, dangling link or symlink loop fails it, as does a missing or
Markdown-free docs dir (a second argument, a `-`-leading dir or a glob
character in it is a usage error, exit 2). Its self-tests (nested `x/adr/` not exempt, trailing-slash
dir, newline-in-name, symlinked/dangling files, symlinked dir, loop, usage)
ride `make test-gates` with the others.

**CI (issue #107).** Both gates now run in GitHub Actions
([`.github/workflows/`](../../../.github/workflows)), driven through the same `make`
targets. `ci.yml` runs `make verify` (`check-fmt` + `tree` + `check-lean` +
`file-cap` + `doc-cap` + `userid` + `lint` + `test`) on every PR and every push to `master` — the first time the
`tree`/`check-lean` hygiene gates run automatically rather than at developer
discretion. `release.yml` fires on a `v*` tag: it runs `make verify` and then a
coverage job, `make coverage` (`cargo llvm-cov --workspace`, fails under
`COV_MIN`% — baselined from the first measured run and ratcheted up, never
lowered), uploading the lcov + Cobertura reports as an artifact so a release is
blocked on green tests with a coverage report attached. Both cache cargo
artifacts (`Swatinem/rust-cache`) and inherit the committed `CARGO_BUILD_JOBS=4`
cap from `.cargo/config.toml`.

---

Next: [§8 — Host tools: the tool set, containment & sandbox](host-tools.md)
