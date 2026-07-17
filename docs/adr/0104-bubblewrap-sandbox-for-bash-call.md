# 0104. OS-level process confinement for `bash`/`call` via bubblewrap — opt-in, fail-closed

- Status: Accepted
- Date: 2026-07-17

## Context

`bash` ([ADR-0009][0009]) and `call` ([ADR-0045][0045]/[ADR-0093][0093]) run
model-authored commands with the engine process's **full privileges** — no
filesystem, network, or process confinement. `root` sets only the child's cwd;
a `bash -c` command can still read `~/.ssh`, reach the network, or write
anywhere the invoking user can. [ADR-0054][0054] canonicalizes symlink-safe
containment for the *file tools* (`read`/`edit`/`write`/`glob`/`grep`), but
explicitly is not a sandbox and does not apply to what a shell command itself
does once running. [ADR-0009][0009] named this gap and deferred it: *"a real
sandbox is deferred to a focused security ADR."*

[ADR-0047][0047] accepts unsandboxed execution as correct for entanglement's
**default** deployment — a local, single-user dev tool where the trust
boundary is the user's machine, not a directory inside it. That acceptance
does not extend to every deployment: a `serve` head fed untrusted input, or a
multi-tenant embedder, needs an actual confinement mechanism to even be
considered — this ADR exists to make one available, not to change the
trusted-local default.

Candidates from the issue: Linux namespaces via raw `unshare`, bubblewrap
(`bwrap`), firejail, seccomp. Open questions to settle: fail-open vs
fail-closed, per-profile vs global opt-in, network egress policy, and how a
sandbox composes with ADR-0054's root containment.

## Decision

### 1. Backend: bubblewrap (`bwrap`)

Bubblewrap is the confinement mechanism — it is designed from the ground up
for **unprivileged use** (it is the sandbox underneath Flatpak), needs no
`CAP_SYS_ADMIN` grant from us (it works via unprivileged user namespaces, or
its own minimal setuid helper where those are disabled), is a small,
narrowly-scoped, heavily-reviewed tool, and is invoked as a plain external
binary — no new Rust dependency, `entanglement-runtime` already spawns
`tokio::process::Command`s.

### 2. Fail-closed by omission, not by a new check

There is no code path that falls back to running a command unsandboxed when
the sandboxed spawn can't be entered — `bwrap` missing from `PATH`, or a
kernel with unprivileged user namespaces disabled (e.g.
`kernel.unprivileged_userns_clone=0`), simply makes the `Command::spawn()`
call return an `Err`, which already propagates as a clean tool error (the
existing ADR-0016 contract for a missing binary). Fail-closed is thus the
*absence* of a fallback branch, not new logic to audit.

### 3. Global, opt-in, off by default — per-profile is a follow-up

`ENTANGLEMENT_SANDBOX=bwrap` (mirroring `ENTANGLEMENT_ENABLE_BASH`'s existing
opt-in idiom, [ADR-0010][0010]) turns sandboxing on for every `bash`/`call`
invocation in the process; unset (the default) is byte-identical to today —
unsandboxed, matching [ADR-0047][0047]'s trusted-local default. Per-profile
opt-in (`agent.md: sandbox: bwrap`, so a `debug` profile stays unsandboxed
while `build` is confined) is the more precise design and is **not** ruled
out, but `AgentProfile` ([entanglement-core][protocol]) is a wire type shared
with every head, and the tool registry a session dispatches through
(`SharedRegistry`, [ADR-0096][0096]) is currently profile-agnostic — one
`BashTool`/`CallTool` instance serves every session. Threading a per-profile
policy through cleanly wants the session-aware execution seam
([ADR-0088][0088], `run_for_session`) to carry it, which is a separable
change. Settling for a global switch now ships a real mechanism instead of
blocking on that architecture work; per-profile scoping is the tracked next
step.

### 4. Network: cut by default, explicit opt-in to share it back

Sandboxed calls get `--unshare-net` unless `ENTANGLEMENT_SANDBOX_NETWORK=1` is
also set. The issue is explicit that cutting egress is a large part of a
sandbox's value; defaulting to cut means an operator who wants network access
back (e.g. an agent that must reach a package registry) makes that an
affirmative choice rather than the sandbox silently preserving today's
wide-open egress.

### 5. Filesystem: bwrap is the outer boundary, ADR-0054 containment stays the inner one

The fixed recipe:

```
bwrap
  --ro-bind / /            # host filesystem, read-only
  --bind <root> <root>     # project root, read-write, same path as outside
  --dev /dev --proc /proc --tmpfs /tmp
  --unshare-pid --unshare-ipc --unshare-uts --unshare-cgroup [--unshare-net]
  --die-with-parent --new-session
  -- <program> <args...>
```

The root bind-mount lands at the **same absolute path** inside the sandbox as
outside it, so `resolve_under_root`'s existing canonicalizing containment
(symlink-safe, [ADR-0054][0054]) keeps working completely unmodified — a
`workdir` escape attempt is still caught by the same code, now with the
sandbox as a second, coarser layer underneath (defense in depth, not a
replacement). Everything outside the project root is read-only; `/tmp` is a
fresh, empty tmpfs (scratch space does not survive past one call — acceptable,
`call`'s own durable artifacts already live under the project root, not
`/tmp`).

### 6. Composes with the existing timeout/cancel machinery unchanged

`bash`/`call` already spawn in their own process group and SIGKILL that group
on timeout or `Stop` ([ADR-0009][0009] §168/#169). Verified empirically: a
`--unshare-pid` sandbox's inner bwrap process (the sandbox's PID-1) sets
`PR_SET_PDEATHSIG` against the *outer* bwrap process; killing the outer
process (what `wait_or_kill_group` already does — it only ever sees the outer
`bwrap` as the spawned child) kills the inner one automatically, and a
Linux PID namespace is torn down — every remaining task in it force-killed —
the instant its PID-1 dies. So the whole sandboxed tree dies on a plain
process-group SIGKILL to the one PID `wait_or_kill_group` already tracks; no
change needed to `entanglement-runtime::host::exec`.

### 7. Secret-env scrubbing continues to apply

`bash`/`call`'s `secret_env` scrub ([ADR-0009][0009] #164) is `env_remove` on
the spawned `Command` — which, sandboxed, is the outer `bwrap` process.
`bwrap` passes its own process environment through to the sandboxed command by
default (no `--clearenv` in the recipe), so a scrubbed variable stays absent
inside the sandbox exactly as it was outside it.

### 8. Implementation shape

A new `entanglement-runtime::host::sandbox` module: `SandboxBackend {None,
Bubblewrap}`, `SandboxPolicy {backend, network}`, `SandboxPolicy::from_env()`
reading the two env vars above, and `sandbox::command(policy, root, cwd,
program, args) -> tokio::process::Command` — `None` returns a plain
`Command::new(program)` (today's behavior byte-for-byte), `Bubblewrap` returns
the recipe above. `BashTool`/`CallTool` gain a `.with_sandbox(policy)` builder
(same shape as `.with_secret_env(...)`), wired once in `register_default_tools`
from `SandboxPolicy::from_env()` — the same wiring point `ENTANGLEMENT_ENABLE_BASH`
already uses.

## Consequences

- **(+)** A real, tested confinement mechanism exists and is one env var away
  — the prerequisite the issue asked for before `serve` can be considered for
  anything beyond the fully-trusted local case (`serve` itself stays
  local-only regardless, [ADR-0048][0048] unchanged).
- **(+)** Fail-closed by construction — there is no "sandbox unavailable, ran
  anyway" branch to ever regress.
- **(+)** No change to the process-group timeout/cancel path — verified the
  existing kill mechanism tears down a sandboxed tree too.
- **(−)** Global, not per-profile — every sandboxed call in a process shares
  one policy. A mixed run (one profile confined, another not) needs the
  follow-up threading the policy through `run_for_session`.
- **(−)** Bubblewrap-only dependency: a host without a working `bwrap`, or with
  unprivileged user namespaces disabled, can't use this — it fails closed
  (a clean spawn error), which is correct but means "I set the env var and it
  refuses every command" is a real support case; the spawn error should be
  legible enough to point at the requirement.
- **(−)** No seccomp syscall filtering yet — namespace + mount confinement is
  the win banked here; a seccomp layer under the same recipe is a smaller,
  separable follow-up if ever warranted.
- **(−)** `/tmp` is wiped per call when sandboxed — a command relying on
  scratch state surviving across separate `bash`/`call` invocations would
  break; not a supported use case today regardless.

## Alternatives considered

- **Raw namespaces via `unshare`/a namespaces crate.** Rejected: reinvents
  bubblewrap's mount/pivot_root/uid-gid-mapping logic with more surface to get
  right, for no behavior beyond what `bwrap` already gives as a battle-tested,
  narrowly-scoped external tool.
- **firejail.** Rejected: larger attack surface than bubblewrap, a SUID-root
  model with a history of sandbox-escape CVEs against untrusted input — worse
  fit for the exact threat model (a model-authored command) this ADR is
  guarding against.
- **seccomp as the primary mechanism.** Rejected as a standalone answer:
  syscall filtering doesn't confine filesystem or network reach, which is the
  actual ask. Remains a candidate to layer *under* the bwrap recipe later.
- **Per-profile opt-in in this ADR.** Rejected for v1: the tool registry isn't
  profile-scoped yet ([ADR-0096][0096]); doing it properly wants
  [ADR-0088][0088]'s session-aware seam, a separable change. A global switch
  ships something real now rather than blocking on that.
- **Fail-open (best-effort sandbox, silent unsandboxed fallback when `bwrap` is
  missing).** Rejected: an operator opting in for the security property must be
  able to trust it never silently downgrades — exactly the failure mode that
  would defeat the issue's multi-tenant/untrusted-input motivation.

[0009]: 0009-edit-and-bash-host-tools.md
[0010]: 0010-single-head-crate-and-bash-opt-in.md
[0045]: 0045-call-host-tool-argv-exec-tailed-output.md
[0047]: 0047-local-trust-boundary.md
[0048]: 0048-serve-head-local-trust-model.md
[0054]: 0054-canonicalizing-symlink-safe-root-containment.md
[0088]: 0088-session-aware-tool-execution.md
[0093]: 0093-call-registration-independent-of-bash-opt-in.md
[0096]: 0096-dynamic-toolregistry-sharedregistry.md
[protocol]: ../architecture/protocol.md
