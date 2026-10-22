# Gates & host tools §9 — Lifecycle hooks & pluggable policy seams

> Part of the [gates & host tools module](../gates-and-host-tools.md) of the [architecture overview](../../architecture.md).

## 9. Lifecycle hooks — [ADR-0066](../../adr/0066-lifecycle-hooks-as-runtime-interceptors.md) (#199)

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
[ADR-0205](../../adr/0205-every-compaction-forks-a-successor-session.md) every
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
  unset seam takes a documented default, table in `docs/embedding/tool-execution.md` §4).
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

### Pluggable policy seams — `PermissionResolver` + `GrantStore` — [ADR-0079](../../adr/0079-pluggable-permission-resolver-and-grant-store.md) (#311)

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

### Dynamic `ToolRegistry` — `SharedRegistry` — [ADR-0096](../../adr/0096-dynamic-toolregistry-sharedregistry.md) (#372)

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
([ADR-0076](../../adr/0076-per-session-dynamic-tool-specs.md)) is a plain sync
`Fn` consulted on the turn's hot path and must never block on I/O; a
`tokio::sync::RwLock` would force that closure into `blocking_read` or break
0076's no-async contract. `skutter` (`boot.rs`) wires the resolver to read through the
same `SharedRegistry` handle it hands the executor, reproducing
`cfg.tool_specs`' composition: registry tools plus the runtime-intercepted
pseudo-tool specs that aren't `ToolRegistry` entries
(`update_tasks`/`ask_user`/`poll`/`bash`/`rhai`), sorted by name and deduped.
The resolver projects the one **`KERNEL`** surface out of that roster and
snapshots it per session ([ADR-0226](../../adr/0226-one-advertised-surface-mcp-envelope-registered-is-available.md), §One advertised surface in [one surface](one-surface.md)) — dynamic
tools (MCP, script-only built-ins) stay registered and dispatchable, just
absent from the advertised array.
Because core advertises the resolver's output verbatim now — there is no
more per-agent mask, and the session's permission mode + overlay enforce
only at dispatch — **this resolver is the only thing
that shapes a session's surface**: a spec missing here is a spec no model ever
sees. That is exactly the shape of ADR-0190's Bug 1, where `poll_spec()` was
absent from the roster and vanished from every real head while tests, which run
off the static `tool_specs` fallback, stayed green.

---

Previous: [§8 — Script surface](script-surface.md) · Next: [§10 — MCP client: transports, OAuth, scopes & proxy](mcp.md)
