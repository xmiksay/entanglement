# 0227. One binding path: every rhai call through the dispatch seam, `mcp::`/`skill::` namespaces, `http()` with hidden credentials, endpoint tools removed

- Status: Accepted
- Date: 2026-10-12
- Issue: #819 (part 2 of epic #822; follows [ADR-0226](0226-one-advertised-surface-mcp-envelope-registered-is-available.md))
- Amends: [ADR-0046](0046-rhai-sandboxed-script-tool.md) (the binding set and
  how a binding is graded), [ADR-0115](0115-rhai-exec-bindings-call-bash.md)
  (the per-command approval cache becomes per exact call for every tool; the
  exec timeout clamp is joined by a bridge-level clamp for every other
  binding), [ADR-0130](0130-rhai-exec-bindings-marshal-workdir.md) (the
  workdir-aware grade now comes from the shared seam, not `BindingPolicy`),
  [ADR-0225](0225-web-fetch-grades-redirect-hops.md) (its `check_hop` grades
  `http()` redirect hops too), [ADR-0119](0119-rhai-bindings-route-through-the-escape-root-gate.md)
  (the escape-root gate for bindings is now the seam's own),
  [ADR-0098](0098-rhai-json-yaml-loader-and-read-raw.md)/[ADR-0206](0206-script-facing-structured-search-bindings.md)
  (`read_raw`/`glob_json`/`grep_json` grade as their base tool via the seam's
  `graded_name`)
- Supersedes: [ADR-0129](0129-thread-the-skill-mask-into-rhai-binding-resolution.md)'s
  per-run binding snapshot (`BindingPolicy`; its skill-mask half was already
  retired by [ADR-0194](0194-skills-are-additive-only.md)), and the #560 P8
  declared-endpoint tool source (`config.yml` `endpoints:`, `endpoint__<name>`,
  the skill-tool `endpoint` kind, core's `call_endpoint`)

## Context

ADR-0226 left `rhai` as the single pluggable surface for everything that is
not a kernel tool or an MCP call — but a script's bindings still ran through
a second, smaller grading path. `BindingPolicy` snapshotted the mode, overlay
and ceiling per run and graded each binding; the host tool then ran straight
from the registry. Hooks never fired for a binding, a binding's `edit`
produced no `FileChange`, arguments were not validated, an MCP tool was not
reachable at all, and the approval cache was "once per function" for
everything but `call`/`bash`. Every gate added to the direct-call ladder
(grants, escape-root, the unattended collapse, lazy MCP connect) had to be
re-implemented there or silently missed.

Separately, `config.yml` `endpoints:` declared HTTP tools (`endpoint__<name>`)
with credentials in plain headers, advertised next to the kernel only as an
interim (ADR-0226 §1). A skill wanting an authenticated API had no other way.

#826 measured that GLM models do not chain calls inside one script unless
shown how (8/60 signature-listing runs, 0/58 schema-listing runs on the
compose task), and that signature-only listings were slightly more accurate
than schema + example (99 % vs 96 % args ok).

## Decision

### 1. The dispatch seam

The executor's `run_and_reply` is split into **grade + execute → outcome**
(`tool_runner::dispatch::Dispatcher::run`) and **reply**
(`Dispatcher::dispatch`). `run` returns `Outcome::Refused(text)` (a gate
declined it — it never ran), `Outcome::Ran {content, is_error, duration_ms,
exit_code}`, or `None` (a `Stop` unwound it). A model call replies to core
with it; a script binding returns it over the bridge, so core never receives a
`ToolResult` for an id it never parked on. `dispatch.rs` keeps the types; the
ladder lives in `dispatch/gates.rs`, the approval park in `dispatch/decision.rs`,
execution in `dispatch/execute.rs`, the script-side state in
`dispatch/script.rs`.

### 2. One binding path; `BindingPolicy` retired

Every script binding call goes through `Dispatcher::run` with `Via::Script`:
mode grade, overlay enable/deny, config ceiling, grants (lookup and record),
the escape-root gate, `pre_tool_use`/`post_tool_use` hooks, pre-dispatch
argument validation, lazy MCP connect (including per-user scopes), `FileChange`
capture and the in-call grader (redirect hops). A script-facing read variant
grades as the tool it rides on (`read_raw`→`read`, `glob_json`/`grep_json`→
`glob`/`grep`, `grade::graded_name`). The `Dispatcher` a script runs against
is itself a snapshot taken at launch — registry, chain, overlays, mode — so a
mode switch or overlay change mid-script reaches the next script, not the
running one (as `BindingPolicy`'s snapshot did). The differences from a model
call are only these:

- the approval card is `ApprovalKind::Rhai` with tool `<tool> (rhai)`, and a
  background script suppresses session-state transitions;
- the **per-run approval cache keys on tool + full input** for every tool —
  approving `write(a.txt, x)` covers an identical repeat, never `write(b.txt)`;
- a nested call runs under a bridge-level timeout clamped to the script's
  remaining budget less a short grace (so the script can still handle the
  error); exempt are `bash`/`call`, whose `timeout` parameter is already
  derived from the budget (#419), and the file-mutating `write`/`edit`/
  `apply_patch`, which a dropped future could cut between backup and write
  (ADR-0212). A cut call is `Outcome::TimedOut` — "may or may not have taken
  effect", never the "it never ran" of a refusal;
- a refusal is a catchable exception; a run's text comes back as the value —
  except `http()` and MCP tools, whose failed run throws;
- nested `rhai`, the `mcp` envelope, and every runtime-owned or `Control`
  tool (`agent`, `agent_send`, `poll`, `ask_user`, `propose_plan`,
  `request_mode`, `update_tasks`, `list_*`, `load_skill`, `mcp_tools`) are
  unreachable — refused at the seam (`dispatch::script::unreachable`), and
  never bound in the first place.

`rhai`'s own launch gate is unchanged (resolved through the same resolver and
chain, its approval card carries the script).

### 3. Generated namespaces

Each run registers static Rhai modules generated from the session's view of
the registry:

- `mcp::<server>::<tool>(#{...})` (and a zero-argument overload) for every
  tool of a connected server, plus — unscoped sessions only — the last-seen
  tools of an unconnected one (the call connects it lazily). Each call is
  graded and executed under its real registered `mcp__<server>__<tool>` name
  (part 1's `mcp_tool_name`). A scoped session lists only its scope's servers
  and never reads the shared cache (ADR-0188).
- `skill::<skill>::<fn>(...)` for every skill's compiled `lib.rhai` (§5).

Identifier mapping (`rhai_ident`): non-identifier characters → `_`; a result
Rhai does not accept as an identifier (no letter before the first digit, or
no letter at all — `2fa`, `_2fa`, `--`) gets a `t_` prefix; a keyword or
reserved word gets a `_` suffix (`mcp::srv::call(...)` would not parse).
Validity comes from Rhai itself (`is_valid_identifier`/
`is_valid_function_name`), not a hand-kept list. One naming pass
(`rhai_ident::plan_mcp`/`plan_skills`, deterministic name order) drives the
registration *and* both listings: a collision after mapping logs and skips
the later entry, and `mcp_tools`/`load_skill` list it as `no rhai name
(collides with <winner>) — use mcp(...)` (a skill lib: not callable from a
script) rather than under a name that reaches the winner. `mcp_tools` shows
each tool's mapped name next to the real one.

### 4. `http(method, url, #{headers?, body?}) -> string`

A registered, never-advertised `http` tool (`Capability::Exec`) behind the
binding. Its permission argument is `"{METHOD} {url}"` (both normalized as
sent), so `http(GET https://api.github.com/*): allow` allows reads only. It
rides `web_fetch`'s SSRF-guarded client (`WebFetcher::request`, which
`fetch_checked` now delegates to) — not the endpoint pool, whose per-base-URL
resilience state does not fit arbitrary hosts (ADR-0220). Every redirect hop
is graded through the in-call grader as a fresh `http` call (ADR-0225's
`check_hop`); `Ask` stops rather than parking. 303 (and 301/302 for a
non-GET) become a bodyless GET; 307/308 keep method and body.

Hidden credentials — `config.yml` `http_auth:`:

- **user layer only**: a project layer declaring it is dropped before the
  merge with a warning. The project layer is trusted *code* (ADR-0047), but a
  repo's `lib.rhai` must not spend the user's tokens silently;
- maps a host to `headers:`/`cookies:` (cookies fold into one `Cookie`
  header); `${VAR}` resolves from the environment, where the managed `.env`
  lands at startup (process env wins, as for provider keys);
- matched on the URL host only — exact or dot-boundary suffix, longest key
  wins; `api.github.com` never matches `api.github.com.evil.io`; never path;
- injected at send time, **over https only, on the original host only** — the
  per-hop seam drops them on a redirect to another host or to plain http;
- a script-supplied `Authorization`/`Cookie`/`Proxy-Authorization` header is
  refused, and so are `Host`/`X-Forwarded-Host` (they would route a request
  matched and credentialed for one host as another) and URL userinfo
  (`user:pass@host`, a second credential channel); a map/array `body` is sent
  as JSON with `Content-Type: application/json` unless the script set one;
- the result is the response body alone (no `Set-Cookie`/`Authorization`
  echo); error text is scrubbed of every secret value — each expanded
  `${VAR}`, each cookie value and each whole header value — so a server
  echoing the bare token in a non-2xx body does not leak it. A token echoed
  in a *successful* body is out of reach — accepted;
- **the environment is scrubbed from children**: the managed `.env` exports
  a referenced `${VAR}` into this process so the tool can resolve it, so
  every referenced name is hidden (at config load, and again by any
  `HttpGate`) from every spawn — `bash`/`call`, MCP stdio servers, hooks, the
  status-line command, the editor/clipboard/browser launches (`secret_env`) —
  and `bash("printenv GITHUB_TOKEN")` reads nothing. That is all it closes:
  the managed `.env` file itself and the parent process's
  `/proc/<pid>/environ` stay readable by the exec tools (`bash("cat
  ~/.config/entanglement/.env")` needs no approval in `build`, and bubblewrap
  binds `/` read-only) — the same accepted exposure the provider keys have,
  tracked in #832;
- a **credentialed call forces one approval per host per session** whatever
  the mode grades (the escape-root pattern — `build`'s class-wide `exec`
  allow does not cover it), and the card says `authenticated for <host>`;
  later calls to that host in the session ride it (`http_gate::HttpGate`). In
  an unattended mode (`question_timeout` set, i.e. `auto`) it collapses to a
  refusal unless an explicit, argument-scoped `http(...)` rule in the mode
  allows it (`mode::rules::scoped_grade`). Provenance is tracked per mode: a
  rule only a project layer's `modes:` declares still grades, but is skipped
  for this unlock with a load warning — the same reason `http_auth` itself is
  user-layer only — while the same rule text in the user config (or a
  built-in rule) counts. Approving the host card also earns the script's
  per-run cache entry and any `Session`/`Always` grant the user chose, like a
  plain approval;
- one gate, one tool: `ToolExecutorBuilder::http_gate(gate)` registers the
  `http` tool over that same gate (`HttpTool::new` is crate-private), so an
  embedder cannot wire injection to one gate and the forced approval to
  another; without it there is no `http` tool;
- `http_auth` is read at startup only — not live-reloaded; a change needs a
  restart. A key with a scheme, port, path or wildcard never matches and
  warns at load.

`web_fetch` stays a separate kernel tool (page reading, a future headless
render) and never injects credentials.

### 5. Skill libraries

A `lib.rhai` beside `SKILL.md` — any layer, foreign skill dirs included —
compiles once at skill discovery into a static module (`skills::lib_rhai`),
registered in every run as `skill::<skill>`. Only `fn` definitions are allowed:
a top-level statement would run on every script launch unasked, so it rejects
the whole lib with a warning (never a partial load). The module is
encapsulated — a lib function resolves its siblings in the lib and the
engine's bindings and namespaces at call time; `private fn` stays private. It
is live-reloaded with the skill registry. `load_skill` lists the public
signatures after the body.

### 6. Endpoint tools removed

`config.yml` `endpoints:` (retired-key warning, `config::retired`),
`endpoint__<name>`, `entanglement_runtime::endpoint`, the provider's
`endpoint` module and core's `call_endpoint`/`EndpointMethod`/
`EndpointResponse`/`ENDPOINT_RESPONSE_CAP` re-exports, the skill-tool
`endpoint` kind, and ADR-0226's interim `endpoint__*` advertisement. A skill
expresses the same thing as one `lib.rhai` function calling `http()`. A
native `SKILL.md` still declaring `kind: endpoint` keeps loading — the entry
is skipped at registration with a warning naming the replacement, never a
startup failure.

### 7. Listings and the chained example

The `rhai` kernel spec's binding reference and the embedded `rhai` skill
document `http()`, both namespaces, and carry a chained-call example (search
→ read in one script, the first result passed into the second call).
`load_skill` and `mcp_tools` render rhai-callable functions signature first,
schema second.

Nested-call visibility stays `ToolRequest` (on `Ask`), `FileChange`, hooks and
the script result; a per-nested-call `OutEvent` is #821.

## Consequences

- One ladder to reason about and test: a gate added for direct calls covers
  scripts for free; hooks and `FileChange` are observable from a script.
- A script can do in one round what took several `mcp(...)` rounds — the
  reason the surface exists — and a skill can ship callable code.
- Credentials live in the user's config; the model never receives them
  through `http()` or a child's environment, a repo cannot route them, and
  the first use per host is always seen. They are not secret from the exec
  tools, though: the managed `.env` and the parent's `/proc/<pid>/environ`
  remain readable — accepted risk shared with provider keys, tracked in
  #832.
- An unattended run cannot make a credentialed call without an explicit rule.
- Embedders lose `BindingPolicy`, `entanglement_runtime::endpoint`,
  `entanglement_core::call_endpoint` and friends; `register_skill_tools`
  drops its `HttpClient` parameter; `SkillMeta` gains `lib`;
  `ToolExecutorBuilder::http_gate` wires the gate and registers the `http`
  tool (default: no `http` tool).
  See the migration note in [embedding](../embedding.md).
- The runtime's `rhai` dependency enables Rhai's `internals` feature (for
  `AST::statements`, the fn-only check).

## Alternatives considered

- **Keep `BindingPolicy` and add the missing gates to it.** Two ladders that
  must change in lockstep is how they drifted in the first place.
- **A generic `call(tool, args)` binding instead of namespaces.** One
  function, but no per-tool names for the model to read in a listing, and a
  string-keyed back door to runtime-owned tools to police.
- **`http()` on the shared endpoint pool.** It would mint persistent
  resilience state per arbitrary host and has no SSRF guard; `web_fetch`'s
  guarded client already solves both.
- **Project-layer `http_auth`.** Convenient for team setups, but it would let
  a repository attach the user's token to any host its own scripts call.
- **Graded argument = URL only.** Cannot express "reads only"; the issue's
  `{METHOD} {url}` costs one `*` in a rule to cover every method.
- **Loading a `lib.rhai` with top-level statements by dropping them.** Silent
  partial loads hide author mistakes; rejecting the file is loud and simple.
