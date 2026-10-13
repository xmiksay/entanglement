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
self-test that pins the vacuous-pass fix (plus the `userid` and `file-cap`
gates' self-tests). It runs with the default tools, then `POSIXLY_CORRECT=1`
(strict POSIX GNU grep/find), then `AWK='gawk --posix'` and `AWK=mawk` when
each is installed (mawk ignores `POSIXLY_CORRECT`, so strict-POSIX awk needs
its own pass, #779) — and CI installs both awks and runs it as its own step
(`REQUIRE_AWKS=1`: a missing awk fails rather than skips), so
a gate leaning on a GNU extension fails loudly
(#770: `userid-gate.sh`'s trailing GNU-only `--include` became a filename
operand under strict POSIX, grep's error exit read as "no match", and the gate
passed vacuously; it is now POSIX `find` + `grep` with grep errors fatal, and a
target directory with no `.rs` files fails too, #779).
`cargo-deny` bans (ADR-0006's stated
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
— a defaulted method returning `ToolRun{content, exit_code, is_error}` that
delegates to `Tool::run_for_session` (#360,
[ADR-0088](../adr/0088-session-aware-tool-execution.md)) with `exit_code: None`
and `is_error: false`, so only the process runners (`bash`/`call`) override it
to surface their numeric exit status as a real field, and `McpTool` to carry a
server's in-band `isError` onto the ADR-0176 `is_error` channel (#727).
`ToolRun` derives `Default` and has `ToolRun::new(content)`; implementors build
it with struct-update syntax so a future metadata field is non-breaking.
`run_for_session` itself stays a default-delegating
method (falls back to `run_content`) so every in-tree tool is unaffected; a
multi-tenant embedder overrides it to dispatch per-tenant MCP endpoints or scope
a DB-backed tool's writes to the caller, since a shared `ToolRegistry` otherwise
can't tell tenants apart at execution time even though the executor's
`.resolver(..)` seam (#311) already resolves *permission* per session. `Ask`
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
or let an `Always`-scoped approval record a grant for it. (The former
carve-out for an unregistered-but-advertised `bash` is gone: ADR-0195
registers `bash` at startup, so there is no advertised-but-unregistered
built-in whose decline needed its own wording. A call to a tool that is
**registered but unadvertised** — the script-only `call`/`glob`/`grep` — is
not "unknown": ADR-0192 already dispatches any registered tool regardless of
advertisement, so it reaches permission resolution normally. An unregistered
`mcp__<server>__*` name connects its server lazily first (§10, [ADR-0226](../adr/0226-one-advertised-surface-mcp-envelope-registered-is-available.md)); a
malformed `mcp` envelope is declined with the envelope's schema. Only a
name the registry genuinely doesn't hold falls to this short-circuit, whose
fuzzy-match hint is the unknown-tool leg of [ADR-0196](../adr/0196-tool-search-and-lazy-discovery-replace-the-invoke-envelope.md)'s
error taxonomy, below.)
`ToolRegistry::unknown_tool_message` backs both this short-circuit and `execute`'s own
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
| `bash` ⚠ | `{command, timeout?, workdir?, background?, tail?}` | `sh -c` rooted at root (or at `workdir`, a subdir validated under root by the same symlink-safe containment as the fs tools, #170); `[exit N]` + stdout + `[stderr]`; default 120 s timeout, capped at 600; spawned in its **own process group** (`process_group(0)`) so an expiry SIGKILLs the whole tree — grandchildren (a launched server/pipeline) can't orphan (#168); a `Stop`-driven task abort drops the wait future, whose group-kill guard SIGKILLs the same group so cancellation matches the timeout's containment rather than orphaning under bare `kill_on_drop` (#167). Output is drained incrementally, so a timeout returns the **partial output buffered before the kill** under a `[killed: timed out after Ns]` header instead of discarding it (#169). Output is tailed to the last `tail` lines per stream (default 30, matching `call`; `tail=0` = full, byte-cap still applies, #622), then the exit/killed status line plus that body is byte-capped **head + tail** (¼ head / ¾ tail on the body, status line always kept in full — `bounded_result`) so the trailing error survives — head-only truncation dropped exactly what a failing build needs (#170). `background: true` (renamed from `run_in_background`, #606, [ADR-0161](../adr/0161-unified-async-work-background-flag-and-one-poll.md) §1) spawns the command **detached** and returns a job id instead of blocking — wait on it with the runtime-owned `poll` tool (#605, described below); **`timeout` still applies** to a backgrounded job ([ADR-0165](../adr/0165-background-bash-jobs-are-bounded-by-timeout.md), #617) — a deadline task kills the job's process group once it outlives the same default-120s/600s-cap bound a foreground call uses, so backgrounding no longer means unbounded. Stdin is always closed (`Stdio::null()`), never inherited from the engine — the same leaked-by-default class ADR-0092 closed for `call`, applying uniformly to both the foreground and `background` paths since both share the one command builder (#389); use shell-native `< file` redirection if a command needs input. Its spec description is the **single home of the exec-tool decision rule** (#740) — `bash` for shell syntax, `call` for one fixed argv command, `rhai` for multi-step/structured-data logic (that sentence compiled in only with the `rhai` feature), all graded by the same mode — because `bash` is the kernel's shell tool and its description rides the cached tools block every session sees; no shipped agent body repeats it (`built_in_bodies_leave_the_exec_rule_to_the_bash_spec`) |
| `call` ⚠ | `{command, args?, tail?, timeout?, input_file?, output_file?, workdir?, background?}` | **argv, no shell** — `command`+`args` exec verbatim (no `sh -c`, so no pipe/glob/`$VAR`/metachar interpretation); output tailed to the last `tail` lines per stream (default 30, `tail=0` = full, byte-cap still applies), with a `(… N earlier lines omitted, tail=30 — rerun with tail=0 …)` notice; the outer byte cap is head + tail on the body, not head-only (`bounded_result`, #622) — same shape `bash` uses; same envelope as `bash` (`[exit N]` + stdout + `[stderr]`, 120 s/600 s, own-process-group kill on timeout #168, partial output preserved on timeout #169) — ADR-0045. `input_file`/`output_file` (ADR-0092, #381), both root-contained via `resolve_under_root` and validated **before spawn** (relative to the **root**, not `workdir`): `input_file` is read and piped to the child's stdin (fed concurrently with the stdout/stderr drain to avoid a full-pipe deadlock); its **absence closes stdin** (`Stdio::null()`) rather than inheriting the engine's own (a leaked-by-default behavior until now). With an explicit `output_file`, the full **untruncated raw** stdout is persisted there (missing parent dirs created) with a `<output_file>.stderr` sibling always alongside, and its path is named in the result header (`[output: …]`, plus `[stderr: …]` when stderr is non-empty) regardless of truncation; a write failure there is a hard error. With **no** `output_file`, nothing is written to disk — a result that overflows the tail/byte cap instead mints a fresh retained-output handle (`o-…`, ADR-0164) in `RetainedOutputRegistry` and names *that* in the header instead of a path (#608, [ADR-0161](../adr/0161-unified-async-work-background-flag-and-one-poll.md) §7); a small, untruncated result carries no header at all. Either way, **a truncated result keeps its handle** — poll it (`poll`, described below) to page the retained remainder, or to be reminded of the `output_file` path. `workdir` (#386) sets the child's **cwd** to a subdirectory validated under root via the shared `resolve_workdir` (same containment as `bash`'s); a non-directory or escaping `workdir` errors before spawn. A `command` that is really a whole shell line — multiple whitespace-separated tokens with empty `args` (a non-path `command`), or any shell metachar (`| & ; < > $ \` ( )`) — is rejected **before spawn** by `call::validate::check_no_shell` with an actionable error that names the fix (split the line into `command`+`args`, or use `bash` for a real pipeline), rather than failing with an opaque ENOENT on a "binary" named after the entire line (the loop that stranded a delegated PR review). **Registered unconditionally** ([ADR-0093](../adr/0093-call-registration-independent-of-bash-opt-in.md); since [ADR-0195](../adr/0195-bash-is-the-default-exec-and-curated-read-only-rules.md) `bash` registers unconditionally too — and since [ADR-0226](../adr/0226-one-advertised-surface-mcp-envelope-registered-is-available.md) `call` is never advertised: registered, reachable as a `rhai` binding, graded under its own name). `background: true` (#606, ADR-0161 §1) spawns detached into the same shared job registry `bash` uses (`CallTool::with_jobs`) and returns a job id to `poll` instead of blocking; refused up front alongside `input_file`/`output_file` — a backgrounded job's output streams through `poll`, not a file artifact, and there is no running wait left to pipe stdin into once the call has already returned |
| `rhai` | `{script` XOR `path, args?, timeout?, background?}` | behind the crate's default-on `rhai` feature (#502, [ADR-0135](../adr/0135-deferred-build-speed-trims-tokio-rhai-syntect.md) amending ADR-0025). The **model-facing spec is self-sufficient**: it inlines the full binding signature list plus a chained-call example (`script/spec.rs`'s `BINDING_REFERENCE`, pinned to the registration code by `spec_description_lists_every_registered_binding`; #826 measured that GLM models only chain calls inside one script when shown how); the embedded `rhai` skill (`skills/rhai.md`) keeps the long-form prose and worked examples, and the same reference is appended to an unknown-name script error (`spec::binding_hint`). Exactly one of inline `script` or a script file `path` ([ADR-0228](../adr/0228-rhai-path-and-args-skill-tools-frontmatter-removed.md), §`rhai(path)` below — the launch is graded like any tool on its `path`; a `rhai.trusted` script runs with no card, an `Ask` grade gets one Run all / Ask per command / Reject card); `args` (an object) is the constant map `args` in either. A **capability-sandboxed** engine — no fs/network/process/env access beyond what is bound. **Every binding call goes through the executor's dispatch seam** (`Dispatcher::run` with `Via::Script`, [ADR-0227](../adr/0227-one-binding-path-namespaces-http-skill-libraries.md)): mode grade, overlay, ceiling, grants, escape-root (ADR-0109/[0119](../adr/0119-rhai-bindings-route-through-the-escape-root-gate.md)), `pre_tool_use`/`post_tool_use` hooks, pre-dispatch arg validation, lazy MCP connect, `FileChange` capture, the in-call grader — so a binding grades, runs and is observed exactly like a direct call; the nested call's approval card is `ApprovalKind::Rhai`, tool `<tool> (rhai)`, request id `<rhai id>:rhai:<tool>`. Bindings: `read`/`read_raw` (exact content; grades as `read`)/`glob`/`grep`/`glob_json`/`grep_json` (structured, ADR-0206; grade as `glob`/`grep` via `grade::graded_name`)/`edit`/`write`; `exec(command[, args[, workdir]])` (the `call` tool — `call` is a reserved Rhai keyword) and `bash(command[, workdir])` (bound only when `bash` is registered), both with a `timeout` derived from the script's remaining budget (#419) and an explicit `workdir` ([ADR-0130](../adr/0130-rhai-exec-bindings-marshal-workdir.md)); `http(method, url[, #{headers, body}])` (§`http()` below); the generated `mcp::<server>::<tool>(#{…})` and `skill::<skill>::<fn>(…)` namespaces (§Script surface below); pure `parse_json`/`to_json`/`parse_yaml`/`to_yaml`. The seam's context is captured at script launch (the `Dispatcher` is a per-run snapshot: registry, chain, overlays, mode). The per-run approval cache keys on **tool + full input** for every binding; every other nested call — except the file-mutating `write`/`edit`/`apply_patch` — runs under a bridge timeout clamped to the remaining budget less a short grace, and a cut call reports its outcome as unknown (`Outcome::TimedOut`), never "never ran"; a refusal throws (catchable), a run returns its text — `http()`/MCP failures throw too. Nested `rhai`, the `mcp` envelope and every runtime-owned/`Control` tool are unreachable (`dispatch::script::unreachable`). Last-expression value serialized + captured `print(...)`; bounded by op/string/array/map caps + wall-clock (default 5 s, max 30). `background: true` (#637, [ADR-0185](../adr/0185-rhai-joins-background-and-poll.md)) detaches after the launch gate and returns an `x-` handle to `poll`: the `bash`/`call` background regime (default 120 s, cap 600), live `print` streaming into the `ScriptRegistry`, survives a session `Stop`, keeps mid-run binding `Ask`s with session-state transitions suppressed; `poll kill=true` is cooperative — [ADR-0046](../adr/0046-rhai-sandboxed-script-tool.md) (amended by [ADR-0115](../adr/0115-rhai-exec-bindings-call-bash.md), [ADR-0130](../adr/0130-rhai-exec-bindings-marshal-workdir.md), [ADR-0227](../adr/0227-one-binding-path-namespaces-http-skill-libraries.md), [ADR-0228](../adr/0228-rhai-path-and-args-skill-tools-frontmatter-removed.md)), [ADR-0098](../adr/0098-rhai-json-yaml-loader-and-read-raw.md) |

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
  `read`/`edit`/`write`/`apply_patch` path, a `rhai` script `path` (ADR-0228) or a `bash`/`call` `workdir` that resolves *outside*
  root can be reached after the user explicitly approves it. The executor detects
  the out-of-root target (`permission::escape_root_target` + `host::escaping_path`),
  forces an approval prompt even when the profile would `Allow` (a `Deny` floor
  still wins), and records the grant in a shared `ExtraRootStore`
  (`extra_roots.rs`; managed `extra-roots.yml` in the managed-state dir,
  `ENTANGLEMENT_STATE_DIR`) keyed by `(tool, resolved-absolute-path)` —
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
- **Session working directory** (#764,
  [ADR-0224](../adr/0224-session-working-directory.md), `workdir.rs`): a
  session's `/cd` directory (core's `ToolExec.workdir`) is the default cwd of
  `bash`/`call`/`rhai` exec and the base of relative path arguments. The
  executor applies it as a **rewrite of the call's input** on each `ToolExec`,
  before grading, hooks, the escape-root gate and execution
  (`workdir::rewrite`; a rhai binding on the script bridge before it enters the seam):
  `read`/`read_raw`/`edit`/`write`/`apply_patch` and `rhai`'s script `path`
  ([ADR-0228](../adr/0228-rhai-path-and-args-skill-tools-frontmatter-removed.md)) join a relative `path`;
  `glob`/`grep` (+ `_json`) join `path` or default it to the workdir;
  `bash` joins or defaults `workdir`; `call` also joins `input_file`/
  `output_file`. Inside the root the result is root-relative, outside it
  absolute — so ADR-0125 normalization, argument- and workdir-scoped rules,
  grants and containment see exactly what they would without a workdir, and
  containment is re-checked at every use (a workdir later swapped for a
  symlink out of the root trips the escape-root gate). `glob`/`grep` list
  matches relative to the workdir (absolute outside it) via the shared
  `SessionWorkdirs` view the executor fills from each `ToolExec`. The
  runtime's `workdir::resolver` (core's `workdir_resolver` seam) canonicalizes
  a `/cd` target, requires a directory inside the root, or — outside it — a
  durable `bash`+`call` grant in the `ExtraRootStore`, which a
  `Session`/`Always` approval on `SetWorkdir` records (`Once` is refused).
  That grant is process-wide (ADR-0109): any session, model-spawned
  children included, may then run `bash`/`call` there unasked. `call`'s
  `input_file`/`output_file` outside the root resolve under a durable `call`
  grant on the path or an ancestor (`call::output::resolve_call_file`) —
  except a sandboxed call's `output_file`, which the unsandboxed host process
  writes, so it keeps strict root containment under bubblewrap.
  File tools keep their per-path escape-root rules outside the root.
- **The trusted scratch dir is exempt from the escape-root prompt** (#524,
  [ADR-0142](../adr/0142-trusted-scratch-dir-and-plans-folder-carve-outs.md),
  amends ADR-0109): the runtime-owned per-project scratch dir
  (`SessionStore::scratch_dir`, already the default `call`-output target)
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
  included. The layers that actually bound an exec tool are the session's
  permission mode (whether/what it may run), the config ceiling (#172), and
  the opt-in bubblewrap sandbox below — not filesystem containment, which
  governs only the six path-arg tools.
- **OS sandbox, opt-in and now a mode fact (#399/#479,
  [ADR-0104](../adr/0104-bubblewrap-sandbox-for-bash-call.md), scoping
  superseded by
  [ADR-0207](../adr/0207-permission-modes-replace-agent-borne-authority.md)
  §6):**
  `ENTANGLEMENT_SANDBOX=bwrap` confines every `bash`/`call` spawn under
  bubblewrap by default — `--ro-bind / /` plus the project
  root re-bound read-write at the same path (so `resolve_under_root`'s
  containment above keeps working unmodified inside the sandbox), a fresh
  `/tmp`/`/dev`/`/proc`, and its own pid/ipc/uts/cgroup namespaces.
  `--unshare-net` cuts network by default; `ENTANGLEMENT_SANDBOX_NETWORK=1`
  shares the host network namespace back in. **Fail-closed by omission**: there
  is no fallback to unsandboxed execution when `bwrap` can't be entered (missing
  binary, unprivileged user namespaces disabled) — the spawn simply errors, like
  any missing binary (ADR-0016). Confinement is a **mode** fact now, not a
  per-agent one: each built-in mode carries `sandbox`/`sandbox_network` YAML
  keys, but none of the four ships `sandbox: bwrap` — declaring it in every
  built-in would make confinement mandatory instead of opt-in, which breaks
  ordinary work (a confined `git push` fails on `/etc/ssh/ssh_config.d/`
  ownership once the namespace changes what ssh sees). A mode is still free
  to declare `sandbox` (and `config.yml` `modes:` tuning can add it to a
  built-in), applying to the session's **whole spawn sub-tree** uniformly
  once it does — there is no more
  per-spawn override or ancestor-walk clamp to compute, since every session
  under one mode already gets the identical policy. `BashTool`/`CallTool`
  still hold a `sandbox_resolver: Arc<dyn policy::SandboxResolver>` rather
  than a fixed `SandboxPolicy`, but it now resolves a session's *mode* (via
  `Mode::sandbox_policy`) instead of its agent profile; `ENTANGLEMENT_SANDBOX`/
  `ENTANGLEMENT_SANDBOX_NETWORK` layer on top via `SandboxPolicy::most_confined`
  — env may only *tighten* what the mode declares, never loosen it, since
  `most_confined` always picks the more-confined of the two. The existing
  process-group timeout/cancel kill
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
- **Wiring (ADR-0010, amended by [ADR-0093](../adr/0093-call-registration-independent-of-bash-opt-in.md);
  bash posture superseded by [ADR-0195](../adr/0195-bash-is-the-default-exec-and-curated-read-only-rules.md)):**
  `host_tools(root)` registers the **root-contained sextet**
  (`read`/`glob`/`grep`/`edit`/`write`/`apply_patch`; `write` added in
  ADR-0031, `apply_patch` in #455). The
  `skutter` binary registers `CallTool` and **`BashTool` unconditionally**
  at startup — `call` because no shell means no injection surface (#386),
  `bash` because registration is not where the security story lives (the
  session's permission mode, ceiling, and sandbox are; ADR-0195 reverses
  ADR-0010's opt-in). The retired opt-in machinery —
  `ENTANGLEMENT_ENABLE_BASH`, the closed lazily-registrable-built-in table
  and its `/enable tool bash` registration fold, and
  `decline::disabled_builtin_decline` — is gone: there is no
  advertised-but-unregistered built-in left. An overlay **grade** entry
  naming `bash` is still meaningful (a pure grade override now that the
  tool is always registered) and a deny entry still withdraws it at
  dispatch, same as any tool. `bash` shares its long-lived `JobRegistry`
  with the `poll` tool (#605; one registry, never minted fresh —
  ADR-0163 §3). `EngineConfig::default()` ships an
  empty registry (embedders opt in via `host_tools`).
  **Curated read-only Allow rules (ADR-0195, relocated by ADR-0207)** ship
  as the shared read-only exec set spliced into the `research`/`plan`/`auto`
  modes (`mode/builtin/readonly_exec.yml` — `bash(pattern)` keys, which
  `call` shares since both exec tools use one rule set; `build` needs none
  of it, it class-allows `exec`) — see
  [agents & permissions](agents-and-permissions.md) §the shared read-only
  exec set for the list and its deliberate exclusions — so read-only
  commands stop costing an approval round-trip. The config ceiling still
  clamps least-privilege over every grade: a user ceiling of `bash: ask`
  (or `bash(find *): ask`) forces the prompt back over any curated Allow,
  exactly as it clamps a mode's own `Allow`. Graded per
  top-level segment of a compound command, not the whole raw string, since
  [ADR-0197](../adr/0197-compound-bash-commands-grade-per-segment.md). See
  [agents & permissions](agents-and-permissions.md) §permission ceiling.
  **Advertisement ([ADR-0226](../adr/0226-one-advertised-surface-mcp-envelope-registered-is-available.md)):** `call` (like `glob`/`grep`) is registered but
  **never advertised** — a script-only `rhai` binding, still graded under its
  own name; `bash` is in the kernel.
  The overlay grade lookup that finds the applicable entry
  (`permission::overlay_grade_entry`)
  walks the session's ancestor chain nearest-first (✅ #628), so a `bash()`
  binding run from inside a `rhai` script grades identically to a direct
  `bash` call — both run the same seam (ADR-0227) — and a
  spawned child with no overlay of its own inherits its parent's grade, not
  just the enable entry that lets it use the tool.

Every session is advertised the same tool set — there is no more per-agent
mask to narrow advertisement or dispatch with
([ADR-0207](../adr/0207-permission-modes-replace-agent-borne-authority.md),
superseding [ADR-0038](../adr/0038-physical-per-agent-tool-restriction.md);
see [agents & permissions](agents-and-permissions.md) §permission modes).
What differs now is the session's **mode**, not the agent, and it carves the
exec surface differently per mode: `build` mode auto-allows
`edit`/`write`/`apply_patch`/`bash`/`call` (`allow: [read, write, exec]`) bar
a short destructive-command deny list; `plan` mode denies the `write` class
outright except the plans-folder carve-out (ADR-0142), with exec ungraded
(falls through to `default: prompt`); `research` mode denies `write` and
`plan` outright while curated-allowing a read-only `bash`/`call` verb list
and prompting on everything else; `auto` mode allows by default, bar a
destructive and network-mutating deny list and a prompt list of
destructive deletions — [ADR-0228](../adr/0228-rhai-path-and-args-skill-tools-frontmatter-removed.md)
amends ADR-0209 here. Registration and mode grading are two
orthogonal axes: registration decides whether the tool can *execute* at all
(unconditional for both `call` and `bash`, ADR-0195), and the mode's rule
table decides `Allow`/`Ask`/`Deny` for a given call — so `bash` being
always-registered does not change what a non-`build` mode can do with it.

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
`poll` pages it back. (`propose_plan`'s own approval reply is a much smaller
`InMsg::SetMode` mode-switch acknowledgment now — no build child, no
result to bound —
[ADR-0207](../adr/0207-permission-modes-replace-agent-borne-authority.md)
§7 retires [ADR-0138](../adr/0138-sponsored-build-child-and-propose-plan-cycle.md)
wholesale; see [engine](engine.md) §plan acceptance.)

`agent_send { agent_id, prompt, background? }` (#609, ADR-0162) is a launcher
against an *existing* session instead of a fresh one: it sends
`InMsg::Prompt` to a child `agent`
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
is a `RetainedOutputRegistry` shared with `call` and `agent` (#614) the same
way `JobRegistry` is shared with `bash`/`call` — capped per-operation (256 KiB, keeping the
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

### `web_fetch` — SSRF-guarded page fetch — [ADR-0220](../adr/0220-web-fetch-ssrf-guard-and-clipboard-image-paste.md) (#736), [ADR-0225](../adr/0225-web-fetch-grades-redirect-hops.md) (redirect grading, #808)

`web_fetch { url, format?: markdown|text|raw, max_bytes?, offset? }`
(`host::web_fetch`, behind the default-on `web-fetch` feature) fetches one
http(s) URL. HTML becomes Markdown (`htmd`; script/style/head/svg dropped),
`text` also strips link/image targets, `raw` is verbatim. Output is paged at
the 32 KiB host-tool cap with an `offset=` pointer. A non-text body is never
inlined: `[binary omitted: mime, size, url]`. Registered at startup and part
of the advertised kernel ([ADR-0226](../adr/0226-one-advertised-surface-mcp-envelope-registered-is-available.md): with `describe` gone every model-facing host
tool must be).

**Grading.** It declares `Capability::Exec`: outbound network to a
model-chosen URL is graded like `curl`, not like a host `read`. The shipped
modes give `build` → allow and `research`/`plan`/`auto` → prompt. The
permission argument is the **normalized** URL (`web_fetch::normalize_url`:
WHATWG-parsed with the host's trailing dot stripped), the same string the
fetcher requests. So `modes: research: allow:
["web_fetch(https://docs.rs/*)"]` widens it for one site, and a deny cannot be
dodged with `HTTPS://`, a trailing dot, or `:443`. Write rules as a literal
`scheme://host/` prefix: rule globs let `*` match `/`, so a host wildcard
(`https://*.example.com/*`) is unsafe.

**Redirect hops are graded like the initial URL**
([ADR-0225](../adr/0225-web-fetch-grades-redirect-hops.md), #808). The
executor scopes the call's own grading context around every graded dispatch.
It is a tokio task-local, `tool_runner::call_grader`, and holds the same
resolver, ancestor chain, grants, overlay and ceiling as `dispatch`, sharing
one `grade::grade_call`. `web_fetch` grades each normalized hop through it
before following the hop (`WebFetcher::fetch_checked`):

- `Allow` follows the hop.
- `Deny` stops the fetch with an error naming the mode.
- `Ask` stops the fetch with a hint to call `web_fetch` on the target
  directly, which goes through ordinary approval. Nothing parks mid-fetch.

`tool_runner::grade_in_scope` is the public seam for any tool that reaches
targets beyond its own argument. Outside an executor no grader is in scope,
and hops are followed under the SSRF guard alone.

**Hostile HTML.** `htmd` recurses without a depth limit, and html5ever's
tree builder is quadratic in nesting depth. The conversion is guarded in
layers:

- **Tokenizer pre-scan.** html5ever's linear spec tokenizer runs first and
  builds no tree. An open-element stack estimates the nesting and collects
  the page text. A quoted `>` can't fool it. Inside `<svg>`/`<math>` it
  never switches to raw text, just as the tree builder doesn't. The scan is
  amortized O(1) per tag. A page above 256 levels, or with more than 50,000
  start tags, is answered with that text and is never parsed into a tree.
- **Real-tree check.** Otherwise the rcdom tree is built and its real depth
  is measured with an iterative walk. Above the limit, its text is extracted
  iteratively instead of handing the tree to `htmd`.
- **Isolation and limits.** Everything runs on its own 64 MiB-stack thread,
  never on a tokio worker. At most 2 conversions run at once, and each holds
  its slot until its thread really exits, even after a timeout. Waiting for
  a slot and converting share one 5 s budget. When that budget runs out, the
  tool falls back to the tokenizer text.

The code is in `host/web_fetch/html.rs`.

**SSRF policy** (`entanglement_provider::web_fetch`, re-exported as
`entanglement_core::web_fetch`):

- Only `http`/`https`. URLs with userinfo credentials are refused.
- Blocked: unspecified, `0/8`, loopback, RFC 1918, CGNAT `100.64/10`,
  link-local `169.254/16` (cloud metadata), `192.0.0/24`, documentation and
  benchmarking ranges, multicast, broadcast, `240/4`, `::`, `::1`, ULA
  `fc00::/7`, `fe80::/10`, `fec0::/10`, `2001:db8::/32` and Teredo. An
  IPv6 address that embeds an IPv4 one is judged by that IPv4 address. That
  covers IPv4-mapped, IPv4-translated (`::ffff:0:0:0/96`), IPv4-compatible,
  NAT64 (`64:ff9b::/96`), NAT64 local-use (`64:ff9b:1::/48`, where both the
  `/48` and `/96` embeddings are checked) and 6to4.
- An IP-literal host is checked after WHATWG parsing, so `2130706433`,
  `0x7f.1` and `[::ffff:127.0.0.1]` are all caught. `localhost` and
  `*.localhost` are refused by name.
- The client's own DNS resolver refuses the whole answer if any address is
  blocked. The addresses it vets are exactly the addresses the connector
  dials, so DNS rebinding has no second lookup to exploit.
- Redirects are followed manually, at most 5, and every hop is validated
  again.
- No proxy is ever used, and proxy environment variables are ignored: a
  proxy would resolve the name itself and bypass the guard.
- Timeouts are 10 s to connect, 30 s per request and 60 s for the whole
  chain. The byte cap defaults to 2 MiB and is clamped to 10 MiB.
- The client is built from `client_builder()` (shared TLS) but is not the
  pooled `HttpClient`, because the resolver is a client-wide setting. The
  endpoint pool would also create a persisted state entry for every site
  fetched.
- Tests reach `127.0.0.1` fixtures through a `#[cfg(test)]`-only guard
  field. No production build has a bypass.

### Undo / rewind — per-write file backups — [ADR-0212](../adr/0212-undo-rewind-via-per-write-file-backups.md) (#731)

`write`, `edit` and `apply_patch` — and therefore the `rhai` `write`/`edit`
bindings, which run those same registry tools — back up what they replace
when built `with_backups(FileBackups)` (`host::host_tools_with` + `HostToolOptions::backups`;
`skutter` always wires it, an embedder opts in). Only a session's run
(`run_for_session`) is recorded; a bare `run` is not.

- **Storage** (`file_backup.rs`): under the root session's
  `SessionStore::backups_dir` — `<store>/backups/<root_id>/` — a
  content-addressed `blobs/<sha256>` (write-then-rename, deduped per root) and
  an append-only `manifest.jsonl`: one `change` line per mutation
  `{id, ts, session, request_id, exec_seq?, path (absolute, resolved),
  display, before: absent | stored{hash,size} | skipped{reason}, after:
  sha256 | null, mode?}` plus `reverted{ids, attempted}` markers; the id
  counter and blob set are cached per root, so a write never re-reads the
  manifest. A spawned child's changes land in its root's directory
  (`FileBackups::track_roots` folds `SessionStarted` lineage, and `Status`
  for the busy check below).
  Backups ride the log's lifecycle: `SessionStore::delete` removes them with
  the `.jsonl`, and `prune` removes any backup directory whose log is gone —
  session retention is the only retention.
- **Caps**: a prior version over `max_file_bytes` (default 8 MiB), or past a
  root's `max_root_bytes` stored budget (default 256 MiB), is recorded as
  `skipped` (the change is still listed, so a revert names it as "can't
  restore … no backup (…)"). A backup failure is logged, never a tool error —
  the edit already happened.
- **Configuration** (#787): `config.yml` `undo: {enabled, max_file_bytes,
  max_root_bytes}` (`config/undo.rs` owns the size grammar: bytes or
  `B`/`KB`/`MB`/`GB`/`KiB`/`MiB`/`GiB`, 1 byte – 64 GiB, `0` rejected in
  favor of `enabled: false`) deserializes into
  `file_backup::BackupLimits`; `boot.rs` builds `FileBackups::with_backup_limits`
  from it, and the definitions watcher re-applies it on every config change
  via `FileBackups::set_limits` (a config that no longer parses keeps the
  previous limits). Limits are read per record, so a change applies to the
  next write; nothing already recorded is re-evaluated. `enabled: false`
  stops recording (no manifest line at all — a later revert treats such a
  file as "changed since"), while backups taken before stay restorable; a
  revert that finds nothing left to restore while disabled fails with
  "undo disabled" instead of an empty report. `skutter inspect config` shows
  the effective block and its winning layer.
- **Turns**: the executor runs each permission-graded/`rhai` call task under
  `file_backup::with_exec_seq(ToolExec.seq)`, so a change records the exact
  `(session, seq)` of its call and belongs to the turn of the session's last
  user `Prompt` logged before that record. Request ids are never the key
  (Gemini's `name#ordinal` repeats every turn). Without a seq (a detached
  `rhai` script): the latest `(session, request_id)` call logged no later
  than the change (`<id>:rhai:<tool>` → outer call), then timestamps.
- **Revert** (`FileBackups::revert`, behind core's `EngineConfig::file_reverter`
  seam — see [protocol](protocol.md) `RevertFiles`/`FilesReverted`): refused
  with an `Error` while any session of the sub-tree is mid-turn or running
  detached work (`file_backup::detached_work_probe` over the job and script
  registries, wired in `boot.rs`). Otherwise every unreverted change of the
  session and its spawn sub-tree after the target turn is grouped per file;
  the file goes back to the *first* change's `before` (rewritten through a
  synced, randomly named temp file created `create_new` + `O_NOFOLLOW` — no
  planted symlink is ever written through — then rename + directory fsync,
  keeping its mode; a file whose directory was since swapped for a symlink
  is refused as "its directory moved"; or deleted when the agent
  created it; recreated when the agent deleted it) **only if** it still
  hashes to the *last* change's `after` — or counts as restored if it is
  already at `before`. A user edit or a `bash` change is reported as skipped,
  left untouched, and marked `attempted` (still revertable, but undo steps
  past it); a change with no backup is reported once and marked `reverted`.
  A failed marker append is only logged. Out-of-root writes (an approved
  escape-root grant) are covered the same way — the manifest holds the
  resolved absolute path.
- **Not covered**: anything `bash`, `call` or a `rhai` `exec`/`bash` binding
  does to the filesystem, plan files written by `propose_plan`, and
  `!bash` passthrough commands. Every report and the model's note say so.

### One advertised surface and discovery — [ADR-0226](../adr/0226-one-advertised-surface-mcp-envelope-registered-is-available.md)

(Supersedes [ADR-0196](../adr/0196-tool-search-and-lazy-discovery-replace-the-invoke-envelope.md)
§1–§4 and [ADR-0204](../adr/0204-invoke-fallback-for-client-side-discovery.md).)
There is no advertising mode, per-wire encoding or discovery strategy. Every
session on every wire is advertised the compiled-in **`KERNEL`**
(`tool_names::KERNEL`): `read`/`edit`/`write`/`apply_patch`/`bash`/`web_fetch`,
`poll`/`ask_user`/`update_tasks`, `propose_plan`/`request_mode`,
`agent`/`agent_send`, `rhai`, the six discovery tools and the `mcp` envelope.
`tool_advertising::surface` builds it from the registered and runtime-owned
specs (deduped, name-sorted), **snapshots it at the session's first
resolution** (`AdvertisingState::surfaces`) and returns the same bytes every
later round — a tool registered or removed afterwards never moves the cached
prefix, and an overlay enable or MCP connect never touches it. End/hibernate
forgets the snapshot; a resume rebuilds the same bytes. Not advertised:
`call`/`glob`/`grep` (script-only `rhai` bindings, graded under their own
names, like `read_raw`/`glob_json`/`grep_json`) and every `mcp__*` tool.
ADR-0226's interim `endpoint__*`/`skill__*` additions are gone
([ADR-0227](../adr/0227-one-binding-path-namespaces-http-skill-libraries.md),
[ADR-0228](../adr/0228-rhai-path-and-args-skill-tools-frontmatter-removed.md)):
the surface is exactly `KERNEL`.

**The six discovery tools** — each with no argument or one `name`, all
`Capability::Control` and non-maskable — the `poll` pattern: a Control
tool is never graded ([ADR-0207](../adr/0207-permission-modes-replace-agent-borne-authority.md) §3;
`tool_names::DISCOVERY_TOOLS`, [ADR-0226](../adr/0226-one-advertised-surface-mcp-envelope-registered-is-available.md)): the dispatch ladder's Control path
returns before the mode, overlay or ceiling are consulted, so nothing can
withdraw or decline them. `list_agents`, `list_models` and `list_skills`
render static rosters (`discover::lists`); `load_skill` is the registry tool;
`list_mcp`/`mcp_tools` are the MCP pair (§10). The five non-registry ones
route through `Intercept::Discover`. Discovery is **pull-only**: the system
prompt carries no roster, only the static `system_prompt::TOOL_CONVENTIONS`
line (direct calls for the advertised tools, `mcp(server, tool, args)` for
MCP, `rhai` for anything else).

**The `mcp` envelope** — `mcp {server, tool, args}` (`discover::mcp_spec`;
the name `MCP_TOOL` is shared through the leaf crate) is the only way to call
an MCP tool. Core unwraps it to `mcp__<server>__<tool>` before any event and
keeps the emitted call in `Context` byte for byte ([engine](engine.md)), so
every gate, hook, approval and `arg_validate` sees the inner tool. A
`ToolExec` still named `mcp` (a malformed envelope core refused) is declined
where an unknown tool is, via `tool_advertising::unknown_tool_reply`, on the
ADR-0176 `is_error` channel with the envelope's shape and schema. Calling an
`mcp__*` name natively is not advertised but dispatches and grades the same.

### Error taxonomy — three cases — [ADR-0196](../adr/0196-tool-search-and-lazy-discovery-replace-the-invoke-envelope.md) §6

One wording family on the ADR-0176 `is_error` channel:

| case | reply |
| --- | --- |
| user denial | the denial reason only |
| schema violation (malformed call, missing/unexpected params) | reason + **the tool's schema** + one example — the fix is to the call's shape |
| parameter error (valid shape, bad value) | why it failed + context (a directory listing, a valid range) — the fix is to a value, not the shape |
| command failure (e.g. a shelled-out command exits nonzero) | the normal result, verbatim — **not** `is_error`; a nonzero exit is a legitimate result to read, not a call to fix (unchanged from ADR-0176/0186) |
| unknown tool name | fuzzy catalog matches (closest-Levenshtein-distance hint), not a schema — the intended tool is unknown, so there is nothing to show a schema for |

The mechanism is **pre-dispatch validation of the input against the tool's
advertised `ToolSpec` schema** (`entanglement-runtime::arg_validate`,
subsuming the MCP required-param pre-check, §10's proxy): today an arg-parse
failure is indistinguishable from a runtime failure at the executor boundary
— all host tools deserialize inside `run()` and return bare `anyhow` errors —
so the validation runs before dispatch and renders the schema decline from
the same spec the model was advertised. A schema declaring no shape (the
permissive `Tool::schema()` default — no `properties`/`required` at all)
validates nothing: a tool with nothing structured to say makes no promise
about the input's format, so even non-JSON/non-object input isn't a
violation for it — schema-less tools keep their anything-goes input
contract, and every real structured tool declares at least one of
`properties`/`required`. An MCP call through the `mcp` envelope is validated
the same way against the server's real schema, so a violation never reaches
the server; its decline shows the example as the `mcp {server, tool, args}`
call that reaches the tool ([ADR-0226](../adr/0226-one-advertised-surface-mcp-envelope-registered-is-available.md)). Two guards ride alongside: per-session
delivered-schema tracking (`tool_advertising::DeliveredSchemas`, marked by
every `mcp_tools` delivery and every decline — never re-send a schema already
in context; reply with the diff instead) and a two-identical-failures loop breaker (a repeated
identical failure means the schema isn't the problem — stop retrying the
same shape).

Parallel tool calls are unaffected by any of this: ADR-0061's batch
`ToolExec` parking already resolves a batch of calls in any order with every
call guaranteed a paired result, so there is no envelope-shaped concurrency
question to answer here.

### Definition-driven tool sources — removed

The #560 P8 sources are gone: `config.yml` `endpoints:` and the skill-tool
`endpoint` kind ([ADR-0227](../adr/0227-one-binding-path-namespaces-http-skill-libraries.md)),
then the whole `SKILL.md` `tools:` frontmatter — its preset-script `rhai`
and `alias` kinds, `AliasTool`, `Tool::alias_rewrite` and the ladder's
alias-rewrite step ([ADR-0228](../adr/0228-rhai-path-and-args-skill-tools-frontmatter-removed.md)).
A skill ships callable behavior as `lib.rhai` functions (§Script surface) and
predefined scripts as files run with `rhai` `path`. A stale `endpoints:` key
warns and is ignored; a native `SKILL.md` still declaring `tools:` is skipped
with an error-level log naming the replacement — the rest of the registry
(and the engine) still loads. A foreign skill dir's `tools:` is dropped like
any unknown key (ADR-0074).

### Script surface — namespaces, `http()`, skill libraries — [ADR-0227](../adr/0227-one-binding-path-namespaces-http-skill-libraries.md) (#819)

**The dispatch seam.** `tool_runner::dispatch` splits the old
`run_and_reply` into `Dispatcher::run` (grade + execute → `Outcome::Refused`
/ `Outcome::Ran` / `None` on Stop) and `Dispatcher::dispatch` (reply to
core). A model call takes both; a script binding takes only `run` and gets
its outcome over the bridge (`script::service`), so core never sees a nested
call. The ladder (`dispatch/gates.rs`): script reachability → resolve the
tool (scoped MCP overlay, lazy connect) → `Control` bypass →
overlay deny → grade + grants → `pre_tool_use` → mode deny → escape-root →
credentialed-`http` gate → run / unattended collapse / park
(`dispatch/decision.rs`) → execute (`dispatch/execute.rs`: state tools,
arg validation, in-call grader, `FileChange` capture, script budget, loop
breaker, `post_tool_use`).

**Namespaces** (`script::namespaces`), generated per run:
`mcp::<server>::<tool>(#{…})` for every tool of a connected server — and, for
an unscoped session, the last-seen tools of an unconnected one, which the
call connects lazily — graded and executed under the real
`mcp__<server>__<tool>` name; a scoped session lists only its scope's servers
and never the shared cache (ADR-0188). `skill::<skill>::<fn>(…)` for every
skill's `lib.rhai`. Identifier mapping (`rhai_ident`): non-identifier
characters → `_`, a result with no letter before its first digit (or no
letter at all) gets a `t_` prefix, a Rhai keyword/reserved word a `_` suffix
— validity per Rhai's own `is_valid_identifier`/`is_valid_function_name`; one
shared pass (`rhai_ident::plan_mcp`/`plan_skills`) drives registration and
the `mcp_tools`/`load_skill` listings, so a collision logs, skips the later
entry and lists it as `no rhai name (collides with <winner>)`.
`mcp_tools` renders each tool signature-first — `in rhai: mcp::…(#{…})`, the
`mcp {…}` call line, then the schema.

**`http(method, url, #{headers?, body?})`** — the registered, never-advertised
`http` tool (`host::http`, `Capability::Exec`). Permission argument
`"{METHOD} {url}"` (normalized as sent): `http(GET https://api.github.com/*)`
allows reads only, `http(* https://api.github.com/*)` every method. Rides
`web_fetch`'s SSRF-guarded client (`WebFetcher::request`); every redirect hop
is graded as a fresh `http` call (ADR-0225's `check_hop`), `Ask` stops it.
Returns the body; a non-2xx status is an error (thrown in the script).

**Hidden credentials — `config.yml` `http_auth:`** (`config::http_auth`,
`http_gate::HttpGate`): host → `headers:`/`cookies:` with `${VAR}` from the
environment (the managed `.env` lands there at startup). **User layer only** —
a project layer's is dropped before the merge with a warning. Matched on host
only (exact or dot-boundary suffix, longest key wins), injected per hop over
https to the original host only — a redirect elsewhere goes bare. A
script-supplied `Authorization`/`Cookie`/`Proxy-Authorization`/`Host`/
`X-Forwarded-Host` header or URL userinfo is refused; a map/array body is sent
as JSON with `Content-Type: application/json` unless the script set one;
response headers never reach the script; error text is scrubbed of every
expanded `${VAR}`, cookie value and whole header value (a token echoed in a
successful body is out of reach — accepted). Every referenced `${VAR}` is
scrubbed from every child's environment (`secret_env`, hidden at config load:
`bash`/`call`, MCP stdio, hooks, status line, editor/clipboard/browser), since
the managed `.env` exports it into this process. The managed `.env` file and
the parent's `/proc/<pid>/environ` remain readable by the exec tools — an
accepted risk shared with provider keys, tracked in #832. A credentialed call **forces one
approval per host per session** regardless of the mode grade (card:
`authenticated for <host>`; approving it also earns the script cache entry
and any wider grant); in an unattended mode it is refused unless an explicit,
argument-scoped `http(...)` rule allows it (`mode::rules::scoped_grade`),
provenance tracked per mode: a rule only a project layer declares grades but
never unlocks (load warning); the same rule in the user config does. `ToolExecutorBuilder::http_gate` registers
the `http` tool over the same gate the seam checks — one source. `http_auth`
is read at startup only (not live-reloaded); a key with a scheme, port, path
or wildcard warns at load. `web_fetch` never injects.

**Skill libraries** (`skills::lib_rhai`): a `lib.rhai` beside `SKILL.md`
(any layer, foreign dirs included) compiles once at discovery into an
encapsulated static module, registered as `skill::<skill>`; only `fn`
definitions — a top-level statement rejects the whole lib with a warning.
Live-reloaded with the skill registry; `load_skill` lists the public
signatures after the body, each with its `///`/`/** */` doc comment (read
by a text pass, `skills::lib_docs` — not Rhai's `metadata` feature, which
rejects any doc-style comment not followed by `fn`, [ADR-0228](../adr/0228-rhai-path-and-args-skill-tools-frontmatter-removed.md)).

**`rhai(path)` and `args`** ([ADR-0228](../adr/0228-rhai-path-and-args-skill-tools-frontmatter-removed.md),
`script::input`/`script::gate`): `path` names a script file — relative paths
are rewritten onto the session workdir first (ADR-0224) — that the runtime
reads off the async runtime (256 KiB cap on the bytes read) *before* the
card, shows on the launch card (headed `// path: "<quoted>"`/`// args:`)
and evaluates verbatim; the model never reads it. An inline script's card
opens with the runtime's own `// inline script` line instead, so script
text can never pass itself off as a path card (the TUI offers a path grant
only on one). The read canonicalizes
once, opens that canonical path (non-blocking, no final-symlink follow — a
FIFO is refused as "not a file" instead of hanging the launch), checks the
opened file's `(dev, ino)` against the canonical path's and reads from that
descriptor; on Linux the canonical path is then re-read from the descriptor
itself (`/proc/self/fd`), so an intermediate directory swapped for a symlink
between canonicalize and open cannot split the two (elsewhere that narrow
residual window remains). Trust is judged on that canonical path, so the
trust decision and the text come from one file. **Only a script is ever
run by `path`**: a `path` without a `.rhai` extension (any case) is declined
at validation, and a canonical target without one (a `x.rhai` symlink to
`x.txt`) is refused at the read — so trust never extends to a non-script
file in a trusted folder (`rhai_trust::is_script`, shared with the write
floor).

**The launch** (`script::gate`) is graded like any tool
([ADR-0228](../adr/0228-rhai-path-and-args-skill-tools-frontmatter-removed.md)):
`Dispatcher::grade` — the overlay deny (`/disable rhai` is an ordinary
deny), then the overlay enable entry or the mode's rules, the ceiling and
grants — then the `pre_tool_use` veto, all before the file is opened. The
rule/grant argument is the script's **canonical** `path`, root-relative
inside the root and absolute outside it (`permission_arg` +
`permission_path::rooted_arg`, which canonicalizes for `rhai` alone; the
gate rewrites the input's `path` to that form before grading, against the
escape-root policy's root or, with none wired, the launch's project root), so
`rhai(scripts/deploy/*)` matches and a link in `scripts/deploy/` grades as
where it points; the file read must be the file graded. An inline script
matches only a bare `rhai` rule. Then:

| grade / script | launch | nested calls |
| --- | --- | --- |
| Deny (mode, ceiling, overlay deny, hook veto) | refused, a catchable error — a trusted path too | — |
| `path` inside a trusted folder (`TrustedFolders::runs_trusted`) | no card | force-allowed (`Posture::Trusted`) |
| Allow (mode rule, overlay enable, grant) | no card | graded like direct calls (`Posture::AskPerCommand`) |
| Ask | one card: **Run all** · **Ask per command** · **Reject** (`InMsg::Approve.run_all`) + scope | Run all → force-allowed (`Posture::RunAll`); Ask per command → graded like direct calls |

A `Session`/`Always` approval of an `Ask` card records a grant for the
**exact path** (`rhai`, the canonical root-relative path), so the next
launch grades Allow — no card, Ask per command; an inline script's
approval is once only, and `run_all` is never recorded. Built-in grades:
`build` asks (`prompt: [rhai]` — a rule naming the tool outranks the class
allows, [agents & permissions](agents-and-permissions.md)), `research`/`plan`
deny (class `write` deny), `auto` allows. **No answerer**: an `Ask` card
waits for the head unless the mode has `question_timeout`; on that timeout
it runs Ask per command (an out-of-root launch is refused instead).
`skutter run` answers a launch card at once with Ask per command (refusing an
out-of-root one, `rhai_trust::LAUNCH_ESCAPE_NOTE`) and rejects every nested
card.

*Force-allowed* is a branch of the shared seam (`Dispatcher::clear` with a
`run_all` script): the overlay deny, the hook veto and a mode/ceiling
`Deny` still refuse (a catchable exception), and every later gate passes — a
graded `Ask`, the escape-root gate (a single-use token the host tool
spends), the credentialed-`http` per-host approval — except a **floored
write** under Run all, which gets its own nested card (below).
`rhai.trusted` (`config::rhai_trusted` → `rhai_trust::TrustedFolders`) is
read from the user **and** project layers — the lists add up, project
entries relative to the root, user entries to the user config's folder (`~`
expands) — and canonicalized at load, so a symlink inside a trusted folder
pointing out is judged by its target. A **user-layer** folder is not
trusted in a mode the project config gives `prompt:` rules — in every mode
when the project's `permissions:` ceiling has a `prompt` list or
`default: prompt`. The whole script is then graded and carded like any
other. Project-layer trust always applies, and a folder both layers list is the project's. Read at
startup only. This deliberately lets a repository unlock credentialed
`http()` for its trusted scripts (amends ADR-0227 §4). A non-trusted `path`
outside the root shows the escape-root warning on the same launch card (no
second prompt; an Allow grade then asks too). With no escape-root policy
wired, **every** out-of-root `path` is refused outright — a trusted one
too. A trusted / Run-all script's in-call grader (`CallGrader::run_all`)
passes an `Ask`-graded `http()` redirect hop the way the call itself
passed; a `Deny` hop still stops it. `skutter inspect config` lists each
trusted folder with the layer that listed it, and the modes where the
user layer's trust is suppressed.

**Trusted folders must not be agent-writable.** A script inside one runs
force-allowed, and a skill's `SKILL.md`/`lib.rhai` is loaded by every later
session, so writing one is as good as running it: a `write`/`edit`/
`apply_patch` whose target is a `*.rhai` file inside a trusted folder, or a
`SKILL.md`/`lib.rhai` inside any user or project skill layer dir
(`skills::skill_dirs` — `.entanglement/skills`, `.claude/skills`,
`.agents/skills`, the user dirs or `ENTANGLEMENT_SKILLS_DIR`; wired by
`ToolExecutorBuilder::skill_dirs`) — canonical comparison, a missing file
by its nearest existing ancestor, a symlink by where the write lands — is
graded at least `Ask`: never auto-allowed by a mode rule, a grant or an
overlay enable, and no wider grant is recorded for it; an explicit `Deny`
stays `Deny`; unattended, the `Ask` collapses per the mode
(`rhai_trust::floored_write` in `grade_call`). The floor holds inside a
**Run-all** script (its own nested card) but not for a **trusted** script's
own writes, and it applies in every mode, `auto` included (unattended it
collapses to a refusal, then parks up to `question_timeout` on a repeat). Other files in the folder grade normally — safe because a
non-`.rhai` file can never run by `path`. When the escape-root card is what
parks a floored write, its reason names the floor too. `bash` (and a
`call`/`exec` command) cannot be parsed reliably, and an **MCP filesystem
server**'s writes are not host-tool writes, so neither is covered: a
trusted folder the agent can reach that way is effectively an auto-allow
for anything it writes there. Keep trusted folders outside what the agent's
mode lets `bash` (or an MCP server) write — a project-layer entry may name
any folder, so this
is the repository owner's call. `args` is pushed into the scope as a constant (the source is
compiled without the scope, so Rhai cannot constant-fold it into a panic on
assignment). `script` XOR `path`, the field types and unknown keys are
validated before the launch. A background run detaches after the launch and
the read; its listing label is `rhai: <path>`. Separately, for every tool, an
escape card that also cleared a graded `Ask` records both grants on a
`Session`/`Always` approval.

## 9. Lifecycle hooks — [ADR-0066](../adr/0066-lifecycle-hooks-as-runtime-interceptors.md) (#199)

User-configured external commands run around tool execution and on prompt
ingress, for policy, telemetry, and formatting side-effects. Hooks are a
**runtime interceptor** (`entanglement-runtime::hooks`), not a core concept:
core neither knows nor cares that a command runs before a tool. They hang off
seams the runtime already owns — the `tool_runner` dispatch of a `ToolExec`,
the inbound `InMsg::Prompt` fan-out, and (#734) the `OutEvent` stream — so no
new protocol surface is added.

| point | fires | can block? | payload |
| --- | --- | --- | --- |
| `pre_tool_use` | top of the generic `dispatch` (`Intercept::Permission`), **before** the `Allow`/`Ask`/`Deny` decision | **yes** — a non-zero exit vetoes: the tool neither prompts nor runs, and the hook's output becomes the `ToolResult` | `{event, session, tool, input}` |
| `post_tool_use` | in `run_and_reply` after the tool result, before it folds back | no — observational (exit code logged, never fed to the model); it cannot rewrite the result | `{event, session, tool, input, output, is_error, exit_code}` — `is_error` (#636, ADR-0176) is the same structured classification that rides `OutEvent::ToolOutput`, so a hook can branch on outcome without re-parsing `output`'s text; `exit_code` (#681, ADR-0186) is the observed process exit status for `bash`/`call` (JSON `null` for every other tool and for a killed process), so a hook can branch on a specific status without string-matching `[exit N]` |
| `user_prompt_submit` | when an `InMsg::Prompt` reaches the engine (the executor's inbound `Stop` watcher) | no — observational | `{event, session, prompt}` |
| `session_start` | on `OutEvent::SessionStarted` — every session, root or child, incl. a compaction successor and each session a resume re-announces | no | `{event, session, source, parent, predecessor, agent, model, root}` — `source` is `resume` when an `InMsg::Resume`'s records name the id, else `compact` when it has a `predecessor`, else `fresh` |
| `session_end` | on `OutEvent::SessionEnded` / `SessionHibernated` | no | `{event, session, reason, parent, agent}` — `reason`: `hibernated`, `compacted` (the source after its `Compacted`), else `closed` |
| `stop` | a **root** session's turn finished: its `Done`, classified by the `Status` core always emits right after it | no | `{event, session, reason, error, parent, agent}` — `reason`: `completed` \| `error` (`error` carries the turn's last `Error` message). A `Stop`-cancelled turn emits no `Done`, so it fires nothing; neither does a manual `/compact`'s closing `Done` |
| `subagent_stop` | the same, for a session spawned with a `parent` (instead of `stop`) | no | as `stop`; `parent` is the spawning session, `agent` the child's profile |
| `pre_compact` | on `OutEvent::Compacted` — the source's last content event, before the successor starts and the source retires | no | `{event, session, kind, kept, summary}` — `kind`: `manual` \| `auto` \| `prune` |
| `notification` | an approval parked (`OutEvent::ToolRequest`) or an `ask_user` question parked (`OutEvent::UserQuestion`) | no | approval: `{event, session, kind: "approval", request_id, tool, input, message}` (+ `ENTANGLEMENT_TOOL_NAME`); question: `{event, session, kind: "question", request_id, questions, message}` |

**Why the six #734 points are observe-only.** They are derived by a runtime
subscriber from events core has already acted on, so there is nothing left to
veto without a new core seam — and core stays policy-free (ADR-0066). That
holds even for `pre_compact`, the one that sounds like a gate: since
[ADR-0205](../adr/0205-every-compaction-forks-a-successor-session.md) every
compaction forks and never rewrites the source log, so a hook has no history
to rescue first, and an auto-compaction must never be held back into a
context overflow anyway. Blocking stays `pre_tool_use`'s alone, exactly
ADR-0066's model.

- **Config:** the `hooks:` section of the layered user config (§ADR-0047/#172).
  `Config.hooks: Hooks` is nine `Vec<HookSpec>` (one per point) deep-merged and
  `deny_unknown_fields`-validated by the same loader as `permissions`. A
  `HookSpec` is `{command, tools?, timeout_secs?}`; `tools` is an optional
  name-filter for the tool hooks (empty ⇒ every tool), ignored by every
  other point. Empty section ⇒ no hooks (the norm).
- **Execution:** each hook is an `sh -c <command>` child fed the JSON payload on
  stdin and given `ENTANGLEMENT_HOOK_EVENT` / `ENTANGLEMENT_SESSION_ID` /
  `ENTANGLEMENT_TOOL_NAME` (tool hooks and an approval `notification`) env vars. It runs under `timeout_secs`
  (default 30) in its **own process group**, reusing the exec tools' containment
  (`host::exec`, §8/#168) so a hook that spawns children can't orphan them. A
  timeout or a spawn failure counts as a **failure**, so a `pre_tool_use` hook
  that can't launch **fails closed** (vetoes the tool) rather than letting it
  through.
- **Scope:** the tool hooks cover only the generic host-tool dispatch route. The orchestration tools
  (`agent`/`ask_user`/`propose_plan`, which touch no host resource) and the
  self-permissioning `rhai` tool bypass hooks — matching the issue's "around
  `tool_runner::dispatch`" scope.
- **Wiring:** `skutter` passes its hooks via `ToolExecutor::builder(..).hooks(..)`
  (`services.rs`, #724 — the builder replaced the positional
  `spawn_tool_executor*` constructors, kept deprecated for 0.7 only; every
  unset seam takes a documented default, table in `docs/embedding.md` §4).
  `ToolExecutorBuilder::spawn` subscribes to the inbound fan-out
  synchronously before the executor task spawns so a first `Prompt` can't race the
  `user_prompt_submit` watcher. When any #734 point is configured it likewise
  subscribes the outbound stream and a second inbound receiver up front for
  `hooks::lifecycle::spawn_watch`: a pure `Tracker` folds events into firings
  (draining buffered inbound frames first — the supervisor fans a `Resume` out
  before handling it, so it is always seen ahead of the `SessionStarted`s it
  causes), and one sequential worker runs them, so hooks fire in event order
  while a slow hook never lags the subscription. Firings are best-effort under a
  broadcast lag: dropped events fire nothing, and a pending turn end is
  forgotten rather than misclassified. Engine-internal LLM calls
  (aux models: `summarize`/`session_title`/`narrate`) open no session and so
  fire nothing; a spawned child is an ordinary session and does.

### Pluggable policy seams — `PermissionResolver` + `GrantStore` — [ADR-0079](../adr/0079-pluggable-permission-resolver-and-grant-store.md) (#311)

The executor hard-codes *no* policy source. `ToolExecutor::builder(..)
.resolver(Arc<dyn PermissionResolver>).grants(Arc<dyn GrantStore>)` (traits in
`entanglement-runtime::policy`) drives two trait objects, so a multi-tenant
embedder that stores rules per user in its own DB swaps both without forking the
~350-line executor — keeping the shared interception ladder, spawn bounds,
hooks, rhai, and plan/tasks tools.

- **`PermissionResolver::resolve(session, tool, input) → Permission`** decides one
  session's `Allow|Ask|Deny` grade (async — a real embedder hits a DB, and the
  ladder already runs in a detached task). It runs **where the mode/base
  resolution ran before**, but the sub-agent ancestor clamp (ADR-0024) and
  spawn bounds stay in the ladder **on top of** it: the executor snapshots
  the call's ancestor chain (`permission::ancestor_chain`) in the loop and takes
  the least-privileged resolver grade across it (`resolve_effective`), so a tenant
  rule can never widen a child beyond its parent. `apply_grant` then upgrades a
  resolved `Ask` to `Allow` from a `GrantStore` grant.
- **`GrantStore`** persists + reads "always allow" grants (§ agents-and-permissions
  #174). `record(session, tool, arg, scope)` is async so an `ApprovalScope::Always`
  can hit a DB; `is_granted` is a sync fast check. A multi-tenant store writes its
  "always" rule to the DB and resolves later reads through its own resolver, so its
  `is_granted` can return `false`.
- **Defaults (byte-identical CLI):** `ModeResolver` grades from the session's
  permission **mode** (ADR-0207 stage 4), not its `Agent` — it reads the
  `Arc<Mutex<session→mode map>>` the executor folds `OutEvent::ModeChanged`
  into, resolves the tool's declared `Capability` against the mode's rules, and
  clamps the result to the config ceiling internally, so `resolve_effective`'s
  outer chain-fold needs no separate `clamp_to_base` call. `DefaultGrantStore`
  wraps the managed file store (`grants::FileGrantStore`). Every `rhai`
  binding call grades through this *same* resolver + ancestor chain — it runs
  the executor's own dispatch seam (ADR-0227).

### Dynamic `ToolRegistry` — `SharedRegistry` — [ADR-0096](../adr/0096-dynamic-toolregistry-sharedregistry.md) (#372)

`ToolExecutor::builder`'s required `tools` argument is a `SharedRegistry`
(`Arc<std::sync::RwLock<ToolRegistry>>`, `ToolRegistry::shared()`), not an owned
`ToolRegistry` — the seam a live MCP add/remove (#4) needs to mutate the
dispatch table without restarting the engine. An owned registry converts with
`.shared()` (mirroring `.agents(catalog)` vs `.live_agents(shared)` for the
agent catalog). The builder's grant store defaults to **in-memory** (#747/#775 —
never reading or writing the managed `grants.yml`, so test callers can't touch
the developer's real file); a production caller wanting persisted `Always`
grants sets `.grants(Arc::new(DefaultGrantStore::load()))`, as `services.rs`
does.
`ToolRegistry` itself gains `unregister`/`unregister_prefix`/`contains`/
`names` alongside `register`.

Each `ToolExec` dispatch takes a brief **synchronous** read lock and clones an
owned snapshot before spawning its detached task — `std::sync::RwLock`, not
`tokio::sync`, because `EngineConfig.tool_spec_resolver`
([ADR-0076](../adr/0076-per-session-dynamic-tool-specs.md)) is a plain sync
`Fn` consulted on the turn's hot path and must never block on I/O; a
`tokio::sync::RwLock` would force that closure into `blocking_read` or break
0076's no-async contract. `skutter` (`boot.rs`) wires the resolver to read through the
same `SharedRegistry` handle it hands the executor, reproducing
`cfg.tool_specs`' composition: registry tools plus the runtime-intercepted
pseudo-tool specs that aren't `ToolRegistry` entries
(`update_tasks`/`ask_user`/`poll`/`bash`/`rhai`), sorted by name and deduped.
The resolver projects the one **`KERNEL`** surface out of that roster and
snapshots it per session ([ADR-0226](../adr/0226-one-advertised-surface-mcp-envelope-registered-is-available.md), §One advertised surface above) — dynamic
tools (MCP, script-only built-ins) stay registered and dispatchable, just
absent from the advertised array.
Because core advertises the resolver's output verbatim now — there is no
more per-agent mask, and the session's permission mode + overlay enforce
only at dispatch — **this resolver is the only thing
that shapes a session's surface**: a spec missing here is a spec no model ever
sees. That is exactly the shape of ADR-0190's Bug 1, where `poll_spec()` was
absent from the roster and vanished from every real head while tests, which run
off the static `tool_specs` fallback, stayed green.

## 10. MCP client — external tool servers — [ADR-0067](../adr/0067-mcp-client-as-runtime-tool-provider.md) (#198)

Attach any external [MCP](https://modelcontextprotocol.io) tool server as a
**runtime-side tool provider**, with **no core change**. Since the `Tool` trait +
`ToolRegistry` live in the runtime (§ADR-0059), an external tool is the same shape
as a host tool: a `dyn Tool` with a name, description, and `inputSchema`. The MCP
client (`entanglement-runtime::mcp`) connects each server on first use,
discovers its tools, and registers them into the same registry — so they ride
the `ToolExec`/`ToolResult` round-trip (execution) unchanged, graded by the
same permission modes as `read`/`bash`. Their schemas are **never advertised**
([ADR-0226](../adr/0226-one-advertised-surface-mcp-envelope-registered-is-available.md)): the model lists servers with `list_mcp`, reads one server's catalog with
`mcp_tools`, and calls through the `mcp(server, tool, args)` envelope.

- **Transport (`mcp::client::McpClient`):** an enum over two concrete transports,
  chosen per server by the `command` XOR `url` config (§ADR-0080/#312). `McpTool`
  holds an `Arc<McpClient>` and only calls `list_tools`/`call_tool`, so it adapts
  whichever backs a server. Both share the handshake (`initialize` +
  `notifications/initialized`) then `tools/list` (discovery) / `tools/call`
  (execution) and the JSON-RPC result/error split (`client::jsonrpc_payload`).
  Discovery is **paginated** (#727, `list_tools_paginated`): `nextCursor` is
  followed until absent/empty, capped at 100 pages so a server whose cursor
  loops fails the connect with a clear error instead of hanging it. A
  **server-initiated request** is answered rather than dropped
  (`server_request_reply`): `ping` gets the spec's empty result, anything else
  `-32601`; and a message carrying a `method` is never taken for a response,
  since each side numbers its ids independently and a server `ping` may reuse
  an id the client has in flight. No client-side keepalive ping is sent.
  stdio keeps a flat **60 s** per-request timeout so a hung subprocess can't
  park a turn; HTTP's per-request bound is now the shared endpoint pool's own
  (below, [ADR-0157](../adr/0157-mcp-http-transport-shares-the-endpoint-pool.md)).
  - **stdio (`mcp::stdio::StdioClient`, #198):** one JSON-RPC 2.0 session over the
    spawned subprocess's stdio, newline-delimited frames. A background reader task
    demultiplexes responses to callers by JSON-RPC `id`; notifications are dropped,
    a server request (`ping`) is answered on stdin from its own task (so a full
    stdin pipe can never stall the stdout drain), and on EOF the reader **drains all pending requests with an error** so a crashed
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
    stream (drained until the event whose JSON-RPC `id` matches; a `ping`
    request met on that stream is answered with its own `POST` that
    deliberately bypasses the endpoint pool — the drain still holds the
    endpoint's concurrency permit, so a pooled reply could deadlock under a cap
    of 1 — best-effort, 10 s bound, failures only logged). Static per-server
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
    core's re-export, the same path `ProviderMcpServer` takes — mechanism in the leaf
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
    lazy connect; a live `/mcp add` or an OAuth reconnect passes
    `api_key: None` (still pooled, just unkeyed).
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
    sibling managed files in the managed-state dir, `ENTANGLEMENT_STATE_DIR`) **together
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
  `mcp__*` namespace is entirely scope-owned — `list_mcp`/`mcp_tools` resolve
  through the scope (`McpScopes::rows`, `overlay_registry_for_call`; a scoped
  session never sees a global server and never reads the shared last-seen
  cache, [ADR-0226](../adr/0226-one-advertised-surface-mcp-envelope-registered-is-available.md)) and global MCP tools are stripped from every dispatch snapshot
  (`overlay_registry_for_call`, applied in the executor's detached task before
  `dispatch`; the `rhai` arm sees cached scope tools only, no lazy connect) —
  which is what keeps same-named servers (user A's `kb`, user B's `kb`, the
  global `kb`) unambiguous behind unchanged `mcp__<server>__<tool>` names.
  Connections are lazy, cached per `(scope key, server)` with the #556
  double-checked guard, and live for the process lifetime; eviction is the
  embedder's explicit `evict_scope` (logout, config drift under an unchanged
  key). `prewarm(&session)` between `Spawn` and the first prompt connects the
  scope's servers so `list_mcp` already shows their tools (optional —
  `mcp_tools` and dispatch connect lazily anyway).
  An `oauth:` server with no stored credential in the scope's slice fails as a
  clean auth-required *tool error* before any connect — the embedder's cue to
  run its web-OAuth flow. A null resolver (and every in-tree head: skutter
  passes `None`) is byte-identical to the global single-user behavior.
- **Proxy (`mcp::tool::McpTool`):** adapts one remote tool. `schema()` returns the
  server's `inputSchema` verbatim; `run()` JSON-decodes the model's input to the
  `arguments` object, checks it against the schema's top-level `required` array
  before ever contacting the server — a missing field bails with `` tool `name`
  requires parameter `field` `` instead of a cryptic server-side JSON-RPC error
  (#594) — then calls `tools/call` and maps the result (`mcp::result`, #727):
  text blocks fold into a text part (32 KiB-capped like every host tool), a
  `resource`'s text renders under a `[resource <uri> (<mime>)]` header and a
  `resource_link` as a `[resource link: name <uri> (mime) — description]`
  line; `structuredContent` is rendered as JSON only when no text block
  carried the answer.
  **Binary is never inlined**: `image`, `audio` and a `resource` `blob` all
  become `[binary omitted: <mime>, <size>, uri=<uri>]` (uri only when the
  block has one; size computed from the base64 length without decoding,
  `size unknown` for malformed base64) —
  never an image `ContentPart`. WHY: binary through MCP bloats the context,
  and a part the provider rejects is replayed on every later request (and on
  resume), poisoning the session; an agent that needs the bytes fetches them
  through an endpoint/API tool instead. `isError: true` sets
  `ToolRun::is_error`, so the call
  reaches `OutEvent::ToolOutput`/hooks with the structured `is_error` flag
  ([ADR-0176](../adr/0176-structured-tool-result-is-error-and-duration-fields.md));
  the `MCP tool reported an error:` text prefix stays, since no provider wire
  carries `is_error` to the model.
  Registered (dispatch) name **`mcp__<server>__<tool>`** — never advertised
  ([ADR-0226](../adr/0226-one-advertised-surface-mcp-envelope-registered-is-available.md)) —
  with every character outside `[A-Za-z0-9_-]` mapped to `_` by the one
  shared rule (`mcp_tool_name`) that both core's `mcp` unwrap and the
  runtime's registration use, so `my.docs` registers as `mcp__my_docs__…`.
  A tool's owning server is resolved by the longest matching sanitized
  `mcp__<server>__` prefix (`mcp::tool::owning_server`), never a split on
  `__`; mode rules and the `/mcp` panel's `d` deny build their
  `mcp__<server>__*` pattern from the same spelling. Two server names that
  sanitize identically (`my.docs` / `my_docs`) or whose prefixes nest (`a` /
  `a__b`) would make tool names and such patterns ambiguous, so the roster
  keeps only the first — bundles in catalog order, then user entries, each by
  name; only a server that can connect (not disabled, key resolved) claims a
  name — reports the ignored one as a startup config warning, and refuses a
  clashing live `/mcp add` with an `McpChanged` carrying `error`. A per-user
  scope applies the same rule to its own servers (first by name wins, the
  later is skipped with a warning).
  Governed by the same session permission mode as any host tool — there is
  no more agent tool mask to opt an MCP tool into or out of
  ([ADR-0207](../adr/0207-permission-modes-replace-agent-borne-authority.md)
  retired the `tools:`/`disallowed_tools:` wildcard pattern this row used to
  describe, ADR-0148 with it); a mode rule names one MCP tool exactly
  (`mcp__<server>__<tool>`, optionally scoped) or uses the `*` wildcard —
  mode and ceiling rules have no server-wide `mcp__<server>__*` glob (only
  the session tool overlay matches name globs) — but there is nothing to *admit* separately
  from grading it — every registered tool is reachable by name, mode-graded
  the same as everything else. Today `McpTool::capabilities()` pins every
  MCP tool to `Capability::Write` unconditionally (a deliberate fail-safe —
  an external server is opaque, it could do anything), so a mode class-denying
  `write` (`research`) prompts/denies an MCP tool by its `default`/`exec`-class
  rules unless a mode rule names it explicitly — the per-server
  `capabilities:` hint below does **not** yet feed the new `Capability`
  system (a gap left by ADR-0207; see the `capabilities:` paragraph below for
  what it still does).
- **Config:** the `mcp:` section of the layered user config (§ADR-0047/#172), a map
  of server name → `McpServerConfig`. A block is one transport XOR the other —
  `{command, args, env}` (stdio) **or** `{url, headers, oauth}` (HTTP; `oauth` is
  the ADR-0153 block above), plus a shared
  `disabled` (the one opt-out) and `description` (shown by `list_mcp`; a
  retired three-state `state:` is stripped with a warning, [ADR-0226](../adr/0226-one-advertised-surface-mcp-envelope-registered-is-available.md)) — resolved by
  `McpServerConfig::transport()`, which rejects both-set or
  neither-set. `deny_unknown_fields`-validated by the same loader as
  `permissions`/`hooks`. Empty ⇒ no servers (the norm). `skutter inspect config`
  lists the configured servers and their resolved transport. An optional
  `capabilities: {tool: read|write|call}` map (raw tool name, #426,
  [ADR-0117](../adr/0117-mcp-tool-capability-fan-out.md)) hand-annotates a
  server's tools — this predates
  [ADR-0207](../adr/0207-permission-modes-replace-agent-borne-authority.md)'s
  `Capability` system and is **not** wired into it (`McpTool::capabilities()`
  is hardcoded `Write`, above); the one place it still feeds is the
  **`config.yml` `permissions:` ceiling's own capability-key expansion**
  (the pre-ADR-0207 `agents::permission_from_value`/`expand_capabilities`
  pipeline, which the ceiling alone still runs before its result is
  reinterpreted as a `Mode` — `mcp::capability_index` folds this hint into an
  `McpCapabilityIndex` keyed by capability name, resolved from config alone
  (no live connection needed) and consumed by `agents::expand_capabilities`
  so a bare `read: allow` in the ceiling reaches a hinted MCP tool). A
  catalog-bundled server (below) never joins `user_config.mcp`, so its own
  `capabilities:` block (e.g. z.ai's `web_search_prime` →
  `{webSearchPrime: read}`) is invisible to `capability_index` alone;
  `mcp::capability_index_with_catalog` is what `skutter` actually calls (`boot.rs`) —
  `capability_index` plus every bundled server's hint, merged with any
  same-name user override the same way `AvailableMcp::from_config` merges one.
- **Wiring:** `build_config` builds the roster (`AvailableMcp::from_config`)
  and an empty `ActiveServers`; **nothing connects at startup** ([ADR-0226](../adr/0226-one-advertised-surface-mcp-envelope-registered-is-available.md),
  retiring [ADR-0169](../adr/0169-startup-mcp-connect-is-concurrent-and-fast-fail.md)'s
  concurrent startup connect). The first `mcp_tools` read or `mcp(...)` call
  on a server connects it for the process (`available::ensure_connected`,
  per-server guard, 60 s timeout); a failure is a tool error, never a startup
  failure. The stdio path lives in the **lean library**; the HTTP path rides
  the `mcp-http` feature, so an embedder gets stdio tool servers with no
  CLI/TUI/transport dependency and opts into HTTP by enabling the feature.

### Live add/remove/list — [ADR-0096](../adr/0096-dynamic-toolregistry-sharedregistry.md) + [ADR-0097](../adr/0097-live-mcp-server-management.md) (#372, #375)

The tool registry is not frozen:
`entanglement_runtime::SharedRegistry` (`Arc<std::sync::RwLock<ToolRegistry>>`)
replaces the owned `ToolRegistry` as the executor's tool input
(`ToolExecutor::builder(holly, tools)`), and `EngineConfig.tool_spec_resolver` (§ADR-0076) is wired to
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
  is no separate teardown call. A live add/remove also updates the roster
  (`AvailableMcp::insert_user`/`remove`), so registered = available holds with
  no restart. The `McpList` snapshot (`mcp::roster`) lists every registered
  server, connected or not, with its OAuth posture; `McpServerStatus.state` is
  retired and always `None` (serde-defaulted for old logs).
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

### Registered = available — [ADR-0226](../adr/0226-one-advertised-surface-mcp-envelope-registered-is-available.md) (bundled servers: [ADR-0152](../adr/0152-provider-bundled-mcp-servers-three-state-enablement.md), #542)

Provider-bundled MCP servers are **catalog data**: `ProviderEntry.mcp_servers`
(name → `ProviderMcpServer` — transport, `${VAR}` headers, #426 capability
hint, `description`, `disabled`) ships z.ai's `web_search_prime`/
`web_reader`/`zread` in the embedded `defaults.yml`, authenticated by the
provider's own `key_env` (`Bearer ${ZAI_API_KEY}`), every tool hinted
`read`. A same-name user `mcp:` entry overrides field-wise
(`available::merge_user_over_bundled`); bundled servers never enter
`ServerConfigs`/`save_mcp` — they are not persistable state, and a
`McpRemove` of one merely disconnects it (it stays available).

ADR-0152's three states are gone. `AvailableMcp::from_config` builds one
roster: every bundled server whose provider key resolves (read **live**, so a
`/key` save unlocks a bundle with no restart; keyless ⇒ silently absent) and
every configured server, minus those with **`disabled: true`** — the single
explicit opt-out, kept in `disabled_names` so a call to one is declined
truthfully rather than as unknown. There is no per-session enablement, no
`mcp_enable` tool, no `/enable mcp`/`/disable mcp`, no ancestry fold: an
available server is available to every session, and every call to one of
its tools is graded by the session's mode (default `prompt`, so the user
audits it). A retired `state:` key is stripped with one warning.

**`list_mcp`** (`discover::mcp_lists`) never connects: one row per server
(`mcp::roster` — name, description, transport, connected, tools, OAuth
posture) with state `connected` / `available` / `auth required` (an OAuth
server with no stored credential, with the `/mcp connect <name>` hint), and
tool names — live for a connected server, else the **last-seen cache**
(`mcp::McpToolCache`, the managed `<state dir>/mcp-tools.yml`, keyed by
server name **and** transport target (`McpServerConfig::target()`: URL or
command line) so a repointed name never shows the old endpoint's list).
Every successful global connect — lazy, `/mcp add`, `/mcp connect` — records
it (`record_last_seen`); a write failure is logged, never surfaced. The
definitions watcher ignores that file.

**`mcp_tools(name)`** is a read of one server's catalog — every tool with its
JSON schema and a `call as: mcp {"server": …, "tool": …, "args": <minimal
example>}` line — the MCP counterpart of `load_skill`. MCP has no offline
catalog, so the first read of a server in the process connects it
(`available::try_lazy_connect`); every listed tool is marked in the session's
`DeliveredSchemas`. Both tools are `Capability::Control`: reading a catalog
grants nothing.

A bundled server's lazy connect resolves `AvailableServer.key_env` live and
hands the value to the HTTP transport as `api_key` (✅ #559,
[ADR-0157](../adr/0157-mcp-http-transport-shares-the-endpoint-pool.md)), so
its traffic shares the endpoint pool's rate-limit budget with the provider's
LLM endpoint using the same key — see §"MCP client" above.

### Dispatch-time lazy connect — [ADR-0201](../adr/0201-unregistered-mcp-tool-dispatch-resolves-truthfully.md), amended by [ADR-0226](../adr/0226-one-advertised-surface-mcp-envelope-registered-is-available.md)

An unknown-tool dispatch for an `mcp__<server>__*` name — the normal case for
the first `mcp(...)` on a server, since nothing connects at startup, and for
a resumed session whose registry is process-lifetime — is not a flat
registry-membership test. `AvailableMcp::tier_of` (`mcp/available_tier.rs`)
classifies `server` with **no connect attempt**: `Available` (in the
roster), `Disabled` (configured off), `Unknown`. `server_name_of` parses the
namespaced form; `disabled_decline` renders the one truthful-decline string.

- **`tool_runner::dispatch`'s** unknown-tool check calls
  `mcp::available::try_lazy_connect`: `Available` connects
  (`ensure_connected`, same timeout and per-server guard), re-snapshots the
  live registry and **continues the same function** — grading,
  escape-root, hooks, approval all still run unchanged, since a
  connect restores registration only, never permission. A connected server
  without the named tool answers "has no tool named … — `mcp_tools` lists its
  tools". `Disabled` → `disabled_decline`. `Unknown` → the Levenshtein-hinted
  unknown-tool message. A connect failure is a distinguishable error
  (`"...could not be connected: <cause>"`).
- A per-server **failure cooldown** (30 s) guards a broken server's connect
  timeout against a batch of calls each serially retrying it — complementary
  to the per-server connect guard (#556), which serializes *concurrent*
  attempts.

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
closes) listing every registered server's name, transport, state
(`connected`, `available`, or `auth required (/mcp connect <name>)`), and
namespaced tools — a stray reply for a different correlation id (e.g. another
head sharing the engine) is ignored, never opening the panel with the wrong
snapshot. `/mcp add <name> -- <command> [args...]` (stdio) / `/mcp add <name>
--url <url> [--header KEY:VALUE]...` (streamable HTTP) and `/mcp remove
<name>` send `InMsg::McpAdd`/`McpRemove` directly; the confirming
`OutEvent::McpChanged` (or a parse error, caught before the engine is
touched) renders as a transcript status line via `App::handle_mcp_changed`/
`record_mcp_error`, mirroring `/key`'s save notice. The panel has no enable key
any more ([ADR-0226](../adr/0226-one-advertised-surface-mcp-envelope-registered-is-available.md)); `/mcp connect` (OAuth) stays. No new wire surface — this
is entirely a head-side consumer of the `InMsg`/`OutEvent` pair #375 already
shipped.

[holly]: ../../entanglement-core/src/holly.rs
[profile]: ../../entanglement-core/src/protocol/permission.rs
[perm]: ../../entanglement-core/src/protocol/permission.rs
