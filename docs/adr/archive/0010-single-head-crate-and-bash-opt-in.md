# 0010. `entanglement-runtime`: the head crate — tools, execution, permissions, sessions

- Status: Accepted (§3's `call`-piggybacks-`bash`'s-gate framing superseded for `call` by [0093](0093-call-registration-independent-of-bash-opt-in.md); `bash`'s own opt-in gate unchanged)
- Date: 2026-07-07

## Context

The head is a thin adapter over [`Holly::send`][0001]/[`Holly::subscribe`][0001]
that exists to produce the `skutter` binary. It has **no library consumers** —
embedders integrate in-process via the ABI, not via a head-as-a-library. So all
transports (stdio today; WebSocket + TUI next) belong in **one** crate, not one
crate each; separate head crates would be false modularity and would duplicate
the shared head infrastructure (provider selection, config wiring).

That crate is also the natural home for everything the *engine* should not own
([ADR-0006][0006]): the concrete host tools, their execution, the permission
decision, the approval UX, and user sessions. Keeping those in core coupled
every embedder to a fixed toolset and put filesystem/shell I/O inside the pure
engine. The runtime is where the final, deployable logic lives.

The crate was historically named `entanglement-cli`; that name is inaccurate now
that it holds WS + TUI and the whole runtime, so it is renamed
**`entanglement-runtime`**. The binary identity stays `skutter`.

## Decision

### 1. One head crate — `entanglement-runtime` — holds every transport

Binary `skutter`, all interfaces as subcommands:

| subcommand | transport | status |
| --- | --- | --- |
| `skutter run [--format text\|json] [--agent <name>]` | stdio one-shot | ✅ ([ADR-0005][0005]) |
| `skutter pipe` | stdio bidirectional NDJSON | ✅ ([ADR-0005][0005]) |
| `skutter serve` | WebSocket (`axum /ws`) | next |
| `skutter tui` | opencode-style terminal UI | next ([ADR-0011][0011]) |

The "four interfaces, one ABI" framing ([ADR-0001][0001]) is unchanged — four
interfaces (in-process ABI + three transports), packaged into one crate. The
only seam the hygiene gate ([ADR-0006][0006]) protects is
`entanglement-core` ↔ everything else.

### 2. The runtime owns tools, execution, permissions, and approval

- **Host tools** (`read`/`glob`/`grep`/`edit`/`bash`, [ADR-0008][0008],
  [ADR-0009][0009]) are *implemented* here, not in core. Core defines the
  `Tool` **trait**; the runtime supplies the implementations.
- **Tool execution** happens here. Core's turn loop emits a tool request over
  the protocol; the runtime runs the tool and returns the output. Execution is
  filesystem/shell I/O and does not belong in the engine.
- **Permission dispatch** (`Allow | Ask | Deny`, [ADR-0003][0003]) is decided
  here: the runtime holds the active `PermissionProfile` and resolves each tool
  request before executing, asking the user on `Ask` via the approval UX
  ([ADR-0014][0014]).
- **User sessions** and their **persistence** ([ADR-0020][0020]) and
  **hierarchy** ([ADR-0021][0021]) live here; core keeps only the per-turn
  session *state* it needs to run the loop.

### 3. `bash` is opt-in

The runtime registers the root-contained quartet (`read`/`glob`/`grep`/`edit`)
by default. `BashTool` runs arbitrary code with full privileges and is **not**
sandboxed ([ADR-0009][0009]), so it is registered only when
`ENTANGLEMENT_ENABLE_BASH=1`, with a logged warning. The opt-in gate controls
*registration* (is the tool advertised at all); the permission profile controls
*dispatch* (Allow/Ask/Deny when the model calls it) — orthogonal.

## Consequences

- **(+)** One binary, one install, one crate to evolve; shared head infra lives
  in one place. Matches opencode / `agent` (single-binary) and `gh`/`cargo`
  (subcommand UX).
- **(+)** Core is reusable: an embedder pairs it with its own tool set and
  permission policy instead of inheriting `skutter`'s.
- **(+)** Default registration is safe without a sandbox: a model can
  read/inspect/edit within the root but cannot run a shell until the operator
  opts in; a networked `serve` head inherits this safe default.
- **(+)** The name `entanglement-runtime` says what the crate is — the runtime
  that wires provider + core together and holds the final logic.
- **(−)** The crate pulls every transport's deps once `serve`/`tui` land
  (`axum`, `ratatui`, `crossterm`). A lean stdio-only build is recoverable later
  via cargo features (`["ws","tui"]`) if a concrete need arises — deferred.
- **(−)** Routing *every* tool call through the protocol to the runtime (not
  just `Ask` ones) adds a round-trip per call. Accepted: it is what makes tools
  a runtime concern rather than a core dependency ([ADR-0006][0006]).
- **(−)** `bash` is still unsandboxed when opted in — the opt-in gate makes the
  default safe but does not close the privilege gap; a real sandbox remains a
  future security-focused decision.

## Alternatives considered

- **Separate head crates (`entanglement-ws`, `entanglement-tui`).** Rejected:
  false modularity — heads have no library consumers ([ADR-0001][0001]); the
  reference projects are single binaries; shared head infra would duplicate.
- **Keep tools/execution/permissions in core.** Rejected: couples every
  embedder to a fixed toolset and puts filesystem/shell I/O in the pure engine
  ([ADR-0006][0006]).
- **Cargo features for `ws`/`tui` now.** Deferred: gating subcommands behind
  `#[cfg(feature)]` (fiddly with clap) solves a dep-weight problem that does not
  exist yet.
- **Keep the `entanglement-cli` name.** Rejected: inaccurate — the crate holds
  WS + TUI and the whole runtime, not just a CLI. `entanglement-runtime` keeps
  the `entanglement-*` library convention; the binary `skutter` keeps the
  user-facing identity (Red Dwarf lore).
- **Rename the crate to `skutter`.** Rejected: reverses the `entanglement-*`
  convention; the binary already carries that identity.
- **Keep `bash` auto-registered, gate at the permission profile only.**
  Rejected: `build` allows by default, so auto-registration would expose an
  unsandboxed shell under the default profile — unacceptable once `serve`
  (networked) ships.

[0001]: 0001-actor-model-abi.md
[0003]: 0003-agent-and-permission-profiles.md
[0005]: 0005-ndjson-stdio-head.md
[0006]: 0006-core-dependency-hygiene-gate.md
[0008]: 0008-host-tools-workdir-and-bounded-output.md
[0009]: 0009-edit-and-bash-host-tools.md
[0011]: 0011-tui-head-ratatui-crossterm.md
[0014]: 0014-tool-approval-inline-modal.md
[0020]: 0020-event-sourced-session-persistence.md
[0021]: 0021-hierarchical-session-model.md
