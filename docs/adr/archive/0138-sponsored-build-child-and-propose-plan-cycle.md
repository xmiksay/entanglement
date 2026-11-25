# 0138. Sponsored build child — plan acceptance spawns a clamp-exempt child

- Status: Accepted — Amended by [0145]
- Date: 2026-07-30
- Amends: [ADR-0042](0042-plan-acceptance-via-propose-plan-approval-roundtrip.md) (the handoff becomes a sponsored child, not a fresh root), [ADR-0024](0024-subagent-permission-gating.md) (sponsored children are exempt from the ancestor privilege clamp), [ADR-0023](0023-subagent-spawn-limits.md) (sponsored spawns are exempt from `MAX_SPAWNS_PER_ROOT`)

## Context

When the user approves a `propose_plan` call ([ADR-0042](0042-plan-acceptance-via-propose-plan-approval-roundtrip.md)), the plan is handed off to a `build` session that implements it. ADR-0042 modelled the build session as a **fresh root** minted by **head policy**: the TUI's `event_loop` intercepted the approval, sent `InMsg::SetAgent` on a fresh id, then `InMsg::Prompt` with the wrapped plan, and switched the view. No connection back to the plan session, no result return, no cycling.

Two problems followed from the fresh-root-plus-head-policy shape:

1. **No cycling.** A plan/build cycle — propose, build, learn the outcome, revise, re-propose — was impossible: the build session had no link to its plan, so the plan agent never received the implementation result. A plan that surfaced a flaw mid-build had no path back.
2. **Latent bug on other heads.** The handoff lived in the TUI's `event_loop`. Pipe and WS heads never implemented it — an approved plan on those heads acked the model with "plan accepted by the user" and stopped. The behavior a user sees depends on which head they happen to use.

A parent-child link is the natural fix for (1): the build becomes a child of plan, its result folds back, the plan agent cycles. But ADR-0024's ancestor privilege clamp makes that useless as-is — a `build` child of a read-only `plan` session would be clamped to `Ask` on every write tool, so it couldn't implement anything. ADR-0023's per-root fan-out cap would also drain the plan root's spawn budget across a long plan/build cycle.

## Decision

A **sponsored child** has a parent-child link (result return, lifecycle, session-tree visibility) but its **permission resolution stops at the child** — it runs with its own profile's permissions, no ancestor walk. **Authorization is user approval of the plan** (`propose_plan` accept), which is exactly the transfer of authority ADR-0042 modelled as a fresh root — but routed through the runtime instead of head policy, so every head gets it.

```
plan agent calls propose_plan { plan }
  → user approves
  → runtime spawns build as a SPONSORED child of plan
    (full build perms via sponsorship; plan text via wrap_plan)
  → plan emits WaitingAgent (ADR-0139), parks
  → build runs to completion
  → result folds back to plan as the propose_plan tool result
  → plan has the result in context → can revise + re-propose (cycle)
```

### SpawnGuard

`SpawnGuard` gains a `sponsored: HashSet<SessionId>` alongside its `parents`/`spawns_per_root` maps. Two new methods:

- `record_sponsored_start(child, parent)` — establishes the parent link and marks the child a permission root.
- `is_sponsored(session)` — consulted by the permission resolver.

`try_sponsor_spawn(parent)` is the bounding check: sponsored spawns are **exempt from `MAX_SPAWNS_PER_ROOT`** (sequential, user-authorized — a long plan/build cycle can't exhaust the fan-out budget meant for unattended `agent_spawn` fan-out), but **`MAX_SPAWN_DEPTH` still applies** — a sponsored build nested three levels deep still can't sponsor further.

The `SpawnGuard` mutation (sponsor check + `record_sponsored_start`) happens in the tool executor's single-threaded loop, before the detached per-call task is spawned — so the parent link is in place by the time the child's first `ToolExec` arrives and the ancestor clamp skips it.

### Permission resolution

`effective_permission`, `resolve_with_source`, `ancestor_chain`, `permission_chain`, and `tool_masked` all short-circuit when they encounter a sponsored session: the walk stops, and the sponsored session's own profile (or, for a sponsored ancestor mid-walk, that ancestor's profile plus a stop) stands. This is what lets a `build` child of a read-only `plan` advertise and run `edit`/`write`/`bash` — without the exemption, the plan ancestor's read-only mask and `Ask`/`Deny` grades would erase them.

The pluggable [`PermissionResolver`](0079-pluggable-permission-resolver-and-grant-store.md) path gets the same treatment via `ancestor_chain`: the chain stops at a sponsored session, so a tenant rule can never widen a sponsored child beyond its own profile but also can't narrow it via the chain above.

### Head changes

The handoff moves from **head policy** to **runtime policy**. The TUI's `send_approval` no longer captures a `propose_plan` handoff and no longer calls `handoff_accepted_plan` (deleted). Pipe/WS heads, which never implemented the handoff, get correct behavior automatically — they just forward the `Approve`.

### `InMsg::Spawn` protocol

Unchanged. Sponsorship is runtime-local `SpawnGuard` state — a wire head cannot forge it. The `Spawn` the runtime sends is a normal parented spawn; the sponsorship marker is recorded in `SpawnGuard` before the `Spawn` lands.

## Consequences

- **(+)** A plan/build cycle is now possible: the build's answer folds back as the `propose_plan` tool result, so the plan agent has the implementation outcome in context and can revise + re-propose.
- **(+)** All heads get the handoff. The TUI no longer carries head-specific handoff code; pipe/WS heads behave identically.
- **(+)** The plan agent stays read-only. Sponsorship means the *child* runs with full build perms; the plan session's own profile is untouched.
- **(+)** Non-sponsored ancestor clamp (ADR-0024) is intact — a regression test pins a plain `build` child under `plan` still clamps to `Ask` on `edit`.
- **(+)** The fan-out cap (ADR-0023) still bounds unattended `agent_spawn` fan-out; only user-authorized sponsored spawns skip it.
- **(−)** One new concept ("sponsored") in the spawn tree. Its blast radius is contained: it only affects `propose_plan` accept today, and the exemption is authorization-backed (user plan approval), not a profile-level escape hatch.
- **(−)** A sponsored build child is visible in the session tree (it has a parent link), so a head that enumerates sessions sees it — unlike the pre-ADR-0138 fresh root, which was disconnected. This is a feature (the user can watch the build run), but it's a visible change.

## Alternatives considered

- **Keep the fresh-root handoff; add a separate "cycle" protocol message.** Rejected: multiplies the protocol surface for one feature, and still leaves the pipe/WS latent bug unless every head implements both the handoff and the new cycle message.
- **Make `build` a plain child of `plan` and add a `propose_plan`-specific permission exemption.** Rejected: a profile-specific exemption in the permission resolver is exactly the kind of special case the clean ladder ([ADR-0070](0070-authoritative-tool-exec-profile-and-fail-closed-fallback.md)) avoids. Sponsorship is a property of the spawn, not the tool or profile.
- **Sponsor the build but keep the ancestor clamp for tool masking.** Rejected: without the mask exemption, `edit`/`write`/`bash` would be masked out by the plan ancestor's read-only allowlist before permission resolution even runs — the sponsorship would be useless.
- **Lift `MAX_SPAWN_DEPTH` for sponsored spawns.** Rejected: depth bounds unbounded recursion (ADR-0023). A sponsored build nested three deep has no business sponsoring another; the per-root fan-out exemption is enough for the plan/build cycle.
