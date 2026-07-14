# 0066. Lifecycle hooks are runtime interceptors around tool dispatch and prompt ingress

- Status: Accepted
- Date: 2026-07-14
- Hangs off the `tool_runner` interception pipeline ([0059](0059-tool-trait-and-registry-live-in-the-runtime.md)/#203) and the inbound `InMsg` fan-out; composes with the permission dispatch of #59 and the user-config layering of [0047](0047-local-trust-boundary.md)/#172.

## Context

The engine had no hook mechanism (`grep -i hook` found only
`std::panic::set_hook`). There was no way to run policy, telemetry, or formatting
around tool execution or prompt submission: a user could not veto a `bash rm …`
from outside the permission model, log every tool call to an external sink, or
auto-format a file after a `write` (#199, part of the audit epic #196).

Two natural seams already existed in the **runtime**, not core:

- the `tool_runner` executor owns the `ToolExec → ToolResult` round-trip and,
  since #203, routes each call through an explicit interception pipeline
  (`Intercept::classify` → per-route handler);
- every inbound `InMsg` — including `Prompt` — is fanned out on
  `Holly::subscribe_inbound()`, which the executor already watches for `Stop`.

Core neither knows nor should know that an external command runs before a tool.
Putting hooks in core would drag process-spawning policy across the
`provider ← core ← runtime` seam and duplicate the permission model's home.

## Decision

**Add lifecycle hooks as a runtime interceptor, configured in the layered user
config (`hooks:` section), with three points modeled on Claude Code's hooks:**

- **`pre_tool_use`** — runs at the top of the generic `dispatch`
  (`Intercept::Permission` route), *before* the `Allow | Ask | Deny` decision. A
  hook that exits **non-zero vetoes** the call: `dispatch` folds the hook's
  output back as the `ToolResult` and the tool neither prompts nor runs. This is
  the policy gate. A cleared hook (exit 0) falls through to permission unchanged.
- **`post_tool_use`** — runs in `run_and_reply` after the tool produces its
  result, before it is folded back. **Observational**: the exit code is logged,
  never fed to the model, and it cannot rewrite the `ToolResult`. It serves the
  formatter/telemetry side-effect (e.g. `prettier` on a just-written file, which
  acts on the filesystem, not the result).
- **`user_prompt_submit`** — runs when an `InMsg::Prompt` reaches the engine,
  fired from the executor's inbound watcher (the same task that catches `Stop`).
  Observational (telemetry/logging); it does not gate the prompt.

Each hook is an `sh -c <command>` child that receives a JSON payload on stdin
(`{event, session, tool?, input?, output?, prompt?}`) plus `ENTANGLEMENT_HOOK_EVENT`
/ `ENTANGLEMENT_SESSION_ID` / `ENTANGLEMENT_TOOL_NAME` env vars. It runs under a
per-hook timeout in its **own process group**, reusing the exec tools' containment
(`host::exec::own_process_group` + `wait_or_kill_group`, [0009](0009-edit-and-bash-host-tools.md)/#168)
so a hook that spawns children can't orphan them past the timeout. A timeout or a
spawn failure counts as a **failure**, so a `pre_tool_use` hook that can't launch
vetoes the tool rather than silently letting it through (fail-closed on the policy
gate).

Config lives in `Config.hooks: Hooks` (`entanglement-runtime::hooks`), a plain
serde struct — three `Vec<HookSpec>` — deep-merged and `deny_unknown_fields`-validated
by the same loader as `permissions` (#172). Each `HookSpec` carries the `command`,
an optional `tools` name-filter for the tool hooks (empty ⇒ all), and a
`timeout_secs`. The wiring is a new `spawn_tool_executor_with_hooks` seam; the
historical `spawn_tool_executor` stays a no-hook wrapper so existing callers and
tests are untouched. The inbound subscription is hoisted to run **synchronously**
before the executor task spawns, so a `Prompt` sent right after startup can't race
ahead of the `user_prompt_submit` watcher.

### Scope: only the generic dispatch route

Hooks fire only for the generic host-tool `Intercept::Permission` route — the
orchestration tools (`agent`/`agent_spawn`/`agent_poll`/`ask_user`/`propose_plan`,
which touch no host resource) and the self-permissioning `rhai` tool bypass them,
matching the issue's "around `tool_runner::dispatch`" scope. Extending hooks to
`rhai`'s in-script tool calls is a future option, exactly as the `FileChange`
audit scope note in [0060](0060-filechange-audit-via-executor-as-path-kind-hash.md)
leaves that door open.

## Alternatives rejected

- **Hooks in core, on the turn loop.** Rejected: core holds no executable tools
  and makes no policy call ([0059](0059-tool-trait-and-registry-live-in-the-runtime.md));
  spawning shell commands there violates the crate seam and duplicates the
  runtime-owned permission home (#59).
- **A `post_tool_use` that rewrites the result.** Rejected for now: the result is
  multimodal (`Vec<ContentPart>`, [0064](0064-message-content-blocks.md)/#221) and
  a JSON-round-trip rewrite protocol is a larger surface than the formatting
  use-case needs — a side-effecting formatter acts on the file, not the result.
  The observational shape can grow a rewrite channel later without breaking the
  config.
- **A Rust/dylib plugin ABI.** Rejected as over-scoped: an `sh -c` command with a
  JSON stdin payload is the KISS interface, scriptable in any language, and
  matches the exec tools' existing containment story.
- **Fold the pre-hook veto into the permission model.** Rejected: permission is a
  static `Allow | Ask | Deny` grade clamped by a ceiling (#172); a hook is
  *dynamic* policy (it can inspect the actual arguments and the environment). They
  compose — the pre-hook runs first and can veto before permission — rather than
  one subsuming the other.
