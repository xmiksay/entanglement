# entanglement Architecture — LLM I/O (provider crate)

> Part of the [architecture overview](../architecture.md). The *why* behind each choice is in the [decision log](../adr/README.md).

This module is split into the pages below, one per seam; each stays under the 400-line doc cap (`make doc-cap`). Every section heading the single-file version had is kept here, pointing at where that section now lives.

| page | covers |
| --- | --- |
| [llm-trait](provider/llm-trait.md) | §5b — The `Llm` trait, TLS & provider topology |
| [surface-and-resilience](provider/surface-and-resilience.md) | §5b — Advertised surface, web search & per-endpoint resilience |
| [timeouts-and-shared-state](provider/timeouts-and-shared-state.md) | §5b — Timeouts, throttle visibility & cross-process state |
| [catalog](provider/catalog.md) | §5b — Provider/model catalog & advanced wire facts |
| [thinking-and-selection](provider/thinking-and-selection.md) | §5b — Extended thinking, provider selection & multi-user |

## 5b. LLM I/O (`entanglement-provider`) — [ADR-0007](../adr/0007-streaming-llm-and-provider-crate.md), [ADR-0053](../adr/0053-invert-core-provider-seam.md)

Now in [§5b — The `Llm` trait, TLS & provider topology](provider/llm-trait.md#5b-llm-io-entanglement-provider--adr-0007-adr-0053).
Continued in [§5b — Advertised surface, web search & per-endpoint resilience](provider/surface-and-resilience.md).
Continued in [§5b — Timeouts, throttle visibility & cross-process state](provider/timeouts-and-shared-state.md).
Continued in [§5b — Provider/model catalog & advanced wire facts](provider/catalog.md).

### Advanced catalog (wire facts)

Now in [§5b — Provider/model catalog & advanced wire facts](provider/catalog.md#advanced-catalog-wire-facts).

### Extended thinking: capture and replay

Now in [§5b — Extended thinking, provider selection & multi-user](provider/thinking-and-selection.md#extended-thinking-capture-and-replay).
