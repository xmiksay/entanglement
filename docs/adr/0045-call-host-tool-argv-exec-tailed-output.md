# 0045. Host tool `call`: argv exec (no shell) with auto-tailed output

- Status: Accepted
- Date: 2026-07-11

## Context

[ADR-0009][0009] added `bash` — run a command via `sh -c`, rooted at the working
directory, unsandboxed. [ADR-0010][0010] made it opt-in
(`ENTANGLEMENT_ENABLE_BASH=1`) because it runs with the engine's full
privileges. `bash` is the general escape hatch, but the shell is exactly what
makes it hard to reason about: what the model *sends* (`command`) is not what
*execs* — `sh -c` re-parses it, so pipes, globbing, `$VAR` expansion, and
metacharacters all apply. That's power, but it defeats auditing and opens
injection when any part of the command is assembled from untrusted text.

Two recurring needs are unserved by `bash`:

1. **A structurally injection-free exec path.** Running a *fixed* binary with a
   *fixed* argv — `cargo test`, `git status`, `ls somedir` — should not route
   through a shell at all. If argv is passed verbatim to `exec`, there is no
   parse step to inject into, and the call is auditable (the argv *is* the whole
   story). This is a strong enough property that a permission profile may
   reasonably `Allow` such a tool while keeping `bash` at `Ask`/`Deny`.

2. **Output that fits the context window by default.** `bash`/`call` output is
   byte-capped ([ADR-0008][0008], 32 KiB) but capped from the *front*: a long
   build/test run's most valuable lines — the summary, the final error — sit at
   the *end*, exactly where the byte cap drops them. The model then re-runs to
   see the tail it needed.

## Decision

### 1. `call` — direct process execution, no shell

`CallTool` takes `{ command, args = [], tail = 30, timeout? }` and runs
`command` with `args` as **argv**, via `tokio::process::Command::new(command)
.args(args)` — **no `sh -c`**. What the model sends is exactly what execs: no
pipe/glob/`$VAR`/metacharacter interpretation. This is the structural difference
from `bash` and the reason `call` is separately gate-able by a profile.

The execution envelope is otherwise identical to `bash` ([ADR-0009][0009]): cwd
= tool root (cwd only — *not* sandboxed), default 120 s timeout capped at 600 s,
`kill_on_drop(true)` reaps on expiry, `[exit N]` prefix, stderr rendered in a
separate `[stderr]` block. A missing binary surfaces as a clean spawn error
(`spawning \`<command>\``), never a panic ([ADR-0016][0016]).

### 2. Auto-tailed output (`tail`, default 30)

Each stream is reduced to its **last `tail` lines** (`tail -n 30` semantics)
because command-output value concentrates at the end. When lines are dropped, a
notice is **prepended** so the model can self-correct mechanically
([ADR-0016][0016] empty/truncated-result contract):

```
(… 412 earlier lines omitted, tail=30 — rerun with tail=0 for full output)
```

- **`tail = 0`** disables line cutting — full output, bounded only by the outer
  byte cap. Documented in the schema so the model reaches for it *deliberately*
  (when it needs the whole thing) rather than habitually.
- stdout and stderr are tailed **independently** with the same `tail` value, so
  a noisy stderr can't crowd out stdout's tail and vice-versa.
- The **32 KiB byte cap ([ADR-0008][0008]) still applies** as the outer bound
  after tailing — a `tail=0` run, or 30 pathologically long lines, is still
  truncated, and that notice (`... [truncated: N bytes total]`) names the *byte*
  limit, so the two limits are distinguishable in the output.

### 3. Registration & permissions — same gate, orthogonal dispatch

`call` runs arbitrary binaries with the engine's privileges — the same
blast-radius class as `bash`, minus the shell — so it registers under the **same
opt-in gate**: `ENTANGLEMENT_ENABLE_BASH=1` now enables the whole exec *pair*
(`bash` + `call`). Embedders never get silent exec capability. Gate
(registration) and profile (dispatch) stay orthogonal ([ADR-0010][0010]):
`build` auto-allows both; a profile could `Allow` `call` while keeping `bash` at
`Ask`/`Deny`, trading the shell's flexibility for the argv path's auditability.
No profile-schema change is needed — the existing wildcard defaults
(`build`→`Allow`, `plan`→`Ask`, `explore`→`Deny`) apply to `call` as-is.

## Consequences

- **(+)** An injection-free, auditable exec path: a fixed argv can't be
  shell-injected, so a security-conscious profile can allow it without allowing
  the shell.
- **(+)** Default-tailed output puts the valuable end of build/test logs in front
  of the model without a re-run, and the omission notice makes widening to
  `tail=0` a one-step mechanical fix.
- **(+)** Complements `bash` rather than replacing it — both can be registered;
  the model picks the shell only when it genuinely needs shell features.
- **(−)** No shell means no pipes/redirection/expansion in a single `call` — a
  pipeline needs `bash` (or multiple `call`s). Intended: that's the whole point.
- **(−)** Two exec tools is more surface for the model to choose between; the
  schema descriptions carry the "fixed command → `call`, shell features →
  `bash`" guidance.

## Alternatives considered

- **Head-instead-of-tail (keep the first N lines).** Rejected: build/test/CI
  output front-loads setup noise and back-loads the result (summary, first
  failing assertion, final error). Head-truncation drops exactly the lines the
  model came for. Tail matches where value sits; `tail=0` covers the rare
  need-the-whole-thing case.
- **Byte-based truncation only (no line tail).** That's the [ADR-0008][0008]
  status quo and the problem: a byte cap from the front discards the tail. A
  line-aware tail is what makes the default useful; the byte cap is kept purely
  as the outer safety bound.
- **Fold argv-exec into `bash` (add an optional `args` array).** Rejected: it
  would conflate two security models in one tool and one permission decision. The
  whole value of `call` is that a profile can gate it *separately* from the
  shell; a merged tool can't express "argv yes, shell no."
- **Make `call` a distinct opt-in gate.** Rejected for v1: `call`'s blast radius
  (arbitrary binary, engine privileges) is the same class as `bash`, so a second
  env var buys no real isolation while adding config surface. Profiles already
  provide the per-tool differentiation. **Revisit trigger:** a concrete embedder
  that wants `call` without `bash` at the *registration* layer.

[0008]: 0008-host-tools-workdir-and-bounded-output.md
[0009]: 0009-edit-and-bash-host-tools.md
[0010]: 0010-single-head-crate-and-bash-opt-in.md
[0016]: 0016-host-tool-empty-result-contract.md
