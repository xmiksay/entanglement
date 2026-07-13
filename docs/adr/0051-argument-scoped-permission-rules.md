# 0051. Argument-scoped permission rule keys (`tool(pattern)`)

- Status: Accepted
- Date: 2026-07-13

## Context

`PermissionProfile` graded a tool call on the **tool name alone**
([ADR-0003](0003-agent-and-permission-profiles.md)): `rules: Vec<(String,
Permission)>`, resolved by `for_tool(name)` doing exact-name-or-`*`, last match
wins. So `bash` was one grade for `git status` and `rm -rf /` alike — no
`bash(git *)`-style command patterns, no path-scoped `edit`/`write` rules. The
runtime tool executor resolved permission passing only the tool name
(`tool_runner.rs`), never the call's input JSON — even though the input is right
there in the `ToolExec` frame it is handling.

The [#171](https://github.com/xmiksay/entanglement/issues/171) user-config &
permissions epic needs finer grain: a user config `permissions` ceiling (#172,
[ADR-0047](0047-local-trust-boundary.md)) that can say "always allow `git`, never
allow `rm`" without forcing every `bash` call through approval, and agent
profiles that pre-approve edits under `src/` but ask elsewhere. `call`'s fixed
argv already lets a profile grade `call` ≠ `bash`, but that too was name-only.

## Decision

Extend a rule key from `name`/`*` to also allow **`tool(pattern)`**, matched
against a tool-specific argument string. Resolution takes the argument where the
input JSON is already in hand — the runtime.

### Core: `resolve(name, arg)` + a dependency-free glob

`PermissionProfile` keeps `rules: Vec<(String, Permission)>` (wire-compatible —
keys are still plain strings) and gains
`resolve(name: &str, arg: Option<&str>) -> Permission`. Each rule key parses to
`(tool, Option<pattern>)`: `bash(git *)` ⇒ `("bash", Some("git *"))`, `bash` ⇒
`("bash", None)`. A rule matches when its tool part equals `name` (or is `*`)
**and** either it carries no pattern, or `arg` is `Some` and the pattern's
`*`/`?` glob matches it. Last match still wins, so an argument-scoped rule placed
after a coarse one refines it (`bash: ask` then `bash(git *): allow`).
`for_tool(name)` becomes `resolve(name, None)` — a name-only view for callers
without a concrete call (inspect/TUI posture panels), under which every
argument-scoped rule is a non-match.

The glob is a hand-rolled iterative `*`/`?` matcher (`*` = any run incl. `/` and
empty, `?` = one char, everything else literal) so **core stays
dependency-free** ([ADR-0006](0006-core-dependency-hygiene-gate.md)) — no `glob`
crate, no path semantics, no `**`.

### Runtime: argument extraction + threading

`runtime::permission::permission_arg(tool, input)` parses the `ToolExec` input
and returns the argument a pattern matches against: the `command` for `bash`, the
`command` + `args` line for `call`, the target `path` for `edit`/`write`/`read`,
`None` otherwise (or on malformed input — an argument-scoped rule then simply
never matches). The tool executor extracts it once per call and threads
`Option<&str>` through `effective_permission` (the ancestor-chain privilege
clamp, [ADR-0024](0024-subagent-permission-gating.md)) and `clamp_to_base` (the
config ceiling, #172), so both honor argument-scoped rules. The `rhai` binding
policy ([ADR-0046](0046-rhai-sandboxed-script-tool.md)) captures the profile
chain once and resolves each binding call against it **with the call argument**,
closing the same gap for scripted quintet calls.

## Consequences

- Positive: a config ceiling can pre-approve/deny specific commands and paths
  (`bash(rm *): deny`, `edit(src/*): allow`) instead of an all-or-nothing per-tool
  grade — the granularity #171/#172 need. Enforced on every path (direct dispatch,
  ancestor clamp, config ceiling, rhai bindings), so no bypass.
- Positive: fully backward-compatible. Existing name-only rules and configs
  resolve identically; the rule vector's wire shape is unchanged.
- Neutral: the argument extractor is a small tool→field table in the runtime, the
  one place that must know each tool's input shape. New arg-matchable tools add a
  match arm; unlisted tools stay name-only.
- Negative: the glob is intentionally simple — no `**`, no brace/character
  classes. A user wanting deep-path precision writes `src/*` (which, being
  separator-agnostic, already matches everything under `src/`).

## Alternatives considered

- **Match in core with the `glob` crate.** Would add a dependency to
  `entanglement-core` and drag in path semantics (`/`-aware, `**`) that fit files,
  not shell command lines. Rejected for the hygiene gate and for wrong semantics;
  the hand-rolled matcher is ~20 lines and predictable.
- **Regex rule keys.** More powerful, but heavier to author, easy to get
  catastrophically wrong (ReDoS on untrusted-ish config), and overkill for
  command-prefix / path-prefix scoping. A `*`/`?` glob covers the ergonomic cases.
- **Resolve in core by passing the input JSON through the protocol.** Core would
  have to parse tool inputs and know per-tool argument shapes — exactly the policy
  knowledge [ADR-0010](0010-single-head-crate-and-bash-opt-in.md)/#59 moved out of
  core. Keeping extraction in the runtime preserves "core makes no policy call."
- **A structured rule type on the wire** (`{tool, pattern, perm}`) instead of a
  parsed string key. Rejected: it breaks the existing `Vec<(String, Permission)>`
  shape and the YAML `tool: perm` mapping ergonomics for no gain — the string key
  already round-trips and reads naturally in a config file.
