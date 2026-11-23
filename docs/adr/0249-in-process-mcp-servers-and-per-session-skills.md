# 0249. Per-call tool grading, host script namespaces and per-session skills

- Status: Accepted
- Date: 2026-11-21
- Requested by: nexial-ventures/infra#722
- Supersedes the in-process MCP transport this ADR originally proposed
- Amends: [ADR-0037](0037-load-skill-tool-deterministic-resolution.md) and
  [ADR-0227](0227-one-binding-path-namespaces-http-skill-libraries.md) §5
  (`load_skill`, `list_skills` and the `skill::` namespace read a
  session's registry)

## Context

A multi-tenant embedder serves its own tool surface — the modules of its
application — and wants the model to reach it through the same permission
gates (mode grading, grants, approval cards) that every built-in tool uses,
and to be callable from a Rhai script through a generated namespace (like
`mcp::<server>::<tool>`). The embedder registers one native tool
(e.g. `invoke`) that dispatches to its internal hub; the dispatch target and
its capability vary per call, so the permission seam must grade each call as
its target, not as the envelope tool.

Skills have the same per-session shape of problem. `list_skills`,
`load_skill` and the `skill::` namespace read one process-wide registry
discovered from disk, so a host cannot give one tenant a skill another
tenant does not have, nor build a skill in code without writing a
`SKILL.md`.

## Decision

1. **Per-call tool grading.** `Tool::call_grading(input) -> Option<CallGrading>`
   (default `None`). When `Some`, the dispatch ladder grades the call under
   the returned `name` and `capabilities` instead of the tool's own. The
   `PermissionResolver`, the `GrantStore` and every `Always` approval see
   the target name. The mode table's class rules (`read`/`write`/`exec`)
   match the provided capabilities. Direct calls and Rhai nested calls
   both go through the same path.

2. **Host script namespaces.** `HostNamespace { name, tool, resolver }`:
   an embedder registers a namespace (e.g. `infra`) bound to a registered
   tool (e.g. `invoke`) plus a `HostFnResolver` listing the callable
   function names (+ descriptions) per session. Each Rhai run generates
   `<ns>::<fn>(#{...})` from the resolver (reusing the identifier mapping
   `rhai_ident`), and a call dispatches to the backing tool with
   `{"tool": "<fn>", "args": {...}}`, graded via the tool's
   `call_grading`. A hidden/unlisted name is absent from the namespace.
   Skill `lib.rhai` code can call it. No file/exec/env access is opened.

3. **Per-session skills.** `skills::SkillResolver`
   (`Arc<dyn Fn(&SessionId) -> Option<Arc<SkillRegistry>>>`) picks a session's
   registry; `None` falls back to the live one. It is set on
   `ToolExecutorBuilder::skill_resolver` (read by `list_skills`, a script's
   `skill::` namespaces and the `SkillActive` bookkeeping) and on
   `LoadSkillTool::with_resolver` (the tool, which now resolves in
   `run_for_session`). The executor's live registry and the resolver travel
   together as `skills::SessionSkills`.

4. **Skills built in code.** `SkillMeta::new(name, description, body)` makes
   a skill with no directory, and `SkillMeta::with_lib(source)` (feature
   `rhai`) compiles its `lib.rhai` with the discovery rules — only `fn`
   definitions.

## Consequences

- An embedder reaches the model's tool calls through one registered native
  tool, graded per target, with no network hop.
- Rhai scripts call the host namespace like `mcp::` — same bridge, same
  `NestedCall` events, same grading.
- Skutter itself is unchanged: it registers no host namespace and no
  resolver.

## Rejected

- **An in-process MCP transport.** It was the original ADR-0249 proposal,
  carrying `InProcessMcp`, `InProcessServer`, `Transport::InProcess`,
  `McpClient::InProcess`, `list_tools_in`/`call_tool_in`, and a `Default`
  derive on `McpServerConfig`. It coupled the embedder's internal dispatch
  to the MCP protocol shape unnecessarily and introduced a breaking change
  to `McpServerConfig`. A native tool with per-call grading is simpler and
  does not change any MCP type.
- **Registering the host's tools as native tools (one per function).** They
  would be advertised on every round, outside any envelope, and the
  per-session resolver would have to add/remove tools instead of just
  listing names.
