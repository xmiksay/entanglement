# 0163. Live bash enablement is a tool-overlay entry, not its own message pair

- Status: Accepted
- Date: 2026-08-03
- Supersedes: [ADR-0133](0133-live-bash-enablement-graded-by-permission.md)
- Amends: [ADR-0149](0149-per-session-tool-overlay.md) (`ToolOverlayEntry` gains an argument-scope
  field; an enable entry may register a built-in)

## Context

[ADR-0133](0133-live-bash-enablement-graded-by-permission.md) (#498) shipped live bash enablement
as a bespoke message pair — `InMsg::BashEnable { grade }` / `InMsg::BashDisable`, acked by
`OutEvent::BashChanged`, carrying a `BashGrade` enum — because at the time nothing generic existed
to express "make this tool exist for this session, at this permission grade."

[ADR-0149](0149-per-session-tool-overlay.md) (#539) then built exactly that generic thing, and built
it *on the ADR-0133 precedent*: `InMsg::SetToolOverlay { session, entries: Vec<ToolOverlayEntry> }`,
trusted-only "per the `BashEnable` rationale," with `allow: false` documented as "the default,
mirroring `BashGrade::Ask`" and `allow: true` granting outright, "both runtime-side and still
clamped by the config ceiling (#172), **exactly like a live bash grade (#498)**."

So the generic surface already restates the bespoke one almost field for field. What remains is
duplication with two visible costs:

- **A permission special-case.** `policy.rs` carries
  `matches!(tool, "bash" | "bash_output")` to consult the live grade before ordinary per-profile
  resolution — a hardcoded tool-name test in the permission path.
- **A second registration path.** `bash_live::bash_enable` builds a whole `BashTool`/`BashOutputTool`
  pair, "mirroring `register_default_tools`'s bash arm in `main.rs`" — including, on every enable, a
  **fresh `JobRegistry`**. So `/bash off` followed by `/bash on` orphans every outstanding background
  job and restarts the `bg-N` id counter at 0, making job ids non-unique within a single session.

## The gap the overlay does *not* yet cover

`ToolOverlayEntry` is **not** a strict superset of `BashGrade`, and the reason is easy to miss
because both fields are called `pattern`:

- `ToolOverlayEntry.pattern` is a **tool-name** glob (ADR-0148 mask semantics — `bash`,
  `mcp__chessbase__*`).
- `BashGrade::Allow { pattern: Some("git *") }` is a **command-argument** pattern. It materializes
  (`bash_live::grade_profile`) into `PermissionProfile::new(Ask)` plus an argument-scoped
  `bash(git *): allow` rule — the ADR-0114/#173 `tool(pattern)` syntax.

The overlay's `allow: bool` is flat: `true` → Allow, `false` → Ask. It can say "bash exists and is
Allow," and it can say "bash exists and is Ask." It **cannot** say "bash exists, Ask by default,
Allow only for commands matching `git *`" — which is precisely ADR-0133's most useful grade and the
one `/bash on --allow 'git *'` exists to set.

Retiring `BashEnable` without closing this would be a silent capability regression.

## Decision

### 1. `ToolOverlayEntry` gains an argument scope

```rust
pub struct ToolOverlayEntry {
    pub pattern: String,          // tool-name glob (unchanged)
    pub allow: bool,              // unchanged
    pub deny: bool,               // unchanged
    pub arg_pattern: Option<String>,  // NEW — argument scope for the granted permission
}
```

`arg_pattern` is `None` for every existing entry (`#[serde(default)]`, skipped when absent), so the
wire shape is unchanged for current senders and replay of existing logs is unaffected.

When present on an **enable** entry, the entry materializes the same way `grade_profile` does today:
a flat `Ask` default plus an argument-scoped `tool(arg_pattern): allow` rule, fanned out over every
tool the name-glob matches. `arg_pattern` is meaningless on a `deny` entry and ignored there, mirroring
how `allow` is already meaningless-and-ignored on a deny.

This generalizes beyond bash: any tool can now be session-enabled at a narrowed grade, which the
overlay could not previously express for *any* tool.

It does **not** close ADR-0149's deferred "rhai binding *grades*" gap (ledger row 8). That gap is
about *where* the overlay grade is consulted — the generic dispatch route only, never the `rhai`
`BindingPolicy` snapshot or a child session's own chain. `arg_pattern` widens what a grade can
*say*, not where it *reaches*, so row 8 stays open on its own terms.

### 2. Enabling a known-but-unregistered built-in registers it

The overlay is mask-level: it makes tools *exist for a session* past the profile mask. But `bash`
and `bash_output` are not in the `SharedRegistry` at all unless `ENTANGLEMENT_ENABLE_BASH=1`, so an
overlay entry alone cannot conjure them.

An enable entry whose name-glob matches an entry in a **closed, explicit table of lazily-registrable
built-ins** triggers that built-in's registration into the `SharedRegistry`, exactly as
`bash_live::bash_enable` does today. The table is a fixed list in the runtime — *not* "any name the
runtime recognises" — because this is the one genuinely new power in this ADR: today `SetToolOverlay`
can only reveal tools the registry already holds, and teaching it to *instantiate* one widens what a
trusted frame can do.

`bash`/`bash_output` are its only members at introduction.

### 3. One long-lived `JobRegistry`

The registry is built once and reused across enable/disable cycles, rather than minted per
enablement. This is not a cleanup rider: it is what makes background-job ids stable across a
`/bash off` + `/bash on`, and a prerequisite for the shared `poll` handle namespace
([ADR-0161](0161-unified-async-work-background-flag-and-one-poll.md),
[ADR-0164](0164-short-sortable-kind-tagged-ids.md)).

### 4. What is removed

- `InMsg::BashEnable`, `InMsg::BashDisable`
- `OutEvent::BashChanged`
- the `BashGrade` enum
- the `matches!(tool, "bash" | "bash_output")` special case in `policy.rs`
- most of `bash_live.rs`; what survives is the lazily-registrable-built-in table and the
  `BashToolConfig` capture the registration needs

TUI `/bash on [--allow [<pattern>]] | --ask | off` becomes
`/enable tool bash [--allow [<pattern>]]` / `/disable tool bash`, which is the command surface
ADR-0149 already ships for every other tool.

`ENTANGLEMENT_ENABLE_BASH=1` is unchanged — startup registration stays a separate axis from session
enablement, exactly as [ADR-0010](0010-single-head-crate-and-bash-opt-in.md) established (gate and
profile stay orthogonal).

## Consequences

### Positive

- **Net protocol shrinkage**: two `InMsg` variants, one `OutEvent` variant and an enum removed,
  against one optional struct field added. The brief's contract block loses a bullet.
- **A hardcoded tool-name test leaves the permission path.** `policy.rs` stops knowing the string
  `"bash"`.
- **The `/bash off` + `/bash on` job-orphaning bug is dissolved, not fixed** — a registry built once
  has nothing to orphan.
- **Argument-scoped session grants become available for every tool**, closing an ADR-0149 deferral
  rather than only serving bash.
- **One command surface.** Users learn `/enable tool <name>` once instead of `/enable` for
  everything and `/bash` for bash.

### Negative / neutral

- **This reverses an accepted ADR and removes wire types.** `serve`/`pipe` clients and the TUI all
  speak `BashEnable` today. Both messages are trusted-only and `serve` is local-single-user
  ([ADR-0048](0048-serve-head-local-trust-model.md)), so the blast radius is small — but it is a
  wire removal, and any out-of-tree head sending `BashEnable` breaks.
- **The overlay learns to instantiate tools**, which is a real widening of a trusted frame's power.
  Bounded by the closed table (§2), but the table is now a security-relevant list that must stay
  short and reviewed.
- **`arg_pattern` adds a second pattern field to a struct that already has one**, and the two mean
  different things (tool name vs command argument). Confusing enough that it needs to be spelled out
  in the field's doc comment, not just here — the near-collision of names is exactly what hid this
  gap in the first place.
- **`BashChanged`'s dedicated ack is folded into `ToolOverlayChanged`**, which carries the full
  effective overlay rather than a bash-specific `enabled: bool`. Heads rendering a bash indicator
  must derive it from the overlay list.

## Alternatives considered

- **Keep `BashEnable`; just share the `JobRegistry`.** Fixes the orphaning bug and nothing else,
  leaving two mechanisms for one concept and the `policy.rs` special-case in place. Rejected: the
  duplication is the actual problem, and ADR-0149 already proved the generic form works.
- **Drop command-pattern narrowing and let users write ordinary permission rules in config.**
  Would let the overlay absorb `BashGrade` with no new field. Rejected: `/bash on --allow 'git *'`
  is a *session-scoped, in-the-moment* decision; pushing it into a config file that persists is a
  materially different and worse UX, and would make the migration a capability regression.
- **A `grade: Permission` field instead of `allow: bool` + `arg_pattern`.** More expressive and
  arguably cleaner. Rejected for this change: it would rewrite ADR-0149's wire shape for existing
  senders and replayed logs, where the additive optional field does not. Worth revisiting if a third
  grade axis ever appears.
- **Let any registered-tool *name* be lazily instantiable** rather than a closed table. Rejected:
  that turns a trusted frame into a general tool-construction facility with no review surface.

## References

- [ADR-0133](0133-live-bash-enablement-graded-by-permission.md): the bespoke pair this supersedes
- [ADR-0149](0149-per-session-tool-overlay.md): the generic overlay this extends — built on the
  ADR-0133 precedent it now absorbs
- [ADR-0148](0148-glob-patterns-in-the-agent-tool-mask.md): the tool-name glob semantics
  `ToolOverlayEntry.pattern` uses
- [ADR-0114](0114-capability-level-permission-keys.md): the argument-scoped `tool(pattern)` syntax
  `arg_pattern` materializes into
- [ADR-0010](0010-single-head-crate-and-bash-opt-in.md): the registration/dispatch orthogonality
  `ENTANGLEMENT_ENABLE_BASH` keeps
- [ADR-0161](0161-unified-async-work-background-flag-and-one-poll.md),
  [ADR-0164](0164-short-sortable-kind-tagged-ids.md): the single `JobRegistry` and stable job ids
  this enables
- [ADR-0124](0124-wire-refused-mcp-mutation-and-stdio-key-scrub.md): the trusted-only bar both
  messages meet, unchanged
