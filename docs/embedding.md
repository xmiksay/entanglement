# Embedding `entanglement` — a multi-tenant guide

How to build a **custom head** on top of the engine: one process serving many
users, each isolated, without forking anything. This is the path
`entanglement-runtime --no-default-features` exists for
([ADR-0025](adr/0025-runtime-cargo-feature-gates.md)) — the lean library gives
you the tool-execution loop, permission dispatch, sub-agent spawn, and
persistence machinery with zero CLI/TUI/transport weight. Everything below is exercised by compiling, runnable examples — the
multi-tenant policy seam in
[`entanglement-runtime/examples/embedded.rs`](../entanglement-runtime/examples/embedded.rs)
(`cargo run -p entanglement-runtime --example embedded --no-default-features`),
the dynamic-resolver and hibernate/resume lifecycle seams in
[`entanglement-runtime/examples/embedded_lifecycle.rs`](../entanglement-runtime/examples/embedded_lifecycle.rs)
(`cargo run -p entanglement-runtime --example embedded_lifecycle
--no-default-features`), and the streamable-HTTP MCP transport in
[`entanglement-runtime/examples/mcp_http.rs`](../entanglement-runtime/examples/mcp_http.rs)
(`cargo run -p entanglement-runtime --example mcp_http --features mcp-http`) —
none need a provider key, running against the built-in `EchoLlm`. `make
lint`/`make check-lean` run `clippy --all-targets` over all three on every
change (`mcp_http.rs` only under `lint`, since `mcp-http` is compiled out of
the lean `--no-default-features` build the other two target — see [§6](embedding/approvals-mcp-multi-user.md)),
so the snippets quoted here can't silently drift from the real API.

Until this guide, the only wiring reference was `main.rs`/`run.rs` — written
for a single-user CLI. Multi-tenancy is not a separate mode the engine knows
about; it falls out of five ordinary decisions an embedder makes about its own
process, covered in order below. Part of #307/#315.

This module is split into the pages below, one per seam; each stays under the 400-line doc cap (`make doc-cap`). Every section heading the single-file version had is kept here, pointing at where that section now lives.

| page | covers |
| --- | --- |
| [tenants-and-heads](embedding/tenants-and-heads.md) | §1–§2 — Many tenants & a custom head |
| [persistence](embedding/persistence.md) | §3 — Custom persistence |
| [tool-execution](embedding/tool-execution.md) | §4 — Custom tool execution / policy |
| [approvals-mcp-multi-user](embedding/approvals-mcp-multi-user.md) | §5–§7 — Approvals across restart, MCP & multi-user |
| [migrations](embedding/migrations.md) | Migration notes (0.6 → 0.7: start with agents, modes & permissions) |
| [tls-and-pinning](embedding/tls-and-pinning.md) | TLS & dependency pinning |

## 1. One engine, many tenants

Now in [§1–§2 — Many tenants & a custom head](embedding/tenants-and-heads.md#1-one-engine-many-tenants).

## 2. A custom head over the ABI: the trust split

Now in [§1–§2 — Many tenants & a custom head](embedding/tenants-and-heads.md#2-a-custom-head-over-the-abi-the-trust-split).

## 3. Custom persistence: tap → sink → lazy `resume`

Now in [§3 — Custom persistence](embedding/persistence.md#3-custom-persistence-tap--sink--lazy-resume).

## 4. Custom tool execution / policy

Now in [§4 — Custom tool execution / policy](embedding/tool-execution.md#4-custom-tool-execution--policy).

### Constructing the executor — `ToolExecutor::builder`

Now in [§4 — Custom tool execution / policy](embedding/tool-execution.md#constructing-the-executor--toolexecutorbuilder).

### The policy seams

Now in [§4 — Custom tool execution / policy](embedding/tool-execution.md#the-policy-seams).

## 5. Approval-across-restart semantics

Now in [§5–§7 — Approvals across restart, MCP & multi-user](embedding/approvals-mcp-multi-user.md#5-approval-across-restart-semantics).

## 6. Attaching MCP servers programmatically

Now in [§5–§7 — Approvals across restart, MCP & multi-user](embedding/approvals-mcp-multi-user.md#6-attaching-mcp-servers-programmatically).

## 7. Multi-user: per-user providers, permissions, tokens

Now in [§5–§7 — Approvals across restart, MCP & multi-user](embedding/approvals-mcp-multi-user.md#7-multi-user-per-user-providers-permissions-tokens).

## Migrating to one advertised surface (ADR-0226, #818)

Now in [Migration notes](embedding/migrations/one-surface-and-rhai.md#migrating-to-one-advertised-surface-adr-0226-818).

## Migrating to one binding path (ADR-0227, #819)

Now in [Migration notes](embedding/migrations/one-surface-and-rhai.md#migrating-to-one-binding-path-adr-0227-819).

## Migrating to `rhai(path)` and skill tools' removal (ADR-0228, #820)

Now in [Migration notes](embedding/migrations/one-surface-and-rhai.md#migrating-to-rhaipath-and-skill-tools-removal-adr-0228-820).

## Migrating to every skill model-visible (ADR-0229, #845)

Now in [Migration notes](embedding/migrations/one-surface-and-rhai.md#migrating-to-every-skill-model-visible-adr-0229-845).

## Structured card reason and honest scopes (ADR-0230, #836/#846)

Now in [Migration notes](embedding/migrations/one-surface-and-rhai.md#structured-card-reason-and-honest-scopes-adr-0230-836846).

## Live-only nested-call events (ADR-0233, #821)

Now in [Migration notes](embedding/migrations/cards-and-sessions.md#live-only-nested-call-events-adr-0233-821).

## TLS and the rustls crypto provider

Now in [TLS & dependency pinning](embedding/tls-and-pinning.md#tls-and-the-rustls-crypto-provider).

## Pinning a dependency

Now in [TLS & dependency pinning](embedding/tls-and-pinning.md#pinning-a-dependency).
