# 0180. Build-speed trims round 2: dependency cuts and integration-test consolidation

- Status: Accepted — amends [ADR-0135](0135-deferred-build-speed-trims-tokio-rhai-syntect.md) (continues its build-speed program; none of its decisions are reversed)
- Date: 2026-08-10

## Context

ADR-0135 (issue #502) shipped the first build-speed pass: per-crate tokio
features, an optional `rhai`, a trimmed `syntect`, plus the "safe set"
(lld linker, `line-tables-only`, deps at opt-level 1, `jobs = 4`). A
2026-08-10 re-measurement (cold, jobs=4, scratch target dir) showed where the
remaining time goes:

- `cargo build --workspace`: **130 s wall** / 330 units. Alongside the
  load-bearing heavies (rhai, rustls, tokio, reqwest), a proc-macro tail rode
  in through `url` → `idna` → ICU4X (`zerofrom-derive` 8.1 s, `yoke-derive`
  6.5 s, plus icu_normalizer/icu_properties and their data crates), and
  `encoding_rs` (4.8 s) rode in through reqwest's `charset` feature.
- `cargo test --workspace --no-run`: **+58 s wall** — **74 separate
  integration-test binaries** (core 40, runtime 34), each compiled and linked
  against the full tree, and `make verify`'s `clippy --all-targets` pays for
  every one of them a second time. `entanglement-core`'s shared
  `tests/common` module compiled 40 times over.

A grep-verified usage audit found every other direct dependency load-bearing
and correctly gated.

## Decision

### 1. Dependency cuts (no behavior change)

- **reqwest loses `charset`.** Every endpoint the provider reads is UTF-8
  JSON/SSE; without the feature reqwest's `.text()` falls back to `bytes()` +
  `String::from_utf8_lossy` — identical for UTF-8 bodies. Drops `encoding_rs`.
- **`idna_adapter = "~1.1"`** pinned in `entanglement-provider`. Holds
  `url`→`idna`'s pluggable backend on the unicode-rs implementation instead of
  ICU4X, removing ~15 crates including the two derive macros above. This
  selects a backend, it does not disable IDNA — punycode handling is preserved.
- **`uuid` dropped from `entanglement-provider`.** Its sole use was 32 bytes of
  PKCE/state entropy (`mcp/auth/pkce.rs`) via two `Uuid::new_v4()` calls;
  `getrandom::fill` reads the same OS CSPRNG directly. (The manifest comment
  claiming core "already uses uuid for session ids" was stale — core mints ids
  via `id_gen.rs`, ADR-0164, and never depended on uuid.)
- **`async-trait` moves to core's `[dev-dependencies]`** — only the
  integration-test mock `Llm`s use it; `src/` has zero references.

Cargo.lock: 347 → 328 crates.

### 2. Integration tests consolidate into per-crate harnesses

Both crates gain a single `tests/it/main.rs` declaring one `mod` per former
test file (files moved verbatim to `tests/it/*.rs`); the crate and its
dependency tree now compile and link once per crate instead of once per file.
Module selection replaces binary selection:
`cargo test -p <crate> --test it <mod>`.

- `entanglement-runtime/tests/rhai.rs` **stays a separate binary** — its
  `[[test]] required-features = ["rhai"]` gate (ADR-0135) must keep letting
  `cargo test --no-default-features` skip the build entirely.
- The four feature-guarded runtime files trade their crate-level `#![cfg(…)]`
  for the same gate on the `mod` declaration (`serve`, `serve_auth` →
  `serve`; `session_title` → `provider`; `mcp_http` → `mcp-http` + `serve`).
- **Env-var discipline tightens**: modules now share one address space, so the
  five per-file `static ENV_LOCK`s merged into a single poison-tolerant
  `crate::env_lock()` in the harness root, and the six formerly unlocked
  `set_var` sites (`policy_seam`, `skill_mask`, `system_prompt_assembly`,
  `load_skill`) hold it too. Every writer restores (`remove_var`) before the
  guard drops.

## Consequences

### Positive

- Cold `cargo build --workspace` drops the ICU/encoding_rs/uuid subtrees
  (~19 lockfile crates, including two of the slowest derive macros).
- The test-suite build collapses 74 compile+link units into 3; core's
  `tests/common` compiles once. `clippy --all-targets` gets the same saving
  again. Fewer test processes also start faster under `cargo test`.
- Test-count parity verified: 2151 passed / 0 failed before and after.

### Negative / accepted

- `idna_adapter ~1.1` is a backend pin that must be dropped deliberately if a
  future `url`/`idna` major requires the 1.2+ adapter line.
- Merged modules run in one process: `mcp_http`'s process-wide
  `ENTANGLEMENT_NO_SHARED_ENDPOINT_STATE=1` (set once, never unset — its tests
  must not touch the cross-process endpoint-state files, ADR-0144) can now
  leak to sibling modules. Accepted: no other runtime test constructs an
  endpoint pool, and the leaked setting is the more hermetic direction for a
  test process anyway.
- Per-file test selection (`--test <name>`) becomes module filtering
  (`--test it <name>`); a single test file can no longer be built in isolation.

### Considered and rejected

- **`futures` → `futures-util`**: verified feasible (nothing outside
  futures-util is used anywhere), but it removes only two small crates for
  ~40 import-site rewrites — not worth the churn.
- **Splitting `src/tui/` (27k lines, 38 % of the runtime crate) into its own
  crate**: would cut the runtime crate's 4×-per-test-build compile cost by a
  third and parallelize, but the structural cost (fourth crate, feature-graph
  rewiring, dep-gate and docs updates) was judged not worth it — explicitly
  declined, no follow-up filed.
- Further `syntect`/`tracing-subscriber` trims: behavior loss (bundled
  syntax/theme dumps, `RUST_LOG` directives).
