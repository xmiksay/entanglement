# 0235. Scoped spawn rules grade the target

- Status: Accepted
- Date: 2026-10-18
- Issue: #867 (found in the #849 review)
- Amends: [ADR-0232](0232-spawning-is-graded-bookkeeping-control-tools-non-maskable.md)
  §1 (spawning is graded — now with a permission argument)

## Context

ADR-0232 made `agent`/`agent_send` climb the dispatch ladder, but
`permission::permission_arg` gave neither an argument. Two things broke:

- A scoped rule like `deny: ["agent(build)"]` loaded, named a graded tool,
  and never matched. Nothing warned.
- The unattended repeat-denial key (ADR-0207 §11) and the grant key are
  `(tool, argument)`. With no argument, every spawn had the same key. A
  second, *different* spawn counted as a repeat and parked a card, and an
  "always allow" earned on one spawn covered every target.

The choice was to give the pair a real argument or to warn on scoped keys.
Scoped rules were chosen: "don't spawn `debug` unattended" is a real
policy, and the repeat key needs a target either way.

## Decision

1. **`agent`'s permission argument is the profile the spawn runs.** It
   comes from `subagent::target_agent`, the same parse the spawn uses. An
   omitted or empty `agent`, or a non-JSON input, is the default profile
   (`general`). The argument is what actually runs, so `agent(general)`
   also covers a call that names no agent. A rule cannot be dodged by
   leaving the field out. No extra normalization: an unknown or misspelled
   name never reaches the gate — `spawn_refusal` (with the model check)
   refuses it first, before the ladder runs.
2. **`agent_send`'s permission argument is its `agent_id` handle,
   trimmed.** Empty or missing means no argument (only a bare rule
   matches; the call is refused for the missing id anyway). `agent_send`
   resolves the handle trimmed the same way, so the handle graded is the
   handle prompted. The handle is
   chosen over the live session behind it: the model only knows the
   handle, and the handle stays stable across the child's compactions
   (ADR-0234), so a grant earned on it keeps matching. Handles are minted
   ids, so a config rule is in practice a bare `agent_send` or a tenant
   prefix (`agent_send(acme:*)`). The handle is still the right key for
   grants and repeats. For the same reason an `agent_send` card offers only
   `Once` and `Session` (`pending::scopes_for`): a persisted `Always` grant
   on a handle minted for one run could never match again.
3. Everything else follows from the generic `(tool, argument)` machinery,
   with no spawn-specific code: scoped rules match through the usual
   matcher and precedence (a rule naming the tool outranks a class rule; a
   `*` rule competes by length with a named rule; longest within a kind).
   A grant is recorded and matched per target. The unattended repeat key
   tells targets apart. `config::mode_warn` never warned on scoped
   `agent(...)` keys, and a scoped key on a non-maskable tool still warns.
4. The run summary and TUI header keep their `target + prompt hint`
   rendering for the pair rather than the bare permission argument. The
   `agent` header leads with the graded profile (`general` when the input
   names none), so a card always names what a grant would record.

## Consequences

- A bare `agent` grant recorded since #849 (key `("agent", None)`) no
  longer matches any call. #849 is unreleased, so no migration is needed.
- `PermissionResolver` implementations that read `permission_arg` see the
  target for the pair from now on.

Rejected:

- **Warn on scoped `agent(...)` keys instead.** This leaves the repeat and
  grant keys collapsed, which is the actual bug.
- **Key `agent_send` on the child's profile.** That is a stable, readable
  rule, but `permission_arg` is a pure function of `(tool, input)` shared
  by resolvers, grants and display, and the profile needs registry state.
  Threading the registry through every resolver is out of scope.
