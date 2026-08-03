# AGENTS.md

Compact ramp-up for AI agents working in `entanglement`. Every line below is
something you'd plausibly get wrong without being told. For the *why* and depth,
read the authoritative sources it defers to:

- **`.claude/CLAUDE.md`** — the full project brief (stack, crates, contract, conventions, open work). Read this first.
- **`docs/architecture.md`** — architecture reference, now a per-module index under `docs/architecture/` (actor model, wire protocol, heads, host tools), each module under the 400-line cap.
- **`docs/adr/`** — immutable decision log; the *why* behind every hard-to-reverse choice. Supersede, never edit in place.

## Commands — drive through `make`, NOT raw `cargo`

This is a hard project rule, not a style preference. The Makefile wraps every
command and `make help` lists them. Key targets:

- **`make verify`** — the pre-"done" gate. Equals `check-fmt + tree + check-lean + file-cap + lint + test`. Run it before declaring a task complete or pushing.
- **`make tree`** — the **non-obvious** one. It's the dependency-hygiene gate (ADR-0006, amended by ADR-0053): `entanglement-core` must pull in **zero** UI/web-server crates. Adding `clap`/`axum`/`warp`/`actix`/`rocket`/`tonic`/`tungstenite`/`crossterm`/`ratatui`/`ureq` to `entanglement-core` will make `make verify` fail here even though `cargo build` is green. `reqwest`/`hyper`/`tower` are **not** forbidden — they ride in legitimately via provider (ADR-0053).
- **`make file-cap`** — enforces the 400-line file cap below (issue #451). A currently-over-cap file must be listed in `scripts/file-cap-allowlist.txt` (grandfathered debt) or the gate fails; splitting a file below the cap requires deleting its row in the same change, or the gate fails the other way (a stale allowlist entry).
- `make test-unit` / `make test-integration` — split suites (`--lib --bins` vs `--test '*'`).
- `make run` / `make run-json` / `make run-tui` — build + run the `skutter` binary one turn (text / NDJSON / TUI). `make inspect ARGS=…` prints the resolved prompt/agents/skills with no engine; `make sessions` lists past sessions.

For a **single test** the Makefile has no target — raw cargo is fine here:
`cargo test -p entanglement-core --lib session::tests::<name>`.

Build jobs are capped at 4 via `.cargo/config.toml`; don't override unless asked.

## The one crate boundary that matters

Workspace = `entanglement-core`, `entanglement-provider`, `entanglement-runtime`. Dependency
direction is `provider (leaf) ← core ← runtime` (ADR-0053, inverting ADR-0006/0007):

- **`entanglement-provider`** — the **leaf** crate (no `entanglement-*` deps): owns the LLM ABI — the `Llm` *trait* + DTOs — plus all concrete backends over `reqwest`. Usable standalone.
- **`entanglement-core`** — the actor engine. Depends on provider, drives `dyn Llm`, and re-exports the ABI. **Zero UI/web-server deps** (enforced by `make tree`): `clap`/`axum`/`crossterm`/`ratatui` are *forbidden*, but `reqwest`/`hyper`/`tower` ride in transitively via provider and are allowed (ADR-0053).
- **`entanglement-runtime`** — the only head crate, binary **`skutter`**. All transports (stdio, `tui`, and the WebSocket `serve` head — all shipped) live here (ADR-0010). Note the binary name differs from the crate name.

Heads depend on core, **never the reverse.**

## Code conventions (this repo-specific)

- **Files must not exceed 400 lines of code.** Split long files into modules when they exceed this limit. Enforced by `make file-cap` (issue #451) — see `scripts/file-cap-allowlist.txt` for the shrinking list of grandfathered violations.
- **Tests ship with the change.** Pure logic → in-module `#[cfg(test)] mod tests`; actor/protocol behavior → `entanglement-core/tests/` (e.g. `actor.rs`, `turn_loop.rs`); runtime/host-tool behavior → `entanglement-runtime/tests/`.
- **No panicking operators on I/O / user / network / config paths** in `entanglement-core`. Propagate with `?` + `.context()`. `.unwrap()`/`.expect()` only in tests or provably-unreachable spots (then `.expect("invariant …")`).
- **Comments: WHY, not WHAT.**
- Rust stable, edition 2021, MSRV 1.82 (`rust-version` in the workspace `Cargo.toml`; `rust-toolchain.toml` pins only `channel = "stable"`).

## Commit & PR workflow

- **Conventional Commits with a real scope**: `feat(engine): …`, `fix(cli): …`, `docs: …`. No `Co-Authored-By` trailer.
- **Fast-forward only; never commit to `master`.** Work on a feature branch; rebase; push `--force-with-lease` (never `--force`) after a rebase.
- **Hard-to-reverse decisions get an ADR** (`docs/adr/`, next number, immutable) **and** a `docs/architecture.md` update, in the same change.
- **Never touch `CHANGELOG.md` in a feature/fix PR.** The changelog is written once, at release time, generated from `git log <last-tag>..HEAD` + the release's closed issues ([`docs/releasing.md`](docs/releasing.md)). Per-PR `[Unreleased]` entries conflict on every concurrent merge — leave the file alone.
- Full issue→PR loop (branch → push → PR → address review): see the `/git` skill at `.agents/skills/git/SKILL.md`.
- **Cutting a release**: `make tag VERSION=v0.1.x` (refuses dirty tree / red `make verify` / a version that doesn't match `workspace.package.version`), then `git push origin v0.1.x`. Full runbook, including the one-time crates.io Trusted Publishing setup: [`docs/releasing.md`](docs/releasing.md).

## Runtime env (for `make run`/`skutter`)

With no provider configured, the engine runs on an `EchoLlm` (no network) — this
is the default and is fine for most dev loops. To hit a real backend:

- `ENTANGLEMENT_PROVIDER` = `zai` (primary) | `openai` | `ollama` | `anthropic` | `gemini` | `echo`; or auto-detected by key presence (z.ai first).
- `<PROV>_API_KEY` / `<PROV>_MODEL` / `<PROV>_API_BASE` (legacy `<PROV>_BASE` still accepted; Ollama is keyless).
- `ENTANGLEMENT_ENABLE_BASH=1` — **opt-in**: registers `bash`; its background jobs (`run_in_background=true`) are joined with the always-available runtime-owned `poll` tool (#605), not a paired registry tool. Off by default. `call` (argv exec, no shell) is registered **unconditionally** (ADR-0093). Both run unsandboxed by default but can be bubblewrap-confined via `ENTANGLEMENT_SANDBOX=bwrap` (ADR-0104/0134). The sandboxed `rhai` script tool needs no opt-in.
