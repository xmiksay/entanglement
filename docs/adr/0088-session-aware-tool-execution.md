# 0088. Session-aware tool execution: thread `SessionId` through `ToolRegistry::execute`/`Tool::run`

- Status: Accepted
- Date: 2026-07-16

## Context

`spawn_tool_executor_with_policy` (#311) made permission *resolution*
session-aware: an `Arc<dyn PermissionResolver>` and `Arc<dyn GrantStore>` both
take the caller's `SessionId`, so a multi-tenant embedder can plug in per-tenant
policy without forking `tool_runner`. Tool *execution* stayed session-blind:
`Tool::run(&self, input: &str)` and `ToolRegistry::execute(&self, call:
&ToolCall)` (the only entry points `tool_runner`/`rhai` actually call) carry no
caller identity, even though every call site already holds a `SessionId` at the
point it invokes `execute` (`run_and_reply` in `tool_runner.rs`, `exec` in
`script.rs`).

That gap blocks a shared `ToolRegistry` from telling tenants apart at dispatch
time:

- **Per-tenant MCP servers are undispatchable.** Two users can configure a
  server of the same name with different URLs/tokens; `tool_spec_resolver`
  (#308) already advertises each user only their own tools, but one
  `McpTool`/`HttpClient` instance backing that name can't pick the caller's
  endpoint + auth headers without knowing who's calling.
- **Tenant-owned host tools can't enforce ownership.** A DB-backed tool (e.g.
  a site's `edit_page`) must scope writes to the calling user; without session
  identity it would need one registry per user, defeating the shared executor
  `spawn_tool_executor_with_policy` was built to enable.

Without this seam, the only workaround is bypassing `tool_runner` with an
embedder-owned executor — losing hooks, `rhai`, plan/tasks, `agent_spawn`/
`agent_poll`, and the interception ladder in the process.

## Decision

Add a session-aware entry point to the `Tool` trait, mirroring the existing
`run`/`run_content` default-delegation pattern, and thread the caller's
`SessionId` through `ToolRegistry::execute`:

```rust
#[async_trait]
pub trait Tool: Send + Sync {
    // existing: name, description, schema, run, run_content

    async fn run_for_session(
        &self, _session: &SessionId, input: &str,
    ) -> anyhow::Result<Vec<ContentPart>> {
        self.run_content(input).await
    }
}

impl ToolRegistry {
    pub async fn execute(&self, call: &ToolCall, session: &SessionId) -> Vec<ContentPart> {
        // dispatches to `tool.run_for_session(session, &call.input)`
    }
}
```

`run_for_session` defaults to delegating to `run_content`, so every existing
single-tenant tool (the host quintet, `bash`/`call`/`bash_output`, `McpTool`)
compiles and behaves unchanged without touching a single impl. A session-aware
tool — an embedder's own `Tool` that picks a tenant's `HttpClient` by session
prefix, or scopes a DB write to the caller — overrides `run_for_session`
instead.

`ToolRegistry::execute`'s three in-tree call sites already hold the
`SessionId` they need to pass: `tool_runner::run_and_reply` (which receives
`session` from the parked-turn dispatch loop) and `script::exec` (via
`service_binding`, which already threads `session: &SessionId` for the `rhai`
approval round-trip).

`McpTool` itself stays session-blind — its config (command/URL, static
headers) is fixed at registration, and giving it per-call session routing
would mean re-resolving connection state on every call for a capability that,
in-tree, is single-tenant. An embedder that needs per-tenant MCP dispatch
registers its own `Tool` wrapping several `HttpClient`s and routes on
`session` in its `run_for_session` override — the seam this ADR adds is what
makes that possible, not a built-in multi-server-per-name feature.

## Consequences

- **(+)** A multi-tenant embedder can share one `ToolRegistry` across sessions
  and still dispatch per-tenant, closing the gap `spawn_tool_executor_with_policy`
  (#311) left between session-aware *policy* and session-blind *execution*.
- **(+)** Zero signature churn for existing tools — `run_for_session`'s default
  body makes this purely additive; `run`/`run_content` keep their exact
  meaning and every current `impl Tool` is untouched.
- **(+)** The `SessionId` was already live at every `execute` call site; this
  is a plumbing change, not new state.
- **(−)** `ToolRegistry::execute` gained a required parameter — a small,
  mechanical break for any out-of-tree caller (in-tree: `tool_runner.rs`,
  `script.rs`, plus registry unit tests, all updated in the same change).

## Alternatives considered

- **Tokio task-local for the current session.** Zero signature churn — no
  `execute`/`run_for_session` parameter at all, just `SESSION.with(...)` inside
  a tool. Rejected: implicit context is easy to miss in a spawned subtask
  (`tokio::spawn` doesn't propagate a task-local unless explicitly re-scoped),
  and a tool silently reading the wrong tenant's session because a task-local
  wasn't threaded through a `spawn` is exactly the failure mode multi-tenant
  isolation can't afford. The explicit parameter makes the dependency visible
  at every call site and at the type level.
- **One `ToolRegistry` per session/tenant.** Sidesteps the need for session
  identity inside a tool entirely. Rejected by the issue's own motivating
  case: it defeats the shared executor `spawn_tool_executor_with_policy` (#311)
  was built to enable, and duplicates registry construction (and any
  in-memory state a tool holds) per tenant instead of per process.
- **New trait, `SessionAwareTool`, alongside `Tool`.** Two trait objects per
  registration, `ToolRegistry` would need to dispatch on which trait a tool
  implements. Rejected: the default-delegating method on the existing trait
  (the same pattern `run_content` already established over `run`) is strictly
  simpler and keeps one trait object per tool.
