# Gates & host tools §9 — Lifecycle hooks & pluggable policy seams

> Part of the [gates & host tools module](../gates-and-host-tools.md) of the [architecture overview](../../architecture.md).

## 9. Lifecycle hooks — [ADR-0066](../../adr/archive/0066-lifecycle-hooks-as-runtime-interceptors.md) (#199)

User-configured external commands run around tool execution and on prompt
ingress, for policy, telemetry and notifications. Hooks are
**`skutter`'s** (`skutter::hooks`, #964): neither core nor the library
executor runs or knows any hook. They hang off seams every embedder already
has — the pluggable `PermissionResolver` (below) and the `InMsg`/`OutEvent`
fan-out — so no hook seam exists in the library or the protocol; the one
library change is that a resolver's `Deny` carries its reason (`Grade`).

| point | fires | can block? | payload |
| --- | --- | --- | --- |
| `pre_tool_use` | while a call is graded: `HookResolver` wraps `skutter`'s `ModeResolver`, so it runs wherever the executor asks the resolver — every graded model call (the `agent`/`agent_send` gate and `propose_plan` included), a script's launch and each nested binding call, a redirect hop's in-call grade | **yes** — a non-zero exit grades the call `Deny` carrying the hook's reason: the tool neither prompts nor runs and the model gets `tool X blocked by pre_tool_use hook: <output>` (as in 0.7), not the mode-deny text; the reason is also logged (`warn`) | `{event, session, tool, input}` |
| `post_tool_use` | on the `ToolOutput` of a host-tool call the runtime ran (`ToolExec`, matched by `(session, request_id)`, with a measured `duration_ms` — ADR-0176's "it ran" mark; `update_tasks`'s ack is the one unmeasured run). A refused call fires nothing, nor does a call declined at the door by argument validation; a script's nested calls are not covered (they emit no `ToolExec`). The executor's own routes fire nothing, as in 0.7: `agent`, `agent_send`, `poll`, `ask_user`, `propose_plan`, `request_mode`, the `rhai` launch and the discovery tools but `load_skill` | no — observe-only telemetry (exit code logged, never fed to the model); it runs after the result folded back, so it cannot rewrite it | `{event, session, tool, input, output, is_error, exit_code}` — `input` as the executor ran it: a `/cd` session's relative path arguments joined onto its workdir (`path_grading::rewrite` over `ToolExec.workdir`, as in 0.7 and as `pre_tool_use` grades it), since the hook runs from the project root; `is_error` (#636, ADR-0176) is the same structured classification that rides `OutEvent::ToolOutput`; `exit_code` (#681, ADR-0186) is the observed process exit status for `bash` (JSON `null` for every other tool and for a killed process) |
| `user_prompt_submit` | on an inbound `InMsg::Prompt` (run when the first event it causes arrives) | no — observational | `{event, session, prompt}` |
| `session_start` | on `OutEvent::SessionStarted` — every session, root or child, incl. a compaction successor and each session a resume re-announces | no | `{event, session, source, parent, predecessor, agent, model, root}` — `source` is `resume` when an `InMsg::Resume`'s records name the id, else `compact` when it has a `predecessor`, else `fresh` |
| `session_end` | on `OutEvent::SessionEnded` / `SessionHibernated` | no | `{event, session, reason, parent, agent}` — `reason`: `hibernated`, `compacted` (the source after its `Compacted`), else `closed` |
| `stop` | a **root** session's turn finished: its `Done`, classified by the `Status` core always emits right after it | no | `{event, session, reason, error, parent, agent}` — `reason`: `completed` \| `error` (`error` carries the turn's last `Error` message). A `Stop`-cancelled turn emits no `Done`, so it fires nothing; neither does a manual `/compact`'s closing `Done` |
| `subagent_stop` | the same, for a session spawned with a `parent` (instead of `stop`) | no | as `stop`; `parent` is the spawning session, `agent` the child's profile |
| `pre_compact` | on `OutEvent::Compacted` — the source's last content event, before the successor starts and the source retires | no | `{event, session, kind, kept, summary}` — `kind`: `manual` \| `auto` \| `prune` |
| `notification` | an approval parked (`OutEvent::ToolRequest`) or an `ask_user` question parked (`OutEvent::UserQuestion`) | no | approval: `{event, session, kind: "approval", request_id, tool, input, message}` (+ `ENTANGLEMENT_TOOL_NAME`); question: `{event, session, kind: "question", request_id, questions, message}` |

**Why only `pre_tool_use` blocks.** Every other point is derived by a
subscriber from frames core or the executor already acted on, so there is
nothing left to veto — and core stays policy-free (ADR-0066). That holds
even for `pre_compact`, the one that sounds like a gate: since
[ADR-0205](../../adr/archive/0205-every-compaction-forks-a-successor-session.md) every
compaction forks and never rewrites the source log, so a hook has no history
to rescue first. `pre_tool_use` blocks only because the grade is already a
pluggable decision every call passes.

- **Config:** the `hooks:` section of the layered user config (§ADR-0047/#172),
  deep-merged like any key. The library carries it through unparsed
  (`Config::hooks`, a `serde_yaml::Value`); `skutter` parses it with
  `Hooks::parse` at startup (`main.rs`, before any subcommand) into nine
  `Vec<HookSpec>` (one per point), `deny_unknown_fields`-validated, so a typo
  is still a loud startup error. A `HookSpec` is `{command, tools?,
  timeout_secs?}`; `tools` is an optional name-filter for the tool hooks
  (empty ⇒ every tool), ignored by every other point. Empty section ⇒ no
  hooks (the norm).
- **Execution:** each hook is an `sh -c <command>` child fed the JSON payload on
  stdin and given `ENTANGLEMENT_HOOK_EVENT` / `ENTANGLEMENT_SESSION_ID` /
  `ENTANGLEMENT_TOOL_NAME` (tool hooks and an approval `notification`) env
  vars, with `secret_env`'s hidden vars scrubbed. It runs under
  `timeout_secs` (default 30) in its **own process group**, reusing the exec
  tools' containment (`entanglement_runtime::exec`, §8/#168) so a hook that
  spawns children can't orphan them. A timeout or a spawn failure counts as a
  **failure**, so a `pre_tool_use` hook that can't launch **fails closed**
  (denies the call) rather than letting it through.
- **`pre_tool_use` is a grade.** `HookResolver` returns
  `Grade::deny(reason)` on a veto, and the executor hands that reason to the
  model in place of the mode-deny text (below). Known limitation, fixed by
  #936: the executor asks the resolver once per session in a call's
  ancestor chain, so a sub-agent's call runs the hooks once per ancestor —
  N sequential hook processes at depth N, each with
  `ENTANGLEMENT_SESSION_ID` naming an ancestor that did not make the call
  (an audit hook logs phantom calls); a veto from any of them denies. A
  `Control` tool that bypasses grading (the discovery tools, `ask_user`,
  `update_tasks`, …) never reaches it. The hook sees the
  call's **grade identity**, not the raw call: `read_raw`/`glob_json`/
  `grep_json` arrive as `read`/`glob`/`grep`, a `Tool::call_grading` override
  as its target, a rhai launch with its root-rewritten graded input — so a
  `tools:` filter (and `ENTANGLEMENT_TOOL_NAME`) must name the graded tool.
- **`post_tool_use` is asynchronous and observe-only.** It runs on the
  watcher after the result folded back, so it races the agent's next call:
  a formatter (`prettier --write` on `write`/`edit`) can run before or after
  the next `read`/`edit` of the same file — in the same batch or the next
  turn — or overwrite the agent's next write. A hook that changes the
  workspace makes the loop non-deterministic; keep `post_tool_use` to
  telemetry, or format at turn end (`stop`). A script's nested writes are
  not seen at all (they emit no `ToolExec`). Once host tools (#968) and the
  file/exec bindings (#933) move to `skutter`, it runs `post_tool_use`
  synchronously inside its own tools, script calls included, which makes a
  formatter deterministic again.
- **Changed from 0.7 (signed off on #964).** Each follows from decision B
  (no hook seam in the executor): (b) the per-ancestor `pre_tool_use` runs
  above, until #936; (c) `post_tool_use` no longer fires for a rhai
  script's nested binding calls; (d) `post_tool_use` runs after the result
  folded back (the race above) instead of before. `pre_tool_use` also runs
  on `propose_plan` and redirect-hop grades and matches the graded name
  (above), since the hook is the resolver. (a), the veto reason, is kept:
  a resolver's `Deny` carries it.
- **Wiring (`services.rs`):** `skutter` builds the `ModeResolver` itself and
  hands the builder `HookResolver::new(mode_resolver, hooks)` via
  `.resolver(..)` when any `pre_tool_use` command exists. `hooks::spawn`
  subscribes the outbound stream and the inbound fan-out synchronously
  (before any prompt can race them) when any other point has a command, and
  returns the watcher's handle, aborted with the other services. A pure
  `Tracker` folds frames into firings (draining buffered inbound frames
  before each event — the supervisor fans a `Prompt`/`Resume` out before
  handling it, so it is always seen ahead of the events it causes), and two
  sequential workers run them — one for `post_tool_use`, one for every other
  point — so hooks fire in event order within each while a slow hook never
  lags the subscription, and a batch of slow `post_tool_use` hooks never
  delays an approval `notification`. The order *between* a `post_tool_use`
  and another point's firing is not kept. Firings are best-effort under a
  broadcast lag: dropped events fire nothing, and a pending turn end is
  forgotten rather than misclassified. Engine-internal LLM calls (aux
  models: `summarize`/`session_title`/`narrate`) open no session and so
  fire nothing; a spawned child is an ordinary session and does. The
  skipped routes are a copy (`ROUTE_TOOLS`) of the runtime's private
  `Intercept::classify` plus `request_mode`; a note there keeps them in
  step.

### Pluggable policy seams — `PermissionResolver` + `GrantStore` — [ADR-0079](../../adr/archive/0079-pluggable-permission-resolver-and-grant-store.md) (#311)

The executor hard-codes *no* policy source. `ToolExecutor::builder(..)
.resolver(Arc<dyn PermissionResolver>).grants(Arc<dyn GrantStore>)` (traits in
`entanglement-runtime::policy`) drives two trait objects, so a multi-tenant
embedder that stores rules per user in its own DB swaps both without forking the
~350-line executor — keeping the shared interception ladder, spawn bounds,
rhai, and plan/tasks tools.

- **`PermissionResolver::resolve(session, tool, input) → Grade`** decides one
  session's `Allow|Ask|Deny` grade (async — a real embedder hits a DB, and the
  ladder already runs in a detached task). A `Grade::Deny(Some(reason))`
  reaches the model verbatim as the call's result — from the dispatch gate,
  the `rhai` launch and `propose_plan`; a redirect hop's refusal quotes it —
  in place of the `denied by mode … use /mode to switch` text a `Deny(None)`
  gets (#964);
  across the ancestor chain the first deny, leaf first, gives the reason.
  It runs **where the mode/base resolution ran before**, but the sub-agent
  ancestor clamp (ADR-0024) and spawn bounds stay in the ladder **on top
  of** it: the executor snapshots the call's ancestor chain
  (`permission::ancestor_chain`) in the loop and takes the least-privileged
  resolver grade across it (`resolve_effective`), so a tenant rule can
  never widen a child beyond its parent. `apply_grant` then upgrades a
  resolved `Ask` to `Allow` from a `GrantStore` grant.
- **`GrantStore`** persists + reads "always allow" grants (§ agents-and-permissions
  #174). `record(session, tool, arg, scope, mode)` is async so an `ApprovalScope::Always`
  can hit a DB; `is_granted(session, tool, arg, mode)` is a sync fast check. Both
  take the mode the grant was earned in, and a grant matches only that mode
  (ADR-0207 §8). A multi-tenant store writes its
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

### Dynamic `ToolRegistry` — `SharedRegistry` — [ADR-0096](../../adr/archive/0096-dynamic-toolregistry-sharedregistry.md) (#372)

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
([ADR-0076](../../adr/archive/0076-per-session-dynamic-tool-specs.md)) is a plain sync
`Fn` consulted on the turn's hot path and must never block on I/O; a
`tokio::sync::RwLock` would force that closure into `blocking_read` or break
0076's no-async contract. `skutter` (`boot.rs`) wires the resolver to read through the
same `SharedRegistry` handle it hands the executor, reproducing
`cfg.tool_specs`' composition: registry tools plus the executor's own route
specs that aren't `ToolRegistry` entries (`discover::runtime_owned_specs` —
`update_tasks`/`ask_user`/`poll`/`rhai`/discovery/`mcp`/… — and
`subagent::agent_specs`), sorted by name and deduped.
The resolver keeps the registry tools that set **`Tool::advertised`** (plus
every route spec) and snapshots that one surface per session ([ADR-0226](../../adr/archive/0226-one-advertised-surface-mcp-envelope-registered-is-available.md), §One advertised surface in [one surface](one-surface.md)) — dynamic
tools (MCP, script-only built-ins) stay registered and dispatchable, just
absent from the advertised array.
Because core advertises the resolver's output verbatim now — there is no
more per-agent mask, and the session's permission mode enforces
only at dispatch — **this resolver is the only thing
that shapes a session's surface**: a spec missing here is a spec no model ever
sees. That is exactly the shape of ADR-0190's Bug 1, where `poll_spec()` was
absent from the roster and vanished from every real head while tests, which run
off the static `tool_specs` fallback, stayed green.

---

Previous: [§8 — Script surface](script-surface.md) · Next: [§10 — MCP client: transports, OAuth, scopes & proxy](mcp.md)
