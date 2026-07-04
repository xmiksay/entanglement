# 0006. brain-core zero-UI-dep hygiene gate

- Status: Accepted
- Date: 2026-07-04

## Context

PLAN.md calls out as a top risk: *"accidentally coupling core logic to CLI
crates."* The entire project premise is a **headless core** that stays
UI/transport-agnostic. Once `clap`, `crossterm`, or `axum` leaks into
`brain-core`, every embedder drags in a UI stack, and the seam is gone.

## Decision

`brain-core` depends **only** on: `tokio`, `serde`, `serde_json`, `async-trait`,
`anyhow`, `thiserror`, `tracing`, `futures`. It must **never** pull in
`clap`, `axum`, `tower`, `tonic`, `crossterm`, `ratatui`, `tui`, or any other
UI/transport crate.

This is enforced automatically:

- Heads live in **separate crates** (`brain-stdio`, future `brain-ws`,
  `brain-cli`) that depend on `brain-core`, never the reverse.
- `make tree` runs `cargo tree -p brain-core` and greps for forbidden crates; it
  is part of `make verify`'s CI-equivalent gate.

## Consequences

- **(+)** A structural guarantee of the headless seam, not just a convention.
- **(+)** `make tree` is a fast, automatable CI check.
- **(+)** Embedders link `brain-core` without any UI baggage.
- **(−)** More crates to version/publish — a minor cost for the guarantee.

## Alternatives considered

- **Feature-gate UI deps behind cargo features in one crate.** Rejected: weaker
  enforcement — it's easy to forget `--no-default-features`, and a stray `use
  clap::...` compiles fine until someone disables the feature.
- **Rely on code review only.** Rejected: no automation; the risk is exactly the
  kind of slow drift review misses.
- **A `cargo-deny` config instead of `make tree`.** Reasonable future addition
  for stronger rules, but `make tree` is zero-dependency and sufficient today.
