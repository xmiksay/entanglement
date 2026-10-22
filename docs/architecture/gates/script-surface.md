# Gates & host tools §8 — Script surface

> Part of the [gates & host tools module](../gates-and-host-tools.md) of the [architecture overview](../../architecture.md).

### Script surface — namespaces, `http()`, skill libraries — [ADR-0227](../../adr/0227-one-binding-path-namespaces-http-skill-libraries.md) (#819)

**The dispatch seam.** `tool_runner::dispatch` splits the old
`run_and_reply` into `Dispatcher::run` (grade + execute → `Outcome::Refused`
/ `Outcome::Ran` / `None` on Stop) and `Dispatcher::dispatch` (reply to
core). A model call takes both; a script binding takes only `run` and gets
its outcome over the bridge (`script::service`), so core never sees a nested
call. The ladder (`dispatch/gates.rs`): script reachability → resolve the
tool (`dispatch/resolve.rs`: scoped MCP overlay, lazy connect — a script waits
on it no longer than its budget, §Dispatch-time lazy connect) → `Control` bypass
(every Control-only tool but `agent`/`agent_send`, ADR-0232) →
overlay deny → grade + grants → `pre_tool_use` → mode deny → escape-root →
credentialed-`http` gate → run / unattended collapse / park
(`dispatch/decision.rs`) → execute (`dispatch/execute.rs`: state tools,
arg validation, in-call grader, `FileChange` capture, script budget, loop
breaker, `post_tool_use`).

**Live nested-call reports** ([ADR-0233](../../adr/0233-live-only-nested-call-events-for-rhai-scripts.md), #821,
`script::activity`). The bridge's service loop is the one place every binding
of every run passes, skill-library and `mcp::` calls included. It times each
`Dispatcher::run` and reports it as a live-only `OutEvent::NestedCall`:
`parent_request_id` (the `rhai` call's id), a 1-based `index`, the tool, an
args summary, wall-clock `duration_ms` including any approval wait, and
`is_error` for a refusal, timeout or failed run. The first
`NESTED_CALL_EVENT_CAP` (50) calls of a run are reported one by one; later
ones are only counted. When the run ends, one
`NestedCallSummary{suppressed, errors}` follows if any were held back. The
reporter emits it on drop, so a `Stop` that aborts the bridge task mid-call
still sends it, with the cut-off call counted as failed. `narrate` narrates
only a run's first report (`index == 1`).

The args summary never carries a secret:
- for `bash`/`call`, only the program's basename plus ` …`;
- for any other tool, its permission argument (a path, `{METHOD} {url}`), with
  every URL's userinfo, query and fragment cut;
- for a tool with no permission argument, the input's key names only.

It is one line of at most 80 characters, computed only for a reported call.
`http_auth` credentials never appear in a script's input, because the `http`
tool adds them at send time. The reports are never persisted or replayed (see
[protocol §live-only events](../protocol/messages.md)). The TUI folds them into their
`rhai` op, which shows every line while expanded and only the latest line
while collapsed, with a `· N calls` count in its header. An export leaves them
out. A
resumed transcript shows the script's call and result without them.

**Namespaces** (`script::namespaces`), generated per run:
`mcp::<server>::<tool>(#{…})` for every tool of a connected server — and, for
an unscoped session, the last-seen tools of an unconnected one, which the
call connects lazily — graded and executed under the real
`mcp__<server>__<tool>` name; a scoped session lists only its scope's servers
and never the shared cache (ADR-0188). `skill::<skill>::<fn>(…)` for every
skill's `lib.rhai` — every skill is model-visible, there is no hidden-skill
filter ([ADR-0229](../../adr/0229-every-skill-is-model-visible-user-only-removed.md)). Identifier mapping (`rhai_ident`): non-identifier
characters → `_`, a result with no letter before its first digit (or no
letter at all) gets a `t_` prefix, a Rhai keyword/reserved word a `_` suffix
— validity per Rhai's own `is_valid_identifier`/`is_valid_function_name`; one
shared pass (`rhai_ident::plan_mcp`/`plan_skills`) drives registration and
the `mcp_tools`/`load_skill` listings, so a collision logs, skips the later
entry and lists it as `no rhai name (collides with <winner>)`.
`mcp_tools` renders each tool signature-first — `in rhai: mcp::…(#{…})`, the
`mcp {…}` call line, then the schema.

**`http(method, url, #{headers?, body?})`** — the registered, never-advertised
`http` tool (`host::http`, `Capability::Exec`). Permission argument
`"{METHOD} {url}"` (normalized as sent): `http(GET https://api.github.com/*)`
allows reads only, `http(* https://api.github.com/*)` every method. Rides
`web_fetch`'s SSRF-guarded client (`WebFetcher::request`); every redirect hop
is graded as a fresh `http` call (ADR-0225's `check_hop`), `Ask` stops it.
Returns the body; a non-2xx status is an error (thrown in the script).

**Hidden credentials — `config.yml` `http_auth:`** (`config::http_auth`,
`http_gate::HttpGate`): host → `headers:`/`cookies:` with `${VAR}` from the
environment (the managed `.env` lands there at startup). **User layer only** —
a project layer's is dropped before the merge with a warning. Matched on host
only (exact or dot-boundary suffix, longest key wins), injected per hop over
https to the original host only — a redirect elsewhere goes bare. A
script-supplied `Authorization`/`Cookie`/`Proxy-Authorization`/`Host`/
`X-Forwarded-Host` header or URL userinfo is refused; a map/array body is sent
as JSON with `Content-Type: application/json` unless the script set one;
response headers never reach the script; error text is scrubbed of every
expanded `${VAR}`, cookie value and whole header value (a token echoed in a
successful body is out of reach — accepted). Every referenced `${VAR}` is
scrubbed from every child's environment (`secret_env`, hidden at config load:
`bash`/`call`, MCP stdio, hooks, status line, editor/clipboard/browser), since
the managed `.env` exports it into this process. The managed `.env` file and
the parent's `/proc/<pid>/environ` remain readable by the exec tools — an
accepted risk shared with provider keys, tracked in #832. A credentialed call **forces one
approval per host per session** regardless of the mode grade (card:
`authenticated for <host>`; approving it also earns the script cache entry
and any wider grant); in an unattended mode it is refused unless an explicit,
argument-scoped `http(...)` rule allows it (`mode::rules::scoped_grade`),
provenance tracked per mode: a rule only a project layer declares grades but
never unlocks (load warning); the same rule in the user config does. `ToolExecutorBuilder::http_gate` registers
the `http` tool over the same gate the seam checks — one source. `http_auth`
is read at startup only (not live-reloaded); a key with a scheme, port, path
or wildcard warns at load. `web_fetch` never injects.

**Skill libraries** (`skills::lib_rhai`): a `lib.rhai` beside `SKILL.md`
(any layer, foreign dirs included) compiles once at discovery into an
encapsulated static module, registered as `skill::<skill>`; only `fn`
definitions — a top-level statement rejects the whole lib with a warning;
an `import` inside a lib function resolves per run through the run's module
resolver ([ADR-0248](../../adr/0248-importable-rhai-library-modules.md)).
Live-reloaded with the skill registry; `load_skill` lists the public
signatures after the body, each with its `///`/`/** */` doc comment (read
by a text pass, `skills::lib_docs` — not Rhai's `metadata` feature, which
rejects any doc-style comment not followed by `fn`, [ADR-0228](../../adr/0228-rhai-path-and-args-skill-tools-frontmatter-removed.md)).

**Importable modules** ([ADR-0248](../../adr/0248-importable-rhai-library-modules.md),
`skills::rhai_modules`, `script::modules`): `import "json_schema" as js;`.
`<name>.rhai` files in three layers, with a later layer winning on a name:
built-in (embedded) < `${config_dir}/entanglement/rhai/modules/` <
`<root>/.entanglement/rhai/modules/`. A sub-dir gives a slash name.
- The run's `Resolver` looks a bare name (`[a-z0-9_]` segments joined by `/`)
  up in the discovered set and never touches a file. A path, `..` or an
  extension is refused. `set_max_modules(32)` caps a run's executed
  `import`s.
- A module holds `fn`s plus top-level `import "<name>" as x;`. Any other
  statement, an `import` inside a function, or a typed method (`fn int.f()`)
  rejects it, and the rejected file still claims its name.
- Modules are compiled once at discovery. Module-to-module imports resolve
  there too (`rhai_modules::graph`, iterative): a cycle rejects its members
  (one warning naming the path), a chain deeper than 8 is rejected, a
  diamond shares one `Shared<Module>`, and names resolve through the
  winning layer at every level, so a broken shadow rejects its importers.
  An importing module carries its imports only in its functions' environ,
  never as sub-modules (`lib_rhai::with_imports`), so indexing stays
  linear across an import lattice.
- The set rides in `SkillRegistry::modules`, so the definitions watcher
  reloads it (a `lib.rhai` or a file under `rhai/modules/` counts as
  definition content; other `*.rhai` files do not).
- Module code runs in the caller's posture: bindings resolve when called.
- Only `load_skill rhai` lists the modules (layer, imports, signatures with
  docs). The tool spec stays static.
- Built-in: `json_schema` (`validate(value, schema)`, a JSON Schema subset,
  error strings with JSON-pointer paths).

**`rhai(path)` and `args`** ([ADR-0228](../../adr/0228-rhai-path-and-args-skill-tools-frontmatter-removed.md),
`script::input`/`script::gate`): `path` names a script file — relative paths
are rewritten onto the session workdir first (ADR-0224) — that the runtime
reads off the async runtime (256 KiB cap on the bytes read) *before* the
card, shows on the launch card (headed `// path: "<quoted>"`/`// args:`)
and evaluates verbatim; the model never reads it. An inline script's card
opens with the runtime's own `// inline script` line instead, so script
text can never pass itself off as a path card. A head tells the two apart
by the card's `reason` (`script_launch` / `inline_launch`), and an inline
card's `scopes` are `[once]`, so no path grant is offered for it
([ADR-0230](../../adr/0230-structured-card-reason-and-honest-scopes-on-tool-request.md)). The read canonicalizes
once, opens that canonical path (non-blocking, no final-symlink follow — a
FIFO is refused as "not a file" instead of hanging the launch), checks the
opened file's `(dev, ino)` against the canonical path's and reads from that
descriptor; on Linux the canonical path is then re-read from the descriptor
itself (`/proc/self/fd`), so an intermediate directory swapped for a symlink
between canonicalize and open cannot split the two (elsewhere that narrow
residual window remains). Trust is judged on that canonical path, so the
trust decision and the text come from one file. **Only a script is ever
run by `path`**: a `path` without a `.rhai` extension (any case) is declined
at validation, and a canonical target without one (a `x.rhai` symlink to
`x.txt`) is refused at the read — so trust never extends to a non-script
file in a trusted folder (`rhai_trust::is_script`, shared with the write
floor).

**The launch** (`script::gate`) is graded like any tool
([ADR-0228](../../adr/0228-rhai-path-and-args-skill-tools-frontmatter-removed.md)):
`Dispatcher::grade` — the overlay deny (`/disable rhai` is an ordinary
deny), then the overlay enable entry or the mode's rules, the ceiling and
grants — then the `pre_tool_use` veto, all before the file is opened. The
rule/grant argument is the script's **canonical** `path`, root-relative
inside the root and absolute outside it (`permission_arg` +
`permission_path::rooted_arg`, which canonicalizes for `rhai` alone; the
gate rewrites the input's `path` to that form before grading, against the
escape-root policy's root or, with none wired, the launch's project root), so
`rhai(scripts/deploy/*)` matches and a link in `scripts/deploy/` grades as
where it points; the file read must be the file graded. An inline script
matches only a bare `rhai` rule. Then:

| grade / script | launch | nested calls |
| --- | --- | --- |
| Deny (mode, ceiling, overlay deny, hook veto) | refused, a catchable error — a trusted path too | — |
| `path` inside a trusted folder (`TrustedFolders::runs_trusted`) | no card | force-allowed (`Posture::Trusted`) |
| Allow (mode rule, overlay enable, grant) | no card | graded like direct calls (`Posture::AskPerCommand`) |
| Ask | one card: **Run all** · **Ask per command** · **Reject** (`InMsg::Approve.run_all`) + scope | Run all → force-allowed (`Posture::RunAll`); Ask per command → graded like direct calls |

A `Session`/`Always` approval of an `Ask` card records a grant for the
**exact path** (`rhai`, the canonical root-relative path), so the next
launch grades Allow — no card, Ask per command; an inline script's
approval is once only, and `run_all` is never recorded. Built-in grades:
`build` asks (`prompt: [rhai]` — a rule naming the tool outranks the class
allows, [agents & permissions](../agents-and-permissions.md)), `research`/`plan`
deny (class `write` deny), `auto` allows. **No answerer**: an `Ask` card
waits for the head unless the mode has `question_timeout`; on that timeout
it runs Ask per command (an out-of-root launch is refused instead).
`skutter run` answers a launch card at once with Ask per command and rejects
every nested card. It decides on the card's `reason`, never its text
(`pending::headless_starts`, [ADR-0230](../../adr/0230-structured-card-reason-and-honest-scopes-on-tool-request.md)):
only `script_launch`/`inline_launch` start, so an out-of-root launch
(`launch_escape`) is refused.

*Force-allowed* is a branch of the shared seam (`Dispatcher::clear` with a
`run_all` script): the overlay deny, the hook veto and a mode/ceiling
`Deny` still refuse (a catchable exception), and every later gate passes — a
graded `Ask`, the escape-root gate (a single-use token the host tool
spends), the credentialed-`http` per-host approval — except a **floored
write** under Run all, which gets its own nested card (below).
`rhai.trusted` (`config::rhai_trusted` → `rhai_trust::TrustedFolders`) is
read from the user **and** project layers — the lists add up, project
entries relative to the root, user entries to the user config's folder (`~`
expands) — and canonicalized at load, so a symlink inside a trusted folder
pointing out is judged by its target. A **user-layer** folder is not
trusted in a mode the project config gives `prompt:` rules — in every mode
when the project's `permissions:` ceiling has a `prompt` list or
`default: prompt`. The whole script is then graded and carded like any
other. Project-layer trust always applies, and a folder both layers list is the project's. Read at
startup only. This deliberately lets a repository unlock credentialed
`http()` for its trusted scripts (amends ADR-0227 §4). A non-trusted `path`
outside the root shows the escape-root warning on the same launch card (no
second prompt; an Allow grade then asks too). With no escape-root policy
wired, **every** out-of-root `path` is refused outright — a trusted one
too. A trusted / Run-all script's in-call grader (`CallGrader::run_all`)
passes an `Ask`-graded `http()` redirect hop the way the call itself
passed; a `Deny` hop still stops it. `skutter inspect config` lists each
trusted folder with the layer that listed it, and the modes where the
user layer's trust is suppressed.

**Trusted folders must not be agent-writable.** A script inside one runs
force-allowed, and a skill's `SKILL.md`/`lib.rhai` or a module is loaded by
every later session, so writing one is as good as running it: a `write`/`edit`/
`apply_patch` whose target is a `*.rhai` file inside a trusted folder or a
user/project module dir (`skills::module_dirs`, wired by
`ToolExecutorBuilder::module_dirs`, [ADR-0248](../../adr/0248-importable-rhai-library-modules.md)), or a
`SKILL.md`/`lib.rhai` inside any user or project skill layer dir
(`skills::skill_dirs` — `.entanglement/skills`, `.claude/skills`,
`.agents/skills`, the user dirs or `ENTANGLEMENT_SKILLS_DIR`; wired by
`ToolExecutorBuilder::skill_dirs`) — canonical comparison, a missing file
by its nearest existing ancestor, a symlink by where the write lands — is
graded at least `Ask`: never auto-allowed by a mode rule, a grant or an
overlay enable, and no wider grant is recorded for it; an explicit `Deny`
stays `Deny`; unattended, the `Ask` collapses per the mode
(`rhai_trust::floored_write` in `grade_call`). The floor holds inside a
**Run-all** script (its own nested card) but not for a **trusted** script's
own writes, and it applies in every mode, `auto` included (unattended it
collapses to a refusal, then parks up to `question_timeout` on a repeat). Other files in the folder grade normally — safe because a
non-`.rhai` file can never run by `path`. When the escape-root card is what
parks a floored write, its reason names the floor too. A floored card
carries `reason: floored_write` (which outranks `root_escape`) and
`scopes: [once]`, because a wider approval would never be consulted
([ADR-0230](../../adr/0230-structured-card-reason-and-honest-scopes-on-tool-request.md)). `bash` (and a
`call`/`exec` command) cannot be parsed reliably, and an **MCP filesystem
server**'s writes are not host-tool writes, so neither is covered: a
trusted folder the agent can reach that way is effectively an auto-allow
for anything it writes there. Keep trusted folders outside what the agent's
mode lets `bash` (or an MCP server) write — a project-layer entry may name
any folder, so this
is the repository owner's call. `args` is pushed into the scope as a constant (the source is
compiled without the scope, so Rhai cannot constant-fold it into a panic on
assignment). `script` XOR `path`, the field types and unknown keys are
validated before the launch. A background run detaches after the launch and
the read; its listing label is `rhai: <path>`. After its session compacts,
a background run's nested calls grade, grant and park as the successor
([ADR-0241](../../adr/0241-background-script-follows-compaction.md)). A
background run holds a `SeqLease` on its session, and on each successor it
follows, until it ends, so a card it parks after those sessions retired
keeps a real seq however many other sessions retire meanwhile
([ADR-0244](../../adr/0244-seq-order-leases-and-one-task-per-id.md)). Every
run, blocking or background, re-reads its session's mode before each nested
call, so a `SetMode` mid-run changes its grant scope, and a session's overlay
entry (an ancestor's too, across its compaction) stops applying to it
once that session switched mode, and the run's earlier approvals lapse
([ADR-0243](../../adr/0243-running-script-reads-mode-and-overlay-epochs.md)). Separately, for every tool, an
escape card that also cleared a graded `Ask` records both grants on a
`Session`/`Always` approval.

---

Previous: [§8 — One advertised surface, error taxonomy & removed sources](one-surface.md) · Next: [§9 — Lifecycle hooks & pluggable policy seams](hooks-and-seams.md)
