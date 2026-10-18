# entanglement — Architecture

How the headless engine is structured and how the four interfaces share one
contract. Overview & roadmap in [`../README.md`](../README.md). The *why* behind
each choice here is recorded in the [decision log](adr/README.md) (ADRs).

This document describes the current *what is* — the three-layer dependency
direction `provider ← core ← runtime`
([ADR-0053](adr/0053-invert-core-provider-seam.md), superseding the direction
of [ADR-0006](adr/0006-core-dependency-hygiene-gate.md)'s original gate).
Decided-but-pending work lives in GitHub issues, not inline here.

## Modules

The architecture is split by module, and each module file is an index over the
sub-pages in its own directory (`architecture/<module>/`); every page stays
under the 400-line cap (same convention as source, issue #109), enforced for
all of `docs/` outside the ADR log by `make doc-cap` (#848). The section
numbers (§0–§10) referenced throughout the docs and ADRs map as follows:

| Module | §§ | Topic |
| --- | --- | --- |
| [Layers & the actor model](architecture/layers-and-abi.md) | §0–1 | core/provider/runtime layering + the actor ABI |
| [Wire protocol & structured outputs](architecture/protocol.md) | §2, §4 | `InMsg`/`OutEvent`, Plan/TaskList events |
| [Agents, permissions, skills & prompt](architecture/agents-and-permissions.md) | §3 | agent profiles (identity), permission modes (authority), spawn limits, skills, prompt assembly |
| [Per-session engine](architecture/engine.md) | §5 | turn loop, tool round-trip, steering, cancellation |
| [LLM I/O (provider)](architecture/provider.md) | §5b | streaming client, catalog, pool/retry/rate-limit |
| [Heads & persistence](architecture/heads-and-persistence.md) | §6, §6b–§6d | stdio/TUI/serve heads, event-sourced sessions, managed key file, aux models |
| [Hygiene gates & host tools](architecture/gates-and-host-tools.md) | §7–10 | dependency gates, host tools (model-facing `read`/`write`/`edit`/`apply_patch` + `bash` + `web_fetch`; script-only `glob`/`grep`/`call`), the one advertised surface |

Trust & scope decisions cut across modules: the local trust boundary
([ADR-0047](adr/0047-local-trust-boundary.md)) and the local-only `serve` head
([ADR-0048](adr/0048-serve-head-local-trust-model.md)).

Building a custom, multi-tenant head on top of `entanglement-runtime`'s
embeddable library (rather than one of the four shipped interfaces) is a
separate *how to integrate* guide, not a description of this repo's own
heads: see [`embedding.md`](embedding.md).

Intentionally-deferred work and docs/implementation drift findings are
tracked as GitHub issues, one per item (#689 retired the old standing ledger
file), not re-derived from scratch each audit.
