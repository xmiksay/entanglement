# 0053. Invert the core↔provider seam: provider owns the LLM ABI as a leaf crate

- Status: Accepted
- Date: 2026-07-13
- Supersedes the dependency-direction decision of [0006](0006-core-dependency-hygiene-gate.md) and [0007](0007-streaming-llm-and-provider-crate.md); amends the lean-transport claim of [0025](0025-runtime-cargo-feature-gates.md).

## Context

[ADR-0006](0006-core-dependency-hygiene-gate.md) and
[ADR-0007](0007-streaming-llm-and-provider-crate.md) put the `Llm` **trait** and
its DTOs in `entanglement-core`, and made `entanglement-provider` depend on core
to *implement* the trait. The dependency ran **provider → core**. The stated
goal was a transport-free core: keep `reqwest` out of `entanglement-core` so an
embedder gets a pure, network-free engine.

In practice that direction is backwards for how the crates are meant to be
consumed:

- **`entanglement-provider` is the communication layer.** Its whole job is to
  turn a conversation turn into an API request — pooling, retry, rate-limiting
  ([ADR-0050](0050-per-endpoint-connection-pool-retry-rate-limit.md)), SSE
  parsing — over `reqwest`. `reqwest` is its *essence*, not an incidental dep.
- **Provider was not usable on its own.** Because it implemented a trait defined
  in core, you could not depend on `entanglement-provider` to issue a raw LLM
  query without also pulling in the entire engine crate. A crate that implements
  a trait must depend on the crate that defines it — so the seam direction
  forced the coupling.
- **The intended stack is linear:** provider (raw LLM I/O) is the foundation,
  `entanglement-core` (the reasoning + tool loop) builds on it, and
  `entanglement-runtime` glues a concrete provider + core + host tools +
  transports together. That is `provider ← core ← runtime`, the conventional
  low-level-client-under-framework shape (cf. `hyper` under `axum`).

The one thing ADR-0006 bought that this gives up — a `reqwest`-free core — is
worth less than it appears: core was never runnable without *some* `Llm` backend
anyway, and the two in-tree backends it shipped (`DummyLlm`, `EchoLlm`) moved to
provider with the rest of the ABI.

## Decision

**Invert the seam. `entanglement-provider` becomes a leaf crate that owns the
LLM ABI; `entanglement-core` depends on it.**

- The `Llm` trait, `LlmRequest`/`LlmResponse`/`LlmEvent`/`LlmStream`,
  `LlmSession`, `LlmFactory`, `ToolCall`, `ToolSpec`, `stream_from_response`, and
  the stub backends `DummyLlm`/`EchoLlm` move to
  `entanglement-provider/src/llm.rs`. The wire message types `Message` /
  `MessageRole` move to `entanglement-provider/src/message.rs` (they are part of
  the `LlmRequest` contract; a raw-LLM consumer needs them without the engine).
- `entanglement-provider` **drops its dependency on `entanglement-core`** and
  gains no `entanglement-*` dependency — it is a leaf, usable standalone for raw
  LLM queries against `AnthropicLlm` / `OpenAiLlm`.
- `entanglement-core` **depends on `entanglement-provider`**, keeps the engine
  (`Holly`, protocol, turn loop, `Context`, the `Tool` trait, `EngineConfig`),
  builds `Context` on provider's `Message`, drives `dyn Llm` from the turn loop,
  and **re-exports** the provider ABI (`pub use entanglement_provider::{Llm,
  Message, ToolSpec, …}`) so its heads keep their import paths.
- `entanglement-runtime` depends on **both** core and provider.

### Consequence: core is no longer transport-free

Because `core → provider → reqwest`, `reqwest`/`hyper`/`tower` are now
legitimately in `entanglement-core`'s transitive tree. The hygiene gates are
narrowed accordingly:

- **`make tree`** (was ADR-0006) now forbids only UI/web-server crates in core —
  `clap`/`axum`/`tonic`/`crossterm`/`ratatui` — and **no longer** forbids
  `reqwest`/`hyper`/`tower` (the LLM transport, which arrives via provider).
- **`make check-lean`** (was ADR-0025) now asserts the lean
  (`--no-default-features`) runtime is **CLI/TUI-free**, not transport-free:
  `reqwest` rides in through core, so it is no longer flagged; `clap`/`ratatui`/
  `crossterm`/`syntect`/`pulldown-cmark`/`diffy`/`tracing-subscriber` still are.

## Alternatives considered

- **Extract a third `entanglement-llm` ABI crate** (trait + DTOs, no `reqwest`),
  with provider *and* core depending on it — a diamond that keeps core
  transport-free. Rejected: it adds a crate and its own seam without buying much
  over the linear stack, and the "transport-free core" it preserves was judged
  low-value (see Context). Kept on the table as the fallback if a future
  embedder genuinely needs a `reqwest`-free core.
- **Feature-gate provider's HTTP clients** so core depends on provider with
  `default-features = false` and stays `reqwest`-free. Rejected: `reqwest` is
  the point of the provider crate; a "provider without communication" default is
  a confusing shape, and the transport-free-core guarantee it would preserve is
  the same low-value goal.
- **Leave the seam as `provider → core`.** Rejected: this is the status quo whose
  central defect — provider unusable without the engine — is the whole reason for
  this ADR.

## Consequences

- `entanglement-provider` is now independently testable and consumable for raw
  LLM I/O; its unit + streaming tests reference `entanglement_provider::*` and
  run with no engine present.
- `ToolSpec`/`ToolCall` now live in the provider crate because `LlmRequest`
  carries tools. This is the one arguable placement — "tools" are otherwise an
  engine/runtime vocabulary (see [#206](https://github.com/xmiksay/entanglement/issues/206)).
  It is acceptable today; if it grates, the fix is a small neutral types module,
  not a re-inversion.
- ADR-0006/0007 remain the record of *why* the trait/impl split existed; this ADR
  is the record of why it was inverted. ADR-0025's feature-gate mechanism stands;
  only its "lean runtime is transport-free" property is amended.
- The change is mechanical and reversible: nothing about it is a one-way door.
