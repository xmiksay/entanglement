# AGENTS.md

Compact ramp-up for AI agents working in `entanglement`. Every line below is
something you'd plausibly get wrong without being told. For the *why* and depth,
read the authoritative sources it defers to:

- **`.claude/CLAUDE.md`** — the full project brief (stack, crates, contract, conventions, open work). Read this first.
- **`docs/architecture.md`** — deep architecture reference (actor model, wire protocol, heads, host tools).
- **`docs/adr/`** — immutable decision log; the *why* behind every hard-to-reverse choice. Supersede, never edit in place.

## Commands — drive through `make`, NOT raw `cargo`

This is a hard project rule, not a style preference. The Makefile wraps every
command and `make help` lists them. Key targets:

- **`make verify`** — the pre-"done" gate. Equals `check-fmt + tree + lint + test`. Run it before declaring a task complete or pushing.
- **`make tree`** — the **non-obvious** one. It's the dependency-hygiene gate (ADR-0006): `entanglement-core` must pull in **zero** UI/transport crates. Adding `clap`/`axum`/`tower`/`tonic`/`crossterm`/`ratatui`/`reqwest`/`hyper` to `entanglement-core` will make `make verify` fail here even though `cargo build` is green.
- `make test-unit` / `make test-integration` — split suites (`--lib --bins` vs `--test '*'`).
- `make run` / `make run-json` / `make run-tui` — build + run the `skutter` binary one turn (text / NDJSON / TUI).

For a **single test** the Makefile has no target — raw cargo is fine here:
`cargo test -p entanglement-core --lib session::tests::<name>`.

Build jobs are capped at 4 via `.cargo/config.toml`; don't override unless asked.

## The one crate boundary that matters

Workspace = `entanglement-core`, `entanglement-provider`, `entanglement-cli`. The seam
is **core ↔ everything else**:

- **`entanglement-core`** — the actor engine. **Zero UI/transport deps** (enforced by `make tree`). This is where `reqwest`/`clap`/`axum`/`crossterm` are *forbidden*. The `Llm` *trait* lives here; concrete backends do not.
- **`entanglement-provider`** — concrete LLM backends over `reqwest` (may depend on transport). Implements `entanglement_core::Llm`.
- **`entanglement-cli`** — the only head crate, binary **`skutter`**. All transports (stdio today; `serve`/`tui` next) live here (ADR-0010). Note the binary name differs from the crate name.

Heads depend on core, **never the reverse.**

## Code conventions (this repo-specific)

- **Files must not exceed 400 lines of code.** Split long files into modules when they exceed this limit.
- **Tests ship with the change.** Pure logic → in-module `#[cfg(test)] mod tests`; actor/protocol behavior → `entanglement-core/tests/` (`actor.rs`, `host_tools.rs`).
- **No panicking operators on I/O / user / network / config paths** in `entanglement-core`. Propagate with `?` + `.context()`. `.unwrap()`/`.expect()` only in tests or provably-unreachable spots (then `.expect("invariant …")`).
- **Comments: WHY, not WHAT.**
- Rust stable, edition 2021, MSRV 1.82 (pinned via `rust-toolchain.toml`).

## Commit & PR workflow

- **Conventional Commits with a real scope**: `feat(engine): …`, `fix(cli): …`, `docs: …`. No `Co-Authored-By` trailer.
- **Fast-forward only; never commit to `master`.** Work on a feature branch; rebase; push `--force-with-lease` (never `--force`) after a rebase.
- **Hard-to-reverse decisions get an ADR** (`docs/adr/`, next number, immutable) **and** a `docs/architecture.md` update, in the same change.
- Full issue→PR loop (branch → push → PR → address review): see the `/git` skill at `.agents/skills/git/SKILL.md`.

## Runtime env (for `make run`/`skutter`)

With no provider configured, the engine runs on a `DummyLlm` (no network) — this
is the default and is fine for most dev loops. To hit a real backend:

- `ENTANGLEMENT_PROVIDER` = `zai` (primary) | `openai` | `ollama` | `anthropic`; or auto-detected by key presence (z.ai first).
- `<PROV>_API_KEY` / `<PROV>_MODEL` / `<PROV>_BASE` (Ollama is keyless).
- `ENTANGLEMENT_ENABLE_BASH=1` — **opt-in**: the `bash` host tool is registered only when this is set, because it runs **unsandboxed** with the engine's full privileges (ADR-0009). Off by default.
