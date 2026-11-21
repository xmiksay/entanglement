# 0249. In-process MCP servers and per-session skills for embedders

- Status: Accepted
- Date: 2026-11-21
- Requested by: nexial-ventures/infra#722
- Amends: [ADR-0067](0067-mcp-client-as-runtime-tool-provider.md) (a third
  transport beside stdio and streamable HTTP),
  [ADR-0188](0188-session-keyed-per-user-mcp-scopes.md) (a scoped connect
  now knows the session that triggered it),
  [ADR-0037](0037-load-skill-tool-deterministic-resolution.md) and
  [ADR-0227](0227-one-binding-path-namespaces-http-skill-libraries.md) §5
  (`load_skill`, `list_skills` and the `skill::` namespace read a
  session's registry)

## Context

A multi-tenant embedder serves its own tool surface — the modules of its
application — and wants the model to reach it the way it reaches every other
external tool: through the `mcp(server, tool, args)` envelope, the scoped
connect cache, mode and rule grading on `mcp__<server>__<tool>`, and a
script's `mcp::<server>::<tool>()` binding. Two transports exist, a stdio
subprocess and streamable HTTP. Neither fits a server living in the same
process: an HTTP call to oneself costs a hop and re-authenticates a user the
host already knows, and the tools must authorize per user and tenant, which
needs the calling session — something neither transport carries.

Skills have the same shape of problem. `list_skills`, `load_skill` and the
`skill::` namespace read one process-wide registry discovered from disk, so a
host cannot give one tenant a skill another tenant does not have, nor build a
skill in code at all without writing a `SKILL.md`.

## Decision

1. **A third transport, `InProcess`.** An embedder implements
   `mcp::InProcessMcp`:

   ```rust,ignore
   #[async_trait]
   pub trait InProcessMcp: Send + Sync {
       async fn list_tools(&self, session: &SessionId) -> Result<Vec<McpToolDef>>;
       async fn call_tool(&self, session: &SessionId, request_id: &str,
                          name: &str, arguments: Value) -> Result<Value>;
   }
   ```

   and carries it on `McpServerConfig::in_process` (an `InProcessServer`
   handle, `#[serde(skip)]`; equality is identity), most simply via
   `McpServerConfig::from_in_process(handler)`. `transport()` becomes a
   three-way choice: `in_process` excludes `command` and `url`. `target()`
   and the `list_mcp` transport label read `in-process`.
2. **The session reaches the handler.** `McpClient::InProcess` dispatches
   `call_tool_in(Some((session, request_id)), …)`, which `McpTool` passes from
   `run_with_meta` — the path the registry and every rhai binding take. The
   session-less `run`/`run_content` (and `McpClient::call_tool`) refuse an
   in-process server instead of inventing a session. `call_tool` answers an
   MCP `CallToolResult` object, rendered like any server's.
3. **Scope-only.** `list_tools` is called with the session whose call (or
   `prewarm`) triggered the connect; `McpScopes::ensure_server` and
   `script_connect` now take that session. A connect with no session — the
   global `mcp:` map, `/mcp add`, a YAML file, which cannot carry the handler
   anyway — fails with a clear error. The listed tools are cached per
   `(scope key, server)` like any scoped connection, so the embedder keys its
   scopes by everything that changes the list, or calls `evict_scope` when it
   changes.
4. **Grading is unchanged.** The server's `capabilities` map grades its
   tools (`read`/`write`/`call`), and an unannotated tool is `Write`.
5. **Per-session skills.** `skills::SkillResolver`
   (`Arc<dyn Fn(&SessionId) -> Option<Arc<SkillRegistry>>>`) picks a session's
   registry; `None` falls back to the live one. It is set on
   `ToolExecutorBuilder::skill_resolver` (read by `list_skills`, a script's
   `skill::` namespaces and the `SkillActive` bookkeeping) and on
   `LoadSkillTool::with_resolver` (the tool, which now resolves in
   `run_for_session`). The executor's live registry and the resolver travel
   together as `skills::SessionSkills`.
6. **Skills built in code.** `SkillMeta::new(name, description, body)` makes
   a skill with no directory, and `SkillMeta::with_lib(source)` (feature
   `rhai`) compiles its `lib.rhai` with the discovery rules — only `fn`
   definitions.

## Consequences

- An embedder serves its application to the model as one MCP server with no
  network hop, per-call identity, and the same gates as a remote server.
- `McpServerConfig` gains a public field and derives `Default`: a struct
  literal must add `in_process: None` or use `..Default::default()`.
- The first session of a scope key decides the cached tool list. A host
  whose list depends on more than the key must key finer or evict.
- Skutter itself is unchanged: it registers no in-process server and no
  resolver.

## Rejected

- **An HTTP self-call.** It works today with no change, but pays a hop and
  re-validates a user the host already holds, and the tools still cannot see
  the session or request id.
- **Passing the session through `McpScope` instead of per call.** A scope is
  shared by every session of its key, so it cannot name the caller.
- **Registering the host's tools as native tools.** They would be advertised
  on every round, outside the `mcp` envelope, and out of reach of the
  script's `mcp::` namespace.
- **A per-session `SkillRegistry` installed by a hook on session start.** The
  resolver is a pure lookup and needs no lifecycle; a session resumed after a
  restart reads the same answer.
