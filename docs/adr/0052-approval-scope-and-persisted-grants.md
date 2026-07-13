# 0052. Approval scope (`Once | Session | Always`) + persisted tool grants

- Status: Accepted
- Date: 2026-07-13

## Context

An `Ask` tool approval was **one-shot**. `InMsg::Approve` carried only
`{session, request_id}` (`protocol.rs`); the runtime tool executor
(`await_decision`, [ADR-0010](0010-single-head-crate-and-bash-opt-in.md)/#59)
parked per `request_id`, ran the tool once, and forgot the decision. There was no
grant cache and no persistence, and the TUI modal offered only
approve / reject / reject-with-reason. So the *next identical call* — same tool,
same command/path — asked again, and again across restarts. The only softening
was `rhai`, which memoized an `Ask` per binding **within a single script run**
([ADR-0046](0046-rhai-sandboxed-script-tool.md)).

For [#171](https://github.com/xmiksay/entanglement/issues/171)'s permission
epic, argument-scoped rules (#173, [ADR-0051](0051-argument-scoped-permission-rules.md))
let a user *pre-write* `bash(git *): allow`, but there was no way to say "stop
asking me about **this** call" from the approval prompt itself — the interactive
counterpart to the static config.

## Decision

Give an approval a **scope** and remember the wider ones in a grant set.

### Protocol: `Approve { scope: ApprovalScope }`

`InMsg::Approve` gains `scope: ApprovalScope` (`Once | Session | Always`), a core
enum defaulting to `Once`. The field is `#[serde(default,
skip_serializing_if = "is_once")]`, so a bare `Approve` is byte-identical to the
pre-#174 wire shape — older heads keep working and omit the field. Core still
never reads it: approval semantics live entirely in the runtime (#59). This is
the only new protocol surface.

### Runtime: a grant store, applied post-resolution

A new `runtime::grants::GrantStore` holds two sets keyed by `GrantKey {tool,
arg}` (the same `(tool, argument)` #173 resolves against, matched by **exact
equality**):

- **Session** grants — in memory, keyed by `SessionId`; dropped on
  `SessionEnded`. A child session never inherits a parent's (least privilege,
  mirroring the ancestor clamp).
- **Always** grants — persisted, global across sessions and restarts.

The executor consults the store **only after** the full permission resolution
(`effective_permission` ancestor clamp → `clamp_to_base` config ceiling): if the
result is `Ask` **and** the store already grants `(session, tool, arg)`, it is
upgraded to `Allow` and runs without a `ToolRequest`. A grant therefore *only
ever raises `Ask` → `Allow`* — it never overrides a `Deny`, so an agent-profile
or config-ceiling floor stands regardless of what was once approved. On an
`Approve`, `await_decision` records the scope's grant before running; `Once`
records nothing.

The store is an `Arc<Mutex<…>>` shared between the executor's single-threaded
event loop (which reads it to skip the prompt) and the per-request dispatch tasks
(which write it off an `Approve`). The lock is never held across an `.await`.

### Persistence: a managed sibling file, not `config.yml`

`Always` grants are written to a **managed** file
`${config_dir}/entanglement/grants.yml` (override `ENTANGLEMENT_GRANTS_FILE`) — a
top-level `grants:` list of `tool(arg)` / bare-`tool` rule keys, re-written whole
on each new grant, loaded at startup. It sits *beside* the user config, not
inside it: the runtime rewrites this file freely, so — exactly like the managed
provider-key env file (#220) — it stays out of the **hand-edited, commented**
`config.yml` whose comments a programmatic write would clobber. A missing or
malformed grants file loads empty (logged); a write failure is logged, never
fatal. Both failures fail *closed* (the user is asked again), the safe direction
for a mechanism that only widens access.

## Consequences

- Positive: the exact repeated-prompt complaint is fixed — `s` (session) or `a`
  (always) at the modal stops the identical call from re-asking; `Always`
  survives restarts. The TUI hint and keys (`y`/`s`/`a`/`n`/`e`/`Esc`) carry it.
- Positive: fully backward-compatible. A head that never sends `scope` gets the
  historical one-shot behavior; the wire frame is unchanged.
- Positive: security-conservative by construction — grants only raise `Ask`, are
  matched by exact `(tool, arg)` (no pattern widening), don't touch `Deny`, and
  aren't inherited by sub-agents. A corrupt/absent store fails closed.
- Neutral: persisted grants live in their own file, so the config `permissions`
  section stays a pure **ceiling** (it can only tighten). The two are orthogonal:
  the ceiling lowers, a grant raises — and the ceiling still clamps first, so a
  `Deny` ceiling can't be re-opened by a stale grant.
- Negative: exact-match grants don't generalize — `git status` granted does not
  cover `git status -s`. That is deliberate (the issue is the *same* call
  re-asking); a user wanting a pattern writes an argument-scoped rule (#173) in
  the config instead.

## Alternatives considered

- **Persist `Always` into the `config.yml` `permissions` section.** Wrong
  direction and wrong file: that section is a least-privilege *ceiling*
  (`clamp_to_base` takes the min), so an `allow` there can't raise an agent's
  `Ask`; and a programmatic write would destroy the file's scaffolded comments
  (#219). A separate override set in a managed file keeps both the semantics and
  the hand-edited config intact.
- **A structured, argument-pattern grant.** Storing a glob so `git status` grants
  `git *` would silently widen authority beyond what the user saw and approved.
  Exact match keeps "always allow" meaning *this* call.
- **Let a grant override `Deny`.** Rejected — `Deny` is a hard policy floor
  (agent profile or config ceiling); a runtime approval must not be able to
  escalate past it.
- **Cache in core.** Core holds no executable tools and makes no policy call
  (#58/#59); the grant set is policy, so it lives in the runtime beside the rest
  of permission dispatch, zero new core surface beyond the `scope` enum.
