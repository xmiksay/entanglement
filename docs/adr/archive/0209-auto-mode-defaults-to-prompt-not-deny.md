# 0209. `auto` defaults to `prompt`, not `deny`

- Status: Accepted
- Date: 2026-09-23
- Amends: [ADR-0207](0207-permission-modes-replace-agent-borne-authority.md) §11
  (the unattended posture keeps its bounds; only the *default grade* changes)

## Context

ADR-0207 §11 gave `auto` `default: deny` and called that default "load-bearing
rather than decorative". Two separate claims were folded into that one line:

1. Exec must be an **explicit command list**, never the `exec` capability
   class — otherwise nothing unenumerated is ever caught.
2. The **default grade** must be `deny`, because "there is no one to ask".

Claim 1 is correct and this ADR keeps it. Claim 2 was wrong, and in a way that
disabled the rest of §11.

§11 also specifies an escalation: "A `prompt` grade likewise collapses to a
denial here, since there is no one to ask. A repeated *identical* denied call
parks an approval on its second occurrence." That is implemented, in
`run_limits::collapses_ask` and `run_limits::DenialTracker::record_repeat`,
and dispatch wires it on the `Permission::Ask` arm:

```rust
Permission::Deny => { /* absolute — no prompt, replies immediately */ }
Permission::Ask if run_limits::collapses_ask(&limits)
    && !denials.record_repeat(&session, &tool, arg.as_deref()) => { /* collapsed denial */ }
_ => { /* park an approval, bounded by question_timeout */ }
```

A `Deny` never reaches the escalation. With `default: deny`, *every command
not explicitly enumerated* — i.e. nearly every command — took the absolute
arm. The reported case was `gh issue delete`, which matches no rule in
`readonly_exec.yml` (the curated set allows `gh issue list`/`view`, not
`delete`) and no entry in `auto`'s own deny list. The model called it, was
refused, correctly retried to trigger the documented second-call escalation,
and was refused identically. There was no retry count that would ever have
worked, and no way for an attended user to approve it.

So the §11 escalation machinery was fully implemented and fully tested — and
unreachable from the mode that ships. Every test covering it
(`unattended_mode.rs`) built its own synthetic `Permission::Ask` mode table,
so none of them noticed.

`auto` was not "unattended with bounded escalation". It was "silently refuse
whatever nobody thought to enumerate", which is a poor posture even when
genuinely unattended and an actively broken one when a user is sitting there
watching — the common case, since `auto` is also what a bare plan approval
selects (§7, #560).

## Decision

**`auto.yml` sets `default: prompt`.** The unattended guarantee comes from
`question_timeout` + `on_timeout`, not from the default grade.

Nothing else about the posture changes:

- Exec stays an **explicit command list**, never the `exec` capability class.
  This is the part of §11's "load-bearing" claim that was actually doing the
  work: a class allow would run unenumerated commands *silently*, which is a
  different thing from prompting for them.
- The explicit `deny` list stays absolute. Under `default: prompt` it carries
  more weight than before — it is now the only grade an unattended run refuses
  with no escalation path — so anything genuinely irreversible belongs there
  rather than relying on the default.
- `question_timeout: 60`, `on_timeout: deny`, `max_depth`, `max_agents`,
  `max_turns`, `max_duration` are untouched.
- `request_mode` is still refused outright in `auto` (§10): a model may not
  widen its own authority, and asking for one bounded approval is not that.

The resulting behavior for an unenumerated command:

| | first call | identical second call | outcome |
| --- | --- | --- | --- |
| Genuinely unattended | collapsed refusal, no park | parks an approval | nobody answers → `on_timeout: deny` refuses after 60s |
| Attended | collapsed refusal, no park | parks an approval | the user decides |

The unattended column ends exactly where `default: deny` ended — refused, turn
continues — one bounded prompt later. The attended column is the one that was
broken.

## Consequences

- An attended `auto` session can approve an unenumerated call. This is the
  point.
- A genuinely unattended run pays one parked approval (up to
  `question_timeout`) per distinct repeated call before refusing. Bounded by
  `max_turns`/`max_duration` like everything else.
- A malformed `bash` input (one `permission::permission_arg` cannot parse)
  grades with `arg: None`, matches no argument-scoped rule, and so now reaches
  `prompt` where it previously hit `deny`. Not a widening in practice: the
  tool receives the same unparseable input and cannot execute it either.
- `mode/describe.rs`'s `auto` summary — which the model reads in its system
  prompt — now states the escalation, so a blocked model knows retrying once
  is the documented move rather than discovering it by accident.
- The regression pin is `unattended_mode.rs`'s two
  `the_real_auto_mode_*` tests, which exercise `ModeTable::builtin()` rather
  than a synthetic fixture. The synthetic-fixture blind spot is the reason
  this shipped; testing the real table is the fix for the blind spot, not just
  for the bug.

## Alternatives considered

**Enumerate the missing commands in `readonly_exec.yml` / `auto.yml`.** Treats
the symptom. `gh issue delete` was one instance of an unbounded class — every
command nobody happened to list behaves the same way. Enumeration is also the
wrong tool here: the curated list exists to let read-only commands skip the
prompt, not to be exhaustive over everything a user might legitimately approve.

**Leave `auto` alone; tell users to `/mode build`.** This is what the denial
message says, and it is a reasonable answer to "I picked the wrong mode". It is
not an answer to "`auto` refuses one command in a 200-step unattended run":
switching mode for that one call discards the bounded posture the user chose
`auto` for, and the whole run's `max_turns`/`max_duration` with it.

**Make `Permission::Deny` escalate on a repeat too.** Collapses the distinction
between "not enumerated" and "explicitly forbidden". A mode `deny` being
absolute is load-bearing across the whole ADR-0207 design (§4), and `rm -rf /`
becoming approvable-on-retry in the *unattended* mode is the opposite of the
fix.

**A fifth mode** (`auto` strict + `auto` interactive). Modes are compiled in
and deliberately not user-definable (§5); adding one to paper over a wrong
default in an existing one is cost with no benefit — the timeout already
distinguishes the two situations at runtime, without the user having to
predict in advance which one they are in.
