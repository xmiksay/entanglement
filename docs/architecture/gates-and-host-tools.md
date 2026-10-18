# entanglement Architecture — Hygiene gates & host tools

> Part of the [architecture overview](../architecture.md). The *why* behind each choice is in the [decision log](../adr/README.md).

This module is split into the pages below, one per seam; each stays under the 400-line doc cap (`make doc-cap`). Every section heading the single-file version had is kept here, pointing at where that section now lives.

| page | covers |
| --- | --- |
| [hygiene-gates](gates/hygiene-gates.md) | §7 — Hygiene gates |
| [host-tools](gates/host-tools.md) | §8 — Host tools: the tool set, containment & sandbox |
| [host-tools-contracts](gates/host-tools-contracts.md) | §8 — Host tools: output, wiring, orchestration & `poll` |
| [web-fetch-and-undo](gates/web-fetch-and-undo.md) | §8 — `web_fetch` and undo/rewind |
| [one-surface](gates/one-surface.md) | §8 — One advertised surface, error taxonomy & removed sources |
| [script-surface](gates/script-surface.md) | §8 — Script surface |
| [hooks-and-seams](gates/hooks-and-seams.md) | §9 — Lifecycle hooks & pluggable policy seams |
| [mcp](gates/mcp.md) | §10 — MCP client: transports, OAuth, scopes & proxy |
| [mcp-config-and-live](gates/mcp-config-and-live.md) | §10 — MCP client: config, live management & lazy connect |

## 7. Hygiene gates — [ADR-0006](../adr/0006-core-dependency-hygiene-gate.md) + [ADR-0053](../adr/0053-invert-core-provider-seam.md) (`tree`), [ADR-0025](../adr/0025-runtime-cargo-feature-gates.md) + [ADR-0053](../adr/0053-invert-core-provider-seam.md) (`check-lean`)

Now in [§7 — Hygiene gates](gates/hygiene-gates.md#7-hygiene-gates--adr-0006--adr-0053-tree-adr-0025--adr-0053-check-lean).

## 8. Host tools — [ADR-0008](../adr/0008-host-tools-workdir-and-bounded-output.md) (trio), [ADR-0009](../adr/0009-edit-and-bash-host-tools.md) (`edit`/`bash`), [ADR-0010](../adr/0010-single-head-crate-and-bash-opt-in.md) (`bash` opt-in), [ADR-0045](../adr/0045-call-host-tool-argv-exec-tailed-output.md) (`call`), [ADR-0092](../adr/0092-call-file-based-stdin-stdout.md) (`call` file-based stdin/stdout), [ADR-0093](../adr/0093-call-registration-independent-of-bash-opt-in.md) (`call` always-registered + `workdir`), [ADR-0104](../adr/0104-bubblewrap-sandbox-for-bash-call.md) (optional bubblewrap confinement), [ADR-0134](../adr/0134-per-profile-sandbox-scoping-and-spawn-chain-clamp.md) (per-profile scoping + spawn-chain clamp)

Now in [§8 — Host tools: the tool set, containment & sandbox](gates/host-tools.md#8-host-tools--adr-0008-trio-adr-0009-editbash-adr-0010-bash-opt-in-adr-0045-call-adr-0092-call-file-based-stdinstdout-adr-0093-call-always-registered--workdir-adr-0104-optional-bubblewrap-confinement-adr-0134-per-profile-scoping--spawn-chain-clamp).
Continued in [§8 — Host tools: output, wiring, orchestration & `poll`](gates/host-tools-contracts.md).

### `web_fetch` — SSRF-guarded page fetch — [ADR-0220](../adr/0220-web-fetch-ssrf-guard-and-clipboard-image-paste.md) (#736), [ADR-0225](../adr/0225-web-fetch-grades-redirect-hops.md) (redirect grading, #808)

Now in [§8 — `web_fetch` and undo/rewind](gates/web-fetch-and-undo.md#web_fetch--ssrf-guarded-page-fetch--adr-0220-736-adr-0225-redirect-grading-808).

### Undo / rewind — per-write file backups — [ADR-0212](../adr/0212-undo-rewind-via-per-write-file-backups.md) (#731)

Now in [§8 — `web_fetch` and undo/rewind](gates/web-fetch-and-undo.md#undo--rewind--per-write-file-backups--adr-0212-731).

### One advertised surface and discovery — [ADR-0226](../adr/0226-one-advertised-surface-mcp-envelope-registered-is-available.md)

Now in [§8 — One advertised surface, error taxonomy & removed sources](gates/one-surface.md#one-advertised-surface-and-discovery--adr-0226).

### Error taxonomy — five cases — [ADR-0196](../adr/0196-tool-search-and-lazy-discovery-replace-the-invoke-envelope.md) §6

Now in [§8 — One advertised surface, error taxonomy & removed sources](gates/one-surface.md#error-taxonomy--five-cases--adr-0196-6).

### Definition-driven tool sources — removed

Now in [§8 — One advertised surface, error taxonomy & removed sources](gates/one-surface.md#definition-driven-tool-sources--removed).

### Script surface — namespaces, `http()`, skill libraries — [ADR-0227](../adr/0227-one-binding-path-namespaces-http-skill-libraries.md) (#819)

Now in [§8 — Script surface](gates/script-surface.md#script-surface--namespaces-http-skill-libraries--adr-0227-819).

## 9. Lifecycle hooks — [ADR-0066](../adr/0066-lifecycle-hooks-as-runtime-interceptors.md) (#199)

Now in [§9 — Lifecycle hooks & pluggable policy seams](gates/hooks-and-seams.md#9-lifecycle-hooks--adr-0066-199).

### Pluggable policy seams — `PermissionResolver` + `GrantStore` — [ADR-0079](../adr/0079-pluggable-permission-resolver-and-grant-store.md) (#311)

Now in [§9 — Lifecycle hooks & pluggable policy seams](gates/hooks-and-seams.md#pluggable-policy-seams--permissionresolver--grantstore--adr-0079-311).

### Dynamic `ToolRegistry` — `SharedRegistry` — [ADR-0096](../adr/0096-dynamic-toolregistry-sharedregistry.md) (#372)

Now in [§9 — Lifecycle hooks & pluggable policy seams](gates/hooks-and-seams.md#dynamic-toolregistry--sharedregistry--adr-0096-372).

## 10. MCP client — external tool servers — [ADR-0067](../adr/0067-mcp-client-as-runtime-tool-provider.md) (#198)

Now in [§10 — MCP client: transports, OAuth, scopes & proxy](gates/mcp.md#10-mcp-client--external-tool-servers--adr-0067-198).
Continued in [§10 — MCP client: config, live management & lazy connect](gates/mcp-config-and-live.md).

### Live add/remove/list — [ADR-0096](../adr/0096-dynamic-toolregistry-sharedregistry.md) + [ADR-0097](../adr/0097-live-mcp-server-management.md) (#372, #375)

Now in [§10 — MCP client: config, live management & lazy connect](gates/mcp-config-and-live.md#live-addremovelist--adr-0096--adr-0097-372-375).

### Registered = available — [ADR-0226](../adr/0226-one-advertised-surface-mcp-envelope-registered-is-available.md) (bundled servers: [ADR-0152](../adr/0152-provider-bundled-mcp-servers-three-state-enablement.md), #542)

Now in [§10 — MCP client: config, live management & lazy connect](gates/mcp-config-and-live.md#registered--available--adr-0226-bundled-servers-adr-0152-542).

### Dispatch-time lazy connect — [ADR-0201](../adr/0201-unregistered-mcp-tool-dispatch-resolves-truthfully.md), amended by [ADR-0226](../adr/0226-one-advertised-surface-mcp-envelope-registered-is-available.md)

Now in [§10 — MCP client: config, live management & lazy connect](gates/mcp-config-and-live.md#dispatch-time-lazy-connect--adr-0201-amended-by-adr-0226).

### TUI `/mcp` command — [ADR-0100](../adr/0100-tui-mcp-command.md) (#373)

Now in [§10 — MCP client: config, live management & lazy connect](gates/mcp-config-and-live.md#tui-mcp-command--adr-0100-373).
