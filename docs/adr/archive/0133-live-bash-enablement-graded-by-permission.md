# 0133. Live bash enablement, graded by the permission model

- Status: Accepted
- Date: 2026-07-23
- Issue #498, on top of [0096](0096-dynamic-toolregistry-sharedregistry.md)/
  [0097](0097-live-mcp-server-management.md) (`SharedRegistry` + live MCP
  management — the pattern this follows) and
  [0093](0093-call-registration-independent-of-bash-opt-in.md) (`call`'s
  registration split from `bash`'s opt-in).

## Context

`bash`/`bash_output` registration was a **startup-only** decision:
`ENTANGLEMENT_ENABLE_BASH` is read once in `main.rs` before `Holly::spawn`,
and `register_default_tools(..., bash_enabled, ...)` either registers the pair
or doesn't. There was no way to turn it on mid-session — the only workarounds
were restarting with the env var set and resuming, or falling back to `call`.

This bit in practice: the built-in `commit` skill's `allowed_tools` is
`[bash, read, grep]` — it assumes `bash` but omits `call`. With bash not
enabled at startup, the skill mask (#400, ADR-0106) blocks `call` (not in the
allowlist) and the allowed `bash` entry points at a tool that was never
registered, leaving the agent with no exec tool at all for the rest of the
turn.

The infrastructure for a live toggle was already in place: `SharedRegistry`
(#372, ADR-0096) — every executor dispatch and `tool_spec_resolver` snapshot
reads the registry fresh, so a live registration reaches execution and the
model's advertised schemas with no restart — and `/mcp add` (#375, ADR-0097)
already exercises exactly this seam for MCP tools. Only the command surface
and the enablement's *permission* wiring were missing.

## Decision

### Wire shape: two new global ops, no session, trusted-only

`InMsg::BashEnable { grade: BashGrade }` / `BashDisable`, answered by
`OutEvent::BashChanged { enabled, grade }`. The tool registry is
process-wide, not per-session, so these follow `McpAdd`/`McpRemove`'s
precedent exactly: `session()` returns `None`, `msg_to_cmd` routes them to
`None` (never a session task), and — like `McpAdd`/`McpRemove` since #472/
[0124](0124-wire-refused-mcp-mutation-and-stdio-key-scrub.md) — they are
**not** `wire_allowed()`: live-enabling bash with a blanket `Allow` grade
hands the model a full shell with no approval prompt at all, so an
unauthenticated wire frame must never trigger it. The TUI `/bash` command
sends over the privileged `Holly::send`, unaffected.

`BashGrade` is a new core DTO (`entanglement-core/src/protocol.rs`):

```rust
enum BashGrade {
    Ask,
    Allow { pattern: Option<String> },
}
```

`Ask` is the safe default — every `bash` call still goes through the normal
`ToolRequest` approval prompt. `Allow` grants permission outright; an
optional command `pattern` narrows the grant to matching commands only
(materializing an argument-scoped rule like `bash(git *): allow` — the
existing `tool(pattern)` syntax, #173 — rather than a bespoke mechanism).

### Answered by a runtime service, not the core supervisor

Mirrors `mcp::spawn_mcp_responder` exactly: a new
`bash_live::spawn_bash_responder` subscribes to `Holly::subscribe_inbound()`
and answers `BashEnable`/`BashDisable`, since it alone holds the
`SharedRegistry` + the new `LiveBashState`. Neither op can fail (registration
is infallible, unlike an MCP connect/handshake), so every request always
replies with `OutEvent::BashChanged` — no logged-and-dropped failure path
like MCP's best-effort connect.

### `LiveBashState`: registration flag + grade override, seeded from the env var

`bash_live::LiveBashState` holds an `AtomicBool` (`registered`) and an
`Option<BashGrade>` (`grade`) behind a `RwLock`. At startup it is seeded with
`registered = bash_enabled` (the env var) but `grade = None` — a
startup-registered pair carries **no** live override, so it resolves through
the session's own profile exactly as before #498. `grade` only becomes
`Some` via a live `BashEnable`, and clears back to `None` on `BashDisable`.
This is what makes the feature strictly additive: a caller that never issues
`BashEnable` sees byte-identical behavior to pre-#498.

`bash_enable`/`bash_disable` (mirroring `mcp::live::mcp_add`/`mcp_remove`)
register/unregister the pair into the `SharedRegistry` — idempotent, so a
repeated `/bash on` with a different grade just updates the override without
double-registering — and update `LiveBashState`.

### Permission wiring: the live grade overrides the profile, then the ceiling clamps as always

`ProfileResolver` (the CLI's default `PermissionResolver`) gained an optional
`live_bash: Option<Arc<LiveBashState>>` (`with_live_bash`, opt-in — omitting
it is byte-identical to before). For `bash`/`bash_output` specifically, when
`live_bash.grade()` is `Some`, its materialized `PermissionProfile`
(`bash_live::grade_profile`) is used as the call's "own" permission **in
place of** the session's actual agent-profile grade; every other tool, and
bash/bash_output when no live grade is set, resolve exactly as before.

This is a deliberate override, not an additive rule: a profile authored
before bash was live-enabled has no real opinion on it (bash didn't exist for
it to configure), so there is nothing meaningful to merge with. The result
still passes through the existing `clamp_to_base` ceiling clamp (#172,
ADR-0047) unconditionally — **a config ceiling of `bash: deny` still wins
over a live `Allow`**, since the ceiling is a pure least-privilege clamp
applied after resolution regardless of where the "own" grade came from.

### TUI: `/bash on [--allow [<pattern>]|--ask] | /bash off`

`tui/bash_command.rs` mirrors `mcp_command.rs`'s raw-text re-parse pattern. A
bare `/bash` (typed, or picked from the command palette, which carries no
trailing text) defaults to `on` with `Ask` — safe, since it never
auto-approves anything, mirroring how a bare `/mcp` defaults to the read-only
`list`. Confirmations (`OutEvent::BashChanged`) render as a transcript status
line (`App::handle_bash_changed`) — there is nothing to list like `/mcp`'s
panel, so no modal.

The TUI's `!bash` passthrough gate (`App::bash_enabled`) used to read a
plain `bool` snapshotted once at startup from the env var
(`App::init_head_context`). It now reads `LiveBashState::is_enabled()` live,
so a mid-session `/bash on`/`off` takes effect immediately without needing a
separate signal to reach the gate.

## Consequences

- Enabling bash live is process-wide, not per-session — matching how the
  `SharedRegistry` itself is process-wide (#372). The permission *grade* is
  likewise process-wide for the duration a live grade is set: every session's
  `bash`/`bash_output` calls see the same override while one is active. This
  answers the issue's "registry granularity" open question in favor of the
  simpler option ("process-wide enable + a shared grade" over per-session
  masking), consistent with `/mcp add`'s own process-wide scope.
- A live enable is **ephemeral** — it does not persist to `config.yml`. A
  process restart reverts to whatever `ENTANGLEMENT_ENABLE_BASH` says. This
  answers the issue's "persistence" open question: no `--always` flag, unlike
  `Approve { scope: Always }`'s persisted grants — a materialized
  `bash(pattern): allow` rule is process lifetime only. Deferred to a
  follow-up if requested.
- `/bash off` unregisters the pair outright via `SharedRegistry` — the same
  "next dispatch sees the fresh snapshot" seam `/mcp remove` uses. A turn
  parked mid-`bash` call is unaffected structurally (the executor already
  dispatches against a registry snapshot taken at call time), matching how
  `mcp_remove` doesn't special-case an in-flight call either.
- The unsandboxed-privileges warning notice (`skutter: bash enabled ... full
  privileges`) still only fires for the startup env-var path in `main.rs`; a
  live `/bash on --allow` does not re-print it to the terminal (the TUI has
  no bare stderr to write to mid-session) — the TUI status line
  (`enabled (allow)`) is the equivalent in-session signal.

## Rejected alternatives

- **Materializing the live grade as an additional rule in the config
  ceiling.** The ceiling (`user_config.permissions`) is a pure
  least-privilege clamp — documented as "can tighten, never loosen" — so
  adding an `Allow` rule there would either do nothing (already allow-all) or
  contradict the ceiling's own invariant. The grade instead overrides the
  *own*-profile side of the resolution, which the ceiling still clamps
  afterward exactly as it clamps every other tool.
- **Per-session live enablement (a mask layered per session, like
  `tool_masked`).** Would answer the issue's granularity question the other
  way, but the registry itself is already one process-wide
  `SharedRegistry` (#372) — a per-session registration would need a second,
  parallel mechanism just for bash, inconsistent with every other live tool
  addition (MCP). Rejected for the simpler process-wide grade, matching
  `/mcp add`'s own scope.
- **A `bash_output`-specific pattern clause.** `bash_output` only takes a job
  id, not a command line, so an argument-scoped `bash_output(pattern)` rule
  has nothing meaningful to match against. A narrowed `Allow { pattern:
  Some(_) }` therefore leaves `bash_output` at the grade's `Ask` default
  rather than special-casing it to always-allow — consistent, if slightly
  more prompt-heavy, over adding an exemption whose scope would be unclear.
