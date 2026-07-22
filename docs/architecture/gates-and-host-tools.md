# entanglement Architecture — Hygiene gates & host tools

> Part of the [architecture overview](../architecture.md). The *why* behind each choice is in the [decision log](../adr/README.md).

## 7. Hygiene gates — [ADR-0006](../adr/0006-core-dependency-hygiene-gate.md) + [ADR-0053](../adr/0053-invert-core-provider-seam.md) (`tree`), [ADR-0025](../adr/0025-runtime-cargo-feature-gates.md) + [ADR-0053](../adr/0053-invert-core-provider-seam.md) (`check-lean`)

`entanglement-core` must stay free of UI/web-server deps. Enforced by
`make tree`, which runs `cargo tree -e normal -p entanglement-core` and **fails**
if a forbidden crate appears — ADR-0053's named set
(`clap`/`axum`/`tonic`/`crossterm`/`ratatui`) plus the web/websocket stacks a
name blocklist must also cover (`warp`/`actix`/`rocket`/`tungstenite`/`ureq`,
issue #207). Since [ADR-0053](../adr/0053-invert-core-provider-seam.md) inverted
the seam, core depends on `entanglement-provider`, so `reqwest`/`hyper`/`tower`
(the LLM transport) are now **legitimately** in core's transitive tree and are
not forbidden. It is part of `make verify`. Current core direct deps:
`entanglement-provider`, `tokio`, `serde`, `serde_json`, `async-trait`, `anyhow`,
`thiserror`, `tracing`, `futures`, `uuid`. `glob`/`regex` (which back the host
tools, §8) and `diffy` moved out with the host-tool implementations to
`entanglement-runtime` (✅ #57); the `Llm` trait + DTOs + the `reqwest` LLM
backends live in `entanglement-provider`, the leaf crate — see ADR-0053.

A second gate, **`make check-lean`** (ADR-0025, amended by ADR-0053), protects the
runtime's lean library surface: it runs `cargo tree -e normal -p
entanglement-runtime --no-default-features` and **fails** if `clap`/`ratatui`/
`crossterm`/`syntect`/`pulldown-cmark`/`diffy`/`tracing-subscriber` leak into the
no-default-features build (`reqwest`/`hyper` now ride in via core → provider and
are no longer flagged — ADR-0053), then runs lean `clippy --all-targets` (which
type-checks the lib + the integration tests with the bin auto-skipped via
`required-features` — the load-bearing check). It joins `tree` in `make verify`.

Both gates share one mechanism, [`scripts/dep-gate.sh`](../../scripts/dep-gate.sh)
(issue #207): the Makefile supplies the forbidden set (`CORE_FORBIDDEN` /
`LEAN_FORBIDDEN`) and the `cargo tree` selectors; the script unifies edge policy
(normal edges only — build/dev/proc-macro deps are excluded so they neither trip
nor mask the gate) and **hard-fails on a `cargo tree` error or empty output**.
That last point fixes the gates' original defect: they piped `cargo tree` through
`2>/dev/null` and never checked its exit status, so a *failed* `cargo tree`
grepped clean and passed **vacuously**. `make test-gates` runs
[`scripts/dep-gate.test.sh`](../../scripts/dep-gate.test.sh), a stubbed-`cargo`
self-test that pins the vacuous-pass fix. `cargo-deny` bans (ADR-0006's stated
future) were considered but **not** adopted: they evaluate the whole workspace
graph and can't scope a rule to one crate's subtree, so they cannot express
"forbidden in core but fine in runtime" (`clap`/`crossterm`/`ratatui` live
legitimately in the full runtime graph, and `axum` is reserved for the future
`serve` head) — the per-crate `cargo tree -p` subgraph is exactly what they lack.

**CI (issue #107).** Both gates now run in GitHub Actions
([`.github/workflows/`](../../.github/workflows/)), driven through the same `make`
targets. `ci.yml` runs `make verify` (`check-fmt` + `tree` + `check-lean` +
`lint` + `test`) on every PR and every push to `master` — the first time the
`tree`/`check-lean` hygiene gates run automatically rather than at developer
discretion. `release.yml` fires on a `v*` tag: it runs `make verify` and then a
coverage job, `make coverage` (`cargo llvm-cov --workspace`, fails under
`COV_MIN`% — baselined from the first measured run and ratcheted up, never
lowered), uploading the lcov + Cobertura reports as an artifact so a release is
blocked on green tests with a coverage report attached. Both cache cargo
artifacts (`Swatinem/rust-cache`) and inherit the committed `CARGO_BUILD_JOBS=4`
cap from `.cargo/config.toml`.

## 8. Host tools — [ADR-0008](../adr/0008-host-tools-workdir-and-bounded-output.md) (trio), [ADR-0009](../adr/0009-edit-and-bash-host-tools.md) (`edit`/`bash`), [ADR-0010](../adr/0010-single-head-crate-and-bash-opt-in.md) (`bash` opt-in), [ADR-0045](../adr/0045-call-host-tool-argv-exec-tailed-output.md) (`call`), [ADR-0092](../adr/0092-call-file-based-stdin-stdout.md) (`call` file-based stdin/stdout), [ADR-0093](../adr/0093-call-registration-independent-of-bash-opt-in.md) (`call` always-registered + `workdir`), [ADR-0104](../adr/0104-bubblewrap-sandbox-for-bash-call.md) (optional bubblewrap confinement)

Concrete filesystem + shell tools, dispatched under the active permission
profile ([ADR-0003](../adr/0003-agent-and-permission-profiles.md)). The
`Tool` **trait** and `ToolRegistry` live in **`entanglement-runtime`**
(`entanglement-runtime::tools`, ✅ #206, [ADR-0059](../adr/0059-tool-trait-and-registry-live-in-the-runtime.md)) —
core holds no executable tools, only advertises schemas (§tool round-trip);
the implementations live in **`entanglement-runtime::host`**
(✅ #57) and are assembled by `host_tools(root: PathBuf) -> ToolRegistry`.
Execution *and* permission dispatch now run in the runtime (✅ #58, #59):
`entanglement-runtime::tool_runner` subscribes to the engine, resolves each
`ToolExec`'s `Allow|Ask|Deny` against the session's active profile (§3), runs the
cleared tool against the registry, and replies with `InMsg::ToolResult`.
`ToolRegistry::execute(&self, call: &ToolCall, session: &SessionId)` threads the
caller's `SessionId` through to `Tool::run_for_session` (#360,
[ADR-0088](../adr/0088-session-aware-tool-execution.md)) — a default-delegating
method (falls back to `run_content`) so every in-tree tool is unaffected; a
multi-tenant embedder overrides it to dispatch per-tenant MCP endpoints or scope
a DB-backed tool's writes to the caller, since a shared `ToolRegistry` otherwise
can't tell tenants apart at execution time even though `spawn_tool_executor_with_policy`
(#311) already resolves *permission* per session. `Ask`
emits the `ToolRequest` prompt and waits for the head's decision on
`Holly::subscribe_inbound()` (the engine's inbound `InMsg` fan-out). The executor
is **idempotent by `request_id`** (✅ #274,
[ADR-0071](../adr/0071-parked-turn-reoffer-timer.md)): it keeps a per-session set
of **in-flight** request ids — dispatched but not yet resolved — and skips a
`ToolExec` whose id is still in flight, so core's re-offer timer (which re-emits a
parked batch after `reoffer_interval` of silence to recover an offer dropped
under `broadcast` lag, see [engine.md](engine.md)) never double-runs a call it is
already executing. An id is dropped again on the resolving `ToolOutput` (and on
`SessionEnded`), so a later round that reuses the id still dispatches. Core only
advertises the tool *schemas* (`EngineConfig.tool_specs`) — it holds no executable
tools and makes no policy decision.

**Unknown-tool short-circuit (#437).** A hallucinated tool name is checked
against the freshly-snapshotted registry at the **top** of `dispatch`, before
permission resolution or the `pre_tool_use` hook (§9) run — a name the registry
doesn't hold (and isn't a state tool — `update_plan`/`update_tasks`, exempt
since they're never registered, [ADR-0049](../adr/0049-plan-task-tools-as-runtime-state-tools.md))
can never execute, so it would be
pointless to prompt the user for `Ask` approval, run a hook that could veto it,
or let an `Always`-scoped approval record a grant for it. `ToolRegistry::
unknown_tool_message` backs both this short-circuit and `execute`'s own
registry-miss fallback: it enriches `unknown tool: `name`` with a closest-match
hint (smallest Levenshtein distance over the registered names, capped so a
wildly different name surfaces no hint) plus the full name list when the
registry is short, so a weak model can self-correct in one round instead of
guessing again:

| tool | input | output |
| --- | --- | --- |
| `read` | `{path, offset?, limit?}` | text file → contents as `{lineno}: {line}`, 1-based, line-ranged; an **image file** (`.png`/`.jpg`/`.jpeg`/`.gif`/`.webp`, by extension) → a base64 **image content block** the provider renders natively (Anthropic `image` / OpenAI `image_url`), routed through the `ToolResult` `content` path (`offset`/`limit` ignored) — #221 |
| `glob` | `{pattern, exclude?}` | matching paths (relative to root), one per line; `.git` is always excluded (path-component check, can't be disabled), `exclude` adds caller glob patterns on top ([ADR-0099](../adr/0099-glob-grep-exclude-and-default-git-exclusion.md)) |
| `grep` | `{pattern, path?, exclude?}` | matches as `path:lineno:line` over files matched by `path` (default `**/*`) minus `exclude` and the always-on `.git` exclusion ([ADR-0099](../adr/0099-glob-grep-exclude-and-default-git-exclusion.md)); a file over the 1 MiB **scan** cap (independent of the 32 KiB output cap, [ADR-0091](../adr/0091-grep-file-scan-size-cap-decoupled-from-output-cap.md)) or sniffed as binary (a NUL byte in its content) is skipped and named in a labeled notice appended to the result — regardless of match count |
| `edit` | `{path, oldString, newString, replaceAll?}` | exact-string replace; empty `oldString` creates (refused if exists → hints `write`); non-unique match errors unless `replaceAll` |
| `write` | `{path, content}` | whole-file create/overwrite; missing parent dirs created; `created <path> (N lines)` / `overwrote <path> (N lines, was M)` — confirmation only, never echoes content (ADR-0031) |
| `apply_patch` | `{path, patch}` | apply a unified diff (one or more `@@ -oldStart,oldLen +newStart,newLen @@` hunks) against the current file; each hunk's context/deleted lines are matched **exactly** at the position its header declares (offset by the net line-count delta of hunks already applied in the same patch) — no fuzzy alternate-position search, a mismatch hard-errors before any write and leaves the file untouched; emits `FileChangeKind::ApplyDiff` (#455, the first producer of that previously-reserved variant). Parsing/applying is a small hand-rolled module (`host::unified_diff`), not the `diffy` crate — `diffy` is `tui`-feature-gated and named in `LEAN_FORBIDDEN` above, and `apply_patch` is unconditional lean-library code alongside `edit`/`write` |
| `bash` ⚠ | `{command, timeout?, workdir?, run_in_background?}` | `sh -c` rooted at root (or at `workdir`, a subdir validated under root by the same symlink-safe containment as the fs tools, #170); `[exit N]` + stdout + `[stderr]`; default 120 s timeout, capped at 600; spawned in its **own process group** (`process_group(0)`) so an expiry SIGKILLs the whole tree — grandchildren (a launched server/pipeline) can't orphan (#168); a `Stop`-driven task abort drops the wait future, whose group-kill guard SIGKILLs the same group so cancellation matches the timeout's containment rather than orphaning under bare `kill_on_drop` (#167). Output is drained incrementally, so a timeout returns the **partial output buffered before the kill** under a `[killed: timed out after Ns]` header instead of discarding it (#169). Oversized output is capped **head + tail** (¼ head / ¾ tail, `truncate_head_tail`) so the trailing error survives — head-only truncation dropped exactly what a failing build needs (#170). `run_in_background: true` spawns the command **detached** and returns a job id instead of blocking — poll it with `bash_output` (#170). Stdin is always closed (`Stdio::null()`), never inherited from the engine — the same leaked-by-default class ADR-0092 closed for `call`, applying uniformly to both the foreground and `run_in_background` paths since both share the one command builder (#389); use shell-native `< file` redirection if a command needs input |
| `bash_output` ⚠ | `{job_id, kill?}` | poll a background `bash` job (started with `run_in_background`) for the output produced **since the last poll**, plus status (`running` / `exited N` / `exited (killed)`). Buffers are drained per poll (`mem::take`) so memory is reclaimed and each read is incremental; between polls each stream is capped at 256 KiB dropping the **oldest** bytes (the live tip is kept) with a `[N bytes … dropped]` notice. `kill: true` SIGKILLs the job's whole process group before reading. Registered as a pair with `bash` under the same opt-in gate (#170) |
| `call` ⚠ | `{command, args?, tail?, timeout?, input_file?, output_file?, workdir?}` | **argv, no shell** — `command`+`args` exec verbatim (no `sh -c`, so no pipe/glob/`$VAR`/metachar interpretation); output tailed to the last `tail` lines per stream (default 30, `tail=0` = full, byte-cap still applies), with a `(… N earlier lines omitted, tail=30 — rerun with tail=0 …)` notice; same envelope as `bash` (`[exit N]` + stdout + `[stderr]`, 120 s/600 s, own-process-group kill on timeout #168, partial output preserved on timeout #169) — ADR-0045. `input_file`/`output_file` (ADR-0092, #381), both root-contained via `resolve_under_root` and validated **before spawn** (relative to the **root**, not `workdir`): `input_file` is read and piped to the child's stdin (fed concurrently with the stdout/stderr drain to avoid a full-pipe deadlock); its **absence closes stdin** (`Stdio::null()`) rather than inheriting the engine's own (a leaked-by-default behavior until now). The full **untruncated raw** stdout is always persisted — to `output_file` if given (missing parent dirs created), else to an **auto-named default artifact** in a runtime-owned per-project **scratch dir outside the repo** (`session_store::scratch_dir` → `<data_dir>/entanglement/sessions/<cwd>/tmp/call-output/call-{pid}-{seq}.stdout`, wired via `CallTool::with_scratch_base`) so a routine `call` neither pollutes the workdir nor re-triggers the definitions watcher — with a `<output_file>.stderr` sibling always alongside; the artifact's **absolute** path is named in the result header (`[output: …] [stderr: …]`). (Standalone/test constructors with no scratch base fall back to the legacy in-repo `.entanglement/tmp/call-output/`.) An explicit `output_file` write failure is a hard error; a default-artifact write failure is best-effort (logged + a degraded notice, never fails an otherwise-successful call). `workdir` (#386) sets the child's **cwd** to a subdirectory validated under root via the shared `resolve_workdir` (same containment as `bash`'s); a non-directory or escaping `workdir` errors before spawn. **Registered unconditionally** — independent of `ENTANGLEMENT_ENABLE_BASH` ([ADR-0093](../adr/0093-call-registration-independent-of-bash-opt-in.md)) |
| `rhai` | `{script, timeout?}` | run a Rhai script ([rhai.rs](https://rhai.rs)) in a **capability-sandboxed** engine — no fs/network/process/env access beyond what is explicitly bound; the host bindings are `read`/`glob`/`grep`/`edit`/`write` plus `read_raw` (exact file content, no line-number prefix — `read`'s `"{lineno}: {line}"` format isn't parseable as JSON/YAML; graded and masked as an alias of `read`, not a distinct permission surface, since it is never advertised as a standalone tool), plus permission-gated process-exec — `exec(command)`/`exec(command, args)`/`exec(command, args, workdir)` (marshalled to the `call` tool) and `bash(command)`/`bash(command, workdir)` (marshalled to `bash`, bound only when the host `bash` tool is registered — otherwise an unknown-function script error, not a graded-then-refused binding) — each routed through that tool's permission check (`exec`/`bash` graded under the Call capability like their host-tool counterparts, #419/[ADR-0114](../adr/0114-capability-level-permission-keys.md)). The script-facing name is `exec`, not `call`: `call` is a hard-reserved Rhai keyword for function-pointer invocation the interpreter special-cases ahead of any same-named registered function; the dispatched tool name/permission grade stay the literal `call`. `exec`/`bash` additionally derive their `timeout` from the script's own remaining wall-clock budget rather than the tool's much longer default, since rhai's timeout interrupt can't reach a binding call parked on the sync/async bridge; their `Ask` approval is cached **per resolved command line + `workdir`** (#480), not per bare tool name, so approving one command/workdir pair cannot silently pre-clear a different one in the same run (every other binding keeps the coarser once-per-function cache). An explicit `workdir` (#480, [ADR-0130](../adr/0130-rhai-exec-bindings-marshal-workdir.md), amending [ADR-0115](../adr/0115-rhai-exec-bindings-call-bash.md)/[ADR-0116](../adr/0116-workdir-scoped-permission-rules-for-bash-call.md)) marshals into the delegated tool's own `workdir` field — a `tool{pattern}` workdir-scoped permission rule (#425) now resolves for a binding call exactly as it would a direct `bash`/`call` call (`BindingPolicy::decide` grades through `resolve_scoped`, not the workdir-blind `resolve`), and the same field feeds the escape-root gate below with no separate wiring. A `read`/`edit`/`write`/`exec`/`bash` binding targeting a path or `workdir` outside the project root routes through the same escape-root gate a direct call uses (#446, [ADR-0119](../adr/0119-rhai-bindings-route-through-the-escape-root-gate.md)): it forces an approval carrying the ADR-0109 "outside the project root" warning, bypassing the per-run `Ask` cache, and records the grant into the shared `ExtraRootStore` on approval; a durably-granted (`Session`/`Always`) path resolves silently, same as a direct call. A binding excluded by the session's active skill's `allowed_tools` (#400, [ADR-0106](../adr/0106-skill-scoped-allowed-tools-enforcement.md)) refuses too — checked after the agent mask, same as generic dispatch — since `BindingPolicy` folds in a one-time snapshot of the session's skill mask alongside the agent mask (#477, [ADR-0129](../adr/0129-thread-the-skill-mask-into-rhai-binding-resolution.md); sound as a snapshot because `load_skill` is not itself a binding, so a running script cannot change which skill is active). Also bound, pure (no IO, no permission check, since they only transform a value already in the script): `parse_json`/`to_json`/`parse_yaml`/`to_yaml`, built on Rhai's own `serde` bridge (`null` → `()`, an out-of-`i64`-range integer silently widens to an approximate float rather than erroring — same as JS's `JSON.parse`); last-expression value serialized + captured `print(...)`; bounded by op/string/array/map caps + wall-clock (default 5 s, max 30) — [ADR-0046](../adr/0046-rhai-sandboxed-script-tool.md) (amended by [ADR-0115](../adr/0115-rhai-exec-bindings-call-bash.md), [ADR-0129](../adr/0129-thread-the-skill-mask-into-rhai-binding-resolution.md), and [ADR-0130](../adr/0130-rhai-exec-bindings-marshal-workdir.md)), [ADR-0098](../adr/0098-rhai-json-yaml-loader-and-read-raw.md) |

- **Working directory:** each tool holds a `root` (the cwd, **canonicalized once
  at startup**); model-supplied paths resolve against it and are rejected on `..`
  escape **and on symlink escape** — `resolve_under_root` canonicalizes the
  resolved target's deepest existing ancestor and requires it under the canonical
  root, so a `root/link -> /etc` symlink can't be followed out of tree by
  `read`/`edit`/`write`/`apply_patch` (the create path still works: only the existing ancestor
  is canonicalized), and `glob`/`grep` (`list_files`) drop any match whose
  canonical path escapes — ADR-0008 upgraded by [ADR-0054](../adr/0054-canonicalizing-symlink-safe-root-containment.md)
  (#163). Not TOCTOU-tight (an OS sandbox via `openat2(RESOLVE_BENEATH)` is
  deferred).
- **Approval-gated escape (ADR-0109):** containment is no longer absolute — a
  `read`/`edit`/`write`/`apply_patch` path or a `bash`/`call` `workdir` that resolves *outside*
  root can be reached after the user explicitly approves it. The executor detects
  the out-of-root target (`permission::escape_root_target` + `host::escaping_path`),
  forces an approval prompt even when the profile would `Allow` (a `Deny` floor
  still wins), and records the grant in a shared `ExtraRootStore`
  (`extra_roots.rs`; managed `extra-roots.yml`, override
  `ENTANGLEMENT_EXTRA_ROOTS_FILE`) keyed by `(tool, resolved-absolute-path)` —
  **per tool** (a `read` grant never unlocks `write`) at `Once`/`Session`/`Always`
  scope. `Once` is additionally bound to the approving call's `request_id`
  (#449, [ADR-0120](../adr/0120-once-scoped-escape-root-grant-bound-to-request-id.md)):
  per-call executor tasks are detached and run concurrently, so without that
  binding a single-use token approved for one call could be spent by a
  different in-flight call to the same `(tool, path)`; `Session`/`Always`
  still match `(tool, path)` alone, since a durable grant is meant to cover
  every later call. `Tool::run_for_session` carries the `request_id` (the
  `ToolCall.id` `ToolRegistry::execute` already had) into the six
  escape-root-capable host tools for this. The host tools consult the store via
  `resolve_under_root_or_grant` to relax containment for the approved path
  (checked against the symlink-resolved target). `glob`/`grep` stay strictly
  root-contained; no store wired (`None`) is byte-identical to strict
  containment. `rhai`'s file/exec bindings route through the identical gate
  (#446, [ADR-0119](../adr/0119-rhai-bindings-route-through-the-escape-root-gate.md)):
  `service_binding` forces the same approval + warning for a first-time
  out-of-root binding call and records the grant into the same
  `ExtraRootStore` — keyed by the binding's own `bind_rid`, threaded into the
  delegated call so a script-obtained `Once` grant is redeemed by that exact
  binding invocation too — so a script is no more (and no less) able to escape
  root than an equivalent direct tool call. See
  [ADR-0109](../adr/0109-escape-root-access-via-approval.md). `bash`/`call` set only the **cwd** (root, or `workdir` if given,
  through the shared `resolve_workdir` helper both tools call) and run with the
  engine's full privileges **by default** — unsandboxed unless opted in
  (ADR-0009/ADR-0045); permission profiles gate whether they run at all. `call`
  is the injection-free sibling: a fixed argv can't be shell-injected, so a
  profile may `Allow` `call` while keeping `bash` at `Ask`/`Deny` — and, since
  [ADR-0093](../adr/0093-call-registration-independent-of-bash-opt-in.md),
  `call` is registered regardless of whether `bash` is even opted in.
- **OS sandbox, opt-in (#399, [ADR-0104](../adr/0104-bubblewrap-sandbox-for-bash-call.md)):**
  `ENTANGLEMENT_SANDBOX=bwrap` confines every `bash`/`call` spawn under
  bubblewrap for the process's lifetime — `--ro-bind / /` plus the project
  root re-bound read-write at the same path (so `resolve_under_root`'s
  containment above keeps working unmodified inside the sandbox), a fresh
  `/tmp`/`/dev`/`/proc`, and its own pid/ipc/uts/cgroup namespaces.
  `--unshare-net` cuts network by default; `ENTANGLEMENT_SANDBOX_NETWORK=1`
  shares the host network namespace back in. **Fail-closed by omission**: there
  is no fallback to unsandboxed execution when `bwrap` can't be entered (missing
  binary, unprivileged user namespaces disabled) — the spawn simply errors, like
  any missing binary (ADR-0016). Global for the process (not yet per-profile —
  see the ADR's follow-up); `BashTool`/`CallTool::with_sandbox` wires it,
  `entanglement_runtime::host::sandbox::SandboxPolicy::from_env()` reads the two
  env vars. The existing process-group timeout/cancel kill (#167/#168/#169,
  below) needs no change — killing the outer `bwrap` process cascades through
  its PID-namespace death to the whole sandboxed tree.
- **Secret scrubbing (#164):** both exec tools `env_remove` the catalog's
  provider API-key env vars (`Catalog::key_envs()` — `ZAI_API_KEY`,
  `OPENAI_API_KEY`, `ANTHROPIC_API_KEY`, …) from the child before spawn, so a
  model-authored `env`/`printenv` can't read the engine's credentials. `call`'s
  no-shell design doesn't help — a plain `env` still inherits them — so the scrub
  covers both. The head wires the set via
  `BashTool::new(root).with_secret_env(catalog.key_envs())` (same for `CallTool`);
  a broader env-allowlist policy remains a possible follow-up to the sandbox
  above ([ADR-0104](../adr/0104-bubblewrap-sandbox-for-bash-call.md)). `rhai`'s
  `exec`/`bash` bindings (#419) delegate to these same scrubbed tool instances
  via `tools.execute()` (the bridge holds a registry snapshot, not its own
  spawn path), so the scrub applies to script-issued exec identically — no
  separate wiring needed.
- **Bounded output:** 32 KiB byte cap with a truncation notice; `read` defaults
  to 2000 lines; `glob`/`grep` cap at 1000 results. Prevents a huge file/tree
  from blowing the context window. `bash`/`bash_output` cap **head + tail**
  (`truncate_head_tail`) rather than head-only — build/test output puts the
  load-bearing error at the end (#170). `grep`'s per-file **scan** cap (how
  much of a candidate file it reads and searches) is a separate, grep-local 1
  MiB bound (`MAX_SCAN_BYTES`), not the 32 KiB output cap — conflating the two
  meant any file over 32 KiB was silently skipped regardless of the
  match-output size ([ADR-0091](../adr/0091-grep-file-scan-size-cap-decoupled-from-output-cap.md), #380).
- **Empty-result contract (ADR-0016):** a host tool may not return a silent
  zero-output when multiple distinguishable underlying states produce it.
  `list_files` returns `FileList { files, matched_dirs, skipped_errors }`;
  per-entry walk errors are `warn!`-logged and counted, not swallowed. When
  `glob`'s result would be empty but the pattern matched something (the common
  bare-`**` trap, which matches only directories), it returns a hint like
  *"`**` matched 7 directories but no files — try `**/*`"* so the model can
  self-correct mechanically. `grep` consumes the same `FileList` but stays
  silent on zero matches (a clean no-match is a single well-defined state);
  it is **not** silent, however, about files it excluded from the scan — a
  file over `MAX_SCAN_BYTES` or sniffed as binary (NUL byte in its content) is
  tracked by skip reason (`TooLarge`/`Binary`) and, whenever that list is
  non-empty, surfaced as a labeled notice (capped preview, `... and N more`
  past 20 entries per reason) appended to the result regardless of match
  count — otherwise a match that exists only in an excluded file would look
  identical to a genuine no-match ([ADR-0091](../adr/0091-grep-file-scan-size-cap-decoupled-from-output-cap.md)).
- **Schema advertisement:** `Tool::schema()` feeds `ToolRegistry::specs()`, so
  the model sees a real `input_schema` per host tool (not an empty object).
- **Wiring (ADR-0010, amended by [ADR-0093](../adr/0093-call-registration-independent-of-bash-opt-in.md)):**
  `host_tools(root)` registers the **root-contained sextet**
  (`read`/`glob`/`grep`/`edit`/`write`/`apply_patch`; `write` added in
  ADR-0031, `apply_patch` in #455). The
  `skutter` binary registers `CallTool` **unconditionally**, alongside the
  sextet — no shell means no injection surface, so its registration no
  longer rides `bash`'s opt-in gate (#386). `BashTool` **and**
  `BashOutputTool` (the background-job poller, #170) still register only
  when `ENTANGLEMENT_ENABLE_BASH=1`, because `bash` runs arbitrary shell code
  (ADR-0009). `bash` and `bash_output` share one `JobRegistry` so background
  jobs are pollable across the pair. `EngineConfig::default()` ships an empty
  registry (embedders opt in via `host_tools`).

`edit`/`write`/`apply_patch`/`bash`/`bash_output`/`call` are advertised only to the inherit-all
`build` profile (`tools: None`), which auto-allows them (default `Allow`). The `plan`
and `explore` profiles set an explicit `tools` allowlist that omits them
(#116/#140, [ADR-0038](../adr/0038-physical-per-agent-tool-restriction.md)), so
the tools are **masked out** of those profiles entirely — never advertised, so
no `Allow`/`Ask`/`Deny` default is reached for them there. Registration is
orthogonal to both mask and profile: it controls whether the tool is advertised
at all (unconditional for `call`, opt-in for `bash`/`bash_output`), the mask
controls *existence* per profile, and the profile controls *dispatch*
(Allow/Ask/Deny when the model calls a tool that survives the mask) — so `call`
being always-registered does not change what a non-`build` profile can do with
it.

Five **runtime-owned orchestration tools** are *not* in the registry — the
`tool_runner` intercepts them on `ToolExec` before permission resolution (they
touch no host resource) and advertises their schemas separately: the `agent_*`
family (§5, ADR-0033) —
`agent_spawn { agent, prompt }` (renamed from `spawn_agent`, ADR-0022), its
non-blocking join `agent_poll { agent_id, timeout_secs }` (ADR-0026; `timeout_secs: 0` is the indefinite-wait sentinel, ADR-0123), and the
blocking `agent { agent, prompt }` (spawn-and-wait in one call) —
`ask_user { questions: [{question, options, multi_select}] }` (§5, ADR-0027;
v2 #488, ADR-0127 — batched questions, `multi_select` per question, an
unconditional free-text "Other" answer), and
`propose_plan { plan }`, the plan agent's finalize step, force-parked on the
user-approval round-trip since acceptance *is* its semantics (#141,
[ADR-0042](../adr/0042-plan-acceptance-via-propose-plan-approval-roundtrip.md);
advertised only to a profile that explicitly allowlists it, #231). The `rhai`
script tool (table above) is intercepted the same way but is **not** a bypass:
it resolves its own `Allow`/`Ask`/`Deny` live inside the sandboxed script task
(#122, [ADR-0046](../adr/0046-rhai-sandboxed-script-tool.md)).

## 9. Lifecycle hooks — [ADR-0066](../adr/0066-lifecycle-hooks-as-runtime-interceptors.md) (#199)

User-configured external commands run around tool execution and on prompt
ingress, for policy, telemetry, and formatting side-effects. Hooks are a
**runtime interceptor** (`entanglement-runtime::hooks`), not a core concept:
core neither knows nor cares that a command runs before a tool. They hang off the
two seams the runtime already owns — the `tool_runner` dispatch of a `ToolExec`
and the inbound `InMsg::Prompt` fan-out — so no new protocol surface is added.

| point | fires | can block? | payload |
| --- | --- | --- | --- |
| `pre_tool_use` | top of the generic `dispatch` (`Intercept::Permission`), **before** the `Allow`/`Ask`/`Deny` decision | **yes** — a non-zero exit vetoes: the tool neither prompts nor runs, and the hook's output becomes the `ToolResult` | `{event, session, tool, input}` |
| `post_tool_use` | in `run_and_reply` after the tool result, before it folds back | no — observational (exit code logged, never fed to the model); it cannot rewrite the result | `{event, session, tool, input, output}` |
| `user_prompt_submit` | when an `InMsg::Prompt` reaches the engine (the executor's inbound `Stop` watcher) | no — observational | `{event, session, prompt}` |

- **Config:** the `hooks:` section of the layered user config (§ADR-0047/#172).
  `Config.hooks: Hooks` is three `Vec<HookSpec>` deep-merged and
  `deny_unknown_fields`-validated by the same loader as `permissions`. A
  `HookSpec` is `{command, tools?, timeout_secs?}`; `tools` is an optional
  name-filter for the tool hooks (empty ⇒ every tool), ignored by
  `user_prompt_submit`. Empty section ⇒ no hooks (the norm).
- **Execution:** each hook is an `sh -c <command>` child fed the JSON payload on
  stdin and given `ENTANGLEMENT_HOOK_EVENT` / `ENTANGLEMENT_SESSION_ID` /
  `ENTANGLEMENT_TOOL_NAME` (tool hooks) env vars. It runs under `timeout_secs`
  (default 30) in its **own process group**, reusing the exec tools' containment
  (`host::exec`, §8/#168) so a hook that spawns children can't orphan them. A
  timeout or a spawn failure counts as a **failure**, so a `pre_tool_use` hook
  that can't launch **fails closed** (vetoes the tool) rather than letting it
  through.
- **Scope:** only the generic host-tool dispatch route. The orchestration tools
  (`agent`/`ask_user`/`propose_plan`, which touch no host resource) and the
  self-permissioning `rhai` tool bypass hooks — matching the issue's "around
  `tool_runner::dispatch`" scope.
- **Wiring:** `spawn_tool_executor_with_hooks(holly, tools, profiles, base, hooks)`
  is the seam `main.rs` uses; the historical `spawn_tool_executor` is a no-hook
  wrapper (existing callers/tests unchanged). The inbound subscription is hoisted
  synchronous before the executor task spawns so a first `Prompt` can't race the
  `user_prompt_submit` watcher.

### Pluggable policy seams — `PermissionResolver` + `GrantStore` — [ADR-0079](../adr/0079-pluggable-permission-resolver-and-grant-store.md) (#311)

The executor hard-codes *no* policy source. `spawn_tool_executor_with_policy(…,
resolver: Arc<dyn PermissionResolver>, grants: Arc<dyn GrantStore>, …)` (module
`entanglement-runtime::policy`) drives two trait objects, so a multi-tenant
embedder that stores rules per user in its own DB swaps both without forking the
~350-line executor — keeping the shared interception ladder, spawn/mask gating,
hooks, rhai, and plan/tasks tools.

- **`PermissionResolver::resolve(session, tool, input) → Permission`** decides one
  session's `Allow|Ask|Deny` grade (async — a real embedder hits a DB, and the
  ladder already runs in a detached task). It runs **where the profile/base
  resolution ran before**, but the sub-agent ancestor clamp (ADR-0024) and
  spawn/mask gating stay in the ladder **on top of** it: the executor snapshots
  the call's ancestor chain (`permission::ancestor_chain`) in the loop and takes
  the least-privileged resolver grade across it (`resolve_effective`), so a tenant
  rule can never widen a child beyond its parent. `apply_grant` then upgrades a
  resolved `Ask` to `Allow` from a `GrantStore` grant.
- **`GrantStore`** persists + reads "always allow" grants (§ agents-and-permissions
  #174). `record(session, tool, arg, scope)` is async so an `ApprovalScope::Always`
  can hit a DB; `is_granted` is a sync fast check. A multi-tenant store writes its
  "always" rule to the DB and resolves later reads through its own resolver, so its
  `is_granted` can return `false`.
- **Defaults (byte-identical CLI):** `ProfileResolver` reads the same
  `Arc<Mutex<active-profile map>>` the executor folds lifecycle events into and
  returns own-profile-clamped-by-base — since `clamp_to_base` is monotonic,
  min-of-clamped over the chain equals the pre-seam `effective_permission` +
  `clamp_to_base`. `DefaultGrantStore` wraps the managed file store
  (`grants::FileGrantStore`). `rhai` keeps the profile/base path (its inner
  bindings are a separate sync mechanism) and is not routed through the resolver.

### Dynamic `ToolRegistry` — `SharedRegistry` — [ADR-0096](../adr/0096-dynamic-toolregistry-sharedregistry.md) (#372)

`spawn_tool_executor_with_policy`'s `tools` parameter is a `SharedRegistry`
(`Arc<std::sync::RwLock<ToolRegistry>>`, `ToolRegistry::shared()`), not an owned
`ToolRegistry` — the seam a future live MCP add/remove (#4) needs to mutate the
dispatch table without restarting the engine. The two convenience wrappers,
`spawn_tool_executor`/`spawn_tool_executor_with_hooks`, keep their historical
owned-`ToolRegistry` signature and `.shared()`-wrap internally (mirroring the
existing `profiles: Arc<RwLock<ProfileRegistry>>` pattern, §"Pluggable policy
seams" above), so existing single-owner callers and tests are unaffected.
`ToolRegistry` itself gains `unregister`/`unregister_prefix`/`contains`/
`names` alongside `register`.

Each `ToolExec` dispatch takes a brief **synchronous** read lock and clones an
owned snapshot before spawning its detached task — `std::sync::RwLock`, not
`tokio::sync`, because `EngineConfig.tool_spec_resolver`
([ADR-0076](../adr/0076-per-session-dynamic-tool-specs.md)) is a plain sync
`Fn` consulted on the turn's hot path and must never block on I/O; a
`tokio::sync::RwLock` would force that closure into `blocking_read` or break
0076's no-async contract. `main.rs` wires the resolver to read through the
same `SharedRegistry` handle it hands the executor, reproducing
`cfg.tool_specs`' exact original composition (registry tools plus the three
runtime-intercepted pseudo-tool specs `update_tasks`/`ask_user`/`rhai`, which
aren't `ToolRegistry` entries) — so today this is purely internal plumbing,
byte-identical advertised schemas, with every *future* registry mutation
landing on the *next* turn for free and no `EngineConfig` reload.

## 10. MCP client — external tool servers — [ADR-0067](../adr/0067-mcp-client-as-runtime-tool-provider.md) (#198)

Attach any external [MCP](https://modelcontextprotocol.io) tool server as a
**runtime-side tool provider**, with **no core change**. Since the `Tool` trait +
`ToolRegistry` live in the runtime (§ADR-0059), an external tool is the same shape
as a host tool: a `dyn Tool` with a name, description, and `inputSchema`. The MCP
client (`entanglement-runtime::mcp`) spawns each server, discovers its tools, and
registers them into the same registry — so they ride `EngineConfig.tool_specs`
(schemas) and the `ToolExec`/`ToolResult` round-trip (execution) unchanged, under
the same permission profiles as `read`/`bash`.

- **Transport (`mcp::client::McpClient`):** an enum over two concrete transports,
  chosen per server by the `command` XOR `url` config (§ADR-0080/#312). `McpTool`
  holds an `Arc<McpClient>` and only calls `list_tools`/`call_tool`, so it adapts
  whichever backs a server. Both share the handshake (`initialize` +
  `notifications/initialized`) then `tools/list` (discovery) / `tools/call`
  (execution), a **60 s** per-request timeout so a hung server can't park a turn,
  and the JSON-RPC result/error split (`client::jsonrpc_payload`).
  - **stdio (`mcp::stdio::StdioClient`, #198):** one JSON-RPC 2.0 session over the
    spawned subprocess's stdio, newline-delimited frames. A background reader task
    demultiplexes responses to callers by JSON-RPC `id`; notifications are dropped,
    and on EOF the reader **drains all pending requests with an error** so a crashed
    server can't hang a caller. The subprocess is held for the client's lifetime
    (`kill_on_drop`); keeping the registered tools alive keeps the server alive.
    The child env is the inherited environment **minus the provider API-key
    vars** (`catalog.key_envs()`, the same #164 scrub `bash`/`call` apply —
    #472, [ADR-0124](../adr/0124-wire-refused-mcp-mutation-and-stdio-key-scrub.md));
    an explicit per-server `env:` entry naming a key still wins, since writing
    it into the server's own config block is deliberate consent.
    Lives in the **lean library** (tokio process + `serde_json` only).
  - **streamable HTTP (`mcp::http::HttpClient`, #312, behind the `mcp-http`
    feature):** a remote server over `POST <url>` — the streamable-HTTP transport.
    Each request is a discrete `POST` with `Accept: application/json,
    text/event-stream`; the server answers with a lone JSON body **or** an SSE
    stream (drained until the event whose JSON-RPC `id` matches). Static per-server
    `headers` (e.g. `Authorization: Bearer …`) authenticate every request, with
    `${VAR}` expanded from the environment so a token stays out of the config file;
    the flip side of that expansion is a documented, accepted leak surface
    (§ADR-0080/[ADR-0128](../adr/0128-mcp-http-var-header-expansion-leak-surface.md)):
    `expand_env` resolves `${VAR}` against the engine's whole process
    environment with no allowlist, so a header naming a provider secret sends
    that secret's live value to the configured server — consent, not a bug,
    since the config file is trusted and enabling a server is consent
    ([ADR-0047](../adr/0047-local-trust-boundary.md)), same as the stdio
    transport's `env:` block. Any future logging of resolved request headers
    must redact expanded values; none exists today. An `Mcp-Session-Id`
    handed back on `initialize` is echoed on every later request
    (and the negotiated `MCP-Protocol-Version`). `reqwest` rides the `mcp-http`
    feature so the lean build carries no HTTP transport (§ADR-0025). `HttpClient` is
    **public** so an embedder can build a per-tenant client with a per-user token and
    register its tools without the YAML path.
- **Proxy (`mcp::tool::McpTool`):** adapts one remote tool. `schema()` returns the
  server's `inputSchema` verbatim; `run()` JSON-decodes the model's input to the
  `arguments` object, calls `tools/call`, and flattens the result's text content
  (v1 is text-only — a non-text block is noted, an `isError` result prefixed).
  Advertised name **`mcp__<server>__<tool>`**, sanitized to the providers'
  `^[A-Za-z0-9_-]+$` rule, so it can't collide with a host tool or another server.
- **Config:** the `mcp:` section of the layered user config (§ADR-0047/#172), a map
  of server name → `McpServerConfig`. A block is one transport XOR the other —
  `{command, args, env}` (stdio) **or** `{url, headers}` (HTTP), plus a shared
  `disabled` — resolved by `McpServerConfig::transport()`, which rejects both-set or
  neither-set. `deny_unknown_fields`-validated by the same loader as
  `permissions`/`hooks`. Empty ⇒ no servers (the norm). `skutter inspect config`
  lists the configured servers and their resolved transport. An optional
  `capabilities: {tool: read|write|call}` map (raw tool name, #426,
  [ADR-0117](../adr/0117-mcp-tool-capability-fan-out.md)) hand-annotates a
  server's tools for the permission capability fan-out (§agents-and-permissions)
  — an MCP tool carries no such hint of its own, so without this a bare
  `read: allow` would never reach it; `mcp::capability_index` folds it into an
  `McpCapabilityIndex` keyed by capability name, resolved from config alone
  (no live connection needed) and consumed by `agents::expand_capabilities`.
- **Wiring:** `build_config` is `async` and calls `mcp::connect(&config.mcp, &mut
  tools)` after the host tools are registered but before `tool_specs` is derived, so
  MCP tools flow into both the advertised schemas and the executor's registry with
  the existing code. Connection is **best-effort per server**: a spawn / handshake /
  `tools/list` failure is logged and skipped — a down server degrades to "that tool
  is absent," never a startup failure. The stdio path lives in the **lean library**;
  the HTTP path rides the `mcp-http` feature, so an embedder gets stdio tool servers
  with no CLI/TUI/transport dependency and opts into HTTP by enabling the feature.

### Live add/remove/list — [ADR-0096](../adr/0096-dynamic-toolregistry-sharedregistry.md) + [ADR-0097](../adr/0097-live-mcp-server-management.md) (#372, #375)

The registry `mcp::connect` populates at startup is no longer frozen:
`entanglement_runtime::SharedRegistry` (`Arc<std::sync::RwLock<ToolRegistry>>`)
replaces the owned `ToolRegistry` on every `tool_runner::spawn_tool_executor*`
entry point, and `EngineConfig.tool_spec_resolver` (§ADR-0076) is wired to
snapshot it fresh every turn — so the tools a live add/remove registers reach
both dispatch and model-advertised schemas with no engine restart.

- **`InMsg::McpList { correlation_id }` / `McpAdd { name, config }` /
  `McpRemove { name }`** are engine-global, exactly like `ListSessions`:
  `session()` is `None` and `msg_to_cmd` routes them to no session task. Only
  the read-only `McpList` is wire-allowed; `McpAdd`/`McpRemove` are
  **trusted-only** (#472,
  [ADR-0124](../adr/0124-wire-refused-mcp-mutation-and-stdio-key-scrub.md),
  reversing #375's wire tier — an unapproved `McpAdd` spawns an arbitrary
  local subprocess, and ADR-0047's "enabling is consent" covers the trusted
  config file, not an unauthenticated wire frame). The TUI `/mcp` command is
  unaffected: it sends over the privileged `Holly::send`.
  `McpAdd`'s `config` is a core-owned `McpServerSpec` DTO — core cannot depend
  on the runtime crate, so it never carries the runtime's `McpServerConfig`
  directly; a `From<McpServerSpec>` conversion happens runtime-side. Answered
  by `OutEvent::McpList { correlation_id, servers: Vec<McpServerStatus> }` /
  `McpChanged { name, action: McpAction }` — no `seq`, point-in-time.
- **`mcp::spawn_mcp_responder`** subscribes to `Holly::subscribe_inbound()` and
  answers these three, mirroring `history::spawn_history_responder`'s answer
  to `ReplayFrom`: the runtime, not core, owns the state involved, so a
  runtime-side service is the sole answerer rather than the core supervisor
  (unlike `ListSessions`, which the supervisor answers directly from its own
  live-session directory). Two `Holly::emit_mcp_list`/`emit_mcp_changed`
  helpers mirror `emit_history`.
- **`mcp::live`** holds the runtime state: `ActiveServers` (what is currently
  connected — `name → { client: Arc<McpClient>, tools, transport }`) and the
  wider `ServerConfigs` (every *configured* server, including a `disabled` or
  failed-to-connect one — the full set a persist write must round-trip).
  `mcp_add` upserts (drops any prior tools/connection under the same name
  first, so re-adding cleanly replaces a broken server) and never holds the
  registry's write lock across the connect/`tools/list` awaits. `mcp_remove`
  drops the tracked `Arc<McpClient>`, which is what actually kills the
  subprocess/closes the HTTP session (`StdioClient`'s `kill_on_drop`) — there
  is no separate teardown call. `mcp_list` enumerates `ActiveServers`, sorted
  by name.
- **`config::save_mcp`** (`config/mcp_persist.rs`) persists a live add/remove
  back to `config.yml`: a **surgical `serde_yaml::Value` edit** of just the
  top-level `mcp` key (not the typed `Config`, which would drop unknown keys
  under `deny_unknown_fields`), locked (§ADR-0084) and atomic. Unlike the
  managed sibling files (grants/agent-models/agent-generation/the provider-key
  env file), MCP servers stay part of the primary hand-edited `config.yml` —
  the surgical edit exists precisely to avoid disturbing whatever else
  (`permissions`, `hooks`, …) a user set alongside `mcp:`. Does not preserve
  comments (no layer in this config loader does).
- A failed live add/remove is `tracing::warn!`-logged, not a new `OutEvent` —
  matching the existing best-effort MCP philosophy (§ADR-0067): there is no
  session to attach an error to.
- **Out of scope here:** reconnect-on-external-config-edit (a file watcher) is
  a separate, unscheduled follow-up. The TUI `/mcp` surface landed next —
  §"TUI `/mcp` command" below.

### TUI `/mcp` command — [ADR-0100](../adr/0100-tui-mcp-command.md) (#373)

`Command::Mcp` (`tui/commands.rs`) joins `all_commands()`; its subcommand
parsing (`McpCommand::List`/`Add`/`Remove`, `parse_mcp_args`) and the async
`send_mcp`/`send_mcp_list` wire-dispatch helpers live in a new sibling
`tui/mcp_command.rs` — `commands.rs` and `event_loop.rs` were already past the
400-line cap, mirroring how `CommandPalette` was split out of `commands.rs`
(§ADR-0095). `/mcp list` (or a bare `/mcp`, or picking `/mcp` from the command
palette, which carries no trailing text) sends `InMsg::McpList` with a fresh
correlation id recorded on `tui::mcp_panel::McpPanel`; the matching
`OutEvent::McpList` opens a read-only popup (`modals::draw_mcp_panel`, `Esc`
closes) listing each server's name, transport, connected/error status, and
namespaced tools — a stray reply for a different correlation id (e.g. another
head sharing the engine) is ignored, never opening the panel with the wrong
snapshot. `/mcp add <name> -- <command> [args...]` (stdio) / `/mcp add <name>
--url <url> [--header KEY:VALUE]...` (streamable HTTP) and `/mcp remove
<name>` send `InMsg::McpAdd`/`McpRemove` directly; the confirming
`OutEvent::McpChanged` (or a parse error, caught before the engine is
touched) renders as a transcript status line via `App::handle_mcp_changed`/
`record_mcp_error`, mirroring `/key`'s save notice. No new wire surface — this
is entirely a head-side consumer of the `InMsg`/`OutEvent` pair #375 already
shipped.

[holly]: ../entanglement-core/src/holly.rs
[profile]: ../entanglement-core/src/protocol.rs
[perm]: ../entanglement-core/src/protocol.rs
