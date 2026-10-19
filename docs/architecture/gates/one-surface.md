# Gates & host tools §8 — One advertised surface, error taxonomy & removed sources

> Part of the [gates & host tools module](../gates-and-host-tools.md) of the [architecture overview](../../architecture.md).

### One advertised surface and discovery — [ADR-0226](../../adr/0226-one-advertised-surface-mcp-envelope-registered-is-available.md)

(Supersedes [ADR-0196](../../adr/0196-tool-search-and-lazy-discovery-replace-the-invoke-envelope.md)
§1–§4 and [ADR-0204](../../adr/0204-invoke-fallback-for-client-side-discovery.md).)
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
([ADR-0227](../../adr/0227-one-binding-path-namespaces-http-skill-libraries.md),
[ADR-0228](../../adr/0228-rhai-path-and-args-skill-tools-frontmatter-removed.md)):
the surface is exactly `KERNEL`.

**The six discovery tools** — each with no argument or one `name`, all
`Capability::Control` and non-maskable — the `poll` pattern: a Control
tool other than `agent`/`agent_send` is never graded ([ADR-0207](../../adr/0207-permission-modes-replace-agent-borne-authority.md) §3;
`tool_names::DISCOVERY_TOOLS`, [ADR-0226](../../adr/0226-one-advertised-surface-mcp-envelope-registered-is-available.md)): the dispatch ladder's Control path
returns before the mode, overlay or ceiling are consulted, so nothing can
withdraw or decline them.

**Which Control tools are graded** ([ADR-0232](../../adr/0232-spawning-is-graded-bookkeeping-control-tools-non-maskable.md), #849). The ungraded
ones are the bookkeeping tools — `poll`, `ask_user`, `update_tasks`,
`request_mode` (plus the discovery tools above): each only collects or
reports work the mode already authorized, or asks the user, so the dispatch
`Control` path (`capability::bypasses_grading`) or the runtime-owned
`Intercept` route skips the overlay, the mode and the ceiling. `agent`/
`agent_send` are the exception: still `Capability::Control`, but a spawn
starts a session that runs tools, so `orchestration::spawn`/`agent_send`
climb the same ladder as a host tool (`Dispatcher::gate` — overlay deny,
grade + grants, `pre_tool_use`, mode deny, the unattended collapse, the
approval park) before launching. A refusal is an ordinary `is_error`
result, and no child starts. The spawn slot is reserved first (the guard is
loop state; the ladder may park in the detached task) and released on a
refusal. The child's event receiver is taken only once the gate clears, so a
long approval park can't lag it past the outbox capacity. A session that
ends or hibernates unwinds the cards of its turn
(`PendingDecisions::stop_attached` in the executor's lifecycle fold): tool
and spawn cards, `ask_user` questions, plan/mode cards, a blocking script's
nested cards. A resume re-offers a parked `ToolExec`. A background
(`detached`) script's nested card is exempt there. It is no `ToolExec`, so
nothing would re-offer it, and the script outlives the session (a compaction
successor inherits it,
[ADR-0231](../../adr/0231-compaction-successor-inherits-background-work.md)).
A session `Stop` is different: it unwinds every card of the session,
background ones included (`stop_session`). `Stop` is the user's explicit
escape hatch and the TUI clears its card view on it, while an attended park
never times out. The card itself says it is detached (`ToolRequest.detached`,
[ADR-0237](../../adr/0237-detached-flag-on-tool-request.md)), so the TUI keeps
it past the turn's terminal `Status`. The script itself is not aborted
([ADR-0185](../../adr/0185-rhai-joins-background-and-poll.md)): its nested call
fails with "rhai run stopped" and the run ends. The built-in modes class-allow `control`, so a spawn runs
unprompted by default and a rule naming `agent` outranks that.
`propose_plan` grades only its own `plan` class (`resolve_effective`, before
any file is touched): that, not the Control path, keeps it in `plan` mode.

**Non-maskable names** (#843, `tool_names::NON_MASKABLE_TOOLS`) — the six
discovery tools, the `mcp` envelope (core unwraps it to
`mcp__<server>__<tool>` before dispatch, so the gates never see `mcp`),
the script-only `read_raw`/`glob_json`/`grep_json` (graded as
`read`/`glob`/`grep`, `grade::graded_name`) and the bookkeeping Control
tools `poll`/`ask_user`/`update_tasks`/`request_mode`/`propose_plan`
(#849): an overlay entry or a mode/ceiling rule may spell one but can
never match it. For `propose_plan`, the one member that is graded at all,
this is enforced by the matcher: `mode::rules::best_match` skips a rule
naming a non-maskable tool, so only its class or a `*` rule decides, and the
built-in `deny: [plan]` class rule never warns. The runtime says so
instead of accepting a silent no-op: the TUI's toggle roster
(`engine_setup::toggle_roster` — `/tools`, the bare-`/enable` checklist, the
`/set` Tools tab) leaves them out, `/enable`/`/disable` refuses one with
`tool_names::non_maskable_reason`, a `modes:`/`permissions:` rule naming one
warns at config load (`config::mode_warn`, alongside the stale-name check),
and a wire `SetToolOverlay` entry naming one is applied as sent — core holds
no tool policy to refuse it — with a runtime warning logged when the
executor folds the `ToolOverlayChanged` (`tool_runner::overlay_warn`).
Per-tool MCP control is a `mcp__<server>__<tool>` rule or overlay entry (a
server-wide `mcp__<server>__*` glob only in the overlay — rule keys match
the tool part exactly, so a mode/ceiling rule spelling a name glob also
warns as never matching): it grades an envelope call and a script's
`mcp::<server>::<tool>()` binding identically, since both reach
`Dispatcher::grade` under the inner name; a server the user never wants is
removed from config. `list_agents`, `list_models` and `list_skills`
render static rosters (`discover::lists`); `load_skill` is the registry tool;
`list_mcp`/`mcp_tools` are the MCP pair (§10). The five non-registry ones
route through `Intercept::Discover`. Discovery is **pull-only**: the system
prompt carries no roster, only the static `system_prompt::TOOL_CONVENTIONS`
line (direct calls for the advertised tools, `mcp(server, tool, args)` for
MCP, `rhai` for anything else).

**The `mcp` envelope** — `mcp {server, tool, args}` (`discover::mcp_spec`;
the name `MCP_TOOL` is shared through the leaf crate) is the only way to call
an MCP tool. Core unwraps it to `mcp__<server>__<tool>` before any event and
keeps the emitted call in `Context` byte for byte ([engine](../engine.md)), so
every gate, hook, approval and `arg_validate` sees the inner tool. A
`ToolExec` still named `mcp` (a malformed envelope core refused) is declined
where an unknown tool is, via `tool_advertising::unknown_tool_reply`, on the
ADR-0176 `is_error` channel with the envelope's shape and schema. Calling an
`mcp__*` name natively is not advertised but dispatches and grades the same.

### Error taxonomy — five cases — [ADR-0196](../../adr/0196-tool-search-and-lazy-discovery-replace-the-invoke-envelope.md) §6

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
call that reaches the tool ([ADR-0226](../../adr/0226-one-advertised-surface-mcp-envelope-registered-is-available.md)). Two guards ride alongside: per-session
delivered-schema tracking (`tool_advertising::DeliveredSchemas`, marked by
every `mcp_tools` delivery and every decline to the model — never re-send a
schema already in context; reply with the diff instead) and a
two-identical-failures loop breaker (a repeated identical failure means the
schema isn't the problem — stop retrying the same shape). Both are about the
**model's** context, so only `Via::Model` calls touch them (#844): a script's
binding call (`Via::Script`) is validated the same way but never marks a
schema delivered, never consults the dedup, and never feeds or resets the
loop-breaker streak. Its decline (`arg_validate::Reader::Script`) always
carries the full schema. An MCP tool's example is the rhai call that reaches
it: the `mcp::<server>::<tool>` path the binding itself was called through,
carried on `dispatch::Call::rhai_path` (a server named `a__b` makes the
registered name ambiguous to split), with `rhai_ident::rhai_literal` args —
never the `mcp` envelope a script cannot call. A native tool's example is
labelled as the tool input its positional binding builds.

Parallel tool calls are unaffected by any of this: ADR-0061's batch
`ToolExec` parking already resolves a batch of calls in any order with every
call guaranteed a paired result, so there is no envelope-shaped concurrency
question to answer here.

### Definition-driven tool sources — removed

The #560 P8 sources are gone: `config.yml` `endpoints:` and the skill-tool
`endpoint` kind ([ADR-0227](../../adr/0227-one-binding-path-namespaces-http-skill-libraries.md)),
then the whole `SKILL.md` `tools:` frontmatter — its preset-script `rhai`
and `alias` kinds, `AliasTool`, `Tool::alias_rewrite` and the ladder's
alias-rewrite step ([ADR-0228](../../adr/0228-rhai-path-and-args-skill-tools-frontmatter-removed.md)).
A skill ships callable behavior as `lib.rhai` functions (§Script surface) and
predefined scripts as files run with `rhai` `path`. A stale `endpoints:` key
warns and is ignored; a native `SKILL.md` still declaring `tools:` is skipped
with an error-level log naming the replacement — the rest of the registry
(and the engine) still loads. A foreign skill dir's `tools:` is dropped like
any unknown key (ADR-0074).

---

Previous: [§8 — `web_fetch` and undo/rewind](web-fetch-and-undo.md) · Next: [§8 — Script surface](script-surface.md)
