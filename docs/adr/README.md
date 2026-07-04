# Architecture Decision Records

An **ADR** is a short, immutable record of *why* an architecture decision was
made. The [architecture doc](../architecture.md) describes the current state
(*what is*); ADRs are the decision log (*how we got here, and what else we
considered*). The two run in parallel: a decision lands here first, then the
arch doc is updated to reflect it.

## When to write one

Write an ADR for any decision that is **hard to reverse** or that a reader would
reasonably ask *"why?"* about — protocol shapes, crate boundaries, a chosen
pattern over an obvious alternative, security/permission models. Don't write one
for trivial refactors or local naming.

## Format

File name: `NNNN-kebab-case-title.md` (zero-padded, monotonically numbered).
Each record:

```
# NNNN. Title
- Status: Proposed | Accepted | Superseded by ADR-XXXX | Deprecated
- Date: YYYY-MM-DD

## Context
Why this came up — forces, constraints, what the reference projects do.

## Decision
What we chose, precisely.

## Consequences
Positive / negative / neutral effects.

## Alternatives considered
The options rejected and why. (This is the part the arch doc can't carry.)
```

## Status lifecycle

`Proposed` → `Accepted` → (`Superseded by ADR-XXXX` | `Deprecated`). Never edit
an accepted ADR in place — supersede it with a new one that links back.

## Index

| # | Title | Status |
| --- | --- | --- |
| [0001](0001-actor-model-abi.md) | Actor model is the integration ABI | Accepted |
| [0002](0002-session-multiplexed-protocol.md) | Session-multiplexed wire protocol | Accepted |
| [0003](0003-agent-and-permission-profiles.md) | Agent + permission profiles (opencode-style) | Accepted |
| [0004](0004-structured-plan-and-task-events.md) | Structured Plan & TaskList events (profiles + events, both) | Accepted |
| [0005](0005-ndjson-stdio-head.md) | NDJSON stdio head (`run` + `pipe`) | Accepted |
| [0006](0006-core-dependency-hygiene-gate.md) | entanglement-core zero-UI-dep hygiene gate | Accepted |
| [0007](0007-streaming-llm-and-provider-crate.md) | Streaming `Llm` trait + out-of-core `entanglement-llm` provider crate | Accepted |
| [0008](0008-host-tools-workdir-and-bounded-output.md) | Host tools: working-directory root + bounded output | Accepted |
