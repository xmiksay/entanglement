# 0006. Layering: core / provider / runtime, and the core hygiene gate

- Status: Accepted — dependency direction superseded by [0053](0053-invert-core-provider-seam.md)
- Date: 2026-07-07

> **Amended by [ADR-0053](0053-invert-core-provider-seam.md) (2026-07-13).** The
> seam was inverted to `provider ← core ← runtime`: `entanglement-core` now
> depends on `entanglement-provider`, so `reqwest`/`hyper`/`tower` are
> legitimately in core's transitive tree and the `make tree` gate now forbids
> only UI/web-server crates. The layering rationale below still holds; only the
> dependency direction and the transport-free-core rule changed.

## Context

`entanglement` is a headless engine. The founding risk (PLAN.md) is
*"accidentally coupling core logic to CLI/transport crates."* If `clap`,
`crossterm`, `axum`, or `reqwest` leaks into `entanglement-core`, every embedder
drags in a UI/transport stack and the headless seam is gone.

Beyond that one rule, experience with the codebase showed a second, subtler
drift: **`entanglement-core` accreted responsibilities that are not the engine's
job** — it owned the concrete host-tool implementations, executed those tools,
decided permissions, and instantiated a per-session HTTP LLM client. That makes
core hard to reuse (an embedder inherits a fixed toolset and permission model)
and blurs where I/O lives. The layering below settles *what each crate is for*,
not just *what core may not depend on*.

## Decision

### Three layers, two seams

```
┌───────────────────── entanglement-runtime (head, binary `skutter`) ──────────────────┐
│  user sessions · host tools (read/glob/grep/edit/bash) · tool execution ·            │
│  permission dispatch (Allow/Ask/Deny) · approval UX · persistence · transports       │
└───────────────▲───────────────────────────────────────────────────▲──────────────────┘
                │ tool exec + approval (over the protocol)            │ send/subscribe (ABI)
┌───────────────┴───────────────── entanglement-core (engine) ────────┴──────────────────┐
│  Holly actor · InMsg/OutEvent protocol · agent turn loop · Tool *trait* · Context      │
│  · AgentProfile shape · Plan/TaskList snapshots                                        │
└───────────────▲────────────────────────────────────────────────────────────────────────┘
                │ Llm trait: stream() + session handle
┌───────────────┴───────────────── entanglement-provider (LLM I/O) ───────────────────────┐
│  OpenAI-compat + Anthropic clients · connection pool · retry/backoff · rate-limit ·     │
│  reasoning/thinking stream · models-per-provider                                        │
└──────────────────────────────────────────────────────────────────────────────────────────┘
```

- **`entanglement-core`** owns the *reasoning* work only: the actor
  ([ADR-0001][0001]), the wire protocol ([ADR-0002][0002]), the agent turn loop,
  the `Tool` **trait** (not its implementations), `Context`, and the
  `AgentProfile` shape. It is pure and reusable — no I/O, no concrete tools, no
  UI.
- **`entanglement-provider`** owns all LLM I/O ([ADR-0007][0007]): the concrete
  backends behind the `Llm` trait, plus the connection pool, retry, rate-limit,
  and reasoning-stream handling. Reusable by any embedder.
- **`entanglement-runtime`** owns the *head* ([ADR-0010][0010]): host tools and
  their execution, permission dispatch and approval UX, user sessions and their
  persistence, and every transport. This is where the final, deployable logic
  lives.

**Two seams.** Core talks *down* to the provider through the `Llm` trait
(streamed tokens + a session/connection handle). Core talks *up* to the runtime
through the protocol: it emits a tool request and the runtime executes the tool
and returns the output — the same mechanism that already carries approval
([ADR-0003][0003]). Core never links the provider or the runtime; both depend on
core, never the reverse.

### The core hygiene gate

`entanglement-core` depends **only** on: `tokio`, `serde`, `serde_json`,
`async-trait`, `anyhow`, `thiserror`, `tracing`, `futures`, `uuid`. It must
**never** pull in `clap`, `axum`, `tower`, `tonic`, `crossterm`, `ratatui`,
`reqwest`, `hyper`, or any other UI/transport crate.

- Enforced by `make tree` (`cargo tree -p entanglement-core` grepped for the
  forbidden list); part of `make verify`.
- Once the host tools move to the runtime ([ADR-0008][0008], [ADR-0010][0010]),
  core also sheds `glob` and `regex` — they belong with the tool
  implementations, not the engine.

## Consequences

- **(+)** Each layer is independently reusable: an embedder can take core +
  provider and supply its own tools/permissions, or take core alone and supply
  its own provider.
- **(+)** I/O has one home per direction — LLM I/O in the provider, filesystem
  and shell I/O in the runtime. Core stays a pure state machine that is trivial
  to test with a scripted `Llm` and scripted tool results.
- **(+)** The hygiene gate is a structural guarantee, not a convention, and a
  fast CI check.
- **(−)** The core→runtime tool seam adds a protocol round-trip for *every* tool
  call, not just the ones that need approval (see [ADR-0010][0010]). Accepted:
  it is what makes tools a runtime concern rather than a core dependency.
- **(−)** More crates to version. A minor cost for the guarantee and the reuse.

## Alternatives considered

- **Keep tools, execution, and permissions in core (the prior design).**
  Rejected: it couples every embedder to a fixed toolset and permission model
  and puts filesystem/shell I/O inside the "pure" engine. The reuse story is the
  whole point of a headless core.
- **Feature-gate UI/transport deps behind cargo features in one crate.**
  Rejected: weak enforcement — easy to forget `--no-default-features`; a stray
  `use clap::…` compiles until someone disables the feature.
- **Rely on code review for the hygiene rule.** Rejected: no automation; slow
  drift is exactly what review misses. `make tree` catches it in CI.
- **A `cargo-deny` config instead of `make tree`.** A reasonable future
  addition for richer rules, but `make tree` is zero-dependency and sufficient.

[0001]: 0001-actor-model-abi.md
[0002]: 0002-session-multiplexed-protocol.md
[0003]: 0003-agent-and-permission-profiles.md
[0007]: 0007-streaming-llm-and-provider-crate.md
[0008]: 0008-host-tools-workdir-and-bounded-output.md
[0010]: 0010-single-head-crate-and-bash-opt-in.md
