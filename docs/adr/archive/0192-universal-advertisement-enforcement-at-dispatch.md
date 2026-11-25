# 0192. The advertised tool surface is universal and session-stable; masks enforce at dispatch

- Status: Accepted
- Date: 2026-09-13
- Amends: [ADR-0190](0190-poll-is-always-on-non-maskable-internal-tool.md) (its
  advertisement-mask exemption becomes the universal rule — `ALWAYS_ADVERTISED_TOOLS`
  and `is_always_advertised` are removed; the resolver-roster half of its fix
  stands unchanged). **Supersedes** [ADR-0179](0179-lazily-registered-built-ins-advertise-session-scoped.md)
  (its session-scoped advertisement store is retired and
  `builtin_visibility.rs` deleted: with advertisement universal there is nothing
  left to scope). Related: [ADR-0176](0176-structured-tool-result-is-error-and-duration-fields.md)
  (the `is_error` channel refusals ride), [ADR-0163](0163-live-bash-enablement-is-a-tool-overlay-entry.md)
  (overlay entries now have only registration + grade effects),
  [ADR-0149](0149-per-session-tool-overlay.md)/[ADR-0106](0106-skill-scoped-allowed-tools-enforcement.md)/#116
  (the three enforcement layers that moved to dispatch-only).

## Context

The advertised `tools` array is a prefix of the prompt: any mid-session change
to it — a session tool overlay toggle, `/enable tool bash`, a skill's
`allowed_tools` activating, a `SetAgent` to a differently-masked profile —
invalidates the provider's prompt cache from the tools block onward, i.e. the
whole prompt re-bills at the uncached rate. ADR-0179 had already paid this
price once for one tool (`/enable tool bash` grew every live session's array);
ADR-0190's `poll` exemption was the same fix applied to one name. Both were
special cases of a general problem: advertisement was derived from
enforcement state, so every enforcement change was a cache bust.

Meanwhile the thing the filtering bought — "the model cannot even attempt a
masked call" — was worth little: the model still saw masked-out tools
described in its prompt text, attempts cost one declined round-trip, and the
declines were unattributed, so a model that did attempt one often retried
blindly.

## Decision

**Advertisement is unconditional; enforcement is dispatch-only.**

1. Core advertises every spec the `tool_spec_resolver` (or static
   `cfg.tool_specs`) provides, every turn, with **no filtering** by the
   profile mask (#116/ADR-0038), the session tool overlay (ADR-0149), or an
   active skill's `allowed_tools` (ADR-0106). The resolver becomes the single
   seam shaping a session's base surface. The only tools that legitimately
   vary are the profile-*defining* specs — `propose_plan` (plan-only), the
   `agent`/`agent_send` spawn enum (which agents may be spawned), `mcp__*`
   (connection-dependent) — and those differ *across* profiles, never
   mid-session within one.
2. The runtime dispatch gate (the executor's ladder) enforces all three mask
   layers at call time. A refused call returns an **attributed** message from
   one wording table (`decline.rs`): `Declined by agent profile …` /
   `Declined by ancestor agent …` / `Declined by session tool overlay …` /
   `Declined by skill …`, plus `bash`'s enabling hint — carried on the
   ADR-0176 `is_error` channel. Attribution is the point: a model told *who*
   declined stops retrying blind.
3. `bash` is advertised even while unregistered (dispatch declines with the
   `/enable tool bash` hint) — retiring ADR-0179's per-session visibility
   store wholesale; overlay entries keep only their registration + grade
   effects. ADR-0190's `poll` exemption is subsumed: the constant and its
   short-circuit are removed because there is nothing left to exempt.
4. The UI consequence ([`a842a45`]): the TUI `/agent` checklist and
   `skutter inspect agents` can no longer present the mask as tool
   presence/absence — a shared `tool_state` module derives a per-tool
   *dispatch* state (allowed / asks / declines, with argument-scoped grades
   spelled out) from the permission grade + mask, and shows that beside the
   still-editable mask checkbox.

## Consequences

- The tools array is byte-stable within a session ⇒ the provider prompt cache
  survives overlay toggles, skill loads, and bash enablement. (A `SetAgent`
  rebind can still change the profile-defining specs — that is a deliberate
  profile switch, not an enforcement drift.)
- A masked call now costs one declined round-trip instead of being invisible;
  in exchange every decline names its authority. Unadvertised tools are no
  longer a security boundary — they never were one (the permission ladder
  was), but this makes it explicit.
- `builtin_visibility.rs` (~190 lines), the `spec_visible` session filter,
  and the `ALWAYS_ADVERTISED_TOOLS` special case are deleted; the ancestor
  walk shared with MCP visibility remains for the *enforcement* side.
- More tools in every prompt: ~1–2 KB of schemas for tools a profile will
  never admit. Accepted — the tools prefix is cached, so the steady-state
  cost is cache writes once, not per-turn re-billing; and the alternative
  (mask-filtered advertisement) is precisely the cache-busting behavior
  being removed.
- The enforcement outcomes are bit-identical to before: same masks, same
  ladder, same refusals — only their *channel* changed (a structured
  attributed error instead of "not available" text).

## Alternatives considered

- **Keep advertisement mask-filtered and accept the cache busts.** The status
  quo ADR-0179/0190 were patching one tool at a time; every new enforcement
  seam re-introduced the same bug.
- **Filter advertisement but freeze it per session.** Freezing the *filtered*
  view means an overlay toggle silently stops working until the session
  restarts — worse than a cache bust.
- **Mask-filter at advertisement + re-advertise on change (status quo) with
  cache-busting accepted and documented.** Re-billing the entire prompt per
  toggle is exactly the cost the #673 cache-stability work exists to avoid.
- **Keep ADR-0179's visibility store and extend it to all masks.** That is
  per-session filtered advertisement again, with a second source of truth to
  keep in sync with the enforcement walk — the complexity the retirement
  deletes.
