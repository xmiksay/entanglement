# 0195. `bash` is the default exec + curated read-only Allow rules

- Status: Accepted
- Date: 2026-09-13
- Supersedes: [ADR-0163](0163-live-bash-enablement-is-a-tool-overlay-entry.md)
  (its *posture* — `bash` as a lazily-registrable built-in behind
  `/enable tool bash` and `ENTANGLEMENT_ENABLE_BASH` — is retired; `bash` is
  registered at startup like every other built-in). The rest of ADR-0163
  stands: the overlay entry shape (`arg_pattern`), the closed
  lazily-registrable table concept (now empty), and the single long-lived
  `JobRegistry` are unaffected.
- Relates to: [ADR-0010](0010-single-head-crate-and-bash-opt-in.md) (the
  original opt-in gate this reverses), [ADR-0093](0093-call-registration-independent-of-bash-opt-in.md)
  (`call`'s unconditional registration — the precedent this extends to
  `bash`), [ADR-0114](0114-capability-level-permission-keys.md)/[ADR-0051](0051-argument-scoped-permission-rules.md)
  (the argument-scoped rule machinery the curated defaults ride),
  [ADR-0176](0176-structured-tool-result-is-error-and-duration-fields.md)
  (the `is_error` channel the retired decline rode).

## Context

ADR-0163 left `bash` behind an opt-in: registered only when
`ENTANGLEMENT_ENABLE_BASH=1`, otherwise conjured live by a trusted overlay
entry (`/enable tool bash`). The machinery that supports this is
nontrivial: a closed table of lazily-registrable built-ins (`LAZY_BUILTINS`,
one member), a `bash_live` responder folding `ToolOverlayChanged` into
registrations, a dedicated `disabled_builtin_decline` wording for the
advertised-but-unregistered state, and ADR-0179-then-ADR-0192's
advertisement gymnastics to keep the tools array stable across an enable.

The posture dates from ADR-0010, when `bash` was the only exec tool and
gating it gated the whole shell surface. Two things changed:

1. **Permission is the real gate and always was.** Every `bash` call grades
   through the profile (`explore`/`plan` ask; a ceiling `bash: deny` wins
   over everything), the argument-scoped rule machinery (#173/#418) can
   grade per command, and the sandbox/per-profile scoping (ADR-0104/0134)
   bounds what a run can touch. Registration only decides whether the tool
   *exists* — and ADR-0093 already accepted, for `call`, that registration
   is not where the security story lives.
2. **The opt-in's UX cost is now the observed failure mode.** In practice
   the agent hits `tool bash is disabled — enable with /enable tool bash`
   on first shell use, every session, and the user re-enables by hand (or
   sets the env var, which the docs then have to carry). An opt-in whose
   steady state is "everyone turns it on" is a speed bump, not a boundary —
   and each enable historically carried advertisement/seam costs that
   ADR-0179 and ADR-0192 existed to paper over.

## Decision

### 1. `bash` is registered at startup

`BashTool` registers alongside the sextet and `call` in every head; the
`skutter` binary no longer gates it on `ENTANGLEMENT_ENABLE_BASH`. What is
retired:

- `/enable tool bash` overlay registration (the closed
  lazily-registrable-built-in table loses its only member and the table
  itself goes), `ENTANGLEMENT_ENABLE_BASH` (the env var is ignored/removed
  from the index), the `LAZY_BUILTINS` machinery, and
  `decline::disabled_builtin_decline` (there is no
  advertised-but-unregistered built-in left to decline for).
- `bash_live.rs`'s registration half; what survives is the one long-lived
  `JobRegistry` (ADR-0163 §3) and the `BashToolConfig` capture, now wired
  at startup.

The overlay's *grade* semantics for other tools (enable entries,
`arg_pattern` narrowing, ADR-0163 §1) are untouched — only bash's
lazily-registrable membership is gone. An overlay enable entry naming
`bash` is now a pure grade override (the tool is always registered);
a `/disable tool bash` deny entry still withdraws it at dispatch for that
session, same as any tool.

Sandbox (`ENTANGLEMENT_SANDBOX`), per-profile sandbox scoping, secret
scrubbing, process-group containment, and the permission posture are all
unchanged — this is a registration-posture change only.

### 2. `call` stays registered — but unadvertised

`call` keeps its unconditional registration (ADR-0093): rhai's `exec`
binding marshals through it, embedders use it, and its argv-precise
`command`+`args` shape remains the injection-free sibling for a fixed
command. But under [ADR-0193](0193-two-tool-call-modes-and-lazy-tool-discovery.md)
it is **not part of the lean kernel**: in invoke mode it is
invoke-reachable and `tools`-listed, never in the advertised array. In
native mode it stays advertised as today (the full surface is the native
contract).

### 3. Curated embedded read-only Allow rules

Rather than an exec shim or a read-only command classifier (both rejected —
see Alternatives), exec friction is handled by the **existing**
argument-scoped permission machinery: the embedded permission defaults ship
a small, curated set of **Allow rules for read-only commands**, exact-prefix
patterns in the ADR-0114 `tool(pattern)` syntax:

- `bash(find *)`, `bash(grep *)`, `bash(ls *)`, `bash(cat *)`,
  `bash(head *)`, `bash(tail *)`, `bash(wc *)`
- `call(find *)`, `call(grep *)`, `call(rg *)`, and the same
  ls/cat/head/tail/wc set for `call`

These are *defaults* in the embedded `config.yml` layer, so the usual
layering applies: a user or project layer can tighten or widen them, and
**the config ceiling still clamps least-privilege over every grade** — a
user ceiling of `bash: ask` (or `bash(find *): ask`) still forces a prompt
over a curated Allow, exactly as it clamps any profile's own `Allow`. The
curated set never loosens what the ceiling or a profile denies; it only
removes the prompt for commands that cannot mutate anything.

The list is deliberately short and exact-prefix (no `bash(git *)` —
`git` can write; no `bash(echo *)` — redirection makes it a write; no
class like `bash(* --help)`). Growing it is a deliberate embedded-defaults
change, reviewed like any code change.

## Consequences

### Positive

- The agent's first shell use just works; the every-session re-enable dance
  and its documentation burden are gone.
- The lazily-registrable machinery (`LAZY_BUILTINS`, the registration fold,
  `disabled_builtin_decline`) is deleted — the last built-in whose
  existence was conditional, and with it the last
  advertised-but-unregistered decline path.
- Read-only commands stop costing an approval round-trip each, without any
  new permission concept: the rules are ordinary `tool(pattern)` entries a
  user already knows how to read and override.
- The ceiling keeps final authority — the curated defaults are
  least-privilege-compatible by construction (Allow only, exact-prefix
  read-only commands, clamped by everything above them).

### Negative / neutral

- `bash` exists for every profile out of the box. A deployment that wants
  no shell at all must now say so in config (`bash: deny`) rather than
  rely on omission. This is the same trade ADR-0093 made for `call`, now
  extended: existence is cheap, grading is the control.
- An embedder that previously never set `ENTANGLEMENT_ENABLE_BASH` and
  relied on bash's absence now has `bash` registered — the permission
  default for it in `EngineConfig::default()`'s empty-profile posture must
  be checked in implementation (the shipped heads' profiles already grade
  it; the lean embedder path is the one to verify).
- The curated Allow list is a new embedded default users inherit — visible
  in `skutter inspect config`, overridable per layer, but it changes
  out-of-the-box approval behavior for the listed commands. Deliberate:
  that is its purpose.
- A repo (project-layer config) can re-loosen or re-tighten the curated
  set like any permission rule; the existing `ceiling_warn` startup
  warning covers the re-loosening direction, as it already does for any
  key the project layer flips.

## Alternatives considered

- **Keep the opt-in; just document it better.** Rejected: the failure mode
  is not ignorance of the flag, it is that the steady state is "enabled".
  Documentation cannot fix a posture whose default everyone overrides.
- **An exec shim** (wrap every command in a classifier/proxy that gates
  read-only vs mutating). Rejected (plan's settled decision 2): a whole new
  trusted component on the exec path, wrong layer — the argument-scoped
  permission rules already express "this command, this grade" exactly, with
  review, layering, and a ceiling.
- **A read-only command classifier** (parse the command, decide
  read-only-ness at dispatch). Rejected for the same reason, plus: command
  grammar is not a security predicate (`grep` can `-r` into anything,
  redirection makes `cat` a write; the classifier would be forever
  behind). Exact-prefix rules are auditable one-liners instead.
- **`Allow` the whole `bash` tool by default.** Rejected: that discards the
  per-command granularity the machinery already has, and hands the model
  blanket shell with no prompt. The curated list is deliberately narrow.
- **Ship the curated rules in the ceiling layer rather than the profile
  layer.** Rejected as framing: the embedded defaults *are* the lowest
  layer both profiles and the ceiling compose over; placing the Allow rules
  there lets profiles clamp them (a read-only agent still asks) while the
  ceiling clamps everything — the reverse placement (ceiling-side Allow)
  would fight the ceiling's tighten-only semantics.

## References

- [ADR-0163](0163-live-bash-enablement-is-a-tool-overlay-entry.md): the
  opt-in posture superseded (its §1/§3 machinery that survives)
- [ADR-0010](0010-single-head-crate-and-bash-opt-in.md): the original gate
  this finally reverses
- [ADR-0093](0093-call-registration-independent-of-bash-opt-in.md): the
  registration-is-not-security precedent
- [ADR-0114](0114-capability-level-permission-keys.md)/[ADR-0051](0051-argument-scoped-permission-rules.md):
  the `tool(pattern)` syntax the curated rules are written in
- [ADR-0192](0192-universal-advertisement-enforcement-at-dispatch.md): the
  advertisement seam the lazy machinery existed to manage
- [ADR-0193](0193-two-tool-call-modes-and-lazy-tool-discovery.md): the
  lean kernel `call` sits outside of
