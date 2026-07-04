# 0010. Single head crate (`entanglement-cli`) + `bash` opt-in gate

- Status: Accepted
- Date: 2026-07-04
- Supersedes: the packaging sub-claim of [ADR-0006][0006] ("Heads live in
  separate crates … future `entanglement-ws`, `entanglement-cli`") and the
  wiring sub-claim of [ADR-0009][0009] ("`skutter` always registers the
  quintet"). The hygiene-gate core of ADR-0006 and the `edit`/`bash` tool
  design of ADR-0009 both stand.

## Context

Two accepted decisions needed revisiting together, because both are about how
the **head** is packaged and wired:

1. **ADR-0006** assumed each head lives in its own crate (`entanglement-stdio`
   today; `entanglement-ws` and `entanglement-cli` later). But a head is a thin
   adapter over [`Holly::send`]/[`Holly::subscribe`] (ADR-0001) that exists
   only to produce the `skutter` binary. It has **no library consumers** —
   embedders integrate in-process via the ABI, not via `entanglement-ws`-as-a-
   library. A crate with one binary consumer and no library API is false
   modularity, and it would duplicate shared head infrastructure (the provider
   selection + config wiring already in `build_config()`) across three crates.

2. **ADR-0009** shipped `bash` unsandboxed with the engine's full privileges
   and wired it into the default `host_tools(root)` quintet. With a networked
   head (the forthcoming `serve` subcommand) on the horizon, auto-registering
   an unsandboxed shell is the wrong default.

## Decision

### 1. One head crate — `entanglement-cli` — holds every transport

The single crate `entanglement-cli` (binary `skutter`) contains all heads as
subcommands:

| subcommand | transport | status |
| --- | --- | --- |
| `skutter run [--format text\|json] [--agent <name>]` | stdio one-shot | ✅ (ADR-0005) |
| `skutter pipe` | stdio bidirectional NDJSON | ✅ (ADR-0005) |
| `skutter serve` | WebSocket (`axum /ws`) | next |
| `skutter tui` | opencode-style terminal UI | next |

The "four interfaces, one ABI" framing (ADR-0001) is unchanged — there are
still four interfaces (in-process ABI + three transports); they are just
**packaged into one crate**, not one crate each. The real seam, and the only
one the hygiene gate (ADR-0006) actually protects, is `entanglement-core` ↔
everything else.

### 2. `bash` is opt-in; `host_tools` returns the root-contained quartet

`host_tools(root)` now registers `read`/`glob`/`grep`/`edit` only. `BashTool`
is re-exported from `entanglement-core` and a head opts in explicitly:
`skutter` registers it when `ENTANGLEMENT_ENABLE_BASH=1`, and logs that it is
unsandboxed. `edit` stays in the default set because it is root-contained
(`resolve_under_root` rejects `..` escape, ADR-0008); `bash` is the one tool
that runs arbitrary code with full privileges, so it is the one that is gated.

## Consequences

- **(+)** One binary, one install, one crate to evolve; shared head infra
  (provider selection, logging, `host_tools` wiring) lives in one place.
  Matches opencode / `agent` (single-binary references) and `gh`/`cargo`
  (subcommand UX).
- **(+)** Default head registration is safe without a sandbox: a model can
  read/inspect/edit within the root, but cannot run a shell until the operator
  explicitly opts in. A networked `serve` head inherits this safe default.
- **(+)** ADR-0006's hygiene gate is unchanged — `entanglement-core` still has
  zero UI/transport deps; the head crate is where `axum`/`ratatui`/`crossterm`
  will land when `serve`/`tui` arrive.
- **(−)** `entanglement-cli` pulls every transport's deps once `serve`/`tui`
  land (`axum`, `ratatui`, `crossterm`). A lean stdio-only CI binary is no
  longer free. Recoverable later via Cargo features (`["ws", "tui"]`) if a
  concrete need arises — deliberately deferred to avoid speculative
  complexity.
- **(−)** `bash` is still unsandboxed when opted in — this ADR does **not**
  close the privilege gap from ADR-0009; it only makes the default safe. A
  real sandbox remains a future security-focused decision.
- **(−)** The crate name `entanglement-cli` is generic (holds WS + TUI too,
  not just a "CLI" in the narrow sense). Chosen over `entanglement-stdio`
  (inaccurate post-merge) and `skutter` (rejected to keep the `entanglement-*`
  library convention; the binary identity `skutter` is preserved).

## Alternatives considered

- **Separate head crates (`entanglement-ws`, `entanglement-cli`-as-TUI).**
  Rejected: false modularity — heads have no library consumers by design
  (embedders use the ABI, ADR-0001); the reference projects are single
  binaries; shared head infra would duplicate.
- **Cargo features for `ws`/`tui` now, within the merged crate.** Deferred:
  would gate subcommands behind `#[cfg(feature)]` (fiddly with clap) to solve
  a dep-weight problem that doesn't exist yet. Add later only if a lean
  stdio-only build becomes a real requirement.
- **A real `bash` sandbox now** (bubblewrap / namespaces / seccomp). Deferred
  per ADR-0009; this ADR makes the default safe in the meantime via the opt-in
  gate, which is belt-and-suspenders until a sandbox lands.
- **Keep `bash` auto-registered; gate at the permission profile only.**
  Rejected: `build` allows by default, so auto-registration would expose an
  unsandboxed shell to any model under the default profile — unacceptable,
  especially once `serve` (networked) ships.
- **Rename the merged crate to `skutter`.** Rejected: reverses the
  `entanglement-*` library convention the project deliberately keeps; the
  binary `skutter` already carries the user-facing identity + Red Dwarf lore.

[0001]: 0001-actor-model-abi.md
[0006]: 0006-core-dependency-hygiene-gate.md
[0009]: 0009-edit-and-bash-host-tools.md
