# 0093. `call` registers independently of `bash`'s opt-in gate

- Status: Accepted
- Date: 2026-07-16
- Supersedes [0010](0010-single-head-crate-and-bash-opt-in.md) §3 ("`bash` is
  opt-in") and [0045](0045-call-host-tool-argv-exec-tailed-output.md) §3
  ("Registration & permissions — same gate, orthogonal dispatch") for `call`
  specifically; both ADRs stand unmodified for `bash` itself. Issue #386
  (part of #382).

## Context

`call` (argv exec, no shell, [ADR-0045][0045]) has registered under the same
`ENTANGLEMENT_ENABLE_BASH=1` gate as `bash` since it was added — [0045][0045]
§3 reasoned that "`call` runs arbitrary binaries with the engine's privileges —
the same blast-radius class as `bash`, minus the shell — so it registers under
the same opt-in gate," and explicitly deferred a separate gate: *"Revisit
trigger: a concrete embedder that wants `call` without `bash` at the
registration layer."*

That trigger has arrived. `call`'s whole reason to exist is that a fixed argv
— `command` + `args` execed verbatim via `tokio::process::Command`, never
`sh -c` — cannot be shell-injected: no pipes, no globbing, no `$VAR`
expansion, no metacharacter interpretation. What the model *sends* is exactly
what *execs*. That auditability is precisely why [0045][0045] itself argues "a
profile may reasonably `Allow` such a tool while keeping `bash` at
`Ask`/`Deny`." But tying `call`'s *registration* to `bash`'s opt-in flag
undercuts that argument before it can even apply: an operator who wants the
auditable argv-exec path but not the shell currently has no way to get one —
`ENTANGLEMENT_ENABLE_BASH=1` is all-or-nothing, so enabling `call` always also
advertises `bash`.

[0010][0010] §3 only ever analyzed `bash`'s risk (arbitrary shell code,
unsandboxed); it never separately assessed `call`, which didn't exist at the
time (0010 is dated 2026-07-07; `call` landed four days later in 0045). `call`
piggybacked 0010's gate by association, not by a risk analysis of its own.

## Decision

**`call` registers unconditionally**, alongside the root-contained quintet
(`read`/`glob`/`grep`/`edit`/`write`), independent of
`ENTANGLEMENT_ENABLE_BASH`. `entanglement-runtime`'s `main.rs` now registers
`CallTool` outside the `ENTANGLEMENT_ENABLE_BASH` block; `BashTool` and its
`BashOutputTool` poller stay behind the flag exactly as [0010][0010]
established.

This is a change to *registration* only, not to `call`'s privilege or
sandboxing:

- `call` still execs with the engine's full, unsandboxed privileges — no
  sandbox change, no new containment. Registering it by default is safe for
  the same reason the quintet's default registration is safe (0010's own
  argument): a model can now *reach* the tool, but a permission profile still
  decides whether it may *run*.
- Per-profile permission (`Allow`/`Ask`/`Deny`) remains the actual dispatch
  gate, unchanged. `build`'s inherit-all default (`tools: None`, default
  `Allow`) already auto-allowed `call` whenever it was registered under the
  old gate; `plan`/`explore` already mask it out of their explicit `tools`
  allowlist ([ADR-0038][0038]), so neither built-in profile's *behavior*
  changes — only whether the tool is *advertised at all* when
  `ENTANGLEMENT_ENABLE_BASH` is unset.
- A custom profile that wants `call` at `Ask`/`Deny` while still exposing it
  (rather than masking it out) now gets that distinction with zero
  configuration: no env var needs setting for the tool to exist, only the
  profile's own permission grade governs whether it runs unattended.

`bash` is untouched: it keeps registering only under
`ENTANGLEMENT_ENABLE_BASH=1`, per [0010][0010], because it runs arbitrary
shell code — pipes, redirection, `$VAR` expansion, subshells — which is a
categorically larger audit surface than a fixed argv.

## Consequences

- **(+)** A profile can now `Allow call` / `Ask bash` / `Deny bash` *and* have
  `call` actually registered without also opting the whole exec surface into
  `bash`'s shell — closing the gap [0045][0045] flagged as its own revisit
  trigger.
- **(+)** No config surface added: no new env var, no new profile field. The
  existing wildcard defaults (`build`→`Allow`, `plan`→`Ask`, `explore`→`Deny`)
  apply to `call` exactly as before; only its registration became
  unconditional.
- **(+)** Consistent with how the quintet is already unconditionally
  registered "so the `build`/`plan`/`explore` permission profiles gate
  something real out of the box" — `call` now gets the same treatment instead
  of being an exception tied to `bash`.
- **(neutral)** `ENTANGLEMENT_ENABLE_BASH=1` still enables `bash` +
  `bash_output` together (unchanged pair) and now, redundantly, `call` (which
  was already registered). Setting the flag remains a no-op regression risk
  free change for existing deployments that rely on it.
- **(−)** `call` still runs unsandboxed with full engine privileges — this ADR
  does not reduce that risk, only who advertises the tool. A profile whose
  default permission is `Allow` (i.e. `build`) now exposes `call` without any
  operator action at all, same as it already does for `edit`/`write`; a real
  sandbox for `call`/`bash` remains the deferred future security decision
  [0010][0010] and [0045][0045] both already flag.

## Alternatives considered

- **Leave `call` behind `ENTANGLEMENT_ENABLE_BASH` (status quo).** Rejected:
  this is exactly the gap this ADR closes — see Context.
- **A dedicated `ENTANGLEMENT_ENABLE_CALL` env var.** Rejected: `call`'s blast
  radius (arbitrary binary, full engine privilege) is real even without a
  shell, so a bare env-var gate buys no isolation a permission profile
  doesn't already provide, while adding config surface an operator has to
  learn. The per-profile `Allow`/`Ask`/`Deny` grade is the control that
  actually matters here, and it already exists.
- **Reduce `call`'s privilege (e.g. an allowlist of binaries) instead of
  changing registration.** Out of scope: this issue is about *who advertises
  the tool*, not sandboxing `call`'s execution. A real sandbox is the
  standing deferred item from [0010][0010]/[0045][0045].

[0010]: 0010-single-head-crate-and-bash-opt-in.md
[0038]: 0038-physical-per-agent-tool-restriction.md
[0045]: 0045-call-host-tool-argv-exec-tailed-output.md
