# entanglement — Architecture

How the headless engine is structured and how the four interfaces share one
contract. Overview & roadmap in [`../README.md`](../README.md). The *why* behind
each choice here is recorded in the [decision log](adr/README.md) (ADRs).

This document describes the current *what is*, with the three-layer direction
([ADR-0006](adr/0006-core-dependency-hygiene-gate.md)) marked inline:
**✅ shipped** vs **🚧 decided but pending** (tracked in GitHub issues).

## Modules

The architecture is split by module — each file stays under the 400-line cap
(same convention as source, issue #109). The section numbers (§0–§8) referenced
throughout the docs and ADRs map as follows:

| Module | §§ | Topic |
| --- | --- | --- |
| [Layers & the actor model](architecture/layers-and-abi.md) | §0–1 | core/provider/runtime layering + the actor ABI |
| [Wire protocol & structured outputs](architecture/protocol.md) | §2, §4 | `InMsg`/`OutEvent`, Plan/TaskList events |
| [Agents, permissions, skills & prompt](architecture/agents-and-permissions.md) | §3 | profiles, tool mask, spawn gating, skills, prompt assembly |
| [Per-session engine](architecture/engine.md) | §5 | turn loop, tool round-trip, steering, cancellation |
| [LLM I/O (provider)](architecture/provider.md) | §5b | streaming client, catalog, pool/retry/rate-limit |
| [Heads & persistence](architecture/heads-and-persistence.md) | §6, §6b | stdio/TUI/serve heads, event-sourced sessions |
| [Hygiene gates & host tools](architecture/gates-and-host-tools.md) | §7–8 | dependency gates, the quintet + exec tools |

Trust & scope decisions cut across modules: the local trust boundary
([ADR-0047](adr/0047-local-trust-boundary.md)) and the local-only `serve` head
([ADR-0048](adr/0048-serve-head-local-trust-model.md)).
