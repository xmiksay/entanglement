# 0046. `rhai` host tool — embedded, capability-sandboxed script engine

- Status: Accepted
- Date: 2026-07-11

## Context

Models routinely reach for "shell out to `python3`/`node` with a heredoc" to do
multi-step logic in one turn — filter a `glob`, transform some text, loop an
`edit` over N matches. The engine's only scripting escape today is `bash`
(ADR-0009/ADR-0010): opt-in, **unsandboxed**, and inheriting the process's full
privileges. A real OS sandbox for `bash` is deferred to a future security ADR;
this issue (#122) is the answer for the *scripting* use-case without waiting for
that.

The forces:

1. **Capability-based, deny-by-default.** Any external interpreter (`python3`,
   `node`) starts with the whole OS — filesystem, network, process spawn, env —
   and can only be contained by OS-level sandboxing we don't have. We want the
   inverse: an engine that starts with **nothing** and is handed exactly the
   capabilities the harness binds.
2. **Resource-bounded by construction.** A model-authored loop must terminate
   deterministically with a clear error, never an OOM or a hung process.
3. **Bindings must not escalate privilege.** A script that can `edit` must be no
   more privileged than a model-issued `edit` call: the same `Allow | Ask | Deny`
   dispatch (#59), the same #116 tool mask, the same ancestor clamp (#77).
4. **Zero core surface.** Like `ask_user`/`propose_plan`, this belongs entirely
   in the runtime executor.

## Decision

Add a runtime-owned host tool **`rhai`** (`entanglement_runtime::script`) that
runs a [Rhai](https://rhai.rs) script — pure-Rust, embeddable, no new binary
dependency, cross-compile + lean-build friendly.

**Sandbox.** A `rhai::Engine::new_raw()` (no packages, **no module resolver** —
so `import` cannot reach the filesystem) plus the IO-free `StandardPackage`
(arithmetic/logic/strings/arrays/maps/time — no file/network/process). `eval` is
disabled (no parser re-entry). Resource caps: `max_operations`,
`max_call_levels`, `max_string_size`/`max_array_size`/`max_map_size`, and a
wall-clock timeout (default 5s, max 30s) enforced by the `on_progress`
interrupt. `print(...)` is captured; the script's last-expression value is
serialized (JSON, with a display-form fallback) and returned bounded to the
ADR-0008 32 KiB cap.

**Bindings = the tool registry, permission-checked per call.** The only
capabilities bound are the root-contained quintet — `read`/`glob`/`grep`/`edit`/
`write` (with the same overloads the tools expose) — each **delegating to the
registered `Tool` impl**, so root containment, bounded output, the ADR-0016
empty-result contract, and any `FileChange` audit come for free. Each binding
resolves permission exactly like a `ToolExec`: `Deny` (or a #116 mask) throws a
catchable script exception; `Allow` runs; `Ask` parks the script on the normal
`ToolRequest` → `Approve`/`Reject` round-trip. **`Ask` is resolved once per
function per run** (the first `edit` asks, approval covers the rest) — per-call
prompts in a loop would be noise. No exec bindings (`bash`/`call`) in v1 — that
would let a script escalate past its sandbox.

**The sync/async bridge.** Rhai's engine is synchronous; the permission
round-trip is async. The script runs under `tokio::task::spawn_blocking`; each
binding sends a `BindingCall` over an `mpsc` and blocks (`oneshot::blocking_recv`)
on the reply from an async resolver running on the executor task, which resolves
permission, drives any `Ask` round-trip, and executes the delegated tool. The
timeout is enforced *inside* the engine (progress callback), not by aborting the
un-abortable blocking task.

**Registration + gating.** Because the bindings are exactly the always-registered
quintet, `rhai` is precisely as privileged as those tools — so it is
**registered by default** in the shared `tool_specs` (no opt-in env). A profile
gates it like any tool: it rides the #116 tool mask (a read-only `explore` with
`tools: [read, glob, grep]` never sees it) and the #59 permission dispatch for
`rhai` itself. The executor intercepts `rhai` before the generic dispatch —
it needs this loop's per-session profile state to snapshot each binding's
mask + clamped permission — but its *own* Allow/Ask/Deny is resolved the same
way as any host tool.

## Consequences

- **Positive.** A sandboxed, capability-scoped scripting path that needs no OS
  sandbox and no external interpreter — works on every cross-compile target and
  in the lean build. Multi-step logic collapses to one tool call without opening
  the `bash` hole.
- **Positive.** No new core surface and no new protocol: bindings reuse the #58
  `ToolExec`/`ToolResult` round-trip, the #59 dispatch, the #116 mask, and the
  #77 clamp. `rhai`'s privilege is definitionally bounded by the quintet.
- **Negative / cost.** The sync engine costs one contained bridge (a
  `spawn_blocking` thread + an `mpsc`/`oneshot` hop per binding call). Fine at
  the expected per-script binding-call volume; revisit if it becomes hot.
- **Neutral.** The sandbox is enforced by Rhai's own limits + the disabled
  module resolver/`eval`; correctness rests on Rhai having no ambient IO (it
  does not) rather than on OS isolation.
- **Deferred.** Skill-provenance tagging (`rhai` run id on nested binding calls,
  tying into [ADR-0037](0037-load-skill-tool-deterministic-resolution.md)) lands
  with the broader provenance work; exec bindings, if ever, get their own ADR.

## Alternatives considered

- **Rune** — the serious runner-up: a Rust-like language, stack VM, and crucially
  **native async host functions** (the `Ask` round-trip would slot in without the
  sync bridge). Rejected for v1 because Rhai's sandbox limits are more mature and
  granular (`max_operations` + progress-callback interrupt + `max_call_levels` +
  string/array/map size caps vs. Rune's budget-based instruction limiting), it's
  the more stable/documented crate, and the simpler dynamic syntax yields fewer
  model-authored errors. The bridge is a small, contained cost. Revisit Rune if
  per-script binding-call volume makes the bridge cost real.
- **JS via deno_core/QuickJS, Python via PyO3 or a subprocess.** Rejected:
  heavier deps, worse embed/cross-compile story, and no capability model without
  the OS sandboxing we're explicitly avoiding.
- **WASM sandbox.** Strong isolation but a heavyweight toolchain and no story for
  the model authoring a module inline in a single tool call.
- **Make `rhai` a plain registry `Tool` (no executor interception).** Rejected:
  `Tool::run(input)` has no session context, so its bindings could not resolve
  session-scoped permission or run the `Ask` round-trip — they would either
  bypass per-binding permission (a `rhai: Allow` / `edit: Ask` profile could edit
  without asking, an escalation) or need core plumbing. Interception in the
  executor, where the profile state lives, is the security-correct seam.
- **Bind exec (`bash`/`call`) into the script.** Rejected for v1: it would let a
  script escape the capability sandbox to the same unsandboxed shell this tool
  exists to avoid; revisit only with its own ADR.
