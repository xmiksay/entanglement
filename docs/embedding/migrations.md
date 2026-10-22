# Embedding Migration notes

> Part of the [embedding guide](../embedding.md).

Split into the pages below to stay under the 400-line doc cap (`make doc-cap`). Upgrading from 0.6.0, start with [agents, modes & permissions](migrations/agents-modes-and-permissions.md). The sections of the other two pages once lived in this file, so their headings are kept below, pointing at where each now lives.

| page | covers |
| --- | --- |
| [agents-modes-and-permissions](migrations/agents-modes-and-permissions.md) | agents, modes, permissions & the other 0.6 → 0.7 breaks before ADR-0226 (ADR-0207 family, ADR-0213, ADR-0161, #724, ADR-0181) — **start here** |
| [one-surface-and-rhai](migrations/one-surface-and-rhai.md) | one surface, binding path & skills (ADR-0226–0230) |
| [cards-and-sessions](migrations/cards-and-sessions.md) | cards, lineages & sessions (ADR-0233–0248) |

## Migrating to one advertised surface (ADR-0226, #818)

Now in [one surface, binding path & skills (ADR-0226–0230)](migrations/one-surface-and-rhai.md#migrating-to-one-advertised-surface-adr-0226-818).

## Migrating to one binding path (ADR-0227, #819)

Now in [one surface, binding path & skills (ADR-0226–0230)](migrations/one-surface-and-rhai.md#migrating-to-one-binding-path-adr-0227-819).

## Migrating to `rhai(path)` and skill tools' removal (ADR-0228, #820)

Now in [one surface, binding path & skills (ADR-0226–0230)](migrations/one-surface-and-rhai.md#migrating-to-rhaipath-and-skill-tools-removal-adr-0228-820).

## Migrating to every skill model-visible (ADR-0229, #845)

Now in [one surface, binding path & skills (ADR-0226–0230)](migrations/one-surface-and-rhai.md#migrating-to-every-skill-model-visible-adr-0229-845).

## Structured card reason and honest scopes (ADR-0230, #836/#846)

Now in [one surface, binding path & skills (ADR-0226–0230)](migrations/one-surface-and-rhai.md#structured-card-reason-and-honest-scopes-adr-0230-836846).

## Live-only nested-call events (ADR-0233, #821)

Now in [cards, lineages & sessions (ADR-0233–0248)](migrations/cards-and-sessions.md#live-only-nested-call-events-adr-0233-821).

## Detached cards (ADR-0237, #872)

Now in [cards, lineages & sessions (ADR-0233–0248)](migrations/cards-and-sessions.md#detached-cards-adr-0237-872).

## Cards moved after a compaction (ADR-0238, #871)

Now in [cards, lineages & sessions (ADR-0233–0248)](migrations/cards-and-sessions.md#cards-moved-after-a-compaction-adr-0238-871).

## A card's end on the wire (ADR-0240, #883)

Now in [cards, lineages & sessions (ADR-0233–0248)](migrations/cards-and-sessions.md#a-cards-end-on-the-wire-adr-0240-883).

## Heals walk the whole compaction chain (ADR-0242, #888/#887)

Now in [cards, lineages & sessions (ADR-0233–0248)](migrations/cards-and-sessions.md#heals-walk-the-whole-compaction-chain-adr-0242-888887).

## Linear lineages and live script authority (ADR-0243, #898/#899/#900)

Now in [cards, lineages & sessions (ADR-0233–0248)](migrations/cards-and-sessions.md#linear-lineages-and-live-script-authority-adr-0243-898899900).

## Seq order, leases and one task per id (ADR-0244, #891/#892/#893)

Now in [cards, lineages & sessions (ADR-0233–0248)](migrations/cards-and-sessions.md#seq-order-leases-and-one-task-per-id-adr-0244-891892893).

## A decision only answers its own kind of card (ADR-0245, #895/#894)

Now in [cards, lineages & sessions (ADR-0233–0248)](migrations/cards-and-sessions.md#a-decision-only-answers-its-own-kind-of-card-adr-0245-895894).

## Resumed sub-agents and the teardown tail (ADR-0246, #905/#906)

Now in [cards, lineages & sessions (ADR-0233–0248)](migrations/cards-and-sessions.md#resumed-sub-agents-and-the-teardown-tail-adr-0246-905906).

## Attended sessions wait for their approver (ADR-0247, #828)

Now in [cards, lineages & sessions (ADR-0233–0248)](migrations/cards-and-sessions.md#attended-sessions-wait-for-their-approver-adr-0247-828).

## Importable rhai modules (ADR-0248, #835)

Now in [cards, lineages & sessions (ADR-0233–0248)](migrations/cards-and-sessions.md#importable-rhai-modules-adr-0248-835).

---

Previous: [§5–§7 — Approvals across restart, MCP & multi-user](approvals-mcp-multi-user.md) · Next: [TLS & dependency pinning](tls-and-pinning.md)
