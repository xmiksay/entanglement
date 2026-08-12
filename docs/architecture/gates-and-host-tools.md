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
`thiserror`, `tracing`, `futures`, `uuid`. `glob`/`regex`/`ignore` (which back the host
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
`rhai` is deliberately **not** in `LEAN_FORBIDDEN` — the sandboxed script tool
lives behind its own default-on `rhai` feature (#502,
[ADR-0135](../adr/0135-deferred-build-speed-trims-tokio-rhai-syntect.md)
amending ADR-0025; the tool itself is documented in §8 below), so
`--no-default-features` now excludes it (a
lean embedder that never registers the tool sheds one of the heaviest
always-compiled deps) while every default build — including the `skutter`
binary — is unaffected. Each of the three crates' `tokio` dependency also
dropped `features = ["full"]` for a per-crate minimal list grep-verified
against its own API surface (ADR-0135); `syntect` (behind `tui`) trimmed
`default-fancy` down to `parsing`/`default-syntaxes`/`default-themes`/
`regex-fancy` (drops `html`/`plist-load`/`yaml-load`/`dump-create`, none used
by `tui/markdown.rs`) — neither trim touches this gate's forbidden-crate set.

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

## 8. Host tools — [ADR-0008](../adr/0008-host-tools-workdir-and-bounded-output.md) (trio), [ADR-0009](../adr/0009-edit-and-bash-host-tools.md) (`edit`/`bash`), [ADR-0010](../adr/0010-single-head-crate-and-bash-opt-in.md) (`bash` opt-in), [ADR-0045](../adr/0045-call-host-tool-argv-exec-tailed-output.md) (`call`), [ADR-0092](../adr/0092-call-file-based-stdin-stdout.md) (`call` file-based stdin/stdout), [ADR-0093](../adr/0093-call-registration-independent-of-bash-opt-in.md) (`call` always-registered + `workdir`), [ADR-0104](../adr/0104-bubblewrap-sandbox-for-bash-call.md) (optional bubblewrap confinement), [ADR-0134](../adr/0134-per-profile-sandbox-scoping-and-spawn-chain-clamp.md) (per-profile scoping + spawn-chain clamp)

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
caller's `SessionId` through to `Tool::run_with_meta` (#681,
[ADR-0186](../adr/0186-exit-code-joins-the-structured-tool-result-side-channel.md))
— a defaulted method returning `ToolRun{content, exit_code}` that delegates to
`Tool::run_for_session` (#360,
[ADR-0088](../adr/0088-session-aware-tool-execution.md)) with `exit_code: None`,
so only the process runners (`bash`/`call`) override it to surface their numeric
exit status as a real field. `run_for_session` itself stays a default-delegating
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
doesn't hold (and isn't a state tool — `update_tasks`, exempt since it's never
registered, [ADR-0049](../adr/0049-plan-task-tools-as-runtime-state-tools.md);
`propose_plan` never reaches `dispatch` at all, intercepted earlier like
`ask_user`/the spawn family) can never execute, so it would be
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
| `glob` | `{pattern, path?, exclude?}` | matching paths (relative to root), one per line; a metachar-free `pattern` naming an existing directory lists it recursively (`dir/**/*`, containment-gated), `{a,b}` brace sets expand (both [ADR-0150](../adr/0150-search-tool-cli-ergonomics.md)), `path` is an optional base dir the pattern resolves under; `.git` is always excluded (path-component check plus walker pruning, can't be disabled) and so is anything `.gitignore`'d — root or nested `.gitignore`, `.git/info/exclude`, the user's global excludes file: the walk *is* an `ignore`-crate traversal that prunes ignored subtrees before descending (so they cost nothing), matches each entry against the compiled glob pattern with iterator-parity shims, survives symlink loops, and stops after a 100k-entry total-scan budget (`scan_capped`, reported as a "narrow the pattern" notice) — an extra-root-granted walk root, below, disables `.gitignore` filtering so a granted directory's own ignores can't hide what the grant exposed ([ADR-0170](../adr/0170-gitignore-aware-glob-grep-walk.md) semantics, single-pass + bounded mechanism [ADR-0178](../adr/0178-single-pass-gitignore-pruned-walk-with-scan-budget.md), #629/#678) — `exclude` adds caller glob patterns on top of all of that ([ADR-0099](../adr/0099-glob-grep-exclude-and-default-git-exclusion.md)); unknown input fields are refused (`deny_unknown_fields`) and a zero-result call always returns a one-line explanation, never the empty string |
| `grep` | `{pattern, path?, exclude?, case_insensitive?}` | matches as `path:lineno:line` over files matched by `path` — a **directory (searched recursively)** or a glob filter, brace sets included (default `**/*`, [ADR-0150](../adr/0150-search-tool-cli-ergonomics.md)) — minus `exclude`, the always-on `.git` exclusion, and `.gitignore` ([ADR-0099](../adr/0099-glob-grep-exclude-and-default-git-exclusion.md), [ADR-0170](../adr/0170-gitignore-aware-glob-grep-walk.md)); `case_insensitive` (serde alias `"-i"`) complements the inline `(?i)`; unknown input fields are refused; a zero-match call names its cause (*"path filter matched no files — nothing was searched"* vs *"no matches for `X` in N file(s) scanned"*); a file over the 1 MiB **scan** cap (independent of the 32 KiB output cap, [ADR-0091](../adr/0091-grep-file-scan-size-cap-decoupled-from-output-cap.md)) or sniffed as binary (a NUL byte in its content) is skipped and named in a labeled notice appended to the result — regardless of match count |
| `edit` | `{path, oldString, newString, replaceAll?}` | exact-string replace; empty `oldString` creates (refused if exists → hints `write`); non-unique match errors unless `replaceAll` |
| `write` | `{path, content}` | whole-file create/overwrite; missing parent dirs created; `created <path> (N lines)` / `overwrote <path> (N lines, was M)` — confirmation only, never echoes content (ADR-0031) |
| `apply_patch` | `{path, patch}` | apply a unified diff (one or more `@@ -oldStart,oldLen +newStart,newLen @@` hunks) against the current file; each hunk's context/deleted lines are matched **exactly** at the position its header declares (offset by the net line-count delta of hunks already applied in the same patch) — no fuzzy alternate-position search, a mismatch hard-errors before any write and leaves the file untouched; emits `FileChangeKind::ApplyDiff` (#455, the first producer of that previously-reserved variant). Parsing/applying is a small hand-rolled module (`host::unified_diff`), not the `diffy` crate — `diffy` is `tui`-feature-gated and named in `LEAN_FORBIDDEN` above, and `apply_patch` is unconditional lean-library code alongside `edit`/`write` |
| `bash` ⚠ | `{command, timeout?, workdir?, background?, tail?}` | `sh -c` rooted at root (or at `workdir`, a subdir validated under root by the same symlink-safe containment as the fs tools, #170); `[exit N]` + stdout + `[stderr]`; default 120 s timeout, capped at 600; spawned in its **own process group** (`process_group(0)`) so an expiry SIGKILLs the whole tree — grandchildren (a launched server/pipeline) can't orphan (#168); a `Stop`-driven task abort drops the wait future, whose group-kill guard SIGKILLs the same group so cancellation matches the timeout's containment rather than orphaning under bare `kill_on_drop` (#167). Output is drained incrementally, so a timeout returns the **partial output buffered before the kill** under a `[killed: timed out after Ns]` header instead of discarding it (#169). Output is tailed to the last `tail` lines per stream (default 30, matching `call`; `tail=0` = full, byte-cap still applies, #622), then the exit/killed status line plus that body is byte-capped **head + tail** (¼ head / ¾ tail on the body, status line always kept in full — `bounded_result`) so the trailing error survives — head-only truncation dropped exactly what a failing build needs (#170). `background: true` (renamed from `run_in_background`, #606, [ADR-0161](../adr/0161-unified-async-work-background-flag-and-one-poll.md) §1) spawns the command **detached** and returns a job id instead of blocking — wait on it with the runtime-owned `poll` tool (#605, described below); **`timeout` still applies** to a backgrounded job ([ADR-0165](../adr/0165-background-bash-jobs-are-bounded-by-timeout.md), #617) — a deadline task kills the job's process group once it outlives the same default-120s/600s-cap bound a foreground call uses, so backgrounding no longer means unbounded. Stdin is always closed (`Stdio::null()`), never inherited from the engine — the same leaked-by-default class ADR-0092 closed for `call`, applying uniformly to both the foreground and `background` paths since both share the one command builder (#389); use shell-native `< file` redirection if a command needs input |
| `call` ⚠ | `{command, args?, tail?, timeout?, input_file?, output_file?, workdir?, background?}` | **argv, no shell** — `command`+`args` exec verbatim (no `sh -c`, so no pipe/glob/`$VAR`/metachar interpretation); output tailed to the last `tail` lines per stream (default 30, `tail=0` = full, byte-cap still applies), with a `(… N earlier lines omitted, tail=30 — rerun with tail=0 …)` notice; the outer byte cap is head + tail on the body, not head-only (`bounded_result`, #622) — same shape `bash` uses; same envelope as `bash` (`[exit N]` + stdout + `[stderr]`, 120 s/600 s, own-process-group kill on timeout #168, partial output preserved on timeout #169) — ADR-0045. `input_file`/`output_file` (ADR-0092, #381), both root-contained via `resolve_under_root` and validated **before spawn** (relative to the **root**, not `workdir`): `input_file` is read and piped to the child's stdin (fed concurrently with the stdout/stderr drain to avoid a full-pipe deadlock); its **absence closes stdin** (`Stdio::null()`) rather than inheriting the engine's own (a leaked-by-default behavior until now). With an explicit `output_file`, the full **untruncated raw** stdout is persisted there (missing parent dirs created) with a `<output_file>.stderr` sibling always alongside, and its path is named in the result header (`[output: …]`, plus `[stderr: …]` when stderr is non-empty) regardless of truncation; a write failure there is a hard error. With **no** `output_file`, nothing is written to disk — a result that overflows the tail/byte cap instead mints a fresh retained-output handle (`o-…`, ADR-0164) in `RetainedOutputRegistry` and names *that* in the header instead of a path (#608, [ADR-0161](../adr/0161-unified-async-work-background-flag-and-one-poll.md) §7); a small, untruncated result carries no header at all. Either way, **a truncated result keeps its handle** — poll it (`poll`, described below) to page the retained remainder, or to be reminded of the `output_file` path. `workdir` (#386) sets the child's **cwd** to a subdirectory validated under root via the shared `resolve_workdir` (same containment as `bash`'s); a non-directory or escaping `workdir` errors before spawn. A `command` that is really a whole shell line — multiple whitespace-separated tokens with empty `args` (a non-path `command`), or any shell metachar (`| & ; < > $ \` ( )`) — is rejected **before spawn** by `call::validate::check_no_shell` with an actionable error that names the fix (split the line into `command`+`args`, or use `bash` for a real pipeline), rather than failing with an opaque ENOENT on a "binary" named after the entire line (the loop that stranded a delegated PR review). **Registered unconditionally** — independent of `ENTANGLEMENT_ENABLE_BASH` ([ADR-0093](../adr/0093-call-registration-independent-of-bash-opt-in.md)). `background: true` (#606, ADR-0161 §1) spawns detached into the same shared job registry `bash` uses (`CallTool::with_jobs`) and returns a job id to `poll` instead of blocking; refused up front alongside `input_file`/`output_file` — a backgrounded job's output streams through `poll`, not a file artifact, and there is no running wait left to pipe stdin into once the call has already returned |
| `rhai` | `{script, timeout?, background?}` | behind the crate's default-on `rhai` feature (#502, [ADR-0135](../adr/0135-deferred-build-speed-trims-tokio-rhai-syntect.md) amending ADR-0025) — a `--no-default-features` embedder that never registers it can drop the dep entirely; every default build (incl. `skutter`) is unaffected. The **model-facing spec is a short stub** (#619): the binding catalogue below moved into the embedded `rhai` skill (`entanglement-runtime/src/skills/rhai.md`, loaded via `load_skill` — no `allowed_tools`, so loading it never narrows the session's tool mask, ADR-0106) since the full reference sat in the engine's *shared* `tool_specs` and was re-sent on every request of every session regardless of use ([ADR-0037](../adr/0037-load-skill-tool-deterministic-resolution.md) progressive disclosure). Run a Rhai script ([rhai.rs](https://rhai.rs)) in a **capability-sandboxed** engine — no fs/network/process/env access beyond what is explicitly bound; the host bindings are `read`/`glob`/`grep`/`edit`/`write` plus `read_raw` (exact file content, no line-number prefix — `read`'s `"{lineno}: {line}"` format isn't parseable as JSON/YAML; graded and masked as an alias of `read`, not a distinct permission surface, since it is never advertised as a standalone tool), plus permission-gated process-exec — `exec(command)`/`exec(command, args)`/`exec(command, args, workdir)` (marshalled to the `call` tool) and `bash(command)`/`bash(command, workdir)` (marshalled to `bash`, bound only when the host `bash` tool is registered — otherwise an unknown-function script error, not a graded-then-refused binding) — each routed through that tool's permission check (`exec`/`bash` graded under the Call capability like their host-tool counterparts, #419/[ADR-0114](../adr/0114-capability-level-permission-keys.md)). The script-facing name is `exec`, not `call`: `call` is a hard-reserved Rhai keyword for function-pointer invocation the interpreter special-cases ahead of any same-named registered function; the dispatched tool name/permission grade stay the literal `call`. `exec`/`bash` additionally derive their `timeout` from the script's own remaining wall-clock budget rather than the tool's much longer default, since rhai's timeout interrupt can't reach a binding call parked on the sync/async bridge; their `Ask` approval is cached **per resolved command line + `workdir`** (#480), not per bare tool name, so approving one command/workdir pair cannot silently pre-clear a different one in the same run (every other binding keeps the coarser once-per-function cache). An explicit `workdir` (#480, [ADR-0130](../adr/0130-rhai-exec-bindings-marshal-workdir.md), amending [ADR-0115](../adr/0115-rhai-exec-bindings-call-bash.md)/[ADR-0116](../adr/0116-workdir-scoped-permission-rules-for-bash-call.md)) marshals into the delegated tool's own `workdir` field — a `tool{pattern}` workdir-scoped permission rule (#425) now resolves for a binding call exactly as it would a direct `bash`/`call` call (`BindingPolicy::decide` grades through `resolve_scoped`, not the workdir-blind `resolve`), and the same field feeds the escape-root gate below with no separate wiring. A `read`/`edit`/`write`/`exec`/`bash` binding targeting a path or `workdir` outside the project root routes through the same escape-root gate a direct call uses (#446, [ADR-0119](../adr/0119-rhai-bindings-route-through-the-escape-root-gate.md)): it forces an approval carrying the ADR-0109 "outside the project root" warning, bypassing the per-run `Ask` cache, and records the grant into the shared `ExtraRootStore` on approval; a durably-granted (`Session`/`Always`) path resolves silently, same as a direct call. A binding excluded by the session's active skill's `allowed_tools` (#400, [ADR-0106](../adr/0106-skill-scoped-allowed-tools-enforcement.md)) refuses too — checked after the agent mask, same as generic dispatch — since `BindingPolicy` folds in a one-time snapshot of the session's skill mask alongside the agent mask (#477, [ADR-0129](../adr/0129-thread-the-skill-mask-into-rhai-binding-resolution.md); sound as a snapshot because `load_skill` is not itself a binding, so a running script cannot change which skill is active). Also bound, pure (no IO, no permission check, since they only transform a value already in the script): `parse_json`/`to_json`/`parse_yaml`/`to_yaml`, built on Rhai's own `serde` bridge (`null` → `()`, an out-of-`i64`-range integer silently widens to an approximate float rather than erroring — same as JS's `JSON.parse`); last-expression value serialized + captured `print(...)`; bounded by op/string/array/map caps + wall-clock (default 5 s, max 30). `background: true` (#637, [ADR-0185](../adr/0185-rhai-joins-background-and-poll.md)) detaches after the launch gate and returns an `x-` handle to `poll` instead of the result: the script gets the `bash`/`call` background timeout regime (default 120 s, cap 600 — still enforced *inside* the engine by `on_progress`, `spawn_blocking` cannot be aborted), streams its `print` output into the shared `ScriptRegistry` live, survives a session `Stop` exactly like a background job (the executor skips the canceller registration), and keeps mid-run binding `Ask`s (the `ToolRequest` round-trip works detached; session-state transitions are suppressed so an idle session's status is never stranded); `poll kill=true` is cooperative — the stop flag ends the script at its next engine op, after any in-flight `exec`/`bash` binding's own budget-clamped timeout — [ADR-0046](../adr/0046-rhai-sandboxed-script-tool.md) (amended by [ADR-0115](../adr/0115-rhai-exec-bindings-call-bash.md), [ADR-0129](../adr/0129-thread-the-skill-mask-into-rhai-binding-resolution.md), and [ADR-0130](../adr/0130-rhai-exec-bindings-marshal-workdir.md)), [ADR-0098](../adr/0098-rhai-json-yaml-loader-and-read-raw.md) |

- **Working directory:** each tool holds a `root` (the cwd, **canonicalized once
  at startup**); model-supplied paths resolve against it and are rejected on `..`
  escape **and on symlink escape** — `resolve_under_root` canonicalizes the
  resolved target's deepest existing ancestor and requires it under the canonical
  root, so a `root/link -> /etc` symlink can't be followed out of tree by
  `read`/`edit`/`write`/`apply_patch` (the create path still works: only the existing ancestor
  is canonicalized), and `glob`/`grep` (`list_files`) drop any match whose
  canonical path escapes — ADR-0008 upgraded by [ADR-0054](../adr/0054-canonicalizing-symlink-safe-root-containment.md)
  (#163), unless a durable escape-root grant widens it (#482, below). Not
  TOCTOU-tight (an OS sandbox via `openat2(RESOLVE_BENEATH)` is deferred).
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
  (checked against the symlink-resolved target). No store wired (`None`) is
  byte-identical to strict containment. **`glob`/`grep` reach outside root
  through a distinct, narrower mechanism** (#482,
  [ADR-0132](../adr/0132-glob-grep-escape-root-search-via-durable-grant.md)
  amending ADR-0109): unlike the six-tool path above, a search never forces
  its own `Ask` — the executor's `escape_root_target` still returns `None` for
  `glob`/`grep`, so it never trips the gate at all. Instead
  `list_files_with_extra_roots` widens its containment check per match: a
  match whose canonical path escapes root is admitted anyway when it (or an
  ancestor of it) already carries a **durable** (`Session`/`Always`) `read`
  grant (`ExtraRootStore::is_durably_allowed_under`, an ancestor walk over the
  existing per-tool grant set — no new store, no enumeration API). A `Once`
  grant never widens a search, only `read`/`edit`/etc.'s own exact-path check
  — a search's match count is unbounded ahead of time, so treating it as
  consuming a single-use token would let one approval cover arbitrarily many
  reads. `GlobTool`/`GrepTool` gained the same `with_extra_roots` builder the
  other four escape-root tools have; `host/mod.rs`'s `list_files`/`FileList`
  moved into `host/walk.rs` to stay under the 400-line file cap. `rhai`'s file/exec bindings route through the identical (six-tool) gate
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
- **The trusted scratch dir is exempt from the escape-root prompt** (#524,
  [ADR-0142](../adr/0142-trusted-scratch-dir-and-plans-folder-carve-outs.md),
  amends ADR-0109): the runtime-owned per-project scratch dir
  (`session_store::scratch_dir`, already the default `call`-output target)
  needs no approval and no prior grant, for `read`/`edit`/`write`/
  `apply_patch`, `glob`/`grep`, and a `bash`/`call` `workdir` — in every
  profile. `ExtraRootStore` gains an optional `scratch: Option<PathBuf>`
  (`.with_scratch`, set once at startup), consulted by `is_durably_allowed`
  and `take_allowance` **before** the ordinary per-`(tool, path)` grant
  lookup: a `starts_with` check against the canonicalized scratch path, not
  the exact-match key every other grant in the store uses, so every file
  under it is covered with no per-file approval. It composes for free with
  the `is_durably_allowed_under` search-widening above (ADR-0132) — no
  separate wiring for `glob`/`grep`. Not per-tool (a trusted *location*, not a
  one-off tool approval) and never persisted (re-derived from the cwd at
  every startup, nothing to revoke). Only the escape-root tax disappears — a
  profile's own permission grade for the tool (e.g. `explore`'s `bash: ask`)
  is untouched, since the scratch dir being the `workdir` says nothing about
  the command being run there. The generated `<env>` system-prompt block
  (`system_prompt::EnvBlock`) names the scratch dir and steers the model to
  prefer it over `/tmp`.
- **The trust boundary for exec, stated plainly:** root containment applies to
  a `bash`/`call` invocation's **`workdir` only, never its command body** —
  `escape_root_target` inspects no command line, so `bash: allow` (or a live
  tool-overlay `allow: true` entry, #611/ADR-0163) hands the model the engine
  process's full local privileges: it can read/write any path the process can, outside root
  included. The layers that actually bound an exec tool are the permission
  profile (whether/what it may run), the config ceiling (#172), and the opt-in
  bubblewrap sandbox below — not filesystem containment, which governs only
  the six path-arg tools.
- **OS sandbox, opt-in and per-profile scopable (#399/#479,
  [ADR-0104](../adr/0104-bubblewrap-sandbox-for-bash-call.md)/[ADR-0134](../adr/0134-per-profile-sandbox-scoping-and-spawn-chain-clamp.md)):**
  `ENTANGLEMENT_SANDBOX=bwrap` confines every `bash`/`call` spawn under
  bubblewrap by default — `--ro-bind / /` plus the project
  root re-bound read-write at the same path (so `resolve_under_root`'s
  containment above keeps working unmodified inside the sandbox), a fresh
  `/tmp`/`/dev`/`/proc`, and its own pid/ipc/uts/cgroup namespaces.
  `--unshare-net` cuts network by default; `ENTANGLEMENT_SANDBOX_NETWORK=1`
  shares the host network namespace back in. **Fail-closed by omission**: there
  is no fallback to unsandboxed execution when `bwrap` can't be entered (missing
  binary, unprivileged user namespaces disabled) — the spawn simply errors, like
  any missing binary (ADR-0016). An `AgentProfile`'s optional `sandbox:`
  frontmatter key (`bwrap`/`none`/`inherit`) overrides this process-global
  default per profile (#479) — `BashTool`/`CallTool` hold a
  `sandbox_resolver: Arc<dyn policy::SandboxResolver>` instead of a fixed
  `SandboxPolicy`, consulted per call via `Tool::run_for_session`'s
  `SessionId`; `.with_sandbox(policy)` (a fixed policy is trivially its own
  resolver) stays the pre-#479 API, `.with_sandbox_resolver(..)` is the new
  per-profile wiring `main.rs` uses. A spawned child's confinement is clamped
  to its parent's *effective* policy at spawn time — `most_confined` ranks
  confinement (unconfined < bubblewrap-with-network < bubblewrap-no-network),
  the confinement-axis mirror of the ADR-0024 permission ceiling — computed
  once via `policy::record_session_sandbox`/`resolve_sandbox` rather than a
  live per-call ancestor walk, since a confined parent must not spawn an
  unconfined child. The existing process-group timeout/cancel kill
  (#167/#168/#169, below) needs no change — killing the outer `bwrap` process
  cascades through its PID-namespace death to the whole sandboxed tree.
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
  to 2000 lines; `glob`/`grep` cap at 1000 results **with a one-line notice
  when the cap fires** (`FileList.capped`, [ADR-0150](../adr/0150-search-tool-cli-ergonomics.md)
  — a silently truncated broad search used to read as a clean no-match past
  the first 1000 files). Prevents a huge file/tree
  from blowing the context window. `read`/`glob`/`grep`/MCP tools cap
  **head-only** (`truncate_output`) — the head is what matters for a file or a
  search hit list. The exec/agent-shaped tools instead cap **head + tail**
  (`bounded_result`, built on `truncate_head_tail`'s ¼ head / ¾ tail split) —
  build/test output, a script's return value, or a sub-agent's answer put the
  load-bearing part (the error, the result, the conclusion) at the end, so
  head-only truncation would drop exactly what the model needs (#170, unified
  across `bash`/`call`/`rhai`/`agent`/`agent_send`/`poll`
  as one shape with a stated rule, #622); a `bounded_result` status line (an
  exit/job header, a "completed in Ns" line) is kept verbatim and uncounted
  against the cap, only the body is split. `grep`'s per-file **scan** cap (how
  much of a candidate file it reads and searches) is a separate, grep-local 1
  MiB bound (`MAX_SCAN_BYTES`), not the 32 KiB output cap — conflating the two
  meant any file over 32 KiB was silently skipped regardless of the
  match-output size ([ADR-0091](../adr/0091-grep-file-scan-size-cap-decoupled-from-output-cap.md), #380).
- **Empty-result contract (ADR-0016, extended by [ADR-0150](../adr/0150-search-tool-cli-ergonomics.md)):**
  a host tool may not return a silent zero-output — **`glob`/`grep` never
  return the empty string at all**. `list_files` returns `FileList { files,
  matched_dirs, skipped_errors, capped, out_of_root }`; per-entry walk errors
  are `warn!`-logged and counted, not swallowed, and containment drops are
  counted (`out_of_root`) so an absolute-path typo doesn't read as a clean
  no-match. When `glob`'s result would be empty but the pattern matched
  something (the bare-`**` trap, which matches only directories), it returns
  a hint like *"`**` matched 7 directories but no files — try `**/*`"*; a
  clean no-match returns *"pattern `X` matched no files."*. `grep`'s
  zero-match reply distinguishes *"path filter `X` matched no files — nothing
  was searched"* (with the same dir-glob suggestion when the filter matched
  only directories) from *"no matches for `X` in N file(s) scanned"* —
  ADR-0016 originally exempted `grep` from any hint; ADR-0150 supersedes that
  after 57% of real grep calls returned the empty string. `grep` is also
  **not** silent about files it excluded from the scan — a file over
  `MAX_SCAN_BYTES` or sniffed as binary (NUL byte in its content) is tracked
  by skip reason (`TooLarge`/`Binary`) and, whenever that list is non-empty,
  surfaced as a labeled notice (capped preview, `... and N more` past 20
  entries per reason) appended to the result regardless of match count —
  otherwise a match that exists only in an excluded file would look identical
  to a genuine no-match ([ADR-0091](../adr/0091-grep-file-scan-size-cap-decoupled-from-output-cap.md)).
- **Schema advertisement:** `Tool::schema()` feeds `ToolRegistry::specs()`, so
  the model sees a real `input_schema` per host tool (not an empty object).
- **Wiring (ADR-0010, amended by [ADR-0093](../adr/0093-call-registration-independent-of-bash-opt-in.md)):**
  `host_tools(root)` registers the **root-contained sextet**
  (`read`/`glob`/`grep`/`edit`/`write`/`apply_patch`; `write` added in
  ADR-0031, `apply_patch` in #455). The
  `skutter` binary registers `CallTool` **unconditionally**, alongside the
  sextet — no shell means no injection surface, so its registration no
  longer rides `bash`'s opt-in gate (#386). `BashTool` still registers only
  when `ENTANGLEMENT_ENABLE_BASH=1`, because `bash` runs arbitrary shell code
  (ADR-0009). `bash` shares its `JobRegistry` with the always-available
  `poll` tool (#605) — background jobs are pollable regardless of whether
  `bash` itself is registered at startup or later via `/enable tool bash`,
  since it's the same registry either way (one long-lived registry, never
  minted fresh per enable — ADR-0163 §3). `EngineConfig::default()` ships an
  empty registry (embedders opt in via `host_tools`).
  **Live enablement** (#498, originally
  [ADR-0133](../adr/0133-live-bash-enablement-graded-by-permission.md); folded
  into the session tool overlay, #611,
  [ADR-0163](../adr/0163-live-bash-enablement-is-a-tool-overlay-entry.md)):
  the env var is startup-only — a trusted head instead sends
  `InMsg::SetToolOverlay` with a `bash` enable entry, the same generic op that
  enables an MCP server or any other tool (#539, ADR-0149). `bash_live`'s
  runtime responder watches the outbound `OutEvent::ToolOverlayChanged`
  broadcast and, when an enable entry matches `bash` — the one member of a
  closed, runtime-fixed table of lazily-registrable built-ins (ADR-0163
  §2) — registers it into the live `SharedRegistry` mid-session (a no-op if
  already present). The registration is process-global, but its
  **advertisement is session-scoped** (#673,
  [ADR-0179](../adr/0179-lazily-registered-built-ins-advertise-session-scoped.md)):
  the same responder folds each `ToolOverlayChanged` into a
  `BuiltinVisibility` store (cleared per session on `SessionEnded`), and the
  runtime `tool_spec_resolver` filters a lazy built-in's spec to the sessions
  whose own overlay chain enabled it — ancestor walk shared with
  `AvailableMcp::spec_visible` (#630's parent map, via
  `enabled_by_or_ancestor`) — or to everyone when it was registered at
  startup via the env var. Without that filter, one session's
  `/enable tool bash` rewrote every other live session's advertised tools
  array mid-session — a provider prompt-cache bust plus a tool nobody there
  opted into. The TUI command is `/enable tool bash [--allow
  [<pattern>]]` / `/disable tool bash`, the same surface every other tool
  uses. The overlay entry's grade (`allow: false` ⇒ `Ask`; `allow: true` ⇒
  `Allow`, optionally narrowed by `arg_pattern` to an argument-scoped
  `bash(pattern)` rule, ADR-0163 §1) overrides the session's own profile for
  `bash` specifically via `tool_runner`'s generic overlay-grade dispatch
  (`permission::overlay_entry_grade`), still clamped by the config permission
  ceiling (#172) — a `bash: deny` ceiling wins over a live `Allow`. Unlike the
  pre-ADR-0163 `BashGrade`, this composes for *any* tool, not just `bash`.
  The lookup that finds the applicable entry (`permission::overlay_grade_entry`)
  walks the session's ancestor chain nearest-first (✅ #628), so a `bash()`
  binding run from inside a `rhai` script grades identically to a direct
  `bash` call — the `BindingPolicy` snapshot consults the same lookup — and a
  spawned child with no overlay of its own inherits its parent's grade, not
  just the mask that lets the tool exist for it.

The inherit-all profiles (`build`, `debug`; `tools: None`) advertise
`edit`/`write`/`apply_patch`/`bash`/`call` and auto-allow them (default
`Allow`). The masked profiles carve differently
(#116/#140, [ADR-0038](../adr/0038-physical-per-agent-tool-restriction.md)):
`plan`'s allowlist carries `write`/`edit` (graded `deny` outside the
plans-folder carve-out, ADR-0142) and `call`/`bash` (for the ADR-0159
ancestor-clamp reason, graded `ask` per dispatch); `explore` and `research`
mask the write tools out entirely — never advertised, so no
`Allow`/`Ask`/`Deny` default is reached for them there — while advertising
`call`/`bash`/`rhai` at `Ask` grade (ADR-0137/ADR-0167). Registration is
orthogonal to both mask and profile: it controls whether the tool is advertised
at all (unconditional for `call`, opt-in for `bash`), the mask
controls *existence* per profile, and the profile controls *dispatch*
(Allow/Ask/Deny when the model calls a tool that survives the mask) — so `call`
being always-registered does not change what a non-`build` profile can do with
it.

Six **runtime-owned orchestration tools** are *not* in the registry — the
`tool_runner` intercepts them on `ToolExec` before permission resolution (they
touch no host resource) and advertises their schemas separately: `agent {
agent, prompt, background? }` (renamed from `spawn_agent`, ADR-0022; §5,
ADR-0033; the separate `agent_spawn` tool it was later split into is retired
again, #606, ADR-0161 §1 — one tool, `background: bool` picks the return
shape) blocks by default (spawn-and-wait in one call) and, with `background:
true`, hands back a handle immediately instead — both paths reply with `` sub-agent `id` completed in Ns:\n\n{answer} `` bounded by
`bounded_result` (#622): the "completed in Ns" status line is kept verbatim,
the answer gets the same head + tail byte cap as `bash`/`call`/`rhai` so a
long answer's conclusion survives instead of growing the reply unbounded. A
truncated answer also mints a retained-output handle (#614) exactly like a
truncated `call`/`bash` result, so the dropped middle isn't silently lost —
`poll` pages it back. The sponsored-build reply `propose_plan` folds back on
approval (ADR-0138) shares the same bounding + retained-output-handle helper,
and — since #609, ADR-0162 §5 — names the build child's `agent_id` in that
reply too.

`agent_send { agent_id, prompt, background? }` (#609, ADR-0162) is a launcher
against an *existing* session instead of a fresh one: it sends
`InMsg::Prompt` to a child `agent` (or a sponsored `propose_plan` build)
already produced, so the same session picks its turn back up with its
accumulated context rather than starting over. It carries the same
`background` flag and shares `agent`'s guard path — [`AgentRegistry::begin_send`]
resolves ownership (only the launching session may send — the ADR-0161 §4
descendant check, generalized: `agent_send` is a *write* verb on the
registry, so this scoping is a hard prerequisite rather than a hardening
pass) and the child's session lifecycle in one lock acquisition before
anything is sent. A handle that isn't the caller's own, or was never
launched at all, refuses with the same "unknown agent_id" message either
way. A **closed** (tombstoned) child refuses clearly — its id is spent
(ADR-0028). A **hibernated** child refuses loudly rather than silently: a
fresh `Prompt` at a hibernated id would otherwise fall into the supervisor's
lazy-respawn path and come back as a *blank* session wearing the right id,
discarding its context — the idle-TTL sweep (ADR-0090) makes this the likely
failure, not a corner case, so `agent_send` never sends until the lifecycle
check confirms the child is still live. Only a **live** child is actually
sent the prompt; the default (blocking) path then reuses
`collect_child_answer` to wait for its *next* answer (not its first) and
folds it back the same way the blocking `agent` path does; `background: true`
returns immediately and the follow-up answer is collected later with `poll`,
exactly like a `background: true` launch. The lifecycle itself is folded from
the engine-wide `SessionStarted`/`SessionHibernated`/`SessionEnded` broadcast
— any session's transition, not just this executor's own — into the same
`AgentRegistry` entry `poll` already reads.

`poll { handle?, timeout_secs?, kill?, offset?, tail? }` (#605, ADR-0161
§1-4, replacing `bash_output`/`agent_poll` outright — no aliases) is the
single join tool for all four: it dispatches on the handle's kind prefix
(ADR-0164) to the job registry (`j-`), the background-script registry (`x-`,
a `rhai background=true` launch — #637,
[ADR-0185](../adr/0185-rhai-joins-background-and-poll.md)), the
retained-output registry (`o-`, a
completed operation whose result overflowed its cap — #608, ADR-0161 §7), or
the sub-agent registry (anything else, a `s-` session id), waiting up to
`timeout_secs` (default 60, cap 600, `0` = wait until terminal) for new
output/exit (job), new print output/finish (script — the terminal poll also
carries the script's final `=> value`/error line), or the final answer
(agent) — the delta-vs-final
distinction rides the `running`/`complete` status, not the tool name. A
retained-output poll never waits (the operation already finished) and
instead pages the text: `offset` (default 0) is a 0-based line index into
the retained text, `tail` (default 30, matching `call`'s own default) is how
many lines to return from there — `0` returns the rest, still byte-capped.
An entry whose operation had an explicit `output_file` is named instead of
paged, since the file already holds the full text — the one place a path
still appears. `kill: true` SIGKILLs a job's process group; on a script
handle it is **cooperative** (ADR-0185) — it trips the stop flag the
engine's progress callback polls, the script ends at its next operation
(an in-flight `exec`/`bash` binding finishes its own budget-clamped timeout
first), the kill poll reports "stop requested", and the *next* poll reports
`stopped (killed)`; refused on a
sub-agent or retained-output handle (nothing to cancel/kill in the latter
case). An unknown (or not-the-caller's-own) handle is an *error*, adopting
`agent_poll`'s convention over `bash_output`'s return-it-as-text — and these
model-mistake branches (unknown handle, refused `kill`) plus a script's own
terminal error also set the structured `is_error` flag on the `ToolResult`
(#695, closing the hand-audit remainder ADR-0176 deferred for this
runtime-owned route); every outcome of a poll that ran —
running/complete/list/paged, a job exiting nonzero (`exit_code` stays
orthogonal, ADR-0186), a killed job or cooperatively-stopped script — stays
`false`. Retention
is a `RetainedOutputRegistry` shared with `call`, `agent`, and the sponsored
build reply `propose_plan` folds back (#614) the same way `JobRegistry` is
shared with `bash`/`call` — capped per-operation (256 KiB, keeping the
tail) and evicted on the same 15-minute-TTL/200-entry shape `JobRegistry`
uses (`ScriptRegistry` mirrors all of it: capped streamed output with
reported drops, owner scoping, the same TTL/count sweep); overflow past that
cap is reported in the poll result, never silent.
Called with
**no `handle`** (#607, ADR-0161 §6), `poll` instead lists this session's own
pending operations — every outstanding job/script/sub-agent it launched, with
kind,
handle, launcher, elapsed time, and status — via
`entanglement_runtime::operations::list_operations`, shared with the
head-facing `InMsg::ListOperations` → `OutEvent::OperationList` (wire-allowed,
same read-only-snapshot rationale as `ListQuestions`). Attribution rides the
*same* ownership bookkeeping the single-handle join already needs: `JobRegistry`
tracks each job's owning `SessionId` (#605), `ScriptRegistry` each script's
launching session (#637), and `AgentRegistry` each child's
launching parent + profile (#618, #607) — one mechanism serving both. Lifetime
is deliberately not uniform across kinds: an agent handle is itself a session,
so a completed entry stays listed until polled by handle (never evicted,
unlike a finished job's TTL-bounded entry); a background job is an OS process
owned by this engine process — and a background script an in-process task with
the same engine-bound lifetime — so neither can outlive it; a resumed session
can therefore legitimately show agents and no jobs/scripts. `poll` is
intercepted before permission resolution exactly like `agent`
— it starts nothing and touches no host resource, only reading state a
previously-graded launch produced —
`ask_user { questions: [{question, options, multi_select}] }` (§5, ADR-0027;
v2 #488, ADR-0127 — batched questions, `multi_select` per question, an
unconditional free-text "Other" answer; #515, ADR-0146 — a head can list every
open question (`InMsg::ListQuestions`), withdraw one without cancelling the
turn (`InMsg::RetractQuestion`), or swap its content in place
(`InMsg::ReplaceQuestion`), tracked by a runtime-owned `OpenQuestions`
registry beside `PendingDecisions`), and
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
| `post_tool_use` | in `run_and_reply` after the tool result, before it folds back | no — observational (exit code logged, never fed to the model); it cannot rewrite the result | `{event, session, tool, input, output, is_error, exit_code}` — `is_error` (#636, ADR-0176) is the same structured classification that rides `OutEvent::ToolOutput`, so a hook can branch on outcome without re-parsing `output`'s text; `exit_code` (#681, ADR-0186) is the observed process exit status for `bash`/`call` (JSON `null` for every other tool and for a killed process), so a hook can branch on a specific status without string-matching `[exit N]` |
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
  (execution) and the JSON-RPC result/error split (`client::jsonrpc_payload`).
  stdio keeps a flat **60 s** per-request timeout so a hung subprocess can't
  park a turn; HTTP's per-request bound is now the shared endpoint pool's own
  (below, [ADR-0157](../adr/0157-mcp-http-transport-shares-the-endpoint-pool.md)).
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
    (and the negotiated `MCP-Protocol-Version`). Since
    [ADR-0153](../adr/0153-mcp-server-oauth.md) the transport itself lives in
    **`entanglement-provider::mcp`** (as `McpHttpClient`) and is reached through
    core's re-export, the same path `McpServerState` takes — mechanism in the leaf
    crate, policy (config, registry, permissions, the token file, the browser
    launch) in the runtime. The runtime therefore names no `reqwest` of its own;
    `mcp-http` is now a pure compile gate deciding whether the HTTP-MCP paths are
    built at all, and `reqwest` rides in via core→provider exactly as ADR-0025's
    lean gate already sanctions. Re-exported as `mcp::HttpClient` under its
    historical name, still **public** so an embedder can build a per-tenant client
    with a per-user token and register its tools without the YAML path.
    Every request rides the **shared per-endpoint pool** (✅ #559,
    [ADR-0157](../adr/0157-mcp-http-transport-shares-the-endpoint-pool.md)) —
    `connect`/`connect_authenticated` take the caller's `entanglement_provider::
    HttpClient` (re-exported from core alongside `McpHttpClient` so the
    `mcp-http`-only lean build still names no direct provider dependency) and
    an optional `api_key`, and every `POST` goes through
    `HttpClient::execute_with_retry` exactly like the LLM wire clients — the
    same connection pool, RPM/concurrency caps, and 429/`Retry-After` handling,
    keyed by `(this server's own URL, api_key)` so it gets its own bucket,
    isolated from its provider's LLM endpoint. This closes the gap where a
    provider-bundled server sharing its provider's key (below) issued
    completely unmetered requests against that key's real rate limit. The
    provider key is resolved from `AvailableServer.key_env` (below) at the
    startup connect and the lazy `/enable mcp` connect; a live `/mcp add` or an
    OAuth reconnect passes `api_key: None` (still pooled, just unkeyed).
  - **OAuth (✅ [ADR-0153](../adr/0153-mcp-server-oauth.md)):** a server entry may
    carry an optional `oauth:` block. Present — *even empty* — it switches that
    server from static-header auth to a browser-obtained bearer token, since most
    remote MCP servers are OAuth-protected and issue no pre-registered
    `client_id`. Endpoints come from **RFC 9728** protected-resource metadata (off
    the `401` `WWW-Authenticate: resource_metadata` pointer, else the well-known
    path) chained into **RFC 8414** authorization-server metadata; a client is
    minted on the fly by **RFC 7591** dynamic client registration as a *public*
    client (`token_endpoint_auth_method: none`), so a URL alone suffices. **PKCE
    S256** is mandatory and the only method offered. Every field in the block is an
    override; setting `authorization_url` + `token_url` skips discovery entirely.
    The redirect is caught by a one-request loopback listener hand-rolled on
    `tokio::net` (RFC 8252, bound to `127.0.0.1`, mismatched `state` refused
    outright — never `axum`, which must not reach the leaf crate). Credentials
    live in the managed `mcp-tokens.yml` (`0600`, atomic + `fd-lock`ed like the
    sibling managed files, override `ENTANGLEMENT_MCP_TOKENS_FILE`) **together
    with the resolved endpoints and client id**, so a startup connect skips
    discovery/registration entirely — those run only during an explicit
    `/mcp connect`. Tokens refresh on expiry and once more on a `401`; every
    `Debug` impl redacts secrets. Driven by the trusted-only, **wire-refused**
    `InMsg::McpAuth { name, action: Connect|Check|Disconnect }` →
    `OutEvent::McpAuthChanged` (a forged connect would open a browser and mint a
    durable credential; even `Check` mutates state by refreshing), answered by the
    same `mcp::spawn_mcp_responder` that answers `McpList` — each op detached, since
    a connect parks up to five minutes on the browser. Startup **never** opens a
    browser: an unauthenticated OAuth server is skipped non-fatally and reported as
    `needs auth` in `/mcp list`. TUI: `/mcp connect|check|disconnect <name>` plus
    `c`/`t` on the panel's highlighted row; the authorize URL is always rendered as
    transcript content (never a toast) so a headless/SSH session can copy it when
    the browser launch fails. `/mcp disconnect` attempts RFC 7009 revocation when
    advertised, then deletes locally regardless. `/mcp connect` only ever works
    for a server whose deployment actually exposes RFC 9728/8414 discovery — a
    server that issues static bearer tokens has no such endpoints, so the flow
    can never complete no matter what the `oauth:` block contains; `connect`'s
    error says so and points at static `headers:` + the managed `.env` instead
    (#561). **Device-code flow (✅ [ADR-0182](../adr/0182-mcp-oauth-device-code-flow-and-closed-refresh-race.md),
    #631):** `/mcp connect <name> --device-code` runs RFC 8628 instead — no
    browser, no loopback listener. `DeviceFlow` shares discovery/DCR with the
    browser flow (registration now declares an explicit `grant_types`, e.g.
    `["urn:ietf:params:oauth:grant-type:device_code", "refresh_token"]`, with no
    `redirect_uri`) and polls the token endpoint
    (`grant_type=urn:ietf:params:oauth:grant-type:device_code`) honoring
    `authorization_pending`/`slow_down` until the user finishes elsewhere —
    `McpAuthChanged`'s interim event carries a `user_code` alongside
    `authorize_url` (the plain `verification_uri`) for every head to render. The
    **cross-process refresh race** ADR-0153 accepted for v1 is closed the same
    ADR: `TokenStore::with_exclusive` lets `McpTokenStore` hold its file lock for
    the whole load→refresh→save section instead of just the write, so two
    `skutter` instances can no longer both redeem the same rotating refresh
    token — a losing racer picks up the winner's already-refreshed token instead
    of failing. **Web-redirect flow (✅ [ADR-0187](../adr/0187-mcp-oauth-web-redirect-flow-for-embedders.md),
    #684):** a server-side web embedder can't use either in-tree flow — the
    loopback listener is CLI-shaped and device-code trades UX for it — so
    `provider::oauth::web::WebFlow` prepares the same authorization-code +
    PKCE request against the *embedder's own* HTTPS callback URI: `begin`
    shares discovery/DCR/PKCE with the browser flow (via the extracted
    `flow::prepare` helper; the DCR `client_name` is caller-supplied so the
    consent screen names the embedder's product) but binds nothing and never
    blocks; the returned `PendingWebAuthorization` is **serde-serializable**
    plain data (a multi-replica embedder round-trips it through its shared
    store between the two callback requests — it briefly holds the PKCE
    verifier and any `client_secret`, accepted because `StoredAuth` persists
    strictly more; `Debug` still redacts both), and `complete(code, state)`
    verifies `state` before any network I/O, then exchanges and returns the
    `StoredAuth` the embedder saves into its per-user `UserTokenStore`
    (ADR-0184). The runtime/TUI never drive it; core deliberately does not
    re-export it (embedder-facing, like `user_store`).
- **Session-keyed per-user scopes (✅ [ADR-0188](../adr/0188-session-keyed-per-user-mcp-scopes.md),
  #684):** `mcp::scoped::McpScopes` gives a multi-user embedder per-user MCP
  server *sets* and credentials with no user identity in the runtime
  (ADR-0181): an embedder-supplied `McpScopeResolver` maps a `SessionId` to an
  `McpScope { key, servers, token_store }` — the key an opaque string derived
  from its own user identity, the servers the config `mcp:` shape (capability
  hints and `oauth:` blocks included), the store typically
  `user_scoped(store, user)`. **Replace semantics:** a scoped session's
  `mcp__*` namespace is entirely scope-owned — global MCP tools are stripped
  from its advertised specs (`overlay_specs`, called from the embedder's
  `tool_spec_resolver`) and from every dispatch snapshot
  (`overlay_registry_for_call`, applied in the executor's detached task before
  `dispatch`; the `rhai` arm sees cached scope tools only, no lazy connect) —
  which is what keeps same-named servers (user A's `kb`, user B's `kb`, the
  global `kb`) unambiguous behind unchanged `mcp__<server>__<tool>` names.
  Connections are lazy, cached per `(scope key, server)` with the #556
  double-checked guard, and live for the process lifetime; eviction is the
  embedder's explicit `evict_scope` (logout, config drift under an unchanged
  key). `prewarm(&session)` between `Spawn` and the first prompt connects and
  lists the scope's servers so the sync advertisement path has specs to serve.
  An `oauth:` server with no stored credential in the scope's slice fails as a
  clean auth-required *tool error* before any connect — the embedder's cue to
  run its web-OAuth flow. A null resolver (and every in-tree head: skutter
  passes `None`) is byte-identical to the global single-user behavior.
- **Proxy (`mcp::tool::McpTool`):** adapts one remote tool. `schema()` returns the
  server's `inputSchema` verbatim; `run()` JSON-decodes the model's input to the
  `arguments` object, checks it against the schema's top-level `required` array
  before ever contacting the server — a missing field bails with `` tool `name`
  requires parameter `field` `` instead of a cryptic server-side JSON-RPC error
  (#594) — then calls `tools/call` and flattens the result's text content
  (v1 is text-only — a non-text block is noted, an `isError` result prefixed).
  Advertised name **`mcp__<server>__<tool>`**, sanitized to the providers'
  `^[A-Za-z0-9_-]+$` rule, so it can't collide with a host tool or another server.
  Governed by the same #116 agent tool mask as any host tool — and since a
  profile author can't know (or keep current) the namespaced names, mask entries
  are wildcard patterns (✅ #537,
  [ADR-0148](../adr/0148-glob-patterns-in-the-agent-tool-mask.md)): a
  `tools:`-restricted profile opts into MCP with `"mcp__*"` (every server) or
  `"mcp__<server>__*"` (one server), an inherit-all profile opts out with
  `disallowed_tools: ["mcp__*"]`. Existence (the mask, per-name via glob) and
  grading (`capabilities:` fan-out below, ADR-0117) now compose: a profile can
  both *hold* an MCP tool and grade it through a bare capability rule.
- **Config:** the `mcp:` section of the layered user config (§ADR-0047/#172), a map
  of server name → `McpServerConfig`. A block is one transport XOR the other —
  `{command, args, env}` (stdio) **or** `{url, headers, oauth}` (HTTP; `oauth` is
  the ADR-0153 block above), plus a shared
  `disabled` and the three-state `state` (ADR-0152, below) — resolved by
  `McpServerConfig::transport()`, which rejects both-set or
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
  `mcp::connect` fans every server's connect+handshake+`tools/list` out
  **concurrently** (`tokio::task::JoinSet`) rather than one after another, and
  the HTTP transport's `initialize`/`notifications/initialized` handshake rides
  a **fast-fail retry override** (2 attempts, a short fixed backoff, a short
  response-header deadline) instead of the shared endpoint pool's LLM-tuned
  ladder — so one unreachable server costs a fraction of a second instead of
  ~10s, and no longer serializes behind every other configured server
  (✅ #660, [ADR-0169](../adr/0169-startup-mcp-connect-is-concurrent-and-fast-fail.md)).
  A live `/mcp add`/`/mcp connect`/`mcp_enable` connect keeps the pool's
  patient default — only the startup path passes the override.

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

### Provider-bundled servers & three-state enablement — [ADR-0152](../adr/0152-provider-bundled-mcp-servers-three-state-enablement.md) (#542)

Provider-bundled MCP servers are **catalog data**: `ProviderEntry.mcp_servers`
(name → `ProviderMcpServer` — transport, `${VAR}` headers, #426 capability
hint, default state) ships z.ai's `web_search_prime`/`web_reader`/`zread` in
the embedded `defaults.yml`, authenticated by the provider's own
`key_env` (`Bearer ${ZAI_API_KEY}`), every tool hinted `read`. A same-name
user `mcp:` entry overrides field-wise (`available::merge_user_over_bundled`);
bundled servers never enter `ServerConfigs`/`save_mcp` — they are not
persistable state, and a `McpRemove` of one merely disconnects it.

Every MCP server now has a **three-state activation**
(`McpServerState`, `McpServerConfig::effective_state()` — the optional
`state:` field wins, else the legacy `disabled` bool maps `true` ⇒ `disabled`
/ `false` ⇒ `enabled`):

- **`enabled`** — connects at startup, advertised everywhere (mask/overlay
  still apply). The user-entry default.
- **`allowed`** — *available*, not connected: visible to `/enable mcp <name>`,
  the `/mcp` panel's `e` key, and the agent's **`mcp_enable` tool** (an
  ordinary profile-graded registry tool, `load_skill`-shaped; the live
  available roster rides its schema as a dynamic `enum`). Enabling lazily
  connects (`available::enable_for_session` — `mcp_add` minus persistence)
  and marks the calling session in `AvailableMcp`; the runtime's
  `tool_spec_resolver` then filters a lazily-connected server's specs to its
  enabling sessions **and their spawn descendants** (✅ #630,
  `available_lifecycle::ancestor_enabled` walks a child→parent map
  `spawn_mcp_responder` folds off `SessionStarted`, resolved live so an
  ancestor's enable that happens after the child already exists is picked up
  too — a second, independent copy of the same links `subagent::SpawnGuard`
  tracks, since that one stays deliberately single-threaded inside the tool
  executor's own loop), so enablement is **session-scoped and ephemeral**
  (`/disable mcp <name>` unmarks; the connection stays up for others). The
  bundled-entry default. Availability is **key presence read live** from the
  process env, so a `/key` save unlocks a bundle with no restart; keyless ⇒
  silently absent everywhere. Both the enablement marks and the parent links
  are dropped on `SessionEnded` (`available_lifecycle::forget_session`, same
  responder fold) so neither grows for the process lifetime — deliberately
  **not** on `SessionHibernated`, since a lazy enable is never logged for
  replay the way the #539 tool overlay is, so clearing it there would strand
  a resumed session with no way to get its tools back short of re-enabling.
- **`disabled`** — invisible; only a config edit lifts it.

`McpServerStatus` gains an optional `state` (`"enabled"`/`"allowed"`) and the
responder's `McpList` snapshot appends available-unconnected servers
(`connected: false`), which the TUI panel paints "available", not red.

A bundled server's connect — startup (`enabled`) or lazy
(`enable_for_session`, the `allowed` default's actual activation path) —
resolves `AvailableServer.key_env` live and hands the value to the HTTP
transport as `api_key` (✅ #559,
[ADR-0157](../adr/0157-mcp-http-transport-shares-the-endpoint-pool.md)), so
its traffic shares the shared endpoint pool's rate-limit budget with the
provider's LLM endpoint using the same key — see §"MCP client" above.

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
